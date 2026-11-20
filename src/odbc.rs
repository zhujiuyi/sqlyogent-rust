use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::AtomicBool;

use libloading::Library;
use zeroize::Zeroizing;

use crate::database::{
    Cell, IMPORT_MEMORY_LIMIT, MAX_IMPORT_COLUMNS, MAX_IMPORT_ROWS, OdbcColumnDefinition,
    check_import_cancelled,
};

mod file_dsn;
mod foreign_keys;
mod indexes;
pub(crate) mod mapping;
mod prepared_source_action;
mod source_action;
mod source_write;
mod tables;
pub use file_dsn::{
    create_file_dsn, file_dsn_connection, installed_odbc_drivers, validate_file_dsn_path,
};
pub use foreign_keys::SourceForeignKey;
pub(crate) use indexes::SourceIndex;
pub use indexes::SourceTable;
pub use mapping::map_single_table_metadata;
pub use prepared_source_action::{PreparedSourceAction, read_with_source_action};
pub use source_action::{SourceAction, SourceAssignment, SourceChange};
pub(crate) use source_write::{
    SOURCE_SERVER_ID_SQL, SourceActionPlan, execute_source_write, read_for_source_write,
};
pub(crate) use tables::discover_tables_for_login;
pub use tables::{DiscoveredTable, discover_tables};

pub struct SourceData {
    pub columns: Vec<SourceColumn>,
    pub rows: Vec<Vec<Cell>>,
    pub(crate) indexes: Vec<SourceIndex>,
    pub foreign_keys: Vec<SourceForeignKey>,
    pub source_table: Option<SourceTable>,
}

pub struct SourceColumn {
    pub name: String,
    pub definition: OdbcColumnDefinition,
}

pub fn read(connection: String, sql: &str) -> Result<SourceData, String> {
    read_with_cancel(connection, sql, None, false, None, false)
}

pub fn read_columns(connection: String, sql: &str) -> Result<SourceData, String> {
    read_with_cancel(connection, sql, None, false, None, true)
}

pub fn read_with_metadata(
    connection: String,
    sql: &str,
    source_table: &SourceTable,
    copy_foreign_keys: bool,
    columns_only: bool,
) -> Result<SourceData, String> {
    read_with_cancel(
        connection,
        sql,
        Some(source_table),
        copy_foreign_keys,
        None,
        columns_only,
    )
}

pub(crate) fn read_with_cancel(
    connection: String,
    sql: &str,
    source_table: Option<&SourceTable>,
    copy_foreign_keys: bool,
    cancel: Option<&AtomicBool>,
    columns_only: bool,
) -> Result<SourceData, String> {
    let connection = Zeroizing::new(connection);
    read_query(
        SourceLogin::ConnectionString(&connection),
        sql,
        source_table,
        copy_foreign_keys,
        cancel,
        columns_only,
    )
}

const MAX_DSN_LENGTH: usize = 32;

pub(crate) fn validate_dsn(dsn: &str) -> Result<(), String> {
    if dsn.is_empty()
        || dsn.trim_start() != dsn
        || dsn.encode_utf16().count() > MAX_DSN_LENGTH
        || dsn.contains([
            '[', ']', '{', '}', '(', ')', ',', ';', '?', '*', '=', '!', '@', '\\',
        ])
        || dsn.chars().any(char::is_control)
    {
        return Err("dsn 须为有效的 ODBC 数据源名称，最多 32 个 UTF-16 代码单元。".into());
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) enum SourceLogin<'a> {
    ConnectionString(&'a str),
    Dsn {
        name: &'a str,
        user: Option<&'a str>,
        password: Option<&'a str>,
    },
}

impl SourceLogin<'_> {
    fn connect(
        self,
        environment: &Resource<'_>,
        source: &Resource<'_>,
        cancel: Option<&AtomicBool>,
    ) -> Result<Return, String> {
        match self {
            Self::ConnectionString(input) => {
                let mut input = Zeroizing::new(connection_text(input)?);
                let size = i16::try_from(input.len()).map_err(|_| "ODBC 连接字符串过长。")?;
                input.push(0);
                // No output connection string or prompts are requested.
                let connected = unsafe {
                    (source.manager.driver_connect)(
                        source.handle,
                        null_mut(),
                        input.as_mut_ptr(),
                        size,
                        null_mut(),
                        0,
                        null_mut(),
                        0,
                    )
                };
                Ok(connected)
            }
            Self::Dsn {
                name,
                user,
                password,
            } => {
                validate_dsn(name)?;
                // A missing DSN can otherwise fall back to a default driver.
                require_registered_dsn(environment, name, cancel)?;
                let mut dsn: Vec<_> = name.encode_utf16().chain(std::iter::once(0)).collect();
                let mut user = connection_argument(user)?;
                let mut password = connection_argument(password)?;
                let (user_ptr, user_len) = argument_parts(&mut user);
                let (password_ptr, password_len) = argument_parts(&mut password);
                let connected = unsafe {
                    (source.manager.connect)(
                        source.handle,
                        dsn.as_mut_ptr(),
                        (dsn.len() - 1) as i16,
                        user_ptr,
                        user_len,
                        password_ptr,
                        password_len,
                    )
                };
                for buffer in [&mut user, &mut password].into_iter().flatten() {
                    buffer.fill(0);
                }
                Ok(connected)
            }
        }
    }
}

