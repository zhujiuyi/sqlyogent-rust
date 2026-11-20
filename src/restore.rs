use std::fs::File;
use std::io::{self, Read, Seek};
use std::path::Path;

use mysql::{Conn, prelude::Queryable};

use crate::database::transfer::{DUMP_CLIENT_ENCODING_TAG, DUMP_CLIENT_STATE};
use crate::sql_encoding::SqlDumpCharset;
use crate::sql_tools::StreamingStatementReader;

mod client_encoding;
mod input;
pub(crate) use client_encoding::ExplicitClientEncoding;
use input::PreparedSource;
pub(crate) use input::{RESTORE_MANIFEST_NAME, directory_sources, ordered_part_name};

pub(crate) const MAX_HTTP_SCRIPT: u64 = crate::http_tunnel::MAX_REQUEST_BYTES as u64;
pub(crate) const RESTORE_MANIFEST_LIMIT: u64 = 8 * 1024 * 1024;
pub const RESTORE_CANCELLED: &str = "已停止 SQL 还原。之前已提交的更改可能保留。";

#[derive(Clone, Copy, Debug)]
pub struct RestoreProgress {
    pub bytes: u64,
    pub total_bytes: u64,
    pub statements: usize,
    pub validating: bool,
    pub finishing: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RestoreEncoding {
    Plain,
    Marked(SqlDumpCharset),
    Explicit(SqlDumpCharset),
}

impl RestoreEncoding {
    pub(crate) fn charset(self) -> Option<SqlDumpCharset> {
        match self {
            Self::Plain => None,
            Self::Marked(charset) | Self::Explicit(charset) => Some(charset),
        }
    }
}

pub(crate) struct GeneratedClientEncoding {
    statement: String,
    after_state: String,
}

impl GeneratedClientEncoding {
    pub(crate) fn new(encoding: SqlDumpCharset) -> Self {
        let statement = format!("SET NAMES {}", encoding.mysql_name());
        let after_state = format!("\n{statement};{DUMP_CLIENT_ENCODING_TAG}\n");
        Self {
            statement,
            after_state,
        }
    }

    pub(crate) fn tagged_line(&self) -> &str {
        &self.after_state[1..]
    }

    pub(crate) fn after_state(&self) -> &str {
        &self.after_state
    }

    pub(crate) fn statement(&self) -> &str {
        &self.statement
    }

    pub(crate) fn is_state_statement(sql: &str) -> bool {
        DUMP_CLIENT_STATE.strip_suffix(";\n") == Some(sql)
    }
}

pub(crate) fn byte_position_label(normalized: bool) -> &'static str {
    if normalized {
        "转码后的 UTF-8 字节位置"
    } else {
        "字节位置"
    }
}

#[cfg(test)]
pub(crate) fn restore(conn: &mut Conn, schema: &str, path: &Path) -> Result<usize, String> {
    restore_with_progress(conn, schema, path, None, |_| Ok(()))
}

pub(crate) fn restore_with_progress(
    conn: &mut Conn,
    schema: &str,
    path: &Path,
    selected: Option<SqlDumpCharset>,
    progress: impl FnMut(RestoreProgress) -> Result<(), String>,
) -> Result<usize, String> {
    restore_with_preparation(conn, schema, path, selected, |_| Ok(()), progress)
}

