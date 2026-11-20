use std::io::Cursor;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use calamine::{Data, Reader, SheetType, Sheets, Xls, Xlsb, Xlsx};
use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};

use crate::database::{
    Cell, IMPORT_MEMORY_LIMIT, MAX_IMPORT_COLUMNS, MAX_IMPORT_ROWS, OdbcColumnDefinition,
    check_import_cancelled,
};
use crate::odbc::{SourceColumn, SourceData};

use super::{column_names, imported, read_bytes};

mod named_ranges;
mod xls_limits;
mod xls_named_ranges;

type Workbook = Sheets<Cursor<Vec<u8>>>;
const MAX_ARCHIVE_MEMBERS: usize = 16_384;

pub(super) enum Format {
    Xls,
    Xlsx,
    Xlsb,
}

pub(super) fn format(path: &Path, named_ranges: bool) -> Result<Format, String> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    let format = match extension.as_deref() {
        Some("xls") => Format::Xls,
        Some("xlsx" | "xlsm") => Format::Xlsx,
        Some("xlsb") => Format::Xlsb,
        _ => return Err("Excel 源文件须为 .xls、.xlsx、.xlsm 或 .xlsb。".into()),
    };
    if named_ranges && matches!(format, Format::Xlsb) {
        return Err(
            "命名区域支持 .xlsx、.xlsm 和 Excel 97–2003 的 .xls。读取 .xlsb 时请选择工作表。"
                .into(),
        );
    }
    Ok(format)
}

fn open(
    path: &Path,
    named_ranges: bool,
    cancel: Option<&AtomicBool>,
) -> Result<(Workbook, Vec<xls_named_ranges::NamedRange>), String> {
    let format = format(path, named_ranges)?;
    let contents = read_bytes(path, cancel)?;
    let (contents, xls_names) = match format {
        Format::Xls => xls_limits::prepare(contents, named_ranges, cancel)?,
        Format::Xlsx | Format::Xlsb => {
            check_archive(&contents, cancel)?;
            (contents, Vec::new())
        }
    };
    check_import_cancelled(cancel)?;
    let cursor = Cursor::new(contents);
    let workbook = match format {
        Format::Xls => Xls::new(cursor).map(Sheets::Xls).map_err(workbook_error),
        Format::Xlsx => Xlsx::new(cursor).map(Sheets::Xlsx).map_err(workbook_error),
        Format::Xlsb => Xlsb::new(cursor).map(Sheets::Xlsb).map_err(workbook_error),
    }?;
    check_import_cancelled(cancel)?;
    Ok((workbook, xls_names))
}

fn check_archive(contents: &[u8], cancel: Option<&AtomicBool>) -> Result<(), String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(contents)).map_err(workbook_error)?;
    if archive.len() > MAX_ARCHIVE_MEMBERS {
        return Err("Excel 工作簿包含过多压缩项目。".into());
    }
    let mut remaining = IMPORT_MEMORY_LIMIT as u64;
    for index in 0..archive.len() {
        check_import_cancelled(cancel)?;
        let entry = archive.by_index_raw(index).map_err(workbook_error)?;
        remaining = remaining
            .checked_sub(entry.size())
            .ok_or("Excel 工作簿解压后超过 32 MiB。")?;
    }
    Ok(())
}

fn workbook_error(error: impl std::fmt::Display) -> String {
    format!("无法读取 Excel 工作簿。{error}")
}

pub(super) fn discover(path: &Path, named_ranges: bool) -> Result<Vec<String>, String> {
    let (workbook, xls_names) = open(path, named_ranges, None)?;
    if named_ranges {
        return Ok(named_ranges::list(&workbook, &xls_names)
            .into_iter()
            .map(|range| range.name.into_owned())
            .collect());
    }
    Ok(workbook
        .sheets_metadata()
        .iter()
        .filter(|sheet| sheet.typ == SheetType::WorkSheet)
        .map(|sheet| sheet.name.clone())
        .collect())
}

