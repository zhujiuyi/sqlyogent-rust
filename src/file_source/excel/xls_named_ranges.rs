use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;

use crate::database::check_import_cancelled;

use super::xls_limits::{next, number16};
use super::{Bounds, MAX_ARCHIVE_MEMBERS};

const BOF: u16 = 0x0809;
pub(super) const FILE_PASS: u16 = 0x002f;
const BOUND_SHEET: u16 = 0x0085;
const SUP_BOOK: u16 = 0x01ae;
const EXTERN_SHEET: u16 = 0x0017;
const LABEL: u16 = 0x0018;
const CONTINUE: u16 = 0x003c;
const INVALID_LAYOUT: &str = "XLS 命名区域的工作簿记录格式无效或不受支持。可使用工作表模式读取。";
pub(super) const LABEL_FORMULA_LENGTH_OFFSET: usize = 4;

pub(super) struct NamedRange {
    pub(super) name: String,
    pub(super) scope: Option<usize>,
    pub(super) sheet: usize,
    pub(super) bounds: Bounds,
}

#[derive(Default)]
pub(super) struct Names {
    books: Vec<bool>,
    links: Option<Vec<SheetLink>>,
    labels: Vec<Label>,
}

struct SheetLink {
    book: usize,
    first: u16,
    last: u16,
}

struct Label {
    name: String,
    scope: Option<usize>,
    reference: Option<(usize, Bounds)>,
}

pub(super) fn check_globals(input: &mut &[u8]) -> Result<(), String> {
    let (record, data) = next(input)?;
    if !is_biff8_globals(record, data) {
        return Err(
            "XLS 命名区域需要 Excel 97–2003（BIFF8）工作簿。可使用工作表模式读取此文件。".into(),
        );
    }
    Ok(())
}

pub(super) fn is_biff8_globals(record: u16, data: &[u8]) -> bool {
    record == BOF && data.get(..4) == Some(&[0x00, 0x06, 0x05, 0x00])
}

pub(super) fn has_ancillary_label(record: u16, data: &[u8]) -> bool {
    record == LABEL
        && label_data(data).is_ok_and(|label| label.ancillary && !label.formula.is_empty())
}

impl Names {
    pub(super) fn observe(
        &mut self,
        record: u16,
        data: &[u8],
        remaining: &mut &[u8],
        cancel: Option<&AtomicBool>,
    ) -> Result<bool, String> {
        match record {
            BOF => return Err(INVALID_LAYOUT.into()),
            FILE_PASS => return Err("不能读取加密 XLS 的命名区域。请选择解密后的工作簿。".into()),
            BOUND_SHEET => {
                let count = usize::from(*data.get(6).ok_or(INVALID_LAYOUT)?);
                unicode_bytes(data.get(7..).ok_or(INVALID_LAYOUT)?, count)?;
            }
            SUP_BOOK => {
                let marker = number16(data.get(2..4).ok_or(INVALID_LAYOUT)?);
                self.books.push(marker == 0x0401 && data.len() == 4);
            }
            EXTERN_SHEET => {
                // Separate tables have conflicting index ordering in spreadsheet readers.
                if self.links.is_some() {
                    return Err(
                        "此 XLS 有多个工作表引用表，无法确定命名区域的位置。可使用工作表模式读取。"
                            .into(),
                    );
                }
                self.links = Some(read_links(data, remaining, cancel)?);
            }
            LABEL => {
                let (label, ancillary_data) = parse_label(data)?;
                if let Some(label) = label {
                    if self.labels.len() == MAX_ARCHIVE_MEMBERS {
                        return Err("XLS 工作簿包含过多名称定义。".into());
                    }
                    self.labels.push(label);
                }
                return Ok(ancillary_data);
            }
            _ => {}
        }
        Ok(false)
    }

    pub(super) fn resolve(
        self,
        sheet_count: usize,
        cancel: Option<&AtomicBool>,
    ) -> Result<Vec<NamedRange>, String> {
        let Some(links) = self.links else {
            return Ok(Vec::new());
        };
        let mut counts = HashMap::new();
        for label in &self.labels {
            check_import_cancelled(cancel)?;
            *counts
                .entry((label.scope, label.name.to_lowercase()))
                .or_insert(0_usize) += 1;
        }
        let mut ranges = Vec::new();
        for label in self.labels {
            check_import_cancelled(cancel)?;
            let Some((link, bounds)) = label.reference else {
                continue;
            };
            let Some(link) = links.get(link) else {
                continue;
            };
            let sheet = usize::from(link.first);
            if self.books.get(link.book) != Some(&true)
                || link.first != link.last
                || sheet >= sheet_count
                || label.scope.is_some_and(|scope| scope >= sheet_count)
                || counts.get(&(label.scope, label.name.to_lowercase())) != Some(&1)
            {
                continue;
            }
            ranges.push(NamedRange {
                name: label.name,
                scope: label.scope,
                sheet,
                bounds,
            });
        }
        Ok(ranges)
    }
}

