use std::fmt::Write as _;
use std::time::{Duration, Instant};

use crate::http_tunnel;
use crate::sql_tools::mysql_text_literal;

use super::super::Failure;
use super::super::http_session::{RESPONSE_TOO_LARGE, Session, batch_statements};
use super::*;

const UTC: &str = "SET SESSION time_zone = '+00:00'";
const ROW_CHANGED: &str = "此行已被其他操作修改或删除，请刷新后重试。";
const LAYOUT_CHANGED: &str = "表结构已变化，请刷新后重试。";
const KEYLESS_READ_ONLY: &str = "隧道未能完整读取无键行的回写校验数据，本页仅供浏览。";
const EDIT_SESSION: [&str; 4] = [
    UTC,
    "SET SESSION autocommit = 1",
    "SET SESSION sql_mode = 'STRICT_ALL_TABLES,NO_ENGINE_SUBSTITUTION,NO_AUTO_VALUE_ON_ZERO'",
    "SET SESSION foreign_key_checks = 1, SESSION unique_checks = 1",
];
const FINISH_EDIT: [&str; 5] = [
    "SET @harbor_finish = IF(@harbor_accepted = 1, 'COMMIT AND NO CHAIN NO RELEASE', 'DO 0')",
    "PREPARE harbor_transaction FROM @harbor_finish",
    "EXECUTE harbor_transaction",
    "DEALLOCATE PREPARE harbor_transaction",
    "ROLLBACK AND NO CHAIN NO RELEASE",
];

pub(in crate::database) fn http_table_page(
    session: &Session,
    object: DbObject,
    limit: usize,
    offset: usize,
    selection: &TableSelection,
) -> Result<TablePage, String> {
    let range = TableReadRange::Page {
        offset,
        rows: NonZeroUsize::new(limit.clamp(1, MAX_PAGE_ROWS)).unwrap(),
    };
    read_http_table_data(session, object, selection, range, false, || Ok(()))
}

pub(in crate::database) fn http_table_data(
    session: &Session,
    object: DbObject,
    selection: &TableSelection,
    range: TableReadRange,
    check: impl FnMut() -> Result<(), String>,
) -> Result<TablePage, String> {
    read_http_table_data(session, object, selection, range, true, check)
}

fn read_http_table_data(
    session: &Session,
    object: DbObject,
    selection: &TableSelection,
    range: TableReadRange,
    multiple: bool,
    mut check: impl FnMut() -> Result<(), String>,
) -> Result<TablePage, String> {
    check()?;
    let layout = http_layout(session, object)?;
    let mut parameters = Vec::new();
    let filter = table_where(&layout, &selection.predicates, &mut parameters)?;
    let where_clause = bind_literals(&filter, &parameters)?;
    let order = table_order(&layout, &selection.order, multiple)?;
    let mut fingerprint_name = "__harbor_row_fingerprint".to_owned();
    while layout
        .columns
        .iter()
        .any(|column| column.name.eq_ignore_ascii_case(&fingerprint_name))
    {
        fingerprint_name.push('_');
    }
    let mut fingerprint =
        (layout.edit_error.is_none()).then(|| fingerprint_column(&layout, None, &fingerprint_name));
    let projection = layout.projection();
    let table = layout.object.qualified_name();
    let indices = (0..layout.columns.len()).collect::<Vec<_>>();
    let mut page = PageBuffer::new(layout);
    let mut batch_limit = MAX_PAGE_ROWS;
    let mut read = |page: &mut PageBuffer, limit: usize, offset: usize| {
        let select = |fingerprint: Option<&str>, request| {
            let extra = fingerprint.map_or_else(String::new, |column| format!(", {column}"));
            format!(
                "SELECT {projection}{extra} FROM {table}{where_clause}{order} LIMIT {request} OFFSET {offset}"
            )
        };
        let limit = batch_limit.min(limit);
        let mut query = |fingerprint: Option<&str>| {
            http_table_select(
                session,
                limit,
                |request| select(fingerprint, request),
                multiple,
                &mut check,
            )
        };
        let result = match query(fingerprint.as_deref()) {
            Err(error)
                if fingerprint.is_some()
                    && page.layout.is_keyless()
                    && error == RESPONSE_TOO_LARGE =>
            {
                fingerprint = None;
                page.edit_error = Some(KEYLESS_READ_ONLY.into());
                if let Some(layout) = Arc::get_mut(&mut page.layout) {
                    layout.edit_error = page.edit_error.clone();
                }
                query(None)
            }
            other => other,
        };
        let (reply, effective_limit, with_lookahead) = match result {
            Ok(reply) => reply,
            Err(error) if multiple && !page.rows.is_empty() && error == RESPONSE_TOO_LARGE => {
                return Ok(BatchEnd::Full);
            }
            Err(error) => return Err(error),
        };
        let names = page
            .layout
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .chain(fingerprint.as_ref().map(|_| fingerprint_name.as_str()));
        if reply
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .ne(names)
        {
            return Err(LAYOUT_CHANGED.into());
        }
        batch_limit = effective_limit;
        let has_row = !reply.rows.is_empty();
        let end = push_http_rows(
            page,
            reply,
            &indices,
            fingerprint.is_some(),
            effective_limit,
        )?;
        if !with_lookahead && has_row && matches!(end, BatchEnd::Complete) {
            check()?;
            let next = offset.checked_add(1).ok_or("读取位置已超出可用范围。")?;
            let sql = format!("SELECT 1 FROM {table}{where_clause} LIMIT 1 OFFSET {next}");
            let (following, _) = http_select(session, "", &sql, 1)?;
            if !following.rows.is_empty() {
                return Ok(BatchEnd::More);
            }
        }
        Ok(end)
    };
    let has_more = if multiple {
        page.fill(range, read)?
    } else {
        !matches!(
            read(&mut page, range.batch_limit(0), range.offset())?,
            BatchEnd::Complete
        )
    };
    check()?;
    Ok(page.finish(range.offset(), has_more))
}

