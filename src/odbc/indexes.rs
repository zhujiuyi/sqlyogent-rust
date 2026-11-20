use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;

use serde::{Deserialize, Serialize};

use super::{
    Handle, NO_DATA, Resource, Return, STMT, SourceColumn, argument_parts, check_import_cancelled,
    connection_argument, read_cell,
};

#[derive(Clone, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceTable {
    pub table: String,
    pub catalog: Option<String>,
    pub schema: Option<String>,
}

impl SourceTable {
    pub(crate) fn validate(&self) -> Result<(), String> {
        for name in [
            Some(&self.table),
            self.catalog.as_ref(),
            self.schema.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            if !valid_name(name) {
                return Err("请填写有效的 ODBC 源表名，以及可选的目录和架构。".into());
            }
        }
        Ok(())
    }

    pub(super) fn from_driver(
        catalog: Option<String>,
        schema: Option<String>,
        table: Option<String>,
    ) -> Result<Self, String> {
        let table = Self {
            table: table.unwrap_or_default(),
            catalog: catalog.filter(|value| !value.is_empty()),
            schema: schema.filter(|value| !value.is_empty()),
        };
        table
            .validate()
            .map_err(|_| "ODBC 驱动返回的目录、架构或表名无效。")?;
        Ok(table)
    }

    pub(super) fn record_identity(
        &self,
        identity: &mut Option<Self>,
        actual: Self,
    ) -> Result<(), String> {
        let matches = |expected: &str, found: &str| expected.to_lowercase() == found.to_lowercase();
        let matches_scope = |expected: Option<&str>, found: Option<&str>| {
            expected.is_none_or(|expected| found.is_some_and(|found| matches(expected, found)))
        };
        if !matches(&self.table, &actual.table)
            || !matches_scope(self.catalog.as_deref(), actual.catalog.as_deref())
            || !matches_scope(self.schema.as_deref(), actual.schema.as_deref())
        {
            return Err("ODBC 驱动返回了其他表的元数据，请核对源表的目录、架构及表名。".into());
        }
        if identity.get_or_insert_with(|| actual.clone()) != &actual {
            return Err("ODBC 驱动返回了不同源表的元数据，请指定准确的目录和架构。".into());
        }
        Ok(())
    }
}

pub(super) fn valid_name(name: &str) -> bool {
    !name.trim().is_empty() && !name.contains('\0') && name.encode_utf16().count() <= 255
}

pub(crate) struct SourceIndex {
    pub(crate) name: String,
    pub(crate) unique: bool,
    pub(crate) columns: Vec<(String, bool)>,
}

pub(super) type Statistics = unsafe extern "system" fn(
    Handle,
    *mut u16,
    i16,
    *mut u16,
    i16,
    *mut u16,
    i16,
    u16,
    u16,
) -> Return;

pub(super) fn read_indexes(
    source: &Resource<'_>,
    table: &SourceTable,
    columns: &[SourceColumn],
    identity: &mut Option<SourceTable>,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<SourceIndex>, String> {
    let manager = source.manager;
    let statistics = manager
        .statistics
        .ok_or("当前 ODBC 驱动管理器没有提供索引读取接口。")?;
    let statement = Resource::allocate(manager, STMT, source.handle)?;
    let mut catalog = connection_argument(table.catalog.as_deref())?;
    let mut schema = connection_argument(table.schema.as_deref())?;
    let mut name = connection_argument(Some(&table.table))?;
    let (catalog_ptr, catalog_len) = argument_parts(&mut catalog);
    let (schema_ptr, schema_len) = argument_parts(&mut schema);
    let (table_ptr, table_len) = argument_parts(&mut name);
    check_import_cancelled(cancel)?;
    let result = unsafe {
        (manager.set_statement)(statement.handle, 0, 30 as Handle, 0);
        statistics(
            statement.handle,
            catalog_ptr,
            catalog_len,
            schema_ptr,
            schema_len,
            table_ptr,
            table_len,
            1, // SQL_INDEX_ALL
            0, // SQL_QUICK
        )
    };
    check_import_cancelled(cancel)?;
    statement.check(result, "无法读取 ODBC 源表索引。")?;
    if result == super::SUCCESS_WITH_INFO {
        return Err("ODBC 源表索引元数据返回了警告。".into());
    }

    let mut remaining = 256 * 1024;
    let mut indexes = BTreeMap::new();
    loop {
        check_import_cancelled(cancel)?;
        let fetched = unsafe { (manager.fetch)(statement.handle) };
        check_import_cancelled(cancel)?;
        if fetched == NO_DATA {
            break;
        }
        statement.check(fetched, "无法读取 ODBC 源表索引信息。")?;
        let mut field = |column| read_cell(&statement, column, &mut remaining, cancel);
        let actual = SourceTable::from_driver(field(1)?, field(2)?, field(3)?)?;
        table.record_identity(identity, actual)?;
        let non_unique = field(4)?;
        let key = field(6)?.unwrap_or_default();
        let kind = field(7)?.unwrap_or_default();
        if kind == "0" {
            continue;
        }
        let position = field(8)?
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|&position| position > 0)
            .ok_or("ODBC 驱动返回了无效的索引列顺序。")?;
        let column = field(9)?.unwrap_or_default();
        let direction = field(10)?.unwrap_or_default();
        let filter = field(13)?;
        let unique = match non_unique.as_deref() {
            Some("0") => true,
            Some("1") => false,
            _ => return Err("无法确认 ODBC 源索引是否唯一。".into()),
        };
        let descending = match direction.as_str() {
            "" | "A" => false,
            "D" => true,
            _ => return Err("ODBC 驱动返回了无效的索引列排序方向。".into()),
        };
        if !matches!(kind.as_str(), "1" | "3") || key.is_empty() || filter.is_some() {
            return Err("源表包含暂不支持自动迁移的索引类型或筛选条件。".into());
        }
        let mapped = columns
            .iter()
            .any(|field| field.name.to_lowercase() == column.to_lowercase());
        if column.is_empty() || !mapped {
            return Err(format!(
                "ODBC 源索引 {key} 的列未出现在查询中，或使用了表达式。"
            ));
        }
        let index = indexes.entry(key.clone()).or_insert_with(|| SourceIndex {
            name: key.clone(),
            unique,
            columns: Vec::new(),
        });
        if index.unique != unique || position != index.columns.len() + 1 {
            return Err(format!("ODBC 源索引 {key} 的定义不完整。"));
        }
        index.columns.push((column, descending));
    }
    Ok(indexes.into_values().collect())
}
