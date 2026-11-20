use std::path::Path;
use std::sync::Mutex;

use crate::restore::RestoreProgress;
use crate::sql_tools::mysql_text_literal;

const PARENT_COLLISION: &str =
    "目标端有外键引用表与本次复制的表重名。请选择其他目标库或调整所选对象。";

use super::{
    Connection, DbObject, Failure, MAX_RESULT_ROWS, OperationMonitor, ProgressUpdate, SchemaOrigin,
    copy_cycle_apply, copy_cycle_target, http_session, mysql_error, transfer::CopyDependencies,
    transfer_connection,
};
use mysql::prelude::Queryable;

pub(super) fn run(
    active: &mut Connection,
    schema: &str,
    path: &Path,
    source_schema: &str,
    source: &SchemaOrigin,
    dependencies: &CopyDependencies,
    monitor: Option<&Mutex<OperationMonitor>>,
) -> Result<usize, String> {
    let monitor = monitor.ok_or("复制任务未初始化。")?;
    monitor.lock().unwrap().check_cancelled()?;
    let collision = parent_collision(schema, dependencies);
    let progress = |progress: RestoreProgress| {
        monitor
            .lock()
            .unwrap()
            .update(ProgressUpdate::Restore(progress), progress.finishing)
    };
    match active {
        Connection::Native(active) => {
            let expected = SchemaOrigin::native(&mut active.conn, &active.identity)?;
            let mut connection = transfer_connection(&active.opts)?;
            let target = SchemaOrigin::native(&mut connection, &active.identity)?;
            if !target.same_server(&expected) {
                return Err(
                    "独立复制会话与目标工作区连接到了不同服务器。请重新检查目标连接。".into(),
                );
            }
            verify_destination(schema, source_schema, source, &target, |sql| {
                native_flag(&mut connection, sql)
            })?;
            if let Some(condition) = &collision
                && native_flag(&mut connection, &format!("SELECT {condition}"))?
            {
                return Err(PARENT_COLLISION.into());
            }
            for parent in &dependencies.external_parents {
                monitor.lock().unwrap().check_cancelled()?;
                if !native_flag(
                    &mut connection,
                    &format!("SELECT {}", parent_exists(parent)),
                )? {
                    return Err(missing_parent(parent));
                }
            }
            if dependencies.cyclic_data {
                return copy_cycle_apply::native(
                    &mut connection,
                    schema,
                    path,
                    &dependencies.target_tables,
                    monitor,
                    progress,
                );
            }
            crate::restore::restore_with_preparation(
                &mut connection,
                "",
                path,
                None,
                |connection| {
                    if !dependencies.cyclic_overwrite {
                        return Ok(());
                    }
                    let query = copy_cycle_target::inbound_sql(schema, &dependencies.target_tables);
                    let rows = connection
                        .query::<(String, String, String, bool), _>(query)
                        .map_err(mysql_error)?;
                    let statements = cycle_preparation(rows)?;
                    monitor.lock().unwrap().check_cancelled()?;
                    for statement in statements {
                        connection.query_drop(statement).map_err(|error| {
                            format!(
                                "无法准备循环外键的覆盖复制，之前执行的操作可能已经提交。{}",
                                mysql_error(error)
                            )
                        })?;
                    }
                    Ok(())
                },
                progress,
            )
        }
        Connection::Http(active) => {
            let target = SchemaOrigin::http(active)?;
            verify_destination(schema, source_schema, source, &target, |sql| {
                http_flag(active, sql)
            })?;
            let mut guards = vec![target.guard()];
            if source.matches_identity(&target) {
                let condition = database_names_equal(schema, source_schema);
                guards.push(format!(
                    "SET @harbor_schema_target = IF({condition}, 'HARBOR_COPY_SAME_SCHEMA', 'DO 0')"
                ));
            }
            let mut conditions = Vec::with_capacity(dependencies.external_parents.len());
            if let Some(condition) = &collision {
                if http_flag(active, &format!("SELECT {condition}"))? {
                    return Err(PARENT_COLLISION.into());
                }
                conditions.push(format!("NOT {condition}"));
            }
            for parent in &dependencies.external_parents {
                monitor.lock().unwrap().check_cancelled()?;
                let condition = parent_exists(parent);
                if !http_flag(active, &format!("SELECT {condition}"))? {
                    return Err(missing_parent(parent));
                }
                conditions.push(condition);
            }
            if !conditions.is_empty() {
                guards.push(format!(
                    "SET @harbor_schema_target = IF({}, 'DO 0', 'HARBOR_COPY_PARENT_CHANGED')",
                    conditions.join(" AND ")
                ));
            }
            let (preparation, completion) = if dependencies.cyclic_data {
                copy_cycle_apply::http(active, schema, &dependencies.target_tables, monitor)?
            } else if dependencies.cyclic_overwrite {
                let query = copy_cycle_target::inbound_sql(schema, &dependencies.target_tables);
                let rows = active
                    .metadata_columns::<4>(&query, MAX_RESULT_ROWS)
                    .map_err(Failure::message)?
                    .into_iter()
                    .map(|row| match row {
                        [
                            Some(database),
                            Some(table),
                            Some(constraint),
                            Some(selected),
                        ] => match selected.as_str() {
                            "1" => Ok((database, table, constraint, true)),
                            "0" => Ok((database, table, constraint, false)),
                            _ => Err("隧道返回了无法识别的目标外键范围。"),
                        },
                        _ => Err("隧道返回的目标外键信息不完整。"),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let outside = copy_cycle_target::outside_inbound_condition(
                    schema,
                    &dependencies.target_tables,
                );
                guards.push(format!(
                    "SET @harbor_schema_target = IF({outside}, 'HARBOR_COPY_CYCLE_OUTSIDE_REF', 'DO 0')"
                ));
                (cycle_preparation(rows)?, Vec::new())
            } else {
                (Vec::new(), Vec::new())
            };
            active.restore_guarded(
                "",
                path,
                None,
                http_session::RestoreSetup {
                    guards: &guards,
                    preparation: &preparation,
                    completion: &completion,
                },
                progress,
            )
        }
    }
}

fn cycle_preparation(rows: Vec<(String, String, String, bool)>) -> Result<Vec<String>, String> {
    let mut statements = copy_cycle_target::dropping_statements(rows)?;
    if !statements.is_empty() {
        statements.splice(
            0..0,
            [
                "SET NAMES utf8mb4".into(),
                "SET SESSION foreign_key_checks = 1".into(),
            ],
        );
    }
    Ok(statements)
}

fn parent_collision(schema: &str, dependencies: &CopyDependencies) -> Option<String> {
    if dependencies.external_parents.is_empty() || dependencies.target_tables.is_empty() {
        return None;
    }
    let parents = dependencies
        .external_parents
        .iter()
        .map(|parent| {
            format!(
                "SELECT {} AS parent_db, {} AS parent_table",
                mysql_text_literal(&parent.schema),
                mysql_text_literal(&parent.name)
            )
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let targets = dependencies
        .target_tables
        .iter()
        .map(|table| format!("SELECT {} AS copied_table", mysql_text_literal(table)))
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let parent_db = comparable_sql("parents.parent_db");
    let parent_table = comparable_sql("parents.parent_table");
    let dest_db = comparable_sql(&mysql_text_literal(schema));
    let dest_table = comparable_sql("targets.copied_table");
    Some(format!(
        "EXISTS(SELECT 1 FROM ({parents}) AS parents JOIN ({targets}) AS targets \
         ON {parent_table} = {dest_table} WHERE {parent_db} = {dest_db})"
    ))
}

fn parent_exists(parent: &DbObject) -> String {
    let schema = comparable_sql(&mysql_text_literal(&parent.schema));
    let table = comparable_sql(&mysql_text_literal(&parent.name));
    let db_column = comparable_sql("TABLE_SCHEMA");
    let table_column = comparable_sql("TABLE_NAME");
    format!(
        "EXISTS(SELECT 1 FROM information_schema.TABLES WHERE TABLE_TYPE IN ('BASE TABLE', 'SYSTEM VERSIONED') \
         AND {db_column} = {schema} AND {table_column} = {table})"
    )
}

pub(super) fn comparable_sql(expression: &str) -> String {
    format!("BINARY IF(@@lower_case_table_names = 0, {expression}, LOWER({expression}))")
}

fn missing_parent(parent: &DbObject) -> String {
    format!(
        "目标服务器缺少外键引用的表 {}，或当前账号无法查看该表。请先确认该表可用于目标复制。",
        parent.qualified_name()
    )
}

pub(super) fn native_flag(connection: &mut mysql::Conn, sql: &str) -> Result<bool, String> {
    match connection.query_first::<u8, _>(sql).map_err(mysql_error)? {
        Some(1) => Ok(true),
        Some(0) => Ok(false),
        _ => Err("目标服务器未返回有效的复制检查结果。".into()),
    }
}

fn http_flag(session: &http_session::Session, sql: &str) -> Result<bool, String> {
    let reply = session.read_set(sql)?;
    match reply
        .rows
        .first()
        .and_then(|row| http_session::text(row.first()))
    {
        Some("1") => Ok(true),
        Some("0") => Ok(false),
        _ => Err("目标隧道未返回有效的复制检查结果。".into()),
    }
}

fn verify_destination(
    schema: &str,
    source_schema: &str,
    source: &SchemaOrigin,
    target: &SchemaOrigin,
    compare: impl FnOnce(&str) -> Result<bool, String>,
) -> Result<(), String> {
    if source.matches_identity(target)
        && (schema == source_schema
            || compare(&format!(
                "SELECT {}",
                database_names_equal(schema, source_schema)
            ))?)
    {
        return Err("源和目标是同一个服务器上的同一个数据库。请选择其他目标数据库。".into());
    }
    Ok(())
}

fn database_names_equal(schema: &str, source_schema: &str) -> String {
    let target_name = comparable_sql(&mysql_text_literal(schema));
    let source_name = comparable_sql(&mysql_text_literal(source_schema));
    format!("{target_name} = {source_name}")
}