pub(crate) fn restore_with_preparation(
    conn: &mut Conn,
    schema: &str,
    path: &Path,
    selected: Option<SqlDumpCharset>,
    prepare: impl FnOnce(&mut Conn) -> Result<(), String>,
    mut progress: impl FnMut(RestoreProgress) -> Result<(), String>,
) -> Result<usize, String> {
    let (sources, mut state) = prepare_restore_inputs(path, selected, None, &mut progress)?;
    if !schema.is_empty() {
        conn.select_db(schema)
            .map_err(|error| format!("无法切换至默认数据库 {schema}：{error}"))?;
    }
    let packet: u64 = conn
        .query_first("SELECT @@SESSION.max_allowed_packet")
        .map_err(|error| format!("无法读取数据库的通信包上限。{error}"))?
        .ok_or("数据库未返回通信包上限。")?;
    let limit = statement_limit(packet)?;
    progress(state)?;
    prepare(conn)?;
    let mut mode = session_mode(conn)?;
    for source in &sources {
        let file = source.open(|| progress(state))?;
        restore_source(
            conn,
            source,
            file,
            limit,
            &mut mode,
            &mut state,
            &mut progress,
        )?;
    }
    if state.statements == 0 {
        return Err("文件中没有可执行语句。".into());
    }
    state.finishing = true;
    progress(state)?;
    Ok(state.statements)
}

pub(crate) fn prepare_restore_inputs(
    path: &Path,
    selected: Option<SqlDumpCharset>,
    file_limit: Option<u64>,
    progress: &mut impl FnMut(RestoreProgress) -> Result<(), String>,
) -> Result<(Vec<PreparedSource>, RestoreProgress), String> {
    let sources = input::sources(path)?;
    let total_bytes = sources.iter().try_fold(0u64, |total, source| {
        total
            .checked_add(source.bytes)
            .ok_or("备份总字节数超过支持范围。")
    })?;
    let mut state = RestoreProgress {
        bytes: 0,
        total_bytes,
        statements: 0,
        validating: true,
        finishing: false,
    };
    let mut prepared = Vec::with_capacity(sources.len());
    let mut restored_bytes = 0u64;
    for source in sources {
        if file_limit.is_some_and(|limit| source.bytes > limit) {
            let name = source.path.file_name().unwrap_or(source.path.as_os_str());
            return Err(format!(
                "单个 HTTP 还原文件的编码请求最多 64 MiB。「{}」超出上限。",
                name.to_string_lossy()
            ));
        }
        let raw_bytes = source.bytes;
        let source = source.prepare(selected, file_limit, |bytes| {
            progress(RestoreProgress {
                bytes: state.bytes + bytes,
                ..state
            })
        })?;
        state.bytes += raw_bytes;
        restored_bytes = restored_bytes
            .checked_add(source.bytes)
            .ok_or("还原文件的总字节数超过支持范围。")?;
        prepared.push(source);
    }
    state.validating = false;
    state.bytes = 0;
    state.total_bytes = restored_bytes;
    Ok((prepared, state))
}

fn restore_source(
    conn: &mut Conn,
    source: &PreparedSource,
    file: File,
    limit: usize,
    mode: &mut String,
    state: &mut RestoreProgress,
    progress: &mut impl FnMut(RestoreProgress) -> Result<(), String>,
) -> Result<(), String> {
    let base = state.bytes;
    let mut statements = StreamingStatementReader::new(file, limit);
    statements.set_sql_mode(mode);
    let generated = if let RestoreEncoding::Marked(charset) = source.encoding {
        Some(GeneratedClientEncoding::new(charset))
    } else {
        None
    };
    let mut explicit = if let RestoreEncoding::Explicit(charset) = source.encoding {
        run(conn, "SET NAMES utf8mb4")
            .map_err(|error| format!("无法为 SQL 还原启用 UTF-8 会话。{error}"))?;
        Some(ExplicitClientEncoding::new(charset))
    } else {
        None
    };
    let mut matches_header = false;
    loop {
        state.bytes = base + statements.position();
        progress(*state)?;
        let command = statements.next_statement(&mut |bytes| {
            progress(RestoreProgress {
                bytes: base + bytes,
                ..*state
            })
        });
        let command = match command {
            Ok(Some(command)) => command,
            Ok(None) => break,
            Err(error) if error == RESTORE_CANCELLED => return Err(error),
            Err(error) => {
                return Err(restore_error(
                    state.statements + 1,
                    source,
                    statements.position(),
                    &error,
                ));
            }
        };
        let recognized = generated
            .as_ref()
            .is_some_and(|encoding| matches_header && command.sql == encoding.statement());
        let offset = command.offset;
        let sql = if recognized {
            "SET NAMES utf8mb4"
        } else if let Some(encoding) = &mut explicit {
            encoding
                .query(command.sql, mode)
                .map_err(|error| restore_error(state.statements + 1, source, offset, &error))?
        } else {
            command.sql
        };
        let saved_state = generated.is_some() && GeneratedClientEncoding::is_state_statement(sql);
        if let Err(error) = run(conn, sql) {
            return Err(restore_error(
                state.statements + 1,
                source,
                offset,
                &error.to_string(),
            ));
        }
        state.statements += 1;
        if changes_sql_mode(sql) {
            *mode = session_mode(conn)
                .map_err(|error| restore_error(state.statements, source, offset, &error))?;
            statements.set_sql_mode(mode);
        }
        matches_header = false;
        if saved_state
            && let Some(encoding) = &generated
            && statements.position() == offset + DUMP_CLIENT_STATE.len() as u64 - 1
        {
            matches_header =
                statements.followed_by(encoding.after_state().as_bytes(), &mut |bytes| {
                    progress(RestoreProgress {
                        bytes: base + bytes,
                        ..*state
                    })
                })?;
        }
    }
    if statements.position() != source.bytes {
        return Err(restore_error(
            state.statements + 1,
            source,
            statements.position(),
            "还原期间文件长度已变化。",
        ));
    }
    state.bytes = base + source.bytes;
    Ok(())
}

