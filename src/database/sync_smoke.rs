use std::time::{SystemTime, UNIX_EPOCH};

use super::*;

struct Session(Worker, u64);

impl Session {
    fn request(&mut self, command: Command) -> Result<Response, String> {
        self.1 += 1;
        self.0.send(self.1, command)?;
        let result = self.0.recv()?;
        assert_eq!(result.id, self.1);
        result.result
    }

    fn sql(&mut self, query: String) -> Result<QueryReport, String> {
        let Response::Queried(report) = self.request(Command::Query {
            sql: query,
            limit: 10,
        })?
        else {
            return Err("未返回查询结果。".into());
        };
        if let Some(error) = report.error {
            Err(error)
        } else {
            Ok(report)
        }
    }

    fn endpoint(&mut self) -> Result<SyncEndpoint, String> {
        let Response::SyncEndpoint(endpoint) = self.request(Command::SyncEndpoint)? else {
            return Err("未返回同步连接。".into());
        };
        Ok(endpoint)
    }
}

#[test]
#[ignore = "在一次性本机 MariaDB 中运行，需要 HARBOR_SYNC_TEST_PORT"]
fn worker_applies_whole_table_and_rejects_old_endpoint() {
    let port = std::env::var("HARBOR_SYNC_TEST_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let config = ConnectionConfig {
        port,
        ..Default::default()
    };
    let mut source = Session(Worker::new(), 0);
    let mut target = Session(Worker::new(), 0);
    source.request(Command::Connect(config.clone())).unwrap();
    target.request(Command::Connect(config.clone())).unwrap();

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let schema = format!("harbor_sync_worker_{stamp:x}");
    let db = quote_identifier(&schema);
    let make = |name: &str| DbObject {
        schema: schema.clone(),
        name: name.into(),
        kind: ObjectKind::Table,
    };
    let plan = |endpoint| Command::PlanSync {
        source: make("send_table"),
        target: make("receive_table"),
        endpoint,
        mode: SyncMode::InsertAndUpdate,
    };
    let values = (1..=1_103)
        .map(|id| format!("({id}, '第{id}行')"))
        .collect::<Vec<_>>()
        .join(",");
    source
        .sql(format!("CREATE DATABASE {db} CHARACTER SET utf8mb4"))
        .unwrap();
    let result = (|| -> Result<(), String> {
        source.sql(format!(
            "CREATE TABLE {db}.send_table(id INT PRIMARY KEY, label VARCHAR(40)) ENGINE=InnoDB;
             CREATE TABLE {db}.receive_table LIKE {db}.send_table;
             INSERT INTO {db}.send_table VALUES {values};
             INSERT INTO {db}.receive_table VALUES(1, '旧值'), (1599, '目标独有');"
        ))?;
        let Response::SyncPlan(prepared) = source.request(plan(target.endpoint()?))? else {
            return Err("未收到同步计划。".into());
        };
        assert_eq!(prepared.plan.counts.source_only, 1_102);
        assert_eq!(prepared.plan.counts.conflicts, 1);
        assert_eq!(prepared.plan.counts.target_only, 1);
        target.request(Command::Disconnect)?;
        target.request(Command::Connect(config))?;
        let result = source.request(Command::ApplySync { prepared });
        assert!(
            result.is_err_and(|error| error.contains("目标连接已关闭")),
            "过期连接不得使用"
        );

        let Response::SyncPlan(prepared) = source.request(plan(target.endpoint()?))? else {
            return Err("未收到新同步计划。".into());
        };
        let Response::SyncApplied(report) = source.request(Command::ApplySync { prepared })? else {
            return Err("未收到同步结果。".into());
        };
        assert_eq!(
            (
                report.target.inserted,
                report.target.updated,
                report.target.deleted
            ),
            (1_102, 1, 0)
        );
        assert!(report.stopped.is_none() && report.uncertain.is_none());
        let report = target.sql(format!(
            "SELECT COUNT(*), MAX(IF(id=1,label,NULL)), MAX(IF(id=1599,label,NULL)) FROM {db}.receive_table"
        ))?;
        assert_eq!(
            report.sets[0].rows,
            [vec![
                Cell::Text("1104".into()),
                Cell::Text("第1行".into()),
                Cell::Text("目标独有".into()),
            ]]
        );
        Ok(())
    })();
    source.sql(format!("DROP DATABASE {db}")).unwrap();
    result.unwrap();
}

