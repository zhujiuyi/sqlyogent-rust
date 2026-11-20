use crate::database::{
    Cell, Command, ConnectionConfig, DbObject, FieldValue, ObjectKind, QueryReport, Response,
    TableEdit, TablePage, Worker, quote_identifier,
};
use crate::sql_tools::{parse_csv, split_script};
use std::net::IpAddr;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Exercises the database worker against a disposable database on this machine.
pub fn run(host: &str, port: u16, user: &str, password: &str) -> Result<(), String> {
    require(
        host.parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback()),
        "请使用本机回环地址连接临时测试服务器。",
    )?;
    let mut db = Smoke {
        worker: Worker::new(),
        request_id: 0,
    };
    let config = ConnectionConfig {
        host: host.into(),
        port,
        user: user.into(),
        password: password.into(),
        ..Default::default()
    };
    let Response::Connected(server) = db.request("连接数据库", Command::Connect(config.clone()))?
    else {
        return Err("连接时未返回服务器信息。".into());
    };
    require(
        server.connection_id != 0 && !server.version.is_empty(),
        "服务器返回的连接信息不完整。",
    )?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let schema = format!("harbor_smoke_{}_{timestamp:x}", std::process::id());
    let quoted = quote_identifier(&schema);
    db.query(&format!("CREATE DATABASE {quoted} CHARACTER SET utf8mb4"))?;

    let result = run_checks(&mut db, &schema, server.connection_id)
        .and_then(|()| check_table_editor(&mut db, &schema))
        .and_then(|()| check_cancellation(&mut db, config, server.connection_id));
    let cleanup = db
        .query(&format!("ROLLBACK; DROP DATABASE {quoted}"))
        .map(|_| ());
    let disconnect = db
        .request("断开连接", Command::Disconnect)
        .and_then(|response| {
            require(
                matches!(response, Response::Disconnected),
                "未收到断开连接的确认。",
            )
        });
    match (result, cleanup.and(disconnect)) {
        (Err(error), Err(cleanup)) => Err(format!("{error}\n清理测试数据失败。 {cleanup}")),
        (result, cleanup) => result.and(cleanup),
    }
}

struct Smoke {
    worker: Worker,
    request_id: u64,
}

impl Smoke {
    fn request(&mut self, action: &str, command: Command) -> Result<Response, String> {
        let id = self.queue(action, command)?;
        self.receive(action, id)
    }

    fn queue(&mut self, action: &str, command: Command) -> Result<u64, String> {
        self.request_id += 1;
        let id = self.request_id;
        self.worker
            .send(id, command)
            .map_err(|error| format!("{action}: {error}"))?;
        Ok(id)
    }

    fn receive(&self, action: &str, id: u64) -> Result<Response, String> {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Some(event) = self.worker.try_recv() {
                if event.id == id {
                    return event.result.map_err(|error| format!("{action}: {error}"));
                }
            } else {
                thread::sleep(Duration::from_millis(10));
            }
        }
        Err(format!("{action}超时，数据库服务在 15 秒内未响应。"))
    }

    fn query(&mut self, sql: &str) -> Result<QueryReport, String> {
        let Response::Queried(report) = self.request(
            "执行查询",
            Command::Query {
                sql: sql.into(),
                limit: 100,
            },
        )?
        else {
            return Err("数据库服务未返回查询结果。".into());
        };
        if let Some(error) = report.error.as_deref() {
            return Err(format!("SQL 执行失败。 {error}\n{sql}"));
        }
        Ok(report)
    }
}

