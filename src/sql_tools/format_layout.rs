use std::ops::Range;

use serde::{Deserialize, Serialize};

use super::{
    EditorToken, EditorTokenKind, Quoting, ScriptScanner, editor_tokens, formatting_tokens,
};

pub const MAX_FORMAT_INDENT: u8 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct SqlFormatSettings {
    pub stacked: bool,
    pub align_aliases: bool,
    pub comma_first: bool,
    pub indent_spaces: u8,
}

impl Default for SqlFormatSettings {
    fn default() -> Self {
        Self {
            stacked: true,
            align_aliases: true,
            comma_first: false,
            indent_spaces: 2,
        }
    }
}

/// Formats a selection in its original script so that custom delimiters and
/// quoted tokens that cross a selection boundary remain intact.
pub fn format_sql_with_settings(
    script: &str,
    range: Range<usize>,
    settings: SqlFormatSettings,
    modes: (bool, bool),
) -> String {
    let Some(selected) = script.get(range.clone()) else {
        return String::new();
    };
    let mut scanner = ScriptScanner::new(script);
    scanner.quoting = Quoting {
        backslash: !modes.0,
        ansi_identifiers: modes.1,
    };
    let quoting = scanner.quoting;
    let mut output = String::with_capacity(selected.len());
    let mut previous = range.start;
    let mut previous_formatted = false;
    let newline = script
        .find('\n')
        .filter(|&offset| offset > 0 && script.as_bytes()[offset - 1] == b'\r')
        .map_or("\n", |_| "\r\n");
    for part in scanner {
        let statement = part.statement;
        let original = statement.range;
        if original.start >= range.end {
            break;
        }
        let start = original.start.max(range.start);
        let end = original.end.min(range.end);
        if start >= end {
            continue;
        }

        let gap = &script[previous..start];
        let eligible = !part.custom_delimiter
            && ((start == original.start && end == original.end)
                || editor_tokens(statement.sql, modes.0, modes.1).all(|token| {
                    let start = start - original.start;
                    let end = end - original.start;
                    start >= token.range.end
                        || end <= token.range.start
                        || (start <= token.range.start && end >= token.range.end)
                }));
        if previous_formatted
            && eligible
            && gap
                .strip_prefix(';')
                .is_some_and(|rest| rest.bytes().all(|byte| matches!(byte, b' ' | b'\t')))
        {
            output.push(';');
            output.push_str(newline);
        } else {
            output.push_str(gap);
        }
        let sql = &script[start..end];
        if eligible {
            output.push_str(&format_statement(sql, settings, modes, quoting, newline));
        } else {
            output.push_str(sql);
        }
        previous = end;
        previous_formatted = eligible;
    }
    output.push_str(&script[previous..range.end]);
    output
}

fn format_statement(
    sql: &str,
    settings: SqlFormatSettings,
    modes: (bool, bool),
    quoting: Quoting,
    newline: &str,
) -> String {
    let indent = settings.indent_spaces.min(MAX_FORMAT_INDENT);
    let options = sqlformat::FormatOptions {
        indent: sqlformat::Indent::Spaces(indent),
        max_inline_arguments: Some(sql.len()),
        max_inline_top_level: Some(sql.len()),
        ..Default::default()
    };
    let generic = sqlformat::format(sql, &sqlformat::QueryParams::None, &options);
    let base = if formatting_tokens(sql, quoting).eq(formatting_tokens(&generic, quoting)) {
        generic.as_str()
    } else {
        sql
    };
    let candidate = arrange_lists(base, settings, modes, usize::from(indent), newline);
    if formatting_tokens(sql, quoting).eq(formatting_tokens(&candidate, quoting)) {
        candidate
    } else {
        base.to_owned()
    }
}

struct List {
    head: usize,
    first: usize,
    select: bool,
    commas: Vec<usize>,
    aliases: Vec<usize>,
    comment: bool,
}

impl List {
    fn new(index: usize, select: bool) -> Self {
        Self {
            head: index,
            first: index + 1,
            select,
            commas: Vec::new(),
            aliases: Vec::new(),
            comment: false,
        }
    }
}