#[test]
#[ignore = "在一次性本机 MariaDB 中运行，需要 HARBOR_SYNC_TEST_PORT"]
fn worker_exports_sync_mirror_group_with_exact_values_and_foreign_key_order() {
    let config = ConnectionConfig {
        port: std::env::var("HARBOR_SYNC_TEST_PORT")
            .unwrap()
            .parse()
            .unwrap(),
        ..Default::default()
    };
    let mut source = Session(Worker::new(), 0);
    let mut target = Session(Worker::new(), 0);
    source.request(Command::Connect(config.clone())).unwrap();
    target.request(Command::Connect(config)).unwrap();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let schema = format!("harbor_sync_script_{stamp:x}");
    let db = quote_identifier(&schema);
    let table = |name: &str| DbObject {
        schema: schema.clone(),
        name: name.into(),
        kind: ObjectKind::Table,
    };
    let directory = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let script_path = directory.path().join("mirror.sql");
    source
        .sql(format!("CREATE DATABASE {db} CHARACTER SET utf8mb4"))
        .unwrap();
    let result = (|| -> Result<(), String> {
        source.sql(format!(
            "SET SESSION time_zone = '+00:00';
             CREATE TABLE {db}.source_parent(id INT PRIMARY KEY, label VARCHAR(40), amount DECIMAL(35,10),
                 raw VARBINARY(12), changed TIMESTAMP(6), rating DOUBLE, ratio FLOAT, generated INT AS (id + 1)) ENGINE=InnoDB;
             CREATE TABLE {db}.target_parent LIKE {db}.source_parent;
             CREATE TABLE {db}.source_child(id INT PRIMARY KEY, parent_id INT,
                 FOREIGN KEY (parent_id) REFERENCES {db}.source_parent(id) ON DELETE CASCADE) ENGINE=InnoDB;
             CREATE TABLE {db}.target_child(id INT PRIMARY KEY, parent_id INT,
                 FOREIGN KEY (parent_id) REFERENCES {db}.target_parent(id) ON DELETE CASCADE) ENGINE=InnoDB;
             INSERT INTO {db}.source_parent (id,label,amount,raw,changed,rating,ratio) VALUES
                 (1,'同步中文',1234567890123456789012345.6789012345, X'001A5CFF','2025-03-04 05:06:07.123456',3.14e-30,3.14),
                 (2,'新增',NULL, X'','2025-03-04 05:06:07',-2.72e15,2.72);
             INSERT INTO {db}.target_parent (id,label,amount,raw,changed,rating,ratio) VALUES
                 (1,'旧值',1,X'0100','2025-01-01 00:00:00',1.2e-40,2.17),
                 (3,'多余',3,NULL,NULL,NULL,NULL),(4,'保留',4,NULL,NULL,NULL,NULL);
             INSERT INTO {db}.source_child VALUES (20,2);
             INSERT INTO {db}.target_child VALUES (30,3);"
        ))?;
        let Response::SyncGroupPlan(prepared) = source.request(Command::PlanScopedSyncGroup {
            mappings: vec![
                SyncMapping {
                    source: table("source_parent"),
                    target: table("target_parent"),
                    filter: Some(SyncFilter::new("id < 4", SyncFilterScope::Both)?),
                    compare_columns: Vec::new(),
                },
                SyncMapping {
                    source: table("source_child"),
                    target: table("target_child"),
                    filter: None,
                    compare_columns: Vec::new(),
                },
            ],
            endpoint: target.endpoint()?,
            policy: SyncMode::MirrorReceiver.into(),
        })?
        else {
            return Err("未收到镜像计划。".into());
        };
        let Response::SyncScripts {
            prepared: SyncScriptPlan::Group(prepared),
            files,
        } = source.request(Command::ExportSync {
            prepared: SyncScriptPlan::Group(prepared),
            paths: SyncScriptPaths {
                target: Some(script_path.clone()),
                ..Default::default()
            },
        })?
        else {
            return Err("未收到已保存的同步脚本。".into());
        };
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].side, SyncSide::Target);
        assert_eq!(files[0].path, script_path);
        let read_old = format!("SELECT COUNT(*) FROM {db}.target_parent WHERE id = 3");
        assert_eq!(
            source.sql(read_old.clone())?.sets[0].rows,
            [vec![Cell::Text("1".into())]]
        );
        let script = std::fs::read_to_string(script_path).map_err(|error| error.to_string())?;
        let replay = target.sql(script)?;
        let summary = replay
            .sets
            .iter()
            .find(|set| set.columns == ["planned_rows", "changed_rows"])
            .unwrap();
        assert_eq!(
            summary.rows,
            [vec![Cell::Text("5".into()), Cell::Text("5".into())]]
        );
        let check = target.sql(format!(
            "SELECT label, amount, HEX(raw), changed, rating = 3.14e-30, generated FROM {db}.target_parent WHERE id=1;
             SELECT GROUP_CONCAT(id ORDER BY id) FROM {db}.target_parent;
             SELECT GROUP_CONCAT(id ORDER BY id) FROM {db}.target_child;"
        ))?;
        assert_eq!(
            check.sets[0].rows,
            [vec![
                Cell::Text("同步中文".into()),
                Cell::Text("1234567890123456789012345.6789012345".into()),
                Cell::Text("001A5CFF".into()),
                Cell::Text("2025-03-04 05:06:07.123456".into()),
                Cell::Text("1".into()),
                Cell::Text("2".into()),
            ]]
        );
        assert_eq!(check.sets[1].rows, [vec![Cell::Text("1,2,4".into())]]);
        assert_eq!(check.sets[2].rows, [vec![Cell::Text("20".into())]]);
        assert_eq!(
            source.sql(read_old)?.sets[0].rows,
            [vec![Cell::Text("1".into())]]
        );
        target.sql("ROLLBACK".into())?;
        let Response::SyncGroupApplied(applied) =
            source.request(Command::ApplySyncGroup { prepared })?
        else {
            return Err("未收到已保存计划的执行结果。".into());
        };
        assert!(applied.stopped.is_none() && applied.uncertain.is_none());
        assert_eq!(
            applied.target,
            SyncWrites {
                inserted: 2,
                updated: 1,
                deleted: 2
            }
        );
        Ok(())
    })();
    target.sql("ROLLBACK".into()).unwrap();
    source.sql(format!("DROP DATABASE {db}")).unwrap();
    result.unwrap();
}

