use std::collections::HashSet;
use std::io::Write;

use mysql::consts::{ColumnFlags, ColumnType};

use crate::http_tunnel::Field;

use super::super::TableColumn;
use super::{
    Cell, DataKind, ResultExport, SqlExportMode, TransferSummary, column_is_binary, output_error,
    quote_identifier, valid_number, write_value,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct ResultColumn {
    pub(super) declared_width: Option<usize>,
    sql_type: SqlType,
    long_value: bool,
}

#[derive(Clone, Copy, Debug, Default)]
enum SqlType {
    Integer(&'static str, Option<bool>),
    Bit(usize),
    Decimal(Option<(usize, usize)>),
    FloatingPoint(&'static str),
    LegacyReal,
    Text(&'static str),
    Binary(&'static str),
    #[default]
    Unknown,
}

impl ResultColumn {
    pub fn has_declared_width(&self) -> bool {
        self.declared_width.is_some()
    }

    pub(crate) fn is_long_value(&self) -> bool {
        self.long_value
    }

    pub fn is_numeric(&self) -> bool {
        matches!(
            self.sql_type,
            SqlType::Integer(..)
                | SqlType::Bit(..)
                | SqlType::Decimal(..)
                | SqlType::FloatingPoint(..)
                | SqlType::LegacyReal
        )
    }

    fn is_long_value_type(kind: ColumnType) -> bool {
        use ColumnType::*;
        matches!(
            kind,
            MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB
        )
    }

    pub(in crate::database) fn from_native(column: &mysql::Column) -> Self {
        Self {
            declared_width: Self::native_width(column),
            sql_type: SqlType::from_protocol(column.column_type(), Some(column)),
            long_value: Self::is_long_value_type(column.column_type()),
        }
    }

    pub(super) fn native_width(column: &mysql::Column) -> Option<usize> {
        use ColumnType::*;
        let supported = !column_is_binary(column)
            && !Self::is_long_value_type(column.column_type())
            && !matches!(column.column_type(), MYSQL_TYPE_JSON | MYSQL_TYPE_VECTOR);
        let width = column.column_length() as usize;
        (supported && width != 0).then_some(width)
    }

    pub(in crate::database) fn from_http(field: &Field) -> Self {
        // The tunnel reports max_length (observed), not the declared display width.
        let name = field.data_type.split(['(', ' ']).next().unwrap_or_default();
        let (sql_type, long_value) = if let Some(code) = name
            .parse::<u8>()
            .ok()
            .and_then(|code| code.try_into().ok())
        {
            (
                SqlType::from_protocol(code, None),
                Self::is_long_value_type(code),
            )
        } else {
            let name = name.to_ascii_lowercase();
            let sql_type = match name.as_str() {
                "int" | "integer" | "tinyint" | "smallint" | "mediumint" | "bigint" => {
                    SqlType::Integer("BIGINT", None)
                }
                "decimal" | "newdecimal" | "numeric" => SqlType::Decimal(None),
                "float" => SqlType::FloatingPoint("FLOAT"),
                "double" => SqlType::FloatingPoint("DOUBLE"),
                "real" => SqlType::LegacyReal,
                "date" | "newdate" => SqlType::Text("DATE"),
                "time" => SqlType::Text("TIME(6)"),
                "datetime" | "timestamp" => SqlType::Text("DATETIME(6)"),
                "year" => SqlType::Integer("YEAR", Some(false)),
                "geometry" => SqlType::Binary("GEOMETRY"),
                "binary" | "varbinary" | "bit" => SqlType::Binary("LONGBLOB"),
                "text" | "tinytext" | "mediumtext" | "longtext" | "enum" | "set" | "json" => {
                    SqlType::Text("LONGTEXT")
                }
                _ => SqlType::Unknown,
            };
            // SQLyog's PHP tunnel uses the same family names for BLOB and TEXT.
            let long_value = matches!(
                name.as_str(),
                "tinyblob"
                    | "blob"
                    | "mediumblob"
                    | "longblob"
                    | "tinytext"
                    | "text"
                    | "mediumtext"
                    | "longtext"
            );
            (sql_type, long_value)
        };
        Self {
            sql_type,
            declared_width: None,
            long_value,
        }
    }

    pub(super) fn from_table_column(column: &TableColumn) -> Self {
        let declaration = column.data_type.to_ascii_lowercase();
        let kind = declaration.split(['(', ' ']).next().unwrap_or_default();
        let parameters = || {
            declaration
                .split_once('(')?
                .1
                .split_once(')')
                .map(|(p, _)| p)
        };
        let integer = |name| {
            let unsigned = declaration
                .split_ascii_whitespace()
                .any(|word| word == "unsigned");
            SqlType::Integer(name, Some(unsigned))
        };
        let sql_type = match kind {
            "tinyint" => integer("TINYINT"),
            "smallint" => integer("SMALLINT"),
            "mediumint" => integer("MEDIUMINT"),
            "int" | "integer" => integer("INT"),
            "bigint" => integer("BIGINT"),
            "year" => SqlType::Integer("YEAR", Some(false)),
            "bit" => parameters()
                .and_then(|bits| bits.trim().parse::<usize>().ok())
                .filter(|bits| (1..=64).contains(bits))
                .map_or(SqlType::Integer("BIGINT", Some(true)), SqlType::Bit),
            "decimal" | "numeric" => {
                let dimensions = parameters().and_then(|p| {
                    let (precision, scale) = p.split_once(',').unwrap_or((p, "0"));
                    let precision = precision.trim().parse::<usize>().ok()?;
                    let scale = scale.trim().parse::<usize>().ok()?;
                    (scale <= precision && (1..=65).contains(&precision))
                        .then(|| (precision - scale, scale))
                });
                SqlType::Decimal(dimensions)
            }
            "float" => SqlType::FloatingPoint("FLOAT"),
            "double" | "real" => SqlType::FloatingPoint("DOUBLE"),
            "date" => SqlType::Text("DATE"),
            "datetime" | "timestamp" => SqlType::Text("DATETIME(6)"),
            "time" => SqlType::Text("TIME(6)"),
            "geometry" | "point" | "linestring" | "polygon" | "multipoint" | "multilinestring"
            | "multipolygon" | "geometrycollection" | "vector" => SqlType::Binary("LONGBLOB"),
            _ if column.is_binary() => SqlType::Binary("LONGBLOB"),
            _ => SqlType::Text("LONGTEXT"),
        };
        Self {
            declared_width: column.declared_width,
            sql_type,
            long_value: matches!(
                kind,
                "tinyblob"
                    | "blob"
                    | "mediumblob"
                    | "longblob"
                    | "tinytext"
                    | "text"
                    | "mediumtext"
                    | "longtext"
            ),
        }
    }
}

impl SqlType {
    fn from_protocol(kind: ColumnType, native: Option<&mysql::Column>) -> Self {
        use ColumnType::*;
        let unsigned = native.map(|column| column.flags().contains(ColumnFlags::UNSIGNED_FLAG));
        match kind {
            MYSQL_TYPE_TINY => Self::Integer("TINYINT", unsigned),
            MYSQL_TYPE_SHORT => Self::Integer("SMALLINT", unsigned),
            MYSQL_TYPE_INT24 => Self::Integer("MEDIUMINT", unsigned),
            MYSQL_TYPE_LONG => Self::Integer("INT", unsigned),
            MYSQL_TYPE_LONGLONG => Self::Integer("BIGINT", unsigned),
            MYSQL_TYPE_YEAR => Self::Integer("YEAR", Some(false)),
            MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => {
                let width = native.and_then(|column| {
                    let scale = column.decimals() as usize;
                    let extra = usize::from(unsigned == Some(false)) + usize::from(scale != 0);
                    let precision = (column.column_length() as usize).checked_sub(extra)?;
                    (scale <= precision && precision <= 65).then(|| (precision - scale, scale))
                });
                Self::Decimal(width)
            }
            MYSQL_TYPE_FLOAT => Self::FloatingPoint("FLOAT"),
            MYSQL_TYPE_DOUBLE => Self::FloatingPoint("DOUBLE"),
            MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => Self::Text("DATE"),
            MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => Self::Text("TIME(6)"),
            MYSQL_TYPE_DATETIME
            | MYSQL_TYPE_DATETIME2
            | MYSQL_TYPE_TIMESTAMP
            | MYSQL_TYPE_TIMESTAMP2 => Self::Text("DATETIME(6)"),
            MYSQL_TYPE_GEOMETRY => Self::Binary("GEOMETRY"),
            MYSQL_TYPE_BIT | MYSQL_TYPE_VECTOR => Self::Binary("LONGBLOB"),
            MYSQL_TYPE_JSON | MYSQL_TYPE_ENUM | MYSQL_TYPE_SET | MYSQL_TYPE_NULL => {
                Self::Text("LONGTEXT")
            }
            _ => match native.map(column_is_binary) {
                Some(true) => Self::Binary("LONGBLOB"),
                Some(false) => Self::Text("LONGTEXT"),
                None => Self::Unknown,
            },
        }
    }

    fn resolve<'a>(self, values: impl Iterator<Item = Option<&'a [u8]>>) -> Self {
        let mut values = values.flatten();
        match self {
            Self::Unknown
                if values
                    .any(|bytes| bytes.contains(&0) || std::str::from_utf8(bytes).is_err()) =>
            {
                Self::Binary("LONGBLOB")
            }
            Self::Unknown => Self::Text("LONGTEXT"),
            Self::LegacyReal
                if values.any(|bytes| bytes.contains(&b'e') || bytes.contains(&b'E')) =>
            {
                Self::FloatingPoint("DOUBLE")
            }
            Self::LegacyReal => Self::Decimal(None),
            kind => kind,
        }
    }

    fn definition<'a>(
        self,
        values: impl Iterator<Item = Option<&'a [u8]>>,
    ) -> Result<String, String> {
        let definition = match self {
            Self::Integer(data_type, Some(true)) => format!("{data_type} UNSIGNED"),
            Self::Integer(_, None) => "DECIMAL(20,0)".into(),
            Self::Bit(width) => format!("BIT({width})"),
            Self::Integer(data_type, Some(false))
            | Self::FloatingPoint(data_type)
            | Self::Text(data_type)
            | Self::Binary(data_type) => data_type.into(),
            Self::Decimal(declared) => {
                let (mut integer, mut scale) = declared.unwrap_or_default();
                for bytes in values.flatten() {
                    let (integer_digits, fraction_digits) = decimal_width(bytes)?;
                    integer = integer.max(integer_digits);
                    scale = scale.max(fraction_digits);
                }
                let precision = (integer + scale).max(1);
                if precision > 65 || scale > 38 {
                    return Err(
                        "精度超出 DECIMAL 的范围。可改用仅数据或在查询中显式转换为文本。".into(),
                    );
                }
                format!("DECIMAL({precision},{scale})")
            }
            Self::Unknown | Self::LegacyReal => unreachable!("export type must be resolved"),
        };
        Ok(definition)
    }

    fn write(self, out: &mut impl Write, bytes: Option<&[u8]>) -> Result<(), String> {
        let kind = match self {
            Self::Integer(..) | Self::Bit(_) => {
                if bytes.is_some_and(|bytes| {
                    bytes.iter().any(|byte| matches!(byte, b'.' | b'e' | b'E'))
                }) {
                    return Err("服务器返回了无效的整数。".into());
                }
                DataKind::Number
            }
            Self::Decimal(_) => {
                if let Some(bytes) = bytes {
                    decimal_width(bytes)?;
                }
                DataKind::Number
            }
            Self::FloatingPoint(_) => {
                write_value(out, DataKind::Number, bytes, None)?;
                if bytes.is_some_and(|bytes| !bytes.contains(&b'e') && !bytes.contains(&b'E')) {
                    out.write_all(b"e0").map_err(output_error)?;
                }
                return Ok(());
            }
            Self::Text(_) => DataKind::Text,
            Self::Binary(_) => DataKind::Binary,
            Self::Unknown | Self::LegacyReal => unreachable!("export type must be resolved"),
        };
        write_value(out, kind, bytes, None)
    }
}

fn decimal_width(bytes: &[u8]) -> Result<(usize, usize), String> {
    if !valid_number(bytes) || bytes.contains(&b'e') || bytes.contains(&b'E') {
        return Err("服务器返回了无效的定点数。".into());
    }
    let unsigned = bytes
        .strip_prefix(b"-")
        .or_else(|| bytes.strip_prefix(b"+"))
        .unwrap_or(bytes);
    let boundary = unsigned
        .iter()
        .position(|&byte| byte == b'.')
        .unwrap_or(unsigned.len());
    let integer = unsigned[..boundary]
        .iter()
        .skip_while(|&&byte| byte == b'0')
        .count();
    let fraction = unsigned.len().saturating_sub(boundary + 1);
    if integer + fraction > 65 || fraction > 38 {
        return Err("定点数超出 SQL 的精度范围，可在查询中显式转换为文本。".into());
    }
    Ok((integer, fraction))
}

#[derive(Clone, Debug, Default)]
pub struct ResultSqlTarget {
    pub schema: String,
    pub table: String,
}

impl ResultSqlTarget {
    pub fn validate(&self) -> Result<(), String> {
        let valid = |name: &str| valid_identifier(name) && !name.contains(['.', '/', '\\']);
        if !valid(&self.table) {
            return Err("请输入有效的目标表名（最多 64 个字符，库名单独填写）。".into());
        }
        if !self.schema.is_empty() && !valid(&self.schema) {
            return Err("请输入有效的目标库名（最多 64 个字符）。".into());
        }
        Ok(())
    }

    fn qualified_name(&self) -> String {
        let table = quote_identifier(&self.table);
        if self.schema.is_empty() {
            table
        } else {
            format!("{}.{table}", quote_identifier(&self.schema))
        }
    }
}

fn valid_identifier(name: &str) -> bool {
    !name.is_empty()
        && !name.ends_with(' ')
        && name.chars().count() <= 64
        && !name
            .chars()
            .any(|char| char.is_control() || char as u32 > 0xFFFF)
}

fn cell_bytes(cell: &Cell) -> Option<&[u8]> {
    match cell {
        Cell::Null => None,
        Cell::Text(text) => Some(text.as_bytes()),
        Cell::Binary(bytes) => Some(bytes),
    }
}

const HEADER: &str = "SET @harbor_result_sql_mode = @@SESSION.sql_mode,\n\
        @harbor_result_charset_client = @@SESSION.character_set_client,\n\
        @harbor_result_charset_results = @@SESSION.character_set_results,\n\
        @harbor_result_collation = @@SESSION.collation_connection;\n\
    SET NAMES utf8mb4;\n\
    SET SESSION sql_mode = 'STRICT_ALL_TABLES,NO_AUTO_VALUE_ON_ZERO,ALLOW_INVALID_DATES';\n";

const FOOTER: &str = "SET SESSION sql_mode = @harbor_result_sql_mode;\n\
    SET SESSION character_set_client = @harbor_result_charset_client;\n\
    SET SESSION character_set_results = @harbor_result_charset_results;\n\
    SET SESSION collation_connection = @harbor_result_collation;\n";

pub(super) fn write_result_sql(
    out: &mut impl Write,
    result: ResultExport<'_>,
    indices: &[usize],
    names: &[String],
    target: &ResultSqlTarget,
    mode: SqlExportMode,
) -> Result<TransferSummary, String> {
    target.validate()?;
    if names.is_empty() {
        return Err("请选择至少一个导出字段。".into());
    }
    let mut used_names = HashSet::new();
    for name in names {
        if !valid_identifier(name) || !used_names.insert(name.to_lowercase()) {
            return Err(format!(
                "查询结果的字段名「{name}」无效或重复，请在查询中使用唯一的字段别名。"
            ));
        }
    }
    if result.rows().any(|row| row.len() != result.column_count()) {
        return Err("查询结果中的列数不符。".into());
    }
    let data_types = indices
        .iter()
        .map(|&index| {
            result
                .export_column(index)
                .sql_type
                .resolve(result.rows().map(move |row| cell_bytes(&row[index])))
        })
        .collect::<Vec<_>>();
    let table = target.qualified_name();
    let quoted_names = names
        .iter()
        .map(|name| quote_identifier(name))
        .collect::<Vec<_>>();
    writeln!(out, "-- Harbor SQL 查询结果").map_err(output_error)?;
    if result.truncated() && mode != SqlExportMode::Structure {
        writeln!(out, "-- 查询结果已截断，仅包含已加载的数据。").map_err(output_error)?;
    }
    write!(out, "{HEADER}").map_err(output_error)?;
    if mode != SqlExportMode::Data {
        writeln!(out, "CREATE TABLE {table} (").map_err(output_error)?;
        for (field, (&index, &data_type)) in indices.iter().zip(&data_types).enumerate() {
            let definition = data_type
                .definition(result.rows().map(|row| cell_bytes(&row[index])))
                .map_err(|error| format!("字段「{}」：{error}", names[field]))?;
            let separator = if field + 1 == names.len() { "" } else { "," };
            writeln!(out, "  {} {definition}{separator}", quoted_names[field])
                .map_err(output_error)?;
        }
        writeln!(out, ") DEFAULT CHARACTER SET utf8mb4;").map_err(output_error)?;
    }
    let rows = if mode == SqlExportMode::Structure {
        0
    } else {
        let insert = format!("INSERT INTO {table} ({}) VALUES (", quoted_names.join(", "));
        for (row_number, row) in result.rows().enumerate() {
            out.write_all(insert.as_bytes()).map_err(output_error)?;
            for (field, (&index, &data_type)) in indices.iter().zip(&data_types).enumerate() {
                if field > 0 {
                    out.write_all(b", ").map_err(output_error)?;
                }
                data_type
                    .write(out, cell_bytes(&row[index]))
                    .map_err(|error| {
                        format!(
                            "第 {} 行，字段「{}」：{error}",
                            row_number + 1,
                            names[field]
                        )
                    })?;
            }
            writeln!(out, ");").map_err(output_error)?;
        }
        result.row_count() as u64
    };
    write!(out, "{FOOTER}").map_err(output_error)?;
    Ok(TransferSummary {
        objects: 1,
        rows,
        ..Default::default()
    })
}
