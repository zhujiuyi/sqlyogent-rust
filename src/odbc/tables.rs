use std::collections::BTreeSet;
use std::ptr::null_mut;

use zeroize::Zeroizing;

use super::{
    Cell, Handle, IMPORT_MEMORY_LIMIT, Manager, NO_DATA, Resource, Return, STMT, SUCCESS,
    SourceLogin, SourceTable, argument_parts, connect_source, connection_argument, environment,
    read_cell, read_on_source, take_memory,
};

const MAX_TABLE_ROWS: usize = 16_384;
const SQL_DBMS_NAME: u16 = 17;
const SQL_IDENTIFIER_QUOTE_CHAR: u16 = 29;
const SQL_CATALOG_NAME_SEPARATOR: u16 = 41;
const SQL_SCHEMA_USAGE: u16 = 91;
const SQL_CATALOG_USAGE: u16 = 92;
const SQL_CATALOG_LOCATION: u16 = 114;
const SQL_DML_STATEMENTS: u32 = 1;
const SQL_CL_START: u16 = 1;
const SQL_CL_END: u16 = 2;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredTable {
    pub source: SourceTable,
    pub select_sql: String,
}

pub(super) type Tables = unsafe extern "system" fn(
    Handle,
    *mut u16,
    i16,
    *mut u16,
    i16,
    *mut u16,
    i16,
    *mut u16,
    i16,
) -> Return;

pub fn discover_tables(connection: String) -> Result<Vec<DiscoveredTable>, String> {
    let connection = Zeroizing::new(connection);
    discover_tables_for_login(SourceLogin::ConnectionString(&connection))
}

pub(crate) fn discover_tables_for_login(
    login: SourceLogin<'_>,
) -> Result<Vec<DiscoveredTable>, String> {
    let manager = Manager::load()?;
    let environment = environment(&manager)?;
    let source = connect_source(&environment, login, true, None)?;
    let dbms = text_info(&source, SQL_DBMS_NAME)?.to_ascii_lowercase();
    let mut remaining = IMPORT_MEMORY_LIMIT;
    let tables = table_names(&source, &dbms, &mut remaining)?;
    if tables.is_empty() {
        return Ok(Vec::new());
    }
    let naming = Naming::read(&source, &dbms, &tables)?;
    tables
        .into_iter()
        .map(|source| {
            let qualified = naming.qualify(&source)?;
            let select_sql = format!("SELECT * FROM {qualified}");
            take_memory(
                &mut remaining,
                select_sql.len() + size_of::<DiscoveredTable>(),
            )?;
            Ok(DiscoveredTable { source, select_sql })
        })
        .collect()
}

