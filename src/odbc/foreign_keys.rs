use std::collections::{BTreeMap, HashSet};
use std::ptr::null_mut;
use std::sync::atomic::AtomicBool;

use crate::database::OdbcReferentialAction;
use crate::sql_tools::mysql_text_literal;

use super::indexes::valid_name;
use super::{
    Handle, MAX_IMPORT_COLUMNS, NO_DATA, Resource, Return, STMT, SUCCESS, SUCCESS_WITH_INFO,
    SourceColumn, SourceTable, argument_parts, check_import_cancelled, connection_argument,
    read_cell,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceForeignKey {
    pub name: Option<String>,
    pub columns: Vec<String>,
    pub referenced: SourceTable,
    pub referenced_columns: Vec<String>,
    pub on_update: OdbcReferentialAction,
    pub on_delete: OdbcReferentialAction,
}

pub(super) type ForeignKeys = unsafe extern "system" fn(
    Handle,
    *mut u16,
    i16,
    *mut u16,
    i16,
    *mut u16,
    i16,
    *mut u16,
    i16,
    *mut u16,
    i16,
    *mut u16,
    i16,
) -> Return;

#[derive(Eq, Ord, PartialEq, PartialOrd)]
enum ConstraintId {
    Named(String),
    Unnamed {
        referenced: SourceTable,
        referenced_key: Option<String>,
        update_rule: u8,
        delete_rule: u8,
    },
}

struct KeyColumns {
    definition: SourceForeignKey,
    referenced_key: Option<String>,
    fields: BTreeMap<u16, (String, String)>,
}

pub(super) fn read_foreign_keys(
    source: &Resource<'_>,
    table: &SourceTable,
    columns: &[SourceColumn],
    identity: &mut Option<SourceTable>,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<SourceForeignKey>, String> {
    let reported = read_driver_catalog(source, table, columns, identity, cancel)?;
    if !is_mysql_family(source, cancel)? {
        return Ok(reported);
    }

    // SQLForeignKeys is permitted to omit constraints referencing a UNIQUE
    // key. The MySQL-family server catalog includes those relationships.
    let scoped = identity.clone();
    let scoped = scoped.as_ref().unwrap_or(table);
    let authoritative = read_mysql_catalog(source, scoped, columns, identity, cancel)?;
    for key in &reported {
        check_import_cancelled(cancel)?;
        if authoritative
            .iter()
            .filter(|server_key| matches_server_key(key, server_key))
            .count()
            != 1
        {
            let name = key.name.as_deref().unwrap_or("未命名外键");
            return Err(format!("ODBC 驱动报告的外键 {name} 与源数据库记录不一致。"));
        }
    }
    Ok(authoritative)
}

fn read_driver_catalog(
    source: &Resource<'_>,
    table: &SourceTable,
    columns: &[SourceColumn],
    identity: &mut Option<SourceTable>,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<SourceForeignKey>, String> {
    let manager = source.manager;
    let foreign_keys = manager
        .foreign_keys
        .ok_or("当前 ODBC 驱动管理器没有提供外键读取接口。")?;
    let statement = Resource::allocate(manager, STMT, source.handle)?;
    let mut catalog = connection_argument(table.catalog.as_deref())?;
    let mut schema = connection_argument(table.schema.as_deref())?;
    let mut name = connection_argument(Some(&table.table))?;
    let (catalog_ptr, catalog_len) = argument_parts(&mut catalog);
    let (schema_ptr, schema_len) = argument_parts(&mut schema);
    let (table_ptr, table_len) = argument_parts(&mut name);
    check_import_cancelled(cancel)?;
    let result = unsafe {
        (manager.set_statement)(statement.handle, 0, 30 as Handle, 0); // SQL_ATTR_QUERY_TIMEOUT
        foreign_keys(
            statement.handle,
            null_mut(),
            0,
            null_mut(),
            0,
            null_mut(),
            0,
            catalog_ptr,
            catalog_len,
            schema_ptr,
            schema_len,
            table_ptr,
            table_len,
        )
    };
    check_import_cancelled(cancel)?;
    statement.check(result, "无法读取 ODBC 源表的外键。")?;
    if result == SUCCESS_WITH_INFO {
        return Err("ODBC 源表外键元数据返回了警告。".into());
    }
    read_key_rows(&statement, table, columns, identity, cancel)
}

fn read_key_rows(
    statement: &Resource<'_>,
    table: &SourceTable,
    columns: &[SourceColumn],
    identity: &mut Option<SourceTable>,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<SourceForeignKey>, String> {
    let manager = statement.manager;
    let mut count = 0;
    let result = unsafe { (manager.column_count)(statement.handle, &mut count) };
    statement.check(result, "无法读取 ODBC 外键元数据列数。")?;
    if count < 14 {
        return Err("ODBC 驱动返回的外键元数据不包含完整定义。".into());
    }
    let projected: HashSet<_> = columns
        .iter()
        .map(|field| field.name.to_lowercase())
        .collect();
    let mut remaining = 256 * 1024;
    let mut keys = BTreeMap::<ConstraintId, KeyColumns>::new();
    loop {
        check_import_cancelled(cancel)?;
        let fetched = unsafe { (manager.fetch)(statement.handle) };
        check_import_cancelled(cancel)?;
        if fetched == NO_DATA {
            break;
        }
        statement.check(fetched, "无法读取 ODBC 源表的外键信息。")?;
        if fetched == SUCCESS_WITH_INFO {
            return Err("ODBC 驱动读取外键定义时返回了警告。".into());
        }

        // SQLGetData on catalog results may require accessing columns in order.
        let mut field = |column| read_cell(statement, column, &mut remaining, cancel);
        let referenced = SourceTable::from_driver(field(1)?, field(2)?, field(3)?)?;
        let referenced_column = column_name(field(4)?)?;
        let actual = SourceTable::from_driver(field(5)?, field(6)?, field(7)?)?;
        table.record_identity(identity, actual)?;
        let column = column_name(field(8)?)?;
        let sequence = field(9)?
            .and_then(|value| value.parse::<u16>().ok())
            .filter(|&value| value > 0 && usize::from(value) <= MAX_IMPORT_COLUMNS)
            .ok_or("ODBC 驱动返回了无效的外键列顺序。")?;
        let (update_rule, on_update) = action(field(10)?)?;
        let (delete_rule, on_delete) = action(field(11)?)?;
        let name = optional_key_name(field(12)?)?;
        let referenced_key = optional_key_name(field(13)?)?;
        match field(14)?.as_deref() {
            Some("7") => {} // SQL_NOT_DEFERRABLE
            Some("5" | "6") => {
                return Err("源表包含可延迟检查的外键，MySQL 和 MariaDB 无法保留该规则。".into());
            }
            _ => return Err("ODBC 驱动未提供可识别的外键检查时机。".into()),
        }
        let label = name.as_deref().unwrap_or("未命名外键");
        if !projected.contains(&column.to_lowercase()) {
            return Err(format!(
                "ODBC 源外键 {label} 的列 {column} 未出现在查询结果中。"
            ));
        }
        let id = if let Some(name) = &name {
            ConstraintId::Named(name.clone())
        } else {
            ConstraintId::Unnamed {
                referenced: referenced.clone(),
                referenced_key: referenced_key.clone(),
                update_rule,
                delete_rule,
            }
        };
        let key = keys.entry(id).or_insert_with(|| KeyColumns {
            definition: SourceForeignKey {
                name: name.clone(),
                columns: Vec::new(),
                referenced: referenced.clone(),
                referenced_columns: Vec::new(),
                on_update,
                on_delete,
            },
            referenced_key: referenced_key.clone(),
            fields: BTreeMap::new(),
        });
        if key.definition.referenced != referenced
            || key.referenced_key != referenced_key
            || key.definition.on_update != on_update
            || key.definition.on_delete != on_delete
            || key
                .fields
                .insert(sequence, (column, referenced_column))
                .is_some()
        {
            return Err(format!("无法区分 ODBC 源外键 {label} 的定义。"));
        }
    }

    keys.into_values().map(ordered_key).collect()
}

fn is_mysql_family(source: &Resource<'_>, cancel: Option<&AtomicBool>) -> Result<bool, String> {
    let Some(info) = source.manager.info else {
        return Ok(false);
    };
    let mut name = [0u16; 128];
    let mut length = 0;
    check_import_cancelled(cancel)?;
    let result = unsafe {
        info(
            source.handle,
            17, // SQL_DBMS_NAME
            name.as_mut_ptr().cast(),
            size_of_val(&name) as i16,
            &mut length,
        )
    };
    check_import_cancelled(cancel)?;
    let length = usize::try_from(length).unwrap_or(usize::MAX);
    if result != SUCCESS || !length.is_multiple_of(2) || length / 2 >= name.len() {
        return Ok(false);
    }
    let Ok(name) = String::from_utf16(&name[..length / 2]) else {
        return Ok(false);
    };
    Ok(name.eq_ignore_ascii_case("MariaDB") || name.eq_ignore_ascii_case("MySQL"))
}

fn read_mysql_catalog(
    source: &Resource<'_>,
    table: &SourceTable,
    columns: &[SourceColumn],
    identity: &mut Option<SourceTable>,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<SourceForeignKey>, String> {
    let manager = source.manager;
    let statement = Resource::allocate(manager, STMT, source.handle)?;
    let database = table
        .schema
        .as_deref()
        .or(table.catalog.as_deref())
        .map_or_else(|| "DATABASE()".to_owned(), mysql_text_literal);
    let name = mysql_text_literal(&table.table);
    // MySQL-family drivers can expose databases as catalogs or as schemas.
    // Mirror whichever convention the driver reported for the selected table.
    let namespace = |database_column: &str| {
        if table.schema.is_some() {
            let catalog = table
                .catalog
                .as_deref()
                .map_or_else(|| "NULL".to_owned(), mysql_text_literal);
            format!("{catalog}, {database_column}")
        } else {
            format!("{database_column}, NULL")
        }
    };
    let reference_scope = namespace("k.REFERENCED_TABLE_SCHEMA");
    let local_scope = namespace("k.TABLE_SCHEMA");
    let identifier = |column: &str, value: &str| {
        format!(
            "CAST(IF(@@lower_case_table_names = 0, {column}, LOWER({column})) AS BINARY)
             = CAST(IF(@@lower_case_table_names = 0, {value}, LOWER({value})) AS BINARY)"
        )
    };
    let database_matches = identifier("k.TABLE_SCHEMA", &database);
    let table_matches = identifier("k.TABLE_NAME", &name);
    let rule = |column: &str| {
        format!(
            "CASE {column} WHEN 'CASCADE' THEN 0 WHEN 'RESTRICT' THEN 1 WHEN 'SET NULL' THEN 2
               WHEN 'NO ACTION' THEN 3 WHEN 'SET DEFAULT' THEN 4 END"
        )
    };
    let update = rule("r.UPDATE_RULE");
    let delete = rule("r.DELETE_RULE");
    let sql = format!(
        "SELECT {reference_scope}, k.REFERENCED_TABLE_NAME, k.REFERENCED_COLUMN_NAME,
                {local_scope}, k.TABLE_NAME, k.COLUMN_NAME, k.ORDINAL_POSITION,
                {update}, {delete}, k.CONSTRAINT_NAME, r.UNIQUE_CONSTRAINT_NAME,
                CASE WHEN t.ENGINE = 'NDBCLUSTER'
                     AND (r.UPDATE_RULE = 'NO ACTION' OR r.DELETE_RULE = 'NO ACTION')
                     THEN 5 ELSE 7 END
         FROM information_schema.KEY_COLUMN_USAGE k
         JOIN information_schema.TABLES t
           ON CAST(t.TABLE_SCHEMA AS BINARY) = CAST(k.TABLE_SCHEMA AS BINARY)
          AND CAST(t.TABLE_NAME AS BINARY) = CAST(k.TABLE_NAME AS BINARY)
         LEFT JOIN information_schema.REFERENTIAL_CONSTRAINTS r
           ON CAST(r.CONSTRAINT_CATALOG AS BINARY) = CAST(k.CONSTRAINT_CATALOG AS BINARY)
          AND CAST(r.CONSTRAINT_SCHEMA AS BINARY) = CAST(k.CONSTRAINT_SCHEMA AS BINARY)
          AND CAST(r.TABLE_NAME AS BINARY) = CAST(k.TABLE_NAME AS BINARY)
          AND CAST(r.CONSTRAINT_NAME AS BINARY) = CAST(k.CONSTRAINT_NAME AS BINARY)
         WHERE {database_matches} AND {table_matches} AND k.REFERENCED_TABLE_NAME IS NOT NULL"
    );
    let mut sql: Vec<_> = sql.encode_utf16().collect();
    let length = sql.len() as i32;
    sql.push(0);
    check_import_cancelled(cancel)?;
    let result = unsafe {
        (manager.set_statement)(statement.handle, 0, 30 as Handle, 0);
        (manager.execute)(statement.handle, sql.as_ptr(), length)
    };
    check_import_cancelled(cancel)?;
    statement.check(result, "无法核对 MySQL 或 MariaDB 源表的外键。")?;
    if result == SUCCESS_WITH_INFO {
        return Err("源数据库核对外键时返回了警告。".into());
    }
    // If the driver suppresses all namespace values, the catalog query may
    // identify the table's default database. Existing reported scope is exact.
    let mut resolved = identity
        .as_ref()
        .filter(|table| table.catalog.is_some() || table.schema.is_some())
        .cloned();
    let keys = read_key_rows(&statement, table, columns, &mut resolved, cancel)?;
    if resolved.is_some() {
        *identity = resolved;
    }
    Ok(keys)
}

fn matches_server_key(reported: &SourceForeignKey, actual: &SourceForeignKey) -> bool {
    use OdbcReferentialAction::{NoAction, Restrict};
    let same_rule = |left, right| {
        left == right || matches!((left, right), (NoAction, Restrict) | (Restrict, NoAction))
    };
    let same_scope = |reported: Option<&str>, actual: Option<&str>| {
        reported.is_none_or(|reported| actual == Some(reported))
    };
    same_scope(reported.name.as_deref(), actual.name.as_deref())
        && reported.columns == actual.columns
        && reported.referenced_columns == actual.referenced_columns
        && reported.referenced.table == actual.referenced.table
        && same_scope(
            reported.referenced.catalog.as_deref(),
            actual.referenced.catalog.as_deref(),
        )
        && same_scope(
            reported.referenced.schema.as_deref(),
            actual.referenced.schema.as_deref(),
        )
        && same_rule(reported.on_update, actual.on_update)
        && same_rule(reported.on_delete, actual.on_delete)
}

fn ordered_key(mut key: KeyColumns) -> Result<SourceForeignKey, String> {
    let mut local_names = HashSet::new();
    let mut referenced_names = HashSet::new();
    for (index, (sequence, (column, referenced))) in key.fields.into_iter().enumerate() {
        if usize::from(sequence) != index + 1
            || !local_names.insert(column.to_lowercase())
            || !referenced_names.insert(referenced.to_lowercase())
        {
            let label = key.definition.name.as_deref().unwrap_or("未命名外键");
            return Err(format!("ODBC 源外键 {label} 的列定义不完整或重复。"));
        }
        key.definition.columns.push(column);
        key.definition.referenced_columns.push(referenced);
    }
    Ok(key.definition)
}

fn column_name(value: Option<String>) -> Result<String, String> {
    value
        .filter(|name| valid_name(name))
        .ok_or_else(|| "ODBC 驱动返回的外键列名无效。".into())
}

fn optional_key_name(value: Option<String>) -> Result<Option<String>, String> {
    value
        .filter(|name| !name.is_empty())
        .map(|name| {
            if valid_name(&name) {
                Ok(name)
            } else {
                Err("ODBC 驱动返回的外键或引用键名称无效。".into())
            }
        })
        .transpose()
}

fn action(value: Option<String>) -> Result<(u8, OdbcReferentialAction), String> {
    use OdbcReferentialAction::{Cascade, NoAction, Restrict, SetDefault, SetNull};
    match value.as_deref() {
        Some("0") => Ok((0, Cascade)),
        Some("1") => Ok((1, Restrict)),
        Some("2") => Ok((2, SetNull)),
        Some("3") => Ok((3, NoAction)),
        Some("4") => Ok((4, SetDefault)),
        _ => Err("ODBC 驱动未提供可识别的外键更新或删除规则。".into()),
    }
}
