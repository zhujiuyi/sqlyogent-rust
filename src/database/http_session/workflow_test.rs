use std::fmt::Write as _;
use std::net::TcpListener;
use std::num::NonZeroU32;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::STANDARD};
use mysql::{Conn, Opts, OptsBuilder, Row, prelude::Queryable};
use quick_xml::escape::escape;
use serde::Deserialize;

use super::super::{
    FieldValue, TableEdit, Worker,
    transfer::{
        self, CsvOptions, DumpOptions, DumpSettings, ExcelOptions, ExportKind, ExportSettings,
        ResultSqlTarget, SqlExportMode,
    },
};
use super::tests::{read_request, send_reply};
use super::*;

pub(in crate::database) fn fixture_database() -> (u16, Opts) {
    let port = std::env::var("HARBOR_HTTP_FIXTURE_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let opts = Opts::from(
        OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(port)
            .user(Some("root"))
            .prefer_socket(false),
    );
    (port, opts)
}

pub(in crate::database) fn archived_tunnel() -> Option<HttpTunnelConfig> {
    let url = std::env::var("HARBOR_LEGACY_TUNNEL_URL").ok()?;
    let parsed = reqwest::Url::parse(&url).unwrap();
    assert_eq!(parsed.scheme(), "http");
    assert_eq!(parsed.host_str(), Some("127.0.0.1"));
    assert!(
        parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none()
    );
    Some(HttpTunnelConfig {
        url,
        force_base64: true,
        use_utf8mb4: true,
        ..Default::default()
    })
}

#[test]
#[ignore = "需要一次性 MariaDB 11.4 和归档 PHP 隧道，使用 HARBOR_HTTP_FIXTURE_PORT/LEGACY_TUNNEL_URL"]
fn refreshes_all_server_resources_through_native_and_archived_http() {
    use crate::maintenance::{FlushCapabilities, FlushPlan, FlushTarget};

    let (port, opts) = fixture_database();
    let tunnel = archived_tunnel().expect("需要 HARBOR_LEGACY_TUNNEL_URL");
    let mut inspection = Conn::new(opts.clone()).unwrap();
    let schema = format!("harbor_flush_{}", std::process::id());
    inspection
        .query_drop(format!("CREATE DATABASE {schema}"))
        .unwrap();
    let _cleanup = DatabaseCleanup {
        opts,
        name: schema.clone(),
    };
    let table = format!("{schema}.lock_probe");
    inspection
        .query_drop(format!(
            "CREATE TABLE {table}(id INT PRIMARY KEY) ENGINE=InnoDB"
        ))
        .unwrap();
    let flush_count = |conn: &mut Conn| {
        let (_, count): (String, u64) = conn
            .query_first("SHOW GLOBAL STATUS LIKE 'Com_flush'")
            .unwrap()
            .unwrap();
        count
    };
    let config = ConnectionConfig {
        port,
        ..Default::default()
    };
    for (index, connect) in [
        Command::Connect(config.clone()),
        Command::ConnectHttp(config, tunnel),
    ]
    .into_iter()
    .enumerate()
    {
        let worker = Worker::new();
        let mut id = 0;
        let mut request = |command| {
            id += 1;
            worker.send(id, command).unwrap();
            let event = worker.recv().unwrap();
            assert_eq!(event.id, id);
            event.result.unwrap()
        };
        let Response::Connected(server) = request(connect) else {
            panic!("未返回连接信息");
        };
        let plan = FlushPlan::new(
            FlushCapabilities::from_version(&server.version),
            &[false; FlushTarget::ALL.len()],
            true,
            true,
        )
        .unwrap();
        if !server.http_tunnel {
            let Response::Queried(report) = request(Command::Query {
                sql: "START TRANSACTION".into(),
                limit: 1,
            }) else {
                panic!("未返回事务状态");
            };
            assert!(report.error.is_none());
        }
        let before = flush_count(&mut inspection);
        assert!(matches!(
            request(Command::FlushResources(plan)),
            Response::ServerResourcesFlushed(9)
        ));
        assert!(flush_count(&mut inspection) >= before + 9);
        if !server.http_tunnel {
            let Response::Queried(report) = request(Command::Query {
                sql: "SELECT @@in_transaction".into(),
                limit: 1,
            }) else {
                panic!("未返回事务状态");
            };
            assert_eq!(report.sets[0].rows, [vec![Cell::Text("1".into())]]);
        }
        inspection
            .query_drop(format!("INSERT INTO {table} VALUES ({index})"))
            .unwrap();
        request(Command::Disconnect);
    }
}

#[test]
#[ignore = "需要一次性 MariaDB，端口由 HARBOR_HTTP_FIXTURE_PORT 设置"]
fn imports_custom_delimited_text_through_native_and_http() {
    use crate::database::{OdbcConflict, TextImport};
    use crate::sql_tools::delimited::{Options, SkipMode, parse};

    let (port, opts) = fixture_database();
    let mut sql = Conn::new(opts.clone()).unwrap();
    let schema = format!("harbor_http_text_import_{}", std::process::id());
    sql.query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
        .unwrap();
    let _cleanup = DatabaseCleanup {
        opts: opts.clone(),
        name: schema.clone(),
    };
    let gateway = TestGateway::new(opts);
    let config = ConnectionConfig {
        port,
        database: Some(schema.clone()),
        ..Default::default()
    };
    let input = concat!(
        "说明||'跨<记录>行'<记录>内容||标识||值||丢弃<记录>",
        "'甲||乙^'丙<记录>丁'||1||~||不要<记录>",
        "'戊''己'||2||^N||不要<记录>",
        "普通文本||3||'~'||不要<记录>",
    );
    let data = parse(
        input.as_bytes(),
        &Options {
            field_end: "||".into(),
            row_end: "<记录>".into(),
            enclosure: "'".into(),
            escape: "^".into(),
            skip: 1,
            skip_mode: SkipMode::Records,
            null: "~".into(),
            quoted_null: false,
            ..Options::default()
        },
    )
    .unwrap();
    assert_eq!(data.headers.unwrap(), ["内容", "标识", "值", "丢弃"]);
    let rows: Vec<_> = data
        .rows
        .into_iter()
        .map(|mut row| {
            row.pop();
            row
        })
        .collect();

    for (name, connect) in [
        ("native_import", Command::Connect(config.clone())),
        (
            "http_import",
            Command::ConnectHttp(config, gateway.tunnel.clone()),
        ),
    ] {
        let qualified = format!("{schema}.{name}");
        sql.query_drop(format!(
            "CREATE TABLE {qualified} (id INT PRIMARY KEY, note VARCHAR(60),
               optional VARCHAR(20), status VARCHAR(20) DEFAULT '初始') ENGINE=InnoDB"
        ))
        .unwrap();
        sql.query_drop(format!(
            "INSERT INTO {qualified} (id, note, status) VALUES (1, '原值', '保留')"
        ))
        .unwrap();
        let object = DbObject {
            schema: schema.clone(),
            name: name.into(),
            kind: ObjectKind::Table,
        };
        let worker = Worker::new();
        let mut id = 0;
        let mut request = |command| {
            id += 1;
            worker.send(id, command).unwrap();
            let event = worker
                .recv_timeout(Duration::from_secs(12))
                .unwrap()
                .unwrap();
            assert_eq!(event.id, id);
            event
                .result
                .unwrap_or_else(|error| panic!("导入请求 {name} #{id} 失败。{error}"))
        };
        assert!(matches!(request(connect), Response::Connected(_)));
        let Response::Inspected(detail) = request(Command::Inspect {
            object: object.clone(),
        }) else {
            panic!("没有返回目标表元数据。");
        };
        let import = |rows, conflict| {
            Command::ImportDelimitedText(TextImport {
                object: object.clone(),
                columns: vec!["note".into(), "id".into(), "optional".into()],
                rows,
                conflict,
                expected_ddl: Some(detail.ddl.clone()),
            })
        };
        assert!(matches!(
            request(import(rows.clone(), OdbcConflict::KeepTarget)),
            Response::TextImported(counts) if (counts.rows, counts.skipped, counts.replaced) == (2, 1, 0)
        ));
        let retained: (String, String) = sql
            .query_first(format!("SELECT note, status FROM {qualified} WHERE id = 1"))
            .unwrap()
            .unwrap();
        assert_eq!(retained, ("原值".into(), "保留".into()));
        assert!(matches!(
            request(import(vec![rows[0].clone()], OdbcConflict::Replace)),
            Response::TextImported(counts) if (counts.rows, counts.skipped, counts.replaced) == (1, 0, 1)
        ));
        let written: Vec<(u32, String, Option<String>, String)> = sql
            .query(format!(
                "SELECT id, note, optional, status FROM {qualified} ORDER BY id"
            ))
            .unwrap();
        assert_eq!(
            written,
            [
                (1, "甲||乙'丙<记录>丁".into(), None, "初始".into()),
                (2, "戊'己".into(), None, "初始".into()),
                (3, "普通文本".into(), Some("~".into()), "初始".into()),
            ]
        );
    }
}

