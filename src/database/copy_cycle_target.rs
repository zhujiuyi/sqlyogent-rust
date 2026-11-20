use std::collections::{BTreeMap, BTreeSet};

use crate::sql_tools::mysql_text_literal;

use super::{copy_restore::comparable_sql, quote_identifier};

pub(super) fn inbound_sql(schema: &str, tables: &[String]) -> String {
    let (parent, child) = selected_tables(schema, tables);
    // Each member of a composite foreign key has a metadata row. Keep the actual
    // target name and collapse those rows into one entry per constraint.
    format!(
        "SELECT DISTINCT BINARY TABLE_SCHEMA AS child_schema, BINARY TABLE_NAME AS child_table, \
         BINARY CONSTRAINT_NAME AS fk_name, IF({child}, 1, 0) AS child_selected \
         FROM information_schema.KEY_COLUMN_USAGE WHERE {parent} \
         ORDER BY child_schema, child_table, fk_name"
    )
}

pub(super) fn outside_inbound_condition(schema: &str, tables: &[String]) -> String {
    let (parent, child) = selected_tables(schema, tables);
    format!(
        "EXISTS(SELECT 1 FROM information_schema.KEY_COLUMN_USAGE WHERE {parent} AND NOT ({child}))"
    )
}

fn selected_tables(schema: &str, tables: &[String]) -> (String, String) {
    if tables.is_empty() {
        return ("0".into(), "0".into());
    }
    let schema = comparable_sql(&mysql_text_literal(schema));
    let names = tables
        .iter()
        .map(|table| comparable_sql(&mysql_text_literal(table)))
        .collect::<Vec<_>>()
        .join(", ");
    let matches = |schema_column, table_column| {
        let schema_column = comparable_sql(schema_column);
        let table_column = comparable_sql(table_column);
        format!("{schema_column} = {schema} AND {table_column} IN ({names})")
    };
    (
        matches("REFERENCED_TABLE_SCHEMA", "REFERENCED_TABLE_NAME"),
        matches("TABLE_SCHEMA", "TABLE_NAME"),
    )
}

pub(super) fn dropping_statements(
    rows: Vec<(String, String, String, bool)>,
) -> Result<Vec<String>, String> {
    let mut children = BTreeMap::<(String, String), BTreeSet<String>>::new();
    for (schema, table, constraint, selected) in rows {
        if !selected {
            return Err(format!(
                "目标表被本次范围之外的表 {}.{} 通过外键 {} 引用。请调整复制范围或取消覆盖。",
                quote_identifier(&schema),
                quote_identifier(&table),
                quote_identifier(&constraint),
            ));
        }
        children
            .entry((schema, table))
            .or_default()
            .insert(constraint);
    }
    Ok(children
        .into_iter()
        .map(|((schema, table), constraints)| {
            let clauses = constraints
                .into_iter()
                .map(|constraint| format!("DROP FOREIGN KEY {}", quote_identifier(&constraint)))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "ALTER TABLE {}.{} {clauses}",
                quote_identifier(&schema),
                quote_identifier(&table)
            )
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_foreign_key_drops_use_exact_destination_names() {
        let key = |table: &str, constraint: &str| {
            (
                "stock archive".into(),
                table.into(),
                constraint.into(),
                true,
            )
        };
        let sql = dropping_statements(vec![
            key("header", "back`to detail"),
            key("detail", "fk header"),
            key("detail", "fk`second"),
        ])
        .unwrap();
        assert_eq!(
            sql,
            [
                "ALTER TABLE `stock archive`.`detail` DROP FOREIGN KEY `fk header`, DROP FOREIGN KEY `fk``second`",
                "ALTER TABLE `stock archive`.`header` DROP FOREIGN KEY `back``to detail`",
            ]
        );
    }
}
