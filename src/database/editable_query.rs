use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Mutex;
use std::time::Instant;

use mysql::prelude::Queryable;
use mysql::{AccessMode, Column, Conn, Opts, Transaction, TxOpts};

use super::http_session::Session as HttpSession;
use super::row_editor::{
    MAX_PAGE_ROWS, TABLE_LAYOUT_NOT_BASE, TableLayout, fingerprint_column,
    http_bounded_select_timed, http_layout, http_page_from_raw, native_fingerprint_column,
    page_from_rows, try_load_tagged_layout,
};
use super::{
    DbObject, ObjectKind, OperationMonitor, QueryTiming, RegisteredTableRead, TablePage,
    column_is_binary, mysql_error, quote_identifier,
};

pub(super) enum EditableRead<T> {
    Editable(T),
    // The caller may execute the user's original SQL. No result query has been executed.
    ReadOnly(String),
}

impl<T> EditableRead<T> {
    pub(super) fn require_editable(self) -> Result<T, String> {
        match self {
            Self::Editable(page) => Ok(page),
            Self::ReadOnly(reason) => Err(reason),
        }
    }
}

pub(super) fn query_editable(
    opts: &Opts,
    sql: &str,
    limit: usize,
    monitor: Option<&Mutex<OperationMonitor>>,
) -> Result<TablePage, String> {
    try_query_editable(opts, sql, limit, monitor)?.require_editable()
}

pub(super) fn try_query_editable(
    opts: &Opts,
    sql: &str,
    limit: usize,
    monitor: Option<&Mutex<OperationMonitor>>,
) -> Result<EditableRead<TablePage>, String> {
    read_native(opts, monitor, |transaction, reader| {
        read_editable(transaction, sql, limit, reader)
    })
}

pub(super) struct NativeRead<'a> {
    pub(super) tag: &'a str,
    monitor: Option<&'a Mutex<OperationMonitor>>,
}

impl NativeRead<'_> {
    pub(super) fn check(&self) -> Result<(), String> {
        self.monitor
            .map_or(Ok(()), |monitor| monitor.lock().unwrap().check_cancelled())
    }
}

pub(super) fn read_native<T>(
    opts: &Opts,
    monitor: Option<&Mutex<OperationMonitor>>,
    read: impl FnOnce(&mut Transaction<'_>, &NativeRead<'_>) -> Result<T, String>,
) -> Result<T, String> {
    if let Some(monitor) = monitor {
        monitor.lock().unwrap().check_cancelled()?;
    }
    let mut connection = Conn::new(opts.clone()).map_err(mysql_error)?;
    let registration = monitor
        .map(|monitor| RegisteredTableRead::new(monitor, &connection, opts))
        .transpose()?;
    let reader = NativeRead {
        tag: registration.as_ref().map_or("", RegisteredTableRead::tag),
        monitor,
    };
    reader.check()?;
    let mut transaction = connection
        .start_transaction(TxOpts::default().set_access_mode(Some(AccessMode::ReadOnly)))
        .map_err(|error| format!("无法开始只读查询事务。{}", mysql_error(error)))?;
    let result = read(&mut transaction, &reader)?;
    reader.check()?;
    Ok(result)
}

pub(super) fn read_query_context(
    transaction: &mut Transaction<'_>,
    tag: &str,
) -> Result<(Option<String>, u8, String, String), String> {
    transaction
        .query_first_opt(format!(
            "SELECT {tag}DATABASE(), @@lower_case_table_names, @@SESSION.sql_mode,
                        @@SESSION.character_set_client"
        ))
        .map_err(mysql_error)?
        .transpose()
        .map_err(|error| mysql_error(error.into()))?
        .ok_or_else(|| "服务器未返回当前数据库、SQL 模式和字符集。".into())
}

pub(super) const EDITABLE_UTF8_REQUIRED: &str =
    "执行并编辑含非 ASCII 字符的查询需要 UTF-8 会话字符集。";

pub(super) fn supports_editable_charset(sql: &str, charset: &str) -> bool {
    sql.is_ascii() || matches!(charset, "utf8" | "utf8mb3" | "utf8mb4")
}

pub(super) fn failed_editable_prepare<T>(
    error: mysql::Error,
    adds_hidden_columns: bool,
) -> Result<EditableRead<T>, String> {
    let incompatible = matches!(&error, mysql::Error::MySqlError(error) if
        // A temporary table may be available only in the editor connection.
        matches!(error.code, 1146 | 1295)
            // A rewritten projection can need additional data the original query did not read.
            || adds_hidden_columns && matches!(error.code, 1054 | 1142 | 1143));
    let reason = mysql_error(error);
    if incompatible {
        Ok(EditableRead::ReadOnly(reason))
    } else {
        Err(reason)
    }
}

