use std::collections::BTreeMap;

use crate::sql_tools::mysql_text_literal;

use super::sync_engine::stable_definition;
use super::*;

pub(super) const CANCELLED: &str = "已停止结构同步。";
const CONFIRMATION_COLUMN: &str = "__harbor_schema_change";
const SERVER_VARIABLES: &str = "SHOW GLOBAL VARIABLES WHERE Variable_name IN ('hostname','port','version','server_id','server_uuid','server_uid')";

#[derive(Clone, Debug)]
pub struct SchemaOrigin {
    session: Weak<()>,
    server: BTreeMap<String, String>,
    default_database: Option<String>,
}

impl SchemaOrigin {
    pub(super) fn same_server(&self, other: &Self) -> bool {
        self.server == other.server
    }

    pub(super) fn matches_identity(&self, other: &Self) -> bool {
        self.same_server(other)
            || ["server_uuid", "server_uid"].iter().any(|name| {
                self.server
                    .get(*name)
                    .filter(|value| !value.is_empty())
                    .is_some_and(|value| other.server.get(*name) == Some(value))
            })
    }

    pub(super) fn native(conn: &mut Conn, identity: &Arc<()>) -> Result<Self, String> {
        let variables = conn
            .query::<(String, String), _>(SERVER_VARIABLES)
            .map_err(mysql_error)?;
        let default_database = conn
            .query_first::<Option<String>, _>("SELECT DATABASE()")
            .map_err(mysql_error)?
            .flatten();
        Self::new(identity, variables, default_database)
    }

    pub(super) fn http(active: &http_session::Session) -> Result<Self, String> {
        let rows = active.read_set(SERVER_VARIABLES)?.rows;
        let variables = rows
            .iter()
            .map(|row| {
                let name = http_session::text(row.first()).ok_or("隧道未返回服务器标识名称。")?;
                let value = http_session::text(row.get(1)).ok_or("隧道未返回服务器标识值。")?;
                Ok((name.to_owned(), value.to_owned()))
            })
            .collect::<Result<Vec<_>, &str>>()?;
        Self::new(
            &active.identity,
            variables,
            active.database_name().map(str::to_owned),
        )
    }

    fn new(
        identity: &Arc<()>,
        variables: Vec<(String, String)>,
        default_database: Option<String>,
    ) -> Result<Self, String> {
        let server = variables
            .into_iter()
            .filter_map(|(name, value)| {
                let name = name.to_ascii_lowercase();
                matches!(
                    name.as_str(),
                    "hostname" | "port" | "version" | "server_id" | "server_uuid" | "server_uid"
                )
                .then_some((name, value))
            })
            .collect::<BTreeMap<_, _>>();
        if ["hostname", "port", "version", "server_id"]
            .iter()
            .any(|key| server.get(*key).is_none_or(String::is_empty))
        {
            return Err("服务器标识信息不完整。".into());
        }
        Ok(Self {
            session: Arc::downgrade(identity),
            server,
            default_database,
        })
    }

    pub(super) fn guard(&self) -> String {
        let condition = self
            .server
            .iter()
            .map(|(name, value)| {
                format!(
                    "CAST(@@GLOBAL.{name} AS BINARY) = {}",
                    mysql_text_literal(value)
                )
            })
            .collect::<Vec<_>>()
            .join(" AND ");
        format!(
            "SET @harbor_schema_target = IF({condition}, 'DO 0', 'HARBOR_SCHEMA_TARGET_CHANGED')"
        )
    }
}

#[derive(Clone)]
pub struct SchemaChange {
    pub sql: String,
    pub mode: Option<String>,
    pub charset: Option<String>,
    pub collation: Option<String>,
    pub time_zone: Option<String>,
}

pub struct SchemaApply {
    pub expected: SchemaSnapshot,
    pub changes: Vec<SchemaChange>,
}

#[derive(Clone, Copy)]
pub struct SchemaApplyProgress {
    pub completed: usize,
    pub total: usize,
    pub validating: bool,
}

pub struct SchemaApplyResult {
    pub schema: String,
    pub completed: usize,
    pub total: usize,
    pub stopped: Option<SchemaApplyStop>,
    pub objects: Option<Vec<DbObject>>,
}

pub enum SchemaApplyStop {
    Cancelled,
    Failed {
        sql: String,
        error: String,
        uncertain: bool,
    },
}

