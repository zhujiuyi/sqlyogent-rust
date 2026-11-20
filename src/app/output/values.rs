use std::borrow::Cow;
use std::cmp::Ordering;
use std::fmt::Write;
use std::ops::Range;

use crate::database::{Cell, ResultSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Operator {
    Equal,
    NotEqual,
    Less,
    Greater,
    Starts,
    Ends,
    Contains,
}

impl Operator {
    pub(super) const ALL: [Self; 7] = [
        Self::Equal,
        Self::NotEqual,
        Self::Less,
        Self::Greater,
        Self::Starts,
        Self::Ends,
        Self::Contains,
    ];

    pub(super) fn label(self, value: &Cell) -> &'static str {
        match (self, value) {
            (Self::Equal, Cell::Null) => "为 NULL",
            (Self::NotEqual, Cell::Null) => "非 NULL",
            (Self::Equal, _) => "等于",
            (Self::NotEqual, _) => "不等于",
            (Self::Less, _) => "小于",
            (Self::Greater, _) => "大于",
            (Self::Starts, _) => "开头为",
            (Self::Ends, _) => "结尾为",
            (Self::Contains, _) => "包含",
        }
    }

    pub(super) fn supports(self, value: &Cell) -> bool {
        !matches!(value, Cell::Null) || matches!(self, Self::Equal | Self::NotEqual)
    }

    pub(super) fn matches(self, candidate: &Cell, value: &Cell) -> bool {
        if matches!(value, Cell::Null) {
            return match self {
                Self::Equal => matches!(candidate, Cell::Null),
                Self::NotEqual => !matches!(candidate, Cell::Null),
                _ => false,
            };
        }
        if matches!(candidate, Cell::Null) {
            return false;
        }
        match self {
            Self::Equal => candidate == value,
            Self::NotEqual => candidate != value,
            Self::Less | Self::Greater => {
                let ordering = match (candidate, value) {
                    (Cell::Text(left), Cell::Text(right)) => Number::parse(left)
                        .zip(Number::parse(right))
                        .map_or_else(|| left.cmp(right), |(left, right)| left.cmp(right)),
                    (Cell::Binary(left), Cell::Binary(right)) => left.cmp(right),
                    _ => return false,
                };
                (self == Self::Less && ordering.is_lt())
                    || (self == Self::Greater && ordering.is_gt())
            }
            Self::Starts => match (candidate, value) {
                (Cell::Text(left), Cell::Text(right)) => left.starts_with(right.as_str()),
                (Cell::Binary(left), Cell::Binary(right)) => left.starts_with(right),
                _ => false,
            },
            Self::Ends => match (candidate, value) {
                (Cell::Text(left), Cell::Text(right)) => left.ends_with(right.as_str()),
                (Cell::Binary(left), Cell::Binary(right)) => left.ends_with(right),
                _ => false,
            },
            Self::Contains => match (candidate, value) {
                (Cell::Text(left), Cell::Text(right)) => left.contains(right.as_str()),
                (Cell::Binary(left), Cell::Binary(right)) => {
                    right.is_empty() || left.windows(right.len()).any(|slice| slice == right)
                }
                _ => false,
            },
        }
    }
}

/// Decimal values delivered by the text protocol can exceed the precision of a float.
#[derive(Clone, Copy)]
struct Number<'a> {
    significant: &'a str,
    order: i64,
    negative: bool,
}

impl<'a> Number<'a> {
    fn parse(text: &'a str) -> Option<Self> {
        let (negative, unsigned) = if let Some(value) = text.strip_prefix('-') {
            (true, value)
        } else {
            (false, text.strip_prefix('+').unwrap_or(text))
        };
        let (mantissa, exponent) =
            if let Some((mantissa, exponent)) = unsigned.split_once(['e', 'E']) {
                (mantissa, i64::from(exponent.parse::<i32>().ok()?))
            } else {
                (unsigned, 0)
            };
        let mut point = None;
        let mut first_digit = None;
        let mut digits = 0;
        let mut leading_zeros = 0;
        for (index, byte) in mantissa.bytes().enumerate() {
            match byte {
                b'.' if point.is_none() => point = Some(digits),
                b'0' if first_digit.is_none() => {
                    leading_zeros += 1;
                    digits += 1;
                }
                b'0'..=b'9' => {
                    first_digit.get_or_insert(index);
                    digits += 1;
                }
                _ => return None,
            }
        }
        if digits == 0 {
            return None;
        }
        let Some(first) = first_digit else {
            return Some(Self {
                significant: "",
                order: 0,
                negative: false,
            });
        };
        let significant = mantissa[first..].trim_end_matches(['0', '.']);
        Some(Self {
            significant,
            order: point.unwrap_or(digits) - leading_zeros + exponent,
            negative,
        })
    }

