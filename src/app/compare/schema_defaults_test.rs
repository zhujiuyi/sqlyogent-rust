use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::time::Duration;

use mysql::{Conn, OptsBuilder, prelude::Queryable};

use crate::database::{
    ConnectionConfig, HttpTunnelConfig, Response, SchemaDefaults, Worker, quote_identifier,
};

use super::*;

#[test]
#[ignore = "requires a disposable MariaDB and PHP tunnel on loopback"]
fn schema_sync_applies_database_defaults_without_rewriting_existing_tables() {
    let port = std::env::var("HARBOR_SCHEMA_FIXTURE_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let url = std::env::var("HARBOR_SCHEMA_FIXTURE_HTTP_URL").unwrap();
    assert!(url.starts_with("http://127.0.0.1:"));
    let prefix = format!("harbor_schema_defaults_{}", std::process::id());
    let mut server = Conn::new(
        OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(port)
            .user(Some("root"))
            .prefer_socket(false),
    )
    .unwrap();
    let tunnel = HttpTunnelConfig {
        url,
        force_base64: true,
        use_utf8mb4: true,
        ..Default::default()
    };
    let desired = SchemaDefaults {
        charset: "utf8mb4".into(),
        collation: "utf8mb4_bin".into(),
    };
    for (label, charset, collation, tunnel) in [
        ("直连", "latin1", "latin1_swedish_ci", None),
        ("隧道", "utf8mb4", "utf8mb4_general_ci", Some(tunnel)),
    ] {
        let source_name = format!("{prefix}_{label}_源");
        let target_name = format!("{prefix}_{label}_目标");
        let source = quote_identifier(&source_name);
        let target = quote_identifier(&target_name);
        let outcome =
            catch_unwind(AssertUnwindSafe(|| {
                server
                    .query_drop(format!(
                        "CREATE DATABASE {source} DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_bin"
                    ))
                    .unwrap();
                server.query_drop(format!(
                "CREATE DATABASE {target} DEFAULT CHARACTER SET {charset} COLLATE {collation}"
            )).unwrap();
                server
                    .query_drop(format!(
                        "CREATE TABLE {source}.source_only (id INT PRIMARY KEY)"
                    ))
                    .unwrap();
                server
                    .query_drop(format!(
                        "CREATE TABLE {target}.retained (id INT PRIMARY KEY, memo VARCHAR(20))
                 ENGINE=InnoDB DEFAULT CHARACTER SET latin1 COLLATE latin1_swedish_ci"
                    ))
                    .unwrap();
                server
                    .query_drop(format!(
                        "INSERT INTO {target}.retained VALUES (1, 'keep me')"
                    ))
                    .unwrap();
                let definition = |server: &mut Conn| -> (String, String) {
                    server
                        .query_first(format!("SHOW CREATE TABLE {target}.retained"))
                        .unwrap()
                        .unwrap()
                };
                let before = definition(&mut server);

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
                let config = ConnectionConfig {
                    port,
                    database: Some(target_name.clone()),
                    ..Default::default()
                };
                let connect = match tunnel {
                    Some(tunnel) => Command::ConnectHttp(config, tunnel),
                    None => Command::Connect(config),
                };
                assert!(matches!(request(connect), Response::Connected(_)));
                let snapshot = |request: &mut dyn FnMut(Command) -> Response, name: &str| {
                    let Response::Snapshot(snapshot) = request(Command::SnapshotSchema {
                        schema: name.into(),
                    }) else {
                        panic!("server did not return a schema snapshot");
                    };
                    snapshot
                };
                let left = snapshot(&mut request, &source_name);
                let right = snapshot(&mut request, &target_name);
                let (from, to) = sync::database_defaults_diff(&left, &right).unwrap();
                assert_eq!(from, &desired);
                assert_eq!((&*to.charset, &*to.collation), (charset, collation));

                let differences = sync::differences(&left, &right, false);
                let plan = sync::plan(
                    &left,
                    &right,
                    &differences,
                    &BTreeSet::new(),
                    sync::PlanOptions {
                        sync_database_defaults: true,
                        ..Default::default()
                    },
                )
                .unwrap();
                assert_eq!(plan.changes.len(), 1);
                assert_eq!(plan.statements, 1);
                assert!(
                    plan.changes[0]
                        .sql
                        .starts_with(&format!("ALTER DATABASE {target} "))
                );
                assert!(plan.sql.contains(&plan.changes[0].sql));
                let Response::SchemaApplied(result) = request(Command::ApplySchema {
                    apply: Box::new(SchemaApply {
                        expected: plan.target,
                        changes: plan.changes,
                    }),
                }) else {
                    panic!("server did not return a schema application result");
                };
                if let Some(SchemaApplyStop::Failed { error, sql, .. }) = result.stopped {
                    panic!("{sql}: {error}");
                }
                assert_eq!(result.completed, 1);
                let refreshed = snapshot(&mut request, &target_name);
                assert!(sync::database_defaults_diff(&left, &refreshed).is_none());
                assert_eq!(refreshed.objects.len(), 1);
                assert_eq!(refreshed.objects[0].object.name, "retained");
                assert_eq!(before, definition(&mut server));
                let retained: Vec<(u64, String)> = server
                    .query(format!("SELECT id, memo FROM {target}.retained"))
                    .unwrap();
                assert_eq!(retained, [(1, "keep me".into())]);

                server.query_drop(format!(
                "ALTER DATABASE {target} DEFAULT CHARACTER SET {charset} COLLATE {collation}"
            )).unwrap();
                for statement in crate::sql_tools::split_script(&plan.sql) {
                    server.query_drop(statement.sql).unwrap();
                }
                assert_eq!(
                    snapshot(&mut request, &target_name).defaults.as_ref(),
                    Some(&desired)
                );
                server
                    .query_drop(format!(
                        "CREATE TABLE {target}.new_table (memo VARCHAR(20))"
                    ))
                    .unwrap();
                let inherited: Option<String> = server
                    .exec_first(
                        "SELECT COLLATION_NAME FROM information_schema.COLUMNS
                 WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'new_table' AND COLUMN_NAME = 'memo'",
                        (&target_name,),
                    )
                    .unwrap();
                assert_eq!(inherited.as_deref(), Some("utf8mb4_bin"));
            }));
        for schema in [source, target] {
            server
                .query_drop(format!("DROP DATABASE IF EXISTS {schema}"))
                .unwrap();
        }
        if let Err(error) = outcome {
            resume_unwind(error);
        }
    }
}
