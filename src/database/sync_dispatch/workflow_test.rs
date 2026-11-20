use std::time::{SystemTime, UNIX_EPOCH};

use super::super::HttpTunnelConfig;
use super::super::http_session::workflow_test::{TestGateway, archived_tunnel, fixture_database};
use super::*;

#[test]
#[ignore = "requires a disposable MariaDB via HARBOR_HTTP_FIXTURE_PORT"]
fn reconciles_native_and_http_tables_then_mirrors_to_native() {
    let (port, opts) = fixture_database();
    let mut sql = Conn::new(opts.clone()).unwrap();
    let stem = format!("harbor_http_sync_{}", std::process::id());
    let schemas = [format!("{stem}_a"), format!("{stem}_b")];
    let _cleanup = Cleanup {
        opts: opts.clone(),
        schemas: schemas.clone(),
    };
    let objects = schemas.clone().map(|schema| DbObject {
        schema,
        name: "records".into(),
        kind: ObjectKind::Table,
    });
    for object in &objects {
        sql.query_drop(format!(
            "CREATE DATABASE {} CHARACTER SET utf8mb4 COLLATE utf8mb4_bin",
            quote_identifier(&object.schema)
        ))
        .unwrap();
        sql.query_drop(format!(
            "CREATE TABLE {} (
            id INT PRIMARY KEY, label VARCHAR(60) NOT NULL, score DECIMAL(26,4),
            payload VARBINARY(12), weight FLOAT, active TINYINT NOT NULL
        ) ENGINE=InnoDB",
            object.qualified_name()
        ))
        .unwrap();
    }
    let [native_table, http_table] = objects;
    let native_name = native_table.qualified_name();
    let http_name = http_table.qualified_name();
    sql.exec_batch(
        format!("INSERT INTO {native_name} VALUES (?, ?, 9007199254740992.0001, ?, 0.12345678, 1)"),
        (1..=130).map(|id| (id, format!("来源;行 {id}"), vec![0, 255, 128])),
    )
    .unwrap();
    sql.query_drop(format!(
        "INSERT INTO {native_name} VALUES (132,'保留源端',NULL,NULL,0,0)"
    ))
    .unwrap();
    sql.query_drop(format!(
        "INSERT INTO {http_name} VALUES
        (1,'旧值',6,X'02',0.8,1), (131,'另一端',7,X'03',1.2,1), (132,'保留目标',NULL,NULL,0,0)"
    ))
    .unwrap();

    let gateway = TestGateway::new(opts);
    let native = Worker::new();
    let tunneled = Worker::new();
    assert!(matches!(
        request(
            &native,
            1,
            Command::Connect(ConnectionConfig {
                port,
                ..Default::default()
            })
        ),
        Response::Connected(_)
    ));
    assert!(matches!(
        request(
            &tunneled,
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
    let Response::SyncEndpoint(target) = request(&tunneled, 2, Command::SyncEndpoint) else {
        panic!("HTTP sync endpoint")
    };
    let mode = SyncMode::ReconcileBoth(SyncConflict::SourceWins);
    let filter = Some(SyncFilter::new("active = 1", SyncFilterScope::Both).unwrap());
    let mapping = SyncMapping {
        source: native_table.clone(),
        target: http_table.clone(),
        filter: filter.clone(),
        compare_columns: Vec::new(),
    };
    let Response::SyncGroupPlan(plan) = request(
        &native,
        2,
        Command::PlanScopedSyncGroup {
            mappings: vec![mapping],
            endpoint: target,
            policy: mode.into(),
        },
    ) else {
        panic!("expected mixed sync preview")
    };
    assert_eq!(plan.plan.tables[0].counts.source_only, 129);
    assert_eq!(plan.plan.tables[0].counts.target_only, 1);
    assert_eq!(plan.plan.tables[0].counts.conflicts, 1);
    let Response::SyncGroupApplied(result) =
        request(&native, 3, Command::ApplySyncGroup { prepared: plan })
    else {
        panic!("expected mixed sync result")
    };
    assert!(result.stopped.is_none(), "{:?}", result.stopped);
    assert_eq!(
        (
            result.target.inserted,
            result.target.updated,
            result.source.inserted
        ),
        (129, 1, 1)
    );
    let actual = sql
        .query_first::<(String, String, Vec<u8>, f64), _>(format!(
            "SELECT label, score, payload, weight + 0e0 FROM {http_name} WHERE id=1"
        ))
        .unwrap();
    assert_eq!(
        actual,
        Some((
            "来源;行 1".into(),
            "9007199254740992.0001".into(),
            vec![0, 255, 128],
            f64::from(0.12345678_f32)
        ))
    );
    assert_eq!(
        sql.query_first::<String, _>(format!("SELECT label FROM {native_name} WHERE id=131"))
            .unwrap()
            .as_deref(),
        Some("另一端")
    );

    sql.query_drop(format!("DELETE FROM {http_name} WHERE id=2"))
        .unwrap();
    let Response::SyncEndpoint(target) = request(&native, 4, Command::SyncEndpoint) else {
        panic!("native sync endpoint")
    };
    let mapping = SyncMapping {
        source: http_table,
        target: native_table,
        filter,
        compare_columns: Vec::new(),
    };
    let Response::SyncPlan(plan) = request(
        &tunneled,
        3,
        Command::PlanScopedSync {
            mapping,
            endpoint: target,
            policy: SyncMode::MirrorReceiver.into(),
        },
    ) else {
        panic!("expected native mirror preview")
    };
    assert_eq!(plan.plan.planned_target().deleted, 1);
    let Response::SyncApplied(result) =
        request(&tunneled, 4, Command::ApplySync { prepared: plan })
    else {
        panic!("expected mirror result")
    };
    assert_eq!(result.target.deleted, 1);
    assert_eq!(
        sql.query_first::<u64, _>(format!("SELECT COUNT(*) FROM {native_name} WHERE id=2"))
            .unwrap(),
        Some(0)
    );
    assert_eq!(
        sql.query_first::<String, _>(format!("SELECT label FROM {native_name} WHERE id=132"))
            .unwrap()
            .as_deref(),
        Some("保留源端")
    );
    assert_eq!(
        sql.query_first::<String, _>(format!("SELECT label FROM {http_name} WHERE id=132"))
            .unwrap()
            .as_deref(),
        Some("保留目标")
    );

    sql.query_drop(format!(
        "UPDATE {native_name} SET label = '独立作业' WHERE id=1"
    ))
    .unwrap();
    let job_folder = tempfile::tempdir().unwrap();
    let job = job_folder.path().join("tunnel-sync.json");
    let definition = serde_json::json!({
        "version": 1,
        "connection": { "host": "127.0.0.1", "port": port, "user": "root" },
        "job": {
            "type": "data_sync", "source_schema": schemas[0], "target_schema": schemas[1],
            "target": {
                "host": "127.0.0.1", "port": port, "user": "root",
                "http": { "url": gateway.tunnel.url, "force_base64": true, "utf8mb4": true }
            },
            "mode": "insert_and_update", "apply": true,
            "tables": [ { "source": "records", "target": "records", "where": "id = 1", "filter_scope": "both" } ]
        }
    });
    std::fs::write(&job, serde_json::to_vec_pretty(&definition).unwrap()).unwrap();
    assert_eq!(crate::jobs::cli(["run".into(), job.into_os_string()]), 0);
    assert_eq!(
        sql.query_first::<String, _>(format!("SELECT label FROM {http_name} WHERE id=1"))
            .unwrap()
            .as_deref(),
        Some("独立作业")
    );
    gateway.stop();
}

fn request(worker: &Worker, id: u64, command: Command) -> Response {
    request_result(worker, id, command).unwrap()
}

fn request_result(worker: &Worker, id: u64, command: Command) -> Result<Response, String> {
    worker.send(id, command).unwrap();
    let event = worker
        .recv_timeout(Duration::from_secs(25))
        .unwrap()
        .unwrap();
    assert_eq!(event.id, id);
    event.result
}

#[test]
#[ignore = "requires a disposable MariaDB via HARBOR_HTTP_FIXTURE_PORT"]
fn mirrors_filtered_parent_and_child_into_http_receiver() {
    mirrors_filtered_parent_and_child(None);
}

#[test]
#[ignore = "requires disposable MariaDB and HARBOR_LEGACY_TUNNEL_URL"]
fn mirrors_filtered_parent_and_child_into_archived_php_receiver() {
    mirrors_filtered_parent_and_child(Some(
        archived_tunnel()
            .expect("must set HARBOR_LEGACY_TUNNEL_URL to a disposable loopback endpoint"),
    ));
}

fn mirrors_filtered_parent_and_child(archived: Option<HttpTunnelConfig>) {
    let (port, opts) = fixture_database();
    let mut sql = Conn::new(opts.clone()).unwrap();
    let transport = if archived.is_some() { "php" } else { "proxy" };
    let stem = format!("harbor_http_mirror_{}_{transport}", std::process::id());
    let schemas = [format!("{stem}_a"), format!("{stem}_b")];
    let _cleanup = Cleanup {
        opts: opts.clone(),
        schemas: schemas.clone(),
    };
    let table = |schema: &str, name: &str| DbObject {
        schema: schema.to_owned(),
        name: name.to_owned(),
        kind: ObjectKind::Table,
    };
    let [source_tables, target_tables] = schemas
        .each_ref()
        .map(|schema| (table(schema, "parent"), table(schema, "child")));
    for (parent, child) in [&source_tables, &target_tables] {
        sql.query_drop(format!(
            "CREATE DATABASE {} CHARACTER SET utf8mb4 COLLATE utf8mb4_bin",
            quote_identifier(&parent.schema)
        ))
        .unwrap();
        sql.query_drop(format!("CREATE TABLE {} (id INT PRIMARY KEY, active INT NOT NULL, label VARCHAR(24), note VARCHAR(30)) ENGINE=InnoDB", parent.qualified_name())).unwrap();
        sql.query_drop(format!(
            "CREATE TABLE {} (
            id INT PRIMARY KEY, parent_id INT NOT NULL, active INT NOT NULL, payload VARBINARY(20),
            FOREIGN KEY (parent_id) REFERENCES {} (id) ON DELETE CASCADE
        ) ENGINE=InnoDB",
            child.qualified_name(),
            quote_identifier("parent")
        ))
        .unwrap();
    }
    let (from_parent, from_child) = source_tables;
    let (to_parent, to_child) = target_tables;
    let fp = from_parent.qualified_name();
    let fc = from_child.qualified_name();
    let tp = to_parent.qualified_name();
    let tc = to_child.qualified_name();
    sql.query_drop(format!(
        "INSERT INTO {fp} VALUES
        (1,1,'共同','源备注'),(2,1,'新增','源备注'),(4,0,'跨范围','源备注')"
    ))
    .unwrap();
    sql.query_drop(format!(
        "INSERT INTO {tp} VALUES
        (1,1,'共同','目标备注'),(4,1,'跨范围','目标边界'),
        (5,0,'范围外','目标备注'),(9,1,'多余','待删除')"
    ))
    .unwrap();
    sql.query_drop(format!(
        "INSERT INTO {fc} VALUES (11,1,1,X'00FF'),(21,2,1,X'8081')"
    ))
    .unwrap();
    sql.query_drop(format!(
        "INSERT INTO {tc} VALUES (11,1,1,X'00FF'),(51,5,0,X'7F'),(91,9,1,X'81')"
    ))
    .unwrap();
    let gateway = archived.is_none().then(|| TestGateway::new(opts));
    let tunnel = archived.unwrap_or_else(|| gateway.as_ref().unwrap().tunnel.clone());
    let native = Worker::new();
    let http = Worker::new();
    let config = ConnectionConfig {
        port,
        ..Default::default()
    };
    assert!(matches!(
        request(&native, 1, Command::Connect(config.clone())),
        Response::Connected(_)
    ));
    assert!(matches!(
        request(&http, 1, Command::ConnectHttp(config, tunnel)),
        Response::Connected(_)
    ));
    let Response::SyncEndpoint(endpoint) = request(&http, 2, Command::SyncEndpoint) else {
        panic!("expected HTTP endpoint");
    };
    let mapping = |source, target, compare_columns| SyncMapping {
        source,
        target,
        filter: Some(SyncFilter::new("active = 1", SyncFilterScope::Both).unwrap()),
        compare_columns,
    };
    let Response::SyncGroupPlan(prepared) = request(
        &native,
        2,
        Command::PlanScopedSyncGroup {
            endpoint,
            policy: SyncMode::MirrorReceiver.into(),
            mappings: vec![
                mapping(from_child, to_child, Vec::new()),
                mapping(from_parent, to_parent, vec!["label".into()]),
            ],
        },
    ) else {
        panic!("expected scoped mirror preview")
    };
    assert_eq!(prepared.plan.tables[0].target.name, "parent");
    assert_eq!(prepared.plan.tables[0].outside_filter, 1);
    let directory = tempfile::tempdir().unwrap();
    let script_path = directory.path().join("镜像.sql");
    let Response::SyncScripts {
        prepared: SyncScriptPlan::Group(prepared),
        files,
    } = request(
        &native,
        3,
        Command::ExportSync {
            prepared: SyncScriptPlan::Group(prepared),
            paths: SyncScriptPaths {
                source: None,
                target: Some(script_path.clone()),
                replace: false,
            },
        },
    )
    else {
        panic!("expected an exported mirror script")
    };
    assert_eq!(files[0].path, script_path);
    assert_eq!(
        sql.query_first::<u64, _>(format!("SELECT COUNT(*) FROM {tp} WHERE id=9"))
            .unwrap(),
        Some(1)
    );
    let mut replay = Conn::new(
        OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(port)
            .user(Some("root"))
            .prefer_socket(false),
    )
    .unwrap();
    let script = std::fs::read_to_string(script_path).unwrap();
    let mut statements = crate::sql_tools::StatementReader::new(&script);
    while let Some(command) = statements.next_statement() {
        replay.query_drop(command.sql).unwrap();
    }
    assert_eq!(
        replay
            .query_first::<u8, _>("SELECT @harbor_sync_ok")
            .unwrap(),
        Some(1)
    );
    assert_eq!(
        replay
            .query_first::<u64, _>(format!("SELECT COUNT(*) FROM {tp} WHERE id=9"))
            .unwrap(),
        Some(0)
    );
    replay.query_drop("ROLLBACK").unwrap();
    let Response::SyncGroupApplied(applied) =
        request(&native, 4, Command::ApplySyncGroup { prepared })
    else {
        panic!("expected mirror result")
    };
    assert!(applied.stopped.is_none(), "{:?}", applied.stopped);
    assert_eq!(
        (
            applied.completed,
            applied.target.inserted,
            applied.target.deleted
        ),
        (2, 2, 2)
    );
    assert_eq!(
        sql.query::<(u64, String), _>(format!("SELECT id,note FROM {tp} ORDER BY id"))
            .unwrap(),
        [
            (1, "目标备注".into()),
            (2, "源备注".into()),
            (4, "目标边界".into()),
            (5, "目标备注".into()),
        ]
    );
    assert_eq!(
        sql.query::<(u64, Vec<u8>), _>(format!("SELECT id,payload FROM {tc} ORDER BY id"))
            .unwrap(),
        [(11, vec![0, 255]), (21, vec![128, 129]), (51, vec![127]),]
    );
    if let Some(gateway) = gateway {
        gateway.stop();
    }
}

#[test]
#[ignore = "requires disposable MariaDB and archived PHP via HARBOR_SYNC_UNIQUE_FIXTURE_PORT, HARBOR_SYNC_UNIQUE_HTTP_URL, and HARBOR_SYNC_UNIQUE_FIXTURE_DIR"]
fn mirrors_composite_unique_key_to_native_and_archived_php() {
    let (port, opts, tunnel, directory) = archived_sync_fixture("HARBOR_SYNC_UNIQUE");
    let mut sql = Conn::new(opts.clone()).unwrap();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let schema = format!("harbor_sync_unique_{}_{stamp:x}", std::process::id());
    sql.query_drop(format!(
        "CREATE DATABASE {} CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci",
        quote_identifier(&schema)
    ))
    .unwrap();
    let _cleanup = Cleanup {
        opts: opts.clone(),
        schemas: [schema.clone()],
    };
    let [source, direct, archived] = [
        ("source_records", "source_identity"),
        ("direct_records", "direct_identity"),
        ("archived_records", "archived_identity"),
    ]
    .map(|(name, index)| {
        let table = DbObject {
            schema: schema.clone(),
            name: name.into(),
            kind: ObjectKind::Table,
        };
        sql.query_drop(format!(
            "CREATE TABLE {} (
                tenant_id INT NOT NULL, record_code VARCHAR(24) NOT NULL,
                label VARCHAR(24) NOT NULL, prefix_token VARCHAR(24) NOT NULL,
                nullable_token VARCHAR(24), UNIQUE KEY aa_prefix (prefix_token(4)),
                UNIQUE KEY ab_nullable (nullable_token), UNIQUE KEY {index} (tenant_id, record_code)
            ) ENGINE=InnoDB",
            table.qualified_name()
        ))
        .unwrap();
        table
    });
    let source_name = source.qualified_name();
    sql.query_drop(format!(
        "ALTER TABLE {source_name} ADD UNIQUE KEY ac_source_token (prefix_token)"
    ))
    .unwrap();
    sql.query_drop(format!(
        "INSERT INTO {source_name} VALUES
        (1,'MIXED','相同','same-a',NULL), (1,'copy','新增','insert-b',NULL),
        (2,'copy','更新','update-c',NULL)"
    ))
    .unwrap();
    let same_keys = 10..150;
    let same_values = same_keys
        .clone()
        .map(|id| format!("({id},'keep','同一行','p{id:03}',NULL)"))
        .collect::<Vec<_>>()
        .join(", ");
    sql.query_drop(format!("INSERT INTO {source_name} VALUES {same_values}"))
        .unwrap();
    let rows = |connection: &mut Conn, table: &str| {
        connection
            .query::<(i32, String, String), _>(format!(
                "SELECT tenant_id, record_code, label FROM {table} ORDER BY tenant_id, record_code"
            ))
            .unwrap()
    };
    let source_before = rows(&mut sql, &source_name);
    let mut expected = [
        (1, "copy", "新增"),
        (1, "mixed", "相同"),
        (2, "copy", "更新"),
    ]
    .map(|(tenant, code, label)| (tenant, code.to_owned(), label.to_owned()))
    .to_vec();
    expected.extend(
        same_keys
            .clone()
            .map(|id| (id, "keep".into(), "同一行".into())),
    );
    let writes = SyncWrites {
        inserted: 1,
        updated: 1,
        deleted: 1,
    };

    let config = ConnectionConfig {
        port,
        ..Default::default()
    };
    let native = Worker::new();
    assert!(matches!(
        request(&native, 1, Command::Connect(config.clone())),
        Response::Connected(_)
    ));
    for (round, (target, connect, ready_sql)) in [
        (
            direct,
            Command::Connect(config.clone()),
            "SELECT @harbor_sync_endpoint_ok = 1 AND @harbor_sync_rows = 3",
        ),
        (
            archived,
            Command::ConnectHttp(config, tunnel),
            "SELECT @harbor_sync_ok",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let target_name = target.qualified_name();
        sql.query_drop(format!(
            "INSERT INTO {target_name} VALUES
            (1,'mixed','相同','same-a',NULL), (2,'copy','旧值','update-c',NULL),
            (3,'copy','多余','delete-d',NULL)"
        ))
        .unwrap();
        sql.query_drop(format!("INSERT INTO {target_name} VALUES {same_values}"))
            .unwrap();
        let target_before = rows(&mut sql, &target_name);
        let receiver = Worker::new();
        assert!(matches!(
            request(&receiver, 1, connect),
            Response::Connected(_)
        ));
        let Response::SyncEndpoint(endpoint) = request(&receiver, 2, Command::SyncEndpoint) else {
            panic!("expected sync endpoint for {target_name}");
        };
        let id = 2 + 3 * round as u64;
        let Response::SyncPlan(prepared) = request(
            &native,
            id,
            Command::PlanScopedSync {
                mapping: SyncMapping {
                    source: source.clone(),
                    target,
                    filter: None,
                    compare_columns: vec!["label".into()],
                },
                endpoint,
                policy: SyncMode::MirrorReceiver.into(),
            },
        ) else {
            panic!("expected composite unique key plan for {target_name}");
        };
        assert_eq!(prepared.plan.key_names, ["tenant_id", "record_code"]);
        let counts = prepared.plan.counts;
        assert_eq!(
            (
                counts.same,
                counts.source_only,
                counts.target_only,
                counts.conflicts
            ),
            (1 + same_keys.len() as u64, 1, 1, 1)
        );
        assert_eq!(prepared.plan.planned_target(), writes);

        let script_path = directory.path().join(format!("mirror-{round}.sql"));
        let Response::SyncScripts {
            prepared: SyncScriptPlan::Single(prepared),
            files,
        } = request(
            &native,
            id + 1,
            Command::ExportSync {
                prepared: SyncScriptPlan::Single(prepared),
                paths: SyncScriptPaths {
                    target: Some(script_path.clone()),
                    ..Default::default()
                },
            },
        )
        else {
            panic!("expected mirror script for {target_name}");
        };
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, script_path);
        assert_eq!(files[0].side, SyncSide::Target);
        assert_eq!(rows(&mut sql, &target_name), target_before);

        let mut replay = Conn::new(opts.clone()).unwrap();
        let script = std::fs::read_to_string(script_path).unwrap();
        let mut statements = crate::sql_tools::StatementReader::new(&script);
        while let Some(statement) = statements.next_statement() {
            replay.query_drop(statement.sql).unwrap();
        }
        assert_eq!(replay.query_first::<u8, _>(ready_sql).unwrap(), Some(1));
        assert_eq!(rows(&mut replay, &target_name), expected);
        replay.query_drop("ROLLBACK").unwrap();
        assert_eq!(rows(&mut sql, &target_name), target_before);

        let Response::SyncApplied(applied) =
            request(&native, id + 2, Command::ApplySync { prepared })
        else {
            panic!("expected mirror result for {target_name}");
        };
        assert_eq!(
            applied,
            SyncResult {
                target: writes,
                ..Default::default()
            }
        );
        assert_eq!(rows(&mut sql, &target_name), expected);
        assert_eq!(rows(&mut sql, &source_name), source_before);
    }
}