fn run_checks(db: &mut Smoke, schema: &str, connection_id: u32) -> Result<(), String> {
    let response = db.request("列出数据库", Command::ListSchemas)?;
    require(
        matches!(response, Response::Schemas(schemas) if schemas.iter().any(|name| name == schema)),
        "数据库列表中未找到新建的测试库。",
    )?;
    let response = db.request(
        "切换数据库",
        Command::UseSchema {
            schema: schema.into(),
        },
    )?;
    require(
        matches!(response, Response::SchemaChanged(name) if name == schema),
        "未能切换到测试库。",
    )?;
    expect_rows(
        &db.query("SELECT DATABASE(), CONNECTION_ID()")?,
        &[vec![
            Cell::Text(schema.into()),
            Cell::Text(connection_id.to_string()),
        ]],
        "核对数据库名和连接 ID",
    )?;

    const SETUP: &str = "
        CREATE TABLE records (id INT PRIMARY KEY, label VARCHAR(80), payload VARBINARY(8)) ENGINE=InnoDB;
        CREATE VIEW record_labels AS SELECT id, label FROM records;
        DELIMITER $$
        CREATE PROCEDURE read_records() BEGIN
            SELECT 'first' AS label;
            SELECT COUNT(*) AS total FROM records;
        END$$
        DELIMITER ;
    ";
    for statement in split_script(SETUP) {
        db.query(statement.sql)?;
    }

    let response = db.request(
        "读取数据库对象",
        Command::ListObjects {
            schema: schema.into(),
        },
    )?;
    let Response::Objects {
        schema: returned_schema,
        objects,
    } = response
    else {
        return Err("未收到数据库对象列表。".into());
    };
    require(returned_schema == schema, "返回的对象列表属于其他数据库。")?;
    let table = fixture_object(&objects, "records", ObjectKind::Table)?;
    fixture_object(&objects, "record_labels", ObjectKind::View)?;
    let procedure = fixture_object(&objects, "read_records", ObjectKind::Procedure)?;

    let response = db.request(
        "读取表结构",
        Command::Inspect {
            object: table.clone(),
        },
    )?;
    require(
        matches!(response, Response::Inspected(details)
            if details.ddl_error.is_none() && details.ddl.contains("CREATE TABLE")
                && details.columns.iter().map(|column| column.name.as_str()).eq(["id", "label", "payload"])),
        "表结构中的列或 SQL 定义与预期不符。",
    )?;
    let response = db.request("读取存储过程定义", Command::Inspect { object: procedure })?;
    require(
        matches!(response, Response::Inspected(details)
            if details.ddl_error.is_none() && details.ddl.contains("PROCEDURE")),
        "存储过程定义与预期不符。",
    )?;

    db.query("INSERT INTO records VALUES (1, 'initial', X'0041FF'), (2, 'temporary', NULL)")?;
    db.query("UPDATE records SET label = 'updated' WHERE id = 1")?;
    db.query("DELETE FROM records WHERE id = 2")?;
    let response = db.request(
        "预览表数据",
        Command::Preview {
            object: table.clone(),
            limit: 10,
        },
    )?;
    let Response::Previewed(preview) = response else {
        return Err("未返回表数据预览。".into());
    };
    expect_rows(
        &preview,
        &[vec![
            Cell::Text("1".into()),
            Cell::Text("updated".into()),
            Cell::Binary(vec![0, 65, 255]),
        ]],
        "核对增删改后的表数据",
    )?;

    db.query("START TRANSACTION")?;
    db.query("UPDATE records SET label = 'uncommitted' WHERE id = 1")?;
    expect_rows(
        &db.query("SELECT label FROM records WHERE id = 1")?,
        &[vec![Cell::Text("uncommitted".into())]],
        "核对不同请求使用同一事务",
    )?;
    db.query("ROLLBACK")?;
    expect_rows(
        &db.query("SELECT label FROM records WHERE id = 1")?,
        &[vec![Cell::Text("updated".into())]],
        "核对事务回滚",
    )?;

    let report = db.query("CALL read_records()")?;
    let mut data = report.sets.iter().filter(|set| !set.columns.is_empty());
    require(
        data.next()
            .is_some_and(|set| set.rows == [vec![Cell::Text("first".into())]])
            && data
                .next()
                .is_some_and(|set| set.rows == [vec![Cell::Text("1".into())]])
            && data.next().is_none(),
        "存储过程返回的两个结果集与预期不符。",
    )?;

    let input = "id,label\r\n2,\"逗号,换行\n第二行\"\r\n";
    let csv = parse_csv(input.as_bytes(), b',', true).map_err(|error| error.to_string())?;
    let columns = csv.headers.ok_or("测试 CSV 缺少表头。")?;
    let rows = csv
        .rows
        .into_iter()
        .map(|row| row.into_iter().map(Some).collect())
        .collect();
    let response = db.request(
        "导入 CSV",
        Command::ImportRows {
            object: table,
            columns,
            rows,
        },
    )?;
    require(
        matches!(response, Response::Imported(1)),
        "测试 CSV 导入行数应为 1。",
    )?;
    expect_rows(
        &db.query("SELECT label, payload FROM records WHERE id = 2")?,
        &[vec![Cell::Text("逗号,换行\n第二行".into()), Cell::Null]],
        "核对 CSV 导入数据",
    )
}

