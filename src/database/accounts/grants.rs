use std::collections::{BTreeMap, BTreeSet};

use mysql::{Conn, prelude::Queryable};

use crate::sql_tools::{EditorToken, EditorTokenKind, editor_tokens};

use super::{AccountName, Dialect, message};
use crate::database::{
    DbResult,
    object_operations::{MetadataRows, native_rows},
    quote_identifier,
};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum GrantScope {
    Global,
    Database(String),
    Table {
        database: String,
        table: String,
    },
    Column {
        database: String,
        table: String,
        column: String,
    },
    Routine {
        database: String,
        name: String,
        function: bool,
    },
}

impl GrantScope {
    fn target(&self) -> String {
        let qualified =
            |database, name| format!("{}.{}", quote_identifier(database), quote_identifier(name));
        match self {
            Self::Global => "*.*".into(),
            Self::Database(database) => format!("{}.*", quote_identifier(database)),
            Self::Table { database, table }
            | Self::Column {
                database, table, ..
            } => qualified(database, table),
            Self::Routine {
                database,
                name,
                function,
            } => {
                let kind = if *function { "FUNCTION" } else { "PROCEDURE" };
                format!("{kind} {}", qualified(database, name))
            }
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Global => "全局  *.*".into(),
            Self::Column { column, .. } => {
                format!("{}.{}", self.target(), quote_identifier(column))
            }
            _ => self.target(),
        }
    }

    fn permits(&self, name: &str) -> bool {
        let table = matches!(
            name,
            "ALTER"
                | "CREATE"
                | "CREATE VIEW"
                | "DELETE"
                | "DELETE HISTORY"
                | "DELETE VERSIONING ROWS"
                | "DROP"
                | "INDEX"
                | "INSERT"
                | "REFERENCES"
                | "SELECT"
                | "SHOW VIEW"
                | "TRIGGER"
                | "UPDATE"
        );
        let routine = matches!(name, "ALTER ROUTINE" | "EXECUTE" | "SHOW CREATE ROUTINE");
        match self {
            Self::Global => true,
            Self::Database(_) => {
                table
                    || routine
                    || matches!(
                        name,
                        "CREATE ROUTINE" | "CREATE TEMPORARY TABLES" | "EVENT" | "LOCK TABLES"
                    )
            }
            Self::Table { .. } => table,
            Self::Column { .. } => matches!(name, "SELECT" | "INSERT" | "UPDATE" | "REFERENCES"),
            Self::Routine { .. } => routine,
        }
    }