#[test]
#[ignore = "requires disposable MariaDB via HARBOR_HTTP_FIXTURE_PORT"]
fn joined_editable_updates_selected_source_native_and_http() {
    let (port, opts) = fixture_database();
    let mut sql = Conn::new(opts.clone()).unwrap();
    let schema = format!("harbor_http_joined_{}", std::process::id());
    sql.query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
        .unwrap();
    let _cleanup = DatabaseCleanup {
        opts: opts.clone(),
        name: schema.clone(),
    };
    sql.select_db(&schema).unwrap();
    for statement in [
        "CREATE TABLE sellers (id INT PRIMARY KEY, name VARCHAR(50) NOT NULL) ENGINE=InnoDB",
        "CREATE TABLE orders (id INT PRIMARY KEY, seller_id INT, details VARCHAR(50) NOT NULL) ENGINE=InnoDB",
        "CREATE TABLE notes (seller_id INT, message VARCHAR(50), internal_code INT INVISIBLE) ENGINE=InnoDB",
        "INSERT INTO sellers VALUES (1, '甲'), (2, '乙')",
        "INSERT INTO orders VALUES (10, 1, '待处理'), (11, 1, '已关闭')",
        "INSERT INTO notes (seller_id, message, internal_code) VALUES (2, '备注', 17)",
    ] {
        sql.query_drop(statement).unwrap();
    }

    let gateway = TestGateway::new(opts);
    let config = ConnectionConfig {
        port,
        database: Some(schema.clone()),
        ..Default::default()
    };
    let mut transports = vec![
        ("直连", Command::Connect(config.clone())),
        (
            "代理隧道",
            Command::ConnectHttp(config.clone(), gateway.tunnel.clone()),
        ),
    ];
    if let Some(tunnel) = archived_tunnel() {
        transports.push(("旧版 PHP 隧道", Command::ConnectHttp(config, tunnel)));
    }
    for (name, connect) in transports {
        let worker = Worker::new();
        let mut id = 0;
        let mut request = |command| {
            id += 1;
            worker.send(id, command).unwrap();
            let event = worker
                .recv_timeout(Duration::from_secs(12))
                .unwrap()
                .unwrap();
            assert_eq!(event.id, id);
            event
                .result
                .unwrap_or_else(|error| panic!("{name} 请求 {id}：{error}"))
        };
        assert!(matches!(request(connect), Response::Connected(_)));
        let Response::JoinedEditablePage(joined) = request(Command::QueryEditable {
            sql: "SELECT s.name AS seller_name, o.details AS order_details \
                  FROM sellers s LEFT JOIN orders o ON o.seller_id = s.id \
                  ORDER BY s.id, o.id"
                .into(),
            limit: 10,
        }) else {
            panic!("{name} 未返回联表编辑结果");
        };
        assert_eq!(joined.result.columns, ["seller_name", "order_details"]);
        assert_eq!(joined.result.rows.len(), 3);
        assert_eq!(joined.result.rows[2], [Cell::Text("乙".into()), Cell::Null]);
        assert_eq!(joined.targets.len(), 2);
        let sellers = joined
            .targets
            .iter()
            .find(|target| target.alias == "s")
            .unwrap();
        assert_eq!(sellers.page.rows.len(), 2);
        let orders = joined
            .targets
            .iter()
            .find(|target| target.alias == "o")
            .unwrap();
        assert_eq!(orders.page.object.name, "orders");
        assert_eq!(orders.page.rows.len(), 2);
        assert!(!orders.page.can_insert);

        let new_details = format!("{name}已处理");
        assert!(matches!(
            request(Command::EditTable {
                object: orders.page.object.clone(),
                edit: TableEdit::Update {
                    original: orders.page.rows[0].clone(),
                    values: vec![("details".into(), FieldValue::Text(new_details.clone()))],
                },
            }),
            Response::TableEdited { affected_rows: 1 }
        ));
        assert_eq!(
            sql.query::<String, _>("SELECT details FROM orders ORDER BY id")
                .unwrap(),
            [new_details, "已关闭".into()]
        );
        assert_eq!(
            sql.query::<String, _>("SELECT name FROM sellers ORDER BY id")
                .unwrap(),
            ["甲", "乙"]
        );

        let Response::JoinedEditablePage(read_only) = request(Command::QueryEditable {
            sql: "SELECT n.* FROM notes n JOIN sellers s ON n.seller_id = s.id".into(),
            limit: 10,
        }) else {
            panic!("{name} 未返回只读联表结果");
        };
        assert_eq!(read_only.result.columns, ["seller_id", "message"]);
        assert_eq!(
            read_only.result.rows,
            [vec![Cell::Text("2".into()), Cell::Text("备注".into())]]
        );
        assert!(read_only.targets.is_empty());
    }
    gateway.stop();
}