fn table_names(
    source: &Resource<'_>,
    dbms: &str,
    remaining: &mut usize,
) -> Result<BTreeSet<SourceTable>, String> {
    const READ_ERROR: &str = "无法读取 ODBC 源表清单。";
    let manager = source.manager;
    let tables = manager
        .tables
        .ok_or("当前 ODBC 驱动管理器没有提供 Unicode 表清单接口。")?;
    // MySQL-family drivers scope a null catalog to the selected database and
    // can return an empty list when no database is selected. Explicit patterns
    // request table rows across catalogs only in that latter case.
    let unscoped_mysql = if is_mysql(dbms) {
        // Some drivers report the string "null" for SQL_DATABASE_NAME when no
        // catalog is selected. DATABASE() distinguishes that from a real name.
        let context = read_on_source(source, "SELECT DATABASE()", None, false, None, false)?;
        match context.rows.as_slice() {
            [row] if row.as_slice() == [Cell::Null] => true,
            [row] if row.len() == 1 => false,
            _ => return Err("无法确认 ODBC 源连接的默认数据库。".into()),
        }
    } else {
        false
    };
    let statement = Resource::allocate(manager, STMT, source.handle)?;
    let mut catalog = connection_argument(unscoped_mysql.then_some("%"))?;
    let mut name = connection_argument(unscoped_mysql.then_some("%"))?;
    let mut kind = connection_argument(Some("TABLE"))?;
    let (catalog_ptr, catalog_len) = argument_parts(&mut catalog);
    let (name_ptr, name_len) = argument_parts(&mut name);
    let (kind_ptr, kind_len) = argument_parts(&mut kind);
    let result = unsafe {
        (manager.set_statement)(statement.handle, 0, 30 as Handle, 0); // SQL_ATTR_QUERY_TIMEOUT
        tables(
            statement.handle,
            catalog_ptr,
            catalog_len,
            null_mut(),
            0,
            name_ptr,
            name_len,
            kind_ptr,
            kind_len,
        )
    };
    require_complete_status(&statement, result, READ_ERROR)?;
    let mut columns = 0;
    let result = unsafe { (manager.column_count)(statement.handle, &mut columns) };
    require_complete_status(&statement, result, READ_ERROR)?;
    if columns < 4 {
        return Err("ODBC 驱动返回的表清单缺少必要列。".into());
    }

    let mut names = BTreeSet::new();
    let mut scanned = 0;
    loop {
        let result = unsafe { (manager.fetch)(statement.handle) };
        if result == NO_DATA {
            return Ok(names);
        }
        require_complete_status(&statement, result, READ_ERROR)?;
        scanned += 1;
        if scanned > MAX_TABLE_ROWS {
            return Err(format!(
                "ODBC 源表清单超过 {MAX_TABLE_ROWS} 条，请缩小连接可见范围，或使用自定义查询。"
            ));
        }
        // SQLGetData must remain in column order on drivers that do not support random access.
        let mut field = |column| read_cell(&statement, column, remaining, None);
        let catalog = field(1)?;
        let schema = field(2)?;
        let name = field(3)?;
        let kind = field(4)?;
        if kind
            .as_deref()
            .is_some_and(|kind| kind.trim_ascii().eq_ignore_ascii_case("TABLE"))
            && !is_system_namespace(dbms, catalog.as_deref(), schema.as_deref())
        {
            names.insert(SourceTable::from_driver(catalog, schema, name)?);
        }
    }
}

fn is_mysql(dbms: &str) -> bool {
    matches!(dbms, "mysql" | "mariadb")
}

fn is_system_namespace(dbms: &str, catalog: Option<&str>, schema: Option<&str>) -> bool {
    if is_mysql(dbms) {
        return [catalog, schema].into_iter().flatten().any(|name| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "information_schema" | "performance_schema" | "mysql" | "sys"
            )
        });
    }
    schema.is_some_and(|schema| {
        let schema = schema.to_ascii_lowercase();
        match dbms {
            "postgresql" => schema.starts_with("pg_") || schema == "information_schema",
            "microsoft sql server" => matches!(schema.as_str(), "sys" | "information_schema"),
            _ => false,
        }
    })
}

struct Naming {
    quotes: Option<(char, char)>,
    mysql: bool,
    catalog: Option<Catalog>,
    schema: bool,
}

struct Catalog {
    separator: String,
    location: u16,
}

impl Naming {
    fn read(
        source: &Resource<'_>,
        dbms: &str,
        tables: &BTreeSet<SourceTable>,
    ) -> Result<Self, String> {
        let quotes = match text_info(source, SQL_IDENTIFIER_QUOTE_CHAR)?.as_str() {
            "`" => Some(('`', '`')),
            "\"" => Some(('"', '"')),
            "[" | "[]" => Some(('[', ']')),
            " " | "" => None,
            _ => return Err("ODBC 驱动使用的标识符引号无法识别，请使用自定义查询。".into()),
        };
        let mysql = is_mysql(dbms);
        let catalog = if !mysql
            && tables.iter().any(|table| table.catalog.is_some())
            && dml_qualification(source, SQL_CATALOG_USAGE)?
        {
            let separator = text_info(source, SQL_CATALOG_NAME_SEPARATOR)?;
            let location = catalog_location(source)?;
            if !matches!(separator.as_str(), "." | ":" | "::" | "@")
                || !matches!(location, SQL_CL_START | SQL_CL_END)
            {
                return Err("ODBC 驱动使用的目录限定符无法识别，请使用自定义查询。".into());
            }
            Some(Catalog {
                separator,
                location,
            })
        } else {
            None
        };
        let schema = !mysql
            && tables.iter().any(|table| table.schema.is_some())
            && dml_qualification(source, SQL_SCHEMA_USAGE)?;
        if !mysql {
            // A file driver can expose its single implicit namespace even
            // when the SQL dialect does not accept it as a qualifier.
            if catalog.is_none()
                && tables
                    .iter()
                    .map(|table| table.catalog.as_deref())
                    .collect::<BTreeSet<_>>()
                    .len()
                    > 1
            {
                return Err(
                    "源表清单包含多个目录，但驱动无法在查询中限定目录。请缩小连接可见范围。".into(),
                );
            }
            if !schema
                && tables
                    .iter()
                    .map(|table| table.schema.as_deref())
                    .collect::<BTreeSet<_>>()
                    .len()
                    > 1
            {
                return Err(
                    "源表清单包含多个架构，但驱动无法在查询中限定架构。请缩小连接可见范围。".into(),
                );
            }
        }
        Ok(Self {
            quotes,
            mysql,
            catalog,
            schema,
        })
    }

