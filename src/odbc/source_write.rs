use std::collections::{HashMap, HashSet, hash_map::RandomState};
use std::hash::BuildHasher;
use std::ptr::null_mut;
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use crate::database::editable_query::{DuplicateSelect, duplicate_select};
use crate::database::{IMPORT_CANCELLED, IMPORT_UNCONFIRMED, quote_identifier};
use crate::sql_tools::{EditorTokenKind, editor_tokens, mysql_binary_literal, mysql_text_literal};

use super::{
    Cell, DBC, Handle, IMPORT_MEMORY_LIMIT, Manager, Resource, STMT, SUCCESS, SourceAction,
    SourceChange, SourceData, SourceLogin, SourceTable, check_import_cancelled, connect_source,
    environment, read_on_source, read_result, success, validate_read_request,
};

pub(crate) const SOURCE_SERVER_ID_SQL: &str =
    "SELECT @@hostname, @@port, @@server_id, @@version, @@datadir";
const VERIFY_BATCH_ROWS: usize = 128;
const VERIFY_BATCH_BYTES: usize = 512 * 1024;

pub(crate) struct SourceActionPlan {
    server: [String; 5],
    session: SessionContext,
    table: SourceObject,
    layout: SourceLayout,
    change: WritePlan,
    rows: Vec<VerifiedRow>,
}

impl SourceActionPlan {
    pub(crate) fn server_identity(&self) -> &[String; 5] {
        &self.server
    }

    pub(crate) fn table_identity(&self) -> (&str, &str) {
        (&self.table.schema, &self.table.name)
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }
}

#[derive(Eq, PartialEq)]
struct SessionContext {
    database: Option<String>,
    lower_case_names: bool,
    sql_mode: String,
    time_zone: String,
    charset: String,
    result_charset: Option<String>,
    collation: String,
}

#[derive(Clone, Eq, PartialEq)]
struct SourceObject {
    schema: String,
    name: String,
}

impl SourceObject {
    fn qualified(&self) -> String {
        format!(
            "{}.{}",
            quote_identifier(&self.schema),
            quote_identifier(&self.name)
        )
    }

    fn metadata_condition(&self, prefix: &str) -> String {
        let condition = |field, value: &str| {
            let name = format!("{prefix}{field}");
            let value = mysql_text_literal(value);
            format!(
                "{name} = {value} AND
                 CAST(IF(@@lower_case_table_names = 0, {name}, LOWER({name})) AS BINARY)
                 = CAST(IF(@@lower_case_table_names = 0, {value}, LOWER({value})) AS BINARY)"
            )
        };
        format!(
            "{} AND {}",
            condition("TABLE_SCHEMA", &self.schema),
            condition("TABLE_NAME", &self.name)
        )
    }
}

#[derive(Eq, PartialEq)]
struct SourceLayout {
    columns: Vec<SourceField>,
    primary_key: Vec<usize>,
}

#[derive(Eq, PartialEq)]
struct SourceField {
    name: String,
    data_type: String,
    column_type: String,
    extra: String,
    nullable: String,
    collation: Option<String>,
    generation: Option<String>,
}

enum WritePlan {
    Update(String),
    Delete,
}

impl WritePlan {
    fn statement(&self, table: &str) -> String {
        match self {
            Self::Update(assignments) => format!("UPDATE {table} SET {assignments}"),
            Self::Delete => format!("DELETE FROM {table}"),
        }
    }
}

struct VerifiedRow {
    key: String,
    fingerprint: String,
}

