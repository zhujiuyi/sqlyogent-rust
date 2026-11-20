use crate::sql_tools::mysql_text_literal;

use super::super::{DbObject, odbc_identifier_key, sync_engine::stable_definition};
use super::quote_identifier;

#[derive(Clone, Copy)]
pub(super) enum ResetServer {
    MariaDb,
    MySql { partial_revokes: bool },
}

impl ResetServer {
    pub(super) fn parse(version: &str) -> Result<Self, String> {
        let maria = version.to_ascii_lowercase().contains("mariadb");
        let version = version.trim();
        let version = version.strip_prefix("5.5.5-").unwrap_or(version);
        let mut parts = version
            .split(|character: char| !character.is_ascii_digit())
            .take(3)
            .map(|part| part.parse::<u32>().unwrap_or(0));
        let numbers = (
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
        );
        if maria && numbers >= (10, 6, 1) {
            Ok(Self::MariaDb)
        } else if !maria && numbers >= (8, 0, 13) {
            Ok(Self::MySql {
                partial_revokes: numbers >= (8, 0, 16),
            })
        } else {
            Err("替换现有表需要支持原子重命名的 MySQL 8.0.13 或 MariaDB 10.6.1 及以上版本。".into())
        }
    }

    pub(super) fn can_rename_locked(self) -> bool {
        matches!(self, Self::MySql { .. })
    }
}

fn identifier_match(column: &str, literal: &str) -> String {
    let column = odbc_identifier_key(column);
    let literal = odbc_identifier_key(literal);
    format!("CAST({column} AS BINARY) = CAST({literal} AS BINARY)")
}

pub(super) fn reset_table_sql(object: &DbObject) -> String {
    let schema = mysql_text_literal(&object.schema);
    let name = mysql_text_literal(&object.name);
    let table_schema = identifier_match("t.TABLE_SCHEMA", &schema);
    let table_name = identifier_match("t.TABLE_NAME", &name);
    let trigger_schema = identifier_match("g.EVENT_OBJECT_SCHEMA", &schema);
    let trigger_table = identifier_match("g.EVENT_OBJECT_TABLE", &name);
    let fk_schema = identifier_match("k.TABLE_SCHEMA", &schema);
    let fk_table = identifier_match("k.TABLE_NAME", &name);
    let fk_ref_schema = identifier_match("k.REFERENCED_TABLE_SCHEMA", &schema);
    let fk_ref_table = identifier_match("k.REFERENCED_TABLE_NAME", &name);
    format!(
        "SELECT t.TABLE_TYPE AS harbor_kind, t.ENGINE AS harbor_engine,
                COALESCE(e.TRANSACTIONS,'NO') AS harbor_transactions,
                EXISTS(SELECT 1 FROM information_schema.TRIGGERS g
                       WHERE {trigger_schema} AND {trigger_table}) AS harbor_has_triggers,
                EXISTS(SELECT 1 FROM information_schema.KEY_COLUMN_USAGE k
                       WHERE k.REFERENCED_TABLE_NAME IS NOT NULL
                         AND (({fk_schema} AND {fk_table})
                           OR ({fk_ref_schema} AND {fk_ref_table}))) AS harbor_has_foreign_keys
         FROM information_schema.TABLES t
         LEFT JOIN information_schema.ENGINES e ON e.ENGINE = t.ENGINE
         WHERE {table_schema} AND {table_name}"
    )
}

pub(super) fn check_reset_table(
    object: &DbObject,
    kind: &str,
    engine: Option<&str>,
    transactions: &str,
    has_triggers: bool,
    has_foreign_keys: bool,
) -> Result<(), String> {
    let name = object.qualified_name();
    if kind != "BASE TABLE"
        || !engine.is_some_and(|value| value.eq_ignore_ascii_case("InnoDB"))
        || transactions != "YES"
    {
        return Err(format!("目标 {name} 必须是支持事务的 InnoDB 普通表。"));
    }
    if has_triggers || has_foreign_keys {
        return Err(format!("目标表 {name} 有触发器或外键关系，无法替换。"));
    }
    Ok(())
}

