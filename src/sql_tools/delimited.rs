use std::io::Read;

use crate::database::{IMPORT_MEMORY_LIMIT, MAX_IMPORT_COLUMNS, MAX_IMPORT_ROWS};

pub const FILE_SIZE_LIMIT: usize = IMPORT_MEMORY_LIMIT;

#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub enum SkipMode {
    #[default]
    Lines,
    Records,
}

#[derive(Clone, Eq, PartialEq)]
pub struct Options {
    pub field_end: String,
    pub row_end: String,
    pub enclosure: String,
    pub escape: String,
    pub headers: bool,
    pub skip: usize,
    pub skip_mode: SkipMode,
    pub null: String,
    pub quoted_null: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            field_end: ",".into(),
            row_end: String::new(),
            enclosure: "\"".into(),
            escape: String::new(),
            headers: true,
            skip: 0,
            skip_mode: SkipMode::Lines,
            null: String::new(),
            quoted_null: true,
        }
    }
}

pub struct Data {
    pub headers: Option<Vec<String>>,
    pub rows: Vec<Vec<Option<String>>>,
}

pub fn unescape(value: &str) -> Result<String, String> {
    let mut text = String::new();
    let mut chars = value.chars();
    while let Some(char) = chars.next() {
        text.push(if char == '\\' {
            match chars.next() {
                Some('t') => '\t',
                Some('r') => '\r',
                Some('n') => '\n',
                Some('\\') => '\\',
                _ => return Err(r"分隔符支持 \t、\r、\n 和 \\。".into()),
            }
        } else {
            char
        });
    }
    Ok(text)
}

pub fn marker(value: &str, label: &str) -> Result<Option<char>, String> {
    let value = if value == r"\\" { "\\" } else { value };
    let mut chars = value.chars();
    let first = chars.next();
    if first.is_some_and(char::is_control) || chars.next().is_some() {
        return Err(format!("{label}请填写一个字符或留空。"));
    }
    Ok(first)
}

struct Dialect {
    field_end: String,
    row_end: String,
    enclosure: Option<char>,
    escape: Option<char>,
}

enum Boundary<'a> {
    Field(&'a str),
    Record(&'a str),
    Eof,
}

struct Field<'a> {
    value: String,
    raw: &'a str,
    enclosed: bool,
}

impl Dialect {
    fn new(options: &Options) -> Result<Self, String> {
        let field_end = unescape(&options.field_end)?;
        let row_end = unescape(&options.row_end)?;
        let enclosure = marker(&options.enclosure, "封闭符")?;
        let escape = marker(&options.escape, "转义符")?;
        if field_end.is_empty() {
            return Err("请输入字段分隔符。".into());
        }
        if field_end.contains(['\r', '\n', '\0']) || row_end.contains('\0') {
            return Err("字段分隔符不可含换行或 NUL，行分隔符不可含 NUL。".into());
        }
        if !row_end.is_empty()
            && (field_end.starts_with(&row_end) || row_end.starts_with(&field_end))
        {
            return Err("字段分隔符和行分隔符不能相同，也不能互为前缀。".into());
        }
        if enclosure.is_some() && enclosure == escape
            || [enclosure, escape]
                .into_iter()
                .flatten()
                .any(|symbol| field_end.starts_with(symbol) || row_end.starts_with(symbol))
        {
            return Err("封闭符、转义符和分隔符的起始字符需彼此区分。".into());
        }
        Ok(Self {
            field_end,
            row_end,
            enclosure,
            escape,
        })
    }