pub(super) fn apply(
    active: &mut Connection,
    request: SchemaApply,
    monitor: Option<&Mutex<OperationMonitor>>,
) -> Result<SchemaApplyResult, String> {
    let monitor = monitor.ok_or("结构同步任务未初始化。")?;
    let SchemaApply { expected, changes } = request;
    if changes.is_empty() {
        return Err("所选对象没有需要执行的结构变更。".into());
    }
    let identity = match &*active {
        Connection::Native(session) => &session.identity,
        Connection::Http(session) => &session.identity,
    };
    let origin = expected
        .origin
        .as_ref()
        .ok_or("未能取得比较时的目标服务器标识，请重新比较。")?;
    if !Weak::ptr_eq(&origin.session, &Arc::downgrade(identity)) {
        return Err("目标连接已变化，请重新比较后再执行。".into());
    }
    let context = expected
        .context
        .as_ref()
        .ok_or("无法确认目标会话的 SQL 模式及字符集，请重新比较。")?;
    let total = changes.len();
    update(monitor, total, 0, true)?;
    let schema = expected.schema.clone();
    match active {
        Connection::Native(active) => {
            let mut conn = Conn::new(active.opts.clone())
                .map_err(|error| format!("无法建立独立的结构同步会话：{}", mysql_error(error)))?;
            if SchemaOrigin::native(&mut conn, &active.identity)?.server != origin.server {
                return Err("独立会话连接的服务器与比较时不同，请重新比较。".into());
            }
            if let Some(database) = &origin.default_database {
                // SHOW CREATE VIEW can qualify names differently according to DATABASE().
                conn.select_db(database).map_err(mysql_error)?;
            }
            let baseline = settings_sql(context, None)?;
            conn.query_drop(&baseline).map_err(mysql_error)?;
            let current = snapshot_schema(&mut conn, schema.clone()).map_err(Failure::message)?;
            verify(&expected, &current)?;
            conn.select_db(&schema).map_err(mysql_error)?;
            let steps = changes
                .into_iter()
                .map(|change| Ok((settings_sql(context, Some(&change))?, change.sql)))
                .collect::<Result<Vec<_>, String>>()?;
            let mut result = run(schema, steps, monitor, |setup, sql| {
                conn.query_drop(setup)
                    .map_err(|error| (mysql_error(error), false))?;
                crate::restore::run(&mut conn, sql).map_err(|error| {
                    let uncertain = error.is_connectivity_error();
                    (mysql_error(error), uncertain)
                })
            });
            result.objects = list_objects(&mut conn, &result.schema).ok();
            Ok(result)
        }
        Connection::Http(active) => {
            let Response::Snapshot(current) = active.handle(Command::SnapshotSchema {
                schema: schema.clone(),
            })?
            else {
                return Err("目标端未返回结构快照，请重新比较。".into());
            };
            if current
                .origin
                .as_ref()
                .is_none_or(|current| current.server != origin.server)
            {
                return Err("无法确认隧道当前连接的是比较时的服务器，请重新比较。".into());
            }
            verify(&expected, &current)?;
            let select = format!("USE {}", quote_identifier(&schema));
            let marker = format!("SELECT 1 AS {CONFIRMATION_COLUMN}");
            let guard = origin.guard();
            let steps = changes
                .into_iter()
                .map(|change| {
                    let setup = settings_sql(context, Some(&change))?;
                    let batch = http_session::batch_statements(&[
                        &guard,
                        "PREPARE harbor_schema_target FROM @harbor_schema_target",
                        "DEALLOCATE PREPARE harbor_schema_target",
                        &select,
                        &setup,
                        &change.sql,
                        &marker,
                    ])?;
                    let payload = active.prepare_payload(&batch, true, &schema)?;
                    Ok((payload, change.sql))
                })
                .collect::<Result<Vec<_>, String>>()?;
            let mut result = run(schema, steps, monitor, |payload, _| {
                let reply = active
                    .execute_payload(payload, 1)
                    .map_err(|error| (error, true))?;
                if let Some(error) = reply.error {
                    if error.message.contains("HARBOR_SCHEMA_TARGET_CHANGED") {
                        return Err(("隧道连接的目标服务器与比较时不同。".into(), false));
                    }
                    return Err((format!("ERROR {}: {}", error.code, error.message), false));
                }
                let confirmed = reply.sets.last().is_some_and(|set| {
                    set.columns.len() == 1
                        && set.columns[0].name == CONFIRMATION_COLUMN
                        && set
                            .rows
                            .first()
                            .and_then(|row| row.first())
                            .and_then(Option::as_deref)
                            == Some(b"1".as_slice())
                });
                if !confirmed {
                    return Err(("隧道未返回结构变更的完成标记。".into(), true));
                }
                Ok(())
            });
            result.objects = active.objects(&result.schema).ok();
            Ok(result)
        }
    }
}

fn settings_sql(
    baseline: &SnapshotContext,
    change: Option<&SchemaChange>,
) -> Result<String, String> {
    let mode = change
        .and_then(|change| change.mode.as_deref())
        .unwrap_or(&baseline.sql_mode);
    let charset = change
        .and_then(|change| change.charset.as_deref())
        .unwrap_or(&baseline.character_set_client);
    if !matches!(charset, "utf8" | "utf8mb3" | "utf8mb4") {
        return Err("当前会话的客户端字符集不是 UTF-8，请切换字符集后重新比较。".into());
    }
    let collation = change
        .and_then(|change| change.collation.as_deref())
        .unwrap_or(&baseline.collation_connection);
    let time_zone = change
        .and_then(|change| change.time_zone.as_deref())
        .map_or_else(|| "DEFAULT".to_owned(), mysql_text_literal);
    Ok(format!(
        "SET SESSION sql_mode = {}, SESSION character_set_client = {}, SESSION collation_connection = {}, SESSION time_zone = {time_zone}, SESSION foreign_key_checks = 1",
        mysql_text_literal(mode),
        mysql_text_literal(charset),
        mysql_text_literal(collation),
    ))
}

