use std::time::{SystemTime, UNIX_EPOCH};

use super::*;

struct Client(Worker, u64);

impl Client {
    fn request(&mut self, command: Command) -> Result<Response, String> {
        self.1 += 1;
        self.0.send(self.1, command)?;
        let event = self.0.recv()?;
        if event.id != self.1 {
            return Err(format!("意外的连接事件：{}", event.id));
        }
        event.result
    }

    fn query(&mut self, sql: String) -> Result<QueryReport, String> {
        let Response::Queried(report) = self.request(Command::Query { sql, limit: 20 })? else {
            return Err("未返回查询结果。".into());
        };
        if let Some(error) = report.error {
            Err(error)
        } else {
            Ok(report)
        }
    }
}

#[test]
#[ignore = "在一次性 TLS MariaDB 中验证，需要 HARBOR_CONNECTION_TEST_PORT/CA/CERT/KEY"]
fn establishes_certified_compressed_connection_and_idle_keep_alive() {
    let port = std::env::var("HARBOR_CONNECTION_TEST_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let ca_path = PathBuf::from(std::env::var_os("HARBOR_CONNECTION_TEST_CA").unwrap());
    let certificate = PathBuf::from(std::env::var_os("HARBOR_CONNECTION_TEST_CERT").unwrap());
    let private_key = PathBuf::from(std::env::var_os("HARBOR_CONNECTION_TEST_KEY").unwrap());
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let schema = format!("harbor_conn_{stamp:x}");
    let user = format!("hcert_{stamp:x}");
    let account = format!("'{user}'@'127.0.0.1'");
    let db = quote_identifier(&schema);
    let mut admin = Client(Worker::new(), 0);
    admin
        .request(Command::Connect(ConnectionConfig {
            port,
            ..Default::default()
        }))
        .unwrap();
    let result = (|| -> Result<(), String> {
        admin.query(format!(
            "CREATE DATABASE {db};
             CREATE TABLE {db}.connected(id INT PRIMARY KEY) ENGINE=InnoDB;
             CREATE USER {account} REQUIRE X509;
             GRANT SELECT, INSERT ON {db}.* TO {account}"
        ))?;
        let config = ConnectionConfig {
            port,
            user,
            database: Some(schema),
            tls: Some(TlsConfig {
                ca_path: Some(ca_path),
                client_identity: Some(TlsClientIdentity {
                    certificate,
                    private_key,
                }),
            }),
            advanced: AdvancedConnection {
                compression: true,
                wait_timeout: Some(3),
                keep_alive: Some(1),
                sql_mode: Some("NO_BACKSLASH_ESCAPES".into()),
                init_sql: "INSERT INTO connected VALUES(1); SET @harbor_initialized = 'present';"
                    .into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut session = Client(Worker::new(), 0);
        let mut without_identity = config.clone();
        without_identity.tls.as_mut().unwrap().client_identity = None;
        assert!(
            session
                .request(Command::TestConnection {
                    config: without_identity,
                    http: None
                })
                .is_err()
        );
        let Response::ConnectionTest(probe) = session.request(Command::TestConnection {
            config: config.clone(),
            http: None,
        })?
        else {
            return Err("未返回连接测试结果。".into());
        };
        assert!(probe.transport.contains("TLS"));
        assert_eq!(probe.compression, Some(true));
        assert_eq!(
            admin
                .query(format!("SELECT COUNT(*) FROM {db}.connected"))?
                .sets[0]
                .rows,
            [vec![Cell::Text("0".into())]],
            "连接测试不执行初始化写入"
        );

        let Response::Connected(info) = session.request(Command::Connect(config))? else {
            return Err("未返回连接信息。".into());
        };
        std::thread::sleep(Duration::from_secs(4));
        let report = session.query("SELECT CONNECTION_ID(), @@SESSION.wait_timeout, @@SESSION.sql_mode, @harbor_initialized, (SELECT COUNT(*) FROM connected)".into())?;
        assert_eq!(
            report.sets[0].rows,
            [vec![
                Cell::Text(info.connection_id.to_string()),
                Cell::Text("3".into()),
                Cell::Text("NO_BACKSLASH_ESCAPES".into()),
                Cell::Text("present".into()),
                Cell::Text("1".into()),
            ]]
        );
        let Response::TablePage(page) = session.request(Command::TablePage {
            object: DbObject {
                schema: info.database.unwrap(),
                name: "connected".into(),
                kind: ObjectKind::Table,
            },
            limit: 10,
            offset: 0,
        })?
        else {
            return Err("辅助连接未返回表数据。".into());
        };
        assert_eq!(page.rows.len(), 1, "辅助连接不重复初始化脚本");
        session.request(Command::Disconnect)?;
        Ok(())
    })();
    admin
        .query(format!(
            "DROP USER IF EXISTS {account}; DROP DATABASE IF EXISTS {db}"
        ))
        .unwrap();
    result.unwrap();
}
