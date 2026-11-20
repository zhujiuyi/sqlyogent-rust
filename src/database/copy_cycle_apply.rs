use std::{path::Path, sync::Mutex};

use mysql::{Conn, prelude::Queryable};

use crate::restore::{RESTORE_CANCELLED, RestoreProgress};

use super::{
    Failure, MAX_RESULT_ROWS, OperationMonitor, copy_cycle_data, copy_restore::native_flag,
    http_session::Session, mysql_error,
};
use copy_cycle_data::CycleDataPlan;

const BEGIN: [&str; 3] = [
    "SET SESSION autocommit = 1",
    "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
    "START TRANSACTION",
];
const TARGET_CHANGED: &str = "开始写入前，目标表的事务属性、触发器或外键定义发生了变化。";
const PARTIAL_REVOKES: &str = "SHOW GLOBAL VARIABLES WHERE Variable_name = 'partial_revokes'";
const PARTIAL_REVOKE_GUARD: &str = "@@GLOBAL.partial_revokes = 0";

fn trigger_scope(partial_revokes: Option<&str>) -> Result<(bool, bool), String> {
    match partial_revokes {
        None => Ok((true, false)),
        Some(value) if value.eq_ignore_ascii_case("OFF") || value == "0" => Ok((true, true)),
        Some(value) if value.eq_ignore_ascii_case("ON") || value == "1" => Ok((false, false)),
        _ => Err("无法确认目标服务器的权限撤销设置。".into()),
    }
}

pub(super) fn native(
    connection: &mut Conn,
    schema: &str,
    path: &Path,
    tables: &[String],
    monitor: &Mutex<OperationMonitor>,
    mut progress: impl FnMut(RestoreProgress) -> Result<(), String>,
) -> Result<usize, String> {
    let partial = connection
        .query_first::<(String, String), _>(PARTIAL_REVOKES)
        .map_err(mysql_error)?;
    let (allow_global, pin) = trigger_scope(partial.as_ref().map(|(_, value)| value.as_str()))?;
    let [table_sql, key_sql] = copy_cycle_data::inspection_sql(schema, tables, allow_global);
    let target_tables = connection.query(table_sql).map_err(mysql_error)?;
    let keys = connection.query(key_sql).map_err(mysql_error)?;
    let mut plan = copy_cycle_data::plan(schema, tables.len(), target_tables, keys, allow_global)?;
    if pin {
        plan.metadata_guards.push(PARTIAL_REVOKE_GUARD.into());
    }

    let mut final_progress = None;
    let result = (|| {
        let count = crate::restore::restore_with_preparation(
            connection,
            "",
            path,
            None,
            |conn| {
                for sql in BEGIN {
                    conn.query_drop(sql).map_err(mysql_error)?;
                }
                for sql in &plan.locks {
                    monitor.lock().unwrap().check_cancelled()?;
                    conn.query_drop(sql).map_err(mysql_error)?;
                }
                for condition in &plan.metadata_guards {
                    monitor.lock().unwrap().check_cancelled()?;
                    if !native_flag(conn, &format!("SELECT {condition}"))? {
                        return Err(TARGET_CHANGED.into());
                    }
                }
                conn.query_drop("SET SESSION foreign_key_checks = 0")
                    .map_err(mysql_error)
            },
            |mut status| {
                if status.finishing {
                    final_progress = Some(status);
                    status.finishing = false;
                }
                progress(status)
            },
        )?;
        for (label, condition) in &plan.validations {
            monitor.lock().unwrap().check_cancelled()?;
            if !native_flag(connection, &format!("SELECT {condition}"))? {
                return Err(format!("数据不满足外键 {label}。请核对父记录和复制范围。"));
            }
        }
        connection
            .query_drop("SET SESSION foreign_key_checks = 1")
            .map_err(mysql_error)?;
        if let Some(status) = final_progress {
            progress(status)?;
        }
        Ok(count)
    })();
    let count = match result {
        Ok(count) => count,
        Err(error) => return Err(rollback(connection, error)),
    };
    match connection.query_drop("COMMIT") {
        Ok(()) => Ok(count),
        Err(error) if error.is_connectivity_error() => Err(format!(
            "无法确认循环外键数据是否提交。请核对目标数据。{}",
            mysql_error(error)
        )),
        Err(error) => Err(rollback(connection, mysql_error(error))),
    }
}

