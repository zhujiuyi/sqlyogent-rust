use std::time::{Duration, Instant};
use std::{collections::HashMap, mem, sync::Arc};

use crate::http_tunnel::ResultSet as RawResult;
use crate::sql_tools::mysql_text_literal;

use super::editable_query::{EditableRead, project_page};
use super::http_session::{
    RESPONSE_TOO_LARGE, Session as HttpSession, batch_statements, http_cell,
};
use super::joined_editable::{JoinProjection, JoinQuery};
use super::row_editor::{
    MAX_PAGE_ROWS, TableLayout, fingerprint_column, http_layout, http_page_from_raw,
    selected_key_layout_guard,
};
use super::transfer::{ExcelColumnType, ResultColumn};
use super::{
    Cell, DbObject, JoinedEditTarget, JoinedEditablePage, ObjectKind, QueryTiming,
    RESULT_MEMORY_LIMIT, ResultSet, quote_identifier,
};

const UNKNOWN_PROJECTION: &str =
    "HTTP 隧道需要用表名或别名限定每个结果列。常量列须有明确的 AS 别名。";
const CHANGED_RESULT: &str = "联表查询的返回列或源表结构已变化，请重新执行。";

struct VisibleColumn {
    source: Option<(usize, usize)>,
    label: String,
}

struct CapturedRole {
    projected: Vec<usize>,
    column_labels: Vec<String>,
    originals: Vec<usize>,
    fingerprint: usize,
    layout_guard: usize,
}

pub(super) fn query_joined_editable_http(
    session: &HttpSession,
    sql: &str,
    limit: usize,
) -> Result<JoinedEditablePage, String> {
    try_query_joined_editable_http(session, sql, limit)?.require_editable()
}

pub(super) fn try_query_joined_editable_http(
    session: &HttpSession,
    sql: &str,
    limit: usize,
) -> Result<EditableRead<JoinedEditablePage>, String> {
    let context = session.snapshot_context()?;
    if !matches!(
        context.character_set_client.as_str(),
        "utf8" | "utf8mb3" | "utf8mb4"
    ) {
        return Ok(EditableRead::ReadOnly(
            "HTTP 联表结果编辑需要 UTF-8 会话字符集。".into(),
        ));
    }
    let query = match JoinQuery::parse(sql, &context.sql_mode) {
        Ok(query) => query,
        Err(reason) => return Ok(EditableRead::ReadOnly(reason)),
    };
    let database = session.database_name().unwrap_or_default();
    let layouts = query
        .sources
        .iter()
        .map(|source| {
            let schema = source.schema.as_deref().unwrap_or(database);
            if schema.is_empty() {
                return Err("请选择默认数据库，或在查询中明确指定库名。".into());
            }
            http_layout(
                session,
                DbObject {
                    schema: schema.into(),
                    name: source.table.clone(),
                    kind: ObjectKind::Table,
                },
            )
        })
        .collect::<Result<Vec<_>, String>>();
    let layouts = match layouts {
        Ok(layouts) => layouts,
        Err(reason) => return Ok(EditableRead::ReadOnly(reason)),
    };
    let visible = match resolve_projection(&query, &layouts, context.lower_case_table_names) {
        Ok(visible) => visible,
        Err(reason) => return Ok(EditableRead::ReadOnly(reason)),
    };
    if let Err(reason) = query.check_order_columns(visible.len()) {
        return Ok(EditableRead::ReadOnly(reason));
    }
    let prefix = hidden_prefix(sql, &visible);
    let mut labels: Vec<_> = visible.iter().map(|column| column.label.clone()).collect();
    let mut additional = Vec::new();
    let captures = layouts
        .iter()
        .zip(&query.sources)
        .enumerate()
        .map(|(role, (layout, source))| {
            if layout.edit_error.is_some() || layout.row_key().is_empty() {
                return None;
            }
            plan_capture(
                role,
                layout,
                &source.alias,
                &visible,
                &prefix,
                &mut labels,
                &mut additional,
            )
        })
        .collect::<Vec<_>>();

    let mode = format!(
        "SET SESSION sql_mode = {}",
        mysql_text_literal(&context.sql_mode)
    );
    // Report failures from the executed query directly to the editor.
    let (mut raw, limit, elapsed) =
        read_join(session, database, &mode, &query, &additional, limit)?;
    if !raw.columns.iter().map(|field| &field.name).eq(&labels) {
        return Err(CHANGED_RESULT.into());
    }
    let capture_count = captures.iter().flatten().count();
    let displayed = fit_rows(&raw, limit, capture_count, visible.len())?;
    let truncated = displayed < raw.rows.len();
    raw.rows.truncate(displayed);
    let mut result = visible_result(&raw, &query, &layouts, &visible, truncated, sql);
    let mut targets = Vec::new();
    for (role, ((capture, layout), source)) in captures
        .into_iter()
        .zip(layouts)
        .zip(query.sources)
        .enumerate()
    {
        if let Some(capture) = capture
            && let Some(target) =
                capture.into_target(&mut raw, layout, role, source.alias, &visible, truncated)?
        {
            let first_column = target
                .result_columns
                .iter()
                .position(Option::is_some)
                .unwrap_or(usize::MAX);
            targets.push((first_column, target));
        }
    }
    if targets.is_empty() && result.notice.is_none() {
        result.notice = Some("当前结果没有可编辑的匹配行。".into());
    }
    targets.sort_by_key(|(first_column, _)| *first_column);
    let targets = targets.into_iter().map(|(_, target)| target).collect();
    Ok(EditableRead::Editable(JoinedEditablePage {
        result,
        targets,
        query_timing: Some(QueryTiming {
            execution: None,
            elapsed,
        }),
    }))
}