#[test]
#[ignore = "在一次性本机 MariaDB 中运行，需要 HARBOR_SYNC_TEST_PORT"]
fn worker_exports_sync_scripts_for_each_bidirectional_receiver() {
    let config = ConnectionConfig {
        port: std::env::var("HARBOR_SYNC_TEST_PORT")
            .unwrap()
            .parse()
            .unwrap(),
        ..Default::default()
    };
    let mut source = Session(Worker::new(), 0);
    let mut target = Session(Worker::new(), 0);
    source.request(Command::Connect(config.clone())).unwrap();
    target.request(Command::Connect(config)).unwrap();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let schema = format!("harbor_sync_dual_script_{stamp:x}");
    let db = quote_identifier(&schema);
    let table = |name: &str| DbObject {
        schema: schema.clone(),
        name: name.into(),
        kind: ObjectKind::Table,
    };
    let directory = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    source
        .sql(format!("CREATE DATABASE {db} CHARACTER SET utf8mb4"))
        .unwrap();
    let result = (|| -> Result<(), String> {
        source.sql(format!(
            "CREATE TABLE {db}.left_table(id INT PRIMARY KEY) ENGINE=InnoDB;
             CREATE TABLE {db}.right_table LIKE {db}.left_table;
             INSERT INTO {db}.left_table VALUES (1);
             INSERT INTO {db}.right_table VALUES (2);"
        ))?;
        let Response::SyncPlan(prepared) = source.request(Command::PlanSync {
            source: table("left_table"),
            target: table("right_table"),
            endpoint: target.endpoint()?,
            mode: SyncMode::ReconcileBoth(SyncConflict::KeepSeparate),
        })?
        else {
            return Err("未收到双向计划。".into());
        };
        let Response::SyncScripts {
            prepared: SyncScriptPlan::Single(_),
            files,
        } = source.request(Command::ExportSync {
            prepared: SyncScriptPlan::Single(prepared),
            paths: SyncScriptPaths {
                source: Some(directory.path().join("source.sql")),
                target: Some(directory.path().join("target.sql")),
                replace: false,
            },
        })?
        else {
            return Err("未收到双向同步脚本。".into());
        };
        assert_eq!(files.len(), 2);
        for file in files {
            let script = std::fs::read_to_string(file.path).map_err(|error| error.to_string())?;
            let (endpoint, name) = match file.side {
                SyncSide::Source => (&mut source, "left_table"),
                SyncSide::Target => (&mut target, "right_table"),
            };
            endpoint.sql(script)?;
            let report = endpoint.sql(format!(
                "SELECT GROUP_CONCAT(id ORDER BY id) FROM {db}.{name}"
            ))?;
            assert_eq!(report.sets[0].rows, [vec![Cell::Text("1,2".into())]]);
            endpoint.sql("COMMIT".into())?;
        }
        Ok(())
    })();
    target.sql("ROLLBACK".into()).unwrap();
    source.sql("ROLLBACK".into()).unwrap();
    source.sql(format!("DROP DATABASE {db}")).unwrap();
    result.unwrap();
}
