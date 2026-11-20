use std::borrow::Cow;
use std::io::{self, Read, Write};
use std::ops::Range;

pub mod delimited;
mod format_layout;
mod stream;
pub use format_layout::{MAX_FORMAT_INDENT, SqlFormatSettings, format_sql_with_settings};
pub(crate) use stream::StreamingStatementReader;

#[derive(Debug, PartialEq, Eq)]
pub struct SqlStatement<'a> {
    pub sql: &'a str,
    pub range: Range<usize>,
}

/// Finds statements in a MySQL script using default string-escaping rules.
/// Ranges use UTF-8 byte offsets and exclude statement delimiters.
pub fn split_script(script: &str) -> Vec<SqlStatement<'_>> {
    ScriptScanner::new(script)
        .map(|part| part.statement)
        .collect()
}

/// A caret on a delimiter belongs to that statement. Space between statements
/// belongs to the next statement, or to the last statement at the end of input.
pub fn statement_at(script: &str, byte_offset: usize) -> Option<SqlStatement<'_>> {
    statement_at_with_quoting(script, byte_offset, (false, false))
}

/// Finds the SQL to execute at a UTF-8 byte offset. On a DELIMITER command,
/// the caret belongs to the following statement when there is one.
pub fn statement_at_for_execution(script: &str, byte_offset: usize) -> Option<SqlStatement<'_>> {
    find_statement_at(script, byte_offset, Quoting::default(), true)
}

pub fn statement_at_with_quoting(
    script: &str,
    byte_offset: usize,
    (no_backslash_escapes, ansi_quotes): (bool, bool),
) -> Option<SqlStatement<'_>> {
    let quoting = Quoting {
        backslash: !no_backslash_escapes,
        ansi_identifiers: ansi_quotes,
    };
    find_statement_at(script, byte_offset, quoting, false)
}

fn find_statement_at(
    script: &str,
    byte_offset: usize,
    quoting: Quoting,
    skip_trailing_command: bool,
) -> Option<SqlStatement<'_>> {
    let mut previous = None;
    let mut caret_on_command = false;
    let mut scanner = ScriptScanner::new(script);
    scanner.quoting = quoting;
    while let Some(part) = scanner.next_part(|start, end| {
        if skip_trailing_command && byte_offset <= end {
            let line_start = script[..start]
                .rfind(['\r', '\n'])
                .map_or(0, |line_break| line_break + 1);
            caret_on_command |= byte_offset >= line_start;
        }
    }) {
        if byte_offset < part.end {
            return Some(part.statement);
        }
        previous = Some(part.statement);
    }
    previous.filter(|_| !caret_on_command)
}

struct ScriptPart<'a> {
    statement: SqlStatement<'a>,
    end: usize,
    custom_delimiter: bool,
}

struct ScriptScanner<'a> {
    script: &'a str,
    offset: usize,
    delimiter: Cow<'a, str>,
    at_line_start: bool,
    quoting: Quoting,
}

#[derive(Clone, Copy)]
struct Quoting {
    backslash: bool,
    ansi_identifiers: bool,
}

impl Default for Quoting {
    fn default() -> Self {
        Self {
            backslash: true,
            ansi_identifiers: false,
        }
    }
}

impl Quoting {
    fn from_sql_mode(mode: &str) -> Self {
        let has_mode = |flag| {
            mode.split(',')
                .any(|token| token.trim().eq_ignore_ascii_case(flag))
        };
        Self {
            backslash: !has_mode("NO_BACKSLASH_ESCAPES"),
            ansi_identifiers: has_mode("ANSI_QUOTES"),
        }
    }
}

pub(crate) struct StatementReader<'a>(ScriptScanner<'a>);

impl<'a> StatementReader<'a> {
    pub(crate) fn new(sql: &'a str) -> Self {
        Self(ScriptScanner::new(sql))
    }

    pub(crate) fn set_sql_mode(&mut self, mode: &str) {
        self.0.quoting = Quoting::from_sql_mode(mode);
    }

    pub(crate) fn next_statement(&mut self) -> Option<SqlStatement<'a>> {
        self.0.next().map(|part| part.statement)
    }
}

