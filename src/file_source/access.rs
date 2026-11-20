use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use chrono::{Datelike, NaiveDate, TimeDelta};
use jetdb::catalog::CatalogEntry;
use jetdb::data::{TableRow, TableRows, Value, calculated_column_types_in};
use jetdb::format::{ColumnType, ObjectType, index_flags, index_type};
use jetdb::relationship::relationship_flags;
use jetdb::table::{ColumnDef, IndexColumnOrder, TableDef};
use jetdb::{FileError, PageReader, read_catalog, read_table_def};

use crate::database::{
    Cell, IMPORT_MEMORY_LIMIT, MAX_IMPORT_COLUMNS, MAX_IMPORT_ROWS, OdbcColumnDefinition,
    OdbcReferentialAction, check_import_cancelled, import_row_size,
};
use crate::odbc::{SourceColumn, SourceData, SourceForeignKey, SourceIndex, SourceTable};

use super::FileSource;

mod jet_password;
mod linked;

const MAX_LOCAL_TABLES: usize = 16_384;
const CASCADE_NULL_ON_DELETE: u32 = 0x0000_2000;
const PASSWORD_REQUIRED: &str =
    "此 Access 文件需要密码。请在导入窗口输入密码，或为作业设置密码环境变量。";

pub(super) fn validate(path: &Path) -> Result<(), String> {
    let supported = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("mdb") || ext.eq_ignore_ascii_case("accdb"));
    if supported {
        Ok(())
    } else {
        Err("请选择 .mdb 或 .accdb 格式的 Access 数据库。".into())
    }
}

struct Access {
    reader: PageReader,
    catalog: Vec<CatalogEntry>,
}

impl Access {
    fn open(
        path: &Path,
        password: Option<&str>,
        cancel: Option<&AtomicBool>,
    ) -> Result<Self, String> {
        check_import_cancelled(cancel)?;
        if !std::fs::metadata(path).map_err(access_error)?.is_file() {
            return Err("请选择普通 Access 数据库文件。".into());
        }
        let input = File::open(path).map_err(access_error)?;
        let mut reader =
            PageReader::open_reader_with_password(input, password).map_err(open_error)?;
        // JetDB authenticates ACE files itself. Verify the MDB password before reading its catalog.
        jet_password::verify(&mut reader, password)?;
        let catalog = read_catalog(&mut reader).map_err(access_error)?;
        check_import_cancelled(cancel)?;
        Ok(Self { reader, catalog })
    }
}

pub(super) fn discover(source: &FileSource, password: Option<&str>) -> Result<Vec<String>, String> {
    let mut access = Access::open(&source.path, password, None)?;
    let mut names = access
        .catalog
        .iter()
        .filter(|entry| local_table(entry))
        .map(|entry| entry.name.clone())
        .collect::<Vec<_>>();
    if let Some(folder) = source.access_link_folder.as_deref() {
        linked::open_folder(folder)?;
        names.extend(
            linked::catalog(&mut access, None)?
                .into_iter()
                .map(|link| link.alias),
        );
    }
    if names.len() > MAX_LOCAL_TABLES {
        return Err(format!("Access 数据库的本地表超过 {MAX_LOCAL_TABLES} 张。"));
    }
    names.sort();
    Ok(names)
}

fn local_table(entry: &CatalogEntry) -> bool {
    entry.object_type == ObjectType::Table && user_table(entry)
}

fn user_table(entry: &CatalogEntry) -> bool {
    !entry.is_system_or_hidden()
        && !entry.name.starts_with('~')
        && !entry.name.to_ascii_lowercase().starts_with("msys")
}