pub(in crate::database) fn http_layout(
    session: &Session,
    object: DbObject,
) -> Result<TableLayout, String> {
    ensure_table(&object)?;
    let materialize = |sql: String| {
        sql.replace(":schema", &mysql_text_literal(&object.schema))
            .replace(":table", &mysql_text_literal(&object.name))
    };
    let spec = session
        .metadata_columns::<3>(&materialize(table_metadata_sql()), 1)
        .map_err(Failure::message)?;
    let [Some(kind), transactions, engine] = spec.first().ok_or("找不到表的元数据。")?
    else {
        return Err("服务器返回的表属性不完整。".into());
    };
    let columns = session
        .metadata_columns::<10>(&materialize(column_metadata_sql()), 10_000)
        .map_err(Failure::message)?
        .into_iter()
        .map(|row| match row {
            [
                Some(name),
                Some(kind),
                Some(declaration),
                Some(nullable),
                default,
                Some(extra),
                generation,
                charset,
                collation,
                declared_width,
            ] => Ok((
                name,
                kind,
                declaration,
                nullable,
                default,
                extra,
                generation,
                charset,
                collation,
                declared_width
                    .map(|value| value.parse())
                    .transpose()
                    .map_err(|_| "隧道返回的字段字节长度无效。")?,
            )),
            _ => Err("隧道返回的列属性不完整。".into()),
        });
    let key = session
        .metadata_columns::<4>(&materialize(unique_index_sql()), MAX_INDEX_PARTS)
        .map_err(Failure::message)?
        .into_iter()
        .map(|[index, name, sequence, prefix]| {
            let index = index.ok_or("隧道未返回索引名称。")?;
            let sequence = sequence
                .as_deref()
                .and_then(|position| position.parse().ok())
                .ok_or("隧道返回的索引字段顺序无效。")?;
            let prefix = prefix
                .map(|value| value.parse().map_err(|_| "隧道返回的索引前缀长度无效。"))
                .transpose()?;
            Ok((index, name, sequence, prefix))
        });
    build_layout(
        object,
        kind,
        transactions.as_deref(),
        engine.as_deref(),
        columns,
        key,
    )
}

pub(in crate::database) fn http_bounded_select(
    session: &Session,
    database: &str,
    limit: usize,
    build: impl Fn(usize) -> String,
) -> Result<(http_tunnel::ResultSet, usize), String> {
    http_bounded_select_timed(session, database, limit, build).map(|(set, limit, _)| (set, limit))
}

pub(in crate::database) fn http_bounded_select_timed(
    session: &Session,
    database: &str,
    limit: usize,
    build: impl Fn(usize) -> String,
) -> Result<(http_tunnel::ResultSet, usize, Duration), String> {
    http_bounded_select_checked(session, database, limit, build, &mut || Ok(()))
}

fn http_bounded_select_checked(
    session: &Session,
    database: &str,
    mut limit: usize,
    build: impl Fn(usize) -> String,
    check: &mut impl FnMut() -> Result<(), String>,
) -> Result<(http_tunnel::ResultSet, usize, Duration), String> {
    loop {
        check()?;
        let request = limit + 1;
        match http_select(session, database, &build(request), request) {
            Err(error) if error == RESPONSE_TOO_LARGE && limit > 1 => limit = (limit / 2).max(1),
            result => return result.map(|(set, elapsed)| (set, limit, elapsed)),
        }
    }
}

fn http_table_select(
    session: &Session,
    limit: usize,
    build: impl Fn(usize) -> String,
    multiple: bool,
    check: &mut impl FnMut() -> Result<(), String>,
) -> Result<(http_tunnel::ResultSet, usize, bool), String> {
    match http_bounded_select_checked(session, "", limit, &build, check) {
        // Two wide rows may exceed the tunnel's response size even when
        // either row fits by itself. The caller checks for more separately.
        Err(error) if multiple && error == RESPONSE_TOO_LARGE => {
            check()?;
            http_select(session, "", &build(1), 1).map(|(reply, _)| (reply, 1, false))
        }
        result => result.map(|(reply, limit, _)| (reply, limit, true)),
    }
}

fn http_select(
    session: &Session,
    database: &str,
    select: &str,
    limit: usize,
) -> Result<(http_tunnel::ResultSet, Duration), String> {
    let batch = batch_statements(&[UTC, select])?;
    let payload = session.prepare_payload(&batch, true, database)?;
    let started = Instant::now();
    let reply = session.execute_payload(payload, limit)?;
    let elapsed = started.elapsed();
    last_set(reply).map(|set| (set, elapsed))
}

fn last_set(mut reply: http_tunnel::Reply) -> Result<http_tunnel::ResultSet, String> {
    if let Some(error) = reply.error {
        return Err(format!("ERROR {}: {}", error.code, error.message));
    }
    let set = reply.sets.pop().ok_or("HTTP 隧道没有返回预期的结果集。")?;
    if set.truncated || reply.omitted_sets != 0 || !reply.sets.is_empty() {
        return Err("HTTP 隧道返回的结果已截断或结果集数量不符。".into());
    }
    Ok(set)
}

pub(in crate::database) fn fingerprint_column(
    layout: &TableLayout,
    table_alias: Option<&str>,
    label: &str,
) -> String {
    let value = if layout.is_keyless() {
        exact_row_expression(layout, table_alias)
    } else {
        fingerprint_expression(layout, table_alias)
    };
    format!("{value} AS {}", quote_identifier(label))
}

