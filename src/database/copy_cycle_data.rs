use std::collections::{BTreeMap, BTreeSet};

use crate::sql_tools::mysql_text_literal;

use super::{copy_restore::comparable_sql, quote_identifier};

const UNKNOWN_TRIGGERS: u64 = u64::MAX;
const TABLE_TYPES: &str = "TABLE_TYPE IN ('BASE TABLE', 'SYSTEM VERSIONED')";

pub(super) struct CycleDataPlan {
    pub(super) locks: Vec<String>,
    pub(super) metadata_guards: Vec<String>,
    pub(super) validations: Vec<(String, String)>,
}

pub(super) fn inspection_sql(
    schema: &str,
    tables: &[String],
    allow_global_trigger: bool,
) -> [String; 2] {
    let selected = table_scope(schema, tables);
    let matches_schema = format!(
        "{} = {}",
        comparable_sql("g.EVENT_OBJECT_SCHEMA"),
        comparable_sql("t.TABLE_SCHEMA")
    );
    let matches_table = format!(
        "{} = {}",
        comparable_sql("g.EVENT_OBJECT_TABLE"),
        comparable_sql("t.TABLE_NAME")
    );
    let visibility = trigger_visibility(allow_global_trigger);
    [
        format!(
            "SELECT BINARY t.TABLE_NAME AS table_name, COALESCE(t.ENGINE, '') AS table_engine, \
             CAST(IF({visibility}, (SELECT COUNT(*) FROM information_schema.TRIGGERS g \
               WHERE {matches_schema} AND {matches_table} AND g.EVENT_MANIPULATION = 'INSERT'), \
               {UNKNOWN_TRIGGERS}) AS UNSIGNED) AS insert_triggers \
             FROM information_schema.TABLES t WHERE {TABLE_TYPES} AND {selected}"
        ),
        format!(
            "SELECT DISTINCT BINARY TABLE_NAME AS child_table, BINARY CONSTRAINT_NAME AS constraint_name, \
             BINARY COLUMN_NAME AS child_column, ORDINAL_POSITION AS key_position, \
             BINARY REFERENCED_TABLE_SCHEMA AS parent_schema, BINARY REFERENCED_TABLE_NAME AS parent_table, \
             BINARY REFERENCED_COLUMN_NAME AS parent_column \
             FROM information_schema.KEY_COLUMN_USAGE WHERE REFERENCED_TABLE_NAME IS NOT NULL AND {selected}"
        ),
    ]
}

