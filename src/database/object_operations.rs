use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::sql_tools::mysql_text_literal;

use super::{Cell, DbObject, DbResult, Failure, MAX_RESULT_ROWS, ObjectKind, SchemaDefaults};

pub(super) type MetadataRows = Vec<Vec<Option<String>>>;

pub(super) fn native_rows(conn: &mut mysql::Conn, sql: &str) -> DbResult<MetadataRows> {
    use mysql::prelude::Queryable;

    let mut rows = Vec::new();
    for row in conn.query_iter(sql)? {
        if rows.len() == MAX_RESULT_ROWS {
            return Err(Failure::Message("元数据超过读取上限。".into()));
        }
        let mut row = row?;
        let fields = (0..row.len())
            .map(|index| {
                row.take_opt::<Option<String>, _>(index)
                    .expect("column index in range")
                    .map_err(mysql::Error::from)
            })
            .collect::<Result<_, _>>()?;
        rows.push(fields);
    }
    Ok(rows)
}

pub(super) fn http_rows(rows: Vec<Vec<Cell>>) -> DbResult<MetadataRows> {
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| match cell {
                    Cell::Null => Ok(None),
                    Cell::Text(value) => Ok(Some(value)),
                    Cell::Binary(bytes) => String::from_utf8(bytes)
                        .map(Some)
                        .map_err(|_| Failure::Message("隧道返回了无法解码的元数据。".into())),
                })
                .collect()
        })
        .collect()
}

pub(super) fn native_has_global_visibility(conn: &mut mysql::Conn) -> DbResult<bool> {
    use mysql::prelude::Queryable;

    // A server can hide keys belonging to tables the current account cannot see.
    // Only suspend FK enforcement when global metadata visibility can be established.
    let grants = conn.query::<String, _>("SHOW GRANTS")?;
    Ok(grants_allow_global_select(&grants))
}

pub(super) fn grants_allow_global_select(grants: &[String]) -> bool {
    if grants.iter().any(|grant| grant.starts_with("REVOKE ")) {
        return false;
    }
    grants
        .iter()
        .filter_map(|grant| grant.strip_prefix("GRANT "))
        .filter_map(|grant| grant.split_once(" ON "))
        .any(|(privileges, scope)| {
            scope.starts_with("*.* TO ")
                && (privileges == "ALL PRIVILEGES"
                    || privileges
                        .split(',')
                        .any(|privilege| privilege.trim() == "SELECT"))
        })
}

fn fields<const N: usize>(row: &[Option<String>]) -> DbResult<[&str; N]> {
    let mut names = [""; N];
    for (index, name) in names.iter_mut().enumerate() {
        *name = row
            .get(index)
            .and_then(Option::as_deref)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Failure::Message("服务器返回的元数据缺少必要字段。".into()))?;
    }
    Ok(names)
}

fn matches_schema(column: &str, schema: &str) -> String {
    let value = mysql_text_literal(schema);
    format!(
        "{column} = {value} AND BINARY IF(@@lower_case_table_names = 0, {column}, LOWER({column}))
            = BINARY IF(@@lower_case_table_names = 0, {value}, LOWER({value}))"
    )
}

#[derive(Clone, Debug)]
pub struct DatabaseDefaults {
    pub charset: String,
    pub collation: String,
    pub charsets: BTreeMap<String, CharacterSet>,
}

#[derive(Clone, Debug)]
pub struct CharacterSet {
    pub default_collation: String,
    pub collations: BTreeSet<String>,
}

pub(super) fn read_current_defaults(
    schema: Option<&str>,
    read: impl FnOnce(&str) -> DbResult<MetadataRows>,
) -> DbResult<SchemaDefaults> {
    let current_sql = match schema {
        Some(schema) => format!(
            "SELECT DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME
             FROM information_schema.SCHEMATA WHERE {}",
            matches_schema("SCHEMA_NAME", schema)
        ),
        None => "SELECT @@character_set_server, @@collation_server".into(),
    };
    let current = read(&current_sql)?;
    let [charset, collation] = fields(
        current
            .first()
            .ok_or_else(|| Failure::Message("未找到数据库或无权读取它的默认设置。".into()))?,
    )?;
    Ok(SchemaDefaults {
        charset: charset.to_owned(),
        collation: collation.to_owned(),
    })
}