    fn validate(&self) -> Result<(), &'static str> {
        let identifier = |value: &str| !value.trim().is_empty() && !value.contains('\0');
        let valid = match self {
            Self::Global => true,
            Self::Database(database) => identifier(database),
            Self::Table { database, table } => identifier(database) && identifier(table),
            Self::Column {
                database,
                table,
                column,
            } => identifier(database) && identifier(table) && identifier(column),
            Self::Routine { database, name, .. } => identifier(database) && identifier(name),
        };
        if valid {
            Ok(())
        } else {
            Err("请填写此权限范围的所有对象名称。")
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ScopeGrants {
    pub names: BTreeSet<String>,
    pub grant_option: bool,
    pub delegated: BTreeSet<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantSnapshot {
    pub account: AccountName,
    pub scopes: BTreeMap<GrantScope, ScopeGrants>,
    pub other_rules: usize,
    catalog: BTreeSet<String>,
    partial_revokes: bool,
    lower_case_names: bool,
    restrictions: bool,
    unknown: bool,
}

pub struct PrivilegeChanges {
    pub scope: GrantScope,
    pub names: BTreeSet<String>,
    pub grant_option: bool,
}

impl GrantSnapshot {
    pub(super) fn empty(
        account: AccountName,
        dialect: &Dialect,
        catalog: BTreeSet<String>,
    ) -> Self {
        Self {
            account,
            scopes: BTreeMap::from([(GrantScope::Global, ScopeGrants::default())]),
            catalog,
            partial_revokes: dialect.partial_revokes,
            lower_case_names: dialect.lower_case_names,
            other_rules: 0,
            restrictions: false,
            unknown: false,
        }
    }

    pub fn literal_scope(&self, scope: GrantScope) -> GrantScope {
        let normalize = |name: String| {
            if self.lower_case_names {
                name.to_lowercase()
            } else {
                name
            }
        };
        match scope {
            GrantScope::Global => scope,
            GrantScope::Database(database) => {
                let mut database = normalize(database);
                if !self.partial_revokes {
                    database = database
                        .replace('\\', "\\\\")
                        .replace('_', "\\_")
                        .replace('%', "\\%");
                }
                GrantScope::Database(database)
            }
            GrantScope::Table { database, table } => GrantScope::Table {
                database: normalize(database),
                table: normalize(table),
            },
            GrantScope::Column {
                database,
                table,
                column,
            } => {
                let database = normalize(database);
                let table = normalize(table);
                // Server column names are case-insensitive, even on a case-sensitive schema.
                if let Some(scope) = self.scopes.keys().find(|scope| {
                    matches!(scope, GrantScope::Column { database: db, table: tbl, column: col }
                        if db == &database && tbl == &table && col.eq_ignore_ascii_case(&column))
                }) {
                    return scope.clone();
                }
                GrantScope::Column {
                    database,
                    table,
                    column,
                }
            }
            GrantScope::Routine {
                database,
                name,
                function,
            } => {
                let database = normalize(database);
                if let Some(scope) = self.scopes.keys().find(|scope| {
                    matches!(scope, GrantScope::Routine { database: db, name: routine, function: is_function }
                        if db == &database && routine.eq_ignore_ascii_case(&name) && *is_function == function)
                }) {
                    return scope.clone();
                }
                GrantScope::Routine {
                    database,
                    name,
                    function,
                }
            }
        }
    }

    pub fn choices<'a>(&'a self, scope: &'a GrantScope) -> impl Iterator<Item = &'a String> + 'a {
        self.catalog.iter().filter(move |name| scope.permits(name))
    }

    pub fn editable(&self, scope: &GrantScope) -> Result<(), &'static str> {
        scope.validate()?;
        if self.unknown {
            Err("账户含有无法安全读取的授权规则，请用 SQL 管理该账户。")
        } else if self.restrictions && matches!(scope, GrantScope::Global | GrantScope::Database(_))
        {
            Err("账户有部分撤销规则，需用 SQL 一并核对该范围的权限和限制。")
        } else {
            Ok(())
        }
    }

    pub fn statements(&self, changes: &PrivilegeChanges) -> Result<Vec<String>, &'static str> {
        self.statements_for_account(changes, &self.account.label())
    }

    pub(super) fn statements_for_account(
        &self,
        changes: &PrivilegeChanges,
        account: &str,
    ) -> Result<Vec<String>, &'static str> {
        let scope = &changes.scope;
        self.editable(scope)?;
        let empty = ScopeGrants::default();
        let baseline = self.scopes.get(scope).unwrap_or(&empty);
        if baseline
            .names
            .symmetric_difference(&changes.names)
            .any(|name| !scope.permits(name) || !self.catalog.contains(name))
        {
            return Err("所选权限在该范围不可编辑。");
        }
        if matches!(scope, GrantScope::Column { .. }) && changes.grant_option {
            return Err("请在表级范围更改列权限的再授权能力。");
        }

        let revoked = baseline
            .names
            .difference(&changes.names)
            .collect::<Vec<_>>();
        // MySQL-compatible REVOKE at table level also clears corresponding column grants.
        if let GrantScope::Table { database, table } = scope
            && self.scopes.iter().any(|(scope, grants)| {
                matches!(scope, GrantScope::Column { database: db, table: tbl, .. }
                    if db == database && tbl == table)
                    && revoked.iter().any(|name| grants.names.contains(*name))
            })
        {
            return Err("服务器会一并撤销同名的列级权限。请先调整这些列的直接授权。");
        }