pub(crate) fn read_for_source_write(
    login: SourceLogin<'_>,
    sql: &str,
    source_table: Option<&SourceTable>,
    copy_foreign_keys: bool,
    action: &SourceAction,
    cancel: &AtomicBool,
) -> Result<(SourceData, SourceActionPlan), String> {
    let sql = validate_read_request(sql, source_table, copy_foreign_keys)?;
    let query =
        duplicate_select(sql).ok_or("导入后回写需要单表 SELECT，只能选择实际列或使用星号。")?;
    action.validate()?;
    let manager = Manager::load()?;
    let env = environment(&manager)?;
    let mut conn = connect_source(&env, login, true, Some(cancel))?;
    start_transaction(&mut conn, true, cancel)?;
    let server = server_identity(&conn, cancel)?;
    let session = session_context(&conn, cancel)?;
    let table = bind_source_table(&conn, &query, &action.table, &session, cancel)?;
    let layout = read_layout(&conn, &table, cancel)?;
    let change = prepare_change(&action.change, &layout, &session.sql_mode)?;
    let source = read_on_source(
        &conn,
        sql,
        source_table,
        copy_foreign_keys,
        Some(cancel),
        false,
    )?;
    let projection = projection_columns(&query, &layout, &source)?;
    let rows = verify_source_rows(&conn, &table, &layout, &projection, &source, cancel)?;
    // MySQL-family servers can reject EXPLAIN of a mutation in a READ ONLY
    // transaction. End the capture snapshot and explain the generated statement
    // on the same verified connection, without executing the mutation.
    let ended = unsafe { (manager.end_transaction)(DBC, conn.handle, 1) };
    conn.check(ended, "无法结束本次源库只读快照。")?;
    conn.transaction = false;
    start_transaction(&mut conn, false, cancel)?;
    if server_identity(&conn, cancel)? != server || session_context(&conn, cancel)? != session {
        return Err("源库连接在核对回写语句前发生变化。".into());
    }
    let sql = change.statement(&table.qualified());
    execute(&conn, &format!("EXPLAIN {sql} WHERE 1 = 0"), cancel).map_err(|error| {
        if error == IMPORT_CANCELLED {
            error
        } else {
            format!("源库无法解析或授权本次回写。 {error}")
        }
    })?;
    let plan = SourceActionPlan {
        server,
        session,
        table,
        layout,
        change,
        rows,
    };
    Ok((source, plan))
}

pub(crate) fn execute_source_write(
    login: SourceLogin<'_>,
    plan: SourceActionPlan,
    applied: &[bool],
    cancel: &AtomicBool,
) -> Result<usize, String> {
    if applied.len() != plan.len() {
        return Err("目标端回执与源数据行数不匹配，未开始回写。".into());
    }
    let count = applied.iter().filter(|&&row| row).count();
    if count == 0 {
        return Ok(0);
    }
    let manager = Manager::load()?;
    let row_count = manager
        .row_count
        .ok_or("当前 ODBC 驱动管理器无法确认源库写入行数。")?;
    let env = environment(&manager)?;
    let mut conn = connect_source(&env, login, false, Some(cancel))?;
    start_transaction(&mut conn, false, cancel)?;
    let token = format!("{:016x}", RandomState::new().hash_one(SystemTime::now()));
    let token = mysql_text_literal(&token);
    let guard = format!("@@autocommit = 0 AND @harbor_source_write = {token}");

    let outcome = (|| {
        if server_identity(&conn, cancel)? != plan.server
            || session_context(&conn, cancel)? != plan.session
        {
            return Err("回写连接与读取源数据时的服务器或会话设置不同。".into());
        }
        if read_layout(&conn, &plan.table, cancel)? != plan.layout {
            return Err("源表结构已变化，请重新核对本次导入。".into());
        }
        execute(
            &conn,
            &format!("SET @harbor_source_write = {token}"),
            cancel,
        )?;
        let table = plan.table.qualified();
        let fingerprint = fingerprint_sql(&plan.layout);
        let action = plan.change.statement(&table);
        for (index, (row, &accepted)) in plan.rows.iter().zip(applied).enumerate() {
            check_import_cancelled(Some(cancel))?;
            if !accepted {
                continue;
            }
            let original = mysql_text_literal(&row.fingerprint);
            let condition = format!("{} AND {fingerprint} = {original} AND {guard}", row.key);
            let affected = format!("SELECT 1 FROM {table} WHERE {condition} LIMIT 2 FOR UPDATE");
            if query_rows(&conn, &affected, cancel)?.len() != 1 {
                return Err(format!("源数据第 {} 行在导入后发生了变化。", index + 1));
            }
            let statement = execute(&conn, &format!("{action} WHERE {condition}"), cancel)?;
            let mut affected = -1;
            let status = unsafe { row_count(statement.handle, &mut affected) };
            statement.check(status, "无法核对源库回写行数。")?;
            drop(statement);
            let unchanged_update = matches!(plan.change, WritePlan::Update(_)) && affected == 0;
            if affected != 1 && !unchanged_update {
                return Err(format!("无法确认源数据第 {} 行的写入结果。", index + 1));
            }
            let warnings = query_rows(&conn, "SHOW WARNINGS", cancel)?;
            if warnings.iter().any(
                |warning| !matches!(warning.first(), Some(Cell::Text(level)) if level == "Note"),
            ) {
                return Err(format!("源库写入第 {} 行时报告了警告。", index + 1));
            }
        }
        let [verified] = query_text_row(&conn, &format!("SELECT ({guard})"), cancel)?;
        if verified.as_deref() != Some("1") {
            return Err("源库事务连接已变化，无法确认回写。".into());
        }
        check_import_cancelled(Some(cancel))
    })();
    if let Err(error) = outcome {
        return Err(rollback(&mut conn, error));
    }

    let status = unsafe { (manager.end_transaction)(DBC, conn.handle, 0) }; // SQL_COMMIT
    let diagnostic = (status != SUCCESS).then(|| conn.diagnostic()).flatten();
    if success(status)
        && !diagnostic
            .as_ref()
            .is_some_and(|(state, _)| unknown_transaction_state(state))
    {
        conn.transaction = false;
        Ok(count)
    } else if diagnostic
        .as_ref()
        .is_some_and(|(state, _)| state == "25S03")
    {
        conn.transaction = false;
        Err("源库未提交回写，并已回滚本次事务。".into())
    } else if diagnostic
        .as_ref()
        .is_some_and(|(state, _)| state == "25S02")
    {
        Err(rollback(&mut conn, "源库未能提交回写。".into()))
    } else {
        // A lost commit reply can mean the server has already committed.
        // The caller must not repeat source writes on that assumption.
        conn.transaction = false;
        Err(format!(
            "{IMPORT_UNCONFIRMED}无法确认源库回写是否已提交。请核对源数据后再重试。"
        ))
    }
}

