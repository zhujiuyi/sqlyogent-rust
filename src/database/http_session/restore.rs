use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::restore::{
    ExplicitClientEncoding, GeneratedClientEncoding, MAX_HTTP_SCRIPT, RestoreEncoding,
    RestoreProgress, byte_position_label, prepare_restore_inputs, read_http_script,
};
use crate::sql_encoding::SqlDumpCharset;
use crate::sql_tools::{StatementReader, set_assignments};

use super::super::quote_identifier;
use super::{Session, text, without_comments};

const RESTORE_MARKER: &str = "__harbor_restore_complete";
const CYCLE_DATA_CHANGED: &str = "HARBOR_COPY_CYCLE_DATA_CHANGED";
const CYCLE_DATA_INVALID: &str = "HARBOR_COPY_CYCLE_DATA_INVALID";

#[derive(Clone, Copy, Default)]
pub(in crate::database) struct RestoreSetup<'a> {
    pub guards: &'a [String],
    pub preparation: &'a [String],
    pub completion: &'a [String],
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use mysql::{Conn, prelude::Queryable};

    use super::super::super::{Command, ConnectionConfig, Response, Worker, transfer};
    use super::super::workflow_test::{TestGateway, fixture_database};
    use super::*;
    use crate::sql_encoding::encode_sql;

    #[test]
    #[ignore = "requires a disposable MariaDB via HARBOR_HTTP_FIXTURE_PORT"]
    fn explicit_legacy_restore_replays_sqlyog_gbk_over_direct_and_http() {
        let (port, opts) = fixture_database();
        let mut sql = Conn::new(opts.clone()).unwrap();
        let direct = format!("harbor_explicit_gbk_{}_direct", std::process::id());
        let http = format!("harbor_explicit_gbk_{}_http", std::process::id());
        for schema in [&direct, &http] {
            sql.query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
                .unwrap();
        }
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let executable = std::env::current_exe().unwrap();
            let folder = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
            let script = "/* SQLyog 手工转储 */\n/*!40101 SET NAMES gbk */;\n\
                /*!40101 SET SQL_MODE=''*/;\n\
                CREATE TABLE sample (id INT PRIMARY KEY, body VARCHAR(80));\n\
                INSERT INTO sample VALUES (1,'港湾 SET NAMES latin1');\n";
            let tail = "/*!40101 SET NAMES gbk */;\nINSERT INTO sample VALUES (2,'备份');\n";
            let write = |path: &Path, text: &str| {
                let mut bytes = Vec::new();
                encode_sql(SqlDumpCharset::Gbk, text.as_bytes(), &mut bytes).unwrap();
                std::fs::write(path, bytes).unwrap();
            };
            let whole = folder.path().join("整库.sql");
            let parts = folder.path().join("分对象");
            std::fs::create_dir(&parts).unwrap();
            write(&whole, &format!("{script}{tail}"));
            write(&parts.join("00000001-schema.sql"), script);
            write(&parts.join("00000002-data.sql"), tail);
            std::fs::write(
                parts.join("restore-order.json"),
                r#"{"version":1,"files":["00000001-schema.sql","00000002-data.sql"]}"#,
            )
            .unwrap();

            let gateway = TestGateway::new(opts);
            let config = ConnectionConfig {
                port,
                ..Default::default()
            };
            for (connect, schema, path) in [
                (Command::Connect(config.clone()), &direct, whole),
                (
                    Command::ConnectHttp(config, gateway.tunnel.clone()),
                    &http,
                    parts,
                ),
            ] {
                let worker = Worker::new();
                worker.send(1, connect).unwrap();
                let receive = || {
                    worker
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap()
                        .result
                        .unwrap()
                };
                assert!(matches!(receive(), Response::Connected(_)));
                worker
                    .send(
                        2,
                        Command::RestoreEncoded {
                            schema: schema.clone(),
                            path,
                            charset: SqlDumpCharset::Gbk,
                        },
                    )
                    .unwrap();
                assert!(matches!(receive(), Response::Restored(6)));
                let rows: Vec<String> = sql
                    .query(format!("SELECT body FROM {schema}.sample ORDER BY id"))
                    .unwrap();
                assert_eq!(rows, ["港湾 SET NAMES latin1", "备份"]);
            }
            gateway.stop();
        }));
        for schema in [&http, &direct] {
            sql.query_drop(format!("DROP DATABASE IF EXISTS {schema}"))
                .unwrap();
        }
        if let Err(error) = outcome {
            std::panic::resume_unwind(error);
        }
    }

    #[test]
    fn marked_restore_http_changes_only_tagged_top_level_generator_commands() {
        let encoding = SqlDumpCharset::Gbk;
        let generated = GeneratedClientEncoding::new(encoding);
        let tag = generated.tagged_line();
        let header = format!("{}{tag}", transfer::DUMP_CLIENT_STATE);
        let literal = format!("SELECT '正文\n{header}仍在文本内'");
        let input = format!("{header}SELECT '甲';\n{tag}{literal};\n{header}SELECT '乙';\n",);
        let version = ServerVersion::parse("11.4.13-MariaDB").unwrap();
        let (batch, count) = prepare(
            &input,
            "",
            version,
            Modes {
                current: String::new(),
                global: "",
                variables: HashMap::new(),
            },
            RestoreEncoding::Marked(encoding),
            RestoreSetup::default(),
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(count, 8);
        assert_eq!(batch.matches("SET NAMES utf8mb4").count(), 2);
        assert_eq!(batch.matches("SET NAMES gbk").count(), 2);
        assert!(batch.contains(&literal));
    }

    #[test]
    fn http_preflight_offsets_count_the_original_bom_and_utf8_bytes() {
        let input = "\u{feff}SELECT '甲';\r\n  SELECT 2;";
        let version = ServerVersion::parse("11.4.13-MariaDB").unwrap();
        let mut offsets = Vec::new();
        let (_, statement_count) = prepare(
            input,
            "",
            version,
            Modes {
                current: String::new(),
                global: "",
                variables: HashMap::new(),
            },
            RestoreEncoding::Plain,
            RestoreSetup::default(),
            |offset, index| {
                offsets.push((offset, index));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(statement_count, 2);
        assert_eq!(offsets, [(3, 0), (input.find("SELECT 2").unwrap(), 1)]);
    }

    #[test]
    fn http_guarded_copy_validates_and_commits_before_the_completion_marker() {
        let guard = "SET @harbor_schema_target = 'DO 0'".to_owned();
        let preparation = [
            "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ".into(),
            "START TRANSACTION".into(),
            "SET SESSION foreign_key_checks = 0".into(),
        ];
        let completion = [
            format!("SET @harbor_schema_target = IF(TRUE, 'DO 0', '{CYCLE_DATA_INVALID}')"),
            "PREPARE harbor_schema_target FROM @harbor_schema_target".into(),
            "DEALLOCATE PREPARE harbor_schema_target".into(),
            "COMMIT".into(),
        ];
        let inserted = "INSERT INTO sample VALUES (1, '目标记录')";
        let (batch, count) = prepare(
            &format!("{inserted};"),
            "test_db",
            ServerVersion::parse("11.4.13-MariaDB").unwrap(),
            Modes {
                current: String::new(),
                global: "",
                variables: HashMap::new(),
            },
            RestoreEncoding::Plain,
            RestoreSetup {
                guards: std::slice::from_ref(&guard),
                preparation: &preparation,
                completion: &completion,
            },
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(count, 1);
        let marker = format!("SELECT 1 AS {RESTORE_MARKER}");
        let mut remaining = batch.as_str();
        for statement in std::iter::once(guard.as_str())
            .chain(preparation.iter().map(String::as_str))
            .chain(["USE `test_db`", inserted])
            .chain(completion.iter().map(String::as_str))
            .chain([marker.as_str()])
        {
            let (_, rest) = remaining.split_once(statement).unwrap();
            remaining = rest;
        }
    }

    #[test]
    #[ignore = "requires a disposable MariaDB via HARBOR_HTTP_FIXTURE_PORT"]
    fn restores_ordered_object_files_over_multiple_http_requests() {
        let (port, opts) = fixture_database();
        let mut sql = Conn::new(opts.clone()).unwrap();
        let source = format!("harbor_http_restore_{}", std::process::id());
        let target = format!("{source}_copy");
        for schema in [&source, &target] {
            sql.query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
                .unwrap();
        }
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sql.select_db(&source).unwrap();
            sql.query_drop(
                "CREATE TABLE root_row (id INT PRIMARY KEY, body VARCHAR(32)) ENGINE=InnoDB",
            )
            .unwrap();
            sql.query_drop(
                "CREATE TABLE child_row (id INT PRIMARY KEY, parent_id INT,
                FOREIGN KEY (parent_id) REFERENCES root_row(id)) ENGINE=InnoDB",
            )
            .unwrap();
            sql.query_drop("INSERT INTO root_row VALUES (1,'目录还原')")
                .unwrap();
            sql.query_drop("INSERT INTO child_row VALUES (2,1)")
                .unwrap();
            let gateway = TestGateway::new(opts);
            let (session, _) = Session::open(
                ConnectionConfig {
                    port,
                    ..Default::default()
                },
                gateway.tunnel.clone(),
            )
            .unwrap();
            let folder = tempfile::tempdir().unwrap();
            let summary = transfer::dump_over_http(
                &session,
                &source,
                folder.path(),
                transfer::DumpSettings {
                    file_per_object: true,
                    disable_foreign_key_checks: false,
                    ..Default::default()
                },
                |_| Ok(()),
            )
            .unwrap();
            let published = summary.output_directory.unwrap();
            let mut submissions = 0;
            let mut last = false;
            let restored = session
                .restore_with_progress(&target, &published, None, |status| {
                    if status.finishing && !last {
                        submissions += 1;
                    }
                    last = status.finishing;
                    Ok(())
                })
                .unwrap();
            assert!(restored > 2 && submissions >= 2);
            assert_eq!(sql.query_first::<String,_>(format!(
                "SELECT r.body FROM {target}.child_row c JOIN {target}.root_row r ON r.id=c.parent_id"
            )).unwrap().as_deref(), Some("目录还原"));
            gateway.stop();
        }));
        for schema in [&target, &source] {
            sql.query_drop(format!("DROP DATABASE IF EXISTS {schema}"))
                .unwrap();
        }
        if let Err(error) = outcome {
            std::panic::resume_unwind(error);
        }
    }
}