fn table_scope(schema: &str, tables: &[String]) -> String {
    if tables.is_empty() {
        return "0".into();
    }
    let schema = comparable_sql(&mysql_text_literal(schema));
    let table_names = tables
        .iter()
        .map(|table| comparable_sql(&mysql_text_literal(table)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{} = {schema} AND {} IN ({table_names})",
        comparable_sql("TABLE_SCHEMA"),
        comparable_sql("TABLE_NAME")
    )
}

fn trigger_visibility(allow_global_trigger: bool) -> String {
    // GRANTEE holds quoted user/host parts. Split CURRENT_USER at the last @
    // so ordinary account names containing @ do not change the matched account.
    let grantee = "CONCAT(QUOTE(LEFT(CURRENT_USER(), CHAR_LENGTH(CURRENT_USER()) \
        - CHAR_LENGTH(SUBSTRING_INDEX(CURRENT_USER(), '@', -1)) - 1)), '@', \
        QUOTE(SUBSTRING_INDEX(CURRENT_USER(), '@', -1)))";
    let grant = format!("BINARY v.GRANTEE = BINARY {grantee} AND v.PRIVILEGE_TYPE = 'TRIGGER'");
    let schema = format!(
        "{} = {}",
        comparable_sql("v.TABLE_SCHEMA"),
        comparable_sql("t.TABLE_SCHEMA")
    );
    let table = format!(
        "{} = {}",
        comparable_sql("v.TABLE_NAME"),
        comparable_sql("t.TABLE_NAME")
    );
    let scoped = format!(
        "EXISTS(SELECT 1 FROM information_schema.SCHEMA_PRIVILEGES v WHERE {grant} AND {schema}) OR \
         EXISTS(SELECT 1 FROM information_schema.TABLE_PRIVILEGES v WHERE {grant} AND {schema} AND {table})"
    );
    if allow_global_trigger {
        format!(
            "EXISTS(SELECT 1 FROM information_schema.USER_PRIVILEGES v WHERE {grant}) OR {scoped}"
        )
    } else {
        scoped
    }
}

struct ForeignKey {
    parent: (String, String),
    columns: Vec<(u32, String, String)>,
}

pub(super) fn plan(
    schema: &str,
    table_count: usize,
    tables: Vec<(String, String, u64)>,
    keys: Vec<(String, String, String, u32, String, String, String)>,
    allow_global_trigger: bool,
) -> Result<CycleDataPlan, String> {
    let mut selected = BTreeSet::new();
    for (table, engine, triggers) in tables {
        let name = qualified(schema, &table);
        if !engine.eq_ignore_ascii_case("InnoDB") {
            return Err(format!(
                "循环外键的仅数据复制需要 InnoDB，目标表 {name} 的存储引擎为 {engine}。"
            ));
        }
        if triggers == UNKNOWN_TRIGGERS {
            return Err(format!(
                "无法核实目标表 {name} 的插入触发器。请使用对该表直接拥有 TRIGGER 权限的账号。"
            ));
        }
        if triggers > 0 {
            return Err(format!(
                "目标表 {name} 有插入触发器，无法保证循环外键复制失败时能完整回滚。"
            ));
        }
        selected.insert(table);
    }
    if table_count == 0 || selected.len() != table_count {
        return Err("目标库中可用的表与本次复制范围不符。请核对目标表名称和权限。".into());
    }

    let mut foreign_keys = BTreeMap::new();
    let mut expected_keys = Vec::with_capacity(keys.len());
    let mut parents = BTreeSet::new();
    let keys_error = "目标库的外键列映射不完整，请核对目标表定义。";
    for (child, name, column, position, parent_schema, parent_table, parent_column) in keys {
        if !selected.contains(&child) {
            return Err(keys_error.into());
        }
        expected_keys.push(format!(
            "({}, {}, {}, {position}, {}, {}, {})",
            binary_literal(&child),
            binary_literal(&name),
            binary_literal(&column),
            binary_literal(&parent_schema),
            binary_literal(&parent_table),
            binary_literal(&parent_column),
        ));
        let parent = (parent_schema, parent_table);
        parents.insert(parent.clone());
        let key = foreign_keys
            .entry((child, name))
            .or_insert_with(|| ForeignKey {
                parent: parent.clone(),
                columns: Vec::new(),
            });
        if key.parent != parent {
            return Err(keys_error.into());
        }
        key.columns.push((position, column, parent_column));
    }

    let actual_tables: Vec<_> = selected.iter().cloned().collect();
    let [table_sql, key_sql] = inspection_sql(schema, &actual_tables, allow_global_trigger);
    let table_names = actual_tables
        .iter()
        .map(|table| binary_literal(table))
        .collect::<Vec<_>>()
        .join(", ");
    let mut metadata_guards = vec![format!(
        "(SELECT COUNT(*) = {table_count} AND COUNT(CASE WHEN table_name IN ({table_names}) \
          AND BINARY UPPER(table_engine) = 'INNODB' AND insert_triggers = 0 THEN 1 END) = {table_count} \
          FROM ({table_sql}) AS copy_tables)"
    )];
    let key_count = expected_keys.len();
    let key_match = if expected_keys.is_empty() {
        String::new()
    } else {
        format!(
            " AND COUNT(CASE WHEN (child_table, constraint_name, child_column, key_position, \
             parent_schema, parent_table, parent_column) IN ({}) THEN 1 END) = {key_count}",
            expected_keys.join(", ")
        )
    };
    metadata_guards.push(format!(
        "(SELECT COUNT(*) = {key_count}{key_match} FROM ({key_sql}) AS copy_keys)"
    ));
    for (parent_schema, parent) in &parents {
        let selected = table_scope(parent_schema, std::slice::from_ref(parent));
        metadata_guards.push(format!(
            "EXISTS(SELECT 1 FROM information_schema.TABLES WHERE {TABLE_TYPES} \
             AND BINARY UPPER(ENGINE) = 'INNODB' AND {selected})"
        ));
    }

    let locks = actual_tables
        .into_iter()
        .map(|table| (schema.to_owned(), table))
        .chain(parents)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|(database, table)| {
            format!(
                "SELECT 1 FROM {} LIMIT 0 LOCK IN SHARE MODE",
                qualified(&database, &table)
            )
        })
        .collect();
    let mut validations = Vec::with_capacity(foreign_keys.len());
    for ((child, name), mut key) in foreign_keys {
        key.columns.sort_unstable_by_key(|part| part.0);
        if key
            .columns
            .iter()
            .enumerate()
            .any(|(index, part)| part.0 as usize != index + 1)
        {
            return Err(keys_error.into());
        }
        let child = qualified(schema, &child);
        let label = format!("{child} 的外键 {}", quote_identifier(&name));
        validations.push((label, no_orphans(&child, &key)));
    }
    Ok(CycleDataPlan {
        locks,
        metadata_guards,
        validations,
    })
}

