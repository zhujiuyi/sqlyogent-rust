use std::num::NonZeroU32;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::sql_encoding::SqlDumpCharset;

use super::transfer::{DumpOptions, DumpSettings};
use super::{
    Cell, Command, ConnectionConfig, DbObject, ObjectKind, Response, Worker, quote_identifier,
};

struct Fixture {
    worker: Worker,
    id: u64,
}

impl Fixture {
    fn queue(&mut self, command: Command) -> u64 {
        self.id += 1;
        self.worker.send(self.id, command).unwrap();
        self.id
    }

    fn wait(&self, id: u64) -> Result<Response, String> {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Some(event) = self.worker.try_recv() {
                assert_eq!(event.id, id);
                return event.result;
            }
            thread::sleep(Duration::from_millis(5));
        }
        Err("等待临时测试服务器响应超时。".to_owned())
    }

    fn request(&mut self, command: Command) -> Response {
        let id = self.queue(command);
        self.wait(id).unwrap()
    }

    fn sql(&mut self, sql: String) -> Vec<Vec<Cell>> {
        let Response::Queried(report) = self.request(Command::Query { sql, limit: 25 }) else {
            panic!("服务器未返回查询结果。");
        };
        assert_eq!(report.error, None);
        report
            .sets
            .into_iter()
            .last()
            .map(|set| set.rows)
            .unwrap_or_default()
    }
}

#[test]
#[ignore = "在一次性本机 MariaDB 中运行，需要 HARBOR_DUMP_TEST_PORT"]
fn configured_backup_holds_and_releases_the_source_global_read_lock() {
    let port = std::env::var("HARBOR_DUMP_TEST_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let mut db = Fixture {
        worker: Worker::new(),
        id: 0,
    };
    assert!(matches!(
        db.request(Command::Connect(ConnectionConfig {
            port,
            ..Default::default()
        })),
        Response::Connected(_)
    ));
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let schema = format!("harbor_global_lock_{suffix}");
    let db_name = quote_identifier(&schema);
    db.sql(format!(
        "CREATE DATABASE {db_name}; CREATE TABLE {db_name}.test_rows (id INT);
         INSERT INTO {db_name}.test_rows VALUES (41);
         CREATE VIEW {db_name}.skip_view AS SELECT id FROM {db_name}.test_rows;"
    ));
    let executable = std::env::current_exe().unwrap();
    let directory = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
    let output = directory.path().join("backup.sql");
    let response = db.request(Command::DumpConfigured {
        schema,
        path: output.clone(),
        settings: DumpSettings {
            object_kinds: Some(vec![ObjectKind::Table]),
            lock_source_server: true,
            ..DumpSettings::default()
        },
    });
    assert!(matches!(response, Response::Transferred(result) if result.rows == 1));
    let content = std::fs::read_to_string(output).unwrap();
    assert!(content.contains("test_rows") && content.contains("(41)"));
    assert!(!content.contains("skip_view"));
    db.sql(format!("INSERT INTO {db_name}.test_rows VALUES (42)"));
    assert_eq!(
        db.sql(format!("SELECT id FROM {db_name}.test_rows ORDER BY id")),
        vec![vec![Cell::Text("41".into())], vec![Cell::Text("42".into())]]
    );
    db.sql(format!("DROP DATABASE {db_name}"));
}