pub(super) fn reset_dependency_guard(object: &DbObject, server: ResetServer) -> String {
    let schema = mysql_text_literal(&object.schema);
    let name = mysql_text_literal(&object.name);
    // The ordinary catalog hides trigger metadata and can omit foreign keys in
    // other schemas. Prove trigger visibility and read the InnoDB catalog, which
    // requires PROCESS but includes relationships across schemas.
    const GRANTEE: &str = "CONCAT(
        QUOTE(LEFT(CURRENT_USER(),
            CHAR_LENGTH(CURRENT_USER()) - CHAR_LENGTH(SUBSTRING_INDEX(CURRENT_USER(),'@',-1)) - 1)),
        '@', QUOTE(SUBSTRING_INDEX(CURRENT_USER(),'@',-1)))";
    let grant_schema = identifier_match("p.TABLE_SCHEMA", &schema);
    let grant_table = identifier_match("p.TABLE_NAME", &name);
    let direct_trigger_grant = format!(
        "p.PRIVILEGE_TYPE = 'TRIGGER' AND CAST(p.GRANTEE AS BINARY) = CAST({GRANTEE} AS BINARY)"
    );
    let global_grant_applies = match server {
        ResetServer::MySql {
            partial_revokes: true,
        } => "@@GLOBAL.partial_revokes = 0",
        _ => "1",
    };
    let (catalog, key) = match server {
        ResetServer::MariaDb => (
            "INNODB_SYS_FOREIGN",
            format!(
                "CONCAT(CONVERT(IF(@@lower_case_table_names=0,{schema},LOWER({schema})) USING filename),
                        '/', CONVERT(IF(@@lower_case_table_names=0,{name},LOWER({name})) USING filename))"
            ),
        ),
        ResetServer::MySql { .. } => ("INNODB_FOREIGN", format!("CONCAT({schema},'/',{name})")),
    };
    let matches_fk = match server {
        // The filename mapping adds @@@ to reserved Windows basenames. The
        // encoded user identifier cannot contain this sequence itself.
        ResetServer::MariaDb => format!(
            "CAST(REPLACE(f.FOR_NAME,'@@@','') AS BINARY)=CAST({key} AS BINARY)
             OR CAST(REPLACE(f.REF_NAME,'@@@','') AS BINARY)=CAST({key} AS BINARY)"
        ),
        ResetServer::MySql { .. } => format!(
            "({}) OR ({})",
            identifier_match("f.FOR_NAME", &key),
            identifier_match("f.REF_NAME", &key),
        ),
    };
    format!(
        "((({global_grant_applies}) AND EXISTS(SELECT 1 FROM information_schema.USER_PRIVILEGES p
                  WHERE {direct_trigger_grant}))
           OR EXISTS(SELECT 1 FROM information_schema.SCHEMA_PRIVILEGES p
                     WHERE {direct_trigger_grant} AND {grant_schema})
           OR EXISTS(SELECT 1 FROM information_schema.TABLE_PRIVILEGES p
                     WHERE {direct_trigger_grant} AND {grant_schema} AND {grant_table}))
         AND NOT EXISTS(SELECT 1 FROM information_schema.{catalog} f WHERE {matches_fk})"
    )
}

pub(super) fn reset_eligibility_guard(object: &DbObject, server: ResetServer) -> String {
    let inspection = reset_table_sql(object);
    let dependencies = reset_dependency_guard(object, server);
    format!(
        "(EXISTS(SELECT 1 FROM ({inspection}) AS harbor_reset_table
                 WHERE harbor_kind='BASE TABLE' AND harbor_engine='InnoDB' AND harbor_transactions='YES'
                   AND harbor_has_triggers=0 AND harbor_has_foreign_keys=0)
          AND {dependencies})"
    )
}

pub(super) fn reset_definition_body(
    definition: String,
    object: &DbObject,
) -> Result<String, String> {
    // SHOW CREATE used by the application quotes identifiers; also read definitions
    // saved with ANSI_QUOTES or with quoting disabled.
    let name = &object.name;
    let names = [
        quote_identifier(name),
        format!("\"{}\"", name.replace('"', "\"\"")),
        name.clone(),
    ];
    let normalized = stable_definition(definition.replace("\r\n", "\n"));
    let body = normalized
        .strip_prefix("CREATE TABLE ")
        .and_then(|tail| {
            names.iter().find_map(|name| {
                let prefix = tail.get(..name.len())?;
                let after_name = tail.get(name.len()..)?.trim_start();
                // MariaDB and MySQL may canonicalize the case of a name.
                (prefix.to_lowercase() == name.to_lowercase() && after_name.starts_with('('))
                    .then_some(after_name)
            })
        })
        .ok_or_else(|| format!("无法核对目标表 {} 的建表语句。", object.qualified_name()))?;
    Ok(body.to_owned())
}