#[test]
#[ignore = "requires disposable MariaDB via HARBOR_HTTP_FIXTURE_PORT"]
fn selected_data_exports_native_and_http() {
    let (port, opts) = fixture_database();
    let mut sql = Conn::new(opts.clone()).unwrap();
    let schema = format!("harbor_http_selected_exports_{}", std::process::id());
    sql.query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
        .unwrap();
    let _cleanup = DatabaseCleanup {
        opts: opts.clone(),
        name: schema.clone(),
    };
    sql.select_db(&schema).unwrap();
    sql.query_drop(
        "CREATE TABLE samples (
            id INT PRIMARY KEY, note VARCHAR(50), optional INT, payload VARBINARY(10),
            amount DECIMAL(20,4) DEFAULT 12.3499,
            generated_value INT AS (id * 2)
        )",
    )
    .unwrap();
    sql.exec_batch(
        "INSERT INTO samples (id, note, payload) VALUES (?, ?, ?)",
        (1..=503).map(|id| (id, "甲|乙'丙", vec![b'A', 0, 255])),
    )
    .unwrap();
    let gateway = TestGateway::new(opts.clone());
    let folder = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let object = DbObject {
        schema: schema.clone(),
        name: "samples".into(),
        kind: ObjectKind::Table,
    };
    let config = ConnectionConfig {
        port,
        database: Some(schema.clone()),
        ..Default::default()
    };
    for (name, connect) in [
        ("native", Command::Connect(config.clone())),
        ("http", Command::ConnectHttp(config, gateway.tunnel.clone())),
    ] {
        let worker = Worker::new();
        let mut id = 0;
        let mut request = |command| {
            id += 1;
            worker.send(id, command).unwrap();
            let event = worker
                .recv_timeout(Duration::from_secs(12))
                .unwrap()
                .unwrap();
            assert_eq!(event.id, id);
            event
                .result
                .unwrap_or_else(|error| panic!("{name} export workflow request {id}: {error}"))
        };
        assert!(matches!(request(connect), Response::Connected(_)));
        assert!(matches!(
            request(Command::ExportColumns { object: object.clone() }),
            Response::ExportColumns(detail) if detail.object == object && detail.columns.len() == 6
        ));
        let csv_path = folder.path().join(format!("{name}.csv"));
        assert!(matches!(
            request(Command::Export {
                object: object.clone(),
                path: csv_path.clone(),
                settings: ExportSettings {
                    kind: ExportKind::Csv,
                    fields: Some(vec!["note".into(), "optional".into(), "payload".into()]),
                    csv: CsvOptions {
                        fixed_width: false,
                        declared_widths: false,
                        field_end: "|".into(),
                        row_end: r"\n".into(),
                        enclosure: "'".into(),
                        escape: "\\".into(),
                        null: r"\N".into(),
                        headers: false,
                        enclose_all: true,
                    },
                    ..Default::default()
                },
            }),
            Response::Transferred(summary) if summary.rows == 503
        ));
        let csv = std::fs::read_to_string(csv_path).unwrap();
        assert_eq!(csv, "'甲|乙\\'丙'|\\N|'0x4100FF'\n".repeat(503));

        let fixed_path = folder.path().join(format!("{name}-fixed.csv"));
        assert!(matches!(
            request(Command::Export {
                object: object.clone(),
                path: fixed_path.clone(),
                settings: ExportSettings {
                    fields: Some(vec!["id".into(), "note".into(), "optional".into(), "payload".into()]),
                    csv: CsvOptions {
                        fixed_width: true,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            }),
            Response::Transferred(summary) if summary.rows == 503
        ));
        let fixed = std::fs::read_to_string(fixed_path).unwrap();
        let records = fixed.lines().collect::<Vec<_>>();
        assert_eq!(records.len(), 504);
        assert!(records.iter().all(|record| record.len() == 30));
        assert_eq!(records[0], "id note       optionalpayload ");
        assert!(records.contains(&"1  甲|乙'丙        0x4100FF"));
        assert!(records.contains(&"503甲|乙'丙        0x4100FF"));

        if name == "native" {
            let path = folder.path().join("table-declared-widths.csv");
            assert!(matches!(
                request(Command::Export {
                    object: object.clone(),
                    path: path.clone(),
                    settings: ExportSettings {
                        fields: Some(vec!["id".into(), "note".into(), "optional".into()]),
                        csv: CsvOptions { fixed_width: true, declared_widths: true, ..Default::default() },
                        ..Default::default()
                    },
                }),
                Response::Transferred(summary) if summary.rows == 503
            ));
            let contents = std::fs::read_to_string(path).unwrap();
            assert_eq!(contents.lines().count(), 504);
            assert!(contents.lines().all(|record| record.len() == 222));
            let row = contents.lines().nth(1).unwrap().as_bytes();
            assert_eq!(&row[..11], b"1          ");
            assert_eq!(&row[11..22], "甲|乙'丙".as_bytes());
            assert_eq!(&row[211..], b"           ");
        }

        let excel = ExportSettings {
            kind: ExportKind::ExcelXml,
            fields: Some(vec!["note".into(), "amount".into()]),
            excel: ExcelOptions {
                text_limit: Some(2),
                decimal_places: Some(2),
            },
            ..Default::default()
        };
        let excel_path = folder.path().join(format!("{name}-table-excel.xml"));
        assert!(matches!(
            request(Command::Export {
                object: object.clone(),
                path: excel_path.clone(),
                settings: excel.clone(),
            }),
            Response::Transferred(summary) if summary.rows == 503
        ));
        let excel_document = std::fs::read_to_string(excel_path).unwrap();
        assert!(excel_document.contains("xml:space=\"preserve\">甲|</Data>"));
        assert!(excel_document.contains("ss:Type=\"Number\">12.34</Data>"));

        let sql_path = folder.path().join(format!("{name}.sql"));
        assert!(matches!(
            request(Command::Export {
                object: object.clone(),
                path: sql_path.clone(),
                settings: ExportSettings {
                    kind: ExportKind::Sql,
                    fields: Some(vec!["id".into(), "payload".into()]),
                    ..Default::default()
                },
            }),
            Response::Transferred(summary) if summary.rows == 503
        ));
        let inserts = std::fs::read_to_string(sql_path).unwrap();
        let statements: Vec<_> = inserts
            .lines()
            .filter(|line| line.starts_with("INSERT"))
            .collect();
        assert_eq!(statements.len(), 503);
        assert!(
            statements
                .contains(&"INSERT INTO `samples` (`id`, `payload`) VALUES (503, X'4100FF');")
        );

        for (mode, expected_rows) in [
            (SqlExportMode::Structure, 0u64),
            (SqlExportMode::StructureAndData, 503),
        ] {
            let sql_path = folder.path().join(format!("{name}-{mode:?}.sql"));
            assert!(matches!(
                request(Command::Export {
                    object: object.clone(),
                    path: sql_path.clone(),
                    settings: ExportSettings {
                        kind: ExportKind::Sql,
                        fields: Some(vec!["amount".into(), "payload".into(), "id".into(), "note".into()]),
                        sql: mode,
                        ..Default::default()
                    },
                }),
                Response::Transferred(summary) if summary.rows == expected_rows
            ));
            let destination = format!("{schema}_{name}_{expected_rows}");
            sql.query_drop(format!(
                "CREATE DATABASE {destination} CHARACTER SET utf8mb4"
            ))
            .unwrap();
            let _cleanup = DatabaseCleanup {
                opts: opts.clone(),
                name: destination.clone(),
            };
            sql.select_db(&destination).unwrap();
            let script = std::fs::read_to_string(sql_path).unwrap();
            for statement in crate::sql_tools::split_script(&script) {
                sql.query_drop(statement.sql).unwrap();
            }
            let names = sql
                .query_map("SHOW COLUMNS FROM samples", |row: Row| {
                    row.get::<String, _>(0).unwrap()
                })
                .unwrap();
            assert_eq!(names, ["amount", "payload", "id", "note"]);
            assert_eq!(
                sql.query_first::<u64, _>("SELECT COUNT(*) FROM samples")
                    .unwrap(),
                Some(expected_rows)
            );
            if expected_rows > 0 {
                assert_eq!(
                    sql.query_first::<(String, Vec<u8>, String), _>(
                        "SELECT amount, payload, note FROM samples WHERE id = 503"
                    )
                    .unwrap(),
                    Some((
                        "12.3499".to_owned(),
                        vec![b'A', 0, 255],
                        "甲|乙'丙".to_owned()
                    ))
                );
            }
            sql.select_db(&schema).unwrap();
        }

        let Response::Queried(report) = request(Command::Query {
            sql: "SELECT note, payload, id, amount FROM samples WHERE id = 1".into(),
            limit: 10,
        }) else {
            panic!("expected a query result");
        };
        let result = &report.sets[0];
        if name == "native" {
            let path = folder.path().join("result-declared-widths.csv");
            let summary = transfer::export_result(
                &path,
                result,
                &[2, 0],
                &ExportSettings {
                    csv: CsvOptions {
                        fixed_width: true,
                        declared_widths: true,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(summary.rows, 1);
            let contents = std::fs::read_to_string(path).unwrap();
            assert_eq!(contents.lines().count(), 2);
            let row = contents.lines().last().unwrap();
            assert_eq!(row.len(), 211);
            assert_eq!(&row[..11], "1          ");
            assert!(row[11..].starts_with("甲|乙'丙"));
        }
        let query_excel_path = folder.path().join(format!("{name}-query-excel.xml"));
        let summary = transfer::export_result(&query_excel_path, result, &[3, 0], &excel).unwrap();
        assert_eq!(summary.rows, 1);
        let query_excel = std::fs::read_to_string(query_excel_path).unwrap();
        assert!(query_excel.contains("ss:Type=\"Number\">12.34</Data>"));
        assert!(query_excel.contains("xml:space=\"preserve\">甲|</Data>"));
        for (format, expected) in [
            (ExportKind::Csv, "payload,note\r\n0x4100FF,甲|乙'丙"),
            (ExportKind::Html, "<th scope=\"col\">payload</th>"),
            (
                ExportKind::Xml,
                "<column>payload</column><column>note</column>",
            ),
            (ExportKind::ExcelXml, ">payload</Data>"),
            (ExportKind::Json, "\"columns\": [\"payload\",\"note\"]"),
        ] {
            let path = folder
                .path()
                .join(format!("{name}-query.{}", format.extension()));
            let summary = transfer::export_result(
                &path,
                result,
                &[1, 0],
                &ExportSettings {
                    kind: format,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(summary.rows, 1);
            let contents = std::fs::read_to_string(path).unwrap();
            assert!(
                contents.contains(expected),
                "{name} {format:?}: expected {expected:?} in {contents:?}"
            );
        }

        let Response::Queried(report) = request(Command::Query {
            sql: "SELECT s.note AS label, s.payload, s.optional, \
                CAST(18446744073709551615 AS UNSIGNED) AS item_id, \
                CAST(12345678901234567890.124900 AS DECIMAL(26,6)) AS amount, \
                CAST('2026-06-07 14:15:16.123456' AS DATETIME(6)) AS stamp \
                FROM samples s JOIN samples t ON s.id = t.id WHERE s.id = 1"
                .into(),
            limit: 10,
        }) else {
            panic!("expected a query result");
        };
        for mode in [
            SqlExportMode::Data,
            SqlExportMode::Structure,
            SqlExportMode::StructureAndData,
        ] {
            let path = folder.path().join(format!("{name}-query-{mode:?}.sql"));
            let table = format!("{name} query {mode:?}");
            let qualified = format!("`{schema}`.`{table}`");
            if mode == SqlExportMode::Data {
                sql.query_drop(format!("CREATE TABLE {qualified} (amount DECIMAL(26,6), payload BLOB, item_id BIGINT UNSIGNED, label TEXT, optional INT, stamp DATETIME(6))")).unwrap();
            }
            let summary = transfer::export_result(
                &path,
                &report.sets[0],
                &[4, 1, 3, 0, 2, 5],
                &ExportSettings {
                    kind: ExportKind::Sql,
                    sql: mode,
                    result_target: Some(ResultSqlTarget {
                        schema: schema.clone(),
                        table,
                    }),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(summary.rows, u64::from(mode != SqlExportMode::Structure));
            let script = std::fs::read_to_string(path).unwrap();
            assert_eq!(script.contains("CREATE TABLE"), mode != SqlExportMode::Data);
            assert_eq!(
                script.contains("INSERT INTO"),
                mode != SqlExportMode::Structure
            );
            sql.query_drop("USE information_schema").unwrap();
            for statement in crate::sql_tools::split_script(&script) {
                sql.query_drop(statement.sql)
                    .unwrap_or_else(|error| panic!("{name} {mode:?}: {error}. {}", statement.sql));
            }
            let count = sql
                .query_first::<u64, _>(format!("SELECT COUNT(*) FROM {qualified}"))
                .unwrap();
            assert_eq!(count, Some(summary.rows));
            if summary.rows > 0 {
                let values = sql.query_first::<(String, Vec<u8>, String, String, Option<String>, String), _>(
                    format!("SELECT amount, payload, item_id, label, optional, stamp FROM {qualified}"),
                ).unwrap().unwrap();
                assert_eq!(
                    values,
                    (
                        "12345678901234567890.124900".into(),
                        vec![b'A', 0, 255],
                        "18446744073709551615".into(),
                        "甲|乙'丙".into(),
                        None,
                        "2026-06-07 14:15:16.123456".into()
                    )
                );
            }
        }
        sql.select_db(&schema).unwrap();
    }
    gateway.stop();
}

pub(in crate::database) struct TestGateway {
    pub tunnel: HttpTunnelConfig,
    stop: Arc<AtomicBool>,
    handle: thread::JoinHandle<()>,
}

#[test]
#[ignore = "需要一次性 MariaDB，端口由 HARBOR_HTTP_FIXTURE_PORT 设置"]
fn independently_backs_up_one_or_multiple_databases_over_http() {
    let (port, opts) = fixture_database();
    let mut sql = Conn::new(opts.clone()).unwrap();
    let schema = format!("harbor_http_job_{}", std::process::id());
    let second = format!("{schema}_second");
    let directory = tempfile::tempdir().unwrap();
    let mut cleanup = Vec::new();
    for name in [&schema, &second] {
        sql.query_drop(format!("CREATE DATABASE {name} CHARACTER SET utf8mb4"))
            .unwrap();
        cleanup.push(DatabaseCleanup {
            opts: opts.clone(),
            name: name.clone(),
        });
        sql.query_drop(format!("CREATE TABLE {name}.records (id INT PRIMARY KEY)"))
            .unwrap();
        sql.query_drop(format!("INSERT INTO {name}.records VALUES (42)"))
            .unwrap();
    }
    let gateway = TestGateway::new(opts);
    let connection = serde_json::json!({
        "host": "127.0.0.1",
        "port": port,
        "user": "root",
        "http": {"url": gateway.tunnel.url, "utf8mb4": true, "force_base64": true}
    });
    for (output, job) in [
        (
            "one.sql",
            serde_json::json!({"type": "backup", "schema": schema, "output": "one.sql"}),
        ),
        (
            "both.sql",
            serde_json::json!({"type": "backup_databases", "schemas": [schema, second], "output_file": "both.sql"}),
        ),
    ] {
        let path = directory.path().join(format!("{output}.json"));
        let content = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "connection": connection,
            "job": job,
        }))
        .unwrap();
        std::fs::write(&path, content).unwrap();
        crate::jobs::run_fixture(&path).unwrap();
        let script = std::fs::read_to_string(directory.path().join(output)).unwrap();
        assert!(script.contains("CREATE TABLE `records`"));
        assert!(script.contains("INSERT INTO `records` (`id`) VALUES (42);"));
        if output == "both.sql" {
            assert!(script.contains(&format!("USE `{schema}`;")));
            assert!(script.contains(&format!("USE `{second}`;")));
        }
    }
    gateway.stop();
}

impl TestGateway {
    pub(in crate::database) fn new(opts: Opts) -> Self {
        Self::observe_queries(opts, |_| {})
    }

    fn observe_queries(opts: Opts, observe: impl Fn(&str) + Send + 'static) -> Self {
        let (address, stop, handle) = spawn_gateway(opts, observe);
        Self {
            tunnel: HttpTunnelConfig {
                url: format!("http://{address}/tunnel.php"),
                force_base64: true,
                use_utf8mb4: true,
                timeout: 5,
                ..Default::default()
            },
            stop,
            handle,
        }
    }

    pub(in crate::database) fn stop(self) {
        self.stop.store(true, Ordering::Release);
        self.handle.join().unwrap();
    }
}

fn spawn_gateway(
    opts: Opts,
    observe: impl Fn(&str) + Send + 'static,
) -> (
    std::net::SocketAddr,
    Arc<AtomicBool>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = stop.clone();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(45);
        while !server_stop.load(Ordering::Acquire) && Instant::now() < deadline {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(3));
                    continue;
                }
                Err(error) => panic!("loopback gateway: {error}"),
            };
            stream.set_nonblocking(false).unwrap();
            let (head, request) = read_request(&mut stream);
            if head.starts_with("POST /tunnel.php?app=sqlyog&version=8.05 ") {
                assert_eq!(request, b"garbagedata");
                send_reply(&mut stream, b"TunnelVersion:8.05");
                continue;
            }
            let request: Request = quick_xml::de::from_reader(request.as_slice()).unwrap();
            let selected = request.connect_info.db.decoded();
            let query = ValueText {
                encoded: request.query_info.query.encoded,
                text: request.query_info.query.text,
            }
            .decoded();
            let mut connection = Conn::new(opts.clone()).unwrap();
            connection.query_drop("SET NAMES utf8mb4").unwrap();
            connection.query_drop("SET sql_mode = ''").unwrap();
            if !selected.is_empty() {
                assert!(selected == "information_schema" || selected.starts_with("harbor_http_"));
                connection.select_db(&selected).unwrap();
            }
            let commands = if request.query_info.query.batch == 1 {
                batch_commands(&query)
            } else {
                vec![query.as_str()]
            };
            let mut response = String::new();
            for command in commands {
                observe(command);
                response = match query_xml(&mut connection, command) {
                    Ok(xml) => xml,
                    Err(error) => {
                        let code = match &error {
                            mysql::Error::MySqlError(value) => value.code,
                            _ => 9999,
                        };
                        let message = error.to_string();
                        response = format!(
                            "<xml><result v='8.05'><e_i><e_n>{code}</e_n><e_d>{}</e_d></e_i></result></xml>",
                            escape(&message)
                        );
                        break;
                    }
                };
            }
            send_reply(&mut stream, response.as_bytes());
        }
    });
    (address, stop, handle)
}