pub fn is_single_profile_select(sql: &str) -> bool {
    let mut input = sql.trim_start_matches('\u{feff}');
    loop {
        input = input.trim_ascii_start();
        let Some((width, false)) = protected_sql(input.as_bytes(), Quoting::default()) else {
            break;
        };
        input = &input[width..];
    }
    if !input
        .get(..6)
        .is_some_and(|word| word.eq_ignore_ascii_case("SELECT"))
        || input
            .as_bytes()
            .get(6)
            .is_some_and(|&byte| sql_word_byte(byte))
    {
        return false;
    }
    // The editor can have a session whose sql_mode changed after it was opened.
    [
        "",
        "NO_BACKSLASH_ESCAPES",
        "ANSI_QUOTES",
        "ANSI_QUOTES,NO_BACKSLASH_ESCAPES",
    ]
    .into_iter()
    .all(|mode| {
        let mut reader = StatementReader::new(input);
        reader.set_sql_mode(mode);
        reader.next_statement().is_some() && reader.next_statement().is_none()
    })
}

fn sql_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$') || !byte.is_ascii()
}

pub(crate) fn needs_unmodified_query_diagnostics(mut sql: &str) -> bool {
    while let Some(&first) = sql.as_bytes().first() {
        if let Some((width, substantive)) = protected_sql(sql.as_bytes(), Quoting::default()) {
            if first == b'/' && substantive {
                return true;
            }
            sql = &sql[width..];
        } else if sql_word_byte(first) {
            let end = sql.bytes().take_while(|&byte| sql_word_byte(byte)).count();
            let word = &sql[..end];
            if [
                "ROW_COUNT",
                "FOUND_ROWS",
                "SQL_CALC_FOUND_ROWS",
                "WARNING_COUNT",
                "ERROR_COUNT",
            ]
            .iter()
            .any(|keyword| word.eq_ignore_ascii_case(keyword))
            {
                return true;
            }
            sql = &sql[end..];
        } else {
            sql = &sql[1..];
        }
    }
    false
}

pub(crate) fn can_explain_profile_select(mut sql: &str, mode: &str) -> bool {
    let quoting = Quoting::from_sql_mode(mode);
    let mut candidate = None;
    let mut qualified = false;
    while !sql.is_empty() {
        sql = sql.trim_ascii_start();
        let Some(&first) = sql.as_bytes().first() else {
            break;
        };
        if sql.starts_with("/*!") || sql.starts_with("/*M!") || sql.starts_with("/*m!") {
            return false;
        }
        if let Some((width, substantive)) = protected_sql(sql.as_bytes(), quoting) {
            if substantive {
                candidate = None;
                if first == b'`' || first == b'"' && quoting.ansi_identifiers {
                    candidate = Some(false);
                }
                qualified = false;
            }
            sql = &sql[width..];
            continue;
        }
        if sql_word_byte(first) {
            let end = sql.bytes().take_while(|&byte| sql_word_byte(byte)).count();
            let word = sql[..end].to_ascii_uppercase();
            if word == "INTO" {
                return false;
            }
            candidate = Some(
                !qualified
                    && matches!(
                        word.as_str(),
                        "SELECT"
                            | "WHERE"
                            | "HAVING"
                            | "IF"
                            | "AND"
                            | "OR"
                            | "NOT"
                            | "XOR"
                            | "FROM"
                            | "JOIN"
                            | "ON"
                            | "IN"
                            | "EXISTS"
                            | "AS"
                            | "OVER"
                            | "USING"
                            | "DISTINCT"
                            | "ALL"
                            | "BETWEEN"
                            | "THEN"
                            | "ELSE"
                            | "WHEN"
                            | "UNION"
                            | "ABS"
                            | "AVG"
                            | "CAST"
                            | "CEIL"
                            | "CEILING"
                            | "CHAR_LENGTH"
                            | "COALESCE"
                            | "CONCAT"
                            | "CONCAT_WS"
                            | "CONVERT"
                            | "COUNT"
                            | "DATE"
                            | "DATEDIFF"
                            | "FLOOR"
                            | "GROUP_CONCAT"
                            | "IFNULL"
                            | "LENGTH"
                            | "LOWER"
                            | "MAX"
                            | "MIN"
                            | "NOW"
                            | "NULLIF"
                            | "REPLACE"
                            | "ROUND"
                            | "ROW_NUMBER"
                            | "SUBSTR"
                            | "SUBSTRING"
                            | "SUM"
                            | "TRIM"
                            | "UPPER"
                    ),
            );
            qualified = false;
            sql = &sql[end..];
            continue;
        }
        if first == b'@' || first == b'(' && candidate == Some(false) {
            return false;
        }
        candidate = None;
        qualified = first == b'.';
        sql = &sql[1..];
    }
    true
}