pub(super) fn read(
    path: &Path,
    table: &str,
    headers: bool,
    columns_only: bool,
    named_ranges: bool,
    cancel: Option<&AtomicBool>,
) -> Result<SourceData, String> {
    let (mut workbook, xls_names) = open(path, named_ranges, cancel)?;
    let selection = if named_ranges {
        let range = named_ranges::list(&workbook, &xls_names)
            .into_iter()
            .find(|range| range.name == table)
            .ok_or_else(|| format!("工作簿中找不到可导入的命名区域「{table}」。"))?;
        Some((range.sheet.to_owned(), range.bounds))
    } else {
        if !workbook
            .sheets_metadata()
            .iter()
            .any(|sheet| sheet.typ == SheetType::WorkSheet && sheet.name == table)
        {
            return Err(format!("工作簿中找不到工作表「{table}」。"));
        }
        None
    };
    let sheet_name = selection.as_ref().map_or(table, |(name, _)| name.as_str());
    let selected_bounds = selection.as_ref().map(|(_, bounds)| *bounds);
    let sheet_error = |error: String| format!("无法读取工作表「{sheet_name}」。{error}");
    let mut sheet = Worksheet::new(sheet_name, headers, selected_bounds)?;
    match &mut workbook {
        Sheets::Xlsx(workbook) => {
            let mut reader = workbook
                .worksheet_cells_reader(sheet_name)
                .map_err(|error| sheet_error(error.to_string()))?;
            while let Some(cell) = reader
                .next_cell()
                .map_err(|error| sheet_error(error.to_string()))?
            {
                check_import_cancelled(cancel)?;
                let position = cell.get_position();
                if selected_bounds.is_none_or(|bounds| bounds.contains(position)) {
                    sheet.push(position, Data::from(cell.get_value().clone()))?;
                }
            }
        }
        Sheets::Xlsb(workbook) => {
            let mut reader = workbook
                .worksheet_cells_reader(sheet_name)
                .map_err(|error| sheet_error(error.to_string()))?;
            while let Some(cell) = reader
                .next_cell()
                .map_err(|error| sheet_error(error.to_string()))?
            {
                check_import_cancelled(cancel)?;
                sheet.push(cell.get_position(), Data::from(cell.get_value().clone()))?;
            }
        }
        Sheets::Xls(workbook) => {
            let range = workbook
                .worksheet_range(sheet_name)
                .map_err(|error| sheet_error(error.to_string()))?;
            if let Some((top, left)) = range.start() {
                for (row, column, cell) in range.used_cells() {
                    check_import_cancelled(cancel)?;
                    let position = (top + row as u32, left + column as u32);
                    if selected_bounds.is_none_or(|bounds| bounds.contains(position)) {
                        sheet.push(position, cell.clone())?;
                    }
                }
            }
        }
        Sheets::Ods(_) => unreachable!("only declared Excel workbook formats are opened"),
    }
    sheet.finish(columns_only, cancel)
}

#[derive(Clone, Copy)]
struct Bounds {
    top: u32,
    left: u32,
    bottom: u32,
    right: u32,
}

impl Bounds {
    fn one((row, column): (u32, u32)) -> Self {
        Self {
            top: row,
            left: column,
            bottom: row,
            right: column,
        }
    }

    fn include(&mut self, (row, column): (u32, u32)) {
        self.top = self.top.min(row);
        self.left = self.left.min(column);
        self.bottom = self.bottom.max(row);
        self.right = self.right.max(column);
    }

    fn contains(self, (row, column): (u32, u32)) -> bool {
        (self.top..=self.bottom).contains(&row) && (self.left..=self.right).contains(&column)
    }

    fn dimensions(self, headers: bool) -> Result<(usize, usize), String> {
        let height = u64::from(self.bottom - self.top) + 1 - u64::from(headers);
        let width = u64::from(self.right - self.left) + 1;
        if height > MAX_IMPORT_ROWS as u64 || width > MAX_IMPORT_COLUMNS as u64 {
            return Err(format!(
                "每张 Excel 工作表最多导入 {MAX_IMPORT_ROWS} 行、{MAX_IMPORT_COLUMNS} 列。",
            ));
        }
        Ok((height as usize, width as usize))
    }
}

struct Worksheet<'a> {
    name: &'a str,
    headers: bool,
    bounds: Option<Bounds>,
    cells: Vec<StoredCell>,
    text_bytes: usize,
}

struct StoredCell {
    position: (u32, u32),
    value: String,
    kind: ValueKind,
}