#[test]
#[ignore = "在一次性本机 MariaDB 中运行，需要 HARBOR_DUMP_TEST_PORT"]
fn configured_backup_can_stop_while_waiting_for_the_source_global_read_lock() {
    use mysql::{Conn, OptsBuilder, prelude::Queryable};

    let port = std::env::var("HARBOR_DUMP_TEST_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let mut db = Fixture {
        worker: Worker::new(),
        id: 0,
    };
    assert!(matches!(
        db.request(Command::Connect(ConnectionConfig {
            port,
            ..Default::default()
        })),
        Response::Connected(_)
    ));
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let schema = format!("harbor_stop_global_lock_{suffix:x}");
    let qualified = format!("{}.test_rows", quote_identifier(&schema));
    db.sql(format!(
        "CREATE DATABASE {}; CREATE TABLE {qualified} (id INT);",
        quote_identifier(&schema)
    ));
    let opts = OptsBuilder::default()
        .ip_or_hostname(Some("127.0.0.1"))
        .tcp_port(port)
        .user(Some("root"));
    let mut locker = Conn::new(opts.clone()).unwrap();
    let mut observer = Conn::new(opts).unwrap();
    locker
        .query_drop(format!("LOCK TABLES {qualified} WRITE"))
        .unwrap();
    let executable = std::env::current_exe().unwrap();
    let directory = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
    let output = directory.path().join("backup.sql");
    std::fs::write(&output, "previous backup").unwrap();
    let request = db.queue(Command::DumpConfigured {
        schema: schema.clone(),
        path: output.clone(),
        settings: DumpSettings {
            lock_source_server: true,
            overwrite: true,
            ..Default::default()
        },
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while observer
        .query_first::<u32, _>(
            "SELECT ID FROM information_schema.PROCESSLIST
         WHERE ID <> CONNECTION_ID() AND INFO LIKE '%FLUSH TABLES WITH READ LOCK'",
        )
        .unwrap()
        .is_none()
    {
        assert!(Instant::now() < deadline, "等待全局读锁的备份连接未出现。");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(db.worker.cancel_dump(request));
    let Err(error) = db.wait(request) else {
        panic!("已取消的备份返回了成功结果。");
    };
    assert_eq!(error, super::transfer::DUMP_CANCELLED);
    assert_eq!(std::fs::read_to_string(output).unwrap(), "previous backup");
    locker
        .query_drop(format!("INSERT INTO {qualified} VALUES (42)"))
        .unwrap();
    locker.query_drop("UNLOCK TABLES").unwrap();
    assert_eq!(
        db.sql(format!("SELECT id FROM {qualified}")),
        vec![vec![Cell::Text("42".into())]]
    );
    db.sql(format!("DROP DATABASE {}", quote_identifier(&schema)));
}

#[test]
#[ignore = "在一次性本机 MariaDB 中运行，需要 HARBOR_DUMP_TEST_PORT"]
fn interval_backup_exports_transactional_and_nontransactional_tables() {
    let port = std::env::var("HARBOR_DUMP_TEST_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let mut db = Fixture {
        worker: Worker::new(),
        id: 0,
    };
    assert!(matches!(
        db.request(Command::Connect(ConnectionConfig {
            port,
            ..Default::default()
        })),
        Response::Connected(_)
    ));
    let schema = format!("harbor_interval_{}", std::process::id());
    let target = format!("{schema}_restore");
    let source_name = quote_identifier(&schema);
    let target_name = quote_identifier(&target);
    db.sql(format!(
        "CREATE DATABASE {source_name} CHARACTER SET utf8mb4;
         CREATE DATABASE {target_name} CHARACTER SET utf8mb4;
         CREATE TABLE {source_name}.ordinary(id INT PRIMARY KEY) ENGINE=InnoDB;
         CREATE TABLE {source_name}.legacy(id INT PRIMARY KEY, label VARCHAR(16)) ENGINE=MyISAM;
         CREATE TABLE {source_name}.aria_rows(id INT PRIMARY KEY) ENGINE=Aria;
         INSERT INTO {source_name}.ordinary VALUES (11);
         INSERT INTO {source_name}.legacy VALUES (21,'甲'), (22,'乙'), (23,'丙');
         INSERT INTO {source_name}.aria_rows VALUES (31), (32);"
    ));
    let folder =
        tempfile::tempdir_in(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target")).unwrap();
    let full = folder.path().join("interval.sql");
    let dump = |path, object| Command::Dump {
        schema: schema.clone(),
        object,
        path,
        options: DumpOptions::default(),
        chunk_rows: NonZeroU32::new(2),
        bulk_insert_limit_kib: NonZeroU32::new(1),
    };
    let Response::Transferred(report) = db.request(dump(full.clone(), None)) else {
        panic!("服务器未返回备份结果。");
    };
    assert_eq!((report.objects, report.rows), (3, 6));
    let script = std::fs::read_to_string(&full).unwrap();
    assert!(script.contains("InnoDB 数据使用同一快照"));
    assert!(script.contains("非事务表数据稳定"));
    assert!(script.contains(&format!("USE {source_name};")));
    for table in ["ordinary", "legacy", "aria_rows"] {
        assert!(script.contains(&format!("INSERT INTO `{table}`")));
    }

    let one = folder.path().join("myisam-only.sql");
    let object = DbObject {
        schema: schema.clone(),
        name: "legacy".into(),
        kind: ObjectKind::Table,
    };
    let Response::Transferred(report) = db.request(dump(one.clone(), Some(object))) else {
        panic!("服务器未返回备份结果。");
    };
    assert_eq!((report.objects, report.rows), (1, 3));
    assert!(matches!(
        db.request(Command::Restore {
            schema: target,
            path: one,
        }),
        Response::Restored(_)
    ));
    assert_eq!(
        db.sql(format!(
            "SELECT id, label FROM {target_name}.legacy ORDER BY id"
        )),
        [
            vec![Cell::Text("21".into()), Cell::Text("甲".into())],
            vec![Cell::Text("22".into()), Cell::Text("乙".into())],
            vec![Cell::Text("23".into()), Cell::Text("丙".into())],
        ]
    );
    db.sql(format!(
        "DROP DATABASE {target_name}; DROP DATABASE {source_name}"
    ));
}

#[test]
#[ignore = "在一次性本机 MariaDB 中运行，需要 HARBOR_DUMP_TEST_PORT"]
fn configured_dump_applies_choices_and_preserves_file_on_cancel() {
    let port = std::env::var("HARBOR_DUMP_TEST_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let mut db = Fixture {
        worker: Worker::new(),
        id: 0,
    };
    let server = db.request(Command::Connect(ConnectionConfig {
        port,
        ..Default::default()
    }));
    assert!(matches!(server, Response::Connected(_)));
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let schema = format!("harbor_dump_smoke_{suffix:x}");
    let restore = format!("{schema}_r");
    let source = quote_identifier(&schema);
    let target = quote_identifier(&restore);
    db.sql(format!(
        "CREATE DATABASE {source} CHARACTER SET utf8mb4;
         CREATE DATABASE {target} CHARACTER SET utf8mb4;
         CREATE TABLE {source}.keep_me(id INT PRIMARY KEY, label VARCHAR(20)) ENGINE=InnoDB;
         CREATE TABLE {source}.skip_me(id INT PRIMARY KEY) ENGINE=InnoDB;
         INSERT INTO {source}.keep_me VALUES(1, '中文'), (2, '两条');"
    ));

    let build_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target");
    let temporary = tempfile::Builder::new()
        .prefix("harbor-dump-smoke-")
        .tempdir_in(build_dir)
        .unwrap();
    let path = temporary.path().join("selected.sql");
    let settings = DumpSettings {
        objects: Some(vec![DbObject {
            schema: schema.clone(),
            name: "keep_me".into(),
            kind: ObjectKind::Table,
        }]),
        single_transaction: true,
        ..Default::default()
    };
    let dump = |path: PathBuf, settings| Command::DumpConfigured {
        schema: schema.clone(),
        path,
        settings,
    };
    let response = db.request(dump(path.clone(), settings.clone()));
    assert!(
        matches!(response, Response::Transferred(summary) if summary.objects == 1 && summary.rows == 2)
    );
    let progress = db.worker.take_dump_progress(db.id).unwrap();
    assert_eq!(
        (
            progress.completed_objects,
            progress.total_objects,
            progress.rows
        ),
        (1, 1, 2)
    );
    let sql = std::fs::read_to_string(&path).unwrap().to_ascii_uppercase();
    assert!(sql.contains("KEEP_ME") && !sql.contains("SKIP_ME"));
    assert!(
        !sql.lines()
            .any(|line| line.starts_with("USE ") || line.starts_with("CREATE DATABASE"))
    );
    assert_eq!(
        sql.lines()
            .filter(|line| line.starts_with("INSERT INTO "))
            .count(),
        1
    );
    db.request(Command::Restore {
        schema: restore,
        path,
    });
    assert_eq!(
        db.sql(format!("SELECT label FROM {target}.keep_me ORDER BY id")),
        vec![
            vec![Cell::Text("中文".into())],
            vec![Cell::Text("两条".into())],
        ]
    );

    let path = temporary.path().join("options.sql");
    let mut switched = DumpSettings {
        include_use: true,
        include_create_database: true,
        disable_foreign_key_checks: false,
        lock_insert_tables: true,
        bulk_insert: false,
        lock_source_tables: true,
        flush_logs: true,
        single_transaction: false,
        ..settings.clone()
    };
    switched.options.drop_existing = false;
    db.request(dump(path.clone(), switched));
    let sql = std::fs::read_to_string(path).unwrap().to_ascii_uppercase();
    assert!(sql.lines().any(|line| line.starts_with("USE ")) && sql.contains("CREATE DATABASE"));
    assert!(sql.lines().any(|line| line.starts_with("LOCK TABLES ")));
    assert!(sql.lines().any(|line| line.starts_with("UNLOCK TABLES")));
    assert!(!sql.contains("FOREIGN_KEY_CHECKS") && !sql.contains("DROP TABLE"));
    assert_eq!(
        sql.lines()
            .filter(|line| line.starts_with("INSERT INTO "))
            .count(),
        2
    );

    let path = temporary.path().join("existing.sql");
    std::fs::write(&path, b"original").unwrap();
    let blocker = db.queue(Command::Query {
        sql: "SELECT SLEEP(0.4)".into(),
        limit: 1,
    });
    let cancel = db.queue(dump(path.clone(), settings));
    assert!(db.worker.cancel_dump(cancel));
    db.wait(blocker).unwrap();
    assert!(
        db.wait(cancel)
            .is_err_and(|error| error.contains("已取消 SQL 转储"))
    );
    assert_eq!(std::fs::read(path).unwrap(), b"original");
    db.sql(format!("DROP DATABASE {target}; DROP DATABASE {source}"));
}

#[test]
#[ignore = "在一次性本机 Oracle MySQL 中运行，需要 HARBOR_MYSQL_COPY_TEST_PORT"]
fn mysql_cyclic_data_copy_checks_foreign_keys_before_commit() {
    let port = std::env::var("HARBOR_MYSQL_COPY_TEST_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let connect = || {
        let mut db = Fixture {
            worker: Worker::new(),
            id: 0,
        };
        assert!(matches!(
            db.request(Command::Connect(ConnectionConfig {
                port,
                ..Default::default()
            })),
            Response::Connected(_)
        ));
        db
    };
    let mut source = connect();
    let mut target = connect();
    let server = source.sql("SELECT VERSION(), @@version_comment".into());
    assert!(
        matches!(&server[0][1], Cell::Text(comment) if comment == "MySQL Community Server - GPL")
    );
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let source_schema = format!("harbor_v13_{stamp:x}_src");
    let commit_schema = format!("harbor_v13_{stamp:x}_ok");
    let rollback_schema = format!("harbor_v13_{stamp:x}_rollback");
    let schemas = [&source_schema, &commit_schema, &rollback_schema];
    let tables = ["cycle_left", "cycle_right"];
    let rows = |db: &mut Fixture, schema: &str| {
        let schema = quote_identifier(schema);
        tables.map(|table| db.sql(format!("SELECT * FROM {schema}.{table} ORDER BY id")))
    };
    let definitions = |db: &mut Fixture, schema: &str| {
        let schema = quote_identifier(schema);
        tables.map(|table| db.sql(format!("SHOW CREATE TABLE {schema}.{table}")))
    };
    let seed_cycle = |db: &mut Fixture, schema: &str, id: u8, lookup_id: u8, label: &str| {
        let schema = quote_identifier(schema);
        db.sql(format!(
            "INSERT INTO {schema}.cycle_left VALUES ({id}, NULL, '{label}');
             INSERT INTO {schema}.cycle_right VALUES ({id}, {id}, {lookup_id}, '{label}');
             UPDATE {schema}.cycle_left SET peer_id = {id} WHERE id = {id};"
        ));
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for schema in schemas {
            let name = quote_identifier(schema);
            source.sql(format!(
                "CREATE DATABASE {name} CHARACTER SET utf8mb4;
                 CREATE TABLE {name}.lookup(id INT PRIMARY KEY) ENGINE=InnoDB;
                 CREATE TABLE {name}.cycle_left(id INT PRIMARY KEY, peer_id INT, label VARCHAR(20)) ENGINE=InnoDB;
                 CREATE TABLE {name}.cycle_right(id INT PRIMARY KEY, peer_id INT NOT NULL, lookup_id INT NOT NULL, label VARCHAR(20),
                     CONSTRAINT fk_right_left FOREIGN KEY(peer_id) REFERENCES {name}.cycle_left(id),
                     CONSTRAINT fk_right_lookup FOREIGN KEY(lookup_id) REFERENCES {name}.lookup(id)) ENGINE=InnoDB;
                 ALTER TABLE {name}.cycle_left ADD CONSTRAINT fk_left_right FOREIGN KEY(peer_id) REFERENCES {name}.cycle_right(id);
                 INSERT INTO {name}.lookup VALUES (1);"
            ));
            if schema != &rollback_schema {
                source.sql(format!("INSERT INTO {name}.lookup VALUES (2)"));
            }
        }
        seed_cycle(&mut source, &source_schema, 1, 1, "source_one");
        seed_cycle(&mut source, &source_schema, 2, 2, "source_two");
        seed_cycle(&mut source, &commit_schema, 9, 1, "old_commit");
        seed_cycle(&mut source, &rollback_schema, 9, 1, "old_rollback");

        let source_rows = rows(&mut source, &source_schema);
        assert_eq!(source_rows.each_ref().map(Vec::len), [2, 2]);
        let build_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target");
        let temporary = tempfile::Builder::new()
            .prefix("harbor-copy-cycle-")
            .tempdir_in(build_dir)
            .unwrap();
        for (schema, commits) in [(&commit_schema, true), (&rollback_schema, false)] {
            let before_rows = rows(&mut target, schema);
            let before_ddl = definitions(&mut target, schema);
            assert_eq!(before_rows.each_ref().map(Vec::len), [1, 1]);
            assert!(before_ddl.iter().all(|table| matches!(
                &table[0][1], Cell::Text(ddl) if ddl.contains("FOREIGN KEY") && ddl.contains("ENGINE=InnoDB")
            )));
            let path = temporary.path().join(format!("{schema}.sql"));
            let Response::Transferred(summary) = source.request(Command::DumpForCopy {
                schema: source_schema.clone(),
                object: None,
                objects: Some(
                    tables
                        .map(|table| DbObject {
                            schema: source_schema.clone(),
                            name: table.into(),
                            kind: ObjectKind::Table,
                        })
                        .into(),
                ),
                destination_schema: schema.clone(),
                path: path.clone(),
                options: DumpOptions {
                    include_structure: false,
                    include_data: true,
                    drop_existing: false,
                },
                bulk_insert: true,
            }) else {
                panic!("服务器未返回仅数据复制的转储结果。");
            };
            assert_eq!((summary.objects, summary.rows), (2, 4));
            assert!(summary.copy_dependencies.cyclic_data);
            let restore = target.queue(Command::RestoreForCopy {
                schema: schema.clone(),
                path,
                source_schema: source_schema.clone(),
                source: summary.copy_origin.unwrap(),
                dependencies: summary.copy_dependencies,
            });
            let outcome = target.wait(restore);
            let actual_rows = rows(&mut target, schema);
            assert_eq!(definitions(&mut target, schema), before_ddl);
            if commits {
                match outcome {
                    Ok(Response::Restored(_)) => {}
                    Ok(_) => panic!("服务器未返回复制恢复结果。"),
                    Err(error) => panic!("正常循环数据应完整提交。{error}"),
                }
                let mut expected = source_rows.clone();
                for (rows, old_rows) in expected.iter_mut().zip(before_rows) {
                    rows.extend(old_rows);
                }
                assert_eq!(actual_rows, expected);
                eprintln!("MySQL {server:?} {schema} 提交后每表 3 行，两张表定义和外键一致。");
            } else {
                let Err(message) = outcome else {
                    panic!("缺少目标查找记录的复制应在提交前失败。");
                };
                assert!(
                    message.contains("数据不满足外键")
                        && message.contains("fk_right_lookup")
                        && message.contains("未提交"),
                    "{message}"
                );
                assert_eq!(actual_rows, before_rows);
                eprintln!("MySQL {schema} 回滚后每表 1 行，旧记录和表定义一致。{message}");
            }
        }
    }));
    for schema in schemas {
        source.sql(format!(
            "DROP DATABASE IF EXISTS {}",
            quote_identifier(schema)
        ));
    }
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[test]
#[ignore = "在一次性本机 MariaDB 中运行，需要 HARBOR_DUMP_TEST_PORT"]
fn split_dump_preserves_object_restore_order() {
    let port = std::env::var("HARBOR_DUMP_TEST_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let mut db = Fixture {
        worker: Worker::new(),
        id: 0,
    };
    db.request(Command::Connect(ConnectionConfig {
        port,
        ..Default::default()
    }));
    let schema = format!("harbor_sja_split_{}", std::process::id());
    let name = quote_identifier(&schema);
    db.sql(format!(
        "CREATE DATABASE {name} CHARACTER SET utf8mb4;
         USE {name};
         CREATE TABLE z_parent(id INT PRIMARY KEY) ENGINE=InnoDB;
         CREATE TABLE a_child(id INT PRIMARY KEY, parent_id INT, label VARCHAR(20),
             FOREIGN KEY(parent_id) REFERENCES z_parent(id)) ENGINE=InnoDB;
         INSERT INTO z_parent VALUES (7);
         INSERT INTO a_child VALUES (3, 7, '拆分备份');
         CREATE VIEW z_detail AS SELECT id, label FROM a_child;
         CREATE VIEW a_summary AS SELECT label FROM z_detail;"
    ));
    let temporary =
        tempfile::tempdir_in(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target")).unwrap();
    let Response::Transferred(summary) = db.request(Command::DumpConfigured {
        schema: schema.clone(),
        path: temporary.path().into(),
        settings: DumpSettings {
            file_per_object: true,
            include_create_database: true,
            include_use: true,
            single_transaction: true,
            disable_foreign_key_checks: false,
            disable_autocommit: Some(false),
            ..Default::default()
        },
    }) else {
        panic!("服务器未返回备份结果。");
    };
    assert_eq!((summary.objects, summary.rows), (4, 2));
    let output = summary.output_directory.unwrap();
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(output.join("restore-order.json")).unwrap()).unwrap();
    assert_eq!(
        manifest,
        serde_json::json!({
            "version": 1,
            "files": [
                "00000001-prepare.sql",
                "00000002-table-z_parent.sql",
                "00000003-table-a_child.sql",
                "00000004-view-z_detail.sql",
                "00000005-view-a_summary.sql"
            ]
        })
    );
    for filename in manifest["files"].as_array().unwrap() {
        let path = output.join(filename.as_str().unwrap());
        let dump = std::fs::read_to_string(&path).unwrap();
        assert!(!dump.contains("START TRANSACTION;") && !dump.contains("COMMIT;"));
        assert!(!dump.contains("SET SESSION autocommit"));
        db.request(Command::Restore {
            schema: schema.clone(),
            path,
        });
    }
    assert_eq!(
        db.sql(format!("SELECT label FROM {name}.a_summary")),
        [vec![Cell::Text("拆分备份".into())]]
    );
    db.sql(format!("DROP DATABASE {name}"));
}

#[test]
#[ignore = "在一次性本机 MariaDB 中运行，需要 HARBOR_DUMP_TEST_PORT"]
fn configured_global_lock_split_dump_restores_gbk_files() {
    let port = std::env::var("HARBOR_DUMP_TEST_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let mut db = Fixture {
        worker: Worker::new(),
        id: 0,
    };
    assert!(matches!(
        db.request(Command::Connect(ConnectionConfig {
            port,
            ..Default::default()
        })),
        Response::Connected(_)
    ));
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let schema = format!("harbor_split_gbk_{suffix:x}");
    let restored_schema = format!("{schema}_r");
    let source = quote_identifier(&schema);
    let target = quote_identifier(&restored_schema);
    db.sql(format!(
        "CREATE DATABASE {source} CHARACTER SET utf8mb4;
         CREATE DATABASE {target} CHARACTER SET utf8mb4;
         USE {source};
         CREATE TABLE z_parent(id INT PRIMARY KEY) ENGINE=InnoDB;
         CREATE TABLE a_child(id INT PRIMARY KEY, parent_id INT, label VARCHAR(20),
             FOREIGN KEY(parent_id) REFERENCES z_parent(id)) ENGINE=InnoDB;
         INSERT INTO z_parent VALUES (7);
         INSERT INTO a_child VALUES (3, 7, '港湾');"
    ));

    let executable = std::env::current_exe().unwrap();
    let temporary = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
    let Response::Transferred(summary) = db.request(Command::DumpConfigured {
        schema,
        path: temporary.path().into(),
        settings: DumpSettings {
            file_per_object: true,
            file_charset: SqlDumpCharset::Gbk,
            lock_source_server: true,
            disable_foreign_key_checks: false,
            ..Default::default()
        },
    }) else {
        panic!("服务器未返回备份结果。");
    };
    assert_eq!((summary.objects, summary.rows), (2, 2));
    let output = summary.output_directory.unwrap();
    let parts = crate::restore::directory_sources(&output.canonicalize().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(parts.len(), 3);
    let mut bytes = 0;
    for part in parts {
        let contents = std::fs::read(part.path).unwrap();
        assert!(contents.starts_with(b"-- Harbor SQL dump encoding: gbk\n"));
        assert!(std::str::from_utf8(&contents).is_err());
        bytes += contents.len() as u64;
    }
    assert_eq!(summary.bytes, bytes);

    assert!(matches!(
        db.request(Command::Restore {
            schema: restored_schema,
            path: output,
        }),
        Response::Restored(_)
    ));
    assert_eq!(
        db.sql(format!(
            "SELECT p.id, c.id, c.label FROM {target}.a_child c
             JOIN {target}.z_parent p ON p.id = c.parent_id"
        )),
        [vec![
            Cell::Text("7".into()),
            Cell::Text("3".into()),
            Cell::Text("港湾".into()),
        ]]
    );
    db.sql(format!("DROP DATABASE {target}; DROP DATABASE {source}"));
}