fn check_cancellation(
    db: &mut Smoke,
    config: ConnectionConfig,
    connection_id: u32,
) -> Result<(), String> {
    let mode = db.query("SHOW VARIABLES LIKE 'performance_schema'")?;
    let enabled = mode.sets.iter().flat_map(|set| &set.rows).any(
        |row| matches!(row.get(1), Some(Cell::Text(value)) if value.eq_ignore_ascii_case("ON")),
    );
    if !enabled {
        return Ok(());
    }

    let mut observer = Smoke {
        worker: Worker::new(),
        request_id: 0,
    };
    observer.request("建立取消测试所需的监视连接", Command::Connect(config))?;
    const SLOW_QUERY: &str = "SELECT id FROM records WHERE id = 1 AND SLEEP(8) = 0";
    let cancelled = db.queue(
        "提交长时间运行的查询",
        Command::Query {
            sql: SLOW_QUERY.into(),
            limit: 1,
        },
    )?;

    // Wait until the server has accepted the query before requesting cancellation.
    let inspect = format!(
        "SELECT 1 FROM information_schema.PROCESSLIST WHERE ID = {connection_id} AND INFO = '{SLOW_QUERY}'"
    );
    let start_deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let status = observer.query(&inspect)?;
        if status.sets.iter().any(|set| !set.rows.is_empty()) {
            break;
        }
        require(Instant::now() < start_deadline, "测试查询未开始运行。")?;
    }
    db.worker
        .cancel(cancelled)
        .map_err(|error| format!("发送取消请求失败。 {error}"))?;
    let following = db.queue(
        "提交下一条查询",
        Command::Query {
            sql: "SELECT CONNECTION_ID() WHERE SLEEP(0.2) = 0".into(),
            limit: 1,
        },
    )?;

    let cancel_deadline = Instant::now() + Duration::from_secs(6);
    loop {
        if let Some(event) = db.worker.try_recv_cancel() {
            require(event.id == cancelled, "收到取消确认的查询与预期不符。")?;
            event
                .result
                .map_err(|error| format!("数据库未确认取消。 {error}"))?;
            break;
        }
        require(Instant::now() < cancel_deadline, "等待取消确认超时。")?;
        thread::sleep(Duration::from_millis(10));
    }

    let Response::Queried(report) = db.receive("读取被取消的查询结果", cancelled)? else {
        return Err("未返回被取消的查询结果。".into());
    };
    require(
        report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("1317")),
        "数据库未返回查询中断状态。",
    )?;
    require(
        db.worker.cancel(cancelled).is_err(),
        "连接服务接受了已经完成的查询的取消请求。",
    )?;

    let Response::Queried(report) = db.receive("读取下一条查询结果", following)? else {
        return Err("未返回下一条查询结果。".into());
    };
    expect_rows(
        &report,
        &[vec![Cell::Text(connection_id.to_string())]],
        "核对下一条查询仍使用原连接",
    )
}

