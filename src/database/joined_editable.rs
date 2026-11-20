use std::collections::HashSet;
use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[cfg(test)]
use mysql::Conn;
use mysql::consts::ColumnFlags;
use mysql::prelude::Queryable;
use mysql::{Column, Opts, Transaction, Value};

use super::editable_query::{
    EDITABLE_QUOTE_MODES, EDITABLE_UTF8_REQUIRED, EditableRead, Kind, NativeRead, Parser, Quoting,
    failed_editable_prepare, lex, read_native, read_query_context, supports_editable_charset,
};
use super::row_editor::{
    MAX_PAGE_ROWS, TableLayout, joined_page_from_values, native_fingerprint_column,
    read_native_fingerprint, try_load_tagged_layout,
};
use super::{
    Cell, DbObject, JoinedEditTarget, JoinedEditablePage, ObjectKind, OperationMonitor,
    QueryTiming, RESULT_MEMORY_LIMIT, ResultSet, cell, column_is_binary, mysql_error,
    quote_identifier, transfer,
};

pub(super) struct JoinQuery<'a> {
    source: &'a str,
    projection_end: usize,
    limit: Option<(Range<usize>, u64)>,
    order_ordinals: Vec<u64>,
    pub(super) sources: Vec<JoinSource>,
    pub(super) projection: Vec<JoinProjection>,
}

pub(super) struct JoinSource {
    pub(super) schema: Option<String>,
    pub(super) table: String,
    pub(super) alias: String,
}

pub(super) enum JoinProjection {
    Column {
        qualifier: Option<String>,
        name: String,
        label: Option<String>,
    },
    Wildcard {
        qualifier: Option<String>,
    },
    Literal {
        label: Option<String>,
    },
}

fn unsupported_join() -> String {
    "联表编辑需要可确定来源的平面 SELECT。请使用直接表及明确别名，可用 INNER、LEFT、RIGHT 或 CROSS JOIN。".into()
}

pub(super) fn is_joined_query(sql: &str) -> bool {
    for mode in EDITABLE_QUOTE_MODES {
        let Ok(tokens) = lex(sql, Quoting::from_sql_mode(mode)) else {
            continue;
        };
        if !tokens.first().is_some_and(|token| token.word("SELECT")) {
            continue;
        }
        let mut from = false;
        let mut depth = 0_usize;
        for token in &tokens {
            match token.kind {
                Kind::Symbol(b'(') => depth += 1,
                Kind::Symbol(b')') => depth = depth.saturating_sub(1),
                Kind::Symbol(b';') if depth == 0 => break,
                _ if depth == 0 && token.word("FROM") => from = true,
                _ if depth == 0 && from && token.word("JOIN") => return true,
                _ => {}
            }
        }
    }
    false
}

pub(super) fn can_plan_join(sql: &str) -> bool {
    EDITABLE_QUOTE_MODES
        .into_iter()
        .any(|mode| JoinQuery::parse(sql, mode).is_ok())
}

pub(super) fn editable_sources(sql: &str, mode: &str) -> Option<Vec<(Option<String>, String)>> {
    let query = JoinQuery::parse(sql, mode).ok()?;
    Some(
        query
            .sources
            .into_iter()
            .map(|source| (source.schema, source.table))
            .collect(),
    )
}