fn connection_argument(value: Option<&str>) -> Result<Option<Vec<u16>>, String> {
    value
        .map(|value| {
            let units: Vec<_> = value.encode_utf16().chain(std::iter::once(0)).collect();
            if value.contains('\0') || units.len() - 1 > i16::MAX as usize {
                Err("ODBC 运行时凭据的长度或内容无效。".into())
            } else {
                Ok(units)
            }
        })
        .transpose()
}

fn argument_parts(argument: &mut Option<Vec<u16>>) -> (*mut u16, i16) {
    argument.as_mut().map_or((null_mut(), 0), |units| {
        (units.as_mut_ptr(), (units.len() - 1) as i16)
    })
}

fn require_registered_dsn(
    environment: &Resource<'_>,
    dsn: &str,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    let dsn = dsn.to_lowercase();
    if visit_dsns(environment, cancel, |name| name.to_lowercase() == dsn)? {
        Ok(())
    } else {
        Err("找不到已注册的 ODBC 数据源，请检查 DSN 和驱动架构。".into())
    }
}

pub fn registered_data_sources() -> Result<Vec<String>, String> {
    let manager = Manager::load()?;
    let environment = environment(&manager)?;
    let mut names = Vec::new();
    visit_dsns(&environment, None, |name| {
        names.push(name);
        false
    })?;
    names.sort_unstable_by_key(|name| name.to_lowercase());
    names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    Ok(names)
}

fn visit_dsns(
    environment: &Resource<'_>,
    cancel: Option<&AtomicBool>,
    mut matched: impl FnMut(String) -> bool,
) -> Result<bool, String> {
    let mut direction = 2; // SQL_FETCH_FIRST. Includes user and system data sources.
    loop {
        check_import_cancelled(cancel)?;
        let mut name = [0u16; MAX_DSN_LENGTH + 1];
        let mut length = 0;
        let result = unsafe {
            (environment.manager.data_sources)(
                environment.handle,
                direction,
                name.as_mut_ptr(),
                name.len() as i16,
                &mut length,
                null_mut(),
                0,
                null_mut(),
            )
        };
        check_import_cancelled(cancel)?;
        if result == NO_DATA {
            return Ok(false);
        }
        environment.check(result, "无法读取已注册的 ODBC 数据源。")?;
        if let Some(units) = name.get(..length as usize)
            && String::from_utf16(units).is_ok_and(&mut matched)
        {
            return Ok(true);
        }
        direction = 1; // SQL_FETCH_NEXT.
    }
}

pub(crate) fn read_query(
    login: SourceLogin<'_>,
    sql: &str,
    source_table: Option<&SourceTable>,
    copy_foreign_keys: bool,
    cancel: Option<&AtomicBool>,
    columns_only: bool,
) -> Result<SourceData, String> {
    check_import_cancelled(cancel)?;
    let sql = validate_read_request(sql, source_table, copy_foreign_keys)?;
    let manager = Manager::load()?;
    let environment = environment(&manager)?;
    let mut source = connect_source(&environment, login, true, cancel)?;
    source.transaction =
        success(unsafe { (manager.set_connection)(source.handle, 102, null_mut(), 0) });
    read_on_source(
        &source,
        sql,
        source_table,
        copy_foreign_keys,
        cancel,
        columns_only,
    )
}

fn validate_read_request<'a>(
    sql: &'a str,
    source_table: Option<&SourceTable>,
    copy_foreign_keys: bool,
) -> Result<&'a str, String> {
    if let Some(table) = source_table {
        table.validate()?;
    } else if copy_foreign_keys {
        return Err("读取外键需要指定 ODBC 源表。".into());
    }
    checked_select(sql)
}

fn connect_source<'a>(
    environment: &Resource<'a>,
    login: SourceLogin<'_>,
    read_only: bool,
    cancel: Option<&AtomicBool>,
) -> Result<Resource<'a>, String> {
    let manager = environment.manager;
    let mut source = Resource::allocate(manager, DBC, environment.handle)?;
    // These attributes are advisory for some drivers.
    unsafe {
        (manager.set_connection)(source.handle, 101, usize::from(read_only) as Handle, 0);
        (manager.set_connection)(source.handle, 103, 10 as Handle, 0); // Login timeout.
    }
    check_import_cancelled(cancel)?;
    let connected = login.connect(environment, &source, cancel);
    source.connected = connected.as_ref().is_ok_and(|&result| success(result));
    check_import_cancelled(cancel)?;
    let connected = connected?;
    source.check(connected, "无法连接 ODBC 数据源。")?;
    Ok(source)
}

