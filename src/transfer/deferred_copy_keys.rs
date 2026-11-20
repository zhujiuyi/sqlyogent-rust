use std::{borrow::Cow, collections::HashSet};

use crate::sql_tools::{EditorToken, EditorTokenKind, definition_tokens};

use super::comparable_name;

const TABLE_DEFINITION: &str = "无法从服务器返回的建表语句中分离外键。";
const FOREIGN_KEY: &str = "无法识别建表语句中的外键定义。";

pub(super) fn defer_selected(
    ddl: &str,
    destination_schema: &str,
    selected: &HashSet<String>,
    sql_mode: &str,
    case_mode: u8,
) -> Result<(String, Vec<String>), String> {
    let tokens: Vec<_> = definition_tokens(ddl, sql_mode)
        .filter(|token| !is_comment(token))
        .collect();
    let open = table_open(&tokens).ok_or(TABLE_DEFINITION)?;
    let destination = comparable_name(destination_schema, case_mode);
    let mut create = String::with_capacity(ddl.len());
    let mut clause_start = open + 1;
    let mut text_start = tokens[open].range.end;
    let mut position = clause_start;
    let mut kept_clause = false;
    let mut adds = Vec::new();
    create.push_str(&ddl[..text_start]);

    while let Some(token) = tokens.get(position) {
        if symbol(token, '(') {
            position = after_parentheses(&tokens, position).ok_or(TABLE_DEFINITION)?;
            continue;
        }
        let closing = symbol(token, ')');
        if closing || symbol(token, ',') {
            if clause_start == position {
                return Err(TABLE_DEFINITION.into());
            }
            let part = &tokens[clause_start..position];
            let text = &ddl[text_start..token.range.start];
            let defer = referenced_table(part)?.is_some_and(|name| {
                name.schema
                    .is_none_or(|schema| comparable_name(&schema, case_mode) == destination)
                    && selected.contains(&comparable_name(&name.table, case_mode))
            });
            if defer {
                adds.push(format!("ADD {text}"));
                if closing {
                    create.push_str(&text[text.trim_ascii_end().len()..]);
                }
            } else {
                if kept_clause {
                    create.push(',');
                }
                create.push_str(text);
                kept_clause = true;
            }
            if closing {
                if !kept_clause {
                    return Err(TABLE_DEFINITION.into());
                }
                create.push_str(&ddl[token.range.start..]);
                return Ok((create, adds));
            }
            clause_start = position + 1;
            text_start = token.range.end;
        }
        position += 1;
    }
    Err(TABLE_DEFINITION.into())
}

fn is_comment(token: &EditorToken<'_>) -> bool {
    matches!(token.kind, EditorTokenKind::Protected { .. })
        && (token.text.starts_with("/*")
            || token.text.starts_with("--")
            || token.text.starts_with('#'))
}

fn keyword(token: Option<&EditorToken<'_>>, word: &str) -> bool {
    token.is_some_and(|token| {
        token.kind == EditorTokenKind::Word && token.text.eq_ignore_ascii_case(word)
    })
}

fn symbol(token: &EditorToken<'_>, character: char) -> bool {
    token.kind == EditorTokenKind::Symbol(character)
}

fn identifier<'a>(token: &EditorToken<'a>) -> Option<Cow<'a, str>> {
    match token.kind {
        EditorTokenKind::Word if !token.text.bytes().all(|byte| byte.is_ascii_digit()) => {
            Some(Cow::Borrowed(token.text))
        }
        EditorTokenKind::Identifier {
            quote,
            closed: true,
        } => {
            let quote = char::from(if quote == b'[' { b']' } else { quote }).to_string();
            Some(Cow::Owned(
                token.text[1..token.text.len() - 1].replace(&quote.repeat(2), &quote),
            ))
        }
        _ => None,
    }
}

struct Name<'a> {
    schema: Option<Cow<'a, str>>,
    table: Cow<'a, str>,
}

fn read_name<'a>(tokens: &[EditorToken<'a>], start: usize) -> Option<(Name<'a>, usize)> {
    let first = if symbol(tokens.get(start)?, '.') {
        start + 1
    } else {
        start
    };
    let name = identifier(tokens.get(first)?)?;
    if tokens
        .get(first + 1)
        .is_some_and(|token| symbol(token, '.'))
    {
        let table = identifier(tokens.get(first + 2)?)?;
        Some((
            Name {
                schema: Some(name),
                table,
            },
            first + 3,
        ))
    } else {
        Some((
            Name {
                schema: None,
                table: name,
            },
            first + 1,
        ))
    }
}

