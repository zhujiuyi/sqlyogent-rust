mod grants;
mod http;
#[cfg(test)]
mod workflow_test;

use std::collections::BTreeSet;

pub use grants::{GrantScope, GrantSnapshot, PrivilegeChanges};
pub(super) use http::handle as handle_http;

use mysql::{Conn, prelude::Queryable};

use super::{
    DbResult, Failure,
    object_operations::{MetadataRows, native_rows},
    quote_identifier,
};

const ACCOUNT_LIST: &str =
    "SELECT `User`, `Host` FROM `mysql`.`user` WHERE `Host` <> '' ORDER BY `User`, `Host`";
const DIALECT_SETTINGS: &str = "SELECT VERSION(), @@SESSION.sql_mode, @@lower_case_table_names";
const PARTIAL_REVOKES: &str = "SHOW GLOBAL VARIABLES LIKE 'partial_revokes'";

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AccountName {
    pub user: String,
    pub host: String,
}

impl AccountName {
    pub fn label(&self) -> String {
        fn part(value: &str) -> String {
            if value.is_empty() {
                "''".into()
            } else {
                quote_identifier(value)
            }
        }
        format!("{}@{}", part(&self.user), part(&self.host))
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.host.trim().is_empty() {
            Err("请输入账户主机。")
        } else if self.user.contains('\0') || self.host.contains('\0') {
            Err("账户名称和主机不能包含 NUL 字符。")
        } else {
            Ok(())
        }
    }
}

pub enum AccountCommand {
    List,
    ListPrivileges,
    Read(AccountName),
    Create {
        account: AccountName,
        password: String,
        privileges: BTreeSet<String>,
        grant_option: bool,
    },
    Rename {
        account: AccountName,
        target: AccountName,
    },
    Password {
        account: AccountName,
        password: String,
    },
    Drop(AccountName),
    Apply {
        original: GrantSnapshot,
        changes: PrivilegeChanges,
    },
}

pub enum AccountReply {
    Listed(Vec<AccountName>),
    Privileges(BTreeSet<String>),
    Read(GrantSnapshot),
    Changed {
        selected: Option<AccountName>,
        message: &'static str,
        warning: Option<String>,
    },
    Applied(AccountApplyResult),
}

pub struct AccountApplyResult {
    pub completed: usize,
    pub total: usize,
    pub refreshed: Option<GrantSnapshot>,
    pub error: Option<String>,
}

struct Dialect {
    modern_mysql: bool,
    sql_mode: String,
    partial_revokes: bool,
    lower_case_names: bool,
}

impl Dialect {
    fn new(version: &str, sql_mode: String, lower_case_names: bool) -> Self {
        let modern_mysql = !version.to_ascii_lowercase().contains("mariadb")
            && version
                .split('.')
                .next()
                .and_then(|value| value.parse::<u8>().ok())
                .is_some_and(|major| major >= 8);
        Self {
            modern_mysql,
            sql_mode,
            partial_revokes: false,
            lower_case_names,
        }
    }

    fn read(conn: &mut Conn) -> DbResult<Self> {
        let (version, sql_mode, lower_case_names): (String, String, u8) = conn
            .query_first(DIALECT_SETTINGS)?
            .ok_or_else(|| message("服务器未返回会话设置。"))?;
        let mut dialect = Self::new(&version, sql_mode, lower_case_names != 0);
        dialect.partial_revokes = dialect.modern_mysql
            && conn
                .query_first::<(String, String), _>(PARTIAL_REVOKES)?
                .is_some_and(|(_, value)| value.eq_ignore_ascii_case("ON"));
        Ok(dialect)
    }

    fn has_mode(&self, mode: &str) -> bool {
        self.sql_mode
            .split(',')
            .any(|current| current.eq_ignore_ascii_case(mode))
    }

    fn password_literal(&self, password: &str) -> DbResult<String> {
        password_literal(password, !self.has_mode("NO_BACKSLASH_ESCAPES"))
    }
}

fn password_literal(password: &str, backslash_escapes: bool) -> DbResult<String> {
    if password.contains('\0') {
        return Err(message("密码不能包含 NUL 字符。"));
    }
    let mut sql = String::from("'");
    for character in password.chars() {
        match character {
            '\'' => sql.push_str("''"),
            '\\' if backslash_escapes => sql.push_str("\\\\"),
            other => sql.push(other),
        }
    }
    sql.push('\'');
    Ok(sql)
}

