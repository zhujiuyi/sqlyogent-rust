use crate::sql_tools::{mysql_binary_literal, mysql_text_literal};

use super::super::http_session::{Session, batch_statements, text};
use super::super::row_editor::{http_layout, import_layout_guard, layout_guard};
use super::super::{Cell, DbObject, OdbcConflict, OdbcReset};
use super::*;

const MAX_BATCH_SQL: usize = 32 * 1024 * 1024;
const IMPORT_SQL_MODE: &str =
    "SET SESSION sql_mode = 'STRICT_ALL_TABLES,NO_ENGINE_SUBSTITUTION,NO_AUTO_VALUE_ON_ZERO'";
const FOREIGN_KEY_CHECKS: &str = "SET SESSION foreign_key_checks = 1";

struct StagedTarget {
    stage: DbObject,
    destination: DbObject,
    create_sql: String,
    previous: Option<PreviousTable>,
}

struct PreviousTable {
    backup: DbObject,
    definition: String,
    server: ResetServer,
}

enum PublicationError {
    Unpublished(String),
    Unconfirmed(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LockRelease {
    BeforeDdl,
    AfterDdl,
}

pub(in crate::database) fn run_http(
    session: &Session,
    tables: Vec<OdbcTarget>,
    cancel: &AtomicBool,
) -> Result<OdbcImportCounts, String> {
    run_http_checked(session, tables, cancel, None)
}

pub(in crate::database) fn run_http_checked(
    session: &Session,
    mut tables: Vec<OdbcTarget>,
    cancel: &AtomicBool,
    source_guard: Option<&str>,
) -> Result<OdbcImportCounts, String> {
    let staged_import = validate_targets(&tables, cancel)?;
    let foreign_keys = plan_foreign_keys(&tables, cancel)?;
    let linked = !foreign_keys.is_empty();
    if tables
        .iter()
        .any(|table| !table.rows.is_empty() && table.conflict == OdbcConflict::KeepTarget)
        && !supports_duplicate_handler(session)?
    {
        return Err("HTTP 导入的「保留目标行」需要支持匿名复合语句的 MariaDB。其他服务器可用直连或 SSH 执行此策略。".into());
    }
    if let Some(guard) = source_guard {
        let sql = format!("SELECT ({guard}) AS harbor_odbc_destination_safe");
        let safe = confirm(session, &[&sql], "harbor_odbc_destination_safe")
            .map_err(|error| format!("无法核对源表与目标表的身份。{error}"))?;
        if !safe {
            return Err(SOURCE_TARGET_SAME_TABLE.into());
        }
    }
    if !staged_import {
        return write_batch(session, &tables, cancel, source_guard);
    }

    let token = RandomState::new().hash_one(SystemTime::now());
    let mut reset_server = None;
    let plans = tables
        .iter()
        .enumerate()
        .map(|(index, table)| {
            check_import_cancelled(Some(cancel))?;
            plan_target(session, table, token, index, &mut reset_server)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut staged = Vec::new();
    let loaded = (|| {
        for (table, plan) in tables.iter_mut().zip(plans) {
            check_import_cancelled(Some(cancel))?;
            let qualified = plan.stage.qualified_name();
            let created = confirm(
                session,
                &[
                    "SET NAMES utf8mb4",
                    IMPORT_SQL_MODE,
                    &plan.create_sql,
                    "SELECT 1 AS harbor_odbc_stage_created",
                ],
                "harbor_odbc_stage_created",
            )
            .map_err(|error| {
                format!("{IMPORT_UNCONFIRMED}无法确认暂存表 {qualified} 的创建结果。{error}")
            })?;
            if !created {
                return Err(format!(
                    "{IMPORT_UNCONFIRMED}无法确认暂存表 {qualified} 的创建结果。"
                ));
            }
            table.object = plan.stage.clone();
            table.expected_ddl = None;
            staged.push(plan);
            if table.reset == OdbcReset::Truncate {
                let plan = staged.last().expect("confirmed stage");
                let previous = plan.previous.as_ref().expect("truncate requires target");
                if read_reset_definition(session, &plan.stage)?.as_ref()
                    != Some(&previous.definition)
                {
                    return Err(format!(
                        "无法完整复制目标表 {} 的定义。",
                        plan.destination.qualified_name()
                    ));
                }
            }
        }
        let mut counts = write_batch(session, &tables, cancel, source_guard)?;
        if linked {
            let stages: Vec<_> = staged.iter().map(|table| &table.stage).collect();
            install_foreign_keys(
                &foreign_keys,
                &stages,
                token,
                cancel,
                &mut counts.warnings,
                |sql| add_http_foreign_key(session, sql),
            )?;
        }
        Ok(counts)
    })();
    let mut counts = loaded.map_err(|error| cleanup_stages(session, &staged, linked, error))?;
    match publish_staged(session, &staged, cancel, source_guard) {
        Ok(()) => {}
        Err(PublicationError::Unpublished(error)) => {
            return Err(cleanup_stages(session, &staged, linked, error));
        }
        Err(PublicationError::Unconfirmed(error)) => {
            let related = staged
                .iter()
                .flat_map(|table| {
                    std::iter::once(table.stage.qualified_name()).chain(
                        table
                            .previous
                            .as_ref()
                            .map(|previous| previous.backup.qualified_name()),
                    )
                })
                .collect::<Vec<_>>()
                .join("、");
            return Err(format!(
                "{IMPORT_UNCONFIRMED}无法确认 HTTP 导入是否已切换目标表。请核对目标表及以下暂存表和旧表。{related}。{error}"
            ));
        }
    }
    for previous in staged.iter().filter_map(|table| table.previous.as_ref()) {
        if let Err(error) = cleanup_backup(session, previous) {
            counts.warnings.push(format!(
                "导入已发布。旧表 {} 的清理状态需核对。{error}",
                previous.backup.qualified_name()
            ));
        }
    }
    Ok(counts)
}

fn plan_target(
    session: &Session,
    table: &OdbcTarget,
    token: u64,
    index: usize,
    reset_server: &mut Option<ResetServer>,
) -> Result<StagedTarget, String> {
    let destination = table.object.clone();
    let target_name = destination.qualified_name();
    let mut stage = destination.clone();
    stage.name = format!("_harbor_odbc_{token:016x}_{index}");
    let stage_name = stage.qualified_name();
    let previous = if table.reset == OdbcReset::None {
        None
    } else if let Some(definition) = read_reset_definition(session, &destination)? {
        if let Some(expected) = &table.expected_ddl
            && reset_definition_body(expected.clone(), &destination)? != definition
        {
            return Err(format!(
                "目标表 {target_name} 的结构已变化，请重新检查映射。"
            ));
        }
        let server = match *reset_server {
            Some(server) => server,
            None => {
                let server = ResetServer::parse(&server_version(session)?)?;
                *reset_server = Some(server);
                server
            }
        };
        let guard = reset_dependency_guard(&destination, server);
        let sql = format!("SELECT {guard} AS harbor_odbc_reset_dependencies_ok");
        let dependencies_ok = confirm(session, &[&sql], "harbor_odbc_reset_dependencies_ok")
            .map_err(|error| {
                format!(
                    "无法核对目标表 {target_name} 的依赖。检查跨库外键需要 PROCESS 权限。{error}"
                )
            })?;
        if !dependencies_ok {
            return Err(format!(
                "无法确认目标表 {target_name} 的触发器和外键均可安全切换。请核对依赖及账号权限。"
            ));
        }
        let mut backup = destination.clone();
        backup.name = format!("_harbor_odbc_old_{token:016x}_{index}");
        Some(PreviousTable {
            backup,
            definition,
            server,
        })
    } else if table.reset == OdbcReset::Truncate {
        return Err(format!("无法清空目标表 {target_name}，请先确认该表存在。"));
    } else {
        None
    };
    let create_sql = if table.reset == OdbcReset::Truncate {
        format!("CREATE TABLE {stage_name} LIKE {target_name}")
    } else {
        staged_create_sql(table, &stage, "")?
    };
    Ok(StagedTarget {
        stage,
        destination,
        create_sql,
        previous,
    })
}

fn read_reset_definition(session: &Session, object: &DbObject) -> Result<Option<String>, String> {
    let rows = session
        .metadata_columns::<5>(&reset_table_sql(object), 1)
        .map_err(super::super::Failure::message)?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let [
        Some(kind),
        engine,
        Some(transactions),
        Some(triggers),
        Some(foreign_keys),
    ] = row
    else {
        return Err("隧道返回的目标表元数据不完整。".into());
    };
    let flag = |value: &str| match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err("隧道返回的目标表依赖状态无效。"),
    };
    check_reset_table(
        object,
        kind,
        engine.as_deref(),
        transactions,
        flag(triggers)?,
        flag(foreign_keys)?,
    )?;
    let (ddl, _) = session.definition_with_context(object)?;
    reset_definition_body(ddl, object).map(Some)
}

fn publish_staged(
    session: &Session,
    staged: &[StagedTarget],
    cancel: &AtomicBool,
    source_guard: Option<&str>,
) -> Result<(), PublicationError> {
    let prepare = || {
        let mut renames = Vec::new();
        let mut guards = source_guard
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let release = staged
            .iter()
            .find_map(|table| table.previous.as_ref())
            .map(|previous| {
                if previous.server.can_rename_locked() {
                    LockRelease::AfterDdl
                } else {
                    // MariaDB rejects RENAME while an explicit table lock is held.
                    LockRelease::BeforeDdl
                }
            });
        let mut locked = Vec::new();
        for table in staged {
            check_import_cancelled(Some(cancel))?;
            let target = table.destination.qualified_name();
            let stage = table.stage.qualified_name();
            if release.is_some() {
                locked.push(format!("{stage} WRITE"));
            }
            if let Some(previous) = &table.previous {
                let layout = http_layout(session, table.destination.clone())?;
                let current = read_reset_definition(session, &table.destination)?;
                if current.as_ref() != Some(&previous.definition) {
                    return Err(format!(
                        "目标表 {target} 的定义已变化，请重新检查后再导入。"
                    ));
                }
                guards.push(reset_eligibility_guard(&table.destination, previous.server));
                guards.push(layout_guard(&layout));
                locked.push(format!("{target} WRITE"));
                renames.push(format!("{target} TO {}", previous.backup.qualified_name()));
            }
            renames.push(format!("{stage} TO {target}"));
        }
        let guard = if guards.is_empty() {
            "1".into()
        } else {
            guards.join(" AND ")
        };
        let sql = format!("RENAME TABLE {}", renames.join(", "));
        locked.sort_unstable();
        let lock = release.map(|release| (format!("LOCK TABLES {}", locked.join(", ")), release));
        let commands = guarded_ddl(&guard, &sql, lock);
        let payload = confirmation_payload(
            session,
            &commands.iter().map(String::as_str).collect::<Vec<_>>(),
        )?;
        check_import_cancelled(Some(cancel))?;
        Ok(payload)
    };
    let payload = prepare().map_err(PublicationError::Unpublished)?;
    match execute_confirmation(session, payload, "harbor_odbc_ddl_done") {
        Ok(true) => Ok(()),
        Ok(false) => Err(PublicationError::Unpublished(
            "目标表结构或依赖发生变化，请重新检查后再导入。".into(),
        )),
        Err(error) => Err(PublicationError::Unconfirmed(error)),
    }
}

fn cleanup_stages(
    session: &Session,
    staged: &[StagedTarget],
    linked: bool,
    error: String,
) -> String {
    let groups = stage_cleanup_groups(staged.iter().map(|table| &table.stage), linked);
    let stale: Vec<_> = groups
        .into_iter()
        .filter(|group| {
            let sql = format!("DROP TABLE IF EXISTS {}", group.join(", "));
            // The tunnel uses one fresh session for this batch. Its disabled
            // checks apply only to the group's private staging tables.
            let setup = if linked {
                "SET SESSION foreign_key_checks = 0"
            } else {
                FOREIGN_KEY_CHECKS
            };
            let mut commands = vec![setup, &sql];
            if linked {
                commands.push(FOREIGN_KEY_CHECKS);
            }
            commands.push("SELECT 1 AS harbor_odbc_stage_dropped");
            !matches!(
                confirm(session, &commands, "harbor_odbc_stage_dropped"),
                Ok(true)
            )
        })
        .flatten()
        .collect();
    if stale.is_empty() {
        error
    } else {
        format!(
            "{IMPORT_UNCONFIRMED}{} 无法确认以下暂存表是否已清理。{}",
            error.strip_prefix(IMPORT_UNCONFIRMED).unwrap_or(&error),
            stale.join("、")
        )
    }
}

fn cleanup_backup(session: &Session, previous: &PreviousTable) -> Result<(), String> {
    let layout = http_layout(session, previous.backup.clone())?;
    let definition =
        read_reset_definition(session, &previous.backup)?.ok_or("数据库未返回旧表定义。")?;
    if definition != previous.definition {
        return Err("旧表定义发生变化，已保留供核对。".into());
    }
    let guard = format!(
        "{} AND {}",
        reset_eligibility_guard(&previous.backup, previous.server),
        layout_guard(&layout)
    );
    let backup = previous.backup.qualified_name();
    let sql = format!("DROP TABLE {backup}");
    let commands = guarded_ddl(
        &guard,
        &sql,
        Some((format!("LOCK TABLES {backup} WRITE"), LockRelease::AfterDdl)),
    );
    if confirm(
        session,
        &commands.iter().map(String::as_str).collect::<Vec<_>>(),
        "harbor_odbc_ddl_done",
    )? {
        Ok(())
    } else {
        Err("旧表依赖发生变化，已保留供核对。".into())
    }
}

fn guarded_ddl(guard: &str, sql: &str, lock: Option<(String, LockRelease)>) -> Vec<String> {
    let mut commands = vec![FOREIGN_KEY_CHECKS.into()];
    let release = lock.as_ref().map(|(_, release)| *release);
    commands.extend(lock.map(|(sql, _)| sql));
    commands.extend([
        format!("SET @harbor_odbc_ddl_ok = ({guard})"),
        format!(
            "SET @harbor_odbc_ddl = IF(@harbor_odbc_ddl_ok = 1, {}, 'DO 0')",
            mysql_text_literal(sql)
        ),
        "PREPARE harbor_odbc_ddl FROM @harbor_odbc_ddl".into(),
    ]);
    if release == Some(LockRelease::BeforeDdl) {
        commands.push("UNLOCK TABLES".into());
    }
    commands.extend([
        "EXECUTE harbor_odbc_ddl".into(),
        "DEALLOCATE PREPARE harbor_odbc_ddl".into(),
    ]);
    if release == Some(LockRelease::AfterDdl) {
        commands.push("UNLOCK TABLES".into());
    }
    commands.push("SELECT @harbor_odbc_ddl_ok AS harbor_odbc_ddl_done".into());
    commands
}

fn confirm(session: &Session, commands: &[&str], label: &str) -> Result<bool, String> {
    execute_confirmation(session, confirmation_payload(session, commands)?, label)
}

fn confirmation_payload(
    session: &Session,
    commands: &[&str],
) -> Result<crate::http_tunnel::RequestBody, String> {
    session.prepare_payload(&batch_statements(commands)?, true, "")
}

fn execute_confirmation(
    session: &Session,
    payload: crate::http_tunnel::RequestBody,
    label: &str,
) -> Result<bool, String> {
    read_confirmation(session.execute_payload(payload, 1)?, label)
}

fn read_confirmation(reply: crate::http_tunnel::Reply, label: &str) -> Result<bool, String> {
    let set = single_set(reply)?;
    if let ([field], [row]) = (set.columns.as_slice(), set.rows.as_slice())
        && field.name == label
        && let [Some(raw)] = row.as_slice()
    {
        match raw.as_slice() {
            b"1" => return Ok(true),
            b"0" => return Ok(false),
            _ => {}
        }
    }
    Err("隧道未返回可确认的操作结果。".into())
}

fn add_http_foreign_key(session: &Session, sql: &str) -> Result<(), ForeignKeyError> {
    let payload = confirmation_payload(
        session,
        &[
            "SET NAMES utf8mb4",
            FOREIGN_KEY_CHECKS,
            sql,
            "SELECT 1 AS harbor_odbc_foreign_key_added",
        ],
    )
    .map_err(ForeignKeyError::other)?;
    let mut reply = session
        .execute_payload(payload, 1)
        .map_err(ForeignKeyError::other)?;
    if let Some(error) = reply.error.take() {
        return Err(ForeignKeyError::server(error.code, error.message));
    }
    if read_confirmation(reply, "harbor_odbc_foreign_key_added").map_err(ForeignKeyError::other)? {
        Ok(())
    } else {
        Err(ForeignKeyError::other(
            "无法确认暂存表外键的创建结果。".into(),
        ))
    }
}

fn supports_duplicate_handler(session: &Session) -> Result<bool, String> {
    let version = server_version(session)?;
    if !version.to_ascii_lowercase().contains("mariadb") {
        return Ok(false);
    }
    let version = version.strip_prefix("5.5.5-").unwrap_or(&version);
    let mut parts = version
        .split(|character: char| !character.is_ascii_digit())
        .map(|value| value.parse::<u32>().unwrap_or(0));
    Ok((parts.next().unwrap_or(0), parts.next().unwrap_or(0)) >= (10, 1))
}

fn server_version(session: &Session) -> Result<String, String> {
    let set = session.read_set("SELECT VERSION()")?;
    set.rows
        .first()
        .and_then(|row| text(row.first()))
        .map(str::to_owned)
        .ok_or_else(|| "无法读取 HTTP 目标版本。".into())
}

fn write_batch(
    session: &Session,
    tables: &[OdbcTarget],
    cancel: &AtomicBool,
    source_guard: Option<&str>,
) -> Result<OdbcImportCounts, String> {
    let mut batch = ImportBatch::default();
    let tracked_rows = tables
        .iter()
        .any(|table| table.track_applied_rows)
        .then(|| {
            tables
                .iter()
                .filter(|table| table.track_applied_rows)
                .map(|table| table.rows.len())
                .sum::<usize>()
        });
    for sql in [
        "SET NAMES utf8mb4",
        IMPORT_SQL_MODE,
        "SET SESSION autocommit = 0, SESSION foreign_key_checks = 1, SESSION unique_checks = 1",
        "SET @harbor_import_ok = 1, @harbor_rows = 0, @harbor_skipped = 0, @harbor_replaced = 0",
        "START TRANSACTION",
    ] {
        batch.push(sql.into())?;
    }
    if let Some(guard) = source_guard {
        // Each PHP request gets a fresh connection. Recheck on the session
        // doing the writes instead of relying on the earlier read-only proof.
        batch.push(format!("SET @harbor_import_ok = ({guard})"))?;
    }
    if tracked_rows.is_some() {
        batch.push("SET @harbor_row_status = ''".into())?;
    }
    for table in tables {
        check_import_cancelled(Some(cancel))?;
        let object = &table.object;
        let qualified = object.qualified_name();
        let layout = http_layout(session, object.clone())?;
        if let Some(expected) = &table.expected_ddl {
            let (current, _) = session.definition_with_context(object)?;
            if super::super::sync_engine::stable_definition(current)
                != super::super::sync_engine::stable_definition(expected.clone())
            {
                return Err(format!("目标表 {qualified} 的结构已变更，请重新检查映射。"));
            }
        }
        let guard = match table.staged_engine() {
            Some(engine) => {
                let check = import_layout_guard(&layout, engine);
                if table.engine.is_some() || table.create_sql.is_some() {
                    let sql = format!("SELECT ({check}) AS harbor_odbc_engine_confirmed");
                    if !confirm(session, &[&sql], "harbor_odbc_engine_confirmed")? {
                        return Err(format!(
                            "暂存表 {qualified} 的结构或存储引擎与要求不一致。请选择服务器支持的 {engine} 并核对建表 SQL。"
                        ));
                    }
                }
                check
            }
            None => layout_guard(&layout),
        };
        batch.push(format!("SELECT 1 FROM {qualified} LIMIT 0"))?;
        batch.push(format!(
            "SET @harbor_import_ok = (@harbor_import_ok = 1 AND {guard})",
        ))?;
        let columns = table
            .columns
            .iter()
            .map(|name| quote_identifier(name))
            .collect::<Vec<_>>()
            .join(", ");
        let command = if table.conflict == OdbcConflict::Replace {
            "REPLACE"
        } else {
            "INSERT"
        };
        for row in &table.rows {
            check_import_cancelled(Some(cancel))?;
            let values = row
                .iter()
                .map(|cell| match cell {
                    Cell::Null => "NULL".into(),
                    Cell::Text(text) => mysql_text_literal(text),
                    Cell::Binary(bytes) => mysql_binary_literal(bytes),
                })
                .collect::<Vec<_>>()
                .join(", ");
            let write = format!(
                "{command} INTO {qualified} ({columns}) SELECT {values} WHERE @harbor_import_ok = 1"
            );
            batch.row(write, table.conflict, table.track_applied_rows)?;
        }
    }
    if let Some(expected) = tracked_rows {
        let input_rows: usize = tables.iter().map(|table| table.rows.len()).sum();
        batch.push(format!(
            "SET @harbor_import_ok = (@harbor_import_ok = 1 AND COALESCE(OCTET_LENGTH(@harbor_row_status), -1) = {expected} AND @harbor_rows + @harbor_skipped = {input_rows})"
        ))?;
    }
    for sql in [
        "SET @harbor_finish = IF(@harbor_import_ok = 1, 'COMMIT AND NO CHAIN NO RELEASE', 'DO 0')",
        "PREPARE harbor_import_finish FROM @harbor_finish",
        "EXECUTE harbor_import_finish",
        "DEALLOCATE PREPARE harbor_import_finish",
        "ROLLBACK AND NO CHAIN NO RELEASE",
    ] {
        batch.push(sql.into())?;
    }
    let mut report = "SELECT @harbor_import_ok AS harbor_import_ok, @harbor_rows AS written, @harbor_skipped AS skipped, @harbor_replaced AS replaced".to_owned();
    if tracked_rows.is_some() {
        report.push_str(", @harbor_row_status AS harbor_row_status");
    }
    batch.push(report)?;
    let sql = batch_statements(
        &batch
            .commands
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    )?;
    let payload = session.prepare_payload(&sql, true, "")?;
    check_import_cancelled(Some(cancel))?;
    let uncertain = |error| {
        format!("{IMPORT_UNCONFIRMED}无法确认 HTTP 写入事务是否已提交，请先核对目标表。{error}")
    };
    let reply = session.execute_payload(payload, 1).map_err(uncertain)?;
    let set = single_set(reply).map_err(uncertain)?;
    let labels = [
        "harbor_import_ok",
        "written",
        "skipped",
        "replaced",
        "harbor_row_status",
    ];
    let label_count = if tracked_rows.is_some() {
        labels.len()
    } else {
        labels.len() - 1
    };
    let [row] = set.rows.as_slice() else {
        return Err(uncertain("隧道没有返回完整的导入状态。".into()));
    };
    if set
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .ne(labels[..label_count].iter().copied())
    {
        return Err(uncertain("隧道返回了意外的导入状态。".into()));
    }
    let number = |index| {
        row.get(index)
            .and_then(Option::as_deref)
            .and_then(|raw| std::str::from_utf8(raw).ok())
            .and_then(|value| value.parse::<u64>().ok())
    };
    match (number(0), number(1), number(2), number(3)) {
        (Some(1), Some(rows), Some(skipped), Some(replaced)) => {
            let applied_rows = match tracked_rows {
                Some(expected) => {
                    let status = row.get(4).and_then(Option::as_deref);
                    decode_applied_rows(tables, status, expected, rows, skipped)
                        .map_err(uncertain)?
                }
                None => Vec::new(),
            };
            Ok(OdbcImportCounts {
                rows,
                skipped,
                replaced,
                applied_rows,
                ..Default::default()
            })
        }
        (Some(0), _, _, _) => {
            if tracked_rows.is_some() {
                Err("目标表结构、写入状态或逐行结果核对失败，导入事务已回滚。".into())
            } else {
                Err("目标表结构发生变化或数据库返回写入警告，导入事务已回滚。".into())
            }
        }
        _ => Err(uncertain("隧道提交状态无法解析。".into())),
    }
}

fn decode_applied_rows(
    tables: &[OdbcTarget],
    status: Option<&[u8]>,
    expected: usize,
    rows: u64,
    skipped: u64,
) -> Result<Vec<Option<Vec<bool>>>, String> {
    const INVALID_RECEIPT: &str = "隧道返回的逐行导入结果无法核对。";
    let mut status = status
        .filter(|bits| bits.len() == expected)
        .ok_or(INVALID_RECEIPT)?;
    let mut result = Vec::with_capacity(tables.len());
    let mut tracked_skips = 0_u64;
    let mut untracked_keep_rows = 0_u64;
    let mut input_rows = 0_u64;
    for table in tables {
        let count = table.rows.len();
        input_rows += count as u64;
        if !table.track_applied_rows {
            if table.conflict == OdbcConflict::KeepTarget {
                untracked_keep_rows += count as u64;
            }
            result.push(None);
            continue;
        }
        let (table_status, remainder) = status.split_at_checked(count).ok_or(INVALID_RECEIPT)?;
        let mut applied = Vec::with_capacity(count);
        for &bit in table_status {
            match bit {
                b'1' => applied.push(true),
                b'0' if table.conflict == OdbcConflict::KeepTarget => {
                    tracked_skips += 1;
                    applied.push(false);
                }
                _ => return Err(INVALID_RECEIPT.into()),
            }
        }
        result.push(Some(applied));
        status = remainder;
    }
    if !status.is_empty()
        || rows.checked_add(skipped) != Some(input_rows)
        || skipped
            .checked_sub(tracked_skips)
            .is_none_or(|left| left > untracked_keep_rows)
    {
        return Err(INVALID_RECEIPT.into());
    }
    Ok(result)
}

fn single_set(
    mut reply: crate::http_tunnel::Reply,
) -> Result<crate::http_tunnel::ResultSet, String> {
    if let Some(error) = reply.error {
        return Err(format!("ERROR {}: {}", error.code, error.message));
    }
    if reply.sets.len() != 1 || reply.omitted_sets != 0 {
        return Err("隧道没有返回唯一的导入状态结果。".into());
    }
    let set = reply.sets.remove(0);
    if set.truncated {
        return Err("隧道截断了导入状态结果。".into());
    }
    Ok(set)
}

#[derive(Default)]
struct ImportBatch {
    commands: Vec<String>,
    estimated_bytes: usize,
}

impl ImportBatch {
    fn push(&mut self, sql: String) -> Result<(), String> {
        self.estimated_bytes += sql.len() + 40;
        if self.estimated_bytes > MAX_BATCH_SQL {
            return Err("HTTP 导入的 SQL 批次超过 32 MiB，请缩小导入范围。".into());
        }
        self.commands.push(sql);
        Ok(())
    }

    fn row(&mut self, write: String, conflict: OdbcConflict, track: bool) -> Result<(), String> {
        if conflict == OdbcConflict::KeepTarget {
            self.push(
                "SET @harbor_duplicate = 0, @harbor_affected = 0, @harbor_warnings = 0".into(),
            )?;
            self.push(format!(
                "BEGIN NOT ATOMIC\n\
                 DECLARE CONTINUE HANDLER FOR 1062 SET @harbor_duplicate = 1;\n\
                 {write};\n\
                 IF @harbor_duplicate = 0 THEN GET DIAGNOSTICS @harbor_warnings = NUMBER, @harbor_affected = ROW_COUNT; END IF;\n\
                 END"
            ))?;
            self.push("SET @harbor_import_ok = (@harbor_import_ok = 1 AND (@harbor_duplicate = 1 OR (@harbor_affected = 1 AND @harbor_warnings = 0)))".into())?;
            let mut count = "SET @harbor_rows = @harbor_rows + IF(@harbor_import_ok = 1 AND @harbor_duplicate = 0, 1, 0), @harbor_skipped = @harbor_skipped + IF(@harbor_import_ok = 1 AND @harbor_duplicate = 1, 1, 0)".to_owned();
            if track {
                count.push_str(", @harbor_row_status = IF(@harbor_import_ok = 1, CONCAT(@harbor_row_status, IF(@harbor_duplicate = 1, '0', '1')), @harbor_row_status)");
            }
            self.push(count)?;
        } else {
            self.push(write)?;
            self.push(
                "SET @harbor_affected = ROW_COUNT(), @harbor_warnings = @@warning_count".into(),
            )?;
            let affected = if conflict == OdbcConflict::Replace {
                ">= 1"
            } else {
                "= 1"
            };
            self.push(format!("SET @harbor_import_ok = (@harbor_import_ok = 1 AND @harbor_affected {affected} AND @harbor_warnings = 0)"))?;
            let mut count =
                "SET @harbor_rows = @harbor_rows + IF(@harbor_import_ok = 1, 1, 0)".to_owned();
            if track {
                count.push_str(", @harbor_row_status = IF(@harbor_import_ok = 1, CONCAT(@harbor_row_status, '1'), @harbor_row_status)");
            }
            self.push(count)?;
            if conflict == OdbcConflict::Replace {
                self.push("SET @harbor_replaced = @harbor_replaced + IF(@harbor_import_ok = 1, @harbor_affected - 1, 0)".into())?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::super::super::http_session::workflow_test::{
        TestGateway, archived_tunnel, fixture_database,
    };
    use super::super::super::{Command, ConnectionConfig, DbObject, ObjectKind, Response, Worker};
    use super::super::super::{OdbcForeignKey, OdbcReferentialAction};
    use super::*;

    #[test]
    #[ignore = "requires a disposable MariaDB via HARBOR_HTTP_FIXTURE_PORT"]
    fn reports_applied_odbc_rows_in_input_order_across_native_and_http() {
        let (port, opts) = fixture_database();
        let mut sql = Conn::new(opts.clone()).unwrap();
        let schema = format!("harbor_odbc_receipts_{}", std::process::id());
        sql.query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
            .unwrap();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sql.select_db(&schema).unwrap();
            let gateway = TestGateway::new(opts);
            let config = ConnectionConfig {
                port,
                ..Default::default()
            };
            let mut transports = vec![
                Command::Connect(config.clone()),
                Command::ConnectHttp(config.clone(), gateway.tunnel.clone()),
            ];
            transports.extend(archived_tunnel().map(|tunnel| Command::ConnectHttp(config, tunnel)));
            for (index, connect) in transports.into_iter().enumerate() {
                verify_applied_rows(&mut sql, &schema, index, connect);
            }
            gateway.stop();
        }));
        sql.query_drop(format!("DROP DATABASE IF EXISTS {schema}"))
            .unwrap();
        if let Err(error) = outcome {
            std::panic::resume_unwind(error);
        }
    }

    fn verify_applied_rows(sql: &mut Conn, schema: &str, index: usize, connect: Command) {
        let keep = format!("keep_{index}");
        let auxiliary = format!("aux_{index}");
        let replace = format!("replace_{index}");
        let empty = format!("empty_{index}");
        let staged = format!("new_rows_{index}");
        for table in [&keep, &auxiliary, &replace, &empty] {
            sql.query_drop(format!(
                "CREATE TABLE {table} (id INT PRIMARY KEY, body VARCHAR(30)) ENGINE=InnoDB"
            ))
            .unwrap();
        }
        sql.query_drop(format!(
            "INSERT INTO {keep} VALUES (10,'original'),(30,'original')"
        ))
        .unwrap();
        sql.query_drop(format!("INSERT INTO {auxiliary} VALUES (55,'original')"))
            .unwrap();
        sql.query_drop(format!("INSERT INTO {replace} VALUES (101,'original')"))
            .unwrap();

        let target = |table: &str, ids: &[u16], conflict, track_applied_rows| OdbcTarget {
            object: DbObject {
                schema: schema.into(),
                name: table.into(),
                kind: ObjectKind::Table,
            },
            columns: vec!["id".into(), "body".into()],
            rows: ids
                .iter()
                .map(|id| vec![Cell::Text(id.to_string()), Cell::Text("source".into())])
                .collect(),
            create: None,
            create_sql: None,
            engine: None,
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            conflict,
            reset: OdbcReset::None,
            track_applied_rows,
            expected_ddl: None,
        };
        let worker = Worker::new();
        let request = |id, command| {
            worker.send(id, command).unwrap();
            let event = worker
                .recv_timeout(Duration::from_secs(25))
                .unwrap()
                .unwrap();
            assert_eq!(event.id, id);
            event
                .result
                .unwrap_or_else(|error| panic!("transport {index}: {error}"))
        };
        let import = |tables| Command::ImportOdbcTables {
            tables,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        assert!(matches!(request(1, connect), Response::Connected(_)));
        let ids = [10, 11, 30, 31];
        let Response::OdbcImported { counts, .. } = request(
            2,
            import(vec![
                target(&keep, &ids, OdbcConflict::KeepTarget, true),
                target(&auxiliary, &[55, 56], OdbcConflict::KeepTarget, false),
                target(&replace, &[101, 111], OdbcConflict::Replace, true),
                target(&empty, &[], OdbcConflict::Abort, true),
            ]),
        ) else {
            panic!("expected confirmed append receipts")
        };
        assert_eq!((counts.rows, counts.skipped, counts.replaced), (5, 3, 1));
        assert_eq!(
            counts.applied_rows,
            [
                Some(vec![false, true, false, true]),
                None,
                Some(vec![true, true]),
                Some(Vec::new())
            ]
        );
        let mut applied_ids = Vec::new();
        let mut skipped_ids = Vec::new();
        for (source_id, &applied) in ids
            .into_iter()
            .zip(counts.applied_rows[0].as_ref().unwrap())
        {
            if applied {
                applied_ids.push(source_id);
            } else {
                skipped_ids.push(source_id);
            }
        }
        assert_eq!(applied_ids, [11, 31]);
        assert_eq!(skipped_ids, [10, 30]);
        assert_eq!(
            sql.query::<u16, _>(format!(
                "SELECT id FROM {keep} WHERE body='source' ORDER BY id"
            ))
            .unwrap(),
            applied_ids
        );
        assert_eq!(
            sql.query::<u16, _>(format!(
                "SELECT id FROM {keep} WHERE body='original' ORDER BY id"
            ))
            .unwrap(),
            skipped_ids
        );

        let mut new_table = target(&staged, &[1, 1, 2], OdbcConflict::KeepTarget, true);
        new_table.create = Some(vec![
            OdbcColumnDefinition {
                sql_type: "INT".into(),
                primary: true,
                ..Default::default()
            },
            OdbcColumnDefinition {
                sql_type: "VARCHAR(30)".into(),
                ..Default::default()
            },
        ]);
        let Response::OdbcImported { counts, .. } = request(3, import(vec![new_table])) else {
            panic!("expected receipts after staged publication");
        };
        assert_eq!((counts.rows, counts.skipped), (2, 1));
        assert_eq!(counts.applied_rows, [Some(vec![true, false, true])]);
        assert_eq!(
            sql.query::<u16, _>(format!("SELECT id FROM {staged} ORDER BY id"))
                .unwrap(),
            [1, 2]
        );
    }

    #[test]
    #[ignore = "requires a disposable MariaDB via HARBOR_HTTP_FIXTURE_PORT"]
    fn imports_odbc_foreign_keys_across_native_and_http() {
        let (port, opts) = fixture_database();
        let mut sql = Conn::new(opts.clone()).unwrap();
        let schema = format!("harbor_odbc_fk_{}", std::process::id());
        sql.query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
            .unwrap();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sql.select_db(&schema).unwrap();
            let gateway = TestGateway::new(opts);
            let config = ConnectionConfig {
                port,
                ..Default::default()
            };
            let mut transports = vec![
                Command::Connect(config.clone()),
                Command::ConnectHttp(config.clone(), gateway.tunnel.clone()),
            ];
            transports.extend(archived_tunnel().map(|tunnel| Command::ConnectHttp(config, tunnel)));
            for (index, connect) in transports.into_iter().enumerate() {
                verify_foreign_key_import(&mut sql, &schema, index, connect);
            }
            gateway.stop();
        }));
        sql.query_drop(format!("DROP DATABASE IF EXISTS {schema}"))
            .unwrap();
        if let Err(error) = outcome {
            std::panic::resume_unwind(error);
        }
    }

    fn verify_foreign_key_import(sql: &mut Conn, schema: &str, index: usize, connect: Command) {
        use OdbcReferentialAction::{Cascade, Restrict, SetNull};

        let parent = format!("parent_{index}");
        let child = format!("child_{index}");
        let existing_parent = format!("unrelated_parent_{index}");
        let existing_child = format!("unrelated_child_{index}");
        let collision = format!("shared_symbol_{index}");
        let composite = format!("composite_fk_{index}");
        for statement in [
            format!("CREATE TABLE {existing_parent} (id INT PRIMARY KEY) ENGINE=InnoDB"),
            format!(
                "CREATE TABLE {existing_child} (id INT, CONSTRAINT {collision} FOREIGN KEY (id) REFERENCES {existing_parent}(id)) ENGINE=InnoDB"
            ),
            format!("CREATE TABLE {parent} (payload INT) ENGINE=InnoDB"),
            format!("INSERT INTO {parent} VALUES (99)"),
        ] {
            sql.query_drop(statement).unwrap();
        }

        let target =
            |name: &str, columns: [&str; 3], primary: [bool; 3], row: [u8; 3], foreign_keys| {
                OdbcTarget {
                    object: DbObject {
                        schema: schema.into(),
                        name: name.into(),
                        kind: ObjectKind::Table,
                    },
                    columns: columns.map(str::to_owned).into(),
                    rows: vec![row.map(|value| Cell::Text(value.to_string())).into()],
                    create_sql: None,
                    engine: None,
                    create: Some(
                        primary
                            .map(|primary| OdbcColumnDefinition {
                                sql_type: "INT".into(),
                                primary,
                                ..Default::default()
                            })
                            .into(),
                    ),
                    indexes: Vec::new(),
                    foreign_keys,
                    conflict: OdbcConflict::Abort,
                    reset: OdbcReset::Recreate,
                    track_applied_rows: false,
                    expected_ddl: None,
                }
            };
        let tables = vec![
            target(
                &child,
                ["line_id", "buyer_region", "buyer_id"],
                [true, false, false],
                [1, 2, 3],
                vec![
                    OdbcForeignKey {
                        name: Some(composite.clone()),
                        columns: vec!["BUYER_REGION".into(), "buyer_id".into()],
                        referenced_table: parent.clone(),
                        referenced_columns: vec!["region_code".into(), "customer_id".into()],
                        on_update: Cascade,
                        on_delete: SetNull,
                    },
                    OdbcForeignKey {
                        name: None,
                        columns: vec!["line_id".into()],
                        referenced_table: child.clone(),
                        referenced_columns: vec!["line_id".into()],
                        on_update: Restrict,
                        on_delete: Restrict,
                    },
                ],
            ),
            target(
                &parent,
                ["region_code", "customer_id", "sample_line"],
                [true, true, false],
                [2, 3, 1],
                vec![OdbcForeignKey {
                    name: Some(collision.clone()),
                    columns: vec!["sample_line".into()],
                    referenced_table: child.clone(),
                    referenced_columns: vec!["line_id".into()],
                    on_update: Restrict,
                    on_delete: SetNull,
                }],
            ),
        ];
        let worker = Worker::new();
        let request = |id, command| {
            worker.send(id, command).unwrap();
            let event = worker
                .recv_timeout(Duration::from_secs(25))
                .unwrap()
                .unwrap();
            assert_eq!(event.id, id);
            event
                .result
                .unwrap_or_else(|error| panic!("transport {index}: {error}"))
        };
        assert!(matches!(request(1, connect), Response::Connected(_)));
        let Response::OdbcImported { counts, .. } = request(
            2,
            Command::ImportOdbcTables {
                tables,
                cancel: Arc::new(AtomicBool::new(false)),
            },
        ) else {
            panic!("expected published ODBC foreign keys")
        };
        assert_eq!(counts.rows, 2);
        assert_eq!(counts.warnings.len(), 1, "{:?}", counts.warnings);
        assert!(counts.warnings[0].contains(&collision));

        let constraints: Vec<(String, String, String, String)> = sql
            .query(format!(
                "SELECT CONSTRAINT_NAME, TABLE_NAME, COLUMN_NAME, REFERENCED_COLUMN_NAME
             FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA='{schema}'
             AND REFERENCED_TABLE_SCHEMA='{schema}' AND CONSTRAINT_NAME='{composite}'
             AND REFERENCED_TABLE_NAME='{parent}' ORDER BY ORDINAL_POSITION"
            ))
            .unwrap();
        assert_eq!(
            constraints,
            [
                (
                    composite.clone(),
                    child.clone(),
                    "buyer_region".into(),
                    "region_code".into()
                ),
                (
                    composite,
                    child.clone(),
                    "buyer_id".into(),
                    "customer_id".into()
                ),
            ]
        );
        let generated: Vec<String> = sql
            .query(format!(
                "SELECT CONSTRAINT_NAME FROM information_schema.REFERENTIAL_CONSTRAINTS
             WHERE CONSTRAINT_SCHEMA='{schema}' AND (TABLE_NAME='{parent}' OR
                 (TABLE_NAME='{child}' AND REFERENCED_TABLE_NAME='{child}'))"
            ))
            .unwrap();
        assert_eq!(generated.len(), 2);
        assert!(
            generated
                .iter()
                .all(|name| name.starts_with("_harbor_odbc_fk_"))
        );
        assert_eq!(sql.query_first::<String, _>(format!("SELECT CONSTRAINT_NAME FROM information_schema.REFERENTIAL_CONSTRAINTS WHERE CONSTRAINT_SCHEMA='{schema}' AND TABLE_NAME='{existing_child}'")).unwrap(), Some(collision));

        sql.query_drop(format!(
            "UPDATE {parent} SET region_code=4 WHERE customer_id=3"
        ))
        .unwrap();
        let child_keys = |sql: &mut Conn| {
            sql.query_first::<(Option<u8>, Option<u8>), _>(format!(
                "SELECT buyer_region,buyer_id FROM {child}"
            ))
            .unwrap()
        };
        assert_eq!(child_keys(sql), Some((Some(4), Some(3))));
        sql.query_drop(format!("DELETE FROM {parent} WHERE customer_id=3"))
            .unwrap();
        assert_eq!(child_keys(sql), Some((None, None)));
        assert_eq!(sql.query_first::<u64, _>(format!("SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA='{schema}' AND LEFT(TABLE_NAME,13)='_harbor_odbc_'")).unwrap(), Some(0));
    }

    #[test]
    #[ignore = "requires a disposable MariaDB via HARBOR_HTTP_FIXTURE_PORT"]
    fn imports_odbc_reset_modes() {
        let (port, opts) = fixture_database();
        let mut sql = Conn::new(opts.clone()).unwrap();
        let schema = format!("harbor_http_odbc_reset_{}", std::process::id());
        sql.query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
            .unwrap();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let gateway = TestGateway::new(opts);
            let config = ConnectionConfig {
                port,
                ..Default::default()
            };
            let mut transports = vec![
                Command::Connect(config.clone()),
                Command::ConnectHttp(config.clone(), gateway.tunnel.clone()),
            ];
            transports.extend(archived_tunnel().map(|tunnel| Command::ConnectHttp(config, tunnel)));
            sql.select_db(&schema).unwrap();
            for (index, connect) in transports.into_iter().enumerate() {
                let worker = Worker::new();
                let request = |id, command| {
                    worker.send(id, command).unwrap();
                    let event = worker
                        .recv_timeout(Duration::from_secs(25))
                        .unwrap()
                        .unwrap();
                    assert_eq!(event.id, id);
                    event
                        .result
                        .unwrap_or_else(|error| panic!("transport {index}: {error}"))
                };
                assert!(matches!(request(1, connect), Response::Connected(_)));
                let name = format!("target_{index}");
                let missing = format!("new_target_{index}");
                let live = DbObject {
                    schema: schema.clone(),
                    name: name.clone(),
                    kind: ObjectKind::Table,
                };
                sql.query_drop(format!("CREATE TABLE {name} (id INT AUTO_INCREMENT PRIMARY KEY, body VARBINARY(20) NOT NULL, tag INT NOT NULL DEFAULT 19, INDEX idx_body(body)) ENGINE=InnoDB")).unwrap();
                sql.query_drop(format!("INSERT INTO {name} (id,body) VALUES (99,X'FF')"))
                    .unwrap();
                let read_ddl = |conn: &mut Conn| {
                    conn.query_first::<(String, String), _>(format!("SHOW CREATE TABLE {name}"))
                        .unwrap()
                        .unwrap()
                        .1
                };
                let original = read_ddl(&mut sql);
                let import = |tables| Command::ImportOdbcTables {
                    tables,
                    cancel: Arc::new(AtomicBool::new(false)),
                };
                let Response::OdbcImported { counts, .. } = request(
                    2,
                    import(vec![OdbcTarget {
                        object: live.clone(),
                        columns: vec!["body".into()],
                        rows: vec![vec![Cell::Binary(vec![0, 255])]],
                        create: None,
                        create_sql: None,
                        engine: None,
                        indexes: Vec::new(),
                        foreign_keys: Vec::new(),
                        conflict: OdbcConflict::Abort,
                        reset: OdbcReset::Truncate,
                        track_applied_rows: false,
                        expected_ddl: Some(original.clone()),
                    }]),
                ) else {
                    panic!("expected a published truncate")
                };
                assert_eq!(counts.rows, 1);
                assert!(counts.warnings.is_empty(), "{:?}", counts.warnings);
                assert_eq!(
                    sql.query::<(u8, Vec<u8>, u8), _>(format!("SELECT id,body,tag FROM {name}"))
                        .unwrap(),
                    [(1, vec![0, 255], 19)]
                );
                let truncated = read_ddl(&mut sql);
                assert_eq!(
                    reset_definition_body(truncated.clone(), &live).unwrap(),
                    reset_definition_body(original, &live).unwrap()
                );

                let recreated = |object, rows, expected_ddl| OdbcTarget {
                    object,
                    columns: vec!["message".into()],
                    rows,
                    create_sql: None,
                    engine: None,
                    create: Some(vec![OdbcColumnDefinition {
                        sql_type: "VARCHAR(60)".into(),
                        ..Default::default()
                    }]),
                    indexes: Vec::new(),
                    foreign_keys: Vec::new(),
                    conflict: OdbcConflict::Abort,
                    reset: OdbcReset::Recreate,
                    track_applied_rows: false,
                    expected_ddl,
                };
                let Response::OdbcImported { counts, .. } = request(
                    3,
                    import(vec![
                        recreated(
                            live.clone(),
                            vec![vec![Cell::Text("重建成功".into())]],
                            Some(truncated),
                        ),
                        recreated(
                            DbObject {
                                name: missing.clone(),
                                ..live
                            },
                            Vec::new(),
                            None,
                        ),
                    ]),
                ) else {
                    panic!("expected published recreated tables")
                };
                assert_eq!(counts.rows, 1);
                assert!(counts.warnings.is_empty(), "{:?}", counts.warnings);
                assert_eq!(
                    sql.query::<String, _>(format!("SELECT message FROM {name}"))
                        .unwrap(),
                    ["重建成功"]
                );
                assert_eq!(
                    sql.query::<String, _>(format!("SELECT message FROM {missing}"))
                        .unwrap(),
                    Vec::<String>::new()
                );
                assert_eq!(sql.query_first::<u64, _>(format!("SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA='{schema}' AND LEFT(TABLE_NAME,13)='_harbor_odbc_'")).unwrap(), Some(0));
            }
            gateway.stop();
        }));
        sql.query_drop(format!("DROP DATABASE IF EXISTS {schema}"))
            .unwrap();
        if let Err(error) = outcome {
            std::panic::resume_unwind(error);
        }
    }

    #[test]
    #[ignore = "requires a disposable MariaDB via HARBOR_HTTP_FIXTURE_PORT"]
    fn imports_odbc_tables_over_http_with_create_preserve_and_replace() {
        let (port, opts) = fixture_database();
        let mut sql = Conn::new(opts.clone()).unwrap();
        let schema = format!("harbor_http_odbc_{}", std::process::id());
        sql.query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
            .unwrap();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let gateway = TestGateway::new(opts);
            let worker = Worker::new();
            let request = |id, command| {
                worker.send(id, command).unwrap();
                let event = worker
                    .recv_timeout(Duration::from_secs(15))
                    .unwrap()
                    .unwrap();
                assert_eq!(event.id, id);
                event.result.unwrap()
            };
            assert!(matches!(
                request(
                    1,
                    Command::ConnectHttp(
                        ConnectionConfig {
                            port,
                            ..Default::default()
                        },
                        gateway.tunnel.clone()
                    )
                ),
                Response::Connected(_)
            ));
            let table = |name: &str, create_type: Option<&str>, conflict, values: &[(u8, Cell)]| {
                OdbcTarget {
                    object: DbObject {
                        schema: schema.clone(),
                        name: name.into(),
                        kind: ObjectKind::Table,
                    },
                    columns: vec!["id".into(), "body".into()],
                    rows: values
                        .iter()
                        .map(|(id, body)| vec![Cell::Text(id.to_string()), body.clone()])
                        .collect(),
                    create: create_type.map(|sql_type| {
                        vec![
                            OdbcColumnDefinition {
                                sql_type: "INT".into(),
                                primary: true,
                                ..Default::default()
                            },
                            OdbcColumnDefinition {
                                sql_type: sql_type.into(),
                                ..Default::default()
                            },
                        ]
                    }),
                    create_sql: None,
                    engine: None,
                    indexes: Vec::new(),
                    foreign_keys: Vec::new(),
                    conflict,
                    reset: OdbcReset::None,
                    track_applied_rows: false,
                    expected_ddl: None,
                }
            };
            let import = |tables| Command::ImportOdbcTables {
                tables,
                cancel: Arc::new(AtomicBool::new(false)),
            };
            let long_blob = [0, 255, 7].repeat(3000);
            let Response::OdbcImported { counts, created_in } = request(
                2,
                import(vec![
                    table(
                        "first",
                        Some("VARCHAR(40)"),
                        OdbcConflict::Abort,
                        &[(1, Cell::Text("原值;中文".into()))],
                    ),
                    table(
                        "second",
                        Some("BLOB"),
                        OdbcConflict::Abort,
                        &[
                            (2, Cell::Null),
                            (4, Cell::Null),
                            (5, Cell::Binary(Vec::new())),
                            (6, Cell::Binary(long_blob.clone())),
                        ],
                    ),
                ]),
            ) else {
                panic!("expected created ODBC tables");
            };
            assert_eq!(counts.rows, 5);
            assert!(counts.applied_rows.is_empty());
            assert_eq!(created_in.as_deref(), Some(schema.as_str()));
            sql.select_db(&schema).unwrap();
            sql.query_drop("CREATE TABLE audit (id INT) ENGINE=InnoDB")
                .unwrap();
            sql.query_drop("CREATE TRIGGER audit_before BEFORE INSERT ON first FOR EACH ROW INSERT INTO audit VALUES (NEW.id)").unwrap();
            let Response::OdbcImported { counts, .. } = request(
                3,
                import(vec![table(
                    "first",
                    None,
                    OdbcConflict::KeepTarget,
                    &[
                        (1, Cell::Text("重复源行".into())),
                        (3, Cell::Text("新行".into())),
                    ],
                )]),
            ) else {
                panic!("expected kept target row");
            };
            assert_eq!((counts.rows, counts.skipped), (1, 1));
            assert_eq!(sql.query::<u8, _>("SELECT id FROM audit").unwrap(), [3]);
            let Response::OdbcImported { counts, .. } = request(
                4,
                import(vec![table(
                    "second",
                    None,
                    OdbcConflict::Replace,
                    &[(2, Cell::Binary(vec![0, 255]))],
                )]),
            ) else {
                panic!("expected a replaced row");
            };
            assert_eq!((counts.rows, counts.replaced), (1, 1));
            assert_eq!(
                sql.query::<(u8, String), _>("SELECT id,body FROM first ORDER BY id")
                    .unwrap(),
                [(1, String::from("原值;中文")), (3, String::from("新行")),]
            );
            assert_eq!(
                sql.query::<(u8, Option<Vec<u8>>), _>("SELECT id, body FROM second ORDER BY id")
                    .unwrap(),
                [
                    (2, Some(vec![0, 255])),
                    (4, None),
                    (5, Some(Vec::new())),
                    (6, Some(long_blob))
                ]
            );
            assert_eq!(sql.query_first::<u64,_>(format!("SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA='{schema}' AND TABLE_NAME LIKE '_harbor_odbc_%'")).unwrap(), Some(0));
            gateway.stop();
        }));
        sql.query_drop(format!("DROP DATABASE IF EXISTS {schema}"))
            .unwrap();
        if let Err(error) = outcome {
            std::panic::resume_unwind(error);
        }
    }
}