fn table_open(tokens: &[EditorToken<'_>]) -> Option<usize> {
    if !keyword(tokens.first(), "CREATE") {
        return None;
    }
    let mut index = 1;
    if keyword(tokens.get(index), "OR") && keyword(tokens.get(index + 1), "REPLACE") {
        index += 2;
    }
    if keyword(tokens.get(index), "TEMPORARY") {
        index += 1;
    }
    if !keyword(tokens.get(index), "TABLE") {
        return None;
    }
    index += 1;
    if keyword(tokens.get(index), "IF")
        && keyword(tokens.get(index + 1), "NOT")
        && keyword(tokens.get(index + 2), "EXISTS")
    {
        index += 3;
    }
    let (_, open) = read_name(tokens, index)?;
    symbol(tokens.get(open)?, '(').then_some(open)
}

fn after_parentheses(tokens: &[EditorToken<'_>], start: usize) -> Option<usize> {
    if !symbol(tokens.get(start)?, '(') {
        return None;
    }
    let mut depth = 0;
    for (position, token) in tokens.iter().enumerate().skip(start) {
        if symbol(token, '(') {
            depth += 1;
        } else if symbol(token, ')') {
            depth -= 1;
            if depth == 0 {
                return Some(position + 1);
            }
        }
    }
    None
}

fn referenced_table<'a>(tokens: &[EditorToken<'a>]) -> Result<Option<Name<'a>>, &'static str> {
    let first = if keyword(tokens.first(), "CONSTRAINT") {
        if keyword(tokens.get(1), "FOREIGN") {
            1
        } else {
            2
        }
    } else {
        0
    };
    if !keyword(tokens.get(first), "FOREIGN") {
        return Ok(None);
    }
    if !keyword(tokens.get(first + 1), "KEY") {
        return Err(FOREIGN_KEY);
    }
    let mut columns = first + 2;
    let token = tokens.get(columns).ok_or(FOREIGN_KEY)?;
    if !symbol(token, '(') {
        identifier(token).ok_or(FOREIGN_KEY)?;
        columns += 1;
    }
    let references = after_parentheses(tokens, columns).ok_or(FOREIGN_KEY)?;
    if !keyword(tokens.get(references), "REFERENCES") {
        return Err(FOREIGN_KEY);
    }
    let (name, _) = read_name(tokens, references + 1).ok_or(FOREIGN_KEY)?;
    Ok(Some(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defers_only_selected_local_foreign_keys_without_reformatting_definitions() {
        let prefix = r#"CREATE TABLE "cycle_schema"."child" ("#;
        let columns = r#"
  "group_id" INT DEFAULT NULL,
  "parent_id" INT DEFAULT NULL,
  "root_id" INT NOT NULL,
  "external_id" INT NOT NULL,
  "stable_id" INT NOT NULL,
  "payload" VARCHAR(128) DEFAULT '(a, ''b'')',
  KEY "fk_columns" ("group_id", "parent_id"),
  /* retain column conditions, including commas */
  CONSTRAINT "payload_size" CHECK (LENGTH("payload") IN (0, 5))"#;
        let composite = r#"
  /* composite local reference */
  CONSTRAINT "fk""parent" FOREIGN KEY ("group_id", "parent_id")
  REFERENCES "Cycle_Schema" /* between qualifiers */ ."PaRent" ("group_id", "id")
  ON DELETE SET NULL ON UPDATE CASCADE"#;
        let external = r#"
  CONSTRAINT "fk_external" FOREIGN KEY ("external_id")
  REFERENCES "archive"."parent" ("id") ON DELETE RESTRICT"#;
        let unselected = r#"
  CONSTRAINT "fk_stable" FOREIGN KEY ("stable_id")
  REFERENCES "cycle_schema"."stable" ("id") ON UPDATE CASCADE"#;
        let unqualified = r#"
  FOREIGN KEY "root_idx" ("root_id") REFERENCES "root" ("id") ON UPDATE NO ACTION -- local root
"#;
        let options = r#") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COMMENT='preserved, ()';"#;
        let ddl =
            format!("{prefix}{columns},{composite},{external},{unselected},{unqualified}{options}");
        let selected = HashSet::from(["parent".into(), "root".into()]);
        let (create, adds) = defer_selected(
            &ddl,
            "cycle_schema",
            &selected,
            "ANSI_QUOTES,NO_BACKSLASH_ESCAPES",
            1,
        )
        .unwrap();
        assert_eq!(
            create,
            format!("{prefix}{columns},{external},{unselected}\n{options}")
        );
        assert_eq!(
            adds,
            [format!("ADD {composite}"), format!("ADD {unqualified}")]
        );

        let partitioned = "CREATE TABLE `cycle_schema`.`history` (
  `id` INT NOT NULL,
  `marker` VARCHAR(5) DEFAULT '(),',
  PRIMARY KEY (`id`)
) ENGINE=InnoDB /*!50100 PARTITION BY HASH (`id`) PARTITIONS 4 */";
        let (create, adds) = defer_selected(partitioned, "cycle_schema", &selected, "", 0).unwrap();
        assert_eq!(create, partitioned);
        assert!(adds.is_empty());
    }
}
