use std::sync::{Arc, Weak};

use mysql::{Conn, prelude::Queryable};

use super::{Cell, MAX_RESULT_ROWS, ResultSet, Session, execute_sql, mysql_error};

const SHOW: &str = "SHOW FULL PROCESSLIST";
const MARIA_LIST: &str = "SELECT ID, USER, HOST, DB, COMMAND, TIME, STATE, INFO, QUERY_ID FROM information_schema.PROCESSLIST";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessAction {
    StopQuery,
    Disconnect,
}

impl ProcessAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::StopQuery => "终止查询",
            Self::Disconnect => "断开连接",
        }
    }
}

pub enum ProcessRequest {
    List,
    Stop {
        target: ProcessTarget,
        action: ProcessAction,
    },
}

pub enum ProcessReply {
    Listed(ProcessOverview),
    Stopped { id: u64, action: ProcessAction },
}

pub struct ProcessOverview {
    pub rows: Vec<ProcessRow>,
    pub truncated: bool,
    pub own_id: Option<u64>,
    session: Option<Weak<()>>,
}

#[derive(Clone)]
pub struct ProcessRow {
    pub id: u64,
    pub user: String,
    pub host: String,
    pub database: Option<String>,
    pub command: String,
    pub seconds: Option<u64>,
    pub state: Option<String>,
    pub sql: Option<String>,
    query_id: Option<u64>,
}

pub struct ProcessTarget {
    row: ProcessRow,
    session: Weak<()>,
}

impl ProcessOverview {
    pub fn can_stop(&self, row: &ProcessRow, action: ProcessAction) -> bool {
        self.session.is_some() && Some(row.id) != self.own_id && row.can_stop(action)
    }

    pub fn stop(&self, id: u64, action: ProcessAction) -> Option<ProcessRequest> {
        let row = self.rows.iter().find(|row| row.id == id)?;
        if !self.can_stop(row, action) {
            return None;
        }
        let target = ProcessTarget {
            row: row.clone(),
            session: self.session.clone()?,
        };
        Some(ProcessRequest::Stop { target, action })
    }
}

impl ProcessRow {
    fn can_stop(&self, action: ProcessAction) -> bool {
        self.id != 0
            && !self.host.is_empty()
            && !self.user.is_empty()
            && !matches!(self.user.as_str(), "system user" | "unauthenticated user")
            && !matches!(self.command.as_str(), "Daemon" | "Killed")
            && (action != ProcessAction::StopQuery
                || (self.command != "Sleep"
                    && self.sql.as_ref().is_some_and(|sql| !sql.is_empty())))
    }

    fn matches(&self, current: &Self, action: ProcessAction) -> bool {
        let same_session = self.id == current.id
            && self.user == current.user
            && self.host == current.host
            && self.database == current.database
            && current.can_stop(action);
        same_session
            && (action != ProcessAction::StopQuery
                || (self.command == current.command
                    && self.sql == current.sql
                    && self.query_id == current.query_id
                    && self.seconds <= current.seconds))
    }
}

pub(super) fn native(
    session: &mut Session,
    request: ProcessRequest,
) -> (Result<ProcessReply, String>, bool) {
    let handshake_id = u64::from(session.conn.connection_id());
    let own_id = match session.conn.query_first::<u64, _>("SELECT CONNECTION_ID()") {
        Ok(Some(id)) => id,
        Ok(None) => return (Err("服务器未返回当前连接 ID。".into()), false),
        Err(error) => {
            let lost = error.is_connectivity_error();
            return (Err(mysql_error(error)), lost);
        }
    };
    match request {
        ProcessRequest::List => {
            let (listed, lost) = read_native(&mut session.conn, session.mariadb, None);
            let overview = listed.map(|mut overview| {
                overview.session = Some(Arc::downgrade(&session.identity));
                overview.own_id = Some(own_id);
                ProcessReply::Listed(overview)
            });
            (overview, lost)
        }
        ProcessRequest::Stop { target, action } => {
            let id = target.row.id;
            if id == own_id
                || id == handshake_id
                || !target.session.ptr_eq(&Arc::downgrade(&session.identity))
            {
                return (
                    Err("此进程不属于当前列表可操作的会话。请刷新列表。".into()),
                    false,
                );
            }
            let (checked, lost) = read_native(&mut session.conn, session.mariadb, Some(id));
            let current = match checked {
                Ok(list) => list.rows.into_iter().find(|entry| entry.id == id),
                Err(error) => return (Err(error), lost),
            };
            let Some(current) = current.filter(|row| target.row.matches(row, action)) else {
                return (
                    Err("会话或查询已变化，请刷新列表后重新选择。".into()),
                    false,
                );
            };
            // A MariaDB query ID targets the statement even if the connection has since advanced.
            let sql = match (action, current.query_id) {
                (ProcessAction::StopQuery, Some(query)) => format!("KILL QUERY ID {query}"),
                (ProcessAction::StopQuery, None) => format!("KILL QUERY {id}"),
                (ProcessAction::Disconnect, _) => format!("KILL CONNECTION {id}"),
            };
            match session.conn.query_drop(sql) {
                Ok(()) => (Ok(ProcessReply::Stopped { id, action }), false),
                Err(error) => {
                    let lost = error.is_connectivity_error();
                    let error = mysql_error(error);
                    let message = if lost {
                        format!("连接中断，无法确认服务器是否执行了操作。{error}")
                    } else {
                        error
                    };
                    (Err(message), lost)
                }
            }
        }
    }
}