pub(super) fn read_defaults(
    schema: Option<&str>,
    mut read: impl FnMut(&str) -> DbResult<MetadataRows>,
) -> DbResult<DatabaseDefaults> {
    let SchemaDefaults { charset, collation } = read_current_defaults(schema, &mut read)?;
    let mut charsets = BTreeMap::new();
    const CHARSETS: &str = "SELECT CHARACTER_SET_NAME, DEFAULT_COLLATE_NAME
        FROM information_schema.CHARACTER_SETS";
    for row in read(CHARSETS)? {
        let [name, default] = fields(&row)?;
        charsets.insert(
            name.to_owned(),
            CharacterSet {
                default_collation: default.to_owned(),
                collations: BTreeSet::from([default.to_owned()]),
            },
        );
    }
    // A MariaDB collation can apply to several character sets. The applicability
    // table gives the character set for each combination, including such aliases.
    const COLLATIONS: &str = "SELECT CHARACTER_SET_NAME, COLLATION_NAME
        FROM information_schema.COLLATION_CHARACTER_SET_APPLICABILITY";
    for row in read(COLLATIONS)? {
        let [name, value] = fields(&row)?;
        if let Some(options) = charsets.get_mut(name) {
            options.collations.insert(value.into());
        }
    }
    charsets
        .entry(charset.clone())
        .or_insert_with(|| CharacterSet {
            default_collation: collation.clone(),
            collations: BTreeSet::new(),
        })
        .collations
        .insert(collation.clone());
    Ok(DatabaseDefaults {
        charset,
        collation,
        charsets,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatabaseContentsAction {
    Drop,
    Truncate,
}

impl DatabaseContentsAction {
    pub fn includes(self, kind: ObjectKind) -> bool {
        self == Self::Drop || kind == ObjectKind::Table
    }
}

#[derive(Debug)]
pub struct DatabaseContents {
    pub schema: String,
    pub objects: Vec<DbObject>,
    triggers: Vec<(DbObject, DbObject)>,
    foreign_keys: Vec<(DbObject, DbObject)>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct DatabaseContentsPlan {
    schema: String,
    action: DatabaseContentsAction,
    ordered: Vec<DbObject>,
    pub suspend_foreign_key_checks: bool,
}

impl DatabaseContentsPlan {
    pub fn object_count(&self) -> usize {
        self.ordered.len()
    }

    pub fn statements(&self) -> impl Iterator<Item = String> + '_ {
        self.ordered.iter().map(|object| {
            let clause = match (self.action, object.kind) {
                (DatabaseContentsAction::Truncate, _) => "TRUNCATE TABLE",
                (DatabaseContentsAction::Drop, ObjectKind::Table) => "DROP TABLE",
                (DatabaseContentsAction::Drop, ObjectKind::View) => "DROP VIEW",
                (DatabaseContentsAction::Drop, ObjectKind::Procedure) => "DROP PROCEDURE",
                (DatabaseContentsAction::Drop, ObjectKind::Function) => "DROP FUNCTION",
                (DatabaseContentsAction::Drop, ObjectKind::Trigger) => "DROP TRIGGER",
                (DatabaseContentsAction::Drop, ObjectKind::Event) => "DROP EVENT",
            };
            format!("{clause} {}", object.qualified_name())
        })
    }

    pub(super) fn schema(&self) -> &str {
        &self.schema
    }

    pub(super) fn action(&self) -> DatabaseContentsAction {
        self.action
    }

    pub(super) fn verify(&self, current: &DatabaseContents) -> Result<(), String> {
        let selected = self.ordered.iter().cloned().collect();
        let updated = current.plan(self.action, &selected)?;
        if updated != *self {
            return Err("数据库依赖已经变化。请重新读取对象并核对操作范围。".into());
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct DatabaseContentsResult {
    pub schema: String,
    pub completed: usize,
    pub total: usize,
    pub error: Option<String>,
    pub objects: Option<Vec<DbObject>>,
}

pub(super) fn read_contents(
    schema: String,
    objects: Vec<DbObject>,
    action: DatabaseContentsAction,
    mut read: impl FnMut(&str) -> DbResult<MetadataRows>,
) -> DbResult<DatabaseContents> {
    let case_mode = read("SELECT CAST(@@lower_case_table_names AS CHAR)")?;
    let [case_mode] = fields(
        case_mode
            .first()
            .ok_or_else(|| Failure::Message("服务器未返回表名大小写设置。".into()))?,
    )?;
    let key = |name: &str| {
        if case_mode == "0" {
            name.to_owned()
        } else {
            name.to_lowercase()
        }
    };
    let tables = objects
        .iter()
        .filter(|object| object.kind == ObjectKind::Table)
        .map(|object| ((key(&object.schema), key(&object.name)), object))
        .collect::<HashMap<_, _>>();
    let table = |schema: &str, name: &str| {
        tables
            .get(&(key(schema), key(name)))
            .map(|&table| table.clone())
            .unwrap_or_else(|| object(schema, name, ObjectKind::Table))
    };
    let mut triggers = Vec::new();
    if action == DatabaseContentsAction::Drop {
        let query = format!(
            "SELECT TRIGGER_SCHEMA, TRIGGER_NAME, EVENT_OBJECT_SCHEMA, EVENT_OBJECT_TABLE
             FROM information_schema.TRIGGERS WHERE {} ORDER BY BINARY TRIGGER_NAME",
            matches_schema("TRIGGER_SCHEMA", &schema)
        );
        for row in read(&query)? {
            let [trigger_schema, trigger_name, table_schema, table_name] = fields(&row)?;
            triggers.push((
                object(trigger_schema, trigger_name, ObjectKind::Trigger),
                table(table_schema, table_name),
            ));
        }
    }
    let query = format!(
        "SELECT DISTINCT CAST(TABLE_SCHEMA AS BINARY) AS child_schema,
             CAST(TABLE_NAME AS BINARY) AS child_table,
             CAST(REFERENCED_TABLE_SCHEMA AS BINARY) AS parent_schema,
             CAST(REFERENCED_TABLE_NAME AS BINARY) AS parent_table
         FROM information_schema.KEY_COLUMN_USAGE WHERE {}
         ORDER BY child_schema, child_table, parent_schema, parent_table",
        matches_schema("REFERENCED_TABLE_SCHEMA", &schema)
    );
    let mut foreign_keys = Vec::new();
    for row in read(&query)? {
        let [child_schema, child, parent_schema, parent] = fields(&row)?;
        foreign_keys.push((table(child_schema, child), table(parent_schema, parent)));
    }
    Ok(DatabaseContents {
        schema,
        objects,
        triggers,
        foreign_keys,
    })
}

fn object(schema: &str, name: &str, kind: ObjectKind) -> DbObject {
    DbObject {
        schema: schema.into(),
        name: name.into(),
        kind,
    }
}

impl DatabaseContents {
    pub fn plan(
        &self,
        action: DatabaseContentsAction,
        selected: &HashSet<DbObject>,
    ) -> Result<DatabaseContentsPlan, String> {
        if selected.is_empty() {
            return Err("请至少选择一个对象。".into());
        }
        let mut objects = self
            .objects
            .iter()
            .filter(|object| action.includes(object.kind) && selected.contains(*object))
            .collect::<Vec<_>>();
        if objects.len() != selected.len() {
            return Err("有选中的对象已无法处理。请重新读取数据库对象。".into());
        }
        // A trigger disappears with its table. Require the user to select it too.
        if action == DatabaseContentsAction::Drop {
            for (trigger, table) in &self.triggers {
                if selected.contains(table) && !selected.contains(trigger) {
                    return Err(format!(
                        "删除 {} 也会删除关联触发器 {}。请同时选择该触发器。",
                        table.qualified_name(),
                        trigger.qualified_name()
                    ));
                }
            }
        }
        let mut table_dependencies = Vec::new();
        for (child, parent) in &self.foreign_keys {
            if child == parent || !selected.contains(parent) {
                continue;
            }
            if !selected.contains(child) {
                return Err(format!(
                    "{} 的外键引用 {}。请保留被引用的表，或先处理这个外键。",
                    child.qualified_name(),
                    parent.qualified_name()
                ));
            }
            table_dependencies.push((parent, child));
        }
        let suspend_foreign_key_checks =
            action == DatabaseContentsAction::Truncate && !table_dependencies.is_empty();
        // Standalone objects are processed before tables. Referencing tables come
        // before the tables they reference, even when the server lists them last.
        objects.sort_by_key(|object| (object.kind == ObjectKind::Table, object.kind));
        let (order, resolved) = super::transfer::dependency_order(&objects, table_dependencies);
        if resolved != objects.len() {
            return Err("所选表之间存在循环外键。请先处理这些外键。".into());
        }
        Ok(DatabaseContentsPlan {
            schema: self.schema.clone(),
            action,
            ordered: order
                .into_iter()
                .map(|index| objects[index].clone())
                .collect(),
            suspend_foreign_key_checks,
        })
    }
}

pub(super) fn run_contents(
    plan: DatabaseContentsPlan,
    mut execute: impl FnMut(&str) -> Result<(), String>,
) -> DatabaseContentsResult {
    let mut result = DatabaseContentsResult {
        schema: plan.schema.clone(),
        completed: 0,
        total: plan.object_count(),
        error: None,
        objects: None,
    };
    for (object, sql) in plan.ordered.iter().zip(plan.statements()) {
        match execute(&sql) {
            Ok(()) => result.completed += 1,
            Err(error) => {
                result.error = Some(format!("处理 {} 时停止。{error}", object.qualified_name()));
                break;
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::database::{Command, ConnectionConfig, Response, Worker, quote_identifier};

    #[test]
    #[ignore = "在一次性本机 MariaDB 中运行，需要 HARBOR_OBJECTS_TEST_PORT"]
    fn edits_defaults_and_changes_only_confirmed_objects() {
        let port = std::env::var("HARBOR_OBJECTS_TEST_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let schema = format!("harbor_objects_{suffix:x}");
        let db = quote_identifier(&schema);
        let worker = Worker::new();
        let mut id = 0;
        let mut request = |command| -> Result<Response, String> {
            id += 1;
            worker.send(id, command)?;
            worker.recv()?.result
        };
        request(Command::Connect(ConnectionConfig {
            port,
            ..Default::default()
        }))
        .unwrap();
        let query = |sql: String, request: &mut dyn FnMut(Command) -> Result<Response, String>| {
            let Response::Queried(report) = request(Command::Query { sql, limit: 20 })? else {
                return Err("没有返回查询结果。".into());
            };
            if let Some(error) = report.error {
                return Err(error);
            }
            Ok(report)
        };
        let result = (|| -> Result<_, String> {
            let Response::DatabaseDefaults(server) =
                request(Command::ReadDatabaseDefaults { schema: None })?
            else {
                return Err("没有返回服务器的字符集。".into());
            };
            let default = server.charsets.get("utf8mb4").unwrap();
            assert!(default.collations.contains("utf8mb4_bin"));
            query(
                format!(
                    "CREATE DATABASE {db} CHARACTER SET utf8mb4 COLLATE utf8mb4_bin;
                     ALTER DATABASE {db} CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci;
                     CREATE TABLE {db}.parents (id INT PRIMARY KEY) ENGINE=InnoDB;
                     CREATE TABLE {db}.children (id INT PRIMARY KEY, parent_id INT, FOREIGN KEY (parent_id) REFERENCES {db}.parents(id)) ENGINE=InnoDB;
                     CREATE TABLE {db}.kept (id INT PRIMARY KEY) ENGINE=InnoDB;
                     CREATE TRIGGER {db}.stamp BEFORE INSERT ON {db}.children FOR EACH ROW SET @stamp = NEW.id;
                     INSERT INTO {db}.parents VALUES (1);
                     INSERT INTO {db}.children VALUES (1, 1);
                     INSERT INTO {db}.kept VALUES (1);"
                ),
                &mut request,
            )?;
            let Response::DatabaseDefaults(defaults) = request(Command::ReadDatabaseDefaults {
                schema: Some(schema.clone()),
            })?
            else {
                return Err("没有返回数据库的默认设置。".into());
            };
            let selected = [
                ("parents", ObjectKind::Table),
                ("children", ObjectKind::Table),
            ]
            .map(|(name, kind)| object(&schema, name, kind));
            let action = DatabaseContentsAction::Truncate;
            let Response::DatabaseContents(contents) = request(Command::ReadDatabaseContents {
                schema: schema.clone(),
                action,
            })?
            else {
                return Err("没有返回待截断的表。".into());
            };
            let plan = contents.plan(action, &selected.iter().cloned().collect())?;
            let ordered = plan.statements().collect::<Vec<_>>();
            let Response::DatabaseContentsApplied(truncated) =
                request(Command::ApplyDatabaseContents { plan })?
            else {
                return Err("没有返回截断结果。".into());
            };
            let counts = query(
                format!(
                    "SELECT (SELECT COUNT(*) FROM {db}.parents),
                    (SELECT COUNT(*) FROM {db}.children), (SELECT COUNT(*) FROM {db}.kept),
                    @@SESSION.foreign_key_checks"
                ),
                &mut request,
            )?;
            let action = DatabaseContentsAction::Drop;
            let Response::DatabaseContents(contents) = request(Command::ReadDatabaseContents {
                schema: schema.clone(),
                action,
            })?
            else {
                return Err("没有返回待清空的对象。".into());
            };
            let plan = contents.plan(
                action,
                &HashSet::from([
                    object(&schema, "children", ObjectKind::Table),
                    object(&schema, "stamp", ObjectKind::Trigger),
                ]),
            )?;
            let Response::DatabaseContentsApplied(removed) =
                request(Command::ApplyDatabaseContents { plan })?
            else {
                return Err("没有返回删除对象结果。".into());
            };
            Ok((defaults, ordered, truncated, counts, removed))
        })();
        query(format!("DROP DATABASE IF EXISTS {db}"), &mut request).unwrap();

        let (defaults, ordered, truncated, counts, removed) = result.unwrap();
        assert_eq!(defaults.collation, "utf8mb4_general_ci");
        assert!(ordered[0].ends_with(".`children`"));
        assert_eq!(truncated.error, None);
        assert_eq!(truncated.completed, 2);
        assert_eq!(
            counts.sets[0].rows[0],
            ["0", "0", "1", "1"].map(|value| Cell::Text(value.into()))
        );
        assert_eq!(removed.error, None);
        assert_eq!(removed.completed, 2);
        assert_eq!(
            removed
                .objects
                .unwrap()
                .into_iter()
                .map(|object| object.name)
                .collect::<Vec<_>>(),
            ["kept", "parents"]
        );
    }
}