impl Session {
    #[cfg(test)]
    pub(super) fn restore(&self, schema: &str, path: &Path) -> Result<usize, String> {
        self.restore_with_progress(schema, path, None, |_| Ok(()))
    }

    pub(in crate::database) fn restore_with_progress(
        &self,
        schema: &str,
        path: &Path,
        selected: Option<SqlDumpCharset>,
        progress: impl FnMut(RestoreProgress) -> Result<(), String>,
    ) -> Result<usize, String> {
        self.restore_guarded(schema, path, selected, RestoreSetup::default(), progress)
    }

    pub(in crate::database) fn restore_guarded(
        &self,
        schema: &str,
        path: &Path,
        selected: Option<SqlDumpCharset>,
        setup: RestoreSetup<'_>,
        mut progress: impl FnMut(RestoreProgress) -> Result<(), String>,
    ) -> Result<usize, String> {
        let (sources, mut state) =
            prepare_restore_inputs(path, selected, Some(MAX_HTTP_SCRIPT), &mut progress)?;
        if !setup.completion.is_empty() && sources.len() != 1 {
            return Err("事务式 HTTP 复制需要通过一次请求提交。".into());
        }
        progress(state)?;
        let context = self.read_set("SELECT VERSION(), @@SESSION.sql_mode, @@GLOBAL.sql_mode")?;
        let row = context.rows.first().ok_or("无法读取隧道会话 SQL 模式。")?;
        let version = text(row.first())
            .and_then(ServerVersion::parse)
            .ok_or("无法读取隧道数据库版本。")?;
        let initial_mode = text(row.get(1)).ok_or("无法读取隧道会话 SQL 模式。")?;
        let global_mode = text(row.get(2)).ok_or("无法读取数据库的默认 SQL 模式。")?;
        for source in &sources {
            state.finishing = false;
            progress(state)?;
            let run = (|| {
                let script = read_http_script(source, || progress(state))?;
                let modes = Modes {
                    current: initial_mode.to_owned(),
                    global: global_mode,
                    variables: HashMap::new(),
                };
                let (script, count) = prepare(
                    &script,
                    schema,
                    version,
                    modes,
                    source.encoding,
                    setup,
                    |offset, _| {
                        progress(RestoreProgress {
                            bytes: state.bytes + offset as u64,
                            ..state
                        })
                    },
                )?;
                self.submit_restore_batch(&script, setup, || {
                    progress(RestoreProgress {
                        bytes: state.bytes + source.bytes,
                        finishing: true,
                        ..state
                    })
                })?;
                Ok::<_, String>(count)
            })();
            match run {
                Ok(count) => {
                    state.bytes += source.bytes;
                    state.statements += count;
                }
                Err(error) if error == crate::restore::RESTORE_CANCELLED => return Err(error),
                Err(error) => {
                    let path = source.path();
                    let name = path
                        .file_name()
                        .unwrap_or(path.as_os_str())
                        .to_string_lossy();
                    return Err(format!(
                        "HTTP 还原在「{name}」处停止。已完成 {} 条语句。{error}",
                        state.statements
                    ));
                }
            }
        }
        state.finishing = true;
        progress(state)?;
        Ok(state.statements)
    }

