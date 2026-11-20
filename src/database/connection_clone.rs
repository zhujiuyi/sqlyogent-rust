use mysql::prelude::Queryable;

use super::{
    Connection, ConnectionConfig, DbResult, Failure, HttpTunnelConfig, Response, ServerInfo,
    SharedLifetime, Worker, connect, http_session, mysql_error,
};

pub(super) enum Settings {
    Native {
        config: ConnectionConfig,
        current_database: Option<String>,
    },
    Http(ConnectionConfig, HttpTunnelConfig),
}

impl Settings {
    fn capture(connection: &mut Connection) -> DbResult<Self> {
        match connection {
            Connection::Native(source) => {
                let current_database = source
                    .conn
                    .query_first_opt::<Option<String>, _>("SELECT DATABASE()")?
                    .transpose()
                    .map_err(mysql::Error::from)?
                    .ok_or_else(|| Failure::Message("服务器未返回当前数据库。".into()))?;
                let mut config = source.config.clone();
                if config.advanced.init_sql.trim().is_empty() {
                    config.database.clone_from(&current_database);
                }
                if let Some(ssh) = &mut config.ssh {
                    // The live source owns the configured local port. A fresh tunnel
                    // must also remain usable after the source is disconnected.
                    ssh.local_port = None;
                }
                Ok(Self::Native {
                    config,
                    current_database,
                })
            }
            Connection::Http(source) => {
                let (config, tunnel) = source.clone_settings();
                Ok(Self::Http(config, tunnel))
            }
        }
    }

    pub(super) fn open(
        self,
        lifetime: &SharedLifetime,
    ) -> Result<(Connection, ServerInfo), String> {
        match self {
            Self::Native {
                config,
                current_database,
            } => {
                // Preserve the context in which the original initialization ran.
                // Restore the source's current database only after initialization.
                let (mut connection, mut info) = connect(config, lifetime, true)?;
                if info.database != current_database {
                    let database = current_database
                        .as_deref()
                        .ok_or("原连接未选择默认数据库，初始化 SQL 选择了数据库。")?;
                    connection.conn.select_db(database).map_err(mysql_error)?;
                    info.database = current_database;
                }
                Ok((Connection::Native(connection), info))
            }
            Self::Http(config, tunnel) => {
                let (connection, info) = http_session::Session::open(config, tunnel)?;
                Ok((Connection::Http(connection), info))
            }
        }
    }
}