fn read_on_source(
    source: &Resource<'_>,
    sql: &str,
    source_table: Option<&SourceTable>,
    copy_foreign_keys: bool,
    cancel: Option<&AtomicBool>,
    columns_only: bool,
) -> Result<SourceData, String> {
    let mut sql: Vec<_> = sql.encode_utf16().collect();
    let length = i32::try_from(sql.len()).map_err(|_| "源查询过长。")?;
    sql.push(0);
    let manager = source.manager;
    let statement = Resource::allocate(manager, STMT, source.handle)?;
    unsafe {
        (manager.set_statement)(statement.handle, 0, 30 as Handle, 0); // Query timeout.
    }
    check_import_cancelled(cancel)?;
    let executed = unsafe { (manager.execute)(statement.handle, sql.as_ptr(), length) };
    check_import_cancelled(cancel)?;
    statement.check(executed, "ODBC 源查询执行失败。")?;
    let mut data = read_result(&statement, source_table.is_some(), cancel, columns_only)?;
    drop(statement);
    if let Some(table) = source_table {
        data.indexes =
            indexes::read_indexes(source, table, &data.columns, &mut data.source_table, cancel)?;
        if copy_foreign_keys {
            let resolved = data.source_table.clone();
            data.foreign_keys = foreign_keys::read_foreign_keys(
                source,
                resolved.as_ref().unwrap_or(table),
                &data.columns,
                &mut data.source_table,
                cancel,
            )?;
        }
    }
    Ok(data)
}

fn environment(manager: &Manager) -> Result<Resource<'_>, String> {
    let environment = Resource::allocate(manager, ENV, null_mut())?;
    // ODBC integer-valued attributes are encoded in the pointer value.
    let version = unsafe { (manager.set_env)(environment.handle, 200, 3 as Handle, 0) };
    environment.check(version, "无法初始化 ODBC 3 环境。")?;
    Ok(environment)
}

fn read_result(
    statement: &Resource<'_>,
    infer_indexable_columns: bool,
    cancel: Option<&AtomicBool>,
    columns_only: bool,
) -> Result<SourceData, String> {
    let manager = statement.manager;
    let mut count = 0;
    let result = unsafe { (manager.column_count)(statement.handle, &mut count) };
    statement.check(result, "无法读取 ODBC 结果列数。")?;
    if count <= 0 {
        return Err("源查询未返回数据列。".into());
    }
    let count = count as usize;
    if count > MAX_IMPORT_COLUMNS {
        return Err(format!("源查询最多可返回 {MAX_IMPORT_COLUMNS} 列。"));
    }

    let mut columns = Vec::with_capacity(count);
    let mut binary_columns = Vec::with_capacity(count);
    let mut remaining = IMPORT_MEMORY_LIMIT;
    for index in 1..=count as u16 {
        check_import_cancelled(cancel)?;
        let mut name = [0u16; 4096];
        let (mut length, mut data_type, mut column_size, mut decimal_digits, mut nullable) =
            (0, 0, 0, 0, 0);
        let result = unsafe {
            (manager.describe)(
                statement.handle,
                index,
                name.as_mut_ptr(),
                name.len() as i16,
                &mut length,
                &mut data_type,
                &mut column_size,
                &mut decimal_digits,
                &mut nullable,
            )
        };
        statement.check(result, "无法读取 ODBC 源列。")?;
        let length = usize::try_from(length)
            .ok()
            .filter(|&length| length < name.len())
            .ok_or("源查询返回的列名过长。")?;
        let name = String::from_utf16(&name[..length]).map_err(|_| "源列名包含无效的 Unicode。")?;
        binary_columns.push(matches!(
            data_type,
            SQL_BINARY | SQL_VARBINARY | SQL_LONGVARBINARY
        ));
        let native_type = if data_type == SQL_BINARY {
            result_type_name(statement, index)
        } else {
            None
        };
        let sql_type = if infer_indexable_columns
            && matches!(data_type, 1 | 12 | -8 | -9)
            && (1..=16383).contains(&column_size)
        {
            format!("VARCHAR({column_size})")
        } else {
            inferred_type(
                data_type,
                column_size,
                decimal_digits,
                native_type.as_deref(),
            )
        };
        take_memory(
            &mut remaining,
            name.len() + sql_type.len() + size_of::<SourceColumn>(),
        )?;
        columns.push(SourceColumn {
            name,
            definition: OdbcColumnDefinition {
                sql_type,
                not_null: nullable == 0,
                ..OdbcColumnDefinition::default()
            },
        });
    }

    let mut rows = Vec::new();
    if columns_only {
        return Ok(SourceData {
            columns,
            rows,
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            source_table: None,
        });
    }
    loop {
        check_import_cancelled(cancel)?;
        let result = unsafe { (manager.fetch)(statement.handle) };
        check_import_cancelled(cancel)?;
        if result == NO_DATA {
            break;
        }
        statement.check(result, "读取 ODBC 源数据时失败。")?;
        if rows.len() == MAX_IMPORT_ROWS {
            return Err(format!(
                "源查询返回超过 {MAX_IMPORT_ROWS} 行，请缩小查询范围。"
            ));
        }
        take_memory(
            &mut remaining,
            count * size_of::<Cell>() + size_of::<Vec<Cell>>(),
        )?;
        let mut row = Vec::with_capacity(count);
        for (index, binary) in binary_columns.iter().copied().enumerate() {
            let column = index as u16 + 1;
            let cell = if binary {
                read_binary_cell(statement, column, &mut remaining, cancel)?
            } else {
                read_cell(statement, column, &mut remaining, cancel)?.map_or(Cell::Null, Cell::Text)
            };
            row.push(cell);
        }
        rows.push(row);
    }
    let more = unsafe { (manager.more_results)(statement.handle) };
    check_import_cancelled(cancel)?;
    if more != NO_DATA {
        statement.check(more, "无法读取 ODBC 查询的结束状态。")?;
        return Err("请将源查询限制为一个结果集。".into());
    }
    Ok(SourceData {
        columns,
        rows,
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
        source_table: None,
    })
}