impl<'a> ScriptScanner<'a> {
    fn new(script: &'a str) -> Self {
        Self {
            script,
            offset: if script.starts_with('\u{feff}') { 3 } else { 0 },
            delimiter: Cow::Borrowed(";"),
            at_line_start: true,
            quoting: Quoting::default(),
        }
    }

    fn part(&self, range: Range<usize>) -> ScriptPart<'a> {
        let content = &self.script[range.clone()];
        let sql = content.trim_ascii();
        let start = range.start + content.len() - content.trim_ascii_start().len();
        ScriptPart {
            statement: SqlStatement {
                sql,
                range: start..start + sql.len(),
            },
            end: self.offset,
            custom_delimiter: self.delimiter != ";",
        }
    }

    fn next_part(&mut self, mut on_delimiter: impl FnMut(usize, usize)) -> Option<ScriptPart<'a>> {
        let bytes = self.script.as_bytes();
        let mut start = self.offset;
        let mut has_sql = false;

        while let Some(&byte) = bytes.get(self.offset) {
            if self.at_line_start && !has_sql && matches!(byte, b'd' | b'D' | b'\\') {
                if let Some((delimiter, width)) = delimiter_command(&self.script[self.offset..]) {
                    on_delimiter(self.offset, self.offset + width);
                    self.delimiter = delimiter;
                    self.offset += width;
                    start = self.offset;
                    continue;
                }
            }

            let remaining = &bytes[self.offset..];
            if remaining.starts_with(self.delimiter.as_bytes()) {
                let end = self.offset;
                self.offset += self.delimiter.len();
                self.at_line_start = false;
                if has_sql {
                    return Some(self.part(start..end));
                }
                start = self.offset;
                continue;
            }

            if byte.is_ascii_whitespace() {
                self.at_line_start |= matches!(byte, b'\r' | b'\n');
                self.offset += 1;
                continue;
            }

            self.at_line_start = false;
            if let Some((width, substantive)) = protected_sql(remaining, self.quoting) {
                has_sql |= substantive;
                self.offset += width;
            } else {
                has_sql = true;
                self.offset += 1;
            }
        }

        has_sql.then(|| self.part(start..self.offset))
    }
}

impl<'a> Iterator for ScriptScanner<'a> {
    type Item = ScriptPart<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_part(|_, _| {})
    }
}

fn delimiter_command(input: &str) -> Option<(Cow<'_, str>, usize)> {
    let rest = if input
        .get(..9)
        .is_some_and(|word| word.eq_ignore_ascii_case("delimiter"))
    {
        &input[9..]
    } else {
        input.strip_prefix("\\d")?
    };
    if !rest.as_bytes().first().is_some_and(u8::is_ascii_whitespace) {
        return None;
    }

    let line_end = input.find(['\r', '\n']).unwrap_or(input.len());
    let args = &rest[..line_end - (input.len() - rest.len())];
    let args = args.trim_ascii_start();
    let quote = *args.as_bytes().first()?;
    let delimiter = if matches!(quote, b'\'' | b'"' | b'`') {
        let end = quoted_end(args.as_bytes(), true)?;
        let value = &args[1..end - 1];
        let escaped_quote = format!("\\{}", char::from(quote));
        if value.contains(&escaped_quote) {
            Cow::Owned(value.replace(&escaped_quote, &char::from(quote).to_string()))
        } else {
            Cow::Borrowed(value)
        }
    } else {
        Cow::Borrowed(args.split_ascii_whitespace().next()?)
    };
    (!delimiter.is_empty()).then_some((delimiter, line_end))
}

// Returns the byte width and whether the lexeme itself can be sent as SQL.
fn protected_sql(input: &[u8], quoting: Quoting) -> Option<(usize, bool)> {
    let first = *input.first()?;
    if matches!(first, b'\'' | b'"' | b'`') {
        return Some((
            quoted_end(
                input,
                quoting.backslash && first != b'`' && !(first == b'"' && quoting.ansi_identifiers),
            )
            .unwrap_or(input.len()),
            true,
        ));
    }

    let dash_comment = input.starts_with(b"--")
        && input
            .get(2)
            .is_none_or(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control());
    if first == b'#' || dash_comment {
        let width = input.iter().position(|byte| matches!(byte, b'\r' | b'\n'));
        return Some((width.unwrap_or(input.len()), false));
    }

    if input.starts_with(b"/*") {
        let end = input[2..].windows(2).position(|pair| pair == b"*/");
        let executable = input.starts_with(b"/*!") || input.starts_with(b"/*M!");
        return Some(match end {
            Some(end) => (end + 4, executable),
            None => (input.len(), true),
        });
    }

    None
}