impl<'a> Worksheet<'a> {
    fn new(name: &'a str, headers: bool, bounds: Option<Bounds>) -> Result<Self, String> {
        if let Some(bounds) = bounds {
            let (height, width) = bounds.dimensions(headers)?;
            if dense_bytes(height, width) > IMPORT_MEMORY_LIMIT {
                return Err("Excel 导入数据超过 32 MiB。".into());
            }
        }
        Ok(Self {
            name,
            headers,
            bounds,
            cells: Vec::new(),
            text_bytes: 0,
        })
    }

    fn push(&mut self, position: (u32, u32), data: Data) -> Result<(), String> {
        if data == Data::Empty {
            return Ok(());
        }
        let (value, kind) = cell_value(data).map_err(|error| {
            format!(
                "工作表「{}」的 {} 单元格无法导入。{error}",
                self.name,
                address(position)
            )
        })?;
        let bounds = self.bounds.get_or_insert_with(|| Bounds::one(position));
        bounds.include(position);
        let (height, width) = bounds.dimensions(self.headers)?;
        self.text_bytes += value.len();
        let dense = dense_bytes(height, width);
        let sparse = (self.cells.len() + 1) * size_of::<StoredCell>();
        if dense.max(sparse) + self.text_bytes > IMPORT_MEMORY_LIMIT {
            return Err("Excel 导入数据超过 32 MiB。".into());
        }
        self.cells.push(StoredCell {
            position,
            value,
            kind,
        });
        Ok(())
    }

    fn finish(
        mut self,
        columns_only: bool,
        cancel: Option<&AtomicBool>,
    ) -> Result<SourceData, String> {
        let bounds = self.bounds.ok_or("Excel 工作表中没有可读取的列。")?;
        let (height, width) = bounds.dimensions(self.headers)?;
        let mut headers = vec![String::new(); width];
        let mut kinds = vec![None::<ValueKind>; width];
        let mut values = vec![0_usize; width];
        let mut rows = if columns_only {
            Vec::new()
        } else {
            vec![vec![Cell::Null; width]; height]
        };
        self.cells.sort_unstable_by_key(|cell| cell.position);
        let mut previous = None;
        for cell in self.cells {
            check_import_cancelled(cancel)?;
            let (row, column) = cell.position;
            if previous.replace(cell.position) == Some(cell.position) {
                let cell_address = address(cell.position);
                return Err(format!(
                    "工作表「{}」重复定义了 {cell_address} 单元格。",
                    self.name
                ));
            }
            let column = (column - bounds.left) as usize;
            if self.headers && row == bounds.top {
                headers[column] = cell.value;
            } else {
                values[column] += 1;
                kinds[column] = Some(kinds[column].map_or(cell.kind, |kind| kind.merge(cell.kind)));
                if !columns_only {
                    let row = (row - bounds.top) as usize - usize::from(self.headers);
                    rows[row][column] = Cell::Text(cell.value);
                }
            }
        }
        let columns = column_names(headers)?
            .into_iter()
            .zip(kinds)
            .zip(values)
            .map(|((name, kind), values)| SourceColumn {
                name,
                definition: OdbcColumnDefinition {
                    sql_type: kind.unwrap_or(ValueKind::Text).sql_type().into(),
                    not_null: height > 0 && values == height,
                    ..OdbcColumnDefinition::default()
                },
            })
            .collect();
        Ok(imported(columns, rows))
    }
}

fn dense_bytes(height: usize, width: usize) -> usize {
    height * (size_of::<Vec<Cell>>() + width * size_of::<Cell>())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ValueKind {
    Bool,
    Int,
    WideInt,
    Float,
    Date,
    DateTime,
    Time,
    Text,
}

impl ValueKind {
    fn sql_type(self) -> &'static str {
        match self {
            Self::Bool => "TINYINT",
            Self::Int | Self::WideInt => "BIGINT",
            Self::Float => "DOUBLE",
            Self::Date => "DATE",
            Self::DateTime => "DATETIME(6)",
            Self::Time => "TIME(6)",
            Self::Text => "LONGTEXT",
        }
    }

    fn merge(self, other: Self) -> Self {
        use ValueKind::{Bool, Date, DateTime, Float, Int, Text, WideInt};
        match (self, other) {
            (left, right) if left == right => left,
            (Bool, Int) | (Int, Bool) => Int,
            (Bool | Int, WideInt) | (WideInt, Bool | Int) => WideInt,
            (Bool | Int, Float) | (Float, Bool | Int) => Float,
            (Date, DateTime) | (DateTime, Date) => DateTime,
            _ => Text,
        }
    }
}