const SQL_BINARY: i16 = -2;
const SQL_VARBINARY: i16 = -3;
const SQL_LONGVARBINARY: i16 = -4;
const SQL_NULL_DATA: isize = -1;
const SQL_NO_TOTAL: isize = -4;

fn result_type_name(statement: &Resource<'_>, index: u16) -> Option<String> {
    const SQL_DESC_TYPE_NAME: u16 = 14;
    let attribute = statement.manager.column_attribute?;
    let mut name = [u16::MAX; 128];
    let mut written = 0;
    let status = unsafe {
        attribute(
            statement.handle,
            index,
            SQL_DESC_TYPE_NAME,
            name.as_mut_ptr().cast(),
            size_of_val(&name) as i16,
            &mut written,
            null_mut(),
        )
    };
    if !success(status) {
        return None;
    }
    let end = name.iter().position(|&unit| unit == 0)?;
    String::from_utf16(&name[..end]).ok()
}

fn inferred_type(data_type: i16, size: usize, digits: i16, native_type: Option<&str>) -> String {
    // SQLDescribeCol does not distinguish signed and unsigned integers. The
    // destination has enough range for both forms of each source type.
    match data_type {
        -7 => "TINYINT".into(),
        -6 => "SMALLINT".into(),
        5 => "MEDIUMINT".into(),
        4 => "BIGINT".into(),
        -5 => "DECIMAL(20, 0)".into(),
        2 | 3
            if (1..=65).contains(&size)
                && (0..=30).contains(&digits)
                && digits as usize <= size =>
        {
            format!("DECIMAL({size}, {digits})")
        }
        6..=8 => "DOUBLE".into(),
        9 | 91 => "DATE".into(),
        10 | 92 if (0..=6).contains(&digits) => format!("TIME({digits})"),
        11 | 93 if (0..=6).contains(&digits) => format!("DATETIME({digits})"),
        // Some drivers describe a variable BLOB as SQL_BINARY with a placeholder
        // size. The native declaration takes precedence over that fixed length.
        SQL_BINARY if native_type.is_some_and(|name| name.eq_ignore_ascii_case("blob")) => {
            "LONGBLOB".into()
        }
        SQL_BINARY if (1..=255).contains(&size) => format!("BINARY({size})"),
        SQL_VARBINARY if (1..=16383).contains(&size) => format!("VARBINARY({size})"),
        SQL_BINARY | SQL_VARBINARY | SQL_LONGVARBINARY => match size {
            1..=255 => "TINYBLOB".into(),
            256..=65535 => "BLOB".into(),
            65536..=16777215 => "MEDIUMBLOB".into(),
            _ => "LONGBLOB".into(),
        },
        _ => "LONGTEXT".into(),
    }
}