fn arrange_lists(
    sql: &str,
    settings: SqlFormatSettings,
    modes: (bool, bool),
    indent: usize,
    newline: &str,
) -> String {
    let tokens: Vec<_> = editor_tokens(sql, modes.0, modes.1).collect();
    let mut gaps = vec![None; tokens.len()];
    let mut levels: Vec<Option<List>> = vec![None];
    for (index, token) in tokens.iter().enumerate() {
        let nested = levels.len() > 1;
        let current = levels.last_mut().unwrap();
        match token.kind {
            EditorTokenKind::Symbol('(') => levels.push(None),
            EditorTokenKind::Symbol(')') if nested => {
                if let Some(list) = levels.pop().flatten() {
                    apply_list(
                        sql, &tokens, &mut gaps, list, index, settings, indent, newline,
                    );
                }
            }
            EditorTokenKind::Word if word(token, "SELECT") => {
                *current = Some(List::new(index, true));
            }
            EditorTokenKind::Word
                if current.is_some()
                    && [
                        "FROM",
                        "INTO",
                        "WHERE",
                        "GROUP",
                        "HAVING",
                        "WINDOW",
                        "ORDER",
                        "LIMIT",
                        "UNION",
                        "INTERSECT",
                        "EXCEPT",
                        "FOR",
                        "LOCK",
                        "PROCEDURE",
                    ]
                    .iter()
                    .any(|name| word(token, name)) =>
            {
                let list = current.take().unwrap();
                let starts_tables = list.select && word(token, "FROM");
                apply_list(
                    sql, &tokens, &mut gaps, list, index, settings, indent, newline,
                );
                if starts_tables {
                    *current = Some(List::new(index, false));
                }
            }
            _ => {
                if let Some(list) = current {
                    if token.kind == EditorTokenKind::Symbol(',') {
                        list.commas.push(index);
                    } else if word(token, "AS") {
                        list.aliases.push(index);
                    } else if list.select
                        && index == list.first
                        && [
                            "ALL",
                            "DISTINCT",
                            "DISTINCTROW",
                            "HIGH_PRIORITY",
                            "STRAIGHT_JOIN",
                            "SQL_SMALL_RESULT",
                            "SQL_BIG_RESULT",
                            "SQL_BUFFER_RESULT",
                            "SQL_NO_CACHE",
                            "SQL_CALC_FOUND_ROWS",
                        ]
                        .iter()
                        .any(|name| word(token, name))
                    {
                        list.first += 1;
                    } else if is_comment(token) {
                        list.comment = true;
                    }
                }
            }
        }
    }
    for list in levels.into_iter().flatten() {
        apply_list(
            sql,
            &tokens,
            &mut gaps,
            list,
            tokens.len(),
            settings,
            indent,
            newline,
        );
    }

    let mut result = String::with_capacity(sql.len());
    let mut previous = 0;
    for (token, gap) in tokens.into_iter().zip(gaps) {
        if let Some(gap) = gap {
            result.push_str(&gap);
        } else {
            let space = &sql[previous..token.range.start];
            if newline == "\r\n" && space.contains('\n') {
                result.push_str(&space.replace("\r\n", "\n").replace('\n', newline));
            } else {
                result.push_str(space);
            }
        }
        result.push_str(token.text);
        previous = token.range.end;
    }
    result.push_str(&sql[previous..]);
    result
}

fn word(token: &EditorToken<'_>, name: &str) -> bool {
    token.kind == EditorTokenKind::Word && token.text.eq_ignore_ascii_case(name)
}

fn is_comment(token: &EditorToken<'_>) -> bool {
    matches!(token.kind, EditorTokenKind::Protected { .. })
        && (token.text.starts_with('#')
            || token.text.starts_with("--")
            || token.text.starts_with("/*"))
}