fn account_list(rows: MetadataRows) -> DbResult<Vec<AccountName>> {
    rows.into_iter()
        .map(|row| match <[_; 2]>::try_from(row) {
            Ok([Some(user), Some(host)]) => Ok(AccountName { user, host }),
            _ => Err(message("服务器返回了不完整的账户列表。")),
        })
        .collect()
}

pub(super) fn handle(
    conn: &mut Conn,
    command: AccountCommand,
) -> (Result<AccountReply, String>, bool) {
    let mut retire_session = false;
    let reply = run(conn, command, &mut retire_session);
    retire_session |= reply.as_ref().err().is_some_and(Failure::lost_connection);
    (reply.map_err(Failure::message), retire_session)
}

fn run(
    conn: &mut Conn,
    command: AccountCommand,
    retire_session: &mut bool,
) -> DbResult<AccountReply> {
    match command {
        AccountCommand::List => {
            let accounts = account_list(native_rows(conn, ACCOUNT_LIST)?)?;
            Ok(AccountReply::Listed(accounts))
        }
        AccountCommand::ListPrivileges => grants::catalog(conn).map(AccountReply::Privileges),
        AccountCommand::Read(account) => {
            let dialect = Dialect::read(conn)?;
            grants::read(conn, &dialect, account).map(AccountReply::Read)
        }
        AccountCommand::Create {
            account,
            password,
            privileges,
            grant_option,
        } => create(
            conn,
            account,
            &password,
            PrivilegeChanges {
                scope: GrantScope::Global,
                names: privileges,
                grant_option,
            },
            retire_session,
        ),
        AccountCommand::Rename { account, target } => {
            account.validate().map_err(message)?;
            target.validate().map_err(message)?;
            conn.query_drop(format!(
                "RENAME USER {} TO {}",
                account.label(),
                target.label()
            ))?;
            Ok(AccountReply::Changed {
                selected: Some(target),
                message: "已更改账户名称或主机。",
                warning: None,
            })
        }
        AccountCommand::Password { account, password } => {
            account.validate().map_err(message)?;
            let dialect = Dialect::read(conn)?;
            let literal = dialect.password_literal(&password)?;
            let value = if dialect.modern_mysql {
                literal
            } else {
                format!("PASSWORD({literal})")
            };
            let sql = format!("SET PASSWORD FOR {} = {value}", account.label());
            write_password_statement(conn, &sql, retire_session)?;
            if conn.warnings() != 0 {
                return Err(message(
                    "服务器返回了密码操作警告。请核对账户使用的认证方式，再验证登录。",
                ));
            }
            Ok(AccountReply::Changed {
                selected: Some(account),
                message: "已修改账户密码。下一次登录使用新密码。",
                warning: None,
            })
        }
        AccountCommand::Drop(account) => {
            account.validate().map_err(message)?;
            conn.query_drop(format!("DROP USER {}", account.label()))?;
            Ok(AccountReply::Changed {
                selected: None,
                message: "已删除账户及直接授权。",
                warning: None,
            })
        }
        AccountCommand::Apply { original, changes } => {
            let dialect = Dialect::read(conn)?;
            apply(conn, &dialect, original, &changes, retire_session).map(AccountReply::Applied)
        }
    }
}

fn create(
    conn: &mut Conn,
    account: AccountName,
    password: &str,
    changes: PrivilegeChanges,
    retire_session: &mut bool,
) -> DbResult<AccountReply> {
    account.validate().map_err(message)?;
    let dialect = Dialect::read(conn)?;
    let steps = if changes.names.is_empty() && !changes.grant_option {
        Vec::new()
    } else {
        GrantSnapshot::empty(account.clone(), &dialect, grants::catalog(conn)?)
            .statements(&changes)
            .map_err(message)?
    };
    let literal = dialect.password_literal(password)?;
    let sql = format!("CREATE USER {} IDENTIFIED BY {literal}", account.label());
    write_password_statement(conn, &sql, retire_session)?;

    let mut notice = "已创建账户。";
    let warning = if conn.warnings() != 0 {
        Some("服务器返回了账户创建警告。请核对账户的登录和权限。".into())
    } else if steps.is_empty() {
        None
    } else {
        // CREATE USER commits separately. Report confirmed creation even when a grant fails.
        match apply_statements(conn, &dialect, &account, steps, &changes, retire_session) {
            Ok(result) => {
                if let Some(error) = result.error {
                    Some(format!(
                        "服务器已确认执行 {} / {} 条授权语句。{error}",
                        result.completed, result.total
                    ))
                } else {
                    notice = "已创建账户并核对全局权限。";
                    None
                }
            }
            Err(error) => {
                *retire_session |= error.lost_connection();
                Some(format!("无法准备授权会话。{}", error.message()))
            }
        }
    };
    Ok(AccountReply::Changed {
        selected: Some(account),
        message: notice,
        warning,
    })
}