#[test]
#[ignore = "requires disposable MariaDB via HARBOR_HTTP_FIXTURE_PORT"]
fn transfers_and_edits_rows_over_a_stateless_tunnel() {
    let (port, opts) = fixture_database();
    let mut sql = Conn::new(opts.clone()).unwrap();
    let schema = format!("harbor_http_{}", std::process::id());
    sql.query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
        .unwrap();
    let _cleanup = DatabaseCleanup {
        opts: opts.clone(),
        name: schema.clone(),
    };
    sql.select_db(&schema).unwrap();
    sql.query_drop("CREATE TABLE parent (id INT PRIMARY KEY) ENGINE=InnoDB")
        .unwrap();
    sql.query_drop("INSERT INTO parent VALUES (1)").unwrap();
    sql.query_drop(
        "CREATE TABLE attachments (
        id INT PRIMARY KEY, parent_id INT NOT NULL, payload VARBINARY(32), note VARCHAR(48),
        FOREIGN KEY (parent_id) REFERENCES parent(id)
    ) ENGINE=InnoDB",
    )
    .unwrap();
    sql.exec_batch(
        "INSERT INTO attachments VALUES (?,1,?,?)",
        (1..=503).map(|id| (id, vec![b'A', 0, 255], Some("测试;内容"))),
    )
    .unwrap();
    sql.query_drop("CREATE VIEW files AS SELECT id, payload FROM attachments")
        .unwrap();
    sql.query_drop(
        "CREATE PROCEDURE files_count() BEGIN SELECT 7; SELECT COUNT(*) FROM files; END",
    )
    .unwrap();
    let gateway = TestGateway::new(opts);

    let output = tempfile::tempdir().unwrap();
    let worker = Worker::new();
    let mut id = 0;
    let mut request = |command| {
        id += 1;
        worker.send(id, command).unwrap();
        let event = worker
            .recv_timeout(Duration::from_secs(12))
            .unwrap()
            .unwrap();
        assert_eq!(event.id, id);
        event.result.unwrap()
    };
    assert!(matches!(
        request(Command::ConnectHttp(
            ConnectionConfig {
                port,
                database: Some(schema.clone()),
                ..Default::default()
            },
            gateway.tunnel.clone()
        )),
        Response::Connected(_)
    ));
    let object = DbObject {
        schema: schema.clone(),
        name: "attachments".into(),
        kind: ObjectKind::Table,
    };
    let csv = output.path().join("数据.csv");
    assert!(matches!(
        request(Command::Export {
            object: object.clone(),
            path: csv.clone(),
            settings: ExportSettings::default(),
        }),
        Response::Transferred(summary) if summary.rows == 503
    ));
    let csv = std::fs::read_to_string(csv).unwrap();
    assert_eq!(csv.lines().count(), 504);
    assert!(csv.contains("503,1,0x4100FF,测试;内容"));
    let html = output.path().join("结构.html");
    assert!(
        matches!(request(Command::SchemaHtml { schema: schema.clone(), path: html.clone() }), Response::Transferred(summary) if summary.objects == 4)
    );
    assert!(
        std::fs::read_to_string(html)
            .unwrap()
            .to_ascii_lowercase()
            .contains("varbinary")
    );
    let Response::SchemaDesign(design) = request(Command::DescribeSchemaDesign {
        schema: schema.clone(),
    }) else {
        panic!("expected a schema design")
    };
    assert_eq!(
        design.foreign_keys[0].columns,
        [("parent_id".into(), "id".into())]
    );
    let Response::Queried(created) = request(Command::Query {
        sql: format!("ALTER TABLE {schema}.attachments ADD INDEX idx_note (note)"),
        limit: 1,
    }) else {
        panic!("expected the index statement result")
    };
    assert!(created.error.is_none());
    let Response::TableKeys(keys) = request(Command::DescribeTableKeys {
        object: object.clone(),
    }) else {
        panic!("expected table indexes and foreign keys")
    };
    assert_eq!(
        keys.indexes
            .iter()
            .find(|index| index.name == "idx_note")
            .unwrap()
            .parts[0]
            .column
            .as_deref(),
        Some("note")
    );
    assert_eq!(
        keys.foreign_keys[0].columns,
        [("parent_id".into(), "id".into())]
    );
    let dump = output.path().join("隧道.sql");
    assert!(
        matches!(request(Command::DumpConfigured { schema: schema.clone(), path: dump.clone(), settings: DumpSettings {
        include_use: true, include_create_database: true, bulk_insert: true, ..Default::default()
    } }), Response::Transferred(summary) if summary.rows == 504 && summary.objects == 4)
    );
    let native = output.path().join("直连.sql");
    let direct = transfer::dump(&mut sql, &schema, None, &native, DumpOptions::default()).unwrap();
    assert_eq!((direct.objects, direct.rows), (4, 504));
    sql.query_drop("DELETE FROM attachments").unwrap();
    sql.query_drop("DROP PROCEDURE files_count").unwrap();
    assert!(
        matches!(request(Command::Restore { schema: schema.clone(), path: dump }), Response::Restored(count) if count > 15)
    );
    assert_eq!(
        sql.query_first::<(Vec<u8>, String), _>(
            "SELECT payload,note FROM attachments WHERE id=503"
        )
        .unwrap(),
        Some((vec![b'A', 0, 255], "测试;内容".into()))
    );
    let counts = sql.query::<u64, _>("CALL files_count()").unwrap();
    assert_eq!(counts.first().copied(), Some(7));
    assert_eq!(
        sql.query_first::<u64, _>("SELECT COUNT(*) FROM files")
            .unwrap(),
        Some(503)
    );

    use super::super::{
        FieldValue, TableEdit, TablePredicate, TablePredicateOp, TableSelection, TableSort,
    };
    let Response::TablePage(page) = request(Command::TablePageSelected {
        object: object.clone(),
        limit: 2,
        offset: 1,
        selection: TableSelection {
            predicates: vec![TablePredicate {
                column: "id".into(),
                operator: TablePredicateOp::Greater,
                value: mysql::Value::Int(500),
            }],
            order: vec![TableSort {
                column: "id".into(),
                descending: true,
            }],
        },
    }) else {
        panic!("expected the data table")
    };
    assert_eq!(page.rows.len(), 2);
    assert_eq!(page.rows[0].cells[0], Cell::Text("502".into()));
    assert_eq!(page.rows[1].cells[0], Cell::Text("501".into()));
    let Response::EditablePage(projected) = request(Command::QueryEditable {
        sql: "SELECT a.payload AS attachment, a.id AS row_id FROM attachments AS a WHERE a.id = 503 ORDER BY row_id".into(),
        limit: 100,
    }) else { panic!("expected editable query results") };
    assert_eq!(projected.column_labels, ["attachment", "row_id"]);
    assert_eq!(projected.primary_key, [1]);
    assert_eq!(projected.rows[0].cells[0], Cell::Binary(vec![b'A', 0, 255]));
    assert!(matches!(
        request(Command::EditTable {
            object: object.clone(),
            edit: TableEdit::Update {
                original: projected.rows[0].clone(),
                values: vec![("payload".into(), FieldValue::Binary(vec![b'B', 0, 254]))]
            },
        }),
        Response::TableEdited { affected_rows: 1 }
    ));
    assert_eq!(
        sql.query_first::<(Vec<u8>, String), _>(
            "SELECT payload,note FROM attachments WHERE id=503"
        )
        .unwrap(),
        Some((vec![b'B', 0, 254], "测试;内容".into()))
    );
    assert!(matches!(
        request(Command::EditTable {
            object: object.clone(),
            edit: TableEdit::Insert {
                values: vec![
                    ("id".into(), FieldValue::Text("504".into())),
                    ("parent_id".into(), FieldValue::Text("1".into())),
                    ("note".into(), FieldValue::Null)
                ]
            },
        }),
        Response::TableEdited { affected_rows: 1 }
    ));
    assert!(matches!(
        request(Command::EditTable {
            object,
            edit: TableEdit::Delete {
                original: page.rows[1].clone()
            },
        }),
        Response::TableEdited { affected_rows: 1 }
    ));
    assert_eq!(
        sql.query_first::<u64, _>(
            "SELECT COUNT(*) FROM attachments WHERE (id=504 AND note IS NULL) OR id=501"
        )
        .unwrap(),
        Some(1)
    );
    gateway.stop();
}

