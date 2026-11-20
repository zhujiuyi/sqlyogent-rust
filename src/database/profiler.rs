use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use mysql::prelude::{FromRow, Queryable};
use mysql::{AccessMode, Conn, Error, OptsBuilder, TxOpts};
use serde::{Deserialize, Serialize};

use super::{
    CONTROL_TIMEOUT, CancellationState, Cell, QueryReport, Session, cell, execute_cancellable,
    mysql_error,
};

const SESSION_STATUS: &str = "SHOW SESSION STATUS";
const GLOBAL_STATUS: &str = "SHOW GLOBAL STATUS WHERE Variable_name IN (
    'Innodb_buffer_pool_read_requests', 'Innodb_buffer_pool_reads', 'Innodb_data_reads',
    'Innodb_data_writes', 'Innodb_rows_read', 'Qcache_hits', 'Qcache_free_blocks',
    'Qcache_free_memory', 'Qcache_queries_in_cache', 'Qcache_total_blocks')";
const PLAN_ROWS: usize = 512;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct ProfileOptions {
    pub explain: bool,
    pub extended: bool,
    pub status: bool,
    pub stages: bool,
}

impl Default for ProfileOptions {
    fn default() -> Self {
        Self {
            explain: true,
            extended: true,
            status: true,
            stages: true,
        }
    }
}

pub struct QueryProfile {
    pub sql: String,
    pub elapsed: Duration,
    pub error: Option<String>,
    pub sections: Vec<ProfileSection>,
    pub notices: Vec<String>,
}

pub struct ProfileSection {
    pub title: &'static str,
    pub help: &'static str,
    pub data: Result<ProfileTable, String>,
}

pub struct ProfileTable {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub summary: String,
}

impl ProfileTable {
    fn new<const N: usize>(columns: [&str; N], rows: Vec<Vec<String>>, summary: String) -> Self {
        Self {
            columns: columns.map(str::to_owned).into(),
            rows,
            summary,
        }
    }
}

#[derive(Default)]
struct Capture {
    disconnected: bool,
}

impl Capture {
    fn read<T>(&mut self, probe: impl FnOnce() -> mysql::Result<T>) -> Result<T, String> {
        if self.disconnected {
            return Err("当前数据库连接已断开。".into());
        }
        probe().map_err(|error| {
            self.disconnected = error.is_connectivity_error();
            mysql_error(error)
        })
    }
}

