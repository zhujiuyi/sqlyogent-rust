use std::{
    collections::BTreeSet,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use mysql::{Conn, OptsBuilder, prelude::Queryable};

use crate::database::http_session::workflow_test::{TestGateway, archived_tunnel};
use crate::database::{Command, ConnectionConfig, Response, Worker, quote_identifier};

use super::{
    AccountCommand, AccountName, AccountReply, GrantScope, GrantSnapshot, PrivilegeChanges,
};

struct Fixture {
    conn: Conn,
    database: String,
    initial: AccountName,
    renamed: AccountName,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for account in [&self.initial, &self.renamed] {
            let _ = self
                .conn
                .query_drop(format!("DROP USER IF EXISTS {}", account.label()));
        }
        let _ = self.conn.query_drop(format!(
            "DROP DATABASE IF EXISTS {}",
            quote_identifier(&self.database)
        ));
    }
}

#[test]
#[ignore = "在一次性本机 MariaDB 中运行，需要 HARBOR_ACCOUNT_TEST_PORT"]
fn account_lifecycle_and_scope_edits() {
    account_lifecycle(Transport::Native);
}

#[test]
#[ignore = "在一次性本机 MariaDB 和本地协议代理中运行，需要 HARBOR_ACCOUNT_TEST_PORT"]
fn account_lifecycle_and_scope_edits_over_http() {
    account_lifecycle(Transport::Proxy);
}

#[test]
#[ignore = "需要一次性 MariaDB 和 HARBOR_LEGACY_TUNNEL_URL 指定的本机 PHP 脚本"]
fn account_lifecycle_and_scope_edits_over_archived_php() {
    account_lifecycle(Transport::ArchivedPhp);
}

enum Transport {
    Native,
    Proxy,
    ArchivedPhp,
}