    fn submit_restore_batch(
        &self,
        script: &str,
        setup: RestoreSetup<'_>,
        on_submit: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        let uncertain = |reason: String| {
            format!(
                "无法确认 HTTP 还原是否完整。已执行的操作可能已经提交，请核对目标库后再决定是否重新还原。{reason}"
            )
        };
        let payload = self.prepare_payload(script, true, "")?;
        on_submit()?;
        let reply = self.execute_payload(payload, 1).map_err(uncertain)?;
        if let Some(error) = reply.error {
            let guarded = !setup.guards.is_empty();
            if guarded && error.message.contains("HARBOR_SCHEMA_TARGET_CHANGED") {
                return Err(
                    "隧道实际写入的服务器与复制前核对的目标不同。请重新检查目标连接。".into(),
                );
            }
            if guarded && error.message.contains("HARBOR_COPY_PARENT_CHANGED") {
                return Err(
                    "提交复制前，目标命名规则或外键引用表已发生变化。请重新确认目标依赖。".into(),
                );
            }
            if guarded && error.message.contains("HARBOR_COPY_SAME_SCHEMA") {
                return Err("实际写入时，源和目标是同一个数据库。请选择其他目标数据库。".into());
            }
            if guarded && error.message.contains("HARBOR_COPY_CYCLE_OUTSIDE_REF") {
                return Err("待覆盖的表当前被复制范围以外的外键引用。请核对目标对象。".into());
            }
            // These sentinels are the complete SQL submitted to PREPARE before COMMIT.
            if guarded && !setup.completion.is_empty() && error.code == 1064 {
                if error.message.contains(&format!("'{CYCLE_DATA_CHANGED}'"))
                    && setup
                        .preparation
                        .iter()
                        .any(|sql| sql.contains(CYCLE_DATA_CHANGED))
                {
                    return Err(
                        "目标表结构或外键定义在复制前发生变化。本次复制事务已回滚，请重新核对目标表。".into(),
                    );
                }
                if error.message.contains(&format!("'{CYCLE_DATA_INVALID}'"))
                    && setup
                        .completion
                        .iter()
                        .any(|sql| sql.contains(CYCLE_DATA_INVALID))
                {
                    return Err(
                        "复制后有记录找不到外键所引用的目标行。本次复制事务已回滚，请核对所选表与目标数据。".into(),
                    );
                }
            }
            return Err(format!(
                "HTTP 还原已停止。停止前执行的操作可能已经提交。ERROR {}: {}",
                error.code, error.message
            ));
        }
        let confirmed = reply.sets.last().is_some_and(|set| {
            set.columns.len() == 1
                && set.columns[0].name == RESTORE_MARKER
                && set
                    .rows
                    .first()
                    .and_then(|row| row.first())
                    .and_then(Option::as_deref)
                    == Some(b"1".as_slice())
        });
        if !confirmed {
            return Err(uncertain("隧道未返回批次完成标记。".into()));
        }
        Ok(())
    }
}

