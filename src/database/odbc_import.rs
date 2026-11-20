use std::collections::{HashSet, hash_map::RandomState};
use std::hash::BuildHasher;
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

use mysql::prelude::Queryable;
use mysql::{Conn, Opts, Value};
use serde::{Deserialize, Serialize};

use super::odbc_source_action::SOURCE_TARGET_SAME_TABLE;
use super::{
    DbObject, IMPORT_MEMORY_LIMIT, IMPORT_UNCONFIRMED, OdbcImportCounts, OdbcIndex, OdbcReset,
    OdbcTarget, check_import_cancelled, import_transaction, mysql_error, quote_identifier,
    validate_import_rows,
};
use super::odbc_target::{supports_staged_nontransactional_engine, validate_odbc_engine};

mod foreign_keys;
mod http;
mod reset;
mod create_sql;
pub use create_sql::{odbc_create_table_sql, validate_odbc_create_table_sql};
use create_sql::staged_create_sql;
use foreign_keys::*;
pub(super) use http::{run_http, run_http_checked};
use reset::*;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OdbcColumnDefinition {
    pub sql_type: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub not_null: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub primary: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub unsigned: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub binary: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub auto_increment: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub zerofill: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl OdbcColumnDefinition {
    pub fn validate(&self) -> Result<(), String> {
        self.declaration().map(|_| ())
    }

    fn declaration(&self) -> Result<String, String> {
        let input = self.sql_type.trim().to_ascii_uppercase();
        let (base, size) = match input.split_once('(') {
            Some((base, size)) => (
                base.trim_end(),
                Some(
                    size.strip_suffix(')')
                        .ok_or("ODBC 目标列的类型参数无效。")?
                        .split(',')
                        .map(|part| {
                            part.trim()
                                .parse::<u16>()
                                .map_err(|_| "ODBC 目标列的类型参数须为整数。")
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                ),
            ),
            None => (input.as_str(), None),
        };
        let integer = matches!(
            base,
            "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "INTEGER" | "BIGINT"
        );
        let decimal = matches!(base, "DECIMAL" | "NUMERIC");
        let approximate = matches!(base, "FLOAT" | "DOUBLE");
        let string = matches!(
            base,
            "CHAR" | "VARCHAR" | "TINYTEXT" | "TEXT" | "MEDIUMTEXT" | "LONGTEXT"
        );
        let parameters = match (base, size.as_deref()) {
            ("VARCHAR", Some([1..=16383])) | ("CHAR", Some([1..=255])) => size,
            ("BINARY", Some([0..=255])) | ("VARBINARY", Some([_])) | ("BLOB", Some([_])) => size,
            ("CHAR" | "BINARY" | "TINYBLOB" | "BLOB" | "MEDIUMBLOB" | "LONGBLOB", None) => None,
            ("DECIMAL" | "NUMERIC", Some([precision @ 1..=65, scale @ 0..=30]))
                if scale <= precision =>
            {
                size
            }
            ("DECIMAL" | "NUMERIC", Some([1..=65]) | None) => size,
            ("DATETIME" | "TIME" | "TIMESTAMP", Some([0..=6]) | None) => size,
            ("YEAR", Some([4]) | None) => None,
            (
                "TINYTEXT" | "TEXT" | "MEDIUMTEXT" | "LONGTEXT" | "DATE" | "FLOAT" | "DOUBLE",
                None,
            ) => None,
            ("FLOAT" | "DOUBLE", Some([1..=255, 0..=30])) => size,
            ("BIT", Some([1..=64]) | None) => size,
            (_, Some([1..=255])) if integer => size.filter(|_| self.zerofill),
            (_, None) if integer => None,
            _ => return Err("ODBC 目标列的类型或类型参数暂不支持。".into()),
        };
        if (self.unsigned || self.zerofill) && !(integer || decimal || approximate)
            || self.binary && !string
            || self.auto_increment && (!integer || !self.primary || self.default.is_some())
            || self
                .default
                .as_ref()
                .is_some_and(|value| value.contains('\0'))
        {
            return Err("ODBC 目标列的类型与列属性不匹配。".into());
        }
        let mut sql = base.to_owned();
        if let Some(parameters) = parameters {
            sql.push('(');
            sql.push_str(
                &parameters
                    .into_iter()
                    .map(|size| size.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            sql.push(')');
        }
        if self.unsigned {
            sql.push_str(" UNSIGNED");
        }
        if self.zerofill {
            sql.push_str(" ZEROFILL");
        }
        if self.binary {
            sql.push_str(" BINARY");
        }
        Ok(sql)
    }

    fn sql(&self, name: &str, no_backslash_escapes: bool) -> Result<String, String> {
        let mut sql = format!("{} {}", quote_identifier(name), self.declaration()?);
        if self.not_null || self.primary {
            sql.push_str(" NOT NULL");
        }
        if let Some(default) = &self.default {
            sql.push_str(" DEFAULT ");
            sql.push_str(&Value::from(default.as_str()).as_sql(no_backslash_escapes));
        }
        if self.auto_increment {
            sql.push_str(" AUTO_INCREMENT");
        }
        Ok(sql)
    }
}

pub(super) fn run(
    opts: &Opts,
    tables: Vec<OdbcTarget>,
    cancel: &AtomicBool,
) -> Result<OdbcImportCounts, String> {
    run_checked(opts, tables, cancel, None)
}

pub(super) fn run_checked(
    opts: &Opts,
    mut tables: Vec<OdbcTarget>,
    cancel: &AtomicBool,
    source_guard: Option<&str>,
) -> Result<OdbcImportCounts, String> {
    let staged = validate_targets(&tables, cancel)?;
    let foreign_keys = plan_foreign_keys(&tables, cancel)?;
    let mut conn = Conn::new(opts.clone()).map_err(mysql_error)?;
    if let Some(guard) = source_guard {
        let safe: u8 = conn
            .query_first(format!("SELECT ({guard})"))
            .map_err(|error| format!("无法核对源表与目标表的身份。{}", mysql_error(error)))?
            .ok_or("数据库未返回源表与目标表的身份核对结果。")?;
        if safe != 1 {
            return Err(SOURCE_TARGET_SAME_TABLE.into());
        }
    }
    if !staged {
        return import_transaction(&mut conn, tables, Some(cancel));
    }

    let mode: String = conn
        .query_first("SELECT @@SESSION.sql_mode")
        .map_err(mysql_error)?
        .ok_or("无法读取目标数据库的 SQL 模式。")?;
    let has_mode = |name| mode.split(',').any(|part| part == name);
    if !has_mode("NO_AUTO_VALUE_ON_ZERO") {
        let mode = if mode.is_empty() {
            "NO_AUTO_VALUE_ON_ZERO".to_owned()
        } else {
            format!("{mode},NO_AUTO_VALUE_ON_ZERO")
        };
        conn.exec_drop("SET SESSION sql_mode = ?", (mode,))
            .map_err(mysql_error)?;
    }
    if tables.iter().any(|table| table.reset != OdbcReset::None) {
        conn.query_drop(
            "SET SESSION foreign_key_checks = 1,
                 SESSION lock_wait_timeout = LEAST(@@SESSION.lock_wait_timeout, 15)",
        )
        .map_err(mysql_error)?;
    }

    let token = RandomState::new().hash_one(SystemTime::now());
    let mut publication = Vec::with_capacity(tables.len());
    let mut reset_server = None;
    let result = (|| {
        for (index, table) in tables.iter_mut().enumerate() {
            check_import_cancelled(Some(cancel)).map_err(PublishFailure::Unpublished)?;
            let planned = plan_native_stage(&mut conn, table, token, index, &mut reset_server)
                .map_err(PublishFailure::Unpublished)?;
            let sql = if table.reset == OdbcReset::Truncate {
                format!(
                    "CREATE TABLE {} LIKE {}",
                    planned.stage.qualified_name(),
                    planned.destination.qualified_name(),
                )
            } else {
                staged_create_sql(table, &planned.stage, &mode)
                    .map_err(PublishFailure::Unpublished)?
            };
            create_native_stage(&mut conn, &planned, &sql).map_err(PublishFailure::Unpublished)?;
            publication.push(planned);
            let planned = publication.last().unwrap();
            check_create_warnings(&mut conn).map_err(PublishFailure::Unpublished)?;
            table.expected_ddl = if table.reset == OdbcReset::Truncate {
                let snapshot = planned.previous.as_ref().unwrap();
                let definition = native_definition(&mut conn, &planned.stage)
                    .map_err(PublishFailure::Unpublished)?;
                if reset_definition_body(definition.clone(), &planned.stage)
                    .map_err(PublishFailure::Unpublished)?
                    != snapshot.definition
                {
                    return Err(PublishFailure::Unpublished(format!(
                        "无法完整复制目标表 {} 的结构，原表未替换。",
                        planned.destination.qualified_name()
                    )));
                }
                Some(definition)
            } else {
                None
            };
            table.object = planned.stage.clone();
        }
        let mut counts = import_transaction(&mut conn, tables, Some(cancel))
            .map_err(PublishFailure::Unpublished)?;
        if !foreign_keys.is_empty() {
            conn.query_drop("SET SESSION foreign_key_checks = 1")
                .map_err(mysql_error)
                .map_err(PublishFailure::Unpublished)?;
            let stages: Vec<_> = publication.iter().map(|table| &table.stage).collect();
            install_foreign_keys(
                &foreign_keys,
                &stages,
                token,
                cancel,
                &mut counts.warnings,
                |sql| {
                    conn.query_drop(sql).map_err(|error| match error {
                        mysql::Error::MySqlError(error) => {
                            ForeignKeyError::server(u32::from(error.code), error.message)
                        }
                        error => ForeignKeyError::other(mysql_error(error)),
                    })
                },
            )
            .map_err(PublishFailure::Unpublished)?;
        }
        publish_native(&mut conn, &publication, reset_server, cancel, &mut counts)?;
        Ok(counts)
    })();

    result.map_err(|failure| match failure {
        PublishFailure::Unknown(message) => message,
        PublishFailure::Unpublished(message) => {
            let linked = !foreign_keys.is_empty();
            let groups = stage_cleanup_groups(publication.iter().map(|table| &table.stage), linked);
            // The connection belongs to this import. Only private staging tables
            // in this job are cleared with foreign-key checks suspended.
            let can_drop = !linked
                || groups.is_empty()
                || conn
                    .query_drop("SET SESSION foreign_key_checks = 0")
                    .is_ok();
            let stale: Vec<_> = groups
                .into_iter()
                .filter(|group| {
                    !can_drop
                        || conn
                            .query_drop(format!("DROP TABLE IF EXISTS {}", group.join(", ")))
                            .is_err()
                })
                .flatten()
                .collect();
            if stale.is_empty() {
                message
            } else {
                format!(
                    "{IMPORT_UNCONFIRMED}{} 无法确认以下中间表已清理。 {}",
                    message.strip_prefix(IMPORT_UNCONFIRMED).unwrap_or(&message),
                    stale.join("、")
                )
            }
        }
    })
}

struct PreviousTable {
    backup: DbObject,
    definition: String,
}

struct NativeStage {
    destination: DbObject,
    stage: DbObject,
    previous: Option<PreviousTable>,
}

enum PublishFailure {
    Unpublished(String),
    Unknown(String),
}

fn plan_native_stage(
    conn: &mut Conn,
    table: &OdbcTarget,
    token: u64,
    index: usize,
    reset_server: &mut Option<ResetServer>,
) -> Result<NativeStage, String> {
    let destination = &table.object;
    let stage = DbObject {
        name: format!("_harbor_odbc_{token:016x}_{index}"),
        ..destination.clone()
    };
    let previous =
        if table.reset != OdbcReset::None && native_reset_status(conn, destination)?.is_some() {
            let server = match *reset_server {
                Some(server) => server,
                None => {
                    let version: String = conn
                        .query_first("SELECT VERSION()")
                        .map_err(mysql_error)?
                        .ok_or("无法读取目标服务器版本。")?;
                    let server = ResetServer::parse(&version)?;
                    *reset_server = Some(server);
                    server
                }
            };
            let definition = checked_reset_definition(conn, destination, server)?;
            if let Some(expected) = &table.expected_ddl
                && reset_definition_body(expected.clone(), destination)? != definition
            {
                return Err(format!(
                    "目标表 {} 的结构已变更，请重新检查导入映射。",
                    destination.qualified_name()
                ));
            }
            Some(PreviousTable {
                backup: DbObject {
                    name: format!("_harbor_odbc_old_{token:016x}_{index}"),
                    ..destination.clone()
                },
                definition,
            })
        } else {
            if table.reset == OdbcReset::Truncate
                || table.reset == OdbcReset::Recreate && table.expected_ddl.is_some()
            {
                return Err(format!(
                    "找不到要替换的原表 {}，请重新检查导入映射。",
                    destination.qualified_name()
                ));
            }
            None
        };
    Ok(NativeStage {
        destination: destination.clone(),
        stage,
        previous,
    })
}

type NativeResetStatus = (String, Option<String>, String, u8, u8);

fn native_reset_status(
    conn: &mut Conn,
    object: &DbObject,
) -> Result<Option<NativeResetStatus>, String> {
    conn.query_first_opt(reset_table_sql(object))
        .map_err(mysql_error)?
        .transpose()
        .map_err(|error| mysql_error(error.into()))
}

fn checked_reset_definition(
    conn: &mut Conn,
    object: &DbObject,
    server: ResetServer,
) -> Result<String, String> {
    let name = object.qualified_name();
    let (kind, engine, transactions, triggers, foreign_keys) =
        native_reset_status(conn, object)?.ok_or_else(|| format!("找不到要替换的原表 {name}。"))?;
    check_reset_table(
        object,
        &kind,
        engine.as_deref(),
        &transactions,
        triggers != 0,
        foreign_keys != 0,
    )?;
    let eligible: u8 = conn
        .query_first(format!("SELECT {}", reset_dependency_guard(object, server)))
        .map_err(|error| {
            format!(
                "无法核对 {name} 的触发器和外键，需要 TRIGGER 和 PROCESS 权限。 {}",
                mysql_error(error)
            )
        })?
        .ok_or_else(|| format!("服务器未返回 {name} 的依赖检查结果。"))?;
    if eligible != 1 {
        return Err(format!(
            "无法确认 {name} 没有触发器或外键关系，请检查 TRIGGER 权限及跨库外键。"
        ));
    }
    reset_definition_body(native_definition(conn, object)?, object)
}

fn native_definition(conn: &mut Conn, object: &DbObject) -> Result<String, String> {
    let name = object.qualified_name();
    let row = conn
        .query_first_opt::<(String, String), _>(format!("SHOW CREATE TABLE {name}"))
        .map_err(mysql_error)?
        .transpose()
        .map_err(|error| mysql_error(error.into()))?;
    row.map(|(_, definition)| definition)
        .ok_or_else(|| format!("服务器未返回表 {name} 的建表语句。"))
}

fn create_native_stage(conn: &mut Conn, planned: &NativeStage, sql: &str) -> Result<(), String> {
    conn.query_drop(sql).map_err(|error| {
        let rejected = matches!(error, mysql::Error::MySqlError(_));
        let message = format!(
            "无法准备目标表 {}。 {}",
            planned.destination.qualified_name(),
            mysql_error(error)
        );
        if rejected {
            message
        } else {
            format!(
                "{IMPORT_UNCONFIRMED}{message} 无法确认中间表 {} 的创建状态。",
                planned.stage.qualified_name()
            )
        }
    })
}

fn check_create_warnings(conn: &mut Conn) -> Result<(), String> {
    if conn.warnings() != 0 {
        let warnings = conn
            .query::<(String, u16, String), _>("SHOW WARNINGS")
            .map_err(mysql_error)?;
        if let Some((_, code, message)) = warnings
            .into_iter()
            .find(|(level, code, _)| level != "Note" && *code != 1681)
        {
            return Err(format!("建表返回警告 {code}。 {message}"));
        }
    }
    Ok(())
}

fn publish_native(
    conn: &mut Conn,
    tables: &[NativeStage],
    reset_server: Option<ResetServer>,
    cancel: &AtomicBool,
    counts: &mut OdbcImportCounts,
) -> Result<(), PublishFailure> {
    let mut renames = Vec::new();
    let mut private_names = Vec::new();
    let mut lock_names = Vec::new();
    let mut previous_tables = Vec::new();
    for table in tables {
        let stage = table.stage.qualified_name();
        let target = table.destination.qualified_name();
        if let Some(previous) = &table.previous {
            let backup = previous.backup.qualified_name();
            renames.push(format!("{target} TO {backup}"));
            lock_names.push(target.clone());
            private_names.push(backup);
            previous_tables.push((&table.destination, previous));
        }
        renames.push(format!("{stage} TO {target}"));
        lock_names.push(stage.clone());
        private_names.push(stage);
    }
    let sql = format!("RENAME TABLE {}", renames.join(", "));
    let mut locked = false;
    let checked = (|| {
        check_import_cancelled(Some(cancel))?;
        if let Some(server) = reset_server {
            lock_native_tables(conn, &lock_names)?;
            locked = true;
            for (destination, previous) in &previous_tables {
                let current = checked_reset_definition(conn, destination, server)?;
                if current != previous.definition {
                    return Err(format!(
                        "发布前目标表 {} 的结构已变更，请重新检查导入映射。",
                        destination.qualified_name()
                    ));
                }
            }
            check_import_cancelled(Some(cancel))?;
            // MariaDB accepts the checks under an explicit WRITE lock, but it
            // requires releasing that lock before RENAME. Recheck the old tables
            // after publication before removing their recoverable copies.
            if !server.can_rename_locked() {
                conn.query_drop("UNLOCK TABLES").map_err(mysql_error)?;
                locked = false;
            }
        }
        Ok(())
    })();
    if let Err(message) = checked {
        if locked {
            let _ = conn.query_drop("UNLOCK TABLES");
        }
        return Err(PublishFailure::Unpublished(message));
    }
    if let Err(error) = conn.query_drop(sql) {
        if matches!(error, mysql::Error::MySqlError(_)) {
            if locked {
                let _ = conn.query_drop("UNLOCK TABLES");
            }
            return Err(PublishFailure::Unpublished(format!(
                "无法发布 ODBC 导入的目标表。 {}",
                mysql_error(error)
            )));
        }
        return Err(PublishFailure::Unknown(format!(
            "{IMPORT_UNCONFIRMED}无法确认目标表的发布结果。请先核对目标表及可能保留的中间表和旧表。 {}。 {}",
            private_names.join("、"),
            mysql_error(error)
        )));
    }

    if let Some(server) = reset_server {
        if !locked {
            let backups = previous_tables
                .iter()
                .map(|(_, previous)| previous.backup.qualified_name())
                .collect::<Vec<_>>();
            if let Err(message) = lock_native_tables(conn, &backups) {
                counts.warnings.push(format!(
                    "已发布新数据，以下旧表已保留。 {}。{message}",
                    backups.join("、")
                ));
                return Ok(());
            }
        }
        for (_, previous) in previous_tables {
            let backup = previous.backup.qualified_name();
            match checked_reset_definition(conn, &previous.backup, server) {
                Ok(definition) if definition == previous.definition => {
                    if let Err(error) = conn.query_drop(format!("DROP TABLE {backup}")) {
                        counts.warnings.push(format!(
                            "已发布新数据，无法确认旧表 {backup} 已清理。 {}",
                            mysql_error(error)
                        ));
                    }
                }
                Ok(_) => counts.warnings.push(format!(
                    "已发布新数据，旧表 {backup} 的结构在发布期间发生变化，已保留供核对。"
                )),
                Err(message) => counts.warnings.push(format!(
                    "已发布新数据，旧表 {backup} 已保留供核对。 {message}"
                )),
            }
        }
        if let Err(error) = conn.query_drop("UNLOCK TABLES") {
            counts
                .warnings
                .push(format!("无法确认发布连接已解锁。 {}", mysql_error(error)));
        }
    }
    Ok(())
}

fn lock_native_tables(conn: &mut Conn, names: &[String]) -> Result<(), String> {
    let locks = names
        .iter()
        .map(|name| format!("{name} WRITE"))
        .collect::<Vec<_>>()
        .join(", ");
    conn.query_drop(format!("LOCK TABLES {locks}"))
        .map_err(|error| format!("无法锁定 ODBC 导入要替换的表。 {}", mysql_error(error)))
}

fn validate_targets(tables: &[OdbcTarget], cancel: &AtomicBool) -> Result<bool, String> {
    check_import_cancelled(Some(cancel))?;
    let first = tables.first().ok_or("请选择至少一张 ODBC 目标表。")?;
    let staged = first.create.is_some() || first.reset != OdbcReset::None;
    let mut seen = HashSet::new();
    let mut used = 0;
    for table in tables {
        if !seen.insert((&table.object.schema, &table.object.name))
            || (table.create.is_some() || table.reset != OdbcReset::None) != staged
        {
            return Err("ODBC 作业须指定不同的目标表。追加导入请单独执行。".into());
        }
        match (table.reset, table.create.is_some()) {
            (OdbcReset::Truncate, true) => return Err("清空后导入必须沿用目标表结构。".into()),
            (OdbcReset::Recreate, false) => return Err("删除后重建必须指定新的目标表结构。".into()),
            _ => {}
        }
        if table.reset != OdbcReset::None
            && matches!(
                table.object.schema.to_ascii_lowercase().as_str(),
                "mysql" | "information_schema" | "performance_schema" | "sys"
            )
        {
            return Err("ODBC 导入无法替换系统库中的表。".into());
        }
        if table.create.is_none() && !table.indexes.is_empty() {
            return Err("复制源索引需要选择新建目标表。".into());
        }
        if let Some(sql) = &table.create_sql {
            if table.create.is_none() {
                return Err("编辑建表 SQL 需要选择创建或重建目标表。".into());
            }
            validate_odbc_create_table_sql(&table.object, sql)?;
        }
        if let Some(engine) = &table.engine {
            if table.create.is_none() {
                return Err("选择存储引擎需要新建或重建目标表。".into());
            }
            validate_odbc_engine(engine)?;
            if supports_staged_nontransactional_engine(engine)
                && tables.iter().any(|child| {
                    child.foreign_keys.iter().any(|key| {
                        child.object == table.object
                            || child.object.schema == table.object.schema
                                && key.referenced_table.eq_ignore_ascii_case(&table.object.name)
                    })
                })
            {
                return Err(format!("引擎 {engine} 无法复制涉及目标表 {} 的外键。", table.object.qualified_name()));
            }
        }
        used += validate_import_rows(&table.object, &table.columns, &table.rows)?;
        if used > IMPORT_MEMORY_LIMIT {
            return Err("本次 ODBC 作业的数据总量超过 32 MiB。".into());
        }
    }
    Ok(staged)
}

fn create_fields(
    columns: &[String],
    definitions: &[OdbcColumnDefinition],
    indexes: &[OdbcIndex],
    no_backslash_escapes: bool,
) -> Result<Vec<String>, String> {
    if columns.len() != definitions.len()
        || columns
            .iter()
            .any(|column| column.is_empty() || column.chars().count() > 64 || column.contains('\0'))
    {
        return Err("请为每个新建列指定类型及有效的列名，列名最多 64 个字符。".into());
    }
    let mut fields = Vec::with_capacity(columns.len() + 1);
    let mut primary = Vec::new();
    let mut auto_increment = None;
    for (name, field) in columns.iter().zip(definitions) {
        if field.auto_increment {
            if auto_increment.replace(name).is_some() {
                return Err("同一张表最多可以有一个自增列。".into());
            }
        }
        if field.primary {
            primary.push((name.clone(), false));
        }
        fields.push(field.sql(name, no_backslash_escapes)?);
    }
    let mut used = HashSet::new();
    for index in indexes {
        if index.name.is_empty()
            || index.name.chars().count() > 64
            || index.name.contains('\0')
            || !used.insert(index.name.to_lowercase())
            || index.columns.is_empty()
            || {
                let mut seen = HashSet::new();
                index
                    .columns
                    .iter()
                    .any(|(name, _)| !columns.contains(name) || !seen.insert(name))
            }
        {
            return Err("ODBC 目标索引的名称或列映射无效。".into());
        }
        if index.name.eq_ignore_ascii_case("PRIMARY") {
            if !index.unique
                || !primary.is_empty()
                    && (primary.len() != index.columns.len()
                        || primary.iter().any(|(name, _)| {
                            !index.columns.iter().any(|(indexed, _)| indexed == name)
                        }))
            {
                return Err("源主键与指定的目标主键列不匹配。".into());
            }
            primary.clone_from(&index.columns);
            continue;
        }
        let kind = if index.unique { "UNIQUE KEY" } else { "KEY" };
        fields.push(format!(
            "{kind} {} ({})",
            quote_identifier(&index.name),
            index_columns(&index.columns)
        ));
    }
    if let Some(name) = auto_increment
        && primary.first().is_none_or(|(first, _)| first != name)
    {
        return Err("自增列须放在主键的第一列。".into());
    }
    if !primary.is_empty() {
        fields.push(format!("PRIMARY KEY ({})", index_columns(&primary)));
    }
    Ok(fields)
}

fn index_columns(columns: &[(String, bool)]) -> String {
    columns
        .iter()
        .map(|(name, descending)| {
            let name = quote_identifier(name);
            if *descending {
                format!("{name} DESC")
            } else {
                name
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}
