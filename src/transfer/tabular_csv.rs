use std::borrow::{Borrow, Cow};
use std::io::{self, BufReader, BufWriter, Seek, Write};
use std::ops::Range;
use std::path::Path;

use super::super::{RESULT_MEMORY_LIMIT, ResultSet};
use crate::sql_tools::delimited::{marker, unescape};

use super::{Cell, display_value, output_error};

#[derive(Clone, Debug)]
pub struct CsvOptions {
    pub fixed_width: bool,
    pub declared_widths: bool,
    pub field_end: String,
    pub row_end: String,
    pub enclosure: String,
    pub escape: String,
    pub enclose_all: bool,
    pub headers: bool,
    pub null: String,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self {
            fixed_width: false,
            declared_widths: false,
            field_end: ",".into(),
            row_end: r"\r\n".into(),
            enclosure: "\"".into(),
            escape: String::new(),
            enclose_all: false,
            headers: true,
            null: "NULL".into(),
        }
    }
}

impl CsvOptions {
    pub fn validate(&self) -> Result<(), String> {
        Dialect::new(self).map(|_| ())
    }

    pub fn validate_clipboard(&self) -> Result<(), String> {
        Dialect::with_clipboard(self, true).map(|_| ())
    }
}

struct Dialect<'a> {
    options: &'a CsvOptions,
    field_end: String,
    row_end: String,
    field_start: Option<char>,
    row_start: Option<char>,
    enclosure: Option<char>,
    escape: Option<char>,
    clipboard: bool,
}

impl<'a> Dialect<'a> {
    fn new(options: &'a CsvOptions) -> Result<Self, String> {
        Self::with_clipboard(options, false)
    }

    fn with_clipboard(options: &'a CsvOptions, clipboard: bool) -> Result<Self, String> {
        let row_end = unescape(&options.row_end)?;
        let row_start = row_end.chars().next();
        if !clipboard && row_start.is_none() {
            return Err("请输入行分隔符。".into());
        }
        let escape = marker(&options.escape, "转义符")?;
        if options.fixed_width {
            if row_start == Some(' ') || row_end.contains('\0') {
                return Err("定宽格式的行分隔符不能以空格开头或含 NUL。".into());
            }
            if escape == Some(' ') || escape.is_some() && escape == row_start {
                return Err("定宽格式的转义符不能是空格或行分隔符的首字符。".into());
            }
            return Ok(Self {
                options,
                field_end: String::new(),
                row_end,
                field_start: None,
                row_start,
                enclosure: None,
                escape,
                clipboard,
            });
        }
        let field_end = unescape(&options.field_end)?;
        let field_start = field_end.chars().next();
        if !clipboard && field_start.is_none() {
            return Err("请输入字段分隔符。".into());
        }
        let enclosure = marker(&options.enclosure, "封闭符")?;
        if field_end.contains(['\r', '\n', '\0']) || row_end.contains('\0') {
            return Err("字段分隔符不可含换行或 NUL，行分隔符不可含 NUL。".into());
        }
        if !field_end.is_empty()
            && !row_end.is_empty()
            && (field_end.starts_with(&row_end) || row_end.starts_with(&field_end))
        {
            return Err("字段分隔符和行分隔符不能相同，也不能互为前缀。".into());
        }
        if enclosure.is_some() && enclosure == escape
            || [enclosure, escape]
                .into_iter()
                .flatten()
                .any(|marker| Some(marker) == field_start || Some(marker) == row_start)
        {
            return Err("封闭符、转义符和分隔符的起始字符需彼此区分。".into());
        }
        Ok(Self {
            options,
            field_end,
            row_end,
            field_start,
            row_start,
            enclosure,
            escape,
            clipboard,
        })
    }

    fn separator_start(&self, char: char) -> bool {
        matches!(char, '\r' | '\n') && (!self.clipboard || self.row_start.is_some())
            || self.field_start == Some(char)
            || self.row_start == Some(char)
            || self.options.fixed_width && char == '\0'
    }

    fn record<V: AsRef<str>>(
        &self,
        out: &mut impl Write,
        fields: impl ExactSizeIterator<Item = (V, bool)>,
    ) -> Result<(), String> {
        let single = fields.len() == 1;
        for (index, (value, is_null)) in fields.enumerate() {
            if index > 0 {
                out.write_all(self.field_end.as_bytes())
                    .map_err(output_error)?;
            }
            self.field(out, value.as_ref(), is_null, single)?;
        }
        out.write_all(self.row_end.as_bytes()).map_err(output_error)
    }