    fn cmp(self, other: Self) -> Ordering {
        match (self.negative, other.negative) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => {
                let magnitude = match (self.significant.is_empty(), other.significant.is_empty()) {
                    (true, true) => Ordering::Equal,
                    (true, false) => Ordering::Less,
                    (false, true) => Ordering::Greater,
                    (false, false) => self.order.cmp(&other.order).then_with(|| {
                        self.significant
                            .bytes()
                            .filter(|&byte| byte != b'.')
                            .cmp(other.significant.bytes().filter(|&byte| byte != b'.'))
                    }),
                };
                if self.negative {
                    magnitude.reverse()
                } else {
                    magnitude
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Sort {
    pub(super) column: usize,
    pub(super) descending: bool,
}

enum SortValue<'a> {
    Null,
    Number(Number<'a>),
    Text(Cow<'a, str>),
    Binary(&'a [u8]),
}

impl SortValue<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Null, Self::Null) => Ordering::Equal,
            (Self::Null, _) => Ordering::Less,
            (_, Self::Null) => Ordering::Greater,
            (Self::Number(left), Self::Number(right)) => left.cmp(*right),
            (Self::Text(left), Self::Text(right)) => left.cmp(right),
            (Self::Binary(left), Self::Binary(right)) => left.cmp(right),
            (Self::Binary(_), _) => Ordering::Greater,
            (_, Self::Binary(_)) => Ordering::Less,
            _ => Ordering::Equal,
        }
    }
}

fn text_is_declared(set: &ResultSet, column: usize) -> bool {
    let data_type = set.column_hints.get(column).and_then(|hint| {
        hint.lines()
            .find_map(|line| line.strip_prefix("数据类型："))
    });
    data_type.is_some_and(|name| {
        ["char", "varchar", "string", "text", "enum", "set", "json"]
            .iter()
            .any(|type_name| name.to_ascii_lowercase().contains(type_name))
    })
}

pub(super) fn sort_rows(set: &ResultSet, rows: &mut [usize], sorts: &[Sort]) {
    let keys = sorts
        .iter()
        .map(|sort| {
            let cells = || set.rows.iter().map(|row| row.get(sort.column));
            let numeric = !text_is_declared(set, sort.column)
                && cells().all(|cell| match cell {
                    None | Some(Cell::Null) => true,
                    Some(Cell::Text(text)) => Number::parse(text).is_some(),
                    Some(Cell::Binary(_)) => false,
                });
            let keys = cells()
                .map(|cell| match cell {
                    None | Some(Cell::Null) => SortValue::Null,
                    Some(Cell::Binary(bytes)) => SortValue::Binary(bytes),
                    Some(Cell::Text(text)) if numeric => {
                        SortValue::Number(Number::parse(text).expect("column checked"))
                    }
                    Some(Cell::Text(text)) if !text.chars().any(char::is_uppercase) => {
                        SortValue::Text(Cow::Borrowed(text))
                    }
                    Some(Cell::Text(text)) => SortValue::Text(Cow::Owned(text.to_lowercase())),
                })
                .collect::<Vec<_>>();
            (sort.descending, keys)
        })
        .collect::<Vec<_>>();

    rows.sort_unstable_by(|&left, &right| {
        keys.iter()
            .find_map(|(descending, key)| {
                let result = key[left].cmp(&key[right]);
                (result != Ordering::Equal).then(|| {
                    if *descending {
                        result.reverse()
                    } else {
                        result
                    }
                })
            })
            .unwrap_or_else(|| left.cmp(&right))
    });
}

#[derive(Clone, Copy)]
pub(in crate::app) enum ClipboardFormat {
    Tsv,
    Csv,
}

impl ClipboardFormat {
    pub(in crate::app) fn label(self) -> &'static str {
        match self {
            Self::Tsv => "TSV",
            Self::Csv => "CSV",
        }
    }
}

pub(in crate::app) fn cell_text(cell: &Cell) -> String {
    match cell {
        Cell::Text(text) if !text.contains('\0') => text.clone(),
        cell => encode_cell(cell, ClipboardFormat::Tsv),
    }
}

fn encode_cell(cell: &Cell, format: ClipboardFormat) -> String {
    match cell {
        Cell::Null => "\\N".into(),
        Cell::Binary(bytes) => {
            let mut output = String::with_capacity(2 + bytes.len().saturating_mul(2));
            output.push_str("\\x");
            for byte in bytes {
                let _ = write!(&mut output, "{byte:02x}");
            }
            output
        }
        Cell::Text(text) => encode_text(text, format),
    }
}

fn encode_text(text: &str, format: ClipboardFormat) -> String {
    let tsv = matches!(format, ClipboardFormat::Tsv);
    let mut output = String::with_capacity(text.len());
    for char in text.chars() {
        match char {
            '\\' => output.push_str("\\\\"),
            '\0' => output.push_str("\\0"),
            '\t' if tsv => output.push_str("\\t"),
            '\r' if tsv => output.push_str("\\r"),
            '\n' if tsv => output.push_str("\\n"),
            char => output.push(char),
        }
    }
    output
}

pub(super) fn clipboard_rows(
    set: &ResultSet,
    rows: &[usize],
    columns: Range<usize>,
    headers: bool,
    format: ClipboardFormat,
) -> String {
    clipboard_table(
        rows.iter().copied(),
        columns,
        |column| set.columns[column].as_str(),
        |row, column| set.rows.get(row).and_then(|row| row.get(column)),
        headers,
        format,
    )
}

pub(in crate::app) fn clipboard_table<'a>(
    rows: impl IntoIterator<Item = usize>,
    columns: Range<usize>,
    name: impl Fn(usize) -> &'a str,
    cell: impl Fn(usize, usize) -> Option<&'a Cell>,
    headers: bool,
    format: ClipboardFormat,
) -> String {
    let values = |row: usize| {
        let cell = &cell;
        columns
            .clone()
            .map(move |column| encode_cell(cell(row, column).unwrap_or(&Cell::Null), format))
    };
    let names = || {
        columns
            .clone()
            .map(|column| encode_text(name(column), format))
    };
    match format {
        ClipboardFormat::Tsv => {
            let mut output = String::new();
            let mut newline = false;
            let mut write = |fields: Vec<String>| {
                if std::mem::replace(&mut newline, true) {
                    output.push('\n');
                }
                for (index, field) in fields.iter().enumerate() {
                    if index > 0 {
                        output.push('\t');
                    }
                    output.push_str(field);
                }
            };
            if headers {
                write(names().collect());
            }
            for row in rows {
                write(values(row).collect());
            }
            output
        }
        ClipboardFormat::Csv => {
            let mut writer = csv::WriterBuilder::new()
                .terminator(csv::Terminator::CRLF)
                .from_writer(Vec::new());
            if headers {
                writer.write_record(names()).expect("in-memory CSV");
            }
            for row in rows {
                writer.write_record(values(row)).expect("in-memory CSV");
            }
            let bytes = writer.into_inner().expect("in-memory CSV");
            String::from_utf8(bytes).expect("clipboard fields are UTF-8")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(columns: &[&str], rows: Vec<Vec<Cell>>) -> ResultSet {
        ResultSet {
            source_sql: None,
            columns: columns.iter().copied().map(str::to_owned).collect(),
            column_hints: Vec::new(),
            column_origins: Vec::new(),
            excel_types: Vec::new(),
            export_columns: Vec::new(),
            duplicate_source: None,
            notice: None,
            rows,
            affected_rows: 0,
            last_insert_id: None,
            truncated: false,
        }
    }

    #[test]
    fn preserves_cell_types_and_boundaries_in_clipboard() {
        let result = set(
            &["列\t名", "值"],
            vec![
                vec![Cell::Null, Cell::Text("\\N".into())],
                vec![
                    Cell::Binary(vec![0, 255]),
                    Cell::Text("中,\t\"a\"\r\n\0".into()),
                ],
            ],
        );
        assert_eq!(
            clipboard_rows(&result, &[1, 0], 0..2, true, ClipboardFormat::Tsv),
            "列\\t名\t值\n\\x00ff\t中,\\t\"a\"\\r\\n\\0\n\\N\t\\\\N",
        );
        let csv = clipboard_rows(&result, &[1], 0..2, false, ClipboardFormat::Csv);
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(false)
            .from_reader(csv.as_bytes());
        let row = reader.records().next().unwrap().unwrap();
        assert_eq!(&row[0], "\\x00ff");
        assert_eq!(&row[1], "中,\t\"a\"\r\n\\0");
    }

    #[test]
    fn sorts_exact_numbers_and_preserves_source_order_at_ties() {
        let data = [
            "9007199254740993",
            "-0.25",
            "9.007199254740992e15",
            "9007199254740992.00",
            "0.004",
            "0",
            "-12",
        ];
        let result = set(
            &["n"],
            data.iter()
                .map(|text| vec![Cell::Text((*text).into())])
                .collect(),
        );
        let mut rows: Vec<_> = (0..data.len()).collect();
        sort_rows(
            &result,
            &mut rows,
            &[Sort {
                column: 0,
                descending: false,
            }],
        );
        assert_eq!(rows, [6, 1, 5, 4, 2, 3, 0]);
    }

    #[test]
    fn compares_null_and_binary_without_converting_to_display_strings() {
        assert!(Operator::Equal.matches(&Cell::Null, &Cell::Null));
        assert!(Operator::NotEqual.matches(&Cell::Text("NULL".into()), &Cell::Null));
        assert!(!Operator::NotEqual.matches(&Cell::Null, &Cell::Text("x".into())));
        assert!(Operator::Starts.matches(&Cell::Binary(vec![0, 1]), &Cell::Binary(vec![0])));
        assert!(!Operator::Contains.matches(&Cell::Text("\\x00".into()), &Cell::Binary(vec![0])));
        assert!(Operator::Greater.matches(&Cell::Text("10".into()), &Cell::Text("2".into())));
    }
}