#[test]
#[ignore = "requires disposable MariaDB and archived PHP via HARBOR_SYNC_KEYLESS_FIXTURE_PORT, HARBOR_SYNC_KEYLESS_HTTP_URL, and HARBOR_SYNC_KEYLESS_FIXTURE_DIR"]
fn synchronizes_keyless_record_presence_to_native_and_archived_php() {
    let (port, opts, tunnel, directory) = archived_sync_fixture("HARBOR_SYNC_KEYLESS");
    let mut sql = Conn::new(opts.clone()).unwrap();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let schema = format!("harbor_sync_keyless_{}_{stamp:x}", std::process::id());
    sql.query_drop(format!(
        "CREATE DATABASE {} CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci",
        quote_identifier(&schema)
    ))
    .unwrap();
    let _cleanup = Cleanup {
        opts: opts.clone(),
        schemas: [schema.clone()],
    };

    let config = ConnectionConfig {
        port,
        ..Default::default()
    };
    let native = Worker::new();
    assert!(matches!(
        request(&native, 1, Command::Connect(config.clone())),
        Response::Connected(_)
    ));
    type Record = (i32, String, Option<String>, Option<String>);
    let rows = |connection: &mut Conn, table: &str| {
        connection
            .query::<Record, _>(format!(
                "SELECT group_no, label, HEX(payload), note FROM {table}
                 ORDER BY group_no, BINARY label, BINARY payload, BINARY note"
            ))
            .unwrap()
    };
    let shared: Record = (1, "共同".into(), Some("00FF".into()), None);
    let mut mirrored: Vec<Record> = vec![
        shared.clone(),
        shared,
        (2, "新增".into(), Some("80FF".into()), None),
        (3, "Exact".into(), Some("01".into()), None),
    ];
    let shared_range = 10..140;
    let shared_values = shared_range
        .clone()
        .map(|id| format!("({id},'跨页',X'BEEF',NULL)"))
        .collect::<Vec<_>>()
        .join(", ");
    mirrored.extend(
        shared_range
            .clone()
            .map(|id| (id, "跨页".into(), Some("BEEF".into()), None)),
    );
    let mirror_writes = SyncWrites {
        inserted: 1,
        deleted: 5,
        ..Default::default()
    };
    for (round, (transport, connect)) in [
        ("direct", Command::Connect(config.clone())),
        ("archived", Command::ConnectHttp(config, tunnel)),
    ]
    .into_iter()
    .enumerate()
    {
        let [source, target] = ["source", "target"].map(|side| {
            let table = DbObject {
                schema: schema.clone(),
                name: format!("{transport}_{side}"),
                kind: ObjectKind::Table,
            };
            sql.query_drop(format!(
                "CREATE TABLE {} (
                    group_no INT NOT NULL, label VARCHAR(24) NOT NULL, payload VARBINARY(12),
                    note VARCHAR(24)
                ) ENGINE=InnoDB",
                table.qualified_name()
            ))
            .unwrap();
            table
        });
        let source_name = source.qualified_name();
        let target_name = target.qualified_name();
        sql.query_drop(format!(
            "INSERT INTO {source_name} VALUES
                (1,'共同',X'00FF',NULL),
                (2,'新增',X'80FF',NULL), (2,'新增',X'80FF',NULL),
                (3,'Exact',X'01',NULL)"
        ))
        .unwrap();
        sql.query_drop(format!(
            "INSERT INTO {target_name} VALUES
                (1,'共同',X'00FF',NULL), (1,'共同',X'00FF',NULL),
                (3,'Exact',X'01',NULL),
                (3,'exact',X'01',NULL), (3,'exact',X'01',NULL),
                (3,'Exact ',X'01',NULL),
                (3,'Exact',X'0100',NULL),
                (3,'Exact',X'01',''),
                (4,'多余',NULL,'旧值'), (4,'多余',NULL,'旧值')"
        ))
        .unwrap();
        for table in [&source_name, &target_name] {
            sql.query_drop(format!("INSERT INTO {table} VALUES {shared_values}"))
                .unwrap();
        }
        let source_before = rows(&mut sql, &source_name);
        let target_before = rows(&mut sql, &target_name);

        let receiver = Worker::new();
        assert!(matches!(
            request(&receiver, 1, connect),
            Response::Connected(_)
        ));
        let Response::SyncEndpoint(endpoint) = request(&receiver, 2, Command::SyncEndpoint) else {
            panic!("expected sync endpoint for {target_name}");
        };
        let plan_sync = |id, mode| {
            let Response::SyncPlan(prepared) = request(
                &native,
                id,
                Command::PlanSync {
                    source: source.clone(),
                    target: target.clone(),
                    endpoint: endpoint.clone(),
                    mode,
                },
            ) else {
                panic!("expected {mode:?} plan for {target_name}");
            };
            prepared
        };
        let id = 2 + 5 * round as u64;
        let prepared = plan_sync(id, SyncMode::MirrorReceiver);
        assert!(prepared.plan.matches_whole_row());
        assert_eq!(
            prepared.plan.key_names,
            ["group_no", "label", "note", "payload"]
        );
        let counts = prepared.plan.counts;
        assert_eq!(
            (
                counts.same,
                counts.source_only,
                counts.target_only,
                counts.conflicts
            ),
            (2 + shared_range.len() as u64, 1, 5, 0)
        );
        assert_eq!(prepared.plan.planned_source(), SyncWrites::default());
        assert_eq!(prepared.plan.planned_target(), mirror_writes);

        let script_path = directory.path().join(format!("keyless-{transport}.sql"));
        let Response::SyncScripts {
            prepared: SyncScriptPlan::Single(prepared),
            files,
        } = request(
            &native,
            id + 1,
            Command::ExportSync {
                prepared: SyncScriptPlan::Single(prepared),
                paths: SyncScriptPaths {
                    target: Some(script_path.clone()),
                    ..Default::default()
                },
            },
        )
        else {
            panic!("expected mirror script for {target_name}");
        };
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, script_path);
        assert_eq!(files[0].side, SyncSide::Target);
        assert_eq!(rows(&mut sql, &target_name), target_before);

        let mut replay = Conn::new(opts.clone()).unwrap();
        let script = std::fs::read_to_string(script_path).unwrap();
        let mut statements = crate::sql_tools::StatementReader::new(&script);
        while let Some(statement) = statements.next_statement() {
            replay.query_drop(statement.sql).unwrap();
        }
        assert_eq!(
            replay
                .query_first::<u8, _>("SELECT @harbor_sync_ok")
                .unwrap(),
            Some(1)
        );
        assert_eq!(rows(&mut replay, &target_name), mirrored);
        assert_eq!(rows(&mut replay, &source_name), source_before);
        replay.query_drop("ROLLBACK").unwrap();
        assert_eq!(rows(&mut sql, &target_name), target_before);

        let Response::SyncApplied(applied) =
            request(&native, id + 2, Command::ApplySync { prepared })
        else {
            panic!("expected mirror result for {target_name}");
        };
        assert_eq!(
            applied,
            SyncResult {
                target: mirror_writes,
                ..Default::default()
            }
        );
        assert_eq!(rows(&mut sql, &target_name), mirrored);
        assert_eq!(rows(&mut sql, &source_name), source_before);

        // A NULL binary value and an empty binary value are two records in a merge.
        sql.query_drop(format!(
            "INSERT INTO {source_name} VALUES (4,'补齐',NULL,NULL), (4,'补齐',NULL,NULL)"
        ))
        .unwrap();
        sql.query_drop(format!(
            "INSERT INTO {target_name} VALUES (4,'补齐',X'',NULL), (4,'补齐',X'',NULL)"
        ))
        .unwrap();
        let prepared = plan_sync(id + 3, SyncMode::ReconcileBoth(SyncConflict::SourceWins));
        assert!(prepared.plan.matches_whole_row());
        let counts = prepared.plan.counts;
        assert_eq!(
            (
                counts.same,
                counts.source_only,
                counts.target_only,
                counts.conflicts
            ),
            (3 + shared_range.len() as u64, 1, 1, 0)
        );
        let reconcile_writes = SyncWrites {
            inserted: 1,
            ..Default::default()
        };
        assert_eq!(prepared.plan.planned_source(), reconcile_writes);
        assert_eq!(prepared.plan.planned_target(), reconcile_writes);
        let Response::SyncApplied(applied) =
            request(&native, id + 4, Command::ApplySync { prepared })
        else {
            panic!("expected reconcile result for {target_name}");
        };
        assert_eq!(
            applied,
            SyncResult {
                source: reconcile_writes,
                target: reconcile_writes,
                ..Default::default()
            }
        );
        let null_payload: Record = (4, "补齐".into(), None, None);
        let empty_payload: Record = (4, "补齐".into(), Some(String::new()), None);
        let mut expected_source = source_before;
        expected_source.extend([
            null_payload.clone(),
            null_payload.clone(),
            empty_payload.clone(),
        ]);
        expected_source.sort();
        let mut expected_target = mirrored.clone();
        expected_target.extend([null_payload, empty_payload.clone(), empty_payload]);
        expected_target.sort();
        assert_eq!(rows(&mut sql, &source_name), expected_source);
        assert_eq!(rows(&mut sql, &target_name), expected_target);
    }
}