fn read_cell(
    statement: &Resource<'_>,
    column: u16,
    remaining: &mut usize,
    cancel: Option<&AtomicBool>,
) -> Result<Option<String>, String> {
    const WIDE_TEXT: i16 = -8;

    let mut units = Vec::new();
    loop {
        check_import_cancelled(cancel)?;
        let mut chunk = [u16::MAX; 2048];
        let mut bytes = 0;
        let result = unsafe {
            (statement.manager.get_data)(
                statement.handle,
                column,
                WIDE_TEXT,
                chunk.as_mut_ptr().cast(),
                size_of_val(&chunk) as isize,
                &mut bytes,
            )
        };
        check_import_cancelled(cancel)?;
        statement.check(result, "无法读取 ODBC 字段。")?;
        if bytes == SQL_NULL_DATA && units.is_empty() {
            return Ok(None);
        }
        let capacity = chunk.len() - 1;
        let truncated = field_truncated(statement, result, bytes, capacity * size_of::<u16>())?;
        let copied = if truncated {
            // A driver can leave one unit unused to avoid separating a surrogate pair.
            let terminator = chunk.iter().rposition(|&unit| unit != u16::MAX);
            terminator
                .filter(|&index| index > 0 && chunk[index] == 0)
                .ok_or("ODBC 驱动返回了未正确结束的字段。")?
        } else {
            usize::try_from(bytes)
                .ok()
                .filter(|&bytes| bytes.is_multiple_of(2) && bytes / 2 <= capacity)
                .ok_or("ODBC 驱动返回了无效的字段长度。")?
                / 2
        };
        if units.len() + copied > *remaining {
            return Err("源数据超过 32 MiB，请缩小查询范围。".into());
        }
        units.extend_from_slice(&chunk[..copied]);
        if !truncated {
            break;
        }
    }
    let value = String::from_utf16(&units).map_err(|_| "源数据包含无效的 Unicode。")?;
    take_memory(remaining, value.len())?;
    Ok(Some(value))
}

fn read_binary_cell(
    statement: &Resource<'_>,
    column: u16,
    remaining: &mut usize,
    cancel: Option<&AtomicBool>,
) -> Result<Cell, String> {
    const SQL_C_BINARY: i16 = SQL_BINARY;
    let mut value = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        check_import_cancelled(cancel)?;
        let mut bytes = 0;
        let result = unsafe {
            (statement.manager.get_data)(
                statement.handle,
                column,
                SQL_C_BINARY,
                chunk.as_mut_ptr().cast(),
                chunk.len() as isize,
                &mut bytes,
            )
        };
        check_import_cancelled(cancel)?;
        statement.check(result, "无法读取 ODBC 二进制字段。")?;
        if bytes == SQL_NULL_DATA && value.is_empty() && result == SUCCESS {
            return Ok(Cell::Null);
        }
        let truncated = field_truncated(statement, result, bytes, chunk.len())?;
        let copied = if truncated {
            // SQL_C_BINARY fills the entire buffer when more data remains.
            chunk.len()
        } else {
            usize::try_from(bytes)
                .ok()
                .filter(|&bytes| bytes <= chunk.len() && (bytes > 0 || value.is_empty()))
                .ok_or("ODBC 驱动返回了无效的二进制字段长度。")?
        };
        take_memory(remaining, copied)?;
        value.extend_from_slice(&chunk[..copied]);
        if !truncated {
            return Ok(Cell::Binary(value));
        }
    }
}

fn field_truncated(
    statement: &Resource<'_>,
    result: Return,
    bytes: isize,
    capacity: usize,
) -> Result<bool, String> {
    if result != SUCCESS_WITH_INFO {
        return Ok(false);
    }
    let diagnostic = statement.diagnostic();
    if (bytes == SQL_NO_TOTAL || bytes > capacity as isize)
        && diagnostic
            .as_ref()
            .is_some_and(|(state, _)| state == "01004")
    {
        return Ok(true);
    }
    let code = diagnostic.map_or_else(String::new, |(state, native)| {
        format!("（SQLSTATE {state}，驱动代码 {native}）")
    });
    Err(format!("ODBC 字段转换返回警告。{code}"))
}

fn take_memory(remaining: &mut usize, bytes: usize) -> Result<(), String> {
    *remaining = remaining
        .checked_sub(bytes)
        .ok_or("源数据超过 32 MiB，请缩小查询范围。")?;
    Ok(())
}

fn connection_text(input: &str) -> Result<Vec<u16>, String> {
    let input = input.trim();
    if input.is_empty() || input.contains('\0') {
        return Err("请输入有效的 DSN 或 ODBC 连接字符串。".into());
    }
    if input.contains('=') {
        reject_save_file(input)?;
        Ok(input.encode_utf16().collect())
    } else {
        Ok(format!("DSN={{{}}};", input.replace('}', "}}"))
            .encode_utf16()
            .collect())
    }
}

