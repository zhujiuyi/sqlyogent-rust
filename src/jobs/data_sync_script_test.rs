use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

use mysql::{Conn, OptsBuilder, prelude::Queryable};

use super::*;

#[test]
#[ignore = "requires a disposable MariaDB and PHP tunnel on loopback"]
fn standalone_sync_job_exports_reviewed_scripts_and_can_apply_the_same_changes() {
    let port = std::env::var("HARBOR_SCHEMA_FIXTURE_PORT")
        .unwrap()
        .parse::<u16>()
        .unwrap();
    let url = std::env::var("HARBOR_SCHEMA_FIXTURE_HTTP_URL").unwrap();
    assert!(url.starts_with("http://127.0.0.1:"));
    let options = OptsBuilder::new()
        .ip_or_hostname(Some("127.0.0.1"))
        .tcp_port(port)
        .user(Some("root"))
        .prefer_socket(false);
    let mut sql = Conn::new(options.clone()).unwrap();
    let source = format!("harbor_job_export_source_{}", std::process::id());
    let target = format!("harbor_job_export_target_{}", std::process::id());
    let fixture = std::env::var_os("HARBOR_SYNC_OUTPUT_FIXTURE_DIR").unwrap();
    let temp = tempfile::tempdir_in(fixture).unwrap();
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        for schema in [&source, &target] {
            sql.query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
                .unwrap();
            sql.query_drop(format!(
                "CREATE TABLE {schema}.items (
                    id INT PRIMARY KEY, label VARCHAR(40) NOT NULL
                 ) ENGINE=InnoDB DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci"
            ))
            .unwrap();
        }
        sql.query_drop(format!(
            "INSERT INTO {source}.items VALUES (1,'来源新值'),(2,'源端独有')"
        ))
        .unwrap();
        sql.query_drop(format!(
            "INSERT INTO {target}.items VALUES (1,'目标旧值'),(3,'目标独有')"
        ))
        .unwrap();
        sql.query_drop(format!(
            "CREATE TABLE {source}.write_settings (endpoint VARCHAR(10), checks_enabled INT) ENGINE=InnoDB"
        ))
        .unwrap();
        for (side, schema) in [("source", &source), ("target", &target)] {
            sql.query_drop(format!(
                "CREATE TRIGGER {schema}.record_fk_check AFTER INSERT ON {schema}.items
                 FOR EACH ROW INSERT INTO {source}.write_settings
                 VALUES ('{side}', @@SESSION.foreign_key_checks)"
            ))
            .unwrap();
        }
        let rows = |conn: &mut Conn, schema: &str| -> Vec<(u64, String)> {
            conn.query(format!("SELECT id,label FROM {schema}.items ORDER BY id"))
                .unwrap()
        };
        let source_before = rows(&mut sql, &source);
        let target_before = rows(&mut sql, &target);
        let expected = [
            (1, "来源新值".into()),
            (2, "源端独有".into()),
            (3, "目标独有".into()),
        ];

        let job_path = temp.path().join("同步任务.json");
        let script_root = temp.path().join("同步脚本");
        fs::create_dir(&script_root).unwrap();
        let script_root = script_root.canonicalize().unwrap();
        let log_path = temp.path().join("runs.jsonl");
        let native = json!({"host": "127.0.0.1", "port": port, "user": "root"});
        let mut tunneled = native.clone();
        tunneled["http"] = json!({"url":url,"force_base64":true,"utf8mb4":true});
        let mut definition = json!({
            "version":1,
            "connection":native,
            "job":{
                "type":"data_sync",
                "source_schema":source,
                "target_schema":target,
                "target":tunneled,
                "all_tables":true,
                "mode":"reconcile_both",
                "conflict":"source_wins",
                "skip_target_fk_checks":true,
                "apply":false,
                "script_directory":"同步脚本"
            },
            "log":"runs.jsonl"
        });
        let latest = || {
            let log = fs::read_to_string(&log_path).unwrap();
            let entry: Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
            assert_eq!(entry["event"], "succeeded");
            entry
        };
        let script_pairs = |entry: &Value| {
            let files = entry["script_files"].as_array().unwrap();
            assert_eq!(files.len(), 2);
            let script = |side: &str| {
                PathBuf::from(
                    files.iter().find(|file| file["side"] == side).unwrap()["path"]
                        .as_str()
                        .unwrap(),
                )
            };
            let source = script("source");
            let target = script("target");
            assert_eq!(source.parent(), target.parent());
            assert_eq!(
                source.parent().and_then(Path::parent),
                Some(script_root.as_path())
            );
            [source, target]
        };

        fs::write(&job_path, serde_json::to_vec(&definition).unwrap()).unwrap();
        assert_eq!(
            cli([OsString::from("check"), job_path.clone().into_os_string()]),
            0
        );
        run_fixture(&job_path).unwrap();
        let preview = latest();
        assert_eq!(preview["skip_target_fk_checks"], true);
        assert_eq!(preview["planned_source"]["inserted"], 1);
        assert_eq!(preview["planned_target"]["inserted"], 1);
        assert_eq!(preview["planned_target"]["updated"], 1);
        assert_eq!(preview["source_writes"]["inserted"], 0);
        assert_eq!(preview["target_writes"]["updated"], 0);
        assert_eq!(rows(&mut sql, &source), source_before);
        assert_eq!(rows(&mut sql, &target), target_before);
        let published = script_pairs(&preview);
        let original_scripts = published
            .each_ref()
            .map(|path| fs::read_to_string(path).unwrap());
        for (schema, script) in [
            (&source, &original_scripts[0]),
            (&target, &original_scripts[1]),
        ] {
            let mut replay = Conn::new(options.clone()).unwrap();
            let mut statements = crate::sql_tools::StatementReader::new(script);
            while let Some(statement) = statements.next_statement() {
                replay.query_drop(statement.sql).unwrap();
            }
            assert_eq!(
                replay
                    .query_first::<u8, _>("SELECT @harbor_sync_ok")
                    .unwrap(),
                Some(1)
            );
            assert_eq!(rows(&mut replay, schema), expected);
            assert_eq!(
                replay
                    .query_first::<u8, _>("SELECT @@SESSION.foreign_key_checks")
                    .unwrap(),
                Some(1)
            );
            replay.query_drop("ROLLBACK").unwrap();
        }

        definition["job"]["apply"] = json!(true);
        fs::write(&job_path, serde_json::to_vec(&definition).unwrap()).unwrap();
        run_fixture(&job_path).unwrap();
        let applied = latest();
        let applied_paths = script_pairs(&applied);
        assert_ne!(applied_paths[0].parent(), published[0].parent());
        assert_eq!(applied["source_writes"]["inserted"], 1);
        assert_eq!(applied["target_writes"]["inserted"], 1);
        assert_eq!(applied["target_writes"]["updated"], 1);
        for (path, previous) in published.iter().zip(original_scripts) {
            assert_eq!(fs::read_to_string(path).unwrap(), previous);
        }
        assert_eq!(rows(&mut sql, &source), expected);
        assert_eq!(rows(&mut sql, &target), expected);
        assert_eq!(
            sql.query::<(String, u8), _>(format!(
                "SELECT endpoint,checks_enabled FROM {source}.write_settings ORDER BY endpoint"
            ))
            .unwrap(),
            [("source".to_owned(), 1), ("target".to_owned(), 0)]
        );

        run_fixture(&job_path).unwrap();
        let repeat = latest();
        assert!(repeat["script_files"].is_null() || repeat["script_files"] == json!([]));
        assert_eq!(fs::read_dir(&script_root).unwrap().count(), 2);
    }));
    for schema in [&source, &target] {
        sql.query_drop(format!("DROP DATABASE IF EXISTS {schema}"))
            .unwrap();
    }
    if let Err(error) = outcome {
        resume_unwind(error);
    }
}
