use std::borrow::{Borrow, Cow};
use std::io::{self, Write};

use mysql::{Column, consts::ColumnType};

use super::{
    Cell, column_is_binary, display_value, hex_text, html_text, output_error, xml_text_is_valid,
};

pub const EXCEL_CELL_TEXT_LIMIT: usize = 32_767;

#[derive(Clone, Copy, Debug, Default)]
pub struct ExcelOptions {
    pub text_limit: Option<usize>,
    pub decimal_places: Option<u16>,
}

#[derive(Clone, Copy, Debug, Default)]
pub enum ExcelColumnType {
    Text,
    Decimal,
    #[default]
    Other,
}

impl ExcelColumnType {
    pub(in crate::database) fn from_native(column: &Column) -> Self {
        if column_is_binary(column) {
            Self::Other
        } else {
            Self::from_protocol_type(column.column_type(), true)
        }
    }

    pub(in crate::database) fn from_type_name(data_type: &str) -> Self {
        let name = data_type
            .split(|char: char| char == '(' || char.is_ascii_whitespace())
            .next()
            .unwrap_or_default();
        if let Some(kind) = name
            .parse::<u8>()
            .ok()
            .and_then(|code| code.try_into().ok())
        {
            return Self::from_protocol_type(kind, false);
        }
        match name.to_ascii_lowercase().as_str() {
            "char" | "varchar" | "var_string" | "string" | "tinytext" | "text" | "mediumtext"
            | "longtext" | "enum" | "set" => Self::Text,
            "decimal" | "newdecimal" | "numeric" | "float" | "double" | "real" => Self::Decimal,
            _ => Self::Other,
        }
    }

    fn from_protocol_type(kind: ColumnType, known_text_blobs: bool) -> Self {
        use ColumnType::*;
        match kind {
            MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL | MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => {
                Self::Decimal
            }
            MYSQL_TYPE_VARCHAR
            | MYSQL_TYPE_VAR_STRING
            | MYSQL_TYPE_STRING
            | MYSQL_TYPE_ENUM
            | MYSQL_TYPE_SET => Self::Text,
            MYSQL_TYPE_TINY_BLOB
            | MYSQL_TYPE_MEDIUM_BLOB
            | MYSQL_TYPE_LONG_BLOB
            | MYSQL_TYPE_BLOB
                if known_text_blobs =>
            {
                Self::Text
            }
            _ => Self::Other,
        }
    }
}

const HEADER: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
    <?mso-application progid=\"Excel.Sheet\"?>\n\
    <Workbook xmlns=\"urn:schemas-microsoft-com:office:spreadsheet\" xmlns:ss=\"urn:schemas-microsoft-com:office:spreadsheet\">\n\
    <Worksheet ss:Name=\"数据\"><Table>\n";

pub(super) fn export_excel<C: Borrow<Cell>>(
    out: &mut impl Write,
    names: &[String],
    types: &[ExcelColumnType],
    rows: impl Iterator<Item = Result<Vec<C>, String>>,
    options: ExcelOptions,
) -> Result<u64, String> {
    if names.len() > 256 {
        return Err("Excel 2003 XML 最多支持 256 列，请改用 CSV 或普通 XML。".into());
    }
    if options
        .text_limit
        .is_some_and(|limit| !(1..=EXCEL_CELL_TEXT_LIMIT).contains(&limit))
    {
        return Err(format!("文本长度上限应为 1 到 {EXCEL_CELL_TEXT_LIMIT}。"));
    }
    writeln!(out, "{HEADER}<Row>").map_err(output_error)?;
    for name in names {
        write_text(out, name).map_err(output_error)?;
    }
    writeln!(out, "</Row>").map_err(output_error)?;
    let mut count = 0;
    for row in rows {
        if count == 65_535 {
            return Err(
                "Excel 2003 XML 最多支持 65,535 行数据和一行列名，请改用 CSV 或普通 XML。".into(),
            );
        }
        write!(out, "<Row>").map_err(output_error)?;
        for (index, cell) in row?.iter().enumerate() {
            let kind = types.get(index).copied().unwrap_or_default();
            write_cell(out, cell.borrow(), kind, options).map_err(output_error)?;
        }
        writeln!(out, "</Row>").map_err(output_error)?;
        count += 1;
    }
    writeln!(out, "</Table></Worksheet></Workbook>").map_err(output_error)?;
    Ok(count)
}