fn read_editable(
    transaction: &mut Transaction<'_>,
    sql: &str,
    limit: usize,
    reader: &NativeRead<'_>,
) -> Result<EditableRead<TablePage>, String> {
    reader.check()?;
    let tag = reader.tag;
    let (database, lower_case_tables, sql_mode, charset) = read_query_context(transaction, tag)?;
    if !supports_editable_charset(sql, &charset) {
        return Ok(EditableRead::ReadOnly(EDITABLE_UTF8_REQUIRED.into()));
    }
    let limit = limit.clamp(1, MAX_PAGE_ROWS);
    let query = match Select::parse(sql, Quoting::from_sql_mode(&sql_mode)) {
        Ok(query) => query,
        Err(reason) => return Ok(EditableRead::ReadOnly(reason)),
    };
    let Some(schema) = query.schema.as_deref().or(database.as_deref()) else {
        return Ok(EditableRead::ReadOnly(
            "请选定默认数据库，或在查询中明确指定库名。".into(),
        ));
    };
    let bounded = query.bounded_sql(limit + 1);
    reader.check()?;
    let statement = match transaction.prep(format!("{tag}{bounded}")) {
        Ok(statement) => statement,
        Err(error) => return failed_editable_prepare(error, false),
    };
    let prepared_columns = statement.columns();
    let Some(first) = prepared_columns.first() else {
        return Ok(EditableRead::ReadOnly(unproven_columns()));
    };
    let (Ok(actual_schema), Ok(actual_table)) = (
        std::str::from_utf8(first.schema_ref()),
        std::str::from_utf8(first.org_table_ref()),
    ) else {
        return Ok(EditableRead::ReadOnly(unproven_columns()));
    };
    if actual_schema.is_empty()
        || actual_table.is_empty()
        || !same_source_name(schema, actual_schema, lower_case_tables)
        || !same_source_name(&query.table, actual_table, lower_case_tables)
    {
        return Ok(EditableRead::ReadOnly(unproven_columns()));
    }
    let object = DbObject {
        schema: actual_schema.to_owned(),
        name: actual_table.to_owned(),
        kind: ObjectKind::Table,
    };
    reader.check()?;
    let Some(mut layout) =
        try_load_tagged_layout(transaction, object.clone(), &object.qualified_name(), tag)?
    else {
        return Ok(EditableRead::ReadOnly(TABLE_LAYOUT_NOT_BASE.into()));
    };
    if let Some(error) = &layout.edit_error {
        return Ok(EditableRead::ReadOnly(error.clone()));
    }
    let projection = match table_projection(&layout, &object, &prepared_columns) {
        Ok(projection) => projection,
        Err(reason) => return Ok(EditableRead::ReadOnly(reason)),
    };
    if let Err(reason) = layout.select_projected_key(&projection) {
        return Ok(EditableRead::ReadOnly(reason));
    }
    let (full_statement, fingerprint) =
        if projection.len() == layout.columns.len() && !layout.is_keyless() {
            (statement, None)
        } else {
            let (full_sql, fingerprint) =
                query.include_missing_columns(&bounded, &layout, &projection, &prepared_columns);
            reader.check()?;
            let statement = match transaction.prep(format!("{tag}{full_sql}")) {
                Ok(statement) => statement,
                Err(error) => return failed_editable_prepare(error, true),
            };
            (statement, fingerprint)
        };
    let order = match editable_columns(full_statement.columns().as_ref(), fingerprint.as_deref())
        .and_then(|columns| table_order(&layout, &object, columns))
    {
        Ok(order) => order,
        Err(reason) => return Ok(EditableRead::ReadOnly(reason)),
    };

    reader.check()?;
    let started = Instant::now();
    let mut result = transaction
        .exec_iter(&full_statement, ())
        .map_err(mysql_error)?;
    let execution = started.elapsed();
    let result_columns = result.columns();
    let result_columns = editable_columns(result_columns.as_ref(), fingerprint.as_deref())?;
    if table_order(&layout, &object, result_columns)? != order {
        return Err("查询执行时的列结构已变化，请重新执行。".into());
    }
    let binary: Vec<_> = order
        .iter()
        .map(|&index| column_is_binary(&result_columns[index]))
        .collect();
    let labels: Vec<String> = prepared_columns
        .iter()
        .map(|column| column.name_str().into_owned())
        .collect();
    if result_columns
        .iter()
        .take(projection.len())
        .zip(prepared_columns.iter())
        .any(|(actual, expected)| {
            actual.org_name_ref() != expected.org_name_ref()
                || actual.name_ref() != expected.name_ref()
        })
    {
        return Err("查询执行时的列结构已变化，请重新执行。".into());
    }
    let mut page = page_from_rows(
        layout,
        &binary,
        Some(&order),
        result.iter().into_iter().flatten(),
        fingerprint.is_some(),
        limit,
        0,
    )?;
    drop(result);
    page.query_timing = Some(QueryTiming {
        execution: Some(execution),
        elapsed: started.elapsed(),
    });
    Ok(EditableRead::Editable(project_page(
        page,
        &projection,
        labels,
    )))
}

fn editable_columns<'a>(
    columns: &'a [Column],
    fingerprint: Option<&str>,
) -> Result<&'a [Column], String> {
    if let Some(expected) = fingerprint {
        let (marker, columns) = columns.split_last().ok_or_else(unproven_columns)?;
        if marker.name_ref() != expected.as_bytes() {
            return Err("查询未返回可验证的原始行标记，请重新执行。".into());
        }
        Ok(columns)
    } else {
        Ok(columns)
    }
}

pub(super) fn query_editable_http(
    session: &HttpSession,
    sql: &str,
    limit: usize,
) -> Result<TablePage, String> {
    try_query_editable_http(session, sql, limit)?.require_editable()
}