fn start_transaction(
    conn: &mut Resource<'_>,
    read_only: bool,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let changed = unsafe {
        (conn.manager.set_connection)(conn.handle, 102, null_mut(), 0) // SQL_AUTOCOMMIT_OFF
    };
    conn.transaction = success(changed);
    conn.check(changed, "源库不支持关闭自动提交。")?;
    let [autocommit] = query_text_row(conn, "SELECT @@autocommit", cancel)?;
    if autocommit.as_deref() != Some("0") {
        return Err("无法确认源库已关闭自动提交。".into());
    }
    execute(
        conn,
        "SET SESSION lock_wait_timeout = LEAST(@@lock_wait_timeout, 15), SESSION innodb_lock_wait_timeout = LEAST(@@innodb_lock_wait_timeout, 15), SESSION foreign_key_checks = 1",
        cancel,
    )?;
    execute(
        conn,
        "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
        cancel,
    )?;
    let start = if read_only {
        "START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY"
    } else {
        "START TRANSACTION READ WRITE"
    };
    execute(conn, start, cancel)?;
    Ok(())
}

fn rollback(conn: &mut Resource<'_>, error: String) -> String {
    let status = unsafe { (conn.manager.end_transaction)(DBC, conn.handle, 1) }; // SQL_ROLLBACK
    conn.transaction = false;
    if status == SUCCESS {
        if error == IMPORT_CANCELLED {
            error
        } else {
            format!("{error} 源库回写事务已回滚。")
        }
    } else {
        format!("{IMPORT_UNCONFIRMED}{error} 无法确认源库回写已完整回滚。")
    }
}

fn unknown_transaction_state(state: &str) -> bool {
    state.starts_with("08") || matches!(state, "25S01" | "HY117" | "HYT00" | "HYT01")
}