fn rollback(connection: &mut Conn, error: String) -> String {
    if let Err(rollback_error) = connection.query_drop("ROLLBACK") {
        return format!("{error} 无法确认回滚结果。{}", mysql_error(rollback_error));
    }
    if error == RESTORE_CANCELLED {
        return error;
    }
    format!("{error} 循环外键数据复制未提交。")
}

pub(super) fn http(
    session: &Session,
    schema: &str,
    tables: &[String],
    monitor: &Mutex<OperationMonitor>,
) -> Result<(Vec<String>, Vec<String>), String> {
    let partial = session
        .metadata_columns::<2>(PARTIAL_REVOKES, 1)
        .map_err(Failure::message)?;
    let value = match partial.as_slice() {
        [] => None,
        [[Some(name), Some(value)]] if name.eq_ignore_ascii_case("partial_revokes") => {
            Some(value.as_str())
        }
        _ => return Err("隧道未返回有效的权限撤销设置。".into()),
    };
    let (allow_global, pin) = trigger_scope(value)?;
    let [table_sql, key_sql] = copy_cycle_data::inspection_sql(schema, tables, allow_global);
    let target_tables = session
        .metadata_columns::<3>(&table_sql, MAX_RESULT_ROWS)
        .map_err(Failure::message)?
        .into_iter()
        .map(|row| match row {
            [Some(table), Some(engine), Some(triggers)] => triggers
                .parse::<u64>()
                .map(|count| (table, engine, count))
                .map_err(|_| "隧道返回的目标触发器数量无效。"),
            _ => Err("隧道返回的目标表信息不完整。"),
        })
        .collect::<Result<Vec<_>, _>>()?;
    monitor.lock().unwrap().check_cancelled()?;
    let keys = session
        .metadata_columns::<7>(&key_sql, MAX_RESULT_ROWS)
        .map_err(Failure::message)?
        .into_iter()
        .map(|row| match row {
            [
                Some(child),
                Some(name),
                Some(column),
                Some(index),
                Some(parent_db),
                Some(parent),
                Some(parent_column),
            ] => index
                .parse::<u32>()
                .map(|index| (child, name, column, index, parent_db, parent, parent_column))
                .map_err(|_| "隧道返回的外键列顺序无效。"),
            _ => Err("隧道返回的目标外键信息不完整。"),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut plan = copy_cycle_data::plan(schema, tables.len(), target_tables, keys, allow_global)?;
    if pin {
        plan.metadata_guards.push(PARTIAL_REVOKE_GUARD.into());
    }
    Ok(http_steps(plan))
}

fn http_steps(plan: CycleDataPlan) -> (Vec<String>, Vec<String>) {
    let mut prepare = BEGIN.map(str::to_owned).to_vec();
    prepare.extend(plan.locks);
    for condition in plan.metadata_guards {
        guarded_step(&mut prepare, &condition, "HARBOR_COPY_CYCLE_DATA_CHANGED");
    }
    prepare.push("SET SESSION foreign_key_checks = 0".into());

    let mut completion = Vec::new();
    for (_, condition) in plan.validations {
        guarded_step(
            &mut completion,
            &condition,
            "HARBOR_COPY_CYCLE_DATA_INVALID",
        );
    }
    completion.extend(["SET SESSION foreign_key_checks = 1".into(), "COMMIT".into()]);
    (prepare, completion)
}

fn guarded_step(statements: &mut Vec<String>, condition: &str, failure: &str) {
    statements.extend([
        format!("SET @harbor_cycle_copy = IF({condition}, 'DO 0', '{failure}')"),
        "PREPARE harbor_cycle_copy FROM @harbor_cycle_copy".into(),
        "DEALLOCATE PREPARE harbor_cycle_copy".into(),
    ]);
}
