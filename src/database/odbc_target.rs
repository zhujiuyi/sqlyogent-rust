use mysql::prelude::Queryable;
use mysql::{Params, Statement, Transaction, Value};
use serde::{Deserialize, Serialize};

use crate::sql_tools::hexadecimal_bytes;

use super::{Cell, DbObject, OdbcColumnDefinition, mysql_error, quote_identifier};

pub(crate) fn odbc_identifier_key(value: &str) -> String {
    format!("IF(@@lower_case_table_names = 0, {value}, LOWER({value}))")
}

#[derive(Clone, Copy, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OdbcConflict {
    #[default]
    Abort,
    KeepTarget,
    Replace,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OdbcReset {
    #[default]
    None,
    Truncate,
    Recreate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OdbcReferentialAction {
    Cascade,
    Restrict,
    SetNull,
    NoAction,
    SetDefault,
}

pub struct OdbcForeignKey {
    pub name: Option<String>,
    pub columns: Vec<String>,
    pub referenced_table: String,
    pub referenced_columns: Vec<String>,
    pub on_update: OdbcReferentialAction,
    pub on_delete: OdbcReferentialAction,
}

pub struct OdbcTarget {
    pub object: DbObject,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
    pub create: Option<Vec<OdbcColumnDefinition>>,
    pub create_sql: Option<String>,
    pub engine: Option<String>,
    pub indexes: Vec<OdbcIndex>,
    pub foreign_keys: Vec<OdbcForeignKey>,
    pub track_applied_rows: bool,
    pub conflict: OdbcConflict,
    pub reset: OdbcReset,
    pub expected_ddl: Option<String>,
}

impl OdbcTarget {
    pub(super) fn staged_engine(&self) -> Option<&str> {
        self.create
            .as_ref()
            .map(|_| self.engine.as_deref().unwrap_or("InnoDB"))
    }
}

pub fn validate_odbc_engine(engine: &str) -> Result<(), String> {
    if engine.is_empty()
        || engine.len() > 64
        || !engine
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err("请输入有效的存储引擎名称。".into());
    }
    Ok(())
}

pub(super) fn supports_staged_nontransactional_engine(engine: &str) -> bool {
    engine.eq_ignore_ascii_case("MyISAM") || engine.eq_ignore_ascii_case("Aria")
}

pub struct OdbcIndex {
    pub name: String,
    pub unique: bool,
    pub columns: Vec<(String, bool)>,
}

#[derive(Default)]
pub struct OdbcImportCounts {
    pub rows: u64,
    pub skipped: u64,
    pub replaced: u64,
    pub warnings: Vec<String>,
    pub source_action: Option<Result<usize, String>>,
    pub(crate) applied_rows: Vec<Option<Vec<bool>>>,
}

impl OdbcImportCounts {
    pub fn detail(&self) -> String {
        let mut parts = Vec::new();
        if self.skipped > 0 {
            parts.push(format!("保留目标数据，跳过 {} 行重复数据。", self.skipped));
        }
        if self.replaced > 0 {
            parts.push(format!("替换了 {} 行旧目标数据。", self.replaced));
        }
        if let Some(action) = &self.source_action {
            parts.push(match action {
                Ok(rows) => format!("已处理 {rows} 行源记录。"),
                Err(error) => format!("目标数据已提交，源表操作未完成。{error}"),
            });
        }
        parts.extend(self.warnings.iter().cloned());
        parts.join(" ")
    }
}

pub(super) struct PreparedInsert {
    statement: Statement,
    binary_columns: Vec<bool>,
}

pub(super) fn prepare(
    tx: &mut Transaction<'_>,
    object: &DbObject,
    columns: &[String],
    rows: &[Vec<Cell>],
    conflict: OdbcConflict,
    expected_ddl: Option<&str>,
) -> Result<PreparedInsert, String> {
    let qualified = object.qualified_name();
    if let Some(expected) = expected_ddl {
        let (_, current): (String, String) = tx
            .query_first(format!("SHOW CREATE TABLE {qualified}"))
            .map_err(mysql_error)?
            .ok_or("无法读取导入目标表结构。")?;
        if super::sync_engine::stable_definition(current)
            != super::sync_engine::stable_definition(expected.to_owned())
        {
            return Err(format!(
                "目标表 {qualified} 的结构已变更，请重新读取源数据并检查映射。"
            ));
        }
    }

    let verb = if conflict == OdbcConflict::Replace {
        "REPLACE"
    } else {
        "INSERT"
    };
    // mysql::Value::Bytes binds strings using the session character set. Recover
    // binary parameters on the server to preserve every octet across charsets.
    let mut binary_columns = vec![false; columns.len()];
    for row in rows {
        for (binary, cell) in binary_columns.iter_mut().zip(row) {
            *binary |= matches!(cell, Cell::Binary(_));
        }
    }
    let parameters = binary_columns
        .iter()
        .map(|&binary| if binary { "UNHEX(?)" } else { "?" })
        .collect::<Vec<_>>()
        .join(", ");
    let columns = columns
        .iter()
        .map(|name| quote_identifier(name))
        .collect::<Vec<_>>()
        .join(", ");
    let statement = tx
        .prep(format!(
            "{verb} INTO {qualified} ({columns}) VALUES ({parameters})"
        ))
        .map_err(mysql_error)?;
    Ok(PreparedInsert {
        statement,
        binary_columns,
    })
}

pub(super) fn write_row(
    tx: &mut Transaction<'_>,
    insert: &PreparedInsert,
    row: Vec<Cell>,
    conflict: OdbcConflict,
    counts: &mut OdbcImportCounts,
) -> Result<bool, String> {
    let values = row
        .into_iter()
        .zip(&insert.binary_columns)
        .map(|(cell, &binary)| match cell {
            Cell::Null => Ok(Value::NULL),
            Cell::Text(text) if !binary => Ok(Value::from(text)),
            Cell::Text(_) => Err("同一个 ODBC 源列同时返回了文本和二进制数据。"),
            Cell::Binary(bytes) => Ok(Value::Bytes(hexadecimal_bytes(&bytes).collect())),
        })
        .collect::<Result<_, _>>()?;
    match tx.exec_drop(&insert.statement, Params::Positional(values)) {
        Ok(()) => {
            let affected = tx.affected_rows();
            super::check_write_warnings(tx)?;
            counts.rows += 1;
            if conflict == OdbcConflict::Replace {
                counts.replaced += affected.saturating_sub(1);
            }
            Ok(true)
        }
        Err(mysql::Error::MySqlError(error))
            if conflict == OdbcConflict::KeepTarget && error.code == 1062 =>
        {
            counts.skipped += 1;
            Ok(false)
        }
        Err(error) => Err(mysql_error(error)),
    }
}