impl<'a> JoinQuery<'a> {
    pub(super) fn parse(sql: &'a str, sql_mode: &str) -> Result<Self, String> {
        let tokens = lex(sql, Quoting::from_sql_mode(sql_mode)).map_err(|_| unsupported_join())?;
        if tokens.iter().skip(1).any(|token| {
            [
                "SELECT",
                "INTO",
                "FOR",
                "LOCK",
                "SQL_CALC_FOUND_ROWS",
                "UNION",
                "INTERSECT",
                "EXCEPT",
                "DISTINCT",
                "DISTINCTROW",
                "GROUP",
                "HAVING",
                "WINDOW",
                "OVER",
                "QUALIFY",
                "RETURNING",
                "PROCEDURE",
                "NEXTVAL",
                "NATURAL",
                "USING",
            ]
            .iter()
            .any(|word| token.word(word))
        }) {
            return Err(unsupported_join());
        }
        let mut parser = Parser { remaining: &tokens };
        parser
            .expect_word("SELECT")
            .map_err(|_| unsupported_join())?;
        parser.take_word("ALL");
        let mut projection = Vec::new();
        loop {
            projection.push(projection_item(&mut parser)?);
            if !parser.take_symbol(b',') {
                break;
            }
        }
        let projection_end = parser.remaining.first().ok_or_else(unsupported_join)?.start;
        parser.expect_word("FROM").map_err(|_| unsupported_join())?;
        let first = table_source(&mut parser)?;
        let mut aliases = HashSet::from([first.alias.to_lowercase()]);
        let mut sources = vec![first];
        while let Some(needs_on) = consume_join(&mut parser)? {
            let source = table_source(&mut parser)?;
            if !aliases.insert(source.alias.to_lowercase()) {
                return Err("联表编辑需要为每个表来源指定能区分的别名。".into());
            }
            sources.push(source);
            if parser.take_word("ON") {
                read_predicate(&mut parser, true)?;
            } else if needs_on {
                return Err(unsupported_join());
            }
        }
        if sources.len() < 2 {
            return Err(unsupported_join());
        }
        if parser.take_word("WHERE") {
            read_predicate(&mut parser, false)?;
        }
        let mut order_ordinals = Vec::new();
        if parser.take_word("ORDER") {
            parser.expect_word("BY").map_err(|_| unsupported_join())?;
            loop {
                if parser.remaining.first().is_some_and(|token| {
                    token.kind == Kind::Word && token.text.bytes().all(|byte| byte.is_ascii_digit())
                }) {
                    order_ordinals.push(parser.integer()?.1);
                } else {
                    parser.name().map_err(|_| unsupported_join())?;
                    if parser.take_symbol(b'.') {
                        parser.name().map_err(|_| unsupported_join())?;
                    }
                }
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
        let end = tokens[tokens.len() - parser.remaining.len() - 1].end;
        parser.take_symbol(b';');
        if !parser.remaining.is_empty() {
            return Err(unsupported_join());
        }

        Ok(Self {
            source: &sql[..end],
            projection_end,
            limit,
            order_ordinals,
            sources,
            projection,
        })
    }

    pub(super) fn check_order_columns(&self, visible: usize) -> Result<(), String> {
        if self
            .order_ordinals
            .iter()
            .all(|&index| index > 0 && index <= visible as u64)
        {
            Ok(())
        } else {
            Err("ORDER BY 中的列序号超出原始结果的列数。".into())
        }
    }

    pub(super) fn bounded_sql(&self, limit: usize, additional: &[String]) -> String {
        let tail = match &self.limit {
            Some((range, count)) if *count > limit as u64 => format!(
                "{}{limit}{}",
                &self.source[self.projection_end..range.start],
                &self.source[range.end..]
            ),
            Some(_) => self.source[self.projection_end..].to_owned(),
            None => format!("{} LIMIT {limit}", &self.source[self.projection_end..]),
        };
        let prefix = &self.source[..self.projection_end];
        if additional.is_empty() {
            format!("{prefix}{tail}")
        } else {
            format!("{prefix}, {} {tail}", additional.join(", "))
        }
    }
}

fn projection_item(parser: &mut Parser<'_>) -> Result<JoinProjection, String> {
    let token = parser.remaining.first().ok_or_else(unsupported_join)?;
    if [
        "DISTINCT",
        "DISTINCTROW",
        "SQL_CALC_FOUND_ROWS",
        "HIGH_PRIORITY",
    ]
    .iter()
    .any(|word| token.word(word))
    {
        return Err(unsupported_join());
    }
    if parser.take_symbol(b'*') {
        return Ok(JoinProjection::Wildcard { qualifier: None });
    }
    if token.kind == Kind::Literal
        || token.kind == Kind::Word && token.text.bytes().all(|byte| byte.is_ascii_digit())
        || ["NULL", "TRUE", "FALSE"]
            .iter()
            .any(|word| token.word(word))
    {
        parser.remaining = &parser.remaining[1..];
        return Ok(JoinProjection::Literal {
            label: projection_label(parser)?,
        });
    }
    let first = parser.name().map_err(|_| unsupported_join())?;
    let (qualifier, name) = if parser.take_symbol(b'.') {
        if parser.take_symbol(b'*') {
            return Ok(JoinProjection::Wildcard {
                qualifier: Some(first),
            });
        }
        (Some(first), parser.name().map_err(|_| unsupported_join())?)
    } else {
        (None, first)
    };
    Ok(JoinProjection::Column {
        qualifier,
        name,
        label: projection_label(parser)?,
    })
}

fn projection_label(parser: &mut Parser<'_>) -> Result<Option<String>, String> {
    if parser.take_word("AS")
        || parser
            .remaining
            .first()
            .is_some_and(|token| token.is_name() && !token.word("FROM"))
    {
        parser.name().map(Some).map_err(|_| unsupported_join())
    } else {
        Ok(None)
    }
}

fn table_source(parser: &mut Parser<'_>) -> Result<JoinSource, String> {
    let name = parser.name().map_err(|_| unsupported_join())?;
    let (schema, table) = if parser.take_symbol(b'.') {
        (Some(name), parser.name().map_err(|_| unsupported_join())?)
    } else {
        (None, name)
    };
    let alias = if parser.take_word("AS")
        || parser.remaining.first().is_some_and(|token| {
            token.is_name()
                && ![
                    "JOIN",
                    "INNER",
                    "LEFT",
                    "RIGHT",
                    "CROSS",
                    "OUTER",
                    "NATURAL",
                    "STRAIGHT_JOIN",
                    "ON",
                    "USING",
                    "WHERE",
                    "ORDER",
                    "LIMIT",
                    "GROUP",
                    "HAVING",
                    "UNION",
                    "FOR",
                    "LOCK",
                    "INTO",
                    "WINDOW",
                ]
                .iter()
                .any(|word| token.word(word))
        }) {
        parser.name().map_err(|_| unsupported_join())?
    } else {
        table.clone()
    };
    Ok(JoinSource {
        schema,
        table,
        alias,
    })
}

fn consume_join(parser: &mut Parser<'_>) -> Result<Option<bool>, String> {
    if parser.take_word("JOIN") {
        return Ok(Some(true));
    }
    let needs_on = if parser.take_word("LEFT") || parser.take_word("RIGHT") {
        parser.take_word("OUTER");
        true
    } else if parser.take_word("INNER") {
        true
    } else if parser.take_word("CROSS") {
        false
    } else {
        return Ok(None);
    };
    parser.expect_word("JOIN").map_err(|_| unsupported_join())?;
    Ok(Some(needs_on))
}

fn read_predicate(parser: &mut Parser<'_>, in_join: bool) -> Result<(), String> {
    let mut depth = 0_usize;
    let mut end = parser.remaining.len();
    for (index, token) in parser.remaining.iter().enumerate() {
        if depth == 0
            && (token.kind == Kind::Symbol(b';')
                || ["ORDER", "LIMIT"].iter().any(|word| token.word(word))
                || in_join
                    && ["WHERE", "JOIN", "INNER", "LEFT", "RIGHT", "CROSS"]
                        .iter()
                        .any(|word| token.word(word)))
        {
            end = index;
            break;
        }
        match token.kind {
            Kind::Symbol(b'(') => depth += 1,
            Kind::Symbol(b')') => depth = depth.checked_sub(1).ok_or_else(unsupported_join)?,
            _ => {}
        }
    }
    let mut check = Parser {
        remaining: &parser.remaining[..end],
    };
    check.predicate().map_err(|_| unsupported_join())?;
    if !check.remaining.is_empty() {
        return Err(unsupported_join());
    }
    parser.remaining = &parser.remaining[end..];
    Ok(())
}

pub(super) fn query_joined_editable(
    opts: &Opts,
    sql: &str,
    limit: usize,
    monitor: Option<&Mutex<OperationMonitor>>,
) -> Result<JoinedEditablePage, String> {
    read_native(opts, monitor, |transaction, reader| {
        read_joined(transaction, sql, limit, reader, false)
    })?
    .require_editable()
}

pub(super) fn try_query_joined_editable(
    opts: &Opts,
    sql: &str,
    limit: usize,
    monitor: Option<&Mutex<OperationMonitor>>,
) -> Result<EditableRead<JoinedEditablePage>, String> {
    read_native(opts, monitor, |transaction, reader| {
        read_joined(transaction, sql, limit, reader, true)
    })
}

fn read_joined(
    transaction: &mut Transaction<'_>,
    sql: &str,
    limit: usize,
    reader: &NativeRead<'_>,
    prefer_regular_read: bool,
) -> Result<EditableRead<JoinedEditablePage>, String> {
    let limit = limit.clamp(1, MAX_PAGE_ROWS);
    reader.check()?;
    let tag = reader.tag;
    let (database, lower_case_tables, sql_mode, charset) = read_query_context(transaction, tag)?;
    if !supports_editable_charset(sql, &charset) {
        return Ok(EditableRead::ReadOnly(EDITABLE_UTF8_REQUIRED.into()));
    }
    let query = match JoinQuery::parse(sql, &sql_mode) {
        Ok(query) => query,
        Err(reason) => return Ok(EditableRead::ReadOnly(reason)),
    };
    reader.check()?;
    let original = match transaction.prep(format!("{tag}{}", query.bounded_sql(limit + 1, &[]))) {
        Ok(statement) => statement,
        Err(error) => return failed_editable_prepare(error, false),
    };
    let visible = original.columns();
    if let Err(reason) = query.check_order_columns(visible.len()) {
        return Ok(EditableRead::ReadOnly(reason));
    }
    let mut extra = Vec::new();
    let mut roles = plan_roles(
        transaction,
        &query,
        visible.as_ref(),
        database.as_deref(),
        lower_case_tables != 0,
        &mut extra,
        reader,
    )?;
    if prefer_regular_read && roles.is_empty() {
        return Ok(EditableRead::ReadOnly(
            "当前结果没有可核实的写入目标。".into(),
        ));
    }
    let row_index_bytes = roles.len().saturating_mul(size_of::<Option<usize>>());
    let column_index_bytes = row_index_bytes.saturating_mul(visible.len());
    let Some(mut remaining) = RESULT_MEMORY_LIMIT.checked_sub(column_index_bytes) else {
        return Ok(EditableRead::ReadOnly(
            "联表结果的字段映射超过显示容量，请缩小查询范围。".into(),
        ));
    };
    let prepared = if extra.is_empty() {
        original
    } else {
        reader.check()?;
        match transaction.prep(format!("{tag}{}", query.bounded_sql(limit + 1, &extra))) {
            Ok(statement) => statement,
            Err(error) => return failed_editable_prepare(error, true),
        }
    };
    let expected = prepared.columns();
    if expected.len() != visible.len() + extra.len() {
        return Ok(EditableRead::ReadOnly(
            "服务器返回的联表捕获列数与期望不一致。请重新执行。".into(),
        ));
    }
    if !same_columns(&expected[..visible.len()], visible.as_ref(), None) {
        return Ok(EditableRead::ReadOnly(
            "捕获原始行时联表查询的显示列已变化，请重新执行。".into(),
        ));
    }
    if roles
        .iter()
        .any(|role| !role.matches_hidden(expected.as_ref()))
    {
        return Ok(EditableRead::ReadOnly(
            "服务器返回的联表原始列无法与表别名核对，请重新执行。".into(),
        ));
    }

    reader.check()?;
    let started = Instant::now();
    let mut result = transaction.exec_iter(&prepared, ()).map_err(mysql_error)?;
    let execution = started.elapsed();
    if !same_columns(result.columns().as_ref(), expected.as_ref(), Some(&roles)) {
        return Err(changed_columns());
    }
    let binary: Vec<_> = visible.iter().map(column_is_binary).collect();
    let mut display = ResultSet {
        source_sql: Some(Arc::from(sql)),
        columns: visible
            .iter()
            .map(|column| column.name_str().into_owned())
            .collect(),
        column_hints: Vec::new(),
        column_origins: visible
            .iter()
            .map(super::ColumnOrigin::from_native)
            .collect(),
        excel_types: visible
            .iter()
            .map(transfer::ExcelColumnType::from_native)
            .collect(),
        export_columns: visible
            .iter()
            .map(transfer::ResultColumn::from_native)
            .collect(),
        duplicate_source: None,
        notice: None,
        rows: Vec::new(),
        affected_rows: 0,
        last_insert_id: None,
        truncated: false,
    };
    for row in result.iter().into_iter().flatten() {
        let mut values = row.map_err(mysql_error)?.unwrap();
        if values.len() != expected.len() {
            return Err("联表查询返回的行宽与列布局不一致，请重新执行。".into());
        }
        let weight = row_index_bytes
            + values.len() * (size_of::<Value>() + size_of::<Cell>())
            + values
                .iter()
                .map(|value| match value {
                    Value::Bytes(bytes) => 2 * bytes.len(),
                    _ => 32,
                })
                .sum::<usize>();
        if display.rows.len() == limit || weight > remaining {
            if display.rows.is_empty() {
                return Err("此行数据超过联表编辑的显示容量。".into());
            }
            display.truncated = true;
            break;
        }
        remaining -= weight;
        display.rows.push(
            values[..visible.len()]
                .iter_mut()
                .zip(&binary)
                .map(|(value, &binary)| cell(std::mem::replace(value, Value::NULL), binary))
                .collect(),
        );
        for role in &mut roles {
            role.capture(&mut values)?;
        }
    }
    drop(result);
    let query_timing = Some(QueryTiming {
        execution: Some(execution),
        elapsed: started.elapsed(),
    });

    let mut targets = roles
        .into_iter()
        .filter(|role| !role.rows.is_empty())
        .map(|role| {
            let mut result_columns = vec![None; visible.len()];
            for (result, target) in role.column_positions {
                result_columns[result] = Some(target);
            }
            JoinedEditTarget {
                alias: role.alias,
                page: joined_page_from_values(
                    role.layout,
                    role.rows,
                    &role.projection,
                    display.truncated,
                ),
                result_rows: role.result_rows,
                result_columns,
            }
        })
        .collect::<Vec<_>>();
    targets.sort_by_key(|target| {
        target
            .result_columns
            .iter()
            .position(Option::is_some)
            .unwrap_or(usize::MAX)
    });
    if targets.is_empty() {
        display.notice = Some("当前结果没有可编辑的匹配行。".into());
    }
    Ok(EditableRead::Editable(JoinedEditablePage {
        result: display,
        targets,
        query_timing,
    }))
}

struct CaptureRole {
    alias: String,
    layout: TableLayout,
    object: DbObject,
    projection: Vec<(usize, String)>,
    column_positions: Vec<(usize, usize)>,
    offset: usize,
    labels: Vec<String>,
    rows: Vec<(Vec<Value>, String)>,
    result_rows: Vec<Option<usize>>,
}

impl CaptureRole {
    fn matches_hidden(&self, columns: &[Column]) -> bool {
        let Some(span) = columns.get(self.offset..self.offset + self.labels.len()) else {
            return false;
        };
        if span
            .iter()
            .zip(&self.labels)
            .any(|(column, label)| column.name_ref() != label.as_bytes())
        {
            return false;
        }
        span.iter()
            .take(self.layout.columns.len())
            .zip(&self.layout.columns)
            .all(|(column, original)| {
                column.schema_ref() == self.object.schema.as_bytes()
                    && column.org_table_ref() == self.object.name.as_bytes()
                    && column.table_ref() == self.alias.as_bytes()
                    && column.org_name_ref() == original.name.as_bytes()
            })
    }

    fn capture(&mut self, values: &mut [Value]) -> Result<(), String> {
        let end = self.offset + self.layout.columns.len();
        let original = &values[self.offset..end];
        let key = self.layout.row_key();
        if key.iter().any(|&column| original[column] == Value::NULL) {
            // The selected source did not join this row on the outer side.
            self.result_rows.push(None);
            return Ok(());
        }
        let fingerprint =
            read_native_fingerprint(Some(std::mem::replace(&mut values[end], Value::NULL)))?;
        let original = &values[self.offset..end];
        let row = if let Some((row, (previous, previous_hash))) =
            self.rows.iter().enumerate().find(|(_, (previous, _))| {
                key.iter()
                    .all(|&column| previous[column] == original[column])
            }) {
            if previous.as_slice() != original || *previous_hash != fingerprint {
                return Err("同一个源键对应不同的原始行，请重新执行联表查询。".into());
            }
            row
        } else {
            let row = self.rows.len();
            self.rows.push((
                values[self.offset..end]
                    .iter_mut()
                    .map(|value| std::mem::replace(value, Value::NULL))
                    .collect(),
                fingerprint,
            ));
            row
        };
        self.result_rows.push(Some(row));
        Ok(())
    }
}

fn plan_roles(
    transaction: &mut Transaction<'_>,
    query: &JoinQuery<'_>,
    columns: &[Column],
    database: Option<&str>,
    lower_case: bool,
    extra: &mut Vec<String>,
    reader: &NativeRead<'_>,
) -> Result<Vec<CaptureRole>, String> {
    let mut reserved: HashSet<_> = columns
        .iter()
        .map(|column| column.name_str().to_lowercase())
        .collect();
    let query_text = query.source.to_lowercase();
    let mut counter = 0_usize;
    let mut next_label = || loop {
        let label = format!("__harbor_join_capture_{counter}");
        counter += 1;
        if !query_text.contains(&label) && reserved.insert(label.clone()) {
            return label;
        }
    };

    let mut roles = Vec::new();
    'sources: for source in &query.sources {
        reader.check()?;
        let Some(schema) = source.schema.as_deref().or(database) else {
            continue;
        };
        let projected: Vec<_> = columns
            .iter()
            .enumerate()
            .filter(|(_, column)| {
                !column.org_name_ref().is_empty()
                    && matches_name(schema, column.schema_ref(), lower_case)
                    && matches_name(&source.table, column.org_table_ref(), lower_case)
                    && matches_name(&source.alias, column.table_ref(), lower_case)
            })
            .collect();
        let Some((_, first)) = projected.first().copied() else {
            continue;
        };
        let (Ok(schema), Ok(table), Ok(alias)) = (
            std::str::from_utf8(first.schema_ref()),
            std::str::from_utf8(first.org_table_ref()),
            std::str::from_utf8(first.table_ref()),
        ) else {
            continue;
        };
        if projected.iter().any(|(_, column)| {
            column.schema_ref() != first.schema_ref()
                || column.org_table_ref() != first.org_table_ref()
                || column.table_ref() != first.table_ref()
        }) {
            continue;
        }
        let object = DbObject {
            schema: schema.to_owned(),
            name: table.to_owned(),
            kind: ObjectKind::Table,
        };
        let layout = try_load_tagged_layout(
            transaction,
            object.clone(),
            &object.qualified_name(),
            reader.tag,
        );
        reader.check()?;
        let Some(layout) = layout? else {
            continue;
        };
        if layout.edit_error.is_some() || layout.row_key().is_empty() {
            continue;
        }
        let mut projection = Vec::new();
        let mut column_positions = Vec::new();
        let mut target_columns = vec![None; layout.columns.len()];
        for (result_column, column) in projected {
            let Some(source_column) = layout
                .columns
                .iter()
                .position(|field| field.name.as_bytes() == column.org_name_ref())
            else {
                continue 'sources;
            };
            let target_column = *target_columns[source_column].get_or_insert_with(|| {
                let target_column = projection.len();
                projection.push((source_column, column.name_str().into_owned()));
                target_column
            });
            column_positions.push((result_column, target_column));
        }
        let offset = columns.len() + extra.len();
        let actual_alias = quote_identifier(&source.alias);
        let mut labels = Vec::with_capacity(layout.columns.len() + 1);
        for column in &layout.columns {
            let label = next_label();
            extra.push(format!(
                "{actual_alias}.{} AS {}",
                quote_identifier(&column.name),
                quote_identifier(&label)
            ));
            labels.push(label);
        }
        let marker = next_label();
        extra.push(native_fingerprint_column(
            &layout,
            Some(&source.alias),
            &marker,
        ));
        labels.push(marker);
        roles.push(CaptureRole {
            alias: alias.to_owned(),
            layout,
            object,
            projection,
            column_positions,
            offset,
            labels,
            rows: Vec::new(),
            result_rows: Vec::new(),
        });
    }
    Ok(roles)
}

fn matches_name(expected: &str, actual: &[u8], lower_case: bool) -> bool {
    expected.as_bytes() == actual
        || lower_case
            && std::str::from_utf8(actual)
                .is_ok_and(|actual| actual.to_lowercase() == expected.to_lowercase())
}

fn same_columns(left: &[Column], right: &[Column], executed: Option<&[CaptureRole]>) -> bool {
    // Materialized join results can lose advisory index bits. The actual keys
    // come from the table layout and are rechecked when writing.
    let advisory_flags = if executed.is_some() {
        ColumnFlags::PRI_KEY_FLAG
            | ColumnFlags::UNIQUE_KEY_FLAG
            | ColumnFlags::MULTIPLE_KEY_FLAG
            | ColumnFlags::PART_KEY_FLAG
    } else {
        ColumnFlags::empty()
    };
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .enumerate()
            .all(|(index, (actual, expected))| {
                // Only generated fingerprint expressions can substitute the label
                // for the empty original name and normalize the decimal metadata.
                let fingerprint = executed.is_some_and(|roles| {
                    roles
                        .iter()
                        .any(|role| index == role.offset + role.layout.columns.len())
                });
                let original_name = if fingerprint {
                    actual.org_name_ref().is_empty() || actual.org_name_ref() == expected.name_ref()
                } else {
                    actual.org_name_ref() == expected.org_name_ref()
                };
                actual.name_ref() == expected.name_ref()
                    && actual.schema_ref() == expected.schema_ref()
                    && actual.table_ref() == expected.table_ref()
                    && actual.org_table_ref() == expected.org_table_ref()
                    && original_name
                    && actual.column_type() == expected.column_type()
                    && actual.character_set() == expected.character_set()
                    && actual.column_length() == expected.column_length()
                    && (fingerprint || actual.decimals() == expected.decimals())
                    && (actual.flags() & !advisory_flags) == (expected.flags() & !advisory_flags)
            })
}

fn changed_columns() -> String {
    "联表查询执行时的列结构已变化，请重新执行。".into()
}

#[cfg(test)]
mod tests {
    use super::super::http_session::workflow_test::fixture_database;
    use super::super::row_editor::edit_table;
    use super::super::{FieldValue, TableEdit};
    use super::*;