pub(super) fn try_query_editable_http(
    session: &HttpSession,
    sql: &str,
    limit: usize,
) -> Result<EditableRead<TablePage>, String> {
    let context = session.snapshot_context()?;
    let query = match Select::parse(sql, Quoting::from_sql_mode(&context.sql_mode)) {
        Ok(query) => query,
        Err(reason) => return Ok(EditableRead::ReadOnly(reason)),
    };
    let Some(schema) = query.schema.as_deref().or(session.database_name()) else {
        return Ok(EditableRead::ReadOnly(
            "请选定默认数据库，或在查询中明确指定库名。".into(),
        ));
    };
    let object = DbObject {
        schema: schema.to_owned(),
        name: query.table.clone(),
        kind: ObjectKind::Table,
    };
    let mut layout = match http_layout(session, object) {
        Ok(layout) => layout,
        Err(reason) => return Ok(EditableRead::ReadOnly(reason)),
    };
    if let Some(error) = &layout.edit_error {
        return Ok(EditableRead::ReadOnly(error.clone()));
    }
    let mut projection = Vec::new();
    let mut labels = Vec::new();
    let mut seen = HashSet::new();
    for (name, alias) in &query.projection {
        if let Some(name) = name {
            let Some(index) = layout
                .columns
                .iter()
                .position(|column| column.name.eq_ignore_ascii_case(name))
            else {
                return Ok(EditableRead::ReadOnly(unproven_columns()));
            };
            if !seen.insert(index) {
                return Ok(EditableRead::ReadOnly(unproven_columns()));
            }
            projection.push(index);
            labels.push(
                alias
                    .as_deref()
                    .unwrap_or(&layout.columns[index].name)
                    .to_owned(),
            );
        } else {
            for (index, column) in layout
                .columns
                .iter()
                .enumerate()
                .filter(|(_, column)| column.visible_by_default())
            {
                if !seen.insert(index) {
                    return Ok(EditableRead::ReadOnly(unproven_columns()));
                }
                projection.push(index);
                labels.push(column.name.clone());
            }
        }
    }
    if let Err(reason) = layout.select_projected_key(&projection) {
        return Ok(EditableRead::ReadOnly(reason));
    }
    let table_alias = quote_identifier(query.alias.as_deref().unwrap_or(&query.table));
    let mut names: HashSet<_> = labels
        .iter()
        .map(|label| label.to_lowercase())
        .chain(
            layout
                .columns
                .iter()
                .map(|column| column.name.to_lowercase()),
        )
        .collect();
    let mut actual_order = projection.clone();
    let mut actual_labels = labels.clone();
    let mut additional = Vec::new();
    for (index, column) in layout
        .columns
        .iter()
        .enumerate()
        .filter(|(index, _)| !seen.contains(index))
    {
        let alias = free_alias(&mut names, format!("__harbor_original_{index}"));
        additional.push(format!(
            "{table_alias}.{} AS {}",
            quote_identifier(&column.name),
            quote_identifier(&alias)
        ));
        actual_order.push(index);
        actual_labels.push(alias);
    }
    let guard_alias = free_alias(&mut names, "__harbor_row_fingerprint".into());
    additional.push(fingerprint_column(
        &layout,
        Some(query.alias.as_deref().unwrap_or(&query.table)),
        &guard_alias,
    ));
    actual_labels.push(guard_alias);
    let limit = limit.clamp(1, MAX_PAGE_ROWS);
    let additional = additional.join(", ");
    let database = if query.schema.is_some() { "" } else { schema };
    // After this request, errors can only be reported, since the SQL has been sent.
    let (set, limit, elapsed) = http_bounded_select_timed(session, database, limit, |request| {
        let bounded = query.bounded_sql(request);
        format!(
            "{}, {additional} {}",
            &bounded[..query.projection_end],
            &bounded[query.projection_end..]
        )
    })?;
    if set.columns.len() != actual_labels.len()
        || set
            .columns
            .iter()
            .zip(&actual_labels)
            .any(|(actual, expected)| !actual.name.eq_ignore_ascii_case(expected))
    {
        return Err("查询执行时的列结构已变化，请重新执行。".into());
    }
    let mut positions = vec![0; layout.columns.len()];
    for (visible, index) in actual_order.into_iter().enumerate() {
        positions[index] = visible;
    }
    let labels = set
        .columns
        .iter()
        .take(projection.len())
        .map(|column| column.name.clone())
        .collect();
    let mut page = http_page_from_raw(layout, set, &positions, true, limit, 0)?;
    page.query_timing = Some(QueryTiming {
        execution: None,
        elapsed,
    });
    Ok(EditableRead::Editable(project_page(
        page,
        &projection,
        labels,
    )))
}

fn free_alias(reserved: &mut HashSet<String>, mut alias: String) -> String {
    while !reserved.insert(alias.clone()) {
        alias.push('_');
    }
    alias
}

pub(super) fn project_page(
    mut page: TablePage,
    projection: &[usize],
    labels: Vec<String>,
) -> TablePage {
    page.can_insert = projection.len() == page.columns.len();
    let reordered = !page.can_insert || projection.iter().copied().ne(0..projection.len());
    let visible_keys = |key: &[usize]| {
        key.iter()
            .filter_map(|key| projection.iter().position(|index| index == key))
            .collect()
    };
    page.primary_key = visible_keys(&page.primary_key);
    page.row_key = visible_keys(&page.row_key);
    page.columns = projection
        .iter()
        .map(|&index| page.columns[index].clone())
        .collect();
    page.column_labels = labels;
    if reordered {
        for row in &mut page.rows {
            row.cells = projection
                .iter()
                .map(|&index| std::mem::replace(&mut row.cells[index], super::Cell::Null))
                .collect();
        }
    }
    page
}

fn same_source_name(expected: &str, actual: &str, lower_case_tables: u8) -> bool {
    expected == actual || lower_case_tables != 0 && expected.to_lowercase() == actual.to_lowercase()
}

fn table_order(
    layout: &TableLayout,
    object: &DbObject,
    columns: &[Column],
) -> Result<Vec<usize>, String> {
    if columns.len() != layout.columns.len() {
        return Err(unproven_columns());
    }
    let mut order = vec![0; columns.len()];
    for (visible, index) in table_projection(layout, object, columns)?
        .into_iter()
        .enumerate()
    {
        order[index] = visible;
    }
    Ok(order)
}

fn table_projection(
    layout: &TableLayout,
    object: &DbObject,
    columns: &[Column],
) -> Result<Vec<usize>, String> {
    let by_name: HashMap<_, _> = layout
        .columns
        .iter()
        .enumerate()
        .map(|(index, field)| (field.name.as_bytes(), index))
        .collect();
    let mut seen = HashSet::new();
    columns
        .iter()
        .map(|column| {
            if column.schema_ref() != object.schema.as_bytes()
                || column.org_table_ref() != object.name.as_bytes()
            {
                return Err(unproven_columns());
            }
            let index = *by_name
                .get(column.org_name_ref())
                .ok_or_else(unproven_columns)?;
            if !seen.insert(index) {
                return Err(unproven_columns());
            }
            Ok(index)
        })
        .collect()
}