#[test]
#[ignore = "requires disposable MariaDB and archived PHP via HARBOR_SYNC_MIRROR_UNIQUE_FIXTURE_PORT, HARBOR_SYNC_MIRROR_UNIQUE_HTTP_URL, and HARBOR_SYNC_MIRROR_UNIQUE_FIXTURE_DIR"]
fn mirrors_keyless_tables_with_nullable_or_prefix_unique_constraints() {
    let (port, opts, tunnel, directory) = archived_sync_fixture("HARBOR_SYNC_MIRROR_UNIQUE");
    let mut sql = Conn::new(opts.clone()).unwrap();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let schemas = ["source", "native", "http"]
        .map(|side| format!("harbor_v38_{}_{stamp:x}_{side}", std::process::id()));
    let _cleanup = Cleanup {
        opts: opts.clone(),
        schemas: schemas.clone(),
    };
    for schema in &schemas {
        sql.query_drop(format!(
            "CREATE DATABASE {schema} CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci"
        ))
        .unwrap();
    }
    let [source_schema, native_schema, http_schema] = &schemas;
    let table = |schema: &str, name: &str| DbObject {
        schema: schema.into(),
        name: name.into(),
        kind: ObjectKind::Table,
    };
    let rows = |conn: &mut Conn, name: &str| {
        conn.query::<(i32, Option<String>, String), _>(format!(
            "SELECT active, code, label FROM {name} ORDER BY active, BINARY code, BINARY label"
        ))
        .unwrap()
    };
    let config = ConnectionConfig {
        port,
        ..Default::default()
    };
    let sender = Worker::new();
    assert!(matches!(
        request(&sender, 1, Command::Connect(config.clone())),
        Response::Connected(_)
    ));
    let mut ids = 2..;
    let filter = SyncFilter::new("active = 1", SyncFilterScope::Both).unwrap();
    let writes = SyncWrites {
        inserted: 1,
        deleted: 1,
        ..Default::default()
    };
    for (transport, schema, connect) in [
        ("native", native_schema, Command::Connect(config.clone())),
        ("http", http_schema, Command::ConnectHttp(config, tunnel)),
    ] {
        let receiver = Worker::new();
        assert!(matches!(
            request(&receiver, 1, connect),
            Response::Connected(_)
        ));
        let Response::SyncEndpoint(endpoint) = request(&receiver, 2, Command::SyncEndpoint) else {
            panic!("missing receiver endpoint");
        };
        for (constraint, nullability, index, old, incoming, shared) in [
            ("nullable", "NULL", "code", "SLOT", "slot", None),
            (
                "prefix",
                "NOT NULL",
                "code(2)",
                "AB-old",
                "ab-new",
                Some("shared"),
            ),
        ] {
            let from = table(source_schema, &format!("{constraint}_{transport}"));
            let from_name = from.qualified_name();
            let create = |name: &str| {
                format!(
                    "CREATE TABLE {name} (
                        active INT NOT NULL, code VARCHAR(24) {nullability},
                        label VARCHAR(24) NOT NULL, UNIQUE KEY uq_code ({index})
                    ) ENGINE=InnoDB"
                )
            };
            sql.query_drop(create(&from_name)).unwrap();
            sql.exec_batch(
                format!("INSERT INTO {from_name} VALUES (?, ?, ?)"),
                [
                    (1, Some(incoming), "新增"),
                    (1, shared, "共同"),
                    (0, Some("origin"), "源范围外"),
                ],
            )
            .unwrap();
            let source_before = rows(&mut sql, &from_name);

            for blocked in [false, true] {
                let suffix = if blocked { "blocked" } else { "allowed" };
                let name = format!("{constraint}_{suffix}");
                let to = table(schema, &name);
                let to_name = to.qualified_name();
                sql.query_drop(create(&to_name)).unwrap();
                let (selected_old, retained) = if blocked {
                    ("old-free", Some(old))
                } else {
                    (old, shared.map(|_| "retained"))
                };
                sql.exec_batch(
                    format!("INSERT INTO {to_name} VALUES (?, ?, ?)"),
                    [
                        (1, Some(selected_old), "旧行"),
                        (1, shared, "共同"),
                        (0, retained, "目标保留"),
                    ],
                )
                .unwrap();
                let target_before = rows(&mut sql, &to_name);
                let expected: Vec<_> = target_before
                    .iter()
                    .filter(|row| row.0 == 0)
                    .chain(source_before.iter().filter(|row| row.0 == 1))
                    .cloned()
                    .collect();

                // This fixture-only audit survives rollback, so a rejected plan cannot
                // conceal an attempted receiver write by rolling its transaction back.
                let audit = format!("{schema}.{name}_writes");
                sql.query_drop(format!(
                    "CREATE TABLE {audit} (
                        seq INT PRIMARY KEY AUTO_INCREMENT, operation VARCHAR(8) NOT NULL,
                        code VARCHAR(24)
                    ) ENGINE=MyISAM"
                ))
                .unwrap();
                for (operation, row) in [("DELETE", "OLD"), ("INSERT", "NEW"), ("UPDATE", "NEW")] {
                    sql.query_drop(format!(
                        "CREATE TRIGGER {schema}.{name}_{operation} BEFORE {operation} ON {to_name}
                         FOR EACH ROW INSERT INTO {audit} (operation, code) VALUES ('{operation}', {row}.code)"
                    ))
                    .unwrap();
                }
                let audit_rows = |conn: &mut Conn| {
                    conn.query::<(String, String), _>(format!(
                        "SELECT operation, code FROM {audit} ORDER BY seq"
                    ))
                    .unwrap()
                };
                let expected_audit = [
                    ("DELETE".to_owned(), selected_old.to_owned()),
                    ("INSERT".to_owned(), incoming.to_owned()),
                ];
                let Response::SyncPlan(prepared) = request(
                    &sender,
                    ids.next().unwrap(),
                    Command::PlanScopedSync {
                        mapping: SyncMapping {
                            source: from.clone(),
                            target: to,
                            filter: Some(filter.clone()),
                            compare_columns: Vec::new(),
                        },
                        endpoint: endpoint.clone(),
                        policy: SyncMode::MirrorReceiver.into(),
                    },
                ) else {
                    panic!("expected preview for {to_name}");
                };
                assert!(prepared.plan.matches_whole_row());
                assert_eq!(prepared.plan.key_names, ["active", "code", "label"]);
                assert_eq!(prepared.plan.planned_target(), writes);
                assert_eq!(prepared.plan.counts.same, 1);
                assert_eq!(prepared.plan.samples.len(), 2);
                for (kind, code, label) in [
                    (SyncDifferenceKind::SourceOnly, incoming, "新增"),
                    (SyncDifferenceKind::TargetOnly, selected_old, "旧行"),
                ] {
                    assert!(
                        prepared.plan.samples.iter().any(|sample| {
                            sample.kind == kind
                                && sample.key
                                    == [Value::Int(1), Value::from(code), Value::from(label)]
                        }),
                        "missing {kind:?} preview row for {to_name}: {:?}",
                        prepared.plan.samples
                    );
                }
                let path = directory.path().join(format!("{schema}-{name}.sql"));
                let Response::SyncScripts {
                    prepared: SyncScriptPlan::Single(prepared),
                    files,
                } = request(
                    &sender,
                    ids.next().unwrap(),
                    Command::ExportSync {
                        prepared: SyncScriptPlan::Single(prepared),
                        paths: SyncScriptPaths {
                            target: Some(path.clone()),
                            ..Default::default()
                        },
                    },
                )
                else {
                    panic!("expected script for {to_name}");
                };
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].path, path);
                assert_eq!(files[0].side, SyncSide::Target);
                assert_eq!(rows(&mut sql, &to_name), target_before);
                assert!(audit_rows(&mut sql).is_empty());

                let mut replay = Conn::new(opts.clone()).unwrap();
                let script = std::fs::read_to_string(path).unwrap();
                let mut statements = crate::sql_tools::StatementReader::new(&script);
                while let Some(statement) = statements.next_statement() {
                    replay.query_drop(statement.sql).unwrap();
                }
                assert_eq!(
                    replay
                        .query_first::<(u8, u8), _>(
                            "SELECT @harbor_sync_ok, @harbor_sync_unique_failed"
                        )
                        .unwrap(),
                    Some((u8::from(!blocked), u8::from(blocked))),
                    "{to_name}"
                );
                assert_eq!(
                    rows(&mut replay, &to_name),
                    if blocked { &target_before } else { &expected }.as_slice()
                );
                replay.query_drop("ROLLBACK").unwrap();
                assert_eq!(rows(&mut sql, &to_name), target_before);
                if blocked {
                    assert!(audit_rows(&mut sql).is_empty());
                } else {
                    assert_eq!(audit_rows(&mut sql), expected_audit);
                    sql.query_drop(format!("TRUNCATE TABLE {audit}")).unwrap();
                }

                let applied = request_result(
                    &sender,
                    ids.next().unwrap(),
                    Command::ApplySync { prepared },
                );
                if blocked {
                    let error = applied.err().expect("conflicting mirror must be rejected");
                    assert!(
                        error.contains("镜像唯一约束预检未通过"),
                        "{to_name}: {error}"
                    );
                    assert!(audit_rows(&mut sql).is_empty());
                    assert_eq!(rows(&mut sql, &to_name), target_before);
                    eprintln!("{to_name}: {error} Target write triggers fired zero times.");
                } else {
                    let Response::SyncApplied(applied) = applied.unwrap() else {
                        panic!("expected native or HTTP mirror result");
                    };
                    assert_eq!(
                        applied,
                        SyncResult {
                            target: writes,
                            ..Default::default()
                        }
                    );
                    assert_eq!(audit_rows(&mut sql), expected_audit);
                    assert_eq!(rows(&mut sql, &to_name), expected);
                    eprintln!(
                        "{to_name}: preview, SQL replay and live apply verified with {:?}.",
                        expected_audit
                    );
                }
                assert_eq!(rows(&mut sql, &from_name), source_before);
            }
        }
    }
}