pub(super) fn run(
    session: &mut Session,
    id: u64,
    sql: String,
    limit: usize,
    options: ProfileOptions,
    cancellation: &Mutex<CancellationState>,
) -> (QueryReport, QueryProfile, bool) {
    let preserve_diagnostics = crate::sql_tools::needs_unmodified_query_diagnostics(&sql);
    let options = if preserve_diagnostics {
        ProfileOptions {
            explain: false,
            extended: false,
            status: false,
            stages: false,
        }
    } else {
        options
    };
    let mut capture = Capture::default();
    let stage_settings = options.stages.then(|| {
        capture
            .read(|| StageSettings::read(&mut session.conn))?
            .ok_or_else(|| "此服务器未提供 SHOW PROFILE 所需的会话变量。".to_owned())
    });
    let stage_enabled = stage_settings.as_ref().map(|settings| {
        settings
            .as_ref()
            .map_err(String::clone)
            .and_then(|settings| {
                capture.read(|| settings.enable(&mut session.conn, options.status))
            })
    });
    let global_before = options
        .status
        .then(|| capture.read(|| pairs(&mut session.conn, GLOBAL_STATUS)));
    let before = options
        .status
        .then(|| capture.read(|| pairs(&mut session.conn, SESSION_STATUS)));

    let (report, disconnected, tagged_sql) =
        execute_cancellable(session, id, &sql, limit, cancellation, None);
    capture.disconnected |= disconnected;
    let mut profile = QueryProfile {
        sql,
        elapsed: report.elapsed,
        error: report.error.clone(),
        sections: Vec::new(),
        notices: preserve_diagnostics
            .then(|| "为保留会话诊断，当前语句只记录客户端耗时。".to_owned())
            .into_iter()
            .collect(),
    };

    let after = options
        .status
        .then(|| capture.read(|| pairs(&mut session.conn, SESSION_STATUS)));
    let stages = stage_enabled.map(|enabled| {
        enabled.and_then(|()| {
            capture
                .read(|| {
                    stage_table(
                        &mut session.conn,
                        &profile.sql,
                        tagged_sql.as_deref(),
                        options.status,
                    )
                })?
                .ok_or_else(|| "服务器未返回本次查询的阶段记录。".to_owned())
        })
    });
    if let Some(Ok(settings)) = &stage_settings
        && !capture.disconnected
        && let Err(error) = capture.read(|| settings.restore(&mut session.conn, options.status))
    {
        profile
            .notices
            .push(format!("无法还原当前会话的 profiling 设置。{error}"));
    }
    let global_after = options
        .status
        .then(|| capture.read(|| pairs(&mut session.conn, GLOBAL_STATUS)));

    if options.explain {
        let explain = if report.error.is_some() {
            Err("查询未成功执行。".into())
        } else {
            capture
                .read(|| PlanSession::read(&mut session.conn))
                .and_then(|context| context.explain(session, &profile.sql, options.extended))
        };
        let (plan, notes) = match explain {
            Ok((plan, notes)) => (Ok(plan), notes),
            Err(error) => (Err(error.clone()), Err(error)),
        };
        profile.sections.push(ProfileSection {
            title: "执行计划",
            help: "从沿用当前数据库和 SQL 模式的独立只读连接获取优化器估算。临时表等会话私有状态在分析连接中不可见。",
            data: plan,
        });
        if options.extended {
            profile.sections.push(ProfileSection {
                title: "优化器信息",
                help: "执行计划生成后，服务器给出的 SQL 改写和提示。",
                data: notes,
            });
        }
    }
    if let (Some(before), Some(after), Some(global_before), Some(global_after)) =
        (before, after, global_before, global_after)
    {
        profile.sections.push(ProfileSection {
            title: "会话状态变化",
            help: "两次 SHOW SESSION STATUS 快照之间的变化。采集快照也可能增加临时表等计数。",
            data: before.and_then(|before| after.map(|after| session_table(&before, after))),
        });
        profile.sections.push(ProfileSection {
            title: "全局状态",
            help: "数据库服务器的 InnoDB 和查询缓存统计，可能同时包含其他连接的活动。",
            data: global_before
                .and_then(|before| global_after.map(|after| global_table(&before, after))),
        });
    }
    if let Some(data) = stages {
        profile.sections.push(ProfileSection {
            title: "执行阶段",
            help: "按同名阶段汇总服务器返回的耗时，依耗时从高到低排列。",
            data,
        });
    }
    (report, profile, capture.disconnected)
}

fn typed_rows<T: FromRow>(conn: &mut impl Queryable, sql: &str) -> mysql::Result<Vec<T>> {
    conn.query_opt::<T, _>(sql)?
        .into_iter()
        .collect::<Result<_, _>>()
        .map_err(Error::from)
}

fn pairs(conn: &mut Conn, sql: &str) -> mysql::Result<BTreeMap<String, String>> {
    Ok(typed_rows::<(String, String)>(conn, sql)?
        .into_iter()
        .collect())
}

struct StageSettings {
    enabled: bool,
    history: u16,
}

impl StageSettings {
    fn read(conn: &mut Conn) -> mysql::Result<Option<Self>> {
        let settings = pairs(
            conn,
            "SHOW SESSION VARIABLES WHERE Variable_name IN ('profiling', 'profiling_history_size')",
        )?;
        Ok(settings
            .get("profiling")
            .zip(settings.get("profiling_history_size"))
            .and_then(|(enabled, history)| {
                Some(Self {
                    enabled: enabled.eq_ignore_ascii_case("ON") || enabled == "1",
                    history: history.parse().ok()?,
                })
            }))
    }

    fn requested_history(&self, session_status: bool) -> u16 {
        self.history.max(if session_status { 2 } else { 1 })
    }

    fn enable(&self, conn: &mut Conn, session_status: bool) -> mysql::Result<()> {
        let history = self.requested_history(session_status);
        if self.enabled && history == self.history {
            return Ok(());
        }
        conn.query_drop(format!(
            "SET SESSION profiling_history_size = {history}, SESSION profiling = 1"
        ))
    }