fn no_orphans(child: &str, key: &ForeignKey) -> String {
    let parent = qualified(&key.parent.0, &key.parent.1);
    let included = key
        .columns
        .iter()
        .map(|(_, column, _)| format!("c.{} IS NOT NULL", quote_identifier(column)))
        .collect::<Vec<_>>()
        .join(" AND ");
    let matches = key
        .columns
        .iter()
        .map(|(_, column, parent_column)| {
            format!(
                "p.{} = c.{}",
                quote_identifier(parent_column),
                quote_identifier(column)
            )
        })
        .collect::<Vec<_>>()
        .join(" AND ");
    // Lock current child rows and lock matching parent rows inside the nested read.
    // An outer locking clause alone does not hold the parent rows through commit.
    format!(
        "NOT EXISTS(SELECT 1 FROM {child} AS c WHERE {included} AND NOT EXISTS( \
         SELECT 1 FROM {parent} AS p WHERE {matches} LIMIT 1 LOCK IN SHARE MODE) \
         LIMIT 1 LOCK IN SHARE MODE)"
    )
}

fn qualified(schema: &str, table: &str) -> String {
    format!("{}.{}", quote_identifier(schema), quote_identifier(table))
}

fn binary_literal(value: &str) -> String {
    format!("BINARY {}", mysql_text_literal(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_data_plan_locks_and_validates_a_composite_reference() {
        let plan = plan(
            "archive",
            1,
            vec![("orders".into(), "InnoDB".into(), 0)],
            vec![
                (
                    "orders".into(),
                    "account_fk".into(),
                    "account_id".into(),
                    2,
                    "accounts".into(),
                    "customer".into(),
                    "id".into(),
                ),
                (
                    "orders".into(),
                    "account_fk".into(),
                    "region".into(),
                    1,
                    "accounts".into(),
                    "customer".into(),
                    "region".into(),
                ),
            ],
            true,
        )
        .unwrap();
        assert_eq!(
            plan.locks,
            [
                "SELECT 1 FROM `accounts`.`customer` LIMIT 0 LOCK IN SHARE MODE",
                "SELECT 1 FROM `archive`.`orders` LIMIT 0 LOCK IN SHARE MODE",
            ]
        );
        assert_eq!(plan.metadata_guards.len(), 3);
        assert!(plan.metadata_guards[0].contains("insert_triggers = 0"));
        assert_eq!(plan.validations, [(
            "`archive`.`orders` 的外键 `account_fk`".into(),
            "NOT EXISTS(SELECT 1 FROM `archive`.`orders` AS c WHERE c.`region` IS NOT NULL AND c.`account_id` IS NOT NULL AND NOT EXISTS( SELECT 1 FROM `accounts`.`customer` AS p WHERE p.`region` = c.`region` AND p.`id` = c.`account_id` LIMIT 1 LOCK IN SHARE MODE) LIMIT 1 LOCK IN SHARE MODE)".into(),
        )]);
    }
}