pub(crate) fn set_assignments<'a>(input: &'a str, sql_mode: &str) -> Vec<&'a str> {
    let quoting = Quoting::from_sql_mode(sql_mode);
    let bytes = input.as_bytes();
    let (mut start, mut offset, mut depth) = (0, 0, 0usize);
    let mut parts = Vec::new();
    while let Some(&byte) = bytes.get(offset) {
        if let Some((width, _)) = protected_sql(&bytes[offset..], quoting) {
            offset += width;
            continue;
        }
        match byte {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                parts.push(input[start..offset].trim());
                start = offset + 1;
            }
            _ => {}
        }
        offset += 1;
    }
    parts.push(input[start..].trim());
    parts
}

fn quoted_end(input: &[u8], backslash_escapes: bool) -> Option<usize> {
    quoted_end_with(input, input[0], backslash_escapes)
}

fn quoted_end_with(input: &[u8], quote: u8, backslash_escapes: bool) -> Option<usize> {
    let mut offset = 1;
    while let Some(&byte) = input.get(offset) {
        offset += 1;
        if byte == quote {
            if input.get(offset) != Some(&quote) {
                return Some(offset);
            }
            offset += 1;
        } else if byte == b'\\' && backslash_escapes {
            offset += 1;
        }
    }
    None
}

/// Formats ordinary statements and preserves the surrounding script, including
/// client commands and the bodies of statements using a custom delimiter.
pub fn format_sql(script: &str) -> String {
    format_sql_with_settings(
        script,
        0..script.len(),
        SqlFormatSettings::default(),
        (false, false),
    )
}

// Check that only insignificant whitespace changed. The generic formatter has
// different rules for some MySQL operators, comments, and literals.
fn formatting_tokens(mut sql: &str, quoting: Quoting) -> impl Iterator<Item = (&str, bool)> {
    let mut previous = "";
    std::iter::from_fn(move || {
        let trimmed = sql.trim_ascii_start();
        let bytes = trimmed.as_bytes();
        let first = *bytes.first()?;
        let width = if let Some((width, _)) = protected_sql(bytes, quoting) {
            width
        } else if let Some(width) = numeric_width(bytes) {
            width
        } else if word_byte(first) {
            bytes.iter().take_while(|&&byte| word_byte(byte)).count()
        } else {
            [
                "<=>", "->>", "->", "<<", ">>", "<=", ">=", "<>", "!=", ":=", "&&", "||",
            ]
            .into_iter()
            .find(|op| trimmed.starts_with(op))
            .map_or(1, str::len)
        };

        let previous_is_name = previous
            .as_bytes()
            .first()
            .is_some_and(|&byte| word_byte(byte) || byte == b'`');
        let sensitive_gap = (previous_is_name && matches!(first, b'(' | b'\'' | b'"' | b'`'))
            || first == b'@'
            || previous == "@";
        let joined = sensitive_gap && trimmed.len() == sql.len();
        let token = &trimmed[..width];
        sql = &trimmed[width..];
        previous = token;
        Some((token, joined))
    })
}

fn word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$') || !byte.is_ascii()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorTokenKind {
    Word,
    Identifier { quote: u8, closed: bool },
    Protected { closed: bool },
    Symbol(char),
}

pub struct EditorToken<'a> {
    pub text: &'a str,
    pub range: Range<usize>,
    pub kind: EditorTokenKind,
}

/// Scans editor text, including incomplete quoted tokens and comments.
pub fn editor_tokens(
    sql: &str,
    no_backslash_escapes: bool,
    ansi_quotes: bool,
) -> impl Iterator<Item = EditorToken<'_>> {
    let quoting = Quoting {
        backslash: !no_backslash_escapes,
        ansi_identifiers: ansi_quotes,
    };
    tokens_with_quoting(sql, quoting, false)
}

pub(crate) fn definition_tokens<'a>(
    sql: &'a str,
    sql_mode: &str,
) -> impl Iterator<Item = EditorToken<'a>> {
    let quoting = Quoting::from_sql_mode(sql_mode);
    let brackets = sql_mode
        .split(',')
        .any(|mode| mode.trim().eq_ignore_ascii_case("MSSQL"));
    tokens_with_quoting(sql, quoting, brackets)
}