fn read_native(
    conn: &mut Conn,
    mariadb: bool,
    target_id: Option<u64>,
) -> (Result<ProcessOverview, String>, bool) {
    let sql = match (mariadb, target_id) {
        (true, Some(id)) => format!("{MARIA_LIST} WHERE ID = {id}"),
        (true, None) => MARIA_LIST.into(),
        (false, _) => SHOW.into(),
    };
    let (mut report, mut lost) = execute_sql(conn, &sql, MAX_RESULT_ROWS);
    if mariadb
        && report
            .error
            .as_ref()
            .is_some_and(|error| error.contains("ERROR 1054"))
    {
        (report, lost) = execute_sql(conn, SHOW, MAX_RESULT_ROWS);
    }
    let result = if let Some(error) = report.error {
        Err(error)
    } else {
        report
            .sets
            .into_iter()
            .next()
            .ok_or_else(|| "服务器未返回进程列表。".into())
            .and_then(from_set)
    };
    (result, lost)
}

pub(super) fn http(
    session: &super::http_session::Session,
    request: ProcessRequest,
) -> Result<ProcessReply, String> {
    match request {
        ProcessRequest::List => from_set(session.read_set(SHOW)?).map(ProcessReply::Listed),
        ProcessRequest::Stop { .. } => Err("终止进程需要直连或 SSH 会话。".into()),
    }
}

fn from_set(set: ResultSet) -> Result<ProcessOverview, String> {
    let expected = [
        "Id", "User", "Host", "db", "Command", "Time", "State", "Info",
    ];
    if set.columns.len() < expected.len()
        || !set
            .columns
            .iter()
            .zip(expected)
            .all(|(name, column)| name.eq_ignore_ascii_case(column))
    {
        return Err("服务器返回了无法识别的进程列表。".into());
    }
    let query_id = set
        .columns
        .get(8)
        .is_some_and(|column| column.eq_ignore_ascii_case("query_id"));
    let mut rows = Vec::with_capacity(set.rows.len());
    for row in set.rows {
        let [
            id,
            user,
            host,
            database,
            command,
            seconds,
            state,
            sql,
            extra @ ..,
        ] = row.as_slice()
        else {
            return Err("服务器返回了不完整的进程记录。".into());
        };
        rows.push(ProcessRow {
            id: number(id).ok_or("服务器返回了无法识别的连接 ID。")?,
            user: value(user).unwrap_or_default(),
            host: value(host).unwrap_or_default(),
            database: value(database),
            command: value(command).unwrap_or_default(),
            seconds: number(seconds),
            state: value(state),
            sql: value(sql),
            query_id: if query_id {
                extra.first().and_then(number).filter(|id| *id != 0)
            } else {
                None
            },
        });
    }
    rows.sort_by_key(|row| row.id);
    Ok(ProcessOverview {
        rows,
        truncated: set.truncated,
        own_id: None,
        session: None,
    })
}

fn number(cell: &Cell) -> Option<u64> {
    match cell {
        Cell::Text(value) => value.parse().ok(),
        Cell::Binary(value) => std::str::from_utf8(value).ok()?.parse().ok(),
        Cell::Null => None,
    }
}

fn value(cell: &Cell) -> Option<String> {
    match cell {
        Cell::Text(value) => Some(value.clone()),
        Cell::Binary(value) => std::str::from_utf8(value).ok().map(str::to_owned),
        Cell::Null => None,
    }
}