fn account_lifecycle(transport: Transport) {
    let port = std::env::var("HARBOR_ACCOUNT_TEST_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let host = "127.0.0.1";
    let opts = OptsBuilder::default()
        .ip_or_hostname(Some(host))
        .tcp_port(port)
        .prefer_socket(false);
    let gateway = matches!(transport, Transport::Proxy)
        .then(|| TestGateway::new(opts.clone().user(Some("root")).into()));
    let mut fixture = Fixture {
        conn: Conn::new(opts.clone().user(Some("root"))).unwrap(),
        database: format!("harbor_acl_{stamp:x}"),
        initial: AccountName {
            user: format!("harbor_a`{stamp:x}"),
            host: host.into(),
        },
        renamed: AccountName {
            user: format!("harbor_b'{stamp:x}"),
            host: "%".into(),
        },
    };
    let worker = Worker::new();
    let mut next_request = 0;
    let mut request = |command| {
        next_request += 1;
        worker.send(next_request, command).unwrap();
        let response = worker
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert_eq!(response.id, next_request);
        response.result.unwrap()
    };
    let config = ConnectionConfig {
        port,
        ..Default::default()
    };
    let connect = match transport {
        Transport::Native => Command::Connect(config),
        Transport::Proxy => Command::ConnectHttp(config, gateway.as_ref().unwrap().tunnel.clone()),
        Transport::ArchivedPhp => Command::ConnectHttp(
            config,
            archived_tunnel()
                .expect("must set HARBOR_LEGACY_TUNNEL_URL to a disposable loopback endpoint"),
        ),
    };
    let Response::Connected(_) = request(connect) else {
        panic!("server did not connect");
    };
    let mut account_request = |command| {
        let Response::Accounts(reply) = request(Command::Accounts(command)) else {
            panic!("missing account response")
        };
        reply.unwrap()
    };

    let first_password = format!("Temporary'{stamp:x}\\a!");
    let next_password = format!("Replaced\\'{stamp:x}b!");
    let initial = fixture.initial.clone();
    let renamed = fixture.renamed.clone();
    let global_privileges = BTreeSet::from(["CREATE VIEW".into(), "SHOW VIEW".into()]);
    let AccountReply::Privileges(available) = account_request(AccountCommand::ListPrivileges)
    else {
        panic!("missing server privilege catalog")
    };
    assert!(global_privileges.is_subset(&available));
    let AccountReply::Changed {
        selected, warning, ..
    } = account_request(AccountCommand::Create {
        account: initial.clone(),
        password: first_password.clone(),
        privileges: global_privileges.clone(),
        grant_option: true,
    })
    else {
        panic!("account was not created")
    };
    assert_eq!(selected.as_ref(), Some(&initial));
    assert_eq!(warning, None);
    assert!(
        Conn::new(
            opts.clone()
                .user(Some(&initial.user))
                .pass(Some(&first_password))
        )
        .is_ok(),
        "created account cannot authenticate"
    );

    let db = quote_identifier(&fixture.database);
    let on_db = quote_identifier(&fixture.database.replace('_', "\\_"));
    let account = initial.label();
    for sql in [
        format!("CREATE DATABASE {db}"),
        format!("CREATE TABLE {db}.orders (id INT, amount INT)"),
        format!("CREATE PROCEDURE {db}.read_order() SELECT 1"),
        format!("GRANT ALL PRIVILEGES ON {on_db}.* TO {account}"),
        format!("GRANT UPDATE, SELECT (id, amount) ON {db}.orders TO {account}"),
        format!("GRANT EXECUTE ON PROCEDURE {db}.read_order TO {account}"),
    ] {
        fixture.conn.query_drop(sql).unwrap();
    }
    let AccountReply::Read(original) = account_request(AccountCommand::Read(initial.clone()))
    else {
        panic!("missing grants")
    };
    let db_scope = original.literal_scope(GrantScope::Database(fixture.database.clone()));
    let table = GrantScope::Table {
        database: fixture.database.clone(),
        table: "orders".into(),
    };
    let column = GrantScope::Column {
        database: fixture.database.clone(),
        table: "orders".into(),
        column: "id".into(),
    };
    let routine = GrantScope::Routine {
        database: fixture.database.clone(),
        name: "read_order".into(),
        function: false,
    };
    assert_eq!(
        original.scopes[&db_scope].names,
        original.choices(&db_scope).cloned().collect()
    );
    assert_eq!(
        original.scopes[&GrantScope::Global].names,
        global_privileges
    );
    assert!(original.scopes[&GrantScope::Global].grant_option);
    assert!(original.scopes[&routine].names.contains("EXECUTE"));
    let mut apply = |original: GrantSnapshot, changes: PrivilegeChanges| {
        let AccountReply::Applied(result) =
            account_request(AccountCommand::Apply { original, changes })
        else {
            panic!("missing privilege apply result")
        };
        assert_eq!(result.error, None);
        assert_eq!(result.completed, result.total);
        result.refreshed.unwrap()
    };
    let table_after = apply(
        original,
        PrivilegeChanges {
            scope: table.clone(),
            names: BTreeSet::from(["INSERT".into()]),
            grant_option: true,
        },
    );
    assert_eq!(
        table_after.scopes[&table].names,
        BTreeSet::from(["INSERT".into()])
    );
    assert!(table_after.scopes[&table].grant_option);
    assert!(table_after.scopes[&column].names.contains("SELECT"));

    let column_after = apply(
        table_after,
        PrivilegeChanges {
            scope: column.clone(),
            names: BTreeSet::from(["UPDATE".into()]),
            grant_option: false,
        },
    );
    assert_eq!(
        column_after.scopes[&column].names,
        BTreeSet::from(["UPDATE".into()])
    );
    assert_eq!(
        column_after.scopes[&db_scope].names,
        column_after.choices(&db_scope).cloned().collect()
    );

    let cleared = apply(
        column_after,
        PrivilegeChanges {
            scope: GrantScope::Global,
            names: BTreeSet::new(),
            grant_option: false,
        },
    );
    assert!(cleared.scopes[&GrantScope::Global].names.is_empty());
    assert!(!cleared.scopes[&GrantScope::Global].grant_option);
    assert!(cleared.scopes[&routine].names.contains("EXECUTE"));

    assert!(matches!(
        account_request(AccountCommand::Rename {
            account: initial,
            target: renamed.clone()
        }),
        AccountReply::Changed { .. }
    ));
    assert!(matches!(
        account_request(AccountCommand::Password {
            account: renamed.clone(),
            password: next_password.clone()
        }),
        AccountReply::Changed { .. }
    ));
    assert!(
        Conn::new(opts.user(Some(&renamed.user)).pass(Some(&next_password))).is_ok(),
        "renamed account cannot authenticate with the updated password"
    );
    let AccountReply::Read(renamed_grants) = account_request(AccountCommand::Read(renamed.clone()))
    else {
        panic!("missing grants after rename")
    };
    assert_eq!(renamed_grants.scopes, cleared.scopes);

    assert!(matches!(
        account_request(AccountCommand::Drop(renamed.clone())),
        AccountReply::Changed { .. }
    ));
    let AccountReply::Listed(accounts) = account_request(AccountCommand::List) else {
        panic!("missing account list")
    };
    assert!(accounts.iter().any(|account| account.user == "root"));
    assert!(!accounts.contains(&renamed));
    if let Some(gateway) = gateway {
        gateway.stop();
    }
}