fn unproven_columns() -> String {
    "结果需要来自单张基础表，每个返回列须能对应到唯一的源字段。".into()
}

fn unsupported_query() -> String {
    "执行并编辑结果支持单表 SELECT、普通 WHERE、按列排序和 LIMIT。连接、子查询和函数请使用普通执行。".into()
}

pub(super) const EDITABLE_QUOTE_MODES: [&str; 4] = [
    "",
    "ANSI_QUOTES",
    "NO_BACKSLASH_ESCAPES",
    "ANSI_QUOTES,NO_BACKSLASH_ESCAPES",
];

pub(super) fn can_plan_single(sql: &str) -> bool {
    EDITABLE_QUOTE_MODES
        .into_iter()
        .any(|mode| Select::parse(sql, Quoting::from_sql_mode(mode)).is_ok())
}

pub(super) fn editable_source(sql: &str, mode: &str) -> Option<(Option<String>, String)> {
    let select = Select::parse(sql, Quoting::from_sql_mode(mode)).ok()?;
    Some((select.schema, select.table))
}

#[derive(PartialEq, Eq)]
pub(crate) struct DuplicateSelect {
    pub(crate) schema: Option<String>,
    pub(crate) table: String,
    pub(crate) alias: Option<String>,
    pub(crate) projection: Vec<(Option<String>, Option<String>)>,
}

pub(crate) fn duplicate_select(sql: &str) -> Option<DuplicateSelect> {
    let parse = |sql_mode| {
        let query = Select::parse(sql, Quoting::from_sql_mode(sql_mode)).ok()?;
        Some(DuplicateSelect {
            schema: query.schema,
            table: query.table,
            alias: query.alias,
            projection: query.projection,
        })
    };
    let parsed = parse("")?;
    // Do not infer different source relations from text that is ambiguous
    // under the server's quoting modes.
    EDITABLE_QUOTE_MODES
        .into_iter()
        .skip(1)
        .all(|mode| parse(mode).as_ref() == Some(&parsed))
        .then_some(parsed)
}

struct Select<'a> {
    source: &'a str,
    schema: Option<String>,
    table: String,
    alias: Option<String>,
    projection_end: usize,
    projection: Vec<(Option<String>, Option<String>)>,
    limit: Option<(Range<usize>, u64)>,
}

impl<'a> Select<'a> {
    fn parse(sql: &'a str, quoting: Quoting) -> Result<Self, String> {
        let tokens = lex(sql, quoting)?;
        let mut parser = Parser { remaining: &tokens };
        parser.expect_word("SELECT")?;
        let mut projection = Vec::new();
        loop {
            let column = parser.column(true)?;
            let label = if parser.take_word("AS") {
                Some(parser.name()?)
            } else {
                None
            };
            projection.push((column, label));
            if !parser.take_symbol(b',') {
                break;
            }
        }
        let projection_end = parser
            .remaining
            .first()
            .ok_or_else(unsupported_query)?
            .start;
        parser.expect_word("FROM")?;
        let name = parser.name()?;
        let (schema, table) = if parser.take_symbol(b'.') {
            (Some(name), parser.name()?)
        } else {
            (None, name)
        };
        let alias = if parser.take_word("AS") || parser.starts_alias() {
            Some(parser.name()?)
        } else {
            None
        };
        if parser.take_word("WHERE") {
            parser.predicate()?;
        }
        if parser.take_word("ORDER") {
            parser.expect_word("BY")?;
            loop {
                parser.column(false)?;
                if !parser.take_word("ASC") {
                    parser.take_word("DESC");
                }
                if !parser.take_symbol(b',') {
                    break;
                }
            }
        }
        let limit = if parser.take_word("LIMIT") {
            let first = parser.integer()?;
            Some(if parser.take_symbol(b',') {
                parser.integer()?
            } else {
                if parser.take_word("OFFSET") {
                    parser.integer()?;
                }
                first
            })
        } else {
            None
        };
        let end_index = tokens.len() - parser.remaining.len() - 1;
        parser.take_symbol(b';');
        if !parser.remaining.is_empty() {
            return Err(unsupported_query());
        }
        Ok(Self {
            source: &sql[..tokens[end_index].end],
            schema,
            table,
            alias,
            projection_end,
            projection,
            limit,
        })
    }

    fn include_missing_columns(
        &self,
        bounded: &str,
        layout: &TableLayout,
        projection: &[usize],
        columns: &[Column],
    ) -> (String, Option<String>) {
        let source = self.alias.as_deref().unwrap_or(&self.table);
        let table = quote_identifier(source);
        let mut names: HashSet<String> = columns
            .iter()
            .map(|column| column.name_str().to_lowercase())
            .chain(
                layout
                    .columns
                    .iter()
                    .map(|column| column.name.to_lowercase()),
            )
            .collect();
        let mut missing = layout
            .columns
            .iter()
            .enumerate()
            .filter(|(index, _)| !projection.contains(index))
            .map(|(index, column)| {
                let alias = free_alias(&mut names, format!("__harbor_original_{index}"));
                format!("{table}.{} AS {alias}", quote_identifier(&column.name))
            })
            .collect::<Vec<_>>();
        let fingerprint = layout.is_keyless().then(|| {
            let alias = free_alias(&mut names, "__harbor_row_fingerprint".into());
            missing.push(native_fingerprint_column(layout, Some(source), &alias));
            alias
        });
        (
            format!(
                "{}, {} {}",
                &bounded[..self.projection_end],
                missing.join(", "),
                &bounded[self.projection_end..]
            ),
            fingerprint,
        )
    }