pub(super) fn read(
    source: &FileSource,
    name: &str,
    columns_only: bool,
    copy_indexes: bool,
    copy_foreign_keys: bool,
    password: Option<&str>,
    cancel: Option<&AtomicBool>,
) -> Result<SourceData, String> {
    let mut front = Access::open(&source.path, password, cancel)?;
    if front
        .catalog
        .iter()
        .any(|entry| local_table(entry) && entry.name == name)
    {
        return read_base(
            &mut front,
            name,
            columns_only,
            copy_indexes,
            copy_foreign_keys,
            cancel,
        );
    }
    let Some(folder) = source.access_link_folder.as_deref() else {
        return Err(format!("Access 数据库中找不到本地表「{name}」。"));
    };
    let links = linked::catalog(&mut front, cancel)?;
    let link = links
        .iter()
        .find(|link| link.alias == name)
        .ok_or_else(|| format!("Access 数据库中找不到可导入的本地表或文件链接表「{name}」。"))?;
    let path = linked::resolve(folder, link, cancel)?;
    let mut backend = Access::open(&path, None, cancel).map_err(|error| {
        if error == PASSWORD_REQUIRED {
            format!("Access 链接表「{name}」的后端文件需要单独的密码，当前无法读取该链接。")
        } else if error == crate::database::IMPORT_CANCELLED {
            error
        } else {
            format!("无法打开 Access 链接表「{name}」的后端。{error}")
        }
    })?;
    let table = link.table.to_lowercase();
    let Some(entry) = backend
        .catalog
        .iter()
        .find(|entry| local_table(entry) && entry.name.to_lowercase() == table)
    else {
        if backend.catalog.iter().any(|entry| {
            entry.object_type == ObjectType::LinkedTable && entry.name.to_lowercase() == table
        }) {
            return Err(format!(
                "Access 链接表「{name}」指向另一张链接表，需要直接指向本地后端数据表。"
            ));
        }
        return Err(format!(
            "Access 链接表「{name}」的后端缺少本地数据表「{}」。",
            link.table
        ));
    };
    let physical = entry.name.clone();
    let mut data = read_base(
        &mut backend,
        &physical,
        columns_only,
        copy_indexes,
        copy_foreign_keys,
        cancel,
    )?;
    for key in &mut data.foreign_keys {
        check_import_cancelled(cancel)?;
        let alias = linked::parent_alias(
            &links,
            link,
            &key.referenced.table,
            key.name.as_deref().unwrap_or("未命名"),
        )?;
        key.referenced = source_table(alias);
    }
    data.source_table = Some(source_table(name));
    Ok(data)
}