fn tokens_with_quoting(
    sql: &str,
    quoting: Quoting,
    brackets: bool,
) -> impl Iterator<Item = EditorToken<'_>> {
    let mut offset = 0;
    std::iter::from_fn(move || {
        let rest = sql[offset..].trim_ascii_start();
        let bytes = rest.as_bytes();
        let first = *bytes.first()?;
        let bracket = first == b'[' && brackets;
        let (width, kind) = if matches!(first, b'\'' | b'"' | b'`') || bracket {
            let identifier =
                bracket || first == b'`' || (first == b'"' && quoting.ansi_identifiers);
            let close = if bracket { b']' } else { first };
            let end = quoted_end_with(bytes, close, !identifier && quoting.backslash);
            let closed = end.is_some();
            let kind = if identifier {
                EditorTokenKind::Identifier {
                    quote: first,
                    closed,
                }
            } else {
                EditorTokenKind::Protected { closed }
            };
            (end.unwrap_or(bytes.len()), kind)
        } else if let Some((width, _)) = protected_sql(bytes, quoting) {
            let closed = bytes.starts_with(b"/*") && width >= 4 && bytes[..width].ends_with(b"*/");
            (width, EditorTokenKind::Protected { closed })
        } else if word_byte(first) {
            (
                bytes.iter().take_while(|&&byte| word_byte(byte)).count(),
                EditorTokenKind::Word,
            )
        } else {
            (1, EditorTokenKind::Symbol(char::from(first)))
        };
        let start = sql.len() - rest.len();
        offset = start + width;
        Some(EditorToken {
            text: &sql[start..offset],
            range: start..offset,
            kind,
        })
    })
}