    fn end_row<'a>(&self, input: &'a str) -> Option<&'a str> {
        if self.row_end.is_empty() {
            input
                .strip_prefix("\r\n")
                .or_else(|| input.strip_prefix(['\r', '\n']))
        } else {
            input.strip_prefix(&self.row_end)
        }
    }

    fn boundary<'a>(&self, input: &'a str) -> Option<Boundary<'a>> {
        if input.is_empty() {
            return Some(Boundary::Eof);
        }
        if let Some(rest) = self.end_row(input) {
            return Some(Boundary::Record(rest));
        }
        input.strip_prefix(&self.field_end).map(Boundary::Field)
    }

    fn skip_line<'a>(&self, input: &'a str) -> &'a str {
        let index = if self.row_end.is_empty() {
            input.find(['\r', '\n'])
        } else {
            input.find(&self.row_end)
        };
        index
            .and_then(|index| self.end_row(&input[index..]))
            .unwrap_or_default()
    }

    fn field<'a>(&self, mut input: &'a str) -> Result<(Field<'a>, Boundary<'a>), &'static str> {
        let enclosure = self.enclosure.filter(|&symbol| input.starts_with(symbol));
        if let Some(symbol) = enclosure {
            input = &input[symbol.len_utf8()..];
        }
        let start = input;
        let mut value = String::new();
        loop {
            let mut chars = input.chars();
            if enclosure.is_none()
                && let Some(boundary) = self.boundary(input)
            {
                let raw = &start[..start.len() - input.len()];
                return Ok((
                    Field {
                        value,
                        raw,
                        enclosed: false,
                    },
                    boundary,
                ));
            }
            let symbol = chars.next().ok_or("缺少结束封闭符。")?;
            let rest = chars.as_str();
            if self.escape == Some(symbol) {
                let escaped = chars.next().ok_or("转义符后缺少字符。")?;
                value.push(
                    if self.escape == Some(escaped) || enclosure == Some(escaped) {
                        escaped
                    } else {
                        match escaped {
                            '0' => '\0',
                            'b' => '\u{0008}',
                            'n' => '\n',
                            'r' => '\r',
                            't' => '\t',
                            'Z' => '\u{001a}',
                            other => other,
                        }
                    },
                );
                input = chars.as_str();
                continue;
            }
            if enclosure == Some(symbol) {
                if let Some(undoubled) = rest.strip_prefix(symbol) {
                    value.push(symbol);
                    input = undoubled;
                    continue;
                }
                if let Some(boundary) = self.boundary(rest) {
                    let raw = &start[..start.len() - input.len()];
                    return Ok((
                        Field {
                            value,
                            raw,
                            enclosed: true,
                        },
                        boundary,
                    ));
                }
            }
            value.push(symbol);
            input = rest;
        }
    }
}

struct Records<'a> {
    dialect: Dialect,
    remaining: &'a str,
    count: usize,
}

impl<'a> Records<'a> {
    fn next(&mut self) -> Result<Option<Vec<Field<'a>>>, String> {
        // CSV readers skip empty lines. A quoted empty field is still a record.
        while let Some(rest) = self.dialect.end_row(self.remaining) {
            self.remaining = rest;
        }
        if self.remaining.is_empty() {
            return Ok(None);
        }
        self.count += 1;
        let mut record = Vec::new();
        loop {
            if record.len() == MAX_IMPORT_COLUMNS {
                return Err(format!(
                    "第 {} 条记录超过 {MAX_IMPORT_COLUMNS} 个字段。",
                    self.count
                ));
            }
            let (field, boundary) = self
                .dialect
                .field(self.remaining)
                .map_err(|error| format!("无法读取第 {} 条记录。{error}", self.count))?;
            record.push(field);
            match boundary {
                Boundary::Field(rest) => self.remaining = rest,
                Boundary::Record(rest) => {
                    self.remaining = rest;
                    break;
                }
                Boundary::Eof => {
                    self.remaining = "";
                    break;
                }
            }
        }
        Ok(Some(record))
    }
}

fn field_value(field: Field<'_>, options: &Options, escape: Option<char>) -> Option<String> {
    let escaped_null = escape.is_some_and(|escape| field.raw.strip_prefix(escape) == Some("N"));
    let null = (!field.enclosed && escaped_null)
        || ((!field.enclosed || options.quoted_null)
            && !options.null.is_empty()
            && (field.value == options.null || field.raw == options.null && escaped_null));
    (!null).then_some(field.value)
}

