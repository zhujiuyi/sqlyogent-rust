use std::fs;
use std::io::{self, BufWriter, Cursor, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::database::transfer::{display_value, html_text};
use crate::database::{Cell, Command, QueryReport, Response, Worker};
use crate::restore::{changes_sql_mode, open_sql_file, statement_limit, validate_sql_encoding};
use crate::sql_tools::StreamingStatementReader;

use super::{redact, request, validate_identifier, validate_output};

pub const MAX_REPORT_ROWS: usize = 10_000;
const HTML_HEADER: &str = "<!doctype html><html lang=\"zh-CN\">\n<head><meta charset=\"utf-8\">\n<title>SQL 作业报告</title>\n<style>body{margin:2rem auto;max-width:70rem;padding:0 1rem;font:16px system-ui}table{border-collapse:collapse}td,th{padding:.4rem;border:1px solid #c6cbd1;text-align:start;white-space:pre-wrap;overflow-wrap:anywhere}pre{white-space:pre-wrap;overflow-wrap:anywhere}.error{color:#951e24}section{margin:2rem 0}</style>\n</head><body><h1>SQL 作业报告</h1>";

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SqlJob {
    pub schema: Option<String>,
    pub sql: Option<String>,
    pub script: Option<PathBuf>,
    pub report: Option<String>,
    #[serde(default)]
    pub overwrite: bool,
    #[serde(default)]
    pub continue_on_error: bool,
    #[serde(default)]
    pub include_sql: bool,
    #[serde(default = "include_messages")]
    pub include_messages: bool,
    #[serde(default = "default_limit")]
    pub result_row_limit: usize,
}

fn default_limit() -> usize {
    1000
}

fn include_messages() -> bool {
    true
}

impl Default for SqlJob {
    fn default() -> Self {
        Self {
            schema: None,
            sql: Some(String::new()),
            script: None,
            report: None,
            overwrite: false,
            continue_on_error: false,
            include_sql: false,
            include_messages: include_messages(),
            result_row_limit: default_limit(),
        }
    }
}

#[derive(Default)]
pub(super) struct SqlSummary {
    pub(super) statements: usize,
    pub(super) result_sets: usize,
    pub(super) rows: usize,
    pub(super) affected_rows: u64,
    pub(super) truncated_sets: usize,
    pub(super) omitted_sets: usize,
}

impl SqlJob {
    /// Split a script with the default MySQL quoting rules without opening a database connection.
    pub fn check_statements(&self, directory: &Path) -> Result<usize, String> {
        let input = self.input(directory)?;
        let limit = statement_limit(1024 * 1024 * 1024)?;
        let mut reader = StreamingStatementReader::new(input, limit);
        let mut statements = 0;
        while reader.next_statement(&mut |_| Ok(()))?.is_some() {
            statements += 1;
        }
        if statements == 0 {
            return Err("脚本中没有可执行语句。".into());
        }
        Ok(statements)
    }

    pub(super) fn validate(&self, directory: &Path) -> Result<(), String> {
        if let Some(schema) = &self.schema {
            validate_identifier(schema, "数据库名称")?;
        }
        match (&self.sql, &self.script) {
            (Some(sql), None) if !sql.trim().is_empty() => {}
            (None, Some(path)) if !path.as_os_str().is_empty() => {
                script_metadata(&directory.join(path))?;
            }
            _ => return Err("请在 SQL 作业中设置非空的 sql 或 script。".into()),
        }
        if let Some(path) = &self.report {
            validate_output(path)?;
        }
        if !(1..=MAX_REPORT_ROWS).contains(&self.result_row_limit) {
            return Err(format!(
                "每个报告结果集的行数上限应为 1 至 {MAX_REPORT_ROWS}。"
            ));
        }
        Ok(())
    }

    pub(super) fn run(
        &self,
        worker: &Worker,
        directory: &Path,
        output: Option<&Path>,
        password: &str,
    ) -> Result<SqlSummary, String> {
        let input = self.input(directory)?;
        let mut next_id = 1;
        let mut query = |sql: &str, limit| {
            next_id += 1;
            match request(
                worker,
                next_id,
                Command::Query {
                    sql: sql.to_owned(),
                    limit,
                },
            )? {
                Response::Queried(report) => Ok(report),
                _ => Err("数据库连接未返回 SQL 执行结果。".into()),
            }
        };
        let packet = session_value(&mut query, "SELECT @@SESSION.max_allowed_packet")?
            .parse::<u64>()
            .map_err(|_| "服务器返回的通信包上限无效。")?;
        let mut statements = StreamingStatementReader::new(input, statement_limit(packet)?);
        statements.set_sql_mode(&session_mode(&mut query)?);
        let summary = if let Some(path) = output {
            if !self.overwrite && path.exists() {
                return Err(format!("报告文件已存在。{}", path.display()));
            }
            let parent = path.parent().ok_or("无法定位报告文件的目录。")?;
            let mut temporary = NamedTempFile::new_in(parent).map_err(report_error)?;
            let summary = {
                let mut report = BufWriter::new(temporary.as_file_mut());
                writeln!(report, "{HTML_HEADER}").map_err(report_error)?;
                let summary =
                    self.execute(&mut statements, &mut query, Some(&mut report), password)?;
                writeln!(report, "</body></html>").map_err(report_error)?;
                report.flush().map_err(report_error)?;
                summary
            };
            temporary.as_file().sync_data().map_err(report_error)?;
            let saved = if self.overwrite {
                temporary.persist(path)
            } else {
                temporary.persist_noclobber(path)
            };
            saved.map_err(|error| report_error(error.error))?;
            summary
        } else {
            self.execute(&mut statements, &mut query, None, password)?
        };
        summary
    }

    fn input(&self, directory: &Path) -> Result<Box<dyn Read + '_>, String> {
        match (&self.sql, &self.script) {
            (Some(text), None) if !text.trim().is_empty() => {
                Ok(Box::new(Cursor::new(text.as_bytes())))
            }
            (None, Some(path)) if !path.as_os_str().is_empty() => {
                let (mut file, _) = open_sql_file(&directory.join(path))?;
                validate_sql_encoding(&mut file, |_| Ok(()))?;
                Ok(Box::new(file))
            }
            _ => Err("请在 SQL 作业中设置非空的 sql 或 script。".into()),
        }
    }

    fn execute(
        &self,
        statements: &mut StreamingStatementReader<impl Read>,
        query: &mut impl FnMut(&str, usize) -> Result<QueryReport, String>,
        mut report: Option<&mut dyn Write>,
        password: &str,
    ) -> Result<Result<SqlSummary, String>, String> {
        let mut summary = SqlSummary::default();
        let mut failed = 0;
        let mut first_error = None;
        let limit = if report.is_some() {
            self.result_row_limit
        } else {
            0
        };
        while let Some(statement) = statements.next_statement(&mut |_| Ok(()))? {
            summary.statements += 1;
            let result = query(statement.sql, limit);
            if let Some(writer) = report.as_deref_mut() {
                let sql = self.include_sql.then_some(statement.sql);
                write_statement(
                    writer,
                    summary.statements,
                    sql,
                    &result,
                    self.include_messages,
                    password,
                )
                .map_err(report_error)?;
            }
            let result = match result {
                Ok(result) => result,
                Err(error) => {
                    failed += 1;
                    first_error.get_or_insert((summary.statements, error));
                    break;
                }
            };
            summary.omitted_sets += result.omitted_sets;
            for set in result.sets {
                if set.columns.is_empty() {
                    summary.affected_rows = summary.affected_rows.saturating_add(set.affected_rows);
                } else {
                    summary.result_sets += 1;
                    summary.rows += set.rows.len();
                    if limit > 0 && set.truncated {
                        summary.truncated_sets += 1;
                    }
                }
            }
            if let Some(error) = result.error {
                failed += 1;
                first_error.get_or_insert((summary.statements, error));
                if !self.continue_on_error {
                    break;
                }
            }
            if changes_sql_mode(statement.sql) {
                match session_mode(query) {
                    Ok(mode) => statements.set_sql_mode(&mode),
                    Err(error) => {
                        failed += 1;
                        let error = format!("无法同步会话 SQL 模式。{error}");
                        if let Some(writer) = report.as_deref_mut() {
                            writeln!(
                                writer,
                                "<p class=\"error\">{}</p>",
                                html_text(&redact(&error, password))
                            )
                            .map_err(report_error)?;
                        }
                        first_error.get_or_insert((summary.statements, error));
                        break;
                    }
                }
            }
        }
        if summary.statements == 0 {
            return Err("脚本中没有可执行语句。".into());
        }
        if let Some((statement, error)) = first_error {
            Ok(Err(format!(
                "已尝试执行 {} 条 SQL，{} 条失败。首个错误位于第 {statement} 条。{error}",
                summary.statements, failed
            )))
        } else {
            Ok(Ok(summary))
        }
    }
}