#[test]
#[ignore = "requires disposable MariaDB via HARBOR_HTTP_FIXTURE_PORT"]
fn copies_a_schema_and_selected_data_to_a_different_name_over_http() {
    let (port, opts) = fixture_database();
    let mut sql = Conn::new(opts.clone()).unwrap();
    let source = format!("harbor_http_copy_src_{}", std::process::id());
    let destination = format!("harbor_http_copy_dst_{}", std::process::id());
    sql.query_drop(format!(
        "CREATE DATABASE {source} CHARACTER SET utf8mb4 COLLATE utf8mb4_bin"
    ))
    .unwrap();
    let _source_cleanup = DatabaseCleanup {
        opts: opts.clone(),
        name: source.clone(),
    };
    let _destination_cleanup = DatabaseCleanup {
        opts: opts.clone(),
        name: destination.clone(),
    };
    sql.select_db(&source).unwrap();
    sql.query_drop("CREATE TABLE parent (id INT PRIMARY KEY) ENGINE=InnoDB")
        .unwrap();
    sql.query_drop("INSERT INTO parent VALUES (1)").unwrap();
    sql.query_drop(
        "CREATE TABLE attachments (
        id INT PRIMARY KEY, parent_id INT NOT NULL, payload VARBINARY(32), note VARCHAR(48),
        FOREIGN KEY (parent_id) REFERENCES parent(id)
    ) ENGINE=InnoDB",
    )
    .unwrap();
    let file_record = (vec![b'A', 0, 255], "测试;内容".to_owned());
    sql.exec_drop(
        "INSERT INTO attachments VALUES (7,1,?,?)",
        file_record.clone(),
    )
    .unwrap();

    let gateway = TestGateway::new(opts);
    let (mut session, _) = Session::open(
        ConnectionConfig {
            port,
            database: Some(source.clone()),
            ..Default::default()
        },
        gateway.tunnel.clone(),
    )
    .unwrap();
    let output = tempfile::tempdir().unwrap();
    let mut copy = |object, options| {
        let path = output.path().join("copy.sql");
        let Response::Transferred(summary) = session
            .handle(Command::DumpForCopy {
                schema: source.clone(),
                object,
                objects: None,
                destination_schema: destination.clone(),
                path: path.clone(),
                options,
                bulk_insert: false,
            })
            .unwrap()
        else {
            panic!("expected a copy dump")
        };
        assert!(matches!(
            session
                .handle(Command::Restore {
                    schema: String::new(),
                    path
                })
                .unwrap(),
            Response::Restored(_)
        ));
        (summary.objects, summary.rows)
    };

    assert_eq!(copy(None, DumpOptions::default()), (2, 2));
    let defaults = sql
        .exec_first::<(String, String), _, _>(
            "SELECT DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME
             FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?",
            (&destination,),
        )
        .unwrap();
    assert_eq!(defaults, Some(("utf8mb4".into(), "utf8mb4_bin".into())));
    let fk_schema = sql
        .exec_first::<String, _, _>(
            "SELECT REFERENCED_TABLE_SCHEMA FROM information_schema.KEY_COLUMN_USAGE
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'attachments' AND REFERENCED_TABLE_NAME = 'parent'",
            (&destination,),
        )
        .unwrap();
    assert_eq!(fk_schema.as_deref(), Some(destination.as_str()));
    let read_file = format!("SELECT payload, note FROM {destination}.attachments WHERE id = 7");
    assert_eq!(
        sql.query_first::<(Vec<u8>, String), _>(&read_file).unwrap(),
        Some(file_record.clone())
    );

    sql.query_drop(format!("DELETE FROM {destination}.attachments"))
        .unwrap();
    let selected = DbObject {
        schema: source.clone(),
        name: "attachments".into(),
        kind: ObjectKind::Table,
    };
    assert_eq!(
        copy(
            Some(selected),
            DumpOptions {
                include_structure: false,
                ..DumpOptions::default()
            }
        ),
        (1, 1)
    );
    assert_eq!(
        sql.query_first::<(Vec<u8>, String), _>(&read_file).unwrap(),
        Some(file_record)
    );
    gateway.stop();
}

