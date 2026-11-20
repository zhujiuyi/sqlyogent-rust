use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use mysql::prelude::Queryable;
use mysql::{AccessMode, Conn, IsolationLevel, Opts, Params, Transaction, TxOpts, Value, params};
use sha2::{Digest, Sha256};

use super::transfer::dependency_order;
use super::{DbObject, ObjectKind, check_write_warnings, mysql_error, quote_identifier};

mod script;
mod transport;
use transport::ReadTransport;
pub(super) use transport::{
    Transport, apply_many_transported, apply_transported, compare_many_transported,
    compare_transported, export_transported,
};

const PAGE_ROWS: usize = 128;
const PAGE_BYTES: usize = 8 * 1024 * 1024;
const PROBE_SQL_BYTES: usize = 256 * 1024;
const SAMPLES_PER_KIND: u64 = 20;
const MATCH_TABLE: &str = "
    t.TABLE_SCHEMA = CONVERT(:schema USING utf8mb4)
    AND t.TABLE_NAME = CONVERT(:table USING utf8mb4)
    AND CAST(IF(@@lower_case_table_names = 0, t.TABLE_SCHEMA, LOWER(t.TABLE_SCHEMA)) AS BINARY)
        = CAST(IF(@@lower_case_table_names = 0,
            CONVERT(:schema USING utf8mb4), LOWER(CONVERT(:schema USING utf8mb4))) AS BINARY)
    AND CAST(IF(@@lower_case_table_names = 0, t.TABLE_NAME, LOWER(t.TABLE_NAME)) AS BINARY)
        = CAST(IF(@@lower_case_table_names = 0,
            CONVERT(:table USING utf8mb4), LOWER(CONVERT(:table USING utf8mb4))) AS BINARY)";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncMode {
    InsertMissing,
    InsertAndUpdate,
    MirrorReceiver,
    ReconcileBoth(SyncConflict),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncPolicy {
    pub mode: SyncMode,
    pub skip_target_fk_checks: bool,
    pub continue_on_row_error: bool,
}

