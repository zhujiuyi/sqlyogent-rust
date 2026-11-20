use std::collections::{BTreeMap, HashMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use mysql::prelude::Queryable;
use mysql::{Conn, IsolationLevel, Opts, Params, Row, Transaction, TxOpts, Value, params};

use super::quote_identifier;
use super::{
    Cell, DbObject, ObjectKind, OperationMonitor, QueryTiming, RESULT_MEMORY_LIMIT,
    RegisteredTableRead, cell, check_write_warnings, column_is_binary, mysql_error,
};

pub(super) const MAX_PAGE_ROWS: usize = 1_000;
const MAX_INDEX_PARTS: usize = 10_000;
mod http;
mod keyless;
pub(super) use http::{
    fingerprint_column, http_bounded_select, http_bounded_select_timed, http_delete_table_rows,
    http_edit_table_with_confirmation, http_layout, http_page_from_raw, http_table_data,
    http_table_page, import_layout_guard, layout_guard, selected_key_layout_guard,
};

const MATCH_TABLE: &str = "
    t.TABLE_SCHEMA = :schema AND t.TABLE_NAME = :table
    AND CAST(IF(@@lower_case_table_names = 0, t.TABLE_SCHEMA, LOWER(t.TABLE_SCHEMA)) AS BINARY)
        = CAST(IF(@@lower_case_table_names = 0, :schema, LOWER(:schema)) AS BINARY)
    AND CAST(IF(@@lower_case_table_names = 0, t.TABLE_NAME, LOWER(t.TABLE_NAME)) AS BINARY)
        = CAST(IF(@@lower_case_table_names = 0, :table, LOWER(:table)) AS BINARY)";

pub(super) fn native_fingerprint_column(
    layout: &TableLayout,
    table_alias: Option<&str>,
    label: &str,
) -> String {
    format!(
        "{} AS {}",
        http::fingerprint_expression(layout, table_alias),
        quote_identifier(label)
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableColumn {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub default_value: Option<String>,
    pub writable: bool,
    pub auto_increment: bool,
    pub charset: Option<String>,
    pub collation: Option<String>,
    pub declared_width: Option<usize>,
    pub generation_expression: Option<String>,
    extra: String,
    value_type: ValueType,
}

impl TableColumn {
    pub(super) fn visible_by_default(&self) -> bool {
        !self
            .extra
            .split_ascii_whitespace()
            .any(|word| word.eq_ignore_ascii_case("INVISIBLE"))
    }

    pub fn is_binary(&self) -> bool {
        self.value_type == ValueType::Binary
    }

    pub fn parse_filter_value(&self, input: String) -> Result<Value, String> {
        self.value_type.parse(input).ok_or_else(|| {
            format!(
                "列「{}」需要{}。",
                self.name,
                self.value_type.input_format()
            )
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct TableSelection {
    pub predicates: Vec<TablePredicate>,
    pub order: Vec<TableSort>,
}

#[derive(Clone, Debug)]
pub struct TablePredicate {
    pub column: String,
    pub operator: TablePredicateOp,
    pub value: Value,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TablePredicateOp {
    Equal,
    NotEqual,
    Less,
    Greater,
    StartsWith,
    EndsWith,
    Contains,
}

#[derive(Clone, Debug)]
pub struct TableSort {
    pub column: String,
    pub descending: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TableReadRange {
    Page { offset: usize, rows: NonZeroUsize },
    All,
}

impl TableReadRange {
    fn offset(self) -> usize {
        match self {
            Self::Page { offset, .. } => offset,
            Self::All => 0,
        }
    }

    fn batch_limit(self, loaded: usize) -> usize {
        match self {
            Self::Page { rows, .. } => rows.get().saturating_sub(loaded).min(MAX_PAGE_ROWS),
            Self::All => MAX_PAGE_ROWS,
        }
    }
}

pub struct TablePage {
    pub object: DbObject,
    pub columns: Vec<TableColumn>,
    pub column_labels: Vec<String>,
    pub primary_key: Vec<usize>,
    /// Index columns identifying each displayed row for edits.
    pub row_key: Vec<usize>,
    pub rows: Vec<TableRow>,
    pub offset: usize,
    pub has_more: bool,
    pub can_insert: bool,
    pub edit_error: Option<String>,
    pub query_timing: Option<QueryTiming>,
}

impl TablePage {
    pub fn column_label(&self, index: usize) -> &str {
        self.column_labels
            .get(index)
            .unwrap_or(&self.columns[index].name)
    }
}

#[derive(Clone)]
pub struct TableRow {
    pub cells: Vec<Cell>,
    layout: Arc<TableLayout>,
    original: Arc<[Value]>,
    fingerprint: Option<String>,
}

impl TableRow {
    pub fn original(&self) -> &[Value] {
        &self.original
    }

    /// The complete source row, including columns omitted or reordered by F8.
    pub fn source_columns(&self) -> &[TableColumn] {
        &self.layout.columns
    }

    pub fn source_cell(&self, index: usize) -> Option<Cell> {
        let column = self.layout.columns.get(index)?;
        let value = self.original.get(index)?;
        Some(column.value_type.display(value, column.is_binary()))
    }

    /// Compare the source key across snapshots of the same table. Identical
    /// keyless rows share the same identity.
    pub fn same_source_record(&self, other: &Self) -> bool {
        let layout = &self.layout;
        let other_layout = &other.layout;
        if layout.object != other_layout.object
            || layout.columns != other_layout.columns
            || layout.primary_key != other_layout.primary_key
            || layout.edit_error.is_some()
            || other_layout.edit_error.is_some()
            || self.original.len() != layout.columns.len()
            || other.original.len() != other_layout.columns.len()
        {
            return false;
        }
        if layout.is_keyless() || other_layout.is_keyless() {
            return layout.is_keyless()
                && other_layout.is_keyless()
                && self.fingerprint.is_some()
                && self.fingerprint == other.fingerprint
                && keyless::same_values(self.original(), other.original());
        }
        // F8 and the table page may select different keys from the same layout.
        // Complete source snapshots include the first verified key in either view.
        let (Some(key), Some(other_key)) =
            (layout.edit_keys.first(), other_layout.edit_keys.first())
        else {
            return false;
        };
        key == other_key
            && !key.columns.is_empty()
            && key.columns.iter().all(|&index| {
                self.original
                    .get(index)
                    .zip(other.original.get(index))
                    .is_some_and(|(left, right)| keyless::same_value(left, right))
            })
    }
}

#[derive(Clone, Debug)]
pub enum FieldValue {
    Text(String),
    Binary(Vec<u8>),
    Null,
    Default,
}

#[derive(Clone)]
pub enum TableEdit {
    Update {
        original: TableRow,
        values: Vec<(String, FieldValue)>,
    },
    Delete {
        original: TableRow,
    },
    Insert {
        values: Vec<(String, FieldValue)>,
    },
}

/// Render the pending single-row update using its captured source layout.
pub fn preview_table_update(edit: &TableEdit) -> Result<String, String> {
    let TableEdit::Update { original, values } = edit else {
        return Err("只有已有行的修改可以预览 UPDATE 语句。".into());
    };
    if values.is_empty() {
        return Err("没有需要保存的修改。".into());
    }
    let layout = &original.layout;
    if let Some(error) = &layout.edit_error {
        return Err(error.clone());
    }
    if original.original().len() != layout.columns.len() {
        return Err("未读取该行的全部原值，请刷新后重试。".into());
    }
    let assignments = http::write_values(layout, values.clone())?
        .into_iter()
        .map(|(column, value)| format!("{column} = {}", value.as_deref().unwrap_or("DEFAULT")))
        .collect::<Vec<_>>()
        .join(", ");
    let (predicate, comment) = if layout.is_keyless() {
        if original.fingerprint.is_none() {
            return Err("未读取该行的完整原值校验值，请刷新后重试。".into());
        }
        (
            format!(
                "BINARY {} <=> CAST(? AS BINARY)",
                http::exact_row_expression(layout, None)
            ),
            "-- ? 表示所选行的整行原值校验参数。\n",
        )
    } else {
        let key = layout.recheck_key(original)?;
        let predicate = match original.fingerprint {
            Some(_) => http::original_predicate(layout, key, original)?,
            None => http::original_key_conditions(layout, key, original)?.join(" AND "),
        };
        (predicate, "")
    };
    Ok(format!(
        "{comment}UPDATE {}\nSET {assignments}\nWHERE {predicate}\nLIMIT 1;",
        layout.object.qualified_name()
    ))
}

#[derive(Clone, Copy)]
pub enum DuplicateScope {
    One,
    All,
}

#[derive(Clone, Copy)]
pub struct DuplicateChoice {
    pub matches: u64,
    pub scope: DuplicateScope,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TableEditOutcome {
    Applied(u64),
    NeedsConfirmation { matches: u64 },
}

pub(super) struct TableLayout {
    object: DbObject,
    pub(super) columns: Vec<TableColumn>,
    pub(super) primary_key: Vec<usize>,
    pub(super) edit_error: Option<String>,
    edit_keys: Vec<EditKey>,
    selected_key: usize,
}

#[derive(Eq, PartialEq)]
struct EditKey {
    index: String,
    columns: Vec<usize>,
}

impl TableLayout {
    pub(super) fn object(&self) -> &DbObject {
        &self.object
    }

    pub(super) fn projection(&self) -> String {
        self.columns
            .iter()
            .map(|column| quote_identifier(&column.name))
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub(super) fn row_key(&self) -> &[usize] {
        self.edit_keys
            .get(self.selected_key)
            .map_or(&[], |key| key.columns.as_slice())
    }

    pub(super) fn is_keyless(&self) -> bool {
        self.edit_error.is_none() && self.edit_keys.is_empty()
    }

    pub(super) fn select_projected_key(&mut self, projection: &[usize]) -> Result<(), String> {
        // The F8 loaders hydrate all remaining original columns in the source SELECT.
        if self.is_keyless() {
            return Ok(());
        }
        self.selected_key = self
            .edit_keys
            .iter()
            .position(|key| key.columns.iter().all(|index| projection.contains(index)))
            .ok_or("要编辑查询结果，请在 SELECT 中包含源表的完整主键或一个完整的非空唯一键。")?;
        Ok(())
    }

    fn recheck_keyless(&self, original: &TableRow) -> Result<(), String> {
        let prior = &original.layout;
        if self.is_keyless()
            && prior.is_keyless()
            && self.object == prior.object
            && self.columns == prior.columns
            && self.primary_key == prior.primary_key
        {
            return Ok(());
        }
        Err("表结构已变化，请刷新后重试。".into())
    }

    fn recheck_key(&self, original: &TableRow) -> Result<&EditKey, String> {
        let prior = &original.layout;
        if self.object == prior.object
            && self.columns == prior.columns
            && self.primary_key == prior.primary_key
            && self.edit_error == prior.edit_error
            && let Some(key) = prior.edit_keys.get(prior.selected_key)
            && let Some(confirmed) = self.edit_keys.iter().find(|candidate| *candidate == key)
        {
            return Ok(confirmed);
        }
        Err("表结构已变化，请刷新后重试。".into())
    }
}

pub(super) fn table_page(
    opts: &Opts,
    object: DbObject,
    limit: usize,
    offset: usize,
) -> Result<TablePage, String> {
    table_page_selected(opts, object, limit, offset, &TableSelection::default())
}

pub(super) fn table_page_selected(
    opts: &Opts,
    object: DbObject,
    limit: usize,
    offset: usize,
    selection: &TableSelection,
) -> Result<TablePage, String> {
    let range = TableReadRange::Page {
        offset,
        rows: NonZeroUsize::new(limit.clamp(1, MAX_PAGE_ROWS)).unwrap(),
    };
    ensure_table(&object)?;
    let mut connection = Conn::new(opts.clone()).map_err(mysql_error)?;
    read_table_data(&mut connection, object, selection, range, None, || Ok(()))
}

pub(super) fn table_data(
    opts: &Opts,
    object: DbObject,
    selection: &TableSelection,
    range: TableReadRange,
    monitor: &Mutex<OperationMonitor>,
) -> Result<TablePage, String> {
    let check = || monitor.lock().unwrap().check_cancelled();
    check()?;
    ensure_table(&object)?;
    let mut connection = Conn::new(opts.clone()).map_err(mysql_error)?;
    let read = RegisteredTableRead::new(monitor, &connection, opts)?;
    read_table_data(
        &mut connection,
        object,
        selection,
        range,
        Some(&read),
        check,
    )
}

fn read_table_data(
    connection: &mut Conn,
    object: DbObject,
    selection: &TableSelection,
    range: TableReadRange,
    read: Option<&RegisteredTableRead<'_>>,
    mut check: impl FnMut() -> Result<(), String>,
) -> Result<TablePage, String> {
    check()?;
    let qualified = object.qualified_name();
    let stable = read.is_some();
    let tag = read.map_or("", RegisteredTableRead::tag);
    let mut options = TxOpts::default();
    if stable {
        options = options.set_isolation_level(Some(IsolationLevel::RepeatableRead));
    }
    let mut transaction = connection.start_transaction(options).map_err(mysql_error)?;
    let layout = load_tagged_layout(&mut transaction, object, &qualified, tag)?;
    let keyless_fingerprint = layout.is_keyless();
    let mut arguments = Vec::new();
    let clause = table_where(&layout, &selection.predicates, &mut arguments)?;
    let order = table_order(&layout, &selection.order, stable)?;
    let mut projection = layout.projection();
    if keyless_fingerprint {
        projection.push_str(", ");
        projection.push_str(&native_fingerprint_column(
            &layout,
            None,
            "__harbor_row_fingerprint",
        ));
    }
    let sql = format!("SELECT {tag}{projection} FROM {qualified}{clause}{order} LIMIT ? OFFSET ?");
    let mut page = PageBuffer::new(layout);
    let has_more = page.fill(range, |page, limit, offset| {
        check()?;
        let mut arguments = arguments.clone();
        arguments.extend([Value::UInt(limit as u64 + 1), Value::UInt(offset as u64)]);
        let mut result = transaction
            .exec_iter(&sql, Params::Positional(arguments))
            .map_err(mysql_error)?;
        let binary: Vec<_> = result
            .columns()
            .as_ref()
            .iter()
            .take(page.layout.columns.len())
            .map(column_is_binary)
            .collect();
        page.push_native_rows(
            &binary,
            None,
            result.iter().into_iter().flatten(),
            keyless_fingerprint,
            limit,
        )
    })?;
    check()?;
    Ok(page.finish(range.offset(), has_more))
}

fn table_where(
    layout: &TableLayout,
    predicates: &[TablePredicate],
    arguments: &mut Vec<Value>,
) -> Result<String, String> {
    if predicates.is_empty() {
        return Ok(String::new());
    }
    let expressions = predicates
        .iter()
        .map(|predicate| {
            let column = &layout.columns[column_index(layout, &predicate.column)?];
            let name = quote_identifier(&column.name);
            use TablePredicateOp::*;
            if predicate.value == Value::NULL {
                return match predicate.operator {
                    Equal => Ok(format!("{name} IS NULL")),
                    NotEqual => Ok(format!("{name} IS NOT NULL")),
                    _ => Err(format!(
                        "列「{}」的 NULL 筛选需要使用等于或不等于。",
                        column.name
                    )),
                };
            }
            let operator = match predicate.operator {
                Equal => "=",
                NotEqual => "<>",
                Less => "<",
                Greater => ">",
                StartsWith | EndsWith | Contains => {
                    let Value::Bytes(pattern) = &predicate.value else {
                        return Err(format!(
                            "列「{}」的匹配条件需要文本或二进制值。",
                            column.name
                        ));
                    };
                    let mut escaped = Vec::with_capacity(pattern.len() + 2);
                    if predicate.operator != StartsWith {
                        escaped.push(b'%');
                    }
                    for &byte in pattern {
                        if matches!(byte, b'!' | b'%' | b'_') {
                            escaped.push(b'!');
                        }
                        escaped.push(byte);
                    }
                    if predicate.operator != EndsWith {
                        escaped.push(b'%');
                    }
                    let placeholder = bind_bytes(column.value_type, escaped, arguments);
                    return Ok(format!("{name} LIKE {placeholder} ESCAPE '!'"));
                }
            };
            let (placeholder, value) =
                match column.value_type {
                    ValueType::Decimal { precision, scale } => (
                        format!("CAST(? AS DECIMAL({precision}, {scale}))"),
                        predicate.value.clone(),
                    ),
                    ValueType::Bit(_) => (
                        "CAST(? AS UNSIGNED)".into(),
                        Value::UInt(bit_number(&predicate.value).ok_or_else(|| {
                            format!("无法读取列「{}」的 BIT 筛选值。", column.name)
                        })?),
                    ),
                    ValueType::Binary => {
                        let Value::Bytes(bytes) = &predicate.value else {
                            return Err(format!("列「{}」需要二进制筛选值。", column.name));
                        };
                        let placeholder = bind_bytes(column.value_type, bytes.clone(), arguments);
                        return Ok(format!("{name} {operator} {placeholder}"));
                    }
                    _ => ("?".into(), predicate.value.clone()),
                };
            arguments.push(value);
            Ok(format!("{name} {operator} {placeholder}"))
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(format!(" WHERE {}", expressions.join(" AND ")))
}

fn bind_bytes(kind: ValueType, bytes: Vec<u8>, arguments: &mut Vec<Value>) -> &'static str {
    // Hex-encode binary parameters so the client character set cannot change arbitrary bytes.
    if kind == ValueType::Binary {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        let encoded = bytes
            .iter()
            .flat_map(|&byte| [HEX[usize::from(byte >> 4)], HEX[usize::from(byte & 15)]])
            .collect();
        arguments.push(Value::Bytes(encoded));
        "UNHEX(?)"
    } else {
        arguments.push(Value::Bytes(bytes));
        "?"
    }
}

fn table_order(layout: &TableLayout, order: &[TableSort], stable: bool) -> Result<String, String> {
    let mut used = HashSet::new();
    let mut fields = Vec::new();
    for sort in order {
        let index = column_index(layout, &sort.column)?;
        if used.insert(index) {
            let direction = if sort.descending { "DESC" } else { "ASC" };
            fields.push(format!(
                "{} {direction}",
                quote_identifier(&layout.columns[index].name)
            ));
        }
    }
    for &index in if layout.row_key().is_empty() {
        &layout.primary_key
    } else {
        layout.row_key()
    } {
        if used.insert(index) {
            fields.push(quote_identifier(&layout.columns[index].name));
        }
    }
    if stable && layout.row_key().is_empty() && layout.primary_key.is_empty() {
        // OFFSET batches must break ties between visibly different keyless rows.
        fields.push(http::fingerprint_expression(layout, None));
    }
    if fields.is_empty() {
        Ok(String::new())
    } else {
        Ok(format!(" ORDER BY {}", fields.join(", ")))
    }
}

fn column_index(layout: &TableLayout, name: &str) -> Result<usize, String> {
    layout
        .columns
        .iter()
        .position(|column| column.name == name)
        .ok_or_else(|| format!("找不到列「{name}」。"))
}

pub(super) fn page_from_rows(
    layout: TableLayout,
    binary: &[bool],
    order: Option<&[usize]>,
    rows_from_server: impl Iterator<Item = mysql::Result<Row>>,
    keyless_fingerprint: bool,
    limit: usize,
    offset: usize,
) -> Result<TablePage, String> {
    let mut page = PageBuffer::new(layout);
    let end = page.push_native_rows(binary, order, rows_from_server, keyless_fingerprint, limit)?;
    Ok(page.finish(offset, !matches!(end, BatchEnd::Complete)))
}

enum BatchEnd {
    Complete,
    More,
    Full,
}

struct PageBuffer {
    layout: Arc<TableLayout>,
    rows: Vec<TableRow>,
    remaining: usize,
    edit_error: Option<String>,
}

impl PageBuffer {
    fn new(layout: TableLayout) -> Self {
        Self {
            edit_error: layout.edit_error.clone(),
            layout: Arc::new(layout),
            rows: Vec::new(),
            remaining: RESULT_MEMORY_LIMIT,
        }
    }

    fn fill(
        &mut self,
        range: TableReadRange,
        mut read: impl FnMut(&mut Self, usize, usize) -> Result<BatchEnd, String>,
    ) -> Result<bool, String> {
        loop {
            let limit = range.batch_limit(self.rows.len());
            if limit == 0 {
                return Ok(true);
            }
            let offset = range
                .offset()
                .checked_add(self.rows.len())
                .ok_or("读取位置已超出可用范围。")?;
            match read(self, limit, offset)? {
                BatchEnd::Complete => return Ok(false),
                BatchEnd::More => {}
                BatchEnd::Full => return Ok(true),
            }
        }
    }

    fn reserve_row(&mut self, weight: usize) -> Result<bool, String> {
        if weight > self.remaining {
            if self.rows.is_empty() {
                return Err("此行数据超过表格的显示容量。".into());
            }
            return Ok(false);
        }
        self.remaining -= weight;
        Ok(true)
    }

    fn push_native_rows(
        &mut self,
        binary: &[bool],
        order: Option<&[usize]>,
        rows_from_server: impl Iterator<Item = mysql::Result<Row>>,
        keyless_fingerprint: bool,
        limit: usize,
    ) -> Result<BatchEnd, String> {
        for (index, row) in rows_from_server.enumerate() {
            let mut values = row.map_err(mysql_error)?.unwrap();
            let fingerprint = if keyless_fingerprint {
                Some(read_native_fingerprint(values.pop())?)
            } else {
                None
            };
            if values.len() != self.layout.columns.len() || binary.len() != values.len() {
                return Err("服务器返回的列数与表结构不一致。".into());
            }
            if index == limit {
                return Ok(BatchEnd::More);
            }
            let weight = fingerprint.as_ref().map_or(0, String::len)
                + values.len() * (size_of::<Value>() + size_of::<Cell>())
                + values
                    .iter()
                    .map(|value| match value {
                        Value::Bytes(bytes) => 2 * bytes.len(),
                        _ => 32,
                    })
                    .sum::<usize>();
            if !self.reserve_row(weight)? {
                return Ok(BatchEnd::Full);
            }
            if let Some(order) = order {
                // The mutation guard compares values in the table's column order.
                let sorted = order
                    .iter()
                    .map(|&index| std::mem::replace(&mut values[index], Value::NULL))
                    .collect();
                values = sorted;
            }
            let cells = values
                .iter()
                .zip(&self.layout.columns)
                .zip(binary)
                .map(|((value, column), &binary)| column.value_type.display(value, binary))
                .collect();
            self.rows.push(TableRow {
                cells,
                layout: Arc::clone(&self.layout),
                original: values.into(),
                fingerprint,
            });
        }
        Ok(BatchEnd::Complete)
    }

    fn finish(self, offset: usize, has_more: bool) -> TablePage {
        TablePage {
            object: self.layout.object.clone(),
            columns: self.layout.columns.clone(),
            column_labels: Vec::new(),
            primary_key: self.layout.primary_key.clone(),
            row_key: self.layout.row_key().to_vec(),
            rows: self.rows,
            offset,
            has_more,
            can_insert: true,
            edit_error: self.edit_error,
            query_timing: None,
        }
    }
}

pub(super) fn read_native_fingerprint(value: Option<Value>) -> Result<String, String> {
    let Some(Value::Bytes(bytes)) = value else {
        return Err("服务器未返回原始记录校验值，请刷新后重试。".into());
    };
    String::from_utf8(bytes)
        .ok()
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| "服务器返回的原始记录校验值无效。".into())
}

pub(super) fn joined_page_from_values(
    layout: TableLayout,
    rows: Vec<(Vec<Value>, String)>,
    projection: &[(usize, String)],
    has_more: bool,
) -> TablePage {
    let layout = Arc::new(layout);
    let visible_key = |key: &[usize]| {
        key.iter()
            .filter_map(|index| projection.iter().position(|(source, _)| source == index))
            .collect()
    };
    let rows = rows
        .into_iter()
        .map(|(original, fingerprint)| TableRow {
            cells: projection
                .iter()
                .map(|(index, _)| {
                    let column = &layout.columns[*index];
                    column
                        .value_type
                        .display(&original[*index], column.is_binary())
                })
                .collect(),
            layout: Arc::clone(&layout),
            original: original.into(),
            fingerprint: Some(fingerprint),
        })
        .collect();
    TablePage {
        object: layout.object.clone(),
        columns: projection
            .iter()
            .map(|(index, _)| layout.columns[*index].clone())
            .collect(),
        column_labels: projection.iter().map(|(_, label)| label.clone()).collect(),
        primary_key: visible_key(&layout.primary_key),
        row_key: visible_key(layout.row_key()),
        rows,
        offset: 0,
        has_more,
        can_insert: false,
        edit_error: layout.edit_error.clone(),
        query_timing: None,
    }
}

#[cfg(test)]
pub(super) fn edit_table(opts: &Opts, object: DbObject, edit: TableEdit) -> Result<u64, String> {
    match edit_table_with_confirmation(opts, object, edit, None)? {
        TableEditOutcome::Applied(affected_rows) => Ok(affected_rows),
        TableEditOutcome::NeedsConfirmation { .. } => Err("有重复行，请先确认编辑范围。".into()),
    }
}

pub(super) fn edit_table_with_confirmation(
    opts: &Opts,
    object: DbObject,
    edit: TableEdit,
    choice: Option<DuplicateChoice>,
) -> Result<TableEditOutcome, String> {
    ensure_table(&object)?;
    let qualified = object.qualified_name();
    // Keep this transaction independent of transactions in the SQL editor.
    let mut connection = Conn::new(opts.clone()).map_err(mysql_error)?;
    let original = match &edit {
        TableEdit::Update { original, values } if !values.is_empty() => Some(original),
        TableEdit::Update { .. } => return Err("没有需要保存的修改。".into()),
        TableEdit::Delete { original } => Some(original),
        TableEdit::Insert { .. } => None,
    };
    let keyless = original.is_some_and(|row| row.layout.is_keyless());
    let mut transaction = connection
        .start_transaction(
            TxOpts::default()
                .set_isolation_level(keyless.then_some(IsolationLevel::RepeatableRead)),
        )
        .map_err(mysql_error)?;
    let layout = load_layout(&mut transaction, object, &qualified)?;
    if let Some(error) = &layout.edit_error {
        return Err(error.clone());
    }
    let (predicate, key, expected_rows, limit) = if let Some(original) = original {
        if keyless {
            let (predicate, key, matches) =
                keyless::lock_matches(&mut transaction, &layout, &qualified, original)?;
            let (expected_rows, limit) = match choice {
                Some(choice) if choice.matches != matches || matches < 2 => {
                    return Err("匹配行数已变化，请刷新后重试。".into());
                }
                Some(DuplicateChoice {
                    scope: DuplicateScope::One,
                    ..
                }) => (1, " LIMIT 1"),
                Some(DuplicateChoice {
                    scope: DuplicateScope::All,
                    ..
                }) => (matches, ""),
                None if matches > 1 => {
                    return Ok(TableEditOutcome::NeedsConfirmation { matches });
                }
                None => (1, ""),
            };
            (predicate, key, expected_rows, limit)
        } else {
            if choice.is_some() {
                return Err("此行使用唯一键定位，无需确认重复行。".into());
            }
            let (predicate, key) =
                verify_original(&mut transaction, &layout, &qualified, original)?;
            (predicate, key, 1, "")
        }
    } else {
        if choice.is_some() {
            return Err("新增行无需确认重复行。".into());
        }
        (String::new(), Vec::new(), 1, "")
    };
    let (sql, parameters, may_be_unchanged) = match edit {
        TableEdit::Update { values, .. } => {
            let values = prepare_values(&layout, values)?;
            let mut parameters = Vec::new();
            let mut assignments = Vec::new();
            for (column, value) in values {
                let placeholder = match value {
                    Some(value) => {
                        parameters.push(value);
                        "?"
                    }
                    None => "DEFAULT",
                };
                assignments.push(format!("{column} = {placeholder}"));
            }
            parameters.extend(key);
            (
                format!(
                    "UPDATE {qualified} SET {} WHERE {predicate}{limit}",
                    assignments.join(", ")
                ),
                parameters,
                true,
            )
        }
        TableEdit::Delete { .. } => (
            format!("DELETE FROM {qualified} WHERE {predicate}{limit}"),
            key,
            false,
        ),
        TableEdit::Insert { values } => {
            let (columns, parameters): (Vec<_>, Vec<_>) = prepare_values(&layout, values)?
                .into_iter()
                .filter_map(|(column, value)| value.map(|value| (column, value)))
                .unzip();
            let placeholders = vec!["?"; parameters.len()].join(", ");
            (
                format!(
                    "INSERT INTO {qualified} ({}) VALUES ({placeholders})",
                    columns.join(", ")
                ),
                parameters,
                false,
            )
        }
    };
    transaction
        .exec_drop(sql, Params::Positional(parameters))
        .map_err(mysql_error)?;
    let affected_rows = transaction.affected_rows();
    if affected_rows > expected_rows || affected_rows != expected_rows && !may_be_unchanged {
        return Err("服务器返回的受影响行数异常，修改已取消。".into());
    }
    check_write_warnings(&mut transaction).map_err(|error| format!("修改已取消。 {error}"))?;
    transaction
        .commit()
        .map_err(|error| format!("无法确认数据修改是否已提交。 {}", mysql_error(error)))?;
    Ok(TableEditOutcome::Applied(affected_rows))
}

struct DeleteRowGroup {
    original: TableRow,
    count: u64,
}

fn delete_row_groups(
    object: &DbObject,
    originals: Vec<TableRow>,
) -> Result<Vec<DeleteRowGroup>, String> {
    if originals.is_empty() {
        return Err("请选择要删除的行。".into());
    }
    let mut groups: Vec<DeleteRowGroup> = Vec::new();
    let mut snapshots = HashSet::new();
    let mut fingerprints = HashMap::<String, usize>::new();
    for original in originals {
        if &original.layout.object != object {
            return Err("所选行不属于当前表，请刷新后重试。".into());
        }
        if let Some(error) = &original.layout.edit_error {
            return Err(error.clone());
        }
        if original.original().len() != original.layout.columns.len() {
            return Err("未读取该行的全部原值，请刷新后重试。".into());
        }
        // A repeated capture is one displayed row, even on a keyless table.
        if !snapshots.insert(Arc::as_ptr(&original.original)) {
            return Err("所选行有重复，请重新勾选。".into());
        }
        if original.layout.is_keyless() {
            let fingerprint = original
                .fingerprint
                .as_deref()
                .filter(|value| !value.is_empty())
                .ok_or("未读取该行的完整原值校验值，请刷新后重试。")?;
            if let Some(&index) = fingerprints.get(fingerprint) {
                let group = &mut groups[index];
                group.original.layout.recheck_keyless(&original)?;
                if !keyless::same_values(group.original.original(), original.original()) {
                    return Err("有相同校验值的记录与整行原值不一致，请刷新后重试。".into());
                }
                group.count += 1;
                continue;
            }
            fingerprints.insert(fingerprint.to_owned(), groups.len());
        } else if groups.iter().any(|group| {
            let previous = &group.original;
            !previous.layout.is_keyless()
                && original
                    .layout
                    .row_key()
                    .iter()
                    .all(|&index| original.original().get(index) == previous.original().get(index))
                && original.original() == previous.original()
        }) {
            return Err("所选行有重复，请重新勾选。".into());
        }
        groups.push(DeleteRowGroup { original, count: 1 });
    }
    Ok(groups)
}

pub(super) fn delete_table_rows(
    opts: &Opts,
    object: DbObject,
    originals: Vec<TableRow>,
) -> Result<u64, String> {
    ensure_table(&object)?;
    let groups = delete_row_groups(&object, originals)?;
    let keyless = groups
        .iter()
        .any(|group| group.original.layout.is_keyless());
    let qualified = object.qualified_name();
    let mut connection = Conn::new(opts.clone()).map_err(mysql_error)?;
    let mut transaction = connection
        .start_transaction(
            TxOpts::default()
                .set_isolation_level(keyless.then_some(IsolationLevel::RepeatableRead)),
        )
        .map_err(mysql_error)?;
    let layout = load_layout(&mut transaction, object, &qualified)?;
    if let Some(error) = &layout.edit_error {
        return Err(error.clone());
    }

    // Validate and lock the complete selection before any DELETE can run.
    let mut deletions = Vec::with_capacity(groups.len());
    for DeleteRowGroup { original, count } in groups {
        let (sql, parameters) = if original.layout.is_keyless() {
            let (predicate, parameters, matches) =
                keyless::lock_matches(&mut transaction, &layout, &qualified, &original)?;
            if matches < count {
                return Err("原值相同的记录数少于已勾选行数，请刷新后重试。".into());
            }
            (
                format!("DELETE FROM {qualified} WHERE {predicate} LIMIT {count}"),
                parameters,
            )
        } else {
            let (predicate, parameters) =
                verify_original(&mut transaction, &layout, &qualified, &original)?;
            (
                format!("DELETE FROM {qualified} WHERE {predicate}"),
                parameters,
            )
        };
        deletions.push((sql, parameters, count));
    }
    let mut deleted = 0;
    for (sql, parameters, count) in deletions {
        transaction
            .exec_drop(sql, Params::Positional(parameters))
            .map_err(mysql_error)?;
        if transaction.affected_rows() != count {
            return Err("服务器返回的删除行数与勾选数量不一致，删除已取消。".into());
        }
        check_write_warnings(&mut transaction).map_err(|error| format!("删除已取消。{error}"))?;
        deleted += count;
    }
    transaction
        .commit()
        .map_err(|error| format!("无法确认删除是否已提交。{}", mysql_error(error)))?;
    Ok(deleted)
}

fn ensure_table(object: &DbObject) -> Result<(), String> {
    if object.kind != ObjectKind::Table {
        return Err("只有表支持分页数据编辑。".into());
    }
    Ok(())
}

pub(super) fn load_layout(
    transaction: &mut Transaction<'_>,
    object: DbObject,
    qualified: &str,
) -> Result<TableLayout, String> {
    load_tagged_layout(transaction, object, qualified, "")
}

pub(super) fn load_tagged_layout(
    transaction: &mut Transaction<'_>,
    object: DbObject,
    qualified: &str,
    tag: &str,
) -> Result<TableLayout, String> {
    try_load_tagged_layout(transaction, object, qualified, tag)?
        .ok_or_else(|| TABLE_LAYOUT_NOT_BASE.into())
}

pub(super) const TABLE_LAYOUT_NOT_BASE: &str = "只有数据表支持分页编辑。";

pub(super) fn try_load_tagged_layout(
    transaction: &mut Transaction<'_>,
    object: DbObject,
    qualified: &str,
    tag: &str,
) -> Result<Option<TableLayout>, String> {
    // Acquire a metadata lock before checking the engine, columns, and key.
    transaction
        .query_drop(format!("SELECT {tag}1 FROM {qualified} LIMIT 0"))
        .map_err(mysql_error)?;
    let tag_select = |mut sql: String| {
        sql.insert_str("SELECT ".len(), tag);
        sql
    };
    let args = params! { "schema" => object.schema.as_str(), "table" => object.name.as_str() };
    let table_sql = tag_select(table_metadata_sql());
    let (kind, transactions, engine) = transaction
        .exec_first::<(String, Option<String>, Option<String>), _, _>(table_sql, args.clone())
        .map_err(mysql_error)?
        .ok_or("找不到表的元数据。")?;
    if !is_base_table(&kind) {
        return Ok(None);
    }
    let columns = transaction
        .exec_opt::<ColumnMetadata, _, _>(tag_select(column_metadata_sql()), args.clone())
        .map_err(mysql_error)?;
    let keys = transaction
        .exec_opt::<IndexPart, _, _>(tag_select(unique_index_sql()), args)
        .map_err(mysql_error)?;
    if keys.len() > MAX_INDEX_PARTS {
        return Err("表的唯一索引字段超过可读取上限。".into());
    }
    build_layout(
        object,
        &kind,
        transactions.as_deref(),
        engine.as_deref(),
        columns
            .into_iter()
            .map(|row| row.map_err(|error| mysql_error(error.into()))),
        keys.into_iter()
            .map(|row| row.map_err(|error| mysql_error(error.into()))),
    )
    .map(Some)
}

fn table_metadata_sql() -> String {
    format!(
        "SELECT t.TABLE_TYPE, e.TRANSACTIONS, t.ENGINE
         FROM information_schema.TABLES t
         LEFT JOIN information_schema.ENGINES e ON t.ENGINE = e.ENGINE
         WHERE {MATCH_TABLE}"
    )
}

fn column_metadata_sql() -> String {
    format!(
        "SELECT t.COLUMN_NAME, t.DATA_TYPE, t.COLUMN_TYPE, t.IS_NULLABLE,
                t.COLUMN_DEFAULT, t.EXTRA, t.GENERATION_EXPRESSION,
                t.CHARACTER_SET_NAME, t.COLLATION_NAME, t.CHARACTER_OCTET_LENGTH
         FROM information_schema.COLUMNS t WHERE {MATCH_TABLE}
         ORDER BY t.ORDINAL_POSITION"
    )
}

fn unique_index_sql() -> String {
    format!(
        "SELECT t.INDEX_NAME, t.COLUMN_NAME, t.SEQ_IN_INDEX, t.SUB_PART
         FROM information_schema.STATISTICS t
         WHERE t.NON_UNIQUE = 0 AND {MATCH_TABLE}
         ORDER BY BINARY t.INDEX_NAME, t.SEQ_IN_INDEX"
    )
}

type IndexPart = (String, Option<String>, usize, Option<usize>);

type ColumnMetadata = (
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<usize>,
);

fn is_base_table(kind: &str) -> bool {
    matches!(kind, "BASE TABLE" | "SYSTEM VERSIONED")
}

fn build_layout(
    object: DbObject,
    kind: &str,
    transactions: Option<&str>,
    engine: Option<&str>,
    columns: impl Iterator<Item = Result<ColumnMetadata, String>>,
    parts: impl Iterator<Item = Result<IndexPart, String>>,
) -> Result<TableLayout, String> {
    if !is_base_table(kind) {
        return Err(TABLE_LAYOUT_NOT_BASE.into());
    }
    let columns: Vec<_> = columns
        .map(|row| {
            let (
                name,
                kind,
                data_type,
                nullable,
                default_value,
                extra,
                expression,
                charset,
                collation,
                declared_width,
            ) = row?;
            let generated = expression.as_deref().is_some_and(|value| !value.is_empty())
                || extra
                    .split_ascii_whitespace()
                    .any(|word| word.eq_ignore_ascii_case("GENERATED"));
            let value_type = ValueType::from_metadata(&kind, &data_type);
            Ok(TableColumn {
                name,
                data_type,
                nullable: nullable == "YES",
                default_value,
                writable: !generated && value_type != ValueType::Unsupported,
                auto_increment: extra.contains("auto_increment"),
                charset,
                collation,
                declared_width,
                generation_expression: expression,
                extra,
                value_type,
            })
        })
        .collect::<Result<_, String>>()?;
    if columns.is_empty() {
        return Err("未找到可读取的列。".into());
    }
    let (primary_key, edit_keys) = index_keys(&columns, parts)?;
    let edit_error = if transactions != Some("YES") {
        Some("此表的存储引擎不支持事务，仅供浏览。".into())
    } else if edit_keys.is_empty()
        && (kind != "BASE TABLE"
            || !engine.is_some_and(|engine| engine.eq_ignore_ascii_case("InnoDB"))
            || columns
                .iter()
                .any(|column| column.value_type == ValueType::Unsupported))
    {
        Some("此表缺少可用的唯一键，表引擎或列类型也不支持按整行原值定位，仅供浏览。".into())
    } else {
        None
    };
    Ok(TableLayout {
        object,
        columns,
        primary_key,
        edit_error,
        edit_keys,
        selected_key: 0,
    })
}

fn index_keys(
    columns: &[TableColumn],
    parts: impl Iterator<Item = Result<IndexPart, String>>,
) -> Result<(Vec<usize>, Vec<EditKey>), String> {
    let mut indexes = BTreeMap::<String, Vec<_>>::new();
    for part in parts {
        let (index, name, sequence, prefix) = part?;
        indexes
            .entry(index)
            .or_default()
            .push((name, sequence, prefix));
    }
    let mut primary_key = Vec::new();
    let mut edit_keys = Vec::new();
    for (index, parts) in indexes {
        let mut seen = HashSet::new();
        let positions = parts
            .iter()
            .enumerate()
            .map(|(position, (name, sequence, _))| {
                let name = name.as_deref()?;
                columns
                    .iter()
                    .position(|column| column.name == name)
                    .filter(|&slot| *sequence == position + 1 && seen.insert(slot))
            })
            .collect::<Option<Vec<_>>>();
        let Some(positions) = positions else { continue };
        let primary = index == "PRIMARY";
        if primary {
            primary_key.clone_from(&positions);
        }
        if positions.iter().any(|&slot| {
            columns[slot].nullable || columns[slot].value_type == ValueType::Unsupported
        }) || !primary && parts.iter().any(|(_, _, prefix)| prefix.is_some())
        {
            continue;
        }
        let key = EditKey {
            index,
            columns: positions,
        };
        if primary {
            edit_keys.insert(0, key);
        } else {
            edit_keys.push(key);
        }
    }
    Ok((primary_key, edit_keys))
}

fn verify_original(
    transaction: &mut Transaction<'_>,
    layout: &TableLayout,
    qualified: &str,
    original: &TableRow,
) -> Result<(String, Vec<Value>), String> {
    let edit_key = layout.recheck_key(original)?;
    let mut conditions = Vec::new();
    let mut key = Vec::new();
    for &index in &edit_key.columns {
        let column = &layout.columns[index];
        let mut value = original.original()[index].clone();
        let placeholder = match column.value_type {
            ValueType::Decimal { precision, scale } => {
                format!("CAST(? AS DECIMAL({precision}, {scale}))")
            }
            ValueType::Bit(_) => {
                value = Value::UInt(bit_number(&value).ok_or("无法读取用于定位的 BIT 值。")?);
                "CAST(? AS UNSIGNED)".into()
            }
            ValueType::Binary => "CAST(? AS BINARY)".into(),
            _ => "?".into(),
        };
        conditions.push(format!(
            "{} <=> {placeholder}",
            quote_identifier(&column.name)
        ));
        key.push(value);
    }
    let predicate = conditions.join(" AND ");
    // Read the locked row in the same binary protocol used to capture the snapshot.
    let mut current = transaction
        .exec::<Row, _, _>(
            format!(
                "SELECT {} FROM {qualified} WHERE {predicate} LIMIT 2 FOR UPDATE",
                layout.projection()
            ),
            Params::Positional(key.clone()),
        )
        .map_err(mysql_error)?;
    if current.len() != 1 || current.pop().map(Row::unwrap).as_deref() != Some(original.original())
    {
        return Err("此行已被其他操作修改或删除，请刷新后重试。".into());
    }
    Ok((predicate, key))
}

fn prepare_values(
    layout: &TableLayout,
    values: Vec<(String, FieldValue)>,
) -> Result<Vec<(String, Option<Value>)>, String> {
    let mut names = HashSet::new();
    let mut remaining = RESULT_MEMORY_LIMIT;
    values
        .into_iter()
        .map(|(name, input)| {
            let column = layout
                .columns
                .iter()
                .find(|column| column.name == name)
                .ok_or_else(|| format!("找不到列「{name}」。"))?;
            if !column.writable {
                return Err(format!("列「{name}」为只读。"));
            }
            if !names.insert(column.name.as_str()) {
                return Err(format!("列「{name}」有重复的输入。"));
            }
            let value = match input {
                FieldValue::Default => None,
                FieldValue::Null if column.nullable => Some(Value::NULL),
                FieldValue::Null => return Err(format!("列「{name}」不允许 NULL。")),
                FieldValue::Text(text) => {
                    remaining = remaining
                        .checked_sub(text.len())
                        .ok_or("本次编辑的数据超过 32 MiB。")?;
                    Some(column.parse_filter_value(text)?)
                }
                FieldValue::Binary(bytes) if column.is_binary() => {
                    remaining = remaining
                        .checked_sub(bytes.len())
                        .ok_or("本次编辑的数据超过 32 MiB。")?;
                    Some(Value::Bytes(bytes))
                }
                FieldValue::Binary(_) => return Err(format!("列「{name}」不接受二进制值。")),
            };
            Ok((quote_identifier(&name), value))
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ValueType {
    Signed(u32),
    Unsigned(u32),
    Float,
    Double,
    Decimal { precision: usize, scale: usize },
    Date,
    DateTime,
    Time,
    Year,
    Text,
    Binary,
    Bit(u32),
    Unsupported,
}

impl ValueType {
    fn from_metadata(kind: &str, declaration: &str) -> Self {
        let bits = match kind {
            "tinyint" => 8,
            "smallint" => 16,
            "mediumint" => 24,
            "int" | "integer" => 32,
            "bigint" => 64,
            _ => 0,
        };
        if bits != 0 {
            return if declaration.contains("unsigned") {
                Self::Unsigned(bits)
            } else {
                Self::Signed(bits)
            };
        }
        match kind {
            "float" => Self::Float,
            "double" | "real" => Self::Double,
            "decimal" | "numeric" => {
                let Some((precision, scale)) =
                    type_parameters(declaration).and_then(|parameters| {
                        let (precision, scale) =
                            parameters.split_once(',').unwrap_or((parameters, "0"));
                        Some((
                            precision.trim().parse::<usize>().ok()?,
                            scale.trim().parse::<usize>().ok()?,
                        ))
                    })
                else {
                    return Self::Unsupported;
                };
                if (1..=65).contains(&precision) && scale <= precision {
                    Self::Decimal { precision, scale }
                } else {
                    Self::Unsupported
                }
            }
            "date" => Self::Date,
            "datetime" | "timestamp" => Self::DateTime,
            "time" => Self::Time,
            "year" => Self::Year,
            "bit" => type_parameters(declaration)
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|bits| (1..=64).contains(bits))
                .map_or(Self::Unsupported, Self::Bit),
            "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" | "enum"
            | "set" | "json" => Self::Text,
            "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" => {
                Self::Binary
            }
            _ => Self::Unsupported,
        }
    }

    fn display(self, value: &Value, binary: bool) -> Cell {
        if matches!(self, Self::Bit(_)) {
            if let Some(number) = bit_number(value) {
                return Cell::Text(number.to_string());
            }
        }
        match value {
            Value::Date(..) | Value::Time(..) => {
                Cell::Text(value.as_sql(true).trim_matches('\'').to_owned())
            }
            value => cell(value.clone(), binary),
        }
    }

    fn input_format(self) -> &'static str {
        match self {
            Self::Signed(_) => "有效范围内的整数",
            Self::Unsigned(_) | Self::Bit(_) => "有效范围内的非负整数",
            Self::Float | Self::Double => "有效范围内的数值",
            Self::Decimal { .. } => "符合该列精度和小数位数的十进制数",
            Self::Date => "YYYY-MM-DD 格式的日期",
            Self::DateTime => "YYYY-MM-DD HH:MM:SS 格式的日期和时间",
            Self::Time => "HH:MM:SS 格式的时间",
            Self::Year => "有效的四位年份",
            Self::Binary => "十六进制数据（如 0x00FF）",
            Self::Text => "有效文本",
            Self::Unsupported => "数据库支持的类型",
        }
    }

    fn parse(self, text: String) -> Option<Value> {
        if self == Self::Text {
            return Some(Value::Bytes(text.into_bytes()));
        }
        let input = text.trim();
        match self {
            Self::Signed(bits) => {
                let magnitude = 1_i128 << (bits - 1);
                let value = input.parse::<i64>().ok()?;
                (-magnitude..magnitude)
                    .contains(&i128::from(value))
                    .then_some(Value::Int(value))
            }
            Self::Unsigned(bits) | Self::Bit(bits) => {
                let value = input.parse::<u64>().ok()?;
                (value <= u64::MAX >> (64 - bits)).then_some(Value::UInt(value))
            }
            Self::Float => input
                .parse::<f32>()
                .ok()
                .filter(|value| value.is_finite())
                .map(Value::Float),
            Self::Double => input
                .parse::<f64>()
                .ok()
                .filter(|value| value.is_finite())
                .map(Value::Double),
            Self::Decimal { precision, scale } => decimal_fits(input, precision, scale)
                .then(|| Value::Bytes(input.as_bytes().to_vec())),
            Self::Date | Self::DateTime => parse_date(input, self == Self::DateTime),
            Self::Time => {
                let hours_and_minutes = input.strip_prefix('-').unwrap_or(input);
                let (hours, minutes, seconds, micros) = parse_clock(hours_and_minutes)?;
                (hours <= 838).then_some(Value::Time(
                    input.starts_with('-'),
                    hours / 24,
                    (hours % 24) as u8,
                    minutes,
                    seconds,
                    micros,
                ))
            }
            Self::Year => {
                let year = input.parse::<u64>().ok()?;
                (year == 0 || (1901..=2155).contains(&year)).then_some(Value::UInt(year))
            }
            Self::Binary => parse_hex(input).map(Value::Bytes),
            Self::Text | Self::Unsupported => None,
        }
    }
}

fn type_parameters(declaration: &str) -> Option<&str> {
    let (_, parameters) = declaration.split_once('(')?;
    let (parameters, _) = parameters.split_once(')')?;
    Some(parameters)
}

fn bit_number(value: &Value) -> Option<u64> {
    match value {
        Value::Bytes(bytes) if bytes.len() <= size_of::<u64>() => Some(
            bytes
                .iter()
                .fold(0, |number, byte| (number << 8) | u64::from(*byte)),
        ),
        Value::UInt(number) => Some(*number),
        Value::Int(number) => u64::try_from(*number).ok(),
        _ => None,
    }
}

fn decimal_fits(input: &str, precision: usize, scale: usize) -> bool {
    let digits = input.strip_prefix(['-', '+']).unwrap_or(input);
    let (integer, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    (!integer.is_empty() || !fraction.is_empty())
        && integer
            .bytes()
            .chain(fraction.bytes())
            .all(|digit| digit.is_ascii_digit())
        && integer.trim_start_matches('0').len() <= precision - scale
        && fraction.trim_end_matches('0').len() <= scale
}

fn parse_hex(input: &str) -> Option<Vec<u8>> {
    let input = input
        .strip_prefix("0x")
        .or_else(|| input.strip_prefix("0X"))
        .unwrap_or(input);
    if !input.len().is_multiple_of(2) {
        return None;
    }
    input
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = char::from(pair[0]).to_digit(16)?;
            let low = char::from(pair[1]).to_digit(16)?;
            Some((high * 16 + low) as u8)
        })
        .collect()
}

pub(super) fn parse_temporal_value(kind: &str, text: &str) -> Option<Value> {
    match kind {
        "date" => ValueType::Date.parse(text.to_owned()),
        "datetime" | "timestamp" => ValueType::DateTime.parse(text.to_owned()),
        "time" => ValueType::Time.parse(text.to_owned()),
        _ => None,
    }
}

fn parse_date(input: &str, allow_time: bool) -> Option<Value> {
    let (date, time) = if let Some((date, time)) = input.split_once([' ', 'T']) {
        if !allow_time {
            return None;
        }
        (date, parse_clock(time)?)
    } else {
        (input, (0, 0, 0, 0))
    };
    let (hours, minutes, seconds, micros) = time;
    let mut parts = date.split('-');
    let year = parts.next()?.parse::<u16>().ok()?;
    let month = parts.next()?.parse::<u8>().ok()?;
    let day = parts.next()?.parse::<u8>().ok()?;
    if year > 9999 || hours > 23 || parts.next().is_some() {
        return None;
    }
    let leap_year =
        year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let month_days = match month {
        2 if leap_year => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => 0,
    };
    if (year, month, day) != (0, 0, 0) && !(1..=month_days).contains(&day) {
        return None;
    }
    Some(Value::Date(
        year,
        month,
        day,
        hours as u8,
        minutes,
        seconds,
        micros,
    ))
}

fn parse_clock(input: &str) -> Option<(u32, u8, u8, u32)> {
    let (clock, micros) = if let Some((clock, fraction)) = input.split_once('.') {
        if fraction.is_empty()
            || fraction.len() > 6
            || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let micros = fraction.parse::<u32>().ok()? * 10_u32.pow(6 - fraction.len() as u32);
        (clock, micros)
    } else {
        (input, 0)
    };
    let mut parts = clock.split(':');
    let hours = parts.next()?.parse::<u32>().ok()?;
    let minutes = parts.next()?.parse::<u8>().ok()?;
    let seconds = parts.next()?.parse::<u8>().ok()?;
    (minutes <= 59 && seconds <= 59 && parts.next().is_none())
        .then_some((hours, minutes, seconds, micros))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_draft_source_identity_and_renders_typed_update_preview() {
        let object = DbObject {
            schema: "stock".into(),
            name: "items".into(),
            kind: ObjectKind::Table,
        };
        let column = |name: &str, kind: &str, declaration: &str| {
            (
                name.into(),
                kind.into(),
                declaration.into(),
                "NO".into(),
                None,
                String::new(),
                None,
                None,
                None,
                None,
            )
        };
        let layout = |parts: Vec<IndexPart>| {
            build_layout(
                object.clone(),
                "BASE TABLE",
                Some("YES"),
                Some("InnoDB"),
                [
                    column("id", "int", "int"),
                    column("label", "varchar", "varchar(40)"),
                    column("payload", "varbinary", "varbinary(24)"),
                ]
                .into_iter()
                .map(Ok),
                parts.into_iter().map(Ok),
            )
            .unwrap()
        };
        let capture =
            |layout: Arc<TableLayout>, label: &str, fingerprint: Option<String>| TableRow {
                cells: Vec::new(),
                layout,
                original: vec![
                    Value::Int(7),
                    Value::Bytes(label.as_bytes().to_vec()),
                    Value::Bytes(vec![0, 255]),
                ]
                .into(),
                fingerprint,
            };
        let keys = vec![
            ("PRIMARY".into(), Some("id".into()), 1, None),
            ("label_unique".into(), Some("label".into()), 1, None),
        ];
        let mut projected = layout(keys.clone());
        projected.select_projected_key(&[1]).unwrap();
        let original = capture(Arc::new(projected), "original", None);
        let refreshed = capture(Arc::new(layout(keys)), "refreshed", None);
        assert!(original.same_source_record(&refreshed));
        let values = vec![
            ("label".into(), FieldValue::Text("O'Reilly".into())),
            ("payload".into(), FieldValue::Binary(vec![171, 0])),
        ];
        assert_eq!(
            preview_table_update(&TableEdit::Update {
                original,
                values: values.clone(),
            })
            .unwrap(),
            "UPDATE `stock`.`items`\nSET `label` = 'O''Reilly', `payload` = X'AB00'\nWHERE `label` <=> 'original'\nLIMIT 1;"
        );

        let keyless = Arc::new(layout(Vec::new()));
        let fingerprint = Some("37/6F726967696E616C/00FF".to_owned());
        let original = capture(Arc::clone(&keyless), "original", fingerprint.clone());
        let refreshed = capture(keyless, "original", fingerprint);
        assert!(original.same_source_record(&refreshed));
        let preview = preview_table_update(&TableEdit::Update { original, values }).unwrap();
        assert!(preview.contains("WHERE BINARY CONCAT_WS('/', "));
        assert!(preview.ends_with("<=> CAST(? AS BINARY)\nLIMIT 1;"));
    }

    #[test]
    fn converts_editor_values_to_mysql_protocol_types() {
        assert_eq!(
            ValueType::from_metadata("bigint", "bigint unsigned").parse(u64::MAX.to_string()),
            Some(Value::UInt(u64::MAX))
        );
        assert_eq!(
            ValueType::DateTime.parse("2024-02-29 12:34:56.123".into()),
            Some(Value::Date(2024, 2, 29, 12, 34, 56, 123_000))
        );
        assert_eq!(
            ValueType::Time.parse("-25:00:01".into()),
            Some(Value::Time(true, 1, 1, 0, 1, 0))
        );
        assert_eq!(
            ValueType::Binary.parse("0x00FF".into()),
            Some(Value::Bytes(vec![0, 255]))
        );
        assert_eq!(
            ValueType::from_metadata("decimal", "decimal(20,4)")
                .parse("12345678901234.5678".into()),
            Some(Value::Bytes(b"12345678901234.5678".to_vec()))
        );
    }

    #[test]
    #[ignore = "requires a disposable MariaDB on loopback via HARBOR_EDITABLE_FIXTURE_PORT"]
    fn filters_before_paging_and_breaks_sort_ties_with_the_complete_primary_key() {
        let port: u16 = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let opts: Opts = mysql::OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(port)
            .user(Some("root"))
            .prefer_socket(false)
            .into();
        let mut conn = Conn::new(opts.clone()).unwrap();
        let schema = format!(
            "harbor_rows_{:x}_{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        );
        conn.query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
            .unwrap();
        conn.select_db(&schema).unwrap();
        conn.query_drop(
            "CREATE TABLE sample (
                region INT, id INT, score INT, label VARCHAR(30), memo VARCHAR(40),
                price DECIMAL(24,4), payload VARBINARY(10), PRIMARY KEY(region,id)
             ) ENGINE=InnoDB",
        )
        .unwrap();
        conn.query_drop(
            "INSERT INTO sample VALUES
                (2,1,10,X'4125215F5C',NULL,9007199254740992.0001,X'FF255F2100'),
                (1,3,5, X'4125215F5C',NULL,9007199254740992.0001,X'FF255F2100'),
                (1,2,10,X'4125215F5C',NULL,9007199254740992.0001,X'FF255F2100'),
                (1,1,10,X'4125215F5C',NULL,9007199254740992.0001,X'FF255F2100'),
                (3,1,90,X'41215F5C',  NULL,9007199254740992.0001,X'FF255F2100'),
                (4,1,90,X'4125215F5C','occupied',9007199254740992.0001,X'FF255F2100'),
                (5,1,90,X'4125215F5C',NULL,9007199254740992.0000,X'FF255F2100'),
                (6,1,90,X'4125215F5C',NULL,9007199254740992.0001,X'FF25882100')",
        )
        .unwrap();
        let object = DbObject {
            schema: schema.clone(),
            name: "sample".into(),
            kind: ObjectKind::Table,
        };
        let selection = TableSelection {
            predicates: vec![
                TablePredicate {
                    column: "label".into(),
                    operator: TablePredicateOp::Contains,
                    value: Value::Bytes(b"%!_\\".to_vec()),
                },
                TablePredicate {
                    column: "memo".into(),
                    operator: TablePredicateOp::Equal,
                    value: Value::NULL,
                },
                TablePredicate {
                    column: "price".into(),
                    operator: TablePredicateOp::Greater,
                    value: Value::Bytes(b"9007199254740992.0000".to_vec()),
                },
                TablePredicate {
                    column: "payload".into(),
                    operator: TablePredicateOp::StartsWith,
                    value: Value::Bytes(vec![0xff, b'%', b'_', b'!']),
                },
            ],
            order: vec![TableSort {
                column: "score".into(),
                descending: true,
            }],
        };
        let first = table_page_selected(&opts, object.clone(), 2, 0, &selection);
        let second = table_page_selected(&opts, object, 2, 2, &selection);
        conn.query_drop(format!("DROP DATABASE {schema}")).unwrap();
        let first = first.unwrap();
        let second = second.unwrap();
        let keys = |page: &TablePage| {
            page.rows
                .iter()
                .map(|row| row.original()[..2].to_vec())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            keys(&first),
            [
                vec![Value::Int(1), Value::Int(1)],
                vec![Value::Int(1), Value::Int(2)]
            ]
        );
        assert_eq!(
            keys(&second),
            [
                vec![Value::Int(2), Value::Int(1)],
                vec![Value::Int(1), Value::Int(3)]
            ]
        );
        assert!(first.has_more);
        assert!(!second.has_more);
    }
}
