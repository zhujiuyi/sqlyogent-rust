use std::collections::hash_map::RandomState;
use std::fs::{self, File};
use std::hash::BuildHasher;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use mysql::prelude::Queryable;
use mysql::{LocalInfileHandler, Opts};

use crate::sql_tools::mysql_binary_literal;

use super::super::{
    DbObject, mysql_error, quote_identifier, transfer_connection, validate_import_rows,
};

pub struct LoadLocalImport {
    pub object: DbObject,
    pub file: PathBuf,
    pub columns: Vec<String>,
    pub charset: Option<String>,
    pub format: LoadLocalFormat,
    pub line_terminator: String,
    pub escape: Option<char>,
    pub skip_lines: u64,
    pub priority: LoadLocalPriority,
    pub conflict: LoadLocalConflict,
}

pub enum LoadLocalFormat {
    FixedWidth,
    Delimited {
        field_terminator: String,
        enclosure: Option<char>,
        optionally_enclosed: bool,
    },
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub enum LoadLocalPriority {
    #[default]
    Normal,
    Low,
    Concurrent,
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub enum LoadLocalConflict {
    #[default]
    ServerDefault,
    Replace,
    Ignore,
}

pub struct LoadLocalReport {
    pub affected_rows: u64,
    pub records: Option<u64>,
    pub deleted: Option<u64>,
    pub skipped: Option<u64>,
    pub info: String,
    pub warning_count: u64,
    pub warnings: Vec<LoadLocalWarning>,
}

pub struct LoadLocalWarning {
    pub level: String,
    pub code: u16,
    pub message: String,
}

impl LoadLocalImport {
    fn statement(&self, token: &str) -> Result<String, String> {
        validate_import_rows(&self.object, &self.columns, &[])?;
        if self.object.schema.is_empty()
            || self.object.name.is_empty()
            || self.object.schema.contains('\0')
            || self.object.name.contains('\0')
            || self.columns.iter().any(|name| name.contains('\0'))
        {
            return Err("请选择有效的目标库、表和列。".into());
        }

        let charset = match self.charset.as_deref() {
            None => String::new(),
            Some(name)
                if !name.is_empty()
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') =>
            {
                format!(" CHARACTER SET {name}")
            }
            Some(_) => return Err("请输入有效的 MySQL 文件字符集名称。".into()),
        };
        let (field, enclosure, optionally) = match &self.format {
            LoadLocalFormat::FixedWidth => ("", None, false),
            LoadLocalFormat::Delimited {
                field_terminator,
                enclosure,
                optionally_enclosed,
            } => {
                if field_terminator.is_empty() {
                    return Err("请输入字段分隔符，或选择定宽。".into());
                }
                (
                    field_terminator.as_str(),
                    *enclosure,
                    *optionally_enclosed && enclosure.is_some(),
                )
            }
        };
        if !field.is_ascii()
            || !self.line_terminator.is_ascii()
            || enclosure.is_some_and(|character| !character.is_ascii())
            || self.escape.is_some_and(|character| !character.is_ascii())
        {
            return Err("服务器导入的分隔符、封闭符和转义符须使用 ASCII 字符。".into());
        }
        let character_literal = |character: Option<char>| match character {
            None => mysql_binary_literal(&[]),
            Some(character) => mysql_binary_literal(&[character as u8]),
        };
        let priority = match self.priority {
            LoadLocalPriority::Normal => "",
            LoadLocalPriority::Low => " LOW_PRIORITY",
            LoadLocalPriority::Concurrent => " CONCURRENT",
        };
        let conflict = match self.conflict {
            LoadLocalConflict::ServerDefault => "",
            LoadLocalConflict::Replace => " REPLACE",
            LoadLocalConflict::Ignore => " IGNORE",
        };
        let columns = self
            .columns
            .iter()
            .map(|name| quote_identifier(name))
            .collect::<Vec<_>>()
            .join(", ");
        let skip = if self.skip_lines == 0 {
            String::new()
        } else {
            format!(" IGNORE {} LINES", self.skip_lines)
        };
        let table = self.object.qualified_name();
        let field = mysql_binary_literal(field.as_bytes());
        let line = mysql_binary_literal(self.line_terminator.as_bytes());
        let enclosed = character_literal(enclosure);
        let escaped = character_literal(self.escape);
        let optionally = if optionally { "OPTIONALLY " } else { "" };
        Ok(format!(
            "LOAD DATA{priority} LOCAL INFILE '{token}'{conflict} INTO TABLE {table}{charset} \
             FIELDS TERMINATED BY {field} {optionally}ENCLOSED BY {enclosed} ESCAPED BY {escaped} \
             LINES TERMINATED BY {line}{skip} ({columns})"
        ))
    }
}

pub(in crate::database) fn run(
    opts: &Opts,
    import: LoadLocalImport,
) -> Result<LoadLocalReport, String> {
    let token = format!("harbor_load_{:016x}", RandomState::new().hash_one("file"));
    let statement = import.statement(&token)?;
    let mut file = Some(open_file(&import.file)?);
    let mut conn = transfer_connection(opts)?;
    conn.select_db(&import.object.schema).map_err(mysql_error)?;
    conn.query_drop("SET SESSION autocommit = 1")
        .map_err(mysql_error)?;

    // LOAD DATA's filename is an opaque token. The server never controls a
    // filesystem path, and the pinned handle is consumed by exactly one request.
    let delivered = Arc::new(AtomicBool::new(false));
    let sent = Arc::clone(&delivered);
    conn.set_local_infile_handler(Some(LocalInfileHandler::new(move |requested, stream| {
        if requested != token.as_bytes() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "服务器请求的文件与本次选中的文件不匹配。",
            ));
        }
        let mut approved = file.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "服务器重复请求了本次选中的文件。",
            )
        })?;
        io::copy(&mut approved, stream)?;
        sent.store(true, Ordering::Release);
        Ok(())
    })));
    let imported = conn.query_drop(&statement);
    conn.set_local_infile_handler(None);
    imported.map_err(load_error)?;
    if !delivered.load(Ordering::Acquire) {
        return Err("服务器未读取本次选中的文件，无法确认导入结果。".into());
    }

    let affected_rows = conn.affected_rows();
    let info = conn.info_str().into_owned();
    let warning_count = count(&info, "Warnings:").unwrap_or(u64::from(conn.warnings()));
    let warnings = if warning_count > 0 {
        conn.query_map("SHOW WARNINGS", |(level, code, message)| LoadLocalWarning {
            level,
            code,
            message,
        })
        .unwrap_or_default()
    } else {
        Vec::new()
    };
    drop(conn);
    Ok(LoadLocalReport {
        affected_rows,
        records: count(&info, "Records:"),
        deleted: count(&info, "Deleted:"),
        skipped: count(&info, "Skipped:"),
        info,
        warning_count,
        warnings,
    })
}