fn check_table_editor(db: &mut Smoke, schema: &str) -> Result<(), String> {
    db.query(
        "CREATE TABLE edit_records (
            number BIGINT UNSIGNED NOT NULL,
            tenant VARBINARY(8) NOT NULL,
            label VARCHAR(8) NOT NULL DEFAULT 'default',
            note VARCHAR(16) NULL,
            amount DECIMAL(30, 6) NOT NULL DEFAULT 0,
            noted_at DATETIME(6) NOT NULL,
            label_length INT GENERATED ALWAYS AS (CHAR_LENGTH(label)) STORED,
            PRIMARY KEY (tenant, number, amount)
        ) ENGINE=InnoDB",
    )?;
    db.query(
        "INSERT INTO edit_records (number, tenant, label, note, amount, noted_at) VALUES
            (18446744073709551615, X'00FF', 'original', 'old', 1, '2024-02-29 01:02:03.123456'),
            (18446744073709551615, X'01FF', 'neighbor', NULL, 2, '2024-02-29 01:02:03.123456')",
    )?;
    let object = DbObject {
        schema: schema.into(),
        name: "edit_records".into(),
        kind: ObjectKind::Table,
    };
    let page = editor_page(db, &object, 0)?;
    require(
        page.primary_key == [1, 0, 4]
            && page.edit_error.is_none()
            && page.has_more
            && page.rows.len() == 1
            && page.columns.get(6).is_some_and(|column| !column.writable)
            && page.rows[0].cells[..2]
                == [
                    Cell::Text("18446744073709551615".into()),
                    Cell::Binary(vec![0, 255]),
                ],
        "编辑器未按完整复合主键读取分页数据或标记生成列。",
    )?;
    let change = |name: &str, value: &str| (name.into(), FieldValue::Text(value.into()));
    let response = db.request(
        "保存数据编辑",
        Command::EditTable {
            object: object.clone(),
            edit: TableEdit::Update {
                original: page.rows[0].clone(),
                values: vec![
                    ("label".into(), FieldValue::Default),
                    ("note".into(), FieldValue::Null),
                    change("amount", "123456789012345678901234.567890"),
                    change("noted_at", "2024-02-29 12:34:56.123"),
                ],
            },
        },
    )?;
    require(
        matches!(response, Response::TableEdited { affected_rows: 1 }),
        "编辑器未保存一行数据。",
    )?;
    let query = "SELECT label, note, amount, noted_at, label_length FROM edit_records WHERE tenant = X'00FF'";
    let expected = [vec![
        Cell::Text("default".into()),
        Cell::Null,
        Cell::Text("123456789012345678901234.567890".into()),
        Cell::Text("2024-02-29 12:34:56.123000".into()),
        Cell::Text("7".into()),
    ]];
    expect_rows(
        &db.query(query)?,
        &expected,
        "核对默认值、NULL、定点数、日期时间和生成列",
    )?;
    let original = editor_page(db, &object, 0)?
        .rows
        .into_iter()
        .next()
        .ok_or("更新后未找到目标行。")?;
    let rejected = db.request(
        "拒绝超出列长度的数据",
        Command::EditTable {
            object: object.clone(),
            edit: TableEdit::Update {
                original: original.clone(),
                values: vec![
                    change("amount", "0"),
                    change("label", "过长的测试数据123456"),
                ],
            },
        },
    );
    require(
        rejected.is_err_and(|error| error.contains("数据库返回警告") || error.contains("1406")),
        "编辑器未拒绝会被截断的数据。",
    )?;
    expect_rows(&db.query(query)?, &expected, "核对被拒绝编辑的回滚结果")?;

    db.query("UPDATE edit_records SET note = 'newer' WHERE tenant = X'00FF'")?;
    let stale = db.request(
        "拒绝删除已被其他操作修改的行",
        Command::EditTable {
            object: object.clone(),
            edit: TableEdit::Delete { original },
        },
    );
    require(
        stale.is_err_and(|error| error.contains("请刷新")),
        "编辑器未识别过期的行快照。",
    )?;

    let inserted = db.request(
        "新增一行",
        Command::EditTable {
            object: object.clone(),
            edit: TableEdit::Insert {
                values: vec![
                    change("number", "18446744073709551615"),
                    change("tenant", "0x02AA"),
                    change("noted_at", "2024-02-29 00:00:00"),
                    ("label".into(), FieldValue::Default),
                ],
            },
        },
    )?;
    require(
        matches!(inserted, Response::TableEdited { affected_rows: 1 }),
        "编辑器未新增一行数据。",
    )?;
    let mut last = editor_page(db, &object, 2)?;
    require(
        last.rows.len() == 1
            && !last.has_more
            && last.rows[0].cells[1] == Cell::Binary(vec![2, 170]),
        "新增行未出现在按主键排序的最后一页。",
    )?;
    let deleted = db.request(
        "删除刚新增的行",
        Command::EditTable {
            object: object.clone(),
            edit: TableEdit::Delete {
                original: last.rows.remove(0),
            },
        },
    )?;
    require(
        matches!(deleted, Response::TableEdited { affected_rows: 1 }),
        "编辑器未删除一行数据。",
    )?;
    let imported = db.request(
        "拒绝会截断输入的导入",
        Command::ImportRows {
            object,
            columns: ["number", "tenant", "label", "noted_at"]
                .map(str::to_owned)
                .into(),
            rows: [
                ["1", "a", "valid", "2024-02-29 12:00:00"],
                ["2", "a", "标签超过了八个字符", "2024-02-29 12:00:00"],
            ]
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|value| Some(value.to_owned()))
                    .collect()
            })
            .collect(),
        },
    );
    require(
        imported.is_err_and(|error| error.contains("第 2 行") && error.contains("事务已回滚")),
        "导入未回滚含有会被截断的输入。",
    )?;
    expect_rows(
        &db.query("SELECT HEX(tenant), label, note FROM edit_records ORDER BY tenant")?,
        &[
            vec![
                Cell::Text("00FF".into()),
                Cell::Text("default".into()),
                Cell::Text("newer".into()),
            ],
            vec![
                Cell::Text("01FF".into()),
                Cell::Text("neighbor".into()),
                Cell::Null,
            ],
        ],
        "核对复合主键没有误改相邻记录",
    )
}

fn editor_page(db: &mut Smoke, object: &DbObject, offset: usize) -> Result<TablePage, String> {
    match db.request(
        "读取数据页",
        Command::TablePage {
            object: object.clone(),
            limit: 1,
            offset,
        },
    )? {
        Response::TablePage(page) => Ok(page),
        _ => Err("编辑器未返回数据页。".into()),
    }
}

fn fixture_object(objects: &[DbObject], name: &str, kind: ObjectKind) -> Result<DbObject, String> {
    objects
        .iter()
        .find(|object| object.name == name && object.kind == kind)
        .cloned()
        .ok_or_else(|| format!("对象列表中缺少测试对象 {kind:?} {name:?}。"))
}

fn expect_rows(report: &QueryReport, rows: &[Vec<Cell>], action: &str) -> Result<(), String> {
    if let Some(error) = report.error.as_deref() {
        return Err(format!("{action}: {error}"));
    }
    if let [set] = report.sets.as_slice() {
        if !set.truncated && set.rows == rows {
            return Ok(());
        }
    }
    Err(format!("{action}时返回了意外的结果。 {:?}", report.sets))
}

fn require(condition: bool, reason: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(reason.into())
    }
}