/// Read ordinary CSV fields without the SQL text import's NULL marker rules.
pub(crate) fn csv_records<'a>(
    text: &'a str,
    delimiter: &str,
) -> Result<impl Iterator<Item = Result<Vec<String>, String>> + use<'a>, String> {
    let mut records = Records {
        dialect: Dialect::new(&Options {
            field_end: delimiter.to_owned(),
            ..Options::default()
        })?,
        remaining: text,
        count: 0,
    };
    Ok(std::iter::from_fn(move || match records.next() {
        Ok(Some(fields)) => Some(Ok(fields.into_iter().map(|field| field.value).collect())),
        Ok(None) => None,
        Err(error) => {
            records.remaining = "";
            Some(Err(error))
        }
    }))
}

pub fn parse(input: impl Read, options: &Options) -> Result<Data, String> {
    let dialect = Dialect::new(options)?;
    let mut bytes = Vec::new();
    input
        .take(FILE_SIZE_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("无法读取导入文件。{error}"))?;
    if bytes.len() > FILE_SIZE_LIMIT {
        return Err("导入文件大小超过 32 MiB。".into());
    }
    let input =
        std::str::from_utf8(&bytes).map_err(|error| format!("导入文件须为 UTF-8。{error}"))?;
    let mut records = Records {
        dialect,
        remaining: input.strip_prefix('\u{feff}').unwrap_or(input),
        count: 0,
    };
    for _ in 0..options.skip {
        if records.remaining.is_empty() {
            break;
        }
        match options.skip_mode {
            SkipMode::Lines => records.remaining = records.dialect.skip_line(records.remaining),
            SkipMode::Records => {
                records.next()?;
            }
        }
    }
    let headers: Option<Vec<_>> = if options.headers {
        let fields = records.next()?.ok_or("文件中没有可读取的列名。")?;
        Some(fields.into_iter().map(|field| field.value).collect())
    } else {
        None
    };
    let mut width = headers.as_ref().map(Vec::len);
    let mut rows = Vec::new();
    let mut size = 0;
    while let Some(fields) = records.next()? {
        if rows.len() == MAX_IMPORT_ROWS {
            return Err(format!("每次最多导入 {MAX_IMPORT_ROWS} 行数据。"));
        }
        if fields.len() != *width.get_or_insert(fields.len()) {
            return Err(format!(
                "第 {} 条记录的字段数与首条有效记录不同。",
                records.count
            ));
        }
        let row = fields
            .into_iter()
            .map(|field| field_value(field, options, records.dialect.escape))
            .collect::<Vec<_>>();
        size += row.len() * size_of::<Option<String>>()
            + row.iter().flatten().map(String::len).sum::<usize>();
        if size > IMPORT_MEMORY_LIMIT {
            return Err("导入数据超过 32 MiB 上限。".into());
        }
        rows.push(row);
    }
    if rows.is_empty() {
        return Err("文件中没有可导入的数据。".into());
    }
    Ok(Data { headers, rows })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_text_lines_then_reads_standard_csv() {
        let input = "\u{feff}数据说明\r\n\r\nID,note,state\r\n1,\"甲,乙\n\"\"丙\"\"\",NULL\n2,路径\\N,\"NULL\"\r";
        let data = parse(
            input.as_bytes(),
            &Options {
                skip: 2,
                null: "NULL".into(),
                ..Options::default()
            },
        )
        .unwrap();
        assert_eq!(data.headers.unwrap(), ["ID", "note", "state"]);
        assert_eq!(
            data.rows,
            [
                vec![Some("1".into()), Some("甲,乙\n\"丙\"".into()), None],
                vec![Some("2".into()), Some("路径\\N".into()), None],
            ]
        );
    }
}