    fn restore(&self, conn: &mut Conn, session_status: bool) -> mysql::Result<()> {
        if self.enabled && self.requested_history(session_status) == self.history {
            return Ok(());
        }
        conn.query_drop(format!(
            "SET SESSION profiling = {}, SESSION profiling_history_size = {}",
            u8::from(self.enabled),
            self.history,
        ))
    }
}

fn stage_table(
    conn: &mut Conn,
    sql: &str,
    tagged_sql: Option<&str>,
    after_status: bool,
) -> mysql::Result<Option<ProfileTable>> {
    let mut profiles = typed_rows::<(u64, String, Vec<u8>)>(conn, "SHOW PROFILES")?;
    profiles.sort_unstable_by_key(|(id, _, _)| *id);
    // SHOW PROFILES omits itself. The after-status snapshot can occupy the newest ID.
    let mut latest = profiles.into_iter().rev();
    if after_status
        && !latest
            .next()
            .is_some_and(|(_, _, statement)| statement == SESSION_STATUS.as_bytes())
    {
        return Ok(None);
    }
    let Some((id, total, statement)) = latest.next() else {
        return Ok(None);
    };
    // Server builds may truncate the source text of a profile record.
    let matches = |sent: &str| sent.trim_ascii_start().as_bytes().starts_with(&statement);
    if statement.is_empty() || (!matches(sql) && !tagged_sql.is_some_and(matches)) {
        return Ok(None);
    }
    let rows = typed_rows::<(String, String)>(conn, &format!("SHOW PROFILE FOR QUERY {id}"))?;
    if rows.is_empty() {
        return Ok(None);
    }
    let mut stages = BTreeMap::<String, (Duration, u32)>::new();
    for (name, seconds) in rows {
        let Some(duration) = parse_duration(&seconds) else {
            return Ok(None);
        };
        let aggregate = stages.entry(name).or_default();
        aggregate.0 += duration;
        aggregate.1 += 1;
    }
    let mut stages: Vec<_> = stages.into_iter().collect();
    stages.sort_by(|a, b| b.1.0.cmp(&a.1.0));
    let summary = parse_duration(&total)
        .map(|total| format!("服务器总耗时 {:.3} 毫秒", total.as_secs_f64() * 1_000.0))
        .unwrap_or_default();
    let rows = stages
        .into_iter()
        .map(|(name, (duration, count))| {
            vec![
                name,
                format!("{:.3}", duration.as_secs_f64() * 1_000.0),
                count.to_string(),
            ]
        })
        .collect();
    Ok(Some(ProfileTable::new(
        ["阶段", "耗时（毫秒）", "次数"],
        rows,
        summary,
    )))
}

fn parse_duration(seconds: &str) -> Option<Duration> {
    let seconds = seconds.parse().ok()?;
    Duration::try_from_secs_f64(seconds).ok()
}

fn session_table(
    before: &BTreeMap<String, String>,
    after: BTreeMap<String, String>,
) -> ProfileTable {
    let rows: Vec<_> = after
        .into_iter()
        .filter_map(|(name, value)| {
            if !is_session_counter(&name) {
                return None;
            }
            let old = before.get(&name)?;
            if old == &value {
                return None;
            }
            let description = status_description(&name);
            Some(vec![
                name,
                old.clone(),
                value.clone(),
                delta(old, &value),
                description.into(),
            ])
        })
        .collect();
    let summary = if rows.is_empty() {
        "未检测到会话计数变化。"
    } else {
        ""
    };
    ProfileTable::new(
        ["变量", "执行前", "执行后", "变化", "说明"],
        rows,
        summary.into(),
    )
}