fn server_identity(conn: &Resource<'_>, cancel: &AtomicBool) -> Result<[String; 5], String> {
    let [
        Some(host),
        Some(port),
        Some(id),
        Some(version),
        Some(datadir),
    ] = query_text_row(conn, SOURCE_SERVER_ID_SQL, cancel).map_err(|error| {
        if error == IMPORT_CANCELLED {
            error
        } else {
            "源库回写需要可核对服务器身份的 MySQL 或 MariaDB。".into()
        }
    })?
    else {
        return Err("源库未返回完整的服务器身份。".into());
    };
    if [
        host.as_str(),
        port.as_str(),
        id.as_str(),
        version.as_str(),
        datadir.as_str(),
    ]
    .iter()
    .any(|value| value.is_empty())
    {
        return Err("源库未返回完整的服务器身份。".into());
    }
    Ok([host, port, id, version, datadir])
}

fn session_context(conn: &Resource<'_>, cancel: &AtomicBool) -> Result<SessionContext, String> {
    let [
        database,
        Some(lower_case),
        Some(sql_mode),
        Some(time_zone),
        Some(charset),
        result_charset,
        Some(collation),
    ] = query_text_row(
        conn,
        "SELECT DATABASE(), @@lower_case_table_names, @@sql_mode, @@time_zone, @@character_set_client, @@character_set_results, @@collation_connection",
        cancel,
    )?
    else {
        return Err("无法读取源库会话设置。".into());
    };
    let lower_case_names = match lower_case.as_str() {
        "0" => false,
        "1" | "2" => true,
        _ => return Err("源库的标识符大小写设置无效。".into()),
    };
    Ok(SessionContext {
        database,
        lower_case_names,
        sql_mode,
        time_zone,
        charset,
        result_charset,
        collation,
    })
}

fn bind_source_table(
    conn: &Resource<'_>,
    query: &DuplicateSelect,
    action: &SourceTable,
    context: &SessionContext,
    cancel: &AtomicBool,
) -> Result<SourceObject, String> {
    let database = query
        .schema
        .as_deref()
        .or(context.database.as_deref())
        .ok_or("请为源查询指定数据库。")?;
    let matches = |left: &str, right: &str| {
        left == right || context.lower_case_names && left.to_lowercase() == right.to_lowercase()
    };
    if !matches(&query.table, &action.table)
        || [action.catalog.as_deref(), action.schema.as_deref()]
            .into_iter()
            .flatten()
            .any(|name| !matches(name, database))
    {
        return Err("回写指定的源表与 SELECT 查询的表不一致。".into());
    }
    let object = SourceObject {
        schema: database.to_owned(),
        name: query.table.clone(),
    };
    physical_table(conn, &object, cancel)
}

fn physical_table(
    conn: &Resource<'_>,
    table: &SourceObject,
    cancel: &AtomicBool,
) -> Result<SourceObject, String> {
    const UNSUPPORTED: &str = "源库回写需要支持事务的 InnoDB 普通表。";
    let condition = table.metadata_condition("");
    let [Some(schema), Some(name), Some(kind), Some(engine)] = query_text_row(
        conn,
        &format!("SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE, ENGINE FROM information_schema.TABLES WHERE {condition}"),
        cancel,
    ).map_err(|error| if error == IMPORT_CANCELLED { error } else { UNSUPPORTED.into() })?
    else {
        return Err(UNSUPPORTED.into());
    };
    if kind != "BASE TABLE" || !engine.eq_ignore_ascii_case("InnoDB") {
        return Err(UNSUPPORTED.into());
    }
    Ok(SourceObject { schema, name })
}