fn restore_error(statement: usize, source: &PreparedSource, offset: u64, error: &str) -> String {
    let path = source.path();
    let name = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy();
    let label = byte_position_label(source.encoding.charset().is_some());
    format!(
        "还原在第 {statement} 条语句处停止（「{name}」{label} {offset}）。之前已执行的更改可能已经提交。{error}"
    )
}

pub(crate) fn statement_limit(packet: u64) -> Result<usize, String> {
    let content = packet.min(1024 * 1024 * 1024).saturating_sub(1);
    if content == 0 {
        return Err("当前数据库的通信包上限无效。".into());
    }
    Ok(content as usize)
}

pub(crate) fn open_sql_file(path: &Path) -> Result<(File, u64), String> {
    let file = File::open(path).map_err(|error| format!("无法打开 SQL 文件。{error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("无法读取文件信息：{error}"))?;
    if !metadata.is_file() {
        return Err("请选择普通 SQL 文件。".into());
    }
    Ok((file, metadata.len()))
}

pub(crate) fn validate_sql_encoding(
    file: &mut File,
    mut progress: impl FnMut(u64) -> Result<(), String>,
) -> Result<(), String> {
    let before = file
        .metadata()
        .map_err(|error| format!("无法读取 SQL 文件信息。{error}"))?;
    let mut bytes = vec![0; 128 * 1024 + 3];
    let mut carry = 0;
    let mut total = 0u64;
    loop {
        progress(total)?;
        let read = match file.read(&mut bytes[carry..]) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result.map_err(|error| format!("无法读取 SQL 文件。{error}"))?,
        };
        if read == 0 {
            if carry != 0 {
                return Err(format!(
                    "SQL 文件不是完整的 UTF-8，问题位于字节 {}。",
                    total - carry as u64
                ));
            }
            break;
        }
        let valid = &bytes[..carry + read];
        let previous = carry;
        match std::str::from_utf8(valid) {
            Ok(_) => carry = 0,
            Err(error) if error.error_len().is_none() => {
                carry = valid.len() - error.valid_up_to();
                bytes.copy_within(error.valid_up_to()..carry + error.valid_up_to(), 0);
            }
            Err(error) => {
                let offset = total - previous as u64 + error.valid_up_to() as u64;
                return Err(format!("SQL 文件需要 UTF-8 编码，问题位于字节 {offset}。"));
            }
        }
        total += read as u64;
    }
    let after = file
        .metadata()
        .map_err(|error| format!("无法读取 SQL 文件信息。{error}"))?;
    if total != before.len()
        || total != after.len()
        || before.modified().ok() != after.modified().ok()
    {
        return Err("检查期间 SQL 文件已变化，请重新选择文件。".into());
    }
    file.rewind()
        .map_err(|error| format!("无法从头读取 SQL 文件。{error}"))
}

pub(crate) fn read_http_script(
    source: &PreparedSource,
    mut checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<String, String> {
    let file = source.open(&mut checkpoint)?;
    if source.bytes > MAX_HTTP_SCRIPT {
        return Err("HTTP 还原要求整个脚本装入单个请求，编码后的请求上限为 64 MiB。".into());
    }
    let mut file = file.take(MAX_HTTP_SCRIPT + 1);
    let mut bytes = Vec::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        checkpoint()?;
        let read = match file.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result.map_err(|error| format!("无法读取 SQL 转储文件：{error}"))?,
        };
        if read == 0 {
            break;
        }
        if bytes.len() as u64 + read as u64 > MAX_HTTP_SCRIPT {
            return Err("HTTP 还原要求整个脚本装入单个请求，编码后的请求上限为 64 MiB。".into());
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    if bytes.len() as u64 != source.bytes {
        return Err("SQL 文件长度已变化，请重新选择文件。".into());
    }
    String::from_utf8(bytes)
        .map_err(|error| format!("转储文件必须使用 UTF-8 编码：{}", error.utf8_error()))
}

fn session_mode(conn: &mut Conn) -> Result<String, String> {
    conn.query_first("SELECT @@SESSION.sql_mode")
        .map_err(|error| format!("无法读取 SQL 模式：{error}"))?
        .ok_or_else(|| "服务器没有返回当前 SQL 模式。".to_owned())
}

pub(crate) fn changes_sql_mode(mut input: &str) -> bool {
    loop {
        input = input.trim_ascii_start();
        if input.starts_with('#')
            || (input.starts_with("--")
                && input
                    .as_bytes()
                    .get(2)
                    .is_some_and(|b| b.is_ascii_whitespace() || b.is_ascii_control()))
        {
            input = input.find(['\r', '\n']).map_or("", |next| &input[next..]);
        } else if let Some(body) = input.strip_prefix("/*") {
            let Some(end) = body.find("*/") else {
                return false;
            };
            if let Some(executable) = body[..end]
                .strip_prefix('!')
                .or_else(|| body[..end].strip_prefix("M!"))
            {
                if is_mode_set(
                    executable
                        .trim_start_matches(|ch: char| ch.is_ascii_digit())
                        .trim_ascii_start(),
                ) {
                    return true;
                }
            }
            input = &body[end + 2..];
        } else {
            break;
        }
    }
    is_mode_set(input)
}

fn is_mode_set(input: &str) -> bool {
    input
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("SET"))
        && input.as_bytes().get(3).is_some_and(u8::is_ascii_whitespace)
        && input
            .as_bytes()
            .windows(8)
            .any(|token| token.eq_ignore_ascii_case(b"sql_mode"))
}

pub(crate) fn run(conn: &mut Conn, sql: &str) -> mysql::Result<()> {
    let mut result = conn.query_iter(sql)?;
    while let Some(set) = result.iter() {
        for row in set {
            row?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::changes_sql_mode;
    use super::{
        DUMP_CLIENT_STATE, GeneratedClientEncoding, RESTORE_CANCELLED, RestoreEncoding,
        prepare_restore_inputs,
    };
    use crate::sql_encoding::{DUMP_ENCODING_PREFIX, SqlDumpCharset, encode_sql};
    use crate::sql_tools::StatementReader;

    #[test]
    fn marked_restore_validates_decodes_and_preserves_the_original_file() {
        let executable = std::env::current_exe().unwrap();
        let folder = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let path = folder.path().join("转储.sql");
        let charset = SqlDumpCharset::Gbk;
        let generated = GeneratedClientEncoding::new(charset);
        let utf8 = format!(
            "{DUMP_ENCODING_PREFIX}gbk\n-- 港湾 SQL 逻辑备份\n{DUMP_CLIENT_STATE}{}SELECT '订单';\n",
            generated.tagged_line()
        );
        let mut encoded = Vec::new();
        encode_sql(charset, utf8.as_bytes(), &mut encoded).unwrap();
        assert!(utf8.len() > encoded.len());
        std::fs::write(&path, &encoded).unwrap();

        let (sources, progress) =
            prepare_restore_inputs(&path, None, None, &mut |_| Ok(())).unwrap();
        assert_eq!(progress.total_bytes, utf8.len() as u64);
        assert_eq!(sources[0].encoding, RestoreEncoding::Marked(charset));
        let mut output = String::new();
        sources[0]
            .open(|| Ok(()))
            .unwrap()
            .read_to_string(&mut output)
            .unwrap();
        assert_eq!(output, utf8);
        assert_eq!(std::fs::read(&path).unwrap(), encoded);

        let too_large =
            prepare_restore_inputs(&path, None, Some(encoded.len() as u64), &mut |_| Ok(()));
        assert!(matches!(too_large, Err(reason) if reason.contains("解码后的 SQL 文件")));
        let stopped = prepare_restore_inputs(&path, None, None, &mut |progress| {
            if progress.bytes > 0 {
                Err(RESTORE_CANCELLED.into())
            } else {
                Ok(())
            }
        });
        assert!(matches!(stopped, Err(reason) if reason == RESTORE_CANCELLED));
    }

    #[test]
    fn restore_scans_dump_using_mode_after_set() {
        let mut script = StatementReader::new(
            "/*!40101 SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES' */;\n\
             INSERT INTO sample VALUES ('path\\');\nSELECT 2;",
        );
        let mode = script.next_statement().unwrap();
        assert!(changes_sql_mode(mode.sql));
        script.set_sql_mode("NO_BACKSLASH_ESCAPES");
        assert_eq!(
            script.next_statement().unwrap().sql,
            "INSERT INTO sample VALUES ('path\\')"
        );
        assert_eq!(script.next_statement().unwrap().sql, "SELECT 2");
    }

    #[test]
    #[ignore = "requires a disposable MariaDB via HARBOR_HTTP_FIXTURE_PORT"]
    fn restores_a_large_file_and_runs_it_as_a_scheduled_sql_job() {
        use crate::database::{Command, ConnectionConfig, Response, Worker};
        use mysql::{Conn, OptsBuilder, prelude::Queryable};
        use std::io::{Read, Write};
        use std::time::Duration;
        let port = std::env::var("HARBOR_HTTP_FIXTURE_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let opts = OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(port)
            .user(Some("root"))
            .prefer_socket(false);
        let mut sql = Conn::new(opts).unwrap();
        let schema = format!("harbor_big_restore_{}", std::process::id());
        let copy_schema = format!("{schema}_copy");
        sql.query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
            .unwrap();
        let folder = tempfile::tempdir().unwrap();
        let dump = folder.path().join("大转储.sql");
        {
            let mut file = std::fs::File::create(&dump).unwrap();
            file.write_all(b"\xef\xbb\xbf/*").unwrap();
            let count = 256_u64 * 1024 * 1024;
            std::io::copy(&mut std::io::repeat(b'x').take(count), &mut file).unwrap();
            write!(
                file,
                "*/\nUSE {schema};
CREATE TABLE IF NOT EXISTS sample (id INT PRIMARY KEY, body VARCHAR(50)) ENGINE=InnoDB;
/*!40101 SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES' */;
DELIMITER '封'
DROP PROCEDURE IF EXISTS load_sample 封
CREATE PROCEDURE load_sample() BEGIN
    INSERT INTO sample VALUES (1,'路径;中文\\') ON DUPLICATE KEY UPDATE body=VALUES(body);
    SELECT body FROM sample WHERE id=1;
END 封
DELIMITER ;
CALL load_sample();
"
            )
            .unwrap();
        }
        assert!(std::fs::metadata(&dump).unwrap().len() > 256 * 1024 * 1024);
        let worker = Worker::new();
        let run = || {
            worker
                .send(
                    1,
                    Command::Connect(ConnectionConfig {
                        port,
                        ..Default::default()
                    }),
                )
                .unwrap();
            let reply = worker
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap();
            assert!(matches!(reply.result.unwrap(), Response::Connected(_)));
            worker
                .send(
                    2,
                    Command::Restore {
                        schema: schema.clone(),
                        path: dump.clone(),
                    },
                )
                .unwrap();
            let reply = worker
                .recv_timeout(Duration::from_secs(45))
                .unwrap()
                .unwrap();
            assert!(matches!(reply.result.unwrap(), Response::Restored(6)));
            let progress = worker.take_restore_progress(2).unwrap();
            assert!(progress.finishing && !progress.validating);
            assert_eq!(
                (progress.bytes, progress.statements),
                (progress.total_bytes, 6)
            );
            assert_eq!(
                sql.query_first::<String, _>(format!(
                    "SELECT body FROM {schema}.sample WHERE id=1"
                ))
                .unwrap(),
                Some("路径;中文\\".into())
            );
            let job = folder.path().join("restore-job.json");
            let definition = serde_json::json!({
                "version": 1,
                "connection": { "host": "127.0.0.1", "port": port, "user": "root" },
                "job": { "type":"sql", "script":"大转储.sql", "report":"report.html" }
            });
            std::fs::write(&job, serde_json::to_vec(&definition).unwrap()).unwrap();
            assert_eq!(crate::jobs::cli(["run".into(), job.into_os_string()]), 0);
            let report = std::fs::read_to_string(folder.path().join("report.html")).unwrap();
            assert!(report.contains("路径;中文\\"));
            sql.select_db(&schema).unwrap();
            sql.query_drop(
                "CREATE TABLE dependent (id INT PRIMARY KEY, sample_id INT,
                FOREIGN KEY (sample_id) REFERENCES sample(id)) ENGINE=InnoDB",
            )
            .unwrap();
            sql.query_drop("INSERT INTO dependent VALUES (7,1)")
                .unwrap();
            sql.query_drop(format!(
                "CREATE DATABASE {copy_schema} CHARACTER SET utf8mb4"
            ))
            .unwrap();
            let output = folder.path().join("分对象备份");
            std::fs::create_dir(&output).unwrap();
            let summary = crate::database::transfer::dump_configured(
                &mut sql,
                &schema,
                &output,
                crate::database::transfer::DumpSettings {
                    file_per_object: true,
                    disable_foreign_key_checks: false,
                    ..Default::default()
                },
                |_| Ok(()),
            )
            .unwrap();
            let directory = summary.output_directory.unwrap();
            assert!(directory.join("restore-order.json").is_file());
            worker
                .send(
                    3,
                    Command::Restore {
                        schema: copy_schema.clone(),
                        path: directory,
                    },
                )
                .unwrap();
            let reply = worker
                .recv_timeout(Duration::from_secs(20))
                .unwrap()
                .unwrap();
            assert!(matches!(reply.result.unwrap(), Response::Restored(count) if count > 6));
            assert_eq!(sql.query_first::<String,_>(format!(
                "SELECT s.body FROM {copy_schema}.dependent d JOIN {copy_schema}.sample s ON s.id=d.sample_id WHERE d.id=7"
            )).unwrap(), Some("路径;中文\\".into()));
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
        sql.query_drop(format!("DROP DATABASE IF EXISTS {copy_schema}"))
            .unwrap();
        sql.query_drop(format!("DROP DATABASE IF EXISTS {schema}"))
            .unwrap();
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }
}