    fn field(
        &self,
        out: &mut impl Write,
        value: &str,
        is_null: bool,
        single: bool,
    ) -> Result<(), String> {
        // The customary LOAD DATA marker is sent literally when an escape character is enabled.
        let mut chars = value.chars();
        if is_null && self.escape.is_some() && chars.next() == self.escape && chars.as_str() == "N"
        {
            return out.write_all(value.as_bytes()).map_err(output_error);
        }
        let needs_enclosure = value
            .chars()
            .any(|char| self.separator_start(char) || self.enclosure == Some(char))
            || single && value.is_empty();
        if let Some(enclosure) = self.enclosure
            && (needs_enclosure || self.options.enclose_all && !is_null)
        {
            write!(out, "{enclosure}").map_err(output_error)?;
            self.write_text(out, value, Some(enclosure))?;
            return write!(out, "{enclosure}").map_err(output_error);
        }
        self.write_text(out, value, None)
    }

    fn write_text(
        &self,
        out: &mut impl Write,
        value: &str,
        enclosed: Option<char>,
    ) -> Result<(), String> {
        let mut start = 0;
        for (index, char) in value.char_indices() {
            let encoding = if self.clipboard && char == '\0' {
                Some((
                    self.escape.ok_or("复制的数据中含 NUL，请设置转义符。")?,
                    '0',
                ))
            } else if enclosed == Some(char) {
                Some((self.escape.unwrap_or(char), char))
            } else if self.escape == Some(char) {
                Some((char, char))
            } else if enclosed.is_none() && self.separator_start(char) {
                let escape = self.escape.ok_or(if self.options.fixed_width {
                    "定宽字段含换行、NUL 或行分隔字符，请设置转义符。"
                } else {
                    "值中含分隔符或换行，请设置封闭符或转义符。"
                })?;
                let code = match char {
                    '\0' => '0',
                    '\n' => 'n',
                    '\r' => 'r',
                    '\t' => 't',
                    // These escape codes have a different meaning in LOAD DATA.
                    '0' | 'b' | 'n' | 'r' | 't' | 'Z' | 'N' => {
                        return Err("此分隔符会与转义代码冲突，请设置封闭符。".into());
                    }
                    _ => char,
                };
                Some((escape, code))
            } else {
                None
            };
            if let Some((escape, code)) = encoding {
                out.write_all(&value.as_bytes()[start..index])
                    .map_err(output_error)?;
                write!(out, "{escape}{code}").map_err(output_error)?;
                start = index + char.len_utf8();
            }
        }
        out.write_all(&value.as_bytes()[start..])
            .map_err(output_error)
    }
}

fn export_fixed<C: Borrow<Cell>>(
    out: &mut impl Write,
    names: &[String],
    rows: impl Iterator<Item = Result<Vec<C>, String>>,
    dialect: &Dialect<'_>,
    directory: &Path,
) -> Result<u64, String> {
    // Use one pass over the source and stage the escaped rows on disk before padding.
    let spool = tempfile::tempfile_in(directory)
        .map_err(|error| format!("无法创建定宽导出的临时文件：{error}"))?;
    let mut spool = BufWriter::new(spool);
    let mut widths = vec![1; names.len()];
    let mut count = 0;
    for values in dialect
        .options
        .headers
        .then(|| Ok(names.iter().map(|name| name.as_str().into()).collect()))
        .into_iter()
        .chain(rows.map(|row| {
            row.map(|row| {
                row.iter()
                    .map(|cell| fixed_text(cell.borrow()).into_owned())
                    .collect::<Vec<String>>()
            })
        }))
    {
        let mut values = values?;
        for (value, width) in values.iter_mut().zip(&mut widths) {
            let mut encoded = Vec::new();
            dialect.write_text(&mut encoded, value, None)?;
            *width = (*width).max(encoded.len());
            *value = String::from_utf8(encoded).map_err(|error| error.to_string())?;
        }
        serde_json::to_writer(&mut spool, &values).map_err(|error| error.to_string())?;
        writeln!(spool).map_err(output_error)?;
        count += 1;
    }
    let mut spool = spool
        .into_inner()
        .map_err(|error| output_error(error.into_error()))?;
    spool.rewind().map_err(output_error)?;
    let records =
        serde_json::Deserializer::from_reader(BufReader::new(spool)).into_iter::<Vec<String>>();
    for values in records {
        let values = values.map_err(|error| error.to_string())?;
        for (value, &width) in values.iter().zip(&widths) {
            out.write_all(value.as_bytes()).map_err(output_error)?;
            pad_spaces(out, width - value.len())?;
        }
        out.write_all(dialect.row_end.as_bytes())
            .map_err(output_error)?;
    }
    Ok(count - u64::from(dialect.options.headers))
}