fn archived_sync_fixture(prefix: &str) -> (u16, Opts, HttpTunnelConfig, tempfile::TempDir) {
    let port = std::env::var(format!("{prefix}_FIXTURE_PORT"))
        .unwrap()
        .parse()
        .unwrap();
    let url = std::env::var(format!("{prefix}_HTTP_URL")).unwrap();
    let fixture = std::env::var_os(format!("{prefix}_FIXTURE_DIR")).unwrap();
    let directory = tempfile::tempdir_in(fixture).unwrap();
    let parsed = reqwest::Url::parse(&url).unwrap();
    assert_eq!(parsed.scheme(), "http");
    assert_eq!(parsed.host_str(), Some("127.0.0.1"));
    assert!(
        parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none()
    );
    let tunnel = HttpTunnelConfig {
        url,
        force_base64: true,
        use_utf8mb4: true,
        ..Default::default()
    };
    let opts = Opts::from(
        OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(port)
            .user(Some("root"))
            .prefer_socket(false),
    );
    (port, opts, tunnel, directory)
}

struct Cleanup<const N: usize> {
    opts: Opts,
    schemas: [String; N],
}

impl<const N: usize> Drop for Cleanup<N> {
    fn drop(&mut self) {
        if let Ok(mut sql) = Conn::new(self.opts.clone()) {
            for schema in &self.schemas {
                let _ = sql.query_drop(format!(
                    "DROP DATABASE IF EXISTS {}",
                    quote_identifier(schema)
                ));
            }
        }
    }
}

