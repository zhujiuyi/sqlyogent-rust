use std::collections::BTreeSet;

use crate::database::{
    MAX_RESULT_ROWS,
    http_session::{Session, batch_statements},
    object_operations::MetadataRows,
};
use crate::http_tunnel::Reply;
use crate::sql_tools::mysql_text_literal;

use super::{
    ACCOUNT_LIST, AccountApplyResult, AccountCommand, AccountName, AccountReply, DIALECT_SETTINGS,
    Dialect, GrantScope, GrantSnapshot, PARTIAL_REVOKES, PrivilegeChanges, account_list,
    append_error, grants, password_literal,
};

// PHP 8.05 attempts to select a default database, even when the XML name is empty.
// An empty name can leave error 1046 in the diagnostics read after account DDL.
const ACCOUNT_CONTEXT: &str = "information_schema";

pub(in crate::database) fn handle(
    session: &Session,
    command: AccountCommand,
) -> Result<AccountReply, String> {
    match command {
        AccountCommand::List => account_list(read_rows(session, ACCOUNT_LIST, None)?)
            .map(AccountReply::Listed)
            .map_err(|error| error.message()),
        AccountCommand::ListPrivileges => catalog(session).map(AccountReply::Privileges),
        AccountCommand::Read(account) => {
            snapshot(session, &dialect(session)?, account).map(AccountReply::Read)
        }
        AccountCommand::Create {
            account,
            password,
            privileges,
            grant_option,
        } => create(session, account, &password, privileges, grant_option),
        AccountCommand::Rename { account, target } => {
            let sql = format!(
                "RENAME USER {} TO {}",
                literal_account(&account)?,
                literal_account(&target)?
            );
            changed(
                session,
                &sql,
                &session_mode(&dialect(session)?, false),
                Some(target),
                "已更改账户名称或主机。",
            )
        }
        AccountCommand::Password { account, password } => {
            let dialect = dialect(session)?;
            let literal = password_literal(&password, true).map_err(|error| error.message())?;
            let value = if dialect.modern_mysql {
                literal
            } else {
                format!("PASSWORD({literal})")
            };
            let sql = format!("SET PASSWORD FOR {} = {value}", literal_account(&account)?);
            changed(
                session,
                &sql,
                &session_mode(&dialect, false),
                Some(account),
                "已修改账户密码。下一次登录使用新密码。",
            )
        }
        AccountCommand::Drop(account) => changed(
            session,
            &format!("DROP USER {}", literal_account(&account)?),
            &session_mode(&dialect(session)?, false),
            None,
            "已删除账户及直接授权。",
        ),
        AccountCommand::Apply { original, changes } => {
            let dialect = dialect(session)?;
            let current = snapshot(session, &dialect, original.account.clone())?;
            if current != original {
                return Ok(AccountReply::Applied(AccountApplyResult {
                    completed: 0,
                    total: 0,
                    refreshed: Some(current),
                    error: Some(
                        "服务器上的权限或授权设置已变化。请核对刷新后的状态再提交。".into(),
                    ),
                }));
            }
            let steps =
                current.statements_for_account(&changes, &literal_account(&current.account)?)?;
            Ok(AccountReply::Applied(apply_steps(
                session,
                &dialect,
                &current.account,
                steps,
                &changes,
            )))
        }
    }
}

fn literal_account(account: &AccountName) -> Result<String, String> {
    // SQL account components accept string literals. PHP 8.05 has no backtick
    // state in its batch tokenizer, so apostrophes in quoted identifiers would
    // otherwise swallow the rest of a batch.
    account.validate()?;
    let quote = |value: &str| password_literal(value, true).map_err(|error| error.message());
    Ok(format!(
        "{}@{}",
        quote(&account.user)?,
        quote(&account.host)?
    ))
}