pub(super) fn fingerprint_expression(layout: &TableLayout, table_alias: Option<&str>) -> String {
    let parts = encoded_columns(layout, table_alias)
        .map(|bytes| format!("COALESCE(SHA2({bytes},256),'#')"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("SHA2(CONCAT({parts}),256)")
}

pub(super) fn exact_row_expression(layout: &TableLayout, table_alias: Option<&str>) -> String {
    // Hex cells separated by '/' are unambiguous and permit exact comparison in SQL.
    // 'N' is outside the hex alphabet and distinguishes NULL from empty data.
    let parts = encoded_columns(layout, table_alias)
        .map(|bytes| format!("COALESCE(HEX({bytes}), 'N')"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("CONCAT_WS('/', {parts})")
}

fn encoded_columns<'a>(
    layout: &'a TableLayout,
    table_alias: Option<&'a str>,
) -> impl Iterator<Item = String> + 'a {
    let keyless = layout.is_keyless();
    layout.columns.iter().map(move |column| {
        let name = quote_identifier(&column.name);
        let name = table_alias.map_or_else(
            || name.clone(),
            |table| format!("{}.{name}", quote_identifier(table)),
        );
        let kind = column.data_type.split('(').next().unwrap_or_default();
        // Promote FLOAT away from its short display precision. For an exact whole
        // row also render DOUBLE as an expression without declared display scale.
        if column.value_type == ValueType::Float
            || keyless && column.value_type == ValueType::Double
        {
            format!("CAST(({name} + 0e0) AS BINARY)")
        } else if keyless && matches!(kind, "enum" | "set") {
            format!("CAST(({name} + 0) AS BINARY)")
        } else if keyless && kind == "timestamp" {
            // UNIX_TIMESTAMP on a TIMESTAMP field reads its stored epoch value.
            // Retain the string for zero timestamps outside the epoch range.
            format!("COALESCE(CAST(UNIX_TIMESTAMP({name}) AS BINARY), CAST({name} AS BINARY))")
        } else if keyless && kind == "char" {
            // This normalizes SQL-mode-dependent padding of fixed-width CHAR.
            format!("CAST(RTRIM({name}) AS BINARY)")
        } else {
            format!("CAST({name} AS BINARY)")
        }
    })
}

fn valid_fingerprint(layout: &TableLayout, value: &str) -> bool {
    if !layout.is_keyless() {
        return value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit());
    }
    let mut parts = value.split('/');
    layout.columns.iter().all(|_| {
        parts.next().is_some_and(|part| {
            part == "N"
                || part.len().is_multiple_of(2) && part.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    }) && parts.next().is_none()
}

pub(in crate::database) fn http_page_from_raw(
    layout: TableLayout,
    set: http_tunnel::ResultSet,
    positions: &[usize],
    with_fingerprint: bool,
    limit: usize,
    offset: usize,
) -> Result<TablePage, String> {
    let mut page = PageBuffer::new(layout);
    let end = push_http_rows(&mut page, set, positions, with_fingerprint, limit)?;
    Ok(page.finish(offset, !matches!(end, BatchEnd::Complete)))
}

fn push_http_rows(
    page: &mut PageBuffer,
    set: http_tunnel::ResultSet,
    positions: &[usize],
    with_fingerprint: bool,
    limit: usize,
) -> Result<BatchEnd, String> {
    let width = page.layout.columns.len();
    if set.columns.len() != width + usize::from(with_fingerprint) || positions.len() != width {
        return Err("隧道返回的数据列数与表结构不一致。".into());
    }
    let row_size = |row: &[Option<Vec<u8>>]| {
        row.len() * (size_of::<Value>() + size_of::<Cell>())
            + row
                .iter()
                .map(|value| value.as_ref().map_or(0, |bytes| 2 * bytes.len()))
                .sum::<usize>()
    };
    for (index, mut row) in set.rows.into_iter().enumerate() {
        if index == limit {
            return Ok(BatchEnd::More);
        }
        let mut size = row_size(&row);
        let mut with_fingerprint = with_fingerprint;
        if page.rows.is_empty()
            && size > page.remaining
            && page.layout.is_keyless()
            && with_fingerprint
        {
            row.pop();
            size = row_size(&row);
            with_fingerprint = false;
            page.edit_error = Some(KEYLESS_READ_ONLY.into());
        }
        if !page.reserve_row(size)? {
            return Ok(BatchEnd::Full);
        }
        let fingerprint = if with_fingerprint {
            let fingerprint = row
                .pop()
                .flatten()
                .and_then(|value| String::from_utf8(value).ok())
                .filter(|value| valid_fingerprint(&page.layout, value));
            if fingerprint.is_none() {
                if !page.layout.is_keyless() {
                    return Err("隧道未返回原始记录校验值。".into());
                }
                page.edit_error = Some(KEYLESS_READ_ONLY.into());
            }
            fingerprint
        } else {
            None
        };
        let mut values = Vec::with_capacity(width);
        let mut cells = Vec::with_capacity(width);
        for (column, &position) in page.layout.columns.iter().zip(positions) {
            let raw = row.get_mut(position).ok_or(LAYOUT_CHANGED)?.take();
            let value = decode_value(column, raw);
            let binary =
                column.is_binary() || matches!(&value, Value::Bytes(bytes) if bytes.contains(&0));
            cells.push(column.value_type.display(&value, binary));
            values.push(value);
        }
        page.rows.push(TableRow {
            cells,
            layout: Arc::clone(&page.layout),
            original: values.into(),
            fingerprint,
        });
    }
    Ok(BatchEnd::Complete)
}

fn decode_value(column: &TableColumn, raw: Option<Vec<u8>>) -> Value {
    let Some(raw) = raw else { return Value::NULL };
    match column.value_type {
        ValueType::Binary | ValueType::Text | ValueType::Unsupported | ValueType::Bit(_) => {
            Value::Bytes(raw)
        }
        kind => std::str::from_utf8(&raw)
            .ok()
            .and_then(|text| kind.parse(text.to_owned()))
            .unwrap_or(Value::Bytes(raw)),
    }
}

pub(in crate::database) fn http_edit_table_with_confirmation(
    session: &Session,
    object: DbObject,
    edit: TableEdit,
    choice: Option<DuplicateChoice>,
) -> Result<TableEditOutcome, String> {
    let layout = http_layout(session, object)?;
    if let Some(error) = &layout.edit_error {
        return Err(error.clone());
    }
    let table = layout.object.qualified_name();
    let (mut statement, matched, may_be_unchanged) = match edit {
        TableEdit::Update { original, values } => {
            let matched = match_original(&layout, &original, "@harbor_original_row")?;
            let assignments = write_values(&layout, values)?
                .into_iter()
                .map(|(column, value)| {
                    format!("{column} = {}", value.as_deref().unwrap_or("DEFAULT"))
                })
                .collect::<Vec<_>>();
            if assignments.is_empty() {
                return Err("没有需要保存的修改。".into());
            }
            let statement = format!(
                "UPDATE {table} SET {} WHERE {}",
                assignments.join(", "),
                matched.predicate,
            );
            (statement, Some(matched), true)
        }
        TableEdit::Delete { original } => {
            let matched = match_original(&layout, &original, "@harbor_original_row")?;
            (
                format!("DELETE FROM {table} WHERE {}", matched.predicate),
                Some(matched),
                false,
            )
        }
        TableEdit::Insert { values } => {
            let (columns, literals): (Vec<_>, Vec<_>) = write_values(&layout, values)?
                .into_iter()
                .filter_map(|(column, value)| value.map(|value| (column, value)))
                .unzip();
            (
                format!(
                    "INSERT INTO {table} ({}) VALUES ({})",
                    columns.join(", "),
                    literals.join(", ")
                ),
                None,
                false,
            )
        }
    };

    let exact_setup = matched.as_ref().and_then(|row| row.exact_setup.as_deref());
    let keyless = exact_setup.is_some();
    let (expected_matches, max_rows) = match choice {
        Some(choice) if keyless && choice.matches > 1 => (
            choice.matches,
            match choice.scope {
                DuplicateScope::One => {
                    statement.push_str(" LIMIT 1");
                    1
                }
                DuplicateScope::All => choice.matches,
            },
        ),
        Some(_) => return Err("重复行确认已失效，请重新发起修改。".into()),
        None => (1, 1),
    };
    if keyless {
        require_gap_locks(session)?;
    }
    let min_rows = if may_be_unchanged { 0 } else { max_rows };
    let key = matched.as_ref().and_then(|row| row.key);
    let matched_query = matched
        .as_ref()
        .map(|row| lock_row_query(&table, &row.predicate, keyless));
    // Hold the target's metadata lock across metadata verification, row comparison, and mutation.
    let acquire_metadata = format!("SELECT 1 FROM {table} LIMIT 0");
    let same_layout = format!(
        "SET @harbor_layout_ok = {}",
        layout_guard_for_keys(&layout, key, keyless)
    );
    let (initial_match, select_match) = match matched_query.as_deref() {
        Some(query) => ("SET @harbor_row_matches = 0", Some(query)),
        None => ("SET @harbor_row_matches = 1", None),
    };
    let action = format!(
        "SET @harbor_action = IF(@harbor_layout_ok = 1 AND @harbor_row_matches = {expected_matches} AND @harbor_match_warnings = 0, {}, 'DO 0')",
        mysql_text_literal(&statement)
    );
    let accepted = format!(
        "SET @harbor_accepted = (@harbor_layout_ok = 1 AND @harbor_row_matches = {expected_matches} AND @harbor_warnings = 0 AND @harbor_affected BETWEEN {min_rows} AND {max_rows})"
    );
    let mut batch = EDIT_SESSION.to_vec();
    if keyless {
        batch.push("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ");
    }
    batch.extend([
        "START TRANSACTION",
        &acquire_metadata,
        &same_layout,
        initial_match,
    ]);
    batch.extend(exact_setup);
    batch.extend(select_match);
    batch.push(if keyless {
        "SET @harbor_match_warnings = @@warning_count"
    } else {
        "SET @harbor_match_warnings = 0"
    });
    batch.extend([
        &action, "PREPARE harbor_write FROM @harbor_action", "EXECUTE harbor_write",
        "SET @harbor_affected = ROW_COUNT(), @harbor_warnings = GREATEST(@harbor_match_warnings, @@warning_count)",
        "DEALLOCATE PREPARE harbor_write", &accepted,
    ]);
    batch.extend(FINISH_EDIT);
    batch.push("SELECT @harbor_layout_ok AS layout_ok, @harbor_row_matches AS row_matches, @harbor_warnings AS warnings, @harbor_accepted AS committed, @harbor_affected AS affected_rows");
    let sql = batch_statements(&batch)?;
    let payload = session.prepare_payload(&sql, true, "")?;
    let unknown =
        |reason| format!("无法确认 HTTP 隧道是否提交了本次修改。请刷新表格核对后再重试。{reason}");
    let reply = session.execute_payload(payload, 1).map_err(unknown)?;
    let names = [
        "layout_ok",
        "row_matches",
        "warnings",
        "committed",
        "affected_rows",
    ];
    match edit_status(reply, names).map_err(unknown)? {
        [Some(0), _, _, Some(0), _] => Err(LAYOUT_CHANGED.into()),
        [Some(1), Some(0), _, Some(0), _] => Err(ROW_CHANGED.into()),
        [Some(1), _, Some(warnings), Some(0), _] if warnings > 0 => {
            Err("数据库返回核对或写入警告，本次修改已回滚。".into())
        }
        [Some(1), Some(matches), Some(0), Some(0), Some(0)]
            if keyless && choice.is_none() && matches > 1 =>
        {
            Ok(TableEditOutcome::NeedsConfirmation { matches })
        }
        [Some(1), Some(matches), Some(0), Some(0), Some(0)] if matches != expected_matches => {
            Err(if keyless {
                "符合原值的行数已变化，请刷新后重新确认。"
            } else {
                "无法唯一定位此行，本次修改已回滚。"
            }
            .into())
        }
        [Some(1), Some(matches), Some(0), Some(1), Some(affected)]
            if matches == expected_matches && (min_rows..=max_rows).contains(&affected) =>
        {
            Ok(TableEditOutcome::Applied(affected))
        }
        [Some(1), Some(matches), Some(0), Some(0), _] if matches == expected_matches => {
            Err("受影响的行数异常，本次修改已回滚。".into())
        }
        _ => Err(unknown("隧道返回的提交状态无效。".into())),
    }
}

fn edit_status<const N: usize>(
    reply: http_tunnel::Reply,
    names: [&str; N],
) -> Result<[Option<u64>; N], String> {
    let set = last_set(reply)?;
    if set
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .ne(names)
        || set.rows.len() != 1
    {
        return Err("隧道返回的提交状态不完整。".into());
    }
    let row = &set.rows[0];
    Ok(std::array::from_fn(|index| {
        row.get(index)
            .and_then(Option::as_deref)
            .and_then(|value| std::str::from_utf8(value).ok())
            .and_then(|value| value.parse().ok())
    }))
}

pub(in crate::database) fn http_delete_table_rows(
    session: &Session,
    object: DbObject,
    originals: Vec<TableRow>,
) -> Result<u64, String> {
    let groups = delete_row_groups(&object, originals)?;
    let layout = http_layout(session, object)?;
    if let Some(error) = &layout.edit_error {
        return Err(error.clone());
    }
    if layout.is_keyless() {
        require_gap_locks(session)?;
    }
    let (batch, expected) = delete_rows_batch(&layout, &groups)?;
    let payload = session.prepare_payload(&batch, true, "")?;
    let unknown =
        |reason| format!("无法确认 HTTP 隧道是否提交了本次删除。请刷新表格核对后再重试。{reason}");
    let reply = session.execute_payload(payload, 1).map_err(unknown)?;
    let names = [
        "layout_ok",
        "matches_ok",
        "warnings",
        "committed",
        "affected_rows",
    ];
    match edit_status(reply, names).map_err(unknown)? {
        [Some(0), _, _, Some(0), _] => Err(LAYOUT_CHANGED.into()),
        [Some(1), _, Some(warnings), Some(0), _] if warnings > 0 => {
            Err("数据库返回核对或删除警告，本次删除已回滚。".into())
        }
        [Some(1), Some(0), Some(0), Some(0), _] => {
            Err("部分所选行已变化或数量不足，未删除任何行。请刷新后重新选择。".into())
        }
        [Some(1), Some(1), Some(0), Some(0), _] => {
            Err("实际删除数量与选中数量不符，本次删除已回滚。".into())
        }
        [Some(1), Some(1), Some(0), Some(1), Some(affected)] if affected == expected => {
            Ok(affected)
        }
        _ => Err(unknown("隧道返回的提交状态无效。".into())),
    }
}

fn delete_rows_batch(
    layout: &TableLayout,
    groups: &[DeleteRowGroup],
) -> Result<(String, u64), String> {
    let table = layout.object.qualified_name();
    let keyless = layout.is_keyless();
    let expected = groups.iter().map(|group| group.count).sum::<u64>();
    let mut matches = groups
        .iter()
        .enumerate()
        .map(|(index, group)| {
            match_original(
                layout,
                &group.original,
                &format!("@harbor_original_row_{index}"),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let layout_ok =
        layout_guard_for_keys(layout, matches.iter().filter_map(|row| row.key), keyless);
    let mut batch = EDIT_SESSION.map(str::to_owned).to_vec();
    if keyless {
        batch.push("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ".into());
    }
    batch.extend([
        "START TRANSACTION".into(),
        format!("SELECT 1 FROM {table} LIMIT 0"),
        format!("SET @harbor_layout_ok = {layout_ok}"),
        "SET @harbor_matches_ok = 1, @harbor_warnings = 0, @harbor_counts_ok = 1, @harbor_deleted = 0".into(),
    ]);

    // Verify every selected snapshot under row and metadata locks before any write.
    let comparison = if keyless { ">=" } else { "=" };
    for (group, row) in groups.iter().zip(&mut matches) {
        batch.extend(row.exact_setup.take());
        batch.push(lock_row_query(&table, &row.predicate, keyless));
        batch.push(format!(
            "SET @harbor_matches_ok = (@harbor_matches_ok = 1 AND @harbor_row_matches {comparison} {}), \
             @harbor_warnings = GREATEST(@harbor_warnings, @@warning_count)",
            group.count
        ));
    }
    for (group, row) in groups.iter().zip(&matches) {
        batch.push(format!(
            "DELETE FROM {table} WHERE @harbor_layout_ok = 1 AND @harbor_matches_ok = 1 \
             AND @harbor_warnings = 0 AND @harbor_counts_ok = 1 AND ({}) LIMIT {}",
            row.predicate, group.count
        ));
        batch.push(
            "SET @harbor_affected = ROW_COUNT(), @harbor_warnings = GREATEST(@harbor_warnings, @@warning_count)".into(),
        );
        batch.push(format!(
            "SET @harbor_counts_ok = (@harbor_counts_ok = 1 AND @harbor_affected = {}), \
             @harbor_deleted = @harbor_deleted + @harbor_affected",
            group.count
        ));
    }
    batch.push(format!(
        "SET @harbor_accepted = (@harbor_layout_ok = 1 AND @harbor_matches_ok = 1 \
         AND @harbor_warnings = 0 AND @harbor_counts_ok = 1 AND @harbor_deleted = {expected})"
    ));
    batch.extend(FINISH_EDIT.map(str::to_owned));
    batch.push(
        "SELECT @harbor_layout_ok AS layout_ok, @harbor_matches_ok AS matches_ok, @harbor_warnings AS warnings, \
         @harbor_accepted AS committed, @harbor_deleted AS affected_rows".into(),
    );
    let commands = batch.iter().map(String::as_str).collect::<Vec<_>>();
    Ok((batch_statements(&commands)?, expected))
}

fn require_gap_locks(session: &Session) -> Result<(), String> {
    // Older InnoDB builds can disable next-key protection even at REPEATABLE READ.
    // This is a static server option, and absent on modern versions.
    let settings = session
        .metadata_columns::<2>(
            "SHOW GLOBAL VARIABLES LIKE 'innodb_locks_unsafe_for_binlog'",
            1,
        )
        .map_err(Failure::message)?;
    match settings.as_slice() {
        [] => Ok(()),
        [[_, Some(value)]] if value.eq_ignore_ascii_case("OFF") || value == "0" => Ok(()),
        _ => Err("服务器未启用 InnoDB 间隙锁，无法确认无键数据行的编辑范围。".into()),
    }
}

struct EditMatch<'a> {
    predicate: String,
    key: Option<&'a EditKey>,
    exact_setup: Option<String>,
}

fn match_original<'a>(
    layout: &'a TableLayout,
    original: &TableRow,
    keyless_parameter: &str,
) -> Result<EditMatch<'a>, String> {
    if original.layout.is_keyless() {
        layout.recheck_keyless(original)?;
        let original = original
            .fingerprint
            .as_ref()
            .filter(|value| valid_fingerprint(layout, value))
            .ok_or("请重新读取该行后再编辑。")?;
        let bytes = exact_row_expression(layout, None);
        return Ok(EditMatch {
            predicate: format!("CAST({bytes} AS BINARY) = CAST({keyless_parameter} AS BINARY)"),
            key: None,
            // Validation above limits the token to ASCII hex, '/' and 'N'.
            exact_setup: Some(format!("SET {keyless_parameter} = '{original}'")),
        });
    }
    let key = layout.recheck_key(original)?;
    Ok(EditMatch {
        predicate: original_predicate(layout, key, original)?,
        key: Some(key),
        exact_setup: None,
    })
}

fn lock_row_query(table: &str, predicate: &str, keyless: bool) -> String {
    // LIMIT materializes the derived query so its FOR UPDATE locks the actual rows.
    // Read the complete range for keyless rows, including gaps preventing new matches.
    let limit = if keyless { u64::MAX } else { 2 };
    format!(
        "SELECT COUNT(*) INTO @harbor_row_matches FROM (
            SELECT 1 FROM {table} WHERE @harbor_layout_ok = 1 AND {predicate} LIMIT {limit} FOR UPDATE
        ) AS harbor_matching_rows"
    )
}

pub(super) fn original_predicate(
    layout: &TableLayout,
    key: &EditKey,
    original: &TableRow,
) -> Result<String, String> {
    let fingerprint = original
        .fingerprint
        .as_ref()
        .ok_or("请重新读取该行后再编辑。")?;
    let mut conditions = original_key_conditions(layout, key, original)?;
    conditions.push(format!(
        "BINARY {} = {}",
        fingerprint_expression(layout, None),
        mysql_text_literal(fingerprint)
    ));
    Ok(conditions.join(" AND "))
}

pub(super) fn original_key_conditions(
    layout: &TableLayout,
    key: &EditKey,
    original: &TableRow,
) -> Result<Vec<String>, String> {
    key.columns
        .iter()
        .map(|&index| {
            let column = &layout.columns[index];
            let value = &original.original()[index];
            let literal = match column.value_type {
                ValueType::Decimal { precision, scale } => format!(
                    "CAST({} AS DECIMAL({precision}, {scale}))",
                    literal(value, false)?
                ),
                ValueType::Bit(_) => bit_number(value)
                    .ok_or("无法读取用于定位的 BIT 值。")?
                    .to_string(),
                _ => literal(value, column.is_binary())?,
            };
            Ok(format!("{} <=> {literal}", quote_identifier(&column.name)))
        })
        .collect()
}

pub(in crate::database) fn layout_guard(layout: &TableLayout) -> String {
    layout_guard_for_keys(layout, None, false)
}

pub(in crate::database) fn import_layout_guard(layout: &TableLayout, staged_engine: &str) -> String {
    let engine = mysql_text_literal(staged_engine);
    let engine_filter = if crate::database::odbc_target::supports_staged_nontransactional_engine(staged_engine) {
        format!("UPPER(t.ENGINE) = UPPER({engine})")
    } else {
        format!("UPPER(t.ENGINE) = UPPER({engine}) AND e.TRANSACTIONS = 'YES'")
    };
    layout_guard_for_engine(layout, None, false, &engine_filter)
}

pub(in crate::database) fn selected_key_layout_guard(layout: &TableLayout) -> String {
    layout_guard_for_keys(layout, layout.edit_keys.get(layout.selected_key), false)
}

fn layout_guard_for_keys<'a>(
    layout: &TableLayout,
    keys: impl IntoIterator<Item = &'a EditKey>,
    keyless: bool,
) -> String {
    layout_guard_for_engine(layout, keys, keyless, "e.TRANSACTIONS = 'YES'")
}

fn layout_guard_for_engine<'a>(
    layout: &TableLayout,
    keys: impl IntoIterator<Item = &'a EditKey>,
    keyless: bool,
    engine_filter: &str,
) -> String {
    let object = &layout.object;
    let filter = MATCH_TABLE
        .replace(":schema", &mysql_text_literal(&object.schema))
        .replace(":table", &mysql_text_literal(&object.name));
    let table_types = if keyless {
        "t.TABLE_TYPE = 'BASE TABLE' AND t.ENGINE = 'InnoDB'"
    } else {
        "t.TABLE_TYPE IN ('BASE TABLE','SYSTEM VERSIONED')"
    };
    let table_ok = format!(
        "EXISTS (SELECT 1 FROM information_schema.TABLES t JOIN information_schema.ENGINES e ON t.ENGINE = e.ENGINE WHERE {filter} AND {table_types} AND {engine_filter})"
    );
    let columns = layout
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let attributes = [
                ("COLUMN_NAME", Some(column.name.as_str())),
                ("COLUMN_TYPE", Some(column.data_type.as_str())),
                (
                    "IS_NULLABLE",
                    Some(if column.nullable { "YES" } else { "NO" }),
                ),
                ("COLUMN_DEFAULT", column.default_value.as_deref()),
                ("EXTRA", Some(column.extra.as_str())),
                (
                    "GENERATION_EXPRESSION",
                    column.generation_expression.as_deref(),
                ),
                ("CHARACTER_SET_NAME", column.charset.as_deref()),
                ("COLLATION_NAME", column.collation.as_deref()),
            ]
            .into_iter()
            .map(|(field, expected)| {
                format!(
                    "BINARY t.{field} <=> {}",
                    expected.map_or_else(|| "NULL".into(), mysql_text_literal),
                )
            })
            .collect::<Vec<_>>()
            .join(" AND ");
            format!("WHEN {} THEN ({attributes})", index + 1)
        })
        .collect::<Vec<_>>()
        .join(" ");
    let column_count = layout.columns.len();
    let columns_ok = format!(
        "(SELECT COUNT(*) = {column_count} AND COALESCE(SUM(CASE t.ORDINAL_POSITION {columns} ELSE 0 END),0) = {column_count} FROM information_schema.COLUMNS t WHERE {filter})"
    );
    let index_ok = |name, columns: &[usize]| {
        let index_name = mysql_text_literal(name);
        let match_index =
            format!("{filter} AND BINARY t.INDEX_NAME = {index_name} AND t.NON_UNIQUE = 0");
        let count = columns.len();
        if count == 0 {
            return format!(
                "NOT EXISTS (SELECT 1 FROM information_schema.STATISTICS t WHERE {match_index})"
            );
        }
        let prefix = if name == "PRIMARY" {
            ""
        } else {
            " AND t.SUB_PART IS NULL"
        };
        let parts = columns
            .iter()
            .enumerate()
            .map(|(position, &index)| {
                format!(
                    "WHEN {} THEN (BINARY t.COLUMN_NAME <=> {}{prefix})",
                    position + 1,
                    mysql_text_literal(&layout.columns[index].name)
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "(SELECT COUNT(*) = {count} AND COALESCE(SUM(CASE t.SEQ_IN_INDEX {parts} ELSE 0 END),0) = {count} FROM information_schema.STATISTICS t WHERE {match_index})"
        )
    };
    let primary = index_ok("PRIMARY", &layout.primary_key);
    let alternate = if keyless {
        // No complete non-null unique index may have appeared since the row was read.
        format!(
            " AND NOT EXISTS (
                SELECT 1 FROM information_schema.STATISTICS t
                LEFT JOIN information_schema.COLUMNS c
                    ON CAST(c.TABLE_SCHEMA AS BINARY) = CAST(t.TABLE_SCHEMA AS BINARY)
                    AND CAST(c.TABLE_NAME AS BINARY) = CAST(t.TABLE_NAME AS BINARY)
                    AND CAST(c.COLUMN_NAME AS BINARY) = CAST(t.COLUMN_NAME AS BINARY)
                WHERE {filter} AND t.NON_UNIQUE = 0
                GROUP BY t.INDEX_NAME
                HAVING COUNT(*) = COALESCE(SUM(t.SUB_PART IS NULL AND c.IS_NULLABLE = 'NO'),0)
            )"
        )
    } else {
        let mut seen = HashSet::new();
        keys.into_iter()
            .filter(|key| key.index != "PRIMARY" && seen.insert(key.index.as_str()))
            .map(|key| format!(" AND {}", index_ok(&key.index, &key.columns)))
            .collect::<String>()
    };
    format!("({table_ok} AND {columns_ok} AND {primary}{alternate})")
}