#[test]
#[ignore = "requires disposable MariaDB via HARBOR_HTTP_FIXTURE_PORT"]
fn configured_dump_honors_insert_options_for_native_and_http() {
    let (port, opts) = fixture_database();
    let mut sql = Conn::new(opts.clone()).unwrap();
    let prefix = format!("harbor_http_dump_{}", std::process::id());
    let databases = ["source", "native", "http"].map(|suffix| {
        let name = format!("{prefix}_{suffix}");
        sql.query_drop(format!("CREATE DATABASE {name} CHARACTER SET utf8mb4"))
            .unwrap();
        DatabaseCleanup {
            opts: opts.clone(),
            name,
        }
    });
    let [schema, native_target, http_target] = databases.each_ref().map(|db| db.name.as_str());
    sql.select_db(schema).unwrap();
    sql.query_drop(
        "CREATE TABLE items (id INT PRIMARY KEY, tag VARCHAR(20), data VARBINARY(180),
            KEY tag_idx (tag)) ENGINE=MyISAM",
    )
    .unwrap();
    let expected: Vec<_> = (1..=5)
        .map(|id| (id, "转储".to_owned(), vec![0xa5_u8; 180]))
        .collect();
    sql.exec_batch("INSERT INTO items VALUES (?, ?, ?)", expected.clone())
        .unwrap();

    let gateway = TestGateway::new(opts);
    let (http, _) = Session::open(
        ConnectionConfig {
            port,
            ..Default::default()
        },
        gateway.tunnel.clone(),
    )
    .unwrap();
    let build_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
    let output = tempfile::tempdir_in(build_dir).unwrap();
    let native_path = output.path().join("native.sql");
    let http_path = output.path().join("http.sql");
    let settings = DumpSettings {
        chunk_rows: NonZeroU32::new(2),
        bulk_insert_limit_kib: NonZeroU32::new(1),
        complete_insert: false,
        disable_keys: true,
        lock_insert_tables: true,
        disable_autocommit: Some(true),
        ..Default::default()
    };
    let native_dump = transfer::dump_configured(
        &mut sql,
        schema,
        &native_path,
        DumpSettings {
            lock_source_tables: true,
            ..settings.clone()
        },
        |_| Ok(()),
    )
    .unwrap();
    let http_dump =
        transfer::dump_over_http(&http, schema, &http_path, settings, |_| Ok(())).unwrap();
    assert_eq!((native_dump.rows, http_dump.rows), (5, 5));
    for path in [&native_path, &http_path] {
        let script = std::fs::read_to_string(path).unwrap();
        let inserts: Vec<_> = script
            .split_inclusive(";\n")
            .filter(|statement| statement.starts_with("INSERT INTO "))
            .collect();
        assert_eq!(inserts.len(), 3);
        assert!(inserts.iter().all(|statement| {
            statement.starts_with("INSERT INTO `items` VALUES ") && statement.len() <= 1024
        }));
        let disable = script.find("DISABLE KEYS").unwrap();
        let enable = script.find("ENABLE KEYS").unwrap();
        assert!(disable < script.find("INSERT INTO ").unwrap());
        assert!(enable > script.rfind("INSERT INTO ").unwrap());
        let disable_autocommit = script.find("SET SESSION autocommit = 0;").unwrap();
        let commit = script.find("COMMIT;").unwrap();
        let restore_autocommit = script
            .find("SET SESSION autocommit = @harbor_dump_autocommit;")
            .unwrap();
        assert!(disable_autocommit < disable && enable < restore_autocommit);
        assert!(script.rfind("INSERT INTO ").unwrap() < commit && commit < enable);
    }

    crate::restore::restore(&mut sql, native_target, &native_path).unwrap();
    http.restore(http_target, &http_path).unwrap();
    for destination in [native_target, http_target] {
        let restored = sql
            .query::<(i32, String, Vec<u8>), _>(format!(
                "SELECT id, tag, data FROM {destination}.items ORDER BY id"
            ))
            .unwrap();
        assert_eq!(restored, expected);
        let index: Row = sql
            .query_first(format!(
                "SHOW INDEX FROM {destination}.items WHERE Key_name = 'tag_idx'"
            ))
            .unwrap()
            .unwrap();
        assert_eq!(index.get::<String, _>("Comment").as_deref(), Some(""));
    }
    gateway.stop();
}