fn read_layout(
    conn: &Resource<'_>,
    table: &SourceObject,
    cancel: &AtomicBool,
) -> Result<SourceLayout, String> {
    // Acquire the base table metadata lock before checking the live layout.
    execute(
        conn,
        &format!("SELECT 1 FROM {} LIMIT 0", table.qualified()),
        cancel,
    )?;
    if physical_table(conn, table, cancel)? != *table {
        return Err("源表名称在导入期间发生变化。".into());
    }
    // A session-local temporary table can hide the permanent object of the
    // same name while information_schema still describes the permanent table.
    let [Some(_), Some(ddl)] = query_text_row(
        conn,
        &format!("SHOW CREATE TABLE {}", table.qualified()),
        cancel,
    )?
    else {
        return Err("无法确认回写源表的建表定义。".into());
    };
    if !ddl.starts_with("CREATE TABLE ") {
        return Err("源库回写需要持久化的 InnoDB 普通表。".into());
    }
    let condition = table.metadata_condition("");
    let columns = query_rows(
        conn,
        &format!("SELECT COLUMN_NAME, DATA_TYPE, COLUMN_TYPE, EXTRA, IS_NULLABLE, COLLATION_NAME, GENERATION_EXPRESSION
                  FROM information_schema.COLUMNS WHERE {condition} ORDER BY ORDINAL_POSITION"),
        cancel,
    )?.into_iter().map(|row| {
        let [Some(name), Some(data_type), Some(column_type), Some(extra), Some(nullable), collation, generation] = text_row(row)?
        else { return Err("源库未提供完整的列定义。".into()); };
        Ok(SourceField { name, data_type, column_type, extra, nullable, collation, generation })
    }).collect::<Result<Vec<_>,String>>()?;
    let primary = query_rows(
        conn,
        &format!(
            "SELECT COLUMN_NAME, SEQ_IN_INDEX FROM information_schema.STATISTICS
                  WHERE {condition} AND INDEX_NAME = 'PRIMARY' ORDER BY SEQ_IN_INDEX"
        ),
        cancel,
    )?;
    let primary_key = primary
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            let [Some(column), Some(sequence)] = text_row(row)? else {
                return Err("源库未提供完整的主键定义。".into());
            };
            if sequence.parse::<usize>().ok() != Some(index + 1) {
                return Err("源库返回的主键列顺序无效。".into());
            }
            columns
                .iter()
                .position(|field| field.name == column && field.nullable == "NO")
                .ok_or_else(|| "无法将源库主键定位到非空列。".into())
        })
        .collect::<Result<Vec<_>, String>>()?;
    if primary_key.is_empty() {
        return Err("源表需要主键才能执行导入后回写。".into());
    }
    Ok(SourceLayout {
        columns,
        primary_key,
    })
}

fn projection_columns(
    query: &DuplicateSelect,
    layout: &SourceLayout,
    data: &SourceData,
) -> Result<Vec<usize>, String> {
    let mut labels = Vec::new();
    let mut projection = Vec::new();
    for (name, label) in &query.projection {
        if let Some(name) = name {
            let position = column_position(layout, name).ok_or("源查询中有列无法对应到基础表。")?;
            labels.push(label.as_deref().unwrap_or(name));
            projection.push(position);
        } else {
            if label.is_some() {
                return Err("回写需要可以识别的基本列或星号。".into());
            }
            for (index, field) in layout.columns.iter().enumerate() {
                if !field
                    .extra
                    .split_ascii_whitespace()
                    .any(|part| part.eq_ignore_ascii_case("INVISIBLE"))
                {
                    labels.push(&field.name);
                    projection.push(index);
                }
            }
        }
    }
    if projection.len() != data.columns.len()
        || labels
            .iter()
            .zip(&data.columns)
            .any(|(label, field)| label.to_lowercase() != field.name.to_lowercase())
        || projection.iter().collect::<HashSet<_>>().len() != projection.len()
    {
        return Err("源查询列与读取结果不能唯一对应，请直接选择基础表的列。".into());
    }
    Ok(projection)
}