fn apply_list(
    sql: &str,
    tokens: &[EditorToken<'_>],
    gaps: &mut [Option<String>],
    list: List,
    end: usize,
    settings: SqlFormatSettings,
    indent: usize,
    newline: &str,
) {
    if list.comment || list.first >= end || list.commas.last() == Some(&(end - 1)) {
        return;
    }
    let head = tokens[list.head].range.start;
    let line = sql[..head].rsplit('\n').next().unwrap_or("");
    let current_indent = line.bytes().take_while(|byte| *byte == b' ').count();
    let next_line = format!("{newline}{}", " ".repeat(current_indent + indent));
    gaps[list.first] = Some(if settings.stacked && list.select {
        next_line.clone()
    } else {
        " ".to_owned()
    });
    for &comma in &list.commas {
        if !settings.stacked {
            gaps[comma] = Some(String::new());
            gaps[comma + 1] = Some(" ".to_owned());
        } else if settings.comma_first {
            gaps[comma] = Some(next_line.clone());
            gaps[comma + 1] = Some(String::new());
        } else {
            gaps[comma] = Some(String::new());
            gaps[comma + 1] = Some(next_line.clone());
        }
    }
    if !settings.stacked || !settings.align_aliases || !list.select {
        return;
    }
    let mut aliases = Vec::new();
    let mut first = list.first;
    let mut row = 0;
    for &alias in &list.aliases {
        while let Some(&comma) = list.commas.get(row).filter(|&&comma| comma < alias) {
            first = comma + 1;
            row += 1;
        }
        if alias <= first {
            continue;
        }
        let expression = &sql[tokens[first].range.start..tokens[alias - 1].range.end];
        if !expression.contains('\n') {
            let comma_width = usize::from(settings.comma_first && row > 0);
            aliases.push((alias, comma_width + expression.chars().count()));
        }
    }
    if let Some(widest) = aliases.iter().map(|&(_, width)| width).max() {
        for (alias, width) in aliases {
            gaps[alias] = Some(" ".repeat(widest - width + 1));
        }
    }
}

#[cfg(test)]
#[test]
fn formats_lists_in_the_chosen_style_and_aligns_explicit_aliases() {
    let sql = "SELECT id AS item, COALESCE(price, 0) AS total FROM products p, rates r WHERE p.id = r.id;";
    let settings = SqlFormatSettings {
        indent_spaces: 4,
        ..SqlFormatSettings::default()
    };
    let render = |settings| format_sql_with_settings(sql, 0..sql.len(), settings, (false, false));
    let stacked = render(settings);
    assert!(
        stacked.contains("FROM products p,\n    rates r"),
        "{stacked}"
    );
    let columns: Vec<_> = stacked
        .lines()
        .filter_map(|line| line.find(" AS "))
        .collect();
    assert_eq!(columns, [22, 22], "{stacked}");

    let leading = render(SqlFormatSettings {
        align_aliases: false,
        comma_first: true,
        ..settings
    });
    assert!(
        leading.contains("\n    id AS item\n    ,COALESCE(price, 0) AS total"),
        "{leading}"
    );
    assert!(
        leading.contains("FROM products p\n    ,rates r"),
        "{leading}"
    );

    let inline = render(SqlFormatSettings {
        stacked: false,
        ..settings
    });
    assert!(
        inline.contains("SELECT id AS item, COALESCE(price, 0) AS total\nFROM products p, rates r"),
        "{inline}"
    );
}

#[cfg(test)]
#[test]
fn keeps_current_routine_and_formats_crlf_batch_with_literals() {
    let script = "DELIMITER $$\r\nCREATE PROCEDURE p() BEGIN SELECT 'p;'; END$$\r\nDELIMITER ;\r\nSELECT '甲\n乙', b;SELECT 2;";
    let start = script.find("CREATE").unwrap();
    let end = script.find("$$\r\nDELIMITER").unwrap();
    let render = |range| {
        format_sql_with_settings(script, range, SqlFormatSettings::default(), (false, false))
    };
    assert_eq!(render(start..end), script[start..end]);
    let formatted = render(0..script.len());
    assert!(
        formatted.contains("'甲\n乙',\r\n  b;\r\nSELECT\r\n  2;"),
        "{formatted}"
    );
}