    fn bounded_sql(&self, max_rows: usize) -> String {
        match &self.limit {
            Some((range, count)) if *count > max_rows as u64 => {
                format!(
                    "{}{max_rows}{}",
                    &self.source[..range.start],
                    &self.source[range.end..]
                )
            }
            Some(_) => self.source.to_owned(),
            None => format!("{} LIMIT {max_rows}", self.source),
        }
    }
}

pub(super) struct Parser<'a> {
    pub(super) remaining: &'a [Token<'a>],
}

impl Parser<'_> {
    pub(super) fn take_word(&mut self, word: &str) -> bool {
        if self.remaining.first().is_some_and(|token| token.word(word)) {
            self.remaining = &self.remaining[1..];
            true
        } else {
            false
        }
    }

    pub(super) fn expect_word(&mut self, word: &str) -> Result<(), String> {
        self.take_word(word)
            .then_some(())
            .ok_or_else(unsupported_query)
    }

    pub(super) fn take_symbol(&mut self, symbol: u8) -> bool {
        if self
            .remaining
            .first()
            .is_some_and(|token| token.kind == Kind::Symbol(symbol))
        {
            self.remaining = &self.remaining[1..];
            true
        } else {
            false
        }
    }

    pub(super) fn name(&mut self) -> Result<String, String> {
        let Some((token, remaining)) = self.remaining.split_first() else {
            return Err(unsupported_query());
        };
        let name = match token.kind {
            Kind::Word => token.text.to_owned(),
            Kind::Identifier(quote) => {
                let body = &token.text[1..token.text.len() - 1];
                if quote == b'`' {
                    body.replace("``", "`")
                } else {
                    body.replace("\"\"", "\"")
                }
            }
            _ => return Err(unsupported_query()),
        };
        self.remaining = remaining;
        Ok(name)
    }

    fn column(&mut self, wildcard: bool) -> Result<Option<String>, String> {
        for part in 0..3 {
            if wildcard && self.take_symbol(b'*') {
                return Ok(None);
            }
            let name = self.name()?;
            if part == 2 || !self.take_symbol(b'.') {
                return Ok(Some(name));
            }
        }
        Err(unsupported_query())
    }

    fn starts_alias(&self) -> bool {
        self.remaining.first().is_some_and(|token| {
            token.is_name()
                && !["WHERE", "ORDER", "LIMIT"]
                    .iter()
                    .any(|word| token.word(word))
        })
    }

    pub(super) fn integer(&mut self) -> Result<(Range<usize>, u64), String> {
        let error = || "可编辑查询的 LIMIT 和 OFFSET 需要非负整数。".to_owned();
        let (token, remaining) = self.remaining.split_first().ok_or_else(error)?;
        if token.kind != Kind::Word || !token.text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(error());
        }
        let count = token.text.parse().map_err(|_| error())?;
        self.remaining = remaining;
        Ok((token.start..token.end, count))
    }

    pub(super) fn predicate(&mut self) -> Result<(), String> {
        let mut depth = 0_usize;
        let mut previous: Option<&Token<'_>> = None;
        while let Some((token, remaining)) = self.remaining.split_first() {
            if depth == 0
                && (token.word("ORDER") || token.word("LIMIT") || token.kind == Kind::Symbol(b';'))
            {
                break;
            }
            if [
                "INTO",
                "FOR",
                "LOCK",
                "SELECT",
                "UNION",
                "JOIN",
                "INTERSECT",
                "EXCEPT",
                "PROCEDURE",
                "NEXTVAL",
            ]
            .iter()
            .any(|word| token.word(word))
            {
                return Err(unsupported_query());
            }
            match token.kind {
                Kind::Symbol(b'(') => {
                    // A SQL function can execute server-side code even in a SELECT.
                    if previous.is_some_and(|token| {
                        token.is_name()
                            && ![
                                "IN", "AND", "OR", "NOT", "XOR", "BETWEEN", "IS", "LIKE", "REGEXP",
                                "RLIKE",
                            ]
                            .iter()
                            .any(|word| token.word(word))
                    }) {
                        return Err(unsupported_query());
                    }
                    depth += 1;
                }
                Kind::Symbol(b')') => depth = depth.checked_sub(1).ok_or_else(unsupported_query)?,
                Kind::Symbol(b',') if depth != 0 => {}
                Kind::Symbol(
                    b'.' | b'+' | b'-' | b'*' | b'/' | b'%' | b'=' | b'<' | b'>' | b'!' | b'&'
                    | b'|' | b'^' | b'~',
                ) => {}
                Kind::Word | Kind::Identifier(_) | Kind::Literal => {}
                _ => return Err(unsupported_query()),
            }
            self.remaining = remaining;
            previous = Some(token);
        }
        if depth == 0 && previous.is_some() {
            Ok(())
        } else {
            Err(unsupported_query())
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Word,
    Identifier(u8),
    Literal,
    Symbol(u8),
}

pub(super) struct Token<'a> {
    pub(super) kind: Kind,
    pub(super) text: &'a str,
    pub(super) start: usize,
    pub(super) end: usize,
}

impl Token<'_> {
    pub(super) fn word(&self, word: &str) -> bool {
        self.kind == Kind::Word && self.text.eq_ignore_ascii_case(word)
    }

    pub(super) fn is_name(&self) -> bool {
        matches!(self.kind, Kind::Word | Kind::Identifier(_))
    }
}

#[derive(Clone, Copy)]
pub(super) struct Quoting {
    backslash_escapes: bool,
    ansi_quotes: bool,
}

impl Quoting {
    pub(super) fn from_sql_mode(sql_mode: &str) -> Self {
        let has_mode = |name| {
            sql_mode
                .split(',')
                .any(|mode| mode.trim().eq_ignore_ascii_case(name))
        };
        Self {
            backslash_escapes: !has_mode("NO_BACKSLASH_ESCAPES"),
            ansi_quotes: has_mode("ANSI_QUOTES"),
        }
    }
}