fn read_links(
    data: &[u8],
    remaining: &mut &[u8],
    cancel: Option<&AtomicBool>,
) -> Result<Vec<SheetLink>, String> {
    let expected = 2 + usize::from(number16(data.get(..2).ok_or(INVALID_LAYOUT)?)) * 6;
    let data = if data.len() < expected {
        let mut joined = Vec::with_capacity(expected);
        joined.extend_from_slice(data);
        while joined.len() < expected {
            check_import_cancelled(cancel)?;
            let (record, fragment) = next(remaining)?;
            if record != CONTINUE || fragment.len() > expected - joined.len() {
                return Err(INVALID_LAYOUT.into());
            }
            joined.extend_from_slice(fragment);
        }
        Cow::Owned(joined)
    } else {
        Cow::Borrowed(data)
    };
    if data.len() != expected
        || remaining
            .get(..2)
            .is_some_and(|header| number16(header) == CONTINUE)
    {
        return Err(INVALID_LAYOUT.into());
    }
    Ok(data[2..]
        .chunks_exact(6)
        .map(|link| SheetLink {
            book: usize::from(number16(&link[..2])),
            first: number16(&link[2..4]),
            last: number16(&link[4..6]),
        })
        .collect())
}

struct LabelData<'a> {
    wide: bool,
    name: &'a [u8],
    formula: &'a [u8],
    ancillary: bool,
}

fn label_data(data: &[u8]) -> Result<LabelData<'_>, String> {
    let header = data.get(..14).ok_or(INVALID_LAYOUT)?;
    let count = usize::from(header[3]);
    let offset = LABEL_FORMULA_LENGTH_OFFSET;
    let length = usize::from(number16(&header[offset..offset + 2]));
    let (wide, name) = unicode_bytes(&data[14..], count)?;
    let start = 15 + name.len();
    let end = start + length;
    let formula = data.get(start..end).ok_or(INVALID_LAYOUT)?;
    Ok(LabelData {
        wide,
        name,
        formula,
        ancillary: end < data.len(),
    })
}

fn parse_label(data: &[u8]) -> Result<(Option<Label>, bool), String> {
    let LabelData {
        wide,
        name,
        formula,
        ancillary,
    } = label_data(data)?;
    let scope = usize::from(number16(&data[8..10])).checked_sub(1);

    if !ancillary {
        // Calamine will read these tokens even when the name is not selected.
        let minimum = match formula.first() {
            Some(0x3a | 0x5a | 0x7a) => 7,
            Some(0x3b | 0x5b | 0x7b) => 11,
            Some(0x3c | 0x5c | 0x7c | 0x3d | 0x5d | 0x7d) => 3,
            _ => 0,
        };
        if formula.len() < minimum {
            return Err(INVALID_LAYOUT.into());
        }
    }
    // Exclude built-in names and macro/procedure names.
    if number16(&data[..2]) & 0x002e != 0 {
        return Ok((None, ancillary));
    }
    let name = if wide {
        String::from_utf16(&name.chunks_exact(2).map(number16).collect::<Vec<_>>())
            .map_err(|_| INVALID_LAYOUT)?
    } else {
        name.iter().copied().map(char::from).collect()
    };
    if name.trim().is_empty() || name.contains(['!', '\'']) || name.chars().any(char::is_control) {
        return Ok((None, ancillary));
    }

    // A direct static reference has one token and no ancillary revision data.
    let reference = (!ancillary).then(|| absolute_reference(formula)).flatten();
    Ok((
        Some(Label {
            name,
            scope,
            reference,
        }),
        ancillary,
    ))
}

fn unicode_bytes(data: &[u8], count: usize) -> Result<(bool, &[u8]), String> {
    let wide = data.first().ok_or(INVALID_LAYOUT)? & 1 != 0;
    let end = 1 + (1 + usize::from(wide)) * count;
    Ok((wide, data.get(1..end).ok_or(INVALID_LAYOUT)?))
}

fn absolute_reference(formula: &[u8]) -> Option<(usize, Bounds)> {
    let (link, top, bottom, left, right) = match formula {
        [0x3a | 0x5a | 0x7a, i0, i1, r0, r1, c0, c1] => {
            let link = u16::from_le_bytes([*i0, *i1]);
            let row = u16::from_le_bytes([*r0, *r1]);
            let column = u16::from_le_bytes([*c0, *c1]);
            (link, row, row, column, column)
        }
        [0x3b | 0x5b | 0x7b, rest @ ..] if rest.len() == 10 => (
            number16(&rest[..2]),
            number16(&rest[2..4]),
            number16(&rest[4..6]),
            number16(&rest[6..8]),
            number16(&rest[8..10]),
        ),
        _ => return None,
    };
    // The BIFF8 grid has 256 columns; either relative flag sets a higher bit.
    if link == u16::MAX || top > bottom || left > right || right >= 256 {
        return None;
    }
    Some((
        usize::from(link),
        Bounds {
            top: u32::from(top),
            bottom: u32::from(bottom),
            left: u32::from(left),
            right: u32::from(right),
        },
    ))
}