fn numeric_width(bytes: &[u8]) -> Option<usize> {
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
    if width == 0 || bytes[..width] == *b"." {
        return None;
    }
    if matches!(bytes.get(width), Some(b'e' | b'E')) {
        let mut exponent = width + 1;
        exponent += usize::from(matches!(bytes.get(exponent), Some(b'+' | b'-')));
        let digits = bytes[exponent..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if digits != 0 {
            width = exponent + digits;
        }
    }
    (!bytes.get(width).is_some_and(|&byte| word_byte(byte))).then_some(width)
}

#[derive(Debug, PartialEq, Eq)]
pub struct CsvData {
    pub headers: Option<Vec<String>>,
    pub rows: Vec<Vec<String>>,
}

pub fn parse_csv<R: Read>(
    input: R,
    delimiter: u8,
    has_headers: bool,
) -> Result<CsvData, csv::Error> {
    validate_csv_delimiter(delimiter)?;
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(has_headers)
        .from_reader(input);
    let headers = if has_headers {
        Some(reader.headers()?.iter().map(str::to_owned).collect())
    } else {
        None
    };
    let rows = reader
        .records()
        .map(|record| record.map(|record| record.iter().map(str::to_owned).collect()))
        .collect::<Result<_, _>>()?;
    Ok(CsvData { headers, rows })
}

pub fn write_csv<W: Write>(
    writer: W,
    delimiter: u8,
    headers: Option<&[String]>,
    rows: &[Vec<String>],
) -> Result<(), csv::Error> {
    validate_csv_delimiter(delimiter)?;
    let mut writer = csv::WriterBuilder::new()
        .delimiter(delimiter)
        .terminator(csv::Terminator::CRLF)
        .from_writer(writer);
    if let Some(headers) = headers {
        writer.write_record(headers)?;
    }
    for row in rows {
        writer.write_record(row)?;
    }
    writer.flush()?;
    Ok(())
}

fn validate_csv_delimiter(delimiter: u8) -> Result<(), csv::Error> {
    if delimiter == b'\t' || ((b' '..=b'~').contains(&delimiter) && delimiter != b'"') {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "CSV 分隔符须为制表符或可打印的 ASCII 字符，且不能用引号。",
    )
    .into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparison {
    Equal,
    NotEqual,
    LessThan,
    LessOrEqual,
    GreaterThan,
    GreaterOrEqual,
    Like,
    IsNull,
    IsNotNull,
}

/// Parenthesizes an expression and checks that it can be embedded in an editor
/// statement. The database still validates SQL syntax and expression semantics.
pub fn wrap_sql_expression(expression: &str, no_backslash_escapes: bool) -> Option<String> {
    let expression = expression.trim();
    if expression.is_empty() || expression.contains('\0') {
        return None;
    }

    let wrapped = format!("({expression})");
    let select = format!("SELECT {wrapped}");
    let script = format!("{select};\nSELECT 1;");
    let valid = |mode| {
        let mut reader = StatementReader::new(&script);
        reader.set_sql_mode(mode);
        reader
            .next_statement()
            .is_some_and(|part| part.sql == select)
            && reader
                .next_statement()
                .is_some_and(|part| part.sql == "SELECT 1")
            && reader.next_statement().is_none()
    };
    (valid("") && (!no_backslash_escapes || valid("NO_BACKSLASH_ESCAPES"))).then_some(wrapped)
}

pub fn mysql_text_literal(value: &str) -> String {
    if !value
        .bytes()
        .any(|byte| byte == b'\\' || byte.is_ascii_control())
    {
        return format!("'{}'", value.replace('\'', "''"));
    }

    // Hex preserves backslashes and control bytes under either SQL escape mode.
    let mut sql = String::with_capacity(value.len() * 2 + 23);
    sql.push_str("CONVERT(X'");
    sql.extend(hexadecimal_bytes(value.as_bytes()).map(char::from));
    sql.push_str("' USING utf8mb4)");
    sql
}

pub(crate) fn mysql_binary_literal(value: &[u8]) -> String {
    let mut sql = String::with_capacity(value.len() * 2 + 3);
    sql.push_str("X'");
    sql.extend(hexadecimal_bytes(value).map(char::from));
    sql.push('\'');
    sql
}

pub(crate) fn hexadecimal_bytes(bytes: &[u8]) -> impl Iterator<Item = u8> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    bytes
        .iter()
        .flat_map(|&byte| [HEX[usize::from(byte >> 4)], HEX[usize::from(byte & 15)]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_dump_syntax_and_tracks_byte_offsets() {
        let script = "\u{feff}-- header;\r\nDELIMITER '$$'\r\nCREATE PROCEDURE `逗号;`() BEGIN\n  SELECT 'a;$$''b', \"c\\\"$$d\"; /* $$ */\nEND$$\nDELIMITER ;\n/*!40101 SET @x = 1 */;\nSELECT `odd\\`, 7--2;\n# trailing ;\n";
        let statements = split_script(script);
        assert_eq!(statements.len(), 3);
        assert!(statements[0].sql.starts_with("CREATE PROCEDURE `逗号;`"));
        assert!(statements[0].sql.ends_with("END"));
        assert_eq!(statements[1].sql, "/*!40101 SET @x = 1 */");
        assert_eq!(statements[2].sql, "SELECT `odd\\`, 7--2");
        for statement in statements {
            assert_eq!(statement.sql, &script[statement.range.clone()]);
            assert_eq!(statement_at(script, statement.range.start), Some(statement));
        }
    }

    #[test]
    fn editor_caret_skips_delimiter_commands() {
        let script = "DELIMITER $$\r\nSELECT '甲'$$\r\nDELIMITER ;\r\n";
        let delimiter = script.find("'$$").unwrap() + 1;
        let command = script.find("DELIMITER ;").unwrap();
        assert_eq!(
            statement_at_for_execution(script, delimiter + 1).map(|part| part.sql),
            Some("SELECT '甲'")
        );
        assert_eq!(statement_at_for_execution(script, command + 4), None);
        assert_eq!(
            statement_at(script, command + 4).map(|part| part.sql),
            Some("SELECT '甲'")
        );

        let with_next = format!("{script}SELECT '乙';");
        assert_eq!(
            statement_at_for_execution(&with_next, command + 4).map(|part| part.sql),
            Some("SELECT '乙'")
        );
    }

    #[test]
    fn formatting_preserves_routine_and_changes_plain_query() {
        let script = "DELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 'a;'; END$$\nDELIMITER ;\nselect a,b from t where a>=1;";
        let formatted = format_sql(script);
        assert!(formatted.starts_with(
            "DELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 'a;'; END$$\nDELIMITER ;\n"
        ));
        assert_ne!(script, formatted);
        assert_eq!(split_script(&formatted).len(), 2);
    }

    #[test]
    fn csv_round_trips_quotes_newlines_and_unicode() {
        let headers = vec!["id".to_owned(), "说明".to_owned()];
        let rows = vec![vec!["1".to_owned(), "a,\"b\"\nc".to_owned()]];
        let mut bytes = Vec::new();
        write_csv(&mut bytes, b',', Some(&headers), &rows).unwrap();
        let parsed = parse_csv(bytes.as_slice(), b',', true).unwrap();
        assert_eq!(
            parsed,
            CsvData {
                headers: Some(headers),
                rows
            }
        );
    }
}