fn resolve_projection(
    query: &JoinQuery<'_>,
    layouts: &[TableLayout],
    case_mode: u8,
) -> Result<Vec<VisibleColumn>, String> {
    let resolve_role = |qualifier: &str| {
        let mut matches = query.sources.iter().enumerate().filter(|(_, source)| {
            source.alias == qualifier
                || case_mode != 0 && source.alias.eq_ignore_ascii_case(qualifier)
        });
        let index = matches.next().map(|(index, _)| index);
        index
            .filter(|_| matches.next().is_none())
            .ok_or(UNKNOWN_PROJECTION)
    };
    let mut visible = Vec::new();
    let mut remaining = RESULT_MEMORY_LIMIT;
    let mut push = |column: VisibleColumn| -> Result<(), String> {
        remaining = remaining
            .checked_sub(mem::size_of::<VisibleColumn>() + column.label.len())
            .ok_or("联表结果的列名和列数超过显示容量，请缩小查询范围。")?;
        visible.push(column);
        Ok(())
    };
    for item in &query.projection {
        match item {
            JoinProjection::Column {
                qualifier: Some(qualifier),
                name,
                label,
            } => {
                let role = resolve_role(qualifier)?;
                let fields = &layouts[role].columns;
                let column = fields
                    .iter()
                    .position(|column| column.name.eq_ignore_ascii_case(name))
                    .ok_or(UNKNOWN_PROJECTION)?;
                push(VisibleColumn {
                    source: Some((role, column)),
                    label: label.as_ref().unwrap_or(&fields[column].name).clone(),
                })?;
            }
            JoinProjection::Wildcard {
                qualifier: Some(qualifier),
            } => {
                let role = resolve_role(qualifier)?;
                for (column, field) in layouts[role]
                    .columns
                    .iter()
                    .enumerate()
                    .filter(|(_, field)| field.visible_by_default())
                {
                    push(VisibleColumn {
                        source: Some((role, column)),
                        label: field.name.clone(),
                    })?;
                }
            }
            JoinProjection::Literal { label: Some(label) } => push(VisibleColumn {
                source: None,
                label: label.clone(),
            })?,
            _ => return Err(UNKNOWN_PROJECTION.into()),
        }
    }
    Ok(visible)
}

fn hidden_prefix(sql: &str, visible: &[VisibleColumn]) -> String {
    let sql = sql.to_ascii_lowercase();
    let names: Vec<_> = visible
        .iter()
        .map(|column| column.label.to_ascii_lowercase())
        .collect();
    // A hidden label must not make an original ORDER BY name resolve differently.
    (0usize..)
        .map(|index| format!("__harbor_join_{index}_"))
        .find(|prefix| !sql.contains(prefix) && !names.iter().any(|name| name.starts_with(prefix)))
        .expect("unoccupied projection prefix")
}