impl From<SyncMode> for SyncPolicy {
    fn from(mode: SyncMode) -> Self {
        Self {
            mode,
            skip_target_fk_checks: false,
            continue_on_row_error: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncConflict {
    KeepSeparate,
    SourceWins,
    TargetWins,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncFilterScope {
    Source,
    Both,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncFilter {
    original: String,
    scope: SyncFilterScope,
    parts: Vec<FilterPart>,
    uses_json: bool,
}

#[derive(Clone)]
pub struct SyncMapping {
    pub source: DbObject,
    pub target: DbObject,
    pub filter: Option<SyncFilter>,
    pub compare_columns: Vec<String>,
}

impl SyncMapping {
    pub(super) fn all(source: DbObject, target: DbObject) -> Self {
        Self {
            source,
            target,
            filter: None,
            compare_columns: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum FilterPart {
    Sql(String),
    Now(FilterClock, usize),
    Table(FilterTable),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FilterTable {
    schema: Option<String>,
    name: String,
}

impl FilterTable {
    fn resolve(&self, schema: &str) -> DbObject {
        DbObject {
            schema: self.schema.as_deref().unwrap_or(schema).to_owned(),
            name: self.name.clone(),
            kind: ObjectKind::Table,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FilterClock {
    Date,
    Time,
    DateTime,
}

#[derive(PartialEq)]
enum FilterToken<'a> {
    Word(&'a str),
    Identifier(String),
    Text(Vec<u8>),
    Raw(&'a str),
}

impl SyncFilter {
    pub fn new(predicate: &str, scope: SyncFilterScope) -> Result<Self, String> {
        let original = predicate.trim();
        let tokens = filter_tokens(original)?;
        let tokens = if matches!(tokens.first(), Some(FilterToken::Word(word)) if word.eq_ignore_ascii_case("WHERE"))
        {
            &tokens[1..]
        } else {
            &tokens
        };
        if tokens.is_empty() {
            return Err("请输入 WHERE 条件。".into());
        }
        let mut select_roles = filter_select_roles(tokens)?;
        let mut parts = Vec::with_capacity(tokens.len());
        let mut uses_json = false;
        let mut index = 0;
        while let Some(token) = tokens.get(index) {
            if let Some(role) = select_roles.remove(&index) {
                match role {
                    FilterRole::Keyword(keyword) => {
                        parts.push(FilterPart::Sql(keyword.into()));
                        index += 1;
                    }
                    FilterRole::Table { table, end } => {
                        parts.push(FilterPart::Table(table));
                        index = end;
                    }
                }
                continue;
            }
            index += 1;
            let following = tokens.get(index);
            let call = following == Some(&FilterToken::Raw("("));
            let previous = index.checked_sub(2).and_then(|index| tokens.get(index));
            let qualified = previous == Some(&FilterToken::Raw("."))
                || following == Some(&FilterToken::Raw("."));
            if call && qualified {
                return Err("WHERE 条件不能调用带库名的函数。".into());
            }
            let part = match token {
                FilterToken::Raw(raw) => FilterPart::Sql((*raw).into()),
                FilterToken::Text(bytes) => FilterPart::Sql(filter_literal("_utf8mb4", bytes)),
                FilterToken::Identifier(name) if !call => FilterPart::Sql(quote_identifier(name)),
                FilterToken::Identifier(_) => return Err("WHERE 条件不能调用带引号的函数。".into()),
                FilterToken::Word(value) if qualified => FilterPart::Sql(quote_identifier(value)),
                FilterToken::Word(value) => {
                    let upper = value.to_ascii_uppercase();
                    let word = upper.as_str();
                    let previous_word = |keyword: &str| {
                        matches!(
                            previous, Some(FilterToken::Word(value)) if value.eq_ignore_ascii_case(keyword)
                        )
                    };
                    if forbidden_filter_word(word) || (word == "SET" && !previous_word("CHARACTER"))
                    {
                        return Err(format!("WHERE 条件不能包含 {word}。"));
                    }
                    if let Some(clock) = filter_clock(word, call) {
                        let mut precision = 0;
                        if call {
                            index += 1;
                            if let Some(FilterToken::Raw(raw)) = tokens.get(index)
                                && let Ok(value) = raw.parse::<usize>()
                                && value <= 6
                                && clock != FilterClock::Date
                            {
                                precision = value;
                                index += 1;
                            }
                            if tokens.get(index) != Some(&FilterToken::Raw(")")) {
                                return Err("当前日期或时间函数的参数无效。".into());
                            }
                            index += 1;
                        }
                        FilterPart::Now(clock, precision)
                    } else if let Some(FilterToken::Text(bytes)) = following
                        && matches!(
                            word,
                            "N" | "_UTF8" | "_UTF8MB4" | "_BINARY" | "DATE" | "TIME" | "TIMESTAMP"
                        )
                    {
                        index += 1;
                        let literal = filter_literal(
                            if word == "_BINARY" {
                                "_binary"
                            } else {
                                "_utf8mb4"
                            },
                            bytes,
                        );
                        let rendered = match word {
                            "DATE" | "TIME" => format!("CAST({literal} AS {word})"),
                            "TIMESTAMP" => format!("CAST({literal} AS DATETIME)"),
                            _ => literal,
                        };
                        FilterPart::Sql(rendered)
                    } else if call
                        && (pure_filter_function(word)
                            || json_filter_function(word)
                            || (matches!(word, "DECIMAL" | "DATETIME" | "NCHAR" | "BINARY")
                                && previous_word("AS")))
                    {
                        uses_json |= json_filter_function(word);
                        index += 1;
                        FilterPart::Sql(format!("{word}("))
                    } else if filter_keyword(word) && (!call || filter_grouping_keyword(word)) {
                        FilterPart::Sql(upper)
                    } else if !call {
                        let charset_or_collation = matches!(previous, Some(FilterToken::Word(keyword))
                            if keyword.eq_ignore_ascii_case("COLLATE") ||
                               keyword.eq_ignore_ascii_case("USING") ||
                               keyword.eq_ignore_ascii_case("SET"));
                        FilterPart::Sql(if charset_or_collation && sql_word(value) {
                            (*value).into()
                        } else {
                            quote_identifier(value)
                        })
                    } else {
                        return Err(format!("WHERE 条件不支持函数「{value}」。"));
                    }
                }
            };
            parts.push(part);
        }
        Ok(Self {
            original: original.into(),
            scope,
            parts,
            uses_json,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.original
    }

    pub fn scope(&self) -> SyncFilterScope {
        self.scope
    }

    fn sql_at(&self, schema: &str, now: std::time::Duration) -> String {
        self.parts
            .iter()
            .map(|part| match part {
                FilterPart::Sql(sql) => sql.clone(),
                FilterPart::Table(table) => table.resolve(schema).qualified_name(),
                FilterPart::Now(clock, precision) => {
                    let seconds = now.as_secs();
                    let epoch = if *precision == 0 {
                        seconds.to_string()
                    } else {
                        let precision = *precision;
                        let fraction = now.subsec_micros() / 10_u32.pow(6 - precision as u32);
                        format!("{seconds}.{fraction:0precision$}")
                    };
                    let literal = format!("FROM_UNIXTIME({epoch})");
                    match clock {
                        FilterClock::Date => format!("DATE({literal})"),
                        FilterClock::Time => format!("TIME({literal})"),
                        FilterClock::DateTime => literal,
                    }
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn tables(&self, schema: &str) -> Vec<DbObject> {
        let mut seen = BTreeSet::new();
        self.parts
            .iter()
            .filter_map(|part| {
                let FilterPart::Table(table) = part else {
                    return None;
                };
                let object = table.resolve(schema);
                seen.insert((object.schema.clone(), object.name.clone()))
                    .then_some(object)
            })
            .collect()
    }

    fn ensure_functions(&self, endpoint: &EndpointIdentity) -> Result<(), String> {
        if !self.uses_json {
            return Ok(());
        }
        let version = endpoint.server.get("version").map_or("", String::as_str);
        let mariadb = version.to_ascii_lowercase().contains("mariadb");
        let minimum = if mariadb { (10, 2, 7) } else { (5, 7, 8) };
        let supported = version
            .split('-')
            .take(if mariadb { 2 } else { 1 })
            .any(|part| {
                let mut numbers = part.split('.');
                let tuple = (|| {
                    Some((
                        numbers.next()?.parse::<u32>().ok()?,
                        numbers.next()?.parse::<u32>().ok()?,
                        numbers.next()?.parse::<u32>().ok()?,
                    ))
                })();
                tuple.is_some_and(|tuple| tuple >= minimum)
            });
        if !supported {
            return Err(format!(
                "无法确认服务器 {version} 提供 WHERE 所用的内置 JSON 函数。"
            ));
        }
        Ok(())
    }
}

enum FilterRole {
    Keyword(String),
    Table { table: FilterTable, end: usize },
}

#[derive(PartialEq, Eq)]
enum FilterSelectClause {
    Projection,
    Tables,
    Conditions,
}

fn filter_select_roles(tokens: &[FilterToken<'_>]) -> Result<BTreeMap<usize, FilterRole>, String> {
    let mut groups = Vec::new();
    let mut roles = BTreeMap::new();
    let word_at = |index| match tokens.get(index) {
        Some(FilterToken::Word(word)) => Some(*word),
        _ => None,
    };
    let word_is =
        |index, word| word_at(index).is_some_and(|value| value.eq_ignore_ascii_case(word));
    let mut index = 0;
    while let Some(token) = tokens.get(index) {
        let position = index;
        index += 1;
        match token {
            FilterToken::Raw("(") => {
                let select = word_is(index, "SELECT");
                groups.push(select.then_some(FilterSelectClause::Projection));
                if select {
                    roles.insert(index, FilterRole::Keyword("SELECT".into()));
                }
                continue;
            }
            FilterToken::Raw(")") => {
                groups.pop();
                continue;
            }
            _ => {}
        }
        let qualified = position.checked_sub(1).and_then(|i| tokens.get(i))
            == Some(&FilterToken::Raw("."))
            || tokens.get(index) == Some(&FilterToken::Raw("."));
        if qualified {
            continue;
        }
        let word = word_at(position).map(str::to_ascii_uppercase);
        if let Some(operator @ ("EXISTS" | "ANY" | "SOME" | "ALL")) = word.as_deref()
            && tokens.get(index) == Some(&FilterToken::Raw("("))
            && word_is(index + 1, "SELECT")
        {
            roles.insert(position, FilterRole::Keyword(operator.into()));
            continue;
        }
        let Some(Some(clause)) = groups.last_mut() else {
            continue;
        };
        if word.as_deref() == Some("FOR") {
            return Err("WHERE 子查询不能使用加锁读取。".into());
        }
        let from = word.as_deref() == Some("FROM") && *clause == FilterSelectClause::Projection;
        let join = word.as_deref() == Some("JOIN") && *clause == FilterSelectClause::Tables;
        if from
            || join
            || (matches!(token, FilterToken::Raw(",")) && *clause == FilterSelectClause::Tables)
        {
            if let Some(word) = word {
                roles.insert(position, FilterRole::Keyword(word));
            }
            let (table, end) = filter_table_reference(tokens, index)?;
            roles.insert(index, FilterRole::Table { table, end });
            index = end;
            *clause = FilterSelectClause::Tables;
            continue;
        }

        let Some(word) = word else {
            continue;
        };
        let previous_is = |keyword| position.checked_sub(1).is_some_and(|i| word_is(i, keyword));
        let keyword = match word.as_str() {
            "WHERE" | "HAVING" | "LIMIT" | "OFFSET" => {
                *clause = FilterSelectClause::Conditions;
                true
            }
            "GROUP" | "ORDER" if word_is(index, "BY") => {
                *clause = FilterSelectClause::Conditions;
                true
            }
            "BY" => previous_is("GROUP") || previous_is("ORDER"),
            "ASC" | "DESC" => *clause == FilterSelectClause::Conditions,
            "DISTINCT" | "ALL" => *clause == FilterSelectClause::Projection,
            "ON" | "USING" => *clause == FilterSelectClause::Tables,
            "INNER" | "LEFT" | "RIGHT" | "CROSS" | "OUTER" => {
                *clause == FilterSelectClause::Tables
                    && (word_is(index, "JOIN")
                        || word_is(index, "OUTER") && word_is(index + 1, "JOIN"))
            }
            _ => false,
        };
        if keyword {
            roles.insert(position, FilterRole::Keyword(word));
        }
    }
    Ok(roles)
}

fn filter_table_reference(
    tokens: &[FilterToken<'_>],
    start: usize,
) -> Result<(FilterTable, usize), String> {
    let name_at = |index| match tokens.get(index) {
        Some(FilterToken::Identifier(name)) => Some(name.clone()),
        Some(FilterToken::Word(name)) => {
            let word = name.to_ascii_uppercase();
            (!forbidden_filter_word(&word)
                && !matches!(
                    word.as_str(),
                    "FROM" | "WHERE" | "JOIN" | "HAVING" | "GROUP" | "ORDER" | "LIMIT" | "FOR"
                ))
            .then(|| (*name).into())
        }
        _ => None,
    };
    let missing = || "WHERE 子查询的 FROM 和 JOIN 需要指定表名。".to_owned();
    let first = name_at(start).ok_or_else(missing)?;
    let (schema, name, end) = if tokens.get(start + 1) == Some(&FilterToken::Raw(".")) {
        (
            Some(first),
            name_at(start + 2).ok_or_else(missing)?,
            start + 3,
        )
    } else {
        (None, first, start + 1)
    };
    if matches!(tokens.get(end), Some(FilterToken::Raw("(" | "."))) {
        return Err("WHERE 子查询只支持普通表名。".into());
    }
    Ok((FilterTable { schema, name }, end))
}

fn filter_tokens(input: &str) -> Result<Vec<FilterToken<'_>>, String> {
    let mut tail = input;
    let mut tokens = Vec::new();
    let mut depth = 0usize;
    loop {
        tail = tail.trim_start_matches(|ch: char| ch.is_ascii_whitespace());
        if tail.is_empty() {
            break;
        }
        let bytes = tail.as_bytes();
        let ch = tail.chars().next().unwrap();
        let (token, width) = if matches!(ch, '\'' | '"' | '\x60') {
            let (width, decoded) = filter_quote(bytes, bytes[0])?;
            let token = if ch == '\x60' {
                let name = String::from_utf8(decoded).map_err(|_| "WHERE 列名编码无效。")?;
                if name.is_empty() || name.contains('\0') {
                    return Err("WHERE 列名无效。".into());
                }
                FilterToken::Identifier(name)
            } else {
                FilterToken::Text(decoded)
            };
            (token, width)
        } else if matches!(bytes, [b'x' | b'X' | b'b' | b'B', b'\'', ..]) {
            let (width, characters) = filter_quote(&bytes[1..], b'\'')?;
            let expected = &bytes[2..width];
            let is_hex = matches!(bytes[0], b'x' | b'X');
            if characters != expected
                || !characters.iter().all(|byte| {
                    if is_hex {
                        byte.is_ascii_hexdigit()
                    } else {
                        matches!(byte, b'0' | b'1')
                    }
                })
            {
                return Err("WHERE 十六进制或位串字面量无效。".into());
            }
            (FilterToken::Raw(&tail[..width + 1]), width + 1)
        } else if ch.is_ascii_digit() || (ch == '.' && bytes.get(1).is_some_and(u8::is_ascii_digit))
        {
            let width = filter_number_width(bytes);
            (FilterToken::Raw(&tail[..width]), width)
        } else if ch == '_' || ch == '$' || ch.is_alphabetic() {
            let width = tail
                .char_indices()
                .find_map(|(offset, character)| {
                    (!(character == '_' || character == '$' || character.is_alphanumeric()))
                        .then_some(offset)
                })
                .unwrap_or(tail.len());
            (FilterToken::Word(&tail[..width]), width)
        } else {
            if tail.starts_with("/*") || tail.starts_with("--") || ch == '#' {
                return Err("WHERE 条件不能包含 SQL 注释。".into());
            }
            let op = [
                "<=>", "->>", "->", "<=", ">=", "<>", "!=", "<<", ">>", "&&", "||",
            ]
            .into_iter()
            .find(|op| tail.starts_with(op));
            if let Some(op) = op {
                (
                    FilterToken::Raw(match op {
                        "&&" => "AND",
                        "||" => "OR",
                        _ => op,
                    }),
                    op.len(),
                )
            } else if ch.is_ascii() && "().,=<>+-*/%~!&|^".contains(ch) {
                if ch == '(' {
                    depth += 1;
                } else if ch == ')' {
                    depth = depth.checked_sub(1).ok_or("WHERE 条件的括号不匹配。")?;
                }
                (FilterToken::Raw(&tail[..1]), 1)
            } else {
                return Err("WHERE 条件包含不支持的符号或语句分隔符。".into());
            }
        };
        tokens.push(token);
        tail = &tail[width..];
    }
    if depth != 0 {
        return Err("WHERE 条件的括号不匹配。".into());
    }
    Ok(tokens)
}

fn filter_quote(bytes: &[u8], quote: u8) -> Result<(usize, Vec<u8>), String> {
    let mut decoded = Vec::new();
    let mut index = 1;
    while let Some(&byte) = bytes.get(index) {
        index += 1;
        if byte == quote {
            if bytes.get(index) != Some(&quote) {
                return Ok((index, decoded));
            }
            index += 1;
            decoded.push(quote);
        } else if byte == b'\\' && quote != b'\x60' {
            let escape = *bytes.get(index).ok_or("WHERE 字符串转义不完整。")?;
            index += 1;
            let escaped = match escape {
                b'0' => 0,
                b'b' => 8,
                b'n' => b'\n',
                b'r' => b'\r',
                b't' => b'\t',
                b'Z' => 26,
                b'%' | b'_' => {
                    decoded.push(b'\\');
                    escape
                }
                _ => escape,
            };
            decoded.push(escaped);
        } else {
            decoded.push(byte);
        }
    }
    Err("WHERE 条件含未闭合的引号。".into())
}

fn filter_number_width(bytes: &[u8]) -> usize {
    if let [b'0', b'x' | b'X', digits @ ..] = bytes {
        return 2 + digits
            .iter()
            .take_while(|byte| byte.is_ascii_hexdigit())
            .count();
    }
    if let [b'0', b'b' | b'B', digits @ ..] = bytes {
        return 2 + digits
            .iter()
            .take_while(|&&byte| matches!(byte, b'0' | b'1'))
            .count();
    }
    let mut width = bytes
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if bytes.get(width) == Some(&b'.') {
        width += 1 + bytes[width + 1..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
    }
    if matches!(bytes.get(width), Some(b'e' | b'E')) {
        let mut from = width + 1;
        from += usize::from(matches!(bytes.get(from), Some(b'+' | b'-')));
        let digits = bytes[from..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if digits > 0 {
            width = from + digits;
        }
    }
    width
}

fn filter_literal(introducer: &str, bytes: &[u8]) -> String {
    let mut text = format!("{introducer} X'");
    for byte in bytes {
        write!(text, "{byte:02X}").unwrap();
    }
    text.push('\'');
    text
}

fn filter_clock(word: &str, call: bool) -> Option<FilterClock> {
    match word {
        "CURRENT_DATE" | "UTC_DATE" => Some(FilterClock::Date),
        "CURRENT_TIME" | "UTC_TIME" => Some(FilterClock::Time),
        "CURRENT_TIMESTAMP" | "LOCALTIME" | "LOCALTIMESTAMP" | "UTC_TIMESTAMP" => {
            Some(FilterClock::DateTime)
        }
        "CURDATE" if call => Some(FilterClock::Date),
        "CURTIME" if call => Some(FilterClock::Time),
        "NOW" if call => Some(FilterClock::DateTime),
        _ => None,
    }
}

fn forbidden_filter_word(word: &str) -> bool {
    matches!(
        word,
        "SELECT"
            | "WITH"
            | "VALUES"
            | "UNION"
            | "INTERSECT"
            | "EXCEPT"
            | "EXISTS"
            | "INSERT"
            | "UPDATE"
            | "DELETE"
            | "ALTER"
            | "CREATE"
            | "DROP"
            | "TRUNCATE"
            | "RENAME"
            | "LOAD"
            | "HANDLER"
            | "CALL"
            | "DO"
            | "INTO"
            | "OUTFILE"
            | "PROCEDURE"
            | "SQL_CALC_FOUND_ROWS"
            | "DUMPFILE"
            | "GRANT"
            | "REVOKE"
            | "LOCK"
            | "UNLOCK"
            | "COMMIT"
            | "ROLLBACK"
            | "SAVEPOINT"
            | "USE"
            | "NEXT"
            | "PREVIOUS"
    )
}

fn filter_grouping_keyword(word: &str) -> bool {
    matches!(
        word,
        "AND"
            | "OR"
            | "XOR"
            | "NOT"
            | "IS"
            | "IN"
            | "BETWEEN"
            | "LIKE"
            | "RLIKE"
            | "REGEXP"
            | "ESCAPE"
            | "CASE"
            | "WHEN"
            | "THEN"
            | "ELSE"
            | "INTERVAL"
            | "BINARY"
            | "DIV"
            | "FROM"
            | "FOR"
    )
}

fn filter_keyword(word: &str) -> bool {
    filter_grouping_keyword(word)
        || matches!(
            word,
            "NULL"
                | "TRUE"
                | "FALSE"
                | "UNKNOWN"
                | "END"
                | "AS"
                | "USING"
                | "COLLATE"
                | "CHARACTER"
                | "SET"
                | "SIGNED"
                | "UNSIGNED"
                | "INTEGER"
                | "DECIMAL"
                | "CHAR"
                | "NCHAR"
                | "DATE"
                | "TIME"
                | "DATETIME"
                | "JSON"
                | "DISTINCT"
                | "LEADING"
                | "TRAILING"
                | "BOTH"
                | "YEAR"
                | "QUARTER"
                | "MONTH"
                | "WEEK"
                | "DAY"
                | "HOUR"
                | "MINUTE"
                | "SECOND"
                | "MICROSECOND"
                | "YEAR_MONTH"
                | "DAY_HOUR"
                | "DAY_MINUTE"
                | "DAY_SECOND"
                | "DAY_MICROSECOND"
                | "HOUR_MINUTE"
                | "HOUR_SECOND"
                | "HOUR_MICROSECOND"
                | "MINUTE_SECOND"
                | "MINUTE_MICROSECOND"
                | "SECOND_MICROSECOND"
                | "MOD"
        )
}

fn pure_filter_function(word: &str) -> bool {
    matches!(
        word,
        "ABS"
            | "CEIL"
            | "CEILING"
            | "FLOOR"
            | "ROUND"
            | "MOD"
            | "SIGN"
            | "COUNT"
            | "SUM"
            | "AVG"
            | "MIN"
            | "MAX"
            | "POW"
            | "POWER"
            | "SQRT"
            | "EXP"
            | "LOG"
            | "LOG2"
            | "LOG10"
            | "COALESCE"
            | "NULLIF"
            | "IF"
            | "IFNULL"
            | "GREATEST"
            | "LEAST"
            | "LENGTH"
            | "CHAR_LENGTH"
            | "CHARACTER_LENGTH"
            | "OCTET_LENGTH"
            | "BIT_LENGTH"
            | "LOWER"
            | "LCASE"
            | "UPPER"
            | "UCASE"
            | "TRIM"
            | "LTRIM"
            | "RTRIM"
            | "LEFT"
            | "RIGHT"
            | "SUBSTRING"
            | "SUBSTR"
            | "MID"
            | "SUBSTRING_INDEX"
            | "LOCATE"
            | "INSTR"
            | "POSITION"
            | "CONCAT"
            | "CONCAT_WS"
            | "REPLACE"
            | "LPAD"
            | "RPAD"
            | "REVERSE"
            | "ORD"
            | "ASCII"
            | "HEX"
            | "UNHEX"
            | "DATE"
            | "TIME"
            | "YEAR"
            | "QUARTER"
            | "MONTH"
            | "DAY"
            | "DAYOFMONTH"
            | "DAYOFWEEK"
            | "DAYOFYEAR"
            | "WEEKDAY"
            | "WEEK"
            | "YEARWEEK"
            | "HOUR"
            | "MINUTE"
            | "SECOND"
            | "MICROSECOND"
            | "TIMESTAMP"
            | "DATE_ADD"
            | "ADDDATE"
            | "DATE_SUB"
            | "SUBDATE"
            | "DATEDIFF"
            | "TIMESTAMPDIFF"
            | "TIMESTAMPADD"
            | "DATE_FORMAT"
            | "TIME_FORMAT"
            | "STR_TO_DATE"
            | "EXTRACT"
            | "FROM_UNIXTIME"
            | "CAST"
            | "CONVERT"
            | "CHAR"
            | "MD5"
            | "SHA1"
            | "SHA2"
            | "CRC32"
    )
}

fn json_filter_function(word: &str) -> bool {
    matches!(
        word,
        "JSON_EXTRACT"
            | "JSON_CONTAINS"
            | "JSON_CONTAINS_PATH"
            | "JSON_VALID"
            | "JSON_TYPE"
            | "JSON_LENGTH"
            | "JSON_DEPTH"
            | "JSON_KEYS"
            | "JSON_UNQUOTE"
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncSide {
    Source,
    Target,
}

impl SyncSide {
    fn label(self) -> &'static str {
        match self {
            Self::Source => "源端",
            Self::Target => "目标端",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SyncCounts {
    pub same: u64,
    pub source_only: u64,
    pub target_only: u64,
    pub conflicts: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncDifferenceKind {
    SourceOnly,
    TargetOnly,
    Conflict,
    OutsideFilter,
}

#[derive(Clone, Debug)]
pub struct SyncDifference {
    pub key: Vec<Value>,
    pub kind: SyncDifferenceKind,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SyncProgress {
    pub source_rows: u64,
    pub target_rows: u64,
    pub written: u64,
    pub finishing: bool,
    pub side: Option<SyncSide>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SyncWrites {
    pub inserted: u64,
    pub updated: u64,
    pub deleted: u64,
}

impl SyncWrites {
    pub fn total(self) -> u64 {
        self.inserted + self.updated + self.deleted
    }

    fn add(&mut self, other: Self) {
        self.inserted += other.inserted;
        self.updated += other.updated;
        self.deleted += other.deleted;
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SyncResult {
    pub source: SyncWrites,
    pub target: SyncWrites,
    pub stopped: Option<String>,
    pub uncertain: Option<SyncSide>,
    pub skipped_rows: u64,
    pub row_errors: Vec<SyncRowError>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncRowError {
    pub side: SyncSide,
    pub table: DbObject,
    pub operation: &'static str,
    pub code: u16,
}

pub struct SyncPlan {
    pub source: DbObject,
    pub target: DbObject,
    pub mode: SyncMode,
    pub counts: SyncCounts,
    pub samples: Vec<SyncDifference>,
    pub key_names: Vec<String>,
    pub outside_filter: u64,
    approval: Approval,
}

impl SyncPlan {
    pub fn policy(&self) -> SyncPolicy {
        SyncPolicy {
            mode: self.mode,
            skip_target_fk_checks: self.approval.skip_target_fk_checks,
            continue_on_row_error: self.approval.continue_on_row_error,
        }
    }

    pub fn planned_source(&self) -> SyncWrites {
        self.approval.source_writes
    }

    pub fn planned_target(&self) -> SyncWrites {
        self.approval.target_writes
    }

    pub fn filter(&self) -> Option<&SyncFilter> {
        self.approval.scope.filter.as_ref()
    }

    pub fn compare_columns(&self) -> &[String] {
        &self.approval.scope.column_names
    }

    pub fn matches_whole_row(&self) -> bool {
        self.approval.source.key_index.is_empty()
    }

    fn requires_unique_preflight(&self) -> bool {
        self.approval.mode == SyncMode::MirrorReceiver
            && self.approval.target.key_index.is_empty()
            && self.approval.target.has_unique_indexes
            && self.approval.target_writes.inserted != 0
    }
}

type CatalogTable = (String, String);
type ForeignKeyEdge = (CatalogTable, CatalogTable);

pub struct SyncGroupPlan {
    pub tables: Vec<SyncPlan>,
    mappings: Vec<(DbObject, DbObject)>,
    mode: SyncMode,
    skip_target_fk_checks: bool,
    continue_on_row_error: bool,
    dependencies: Dependencies,
}

impl SyncGroupPlan {
    pub fn policy(&self) -> SyncPolicy {
        SyncPolicy {
            mode: self.mode,
            skip_target_fk_checks: self.skip_target_fk_checks,
            continue_on_row_error: self.continue_on_row_error,
        }
    }
}

#[derive(Default)]
pub struct SyncGroupResult {
    pub completed: usize,
    pub source: SyncWrites,
    pub target: SyncWrites,
    pub stopped: Option<String>,
    pub uncertain: Option<SyncSide>,
    pub skipped_rows: u64,
    pub row_errors: Vec<SyncRowError>,
}

struct Approval {
    source: Review,
    target: Review,
    mode: SyncMode,
    skip_target_fk_checks: bool,
    continue_on_row_error: bool,
    source_writes: SyncWrites,
    target_writes: SyncWrites,
    scope: Scope,
    boundary: [u8; 32],
}

struct Scope {
    filter: Option<SyncFilter>,
    source_selection: Option<FilterSelection>,
    target_selection: Option<FilterSelection>,
    column_names: Vec<String>,
    column_indices: Vec<usize>,
}

struct FilterSelection {
    predicate: String,
    tables: Vec<DbObject>,
}

impl Scope {
    fn resolve(
        filter: Option<SyncFilter>,
        columns: Vec<String>,
        mode: SyncMode,
        source: &Layout,
        target: &Layout,
        source_endpoint: &EndpointIdentity,
        target_endpoint: &EndpointIdentity,
    ) -> Result<Self, String> {
        if matches!(mode, SyncMode::ReconcileBoth(_))
            && filter
                .as_ref()
                .is_some_and(|item| item.scope == SyncFilterScope::Source)
        {
            return Err("双向补齐需要使用两端筛选。".into());
        }
        let mut column_names = Vec::new();
        let mut column_indices = Vec::new();
        for name in columns {
            let index = source
                .columns
                .iter()
                .position(|column| {
                    column.name == name || column.name.to_lowercase() == name.to_lowercase()
                })
                .ok_or_else(|| format!("找不到比较列「{name}」。"))?;
            if column_indices.contains(&index) {
                return Err(format!("比较列「{name}」重复。"));
            }
            column_indices.push(index);
            column_names.push(source.columns[index].name.clone());
        }
        if source.is_keyless() {
            column_names.clear();
            column_indices.clear();
        }
        let (source_selection, target_selection) = if let Some(filter) = &filter {
            filter.ensure_functions(source_endpoint)?;
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            let selection = |schema: &str| FilterSelection {
                predicate: filter.sql_at(schema, now),
                tables: filter.tables(schema),
            };
            let target_selection = if filter.scope == SyncFilterScope::Both {
                filter.ensure_functions(target_endpoint)?;
                Some(selection(&target.table.0))
            } else {
                None
            };
            (Some(selection(&source.table.0)), target_selection)
        } else {
            (None, None)
        };
        Ok(Self {
            filter,
            source_selection,
            target_selection,
            column_names,
            column_indices,
        })
    }

    fn source_only(&self) -> bool {
        self.filter
            .as_ref()
            .is_some_and(|filter| filter.scope == SyncFilterScope::Source)
    }

    fn select_rows(&self, source: &mut Layout, target: &mut Layout) {
        for (layout, selection) in [
            (source, &self.source_selection),
            (target, &self.target_selection),
        ] {
            layout.selection = selection.as_ref().map(|filter| filter.predicate.clone());
            layout.filter_tables = selection
                .as_ref()
                .map_or_else(Vec::new, |filter| filter.tables.clone());
        }
    }

    fn equivalent(&self, left: &[Value], right: &[Value]) -> bool {
        if self.column_indices.is_empty() {
            rows_equal(left, right)
        } else {
            self.column_indices
                .iter()
                .all(|&index| same_value(&left[index], &right[index]))
        }
    }
}

#[derive(PartialEq, Eq)]
struct Dependencies {
    source: BTreeSet<ForeignKeyEdge>,
    target: BTreeSet<ForeignKeyEdge>,
}

struct OrderedTables {
    source: Vec<usize>,
    target: Vec<usize>,
    dependencies: Dependencies,
}

struct Review {
    endpoint: EndpointIdentity,
    object: DbObject,
    definition: String,
    key_index: String,
    has_unique_indexes: bool,
    snapshot: Fingerprint,
}

#[derive(Eq, PartialEq)]
struct EndpointIdentity {
    host: String,
    port: u16,
    socket: Option<String>,
    user: Option<String>,
    server: BTreeMap<String, String>,
}

impl EndpointIdentity {
    fn read(connection: &mut Conn, opts: &Opts) -> Result<Self, String> {
        let server = connection
            .query_opt::<(String, String), _>(
                "SHOW GLOBAL VARIABLES WHERE Variable_name IN
                 ('server_uuid', 'server_uid', 'hostname', 'port', 'version', 'server_id')",
            )
            .map_err(mysql_error)?
            .into_iter()
            .map(|row| row.map_err(|error| mysql_error(error.into())))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            host: opts.get_ip_or_hostname().into_owned(),
            port: opts.get_tcp_port(),
            socket: opts.get_socket().map(str::to_owned),
            user: opts.get_user().map(str::to_owned),
            server,
        })
    }

    fn same_server(&self, other: &Self) -> bool {
        ["server_uuid", "server_uid"].iter().any(|name| {
            self.server
                .get(*name)
                .filter(|value| !value.is_empty())
                .is_some_and(|value| other.server.get(*name) == Some(value))
        }) || (self.host.eq_ignore_ascii_case(&other.host)
            && self.port == other.port
            && self.socket == other.socket
            && self.server == other.server)
    }
}

#[cfg(test)]
pub fn compare_many(
    source_opts: &Opts,
    target_opts: &Opts,
    pairs: Vec<(DbObject, DbObject)>,
    mode: SyncMode,
    progress: impl FnMut(SyncProgress) -> Result<(), String>,
) -> Result<SyncGroupPlan, String> {
    let mappings = pairs
        .into_iter()
        .map(|(source, target)| SyncMapping::all(source, target))
        .collect();
    compare_many_filtered(source_opts, target_opts, mappings, mode, progress)
}

pub fn compare_many_filtered(
    source_opts: &Opts,
    target_opts: &Opts,
    mappings: Vec<SyncMapping>,
    policy: impl Into<SyncPolicy>,
    mut progress: impl FnMut(SyncProgress) -> Result<(), String>,
) -> Result<SyncGroupPlan, String> {
    let policy = policy.into();
    let pairs = mappings
        .iter()
        .map(|mapping| (mapping.source.clone(), mapping.target.clone()))
        .collect::<Vec<_>>();
    let mut mappings_by_index: Vec<_> = mappings.into_iter().map(Some).collect();
    let order = group_dependencies(source_opts, target_opts, &pairs, policy, &mut progress)?;
    let (mut tables, mut mappings) = (
        Vec::with_capacity(order.target.len()),
        Vec::with_capacity(order.target.len()),
    );
    let mut completed = SyncProgress::default();
    for index in order.target {
        let (source, target) = &pairs[index];
        let table = compare_filtered(
            source_opts,
            target_opts,
            mappings_by_index[index]
                .take()
                .expect("selected mappings are each visited once"),
            policy,
            |current| progress(group_progress(completed, current)),
        )
        .map_err(|error| {
            format!(
                "{} → {}：{error}",
                source.qualified_name(),
                target.qualified_name()
            )
        })?;
        completed.source_rows += table.approval.source.snapshot.rows;
        completed.target_rows += table.approval.target.snapshot.rows;
        mappings.push((source.clone(), target.clone()));
        tables.push(table);
    }
    Ok(SyncGroupPlan {
        tables,
        mappings,
        mode: policy.mode,
        skip_target_fk_checks: policy.skip_target_fk_checks,
        continue_on_row_error: policy.continue_on_row_error,
        dependencies: order.dependencies,
    })
}

pub fn apply_many(
    source_opts: &Opts,
    target_opts: &Opts,
    plan: &SyncGroupPlan,
    mut progress: impl FnMut(SyncProgress) -> Result<(), String>,
) -> Result<SyncGroupResult, String> {
    if plan.tables.len() != plan.mappings.len()
        || plan
            .tables
            .iter()
            .zip(&plan.mappings)
            .any(|(table, (source, target))| {
                &table.source != source
                    || &table.target != target
                    || table.policy() != plan.policy()
            })
    {
        return Err("选定的同步表已变更，请重新比较。".into());
    }
    let order = group_dependencies(
        source_opts,
        target_opts,
        &plan.mappings,
        plan.policy(),
        &mut progress,
    )?;
    if order.dependencies != plan.dependencies {
        return Err("所选表之间的外键关系已改变，请重新比较。".into());
    }
    if matches!(
        plan.mode,
        SyncMode::MirrorReceiver | SyncMode::ReconcileBoth(_)
    ) {
        let tables: Vec<_> = plan.tables.iter().collect();
        let result = apply_scope(
            source_opts,
            target_opts,
            &tables,
            &order.source,
            &order.target,
            &order.dependencies.target,
            progress,
        )?;
        let completed = tables
            .iter()
            .filter(|table| {
                (table.planned_source().total() == 0 || result.source.total() > 0)
                    && (table.planned_target().total() == 0 || result.target.total() > 0)
            })
            .count();
        return Ok(SyncGroupResult {
            completed,
            source: result.source,
            target: result.target,
            stopped: result.stopped,
            uncertain: result.uncertain,
            skipped_rows: result.skipped_rows,
            row_errors: result.row_errors,
        });
    }
    let mut result = SyncGroupResult::default();
    let mut completed = SyncProgress::default();
    for table in &plan.tables {
        match apply(source_opts, target_opts, table, |current| {
            progress(group_progress(completed, current))
        }) {
            Ok(table_result) => {
                result.source.add(table_result.source);
                result.target.add(table_result.target);
                if let Some(error) = table_result.stopped {
                    result.stopped = Some(format!("{}：{error}", table.target.qualified_name()));
                    result.uncertain = table_result.uncertain;
                    break;
                }
                result.completed += 1;
                completed.source_rows += table.approval.source.snapshot.rows;
                completed.target_rows += table.approval.target.snapshot.rows;
                completed.written = result.source.total() + result.target.total();
            }
            Err(error) => {
                result.stopped = Some(format!("{}：{error}", table.target.qualified_name()));
                break;
            }
        }
    }
    Ok(result)
}

fn group_progress(completed: SyncProgress, current: SyncProgress) -> SyncProgress {
    SyncProgress {
        source_rows: completed.source_rows + current.source_rows,
        target_rows: completed.target_rows + current.target_rows,
        written: completed.written + current.written,
        finishing: current.finishing,
        side: current.side,
    }
}

fn group_dependencies(
    source_opts: &Opts,
    target_opts: &Opts,
    pairs: &[(DbObject, DbObject)],
    policy: SyncPolicy,
    progress: &mut impl FnMut(SyncProgress) -> Result<(), String>,
) -> Result<OrderedTables, String> {
    if pairs.is_empty() {
        return Err("请选择至少一对表。".into());
    }
    progress(SyncProgress::default())?;
    let (mut source, source_endpoint) = connect(source_opts, "源端")?;
    let (mut target, target_endpoint) = connect(target_opts, "目标端")?;
    let mut sources = BTreeSet::new();
    let mut targets = BTreeSet::new();
    let mut ordered_sources = Vec::with_capacity(pairs.len());
    let mut ordered_targets = Vec::with_capacity(pairs.len());
    for (left, right) in pairs {
        progress(SyncProgress::default())?;
        let from = read_catalog(&mut source, left)?;
        let to = read_catalog(&mut target, right)?;
        if !sources.insert(from.clone()) || !targets.insert(to.clone()) {
            return Err("每端一个数据表只能出现在一对映射中。".into());
        }
        ordered_sources.push(from);
        ordered_targets.push(to);
    }
    if source_endpoint.same_server(&target_endpoint) && !sources.is_disjoint(&targets) {
        return Err("同一服务器中的源表和目标表不能重叠。".into());
    }
    let (target_order, target_edges) = if policy.skip_target_fk_checks {
        ((0..pairs.len()).collect(), BTreeSet::new())
    } else {
        foreign_key_order(
            &mut target,
            SyncSide::Target,
            pairs.iter().map(|(_, to)| to),
            &ordered_targets,
            &targets,
            progress,
        )?
    };
    let (source_order, source_edges) = if matches!(policy.mode, SyncMode::ReconcileBoth(_)) {
        foreign_key_order(
            &mut source,
            SyncSide::Source,
            pairs.iter().map(|(from, _)| from),
            &ordered_sources,
            &sources,
            progress,
        )?
    } else {
        (Vec::new(), BTreeSet::new())
    };
    Ok(OrderedTables {
        source: source_order,
        target: target_order,
        dependencies: Dependencies {
            source: source_edges,
            target: target_edges,
        },
    })
}

fn foreign_key_parent_sql() -> String {
    format!(
        "SELECT DISTINCT
             IF(@@lower_case_table_names = 0, t.REFERENCED_TABLE_SCHEMA, LOWER(t.REFERENCED_TABLE_SCHEMA)),
             IF(@@lower_case_table_names = 0, t.REFERENCED_TABLE_NAME, LOWER(t.REFERENCED_TABLE_NAME))
         FROM information_schema.KEY_COLUMN_USAGE t
         WHERE {MATCH_TABLE} AND t.REFERENCED_TABLE_NAME IS NOT NULL"
    )
}

fn foreign_key_order<'a>(
    connection: &mut Conn,
    side: SyncSide,
    objects: impl Iterator<Item = &'a DbObject>,
    catalogs: &[CatalogTable],
    selected: &BTreeSet<CatalogTable>,
    progress: &mut impl FnMut(SyncProgress) -> Result<(), String>,
) -> Result<(Vec<usize>, BTreeSet<ForeignKeyEdge>), String> {
    let parent_sql = foreign_key_parent_sql();
    let mut edges = BTreeSet::new();
    for (catalog, table) in catalogs.iter().zip(objects) {
        progress(SyncProgress::default())?;
        for parent in connection
            .exec_opt::<CatalogTable, _, _>(
                &parent_sql,
                params! { "schema" => table.schema.as_str(), "table" => table.name.as_str() },
            )
            .map_err(mysql_error)?
        {
            let parent = parent.map_err(|error| mysql_error(error.into()))?;
            if selected.contains(&parent) {
                edges.insert((catalog.clone(), parent));
            }
        }
    }
    order_catalogs(catalogs, edges, side)
}

fn order_catalogs(
    catalogs: &[CatalogTable],
    edges: BTreeSet<ForeignKeyEdge>,
    side: SyncSide,
) -> Result<(Vec<usize>, BTreeSet<ForeignKeyEdge>), String> {
    let (order, sorted_count) = dependency_order(catalogs, edges.iter().cloned().collect());
    if sorted_count != catalogs.len() {
        let cyclic = order[sorted_count..]
            .iter()
            .map(|&index| qualified_table(&catalogs[index]))
            .collect::<Vec<_>>()
            .join("、");
        return Err(format!(
            "{}所选表之间存在循环外键依赖，无法按表自动同步：{cyclic}。",
            side.label()
        ));
    }
    Ok((order, edges))
}

fn qualified_table((schema, table): &CatalogTable) -> String {
    format!("{}.{}", quote_identifier(schema), quote_identifier(table))
}

#[cfg(test)]
pub fn compare(
    source_opts: &Opts,
    source: DbObject,
    target_opts: &Opts,
    target: DbObject,
    mode: SyncMode,
    progress: impl FnMut(SyncProgress) -> Result<(), String>,
) -> Result<SyncPlan, String> {
    compare_filtered(
        source_opts,
        target_opts,
        SyncMapping::all(source, target),
        mode,
        progress,
    )
}

pub fn compare_filtered(
    source_opts: &Opts,
    target_opts: &Opts,
    mapping: SyncMapping,
    policy: impl Into<SyncPolicy>,
    mut progress: impl FnMut(SyncProgress) -> Result<(), String>,
) -> Result<SyncPlan, String> {
    let policy = policy.into();
    progress(SyncProgress::default())?;
    let SyncMapping {
        source,
        target,
        filter,
        compare_columns,
    } = mapping;
    let (mut source_connection, source_endpoint) = connect(source_opts, "源端")?;
    let (mut target_connection, target_endpoint) = connect(target_opts, "目标端")?;
    let mut src = snapshot(&mut source_connection)?;
    let mut dst = snapshot(&mut target_connection)?;
    let mut source_layout = Layout::read(&mut src, &source)?;
    let mut target_layout = Layout::read(&mut dst, &target)?;
    if source_endpoint.same_server(&target_endpoint) && source_layout.table == target_layout.table {
        return Err("请选择两个不同的表。".into());
    }
    source_layout.choose_shared_key(&mut target_layout)?;
    let scope = Scope::resolve(
        filter,
        compare_columns,
        policy.mode,
        &source_layout,
        &target_layout,
        &source_endpoint,
        &target_endpoint,
    )?;
    scope.select_rows(&mut source_layout, &mut target_layout);
    let inspection = inspect_pair(
        &mut src,
        &source_layout,
        &mut dst,
        &target_layout,
        &scope,
        true,
        |source_rows, target_rows| {
            progress(SyncProgress {
                source_rows,
                target_rows,
                ..SyncProgress::default()
            })
        },
    )?;
    Ok(build_plan(
        source,
        target,
        policy,
        source_endpoint,
        target_endpoint,
        source_layout,
        target_layout,
        scope,
        inspection,
    ))
}

fn build_plan(
    source: DbObject,
    target: DbObject,
    policy: SyncPolicy,
    source_endpoint: EndpointIdentity,
    target_endpoint: EndpointIdentity,
    source_layout: Layout,
    target_layout: Layout,
    scope: Scope,
    inspection: Inspection,
) -> SyncPlan {
    let mode = policy.mode;
    let counts = inspection.counts;
    let source_writes = SyncWrites {
        inserted: if matches!(mode, SyncMode::ReconcileBoth(_)) {
            counts.target_only
        } else {
            0
        },
        updated: if mode == SyncMode::ReconcileBoth(SyncConflict::TargetWins) {
            counts.conflicts
        } else {
            0
        },
        deleted: 0,
    };
    let target_writes = SyncWrites {
        inserted: counts.source_only,
        updated: if matches!(
            mode,
            SyncMode::InsertAndUpdate
                | SyncMode::MirrorReceiver
                | SyncMode::ReconcileBoth(SyncConflict::SourceWins)
        ) {
            counts.conflicts
        } else {
            0
        },
        deleted: if mode == SyncMode::MirrorReceiver {
            counts.target_only
        } else {
            0
        },
    };
    let key_names = source_layout
        .key
        .iter()
        .map(|part| source_layout.columns[part.column].name.clone())
        .collect();
    SyncPlan {
        source: source.clone(),
        target: target.clone(),
        mode,
        counts,
        samples: inspection.samples.unwrap_or_default(),
        key_names,
        outside_filter: inspection.outside_filter,
        approval: Approval {
            source: Review {
                endpoint: source_endpoint,
                object: source,
                definition: source_layout.definition,
                key_index: source_layout.key_index,
                has_unique_indexes: source_layout.has_unique_indexes,
                snapshot: inspection.source,
            },
            target: Review {
                endpoint: target_endpoint,
                object: target,
                definition: target_layout.definition,
                key_index: target_layout.key_index,
                has_unique_indexes: target_layout.has_unique_indexes,
                snapshot: inspection.target,
            },
            mode,
            skip_target_fk_checks: policy.skip_target_fk_checks,
            continue_on_row_error: policy.continue_on_row_error,
            source_writes,
            target_writes,
            scope,
            boundary: inspection.boundary,
        },
    }
}

struct Inspection {
    source: Fingerprint,
    target: Fingerprint,
    counts: SyncCounts,
    outside_filter: u64,
    samples: Option<Vec<SyncDifference>>,
    boundary: [u8; 32],
}

fn inspect_pair(
    src: &mut impl ReadTransport,
    source: &Layout,
    dst: &mut impl ReadTransport,
    target: &Layout,
    scope: &Scope,
    collect_samples: bool,
    progress: impl FnMut(u64, u64) -> Result<(), String>,
) -> Result<Inspection, String> {
    inspect_pair_record(
        src,
        source,
        dst,
        target,
        scope,
        collect_samples,
        |_| Ok(()),
        progress,
    )
}

#[derive(Clone, Copy)]
enum ObservedRow<'a> {
    Equal(&'a [Value]),
    Different {
        source: &'a [Value],
        target: &'a [Value],
    },
    SourceOnly(&'a [Value]),
    TargetOnly(&'a [Value]),
    TargetOutside(&'a [Value]),
}

fn inspect_pair_record(
    src: &mut impl ReadTransport,
    source: &Layout,
    dst: &mut impl ReadTransport,
    target: &Layout,
    scope: &Scope,
    collect_samples: bool,
    mut record: impl FnMut(ObservedRow<'_>) -> Result<(), String>,
    mut progress: impl FnMut(u64, u64) -> Result<(), String>,
) -> Result<Inspection, String> {
    src.check_filter_tables(&source.filter_tables, None)?;
    dst.check_filter_tables(&target.filter_tables, Some(&target.table))?;
    let mut counts = SyncCounts::default();
    let mut outside_filter = 0;
    let mut samples = collect_samples.then(Vec::new);
    let mut boundary = Sha256::new();
    let mut matched_peer_hash = scope.source_only().then(Sha256::new);
    let source_snapshot = walk(src, source, |src, rows, scanned| {
        let mut page_hashes = scope.source_only().then(|| vec![None; rows.len()]);
        let matched = visit_pairs(src, source, dst, target, rows, |index, left, right| {
            if let (Some(hashes), Some(right)) = (&mut page_hashes, right) {
                let mut hash = Sha256::new();
                hash_row(&mut hash, right);
                hashes[index] = Some(hash.finalize());
            }
            let (kind, count) = match right {
                Some(right) if scope.equivalent(left, right) => {
                    record(ObservedRow::Equal(right))?;
                    counts.same += 1;
                    return Ok(());
                }
                Some(_) => (SyncDifferenceKind::Conflict, &mut counts.conflicts),
                None => (SyncDifferenceKind::SourceOnly, &mut counts.source_only),
            };
            record(match right {
                Some(right) => ObservedRow::Different {
                    source: left,
                    target: right,
                },
                None => ObservedRow::SourceOnly(left),
            })?;
            add_sample(&mut samples, kind, count, source, left);
            Ok(())
        })?;
        for (row, state) in rows.iter().zip(matched) {
            if state == PeerState::Outside {
                record_boundary(&mut boundary, SyncSide::Source, source, row);
                add_sample(
                    &mut samples,
                    SyncDifferenceKind::OutsideFilter,
                    &mut outside_filter,
                    source,
                    row,
                );
            }
        }
        let matched_rows =
            if let (Some(ledger), Some(hashes)) = (&mut matched_peer_hash, page_hashes) {
                for hash in hashes {
                    ledger.update([u8::from(hash.is_some())]);
                    if let Some(hash) = hash {
                        ledger.update(hash);
                    }
                }
                counts.same + counts.conflicts
            } else {
                0
            };
        progress(scanned, matched_rows)
    })?;

    let target_snapshot = if let Some(hash) = matched_peer_hash {
        // A source-only predicate reviews complete target rows at selected source keys.
        Fingerprint {
            rows: counts.same + counts.conflicts,
            digest: hash.finalize().into(),
        }
    } else {
        let mut reverse_matches = 0;
        let snapshot = walk(dst, target, |_, rows, scanned| {
            let matched = source.probe_scoped(src, target, rows, false, |_, _| Ok(()))?;
            for (row, state) in rows.iter().zip(matched) {
                match state {
                    PeerState::Selected => reverse_matches += 1,
                    PeerState::Missing => {
                        record(ObservedRow::TargetOnly(row))?;
                        add_sample(
                            &mut samples,
                            SyncDifferenceKind::TargetOnly,
                            &mut counts.target_only,
                            target,
                            row,
                        );
                    }
                    PeerState::Outside => {
                        record(ObservedRow::TargetOutside(row))?;
                        record_boundary(&mut boundary, SyncSide::Target, target, row);
                        add_sample(
                            &mut samples,
                            SyncDifferenceKind::OutsideFilter,
                            &mut outside_filter,
                            target,
                            row,
                        );
                    }
                }
            }
            progress(source_snapshot.rows, scanned)
        })?;
        if reverse_matches != counts.same + counts.conflicts {
            return Err("两端服务器对键值的匹配规则不一致，无法形成一对一的行匹配。".into());
        }
        snapshot
    };
    Ok(Inspection {
        source: source_snapshot,
        target: target_snapshot,
        counts,
        outside_filter,
        samples,
        boundary: boundary.finalize().into(),
    })
}

fn record_boundary(hash: &mut Sha256, side: SyncSide, layout: &Layout, row: &[Value]) {
    hash.update([u8::from(side == SyncSide::Target)]);
    for part in &layout.key {
        hash_value(hash, &row[part.column]);
    }
}

pub fn apply(
    source_opts: &Opts,
    target_opts: &Opts,
    plan: &SyncPlan,
    progress: impl FnMut(SyncProgress) -> Result<(), String>,
) -> Result<SyncResult, String> {
    apply_scope(
        source_opts,
        target_opts,
        &[plan],
        &[0],
        &[0],
        &BTreeSet::new(),
        progress,
    )
}

struct CheckedTable<'a> {
    plan: &'a SyncPlan,
    source: Layout,
    target: Layout,
}

fn apply_scope(
    source_opts: &Opts,
    target_opts: &Opts,
    plans: &[&SyncPlan],
    source_order: &[usize],
    target_order: &[usize],
    target_dependencies: &BTreeSet<ForeignKeyEdge>,
    mut progress: impl FnMut(SyncProgress) -> Result<(), String>,
) -> Result<SyncResult, String> {
    if let Some(plan) = plans.iter().find(|plan| plan.requires_unique_preflight()) {
        return transport::apply::apply_group(
            &transport::Transport::Native(source_opts.clone()),
            &transport::Transport::Native(target_opts.clone()),
            plans,
            source_order,
            target_order,
            target_dependencies,
            plan.policy(),
            progress,
        )
        .map(|(result, _)| result);
    }
    let mut status = SyncProgress::default();
    progress(status)?;
    let (mut source_connection, source_endpoint) = connect(source_opts, "源端")?;
    let (mut target_connection, target_endpoint) = connect(target_opts, "目标端")?;
    let mut src = snapshot(&mut source_connection)?;
    let mut dst = snapshot(&mut target_connection)?;
    let mut tables = Vec::with_capacity(plans.len());
    for &plan in plans {
        progress(status)?;
        let approval = &plan.approval;
        if plan.source != approval.source.object
            || plan.target != approval.target.object
            || plan.mode != approval.mode
        {
            return Err("同步配置已变更，请重新比较。".into());
        }
        let mut source = Layout::read(&mut src, &plan.source)?;
        let mut target = Layout::read(&mut dst, &plan.target)?;
        approval.scope.select_rows(&mut source, &mut target);
        verify_layout(
            &approval.source,
            &source_endpoint,
            &mut source,
            SyncSide::Source.label(),
        )?;
        verify_layout(
            &approval.target,
            &target_endpoint,
            &mut target,
            SyncSide::Target.label(),
        )?;
        if approval.scope.filter.is_some() {
            let actual = inspect_pair(
                &mut src,
                &source,
                &mut dst,
                &target,
                &approval.scope,
                false,
                |source_rows, target_rows| {
                    progress(SyncProgress {
                        source_rows: status.source_rows + source_rows,
                        target_rows: status.target_rows + target_rows,
                        ..status
                    })
                },
            )?;
            verify_snapshot(&approval.source, actual.source, SyncSide::Source.label())?;
            verify_snapshot(&approval.target, actual.target, SyncSide::Target.label())?;
            if actual.boundary != approval.boundary {
                return Err("两端筛选范围边界的行匹配已变化，请重新比较。".into());
            }
        } else {
            let actual = walk(&mut src, &source, |_, _, source_rows| {
                progress(SyncProgress {
                    source_rows: status.source_rows + source_rows,
                    ..status
                })
            })?;
            verify_snapshot(&approval.source, actual, SyncSide::Source.label())?;
            let actual = walk(&mut dst, &target, |_, _, target_rows| {
                progress(SyncProgress {
                    source_rows: status.source_rows + approval.source.snapshot.rows,
                    target_rows: status.target_rows + target_rows,
                    ..status
                })
            })?;
            verify_snapshot(&approval.target, actual, SyncSide::Target.label())?;
        }
        status.source_rows += approval.source.snapshot.rows;
        status.target_rows += approval.target.snapshot.rows;
        tables.push(CheckedTable {
            plan,
            source,
            target,
        });
    }

    let any_target = plans.iter().any(|plan| plan.planned_target().total() > 0);
    let any_source = plans.iter().any(|plan| plan.planned_source().total() > 0);
    let target_needs_range_locks = plans.iter().any(|plan| {
        let writes = plan.planned_target();
        writes.deleted > 0
            || (plan.matches_whole_row()
                && (writes.total() > 0 || plan.mode == SyncMode::MirrorReceiver))
    });
    let source_needs_range_locks = plans
        .iter()
        .any(|plan| plan.matches_whole_row() && plan.planned_source().total() > 0);
    let skip_target_fk_checks = plans
        .first()
        .is_some_and(|plan| plan.policy().skip_target_fk_checks);
    let mut target_writer = any_target
        .then(|| open_writer(target_opts, SyncSide::Target, skip_target_fk_checks))
        .transpose()?;
    let mut source_writer = any_source
        .then(|| open_writer(source_opts, SyncSide::Source, false))
        .transpose()?;
    let mut target_writes = SyncWrites::default();
    let mut source_writes = SyncWrites::default();
    let mut target_tx = None;
    let mut source_tx = None;
    // Keep both original snapshots readable. Neither endpoint commits until both are staged.
    if let Some((conn, endpoint)) = &mut target_writer {
        let mut writer = write_transaction(conn, target_needs_range_locks)?;
        status.side = Some(SyncSide::Target);
        target_writes = stage_endpoint(
            &mut writer,
            endpoint,
            SyncSide::Target,
            &mut src,
            &mut dst,
            target_order.iter().map(|&index| &tables[index]),
            |written| progress(SyncProgress { written, ..status }),
        )?;
        if plans
            .iter()
            .any(|plan| plan.mode == SyncMode::MirrorReceiver)
        {
            verify_mirror(&mut writer, &mut src, &tables, || {
                progress(SyncProgress {
                    written: target_writes.total(),
                    ..status
                })
            })?;
        }
        target_tx = Some(writer);
    }
    if let Some((conn, endpoint)) = &mut source_writer {
        let mut writer = write_transaction(conn, source_needs_range_locks)?;
        status.side = Some(SyncSide::Source);
        source_writes = stage_endpoint(
            &mut writer,
            endpoint,
            SyncSide::Source,
            &mut dst,
            &mut src,
            source_order.iter().map(|&index| &tables[index]),
            |written| {
                progress(SyncProgress {
                    written: target_writes.total() + written,
                    ..status
                })
            },
        )?;
        source_tx = Some(writer);
    }

    let mut result = SyncResult::default();
    status.written = target_writes.total() + source_writes.total();
    status.finishing = true;
    for (side, transaction, writes) in [
        (SyncSide::Target, target_tx, target_writes),
        (SyncSide::Source, source_tx, source_writes),
    ] {
        let Some(transaction) = transaction else {
            continue;
        };
        status.side = Some(side);
        if let Err(error) = progress(status) {
            if result.target.total() == 0 {
                return Err(error);
            }
            result.stopped = Some(format!("{}提交前已停止。 {error}", side.label()));
            break;
        }
        if let Err(error) = transaction.commit() {
            result.stopped = Some(format!(
                "无法确认{}同步事务是否已提交。 {}",
                side.label(),
                mysql_error(error),
            ));
            result.uncertain = Some(side);
            break;
        }
        match side {
            SyncSide::Source => result.source = writes,
            SyncSide::Target => result.target = writes,
        }
    }
    Ok(result)
}

fn verify_mirror(
    writer: &mut Transaction<'_>,
    peer: &mut impl ReadTransport,
    tables: &[CheckedTable<'_>],
    mut checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    // A later statement in this transaction can affect a row checked after an earlier write.
    for table in tables {
        checkpoint()?;
        let mismatch = || {
            format!(
                "目标端表 {} 的暂存数据与源端快照不一致，同步事务已取消。",
                table.target.qualified,
            )
        };
        let approval = &table.plan.approval;
        let actual = inspect_pair(
            peer,
            &table.source,
            writer,
            &table.target,
            &approval.scope,
            false,
            |_, _| checkpoint(),
        )?;
        if actual.counts.source_only != 0
            || actual.counts.target_only != 0
            || actual.counts.conflicts != 0
            || actual.boundary != approval.boundary
        {
            return Err(mismatch());
        }
    }
    Ok(())
}

fn open_writer(
    opts: &Opts,
    side: SyncSide,
    skip_fk_checks: bool,
) -> Result<(Conn, EndpointIdentity), String> {
    let (mut connection, endpoint) = connect(opts, &format!("{}写入", side.label()))?;
    let foreign_key_checks = u8::from(side != SyncSide::Target || !skip_fk_checks);
    connection
        .query_drop(format!(
            "SET SESSION foreign_key_checks = {foreign_key_checks}, SESSION unique_checks = 1"
        ))
        .map_err(mysql_error)?;
    Ok((connection, endpoint))
}

fn write_transaction(connection: &mut Conn, lock_ranges: bool) -> Result<Transaction<'_>, String> {
    let isolation = if lock_ranges {
        // Mirror checks and full-record matching rely on range locks through commit.
        IsolationLevel::RepeatableRead
    } else {
        IsolationLevel::ReadCommitted
    };
    connection
        .start_transaction(
            TxOpts::default()
                .set_access_mode(Some(AccessMode::ReadWrite))
                .set_isolation_level(Some(isolation)),
        )
        .map_err(mysql_error)
}

fn stage_endpoint<'a>(
    writer: &mut Transaction<'_>,
    endpoint: &EndpointIdentity,
    side: SyncSide,
    peer: &mut impl ReadTransport,
    receiver: &mut impl ReadTransport,
    tables: impl Iterator<Item = &'a CheckedTable<'a>>,
    mut checkpoint: impl FnMut(u64) -> Result<(), String>,
) -> Result<SyncWrites, String> {
    let mut totals = SyncWrites::default();
    let mut deletions = Vec::new();
    for table in tables {
        checkpoint(totals.total())?;
        let (from, to, review, expected) = match side {
            SyncSide::Target => (
                &table.source,
                &table.target,
                &table.plan.approval.target,
                table.plan.planned_target(),
            ),
            SyncSide::Source => (
                &table.target,
                &table.source,
                &table.plan.approval.source,
                table.plan.planned_source(),
            ),
        };
        if expected.total() == 0 {
            if side == SyncSide::Target && table.plan.mode == SyncMode::MirrorReceiver {
                lock_keyless_receiver(writer, to)?;
            }
            continue;
        }
        let mut layout = Layout::read(writer, &review.object)?;
        layout.set_scope_from(to);
        let label = format!("{}表 {}", side.label(), layout.qualified);
        verify_layout(review, endpoint, &mut layout, &label)?;
        writer.check_filter_tables(&layout.filter_tables, Some(&layout.table))?;
        lock_keyless_receiver(writer, &layout)?;
        let write = WriteStatements::new(&layout);
        if expected.inserted + expected.updated > 0 {
            let mut actual = SyncWrites::default();
            walk(peer, from, |peer, rows, _| {
                checkpoint(totals.total() + actual.total())?;
                visit_pairs(peer, from, receiver, to, rows, |_, incoming, original| {
                    if let Some(original) = original {
                        if expected.updated == 0
                            || table.plan.approval.scope.equivalent(incoming, original)
                        {
                            return Ok(());
                        }
                        checkpoint(totals.total() + actual.total())?;
                        write.update(writer, &layout, incoming, original)?;
                        actual.updated += 1;
                    } else {
                        checkpoint(totals.total() + actual.total())?;
                        write.insert(writer, &layout, incoming)?;
                        actual.inserted += 1;
                    }
                    Ok(())
                })
                .map(|_| ())
            })
            .map_err(|error| format!("{label}：{error}"))?;
            if actual.inserted != expected.inserted || actual.updated != expected.updated {
                return Err(format!("{label}的新增或更新行数与预览不一致，事务已取消。"));
            }
            totals.add(actual);
        }
        if expected.deleted > 0 {
            let enforce_fk = side != SyncSide::Target || !table.plan.policy().skip_target_fk_checks;
            deletions.push((
                from,
                to,
                review,
                layout,
                write,
                expected.deleted,
                enforce_fk,
            ));
        }
    }
    for (from, to, review, layout, write, expected, enforce_fk) in deletions.into_iter().rev() {
        let guards = if enforce_fk {
            delete_guards(writer, &review.object, &layout)?
        } else {
            Vec::new()
        };
        let mut deleted = 0;
        walk(receiver, to, |_, rows, _| {
            checkpoint(totals.total() + deleted)?;
            let matched = from.probe_scoped(peer, to, rows, false, |_, _| Ok(()))?;
            for (row, state) in rows.iter().zip(matched) {
                if state == PeerState::Missing {
                    checkpoint(totals.total() + deleted)?;
                    write.delete(writer, &layout, &guards, row)?;
                    deleted += 1;
                }
            }
            Ok(())
        })
        .map_err(|error| format!("{}：{error}", layout.qualified))?;
        if deleted != expected {
            return Err(format!(
                "{}的删除行数与预览不一致，事务已取消。",
                layout.qualified
            ));
        }
        totals.deleted += deleted;
    }
    Ok(totals)
}

fn lock_keyless_receiver(writer: &mut Transaction<'_>, layout: &Layout) -> Result<(), String> {
    if layout.is_keyless() {
        // The scan locks both records and gaps, keeping complete-record presence stable.
        writer
            .query_drop(format!(
                "SELECT COUNT(*) INTO @harbor_keyless_lock FROM {} FOR UPDATE",
                layout.qualified
            ))
            .map_err(mysql_error)?;
    }
    Ok(())
}

fn connect(opts: &Opts, label: &str) -> Result<(Conn, EndpointIdentity), String> {
    let setup = || -> Result<_, String> {
        let mut connection = Conn::new(opts.clone()).map_err(mysql_error)?;
        connection
            .query_drop(
                "SET SESSION time_zone = '+00:00', SESSION sql_quote_show_create = 1,
                 SESSION character_set_client = 'binary', SESSION character_set_connection = 'binary',
                 SESSION character_set_results = 'utf8mb4',
                 SESSION sql_select_limit = 18446744073709551615",
            )
            .map_err(mysql_error)?;
        let mode = connection
            .query_first::<String, _>("SELECT @@SESSION.sql_mode")
            .map_err(mysql_error)?
            .ok_or("服务器未返回 SQL 模式。")?;
        let mut modes: Vec<_> = mode
            .split(',')
            .filter(|mode| {
                !matches!(
                    *mode,
                    "" | "PAD_CHAR_TO_FULL_LENGTH" | "NO_AUTO_VALUE_ON_ZERO"
                )
            })
            .collect();
        modes.push("NO_AUTO_VALUE_ON_ZERO");
        connection
            .exec_drop("SET SESSION sql_mode = ?", (modes.join(","),))
            .map_err(mysql_error)?;
        let endpoint = EndpointIdentity::read(&mut connection, opts)?;
        Ok((connection, endpoint))
    };
    setup().map_err(|error| format!("{label}连接初始化失败。 {error}"))
}

fn snapshot(connection: &mut Conn) -> Result<Transaction<'_>, String> {
    connection
        .start_transaction(
            TxOpts::default()
                .set_access_mode(Some(AccessMode::ReadOnly))
                .set_isolation_level(Some(IsolationLevel::RepeatableRead))
                .set_with_consistent_snapshot(true),
        )
        .map_err(mysql_error)
}

fn verify_layout(
    review: &Review,
    endpoint: &EndpointIdentity,
    layout: &mut Layout,
    label: &str,
) -> Result<(), String> {
    if &review.endpoint != endpoint
        || review.definition != layout.definition
        || !layout.select_reviewed_key(&review.key_index)
    {
        return Err(format!("{label}连接或表结构已变化，请重新比较。"));
    }
    Ok(())
}

fn verify_snapshot(review: &Review, actual: Fingerprint, label: &str) -> Result<(), String> {
    if review.snapshot != actual {
        return Err(format!("{label}数据已变化，请重新比较。"));
    }
    Ok(())
}

#[derive(PartialEq, Eq)]
struct ColumnLayout {
    name: String,
    kind: String,
    declaration: String,
    nullable: bool,
    generation: Option<String>,
    charset: Option<String>,
    collation: Option<String>,
    quoted: String,
    read: String,
    bind: String,
}

impl ColumnLayout {
    fn key_bind(&self) -> String {
        if let Some(collation) = &self.collation
            && self.bind.starts_with("CONVERT(")
        {
            return format!("{} COLLATE {collation}", self.bind);
        }
        self.bind.clone()
    }

    fn match_term(&self, alias: Option<&str>, keyless: bool) -> String {
        let name = alias.map_or_else(
            || self.quoted.clone(),
            |alias| format!("{}.{}", quote_identifier(alias), self.quoted),
        );
        if !keyless {
            return format!("{name} = {}", self.key_bind());
        }
        match self.kind.as_str() {
            "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" | "json" => {
                format!("CAST({name} AS BINARY) <=> CAST(? AS BINARY)")
            }
            "bit" | "enum" | "set" => format!("CAST({name} AS UNSIGNED) <=> {}", self.bind),
            "float" | "double" | "real" => format!("({name} + 0e0) <=> (? + 0e0)"),
            _ => format!("{name} <=> {}", self.bind),
        }
    }
}

#[derive(Clone)]
struct KeyPart {
    column: usize,
    descending: bool,
}

struct RowKey {
    index: String,
    parts: Vec<KeyPart>,
}

impl RowKey {
    fn columns(&self) -> impl Iterator<Item = usize> + '_ {
        self.parts.iter().map(|part| part.column)
    }
}

struct Layout {
    table: CatalogTable,
    qualified: String,
    definition: String,
    columns: Vec<ColumnLayout>,
    available_keys: Vec<RowKey>,
    has_unique_indexes: bool,
    key_index: String,
    key: Vec<KeyPart>,
    record_digest: Option<String>,
    projection: String,
    order: String,
    after: String,
    predicate: String,
    selection: Option<String>,
    filter_tables: Vec<DbObject>,
}

#[derive(Clone, Copy)]
enum RowPrefix {
    None,
    Index,
    Digest,
}

enum PageCursor {
    Indexed(Vec<Value>),
    Digest([u8; 32]),
}

struct ReadPage {
    rows: Vec<Vec<Value>>,
    cursor: Option<PageCursor>,
    truncated: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PeerState {
    Selected,
    Missing,
    Outside,
}

fn read_catalog(
    connection: &mut impl Queryable,
    object: &DbObject,
) -> Result<CatalogTable, String> {
    if object.kind != ObjectKind::Table {
        return Err("只能同步数据表。".into());
    }
    let table = connection
        .exec_first_opt::<CatalogMetadata, _, _>(
            catalog_sql(),
            params! { "schema" => object.schema.as_str(), "table" => object.name.as_str() },
        )
        .map_err(mysql_error)?
        .transpose()
        .map_err(|error| mysql_error(error.into()))?
        .ok_or_else(|| format!("无法读取 {} 的表属性。", object.qualified_name()))?;
    checked_catalog(object, table)
}

type CatalogMetadata = (String, String, String, Option<String>);
type ColumnMetadata = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<u32>,
    Option<u32>,
);
type KeyMetadata = (
    String,
    Option<String>,
    usize,
    Option<u64>,
    Option<String>,
    String,
);

fn catalog_sql() -> String {
    format!(
        "SELECT IF(@@lower_case_table_names = 0, t.TABLE_SCHEMA, LOWER(t.TABLE_SCHEMA)),
                IF(@@lower_case_table_names = 0, t.TABLE_NAME, LOWER(t.TABLE_NAME)),
                t.TABLE_TYPE, t.ENGINE
         FROM information_schema.TABLES t WHERE {MATCH_TABLE}"
    )
}

fn checked_catalog(
    object: &DbObject,
    (schema, name, kind, engine): CatalogMetadata,
) -> Result<CatalogTable, String> {
    if kind != "BASE TABLE"
        || !engine
            .as_deref()
            .is_some_and(|engine| engine.eq_ignore_ascii_case("InnoDB"))
    {
        return Err(format!(
            "{} 需要使用支持一致性快照和写入事务的 InnoDB 普通表。",
            object.qualified_name(),
        ));
    }
    Ok((schema, name))
}

fn columns_sql() -> String {
    format!(
        "SELECT t.COLUMN_NAME, t.DATA_TYPE, t.COLUMN_TYPE, t.IS_NULLABLE,
                t.GENERATION_EXPRESSION, t.CHARACTER_SET_NAME, t.COLLATION_NAME,
                t.NUMERIC_PRECISION, t.NUMERIC_SCALE
         FROM information_schema.COLUMNS t WHERE {MATCH_TABLE} ORDER BY t.COLUMN_NAME"
    )
}

fn key_sql() -> String {
    format!(
        "SELECT t.INDEX_NAME, t.COLUMN_NAME, t.SEQ_IN_INDEX, t.SUB_PART, t.COLLATION, t.INDEX_TYPE
         FROM information_schema.STATISTICS t
         WHERE {MATCH_TABLE} AND t.NON_UNIQUE = 0
         ORDER BY CAST(t.INDEX_NAME AS BINARY), t.SEQ_IN_INDEX"
    )
}

fn row_keys(
    columns: &[ColumnLayout],
    metadata: impl Iterator<Item = Result<KeyMetadata, String>>,
) -> Result<(Vec<RowKey>, bool), String> {
    let mut indexes = BTreeMap::<_, Vec<_>>::new();
    let mut has_unique_indexes = false;
    for row in metadata {
        let (index, column, position, prefix, direction, index_type) = row?;
        has_unique_indexes = true;
        if index != "PRIMARY" && index_type != "BTREE" {
            continue;
        }
        indexes
            .entry(index)
            .or_default()
            .push((column, position, prefix, direction));
    }
    let primary_exists = indexes.contains_key("PRIMARY");
    let mut keys = Vec::new();
    for (index, fields) in indexes {
        if primary_exists && index != "PRIMARY" {
            continue;
        }
        let parts = fields
            .into_iter()
            .enumerate()
            .map(|(offset, (name, position, prefix, direction))| {
                let name = name?;
                let column = columns
                    .binary_search_by(|column| column.name.cmp(&name))
                    .ok()?;
                (prefix.is_none() && position == offset + 1 && !columns[column].nullable).then_some(
                    KeyPart {
                        column,
                        descending: direction.as_deref() == Some("D"),
                    },
                )
            })
            .collect::<Option<Vec<_>>>();
        if let Some(parts) = parts {
            keys.push(RowKey { index, parts });
        }
    }
    keys.sort_by(|left, right| {
        left.parts
            .len()
            .cmp(&right.parts.len())
            .then_with(|| left.columns().cmp(right.columns()))
            .then_with(|| left.index.cmp(&right.index))
    });
    Ok((keys, has_unique_indexes))
}

fn key_queries(columns: &[ColumnLayout], key: &[KeyPart]) -> (String, String, String) {
    let order = key
        .iter()
        .map(|part| {
            let direction = if part.descending { "DESC" } else { "ASC" };
            format!("{} {direction}", columns[part.column].quoted)
        })
        .collect::<Vec<_>>()
        .join(", ");
    let parts = key
        .iter()
        .map(|part| columns[part.column].match_term(None, false))
        .collect::<Vec<_>>();
    // Expand the tuple inequality so the index can seek on each prefix.
    let after = key
        .iter()
        .enumerate()
        .map(|(index, part)| {
            let mut conditions = parts[..index].to_vec();
            let op = if part.descending { "<" } else { ">" };
            let column = &columns[part.column];
            conditions.push(format!("{} {op} {}", column.quoted, column.key_bind()));
            format!("({})", conditions.join(" AND "))
        })
        .collect::<Vec<_>>()
        .join(" OR ");
    (order, after, parts.join(" AND "))
}

fn record_digest(columns: &[ColumnLayout]) -> String {
    // Bound the sort key even when adjacent records differ far into a BLOB.
    // Frame NULL separately from each fixed-size digest of a non-NULL value.
    let terms = columns
        .iter()
        .map(|column| {
            let value = if matches!(column.kind.as_str(), "float" | "double" | "real") {
                format!("({} + 0e0)", column.quoted)
            } else {
                column.read.clone()
            };
            format!(
                "IF({} IS NULL, X'00', CONCAT(X'01', UNHEX(SHA2(CAST({value} AS BINARY), 256))))",
                column.quoted
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("CAST(UNHEX(SHA2(CONCAT({terms}), 256)) AS BINARY(32))")
}

fn normalize_record(row: &mut [Value]) {
    // The SQL numeric comparison equates both spellings of floating-point zero.
    for value in row {
        match value {
            Value::Float(number) if *number == 0.0 => *number = 0.0,
            Value::Double(number) if *number == 0.0 => *number = 0.0,
            _ => {}
        }
    }
}

impl Layout {
    fn read(transaction: &mut Transaction<'_>, object: &DbObject) -> Result<Self, String> {
        let qualified = object.qualified_name();
        // Pin the table definition for every read or write used by this transaction.
        transaction
            .query_drop(format!("SELECT 1 FROM {qualified} LIMIT 0"))
            .map_err(mysql_error)?;
        let table = read_catalog(transaction, object)?;
        let args = params! { "schema" => object.schema.as_str(), "table" => object.name.as_str() };

        let columns = transaction
            .exec_opt::<ColumnMetadata, _, _>(columns_sql(), args.clone())
            .map_err(mysql_error)?
            .into_iter()
            .map(|row| row.map_err(|error| mysql_error(error.into())));
        let key = transaction
            .exec_opt::<KeyMetadata, _, _>(key_sql(), args)
            .map_err(mysql_error)?
            .into_iter()
            .map(|row| row.map_err(|error| mysql_error(error.into())));
        let definition = transaction
            .query_first_opt::<(String, String), _>(format!("SHOW CREATE TABLE {qualified}"))
            .map_err(mysql_error)?
            .transpose()
            .map_err(|error| mysql_error(error.into()))?
            .map(|(_, ddl)| stable_definition(ddl))
            .ok_or_else(|| format!("无法读取 {qualified} 的结构。"))?;
        Self::from_metadata(table, qualified, definition, columns, key)
    }

    fn from_metadata(
        table: CatalogTable,
        qualified: String,
        definition: String,
        column_rows: impl Iterator<Item = Result<ColumnMetadata, String>>,
        key_rows: impl Iterator<Item = Result<KeyMetadata, String>>,
    ) -> Result<Self, String> {
        let mut columns = Vec::new();
        for row in column_rows {
            let (
                name,
                kind,
                declaration,
                nullable,
                generation,
                charset,
                collation,
                precision,
                scale,
            ) = row?;
            let quoted = quote_identifier(&name);
            let (read, bind) = match kind.as_str() {
                "bit" | "enum" | "set" => (
                    format!("CAST({quoted} AS UNSIGNED)"),
                    "CAST(? AS UNSIGNED)".into(),
                ),
                "decimal" | "numeric" => {
                    let (precision, scale) =
                        precision.zip(scale).ok_or("DECIMAL 列缺少精度信息。")?;
                    (
                        quoted.clone(),
                        format!("CAST(? AS DECIMAL({precision}, {scale}))"),
                    )
                }
                "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" => {
                    (quoted.clone(), "CAST(? AS BINARY)".into())
                }
                "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" | "json" => {
                    let charset = charset
                        .as_deref()
                        .or((kind == "json").then_some("utf8mb4"))
                        .filter(|name| sql_word(name))
                        .ok_or_else(|| format!("{qualified}.{quoted} 的字符集无效。"))?;
                    if collation.as_deref().is_some_and(|name| !sql_word(name)) {
                        return Err(format!("{qualified}.{quoted} 的排序规则无效。"));
                    }
                    (
                        format!("CAST({quoted} AS BINARY)"),
                        format!("CONVERT(CAST(? AS BINARY) USING {charset})"),
                    )
                }
                "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "float"
                | "double" | "real" | "date" | "datetime" | "time" | "timestamp" | "year" => {
                    (quoted.clone(), "?".into())
                }
                _ => {
                    return Err(format!(
                        "{qualified}.{quoted} 的类型 {kind} 暂不支持精确同步。"
                    ));
                }
            };
            columns.push(ColumnLayout {
                name,
                declaration: type_shape(&kind, declaration),
                kind,
                nullable: nullable == "YES",
                generation: generation.filter(|value| !value.is_empty()),
                charset,
                collation,
                quoted,
                read,
                bind,
            });
        }
        // Column order in the table and ordering of metadata names can differ by server.
        columns.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        if columns.is_empty() {
            return Err(format!("{qualified} 没有可读取的列。"));
        }

        let (available_keys, has_unique_indexes) = row_keys(&columns, key_rows)?;
        let (key_index, key, record_digest, order, after, predicate) =
            if let Some(selected) = available_keys.first() {
                let (order, after, predicate) = key_queries(&columns, &selected.parts);
                (
                    selected.index.clone(),
                    selected.parts.clone(),
                    None,
                    order,
                    after,
                    predicate,
                )
            } else {
                let key = (0..columns.len())
                    .map(|column| KeyPart {
                        column,
                        descending: false,
                    })
                    .collect();
                let predicate = columns
                    .iter()
                    .map(|column| column.match_term(None, true))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                (
                    String::new(),
                    key,
                    Some(record_digest(&columns)),
                    String::new(),
                    String::new(),
                    predicate,
                )
            };

        let projection = columns
            .iter()
            .map(|column| column.read.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Ok(Self {
            table,
            qualified,
            definition,
            columns,
            available_keys,
            has_unique_indexes,
            key_index,
            key,
            record_digest,
            projection,
            order,
            after,
            predicate,
            selection: None,
            filter_tables: Vec::new(),
        })
    }

    fn set_scope_from(&mut self, reviewed: &Self) {
        self.selection.clone_from(&reviewed.selection);
        self.filter_tables.clone_from(&reviewed.filter_tables);
    }

    fn choose_shared_key(&mut self, other: &mut Self) -> Result<(), String> {
        if self.columns != other.columns {
            return Err("两端的列类型、可空性、排序规则或生成表达式不同，请先同步结构。".into());
        }
        if self.is_keyless() && other.is_keyless() {
            return Ok(());
        }
        let matched = self
            .available_keys
            .iter()
            .enumerate()
            .find_map(|(left, key)| {
                other
                    .available_keys
                    .iter()
                    .position(|candidate| candidate.columns().eq(key.columns()))
                    .map(|right| (left, right))
            });
        let (left, right) =
            matched.ok_or("两端没有列及顺序一致的完整主键或非空唯一索引，请先同步结构。")?;
        self.select_key(left);
        other.select_key(right);
        Ok(())
    }

    fn select_reviewed_key(&mut self, index: &str) -> bool {
        if index.is_empty() {
            return self.is_keyless();
        }
        let Some(position) = self
            .available_keys
            .iter()
            .position(|key| key.index == index)
        else {
            return false;
        };
        self.select_key(position);
        true
    }

    fn select_key(&mut self, position: usize) {
        let selected = &self.available_keys[position];
        if self.key_index != selected.index {
            self.key_index.clone_from(&selected.index);
            self.key.clone_from(&selected.parts);
            (self.order, self.after, self.predicate) = key_queries(&self.columns, &self.key);
        }
    }

    fn ensure_compatible(&self, other: &Self) -> Result<(), String> {
        if self.columns != other.columns
            || self.is_keyless() != other.is_keyless()
            || self
                .key
                .iter()
                .map(|part| part.column)
                .ne(other.key.iter().map(|part| part.column))
        {
            return Err("两端的字段定义或用于匹配的键列不同，请重新比较。".into());
        }
        Ok(())
    }

    fn is_keyless(&self) -> bool {
        self.record_digest.is_some()
    }

    fn keyless_digest(&self) -> Option<&str> {
        self.record_digest.as_deref()
    }

    fn key_predicate(&self, alias: Option<&str>) -> String {
        self.key
            .iter()
            .map(|part| self.columns[part.column].match_term(alias, self.is_keyless()))
            .collect::<Vec<_>>()
            .join(" AND ")
    }

    fn key_values(&self, row: &[Value]) -> Vec<Value> {
        self.key
            .iter()
            .map(|part| row[part.column].clone())
            .collect()
    }

    fn read_page(
        &self,
        transaction: &mut impl ReadTransport,
        after: Option<&PageCursor>,
        limit: usize,
    ) -> Result<ReadPage, String> {
        let projection = transaction.projection(self);
        let (mut sql, order, prefix) = if let Some(digest) = self.keyless_digest() {
            (
                format!(
                    "SELECT DISTINCT {digest}, {projection} FROM {}",
                    self.qualified
                ),
                "1",
                RowPrefix::Digest,
            )
        } else {
            (
                format!("SELECT {projection} FROM {}", self.qualified),
                self.order.as_str(),
                RowPrefix::None,
            )
        };
        let mut args = Vec::new();
        if let Some(selection) = &self.selection {
            write!(sql, " WHERE ({selection})").unwrap();
        }
        if let Some(after) = after {
            let join = if self.selection.is_some() {
                "AND"
            } else {
                "WHERE"
            };
            match after {
                PageCursor::Indexed(key) => {
                    write!(sql, " {join} ({})", self.after).unwrap();
                    for end in 1..=key.len() {
                        args.extend_from_slice(&key[..end]);
                    }
                }
                PageCursor::Digest(bytes) => {
                    let digest = self.keyless_digest().unwrap();
                    write!(sql, " {join} {digest} > CAST(? AS BINARY)").unwrap();
                    args.push(Value::Bytes(bytes.to_vec()));
                }
            }
        }
        write!(sql, " ORDER BY {order} LIMIT ?").unwrap();
        let keyless = self.is_keyless();
        let mut rows = Vec::new();
        let mut cursor = None;
        let mut bytes = 0usize;
        let mut truncated = false;
        let mut requested = limit;
        let mut lookahead = usize::from(keyless);
        loop {
            // Read one extra record so an equal digest at the page boundary cannot be skipped.
            let fetch = requested + lookahead;
            let mut params = args.clone();
            params.push(Value::UInt(fetch as u64));
            let outcome =
                transaction.for_rows(&sql, params, &self.columns, prefix, fetch, |mut row| {
                    if row.len() != self.columns.len() + usize::from(keyless) {
                        return Err(format!("{} 返回了无效的列数据。", self.qualified));
                    }
                    let digest = if keyless {
                        let Value::Bytes(raw) = row.remove(0) else {
                            return Err(format!("{} 未返回有效的整行摘要。", self.qualified));
                        };
                        let digest: [u8; 32] = raw
                            .try_into()
                            .map_err(|_| format!("{} 返回了无效的整行摘要。", self.qualified))?;
                        if let Some(PageCursor::Digest(previous)) = cursor.as_ref().or(after)
                            && previous >= &digest
                        {
                            return Err(format!("{} 的整行摘要无法区分分页记录。", self.qualified));
                        }
                        if rows.len() == requested {
                            return Ok(false);
                        }
                        normalize_record(&mut row);
                        Some(digest)
                    } else {
                        if self.key.iter().any(|part| row[part.column] == Value::NULL) {
                            return Err(format!("{} 返回了无效的键值。", self.qualified));
                        }
                        None
                    };
                    let weight = row.len() * size_of::<Value>()
                        + row
                            .iter()
                            .map(|value| match value {
                                Value::Bytes(bytes) => bytes.len(),
                                _ => 0,
                            })
                            .sum::<usize>();
                    if !rows.is_empty() && bytes.saturating_add(weight) > PAGE_BYTES {
                        truncated = true;
                        return Ok(false);
                    }
                    if let Some(digest) = digest {
                        cursor = Some(PageCursor::Digest(digest));
                    }
                    bytes = bytes.saturating_add(weight);
                    rows.push(row);
                    Ok(true)
                });
            match outcome {
                Err(error) if transport::too_large(&error) && rows.is_empty() && requested > 1 => {
                    requested /= 2
                }
                Err(error) if transport::too_large(&error) && rows.is_empty() && lookahead > 0 => {
                    // A large record may fit by itself even when the next record does not.
                    lookahead = 0;
                    truncated = true;
                }
                result => {
                    result?;
                    break;
                }
            }
        }
        if requested < limit && rows.len() == requested {
            truncated = true;
        }
        if !keyless {
            cursor = rows
                .last()
                .map(|row| PageCursor::Indexed(self.key_values(row)));
        }
        Ok(ReadPage {
            rows,
            cursor,
            truncated,
        })
    }

    fn probe_scoped(
        &self,
        transaction: &mut impl ReadTransport,
        input: &Self,
        rows: &[Vec<Value>],
        fetch_values: bool,
        found: impl FnMut(usize, Vec<Value>) -> Result<(), String>,
    ) -> Result<Vec<PeerState>, String> {
        let selected = self.probe(
            transaction,
            input,
            rows,
            fetch_values,
            self.selection.as_deref(),
            found,
        )?;
        let mut states = selected
            .iter()
            .map(|&found| {
                if found {
                    PeerState::Selected
                } else {
                    PeerState::Missing
                }
            })
            .collect::<Vec<_>>();
        if self.selection.is_some() {
            let absent = rows
                .iter()
                .enumerate()
                .filter_map(|(index, row)| (!selected[index]).then_some((index, row)))
                .collect::<Vec<_>>();
            if !absent.is_empty() {
                // Test physical existence without loading any out-of-scope row values.
                let keys = absent.iter().map(|&(_, row)| row).collect::<Vec<_>>();
                self.probe(transaction, input, &keys, false, None, |index, _| {
                    states[absent[index].0] = PeerState::Outside;
                    Ok(())
                })?;
            }
        }
        Ok(states)
    }

    fn probe(
        &self,
        transaction: &mut impl ReadTransport,
        input: &Self,
        rows: &[impl AsRef<[Value]>],
        fetch_values: bool,
        selection: Option<&str>,
        mut found: impl FnMut(usize, Vec<Value>) -> Result<(), String>,
    ) -> Result<Vec<bool>, String> {
        let keyless = self.is_keyless();
        let fetch_row = fetch_values && !keyless;
        let mut suffix = if keyless {
            format!(
                " FROM DUAL WHERE EXISTS(SELECT 1 FROM {} WHERE {}",
                self.qualified, self.predicate
            )
        } else if fetch_row {
            format!(
                ", {} FROM {} WHERE {}",
                transaction.projection(self),
                self.qualified,
                self.predicate
            )
        } else {
            format!(" FROM {} WHERE {}", self.qualified, self.predicate)
        };
        if let Some(selection) = selection {
            write!(suffix, " AND ({selection})").unwrap();
        }
        if keyless {
            suffix.push(')');
        }
        let mut batch_size = (PROBE_SQL_BYTES / (suffix.len() + 32)).clamp(1, PAGE_ROWS);
        let mut matched = vec![false; rows.len()];
        let mut start = 0;
        while start < rows.len() {
            let end = (start + batch_size).min(rows.len());
            let chunk = &rows[start..end];
            let mut sql = String::new();
            let mut args = Vec::new();
            for (index, row) in chunk.iter().enumerate() {
                if index != 0 {
                    sql.push_str(" UNION ALL ");
                }
                write!(sql, "SELECT {index}{suffix}").unwrap();
                args.extend(input.key_values(row.as_ref()));
            }
            let columns = if fetch_row {
                self.columns.as_slice()
            } else {
                &[]
            };
            let result = transaction.for_rows(
                &sql,
                args,
                columns,
                RowPrefix::Index,
                chunk.len() + 1,
                |row| {
                    let mut values = row.into_iter();
                    let index = values
                        .next()
                        .and_then(|value| mysql::from_value_opt::<usize>(value).ok())
                        .filter(|&index| index < chunk.len())
                        .ok_or("服务器返回了无效的键值匹配序号。")?
                        + start;
                    if std::mem::replace(&mut matched[index], true) {
                        return Err("一个键值匹配了多行，无法安全比较。".into());
                    }
                    if fetch_row && values.len() != self.columns.len() {
                        return Err("服务器返回的列数与表结构不一致。".into());
                    }
                    let observed = if keyless && fetch_values {
                        rows[index].as_ref().to_vec()
                    } else {
                        values.collect()
                    };
                    found(index, observed)?;
                    Ok(true)
                },
            );
            match result {
                Err(error) if transport::too_large(&error) && chunk.len() > 1 => {
                    batch_size = chunk.len() / 2;
                }
                outcome => {
                    outcome?;
                    start = end;
                }
            }
        }
        Ok(matched)
    }

    fn lookup(
        &self,
        transaction: &mut impl ReadTransport,
        key: Vec<Value>,
        lock: bool,
        selected: bool,
    ) -> Result<Option<Vec<Value>>, String> {
        let locking = if lock { " FOR UPDATE" } else { "" };
        let selection = if selected {
            self.selection
                .as_ref()
                .map_or(String::new(), |selection| format!(" AND ({selection})"))
        } else {
            String::new()
        };
        let limit = if self.is_keyless() { 1 } else { 2 };
        let sql = format!(
            "SELECT {} FROM {} WHERE {}{selection} LIMIT {limit}{locking}",
            transaction.projection(self),
            self.qualified,
            self.predicate
        );
        let mut rows = Vec::new();
        transaction.for_rows(&sql, key, &self.columns, RowPrefix::None, limit, |row| {
            rows.push(row);
            Ok(true)
        })?;
        if rows.len() > 1 {
            return Err("一个键值匹配了多行，无法安全同步。".into());
        }
        let mut row = rows.pop();
        if self.is_keyless()
            && let Some(row) = &mut row
        {
            normalize_record(row);
        }
        Ok(row)
    }
}

fn sql_word(word: &str) -> bool {
    !word.is_empty()
        && word
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn type_shape(kind: &str, declaration: String) -> String {
    // Integer display widths vary by server version and do not affect stored values.
    if matches!(
        kind,
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint"
    ) && !declaration.contains("zerofill")
        && let Some((base, tail)) = declaration.split_once('(')
        && let Some((width, suffix)) = tail.split_once(')')
        && width.bytes().all(|byte| byte.is_ascii_digit())
    {
        return format!("{base}{suffix}");
    }
    declaration
}

pub(super) fn stable_definition(mut definition: String) -> String {
    // SHOW CREATE places the volatile allocator value immediately after the engine.
    const MARKER: &str = "\n) ENGINE=InnoDB";
    if let Some(engine) = definition.rfind(MARKER) {
        let from = engine + MARKER.len();
        if let Some(value) = definition[from..].strip_prefix(" AUTO_INCREMENT=") {
            let digits = value.bytes().take_while(u8::is_ascii_digit).count();
            definition.replace_range(from..from + " AUTO_INCREMENT=".len() + digits, "");
        }
    }
    definition
}

fn add_sample(
    samples: &mut Option<Vec<SyncDifference>>,
    kind: SyncDifferenceKind,
    count: &mut u64,
    layout: &Layout,
    row: &[Value],
) {
    if *count < SAMPLES_PER_KIND
        && let Some(samples) = samples
    {
        samples.push(SyncDifference {
            key: layout.key_values(row),
            kind,
        });
    }
    *count += 1;
}

fn visit_pairs(
    src: &mut impl ReadTransport,
    source: &Layout,
    dst: &mut impl ReadTransport,
    target: &Layout,
    rows: &[Vec<Value>],
    mut visit: impl FnMut(usize, &[Value], Option<&[Value]>) -> Result<(), String>,
) -> Result<Vec<PeerState>, String> {
    let matched = target.probe_scoped(dst, source, rows, true, |index, right| {
        let left = &rows[index];
        if source
            .key
            .iter()
            .any(|part| !same_value(&left[part.column], &right[part.column]))
        {
            // A collated key may match another byte spelling. Verify the match is reciprocal.
            let reciprocal = source.lookup(src, target.key_values(&right), false, false)?;
            if reciprocal
                .as_deref()
                .is_none_or(|row| !rows_equal(row, left))
            {
                return Err("两端服务器对键值的匹配规则不一致，无法形成一对一的行匹配。".into());
            }
        }
        visit(index, left, Some(&right))
    })?;
    for (index, (row, &state)) in rows.iter().zip(&matched).enumerate() {
        if state == PeerState::Missing {
            visit(index, row, None)?;
        }
    }
    Ok(matched)
}

#[derive(PartialEq, Eq)]
struct Fingerprint {
    rows: u64,
    digest: [u8; 32],
}

fn walk<R: ReadTransport>(
    transaction: &mut R,
    layout: &Layout,
    mut on_page: impl FnMut(&mut R, &[Vec<Value>], u64) -> Result<(), String>,
) -> Result<Fingerprint, String> {
    let mut after = None;
    let mut hash = Sha256::new();
    let mut scanned = 0;
    let mut limit = PAGE_ROWS;
    loop {
        let ReadPage {
            rows,
            cursor,
            truncated,
        } = layout.read_page(transaction, after.as_ref(), limit)?;
        let Some(cursor) = cursor else {
            break;
        };
        if let (PageCursor::Indexed(key), Some(PageCursor::Indexed(previous))) = (&cursor, &after)
            && rows_equal(key, previous)
        {
            return Err("按键值分页未能继续，请检查两端用于匹配的键列。".into());
        }
        after = Some(cursor);
        for row in &rows {
            hash_row(&mut hash, row);
        }
        scanned += rows.len() as u64;
        on_page(transaction, &rows, scanned)?;
        if truncated {
            limit = rows.len();
        } else if rows.len() < limit {
            break;
        } else {
            limit = (limit * 2).min(PAGE_ROWS);
        }
    }
    Ok(Fingerprint {
        rows: scanned,
        digest: hash.finalize().into(),
    })
}

fn rows_equal(left: &[Value], right: &[Value]) -> bool {
    left.len() == right.len() && left.iter().zip(right).all(|(a, b)| same_value(a, b))
}

fn hash_row(hash: &mut Sha256, row: &[Value]) {
    hash.update((row.len() as u64).to_le_bytes());
    for value in row {
        hash_value(hash, value);
    }
}

fn same_value(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Float(left), Value::Float(right)) => left.to_bits() == right.to_bits(),
        (Value::Double(left), Value::Double(right)) => left.to_bits() == right.to_bits(),
        _ => left == right,
    }
}

fn hash_value(hash: &mut Sha256, value: &Value) {
    match value {
        Value::NULL => hash.update([0]),
        Value::Bytes(bytes) => {
            hash.update([1]);
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
        Value::Int(value) => {
            hash.update([2]);
            hash.update(value.to_le_bytes());
        }
        Value::UInt(value) => {
            hash.update([3]);
            hash.update(value.to_le_bytes());
        }
        Value::Float(value) => {
            hash.update([4]);
            hash.update(value.to_bits().to_le_bytes());
        }
        Value::Double(value) => {
            hash.update([5]);
            hash.update(value.to_bits().to_le_bytes());
        }
        Value::Date(y, m, d, hour, minute, second, micros) => {
            hash.update([6, *m, *d, *hour, *minute, *second]);
            hash.update(y.to_le_bytes());
            hash.update(micros.to_le_bytes());
        }
        Value::Time(negative, days, hour, minute, second, micros) => {
            hash.update([7, u8::from(*negative), *hour, *minute, *second]);
            hash.update(days.to_le_bytes());
            hash.update(micros.to_le_bytes());
        }
    }
}

struct WriteStatements {
    writable: Vec<usize>,
    insert: String,
    update: String,
    delete: String,
}

impl WriteStatements {
    fn new(layout: &Layout) -> Self {
        let writable: Vec<_> = layout
            .columns
            .iter()
            .enumerate()
            .filter_map(|(index, column)| column.generation.is_none().then_some(index))
            .collect();
        let columns: Vec<_> = writable
            .iter()
            .map(|&index| &layout.columns[index])
            .collect();
        let names = columns
            .iter()
            .map(|column| column.quoted.as_str())
            .collect::<Vec<_>>();
        let bind = columns
            .iter()
            .map(|column| column.bind.as_str())
            .collect::<Vec<_>>();
        let assignments = names
            .iter()
            .zip(&bind)
            .map(|(name, value)| format!("{name} = {value}"))
            .collect::<Vec<_>>();
        Self {
            writable,
            insert: format!(
                "INSERT INTO {} ({}) VALUES ({})",
                layout.qualified,
                names.join(", "),
                bind.join(", "),
            ),
            update: format!(
                "UPDATE {} SET {} WHERE {}",
                layout.qualified,
                assignments.join(", "),
                layout.predicate
            ),
            delete: format!(
                "DELETE FROM {} WHERE {}",
                layout.qualified, layout.predicate
            ),
        }
    }

    fn values(&self, row: &[Value]) -> Vec<Value> {
        self.writable
            .iter()
            .map(|&index| row[index].clone())
            .collect()
    }

    fn insert(
        &self,
        transaction: &mut Transaction<'_>,
        layout: &Layout,
        incoming: &[Value],
    ) -> Result<(), String> {
        if layout.is_keyless()
            && layout
                .lookup(transaction, layout.key_values(incoming), true, false)?
                .is_some()
        {
            return Err("接收表已存在待新增的整行记录，请重新比较。".into());
        }
        // A concurrent insert or a collision on another unique index fails the whole transaction.
        transaction
            .exec_drop(&self.insert, Params::Positional(self.values(incoming)))
            .map_err(mysql_error)?;
        if transaction.affected_rows() != 1 {
            return Err("新增行数异常，同步事务已取消。".into());
        }
        verify_write(transaction, layout, incoming)
    }

    fn update(
        &self,
        transaction: &mut Transaction<'_>,
        layout: &Layout,
        incoming: &[Value],
        original: &[Value],
    ) -> Result<(), String> {
        if layout.is_keyless() {
            return Err("整行匹配的记录只能新增或删除。".into());
        }
        let mut values = self.values(incoming);
        values.extend(lock_original(transaction, layout, original)?);
        transaction
            .exec_drop(&self.update, Params::Positional(values))
            .map_err(mysql_error)?;
        if transaction.affected_rows() > 1 {
            return Err("更新行数异常，同步事务已取消。".into());
        }
        verify_write(transaction, layout, incoming)
    }

    fn delete(
        &self,
        transaction: &mut Transaction<'_>,
        layout: &Layout,
        guards: &[DeleteGuard],
        original: &[Value],
    ) -> Result<(), String> {
        let key = lock_original(transaction, layout, original)?;
        let copies = if layout.is_keyless() {
            let lock = format!(
                "SELECT COUNT(*) FROM {} WHERE {} FOR UPDATE",
                layout.qualified, layout.predicate
            );
            transaction
                .exec_first::<u64, _, _>(lock, Params::Positional(key.clone()))
                .map_err(mysql_error)?
                .ok_or("服务器未返回待删除的整行副本数量。")?
        } else {
            1
        };
        for guard in guards {
            let referencing_row = transaction
                .exec_first::<u8, _, _>(&guard.query, Params::Positional(key.clone()))
                .map_err(mysql_error)?;
            check_write_warnings(transaction)?;
            if referencing_row.is_some() {
                return Err(format!(
                    "删除此行会级联修改 {} 中的关联行，同步事务已取消。",
                    guard.table,
                ));
            }
        }
        transaction
            .exec_drop(&self.delete, Params::Positional(key.clone()))
            .map_err(mysql_error)?;
        if copies == 0 || transaction.affected_rows() != copies {
            return Err("删除行数异常，同步事务已取消。".into());
        }
        check_write_warnings(transaction)?;
        if layout.lookup(transaction, key, false, false)?.is_some() {
            return Err("服务器仍返回已计划删除的行，同步事务已取消。".into());
        }
        Ok(())
    }
}

fn lock_original(
    transaction: &mut Transaction<'_>,
    layout: &Layout,
    original: &[Value],
) -> Result<Vec<Value>, String> {
    let key = layout.key_values(original);
    let current = layout.lookup(transaction, key.clone(), true, true)?;
    if current
        .as_deref()
        .is_none_or(|row| !rows_equal(row, original))
    {
        return Err("接收行在同步期间被其他操作修改或删除，请重新比较。".into());
    }
    Ok(key)
}

struct DeleteGuard {
    table: String,
    query: String,
}

const DELETE_RELATIONS_SQL: &str = "
        SELECT k.TABLE_SCHEMA, k.TABLE_NAME, k.CONSTRAINT_NAME,
               k.COLUMN_NAME, k.REFERENCED_COLUMN_NAME
        FROM information_schema.KEY_COLUMN_USAGE k
        JOIN information_schema.REFERENTIAL_CONSTRAINTS r
          ON CAST(k.CONSTRAINT_SCHEMA AS BINARY) = CAST(r.CONSTRAINT_SCHEMA AS BINARY)
          AND CAST(k.CONSTRAINT_NAME AS BINARY) = CAST(r.CONSTRAINT_NAME AS BINARY)
          AND CAST(k.TABLE_NAME AS BINARY) = CAST(r.TABLE_NAME AS BINARY)
        WHERE k.REFERENCED_TABLE_SCHEMA = CONVERT(:schema USING utf8mb4)
          AND k.REFERENCED_TABLE_NAME = CONVERT(:table USING utf8mb4)
          AND CAST(IF(@@lower_case_table_names = 0,
                    k.REFERENCED_TABLE_SCHEMA, LOWER(k.REFERENCED_TABLE_SCHEMA)) AS BINARY)
            = CAST(IF(@@lower_case_table_names = 0,
                    CONVERT(:schema USING utf8mb4), LOWER(CONVERT(:schema USING utf8mb4))) AS BINARY)
          AND CAST(IF(@@lower_case_table_names = 0,
                    k.REFERENCED_TABLE_NAME, LOWER(k.REFERENCED_TABLE_NAME)) AS BINARY)
            = CAST(IF(@@lower_case_table_names = 0,
                    CONVERT(:table USING utf8mb4), LOWER(CONVERT(:table USING utf8mb4))) AS BINARY)
          AND r.DELETE_RULE NOT IN ('RESTRICT', 'NO ACTION')
        ORDER BY k.ORDINAL_POSITION";

type DeleteRelation = (String, String, String, String, String);

fn delete_guards(
    transaction: &mut Transaction<'_>,
    object: &DbObject,
    layout: &Layout,
) -> Result<Vec<DeleteGuard>, String> {
    let relations = transaction
        .exec_opt::<DeleteRelation, _, _>(
            DELETE_RELATIONS_SQL,
            params! { "schema" => object.schema.as_str(), "table" => object.name.as_str() },
        )
        .map_err(mysql_error)?
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| mysql_error(error.into()))?;
    Ok(build_delete_guards(layout, &relations))
}

fn build_delete_guards(layout: &Layout, relations: &[DeleteRelation]) -> Vec<DeleteGuard> {
    let mut constraints = BTreeMap::<(CatalogTable, String), Vec<String>>::new();
    for (schema, table, constraint, child_col, parent_col) in relations {
        constraints
            .entry(((schema.clone(), table.clone()), constraint.clone()))
            .or_default()
            .push(format!(
                "c.{} = p.{}",
                quote_identifier(child_col),
                quote_identifier(parent_col),
            ));
    }
    let parent_key = layout.key_predicate(Some("p"));
    constraints.into_iter().map(|((child, _), columns)| {
        let table = qualified_table(&child);
        // Read through the parent key to compare foreign key values with server type semantics.
        let query = format!(
            "SELECT 1 FROM {} AS p STRAIGHT_JOIN {table} AS c ON {} WHERE {parent_key} LIMIT 1 FOR UPDATE",
            layout.qualified, columns.join(" AND "),
        );
        DeleteGuard { table, query }
    }).collect()
}

fn verify_write(
    transaction: &mut Transaction<'_>,
    layout: &Layout,
    incoming: &[Value],
) -> Result<(), String> {
    check_write_warnings(transaction)?;
    let stored = layout.lookup(transaction, layout.key_values(incoming), false, true)?;
    if stored
        .as_deref()
        .is_none_or(|row| !rows_equal(row, incoming))
    {
        return Err(
            "服务器写回的数据与源端不同，同步事务已取消。请检查目标表约束、生成列和触发器。".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use mysql::OptsBuilder;

    use super::*;

    struct Fixture {
        opts: Opts,
        connection: Conn,
        schema: String,
    }

    impl Fixture {
        fn new(test: &str) -> Self {
            let port = std::env::var("HARBOR_SYNC_TEST_PORT")
                .unwrap()
                .parse()
                .unwrap();
            let opts: Opts = OptsBuilder::new()
                .ip_or_hostname(Some("127.0.0.1"))
                .tcp_port(port)
                .user(Some("root"))
                .prefer_socket(false)
                .into();
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let schema = format!("harbor_engine_{test}_{stamp:x}_{:x}", std::process::id());
            let mut connection = Conn::new(opts.clone()).unwrap();
            connection
                .query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
                .unwrap();
            connection.select_db(&schema).unwrap();
            Self {
                opts,
                connection,
                schema,
            }
        }

        fn sql(&mut self, sql: &str) {
            self.connection.query_drop(sql).unwrap();
        }

        fn rows<T: mysql::prelude::FromRow>(&mut self, sql: &str) -> Vec<T> {
            self.connection.query(sql).unwrap()
        }

        fn table(&self, name: &str) -> DbObject {
            DbObject {
                schema: self.schema.clone(),
                name: name.into(),
                kind: ObjectKind::Table,
            }
        }

        fn compare(&self, mode: SyncMode) -> SyncPlan {
            compare(
                &self.opts,
                self.table("src"),
                &self.opts,
                self.table("dst"),
                mode,
                |_| Ok(()),
            )
            .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self
                .connection
                .query_drop(format!("DROP DATABASE {}", self.schema));
        }
    }

    #[test]
    fn sync_filter_accepts_read_only_subqueries_and_rejects_unsafe_expressions() {
        for unsafe_expression in [
            "id = 1; DELETE FROM t",
            "1 /*!50000 UNION SELECT 1 */",
            "TRUE -- wrapper",
            "(WITH s AS (SELECT 1) SELECT * FROM s) = 1",
            "IF(id > 0, SLEEP(1), 0) = 0",
            "LOAD_FILE('out.txt') IS NULL",
            "external.unknown(id) = 1",
            "\x60abs\x60(id) = 1",
            "(@variable := id) > 0",
            "SET value = 0",
        ] {
            assert!(
                SyncFilter::new(unsafe_expression, SyncFilterScope::Source).is_err(),
                "{unsafe_expression}"
            );
        }
        for pure_expression in [
            "host like '%%'",
            "modtime > (SELECT syncstart - INTERVAL 1 DAY FROM lastsync)",
            "id IN (SELECT id FROM other_table)",
            "EXISTS (SELECT 1 FROM catalog.active_items AS a WHERE a.active = TRUE)",
            "kind IN (1, 2.5e-2) AND payload = X'00FF' AND flag = b'100'",
            "created >= CURRENT_DATE - INTERVAL 1 DAY AND created <= NOW(6)",
            "CASE WHEN checked IS NULL THEN name ELSE LOWER(name) END LIKE '李%; --'",
            "CAST(amount AS DECIMAL(12,2)) > 4.50 AND id <=> 7",
        ] {
            assert!(
                SyncFilter::new(pure_expression, SyncFilterScope::Both).is_ok(),
                "{pure_expression}"
            );
        }
    }

    #[test]
    #[ignore = "需要一次性 MariaDB 实例，通过 HARBOR_SYNC_TEST_PORT 传入端口"]
    fn source_filter_reviews_complete_key_matches_without_deleting_other_target_rows() {
        let mut db = Fixture::new("source_filter");
        db.sql(
            "CREATE TABLE src (
                id INT PRIMARY KEY, host VARCHAR(40), revision INT, payload VARBINARY(10),
                observed DATETIME(6), meta JSON
             ) ENGINE=InnoDB",
        );
        db.sql("CREATE TABLE dst LIKE src");
        db.sql(
            "INSERT INTO src SELECT seq, IF(seq <= 260, 'Active 中文', 'archived'), 1,
                X'00FF', '2024-08-22 09:30:15.123456', '{\"phase\":\"shown\"}'
             FROM seq_1_to_290",
        );
        db.sql(
            "INSERT INTO dst VALUES
                (1,'archived',5,X'01','2023-03-01','{}'),
                (2,'archived',1,X'02','2023-03-01','{}'),
                (300,'other',1,X'03','2023-03-01','{}')",
        );
        let filter = SyncFilter::new(
            "WHERE LOWER(host) LIKE 'active 中文%'
               AND (id BETWEEN 1 AND 255 OR id IN (256,257,258,259,260))
               AND CAST(COALESCE(revision, 0) AS DECIMAL(4,0)) IN (1,2)
               AND observed <= UTC_TIMESTAMP(6)
               AND JSON_UNQUOTE(JSON_EXTRACT(meta, '$.phase')) = 'shown'
               AND payload = X'00FF'",
            SyncFilterScope::Source,
        )
        .unwrap();
        let plan = compare_filtered(
            &db.opts,
            &db.opts,
            SyncMapping {
                source: db.table("src"),
                target: db.table("dst"),
                filter: Some(filter),
                compare_columns: vec!["REVISION".into()],
            },
            SyncMode::MirrorReceiver,
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(plan.compare_columns(), ["revision"]);
        assert_eq!(plan.filter().unwrap().scope(), SyncFilterScope::Source);
        assert_eq!(
            plan.counts,
            SyncCounts {
                same: 1,
                source_only: 258,
                target_only: 0,
                conflicts: 1
            }
        );
        assert_eq!(plan.outside_filter, 0);
        // A target row at a key outside the approved source selection can evolve independently.
        db.sql("UPDATE dst SET payload = X'AB' WHERE id = 300");
        let result = apply(&db.opts, &db.opts, &plan, |_| Ok(())).unwrap();
        assert_eq!(
            result.target,
            SyncWrites {
                inserted: 258,
                updated: 1,
                deleted: 0
            }
        );
        assert_eq!(
            db.rows::<(i32, String, String)>(
                "SELECT id, host, HEX(payload) FROM dst WHERE id IN (1,2,260,300) ORDER BY id"
            ),
            [
                (1, "Active 中文".into(), "00FF".into()),
                (2, "archived".into(), "02".into()),
                (260, "Active 中文".into(), "00FF".into()),
                (300, "other".into(), "AB".into()),
            ]
        );
    }

    #[test]
    #[ignore = "需要一次性 MariaDB 实例，通过 HARBOR_SYNC_TEST_PORT 传入端口"]
    fn symmetric_filter_keeps_boundary_keys_out_of_mirror_and_reconciliation() {
        let mut db = Fixture::new("both_filter");
        db.sql("CREATE TABLE src (id INT PRIMARY KEY, active BOOL, payload VARBINARY(10)) ENGINE=InnoDB");
        db.sql("CREATE TABLE dst LIKE src");
        db.sql(
            "INSERT INTO src VALUES(1,1,X'11'),(2,1,X'22'),(3,1,X'33'),(4,0,X'44'),(90,0,X'AB')",
        );
        db.sql(
            "INSERT INTO dst VALUES(1,1,X'01'),(3,0,X'A3'),(4,1,X'A4'),(5,1,X'05'),(90,0,X'CD')",
        );
        let mapping = SyncMapping {
            source: db.table("src"),
            target: db.table("dst"),
            filter: Some(SyncFilter::new("active IS TRUE", SyncFilterScope::Both).unwrap()),
            compare_columns: Vec::new(),
        };
        let plan = compare_filtered(
            &db.opts,
            &db.opts,
            mapping.clone(),
            SyncMode::MirrorReceiver,
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(plan.outside_filter, 2);
        assert_eq!(
            plan.planned_target(),
            SyncWrites {
                inserted: 1,
                updated: 1,
                deleted: 1
            }
        );
        // Selected row data and the number of boundary keys are identical, but the key set changed.
        db.sql("DELETE FROM dst WHERE id = 3");
        db.sql("INSERT INTO dst VALUES(2,0,X'A2')");
        assert!(
            apply(&db.opts, &db.opts, &plan, |_| Ok(()))
                .unwrap_err()
                .contains("筛选范围边界")
        );
        db.sql("DELETE FROM dst WHERE id = 2");
        db.sql("INSERT INTO dst VALUES(3,0,X'A3')");
        let result = apply(&db.opts, &db.opts, &plan, |_| Ok(())).unwrap();
        assert_eq!(result.target, plan.planned_target());
        assert_eq!(
            db.rows::<(i32, i32, String)>("SELECT id, active, HEX(payload) FROM dst ORDER BY id"),
            [
                (1, 1, "11".into()),
                (2, 1, "22".into()),
                (3, 0, "A3".into()),
                (4, 1, "A4".into()),
                (90, 0, "CD".into())
            ],
        );

        db.sql("INSERT INTO src VALUES(6,1,X'66')");
        db.sql("INSERT INTO dst VALUES(7,1,X'77')");
        let plan = compare_many_filtered(
            &db.opts,
            &db.opts,
            vec![mapping],
            SyncMode::ReconcileBoth(SyncConflict::KeepSeparate),
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(plan.tables[0].outside_filter, 2);
        let result = apply_many(&db.opts, &db.opts, &plan, |_| Ok(())).unwrap();
        assert_eq!((result.source.inserted, result.target.inserted), (1, 1));
        assert_eq!(
            db.rows::<i32>("SELECT id FROM src WHERE active ORDER BY id"),
            [1, 2, 3, 6, 7]
        );
        assert_eq!(
            db.rows::<i32>("SELECT id FROM dst WHERE active ORDER BY id"),
            [1, 2, 4, 6, 7]
        );
    }

    #[test]
    #[ignore = "需要一次性 MariaDB 实例，通过 HARBOR_SYNC_TEST_PORT 传入端口"]
    fn compares_and_applies_all_pages_without_lossy_coercion() {
        let mut db = Fixture::new("pages");
        db.sql(
            "CREATE TABLE src (
                region VARCHAR(16) CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci,
                code DECIMAL(30,4), payload VARBINARY(16), flag BIT(64), serial BIGINT UNSIGNED,
                label VARCHAR(16) CHARACTER SET latin1, dtype ENUM('2','0',''), tags SET('a','b',''),
                stamp TIMESTAMP(6) DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
                elapsed TIME(6), ratio FLOAT, multiplier DOUBLE, missing VARCHAR(10),
                generated DECIMAL(30,4) AS (code + 1) STORED,
                PRIMARY KEY (region, code)
             ) ENGINE=InnoDB",
        );
        db.sql("CREATE TABLE dst LIKE src");
        db.sql(
            "INSERT INTO src (region, code, payload, flag, serial, label, dtype, tags, stamp, elapsed,
                              ratio, multiplier, missing)
             SELECT 'Nord', CAST(9007199254740992 AS DECIMAL(30,4)) + seq / 10000,
                 X'00FF005C', 18446744073709551615, 18446744073709551615,
                 CONVERT(X'DFE9' USING latin1), 2, 4, '2024-10-27 01:30:01.345678',
                 '-25:34:56.123456', 0.3125, 1.0e-160, NULL
             FROM seq_1_to_1301",
        );
        db.sql(
            "INSERT INTO dst (region, code, payload, flag, serial, label, dtype, tags, stamp, elapsed,
                              ratio, multiplier, missing)
             SELECT region, code, payload, flag, serial, label, CAST(dtype AS UNSIGNED),
                    CAST(tags AS UNSIGNED), stamp, elapsed, ratio, multiplier, missing
             FROM src WHERE code > 9007199254740992.0003",
        );
        db.sql(
            "UPDATE dst SET region = 'nord', payload = X'00', dtype = 3, tags = 1, missing = ''
             WHERE code = 9007199254740992.1200",
        );
        db.sql("INSERT INTO dst (region, code) VALUES ('Spare', 1)");
        let plan = db.compare(SyncMode::InsertAndUpdate);
        assert_eq!(plan.key_names, ["region", "code"]);
        assert_eq!(
            plan.counts,
            SyncCounts {
                same: 1297,
                source_only: 3,
                target_only: 1,
                conflicts: 1
            }
        );
        let mut finishing = false;
        let result = apply(&db.opts, &db.opts, &plan, |progress| {
            if progress.finishing {
                assert_eq!(progress.written, 4);
                finishing = true;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(
            result,
            SyncResult {
                target: SyncWrites {
                    inserted: 3,
                    updated: 1,
                    deleted: 0
                },
                ..Default::default()
            }
        );
        assert!(finishing);
        assert_eq!(
            db.compare(SyncMode::InsertMissing).counts,
            SyncCounts {
                same: 1301,
                source_only: 0,
                target_only: 1,
                conflicts: 0
            },
        );
    }

    #[test]
    #[ignore = "需要一次性 MariaDB 实例，通过 HARBOR_SYNC_TEST_PORT 传入端口"]
    fn rejects_stale_preview_and_rolls_back_after_a_concurrent_edit() {
        let mut db = Fixture::new("stale");
        db.sql("CREATE TABLE src (id INT PRIMARY KEY, value VARBINARY(16)) ENGINE=InnoDB");
        db.sql("CREATE TABLE dst LIKE src");
        db.sql("INSERT INTO src VALUES (1, 'first'), (2, 'second'), (3, 'new')");
        db.sql("INSERT INTO dst VALUES (1, 'before'), (2, 'before')");
        let plan = db.compare(SyncMode::InsertAndUpdate);
        db.sql("UPDATE src SET value = 'source edit' WHERE id = 1");
        assert!(
            apply(&db.opts, &db.opts, &plan, |_| Ok(()))
                .unwrap_err()
                .contains("源端数据已变化")
        );

        let plan = db.compare(SyncMode::InsertAndUpdate);
        let opts = db.opts.clone();
        let mut edited = false;
        let error = apply(&opts, &opts, &plan, |progress| {
            if progress.written == 1 && !edited {
                db.sql("UPDATE dst SET value = 'peer' WHERE id = 2");
                edited = true;
            }
            Ok(())
        })
        .unwrap_err();
        assert!(
            edited && error.contains("接收行在同步期间被其他操作修改"),
            "{error}"
        );
        assert_eq!(
            db.connection
                .query::<(i32, String), _>("SELECT id, value FROM dst ORDER BY id")
                .unwrap(),
            [(1, "before".into()), (2, "peer".into())]
        );
        let plan = db.compare(SyncMode::InsertMissing);
        assert_eq!(
            apply(&opts, &opts, &plan, |_| Ok(())).unwrap(),
            SyncResult {
                target: SyncWrites {
                    inserted: 1,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        assert_eq!(db.compare(SyncMode::InsertMissing).counts.conflicts, 2);
    }

    #[test]
    #[ignore = "需要一次性 MariaDB 实例，通过 HARBOR_SYNC_TEST_PORT 传入端口"]
    fn orders_selected_foreign_keys_and_reports_commits_before_a_stale_table() {
        let mut db = Fixture::new("group");
        db.sql("CREATE TABLE `源部门` (id INT PRIMARY KEY) ENGINE=InnoDB");
        db.sql("CREATE TABLE `目标部门` LIKE `源部门`");
        db.sql(
            "CREATE TABLE `源员工` (
                id INT PRIMARY KEY, department INT,
                FOREIGN KEY (department) REFERENCES `源部门` (id)
             ) ENGINE=InnoDB",
        );
        db.sql(
            "CREATE TABLE `目标员工` (
                id INT PRIMARY KEY, department INT,
                FOREIGN KEY (department) REFERENCES `目标部门` (id)
             ) ENGINE=InnoDB",
        );
        db.sql("INSERT INTO `源部门` VALUES (10)");
        db.sql("INSERT INTO `源员工` VALUES (1, 10)");
        let pairs = vec![
            (db.table("源员工"), db.table("目标员工")),
            (db.table("源部门"), db.table("目标部门")),
        ];
        let plan = |opts: &Opts, pairs| {
            compare_many(opts, opts, pairs, SyncMode::InsertMissing, |_| Ok(())).unwrap()
        };
        let review = plan(&db.opts, pairs.clone());
        assert_eq!(review.tables[0].target.name, "目标部门");
        db.sql("INSERT INTO `目标员工` VALUES (2, NULL)");
        let mut prior_commit = false;
        let result = apply_many(&db.opts, &db.opts, &review, |status| {
            if status.written == 1 && !status.finishing {
                prior_commit = true;
            }
            Ok(())
        })
        .unwrap();
        assert!(prior_commit);
        assert_eq!(
            (
                result.completed,
                result.target.inserted,
                result.target.updated
            ),
            (1, 1, 0)
        );
        assert!(result.stopped.unwrap().contains("目标端数据已变化"));
        assert_eq!(
            db.connection
                .query_first::<u32, _>("SELECT id FROM `目标部门`")
                .unwrap(),
            Some(10)
        );

        let review = plan(&db.opts, pairs);
        let result = apply_many(&db.opts, &db.opts, &review, |_| Ok(())).unwrap();
        assert_eq!(
            (
                result.completed,
                result.target.inserted,
                result.target.updated
            ),
            (2, 1, 0)
        );
        assert!(result.stopped.is_none());
        assert_eq!(
            db.connection
                .query::<(i32, Option<i32>), _>("SELECT id, department FROM `目标员工` ORDER BY id")
                .unwrap(),
            [(1, Some(10)), (2, None)]
        );
    }

    #[test]
    #[ignore = "需要一次性 MariaDB 实例，通过 HARBOR_SYNC_TEST_PORT 传入端口"]
    fn mirrors_selected_tables_in_fk_order_without_cascading_to_a_surviving_child() {
        let mut db = Fixture::new("mirror");
        db.sql("CREATE TABLE src_parent(id INT PRIMARY KEY, value VARBINARY(16)) ENGINE=InnoDB");
        db.sql("CREATE TABLE dst_parent LIKE src_parent");
        db.sql(
            "CREATE TABLE src_item (id INT PRIMARY KEY, parent INT, value VARBINARY(16),
                 FOREIGN KEY (parent) REFERENCES src_parent(id)) ENGINE=InnoDB",
        );
        db.sql(
            "CREATE TABLE dst_item (id INT PRIMARY KEY, parent INT, value VARBINARY(16),
                 FOREIGN KEY (parent) REFERENCES dst_parent(id) ON DELETE CASCADE) ENGINE=InnoDB",
        );
        db.sql(
            "CREATE TABLE dst_related (id INT PRIMARY KEY, parent INT,
                 FOREIGN KEY (parent) REFERENCES dst_parent(id) ON DELETE SET NULL) ENGINE=InnoDB",
        );
        db.sql("CREATE TABLE dst_audit (id INT PRIMARY KEY) ENGINE=InnoDB");
        db.sql("CREATE TRIGGER mirror_audit AFTER DELETE ON dst_item FOR EACH ROW INSERT INTO dst_audit VALUES(OLD.id)");
        db.sql("INSERT INTO src_parent VALUES (1, X'00FF'), (2, X'FE00')");
        db.sql("INSERT INTO dst_parent VALUES (1, X'00'), (9, X'01')");
        db.sql("INSERT INTO src_item VALUES (1, 2, X'FF00'), (2, 1, NULL)");
        db.sql("INSERT INTO dst_item VALUES (1, 9, X'00'), (9, 9, X'09')");
        db.sql("INSERT INTO dst_related VALUES (9, 9)");
        let pairs = vec![
            (db.table("src_item"), db.table("dst_item")),
            (db.table("src_parent"), db.table("dst_parent")),
        ];
        let review = compare_many(&db.opts, &db.opts, pairs, SyncMode::MirrorReceiver, |_| {
            Ok(())
        })
        .unwrap();
        for plan in &review.tables {
            assert_eq!(plan.planned_source(), SyncWrites::default());
            assert_eq!(
                plan.planned_target(),
                SyncWrites {
                    inserted: 1,
                    updated: 1,
                    deleted: 1
                }
            );
        }
        let error = apply_many(&db.opts, &db.opts, &review, |_| Ok(()))
            .err()
            .unwrap();
        assert!(
            error.contains("dst_related") && error.contains("级联修改"),
            "{error}"
        );
        assert_eq!(
            db.rows::<(i32, i32)>("SELECT id, parent FROM dst_item ORDER BY id"),
            [(1, 9), (9, 9)]
        );
        assert_eq!(
            db.rows::<i32>("SELECT id FROM dst_parent ORDER BY id"),
            [1, 9]
        );
        assert!(db.rows::<i32>("SELECT id FROM dst_audit").is_empty());
        assert_eq!(
            db.rows::<(i32, i32)>("SELECT id, parent FROM dst_related"),
            [(9, 9)]
        );

        db.sql("UPDATE dst_related SET parent = 1 WHERE id = 9");
        db.sql(
            "CREATE TRIGGER mirror_change_parent AFTER DELETE ON dst_item FOR EACH ROW
             UPDATE dst_parent SET value = X'11' WHERE id = 1",
        );
        let error = apply_many(&db.opts, &db.opts, &review, |_| Ok(()))
            .err()
            .unwrap();
        assert!(error.contains("暂存数据与源端快照不一致"), "{error}");
        assert!(db.rows::<i32>("SELECT id FROM dst_audit").is_empty());
        assert_eq!(
            db.rows::<i32>("SELECT id FROM dst_parent ORDER BY id"),
            [1, 9]
        );
        db.sql("DROP TRIGGER mirror_change_parent");

        let result = apply_many(&db.opts, &db.opts, &review, |_| Ok(())).unwrap();
        assert_eq!(result.completed, 2);
        assert_eq!(
            result.target,
            SyncWrites {
                inserted: 2,
                updated: 2,
                deleted: 2
            }
        );
        assert_eq!(result.source, SyncWrites::default());
        assert!(result.stopped.is_none() && result.uncertain.is_none());
        assert_eq!(
            db.rows::<(i32, String)>("SELECT id, HEX(value) FROM dst_parent ORDER BY id"),
            [(1, "00FF".into()), (2, "FE00".into())]
        );
        assert_eq!(
            db.rows::<(i32, i32, Option<String>)>(
                "SELECT id, parent, HEX(value) FROM dst_item ORDER BY id"
            ),
            [(1, 2, Some("FF00".into())), (2, 1, None)]
        );
        assert_eq!(db.rows::<i32>("SELECT id FROM dst_audit"), [9]);
        assert_eq!(
            db.rows::<(i32, i32)>("SELECT id, parent FROM dst_related"),
            [(9, 1)]
        );
    }

    #[test]
    #[ignore = "需要一次性 MariaDB 实例，通过 HARBOR_SYNC_TEST_PORT 传入端口"]
    fn reconciles_both_sides_and_reports_a_lost_second_commit() {
        let mut db = Fixture::new("reconcile");
        db.sql("CREATE TABLE src (id INT PRIMARY KEY, value VARBINARY(16)) ENGINE=InnoDB");
        db.sql("CREATE TABLE dst LIKE src");
        db.sql("INSERT INTO src VALUES (1, X'00FF'), (3, X'F001')");
        db.sql("INSERT INTO dst VALUES (2, X'FF00'), (3, X'F002')");
        let source_opts: Opts = OptsBuilder::from_opts(db.opts.clone())
            .db_name(Some(&db.schema))
            .into();
        let target_opts = db.opts.clone();
        let mode = SyncMode::ReconcileBoth(SyncConflict::KeepSeparate);
        let review = compare(
            &source_opts,
            db.table("src"),
            &target_opts,
            db.table("dst"),
            mode,
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(
            review.planned_source(),
            SyncWrites {
                inserted: 1,
                ..Default::default()
            }
        );
        assert_eq!(review.planned_target(), review.planned_source());

        let cancelled = apply(&source_opts, &target_opts, &review, |status| {
            if status.finishing && status.side == Some(SyncSide::Target) {
                return Err("已取消测试操作。".into());
            }
            Ok(())
        });
        assert!(cancelled.unwrap_err().contains("已取消"));
        assert_eq!(db.rows::<i32>("SELECT id FROM src ORDER BY id"), [1, 3]);
        assert_eq!(db.rows::<i32>("SELECT id FROM dst ORDER BY id"), [2, 3]);

        let result = apply(&source_opts, &target_opts, &review, |status| {
            if status.finishing && status.side == Some(SyncSide::Source) {
                let writer = db
                    .connection
                    .exec::<u32, _, _>(
                        "SELECT t.trx_mysql_thread_id FROM information_schema.INNODB_TRX t
                         JOIN information_schema.PROCESSLIST p ON p.ID = t.trx_mysql_thread_id
                         WHERE p.DB = :schema AND t.trx_rows_modified > 0",
                        params! { "schema" => db.schema.as_str() },
                    )
                    .unwrap();
                assert_eq!(writer.len(), 1, "locate only this fixture's source writer");
                db.sql(&format!("KILL CONNECTION {}", writer[0]));
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(result.target, review.planned_target());
        assert_eq!(result.source, SyncWrites::default());
        assert_eq!(result.uncertain, Some(SyncSide::Source));
        assert!(result.stopped.unwrap().contains("无法确认源端"));
        assert_eq!(db.rows::<i32>("SELECT id FROM src ORDER BY id"), [1, 3]);
        assert_eq!(db.rows::<i32>("SELECT id FROM dst ORDER BY id"), [1, 2, 3]);

        let review = db.compare(mode);
        let result = apply(&db.opts, &db.opts, &review, |_| Ok(())).unwrap();
        assert_eq!(result.source.inserted, 1);
        assert_eq!(
            db.rows::<(i32, String)>("SELECT id, HEX(value) FROM src ORDER BY id"),
            [(1, "00FF".into()), (2, "FF00".into()), (3, "F001".into())]
        );
        assert_eq!(
            db.rows::<String>("SELECT HEX(value) FROM dst WHERE id = 3"),
            ["F002"]
        );

        let review = db.compare(SyncMode::ReconcileBoth(SyncConflict::SourceWins));
        let result = apply(&db.opts, &db.opts, &review, |_| Ok(())).unwrap();
        assert_eq!(result.target.updated, 1);
        assert_eq!(
            db.rows::<String>("SELECT HEX(value) FROM dst WHERE id = 3"),
            ["F001"]
        );
        db.sql("UPDATE dst SET value = X'F003' WHERE id = 3");
        let review = db.compare(SyncMode::ReconcileBoth(SyncConflict::TargetWins));
        let result = apply(&db.opts, &db.opts, &review, |_| Ok(())).unwrap();
        assert_eq!(result.source.updated, 1);
        assert_eq!(
            db.rows::<String>("SELECT HEX(value) FROM src WHERE id = 3"),
            ["F003"]
        );
    }

    #[test]
    #[ignore = "需要一次性 MariaDB 实例，通过 HARBOR_SYNC_TEST_PORT 传入端口"]
    fn reconciles_scopes_with_different_foreign_key_order_on_each_endpoint() {
        let mut db = Fixture::new("both_fk");
        db.sql("CREATE TABLE src_b (id INT PRIMARY KEY, parent INT) ENGINE=InnoDB");
        db.sql("CREATE TABLE dst_a LIKE src_b");
        db.sql("CREATE TABLE src_a (id INT PRIMARY KEY, parent INT, FOREIGN KEY (parent) REFERENCES src_b(id)) ENGINE=InnoDB");
        db.sql("CREATE TABLE dst_b (id INT PRIMARY KEY, parent INT, FOREIGN KEY (parent) REFERENCES dst_a(id)) ENGINE=InnoDB");
        db.sql("INSERT INTO src_b VALUES(10, 10)");
        db.sql("INSERT INTO src_a VALUES(10, 10)");
        db.sql("INSERT INTO dst_a VALUES(20, 20)");
        db.sql("INSERT INTO dst_b VALUES(20, 20)");
        let pairs = vec![
            (db.table("src_b"), db.table("dst_b")),
            (db.table("src_a"), db.table("dst_a")),
        ];
        let mode = SyncMode::ReconcileBoth(SyncConflict::KeepSeparate);
        let review = compare_many(&db.opts, &db.opts, pairs, mode, |_| Ok(())).unwrap();
        let result = apply_many(&db.opts, &db.opts, &review, |_| Ok(())).unwrap();
        assert_eq!(result.completed, 2);
        assert_eq!(result.source.inserted, 2);
        assert_eq!(result.target.inserted, 2);
        assert!(result.stopped.is_none() && result.uncertain.is_none());
        for table in ["src_a", "src_b", "dst_a", "dst_b"] {
            assert_eq!(
                db.rows::<(i32, i32)>(&format!("SELECT id, parent FROM {table} ORDER BY id")),
                [(10, 10), (20, 20)]
            );
        }
    }
}