    #[test]
    fn bounded_join_keeps_aliases_projection_order_and_offset() {
        let sql = "SELECT a.label AS member, b.* FROM warehouse.family AS a \
                   LEFT OUTER JOIN warehouse.family b ON a.parent_id = b.id \
                   WHERE a.id >= 2 ORDER BY 1 DESC LIMIT 12 OFFSET 3;";
        let query = JoinQuery::parse(sql, "").unwrap();
        assert_eq!(query.sources.len(), 2);
        assert_eq!(query.sources[0].schema.as_deref(), Some("warehouse"));
        assert_eq!(query.sources[0].alias, "a");
        assert_eq!(query.sources[1].alias, "b");
        assert!(matches!(
            &query.projection[0],
            JoinProjection::Column { qualifier: Some(alias), name, label: Some(label) }
                if alias == "a" && name == "label" && label == "member"
        ));
        assert!(matches!(
            &query.projection[1],
            JoinProjection::Wildcard { qualifier: Some(alias) } if alias == "b"
        ));
        query.check_order_columns(3).unwrap();
        let hidden = "a.id AS __harbor_original".to_owned();
        let bounded = query.bounded_sql(5, std::slice::from_ref(&hidden));
        assert!(bounded.contains(&format!("{hidden} FROM ")));
        assert!(bounded.ends_with("WHERE a.id >= 2 ORDER BY 1 DESC LIMIT 5 OFFSET 3"));
    }