pub(super) fn write_values(
    layout: &TableLayout,
    values: Vec<(String, FieldValue)>,
) -> Result<Vec<(String, Option<String>)>, String> {
    let binary = values
        .iter()
        .map(|(name, _)| column_index(layout, name).map(|index| layout.columns[index].is_binary()))
        .collect::<Result<Vec<_>, _>>()?;
    prepare_values(layout, values)?
        .into_iter()
        .zip(binary)
        .map(|((column, value), binary)| {
            Ok((
                column,
                value
                    .as_ref()
                    .map(|value| literal(value, binary))
                    .transpose()?,
            ))
        })
        .collect()
}

fn bind_literals(sql: &str, parameters: &[Value]) -> Result<String, String> {
    let mut values = parameters.iter();
    let mut quote = None;
    let mut out = String::new();
    for character in sql.chars() {
        if let Some(current) = quote {
            if character == current {
                quote = None;
            }
        } else if matches!(character, '\'' | '"' | '\x60') {
            quote = Some(character);
        } else if character == '?' {
            out.push_str(&literal(values.next().ok_or("筛选参数数量不符。")?, false)?);
            continue;
        }
        out.push(character);
    }
    if values.next().is_some() {
        return Err("筛选参数数量不符。".into());
    }
    Ok(out)
}