#[test]
#[ignore = "需要一次性 MariaDB，端口由 HARBOR_HTTP_FIXTURE_PORT 设置"]
fn dump_uses_source_packet_and_limits_http_reads_to_a_thousand_rows() {
    use std::sync::Mutex;

    let (port, opts) = fixture_database();
    let mut sql = Conn::new(opts.clone()).unwrap();
    let schema = format!("harbor_http_dump_limits_{}", std::process::id());
    sql.query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
        .unwrap();
    let _cleanup = DatabaseCleanup {
        opts: opts.clone(),
        name: schema.clone(),
    };
    sql.select_db(&schema).unwrap();
    sql.query_drop("CREATE TABLE items(id INT PRIMARY KEY, payload VARBINARY(64)) ENGINE=InnoDB")
        .unwrap();
    sql.query_drop("INSERT INTO items SELECT seq, REPEAT('a', 64) FROM seq_1_to_1205")
        .unwrap();
    let pages = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&pages);
    let row_source = format!("FROM `{schema}`.`items` ORDER BY ");
    let gateway = TestGateway::observe_queries(opts, move |query| {
        if query.contains(&row_source)
            && let Some((_, window)) = query.rsplit_once(" LIMIT ")
        {
            observed.lock().unwrap().push(window.to_owned());
        }
    });
    let (http, _) = Session::open(
        ConnectionConfig {
            port,
            ..Default::default()
        },
        gateway.tunnel.clone(),
    )
    .unwrap();
    let target = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
    let output = tempfile::tempdir_in(target).unwrap();
    let read_insert_sizes = |path: &std::path::Path| {
        std::fs::read_to_string(path)
            .unwrap()
            .split_inclusive(";\n")
            .filter(|statement| statement.starts_with("INSERT INTO `items`"))
            .map(str::len)
            .collect::<Vec<_>>()
    };
    let packet: usize = sql
        .query_first("SELECT @@SESSION.max_allowed_packet")
        .unwrap()
        .unwrap();
    let read_selects = |sql: &mut Conn| {
        let (_, count): (String, u64) = sql
            .query_first("SHOW SESSION STATUS LIKE 'Com_select'")
            .unwrap()
            .unwrap();
        count
    };
    let mut native_reads = Vec::new();
    for chunk in [None, NonZeroU32::new(400)] {
        let path = output.path().join(format!("native-{chunk:?}.sql"));
        let before = read_selects(&mut sql);
        let report = transfer::dump_with_limits(
            &mut sql,
            &schema,
            None,
            &path,
            DumpOptions::default(),
            chunk,
            None,
        )
        .unwrap();
        native_reads.push(read_selects(&mut sql) - before);
        assert_eq!(report.rows, 1205);
        let sizes = read_insert_sizes(&path);
        assert!(sizes.iter().any(|&bytes| bytes > 1024));
        assert!(sizes.iter().all(|&bytes| bytes < packet));
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains(&format!("USE `{schema}`;"))
        );
    }
    assert!(native_reads[1] >= native_reads[0] + 3, "{native_reads:?}");

    for (chunk, expected) in [
        (None, &["1000 OFFSET 0", "1000 OFFSET 1000"][..]),
        (
            NonZeroU32::new(1500),
            &["1000 OFFSET 0", "1000 OFFSET 1000"],
        ),
        (
            NonZeroU32::new(400),
            &[
                "400 OFFSET 0",
                "400 OFFSET 400",
                "400 OFFSET 800",
                "400 OFFSET 1200",
            ],
        ),
    ] {
        pages.lock().unwrap().clear();
        let path = output.path().join(format!("http-{chunk:?}.sql"));
        let report = transfer::dump_over_http(
            &http,
            &schema,
            &path,
            DumpSettings {
                chunk_rows: chunk,
                bulk_insert_limit_kib: NonZeroU32::new(1),
                ..DumpSettings::default()
            },
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(report.rows, 1205);
        assert_eq!(*pages.lock().unwrap(), expected);
        let sizes = read_insert_sizes(&path);
        assert!(sizes.len() > 100);
        assert!(sizes.iter().all(|&bytes| bytes <= 1024));
    }
    gateway.stop();
}

#[test]
#[ignore = "需要一次性 MariaDB，端口由 HARBOR_HTTP_FIXTURE_PORT 设置"]
fn partial_backup_removes_failed_sql_and_skips_known_dependents() {
    let (port, opts) = fixture_database();
    let mut sql = Conn::new(opts.clone()).unwrap();
    let schema = format!("harbor_http_partial_{}", std::process::id());
    let target = format!("{schema}_restored");
    let _cleanup = [&schema, &target].map(|name| {
        sql.query_drop(format!("CREATE DATABASE {name} CHARACTER SET utf8mb4"))
            .unwrap();
        DatabaseCleanup {
            opts: opts.clone(),
            name: name.clone(),
        }
    });
    sql.select_db(&schema).unwrap();
    for statement in [
        "CREATE TABLE healthy(id INT PRIMARY KEY, note VARCHAR(16))",
        "INSERT INTO healthy VALUES (1, '好数据'), (2, '继续')",
        "CREATE TABLE history(id INT PRIMARY KEY, note VARCHAR(16)) WITH SYSTEM VERSIONING",
        "INSERT INTO history VALUES (1, '历史')",
        "CREATE VIEW dependent AS SELECT id, note FROM history",
        "CREATE TRIGGER history_probe BEFORE INSERT ON history FOR EACH ROW SET NEW.note = NEW.note",
    ] {
        sql.query_drop(statement).unwrap();
    }
    let gateway = TestGateway::new(opts);
    let (http, _) = Session::open(
        ConnectionConfig {
            port,
            ..Default::default()
        },
        gateway.tunnel.clone(),
    )
    .unwrap();
    let folder =
        tempfile::tempdir_in(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target"))
            .unwrap();
    for (over_http, split) in [(false, false), (true, true)] {
        let path = if split {
            folder.path().to_owned()
        } else {
            folder.path().join("backup.sql")
        };
        let settings = DumpSettings {
            file_per_object: split,
            continue_on_error: true,
            ..Default::default()
        };
        let summary = if over_http {
            transfer::dump_over_http(&http, &schema, &path, settings, |_| Ok(()))
        } else {
            transfer::dump_configured(&mut sql, &schema, &path, settings, |_| Ok(()))
        }
        .unwrap();
        assert_eq!((summary.objects, summary.rows), (1, 2));
        let names = summary
            .skipped_objects
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["history", "dependent", "history_probe"]);
        let source = summary.output_directory.as_deref().unwrap_or(&path);
        if split {
            let manifest: serde_json::Value =
                serde_json::from_slice(&std::fs::read(source.join("restore-order.json")).unwrap())
                    .unwrap();
            assert_eq!(
                manifest["files"],
                serde_json::json!(["00000001-table-healthy.sql"])
            );
        } else {
            let script = std::fs::read_to_string(source).unwrap();
            assert!(!script.contains("`history`") && !script.contains("`dependent`"));
        }
        crate::restore::restore(&mut sql, &target, source).unwrap();
        let rows: Vec<(u32, String)> = sql
            .query(format!("SELECT id, note FROM {target}.healthy ORDER BY id"))
            .unwrap();
        assert_eq!(rows, [(1, "好数据".into()), (2, "继续".into())]);
    }
    gateway.stop();
}