fn verify_source_rows(
    conn: &Resource<'_>,
    table: &SourceObject,
    layout: &SourceLayout,
    projection: &[usize],
    data: &SourceData,
    cancel: &AtomicBool,
) -> Result<Vec<VerifiedRow>, String> {
    let keys = layout
        .primary_key
        .iter()
        .map(|column| {
            projection
                .iter()
                .position(|index| index == column)
                .ok_or("源查询必须返回完整主键才能执行导入后回写。")
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut index_by_key = HashMap::with_capacity(data.rows.len());
    let mut verified = Vec::with_capacity(data.rows.len());
    let mut retained = 0;
    for (index, row) in data.rows.iter().enumerate() {
        check_import_cancelled(Some(cancel))?;
        if index_by_key.insert(hash_key(row, &keys)?, index).is_some() {
            return Err("源查询对同一个主键返回了多行数据。".into());
        }
        let condition = layout
            .primary_key
            .iter()
            .zip(&keys)
            .map(|(&field, &column)| {
                let field = &layout.columns[field];
                Ok(format!(
                    "{} <=> {}",
                    quote_identifier(&field.name),
                    key_literal(&row[column], &field.data_type)?
                ))
            })
            .collect::<Result<Vec<_>, String>>()?
            .join(" AND ");
        retained += condition.len() + 64 + size_of::<VerifiedRow>();
        if retained > IMPORT_MEMORY_LIMIT || condition.len() > VERIFY_BATCH_BYTES {
            return Err("源表主键数据过大，无法核对本次回写。".into());
        }
        verified.push(VerifiedRow {
            key: condition,
            fingerprint: String::new(),
        });
    }
    let visible = projection
        .iter()
        .map(|&index| quote_identifier(&layout.columns[index].name))
        .collect::<Vec<_>>()
        .join(", ");
    let table = table.qualified();
    let fingerprint = fingerprint_sql(layout);
    let mut from = 0;
    while from < verified.len() {
        let mut until = from;
        let mut bytes = 0;
        while until < verified.len()
            && until - from < VERIFY_BATCH_ROWS
            && bytes + verified[until].key.len() + 6 <= VERIFY_BATCH_BYTES + 6
        {
            bytes += verified[until].key.len() + 6;
            until += 1;
        }
        let where_keys = verified[from..until]
            .iter()
            .map(|row| format!("({})", row.key))
            .collect::<Vec<_>>()
            .join(" OR ");
        let limit = until - from + 1;
        let sql = format!(
            "SELECT {visible}, {fingerprint} FROM {table} WHERE {where_keys} LIMIT {limit}"
        );
        let rows = query_rows(conn, &sql, cancel)?;
        if rows.len() != until - from {
            return Err("无法在源表中找到与查询结果一一对应的主键。".into());
        }
        for mut row in rows {
            let fingerprint = cell_text(row.pop().ok_or("缺少源行校验值。")?)?
                .filter(|value| {
                    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
                .ok_or("源库返回的行校验值无效。")?;
            let index = index_by_key
                .get(&hash_key(&row, &keys)?)
                .copied()
                .filter(|index| (from..until).contains(index))
                .ok_or("源库返回的主键与读入的数据不一致。")?;
            if row != data.rows[index] || !verified[index].fingerprint.is_empty() {
                return Err("源数据在核对期间变化，或主键无法唯一定位。".into());
            }
            verified[index].fingerprint = fingerprint;
        }
        from = until;
    }
    Ok(verified)
}

fn column_position(layout: &SourceLayout, name: &str) -> Option<usize> {
    let name = name.to_lowercase();
    layout
        .columns
        .iter()
        .position(|column| column.name.to_lowercase() == name)
}

fn hash_key(row: &[Cell], columns: &[usize]) -> Result<[u8; 32], String> {
    let mut hash = Sha256::new();
    for &column in columns {
        let (tag, bytes) = match row.get(column) {
            Some(Cell::Text(value)) => (1, value.as_bytes()),
            Some(Cell::Binary(value)) => (2, value.as_slice()),
            _ => return Err("源记录缺少非空主键值。".into()),
        };
        hash.update([tag]);
        hash.update(bytes.len().to_le_bytes());
        hash.update(bytes);
    }
    Ok(hash.finalize().into())
}

fn key_literal(value: &Cell, data_type: &str) -> Result<String, String> {
    match value {
        Cell::Text(value)
            if matches!(
                data_type,
                "tinyint" | "smallint" | "mediumint" | "int" | "bigint" | "year" | "bit"
            ) =>
        {
            value
                .parse::<i128>()
                .map(|value| value.to_string())
                .map_err(|_| "源主键的整数格式无效。".into())
        }
        Cell::Text(value) if data_type == "decimal" => {
            let digits = value.trim_start_matches(['+', '-']);
            if digits
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'.')
                && digits.bytes().any(|byte| byte.is_ascii_digit())
                && digits.matches('.').count() <= 1
                && value.len() - digits.len() <= 1
            {
                Ok(value.clone())
            } else {
                Err("源主键的小数格式无效。".into())
            }
        }
        Cell::Text(value) => Ok(mysql_text_literal(value)),
        Cell::Binary(value) => Ok(mysql_binary_literal(value)),
        Cell::Null => Err("源记录缺少非空主键值。".into()),
    }
}

fn fingerprint_sql(layout: &SourceLayout) -> String {
    let parts = layout
        .columns
        .iter()
        .map(|field| {
            let name = quote_identifier(&field.name);
            let bytes = match field.data_type.as_str() {
                "float" | "double" => format!("CAST(({name} + 0e0) AS BINARY)"),
                "enum" | "set" => format!("CAST(({name} + 0) AS BINARY)"),
                "timestamp" => format!(
                    "COALESCE(CAST(UNIX_TIMESTAMP({name}) AS BINARY), CAST({name} AS BINARY))"
                ),
                _ => format!("CAST({name} AS BINARY)"),
            };
            format!("COALESCE(SHA2({bytes},256),'#')")
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("SHA2(CONCAT({parts}),256)")
}

fn prepare_change(
    change: &SourceChange,
    layout: &SourceLayout,
    mode: &str,
) -> Result<WritePlan, String> {
    let SourceChange::Update { assignments } = change else {
        return Ok(WritePlan::Delete);
    };
    if assignments.is_empty() {
        return Err("请至少填写一个需更新的源列。".into());
    }
    let mut columns = HashSet::new();
    let setters = assignments
        .iter()
        .map(|assignment| {
            let index = column_position(layout, &assignment.column)
                .ok_or_else(|| format!("找不到需要回写的源列「{}」。", assignment.column))?;
            let field = &layout.columns[index];
            if !columns.insert(index)
                || layout.primary_key.contains(&index)
                || field
                    .generation
                    .as_ref()
                    .is_some_and(|expression| !expression.is_empty())
                || field
                    .extra
                    .split_ascii_whitespace()
                    .any(|part| part.eq_ignore_ascii_case("auto_increment"))
            {
                return Err(format!(
                    "源列「{}」重复、属于主键，或由数据库生成。",
                    field.name
                ));
            }
            let expression = checked_expression(&assignment.expression, layout, mode)?;
            Ok(format!("{} = {expression}", quote_identifier(&field.name)))
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(WritePlan::Update(setters.join(", ")))
}

fn checked_expression(
    expression: &str,
    layout: &SourceLayout,
    mode: &str,
) -> Result<String, String> {
    const INVALID: &str = "请填写只用本行字段、常量和常用标量函数的回写表达式。";
    const FUNCTIONS: &[&str] = &[
        "ABS",
        "CEIL",
        "CEILING",
        "COALESCE",
        "CONCAT",
        "CONCAT_WS",
        "CURRENT_DATE",
        "CURRENT_TIMESTAMP",
        "FLOOR",
        "IF",
        "IFNULL",
        "LCASE",
        "LENGTH",
        "LOWER",
        "LTRIM",
        "NOW",
        "NULLIF",
        "REPLACE",
        "ROUND",
        "RTRIM",
        "SUBSTRING",
        "SYSDATE",
        "TRIM",
        "UCASE",
        "UPPER",
    ];
    const WORDS: &[&str] = &[
        "NULL",
        "TRUE",
        "FALSE",
        "CURRENT_DATE",
        "CURRENT_TIMESTAMP",
        "CASE",
        "WHEN",
        "THEN",
        "ELSE",
        "END",
        "AND",
        "OR",
        "XOR",
        "NOT",
        "IS",
        "DIV",
        "MOD",
        "LIKE",
    ];
    let expression = expression.trim();
    if expression.is_empty() || expression.contains('\0') || expression.len() > 16 * 1024 {
        return Err(INVALID.into());
    }
    let mode_has = |name| mode.split(',').any(|part| part.eq_ignore_ascii_case(name));
    let mut tokens = editor_tokens(
        expression,
        mode_has("NO_BACKSLASH_ESCAPES"),
        mode_has("ANSI_QUOTES"),
    )
    .peekable();
    let mut depth = 0_u32;
    while let Some(token) = tokens.next() {
        match token.kind {
            EditorTokenKind::Word => {
                let word = token.text.to_ascii_uppercase();
                let call = tokens
                    .peek()
                    .is_some_and(|next| next.kind == EditorTokenKind::Symbol('('));
                if call && !FUNCTIONS.contains(&word.as_str())
                    || !call
                        && !WORDS.contains(&word.as_str())
                        && !token
                            .text
                            .as_bytes()
                            .first()
                            .is_some_and(u8::is_ascii_digit)
                        && column_position(layout, token.text).is_none()
                {
                    return Err(INVALID.into());
                }
            }
            EditorTokenKind::Identifier {
                quote,
                closed: true,
            } => {
                let name = &token.text[1..token.text.len() - 1];
                let name = if quote == b'`' {
                    name.replace("``", "`")
                } else {
                    name.replace("\"\"", "\"")
                };
                if column_position(layout, &name).is_none()
                    || tokens
                        .peek()
                        .is_some_and(|next| next.kind == EditorTokenKind::Symbol('('))
                {
                    return Err(INVALID.into());
                }
            }
            EditorTokenKind::Protected { closed: true } if token.text.starts_with(['\'', '"']) => {}
            EditorTokenKind::Symbol('(') => depth += 1,
            EditorTokenKind::Symbol(')') if depth > 0 => depth -= 1,
            EditorTokenKind::Symbol(',') if depth > 0 => {}
            EditorTokenKind::Symbol(
                '+' | '-' | '*' | '/' | '%' | '=' | '<' | '>' | '!' | '|' | '&' | '^' | '~',
            ) => {}
            EditorTokenKind::Symbol('.')
                if tokens.peek().is_some_and(|next| {
                    next.text.as_bytes().first().is_some_and(u8::is_ascii_digit)
                }) => {}
            _ => return Err(INVALID.into()),
        }
    }
    if depth != 0 {
        return Err(INVALID.into());
    }
    Ok(format!("({expression})"))
}

fn execute<'a>(
    conn: &Resource<'a>,
    sql: &str,
    cancel: &AtomicBool,
) -> Result<Resource<'a>, String> {
    check_import_cancelled(Some(cancel))?;
    let statement = Resource::allocate(conn.manager, STMT, conn.handle)?;
    let mut wide: Vec<_> = sql.encode_utf16().collect();
    let length = i32::try_from(wide.len()).map_err(|_| "源库语句过长。")?;
    wide.push(0);
    let result = unsafe {
        (conn.manager.set_statement)(statement.handle, 0, 30 as Handle, 0);
        (conn.manager.execute)(statement.handle, wide.as_ptr(), length)
    };
    check_import_cancelled(Some(cancel))?;
    statement.check(result, "无法执行源库回写语句。")?;
    if result != SUCCESS {
        return Err("源库回写语句返回了警告。".into());
    }
    Ok(statement)
}

fn query_rows(
    conn: &Resource<'_>,
    sql: &str,
    cancel: &AtomicBool,
) -> Result<Vec<Vec<Cell>>, String> {
    let statement = execute(conn, sql, cancel)?;
    read_result(&statement, false, Some(cancel), false).map(|data| data.rows)
}

fn query_text_row<const N: usize>(
    conn: &Resource<'_>,
    sql: &str,
    cancel: &AtomicBool,
) -> Result<[Option<String>; N], String> {
    let [row] = query_rows(conn, sql, cancel)?
        .try_into()
        .map_err(|_| "源库未返回可核对的元数据。")?;
    text_row(row)
}

fn text_row<const N: usize>(row: Vec<Cell>) -> Result<[Option<String>; N], String> {
    row.into_iter()
        .map(cell_text)
        .collect::<Result<Vec<_>, _>>()?
        .try_into()
        .map_err(|_| "源库返回的元数据列数不匹配。".into())
}

fn cell_text(cell: Cell) -> Result<Option<String>, String> {
    match cell {
        Cell::Null => Ok(None),
        Cell::Text(value) => Ok(Some(value)),
        Cell::Binary(value) => String::from_utf8(value)
            .map(Some)
            .map_err(|_| "源库返回的元数据包含无效文本。".into()),
    }
}