fn prepare(
    input: &str,
    schema: &str,
    version: ServerVersion,
    mut modes: Modes<'_>,
    encoding: RestoreEncoding,
    setup: RestoreSetup<'_>,
    mut checkpoint: impl FnMut(usize, usize) -> Result<(), String>,
) -> Result<(String, usize), String> {
    // All statements share the single database connection allocated to this HTTP request.
    // Replacing the file delimiters keeps procedure bodies intact in the legacy batch parser.
    let mut parts = vec![input, schema];
    parts.extend(
        setup
            .guards
            .iter()
            .chain(setup.preparation)
            .chain(setup.completion)
            .map(String::as_str),
    );
    let separator = separator_for(&parts);
    let mut batch = format!("DELIMITER {separator};\n");
    for guard in setup.guards {
        writeln!(batch, "{guard}\n{separator}").unwrap();
        writeln!(
            batch,
            "PREPARE harbor_schema_target FROM @harbor_schema_target\n{separator}"
        )
        .unwrap();
        writeln!(
            batch,
            "DEALLOCATE PREPARE harbor_schema_target\n{separator}"
        )
        .unwrap();
    }
    for statement in setup.preparation {
        if !legacy_token_is_closed(statement) {
            return Err("目标外键的名称无法由旧版 HTTP 隧道安全解析。".into());
        }
        writeln!(batch, "{statement}\n{separator}").unwrap();
    }
    let mut explicit = if let RestoreEncoding::Explicit(charset) = encoding {
        writeln!(batch, "SET NAMES utf8mb4\n{separator}").unwrap();
        Some(ExplicitClientEncoding::new(charset))
    } else {
        None
    };
    if !schema.is_empty() {
        let select = format!("USE {}", quote_identifier(schema));
        if !legacy_token_is_closed(&select) {
            return Err("此数据库名称无法由旧版隧道批处理安全解析。".into());
        }
        writeln!(batch, "{select}\n{separator}").unwrap();
    }
    let sql = input.strip_prefix('\u{feff}').unwrap_or(input);
    let bom_bytes = input.len() - sql.len();
    let mut statements = StatementReader::new(sql);
    statements.set_sql_mode(&modes.current);
    let generated = if let RestoreEncoding::Marked(charset) = encoding {
        Some(GeneratedClientEncoding::new(charset))
    } else {
        None
    };
    let mut generated_at = None;
    let mut count = 0;
    while let Some(statement) = statements.next_statement() {
        let offset = bom_bytes + statement.range.start;
        checkpoint(offset, count)?;
        let preflight_error = |reason| {
            let label = byte_position_label(encoding.charset().is_some());
            format!(
                "预检在文件内第 {} 条语句处停止（{label} {offset}）。{reason}",
                count + 1
            )
        };
        let rewrite = generated.as_ref().is_some_and(|encoding| {
            generated_at == Some(statement.range.start) && statement.sql == encoding.statement()
        });
        let query = if rewrite {
            "SET NAMES utf8mb4"
        } else if let Some(encoding) = &mut explicit {
            encoding
                .query(statement.sql, &modes.current)
                .map_err(preflight_error)?
        } else {
            statement.sql
        };
        generated_at = generated.as_ref().and_then(|encoding| {
            let end = statement.range.end;
            if GeneratedClientEncoding::is_state_statement(without_comments(statement.sql))
                && sql[end..]
                    .strip_prefix(';')
                    .is_some_and(|after| after.starts_with(encoding.after_state()))
            {
                Some(end + 2)
            } else {
                None
            }
        });
        if !legacy_token_is_closed(query) {
            return Err(preflight_error(
                "旧版 HTTP 隧道无法处理此语句中的引号或注释，可使用直连或 SSH 隧道还原。"
                    .to_owned(),
            ));
        }
        modes.update(query, version).map_err(preflight_error)?;
        statements.set_sql_mode(&modes.current);
        if (batch.len() as u64)
            .saturating_add(query.len() as u64)
            .saturating_add(separator.len() as u64 + 2)
            > MAX_HTTP_SCRIPT
        {
            return Err("HTTP 还原要求解码后的 SQL 批次不超过 64 MiB。".into());
        }
        writeln!(batch, "{query}\n{separator}").unwrap();
        count += 1;
    }
    if count == 0 {
        return Err("文件中没有可执行语句。".into());
    }
    for statement in setup.completion {
        if !legacy_token_is_closed(statement) {
            return Err("旧版 HTTP 隧道无法安全处理复制校验语句中的引号或注释。".into());
        }
        writeln!(batch, "{statement}\n{separator}").unwrap();
    }
    writeln!(batch, "SELECT 1 AS {RESTORE_MARKER}\n{separator}").unwrap();
    if batch.len() as u64 > MAX_HTTP_SCRIPT {
        return Err("HTTP 还原要求解码后的 SQL 批次不超过 64 MiB。".into());
    }
    Ok((batch, count))
}