#[test]
#[ignore = "需要一次性 MariaDB，端口由 HARBOR_HTTP_FIXTURE_PORT 设置"]
fn gbk_backup_preserves_chinese_ddl_and_data_across_transports() {
    use crate::sql_encoding::{SqlDumpCharset, decode_sql};

    let (port, opts) = fixture_database();
    let mut sql = Conn::new(opts.clone()).unwrap();
    let schema = format!("harbor_http_gbk_{}", std::process::id());
    let [source, native_target, http_target] = ["source", "native", "http"].map(|suffix| {
        let name = format!("{schema}_{suffix}");
        sql.query_drop(format!("CREATE DATABASE {name} CHARACTER SET utf8mb4"))
            .unwrap();
        DatabaseCleanup {
            name,
            opts: opts.clone(),
        }
    });
    sql.select_db(&source.name).unwrap();
    sql.query_drop(
        "CREATE TABLE 資料 (id INT PRIMARY KEY, 內容 VARCHAR(32) DEFAULT '中文預設', body VARBINARY(4));",
    ).unwrap();
    sql.query_drop("INSERT INTO 資料 VALUES (3, '中文数据 🚀', X'00FF')")
        .unwrap();
    sql.query_drop("CREATE TABLE 分類 (id INT PRIMARY KEY)")
        .unwrap();
    sql.query_drop("INSERT INTO 分類 VALUES (7)").unwrap();

    let build = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
    let folder = tempfile::tempdir_in(build).unwrap();
    let native = folder.path().join("gbk.sql");
    for (index, name) in ["資料", "分類"].into_iter().enumerate() {
        transfer::dump_configured(
            &mut sql,
            &source.name,
            &native,
            DumpSettings {
                file_charset: SqlDumpCharset::Gbk,
                append: index != 0,
                objects: Some(vec![DbObject {
                    schema: source.name.clone(),
                    name: name.into(),
                    kind: ObjectKind::Table,
                }]),
                ..Default::default()
            },
            |_| Ok(()),
        )
        .unwrap();
    }
    let encoded = std::fs::read(&native).unwrap();
    assert!(encoded.starts_with(b"-- Harbor SQL dump encoding: gbk\n"));
    assert!(std::str::from_utf8(&encoded).is_err());
    let mut decoded = Vec::new();
    decode_sql(SqlDumpCharset::Gbk, encoded.as_slice(), &mut decoded).unwrap();
    let decoded = String::from_utf8(decoded).unwrap();
    assert_eq!(decoded.matches("SET NAMES gbk;").count(), 2);
    assert!(decoded.contains("中文預設"));

    let gateway = TestGateway::new(opts);
    let (http, _) = Session::open(
        ConnectionConfig {
            port,
            ..Default::default()
        },
        gateway.tunnel.clone(),
    )
    .unwrap();
    let split = transfer::dump_over_http(
        &http,
        &source.name,
        folder.path(),
        DumpSettings {
            file_charset: SqlDumpCharset::Gbk,
            file_per_object: true,
            ..Default::default()
        },
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!((split.objects, split.rows), (2, 2));
    let split = split.output_directory.unwrap();

    crate::restore::restore(&mut sql, &native_target.name, &native).unwrap();
    http.restore(&http_target.name, &split).unwrap();
    for target in [&native_target.name, &http_target.name] {
        let (text, bytes): (String, Vec<u8>) = sql
            .query_first(format!("SELECT 內容, body FROM {target}.資料 WHERE id = 3"))
            .unwrap()
            .unwrap();
        assert_eq!(text, "中文数据 🚀");
        assert_eq!(bytes, [0, 255]);
        assert_eq!(
            sql.query_first::<i32, _>(format!("SELECT id FROM {target}.分類"),)
                .unwrap(),
            Some(7)
        );
    }
    gateway.stop();
}

struct DatabaseCleanup {
    opts: Opts,
    name: String,
}

impl Drop for DatabaseCleanup {
    fn drop(&mut self) {
        if let Ok(mut connection) = Conn::new(self.opts.clone()) {
            let _ =
                connection.query_drop(format!("DROP DATABASE {}", quote_identifier(&self.name)));
        }
    }
}

#[derive(Deserialize)]
struct Request {
    connect_info: ConnectInfo,
    query_info: QueryInfo,
}

#[derive(Deserialize)]
struct ConnectInfo {
    db: ValueText,
}

#[derive(Deserialize)]
struct QueryInfo {
    query: QueryBody,
}

#[derive(Deserialize)]
struct QueryBody {
    #[serde(rename = "@b")]
    batch: u8,
    #[serde(rename = "@e")]
    encoded: u8,
    #[serde(rename = "$text")]
    text: String,
}

#[derive(Deserialize)]
struct ValueText {
    #[serde(rename = "@e")]
    encoded: u8,
    #[serde(rename = "$text", default)]
    text: String,
}

impl ValueText {
    fn decoded(self) -> String {
        if self.encoded == 1 {
            String::from_utf8(STANDARD.decode(self.text).unwrap()).unwrap()
        } else {
            self.text
        }
    }
}

fn batch_commands(query: &str) -> Vec<&str> {
    let (separator, remaining) = if let Some(custom) = query.strip_prefix("DELIMITER ") {
        custom.split_once(';').unwrap()
    } else {
        (";", query)
    };
    remaining
        .split(separator)
        .map(str::trim)
        .filter(|statement| !statement.is_empty())
        .collect()
}

fn query_xml(connection: &mut Conn, query: &str) -> mysql::Result<String> {
    let mut result = connection.query_iter(query)?;
    let mut fields = String::new();
    let mut rows = String::new();
    let mut field_count = 0;
    let mut row_count = 0;
    let mut affected = 0;
    if let Some(set) = result.iter() {
        affected = set.affected_rows();
        for field in set.columns().as_ref() {
            let name = field.name_str();
            let table = field.table_str();
            let storage_type = field.column_type() as u8;
            write!(
                fields,
                "<f><n>{}</n><t>{}</t><m>0</m><d/><ty>{storage_type}</ty></f>",
                escape(name.as_ref()),
                escape(table.as_ref()),
            )
            .unwrap();
            field_count += 1;
        }
        for row in set {
            rows.push_str("<r>");
            let mut row: Row = row?;
            for column in 0..row.len() {
                let value = row
                    .take_opt::<Option<Vec<u8>>, _>(column)
                    .unwrap()
                    .map_err(mysql::Error::from)?;
                match value {
                    None => rows.push_str("<c l='0'>(NULL)</c>"),
                    Some(value) if value.is_empty() => rows.push_str("<c l='0'>_</c>"),
                    Some(value) => write!(
                        rows,
                        "<c l='{}'>{}</c>",
                        value.len(),
                        STANDARD.encode(value)
                    )
                    .unwrap(),
                }
            }
            rows.push_str("</r>");
            row_count += 1;
        }
    }
    Ok(format!(
        "<xml><result v='8.05'><e_i/><s_v>11.4.13-MariaDB</s_v><m_i/><a_r>{affected}</a_r><i_i>0</i_i><f_i c='{field_count}'>{fields}</f_i><r_i c='{row_count}'>{rows}</r_i></result></xml>"
    ))
}