fn open_file(path: &Path) -> Result<File, String> {
    let checked = |result: io::Result<fs::Metadata>| {
        if result
            .map_err(|error| format!("无法检查导入文件。{error}"))?
            .is_file()
        {
            Ok(())
        } else {
            Err("请选择一个普通文件。".to_owned())
        }
    };
    checked(fs::metadata(path))?;
    let file = File::open(path).map_err(|error| format!("无法打开导入文件。{error}"))?;
    checked(file.metadata())?;
    Ok(file)
}

fn count(info: &str, label: &str) -> Option<u64> {
    let mut tokens = info.split_ascii_whitespace();
    tokens.find(|&token| token == label)?;
    tokens.next()?.parse().ok()
}

fn load_error(error: mysql::Error) -> String {
    let disabled = matches!(&error, mysql::Error::MySqlError(details) if matches!(details.code, 1148 | 3948 | 3950 | 4166));
    let details = mysql_error(error);
    if disabled {
        format!("服务器拒绝了本地文件导入。请检查服务器的 local_infile 设置。{details}")
    } else {
        format!("服务器导入中止。{details} 请核对目标表后再重试。")
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use mysql::Conn;

    use crate::database::http_session::workflow_test::fixture_database;
    use crate::database::{Command, ConnectionConfig, ObjectKind, Response, Worker};

    use super::*;

    #[test]
    #[ignore = "需要启用 local_infile 的一次性 MariaDB，由 HARBOR_HTTP_FIXTURE_PORT 指定端口"]
    fn imports_local_fixed_and_delimited_files_in_a_separate_session() {
        let (port, opts) = fixture_database();
        let mut sql = Conn::new(opts).unwrap();
        let prior_max_error_count: u64 = sql
            .query_first("SELECT @@GLOBAL.max_error_count")
            .unwrap()
            .unwrap();
        let schema = format!("harbor_load_local_{}", std::process::id());
        sql.query_drop(format!("CREATE DATABASE {schema} CHARACTER SET latin1"))
            .unwrap();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sql.query_drop("SET GLOBAL max_error_count = 256").unwrap();
            sql.select_db(&schema).unwrap();
            sql.query_drop("CREATE TABLE fixed (id TINYINT PRIMARY KEY, label CHAR(5), omitted VARCHAR(20) DEFAULT 'default') ENGINE=MyISAM").unwrap();
            sql.query_drop("CREATE TABLE delimited (id INT PRIMARY KEY, label VARCHAR(30)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4").unwrap();
            sql.query_drop("INSERT INTO delimited VALUES (20,'existing')")
                .unwrap();
            sql.query_drop("CREATE TABLE editor_pending (id INT) ENGINE=InnoDB")
                .unwrap();

            let worker = Worker::new();
            let request = |id, command| {
                worker.send(id, command).unwrap();
                let event = worker
                    .recv_timeout(Duration::from_secs(15))
                    .unwrap()
                    .unwrap();
                assert_eq!(event.id, id);
                event.result.unwrap()
            };
            assert!(matches!(
                request(
                    1,
                    Command::Connect(ConnectionConfig {
                        port,
                        database: Some(schema.clone()),
                        ..Default::default()
                    })
                ),
                Response::Connected(_)
            ));
            assert!(matches!(
                request(
                    2,
                    Command::Query {
                        sql: "START TRANSACTION; INSERT INTO editor_pending VALUES (1)".into(),
                        limit: 1
                    }
                ),
                Response::Queried(report) if report.error.is_none()
            ));
            let temporary =
                tempfile::tempdir_in(Path::new(env!("CARGO_MANIFEST_DIR")).join("target")).unwrap();
            let object = |name: &str| DbObject {
                schema: schema.clone(),
                name: name.into(),
                kind: ObjectKind::Table,
            };
            let import = |id, settings| match request(id, Command::ImportLocalFile(settings)) {
                Response::LocalFileImported(report) => report,
                _ => panic!("expected local file import result"),
            };
            let fixed = |name: &str, bytes: &[u8], priority, charset, skip_lines| {
                let file = temporary.path().join(name);
                fs::write(&file, bytes).unwrap();
                LoadLocalImport {
                    object: object("fixed"),
                    file,
                    columns: vec!["id".into(), "label".into()],
                    charset,
                    format: LoadLocalFormat::FixedWidth,
                    line_terminator: "\n".into(),
                    escape: None,
                    skip_lines,
                    priority,
                    conflict: LoadLocalConflict::ServerDefault,
                }
            };
            let first = import(
                3,
                fixed(
                    "定宽.txt",
                    b"header\n   1caf\xe9 \n   2Long \n",
                    LoadLocalPriority::Low,
                    Some("latin1".into()),
                    1,
                ),
            );
            assert_eq!((first.affected_rows, first.records), (2, Some(2)));
            let second = import(
                4,
                fixed(
                    "second.txt",
                    b"   3third\n",
                    LoadLocalPriority::Concurrent,
                    None,
                    0,
                ),
            );
            assert_eq!((second.affected_rows, second.records), (1, Some(1)));
            assert_eq!(
                sql.query::<(u8, String, String), _>(
                    "SELECT id,label,omitted FROM fixed ORDER BY id"
                )
                .unwrap(),
                [
                    (1, "café".into(), "default".into()),
                    (2, "Long".into(), "default".into()),
                    (3, "third".into(), "default".into()),
                ]
            );

            let source = temporary.path().join("分隔文本.csv");
            let duplicates = "\"源重复\";20\r\n".repeat(105);
            fs::write(
                &source,
                format!("label;id\r\n\"中文;值\";10\r\n{duplicates}"),
            )
            .unwrap();
            let report = import(
                5,
                LoadLocalImport {
                    object: object("delimited"),
                    file: source,
                    columns: vec!["label".into(), "id".into()],
                    charset: Some("utf8mb4".into()),
                    format: LoadLocalFormat::Delimited {
                        field_terminator: ";".into(),
                        enclosure: Some('"'),
                        optionally_enclosed: true,
                    },
                    line_terminator: "\r\n".into(),
                    escape: Some('\\'),
                    skip_lines: 1,
                    priority: LoadLocalPriority::Normal,
                    conflict: LoadLocalConflict::Ignore,
                },
            );
            assert_eq!(
                (
                    report.affected_rows,
                    report.records,
                    report.deleted,
                    report.skipped
                ),
                (1, Some(106), Some(0), Some(105))
            );
            assert_eq!(report.warning_count, 105);
            assert_eq!(report.warnings.len(), 105);
            assert!(report.warnings.iter().all(|warning| warning.code == 1062));
            assert_eq!(
                sql.query::<(u8, String), _>("SELECT id,label FROM delimited ORDER BY id")
                    .unwrap(),
                [(10, "中文;值".into()), (20, "existing".into())]
            );
            assert_eq!(
                sql.query_first::<u64, _>("SELECT COUNT(*) FROM editor_pending")
                    .unwrap(),
                Some(0)
            );
            assert!(matches!(
                request(
                    6,
                    Command::Query { sql: "COMMIT".into(), limit: 1 }
                ),
                Response::Queried(report) if report.error.is_none()
            ));
            assert_eq!(
                sql.query_first::<u64, _>("SELECT COUNT(*) FROM editor_pending")
                    .unwrap(),
                Some(1)
            );
        }));
        sql.query_drop(format!(
            "SET GLOBAL max_error_count = {prior_max_error_count}"
        ))
        .unwrap();
        sql.query_drop(format!("DROP DATABASE IF EXISTS {schema}"))
            .unwrap();
        if let Err(error) = outcome {
            std::panic::resume_unwind(error);
        }
    }
}