fn write_password_statement(conn: &mut Conn, sql: &str, retire_session: &mut bool) -> DbResult<()> {
    // The server can include input fragments in a diagnostic. Only display the error code.
    if let Err(error) = conn.query_drop(sql) {
        *retire_session |= error.is_connectivity_error();
        return Err(match error {
            mysql::Error::MySqlError(error) => message(format!(
                "服务器拒绝了操作（错误 {}）。请核对账户、操作权限和服务器密码策略。",
                error.code
            )),
            _ => message("无法确认密码操作是否完成。请先核对账户或尝试重新登录。"),
        });
    }
    Ok(())
}

fn apply(
    conn: &mut Conn,
    dialect: &Dialect,
    original: GrantSnapshot,
    changes: &PrivilegeChanges,
    retire_session: &mut bool,
) -> DbResult<AccountApplyResult> {
    let current = grants::read(conn, dialect, original.account.clone())?;
    if current != original {
        return Ok(AccountApplyResult {
            completed: 0,
            total: 0,
            refreshed: Some(current),
            error: Some("服务器上的权限或授权设置已变化。请核对刷新后的状态再提交。".into()),
        });
    }
    let steps = current.statements(changes).map_err(message)?;
    apply_statements(
        conn,
        dialect,
        &current.account,
        steps,
        changes,
        retire_session,
    )
}

fn apply_statements(
    conn: &mut Conn,
    dialect: &Dialect,
    account: &AccountName,
    steps: Vec<String>,
    changes: &PrivilegeChanges,
    retire_session: &mut bool,
) -> DbResult<AccountApplyResult> {
    // On servers that support implicit account creation by GRANT, keep a concurrent
    // removal from turning a grant into a new passwordless account.
    let change_mode = !dialect.modern_mysql
        && !dialect.has_mode("NO_AUTO_CREATE_USER")
        && steps.iter().any(|sql| sql.starts_with("GRANT "));
    if change_mode {
        let mut modes: Vec<_> = dialect
            .sql_mode
            .split(',')
            .filter(|mode| !mode.is_empty())
            .collect();
        modes.push("NO_AUTO_CREATE_USER");
        conn.exec_drop("SET SESSION sql_mode = ?", (modes.join(","),))?;
    }

    let mut result = AccountApplyResult {
        completed: 0,
        total: steps.len(),
        refreshed: None,
        error: None,
    };
    for sql in steps {
        if let Err(error) = conn.query_drop(sql) {
            *retire_session |= error.is_connectivity_error();
            let prefix = if *retire_session {
                "连接中断，无法确认最后一条语句是否已执行。"
            } else {
                "服务器停止处理后续语句。"
            };
            append_error(
                &mut result,
                format!("{prefix} {}", super::mysql_error(error)),
            );
            break;
        }
        result.completed += 1;
        if conn.warnings() != 0 {
            append_error(&mut result, "服务器返回警告，已停止处理后续语句。".into());
            break;
        }
    }
    if change_mode
        && !*retire_session
        && conn
            .exec_drop("SET SESSION sql_mode = ?", (&dialect.sql_mode,))
            .is_err()
    {
        *retire_session = true;
        append_error(
            &mut result,
            "无法恢复原会话 SQL 模式，已关闭当前会话。".into(),
        );
    }
    if !*retire_session {
        match grants::read(conn, dialect, account.clone()) {
            Ok(snapshot) => {
                if result.error.is_none() && !snapshot.matches(changes) {
                    append_error(
                        &mut result,
                        "服务器确认执行，但读回的权限与所选内容不同。请核对当前权限。".into(),
                    );
                }
                result.refreshed = Some(snapshot);
            }
            Err(error) => {
                *retire_session |= error.lost_connection();
                append_error(
                    &mut result,
                    format!("无法核对当前权限。 {}", error.message()),
                );
            }
        }
    }
    Ok(result)
}

fn append_error(result: &mut AccountApplyResult, error: String) {
    let reason = result.error.get_or_insert_with(String::new);
    if !reason.is_empty() {
        reason.push(' ');
    }
    reason.push_str(&error);
}

fn message(error: impl Into<String>) -> Failure {
    Failure::Message(error.into())
}