fn is_session_counter(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    (name.starts_with("com_") && name != "com_show_status" || name.starts_with("handler_"))
        || ["created_tmp_", "rows_", "select_", "sort_"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

fn global_table(
    before: &BTreeMap<String, String>,
    after: BTreeMap<String, String>,
) -> ProfileTable {
    let rows: Vec<_> = after
        .into_iter()
        .map(|(name, value)| {
            let gauge = matches!(
                name.to_ascii_lowercase().as_str(),
                "qcache_free_blocks"
                    | "qcache_free_memory"
                    | "qcache_queries_in_cache"
                    | "qcache_total_blocks"
            );
            let (previous, change) = before
                .get(&name)
                .filter(|_| !gauge)
                .map(|previous| (previous.clone(), delta(previous, &value)))
                .unwrap_or_else(|| ("—".into(), "—".into()));
            vec![name, previous, value, change]
        })
        .collect();
    let summary = if rows.is_empty() {
        "服务器没有返回相应的全局变量。"
    } else {
        ""
    };
    ProfileTable::new(["变量", "执行前", "执行后", "变化"], rows, summary.into())
}

fn delta(before: &str, after: &str) -> String {
    before
        .parse::<i128>()
        .ok()
        .zip(after.parse::<i128>().ok())
        .and_then(|(old, new)| new.checked_sub(old))
        .map_or_else(|| "—".into(), |change| format!("{change:+}"))
}

fn status_description(name: &str) -> &'static str {
    match name.to_ascii_lowercase().as_str() {
        "com_select" => "执行的 SELECT 数量",
        "created_tmp_tables" => "创建的内部临时表",
        "created_tmp_disk_tables" => "创建的磁盘内部临时表",
        "handler_read_key" => "按索引键读取的请求",
        "handler_read_next" => "按索引顺序读取下一行的请求",
        "handler_read_rnd_next" => "按数据文件顺序读取下一行的请求",
        "rows_sent" => "服务器发送的行数",
        "rows_examined" => "服务器检查的行数",
        "select_full_join" => "因未使用索引而扫描表的连接",
        "select_scan" => "扫描第一张表的查询",
        "sort_rows" => "排序的行数",
        _ => "",
    }
}

struct PlanSession {
    database: Option<String>,
    settings: BTreeMap<String, String>,
}

impl PlanSession {
    fn read(conn: &mut Conn) -> mysql::Result<Self> {
        Ok(Self {
            database: conn
                .query_first_opt::<Option<String>, _>("SELECT DATABASE()")?
                .transpose()?
                .flatten(),
            settings: pairs(
                conn,
                "SHOW SESSION VARIABLES WHERE Variable_name IN (
                'sql_mode', 'optimizer_switch', 'character_set_client', 'collation_connection')",
            )?,
        })
    }

    fn explain(
        self,
        session: &Session,
        sql: &str,
        extended: bool,
    ) -> Result<(ProfileTable, Result<ProfileTable, String>), String> {
        let mode = self
            .settings
            .get("sql_mode")
            .map(String::as_str)
            .unwrap_or_default();
        if !crate::sql_tools::can_explain_profile_select(sql, mode) {
            return Err("此查询含有会话变量、写入目标或可能带副作用的函数。".into());
        }
        self.explain_read_only(session, sql, extended)
            .map_err(mysql_error)
    }

    fn explain_read_only(
        self,
        session: &Session,
        sql: &str,
        extended: bool,
    ) -> mysql::Result<(ProfileTable, Result<ProfileTable, String>)> {
        // EXPLAIN can evaluate derived expressions. Keep it out of the editor's transaction.
        let mut conn = Conn::new(
            OptsBuilder::from_opts(session.opts.clone())
                .db_name(self.database)
                .tcp_connect_timeout(Some(CONTROL_TIMEOUT))
                .read_timeout(Some(CONTROL_TIMEOUT))
                .write_timeout(Some(CONTROL_TIMEOUT)),
        )?;
        for (name, value) in &self.settings {
            conn.exec_drop(format!("SET SESSION {name} = ?"), (value,))?;
        }
        let mut tx =
            conn.start_transaction(TxOpts::default().set_access_mode(Some(AccessMode::ReadOnly)))?;
        let sql = sql.trim_start_matches('\u{feff}');
        let plan = if extended {
            match explain_rows(&mut tx, &format!("EXPLAIN EXTENDED {sql}")) {
                Err(Error::MySqlError(error)) if matches!(error.code, 1064 | 1235) => {
                    // Newer MySQL provides rewritten-SQL notes after plain EXPLAIN.
                    explain_rows(&mut tx, &format!("EXPLAIN {sql}"))?
                }
                result => result?,
            }
        } else {
            explain_rows(&mut tx, &format!("EXPLAIN {sql}"))?
        };
        let notes = if extended {
            typed_rows::<(String, String, String)>(&mut tx, "SHOW WARNINGS LIMIT 512")
                .map(|rows| {
                    let rows: Vec<_> = rows
                        .into_iter()
                        .map(|(level, code, message)| vec![level, code, message])
                        .collect();
                    let summary = if rows.is_empty() {
                        "服务器没有返回优化器提示。"
                    } else {
                        ""
                    };
                    ProfileTable::new(["级别", "代码", "内容"], rows, summary.into())
                })
                .map_err(mysql_error)
        } else {
            Err(String::new())
        };
        Ok((plan, notes))
    }
}