/// Ensures target-only opt-out reaches both the checked plan and each SQL execution path.
#[test]
#[ignore = "requires disposable MariaDB and archived PHP via HARBOR_SYNC_TARGET_FK_FIXTURE_PORT, HARBOR_SYNC_TARGET_FK_HTTP_URL, and HARBOR_SYNC_TARGET_FK_FIXTURE_DIR"]
fn sync_temporarily_skips_target_foreign_keys_for_cycles_and_mirror() {
    let (port, opts, tunnel, directory) = archived_sync_fixture("HARBOR_SYNC_TARGET_FK");
    let mut admin = Conn::new(opts.clone()).unwrap();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let schema = format!("harbor_target_fk_{}_{stamp:x}", std::process::id());
    admin
        .query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
        .unwrap();
    let _cleanup = Cleanup {
        opts: opts.clone(),
        schemas: [schema.clone()],
    };

    let config = ConnectionConfig {
        port,
        ..Default::default()
    };
    let source = Worker::new();
    assert!(matches!(
        request(&source, 1, Command::Connect(config.clone())),
        Response::Connected(_)
    ));
    let table = |name| DbObject {
        schema: schema.clone(),
        name,
        kind: ObjectKind::Table,
    };
    for (index, (label, connect)) in [
        ("direct", Command::Connect(config.clone())),
        ("http", Command::ConnectHttp(config, tunnel)),
    ]
    .into_iter()
    .enumerate()
    {
        let [
            (source_left, left),
            (source_right, right),
            (source_parent, parent),
        ] = ["left", "right", "parent"].map(|suffix| {
            (
                table(format!("{label}_source_{suffix}")),
                table(format!("{label}_target_{suffix}")),
            )
        });
        let child = table(format!("{label}_unselected_child"));
        let [sl, sr, sp, tl, tr, tp, child] = [
            &source_left,
            &source_right,
            &source_parent,
            &left,
            &right,
            &parent,
            &child,
        ]
        .map(DbObject::qualified_name);
        for name in [&sl, &sr, &tl, &tr] {
            admin
                .query_drop(format!(
                    "CREATE TABLE {name} (id INT PRIMARY KEY, peer_id INT NOT NULL) ENGINE=InnoDB"
                ))
                .unwrap();
        }
        admin
            .query_drop(format!(
                "ALTER TABLE {tl} ADD FOREIGN KEY(peer_id) REFERENCES {tr}(id)"
            ))
            .unwrap();
        admin
            .query_drop(format!(
                "ALTER TABLE {tr} ADD FOREIGN KEY(peer_id) REFERENCES {tl}(id)"
            ))
            .unwrap();
        for name in [&sp, &tp] {
            admin
                .query_drop(format!(
                    "CREATE TABLE {name} (id INT PRIMARY KEY) ENGINE=InnoDB"
                ))
                .unwrap();
        }
        admin
            .query_drop(format!(
                "CREATE TABLE {child} (id INT PRIMARY KEY, parent_id INT NOT NULL,
              FOREIGN KEY(parent_id) REFERENCES {tp}(id) ON DELETE CASCADE) ENGINE=InnoDB"
            ))
            .unwrap();
        for (name, values) in [
            (&sl, "(10,20)"),
            (&sr, "(20,10)"),
            (&sp, "(2)"),
            (&tp, "(1)"),
            (&child, "(4,1)"),
        ] {
            admin
                .query_drop(format!("INSERT INTO {name} VALUES {values}"))
                .unwrap();
        }

        let target = Worker::new();
        assert!(matches!(
            request(&target, 1, connect),
            Response::Connected(_)
        ));
        let Response::SyncEndpoint(endpoint) = request(&target, 2, Command::SyncEndpoint) else {
            panic!("missing target sync endpoint");
        };
        let sync = SyncPolicy {
            mode: SyncMode::MirrorReceiver,
            skip_target_fk_checks: true,
            continue_on_row_error: false,
        };
        let id = 2 + 3 * index as u64;
        let Response::SyncGroupPlan(prepared) = request(
            &source,
            id,
            Command::PlanScopedSyncGroup {
                mappings: [
                    (source_left, left),
                    (source_right, right),
                    (source_parent, parent),
                ]
                .into_iter()
                .map(|(from, to)| SyncMapping::all(from, to))
                .collect(),
                endpoint,
                policy: sync,
            },
        ) else {
            panic!("missing the target FK opt-out plan");
        };
        assert_eq!(prepared.plan.policy(), sync);
        let path = directory.path().join(format!("target-fk-{label}.sql"));
        let Response::SyncScripts {
            prepared: SyncScriptPlan::Group(prepared),
            files,
        } = request(
            &source,
            id + 1,
            Command::ExportSync {
                prepared: SyncScriptPlan::Group(prepared),
                paths: SyncScriptPaths {
                    target: Some(path.clone()),
                    ..Default::default()
                },
            },
        )
        else {
            panic!("missing the target FK opt-out script");
        };
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, path);

        let verify_data = |conn: &mut Conn| {
            let cycle = conn
                .query_first::<(i32, i32, i32, i32), _>(format!(
                    "SELECT l.id,l.peer_id,r.id,r.peer_id FROM {tl} l JOIN {tr} r
                 ON l.peer_id=r.id AND r.peer_id=l.id"
                ))
                .unwrap();
            assert_eq!(cycle, Some((10, 20, 20, 10)));
            assert_eq!(
                conn.query::<i32, _>(format!("SELECT id FROM {tp}"))
                    .unwrap(),
                [2]
            );
            assert_eq!(
                conn.query::<(i32, i32), _>(format!("SELECT id,parent_id FROM {child}"))
                    .unwrap(),
                [(4, 1)]
            );
        };
        let mut replay = Conn::new(opts.clone()).unwrap();
        replay
            .query_drop("SET SESSION foreign_key_checks=1")
            .unwrap();
        let script = std::fs::read_to_string(path).unwrap();
        let mut statements = crate::sql_tools::StatementReader::new(&script);
        while let Some(statement) = statements.next_statement() {
            replay.query_drop(statement.sql).unwrap();
        }
        verify_data(&mut replay);
        assert_eq!(
            replay
                .query_first::<u8, _>("SELECT @@SESSION.foreign_key_checks")
                .unwrap(),
            Some(1)
        );
        replay.query_drop("ROLLBACK").unwrap();
        let before: Vec<i32> = admin.query(format!("SELECT id FROM {tp}")).unwrap();
        assert_eq!(before, [1]);

        let Response::SyncGroupApplied(result) =
            request(&source, id + 2, Command::ApplySyncGroup { prepared })
        else {
            panic!("missing target FK opt-out apply result");
        };
        assert!(result.stopped.is_none(), "{:?}", result.stopped);
        assert_eq!(result.completed, 3);
        assert_eq!(
            result.target,
            SyncWrites {
                inserted: 3,
                updated: 0,
                deleted: 1
            }
        );
        verify_data(&mut admin);
        assert_eq!(
            admin
                .query_first::<(u8, u8), _>(
                    "SELECT @@GLOBAL.foreign_key_checks, @@SESSION.foreign_key_checks"
                )
                .unwrap(),
            Some((1, 1))
        );
    }
}