fn plan_capture(
    role: usize,
    layout: &TableLayout,
    alias: &str,
    visible: &[VisibleColumn],
    prefix: &str,
    labels: &mut Vec<String>,
    additional: &mut Vec<String>,
) -> Option<CapturedRole> {
    let mut originals = vec![None; layout.columns.len()];
    let mut projected = Vec::new();
    let mut column_labels = Vec::new();
    for (index, column) in visible.iter().enumerate() {
        if let Some((origin, field)) = column.source
            && origin == role
            && originals[field].is_none()
        {
            originals[field] = Some(index);
            projected.push(field);
            column_labels.push(column.label.clone());
        }
    }
    if projected.is_empty() {
        return None;
    }

    let mut next_label = || {
        let index = labels.len();
        let label = format!("{prefix}{index}");
        labels.push(label.clone());
        (index, label)
    };
    let table = quote_identifier(alias);
    let originals = originals
        .into_iter()
        .zip(&layout.columns)
        .map(|(visible, column)| {
            visible.unwrap_or_else(|| {
                let field = quote_identifier(&column.name);
                let (index, label) = next_label();
                additional.push(format!("{table}.{field} AS {}", quote_identifier(&label)));
                index
            })
        })
        .collect();
    let (fingerprint, label) = next_label();
    additional.push(fingerprint_column(layout, Some(alias), &label));
    let guard = selected_key_layout_guard(layout);
    let (layout_guard, label) = next_label();
    additional.push(format!("IF({guard},1,0) AS {}", quote_identifier(&label)));
    Some(CapturedRole {
        projected,
        column_labels,
        originals,
        fingerprint,
        layout_guard,
    })
}

fn read_join(
    session: &HttpSession,
    database: &str,
    mode: &str,
    query: &JoinQuery<'_>,
    additional: &[String],
    limit: usize,
) -> Result<(RawResult, usize, Duration), String> {
    let mut limit = limit.clamp(1, MAX_PAGE_ROWS);
    loop {
        let request = limit + 1;
        let select = query.bounded_sql(request, additional);
        let batch = batch_statements(&["SET SESSION time_zone = '+00:00'", mode, &select])?;
        let payload = session.prepare_payload(&batch, true, database)?;
        let started = Instant::now();
        let mut reply = match session.execute_payload(payload, request) {
            Err(error) if error == RESPONSE_TOO_LARGE && limit > 1 => {
                limit = (limit / 2).max(1);
                continue;
            }
            result => result?,
        };
        let elapsed = started.elapsed();
        if let Some(error) = reply.error {
            return Err(format!("ERROR {}: {}", error.code, error.message));
        }
        let result = reply.sets.pop().ok_or(CHANGED_RESULT)?;
        if result.truncated || reply.omitted_sets != 0 || !reply.sets.is_empty() {
            return Err("HTTP 隧道返回的联表数据不完整。".into());
        }
        return Ok((result, limit, elapsed));
    }
}

fn fit_rows(
    raw: &RawResult,
    limit: usize,
    capture_count: usize,
    visible_columns: usize,
) -> Result<usize, String> {
    if raw.rows.is_empty() {
        return Ok(0);
    }
    let row_mapping_bytes = capture_count * mem::size_of::<Option<usize>>();
    let mut remaining = row_mapping_bytes
        .checked_mul(visible_columns)
        .and_then(|column_mapping_bytes| RESULT_MEMORY_LIMIT.checked_sub(column_mapping_bytes))
        .ok_or("联表结果的回写列超过显示容量，请缩小查询范围。")?;
    let mut count = 0;
    for row in raw.rows.iter().take(limit) {
        let payload = row.iter().flatten().map(Vec::len).sum::<usize>();
        let weight = 3 * payload
            + row.len() * (mem::size_of::<mysql::Value>() + 2 * mem::size_of::<Cell>())
            + row_mapping_bytes;
        let Some(available) = remaining.checked_sub(weight) else {
            if count == 0 {
                return Err("此行数据和回写校验值超过联表结果的显示容量。".into());
            }
            break;
        };
        remaining = available;
        count += 1;
    }
    Ok(count)
}