fn explain_rows(conn: &mut impl Queryable, sql: &str) -> mysql::Result<ProfileTable> {
    // Preparing the plan with the server enforces the single-statement constraint.
    let statement = conn.prep(sql)?;
    let mut result = conn.exec_iter(&statement, ())?;
    let columns = result
        .columns()
        .as_ref()
        .iter()
        .map(|column| column.name_str().into_owned())
        .collect();
    let mut rows = Vec::new();
    for row in result.by_ref().take(PLAN_ROWS + 1) {
        rows.push(
            row?.unwrap()
                .into_iter()
                .map(|value| match cell(value, false) {
                    Cell::Text(value) => value,
                    Cell::Null => "NULL".into(),
                    Cell::Binary(value) => String::from_utf8_lossy(&value).into_owned(),
                })
                .collect(),
        );
    }
    let summary = if rows.len() > PLAN_ROWS {
        rows.truncate(PLAN_ROWS);
        format!("仅显示前 {PLAN_ROWS} 行执行计划。")
    } else {
        String::new()
    };
    Ok(ProfileTable {
        columns,
        rows,
        summary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::{Command, ConnectionConfig, Response, Worker};

    #[test]
    #[ignore = "需要 HARBOR_PROFILER_TEST_PORT 指向本机临时 MariaDB"]
    fn records_plan_status_stages_and_original_results_on_the_worker() {
        let port = std::env::var("HARBOR_PROFILER_TEST_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let worker = Worker::new();
        let mut id = 0;
        let mut request = |command| {
            id += 1;
            worker.send(id, command).unwrap();
            let event = worker.recv().unwrap();
            assert_eq!(event.id, id);
            assert!(event.connected);
            event.result.unwrap()
        };
        assert!(matches!(
            request(Command::Connect(ConnectionConfig {
                port,
                ..Default::default()
            })),
            Response::Connected(_)
        ));
        request(Command::Query {
            sql: "SET SESSION profiling = 0, SESSION profiling_history_size = 1, @profile_runs = 0"
                .into(),
            limit: 10,
        });
        let options = ProfileOptions::default();
        let sql = format!(
            "SELECT /* {} */ COUNT(*) AS n FROM (SELECT 1 AS i UNION ALL SELECT 2) AS numbers",
            "profile source ".repeat(30)
        );
        let Response::Profiled {
            report, profile, ..
        } = request(Command::QueryProfiled {
            sql: sql.clone(),
            limit: 10,
            options,
            open_profile: true,
        })
        else {
            panic!("缺少查询分析结果")
        };
        assert_eq!(report.error, None);
        assert_eq!(report.sets[0].rows[0], [Cell::Text("2".into())]);
        assert_eq!(profile.sql, sql);
        assert_eq!(profile.elapsed, report.elapsed);
        let execution = report.diagnostics.execution.unwrap();
        let result_elapsed = report.diagnostics.results[0].elapsed;
        assert!(execution <= result_elapsed && result_elapsed <= profile.elapsed);
        let section = |title| {
            &profile
                .sections
                .iter()
                .find(|item| item.title == title)
                .unwrap()
                .data
        };
        let plan = section("执行计划").as_ref().unwrap();
        assert!(plan.columns.iter().any(|label| label == "select_type"));
        assert!(!plan.rows.is_empty());
        assert!(
            section("优化器信息")
                .as_ref()
                .unwrap()
                .rows
                .iter()
                .any(|row| row[1] == "1003")
        );
        assert!(
            section("会话状态变化")
                .as_ref()
                .unwrap()
                .rows
                .iter()
                .any(|row| row[0] == "Com_select" && row[3] == "+1")
        );
        assert!(
            section("全局状态")
                .as_ref()
                .unwrap()
                .rows
                .iter()
                .any(|row| row[0] == "Innodb_buffer_pool_reads")
        );
        let stages = section("执行阶段").as_ref().unwrap();
        assert!(!stages.rows.is_empty());
        assert!(stages.summary.contains("服务器总耗时"));

        let Response::Profiled {
            report, profile, ..
        } = request(Command::QueryProfiled {
            sql: "SELECT @profile_runs := @profile_runs + 1".into(),
            limit: 10,
            options,
            open_profile: false,
        })
        else {
            panic!("缺少带会话变量的查询分析")
        };
        assert_eq!(report.error, None);
        assert_eq!(report.sets[0].rows[0], [Cell::Text("1".into())]);
        assert!(
            profile
                .sections
                .iter()
                .find(|section| section.title == "执行计划")
                .unwrap()
                .data
                .is_err()
        );

        let Response::Queried(report) = request(Command::Query {
            sql: "SELECT @profile_runs, @@SESSION.profiling, @@SESSION.profiling_history_size"
                .into(),
            limit: 10,
        }) else {
            panic!("缺少会话状态")
        };
        assert_eq!(
            report.sets[0].rows[0],
            [
                Cell::Text("1".into()),
                Cell::Text("0".into()),
                Cell::Text("1".into())
            ]
        );
        for (sql, value) in [
            (
                "SELECT SQL_CALC_FOUND_ROWS i FROM (SELECT 1 AS i UNION ALL SELECT 2) AS numbers LIMIT 1",
                "1",
            ),
            ("SELECT FOUND_ROWS()", "2"),
        ] {
            let Response::Profiled {
                report, profile, ..
            } = request(Command::QueryProfiled {
                sql: sql.into(),
                limit: 10,
                options,
                open_profile: false,
            })
            else {
                panic!("缺少诊断辅助查询的耗时")
            };
            assert_eq!(report.error, None);
            assert_eq!(report.sets[0].rows[0], [Cell::Text(value.into())]);
            assert!(profile.sections.is_empty());
        }
    }

    #[test]
    #[ignore = "需要 HARBOR_PROFILER_TEST_PORT 指向本机临时 MariaDB"]
    fn profiles_and_stops_queries_when_server_identity_is_unavailable() {
        let port = std::env::var("HARBOR_PROFILER_TEST_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let (mut session, _) = crate::database::connect(
            ConnectionConfig {
                port,
                ..Default::default()
            },
            &Default::default(),
            true,
        )
        .unwrap();
        session.query_identity = None;
        let cancellation = Mutex::default();
        let options = ProfileOptions::default();
        let sql = "SELECT 17 AS legacy_profile";
        let (report, profile, disconnected) =
            run(&mut session, 1, sql.into(), 10, options, &cancellation);
        assert!(!disconnected);
        assert_eq!(report.error, None);
        assert_eq!(report.sets[0].rows[0], [Cell::Text("17".into())]);
        assert_eq!(profile.sql, sql);
        let stages = profile
            .sections
            .iter()
            .find(|section| section.title == "执行阶段")
            .unwrap();
        assert!(!stages.data.as_ref().unwrap().rows.is_empty());

        let mut observer = Conn::new(session.opts.clone()).unwrap();
        let session_id = session.conn.connection_id();
        let (report, profile, disconnected) = std::thread::scope(|scope| {
            let query = scope.spawn(|| {
                run(
                    &mut session,
                    2,
                    "SELECT 1 + SLEEP(6)".into(),
                    10,
                    options,
                    &cancellation,
                )
            });
            let started = std::time::Instant::now();
            loop {
                let sleeping = observer
                    .exec_first::<u32, _, _>(
                        "SELECT ID FROM information_schema.PROCESSLIST
                         WHERE ID = ? AND STATE = 'User sleep'
                             AND INFO LIKE '/* harbor_sql_query_% */ SELECT 1 + SLEEP(6)'",
                        (session_id,),
                    )
                    .unwrap();
                if sleeping.is_some() {
                    let target = cancellation.lock().unwrap().active.clone().unwrap();
                    crate::database::attempt_cancel(&target, &cancellation).unwrap();
                    break;
                }
                assert!(started.elapsed() < Duration::from_secs(3));
                std::thread::sleep(Duration::from_millis(10));
            }
            query.join().unwrap()
        });
        assert!(!disconnected);
        assert!(report.error.is_some());
        assert_eq!(profile.sql, "SELECT 1 + SLEEP(6)");
        assert_eq!(
            session
                .conn
                .query_first::<u32, _>("SELECT CONNECTION_ID()")
                .unwrap(),
            Some(session_id)
        );
    }
}
