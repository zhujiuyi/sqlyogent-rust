use std::num::NonZeroU32;
use std::path::Path;

use crate::http_tunnel;

use super::super::transfer::{self, ExcelColumnType, ExportKind, ExportSettings, TransferSummary};
use super::super::{Cell, ColumnInfo, DbObject, ObjectKind, quote_identifier};
use super::{RESPONSE_TOO_LARGE, Session};

const PAGE_ROWS: usize = 500;

impl Session {
    pub(super) fn export(
        &self,
        object: &DbObject,
        path: &Path,
        settings: &ExportSettings,
    ) -> Result<TransferSummary, String> {
        if !object.kind.is_tabular() {
            return Err("只能从表或视图导出数据。".into());
        }
        if settings.kind == ExportKind::Sql {
            return transfer::export_sql_over_http(self, object, path, settings);
        }
        let qualified = object.qualified_name();
        let projection = transfer::export_projection(settings.fields.as_deref())?;
        let query = format!("SELECT {projection} FROM {qualified}");
        let names = self
            .read_raw_set(&format!("{query} LIMIT 0"), 1)?
            .columns
            .into_iter()
            .map(|column| column.name)
            .collect::<Vec<_>>();
        if names.is_empty() {
            return Err("服务器没有返回可导出的字段。".into());
        }
        let metadata = self.inspect(object.clone())?;
        let columns = names
            .iter()
            .map(|name| {
                metadata
                    .columns
                    .iter()
                    .find(|column| column.name == *name)
                    .ok_or_else(|| format!("无法确认导出字段「{name}」的数据类型。"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let binary = columns
            .iter()
            .map(|column| is_binary(column))
            .collect::<Vec<_>>();
        let types = columns
            .iter()
            .map(|column| ExcelColumnType::from_type_name(&column.data_type))
            .collect::<Vec<_>>();
        let order = paging_order(object, &metadata.columns, names.iter().map(String::as_str));
        let rows = self
            .paged_rows(format!("{query} ORDER BY {order}"), Some(&names))
            .map(|row| {
                let row = row?;
                if row.len() != binary.len() {
                    return Err("服务器返回的数据列数不符。".into());
                }
                Ok(row
                    .into_iter()
                    .zip(&binary)
                    .map(|(cell, binary)| match cell {
                        None => Cell::Null,
                        Some(bytes) if *binary => Cell::Binary(bytes),
                        Some(bytes) => String::from_utf8(bytes)
                            .map(Cell::Text)
                            .unwrap_or_else(|error| Cell::Binary(error.into_bytes())),
                    })
                    .collect())
            });
        transfer::export_rows(path, &names, &types, rows, settings)
    }

    pub(in crate::database) fn paged_rows<'a>(
        &'a self,
        query: String,
        columns: Option<&'a [String]>,
    ) -> impl Iterator<Item = Result<Vec<Option<Vec<u8>>>, String>> + 'a {
        self.paged_rows_with_limit(query, columns, None)
    }

    pub(in crate::database) fn paged_rows_with_limit<'a>(
        &'a self,
        query: String,
        columns: Option<&'a [String]>,
        limit: Option<NonZeroU32>,
    ) -> impl Iterator<Item = Result<Vec<Option<Vec<u8>>>, String>> + 'a {
        PagedRows {
            session: self,
            query,
            columns,
            offset: 0,
            page_size: limit.map_or(PAGE_ROWS, |size| size.get() as usize),
            rows: Vec::new().into_iter(),
            finished: false,
        }
    }

    pub(super) fn read_raw_set(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<http_tunnel::ResultSet, String> {
        self.read_raw_set_mode(query, limit, false)
    }

    fn read_raw_set_mode(
        &self,
        query: &str,
        limit: usize,
        batch: bool,
    ) -> Result<http_tunnel::ResultSet, String> {
        let mut reply = self.exchange(query, batch, limit)?;
        if let Some(error) = reply.error {
            return Err(format!("ERROR {}: {}", error.code, error.message));
        }
        if reply.sets.len() != 1 || reply.omitted_sets != 0 {
            return Err("HTTP 隧道返回了意外数量的结果集。".into());
        }
        let set = reply.sets.remove(0);
        if set.truncated {
            return Err("HTTP 隧道截断了查询结果。".into());
        }
        Ok(set)
    }
}

struct PagedRows<'a> {
    session: &'a Session,
    query: String,
    columns: Option<&'a [String]>,
    offset: usize,
    page_size: usize,
    rows: std::vec::IntoIter<Vec<Option<Vec<u8>>>>,
    finished: bool,
}

impl PagedRows<'_> {
    fn load(&mut self) -> Result<(), String> {
        let set = loop {
            let query = format!(
                "{} LIMIT {} OFFSET {}",
                self.query, self.page_size, self.offset
            );
            let sql = super::restore::read_batch("SET SESSION time_zone = '+00:00'", &query)?;
            match self.session.read_raw_set_mode(&sql, self.page_size, true) {
                Err(error) if error == RESPONSE_TOO_LARGE && self.page_size > 1 => {
                    self.page_size /= 2;
                }
                outcome => break outcome?,
            }
        };
        if self.columns.is_some_and(|names| {
            set.columns
                .iter()
                .map(|column| column.name.as_str())
                .ne(names.iter().map(String::as_str))
        }) {
            return Err("导出过程中返回的列结构发生了变化。".into());
        }
        self.offset = self
            .offset
            .checked_add(set.rows.len())
            .ok_or("导出行数超过上限。")?;
        self.finished = set.rows.len() < self.page_size;
        self.rows = set.rows.into_iter();
        Ok(())
    }
}

impl Iterator for PagedRows<'_> {
    type Item = Result<Vec<Option<Vec<u8>>>, String>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(row) = self.rows.next() {
                return Some(Ok(row));
            }
            if self.finished {
                return None;
            }
            if let Err(error) = self.load() {
                self.finished = true;
                return Some(Err(error));
            }
        }
    }
}

fn is_binary(column: &ColumnInfo) -> bool {
    let data_type = column.data_type.split(['(', ' ']).next().unwrap_or("");
    matches!(
        data_type,
        "binary"
            | "varbinary"
            | "tinyblob"
            | "blob"
            | "mediumblob"
            | "longblob"
            | "bit"
            | "geometry"
            | "point"
            | "linestring"
            | "polygon"
            | "multipoint"
            | "multilinestring"
            | "multipolygon"
            | "geometrycollection"
    )
}

pub(in crate::database) fn paging_order<'a>(
    object: &DbObject,
    all_columns: &[ColumnInfo],
    columns: impl Iterator<Item = &'a str>,
) -> String {
    let keys = all_columns
        .iter()
        .filter(|column| object.kind == ObjectKind::Table && column.key == "PRI")
        .map(|column| quote_identifier(&column.name))
        .collect::<Vec<_>>();
    if !keys.is_empty() {
        return keys.join(", ");
    }
    // MariaDB can truncate long sort keys. Hash each column for stable paging of large values.
    columns
        .map(|column| format!("MD5(CAST({} AS BINARY))", quote_identifier(column)))
        .collect::<Vec<_>>()
        .join(", ")
}