pub(super) fn lex(sql: &str, quoting: Quoting) -> Result<Vec<Token<'_>>, String> {
    let mut tokens = Vec::new();
    let mut index = 0;
    let bytes = sql.as_bytes();
    while let Some(&byte) = bytes.get(index) {
        let start = index;
        let remaining = &bytes[index..];
        let kind = match byte {
            byte if byte.is_ascii_whitespace() => {
                index += 1;
                continue;
            }
            b'#' => {
                index += line_width(remaining);
                continue;
            }
            b'-' if remaining.starts_with(b"--")
                && remaining
                    .get(2)
                    .is_none_or(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control()) =>
            {
                index += line_width(remaining);
                continue;
            }
            b'/' if remaining.starts_with(b"/*") => {
                if remaining.starts_with(b"/*!")
                    || remaining.starts_with(b"/*M!")
                    || remaining.starts_with(b"/*m!")
                    || remaining.starts_with(b"/*+")
                {
                    return Err(unsupported_query());
                }
                let width = remaining[2..]
                    .windows(2)
                    .position(|pair| pair == b"*/")
                    .ok_or_else(unsupported_query)?;
                if remaining[2..2 + width].windows(2).any(|pair| pair == b"/*") {
                    return Err(unsupported_query());
                }
                index += width + 4;
                continue;
            }
            b'\'' | b'"' | b'`' => {
                let identifier = byte == b'`' || byte == b'"' && quoting.ansi_quotes;
                index += quoted_width(remaining, quoting.backslash_escapes && !identifier)
                    .ok_or_else(unsupported_query)?;
                if identifier {
                    Kind::Identifier(byte)
                } else {
                    Kind::Literal
                }
            }
            byte if word_byte(byte) => {
                index += remaining
                    .iter()
                    .take_while(|&&byte| word_byte(byte))
                    .count();
                Kind::Word
            }
            b'.' | b',' | b';' | b'(' | b')' | b'+' | b'-' | b'*' | b'/' | b'%' | b'=' | b'<'
            | b'>' | b'!' | b'&' | b'|' | b'^' | b'~' => {
                index += 1;
                Kind::Symbol(byte)
            }
            _ => return Err(unsupported_query()),
        };
        if tokens.len() == 65_536 {
            return Err("筛选条件过长，无法用于编辑查询结果。".into());
        }
        tokens.push(Token {
            kind,
            text: &sql[start..index],
            start,
            end: index,
        });
    }
    Ok(tokens)
}

fn word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$') || !byte.is_ascii()
}

fn line_width(text: &[u8]) -> usize {
    text.iter()
        .position(|byte| matches!(byte, b'\r' | b'\n'))
        .unwrap_or(text.len())
}