fn write_cell(
    out: &mut impl Write,
    cell: &Cell,
    kind: ExcelColumnType,
    options: ExcelOptions,
) -> io::Result<()> {
    match (cell, kind) {
        (Cell::Null, _) => write!(out, "<Cell/>"),
        (Cell::Text(value), ExcelColumnType::Text) => {
            let value = if let Some(limit) = options.text_limit
                && xml_text_is_valid(value)
            {
                limit_utf16(value, limit)
            } else {
                value
            };
            write_text(out, value)
        }
        (Cell::Text(value), ExcelColumnType::Decimal) => {
            if let Some(places) = options.decimal_places
                && let Some(decimal) = limit_decimal(value, places)
            {
                // Use Excel numbers only where Excel's 15 significant digits can hold the value.
                if excel_accepts_number(&decimal) {
                    write!(
                        out,
                        "<Cell><Data ss:Type=\"Number\">{decimal}</Data></Cell>"
                    )
                } else {
                    write_text(out, &decimal)
                }
            } else {
                write_text(out, value)
            }
        }
        _ => write_text(out, &display_value(cell)),
    }
}

fn limit_utf16(value: &str, mut remaining: usize) -> &str {
    for (index, char) in value.char_indices() {
        let needed = char.len_utf16();
        if needed > remaining {
            return &value[..index];
        }
        remaining -= needed;
    }
    value
}

fn limit_decimal(value: &str, places: u16) -> Option<Cow<'_, str>> {
    let (mantissa, exponent) = match value.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, exponent.parse::<i32>().ok()?),
        None => (value, 0),
    };
    let sign = if mantissa.starts_with(['-', '+']) {
        &mantissa[..1]
    } else {
        ""
    };
    let unsigned = &mantissa[sign.len()..];
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let digits = whole.len() + fraction.len();
    if digits == 0
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let decimal_point = whole.len() as i64 + i64::from(exponent);
    let retained = decimal_point + i64::from(places);
    if retained >= digits as i64 {
        return Some(value.into());
    }
    if retained <= 0 {
        return Some("0".into());
    }
    if exponent == 0 && !whole.is_empty() {
        let end = sign.len() + retained as usize + usize::from(places > 0);
        return Some(value[..end].into());
    }
    let digits = [whole, fraction].concat();
    let prefix = &digits[..retained as usize];
    if decimal_point <= 0 {
        let zeros = "0".repeat((-decimal_point) as usize);
        return Some(format!("{sign}0.{zeros}{prefix}").into());
    }
    let (whole, fraction) = prefix.split_at(decimal_point as usize);
    if fraction.is_empty() {
        Some(format!("{sign}{whole}").into())
    } else {
        Some(format!("{sign}{whole}.{fraction}").into())
    }
}

fn excel_accepts_number(value: &str) -> bool {
    let mantissa = value.split(['e', 'E']).next().unwrap_or_default();
    let digits = mantissa
        .bytes()
        .filter(u8::is_ascii_digit)
        .skip_while(|&digit| digit == b'0');
    let significant = digits
        .enumerate()
        .filter(|&(_, digit)| digit != b'0')
        .last()
        .map_or(0, |(index, _)| index + 1);
    significant <= 15
        && value
            .parse::<f64>()
            .is_ok_and(|number| number.is_normal() || number == 0.0 && significant == 0)
}

fn write_text(out: &mut impl Write, value: &str) -> io::Result<()> {
    let valid = xml_text_is_valid(value);
    let units = if valid {
        value.encode_utf16().count()
    } else {
        value.len() * 2 + 2
    };
    if units > EXCEL_CELL_TEXT_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "单元格内容超过 Excel 的 32,767 字符限制，请改用 CSV 或普通 XML。",
        ));
    }
    let text = if valid {
        html_text(value).replace('\r', "&#xD;")
    } else {
        format!("0x{}", hex_text(value.as_bytes()))
    };
    write!(
        out,
        "<Cell><Data ss:Type=\"String\" xml:space=\"preserve\">{text}</Data></Cell>"
    )
}