pub(super) fn read_batch(setup: &str, select: &str) -> Result<String, String> {
    batch_statements(&[setup, select])
}

pub(in crate::database) fn batch_statements(commands: &[&str]) -> Result<String, String> {
    if commands.iter().any(|sql| !legacy_token_is_closed(sql)) {
        return Err("旧版 HTTP 隧道无法安全处理此查询中的引号或注释。".into());
    }
    let separator = separator_for(commands);
    let mut batch = format!("DELIMITER {separator};\n");
    for sql in commands {
        writeln!(batch, "{sql}\n{separator}").unwrap();
    }
    Ok(batch)
}

fn separator_for(inputs: &[&str]) -> String {
    let mut hash = Sha256::new();
    for input in inputs {
        hash.update(input.as_bytes());
    }
    let digest = format!("{:x}", hash.finalize());
    let mut separator = format!("__harbor_{}__", &digest[..20]);
    while inputs.iter().any(|text| {
        text.as_bytes()
            .windows(separator.len())
            .any(|window| window.eq_ignore_ascii_case(separator.as_bytes()))
    }) {
        separator.push('_');
    }
    separator
}

#[derive(Clone, Copy)]
struct ServerVersion {
    number: u32,
    maria_db: bool,
}

impl ServerVersion {
    fn parse(version: &str) -> Option<Self> {
        let maria_db = version.to_ascii_lowercase().contains("mariadb");
        let version = if maria_db {
            version.strip_prefix("5.5.5-").unwrap_or(version)
        } else {
            version
        };
        let mut parts = version.split(|ch: char| !ch.is_ascii_digit());
        let major = parts.next()?.parse::<u32>().ok()?;
        let minor = parts.next()?.parse::<u32>().ok()?;
        let patch = parts.next()?.parse::<u32>().ok()?;
        let number = major
            .checked_mul(10_000)?
            .checked_add(minor.checked_mul(100)?)?
            .checked_add(patch)?;
        Some(Self { number, maria_db })
    }