    #[test]
    #[ignore = "需要一次性 MariaDB，端口由 HARBOR_HTTP_FIXTURE_PORT 指定"]
    fn self_join_captures_distinct_aliases_and_writes_only_the_picked_original() {
        let (_, opts) = fixture_database();
        let mut connection = Conn::new(opts.clone()).unwrap();
        let schema = format!("harbor_http_join_alias_{}", std::process::id());
        connection
            .query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
            .unwrap();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            connection
                .query_drop(format!(
                    "CREATE TABLE {schema}.family (
                        id INT PRIMARY KEY, parent_id INT, label VARCHAR(30) NOT NULL
                    ) ENGINE=InnoDB"
                ))
                .unwrap();
            connection
                .query_drop(format!(
                    "INSERT INTO {schema}.family VALUES (1,NULL,'父行'),(2,1,'子行甲'),(3,1,'子行乙')"
                ))
                .unwrap();
            let query = format!(
                "SELECT child.label AS member, parent.label AS leader, '标签' AS marker,
                        parent.label AS repeated_leader
                 FROM {schema}.family AS child LEFT JOIN {schema}.family AS parent
                 ON child.parent_id = parent.id ORDER BY child.id"
            );
            assert!(is_joined_query(&query));
            let page = query_joined_editable(&opts, &query, 10, None).unwrap();
            assert_eq!(
                page.result.columns,
                ["member", "leader", "marker", "repeated_leader"]
            );
            assert_eq!(page.result.rows.len(), 3);
            assert_eq!(page.result.rows[0][1], Cell::Null);
            assert_eq!(page.targets.len(), 2);
            let child = &page.targets[0];
            let parent = &page.targets[1];
            assert_eq!(
                (child.alias.as_str(), parent.alias.as_str()),
                ("child", "parent")
            );
            assert_eq!((child.page.rows.len(), parent.page.rows.len()), (3, 1));
            assert_eq!(child.result_rows, [Some(0), Some(1), Some(2)]);
            assert_eq!(parent.result_rows, [None, Some(0), Some(0)]);
            assert_eq!(child.result_columns, [Some(0), None, None, None]);
            assert_eq!(parent.result_columns, [None, Some(0), None, Some(0)]);
            assert_eq!(parent.page.column_labels, ["leader"]);
            assert!(!parent.page.can_insert);
            assert_eq!(parent.page.rows[0].original()[0], Value::Int(1));
            assert_eq!(
                edit_table(
                    &opts,
                    parent.page.object.clone(),
                    TableEdit::Update {
                        original: parent.page.rows[parent.result_rows[2].unwrap()].clone(),
                        values: vec![("label".into(), FieldValue::Text("已改父行".into()))],
                    },
                )
                .unwrap(),
                1
            );
            assert_eq!(
                connection
                    .query::<(i32, String), _>(format!(
                        "SELECT id,label FROM {schema}.family ORDER BY id"
                    ))
                    .unwrap(),
                [
                    (1, String::from("已改父行")),
                    (2, String::from("子行甲")),
                    (3, String::from("子行乙")),
                ]
            );
        }));
        connection
            .query_drop(format!("DROP DATABASE IF EXISTS {schema}"))
            .unwrap();
        if let Err(error) = outcome {
            std::panic::resume_unwind(error);
        }
    }
}