/// Exercises partial completion on both transports and the job's persisted summary.
#[test]
#[ignore = "requires a disposable MariaDB and archived PHP fixture"]
fn sync_continues_with_later_rows_after_checked_row_errors() {
    let (port, opts, tunnel, directory) = archived_sync_fixture("HARBOR_SYNC_CONTINUE");
    let mut admin = Conn::new(opts.clone()).unwrap();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let schema = format!("harbor_sync_continue_{}_{stamp:x}", std::process::id());
    admin
        .query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
        .unwrap();
    let _cleanup = Cleanup {
        opts,
        schemas: [schema.clone()],
    };
    let native = Worker::new();
    let config = ConnectionConfig {
        port,
        ..Default::default()
    };
    assert!(matches!(
        request(&native, 1, Command::Connect(config.clone())),
        Response::Connected(_)
    ));
    let url = tunnel.url.clone();

    for (index, (label, connect)) in [
        ("direct", Command::Connect(config.clone())),
        ("tunnel", Command::ConnectHttp(config, tunnel)),
    ]
    .into_iter()
    .enumerate()
    {
        let object = |name: String| DbObject {
            schema: schema.clone(),
            name,
            kind: ObjectKind::Table,
        };
        let source = object(format!("{label}_source"));
        let target = object(format!("{label}_target"));
        let child = object(format!("{label}_child")).qualified_name();
        let old = source.qualified_name();
        let new = target.qualified_name();
        admin
            .query_drop(format!(
                "CREATE TABLE {old} (id INT PRIMARY KEY, label VARCHAR(40) NOT NULL) ENGINE=InnoDB"
            ))
            .unwrap();
        admin
            .query_drop(format!(
                "CREATE TABLE {new} (id INT PRIMARY KEY, label VARCHAR(40) NOT NULL,
                    UNIQUE KEY label_once(label)) ENGINE=InnoDB"
            ))
            .unwrap();
        admin
            .query_drop(format!(
                "CREATE TABLE {child} (id INT PRIMARY KEY, parent_id INT NOT NULL,
                    FOREIGN KEY(parent_id) REFERENCES {new}(id) ON DELETE RESTRICT) ENGINE=InnoDB"
            ))
            .unwrap();
        admin
            .query_drop(format!(
                "INSERT INTO {old} VALUES (1,'taken-update'),(2,'taken-insert'),
                (3,'new-ok'),(4,'updated-ok'),(90,'taken-update'),(91,'taken-insert')"
            ))
            .unwrap();
        admin
            .query_drop(format!(
                "INSERT INTO {new} VALUES (1,'keep-old'),(4,'old'),(80,'referenced'),
                (81,'remove-ok'),(90,'taken-update'),(91,'taken-insert')"
            ))
            .unwrap();
        admin
            .query_drop(format!("INSERT INTO {child} VALUES (7,80)"))
            .unwrap();

        let peer = Worker::new();
        assert!(matches!(request(&peer, 1, connect), Response::Connected(_)));
        let Response::SyncEndpoint(endpoint) = request(&peer, 2, Command::SyncEndpoint) else {
            panic!("missing target sync endpoint");
        };
        let id = 2 + 2 * index as u64;
        let policy = SyncPolicy {
            mode: SyncMode::MirrorReceiver,
            skip_target_fk_checks: false,
            continue_on_row_error: true,
        };
        let Response::SyncGroupPlan(prepared) = request(
            &native,
            id,
            Command::PlanScopedSyncGroup {
                mappings: vec![SyncMapping::all(source.clone(), target.clone())],
                endpoint,
                policy,
            },
        ) else {
            panic!("missing continue-on-error sync plan");
        };
        assert_eq!(prepared.plan.policy(), policy);
        assert_eq!(
            prepared.plan.tables[0].planned_target(),
            SyncWrites {
                inserted: 2,
                updated: 2,
                deleted: 2,
            }
        );
        let Response::SyncGroupApplied(result) =
            request(&native, id + 1, Command::ApplySyncGroup { prepared })
        else {
            panic!("missing partial-sync result");
        };
        assert!(result.stopped.is_none(), "{:?}", result.stopped);
        assert!(result.uncertain.is_none());
        assert_eq!(result.completed, 1);
        assert_eq!(
            result.target,
            SyncWrites {
                inserted: 1,
                updated: 1,
                deleted: 1
            }
        );
        assert_eq!(result.source, SyncWrites::default());
        assert_eq!(result.skipped_rows, 3);
        let mut errors = result
            .row_errors
            .iter()
            .map(|error| {
                assert_eq!(error.side, SyncSide::Target);
                assert_eq!(error.table, target);
                (error.operation, error.code)
            })
            .collect::<Vec<_>>();
        errors.sort();
        assert_eq!(
            errors,
            [("delete", 1451), ("insert", 1062), ("update", 1062)]
        );
        let actual: Vec<(i32, String)> = admin
            .query(format!("SELECT id,label FROM {new} ORDER BY id"))
            .unwrap();
        assert_eq!(
            actual,
            [
                (1, "keep-old"),
                (3, "new-ok"),
                (4, "updated-ok"),
                (80, "referenced"),
                (90, "taken-update"),
                (91, "taken-insert"),
            ]
            .map(|(id, label)| (id, label.to_owned()))
        );
        assert_eq!(
            admin
                .query_first::<i32, _>(format!("SELECT parent_id FROM {child}"))
                .unwrap(),
            Some(80)
        );

        if label == "tunnel" {
            admin
                .query_drop(format!("INSERT INTO {old} VALUES (5,'job-ok')"))
                .unwrap();
            let path = directory.path().join("continue.json");
            let log = directory.path().join("continue.jsonl");
            std::fs::write(
                &path,
                serde_json::to_vec(&serde_json::json!({
                    "version": 1,
                    "connection": {"host": "127.0.0.1", "port": port, "user": "root"},
                    "job": {
                        "type": "data_sync", "source_schema": schema, "target_schema": schema,
                        "target": {
                            "host": "127.0.0.1", "port": port, "user": "root",
                            "http": {"url": url, "force_base64": true, "utf8mb4": true}
                        },
                        "tables": [{"source": source.name, "target": target.name}],
                        "mode": "mirror_receiver", "apply": true, "continue_on_row_error": true
                    },
                    "log": "continue.jsonl"
                }))
                .unwrap(),
            )
            .unwrap();
            assert!(crate::jobs::run_fixture(&path).is_err());
            let content = std::fs::read_to_string(log).unwrap();
            let summary: serde_json::Value =
                serde_json::from_str(content.lines().last().unwrap()).unwrap();
            assert_eq!(summary["event"], "failed");
            assert_eq!(summary["skipped_rows"], 3);
            assert_eq!(summary["row_errors"].as_array().unwrap().len(), 3);
            assert_eq!(summary["target_writes"]["inserted"], 1);
            assert_eq!(
                admin
                    .query_first::<String, _>(format!("SELECT label FROM {new} WHERE id=5"))
                    .unwrap()
                    .as_deref(),
                Some("job-ok")
            );
        }
    }
}