fn quoted_width(text: &[u8], backslash_escapes: bool) -> Option<usize> {
    let quote = text[0];
    let mut index = 1;
    while let Some(&byte) = text.get(index) {
        index += 1;
        if byte == quote {
            if text.get(index) != Some(&quote) {
                return Some(index);
            }
            index += 1;
        } else if byte == b'\\' && backslash_escapes {
            index += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::row_editor::{FieldValue, TableEdit, edit_table};
    use mysql::{OptsBuilder, Value};

    #[test]
    fn bounds_an_aliased_filtered_select_including_its_offset() {
        let sql = "SELECT b.`编号`, b.`内容` FROM `资料`.`收录` AS b\nWHERE b.`内容` IN ('INTO', 'it''s') AND (b.`编号` >= 2) ORDER BY b.`编号` DESC LIMIT 1000 OFFSET 5; -- keep filters";
        let query = Select::parse(sql, Quoting::from_sql_mode("")).unwrap();
        assert_eq!(query.schema.as_deref(), Some("资料"));
        assert_eq!(query.table, "收录");
        assert_eq!(
            query.bounded_sql(4),
            sql.split(';')
                .next()
                .unwrap()
                .replace("LIMIT 1000", "LIMIT 4")
        );
    }

    #[test]
    fn rejects_queries_without_a_single_read_only_source() {
        for sql in [
            "SELECT * FROM sample JOIN sample AS other USING (id)",
            "SELECT * FROM sample WHERE side_effect(id)",
            "SELECT * FROM sample INTO OUTFILE 'output'",
            "SELECT * FROM sample WHERE id = 1 /*! FOR UPDATE */",
            "SELECT * FROM sample; SELECT * FROM sample",
        ] {
            assert!(
                Select::parse(sql, Quoting::from_sql_mode("")).is_err(),
                "{sql}"
            );
        }
    }

    #[test]
    #[ignore = "requires a disposable MariaDB on loopback via HARBOR_EDITABLE_FIXTURE_PORT"]
    fn captures_a_filtered_page_and_guards_the_original_row() {
        let port: u16 = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let fixture = OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(port)
            .user(Some("root"))
            .prefer_socket(false);
        let mut connection = Conn::new(fixture.clone()).unwrap();
        let schema = format!("harbor_editable_query_{}", std::process::id());
        connection
            .query_drop(format!("CREATE DATABASE {schema}"))
            .unwrap();
        connection.select_db(&schema).unwrap();
        connection
            .query_drop(
                "CREATE TABLE items (
                id BIGINT UNSIGNED NOT NULL, code VARBINARY(8) NOT NULL,
                price DECIMAL(20,4), payload BLOB, note VARCHAR(32),
                PRIMARY KEY (id, code)
            ) ENGINE=InnoDB",
            )
            .unwrap();
        connection
            .query_drop(
                "INSERT INTO items VALUES
                (1, X'01', 12345678901234.5678, X'00FF', 'first'),
                (2, X'02', NULL, X'FF00', NULL),
                (3, X'03', 10, NULL, 'outside')",
            )
            .unwrap();
        let opts = Opts::from(fixture.db_name(Some(schema.clone())));
        let page = query_editable(
            &opts,
            "SELECT t.note, t.payload, t.price, t.code, t.id FROM items AS t WHERE t.id < 3 ORDER BY t.id DESC",
            1,
            None,
        ).unwrap();
        assert!(page.has_more);
        assert_eq!(page.primary_key, [4, 3]);
        let row = &page.rows[0];
        assert_eq!(
            row.original(),
            [
                Value::Int(2),
                Value::Bytes(vec![2]),
                Value::NULL,
                Value::Bytes(vec![255, 0]),
                Value::NULL,
            ]
        );
        let edit = || TableEdit::Update {
            original: row.clone(),
            values: vec![("note".into(), FieldValue::Text("edited".into()))],
        };
        assert_eq!(edit_table(&opts, page.object.clone(), edit()).unwrap(), 1);
        assert!(
            edit_table(&opts, page.object.clone(), edit())
                .unwrap_err()
                .contains("已被其他操作修改")
        );
        let note: String = connection
            .query_first("SELECT note FROM items WHERE id = 2")
            .unwrap()
            .unwrap();
        assert_eq!(note, "edited");
        assert!(query_editable(&opts, "SELECT id, note FROM items", 1, None).is_err());
        connection
            .query_drop(format!("DROP DATABASE {schema}"))
            .unwrap();
    }

    #[test]
    #[ignore = "requires disposable MariaDB via HARBOR_HTTP_FIXTURE_PORT"]
    fn edits_keyless_f8_rows_with_explicit_duplicate_scope() {
        use crate::database::{
            Cell, Command, ConnectionConfig, DuplicateChoice, DuplicateScope, Response, Worker,
            http_session::workflow_test::{TestGateway, archived_tunnel, fixture_database},
        };
        use std::time::{Duration, SystemTime, UNIX_EPOCH};

        let (port, opts) = fixture_database();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let schema = format!("harbor_http_keyless_{}_{timestamp:x}", std::process::id());
        let mut sql = Conn::new(opts.clone()).unwrap();
        sql.query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
            .unwrap();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sql.select_db(&schema).unwrap();
            let gateway = TestGateway::new(opts);
            let config = ConnectionConfig {
                port,
                database: Some(schema.clone()),
                ..Default::default()
            };
            let mut cases = vec![
                ("direct_rows", Command::Connect(config.clone())),
                (
                    "proxy_rows",
                    Command::ConnectHttp(config.clone(), gateway.tunnel.clone()),
                ),
            ];
            if let Some(tunnel) = archived_tunnel() {
                cases.push(("php_rows", Command::ConnectHttp(config, tunnel)));
            }
            for (table, connect) in cases {
                sql.query_drop(format!(
                    "CREATE TABLE {table} (qty INT, label VARCHAR(40) COLLATE utf8mb4_general_ci,
                      stamp VARBINARY(8), amount DECIMAL(24,4), nested TEXT) ENGINE=InnoDB"
                ))
                .unwrap();
                sql.query_drop(format!(
                    "INSERT INTO {table} VALUES
                     (7,'同名a',X'00FF',8123456789012.3456,NULL),
                     (7,'同名a',X'00FF',8123456789012.3456,NULL),
                     (7,'同名a',X'00FF',8123456789012.3456,NULL),
                     (7,'同名A',X'00FF',8123456789012.3456,NULL),
                     (7,'同名a',X'00FE',8123456789012.3456,NULL)"
                ))
                .unwrap();

                let worker = Worker::new();
                let mut id = 0;
                let mut request = |command| {
                    id += 1;
                    worker.send(id, command).unwrap();
                    let reply = worker
                        .recv_timeout(Duration::from_secs(15))
                        .unwrap()
                        .unwrap();
                    assert_eq!(reply.id, id);
                    reply
                        .result
                        .unwrap_or_else(|error| panic!("{table}: {error}"))
                };
                assert!(matches!(request(connect), Response::Connected(_)));
                let query = format!("SELECT t.label AS memo FROM {table} AS t WHERE t.qty = 7");
                let read = |request: &mut dyn FnMut(Command) -> Response| {
                    let Response::EditablePage(page) = request(Command::QueryEditable {
                        sql: query.clone(),
                        limit: 10,
                    }) else {
                        panic!("expected a projected editable grid");
                    };
                    assert!(page.primary_key.is_empty() && page.row_key.is_empty());
                    assert_eq!(page.column_labels, ["memo"]);
                    assert!(!page.can_insert);
                    let original = page.rows.iter().find(|row| {
                        matches!(row.cells.first(), Some(Cell::Text(label)) if label == "同名a")
                            && matches!(row.original().get(2), Some(Value::Bytes(bytes)) if bytes == &[0, 255])
                    }).unwrap().clone();
                    (page.object, original)
                };
                let (object, original) = read(&mut request);
                let update = TableEdit::Update {
                    original,
                    values: vec![("label".into(), FieldValue::Text("已改".into()))],
                };
                assert!(matches!(
                    request(Command::EditTable {
                        object: object.clone(),
                        edit: update.clone(),
                    }),
                    Response::TableEditConfirmation { matches: 3 }
                ));
                assert_eq!(
                    sql.query_first::<u32, _>(format!(
                        "SELECT COUNT(*) FROM {table} WHERE BINARY label = BINARY '已改'"
                    ))
                    .unwrap(),
                    Some(0)
                );

                assert!(matches!(
                    request(Command::ConfirmTableEdit {
                        object: object.clone(),
                        edit: update,
                        choice: DuplicateChoice {
                            matches: 3,
                            scope: DuplicateScope::One
                        },
                    }),
                    Response::TableEdited { affected_rows: 1 }
                ));
                let (_, original) = read(&mut request);
                let delete = TableEdit::Delete { original };
                assert!(matches!(
                    request(Command::EditTable {
                        object: object.clone(),
                        edit: delete.clone(),
                    }),
                    Response::TableEditConfirmation { matches: 2 }
                ));
                assert!(matches!(
                    request(Command::ConfirmTableEdit {
                        object,
                        edit: delete,
                        choice: DuplicateChoice {
                            matches: 2,
                            scope: DuplicateScope::All
                        },
                    }),
                    Response::TableEdited { affected_rows: 2 }
                ));
                assert_eq!(
                    sql.query::<(String, String), _>(format!(
                        "SELECT label, HEX(stamp) FROM {table} ORDER BY BINARY label, stamp"
                    ))
                    .unwrap(),
                    [
                        ("同名A".into(), "00FF".into()),
                        ("同名a".into(), "00FE".into()),
                        ("已改".into(), "00FF".into()),
                    ],
                    "{table}"
                );
            }
            gateway.stop();
        }));
        sql.query_drop(format!("DROP DATABASE IF EXISTS {schema}"))
            .unwrap();
        if let Err(error) = outcome {
            std::panic::resume_unwind(error);
        }
    }

    #[test]
    #[ignore = "requires a disposable MariaDB via HARBOR_HTTP_FIXTURE_PORT"]
    fn edits_tables_without_a_primary_key_using_complete_non_null_unique_indexes() {
        use crate::database::http_session::workflow_test::{TestGateway, fixture_database};
        use crate::database::{Command, ConnectionConfig, Response, Worker};
        use std::time::{Duration, SystemTime, UNIX_EPOCH};

        let (port, opts) = fixture_database();
        let schema = format!(
            "harbor_http_alternate_key_{}_{:x}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        );
        let mut sql = Conn::new(opts.clone()).unwrap();
        sql.query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
            .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sql.select_db(&schema).unwrap();
            let gateway = TestGateway::new(opts);
            let config = ConnectionConfig {
                port,
                database: Some(schema.clone()),
                ..Default::default()
            };
            for (name, connect) in [
                ("direct_rows", Command::Connect(config.clone())),
                (
                    "tunnel_rows",
                    Command::ConnectHttp(config, gateway.tunnel.clone()),
                ),
            ] {
                sql.query_drop(format!(
                    "CREATE TABLE {name} (
                        tenant INT NOT NULL, code VARBINARY(8) NOT NULL, label VARCHAR(40) NOT NULL,
                        prefix_tag VARCHAR(12) NOT NULL, retired VARCHAR(12), note VARCHAR(40),
                        UNIQUE KEY a_prefix (prefix_tag(2)), UNIQUE KEY a_retired (retired),
                        UNIQUE KEY b_label (label), UNIQUE KEY c_code (tenant, code)
                    ) ENGINE=InnoDB"
                ))
                .unwrap();
                sql.query_drop(format!(
                    "INSERT INTO {name} VALUES
                        (1, X'FF00', 'a', 'tag-one', NULL, 'first'),
                        (2, X'FF00', 'b', 'two-tag', NULL, 'second')"
                ))
                .unwrap();

                let worker = Worker::new();
                let mut id = 0;
                let mut request = |command| {
                    id += 1;
                    worker.send(id, command).unwrap();
                    let event = worker
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    assert_eq!(event.id, id);
                    event.result.unwrap()
                };
                assert!(matches!(request(connect), Response::Connected(_)));
                let object = DbObject {
                    schema: schema.clone(),
                    name: name.into(),
                    kind: ObjectKind::Table,
                };
                let Response::TablePage(grid) = request(Command::TablePage {
                    object: object.clone(),
                    limit: 10,
                    offset: 0,
                }) else {
                    panic!("expected a table page")
                };
                assert!(grid.primary_key.is_empty() && grid.edit_error.is_none());
                assert_eq!(grid.row_key, [2]);
                assert!(matches!(
                    request(Command::EditTable {
                        object: object.clone(),
                        edit: TableEdit::Update {
                            original: grid.rows[0].clone(),
                            values: vec![("note".into(), FieldValue::Text("表格已改".into()))],
                        },
                    }),
                    Response::TableEdited { affected_rows: 1 }
                ));

                let query = format!(
                    "SELECT t.note AS memo, t.code AS id, t.tenant AS region FROM {name} AS t WHERE t.tenant = 2"
                );
                let Response::EditablePage(projected) = request(Command::QueryEditable {
                    sql: query.clone(),
                    limit: 10,
                }) else {
                    panic!("expected an editable projection")
                };
                assert_eq!(projected.column_labels, ["memo", "id", "region"]);
                assert!(projected.primary_key.is_empty() && !projected.can_insert);
                assert_eq!(projected.row_key, [2, 1]);
                assert!(matches!(
                    request(Command::EditTable {
                        object: object.clone(),
                        edit: TableEdit::Update {
                            original: projected.rows[0].clone(),
                            values: vec![("note".into(), FieldValue::Text("查询已改".into()))],
                        },
                    }),
                    Response::TableEdited { affected_rows: 1 }
                ));
                let rows = sql
                    .query::<(u8, String, String), _>(format!(
                        "SELECT tenant, label, note FROM {name} ORDER BY tenant"
                    ))
                    .unwrap();
                assert_eq!(
                    rows,
                    [
                        (1, "a".into(), "表格已改".into()),
                        (2, "b".into(), "查询已改".into())
                    ]
                );

                let Response::EditablePage(refreshed) = request(Command::QueryEditable {
                    sql: query,
                    limit: 10,
                }) else {
                    panic!("expected the refreshed projection")
                };
                assert!(matches!(
                    request(Command::EditTable {
                        object,
                        edit: TableEdit::Delete {
                            original: refreshed.rows[0].clone()
                        },
                    }),
                    Response::TableEdited { affected_rows: 1 }
                ));
                assert_eq!(
                    sql.query::<u8, _>(format!("SELECT tenant FROM {name}"))
                        .unwrap(),
                    [1]
                );
            }
            gateway.stop();
        }));
        sql.query_drop(format!("DROP DATABASE {schema}")).unwrap();
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }
}