fn reject_save_file(mut attributes: &str) -> Result<(), String> {
    // The Driver Manager can persist a connection if SAVEFILE is present.
    loop {
        attributes =
            attributes.trim_start_matches(|ch: char| ch == ';' || ch.is_ascii_whitespace());
        let Some(separator) = attributes.find(['=', ';']) else {
            return Ok(());
        };
        let value = &attributes[separator + 1..];
        if attributes.as_bytes()[separator] == b';' {
            attributes = value;
            continue;
        }
        let key = &attributes[..separator];
        if key.trim().eq_ignore_ascii_case("SAVEFILE") {
            return Err("ODBC 导入只使用临时连接，连接字符串不能含有 SAVEFILE。".into());
        }
        let value = value.trim_start();
        let tail = if value.starts_with('{') {
            let width =
                quoted_width(value.as_bytes(), b'}').ok_or("ODBC 连接字符串中的大括号未闭合。")?;
            &value[width..]
        } else {
            value
        };
        let Some((_, next)) = tail.split_once(';') else {
            return Ok(());
        };
        attributes = next;
    }
}

pub(crate) fn checked_select(sql: &str) -> Result<&str, String> {
    const SELECT_ONLY: &str = "请填写一条返回数据的 SELECT 查询。";
    const WRITES: &[&str] = &[
        "INTO", "INSERT", "UPDATE", "DELETE", "MERGE", "EXEC", "EXECUTE", "CALL", "CREATE",
        "ALTER", "DROP", "TRUNCATE", "GRANT", "REVOKE",
    ];
    let sql = sql.trim().trim_start_matches('\u{feff}').trim();
    if sql.contains('\0') {
        return Err(SELECT_ONLY.into());
    }
    let bytes = sql.as_bytes();
    let mut cursor = 0;
    let mut started = false;
    let mut select = false;
    let mut delimiter = None;
    while let Some(&byte) = bytes.get(cursor) {
        let tail = &bytes[cursor..];
        match byte {
            byte if byte.is_ascii_whitespace() => cursor += 1,
            b'-' if tail.starts_with(b"--") && tail.get(2).is_none_or(u8::is_ascii_whitespace) => {
                cursor += tail
                    .iter()
                    .position(|&byte| matches!(byte, b'\r' | b'\n'))
                    .unwrap_or(tail.len());
            }
            b'/' if tail.starts_with(b"/*") => {
                if tail.starts_with(b"/*!") || tail.starts_with(b"/*M!") {
                    return Err(SELECT_ONLY.into());
                }
                cursor += tail[2..]
                    .windows(2)
                    .position(|pair| pair == b"*/")
                    .ok_or(SELECT_ONLY)?
                    + 4;
            }
            _ if delimiter.is_some() => return Err(SELECT_ONLY.into()),
            b';' if select => {
                delimiter = Some(cursor);
                cursor += 1;
            }
            b'\'' | b'"' | b'`' | b'[' if started => {
                cursor += quoted_width(tail, if byte == b'[' { b']' } else { byte })
                    .ok_or(SELECT_ONLY)?;
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                let width = tail
                    .iter()
                    .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_')
                    .count();
                let token = &sql[cursor..cursor + width];
                cursor += width;
                let is_select = token.eq_ignore_ascii_case("SELECT");
                if !started && !is_select && !token.eq_ignore_ascii_case("WITH")
                    || WRITES.iter().any(|word| token.eq_ignore_ascii_case(word))
                {
                    return Err(SELECT_ONLY.into());
                }
                started = true;
                select |= is_select;
            }
            _ if started => cursor += 1,
            _ => return Err(SELECT_ONLY.into()),
        }
    }
    select
        .then(|| sql[..delimiter.unwrap_or(sql.len())].trim_end())
        .ok_or_else(|| SELECT_ONLY.into())
}

fn quoted_width(bytes: &[u8], end: u8) -> Option<usize> {
    let mut cursor = 1;
    while let Some(&byte) = bytes.get(cursor) {
        cursor += 1;
        if byte == end {
            if bytes.get(cursor) != Some(&end) {
                return Some(cursor);
            }
            cursor += 1;
        }
    }
    None
}

type Handle = *mut c_void;
type Return = i16;
type ColumnAttribute =
    unsafe extern "system" fn(Handle, u16, u16, Handle, i16, *mut i16, *mut isize) -> Return;
const ENV: i16 = 1;
const DBC: i16 = 2;
const STMT: i16 = 3;
const SUCCESS: Return = 0;
const SUCCESS_WITH_INFO: Return = 1;
const NO_DATA: Return = 100;

fn success(result: Return) -> bool {
    matches!(result, SUCCESS | SUCCESS_WITH_INFO)
}