        let added = changes
            .names
            .difference(&baseline.names)
            .collect::<Vec<_>>();
        let target = scope.target();
        let mut statements = Vec::new();
        if baseline.grant_option && !changes.grant_option {
            statements.push(format!("REVOKE GRANT OPTION ON {target} FROM {account}"));
        }
        let list = |names: Vec<&String>| {
            names
                .into_iter()
                .map(|name| match scope {
                    GrantScope::Column { column, .. } => {
                        format!("{name} ({})", quote_identifier(column))
                    }
                    _ => name.clone(),
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        if !revoked.is_empty() {
            statements.push(format!(
                "REVOKE {} ON {target} FROM {account}",
                list(revoked)
            ));
        }
        if !added.is_empty() {
            statements.push(format!("GRANT {} ON {target} TO {account}", list(added)));
        }
        if !baseline.grant_option && changes.grant_option {
            statements.push(format!(
                "GRANT USAGE ON {target} TO {account} WITH GRANT OPTION"
            ));
        }
        Ok(statements)
    }

    pub(super) fn matches(&self, changes: &PrivilegeChanges) -> bool {
        !self.unknown
            && self.scopes.get(&changes.scope).map_or_else(
                || changes.names.is_empty() && !changes.grant_option,
                |current| {
                    current.names == changes.names && current.grant_option == changes.grant_option
                },
            )
    }
}

pub(super) fn catalog(conn: &mut Conn) -> DbResult<BTreeSet<String>> {
    catalog_from_rows(native_rows(conn, "SHOW PRIVILEGES")?)
}

pub(super) fn catalog_from_rows(rows: MetadataRows) -> DbResult<BTreeSet<String>> {
    let catalog = rows
        .iter()
        .filter_map(|row| row.first()?.as_deref())
        .filter_map(canonical_privilege)
        .filter(|name| {
            !matches!(
                name.as_str(),
                "USAGE" | "PROXY" | "GRANT OPTION" | "ALL" | "ALL PRIVILEGES"
            )
        })
        .collect::<BTreeSet<_>>();
    if catalog.is_empty() {
        return Err(message("服务器未返回可识别的权限目录。"));
    }
    Ok(catalog)
}

pub(super) fn read(
    conn: &mut Conn,
    dialect: &Dialect,
    account: AccountName,
) -> DbResult<GrantSnapshot> {
    account.validate().map_err(message)?;
    let read = format!("SHOW GRANTS FOR {}", account.label());
    let mut snapshot = GrantSnapshot::empty(account, dialect, catalog(conn)?);
    for row in conn.query_iter(read)? {
        let sql = row?
            .take_opt::<String, _>(0)
            .and_then(Result::ok)
            .ok_or_else(|| message("服务器返回了无法读取的授权数据。"))?;
        snapshot.examine_grant(dialect, &sql);
    }
    Ok(snapshot)
}

impl GrantSnapshot {
    pub(super) fn examine_grant(&mut self, dialect: &Dialect, sql: &str) {
        // SHOW GRANTS can contain an authentication hash. Retain parsed privileges
        // and never keep the original row.
        if parse_line(self, dialect, sql).is_none() {
            self.unknown = true;
        }
    }
}

fn parse_line(snapshot: &mut GrantSnapshot, dialect: &Dialect, sql: &str) -> Option<()> {
    let tokens =
        editor_tokens(sql, dialect.has_mode("NO_BACKSLASH_ESCAPES"), true).collect::<Vec<_>>();
    let (first, rest) = tokens.split_first()?;
    if word(first, "REVOKE") {
        snapshot.restrictions = true;
        snapshot.other_rules += 1;
        return Some(());
    }
    if word(first, "SET") && rest.first().is_some_and(|token| word(token, "DEFAULT")) {
        snapshot.other_rules += 1;
        return Some(());
    }
    if !word(first, "GRANT") {
        return None;
    }
    let to = rest.iter().position(|token| word(token, "TO"))?;
    let on = rest[..to].iter().position(|token| word(token, "ON"));
    let Some(on) = on.filter(|_| !word(&rest[0], "PROXY")) else {
        snapshot.other_rules += 1;
        return Some(());
    };
    let target = &rest[on + 1..to];
    if target.first().is_some_and(|token| word(token, "PACKAGE")) {
        snapshot.other_rules += 1;
        return Some(());
    }
    let scope = parse_scope(target)?;
    let grant_option = rest[to + 1..].windows(3).any(|tokens| {
        word(&tokens[0], "WITH") && word(&tokens[1], "GRANT") && word(&tokens[2], "OPTION")
    });

    let mut clauses = &rest[..on];
    while !clauses.is_empty() {
        let length = clauses
            .iter()
            .take_while(|token| token.kind == EditorTokenKind::Word)
            .count();
        let name = clauses[..length]
            .iter()
            .map(|token| token.text)
            .collect::<Vec<_>>()
            .join(" ");
        let name = canonical_privilege(&name)?;
        clauses = &clauses[length..];
        if let Some(after_open) = take_symbol(clauses, '(') {
            let GrantScope::Table { database, table } = &scope else {
                return None;
            };
            let mut columns = after_open;
            loop {
                let (column, remaining) = columns.split_first()?;
                let column = identifier(column)?;
                let target = GrantScope::Column {
                    database: database.clone(),
                    table: table.clone(),
                    column,
                };
                snapshot
                    .scopes
                    .entry(target)
                    .or_default()
                    .names
                    .insert(name.clone());
                if let Some(remaining) = take_symbol(remaining, ')') {
                    clauses = remaining;
                    break;
                }
                columns = take_symbol(remaining, ',')?;
            }
            snapshot
                .scopes
                .entry(scope.clone())
                .or_default()
                .grant_option |= grant_option;
        } else {
            let all = matches!(name.as_str(), "ALL" | "ALL PRIVILEGES");
            let entry = snapshot.scopes.entry(scope.clone()).or_default();
            if dialect.modern_mysql && scope == GrantScope::Global && mysql_dynamic(&name) {
                if grant_option {
                    entry.delegated.insert(name.clone());
                }
            } else {
                entry.grant_option |= grant_option || name == "GRANT OPTION";
            }
            match name.as_str() {
                _ if all => entry.names.extend(
                    snapshot
                        .catalog
                        .iter()
                        .filter(|name| scope.permits(name))
                        .cloned(),
                ),
                "USAGE" | "GRANT OPTION" => {}
                _ => {
                    entry.names.insert(name);
                }
            }
        }
        if !clauses.is_empty() {
            clauses = take_symbol(clauses, ',')?;
        }
    }
    Some(())
}

fn parse_scope(tokens: &[EditorToken<'_>]) -> Option<GrantScope> {
    let mut routine = None;
    let tokens = if let Some((first, rest)) = tokens.split_first() {
        if word(first, "FUNCTION") || word(first, "PROCEDURE") {
            routine = Some(word(first, "FUNCTION"));
            rest
        } else if word(first, "TABLE") {
            rest
        } else {
            tokens
        }
    } else {
        return None;
    };
    let [database, dot, object] = tokens else {
        return None;
    };
    if dot.kind != EditorTokenKind::Symbol('.') {
        return None;
    }
    if object.kind == EditorTokenKind::Symbol('*') {
        return if database.kind == EditorTokenKind::Symbol('*') {
            Some(GrantScope::Global)
        } else {
            identifier(database).map(GrantScope::Database)
        };
    }
    let database = identifier(database)?;
    let name = identifier(object)?;
    Some(match routine {
        Some(function) => GrantScope::Routine {
            database,
            name,
            function,
        },
        None => GrantScope::Table {
            database,
            table: name,
        },
    })
}

fn canonical_privilege(name: &str) -> Option<String> {
    let name = name.to_ascii_uppercase();
    if !name.split(' ').all(|word| {
        !word.is_empty()
            && word
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    }) {
        return None;
    }
    Some(if name == "READ_ONLY ADMIN" {
        "READ ONLY ADMIN".into()
    } else {
        name
    })
}

fn mysql_dynamic(name: &str) -> bool {
    // MySQL's dynamic privileges have separate grant options; several of their
    // identifiers contain no underscore, so classify by the static privilege set.
    !matches!(
        name,
        "ALL"
            | "ALL PRIVILEGES"
            | "ALTER"
            | "ALTER ROUTINE"
            | "CREATE"
            | "CREATE ROLE"
            | "CREATE ROUTINE"
            | "CREATE TABLESPACE"
            | "CREATE TEMPORARY TABLES"
            | "CREATE USER"
            | "CREATE VIEW"
            | "DELETE"
            | "DROP"
            | "DROP ROLE"
            | "EVENT"
            | "EXECUTE"
            | "FILE"
            | "GRANT OPTION"
            | "INDEX"
            | "INSERT"
            | "LOCK TABLES"
            | "PROCESS"
            | "REFERENCES"
            | "RELOAD"
            | "REPLICATION CLIENT"
            | "REPLICATION SLAVE"
            | "SELECT"
            | "SHOW DATABASES"
            | "SHOW VIEW"
            | "SHUTDOWN"
            | "SUPER"
            | "TRIGGER"
            | "UPDATE"
            | "USAGE"
    )
}

fn identifier(token: &EditorToken<'_>) -> Option<String> {
    match token.kind {
        EditorTokenKind::Identifier {
            quote,
            closed: true,
        } => {
            let raw = &token.text[1..token.text.len() - 1];
            Some(if quote == b'`' {
                raw.replace("``", "`")
            } else {
                raw.replace("\"\"", "\"")
            })
        }
        EditorTokenKind::Word => Some(token.text.to_owned()),
        _ => None,
    }
}

fn word(token: &EditorToken<'_>, word: &str) -> bool {
    token.kind == EditorTokenKind::Word && token.text.eq_ignore_ascii_case(word)
}

fn take_symbol<'a, 'sql>(
    tokens: &'a [EditorToken<'sql>],
    symbol: char,
) -> Option<&'a [EditorToken<'sql>]> {
    tokens
        .split_first()
        .and_then(|(first, rest)| (first.kind == EditorTokenKind::Symbol(symbol)).then_some(rest))
}