fn read_base(
    access: &mut Access,
    name: &str,
    columns_only: bool,
    copy_indexes: bool,
    copy_foreign_keys: bool,
    cancel: Option<&AtomicBool>,
) -> Result<SourceData, String> {
    let entry = access
        .catalog
        .iter()
        .find(|entry| local_table(entry) && entry.name == name)
        .ok_or_else(|| format!("Access 数据库中找不到本地表「{name}」。"))?;
    let table =
        read_table_def(&mut access.reader, &entry.name, entry.table_page).map_err(access_error)?;
    let calculated = calculated_column_types_in(&access.catalog, &table);
    let positions = visible_positions(&table);
    if !(1..=MAX_IMPORT_COLUMNS).contains(&positions.len()) {
        return Err(format!("Access 表「{name}」没有可读取的列或超过列数上限。"));
    }
    let mut columns = positions
        .iter()
        .map(|&index| {
            let col = &table.columns[index];
            let kind = if col.is_calculated {
                calculated
                    .get(&col.name.to_ascii_lowercase())
                    .copied()
                    .ok_or_else(|| format!("无法确认 Access 计算列「{}」的结果类型。", col.name))?
            } else {
                col.col_type
            };
            column_definition(&table, col, kind).map(|definition| SourceColumn {
                name: col.name.clone(),
                definition,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let indexes = if copy_indexes {
        read_indexes(&table, &mut columns)?
    } else {
        Vec::new()
    };
    let foreign_keys = if copy_foreign_keys {
        read_foreign_keys(access, &table, &columns, cancel)?
    } else {
        Vec::new()
    };
    let rows = if columns_only {
        Vec::new()
    } else {
        read_rows(
            &mut access.reader,
            &table,
            &calculated,
            &positions,
            &columns,
            cancel,
        )?
    };
    Ok(SourceData {
        columns,
        rows,
        indexes,
        foreign_keys,
        source_table: Some(source_table(&table.name)),
    })
}

fn visible_positions(table: &TableDef) -> Vec<usize> {
    let mut positions = (0..table.columns.len())
        .filter(|&index| !table.columns[index].is_hidden())
        .collect::<Vec<_>>();
    positions.sort_by_key(|&index| {
        let column = &table.columns[index];
        (column.display_index, column.col_num)
    });
    positions
}

fn column_definition(
    table: &TableDef,
    column: &ColumnDef,
    kind: ColumnType,
) -> Result<OdbcColumnDefinition, String> {
    let sql_type = match kind {
        ColumnType::Boolean | ColumnType::Byte => "TINYINT".into(),
        ColumnType::Int => "SMALLINT".into(),
        ColumnType::Long => "INT".into(),
        ColumnType::BigInt => "BIGINT".into(),
        ColumnType::Float => "FLOAT".into(),
        ColumnType::Double => "DOUBLE".into(),
        ColumnType::Money => "DECIMAL(19, 4)".into(),
        ColumnType::Numeric => {
            let precision = if column.precision == 0 {
                28
            } else {
                column.precision
            };
            if precision > 65 || column.scale > precision || column.scale > 30 {
                return Err(format!(
                    "Access 列「{}」的小数精度或位数无效。",
                    column.name
                ));
            }
            format!("DECIMAL({precision}, {})", column.scale)
        }
        ColumnType::Timestamp => "DATETIME(6)".into(),
        // The Access extended type records seven fractional digits.
        ColumnType::DateTimeExtended | ColumnType::Memo => "LONGTEXT".into(),
        ColumnType::Text => {
            let length = table.shown_size(column);
            if (1..=16383).contains(&length) {
                format!("VARCHAR({length})")
            } else {
                "LONGTEXT".into()
            }
        }
        ColumnType::Guid => "CHAR(38)".into(),
        ColumnType::Binary if column.col_size > 0 => format!("VARBINARY({})", column.col_size),
        ColumnType::Binary | ColumnType::Ole => "LONGBLOB".into(),
        ColumnType::ComplexType => {
            return Err(format!(
                "Access 列「{}」包含附件、多值或其他复合数据，无法按普通列导入。",
                column.name
            ));
        }
        _ => {
            return Err(format!(
                "Access 列「{}」使用了未识别的字段类型。",
                column.name
            ));
        }
    };
    Ok(OdbcColumnDefinition {
        sql_type,
        unsigned: kind == ColumnType::Byte,
        ..OdbcColumnDefinition::default()
    })
}

fn read_rows(
    reader: &mut PageReader,
    table: &TableDef,
    calculated: &HashMap<String, ColumnType>,
    positions: &[usize],
    columns: &[SourceColumn],
    cancel: Option<&AtomicBool>,
) -> Result<Vec<Vec<Cell>>, String> {
    let mut scanner = TableRows::new(table, calculated).with_columns(positions);
    let mut rows = Vec::new();
    let mut remaining = IMPORT_MEMORY_LIMIT;
    loop {
        check_import_cancelled(cancel)?;
        let number = rows.len() + 1;
        let Some(row) = next_row(reader, &mut scanner, &table.name, number)? else {
            return Ok(rows);
        };
        if rows.len() == MAX_IMPORT_ROWS {
            return Err(format!("每张 Access 源表最多读取 {MAX_IMPORT_ROWS} 行。"));
        }
        if row.values.len() != table.columns.len() {
            return Err(format!(
                "Access 表「{}」第 {number} 行的列数不一致。",
                table.name
            ));
        }
        let mut values = row.values;
        let cells = positions
            .iter()
            .zip(columns)
            .map(|(&index, column)| {
                let value = std::mem::replace(&mut values[index], Value::Null);
                let error = |reason: &str| {
                    format!(
                        "Access 表「{}」第 {number} 行「{}」列无法读取。{reason}",
                        table.name, column.name
                    )
                };
                let cell = cell_value(value).map_err(error)?;
                if cell == Cell::Null && column.definition.not_null {
                    return Err(error("索引要求此列不能为空。"));
                }
                Ok(cell)
            })
            .collect::<Result<Vec<_>, String>>()?;
        remaining = remaining
            .checked_sub(import_row_size(&cells))
            .ok_or("Access 导入数据超过 32 MiB。")?;
        rows.push(cells);
    }
}

fn next_row(
    reader: &mut PageReader,
    scanner: &mut TableRows<'_>,
    table: &str,
    number: usize,
) -> Result<Option<TableRow>, String> {
    let context = |detail| format!("无法读取 Access 表「{table}」第 {number} 行。{detail}");
    let available = scanner
        .peek(reader)
        .map_err(|error| context(error.to_string()))?;
    if scanner.skipped_rows() != 0 {
        return Err(context("发现无法解析的记录。".into()));
    }
    if available.is_none() {
        return Ok(None);
    }
    scanner
        .next_row(reader)
        .map_err(|error| context(error.to_string()))?
        .map(Some)
        .ok_or_else(|| context("记录结构损坏。".into()))
}

fn cell_value(value: Value) -> Result<Cell, &'static str> {
    Ok(match value {
        Value::Null => Cell::Null,
        Value::Bool(value) => Cell::Text(if value { "1" } else { "0" }.into()),
        Value::Byte(value) => Cell::Text(value.to_string()),
        Value::Int(value) => Cell::Text(value.to_string()),
        Value::Long(value) => Cell::Text(value.to_string()),
        Value::BigInt(value) => Cell::Text(value.to_string()),
        Value::Float(value) if value.is_finite() => Cell::Text(value.to_string()),
        Value::Double(value) if value.is_finite() => Cell::Text(value.to_string()),
        Value::Float(_) | Value::Double(_) => return Err("浮点值不有限。"),
        Value::Binary(value) => Cell::Binary(value),
        Value::Text(value)
        | Value::Money(value)
        | Value::Numeric(value)
        | Value::Guid(value)
        | Value::DateTimeExtended(value) => Cell::Text(value),
        Value::Timestamp(value) => Cell::Text(timestamp(value)?),
        _ => return Err("字段值类型暂不支持。"),
    })
}

fn timestamp(value: f64) -> Result<String, &'static str> {
    const INVALID: &str = "日期超出目标 DATETIME 的有效范围。";
    if !value.is_finite() || value.abs() > 4_000_000.0 {
        return Err(INVALID);
    }
    let epoch = NaiveDate::from_ymd_opt(1899, 12, 30)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let days = TimeDelta::days(value.trunc() as i64);
    // OLE automation dates use a positive fraction within a negative day.
    let micros = (value.fract().abs() * 86_400_000_000.0).round() as i64;
    let result = epoch
        .checked_add_signed(days)
        .and_then(|datetime| datetime.checked_add_signed(TimeDelta::microseconds(micros)))
        .filter(|datetime| (1000..=9999).contains(&datetime.year()))
        .ok_or(INVALID)?;
    Ok(result.format("%Y-%m-%d %H:%M:%S%.6f").to_string())
}

fn read_indexes(
    table: &TableDef,
    columns: &mut [SourceColumn],
) -> Result<Vec<SourceIndex>, String> {
    let mut used = HashSet::new();
    let mut indexes = Vec::new();
    for index in &table.indexes {
        // These records point to relationship indexes and do not define key columns.
        if index.foreign_key.is_some() {
            continue;
        }
        let primary = index.index_type == index_type::PRIMARY;
        let name = if primary {
            "PRIMARY".to_owned()
        } else {
            index.name.clone()
        };
        if (!primary && index.index_type != index_type::ORDINARY)
            || (!primary && name.eq_ignore_ascii_case("PRIMARY"))
            || name.trim().is_empty()
            || name.contains('\0')
            || name.chars().count() > 64
            || !used.insert(name.to_lowercase())
            || index.columns.is_empty()
        {
            return Err(format!(
                "Access 索引「{}」的名称或定义无法复制。",
                index.name
            ));
        }
        let mut key_columns = Vec::new();
        let mut seen = HashSet::new();
        for member in &index.columns {
            let column = table
                .columns
                .iter()
                .find(|column| column.col_num == member.col_num)
                .and_then(|field| columns.iter_mut().find(|column| column.name == field.name))
                .ok_or_else(|| format!("Access 索引「{name}」引用了未读取的列。"))?;
            if !seen.insert(column.name.clone()) {
                return Err(format!("Access 索引「{name}」重复引用了同一列。"));
            }
            if primary || index.flags & index_flags::REQUIRED != 0 {
                column.definition.not_null = true;
            }
            key_columns.push((
                column.name.clone(),
                member.order == IndexColumnOrder::Descending,
            ));
        }
        indexes.push(SourceIndex {
            name,
            unique: primary || index.flags & index_flags::UNIQUE != 0,
            columns: key_columns,
        });
    }
    Ok(indexes)
}

fn source_table(name: &str) -> SourceTable {
    SourceTable {
        table: name.to_owned(),
        catalog: None,
        schema: None,
    }
}

struct Relationship {
    parent: String,
    count: usize,
    flags: u32,
    columns: BTreeMap<usize, (String, String)>,
}

fn read_foreign_keys(
    access: &mut Access,
    table: &TableDef,
    columns: &[SourceColumn],
    cancel: Option<&AtomicBool>,
) -> Result<Vec<SourceForeignKey>, String> {
    let Some(metadata) = access.catalog.iter().find(|entry| {
        entry.object_type == ObjectType::Table
            && entry.name.eq_ignore_ascii_case("MSysRelationships")
    }) else {
        return Ok(Vec::new());
    };
    let metadata = read_table_def(&mut access.reader, &metadata.name, metadata.table_page)
        .map_err(access_error)?;
    let names = [
        "szObject",
        "szRelationship",
        "szReferencedObject",
        "szColumn",
        "szReferencedColumn",
        "grbit",
        "icolumn",
        "ccolumn",
    ];
    let positions = names.map(|name| {
        metadata
            .columns
            .iter()
            .position(|column| column.name.eq_ignore_ascii_case(name))
    });
    let [
        Some(child),
        Some(name),
        Some(parent),
        Some(from),
        Some(to),
        Some(flags),
        Some(order),
        Some(count),
    ] = positions
    else {
        return Err("Access 外键系统表缺少表名、列名、规则或列顺序。".into());
    };
    let positions = [child, name, parent, from, to, flags, order, count];
    let calculated = HashMap::new();
    let mut scanner = TableRows::new(&metadata, &calculated).with_columns(&positions);
    let mut relationships = BTreeMap::<String, Relationship>::new();
    for number in 1..=MAX_IMPORT_ROWS + 1 {
        check_import_cancelled(cancel)?;
        let Some(row) = next_row(&mut access.reader, &mut scanner, &metadata.name, number)? else {
            break;
        };
        if number > MAX_IMPORT_ROWS {
            return Err("Access 外键系统表超过记录数上限。".into());
        }
        let values = row.values;
        let text = |index| match values.get(index) {
            Some(Value::Text(text)) if !text.trim().is_empty() && !text.contains('\0') => {
                Ok(text.as_str())
            }
            _ => Err("Access 外键系统表的表名、列名或关系名称无效。".to_owned()),
        };
        let child_name = text(child)?;
        if child_name.to_lowercase() != table.name.to_lowercase() {
            continue;
        }
        let number = |index| match values.get(index) {
            Some(Value::Long(value)) => Ok(*value),
            Some(Value::Int(value)) => Ok(i32::from(*value)),
            Some(Value::Byte(value)) => Ok(i32::from(*value)),
            _ => Err("Access 外键系统表的规则或列顺序无效。".to_owned()),
        };
        let bits = number(flags)? as u32;
        if bits & relationship_flags::NO_REFERENTIAL_INTEGRITY != 0 {
            continue;
        }
        let relation_name = text(name)?;
        let referenced = text(parent)?;
        let local = text(from)?;
        let remote = text(to)?;
        let count = usize::try_from(number(count)?).unwrap_or(0);
        let order = usize::try_from(number(order)?).unwrap_or(usize::MAX);
        if !(1..=jetdb::format::MAX_INDEX_COLUMNS).contains(&count) || order >= count {
            return Err(format!(
                "Access 外键「{relation_name}」的列数量或顺序无效。"
            ));
        }
        let relationship = relationships
            .entry(relation_name.to_owned())
            .or_insert_with(|| Relationship {
                parent: referenced.to_owned(),
                flags: bits,
                count,
                columns: BTreeMap::new(),
            });
        if relationship.parent != referenced
            || relationship.count != count
            || relationship.flags != bits
            || relationship
                .columns
                .insert(order, (local.to_owned(), remote.to_owned()))
                .is_some()
        {
            return Err(format!("Access 外键「{relation_name}」的多列定义不一致。"));
        }
    }
    let mut parents = HashMap::<String, (String, Vec<String>)>::new();
    relationships
        .into_iter()
        .map(|(name, relationship)| {
            check_import_cancelled(cancel)?;
            if relationship.columns.len() != relationship.count {
                return Err(format!("Access 外键「{name}」缺少部分外键列。"));
            }
            let lookup = relationship.parent.to_lowercase();
            if !parents.contains_key(&lookup) {
                let entry = access
                    .catalog
                    .iter()
                    .find(|entry| local_table(entry) && entry.name.to_lowercase() == lookup)
                    .ok_or_else(|| {
                        format!(
                            "Access 外键「{name}」引用了不可导入的表「{}」。",
                            relationship.parent
                        )
                    })?;
                let parent = read_table_def(&mut access.reader, &entry.name, entry.table_page)
                    .map_err(access_error)?;
                let parent_columns = parent
                    .columns
                    .into_iter()
                    .filter(|column| !column.is_hidden())
                    .map(|column| column.name)
                    .collect();
                parents.insert(lookup.clone(), (entry.name.clone(), parent_columns));
            }
            let (parent, parent_columns) = &parents[&lookup];
            let mut local = Vec::new();
            let mut remote = Vec::new();
            for (from, to) in relationship.columns.into_values() {
                let from = from.to_lowercase();
                let to = to.to_lowercase();
                let from = columns
                    .iter()
                    .find(|column| column.name.to_lowercase() == from)
                    .ok_or_else(|| format!("Access 外键「{name}」引用了未读取的子表列。"))?;
                let to = parent_columns
                    .iter()
                    .find(|column| column.to_lowercase() == to)
                    .ok_or_else(|| format!("Access 外键「{name}」引用了未读取的父表列。"))?;
                if local.contains(&from.name) || remote.contains(to) {
                    return Err(format!("Access 外键「{name}」包含重复列。"));
                }
                local.push(from.name.clone());
                remote.push(to.clone());
            }
            let (on_update, on_delete) = actions(&name, relationship.flags)?;
            Ok(SourceForeignKey {
                name: Some(name),
                columns: local,
                referenced: source_table(parent),
                referenced_columns: remote,
                on_update,
                on_delete,
            })
        })
        .collect()
}

fn actions(
    name: &str,
    flags: u32,
) -> Result<(OdbcReferentialAction, OdbcReferentialAction), String> {
    use OdbcReferentialAction::{Cascade, NoAction, SetNull};
    use relationship_flags::{CASCADE_DELETE, CASCADE_UPDATE};
    // DAO's unique, inherited and outer-join display attributes are not actions.
    const KNOWN: u32 = 0x0300_0005 | CASCADE_UPDATE | CASCADE_DELETE | CASCADE_NULL_ON_DELETE;
    if flags & !KNOWN != 0
        || flags & (CASCADE_DELETE | CASCADE_NULL_ON_DELETE)
            == (CASCADE_DELETE | CASCADE_NULL_ON_DELETE)
    {
        return Err(format!("Access 外键「{name}」的更新或删除规则暂不支持。"));
    }
    let update = if flags & CASCADE_UPDATE != 0 {
        Cascade
    } else {
        NoAction
    };
    let delete = if flags & CASCADE_DELETE != 0 {
        Cascade
    } else if flags & CASCADE_NULL_ON_DELETE != 0 {
        SetNull
    } else {
        NoAction
    };
    Ok((update, delete))
}

fn access_error(error: impl std::fmt::Display) -> String {
    format!("无法读取 Access 数据库。{error}")
}

fn open_error(error: FileError) -> String {
    match error {
        FileError::PasswordRequired => PASSWORD_REQUIRED.into(),
        FileError::InvalidPassword => "Access 数据库密码不正确。".into(),
        FileError::UnsupportedEncryption { .. } => {
            "当前版本暂不支持此 Access 文件的加密方式。".into()
        }
        other => access_error(other),
    }
}
