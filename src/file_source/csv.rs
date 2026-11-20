use std::path::Path;
use std::sync::atomic::AtomicBool;

use encoding_rs::{BIG5, DecoderResult, GB18030, GBK, UTF_8, UTF_16BE, UTF_16LE};

use crate::database::{
    Cell, IMPORT_MEMORY_LIMIT, MAX_IMPORT_ROWS, OdbcColumnDefinition, check_import_cancelled,
    import_row_size,
};
use crate::odbc::{SourceColumn, SourceData};
use crate::sql_tools::delimited::csv_records;

use super::{FileSource, FileSourceEncoding, column_names, imported, read_bytes};

pub(super) fn read(
    path: &Path,
    source: &FileSource,
    columns_only: bool,
    cancel: Option<&AtomicBool>,
) -> Result<SourceData, String> {
    let text = decode(&read_bytes(path, cancel)?, source.encoding, cancel)?;
    let mut records = csv_records(&text, &source.delimiter)?;
    let first = records
        .next()
        .transpose()?
        .ok_or("CSV 文件中没有可读取的列。")?;
    let (headers, first_row) = if source.headers {
        (first, None)
    } else {
        (vec![String::new(); first.len()], Some(Ok(first)))
    };
    let columns = column_names(headers)?
        .into_iter()
        .map(|name| SourceColumn {
            name,
            definition: OdbcColumnDefinition {
                sql_type: "LONGTEXT".into(),
                ..OdbcColumnDefinition::default()
            },
        })
        .collect::<Vec<_>>();
    let mut rows = Vec::new();
    if !columns_only {
        let mut used = 0;
        for (index, record) in first_row.into_iter().chain(records).enumerate() {
            check_import_cancelled(cancel)?;
            if index == MAX_IMPORT_ROWS {
                return Err(format!("每张源表最多读取 {MAX_IMPORT_ROWS} 行数据。"));
            }
            let record = record?;
            if record.len() != columns.len() {
                let line = index + 1 + usize::from(source.headers);
                return Err(format!("CSV 第 {line} 条记录的字段数与首条记录不同。"));
            }
            let row = record.into_iter().map(Cell::Text).collect::<Vec<_>>();
            used += import_row_size(&row);
            if used > IMPORT_MEMORY_LIMIT {
                return Err("CSV 导入数据超过 32 MiB。".into());
            }
            rows.push(row);
        }
    }
    Ok(imported(columns, rows))
}

fn decode(
    mut input: &[u8],
    fallback: FileSourceEncoding,
    cancel: Option<&AtomicBool>,
) -> Result<String, String> {
    let selected = [
        (&[0xef, 0xbb, 0xbf][..], FileSourceEncoding::Utf8),
        (&[0xff, 0xfe][..], FileSourceEncoding::Utf16Le),
        (&[0xfe, 0xff][..], FileSourceEncoding::Utf16Be),
    ]
    .into_iter()
    .find_map(|(prefix, encoding)| {
        input.strip_prefix(prefix).map(|rest| {
            input = rest;
            encoding
        })
    })
    .unwrap_or(fallback);
    let encoding = match selected {
        FileSourceEncoding::Utf8 => UTF_8,
        FileSourceEncoding::Gbk => GBK,
        FileSourceEncoding::Gb18030 => GB18030,
        FileSourceEncoding::Big5 => BIG5,
        FileSourceEncoding::Utf16Le => UTF_16LE,
        FileSourceEncoding::Utf16Be => UTF_16BE,
    };
    let mut decoder = encoding.new_decoder_without_bom_handling();
    let mut output = String::new();
    let mut buffer = String::with_capacity(8192);
    loop {
        check_import_cancelled(cancel)?;
        buffer.clear();
        let (status, consumed) =
            decoder.decode_to_string_without_replacement(input, &mut buffer, true);
        if let DecoderResult::Malformed(..) = status {
            return Err(format!("CSV 文件含有无效的 {} 字节。", selected.label()));
        }
        if buffer.contains('\0') {
            return Err("CSV 文件含有 NUL 字符，请检查源文件格式和编码。".into());
        }
        if output.len() + buffer.len() > IMPORT_MEMORY_LIMIT {
            return Err("CSV 解码后的文本超过 32 MiB。".into());
        }
        output.push_str(&buffer);
        if status == DecoderResult::InputEmpty {
            return Ok(output);
        }
        input = &input[consumed..];
    }
}