// Only the platform manager is loaded dynamically. These are ODBC 3 wide UTF-16 entry points.
struct Manager {
    _library: Library,
    allocate: unsafe extern "system" fn(i16, Handle, *mut Handle) -> Return,
    free: unsafe extern "system" fn(i16, Handle) -> Return,
    set_env: unsafe extern "system" fn(Handle, i32, Handle, i32) -> Return,
    set_connection: unsafe extern "system" fn(Handle, i32, Handle, i32) -> Return,
    set_statement: unsafe extern "system" fn(Handle, i32, Handle, i32) -> Return,
    connect:
        unsafe extern "system" fn(Handle, *mut u16, i16, *mut u16, i16, *mut u16, i16) -> Return,
    data_sources: unsafe extern "system" fn(
        Handle,
        u16,
        *mut u16,
        i16,
        *mut i16,
        *mut u16,
        i16,
        *mut i16,
    ) -> Return,
    driver_connect: unsafe extern "system" fn(
        Handle,
        Handle,
        *mut u16,
        i16,
        *mut u16,
        i16,
        *mut i16,
        u16,
    ) -> Return,
    end_transaction: unsafe extern "system" fn(i16, Handle, i16) -> Return,
    disconnect: unsafe extern "system" fn(Handle) -> Return,
    execute: unsafe extern "system" fn(Handle, *const u16, i32) -> Return,
    column_count: unsafe extern "system" fn(Handle, *mut i16) -> Return,
    describe: unsafe extern "system" fn(
        Handle,
        u16,
        *mut u16,
        i16,
        *mut i16,
        *mut i16,
        *mut usize,
        *mut i16,
        *mut i16,
    ) -> Return,
    column_attribute: Option<ColumnAttribute>,
    fetch: unsafe extern "system" fn(Handle) -> Return,
    row_count: Option<unsafe extern "system" fn(Handle, *mut isize) -> Return>,
    get_data: unsafe extern "system" fn(Handle, u16, i16, Handle, isize, *mut isize) -> Return,
    more_results: unsafe extern "system" fn(Handle) -> Return,
    statistics: Option<indexes::Statistics>,
    foreign_keys: Option<foreign_keys::ForeignKeys>,
    tables: Option<tables::Tables>,
    info: Option<unsafe extern "system" fn(Handle, u16, Handle, i16, *mut i16) -> Return>,
    diagnostic: unsafe extern "system" fn(
        i16,
        Handle,
        i16,
        *mut u16,
        *mut i32,
        *mut u16,
        i16,
        *mut i16,
    ) -> Return,
}

impl Manager {
    fn load() -> Result<Self, String> {
        let library = driver_manager()?;
        // The fixed symbol names below match the ODBC Driver Manager ABI. The owned library
        // outlives all copied pointers and any handles allocated through them.
        unsafe fn symbol<T: Copy>(library: &Library, name: &[u8]) -> Result<T, String> {
            unsafe { library.get::<T>(name) }
                .map(|value| *value)
                .map_err(|_| "当前 ODBC 驱动管理器缺少所需的 Unicode 接口。".into())
        }
        unsafe {
            Ok(Self {
                allocate: symbol(&library, b"SQLAllocHandle")?,
                free: symbol(&library, b"SQLFreeHandle")?,
                set_env: symbol(&library, b"SQLSetEnvAttr")?,
                set_connection: symbol(&library, b"SQLSetConnectAttrW")?,
                set_statement: symbol(&library, b"SQLSetStmtAttrW")?,
                connect: symbol(&library, b"SQLConnectW")?,
                data_sources: symbol(&library, b"SQLDataSourcesW")?,
                driver_connect: symbol(&library, b"SQLDriverConnectW")?,
                end_transaction: symbol(&library, b"SQLEndTran")?,
                disconnect: symbol(&library, b"SQLDisconnect")?,
                execute: symbol(&library, b"SQLExecDirectW")?,
                column_count: symbol(&library, b"SQLNumResultCols")?,
                describe: symbol(&library, b"SQLDescribeColW")?,
                column_attribute: symbol(&library, b"SQLColAttributeW").ok(),
                fetch: symbol(&library, b"SQLFetch")?,
                row_count: symbol(&library, b"SQLRowCount").ok(),
                get_data: symbol(&library, b"SQLGetData")?,
                more_results: symbol(&library, b"SQLMoreResults")?,
                statistics: symbol(&library, b"SQLStatisticsW").ok(),
                foreign_keys: symbol(&library, b"SQLForeignKeysW").ok(),
                tables: symbol(&library, b"SQLTablesW").ok(),
                info: symbol(&library, b"SQLGetInfoW").ok(),
                diagnostic: symbol(&library, b"SQLGetDiagRecW")?,
                _library: library,
            })
        }
    }
}

#[cfg(windows)]
fn driver_manager() -> Result<Library, String> {
    use libloading::os::windows::{LOAD_LIBRARY_SEARCH_SYSTEM32, Library as WindowsLibrary};

    // Restrict the DLL search to the operating system directory.
    unsafe { WindowsLibrary::load_with_flags("odbc32.dll", LOAD_LIBRARY_SEARCH_SYSTEM32) }
        .map(Into::into)
        .map_err(|_| "无法加载 Windows ODBC 驱动管理器。".into())
}