#[test]
#[ignore = "requires a disposable MariaDB and archived PHP fixture"]
fn synchronizes_rows_through_endpoint_local_where_subqueries() {
    let (port, opts, tunnel, directory) = archived_sync_fixture("HARBOR_SYNC_SUBQUERY");
    let mut admin = Conn::new(opts.clone()).unwrap();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let stem = format!("harbor_where_{}_{stamp:x}", std::process::id());
    let [source_schema, native_schema, http_schema] =
        ["source", "native", "php"].map(|role| format!("{stem}_{role}"));
    let _cleanup = Cleanup {
        opts: opts.clone(),
        schemas: [
            source_schema.clone(),
            native_schema.clone(),
            http_schema.clone(),
        ],
    };
    for (schema, syncstart) in [
        (&source_schema, "2024-01-05 00:00:00"),
        (&native_schema, "2024-01-07 00:00:00"),
        (&http_schema, "2024-01-07 00:00:00"),
    ] {
        admin
            .query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
            .unwrap();
        admin
            .query_drop(format!(
                "CREATE TABLE {schema}.lastsync (syncstart DATETIME NOT NULL) ENGINE=InnoDB"
            ))
            .unwrap();
        admin
            .query_drop(format!(
                "INSERT INTO {schema}.lastsync VALUES ('{syncstart}')"
            ))
            .unwrap();
        admin
            .query_drop(format!(
                "CREATE TABLE {schema}.records (id INT PRIMARY KEY, modtime DATETIME NOT NULL,
                label VARCHAR(40) NOT NULL) ENGINE=InnoDB"
            ))
            .unwrap();
    }
    admin
        .query_drop(format!(
            "INSERT INTO {source_schema}.records VALUES
         (1,'2024-01-02','source-before'),(2,'2024-01-06','source-active'),
         (3,'2024-01-09','source-new'),(4,'2024-01-10','source-current')"
        ))
        .unwrap();
    let rows = |connection: &mut Conn, schema: &str| -> Vec<(i32, String)> {
        connection
            .query(format!("SELECT id,label FROM {schema}.records ORDER BY id"))
            .unwrap()
    };
    let source_before = rows(&mut admin, &source_schema);
    let object = |schema: &str| DbObject {
        schema: schema.to_owned(),
        name: "records".into(),
        kind: ObjectKind::Table,
    };
    let source = Worker::new();
    let config = ConnectionConfig {
        port,
        database: Some("harbor_copy".into()),
        ..Default::default()
    };
    assert!(matches!(
        request(&source, 1, Command::Connect(config.clone())),
        Response::Connected(_)
    ));
    let filter_sql = "modtime > (SELECT syncstart - INTERVAL 1 DAY FROM lastsync)";
    let expected = [
        (1, "target-before".to_owned()),
        (2, "target-boundary".to_owned()),
        (3, "source-new".to_owned()),
        (4, "source-current".to_owned()),
    ];
    let http_url = tunnel.url.clone();
    for (index, (schema, connect)) in [
        (&native_schema, Command::Connect(config.clone())),
        (&http_schema, Command::ConnectHttp(config, tunnel)),
    ]
    .into_iter()
    .enumerate()
    {
        admin
            .query_drop(format!(
                "INSERT INTO {schema}.records VALUES
             (1,'2024-01-02','target-before'),(2,'2024-01-06','target-boundary'),
             (4,'2024-01-10','target-old'),(5,'2024-01-11','target-remove')"
            ))
            .unwrap();
        let target_before = rows(&mut admin, schema);
        let target = Worker::new();
        assert!(matches!(
            request(&target, 1, connect),
            Response::Connected(_)
        ));
        let Response::SyncEndpoint(endpoint) = request(&target, 2, Command::SyncEndpoint) else {
            panic!("missing target sync endpoint");
        };
        let id = 2 + 3 * index as u64;
        let Response::SyncPlan(prepared) = request(
            &source,
            id,
            Command::PlanScopedSync {
                mapping: SyncMapping {
                    source: object(&source_schema),
                    target: object(schema),
                    filter: Some(SyncFilter::new(filter_sql, SyncFilterScope::Both).unwrap()),
                    compare_columns: Vec::new(),
                },
                endpoint,
                policy: SyncMode::MirrorReceiver.into(),
            },
        ) else {
            panic!("expected scoped subquery plan");
        };
        let writes = SyncWrites {
            inserted: 1,
            updated: 1,
            deleted: 1,
        };
        assert_eq!(prepared.plan.planned_target(), writes);
        assert_eq!(prepared.plan.outside_filter, 1);

        let path = directory
            .path()
            .join(format!("mirror-subquery-{index}.sql"));
        let Response::SyncScripts {
            prepared: SyncScriptPlan::Single(prepared),
            files,
        } = request(
            &source,
            id + 1,
            Command::ExportSync {
                prepared: SyncScriptPlan::Single(prepared),
                paths: SyncScriptPaths {
                    target: Some(path.clone()),
                    ..Default::default()
                },
            },
        )
        else {
            panic!("expected subquery synchronization script");
        };
        assert_eq!(files[0].path, path);
        let script = std::fs::read_to_string(path).unwrap();
        let mut statements = crate::sql_tools::StatementReader::new(&script);
        let mut replay = Conn::new(opts.clone()).unwrap();
        while let Some(statement) = statements.next_statement() {
            replay.query_drop(statement.sql).unwrap();
        }
        assert_eq!(rows(&mut replay, schema), expected);
        replay.query_drop("ROLLBACK").unwrap();
        assert_eq!(rows(&mut admin, schema), target_before);

        let Response::SyncApplied(result) =
            request(&source, id + 2, Command::ApplySync { prepared })
        else {
            panic!("expected applied subquery synchronization");
        };
        assert!(result.stopped.is_none(), "{:?}", result.stopped);
        assert_eq!(result.target, writes);
        assert_eq!(rows(&mut admin, schema), expected);
        assert_eq!(rows(&mut admin, &source_schema), source_before);
    }

    // The installed version's source-only WHERE needs no copy of the lookup table at the target.
    admin
        .query_drop(format!("DROP TABLE {http_schema}.lastsync"))
        .unwrap();
    admin
        .query_drop(format!(
            "INSERT INTO {http_schema}.records VALUES (8,'2024-01-12','target-extra')"
        ))
        .unwrap();
    let path = directory.path().join("source-only-subquery.json");
    std::fs::write(&path, serde_json::to_vec(&serde_json::json!({
        "version":1, "connection":{"host":"127.0.0.1","port":port,"user":"root"},
        "job": {
            "type":"data_sync", "source_schema":source_schema, "target_schema":http_schema,
            "target":{"host":"127.0.0.1","port":port,"user":"root",
                "http":{"url":http_url,"force_base64":true,"utf8mb4":true}},
            "tables":[{"source":"records","target":"records","where":filter_sql,"filter_scope":"source"}],
            "mode":"mirror_receiver","apply":true
        }
    })).unwrap()).unwrap();
    crate::jobs::run_fixture(&path).unwrap();
    let mut source_only = expected.to_vec();
    source_only[1].1 = "source-active".into();
    source_only.push((8, "target-extra".into()));
    assert_eq!(rows(&mut admin, &http_schema), source_only);
}