    fn active_sql(self, sql: &str) -> &str {
        let sql = without_comments(sql);
        let marker = sql
            .strip_prefix("/*!")
            .map(|body| (false, body))
            .or_else(|| sql.strip_prefix("/*M!").map(|body| (true, body)));
        let Some((maria_only, body)) = marker else {
            return sql;
        };
        let Some((body, _)) = body.split_once("*/") else {
            return "";
        };
        let inner = body.trim_start_matches(|ch: char| ch.is_ascii_digit());
        let tag = &body[..body.len() - inner.len()];
        let minimum = tag.parse::<u32>().unwrap_or(0);
        if (maria_only && !self.maria_db)
            || self.number < minimum
            || (self.maria_db && !maria_only && (50_700..100_000).contains(&minimum))
        {
            return "";
        }
        inner.trim_ascii_start()
    }
}

struct Modes<'a> {
    current: String,
    global: &'a str,
    variables: HashMap<String, String>,
}

impl Modes<'_> {
    fn update(&mut self, sql: &str, version: ServerVersion) -> Result<(), String> {
        let sql = version.active_sql(sql);
        let Some(rest) = sql.get(3..).filter(|rest| {
            sql[..3].eq_ignore_ascii_case("SET")
                && rest.as_bytes().first().is_some_and(u8::is_ascii_whitespace)
        }) else {
            return Ok(());
        };
        let parsed = set_assignments(rest, &self.current);
        for assignment in parsed {
            let Some((name, expression)) = assignment.split_once('=') else {
                continue;
            };
            let name = name
                .trim_end_matches(':')
                .chars()
                .filter(|ch| !ch.is_ascii_whitespace() && *ch != '\x60')
                .collect::<String>()
                .to_ascii_lowercase();
            let expression = expression.trim();
            let resolved = match expression.to_ascii_lowercase().as_str() {
                "@@sql_mode" | "@@session.sql_mode" | "@@local.sql_mode" => {
                    Some(self.current.clone())
                }
                "default" | "@@global.sql_mode" => Some(self.global.to_owned()),
                known if known.starts_with('@') => self.variables.get(known).cloned(),
                _ => mode_literal(expression),
            };
            match name.as_str() {
                "sql_mode" | "sessionsql_mode" | "localsql_mode" | "@@sql_mode"
                | "@@session.sql_mode" | "@@local.sql_mode" => {
                    self.current = resolved
                        .ok_or("无法预判 SQL 模式变更。请使用直连或 SSH 隧道执行此脚本。")?;
                }
                name if name.starts_with('@') && !name.starts_with("@@") => {
                    if let Some(mode) = resolved {
                        self.variables.insert(name.to_owned(), mode);
                    } else {
                        self.variables.remove(name);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

fn mode_literal(expression: &str) -> Option<String> {
    let quote = expression
        .chars()
        .next()
        .filter(|ch| matches!(ch, '\'' | '"'))?;
    let value = expression.strip_prefix(quote)?.strip_suffix(quote)?;
    (value.len() <= 1024
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b',' | b' ')))
    .then(|| value.to_owned())
}

// The PHP batch tokenizer treats backslashes as escapes and skips strings and comments.
// Ensure the appended separator is visible before starting a write.
fn legacy_token_is_closed(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    let mut offset = 0;
    while let Some(&byte) = bytes.get(offset) {
        match byte {
            b'\'' | b'"' | b'`' => {
                offset += 1;
                loop {
                    match bytes.get(offset) {
                        Some(b'\\') if byte != b'`' => offset += 2,
                        Some(next) if *next == byte && bytes.get(offset + 1) == Some(&byte) => {
                            offset += 2;
                        }
                        Some(next) if *next == byte => {
                            offset += 1;
                            break;
                        }
                        Some(_) => offset += 1,
                        None => return false,
                    }
                }
            }
            b'/' if bytes.get(offset + 1) == Some(&b'*') => {
                let Some(end) =
                    bytes[offset + 2..]
                        .windows(2)
                        .enumerate()
                        .find_map(|(index, token)| {
                            (token == b"*/" && (index == 0 || bytes[offset + index + 1] != b'\\'))
                                .then_some(index)
                        })
                else {
                    return false;
                };
                offset += end + 4;
            }
            b'#' => {
                offset = bytes[offset..]
                    .iter()
                    .position(|byte| matches!(byte, b'\r' | b'\n'))
                    .map_or(bytes.len(), |end| offset + end + 1);
            }
            b'-' if bytes.get(offset + 1) == Some(&b'-')
                && bytes
                    .get(offset + 2)
                    .is_some_and(|byte| matches!(byte, b' ' | b'\t')) =>
            {
                offset = bytes[offset..]
                    .iter()
                    .position(|byte| matches!(byte, b'\r' | b'\n'))
                    .map_or(bytes.len(), |end| offset + end + 1);
            }
            b'\\' => offset += 2,
            _ => offset += 1,
        }
    }
    true
}
