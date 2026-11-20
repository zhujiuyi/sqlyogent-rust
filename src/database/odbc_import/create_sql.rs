use crate::sql_tools::{EditorToken, EditorTokenKind, definition_tokens};

use super::{DbObject, OdbcColumnDefinition, OdbcIndex, OdbcTarget, create_fields};

pub fn odbc_create_table_sql(
    object: &DbObject,
    columns: &[String],
    definitions: &[OdbcColumnDefinition],
    indexes: &[OdbcIndex],
    engine: Option<&str>,
) -> Result<String, String> {
    generate(
        object,
        columns,
        definitions,
        indexes,
        engine.unwrap_or("InnoDB"),
        false,
    )
    .map(|sql| format!("{sql};"))
}

fn generate(
    object: &DbObject,
    columns: &[String],
    definitions: &[OdbcColumnDefinition],
    indexes: &[OdbcIndex],
    engine: &str,
    no_backslash_escapes: bool,
) -> Result<String, String> {
    super::super::validate_odbc_engine(engine)?;
    let fields = create_fields(columns, definitions, indexes, no_backslash_escapes)?;
    Ok(format!(
        "CREATE TABLE {} (\n  {}\n) ENGINE={engine} DEFAULT CHARSET=utf8mb4",
        object.qualified_name(),
        fields.join(",\n  "),
    ))
}

pub fn validate_odbc_create_table_sql(object: &DbObject, sql: &str) -> Result<(), String> {
    create_statement(sql, object, "")
        .or_else(|_| create_statement(sql, object, "NO_BACKSLASH_ESCAPES"))
        .or_else(|_| create_statement(sql, object, "ANSI_QUOTES"))
        .or_else(|_| create_statement(sql, object, "ANSI_QUOTES,NO_BACKSLASH_ESCAPES"))?;
    Ok(())
}

pub(super) fn staged_create_sql(
    table: &OdbcTarget,
    stage: &DbObject,
    sql_mode: &str,
) -> Result<String, String> {
    if let Some(sql) = &table.create_sql {
        let (before, after) = create_statement(sql, &table.object, sql_mode)?;
        if let Some(engine) = table.engine.as_deref()
            && super::supports_staged_nontransactional_engine(engine)
            && definition_tokens(after, sql_mode).any(|token| word(&token, "REFERENCES"))
        {
            return Err(format!(
                "引擎 {engine} 无法使用外键，请修改建表 SQL 或选择支持外键的引擎。"
            ));
        }
        return Ok(format!("{before}{}{after}", stage.qualified_name()));
    }
    generate(
        stage,
        &table.columns,
        table.create.as_deref().unwrap_or_default(),
        &table.indexes,
        table.staged_engine().unwrap_or("InnoDB"),
        sql_mode
            .split(',')
            .any(|mode| mode == "NO_BACKSLASH_ESCAPES"),
    )
}

fn create_statement<'a>(
    sql: &'a str,
    object: &DbObject,
    sql_mode: &str,
) -> Result<(&'a str, &'a str), String> {
    let invalid = || "请填写一条包含列定义的 CREATE TABLE 语句。".to_owned();
    let mut tokens = Vec::new();
    for token in definition_tokens(sql, sql_mode) {
        if token.text.starts_with("/*!")
            || token
                .text
                .get(..4)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("/*M!"))
        {
            return Err("建表 SQL 不支持可执行注释。".into());
        }
        match token.kind {
            EditorTokenKind::Protected { closed }
                if token.text.starts_with('#')
                    || token.text.starts_with("--")
                    || closed && token.text.starts_with("/*") =>
            {
                continue;
            }
            EditorTokenKind::Identifier { closed: false, .. }
            | EditorTokenKind::Protected { closed: false } => return Err(invalid()),
            _ => tokens.push(token),
        }
    }
    let mut end = sql.len();
    if tokens.last().is_some_and(|token| symbol(token, ';')) {
        end = tokens.pop().unwrap().range.start;
    }
    let [create, table, first, rest @ ..] = tokens.as_slice() else {
        return Err(invalid());
    };
    if !word(create, "CREATE") || !word(table, "TABLE") {
        return Err(invalid());
    }
    let (schema, name, body) = match rest {
        [dot, name, body @ ..] if symbol(dot, '.') => (Some(first), name, body),
        _ => (None, first, rest),
    };
    if schema.is_some_and(|schema| identifier(schema).as_deref() != Some(&object.schema))
        || identifier(name).as_deref() != Some(&object.name)
    {
        return Err(format!(
            "建表 SQL 的目标表应为 {}。",
            object.qualified_name()
        ));
    }
    if !body.first().is_some_and(|token| symbol(token, '(')) {
        return Err(invalid());
    }
    let mut depth = 0_u32;
    for token in body {
        match token.kind {
            EditorTokenKind::Symbol('(') => depth += 1,
            EditorTokenKind::Symbol(')') => depth = depth.checked_sub(1).ok_or_else(invalid)?,
            EditorTokenKind::Symbol(';') => return Err(invalid()),
            EditorTokenKind::Word if depth == 0 && word(token, "SELECT") => return Err(invalid()),
            _ => {}
        }
    }
    if depth != 0 {
        return Err(invalid());
    }
    Ok((&sql[..first.range.start], &sql[name.range.end..end]))
}

fn identifier(token: &EditorToken<'_>) -> Option<String> {
    match token.kind {
        EditorTokenKind::Word => Some(token.text.to_owned()),
        EditorTokenKind::Identifier {
            quote,
            closed: true,
        } => {
            let close = if quote == b'[' {
                ']'
            } else {
                char::from(quote)
            };
            let escaped = format!("{close}{close}");
            Some(token.text[1..token.text.len() - 1].replace(&escaped, &close.to_string()))
        }
        _ => None,
    }
}

fn word(token: &EditorToken<'_>, expected: &str) -> bool {
    matches!(token.kind, EditorTokenKind::Word) && token.text.eq_ignore_ascii_case(expected)
}

fn symbol(token: &EditorToken<'_>, expected: char) -> bool {
    matches!(token.kind, EditorTokenKind::Symbol(value) if value == expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::ObjectKind;

    #[test]
    fn keeps_edited_definition_when_binding_to_staging_table() {
        let target = DbObject {
            schema: "导入测试".into(),
            name: "目录`表".into(),
            kind: ObjectKind::Table,
        };
        let sql = "/* 两列 */ CREATE TABLE `导入测试`.`目录``表` (id INT PRIMARY KEY, name VARCHAR(30)) COMMENT='索引;标题';";
        let (before, after) = create_statement(sql, &target, "").unwrap();
        assert_eq!(
            format!("{before}`导入测试`.`stage`{after}"),
            "/* 两列 */ CREATE TABLE `导入测试`.`stage` (id INT PRIMARY KEY, name VARCHAR(30)) COMMENT='索引;标题'"
        );
        let generated = odbc_create_table_sql(
            &target,
            &["id".into(), "name".into()],
            &[
                OdbcColumnDefinition {
                    sql_type: "INT".into(),
                    primary: true,
                    ..Default::default()
                },
                OdbcColumnDefinition {
                    sql_type: "VARCHAR(30)".into(),
                    ..Default::default()
                },
            ],
            &[],
            Some("MyISAM"),
        )
        .unwrap();
        assert!(generated.contains("ENGINE=MyISAM"));
        assert!(validate_odbc_create_table_sql(&target, &generated).is_ok());
    }
}