fn literal(value: &Value, binary: bool) -> Result<String, String> {
    match value {
        Value::Bytes(bytes) if binary => {
            let mut out = String::from("X'");
            for byte in bytes {
                write!(out, "{byte:02X}").unwrap();
            }
            out.push('\'');
            Ok(out)
        }
        Value::Bytes(bytes) => std::str::from_utf8(bytes)
            .map(mysql_text_literal)
            .map_err(|_| "隧道的文本参数必须使用 UTF-8。".into()),
        Value::Float(value) if !value.is_finite() => Err("隧道参数需要有限数值。".into()),
        Value::Double(value) if !value.is_finite() => Err("隧道参数需要有限数值。".into()),
        _ => Ok(value.as_sql(true)),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::database::{
        Command, ConnectionConfig, Response, Worker,
        http_session::workflow_test::{TestGateway, fixture_database},
    };

    use super::*;

    #[test]
    fn http_batch_delete_locks_all_originals_and_limits_each_duplicate_group() {
        let object = DbObject {
            schema: "warehouse".into(),
            name: "items".into(),
            kind: ObjectKind::Table,
        };
        let layout = Arc::new(TableLayout {
            object: object.clone(),
            columns: vec![TableColumn {
                name: "payload".into(),
                data_type: "varbinary(8)".into(),
                nullable: false,
                default_value: None,
                writable: true,
                auto_increment: false,
                charset: None,
                collation: None,
                declared_width: Some(8),
                generation_expression: None,
                extra: String::new(),
                value_type: ValueType::Binary,
            }],
            primary_key: Vec::new(),
            edit_error: None,
            edit_keys: Vec::new(),
            selected_key: 0,
        });
        let row = |byte| TableRow {
            cells: vec![Cell::Binary(vec![byte])],
            layout: Arc::clone(&layout),
            original: vec![Value::Bytes(vec![byte])].into(),
            fingerprint: Some(format!("{byte:02X}")),
        };
        let groups = delete_row_groups(&object, vec![row(0x00), row(0x00), row(0xFF)]).unwrap();
        let (batch, count) = delete_rows_batch(&layout, &groups).unwrap();
        assert_eq!(count, 3);
        let (checks, mutations) = batch.split_once("DELETE FROM").unwrap();
        assert!(checks.contains("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ"));
        assert_eq!(checks.matches("FOR UPDATE").count(), 2);
        assert!(checks.contains("SET @harbor_original_row_0 = '00'"));
        assert!(checks.contains("SET @harbor_original_row_1 = 'FF'"));
        assert!(checks.contains("@harbor_row_matches >= 2"));
        assert!(checks.contains("@harbor_row_matches >= 1"));
        assert!(mutations.contains("CAST(@harbor_original_row_0 AS BINARY)) LIMIT 2"));
        assert!(mutations.contains("CAST(@harbor_original_row_1 AS BINARY)) LIMIT 1"));
        assert!(mutations.contains("@harbor_counts_ok = 1 AND @harbor_deleted = 3"));
        assert_eq!(
            mutations.matches("COMMIT AND NO CHAIN NO RELEASE").count(),
            1
        );
        assert!(mutations.contains("ROLLBACK AND NO CHAIN NO RELEASE"));
    }

    #[test]
    #[ignore = "requires a disposable MariaDB via HARBOR_HTTP_FIXTURE_PORT"]
    fn http_keyless_edit_confirms_one_and_all_exact_duplicates() {
        let (port, opts) = fixture_database();
        let schema = format!("harbor_http_keyless_{}", std::process::id());
        let mut sql = Conn::new(opts.clone()).unwrap();
        sql.query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
            .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sql.select_db(&schema).unwrap();
            sql.query_drop(
                "CREATE TABLE items (
                    name VARCHAR(24) COLLATE utf8mb4_general_ci,
                    marker INT NOT NULL,
                    payload VARBINARY(12)
                ) ENGINE=InnoDB",
            )
            .unwrap();
            sql.query_drop(
                "INSERT INTO items VALUES
                    ('原值a', 0, X'00'), ('原值a', 0, X'00'), ('原值a', 0, X'00'),
                    ('原值A', 0, X'00'), ('原值a ', 0, X'00'),
                    ('原值a', 0, NULL), ('原值a', 0, X'')",
            )
            .unwrap();
            let gateway = TestGateway::new(opts);
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
            assert!(matches!(
                request(Command::ConnectHttp(
                    ConnectionConfig {
                        port,
                        database: Some(schema.clone()),
                        ..Default::default()
                    },
                    gateway.tunnel.clone(),
                )),
                Response::Connected(_)
            ));
            let object = DbObject {
                schema: schema.clone(),
                name: "items".into(),
                kind: ObjectKind::Table,
            };
            let page = || Command::TablePage {
                object: object.clone(),
                limit: 20,
                offset: 0,
            };
            let Response::TablePage(loaded) = request(page()) else {
                panic!("expected a table page");
            };
            assert!(loaded.row_key.is_empty() && loaded.edit_error.is_none());
            let original_row = |page: &TablePage| {
                let name = Value::Bytes("原值a".as_bytes().to_vec());
                let payload = Value::Bytes(vec![0]);
                page.rows
                    .iter()
                    .find(|row| row.original() == [name.clone(), Value::Int(0), payload.clone()])
                    .unwrap()
                    .clone()
            };
            let update = TableEdit::Update {
                original: original_row(&loaded),
                values: vec![("marker".into(), FieldValue::Text("1".into()))],
            };
            assert!(matches!(
                request(Command::EditTable {
                    object: object.clone(),
                    edit: update.clone(),
                }),
                Response::TableEditConfirmation { matches: 3 }
            ));
            let updated = |sql: &mut Conn| {
                sql.query_first::<u64, _>("SELECT COUNT(*) FROM items WHERE marker <> 0")
                    .unwrap()
            };
            assert_eq!(updated(&mut sql), Some(0));
            assert!(matches!(
                request(Command::ConfirmTableEdit {
                    object: object.clone(),
                    edit: update,
                    choice: DuplicateChoice {
                        matches: 3,
                        scope: DuplicateScope::One,
                    },
                }),
                Response::TableEdited { affected_rows: 1 }
            ));
            assert_eq!(updated(&mut sql), Some(1));
            let Response::TablePage(refreshed) = request(page()) else {
                panic!("expected the refreshed table page");
            };
            let delete = TableEdit::Delete {
                original: original_row(&refreshed),
            };
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
                        scope: DuplicateScope::All,
                    },
                }),
                Response::TableEdited { affected_rows: 2 }
            ));
            let leftovers: Vec<(String, i32, Option<Vec<u8>>)> = sql
                .query("SELECT name, marker, payload FROM items")
                .unwrap();
            assert_eq!(leftovers.len(), 5);
            for expected in [
                ("原值a", 1, Some(vec![0])),
                ("原值A", 0, Some(vec![0])),
                ("原值a ", 0, Some(vec![0])),
                ("原值a", 0, None),
                ("原值a", 0, Some(vec![])),
            ] {
                assert!(leftovers.contains(&(expected.0.into(), expected.1, expected.2)));
            }
            gateway.stop();
        }));
        sql.query_drop(format!("DROP DATABASE IF EXISTS {schema}"))
            .unwrap();
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }
}
