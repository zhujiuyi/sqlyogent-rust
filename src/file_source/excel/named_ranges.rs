use std::borrow::Cow;
use std::collections::HashMap;

use calamine::{Reader, SheetType, Sheets};

use super::xls_named_ranges;
use super::{Bounds, Workbook};

const EXCEL_ROWS: u32 = 1_048_576;
const EXCEL_COLUMNS: u32 = 16_384;

pub(super) struct NamedRange<'a> {
    pub(super) name: Cow<'a, str>,
    pub(super) sheet: &'a str,
    pub(super) bounds: Bounds,
}

pub(super) fn list<'a>(
    workbook: &'a Workbook,
    xls_names: &'a [xls_named_ranges::NamedRange],
) -> Vec<NamedRange<'a>> {
    if matches!(workbook, Sheets::Xls(_)) {
        return biff_ranges(workbook, xls_names);
    }
    let mut sheets = HashMap::new();
    for sheet in workbook
        .sheets_metadata()
        .iter()
        .filter(|sheet| sheet.typ == SheetType::WorkSheet)
    {
        sheets
            .entry(sheet.name.to_lowercase())
            .and_modify(|existing| *existing = None)
            .or_insert(Some(sheet.name.as_str()));
    }

    // Calamine omits the sheet scope of local names. A bare name must be unique.
    let definitions = workbook.defined_names();
    let mut counts = HashMap::new();
    for (name, _) in definitions {
        *counts.entry(name.to_lowercase()).or_insert(0_usize) += 1;
    }

    definitions
        .iter()
        .filter_map(|(name, formula)| {
            let lowercase = name.to_lowercase();
            if name.trim().is_empty()
                || name.chars().any(char::is_control)
                || lowercase.starts_with("_xlnm.")
                || counts.get(&lowercase) != Some(&1)
            {
                return None;
            }
            let (sheet_name, bounds) = reference(formula)?;
            let sheet = sheets.get(&sheet_name.to_lowercase()).copied().flatten()?;
            Some(NamedRange {
                name: Cow::Borrowed(name),
                sheet,
                bounds,
            })
        })
        .collect()
}

fn biff_ranges<'a>(
    workbook: &'a Workbook,
    names: &'a [xls_named_ranges::NamedRange],
) -> Vec<NamedRange<'a>> {
    let sheets = workbook.sheets_metadata();
    let mut counts = HashMap::new();
    for sheet in sheets {
        *counts.entry(sheet.name.to_lowercase()).or_insert(0_usize) += 1;
    }
    let unique_sheet = |index: usize| {
        sheets.get(index).filter(|sheet| {
            !sheet.name.trim().is_empty()
                && !sheet.name.chars().any(char::is_control)
                && counts.get(&sheet.name.to_lowercase()) == Some(&1)
        })
    };
    names
        .iter()
        .filter_map(|range| {
            let sheet = unique_sheet(range.sheet)?;
            if sheet.typ != SheetType::WorkSheet {
                return None;
            }
            let name = match range.scope {
                Some(scope) => {
                    let scope = unique_sheet(scope)?.name.replace('\'', "''");
                    Cow::Owned(format!("'{scope}'!{}", range.name))
                }
                None => Cow::Borrowed(range.name.as_str()),
            };
            Some(NamedRange {
                name,
                sheet: &sheet.name,
                bounds: range.bounds,
            })
        })
        .collect()
}

fn reference(formula: &str) -> Option<(Cow<'_, str>, Bounds)> {
    let formula = formula.trim();
    let formula = formula.strip_prefix('=').unwrap_or(formula).trim();
    let (sheet, cells) = formula.rsplit_once('!')?;
    let sheet = sheet_name(sheet)?;
    let (start, end) = cells.split_once(':').unwrap_or((cells, cells));
    let first = cell_position(start)?;
    let mut bounds = Bounds::one(first);
    bounds.include(cell_position(end)?);
    Some((sheet, bounds))
}

fn sheet_name(name: &str) -> Option<Cow<'_, str>> {
    let name = if let Some(quoted) = name.strip_prefix('\'') {
        let quoted = quoted.strip_suffix('\'')?;
        if quoted.split("''").any(|part| part.contains('\'')) {
            return None;
        }
        Cow::Owned(quoted.replace("''", "'"))
    } else {
        if !name.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '_' | '.')
                || (!character.is_ascii() && !character.is_whitespace() && !character.is_control())
        }) {
            return None;
        }
        Cow::Borrowed(name)
    };
    (!name.is_empty() && !name.contains(['[', ']', ':', '\\', '/', '?', '*', '\0'])).then_some(name)
}

fn cell_position(reference: &str) -> Option<(u32, u32)> {
    let (column, row) = reference.strip_prefix('$')?.split_once('$')?;
    if !(1..=3).contains(&column.len())
        || !column.bytes().all(|byte| byte.is_ascii_alphabetic())
        || !(1..=7).contains(&row.len())
        || !row.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let column = column.bytes().fold(0_u32, |index, byte| {
        index * 26 + u32::from(byte.to_ascii_uppercase() - b'A' + 1)
    });
    let row = row.parse::<u32>().ok()?;
    if !(1..=EXCEL_COLUMNS).contains(&column) || !(1..=EXCEL_ROWS).contains(&row) {
        return None;
    }
    Some((row - 1, column - 1))
}