#[cfg(not(windows))]
fn driver_manager() -> Result<Library, String> {
    #[cfg(target_os = "macos")]
    const MANAGERS: &[&str] = &[
        "/opt/homebrew/opt/unixodbc/lib/libodbc.dylib",
        "/usr/local/opt/unixodbc/lib/libodbc.dylib",
        "/opt/local/lib/libodbc.dylib",
    ];
    #[cfg(not(target_os = "macos"))]
    const MANAGERS: &[&str] = &["libodbc.so.2", "libodbc.so"];
    // These are the OS search names or known platform package locations for unixODBC.
    MANAGERS
        .iter()
        .find_map(|name| unsafe { Library::new(name).ok() })
        .ok_or_else(|| {
            "未找到 unixODBC 驱动管理器。请确认系统已配置对应架构的 unixODBC 和数据源驱动。".into()
        })
}

struct Resource<'a> {
    manager: &'a Manager,
    handle: Handle,
    kind: i16,
    connected: bool,
    transaction: bool,
}

impl<'a> Resource<'a> {
    fn allocate(manager: &'a Manager, kind: i16, parent: Handle) -> Result<Self, String> {
        let mut handle = null_mut();
        // The parent handle is either the appropriate still-live resource or null for an ENV.
        if !success(unsafe { (manager.allocate)(kind, parent, &mut handle) }) || handle.is_null() {
            return Err("无法分配 ODBC 资源。".into());
        }
        Ok(Self {
            manager,
            handle,
            kind,
            connected: false,
            transaction: false,
        })
    }

    fn check(&self, result: Return, context: &str) -> Result<(), String> {
        if success(result) {
            return Ok(());
        }
        let Some((state, native)) = self.diagnostic() else {
            return Err(context.into());
        };
        let detail = match state.as_str() {
            "IM002" | "IM003" => "找不到指定的数据源或驱动。",
            "28000" => "数据源拒绝了登录。",
            "HYT00" | "HYT01" => "数据源响应超时。",
            _ => "",
        };
        Err(format!(
            "{context}{detail}（SQLSTATE {state}，驱动代码 {native}）"
        ))
    }

    fn diagnostic(&self) -> Option<(String, i32)> {
        let mut state = [0u16; 6];
        let mut native = 0;
        // Fetch only the code and state. Driver messages may contain connection parameters.
        let result = unsafe {
            (self.manager.diagnostic)(
                self.kind,
                self.handle,
                1,
                state.as_mut_ptr(),
                &mut native,
                null_mut(),
                0,
                null_mut(),
            )
        };
        if !success(result)
            || !state[..5]
                .iter()
                .all(|&unit| unit <= 127 && (unit as u8).is_ascii_alphanumeric())
        {
            return None;
        }
        Some((String::from_utf16(&state[..5]).ok()?, native))
    }
}

impl Drop for Resource<'_> {
    fn drop(&mut self) {
        // Resources are declared in parent-before-child order and therefore freed in reverse.
        unsafe {
            if self.connected {
                if self.transaction {
                    (self.manager.end_transaction)(DBC, self.handle, 1); // Rollback.
                }
                (self.manager.disconnect)(self.handle);
            }
            (self.manager.free)(self.kind, self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_blob_declaration_avoids_fixed_length_padding() {
        assert_eq!(inferred_type(SQL_BINARY, 255, 0, Some("BLOB")), "LONGBLOB");
        assert_eq!(inferred_type(SQL_BINARY, 255, 0, Some("blob")), "LONGBLOB");
        assert_eq!(inferred_type(SQL_BINARY, 5, 0, Some("BINARY")), "BINARY(5)");
        assert_eq!(inferred_type(SQL_BINARY, 5, 0, None), "BINARY(5)");
    }

    #[test]
    fn preserves_unicode_connection_and_select_syntax() {
        assert_eq!(
            String::from_utf16(&connection_text("测试数据源").unwrap()).unwrap(),
            "DSN={测试数据源};"
        );
        let sql = "WITH test AS (SELECT N'数据😀;行' AS [来源]) SELECT [来源] FROM test";
        assert_eq!(checked_select(&format!("{sql}; -- done")).unwrap(), sql);
        assert!(checked_select("SELECT 1 INTO target").is_err());
        assert!(checked_select("SELECT 1; DELETE FROM target").is_err());
        assert!(connection_text("DRIVER=Test;PWD={分号;仍是密码}; SAVEFILE=test.dsn").is_err());
        assert!(connection_text("DRIVER=Test;PWD={包含;SAVEFILE=字样}").is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn loads_windows_driver_manager_and_returns_redacted_diagnostics() {
        let result = read(
            "DRIVER={Harbor SQL nonexistent test driver};UID=token-placeholder".into(),
            "SELECT 1",
        );
        let error = result.err().expect("the fixture driver is not installed");
        assert!(
            error.contains("SQLSTATE IM002") || error.contains("SQLSTATE IM003"),
            "{error}"
        );
        assert!(!error.contains("token-placeholder"));
    }
}