fn pad_spaces(out: &mut impl Write, mut remaining: usize) -> Result<(), String> {
    let padding = [b' '; 1024];
    while remaining > 0 {
        let chunk = remaining.min(padding.len());
        out.write_all(&padding[..chunk]).map_err(output_error)?;
        remaining -= chunk;
    }
    Ok(())
}

fn fixed_text(cell: &Cell) -> Cow<'_, str> {
    match cell {
        Cell::Null => Cow::Borrowed(""),
        value => display_value(value),
    }
}

fn export_declared_widths<C: Borrow<Cell>>(
    out: &mut impl Write,
    names: &[String],
    rows: impl Iterator<Item = Result<Vec<C>, String>>,
    dialect: &Dialect<'_>,
    widths: &[Option<usize>],
) -> Result<u64, String> {
    let widths = names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            widths.get(index).copied().flatten().ok_or_else(|| {
                format!("字段「{name}」没有可用的声明宽度，请改用按内容定宽或分隔格式。")
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if dialect.options.headers {
        for (name, &width) in names.iter().zip(&widths) {
            if name.len() > width {
                return Err(format!(
                    "字段名「{name}」超过声明宽度 {width} 字节，请取消导出字段名或改用按内容定宽。"
                ));
            }
            write_declared_field(out, dialect, name, width)?;
        }
        out.write_all(dialect.row_end.as_bytes())
            .map_err(output_error)?;
    }
    let mut count = 0;
    for row in rows {
        let row = row?;
        for (index, (cell, &width)) in row.iter().zip(&widths).enumerate() {
            let text = match cell.borrow() {
                Cell::Null => "",
                Cell::Text(value) if value.len() <= width => value,
                Cell::Text(_) => {
                    return Err(format!(
                        "第 {} 行的字段「{}」超过声明宽度 {width} 字节，请改用按内容定宽或分隔格式。",
                        count + 1,
                        names[index]
                    ));
                }
                Cell::Binary(_) => {
                    return Err(format!(
                        "第 {} 行的字段「{}」含二进制数据，请改用按内容定宽或分隔格式。",
                        count + 1,
                        names[index]
                    ));
                }
            };
            write_declared_field(out, dialect, text, width)?;
        }
        out.write_all(dialect.row_end.as_bytes())
            .map_err(output_error)?;
        count += 1;
    }
    Ok(count)
}

fn write_declared_field(
    out: &mut impl Write,
    dialect: &Dialect<'_>,
    value: &str,
    width: usize,
) -> Result<(), String> {
    // Legacy offsets count the bytes before escaping. Escape sequences expand the physical record.
    dialect.write_text(out, value, None)?;
    pad_spaces(out, width - value.len())
}

pub(super) fn export_csv<C: Borrow<Cell>>(
    out: &mut impl Write,
    names: &[String],
    rows: impl Iterator<Item = Result<Vec<C>, String>>,
    options: &CsvOptions,
    widths: &[Option<usize>],
    directory: &Path,
) -> Result<u64, String> {
    let dialect = Dialect::new(options)?;
    if options.fixed_width {
        if options.declared_widths {
            return export_declared_widths(out, names, rows, &dialect, widths);
        }
        return export_fixed(out, names, rows, &dialect, directory);
    }
    export_delimited(out, names, rows, &dialect)
}

fn export_delimited<C: Borrow<Cell>>(
    out: &mut impl Write,
    names: &[String],
    rows: impl Iterator<Item = Result<Vec<C>, String>>,
    dialect: &Dialect<'_>,
) -> Result<u64, String> {
    if dialect.options.headers {
        dialect.record(out, names.iter().map(|name| (name.as_str(), false)))?;
    }
    let mut count = 0;
    for row in rows {
        let row = row?;
        dialect.record(
            out,
            row.iter().map(|cell| match cell.borrow() {
                Cell::Null => (dialect.options.null.as_str().into(), true),
                value => (display_value(value), false),
            }),
        )?;
        count += 1;
    }
    Ok(count)
}

/// Format a selection of loaded result rows without writing a temporary file.
pub fn format_result_clipboard(
    result: &ResultSet,
    row_indices: &[usize],
    columns: Range<usize>,
    options: &CsvOptions,
) -> Result<String, String> {
    let dialect = Dialect::with_clipboard(options, true)?;
    let names = result
        .columns
        .get(columns.clone())
        .filter(|names| !names.is_empty())
        .ok_or("所选字段已不存在。")?;
    let rows = || {
        row_indices.iter().map(|&index| {
            let cells = result
                .rows
                .get(index)
                .and_then(|row| row.get(columns.clone()))
                .ok_or("所选记录或字段已不存在。")?;
            for cell in cells {
                let len = match cell {
                    Cell::Text(value) => value.len(),
                    Cell::Binary(value) => value.len().saturating_mul(2).saturating_add(2),
                    Cell::Null => options.null.len(),
                };
                if len > RESULT_MEMORY_LIMIT {
                    return Err(clipboard_limit());
                }
            }
            Ok(cells)
        })
    };
    let mut output = ClipboardBuffer::default();
    let converted = if options.fixed_width && options.declared_widths {
        let widths = columns
            .clone()
            .map(|index| {
                result
                    .export_columns
                    .get(index)
                    .and_then(|item| item.declared_width)
            })
            .collect::<Vec<_>>();
        export_declared_widths(
            &mut output,
            names,
            rows().map(|row| row.map(|cells| cells.iter().collect::<Vec<_>>())),
            &dialect,
            &widths,
        )
        .map(|_| ())
    } else if options.fixed_width {
        let records = || {
            options
                .headers
                .then(|| {
                    Ok(names
                        .iter()
                        .map(|name| Cow::Borrowed(name.as_str()))
                        .collect::<Vec<_>>())
                })
                .into_iter()
                .chain(rows().map(|row| row.map(|cells| cells.iter().map(fixed_text).collect())))
        };
        (|| {
            let mut widths = vec![1; names.len()];
            for record in records() {
                for (value, width) in record?.into_iter().zip(&mut widths) {
                    let mut counter = ByteCount(0);
                    dialect.write_text(&mut counter, value.as_ref(), None)?;
                    *width = (*width).max(counter.0);
                }
            }
            for record in records() {
                for (value, &width) in record?.into_iter().zip(&widths) {
                    let start = output.bytes.len();
                    dialect.write_text(&mut output, value.as_ref(), None)?;
                    let written = output.bytes.len() - start;
                    pad_spaces(&mut output, width - written)?;
                }
                output
                    .write_all(dialect.row_end.as_bytes())
                    .map_err(output_error)?;
            }
            Ok(())
        })()
    } else {
        export_delimited(
            &mut output,
            names,
            rows().map(|row| row.map(|cells| cells.iter().collect::<Vec<_>>())),
            &dialect,
        )
        .map(|_| ())
    };
    if output.too_large {
        return Err(clipboard_limit());
    }
    converted?;
    if output.bytes.ends_with(dialect.row_end.as_bytes()) {
        output
            .bytes
            .truncate(output.bytes.len() - dialect.row_end.len());
    }
    String::from_utf8(output.bytes).map_err(|error| error.to_string())
}

fn clipboard_limit() -> String {
    format!(
        "复制内容超过 {} MiB，请缩小选区。",
        RESULT_MEMORY_LIMIT / (1024 * 1024)
    )
}

#[derive(Default)]
struct ClipboardBuffer {
    bytes: Vec<u8>,
    too_large: bool,
}

impl Write for ClipboardBuffer {
    fn write(&mut self, source: &[u8]) -> io::Result<usize> {
        if source.len() > RESULT_MEMORY_LIMIT.saturating_sub(self.bytes.len()) {
            self.too_large = true;
            return Err(io::Error::other(clipboard_limit()));
        }
        self.bytes.extend_from_slice(source);
        Ok(source.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct ByteCount(usize);

impl Write for ByteCount {
    fn write(&mut self, source: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(source.len());
        Ok(source.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod clipboard_tests {
    use super::*;

    #[test]
    fn formats_visible_rows_with_custom_delimiters_and_content_width() {
        let result = ResultSet {
            source_sql: None,
            columns: vec!["列".into(), "值".into()],
            column_hints: Vec::new(),
            column_origins: Vec::new(),
            excel_types: Vec::new(),
            export_columns: Vec::new(),
            duplicate_source: None,
            notice: None,
            rows: vec![
                vec![Cell::Text("乙".into()), Cell::Text("a;b".into())],
                vec![Cell::Text("x".into()), Cell::Null],
                vec![Cell::Text("未选".into()), Cell::Text("unused".into())],
            ],
            affected_rows: 0,
            last_insert_id: None,
            truncated: false,
        };
        let mut options = CsvOptions {
            field_end: ";".into(),
            row_end: r"\r\n".into(),
            enclosure: "'".into(),
            null: "<空>".into(),
            ..CsvOptions::default()
        };
        let selected = [1, 0];
        assert_eq!(
            format_result_clipboard(&result, &selected, 0..2, &options).unwrap(),
            "列;值\r\nx;<空>\r\n乙;'a;b'"
        );
        options.fixed_width = true;
        assert_eq!(
            format_result_clipboard(&result, &selected, 0..2, &options).unwrap(),
            "列值\r\nx     \r\n乙a;b"
        );
    }
}