fn visible_result(
    raw: &RawResult,
    query: &JoinQuery<'_>,
    layouts: &[TableLayout],
    visible: &[VisibleColumn],
    truncated: bool,
    sql: &str,
) -> ResultSet {
    let fields = &raw.columns[..visible.len()];
    let binary: Vec<_> = visible
        .iter()
        .map(|column| {
            column
                .source
                .is_some_and(|(role, field)| layouts[role].columns[field].is_binary())
        })
        .collect();
    ResultSet {
        source_sql: Some(Arc::from(sql)),
        columns: fields.iter().map(|field| field.name.clone()).collect(),
        column_hints: visible
            .iter()
            .map(|column| {
                column.source.map_or_else(String::new, |(role, field)| {
                    let field = &layouts[role].columns[field];
                    format!(
                        "来源 {}.{}\n类型 {}",
                        query.sources[role].alias, field.name, field.data_type
                    )
                })
            })
            .collect(),
        column_origins: visible
            .iter()
            .map(|column| {
                let (role, field) = column.source?;
                let layout = &layouts[role];
                let table = layout.object();
                Some(super::ColumnOrigin::new(
                    &table.schema,
                    &table.name,
                    &layout.columns[field].name,
                ))
            })
            .collect(),
        excel_types: fields
            .iter()
            .map(|field| ExcelColumnType::from_type_name(&field.data_type))
            .collect(),
        export_columns: fields.iter().map(ResultColumn::from_http).collect(),
        duplicate_source: None,
        notice: raw.info.clone(),
        rows: raw
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .zip(&binary)
                    .map(|(value, binary)| match (binary, value) {
                        (true, Some(bytes)) => Cell::Binary(bytes.clone()),
                        _ => http_cell(value.clone()),
                    })
                    .collect()
            })
            .collect(),
        affected_rows: raw.affected_rows,
        last_insert_id: raw.last_insert_id,
        truncated,
    }
}

impl CapturedRole {
    fn into_target(
        self,
        raw: &mut RawResult,
        layout: TableLayout,
        role: usize,
        alias: String,
        visible: &[VisibleColumn],
        truncated: bool,
    ) -> Result<Option<JoinedEditTarget>, String> {
        let mut rows = Vec::new();
        let mut seen_keys = HashMap::new();
        let mut result_rows = Vec::with_capacity(raw.rows.len());
        for row in &mut raw.rows {
            if row[self.layout_guard].as_deref() != Some(b"1") {
                return Err(CHANGED_RESULT.into());
            }
            if layout
                .row_key()
                .iter()
                .any(|&index| row[self.originals[index]].is_none())
            {
                if self.originals.iter().any(|&index| row[index].is_some()) {
                    return Err("隧道没有返回可确认的联表行标识。".into());
                }
                // A NULL-extended outer join row has no source row to edit.
                result_rows.push(None);
                continue;
            }
            let key = layout
                .row_key()
                .iter()
                .map(|&index| row[self.originals[index]].clone())
                .collect::<Vec<_>>();
            let values = self
                .originals
                .iter()
                .chain([&self.fingerprint])
                .map(|&index| row[index].take())
                .collect::<Vec<_>>();
            let target_row = if let Some(&prior) = seen_keys.get(&key) {
                if rows[prior] != values {
                    return Err("同一联表编辑目标的原始行数据不一致，请重新执行。".into());
                }
                prior
            } else {
                let index = rows.len();
                seen_keys.insert(key, index);
                rows.push(values);
                index
            };
            result_rows.push(Some(target_row));
        }
        if rows.is_empty() {
            return Ok(None);
        }
        let mut projected_positions = vec![None; layout.columns.len()];
        for (target_column, &field) in self.projected.iter().enumerate() {
            projected_positions[field] = Some(target_column);
        }
        let result_columns = visible
            .iter()
            .map(|column| match column.source {
                Some((origin, field)) if origin == role => projected_positions[field],
                _ => None,
            })
            .collect();
        let columns = self
            .originals
            .iter()
            .chain([&self.fingerprint])
            .map(|&index| mem::take(&mut raw.columns[index]))
            .collect();
        let positions: Vec<_> = (0..self.originals.len()).collect();
        let limit = rows.len();
        let set = RawResult {
            columns,
            rows,
            affected_rows: 0,
            last_insert_id: None,
            server_version: String::new(),
            info: None,
            truncated: false,
        };
        let page = http_page_from_raw(layout, set, &positions, true, limit, 0)?;
        if page.rows.len() != limit {
            return Err(CHANGED_RESULT.into());
        }
        let mut page = project_page(page, &self.projected, self.column_labels);
        page.can_insert = false;
        page.has_more = truncated;
        Ok(Some(JoinedEditTarget {
            alias,
            page,
            result_rows,
            result_columns,
        }))
    }
}