    fn delimit(&self, value: &str) -> Result<String, String> {
        if let Some((open, close)) = self.quotes {
            return Ok(format!(
                "{open}{}{close}",
                value.replace(close, &format!("{close}{close}"))
            ));
        }
        let mut bytes = value.bytes();
        if bytes
            .next()
            .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
            && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
        {
            Ok(value.to_owned())
        } else {
            Err("ODBC 驱动无法引用这张表的名称，请使用自定义查询。".into())
        }
    }

    fn qualify(&self, table: &SourceTable) -> Result<String, String> {
        let name = self.delimit(&table.table)?;
        if self.mysql {
            // MySQL and MariaDB have one database qualifier. Drivers expose
            // it either as the catalog or as the schema in SQLTables.
            return table
                .schema
                .as_deref()
                .or(table.catalog.as_deref())
                .map(|database| {
                    self.delimit(database)
                        .map(|database| format!("{database}.{name}"))
                })
                .unwrap_or(Ok(name));
        }
        let name = if let Some(schema) = table.schema.as_deref().filter(|_| self.schema) {
            format!("{}.{name}", self.delimit(schema)?)
        } else {
            name
        };
        if let (Some(catalog), Some(value)) = (&self.catalog, &table.catalog) {
            let value = self.delimit(value)?;
            let separator = &catalog.separator;
            if catalog.location == SQL_CL_START {
                Ok(format!("{value}{separator}{name}"))
            } else {
                Ok(format!("{name}{separator}{value}"))
            }
        } else {
            Ok(name)
        }
    }
}

fn text_info(source: &Resource<'_>, kind: u16) -> Result<String, String> {
    let mut buffer = [0u16; 256];
    let mut length = 0;
    driver_info(
        source,
        kind,
        buffer.as_mut_ptr().cast(),
        size_of_val(&buffer) as i16,
        &mut length,
    )?;
    let length = usize::try_from(length)
        .ok()
        .filter(|&bytes| bytes.is_multiple_of(2) && bytes < size_of_val(&buffer))
        .ok_or("ODBC 驱动返回的名称规则不完整。")?;
    String::from_utf16(&buffer[..length / 2])
        .map_err(|_| "ODBC 驱动返回的名称规则包含无效字符。".into())
}

fn catalog_location(source: &Resource<'_>) -> Result<u16, String> {
    let mut location = 0_u16;
    driver_info(
        source,
        SQL_CATALOG_LOCATION,
        (&raw mut location).cast(),
        size_of::<u16>() as i16,
        null_mut(),
    )?;
    Ok(location)
}

fn dml_qualification(source: &Resource<'_>, kind: u16) -> Result<bool, String> {
    let mut usage = 0_u32;
    driver_info(
        source,
        kind,
        (&raw mut usage).cast(),
        size_of::<u32>() as i16,
        null_mut(),
    )?;
    Ok(usage & SQL_DML_STATEMENTS != 0)
}

fn driver_info(
    source: &Resource<'_>,
    kind: u16,
    buffer: Handle,
    capacity: i16,
    length: *mut i16,
) -> Result<(), String> {
    let info = source
        .manager
        .info
        .ok_or("当前 ODBC 驱动管理器无法读取表命名规则。")?;
    let result = unsafe { info(source.handle, kind, buffer, capacity, length) };
    require_complete_status(source, result, "无法读取 ODBC 驱动的表命名规则。")
}

fn require_complete_status(
    resource: &Resource<'_>,
    result: Return,
    error: &str,
) -> Result<(), String> {
    resource.check(result, error)?;
    if result != SUCCESS {
        return Err(format!("{error} 驱动返回了警告，无法确认完整信息。"));
    }
    Ok(())
}