fn cell_value(value: Data) -> Result<(String, ValueKind), String> {
    use ValueKind::{Bool, DateTime, Float, Int, Text, WideInt};
    Ok(match value {
        Data::Int(value) => (
            value.to_string(),
            if value.unsigned_abs() > (1_u64 << 53) {
                WideInt
            } else {
                Int
            },
        ),
        Data::Float(value) if value.is_finite() => {
            let kind = if value.fract() == 0.0
                && value.abs() <= (1_u64 << 53) as f64
                && !(value == 0.0 && value.is_sign_negative())
            {
                Int
            } else {
                Float
            };
            (value.to_string(), kind)
        }
        Data::Float(_) => return Err("该单元格含非有限数值。".into()),
        Data::Bool(value) => (if value { "1" } else { "0" }.into(), Bool),
        Data::String(value) | Data::DurationIso(value) => (text_value(value)?, Text),
        Data::DateTime(value) if value.as_f64().abs() < 10_000_000.0 => {
            if value.is_duration() {
                let duration = value.as_duration().ok_or("Excel 时长超出范围。")?;
                duration_value(duration.num_milliseconds())
                    .unwrap_or_else(|| (duration.to_string(), Text))
            } else {
                let date = value.as_datetime().ok_or("Excel 日期超出范围。")?;
                let kind = if (1000..=9999).contains(&date.year()) {
                    DateTime
                } else {
                    Text
                };
                (date.format("%Y-%m-%d %H:%M:%S%.6f").to_string(), kind)
            }
        }
        Data::DateTime(_) => return Err("Excel 日期或时长的数值无效。".into()),
        Data::DateTimeIso(value) => iso_value(text_value(value)?),
        Data::Error(error) => return Err(format!("Excel 记录了错误值 {error}。")),
        Data::Empty => unreachable!("empty cells are represented by missing positions"),
    })
}

fn text_value(value: String) -> Result<String, String> {
    if value.contains('\0') {
        Err("该单元格含有 NUL 字符。".into())
    } else {
        Ok(value)
    }
}

fn iso_value(value: String) -> (String, ValueKind) {
    if let Ok(date) = value.parse::<NaiveDate>()
        && (1000..=9999).contains(&date.year())
    {
        return (date.format("%Y-%m-%d").to_string(), ValueKind::Date);
    }
    let date_time = value
        .parse::<NaiveDateTime>()
        .or_else(|_| NaiveDateTime::parse_from_str(&value, "%Y-%m-%d %H:%M:%S%.f"));
    if let Ok(date_time) = date_time
        && (1000..=9999).contains(&date_time.year())
        && date_time.nanosecond() < 1_000_000_000
        && date_time.nanosecond().is_multiple_of(1000)
    {
        return (
            date_time.format("%Y-%m-%d %H:%M:%S%.6f").to_string(),
            ValueKind::DateTime,
        );
    }
    if let Ok(time) = value.parse::<NaiveTime>()
        && time.nanosecond() < 1_000_000_000
        && time.nanosecond().is_multiple_of(1000)
    {
        return (time.format("%H:%M:%S%.6f").to_string(), ValueKind::Time);
    }
    (value, ValueKind::Text)
}

fn duration_value(millis: i64) -> Option<(String, ValueKind)> {
    let magnitude = millis.unsigned_abs();
    let hours = magnitude / 3_600_000;
    let minutes = magnitude / 60_000 % 60;
    let seconds = magnitude / 1_000 % 60;
    let fraction = magnitude % 1_000;
    let sign = if millis < 0 { "-" } else { "" };
    (magnitude <= 3_020_399_000).then(|| {
        (
            format!("{sign}{hours:02}:{minutes:02}:{seconds:02}.{fraction:03}"),
            ValueKind::Time,
        )
    })
}

fn address((row, column): (u32, u32)) -> String {
    let mut digits = Vec::new();
    let mut column = u64::from(column) + 1;
    while column != 0 {
        column -= 1;
        digits.push((b'A' + (column % 26) as u8) as char);
        column /= 26;
    }
    let letters = digits.into_iter().rev().collect::<String>();
    let row = u64::from(row) + 1;
    format!("{letters}{row}")
}