fn verify(expected: &SchemaSnapshot, current: &SchemaSnapshot) -> Result<(), String> {
    let changed = || "目标库的结构已变化，请重新比较后再执行。".to_owned();
    if expected.schema != current.schema
        || expected.defaults != current.defaults
        || expected.context != current.context
        || expected.foreign_keys != current.foreign_keys
        || expected.omitted_objects != current.omitted_objects
        || expected.objects.len() != current.objects.len()
    {
        return Err(changed());
    }
    for (previous, fresh) in expected.objects.iter().zip(&current.objects) {
        let definitions_match = match (&previous.definition, &fresh.definition) {
            (Ok(left), Ok(right)) if previous.object.kind == ObjectKind::Table && left != right => {
                stable_definition(left.clone()) == stable_definition(right.clone())
            }
            (left, right) => left == right,
        };
        if previous.object != fresh.object
            || previous.context != fresh.context
            || !definitions_match
        {
            return Err(format!(
                "目标对象 {} 的结构已变化，请重新比较后再执行。",
                previous.object.qualified_name(),
            ));
        }
    }
    Ok(())
}

fn update(
    monitor: &Mutex<OperationMonitor>,
    total: usize,
    completed: usize,
    validating: bool,
) -> Result<(), String> {
    monitor.lock().unwrap().update(
        ProgressUpdate::SchemaApply(SchemaApplyProgress {
            completed,
            total,
            validating,
        }),
        false,
    )
}

fn run<T>(
    schema: String,
    steps: Vec<(T, String)>,
    monitor: &Mutex<OperationMonitor>,
    mut submit: impl FnMut(T, &str) -> Result<(), (String, bool)>,
) -> SchemaApplyResult {
    let total = steps.len();
    let mut result = SchemaApplyResult {
        schema,
        completed: 0,
        total,
        stopped: None,
        objects: None,
    };
    for (prepared, sql) in steps {
        if update(monitor, total, result.completed, false).is_err() {
            result.stopped = Some(SchemaApplyStop::Cancelled);
            break;
        }
        if let Err((error, uncertain)) = submit(prepared, &sql) {
            result.stopped = Some(SchemaApplyStop::Failed {
                sql,
                error,
                uncertain,
            });
            break;
        }
        result.completed += 1;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::super::http_session::workflow_test::{TestGateway, fixture_database};
    use super::*;

    #[test]
    #[ignore = "requires a disposable MariaDB via HARBOR_HTTP_FIXTURE_PORT"]
    fn applies_reviewed_schema_changes_over_http() {
        let (port, opts) = fixture_database();
        let mut sql = Conn::new(opts.clone()).unwrap();
        let schema = format!("harbor_http_structure_{}", std::process::id());
        sql.query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
            .unwrap();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let gateway = TestGateway::new(opts);
            let worker = Worker::new();
            let mut id = 0;
            let mut request = |command| {
                id += 1;
                worker.send(id, command).unwrap();
                let event = worker
                    .recv_timeout(Duration::from_secs(15))
                    .unwrap()
                    .unwrap();
                assert_eq!(event.id, id);
                event.result.unwrap()
            };
            assert!(matches!(
                request(Command::ConnectHttp(
                    ConnectionConfig {
                        port,
                        ..Default::default()
                    },
                    gateway.tunnel.clone(),
                )),
                Response::Connected(_)
            ));
            let Response::Snapshot(expected) = request(Command::SnapshotSchema {
                schema: schema.clone(),
            }) else {
                panic!("missing target snapshot");
            };
            let changes = [
                format!("CREATE TABLE `{schema}`.`同步` (id INT PRIMARY KEY) ENGINE=InnoDB"),
                format!("CREATE PROCEDURE `{schema}`.lookup_id() BEGIN SELECT COUNT(*) FROM `同步`; SELECT 2; END"),
            ].into_iter().map(|sql| SchemaChange {
                sql,
                mode: Some("STRICT_TRANS_TABLES".into()),
                charset: None,
                collation: None,
                time_zone: None,
            }).collect();
            let Response::SchemaApplied(result) = request(Command::ApplySchema {
                apply: Box::new(SchemaApply { expected, changes }),
            }) else {
                panic!("missing apply result")
            };
            if let Some(SchemaApplyStop::Failed { error, sql, .. }) = result.stopped {
                panic!("{sql}\n{error}");
            }
            assert_eq!(result.completed, 2);
            assert_eq!(result.objects.unwrap().len(), 2);
            assert_eq!(
                sql.query_first::<u64, _>(format!("CALL `{schema}`.lookup_id()"))
                    .unwrap(),
                Some(0)
            );
            gateway.stop();
        }));
        sql.query_drop(format!("DROP DATABASE IF EXISTS {schema}"))
            .unwrap();
        if let Err(error) = outcome {
            std::panic::resume_unwind(error);
        }
    }
}
