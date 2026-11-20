use std::io::{Cursor, Read, Write};
use std::sync::atomic::AtomicBool;

use crate::database::{IMPORT_MEMORY_LIMIT, check_import_cancelled};

use super::xls_named_ranges::{self, NamedRange};

const END_OF_SUBSTREAM: u16 = 0x000a;
const BOUND_SHEET: u16 = 0x0085;
const DIMENSIONS: u16 = 0x0200;
const FORMULA: u16 = 0x0006;
const MUL_RK: u16 = 0x00bd;
const BOF: u16 = 0x0809;
const BIFF_HEADER_SIZE: usize = 4;
const INVALID: &str = "XLS 工作簿的记录或工作表范围无效。";
const TOO_LARGE: &str = "XLS 工作簿中的工作表使用范围超过 32 MiB。";

// Calamine materializes every legacy sheet during open. Inspect the Workbook
// stream first to bound both declared capacity and the rectangle it will build.
pub(super) fn prepare(
    bytes: Vec<u8>,
    named_ranges: bool,
    cancel: Option<&AtomicBool>,
) -> Result<(Vec<u8>, Vec<NamedRange>), String> {
    let mut file = cfb::CompoundFile::open(Cursor::new(bytes)).map_err(read_error)?;
    let stream_name = if file.is_stream("/Workbook") {
        "/Workbook"
    } else {
        "/Book"
    };
    let stream = file.open_stream(stream_name).map_err(read_error)?;
    if stream.len() > IMPORT_MEMORY_LIMIT as u64 {
        return Err("XLS 工作簿数据超过 32 MiB。".into());
    }
    let mut records = Vec::new();
    stream
        .take(IMPORT_MEMORY_LIMIT as u64 + 1)
        .read_to_end(&mut records)
        .map_err(read_error)?;
    if records.len() > IMPORT_MEMORY_LIMIT {
        return Err("XLS 工作簿数据超过 32 MiB。".into());
    }
    let bytes = file.into_inner().into_inner();

    let mut offsets = Vec::new();
    let mut omit_formula_lengths = Vec::new();
    let mut globals = records.as_slice();
    let mut names = named_ranges.then(xls_named_ranges::Names::default);
    let mut worksheet_biff8 = false;
    if named_ranges {
        xls_named_ranges::check_globals(&mut globals)?;
    }
    loop {
        check_import_cancelled(cancel)?;
        let payload_offset = records.len() - globals.len() + BIFF_HEADER_SIZE;
        let (record, data) = next(&mut globals)?;
        let omit_formula = if let Some(names) = &mut names {
            names.observe(record, data, &mut globals, cancel)?
        } else {
            if payload_offset == BIFF_HEADER_SIZE {
                worksheet_biff8 = xls_named_ranges::is_biff8_globals(record, data);
            } else if matches!(record, BOF | xls_named_ranges::FILE_PASS) {
                worksheet_biff8 = false;
                omit_formula_lengths.clear();
            }
            worksheet_biff8 && xls_named_ranges::has_ancillary_label(record, data)
        };
        if omit_formula {
            omit_formula_lengths
                .push(payload_offset + xls_named_ranges::LABEL_FORMULA_LENGTH_OFFSET);
        }
        match record {
            END_OF_SUBSTREAM => break,
            BOUND_SHEET => {
                let offset = number32(data.get(..4).ok_or(INVALID)?) as usize;
                offsets.push(offset);
                if offsets.len() > 16_384 {
                    return Err("XLS 工作簿包含过多工作表。".into());
                }
            }
            _ => {}
        }
    }
    let sheet_count = offsets.len();
    let mut remaining = IMPORT_MEMORY_LIMIT;
    for offset in offsets {
        check_import_cancelled(cancel)?;
        let sheet = records.get(offset..).ok_or(INVALID)?;
        let used = check_sheet(sheet, cancel)?;
        remaining = remaining.checked_sub(used).ok_or(TOO_LARGE)?;
    }
    if names.is_none() && omit_formula_lengths.is_empty() {
        return Ok((bytes, Vec::new()));
    }
    let names = names
        .map(|names| names.resolve(sheet_count, cancel))
        .transpose()?
        .unwrap_or_default();
    // Calamine 0.38 locates Lbl formulas from the end of the record. Hide the
    // formula length for labels with trailing data in the typed-cell reader.
    // Name order and cell records stay intact.
    for offset in omit_formula_lengths {
        records[offset..offset + 2].fill(0);
    }
    // Calamine looks up the first matching CFB directory entry by name,
    // including entries outside the root. Hand it only the verified stream.
    drop(bytes);
    Ok((verified_compound(&records, cancel)?, names))
}