fn dialect(session: &Session) -> Result<Dialect, String> {
    let rows = read_rows(session, DIALECT_SETTINGS, None)?;
    let [row] = rows.as_slice() else {
        return Err("隧道未返回唯一的数据库会话设置。".into());
    };
    let [Some(version), Some(mode), Some(lower_case)] = row.as_slice() else {
        return Err("隧道返回的数据库会话设置不完整。".into());
    };
    let lower_case = lower_case
        .parse::<u8>()
        .map_err(|_| "数据库名称大小写设置无效。")?;
    // Keep string quoting of both account names and passwords consistent with the
    // backslash-aware PHP tokenizer, regardless of the original session mode.
    let mode = mode
        .split(',')
        .filter(|mode| !mode.eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES"))
        .collect::<Vec<_>>()
        .join(",");
    let mut dialect = Dialect::new(version, mode, lower_case != 0);
    if dialect.modern_mysql {
        dialect.partial_revokes = read_rows(session, PARTIAL_REVOKES, None)?
            .first()
            .and_then(|row| row.get(1))
            .and_then(Option::as_deref)
            .is_some_and(|value| value.eq_ignore_ascii_case("ON"));
    }
    Ok(dialect)
}

fn catalog(session: &Session) -> Result<BTreeSet<String>, String> {
    grants::catalog_from_rows(read_rows(session, "SHOW PRIVILEGES", None)?)
        .map_err(|error| error.message())
}

fn snapshot(
    session: &Session,
    dialect: &Dialect,
    account: AccountName,
) -> Result<GrantSnapshot, String> {
    let command = format!("SHOW GRANTS FOR {}", literal_account(&account)?);
    let mut snapshot = GrantSnapshot::empty(account, dialect, catalog(session)?);
    let rows = read_rows(session, &command, Some(&session_mode(dialect, false)))?;
    if rows.is_empty() {
        return Err("隧道未返回账户授权。".into());
    }
    for row in rows {
        let [Some(sql)] = row.as_slice() else {
            return Err("隧道返回了无法读取的授权数据。".into());
        };
        snapshot.examine_grant(dialect, sql);
    }
    Ok(snapshot)
}

fn create(
    session: &Session,
    account: AccountName,
    password: &str,
    privileges: BTreeSet<String>,
    grant_option: bool,
) -> Result<AccountReply, String> {
    let account_sql = literal_account(&account)?;
    let dialect = dialect(session)?;
    let changes = PrivilegeChanges {
        scope: GrantScope::Global,
        names: privileges,
        grant_option,
    };
    let steps = if changes.names.is_empty() && !grant_option {
        Vec::new()
    } else {
        GrantSnapshot::empty(account.clone(), &dialect, catalog(session)?)
            .statements_for_account(&changes, &account_sql)?
    };
    let literal = password_literal(password, true).map_err(|error| error.message())?;
    let sql = format!("CREATE USER {account_sql} IDENTIFIED BY {literal}");
    let warning_count = write(session, &sql, &session_mode(&dialect, false))?;
    let mut message = "已创建账户。";
    let warning = if warning_count != 0 {
        Some("服务器返回了账户创建警告。请核对账户的登录和权限。".into())
    } else if steps.is_empty() {
        None
    } else {
        let applied = apply_steps(session, &dialect, &account, steps, &changes);
        match applied.error {
            Some(error) => Some(format!(
                "账户已创建，服务器确认执行了 {} / {} 条授权语句。{error}",
                applied.completed, applied.total
            )),
            None => {
                message = "已创建账户并核对全局权限。";
                None
            }
        }
    };
    Ok(AccountReply::Changed {
        selected: Some(account),
        message,
        warning,
    })
}

fn changed(
    session: &Session,
    sql: &str,
    setup: &str,
    selected: Option<AccountName>,
    message: &'static str,
) -> Result<AccountReply, String> {
    let warnings = write(session, sql, setup)?;
    Ok(AccountReply::Changed {
        selected,
        message,
        warning: (warnings != 0).then(|| "服务器返回账户操作警告。请核对实际结果。".into()),
    })
}

fn apply_steps(
    session: &Session,
    dialect: &Dialect,
    account: &AccountName,
    steps: Vec<String>,
    changes: &PrivilegeChanges,
) -> AccountApplyResult {
    let write_mode = session_mode(dialect, true);
    let mut result = AccountApplyResult {
        completed: 0,
        total: steps.len(),
        refreshed: None,
        error: None,
    };
    for sql in steps {
        match write(session, &sql, &write_mode) {
            Ok(warnings) => {
                result.completed += 1;
                if warnings != 0 {
                    append_error(&mut result, "服务器返回警告，已停止处理后续语句。".into());
                    break;
                }
            }
            Err(error) => {
                append_error(&mut result, error);
                break;
            }
        }
    }
    match snapshot(session, dialect, account.clone()) {
        Ok(snapshot) => {
            if result.error.is_none() && !snapshot.matches(changes) {
                append_error(
                    &mut result,
                    "服务器确认执行，但读回的权限与所选内容不同。请核对当前权限。".into(),
                );
            }
            result.refreshed = Some(snapshot);
        }
        Err(error) => append_error(&mut result, format!("无法核对当前权限。{error}")),
    }
    result
}

fn session_mode(dialect: &Dialect, prevent_account_creation: bool) -> String {
    let mode = if prevent_account_creation
        && !dialect.modern_mysql
        && !dialect.has_mode("NO_AUTO_CREATE_USER")
    {
        if dialect.sql_mode.is_empty() {
            "NO_AUTO_CREATE_USER".into()
        } else {
            format!("{},NO_AUTO_CREATE_USER", dialect.sql_mode)
        }
    } else {
        dialect.sql_mode.clone()
    };
    format!("SET SESSION sql_mode = {}", mysql_text_literal(&mode))
}

fn read_rows(session: &Session, sql: &str, setup: Option<&str>) -> Result<MetadataRows, String> {
    let payload = if let Some(setup) = setup {
        session.prepare_payload(&batch_statements(&[setup, sql])?, true, ACCOUNT_CONTEXT)?
    } else {
        session.prepare_payload(sql, false, ACCOUNT_CONTEXT)?
    };
    // Keep server errors out of grant and account diagnostics. Show only the error code.
    let reply = session
        .execute_payload(payload, MAX_RESULT_ROWS)
        .map_err(|_| "无法读取隧道账户信息。")?;
    rows(reply)
}

fn write(session: &Session, sql: &str, setup: &str) -> Result<u32, String> {
    let sql = batch_statements(&[
        setup,
        sql,
        "SELECT @@warning_count AS harbor_account_warnings",
    ])
    .map_err(|_| "无法准备账户操作语句。")?;
    let payload = session
        .prepare_payload(&sql, true, ACCOUNT_CONTEXT)
        .map_err(|_| "无法编码账户操作请求。")?;
    let uncertain = "无法确认隧道账户操作是否完成。请先核对账户、权限或重新登录。";
    let reply = session.execute_payload(payload, 1).map_err(|_| uncertain)?;
    if let Some(error) = reply.error.as_ref() {
        return Err(format!(
            "账户操作返回数据库错误 {}。请刷新状态并核对账户、权限或服务器策略后再继续。",
            error.code
        ));
    }
    if !reply.sets.first().is_some_and(
        |set| matches!(&set.columns[..], [column] if column.name == "harbor_account_warnings"),
    ) {
        return Err(uncertain.into());
    }
    let response = rows(reply).map_err(|_| uncertain)?;
    if let [row] = response.as_slice()
        && let [Some(warnings)] = row.as_slice()
        && let Ok(warnings) = warnings.parse()
    {
        Ok(warnings)
    } else {
        Err(uncertain.into())
    }
}

fn rows(reply: Reply) -> Result<MetadataRows, String> {
    if let Some(error) = reply.error {
        return Err(format!("账户请求返回数据库错误 {}。", error.code));
    }
    let [set] = <[_; 1]>::try_from(reply.sets).map_err(|_| "隧道没有返回唯一的账户结果集。")?;
    if reply.omitted_sets != 0 || set.truncated {
        return Err("隧道返回的账户信息已被截断。".into());
    }
    set.rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| {
                    value
                        .map(String::from_utf8)
                        .transpose()
                        .map_err(|_| "隧道返回的账户信息包含无效的文本。".into())
                })
                .collect()
        })
        .collect()
}