pub(super) fn start(connection: &mut Option<Connection>) -> Result<Response, String> {
    let active = connection.as_mut().ok_or("请先连接数据库服务器。")?;
    let settings = match Settings::capture(active) {
        Ok(settings) => settings,
        Err(error) => {
            if error.lost_connection() {
                *connection = None;
            }
            return Err(error.message());
        }
    };
    let request = 1;
    Ok(Response::ClonedConnection {
        worker: Worker::spawn(Some((request, settings))),
        request,
    })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use mysql::Conn;

    use super::*;
    use crate::database::http_session::workflow_test::{
        TestGateway, archived_tunnel, fixture_database,
    };
    use crate::database::{Cell, Command, QueryReport, quote_identifier};

    struct Fixture {
        admin: Conn,
        databases: [String; 2],
        account: String,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self
                .admin
                .query_drop(format!("DROP USER IF EXISTS {}", self.account));
            for database in &self.databases {
                let _ = self.admin.query_drop(format!(
                    "DROP DATABASE IF EXISTS {}",
                    quote_identifier(database)
                ));
            }
        }
    }

    fn request(worker: &Worker, id: u64, command: Command) -> Response {
        worker.send(id, command).unwrap();
        let result = worker
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert_eq!(result.id, id);
        result.result.unwrap()
    }

    fn query(worker: &Worker, id: u64, sql: impl Into<String>) -> QueryReport {
        let Response::Queried(report) = request(
            worker,
            id,
            Command::Query {
                sql: sql.into(),
                limit: 10,
            },
        ) else {
            panic!("expected query result");
        };
        assert!(report.error.is_none(), "{:?}", report.error);
        report
    }

    fn cloned(source: &Worker, id: u64) -> (Worker, ServerInfo, u64) {
        let Response::ClonedConnection { worker, request } =
            request(source, id, Command::CloneConnection)
        else {
            panic!("expected new worker");
        };
        let event = worker
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert_eq!(event.id, request);
        let Response::Connected(info) = event.result.unwrap() else {
            panic!("expected connection info");
        };
        (worker, info, request + 1)
    }

    #[test]
    #[ignore = "requires disposable MariaDB via HARBOR_HTTP_FIXTURE_PORT"]
    fn clones_runtime_settings_into_independent_connections() {
        let (port, opts) = fixture_database();
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let schema = format!("harbor_http_clone_{stamp:x}");
        let user = format!("hc_{stamp:x}");
        let mut fixture = Fixture {
            admin: Conn::new(opts.clone()).unwrap(),
            databases: [format!("{schema}_a"), format!("{schema}_b")],
            account: format!("'{user}'@'127.0.0.1'"),
        };
        for database in &fixture.databases {
            let name = quote_identifier(database);
            fixture
                .admin
                .query_drop(format!("CREATE DATABASE {name} CHARACTER SET utf8mb4"))
                .unwrap();
            fixture
                .admin
                .query_drop(format!(
                    "CREATE TABLE {name}.probe (id INT PRIMARY KEY) ENGINE=InnoDB"
                ))
                .unwrap();
        }
        let password = "temporary-clone-test";
        fixture
            .admin
            .query_drop(format!(
                "CREATE USER {} IDENTIFIED BY '{password}'",
                fixture.account
            ))
            .unwrap();
        let [initial, current] = &fixture.databases;
        for database in [initial, current] {
            fixture
                .admin
                .query_drop(format!(
                    "GRANT SELECT, INSERT ON {}.* TO {}",
                    quote_identifier(database),
                    fixture.account
                ))
                .unwrap();
        }

        let source = Worker::new();
        let config = ConnectionConfig {
            port,
            user: user.clone(),
            password: password.into(),
            database: Some(initial.clone()),
            advanced: crate::database::AdvancedConnection {
                init_sql: "SET @harbor_clone_initial_db = DATABASE()".into(),
                sql_mode: Some("NO_BACKSLASH_ESCAPES".into()),
                wait_timeout: Some(86),
                ..Default::default()
            },
            ..Default::default()
        };
        let Response::Connected(source_info) = request(&source, 1, Command::Connect(config)) else {
            panic!("expected source connection");
        };
        query(&source, 2, format!("USE {}", quote_identifier(current)));
        query(
            &source,
            3,
            "SET @harbor_clone_source_only = 1; START TRANSACTION; INSERT INTO probe VALUES (7)",
        );
        let (clone, cloned_info, next) = cloned(&source, 4);
        assert_ne!(source_info.connection_id, cloned_info.connection_id);
        assert_eq!(cloned_info.database.as_ref(), Some(current));
        assert_eq!(
            query(&clone, next, "SELECT DATABASE(), @harbor_clone_initial_db, @harbor_clone_source_only, @@SESSION.wait_timeout, @@SESSION.sql_mode, (SELECT COUNT(*) FROM probe)").sets[0].rows,
            [vec![
                Cell::Text(current.clone()), Cell::Text(initial.clone()), Cell::Null,
                Cell::Text("86".into()), Cell::Text("NO_BACKSLASH_ESCAPES".into()), Cell::Text("0".into())
            ]]
        );
        assert!(matches!(
            request(&source, 5, Command::Disconnect),
            Response::Disconnected
        ));
        assert_eq!(
            query(&clone, next + 1, "SELECT COUNT(*) FROM probe").sets[0].rows,
            [vec![Cell::Text("0".into())]]
        );
        request(&clone, next + 2, Command::Disconnect);

        let gateway = TestGateway::new(opts);
        let mut transports = vec![(gateway.tunnel.clone(), "root".into(), String::new())];
        if let Some(archive) = archived_tunnel() {
            transports.push((archive, user, password.into()));
        }
        for (tunnel, user, password) in transports {
            let source = Worker::new();
            let config = ConnectionConfig {
                port,
                user,
                password,
                database: Some(initial.clone()),
                ..Default::default()
            };
            assert!(matches!(
                request(&source, 1, Command::ConnectHttp(config, tunnel)),
                Response::Connected(_)
            ));
            request(
                &source,
                2,
                Command::UseSchema {
                    schema: current.clone(),
                },
            );
            let (clone, info, next) = cloned(&source, 3);
            assert!(info.http_tunnel);
            assert_eq!(info.database.as_ref(), Some(current));
            request(
                &source,
                4,
                Command::UseSchema {
                    schema: initial.clone(),
                },
            );
            assert!(matches!(
                request(&source, 5, Command::Disconnect),
                Response::Disconnected
            ));
            assert_eq!(
                query(
                    &clone,
                    next,
                    "SELECT DATABASE(), @@character_set_connection"
                )
                .sets[0]
                    .rows,
                [vec![
                    Cell::Text(current.clone()),
                    Cell::Text("utf8mb4".into())
                ]]
            );
            request(&clone, next + 1, Command::Disconnect);
        }
        gateway.stop();
    }
}