fn verified_compound(records: &[u8], cancel: Option<&AtomicBool>) -> Result<Vec<u8>, String> {
    let mut file =
        cfb::CompoundFile::create_with_version(cfb::Version::V3, Cursor::new(Vec::new()))
            .map_err(read_error)?;
    {
        let mut stream = file.create_stream("/Workbook").map_err(read_error)?;
        for block in records.chunks(64 * 1024) {
            check_import_cancelled(cancel)?;
            stream.write_all(block).map_err(read_error)?;
        }
        stream.flush().map_err(read_error)?;
    }
    file.flush().map_err(read_error)?;
    Ok(file.into_inner().into_inner())
}

fn check_sheet(mut records: &[u8], cancel: Option<&AtomicBool>) -> Result<usize, String> {
    let mut values = None;
    let mut formulas = None;
    let mut declared_bytes = 0;
    let mut seen_bof = false;
    let mut nested = 0;
    loop {
        check_import_cancelled(cancel)?;
        let (record, data) = next(&mut records)?;
        if record == BOF {
            if seen_bof {
                nested += 1;
            }
            seen_bof = true;
            continue;
        }
        if record == END_OF_SUBSTREAM {
            if nested == 0 {
                break;
            }
            nested -= 1;
        }
        if nested != 0 {
            continue;
        }
        match record {
            DIMENSIONS => {
                let slots = dimensions(data)?;
                // Includes the coordinate pair stored alongside each cell.
                let bytes = slots * size_of::<calamine::Cell<calamine::Data>>();
                if bytes > IMPORT_MEMORY_LIMIT {
                    return Err(TOO_LARGE.into());
                }
                declared_bytes = declared_bytes.max(bytes);
            }
            FORMULA | MUL_RK | 0x0203 | 0x0204 | 0x0205 | 0x027e | 0x00fd | 0x00d6 => {
                let row = number16(data.get(..2).ok_or(INVALID)?);
                let first = number16(data.get(2..4).ok_or(INVALID)?);
                let last = if record == MUL_RK {
                    number16(data.get(data.len() - 2..).ok_or(INVALID)?)
                } else {
                    first
                };
                if first > last || last >= 256 {
                    return Err(INVALID.into());
                }
                include(&mut values, row, first, last);
                if record == FORMULA {
                    include(&mut formulas, row, first, last);
                }
            }
            _ => {}
        }
    }
    let used = values.map_or(0, |range| range.slots() * size_of::<calamine::Data>())
        + formulas.map_or(0, |range| range.slots() * size_of::<String>());
    Ok(used.max(declared_bytes))
}

pub(super) fn next<'a>(input: &mut &'a [u8]) -> Result<(u16, &'a [u8]), String> {
    let header = input.get(..BIFF_HEADER_SIZE).ok_or(INVALID)?;
    let record = number16(&header[..2]);
    let length = usize::from(number16(&header[2..]));
    let end = BIFF_HEADER_SIZE + length;
    let data = input.get(BIFF_HEADER_SIZE..end).ok_or(INVALID)?;
    *input = &input[end..];
    Ok((record, data))
}

fn dimensions(data: &[u8]) -> Result<usize, String> {
    let (top, bottom, left, right) = match data.len() {
        10 => (
            u32::from(number16(&data[..2])),
            u32::from(number16(&data[2..4])),
            number16(&data[4..6]),
            number16(&data[6..8]),
        ),
        14 | 16 => (
            number32(&data[..4]),
            number32(&data[4..8]),
            number16(&data[8..10]),
            number16(&data[10..12]),
        ),
        _ => return Err(INVALID.into()),
    };
    // The last row and column are exclusive in the Excel DIMENSIONS record.
    if top > bottom
        || bottom > 65_536
        || left > right
        || right > 256
        || bottom > 0 && right > 0 && (top == bottom || left == right)
    {
        return Err(INVALID.into());
    }
    Ok(((bottom - top) as usize).max(1) * usize::from((right - left).max(1)))
}

struct Extent {
    top: u16,
    bottom: u16,
    left: u16,
    right: u16,
}

impl Extent {
    fn slots(&self) -> usize {
        (usize::from(self.bottom - self.top) + 1) * (usize::from(self.right - self.left) + 1)
    }
}

fn include(range: &mut Option<Extent>, row: u16, first: u16, last: u16) {
    let range = range.get_or_insert(Extent {
        top: row,
        bottom: row,
        left: first,
        right: last,
    });
    range.top = range.top.min(row);
    range.bottom = range.bottom.max(row);
    range.left = range.left.min(first);
    range.right = range.right.max(last);
}

pub(super) fn number16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(
        bytes
            .try_into()
            .expect("two bytes checked by BIFF record length"),
    )
}

fn number32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(
        bytes
            .try_into()
            .expect("four bytes checked by BIFF record length"),
    )
}

fn read_error(error: std::io::Error) -> String {
    format!("无法读取 XLS 工作簿数据。{error}")
}