fn script_metadata(path: &Path) -> Result<(), String> {
    let metadata = fs::metadata(path).map_err(|error| format!("无法读取 SQL 脚本属性：{error}"))?;
    if !metadata.is_file() {
        return Err("请选择普通 SQL 脚本文件。".into());
    }
    Ok(())
}

fn session_mode(
    query: &mut impl FnMut(&str, usize) -> Result<QueryReport, String>,
) -> Result<String, String> {
    session_value(query, "SELECT @@SESSION.sql_mode")
}

fn session_value(
    query: &mut impl FnMut(&str, usize) -> Result<QueryReport, String>,
    sql: &str,
) -> Result<String, String> {
    let report = query(sql, 1)?;
    if let Some(error) = report.error {
        return Err(error);
    }
    match report
        .sets
        .first()
        .and_then(|set| set.rows.first())
        .and_then(|row| row.first())
    {
        Some(Cell::Text(mode)) => Ok(mode.clone()),
        _ => Err("服务器未返回所需的会话设置。".into()),
    }
}

fn write_statement(
    out: &mut dyn Write,
    number: usize,
    sql: Option<&str>,
    result: &Result<QueryReport, String>,
    include_messages: bool,
    password: &str,
) -> io::Result<()> {
    writeln!(out, "<section><h2>第 {number} 条语句</h2>")?;
    if let Some(sql) = sql {
        writeln!(out, "<pre>{}</pre>", html_text(&redact(sql, password)))?;
    }
    match result {
        Err(error) => writeln!(
            out,
            "<p class=\"error\">{}</p>",
            html_text(&redact(error, password))
        )?,
        Ok(report) => {
            if include_messages {
                writeln!(out, "<p>用时 {} 毫秒</p>", report.elapsed.as_millis())?;
            }
            if let Some(error) = &report.error {
                writeln!(
                    out,
                    "<p class=\"error\">{}</p>",
                    html_text(&redact(error, password))
                )?;
            }
            for set in &report.sets {
                if include_messages && let Some(notice) = &set.notice {
                    writeln!(out, "<p>{}</p>", html_text(&redact(notice, password)))?;
                }
                if set.columns.is_empty() {
                    if include_messages {
                        writeln!(out, "<p>影响 {} 行</p>", set.affected_rows)?;
                    }
                    continue;
                }
                write!(out, "<table><thead><tr>")?;
                for column in &set.columns {
                    write!(
                        out,
                        "<th scope=\"col\">{}</th>",
                        html_text(&redact(column, password))
                    )?;
                }
                writeln!(out, "</tr></thead><tbody>")?;
                for row in &set.rows {
                    write!(out, "<tr>")?;
                    for cell in row {
                        if matches!(cell, Cell::Null) {
                            write!(out, "<td><i>NULL</i></td>")?;
                        } else {
                            write!(
                                out,
                                "<td>{}</td>",
                                html_text(&redact(&display_value(cell), password))
                            )?;
                        }
                    }
                    writeln!(out, "</tr>")?;
                }
                writeln!(out, "</tbody></table>")?;
                if set.truncated {
                    writeln!(
                        out,
                        "<p>已显示 {} 行，结果集达到报告上限。</p>",
                        set.rows.len()
                    )?;
                }
            }
            if report.omitted_sets > 0 {
                writeln!(
                    out,
                    "<p>已略去 {} 个超过上限的结果集。</p>",
                    report.omitted_sets
                )?;
            }
        }
    }
    writeln!(out, "</section>")
}

fn report_error(error: io::Error) -> String {
    format!("无法写入 SQL 作业报告：{error}")
}
