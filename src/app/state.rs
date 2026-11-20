use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::ops::Range;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::database::{
    AccountReply, AdvancedConnection, Command, ConnectionConfig, DatabaseContents,
    DatabaseContentsResult, DatabaseDefaults, DbObject, EDITABLE_READ_CANCELLED, HttpTunnelConfig,
    JoinedEditablePage, LoadLocalReport, ObjectDetail, PreparedSync, PreparedSyncGroup,
    ProcessReply, ProfileOptions, QueryDiagnostics, QueryProfile, QueryReport, QueryTiming,
    Response, SchemaApplyProgress, SchemaApplyResult, SchemaApplyStop, SchemaDesign,
    SchemaSnapshot, ServerInfo, ServerValue, SshConfig, SyncEndpoint, SyncGroupResult,
    SyncProgress, SyncResult, SyncScriptFile, SyncScriptPlan, SyncSide, SyncWrites,
    TABLE_READ_CANCELLED, TableKeys, TablePage, TlsClientIdentity, TlsConfig, Worker,
    transfer::{DumpProgress, TransferSummary},
};
use crate::maintenance::ServerView;

use super::PendingEditableQuery;
use super::completion;
use super::error_details::ErrorDetails;
use super::objects::Operation;
use super::output::{HistoryTiming, ResultMode};
use super::row_editor::RowEditor;

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub(super) struct Profile {
    pub label: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub password: String,
    pub schema: String,
    pub tls: bool,
    pub ca: String,
    pub client_identity: String,
    pub client_private_key: String,
    pub ssh: bool,
    pub ssh_host: String,
    pub ssh_port: u16,
    pub ssh_user: String,
    pub ssh_key: String,
    pub ssh_password_auth: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ssh_password: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ssh_key_passphrase: String,
    pub ssh_known_hosts: String,
    pub ssh_local_port: Option<u16>,
    pub http: Option<HttpProfile>,
    pub advanced: AdvancedProfile,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub(super) struct AdvancedProfile {
    pub compression: bool,
    pub wait_timeout: Option<u32>,
    pub keep_alive: Option<u32>,
    pub connect_timeout: u32,
    pub read_timeout: u32,
    pub sql_mode: Option<String>,
    pub init_file: String,
    pub displayed_databases: String,
}

impl Default for AdvancedProfile {
    fn default() -> Self {
        Self {
            compression: false,
            wait_timeout: None,
            keep_alive: None,
            connect_timeout: 10,
            read_timeout: 120,
            sql_mode: None,
            init_file: String::new(),
            displayed_databases: String::new(),
        }
    }
}

#[derive(Clone, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub(super) struct HttpProfile {
    pub url: String,
    pub content_type: usize,
    pub force_base64: bool,
    pub utf8mb4: bool,
    pub web_user: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub web_password: String,
    pub proxy_url: String,
    pub proxy_user: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub proxy_password: String,
    pub timeout: Option<u32>,
}

impl HttpProfile {
    pub fn config(&self, web_password: String, proxy_password: String) -> HttpTunnelConfig {
        HttpTunnelConfig {
            url: self.url.trim().to_owned(),
            content_type: self.content_type,
            force_base64: self.force_base64,
            use_utf8mb4: self.utf8mb4,
            web_auth: optional_text(&self.web_user).map(|user| (user, web_password)),
            proxy_url: optional_text(&self.proxy_url),
            proxy_auth: optional_text(&self.proxy_user).map(|user| (user, proxy_password)),
            timeout: self.timeout.unwrap_or(120),
        }
    }
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            label: "本机 MySQL".into(),
            host: "127.0.0.1".into(),
            port: 3306,
            user: "root".into(),
            password: String::new(),
            schema: String::new(),
            tls: false,
            ca: String::new(),
            client_identity: String::new(),
            client_private_key: String::new(),
            ssh: false,
            ssh_host: String::new(),
            ssh_port: 22,
            ssh_user: String::new(),
            ssh_key: String::new(),
            ssh_password_auth: false,
            ssh_password: String::new(),
            ssh_key_passphrase: String::new(),
            ssh_known_hosts: String::new(),
            ssh_local_port: None,
            http: None,
            advanced: AdvancedProfile::default(),
        }
    }
}

impl Profile {
    pub fn config(
        &self,
        password: String,
        ssh_password: String,
        ssh_key_passphrase: String,
        init_sql: String,
    ) -> ConnectionConfig {
        ConnectionConfig {
            host: self.host.trim().into(),
            port: self.port,
            user: self.user.trim().into(),
            password,
            database: optional_text(&self.schema),
            tls: (self.tls && self.http.is_none()).then(|| TlsConfig {
                ca_path: optional_text(&self.ca).map(PathBuf::from),
                client_identity: optional_text(&self.client_identity)
                    .zip(optional_text(&self.client_private_key))
                    .map(|(certificate, private_key)| TlsClientIdentity {
                        certificate: PathBuf::from(certificate),
                        private_key: PathBuf::from(private_key),
                    }),
            }),
            ssh: (self.ssh && self.http.is_none()).then(|| SshConfig {
                host: self.ssh_host.trim().into(),
                port: self.ssh_port,
                user: self.ssh_user.trim().into(),
                identity_file: if self.ssh_password_auth {
                    None
                } else {
                    optional_text(&self.ssh_key).map(PathBuf::from)
                },
                password: self.ssh_password_auth.then_some(ssh_password),
                key_passphrase: (!self.ssh_password_auth
                    && !self.ssh_key.trim().is_empty()
                    && !ssh_key_passphrase.is_empty())
                .then_some(ssh_key_passphrase),
                known_hosts_path: optional_text(&self.ssh_known_hosts).map(PathBuf::from),
                local_port: self.ssh_local_port,
            }),
            advanced: AdvancedConnection {
                compression: self.advanced.compression,
                wait_timeout: self.advanced.wait_timeout,
                keep_alive: self.advanced.keep_alive,
                connect_timeout: self.advanced.connect_timeout,
                read_timeout: self.advanced.read_timeout,
                sql_mode: self.advanced.sql_mode.clone(),
                init_sql,
            },
        }
    }
}

fn optional_text(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

pub(super) fn profile_path() -> Option<PathBuf> {
    if let Some(directory) = crate::DATA_DIRECTORY.get() {
        return Some(directory.join("connections.json"));
    }
    directories::ProjectDirs::from("io", "HarborSQL", "Harbor SQL")
        .map(|dirs| dirs.config_dir().join("connections.json"))
}

pub(super) fn load_profiles() -> Result<Vec<Profile>, String> {
    let path = profile_path().ok_or("无法找到本机的配置目录。")?;
    load_profiles_from(&path)
}

fn load_profiles_from(path: &Path) -> Result<Vec<Profile>, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.to_string()),
    }
}

pub(super) fn save_profiles(current: &[Profile], replacement: &[Profile]) -> Result<(), String> {
    let path = profile_path().ok_or("无法找到本机的配置目录。")?;
    std::fs::create_dir_all(path.parent().expect("profile directory"))
        .map_err(|error| error.to_string())?;
    let bytes = serde_json::to_vec_pretty(replacement).map_err(|error| error.to_string())?;
    // The lock must survive replacement of the JSON file.
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))
        .map_err(|error| error.to_string())?;
    lock.lock().map_err(|error| error.to_string())?;
    if load_profiles_from(&path)? != current {
        return Err("连接配置已在其他窗口中更新，请重新打开应用后再操作。".into());
    }
    write_atomic(&path, &bytes).map_err(|error| error.to_string())
}

pub(super) fn import_legacy_profiles(
    path: &Path,
    current: &[Profile],
) -> Result<super::profile_import::ImportReport, String> {
    let report = super::profile_import::read_profiles(path, current)?;
    if report.first_imported.is_some() {
        save_profiles(current, &report.profiles)
            .map_err(|error| format!("无法保存导入的连接配置。{error}"))?;
    }
    Ok(report)
}

pub(super) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let folder = path
        .parent()
        .filter(|folder| !folder.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(folder)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map(|_| ()).map_err(|error| error.error)
}

#[derive(Clone)]
pub(super) enum BrowseAction {
    RefreshAll,
    SchemaDesigner {
        schema: String,
        table: Option<DbObject>,
    },
    TableDiagnostics {
        schema: String,
        table: Option<String>,
    },
    Objects(String),
    RefreshObjects(String),
    UseSchema(String),
    SelectObject(DbObject),
    Inspect(DbObject),
    InsertObjectName(DbObject),
    Preview(DbObject),
    Builder(DbObject),
    Import(DbObject),
    ImportOdbc(DbObject),
    ImportOdbcSchema(String),
    Script(String),
    RunSql(String),
    Manage(Operation),
    Backup {
        schema: String,
        object: Option<DbObject>,
    },
    Schedule {
        schema: String,
        object: Option<DbObject>,
    },
    SchemaHtml(String),
    Restore(String),
    ExportPreview(DbObject),
    Copy {
        schema: String,
        object: Option<DbObject>,
    },
    SyncData(DbObject),
    SyncDatabase(String),
    EditRows(DbObject),
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Output {
    Results,
    Data,
    Profile,
    Messages,
    History,
    Definition,
}

pub(super) enum SyncReply {
    Prepared(Box<PreparedSync>),
    Applied(SyncResult),
    GroupPrepared(Box<PreparedSyncGroup>),
    GroupApplied(SyncGroupResult),
    Scripts(SyncScriptPlan, Vec<SyncScriptFile>),
    Failed(String),
}

pub(super) fn sync_side_label(side: SyncSide) -> &'static str {
    match side {
        SyncSide::Source => "发送端",
        SyncSide::Target => "接收端",
    }
}

pub(super) fn sync_write_summary(source: SyncWrites, target: SyncWrites) -> String {
    format!(
        "发送端新增 {} 行、更新 {} 行、删除 {} 行。接收端新增 {} 行、更新 {} 行、删除 {} 行。",
        source.inserted,
        source.updated,
        source.deleted,
        target.inserted,
        target.updated,
        target.deleted
    )
}

fn sync_status(
    source: SyncWrites,
    target: SyncWrites,
    skipped_rows: u64,
    stopped: Option<&str>,
    uncertain: Option<SyncSide>,
) -> String {
    let mut status = sync_write_summary(source, target);
    if skipped_rows > 0 {
        status.push_str(&format!("有 {skipped_rows} 行因写入出错而跳过。"));
    }
    if let Some(error) = stopped {
        status.push(' ');
        status.push_str(error);
    }
    if let Some(side) = uncertain {
        status.push_str(&format!(" 无法确认{}提交结果。", sync_side_label(side)));
    }
    status
}

pub(super) struct QueryTab {
    pub id: u64,
    pub path: Option<PathBuf>,
    title: Option<String>,
    pub sql: String,
    pub saved: String,
    pub focused_byte: usize,
    pub selection: Option<Range<usize>>,
    pub report: Option<QueryReport>,
    pub batch_messages: Option<BatchMessages>,
    pub command_error: Option<ErrorDetails>,
    pub profile: Option<Box<QueryProfile>>,
    pub set: usize,
    pub result_views: super::output::ResultViews,
    pub data_editor: Option<RowEditor>,
    pub editable_editor: Option<RowEditor>,
    last_output: Output,
    result_mode: ResultMode,
    last_structured_result_mode: ResultMode,
}

impl QueryTab {
    fn new(id: u64, sql: String, path: Option<PathBuf>) -> Self {
        Self {
            id,
            path,
            title: None,
            saved: sql.clone(),
            sql,
            focused_byte: 0,
            selection: None,
            report: None,
            batch_messages: None,
            command_error: None,
            profile: None,
            set: 0,
            result_views: Default::default(),
            data_editor: None,
            editable_editor: None,
            last_output: Output::Results,
            result_mode: ResultMode::Grid,
            last_structured_result_mode: ResultMode::Grid,
        }
    }

    pub(super) fn title(&self) -> String {
        self.title.clone().unwrap_or_else(|| {
            self.path
                .as_deref()
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("查询 {}", self.id))
        })
    }

    pub(super) fn rename(&mut self, title: String) {
        self.title = Some(title);
    }

    pub fn label(&self) -> String {
        let label = self.title();
        if self.saved == self.sql {
            label
        } else {
            format!("{label}  •")
        }
    }
}

struct Batch {
    tab: u64,
    queue: VecDeque<String>,
    report: QueryReport,
    current_sql: Option<String>,
    attempted: usize,
    total: usize,
    messages: Option<BatchMessages>,
    command_error: Option<ErrorDetails>,
}

struct EditorQuery {
    tab: u64,
    batch_stop_button: bool,
}

impl Batch {
    fn record_statement(
        &mut self,
        result_sets: Range<usize>,
        omitted_sets: usize,
        elapsed: Option<Duration>,
        diagnostics: QueryDiagnostics,
        error: Option<String>,
    ) {
        let sql = self.current_sql.take();
        if let Some(error) = &error {
            self.report.error.get_or_insert_with(|| error.clone());
            if self.messages.is_none()
                && self.command_error.is_none()
                && let Some(sql) = &sql
            {
                self.command_error = Some(ErrorDetails::new(sql, error));
            }
        }
        if let Some(messages) = &mut self.messages
            && let Some(sql) = sql
        {
            messages.statements.push(StatementMessage {
                sql,
                result_sets,
                omitted_sets,
                elapsed,
                diagnostics,
                editable_result: None,
                error,
            });
        }
    }
}

pub(super) struct BatchMessages {
    pub total: usize,
    pub statements: Vec<StatementMessage>,
}

impl BatchMessages {
    fn single(sql: String, report: &QueryReport) -> Self {
        Self {
            total: 1,
            statements: vec![StatementMessage {
                sql,
                result_sets: 0..report.sets.len(),
                omitted_sets: report.omitted_sets,
                elapsed: Some(report.elapsed),
                diagnostics: report.diagnostics.clone(),
                editable_result: None,
                error: report.error.clone(),
            }],
        }
    }

    fn editable(sql: String, rows: usize, truncated: bool, timing: Option<QueryTiming>) -> Self {
        Self {
            total: 1,
            statements: vec![StatementMessage {
                sql,
                result_sets: 0..0,
                omitted_sets: 0,
                elapsed: timing.map(|timing| timing.elapsed),
                diagnostics: QueryDiagnostics {
                    execution: timing.and_then(|timing| timing.execution),
                    ..QueryDiagnostics::default()
                },
                editable_result: Some(EditableMessage { rows, truncated }),
                error: None,
            }],
        }
    }

    pub fn summary(&self) -> String {
        let failures = self
            .statements
            .iter()
            .filter(|statement| statement.error.is_some())
            .count();
        format!(
            "已提交 {}/{} 条语句，{failures} 条出错。",
            self.statements.len(),
            self.total
        )
    }
}

pub(super) struct StatementMessage {
    pub sql: String,
    pub result_sets: Range<usize>,
    pub omitted_sets: usize,
    pub elapsed: Option<Duration>,
    pub diagnostics: QueryDiagnostics,
    pub editable_result: Option<EditableMessage>,
    pub error: Option<String>,
}

pub(super) struct EditableMessage {
    pub rows: usize,
    pub truncated: bool,
}

enum RefreshAfterQuery {
    TableCreated(String),
    DatabaseCreated(String),
}

pub(super) enum RestoreOutcome {
    Completed(usize),
    Cancelled,
    Failed(String),
}

pub(super) struct RestoreCompletion {
    pub outcome: RestoreOutcome,
    pub progress: Option<crate::database::RestoreProgress>,
}

fn restore_number(value: &str) -> Option<(u64, &str)> {
    let digits = value.bytes().take_while(u8::is_ascii_digit).count();
    let (number, remainder) = value.split_at(digits);
    Some((number.parse().ok()?, remainder))
}

fn restore_location<'a>(
    source: &'a str,
    marker: &str,
    suffix: &str,
) -> Option<(&'a str, u64, &'a str)> {
    let mut positions = source.match_indices(marker).filter_map(|(index, marker)| {
        let (value, remainder) = restore_number(&source[index + marker.len()..])?;
        Some((&source[..index], value, remainder.strip_prefix(suffix)?))
    });
    let location = positions.next()?;
    positions.next().is_none().then_some(location)
}

fn restore_file_number(name: &str) -> String {
    if let Some((number, "")) = name.get(..8).and_then(restore_number)
        && number > 0
        && name
            .get(8..)
            .is_some_and(|suffix| suffix.starts_with('-') && suffix.ends_with(".sql"))
    {
        format!("备份文件编号 {number}。")
    } else {
        String::new()
    }
}

// Server diagnostics can echo SQL literals or credentials. Only carry trusted
// status categories and numeric fields into the desktop message and report.
pub(super) fn restore_error_summary(error: &str) -> String {
    let direct = error.strip_prefix("还原在第 ").and_then(|value| {
        let (statement, source) = restore_number(value)?;
        let source = source.strip_prefix(" 条语句处停止（「")?;
        let mut locations = ["转码后的 UTF-8 字节位置", "字节位置"]
            .into_iter()
            .filter_map(|label| {
                restore_location(
                    source,
                    &format!("」{label} "),
                    "）。之前已执行的更改可能已经提交。",
                )
                .map(|location| (label, location))
            });
        let (label, (name, offset, detail)) = locations.next()?;
        if locations.next().is_some() {
            return None;
        }
        Some((
            format!(
                "{}第 {statement} 条语句，{label} {offset}。",
                restore_file_number(name)
            ),
            detail,
        ))
    });
    let http = error.strip_prefix("HTTP 还原在「").and_then(|source| {
        let (name, completed, detail) = restore_location(source, "」处停止。已完成 ", " 条语句。")?;
        Some((
            format!(
                "{}HTTP 还原中断，先前已确认完成 {completed} 条语句。",
                restore_file_number(name)
            ),
            detail,
        ))
    });
    let (mut summary, detail) = direct.or(http).unwrap_or((String::new(), error));
    let detail = detail
        .strip_prefix("HTTP 还原已停止。停止前执行的操作可能已经提交。")
        .unwrap_or(detail);
    let detail = detail.strip_prefix("MySqlError { ").unwrap_or(detail);
    let detail = if let Some((statement, label, offset, reason)) = detail
        .strip_prefix("预检在文件内第 ")
        .and_then(restore_number)
        .and_then(|(statement, locator)| {
            let locator = locator.strip_prefix(" 条语句处停止（")?;
            let label = if locator.starts_with("转码后的 UTF-8 字节位置") {
                "转码后的 UTF-8 字节位置"
            } else {
                "字节位置"
            };
            let (offset, end) = restore_number(locator.strip_prefix(label)?.strip_prefix(' ')?)?;
            Some((statement, label, offset, end.strip_prefix("）。")?))
        }) {
        summary.push_str(&format!(
            "预检在文件内第 {statement} 条语句停止，{label} {offset}。"
        ));
        reason
    } else {
        detail
    };
    if let Some((code, suffix)) = detail.strip_prefix("ERROR ").and_then(restore_number)
        && code > 0
        && code <= u64::from(u16::MAX)
        && (suffix.starts_with(" (") || suffix.starts_with(':'))
    {
        summary.push_str(&format!("服务器报告 MySQL 错误代码 {code}。"));
    } else if let Some((offset, _)) = detail
        .strip_prefix("SQL 文件需要 UTF-8 编码，问题位于字节 ")
        .or_else(|| detail.strip_prefix("SQL 文件不是完整的 UTF-8，问题位于字节 "))
        .and_then(restore_number)
    {
        summary.push_str(&format!("SQL 文件编码无效，问题位于字节 {offset}。"));
    } else if matches!(
        detail,
        "文件中没有可执行语句。" | "SQL 文件中没有可执行语句。"
    ) {
        summary.push_str("文件中没有可执行语句。");
    } else if detail.starts_with("无法定位")
        || detail.starts_with("无法打开")
        || detail.starts_with("无法读取 SQL 文件")
        || detail.starts_with("恢复顺序")
    {
        summary.push_str("无法读取选定的 SQL 转储或备份清单。");
    } else if detail.starts_with("无法切换至默认数据库") {
        summary.push_str("无法切换到还原的起始数据库。");
    } else if detail.starts_with("无法确认 HTTP 还原是否完整") {
        summary.push_str("无法确认隧道已完成此批次。");
    } else if detail.starts_with("所选编码与 SQL 转储编码标记不一致") {
        summary.push_str("所选编码与文件编码标记不一致。请选择自动识别或与标记一致的编码。");
    } else if detail.starts_with("文件的 UTF-8 BOM 与所选编码不一致") {
        summary.push_str("文件的 UTF-8 BOM 与所选编码不一致。请选择 UTF-8。");
    } else if detail.starts_with("脚本的 SET NAMES 声明与所选文件编码不一致") {
        summary.push_str("脚本的 SET NAMES 声明与所选文件编码不一致。");
    } else if detail.starts_with("旧编码还原无法安全执行此客户端字符集设置") {
        summary.push_str("脚本中的客户端字符集设置无法安全转换。支持文件开头的简单 SET NAMES 和 SQLyog 的标准声明。");
    } else if detail.starts_with("无法为 SQL 还原启用 UTF-8 会话") {
        summary.push_str("目标服务器无法为还原启用 UTF-8 会话。");
    } else if detail.starts_with("SQL 转储编码标记") {
        summary.push_str("SQL 转储的编码标记缺失所需的文件头或格式无效。");
    } else if detail.starts_with("无法从声明的") || detail.starts_with("无法为 SQL 转码")
    {
        summary.push_str("无法按文件声明的编码转为 UTF-8。");
    } else if detail.starts_with("单个 HTTP 还原文件") || detail.starts_with("HTTP 还原要求")
    {
        summary.push_str("SQL 文件超过 HTTP 隧道的单次还原请求上限。");
    } else if summary.is_empty() {
        summary.push_str("还原中断。请核对 SQL 转储、目标库和连接后再试。");
    }
    summary
}

pub(super) enum EditableResult {
    Table(TablePage),
    Joined(JoinedEditablePage),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RowEditorKind {
    Data,
    EditableResult,
}

impl RowEditorKind {
    fn editor(self, tab: &mut QueryTab) -> &mut Option<RowEditor> {
        match self {
            Self::Data => &mut tab.data_editor,
            Self::EditableResult => &mut tab.editable_editor,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct RowEditorScope {
    tab: u64,
    kind: RowEditorKind,
}

struct PendingF8 {
    workspace_index: usize,
    query: PendingEditableQuery,
    default_read_only: bool,
}

struct PendingHistory {
    request: u64,
    record: PendingHistoryRecord,
}

enum PendingHistoryRecord {
    Native(u64),
    Http(String),
}

struct PendingReconnect {
    request: u64,
    native: bool,
}

pub(super) struct Workspace {
    pub key: u64,
    pub label: String,
    pub profile: Option<Profile>,
    pub server: Option<ServerInfo>,
    pub schemas: Vec<String>,
    pub engines: Vec<String>,
    pub raid_default_engine: Option<String>,
    pub no_backslash_escapes: bool,
    pub objects: BTreeMap<String, Vec<DbObject>>,
    pub tags: completion::Metadata,
    pub database_defaults: Option<DatabaseDefaults>,
    pub database_contents: Option<DatabaseContents>,
    pub database_contents_result: Option<DatabaseContentsResult>,
    pub account_reply: Option<Result<AccountReply, String>>,
    pub process_reply: Option<Result<ProcessReply, String>>,
    pub server_values_reply: Option<(ServerView, Result<Vec<ServerValue>, String>)>,
    pub error_dialog: Option<Box<ErrorDetails>>,
    pub snapshots: BTreeMap<String, SchemaSnapshot>,
    pub schema_apply_progress: Option<SchemaApplyProgress>,
    pub schema_apply_result: Option<Result<SchemaApplyResult, String>>,
    pub schema_designs: BTreeMap<String, SchemaDesign>,
    pub table_keys: Option<TableKeys>,
    pub detail: Option<ObjectDetail>,
    pub export_columns: Option<ObjectDetail>,
    pub selected: Option<DbObject>,
    pub row_page: Option<TablePage>,
    table_read_error: Option<String>,
    pub editable_page: Option<EditableResult>,
    editable_read_error: Option<String>,
    pub duplicate_result_page: Option<TablePage>,
    pub row_edit_affected_rows: Option<u64>,
    pub row_edit_match_count: Option<u64>,
    pub transfer_result: Option<Result<(), String>>,
    pub local_file_import_result: Option<Result<LoadLocalReport, String>>,
    pub backup_result: Option<Result<String, String>>,
    pub dump_result: Option<Result<TransferSummary, String>>,
    pub dump_progress: Option<DumpProgress>,
    pub restore_progress: Option<crate::database::RestoreProgress>,
    pub restore_cancelling: bool,
    pub restore_result: Option<RestoreCompletion>,
    pub sync_endpoint: Option<SyncEndpoint>,
    pub sync_progress: Option<SyncProgress>,
    pub sync_reply: Option<SyncReply>,
    pub output: Output,
    pub copy_column_headers: bool,
    pub show_data_info: bool,
    pub(super) data_entry_selection: Option<u64>,
    pub status: String,
    pub messages: Vec<String>,
    pub history: super::output::History,
    pub tabs: Vec<QueryTab>,
    pub active_tab: usize,
    pub result_limit: usize,
    pub schema_filter: String,
    pub focus_request: bool,
    worker: Option<Worker>,
    cloned_connection: Option<(Worker, u64)>,
    has_connected: bool,
    pending_reconnect: Option<PendingReconnect>,
    next_request: u64,
    next_tab: u64,
    pending: Option<u64>,
    pending_history: Option<PendingHistory>,
    pending_query: bool,
    pending_objects_refresh: bool,
    pending_table_read: bool,
    row_editor_scope: Option<RowEditorScope>,
    pending_row_editor: Option<RowEditorScope>,
    pending_f8: Option<PendingF8>,
    pending_editable_read: bool,
    editable_read_stopping: bool,
    pending_transfer: bool,
    pending_backup: bool,
    pending_dump: bool,
    pending_restore: bool,
    pending_sync: bool,
    pending_schema_apply: bool,
    pending_local_file_import: bool,
    pub pending_schema: Option<String>,
    origin: Option<u64>,
    batch: Option<Batch>,
    editor_query: Option<EditorQuery>,
    refresh_after_query: Option<RefreshAfterQuery>,
}

impl Workspace {
    pub fn new(label: String, profile: Option<Profile>) -> Self {
        static NEXT_KEY: AtomicU64 = AtomicU64::new(1);
        Self {
            key: NEXT_KEY.fetch_add(1, Ordering::Relaxed),
            label,
            profile,
            server: None,
            schemas: Vec::new(),
            engines: Vec::new(),
            raid_default_engine: None,
            no_backslash_escapes: false,
            objects: BTreeMap::new(),
            tags: completion::Metadata::default(),
            database_defaults: None,
            database_contents: None,
            database_contents_result: None,
            account_reply: None,
            process_reply: None,
            server_values_reply: None,
            error_dialog: None,
            snapshots: BTreeMap::new(),
            schema_apply_progress: None,
            schema_apply_result: None,
            schema_designs: BTreeMap::new(),
            table_keys: None,
            detail: None,
            export_columns: None,
            selected: None,
            row_page: None,
            table_read_error: None,
            editable_page: None,
            editable_read_error: None,
            duplicate_result_page: None,
            row_edit_affected_rows: None,
            row_edit_match_count: None,
            transfer_result: None,
            local_file_import_result: None,
            backup_result: None,
            dump_result: None,
            dump_progress: None,
            restore_progress: None,
            restore_cancelling: false,
            restore_result: None,
            sync_endpoint: None,
            sync_progress: None,
            sync_reply: None,
            output: Output::Results,
            copy_column_headers: true,
            show_data_info: true,
            data_entry_selection: None,
            status: String::new(),
            messages: Vec::new(),
            history: super::output::History::default(),
            tabs: vec![QueryTab::new(1, String::new(), None)],
            active_tab: 0,
            result_limit: 2_000,
            schema_filter: String::new(),
            focus_request: false,
            worker: None,
            cloned_connection: None,
            has_connected: false,
            pending_reconnect: None,
            next_request: 1,
            next_tab: 2,
            pending: None,
            pending_history: None,
            pending_query: false,
            pending_objects_refresh: false,
            pending_table_read: false,
            row_editor_scope: None,
            pending_row_editor: None,
            pending_f8: None,
            pending_editable_read: false,
            editable_read_stopping: false,
            pending_transfer: false,
            pending_backup: false,
            pending_dump: false,
            pending_restore: false,
            pending_sync: false,
            pending_schema_apply: false,
            pending_local_file_import: false,
            pending_schema: None,
            origin: None,
            batch: None,
            editor_query: None,
            refresh_after_query: None,
        }
    }

    pub(super) fn result_mode(&self) -> ResultMode {
        self.tabs[self.result_mode_tab_index()].result_mode
    }

    pub(super) fn last_structured_result_mode(&self) -> ResultMode {
        self.tabs[self.result_mode_tab_index()].last_structured_result_mode
    }

    pub(super) fn set_result_mode(&mut self, mode: ResultMode) {
        let index = self.result_mode_tab_index();
        let tab = &mut self.tabs[index];
        if mode != ResultMode::Text {
            tab.last_structured_result_mode = mode;
        }
        tab.result_mode = mode;
    }

    fn result_mode_tab_index(&self) -> usize {
        self.row_editor_scope
            .and_then(|scope| self.tabs.iter().position(|tab| tab.id == scope.tab))
            .unwrap_or(self.active_tab)
    }

    pub fn connect(&mut self, config: ConnectionConfig, tunnel: Option<HttpTunnelConfig>) {
        if let Some(tab) = self.protected_row_editor_tab() {
            self.select_tab(tab);
            self.output = if self.editable_result_protected(tab) {
                Output::Results
            } else {
                Output::Data
            };
            self.message("请先结束数据编辑操作，或保存、取消草稿再连接。", false);
            return;
        }
        self.worker.get_or_insert_with(Worker::new);
        let native = tunnel.is_none();
        let command = match tunnel {
            Some(tunnel) => Command::ConnectHttp(config, tunnel),
            None => Command::Connect(config),
        };
        if self.send(command, "正在连接…") {
            if self.has_connected
                && let Some(request) = self.pending
            {
                self.pending_reconnect = Some(PendingReconnect { request, native });
            }
            // Loaded rows belong to the previous database session.
            for tab in &mut self.tabs {
                tab.data_editor = None;
                tab.editable_editor = None;
            }
        }
    }

    pub(super) fn take_cloned_connection(&mut self) -> Option<(Worker, u64)> {
        self.cloned_connection.take()
    }

    pub(super) fn receive_cloned_worker(&mut self, worker: Worker, request: u64) {
        self.worker = Some(worker);
        self.next_request = request + 1;
        self.pending = Some(request);
        self.origin = Some(self.tab().id);
        self.status = "正在使用当前设置建立新连接…".into();
    }

    pub(super) fn disconnect(&mut self) {
        if self.busy() {
            self.message("当前连接正在执行任务，完成后可断开。", false);
        } else if let Some(tab) = self.protected_row_editor_tab() {
            self.select_tab(tab);
            self.output = if self.editable_result_protected(tab) {
                Output::Results
            } else {
                Output::Data
            };
            self.message("请先保存或取消数据编辑草稿，再断开连接。", false);
        } else if self.server.is_some() {
            self.send(Command::Disconnect, "正在断开连接…");
        }
    }

    pub(super) fn disconnect_all(workspaces: &mut [Self]) -> bool {
        if workspaces
            .iter()
            .any(|workspace| workspace.busy() || workspace.protected_row_editor_tab().is_some())
        {
            return false;
        }
        for workspace in workspaces {
            workspace.disconnect();
        }
        true
    }

    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }

    pub fn active_database(&self) -> Option<&str> {
        self.pending_schema
            .as_deref()
            .or_else(|| self.server.as_ref()?.database.as_deref())
    }

    pub fn request_schema_switch(&mut self, schema: String) {
        if self.server.is_none() || schema.is_empty() {
            return;
        }
        if self
            .server
            .as_ref()
            .and_then(|server| server.database.as_deref())
            == Some(schema.as_str())
        {
            self.pending_schema = None;
            return;
        }
        if !self.busy() && self.batch.is_none() {
            self.pending_schema = None;
            self.send(Command::UseSchema { schema }, "正在切换默认数据库…");
        } else {
            self.pending_schema = Some(schema);
        }
    }

    pub fn cancellable(&self) -> bool {
        (self.pending_query || self.pending_editable_read) && self.busy()
    }

    pub fn tab(&self) -> &QueryTab {
        &self.tabs[self.active_tab]
    }

    pub fn tab_mut(&mut self) -> &mut QueryTab {
        &mut self.tabs[self.active_tab]
    }

    pub(super) fn row_editor_tab_id(&self) -> u64 {
        self.row_editor_scope
            .map_or_else(|| self.tab().id, |scope| scope.tab)
    }

    pub(super) fn select_tab(&mut self, index: usize) {
        if index == self.active_tab {
            return;
        }
        self.tab_mut().last_output = self.output;
        self.active_tab = index;
        self.output = self.tab().last_output;
    }

    pub(super) fn with_data_editor<R>(
        &mut self,
        use_editor: impl FnOnce(&mut Self, &mut Option<RowEditor>) -> R,
    ) -> R {
        self.with_active_row_editor(RowEditorKind::Data, use_editor)
    }

    pub(super) fn with_editable_editor<R>(
        &mut self,
        use_editor: impl FnOnce(&mut Self, &mut Option<RowEditor>) -> R,
    ) -> R {
        self.with_active_row_editor(RowEditorKind::EditableResult, use_editor)
    }

    fn with_active_row_editor<R>(
        &mut self,
        kind: RowEditorKind,
        use_editor: impl FnOnce(&mut Self, &mut Option<RowEditor>) -> R,
    ) -> R {
        self.with_row_editor_on(
            RowEditorScope {
                tab: self.tab().id,
                kind,
            },
            use_editor,
        )
        .expect("active query tab")
    }

    fn with_row_editor_on<R>(
        &mut self,
        scope: RowEditorScope,
        use_editor: impl FnOnce(&mut Self, &mut Option<RowEditor>) -> R,
    ) -> Option<R> {
        let tab = self.tabs.iter_mut().find(|tab| tab.id == scope.tab)?;
        let mut editor = scope.kind.editor(tab).take();
        let previous_scope = self.row_editor_scope.replace(scope);
        let result = use_editor(self, &mut editor);
        self.row_editor_scope = previous_scope;
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == scope.tab) {
            *scope.kind.editor(tab) = editor;
        }
        Some(result)
    }

    pub(super) fn data_tab_protected(&self, index: usize) -> bool {
        self.tabs.get(index).is_some_and(|tab| {
            self.pending_row_editor
                == Some(RowEditorScope {
                    tab: tab.id,
                    kind: RowEditorKind::Data,
                })
                || tab
                    .data_editor
                    .as_ref()
                    .is_some_and(RowEditor::has_draft_or_modal)
        })
    }

    pub(super) fn editable_result_protected(&self, index: usize) -> bool {
        self.tabs.get(index).is_some_and(|tab| {
            self.pending_row_editor
                == Some(RowEditorScope {
                    tab: tab.id,
                    kind: RowEditorKind::EditableResult,
                })
                || self
                    .pending_f8
                    .as_ref()
                    .is_some_and(|pending| pending.query.tab == tab.id)
                || tab
                    .editable_editor
                    .as_ref()
                    .is_some_and(RowEditor::has_draft_or_modal)
        })
    }

    pub(super) fn row_editor_tab_protected(&self, index: usize) -> bool {
        self.data_tab_protected(index) || self.editable_result_protected(index)
    }

    pub(super) fn protected_row_editor_tab(&self) -> Option<usize> {
        (0..self.tabs.len()).find(|&index| self.row_editor_tab_protected(index))
    }

    fn can_replace_results(&mut self, tab: u64) -> bool {
        let protected = self
            .tabs
            .iter()
            .position(|candidate| candidate.id == tab)
            .is_some_and(|index| self.editable_result_protected(index));
        if protected {
            self.message("请先保存或取消可编辑查询结果中的修改。", false);
        }
        !protected
    }

    pub fn new_tab(&mut self, sql: String, path: Option<PathBuf>) {
        self.tabs.push(QueryTab::new(self.next_tab, sql, path));
        self.next_tab += 1;
        self.select_tab(self.tabs.len() - 1);
        self.focus_request = true;
    }

    pub(super) fn replace_sql_file(&mut self, tab: u64, sql: String, path: PathBuf) -> bool {
        let Some(index) = self.tabs.iter().position(|candidate| candidate.id == tab) else {
            return false;
        };
        if self.row_editor_tab_protected(index) {
            return false;
        }
        self.select_tab(index);
        self.tabs[index] = QueryTab::new(self.next_tab, sql, Some(path));
        self.next_tab += 1;
        self.output = Output::Results;
        self.focus_request = true;
        true
    }

    pub fn close_tab(&mut self, index: usize) {
        if self.row_editor_tab_protected(index) {
            self.message("请先结束数据编辑操作，或保存、取消草稿。", false);
            return;
        }
        let closed_active = self.active_tab == index;
        if self.tabs.len() == 1 {
            self.new_tab(String::new(), None);
        }
        self.tabs.remove(index);
        if self.active_tab > index || self.active_tab == self.tabs.len() {
            self.active_tab -= 1;
        }
        if closed_active {
            self.output = self.tab().last_output;
        }
    }

    pub fn send(&mut self, command: Command, label: &str) -> bool {
        self.send_command(command, label, true)
    }

    pub(super) fn refresh_objects(&mut self, schema: String, label: &str) {
        if self.send(Command::ListObjects { schema }, label) {
            self.pending_objects_refresh = true;
        }
    }

    pub(super) fn record_pending_history(&mut self, sql: String) {
        let request = self.pending.expect("sent history request");
        let record = if self
            .server
            .as_ref()
            .is_some_and(|server| server.http_tunnel)
        {
            PendingHistoryRecord::Http(sql)
        } else {
            PendingHistoryRecord::Native(self.history.record(sql))
        };
        self.pending_history = Some(PendingHistory { request, record });
    }

    fn finish_pending_history(&mut self, request: u64, reply: &Result<Response, String>) {
        let Some(pending) = self
            .pending_history
            .take()
            .filter(|pending| pending.request == request)
        else {
            return;
        };
        let timing = match reply {
            Ok(Response::Queried(report) | Response::Profiled { report, .. }) => {
                Some(QueryTiming {
                    execution: report.diagnostics.execution,
                    elapsed: report.elapsed,
                })
            }
            Ok(Response::EditablePage(page)) => page.query_timing,
            Ok(Response::JoinedEditablePage(page)) => page.query_timing,
            _ => None,
        };
        let (entry, timing) = match pending.record {
            PendingHistoryRecord::Native(entry) => (
                entry,
                timing
                    .and_then(|timing| timing.execution)
                    .map(HistoryTiming::Execution),
            ),
            PendingHistoryRecord::Http(sql) if reply.is_ok() => (
                self.history.record(sql),
                timing.map(|timing| HistoryTiming::Total(timing.elapsed)),
            ),
            PendingHistoryRecord::Http(_) => return,
        };
        self.history.finish(entry, timing);
    }

    fn send_command(&mut self, command: Command, label: &str, track_transfer: bool) -> bool {
        if self.busy() {
            return false;
        }
        let pending_query = matches!(
            &command,
            Command::Query { .. } | Command::QueryProfiled { .. } | Command::Preview { .. }
        );
        let origin = if let Some(batch) = &self.batch
            && matches!(&command, Command::Query { .. })
        {
            batch.tab
        } else {
            self.row_editor_tab_id()
        };
        if (pending_query
            || matches!(
                &command,
                Command::QueryF8 { .. } | Command::QueryInSeparateSession { .. }
            ))
            && !self.can_replace_results(origin)
        {
            return false;
        }
        let id = self.next_request;
        self.next_request += 1;
        let pending_table_read = matches!(&command, Command::ReadTableData { .. });
        let editable_refresh = matches!(&command, Command::QueryEditable { .. });
        let editable_query = editable_refresh || matches!(&command, Command::QueryF8 { .. });
        let duplicate_result = matches!(&command, Command::DuplicateResultRow { .. });
        let table_edit = matches!(
            &command,
            Command::EditTable { .. }
                | Command::ConfirmTableEdit { .. }
                | Command::DeleteTableRows { .. }
        );
        let pending_restore = matches!(
            &command,
            Command::Restore { .. }
                | Command::RestoreEncoded { .. }
                | Command::RestoreForCopy { .. }
        );
        let pending_transfer =
            track_transfer && (pending_restore || matches!(&command, Command::CopyTableAs { .. }));
        let pending_backup = matches!(&command, Command::Dump { .. });
        let pending_dump = matches!(
            &command,
            Command::DumpConfigured { .. } | Command::DumpForCopy { .. }
        );
        let pending_sync = matches!(
            &command,
            Command::PlanSync { .. }
                | Command::PlanScopedSync { .. }
                | Command::ApplySync { .. }
                | Command::PlanSyncGroup { .. }
                | Command::PlanScopedSyncGroup { .. }
                | Command::ApplySyncGroup { .. }
                | Command::ExportSync { .. }
        );
        let sync_endpoint = matches!(&command, Command::SyncEndpoint);
        let pending_schema_apply = matches!(&command, Command::ApplySchema { .. });
        let pending_local_file_import = matches!(&command, Command::ImportLocalFile(_));
        let result = self
            .worker
            .as_ref()
            .ok_or_else(|| "请先连接数据库。".to_owned())
            .and_then(|worker| worker.send(id, command));
        if let Err(error) = result {
            self.message(error, true);
            return false;
        }
        self.origin = Some(origin);
        self.pending = Some(id);
        self.pending_query = pending_query;
        self.pending_table_read = pending_table_read;
        self.pending_row_editor = self.row_editor_scope.filter(|scope| {
            table_edit
                || match scope.kind {
                    RowEditorKind::Data => pending_table_read,
                    RowEditorKind::EditableResult => editable_refresh,
                }
        });
        self.pending_editable_read = editable_query;
        if pending_table_read {
            self.row_page = None;
            self.table_read_error = None;
        }
        if editable_query {
            self.editable_page = None;
            self.editable_read_error = None;
            self.editable_read_stopping = false;
        }
        if duplicate_result {
            self.duplicate_result_page = None;
        }
        if table_edit {
            self.row_edit_affected_rows = None;
            self.row_edit_match_count = None;
        }
        self.pending_transfer = pending_transfer;
        if pending_transfer {
            self.transfer_result = None;
        }
        self.pending_backup = pending_backup;
        if pending_backup {
            self.backup_result = None;
        }
        self.pending_dump = pending_dump;
        self.pending_restore = pending_restore;
        self.restore_progress = None;
        self.restore_cancelling = false;
        if pending_dump {
            self.dump_result = None;
            self.dump_progress = None;
        }
        self.pending_sync = pending_sync;
        if pending_sync {
            self.sync_progress = None;
            self.sync_reply = None;
        }
        if sync_endpoint {
            self.sync_endpoint = None;
        }
        self.pending_schema_apply = pending_schema_apply;
        if pending_schema_apply {
            self.schema_apply_progress = None;
            self.schema_apply_result = None;
        }
        self.pending_local_file_import = pending_local_file_import;
        if pending_local_file_import {
            self.local_file_import_result = None;
        }
        self.status = label.to_owned();
        true
    }

    pub fn cancel_dump(&self) -> bool {
        self.pending_dump
            && self.pending.is_some_and(|id| {
                self.worker
                    .as_ref()
                    .is_some_and(|worker| worker.cancel_dump(id))
            })
    }

    pub fn cancel_table_read(&mut self) -> bool {
        if !self.pending_table_read
            || !self.pending.is_some_and(|id| {
                self.worker
                    .as_ref()
                    .is_some_and(|worker| worker.cancel_table_read(id))
            })
        {
            return false;
        }
        self.status = "正在停止读取表数据…".into();
        true
    }

    pub fn take_table_read_error(&mut self) -> Option<String> {
        self.table_read_error.take()
    }

    pub fn reading_editable_result(&self) -> bool {
        self.pending_editable_read && self.busy()
    }

    pub fn editable_read_stopping(&self) -> bool {
        self.editable_read_stopping
    }

    pub fn cancel_editable_read(&mut self) -> bool {
        if !self.pending_editable_read
            || self.editable_read_stopping
            || !self.pending.is_some_and(|id| {
                self.worker
                    .as_ref()
                    .is_some_and(|worker| worker.cancel_editable_read(id))
            })
        {
            return false;
        }
        self.editable_read_stopping = true;
        self.status = if self
            .server
            .as_ref()
            .is_some_and(|server| server.http_tunnel)
        {
            "已请求停止，等待当前 HTTP 请求返回…"
        } else {
            "正在停止当前查询…"
        }
        .into();
        true
    }

    pub fn take_editable_read_error(&mut self) -> Option<String> {
        self.editable_read_error.take()
    }

    pub fn cancel_sync(&self) -> bool {
        self.pending_sync
            && self.pending.is_some_and(|id| {
                self.worker
                    .as_ref()
                    .is_some_and(|worker| worker.cancel_sync(id))
            })
    }

    pub fn cancel_schema_apply(&self) -> bool {
        self.pending_schema_apply
            && self.pending.is_some_and(|id| {
                self.worker
                    .as_ref()
                    .is_some_and(|worker| worker.cancel_schema_apply(id))
            })
    }

    pub fn restoring(&self) -> bool {
        self.pending_restore
    }

    pub(super) fn start_desktop_restore(&mut self, schema: String, path: PathBuf) -> bool {
        self.submit_desktop_restore(Command::Restore { schema, path })
    }

    pub(super) fn start_desktop_restore_encoded(
        &mut self,
        schema: String,
        path: PathBuf,
        charset: crate::database::SqlDumpCharset,
    ) -> bool {
        self.submit_desktop_restore(Command::RestoreEncoded {
            schema,
            path,
            charset,
        })
    }

    fn submit_desktop_restore(&mut self, command: Command) -> bool {
        if !self.send_command(command, "正在准备 SQL 还原…", false) {
            return false;
        }
        // The generic restore command also serves cross-connection Copy.
        self.restore_result = None;
        true
    }

    pub(super) fn restore_status(&self) -> Option<String> {
        if !self.restoring() {
            return None;
        }
        if self.restore_cancelling {
            return Some("正在停止还原…".into());
        }
        let Some(progress) = self.restore_progress else {
            return Some("正在准备 SQL 还原…".into());
        };
        if progress.validating {
            return Some("正在检查 SQL 文件…".into());
        }
        if progress.finishing && self.server.as_ref().is_some_and(|info| info.http_tunnel) {
            return Some("隧道正在执行 SQL 批次…".into());
        }
        Some(format!("正在还原，已执行 {} 条语句", progress.statements))
    }

    pub(super) fn can_stop_restore(&self) -> bool {
        self.restoring()
            && !self.restore_cancelling
            && self
                .restore_progress
                .is_none_or(|progress| !progress.finishing)
    }

    pub fn stop_restore(&mut self) {
        if self.pending_restore
            && self.pending.is_some_and(|id| {
                self.worker
                    .as_ref()
                    .is_some_and(|worker| worker.cancel_restore(id))
            })
        {
            self.restore_cancelling = true;
        }
    }

    pub fn start_create_table(&mut self, sql: String, schema: String) {
        if !self.busy() && self.server.is_some() {
            self.refresh_after_query = Some(RefreshAfterQuery::TableCreated(schema));
            self.start_batch(vec![sql]);
        }
    }

    pub fn start_create_database(&mut self, sql: String, schema: String) {
        if !self.busy() && self.server.is_some() {
            self.refresh_after_query = Some(RefreshAfterQuery::DatabaseCreated(schema));
            self.start_batch(vec![sql]);
        }
    }

    pub fn start_batch(&mut self, statements: Vec<String>) -> bool {
        self.start_batch_with_policy(statements, false)
    }

    fn start_batch_with_policy(&mut self, statements: Vec<String>, from_editor: bool) -> bool {
        if self.busy() {
            return false;
        }
        if self.server.is_none() {
            self.message("请先连接数据库。", true);
            return false;
        }
        if statements.is_empty() {
            self.message("光标处没有可执行的 SQL 语句。", true);
            return false;
        }
        if !self.can_replace_results(self.tab().id) {
            self.refresh_after_query = None;
            return false;
        }
        self.batch = Some(Batch {
            tab: self.tab().id,
            total: statements.len(),
            messages: from_editor.then_some(BatchMessages {
                total: statements.len(),
                statements: Vec::new(),
            }),
            queue: statements.into(),
            report: QueryReport::default(),
            command_error: None,
            current_sql: None,
            attempted: 0,
        });
        self.advance_batch();
        self.batch.is_some()
    }

    pub(super) fn start_editor_batch(&mut self, statements: Vec<String>) {
        if self.start_batch_with_policy(statements, true) {
            self.editor_query = Some(EditorQuery {
                tab: self.tab().id,
                batch_stop_button: false,
            });
        }
    }

    pub(super) fn use_batch_stop_button(&mut self) {
        if let Some(query) = &mut self.editor_query {
            query.batch_stop_button = true;
        }
    }

    pub(super) fn batch_stop_button(&self) -> bool {
        self.editor_query
            .as_ref()
            .is_some_and(|query| query.batch_stop_button)
    }

    pub(super) fn send_editor_query(&mut self, command: Command, label: &str) -> bool {
        if !self.send(command, label) {
            return false;
        }
        self.editor_query = Some(EditorQuery {
            tab: self.tab().id,
            batch_stop_button: false,
        });
        true
    }

    pub(super) fn start_f8(&mut self, index: usize, sql: String, limit: usize) -> bool {
        self.start_f8_with_mode(index, sql, limit, false)
    }

    pub(super) fn start_f8_with_mode(
        &mut self,
        index: usize,
        sql: String,
        limit: usize,
        default_read_only: bool,
    ) -> bool {
        let query = PendingEditableQuery {
            workspace: self.key,
            tab: self.tab().id,
            sql: sql.clone(),
            limit: limit.min(1_000),
        };
        if !self.send(Command::QueryF8 { sql, limit }, "正在执行查询…") {
            return false;
        }
        self.record_pending_history(query.sql.clone());
        self.pending_f8 = Some(PendingF8 {
            workspace_index: index,
            query,
            default_read_only,
        });
        true
    }

    pub(super) fn starting_f8_on_active_tab(&self) -> bool {
        self.pending_f8
            .as_ref()
            .is_some_and(|pending| pending.query.tab == self.tab().id)
    }

    pub fn start_table_diagnostics(&mut self, sql: String) -> bool {
        if self.busy() || self.server.is_none() {
            return false;
        }
        self.new_tab(sql.clone(), None);
        let command = Command::QueryInSeparateSession {
            sql: sql.clone(),
            limit: usize::MAX,
        };
        let started = self.send(command, "正在诊断所选数据表…");
        if started {
            self.record_pending_history(sql);
        }
        started
    }

    pub fn start_profiled_query(
        &mut self,
        sql: String,
        options: ProfileOptions,
        open_profile: bool,
    ) {
        match self.server.as_ref() {
            None => {
                self.message("请先连接数据库。", true);
                return;
            }
            Some(server) if server.http_tunnel => {
                self.message("查询分析可在直连或 SSH 会话中使用。", true);
                return;
            }
            Some(_) => {}
        }
        let command = Command::QueryProfiled {
            sql: sql.clone(),
            limit: self.result_limit,
            options,
            open_profile,
        };
        if self.send_editor_query(command, "正在执行查询并采集分析信息…") {
            self.record_pending_history(sql);
        }
    }

    fn advance_batch(&mut self) {
        let Some(batch) = &mut self.batch else { return };
        if let Some(sql) = batch.queue.pop_front() {
            let label = format!(
                "正在执行第 {} 条语句，共 {} 条…",
                batch.attempted + 1,
                batch.total
            );
            let command = Command::Query {
                sql: sql.clone(),
                limit: self.result_limit,
            };
            if self.send(command, &label) {
                self.record_pending_history(sql.clone());
                if let Some(batch) = &mut self.batch {
                    batch.attempted += 1;
                    batch.current_sql = Some(sql);
                }
            } else if let Some(mut batch) = self.batch.take() {
                self.refresh_after_query = None;
                if batch.attempted > 0 {
                    batch
                        .report
                        .error
                        .get_or_insert_with(|| self.status.clone());
                    self.finish_batch(batch);
                }
            }
        } else if let Some(batch) = self.batch.take() {
            self.finish_batch(batch);
        }
    }

    fn finish_batch(&mut self, batch: Batch) {
        self.publish(batch.tab, batch.report, true, batch.messages);
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == batch.tab) {
            tab.command_error = batch.command_error;
        }
    }

    pub fn stop_batch(&mut self) {
        if self.reading_editable_result() {
            self.cancel_editable_read();
            return;
        }
        if let Some(batch) = &mut self.batch {
            batch.queue.clear();
        }
        if self
            .server
            .as_ref()
            .is_some_and(|server| server.http_tunnel)
        {
            self.status = "当前 HTTP 请求结束后停止执行剩余语句。".to_owned();
            return;
        }
        if let (Some(worker), Some(id)) = (&self.worker, self.pending) {
            match worker.cancel(id) {
                Ok(()) => self.status = "正在请求取消查询…".into(),
                Err(error) => self.message(error, false),
            }
        }
    }

    pub fn poll(&mut self) -> Option<u64> {
        let mut finished_query = None;
        if self.pending_restore
            && let Some(progress) = self.pending.and_then(|id| {
                self.worker
                    .as_ref()
                    .and_then(|worker| worker.take_restore_progress(id))
            })
        {
            self.restore_progress = Some(progress);
        }
        if self.pending_sync
            && let Some(progress) = self.pending.and_then(|id| {
                self.worker
                    .as_ref()
                    .and_then(|worker| worker.take_sync_progress(id))
            })
        {
            self.sync_progress = Some(progress);
        }
        if self.pending_schema_apply
            && let Some(progress) = self.pending.and_then(|id| {
                self.worker
                    .as_ref()
                    .and_then(|worker| worker.take_schema_apply_progress(id))
            })
        {
            self.schema_apply_progress = Some(progress);
        }
        if self.pending_dump
            && let Some(progress) = self.pending.and_then(|id| {
                self.worker
                    .as_ref()
                    .and_then(|worker| worker.take_dump_progress(id))
            })
        {
            self.dump_progress = Some(progress);
        }
        while let Some(event) = self.worker.as_ref().and_then(Worker::try_recv_cancel) {
            if let Err(error) = event.result {
                let busy = self.pending == Some(event.id);
                self.message(format!("无法取消查询：{error}"), !busy);
            }
        }
        while let Some(mut event) = self.worker.as_ref().and_then(Worker::try_recv) {
            if event.id == 0 && !event.connected {
                self.note_connection_loss();
                self.clear_connection_cache();
                if let Err(error) = event.result {
                    self.message(error, true);
                }
                continue;
            }
            if self.pending != Some(event.id) {
                continue;
            }
            self.finish_pending_history(event.id, &event.result);
            if let Some(reconnect) = self
                .pending_reconnect
                .take()
                .filter(|reconnect| reconnect.request == event.id)
            {
                if reconnect.native {
                    let connected =
                        event.connected && matches!(&event.result, Ok(Response::Connected(_)));
                    self.history.note(if connected {
                        "重新连接成功"
                    } else {
                        "重新连接失败"
                    });
                }
            } else if !event.connected && !matches!(&event.result, Ok(Response::Disconnected)) {
                self.note_connection_loss();
            }
            self.pending = None;
            self.pending_query = false;
            let refresh_objects = std::mem::take(&mut self.pending_objects_refresh);
            let pending_table_read = std::mem::take(&mut self.pending_table_read);
            let pending_row_editor = self.pending_row_editor.take();
            let pending_f8 = self.pending_f8.take();
            let pending_editable_read = std::mem::take(&mut self.pending_editable_read);
            self.editable_read_stopping = false;
            if std::mem::take(&mut self.pending_restore) && !self.pending_transfer {
                let progress = self
                    .worker
                    .as_ref()
                    .and_then(|worker| worker.take_restore_progress(event.id))
                    .or(self.restore_progress);
                let outcome = match &event.result {
                    Ok(Response::Restored(count)) => RestoreOutcome::Completed(*count),
                    Ok(_) => RestoreOutcome::Failed("未收到 SQL 还原完成回执。".into()),
                    Err(error) if error == crate::database::RESTORE_CANCELLED => {
                        RestoreOutcome::Cancelled
                    }
                    Err(error) => RestoreOutcome::Failed(restore_error_summary(error)),
                };
                if let RestoreOutcome::Failed(detail) = &outcome {
                    event.result = Err(detail.clone());
                }
                self.restore_result = Some(RestoreCompletion { outcome, progress });
            }
            self.restore_progress = None;
            self.restore_cancelling = false;
            if std::mem::take(&mut self.pending_transfer) {
                self.transfer_result =
                    Some(event.result.as_ref().map(|_| ()).map_err(String::clone));
            }
            if std::mem::take(&mut self.pending_backup) {
                self.backup_result = Some(match &event.result {
                    Ok(Response::Transferred(summary)) => {
                        Ok(format!("最近一次备份完成，导出 {} 行。", summary.rows))
                    }
                    Ok(_) => Err("服务器未返回备份结果。".into()),
                    Err(error) => Err(error.clone()),
                });
            }
            if std::mem::take(&mut self.pending_dump) {
                self.dump_result = Some(match &event.result {
                    Ok(Response::Transferred(summary)) => Ok(summary.clone()),
                    Ok(_) => Err("未收到 SQL 转储结果。".into()),
                    Err(error) => Err(error.clone()),
                });
            }
            if std::mem::take(&mut self.pending_sync)
                && let Err(error) = &event.result
            {
                self.sync_reply = Some(SyncReply::Failed(error.clone()));
            }
            if std::mem::take(&mut self.pending_schema_apply) {
                self.schema_apply_progress = None;
                if let Err(error) = &event.result {
                    self.schema_apply_result = Some(Err(error.clone()));
                }
            }
            if std::mem::take(&mut self.pending_local_file_import)
                && let Err(error) = &event.result
            {
                self.local_file_import_result = Some(Err(error.clone()));
            }
            let origin = self.origin.take().unwrap_or_else(|| self.tab().id);
            let previous_output = self.output;
            if !event.connected {
                self.clear_connection_cache();
            }
            if pending_table_read {
                self.table_read_error = event.result.as_ref().err().cloned();
            }
            if pending_editable_read && pending_f8.is_none() {
                self.editable_read_error = event.result.as_ref().err().cloned();
            }
            let reply = completion::receive(self, event.result);
            if let Some(pending) = pending_f8 {
                let tab = pending.query.tab;
                if self.finish_f8(pending, reply)
                    && tab == self.tab().id
                    && self.output == Output::Results
                {
                    finished_query = Some(tab);
                }
            } else {
                match reply {
                    Ok(Some(response)) => self.receive(origin, response, refresh_objects),
                    Ok(None) => {}
                    Err(error)
                        if (pending_table_read && error == TABLE_READ_CANCELLED)
                            || (pending_editable_read && error == EDITABLE_READ_CANCELLED) =>
                    {
                        self.message(error, false);
                    }
                    Err(error) => {
                        if let Some(mut batch) = self.batch.take() {
                            let end = batch.report.sets.len();
                            batch.record_statement(
                                end..end,
                                0,
                                None,
                                QueryDiagnostics::default(),
                                Some(error),
                            );
                            self.finish_batch(batch);
                        } else {
                            let show_messages = pending_row_editor.is_none_or(|scope| {
                                scope.kind != RowEditorKind::EditableResult
                                    && scope.tab == self.tab().id
                            });
                            self.message(error, show_messages);
                        }
                    }
                }
            }
            if let Some(scope) = pending_row_editor {
                self.with_row_editor_on(scope, |workspace, editor| {
                    if let Some(editor) = editor {
                        editor.poll(workspace);
                    }
                });
            }
            if self.batch.is_none()
                && let Some(EditorQuery { tab, .. }) = self.editor_query.take()
            {
                if tab != self.tab().id {
                    self.output = previous_output;
                }
                finished_query = Some(tab);
            }
        }
        if !self.busy()
            && self.batch.is_none()
            && self.server.is_some()
            && let Some(schema) = self.pending_schema.take()
            && self
                .server
                .as_ref()
                .and_then(|server| server.database.as_deref())
                != Some(schema.as_str())
        {
            self.send(Command::UseSchema { schema }, "正在切换默认数据库…");
        }
        finished_query
    }

    fn finish_f8(&mut self, pending: PendingF8, reply: Result<Option<Response>, String>) -> bool {
        let PendingF8 {
            workspace_index,
            query,
            default_read_only,
        } = pending;
        let origin = query.tab;
        let (mut editor, messages, status) = match reply {
            Ok(Some(Response::EditablePage(page))) => {
                let status = format!("已读取 {} 行查询结果", page.rows.len());
                let messages = BatchMessages::editable(
                    query.sql.clone(),
                    page.rows.len(),
                    page.has_more,
                    page.query_timing,
                );
                (
                    RowEditor::from_query_with_mode(
                        workspace_index,
                        page,
                        query.sql,
                        query.limit,
                        default_read_only,
                    ),
                    messages,
                    status,
                )
            }
            Ok(Some(Response::JoinedEditablePage(page))) => {
                let status = format!("已读取 {} 行联表查询结果", page.result.rows.len());
                let messages = BatchMessages::editable(
                    query.sql.clone(),
                    page.result.rows.len(),
                    page.result.truncated,
                    page.query_timing,
                );
                (
                    RowEditor::from_joined_with_mode(
                        workspace_index,
                        page,
                        query,
                        default_read_only,
                    ),
                    messages,
                    status,
                )
            }
            Ok(Some(Response::Queried(report))) => {
                self.update_query_database(&report);
                let error = report.error.clone();
                let messages = BatchMessages::single(query.sql.clone(), &report);
                self.publish(origin, report, true, Some(messages));
                if let Some(error) = error {
                    if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == origin) {
                        tab.command_error = Some(ErrorDetails::new(&query.sql, &error));
                    }
                    return false;
                }
                return true;
            }
            Err(error) => {
                let report = QueryReport {
                    error: Some(error.clone()),
                    ..QueryReport::default()
                };
                let messages = BatchMessages::single(query.sql.clone(), &report);
                self.publish(origin, report, true, Some(messages));
                if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == origin) {
                    tab.command_error = Some(ErrorDetails::new(&query.sql, &error));
                }
                return false;
            }
            Ok(_) => {
                let error = "服务器未返回可编辑查询结果。".to_owned();
                let report = QueryReport {
                    error: Some(error.clone()),
                    ..QueryReport::default()
                };
                let messages = BatchMessages::single(query.sql.clone(), &report);
                self.publish(origin, report, true, Some(messages));
                if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == origin) {
                    tab.command_error = Some(ErrorDetails::new(&query.sql, &error));
                }
                return false;
            }
        };
        let scope = RowEditorScope {
            tab: origin,
            kind: RowEditorKind::EditableResult,
        };
        let installed = self
            .with_row_editor_on(scope, |workspace, slot| {
                editor.use_workspace_view(workspace);
                editor.keep_current_pane_focus();
                let index = workspace.result_mode_tab_index();
                let tab = &mut workspace.tabs[index];
                tab.report = None;
                tab.batch_messages = Some(messages);
                tab.command_error = None;
                tab.profile = None;
                tab.set = 0;
                tab.result_views = Default::default();
                tab.last_output = Output::Results;
                *slot = Some(editor);
            })
            .is_some();
        if installed {
            self.status = status;
            if self.tab().id == origin {
                self.output = Output::Results;
            }
        }
        installed
    }

    fn clear_connection_cache(&mut self) {
        self.server = None;
        self.raid_default_engine = None;
        self.schemas.clear();
        self.objects.clear();
        self.tags = completion::Metadata::default();
        self.database_defaults = None;
        self.database_contents = None;
        self.database_contents_result = None;
        self.account_reply = None;
        self.process_reply = None;
        self.server_values_reply = None;
        self.snapshots.clear();
        self.schema_designs.clear();
        self.table_keys = None;
        self.detail = None;
        self.export_columns = None;
        self.selected = None;
        self.row_page = None;
        self.table_read_error = None;
        self.editable_page = None;
        self.editable_read_error = None;
        self.duplicate_result_page = None;
        self.sync_endpoint = None;
        self.row_edit_affected_rows = None;
        self.row_edit_match_count = None;
        self.pending_schema = None;
    }

    fn note_connection_loss(&mut self) {
        if self
            .server
            .as_ref()
            .is_some_and(|server| !server.http_tunnel)
        {
            self.history.note("连接中断");
        }
    }

    pub(super) fn receive(&mut self, origin: u64, response: Response, refresh_objects: bool) {
        match response {
            Response::Connected(server) => {
                self.raid_default_engine = None;
                self.has_connected = true;
                self.message(format!("已连接到 {}", server.version), false);
                self.server = Some(server);
                self.send(Command::ListSchemas, "正在读取数据库列表…");
            }
            Response::ClonedConnection { worker, request } => {
                self.cloned_connection = Some((worker, request));
                self.status = "已打开独立的连接工作区。".into();
            }
            Response::ConnectionTest(probe) => {
                self.message(format!("连接测试成功：{}", probe.version), false)
            }
            Response::Disconnected => {
                self.clear_connection_cache();
                self.message("连接已断开", false);
            }
            Response::Schemas(mut schemas) => {
                schemas.retain(|schema| self.schema_is_displayed(schema));
                self.message(format!("已读取 {} 个数据库", schemas.len()), false);
                self.schemas = schemas;
                self.objects.clear();
                self.tags.invalidate(None);
                completion::schemas_loaded(self);
            }
            Response::Engines {
                names,
                no_backslash_escapes,
                raid_default_engine,
            } => {
                self.message(format!("已读取 {} 种可用存储引擎。", names.len()), false);
                self.engines = names;
                self.raid_default_engine = raid_default_engine;
                self.no_backslash_escapes = no_backslash_escapes;
            }
            Response::Objects { schema, objects } => {
                self.message(
                    format!("已读取 {} 个对象（{schema}）", objects.len()),
                    false,
                );
                if refresh_objects {
                    self.tags.invalidate(Some(&schema));
                } else {
                    self.tags.objects_loaded(&schema, &objects);
                }
                self.objects.insert(schema, objects);
            }
            Response::DatabaseDefaults(defaults) => {
                self.database_defaults = Some(defaults);
                self.status = "已读取字符集和排序规则。".into();
            }
            Response::DatabaseContents(contents) => {
                self.status = format!("已读取 {} 个数据库对象。", contents.objects.len());
                self.database_contents = Some(contents);
            }
            Response::Accounts(reply) => {
                self.status = "服务器已回应账户管理请求。".into();
                self.account_reply = Some(reply);
            }
            Response::Processes(reply) => {
                self.status = match &reply {
                    Ok(ProcessReply::Listed(overview)) => {
                        format!("已读取 {} 个进程。", overview.rows.len())
                    }
                    Ok(ProcessReply::Stopped { action, .. }) => {
                        format!("服务器已接受{}请求。", action.label())
                    }
                    Err(error) => error.clone(),
                };
                self.process_reply = Some(reply);
            }
            Response::ServerValues { view, values } => {
                self.status = match &values {
                    Ok(rows) => format!("已读取 {} 项{}", rows.len(), view.label()),
                    Err(error) => error.clone(),
                };
                self.server_values_reply = Some((view, values));
            }
            Response::DatabaseContentsApplied(mut result) => {
                let schema = &result.schema;
                if let Some(objects) = result.objects.take() {
                    self.objects.insert(schema.clone(), objects);
                } else {
                    self.objects.remove(schema);
                }
                self.tags.invalidate(Some(schema));
                self.snapshots.remove(schema);
                self.schema_designs.remove(schema);
                if self
                    .detail
                    .as_ref()
                    .is_some_and(|d| &d.object.schema == schema)
                {
                    self.detail = None;
                }
                if self.selected.as_ref().is_some_and(|o| &o.schema == schema) {
                    self.selected = None;
                }
                if self
                    .row_page
                    .as_ref()
                    .is_some_and(|p| &p.object.schema == schema)
                {
                    self.row_page = None;
                }
                self.database_contents = None;
                let mut message =
                    format!("已处理 {} / {} 个对象。", result.completed, result.total);
                if let Some(error) = &result.error {
                    message.push_str(error);
                }
                self.message(message, result.error.is_some());
                self.database_contents_result = Some(result);
            }
            Response::Snapshot(snapshot) => {
                self.message(
                    format!(
                        "已读取 {} 个对象定义（{}）",
                        snapshot.objects.len(),
                        snapshot.schema
                    ),
                    false,
                );
                self.snapshots.insert(snapshot.schema.clone(), snapshot);
            }
            Response::SchemaApplied(mut result) => {
                let schema = &result.schema;
                if let Some(objects) = result.objects.take() {
                    self.objects.insert(schema.clone(), objects);
                } else {
                    self.objects.remove(schema);
                }
                self.tags.invalidate(Some(schema));
                self.snapshots.remove(schema);
                self.schema_designs.remove(schema);
                if self
                    .detail
                    .as_ref()
                    .is_some_and(|detail| &detail.object.schema == schema)
                {
                    self.detail = None;
                }
                if self
                    .export_columns
                    .as_ref()
                    .is_some_and(|detail| &detail.object.schema == schema)
                {
                    self.export_columns = None;
                }
                if self
                    .selected
                    .as_ref()
                    .is_some_and(|object| &object.schema == schema)
                {
                    self.selected = None;
                }
                if self
                    .table_keys
                    .as_ref()
                    .is_some_and(|keys| &keys.object.schema == schema)
                {
                    self.table_keys = None;
                }
                if self
                    .row_page
                    .as_ref()
                    .is_some_and(|page| &page.object.schema == schema)
                {
                    self.row_page = None;
                }
                if self.editable_page.as_ref().is_some_and(|page| match page {
                    EditableResult::Table(page) => &page.object.schema == schema,
                    EditableResult::Joined(page) => page
                        .targets
                        .iter()
                        .any(|target| &target.page.object.schema == schema),
                }) {
                    self.editable_page = None;
                }
                self.database_contents = None;
                let status = match &result.stopped {
                    None => "结构同步完成",
                    Some(SchemaApplyStop::Cancelled) => "已停止结构同步",
                    Some(SchemaApplyStop::Failed {
                        uncertain: true, ..
                    }) => "无法确认最后一条结构变更",
                    Some(SchemaApplyStop::Failed { .. }) => "结构同步因错误停止",
                };
                self.message(
                    format!(
                        "{status}，已确认 {} / {} 条。",
                        result.completed, result.total
                    ),
                    matches!(result.stopped, Some(SchemaApplyStop::Failed { .. })),
                );
                self.schema_apply_result = Some(Ok(result));
            }
            Response::SchemaDesign(design) => {
                self.message(
                    format!(
                        "已读取 {} 张表和 {} 个外键。",
                        design.tables.len(),
                        design.foreign_keys.len()
                    ),
                    false,
                );
                self.schema_designs.insert(design.schema.clone(), design);
            }
            Response::TableKeys(keys) => {
                self.status = format!(
                    "已读取 {} 个索引和 {} 个外键。",
                    keys.indexes.len(),
                    keys.foreign_keys.len()
                );
                self.table_keys = Some(keys);
            }
            Response::SyncEndpoint(endpoint) => {
                self.sync_endpoint = Some(endpoint);
            }
            Response::SyncPlan(prepared) => {
                self.status = "已完成同步前比较。".into();
                self.sync_reply = Some(SyncReply::Prepared(prepared));
            }
            Response::SyncApplied(result) => {
                self.message(
                    sync_status(
                        result.source,
                        result.target,
                        result.skipped_rows,
                        result.stopped.as_deref(),
                        result.uncertain,
                    ),
                    result.skipped_rows > 0
                        || result.stopped.is_some()
                        || result.uncertain.is_some(),
                );
                self.sync_reply = Some(SyncReply::Applied(result));
            }
            Response::SyncGroupPlan(prepared) => {
                self.status = "已完成多表同步前比较。".into();
                self.sync_reply = Some(SyncReply::GroupPrepared(prepared));
            }
            Response::SyncGroupApplied(result) => {
                let label = format!(
                    "已处理 {} 张表。{}",
                    result.completed,
                    sync_status(
                        result.source,
                        result.target,
                        result.skipped_rows,
                        result.stopped.as_deref(),
                        result.uncertain
                    )
                );
                self.message(
                    label,
                    result.skipped_rows > 0
                        || result.stopped.is_some()
                        || result.uncertain.is_some(),
                );
                self.sync_reply = Some(SyncReply::GroupApplied(result));
            }
            Response::SyncScripts { prepared, files } => {
                self.status = format!("已保存 {} 份同步 SQL。", files.len());
                self.sync_reply = Some(SyncReply::Scripts(prepared, files));
            }
            Response::Transferred(summary) => self.message(
                format!(
                    "已写入文件：{} 个对象，{} 行，{} 字节。",
                    summary.objects, summary.rows, summary.bytes
                ),
                false,
            ),
            Response::Restored(count) => {
                self.message(format!("已还原，共执行 {count} 条语句。"), false);
                self.send(Command::ListSchemas, "正在刷新数据库列表…");
            }
            Response::TablePage(page) => {
                self.status = "已读取表数据".into();
                self.row_page = Some(page);
            }
            Response::EditablePage(page) => {
                self.status = format!("已读取 {} 行可编辑查询结果", page.rows.len());
                self.editable_page = Some(EditableResult::Table(page));
            }
            Response::JoinedEditablePage(page) => {
                self.status = format!("已读取 {} 行联表查询结果", page.result.rows.len());
                self.editable_page = Some(EditableResult::Joined(page));
            }
            Response::ResultRowDuplicate(page) => {
                self.status = "已核对源记录，可在草稿中编辑后插入。".into();
                self.duplicate_result_page = Some(page);
            }
            Response::TableEdited { affected_rows } => {
                self.status = format!("已保存修改，影响 {affected_rows} 行");
                self.row_edit_affected_rows = Some(affected_rows);
            }
            Response::TableEditConfirmation { matches } => {
                self.status = format!("找到 {matches} 条原值完全相同的记录，请选择处理范围。");
                self.row_edit_match_count = Some(matches);
            }
            Response::SchemaChanged(schema) => {
                if self.pending_schema.as_deref() == Some(schema.as_str()) {
                    self.pending_schema = None;
                }
                if let Some(server) = self.server.as_mut() {
                    server.database = Some(schema.clone());
                }
                self.message(format!("已将 {schema} 设为默认数据库"), false);
            }
            Response::Inspected(detail) => {
                self.message(format!("已读取 {} 的定义", detail.object.name), false);
                self.tags.remember(&detail);
                self.detail = Some(detail);
                self.output = Output::Definition;
            }
            Response::ExportColumns(detail) => {
                self.status = format!("已读取 {} 个字段。", detail.columns.len());
                self.export_columns = Some(detail);
            }
            Response::Previewed(report) => self.publish(origin, report, true, None),
            Response::Profiled {
                report,
                profile,
                open_profile,
            } => {
                let succeeded = report.error.is_none();
                let messages = BatchMessages::single(profile.sql.clone(), &report);
                self.publish(origin, report, true, Some(messages));
                if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == origin) {
                    tab.profile = Some(profile);
                }
                if succeeded && open_profile && self.tab().id == origin {
                    self.output = Output::Profile;
                }
            }
            Response::Queried(mut report) => {
                self.update_query_database(&report);
                if let Some(batch) = &mut self.batch {
                    let first = batch.report.sets.len();
                    let available = 128_usize.saturating_sub(first);
                    let keep = report.sets.len().min(available);
                    let omitted = report.omitted_sets + report.sets.len() - keep;
                    batch.report.omitted_sets += omitted;
                    batch.report.sets.extend(report.sets.drain(..keep));
                    batch.report.elapsed += report.elapsed;
                    report.diagnostics.results.truncate(keep);
                    let statement_failed = report.error.is_some();
                    batch.record_statement(
                        first..first + keep,
                        omitted,
                        Some(report.elapsed),
                        report.diagnostics,
                        report.error,
                    );
                    if self.server.is_none()
                        || (statement_failed
                            && (batch.messages.is_none() || !report.continue_after_error))
                    {
                        batch.queue.clear();
                    }
                    self.advance_batch();
                } else {
                    self.publish(origin, report, true, None);
                }
            }
            Response::ServerResourcesFlushed(targets) => {
                self.output = Output::Messages;
                self.message(format!("已完成 {targets} 项服务器刷新。"), false);
            }
            Response::Imported(rows) => self.message(format!("已导入 {rows} 行"), false),
            Response::TextImported(counts) => {
                self.message(
                    format!("已导入 {} 行。{}", counts.rows, counts.detail()),
                    false,
                );
            }
            Response::LocalFileImported(report) => {
                self.message(
                    format!(
                        "文本导入完成。受影响 {} 行，收到 {} 条警告。",
                        report.affected_rows, report.warning_count,
                    ),
                    false,
                );
                self.local_file_import_result = Some(Ok(report));
            }
            Response::OdbcImported { counts, created_in } => {
                let detail = counts.detail();
                let partial = counts.source_action.as_ref().is_some_and(Result::is_err);
                self.message(format!("已导入 {} 行。{detail}", counts.rows), partial);
                if let Some(schema) = created_in {
                    self.objects.remove(&schema);
                    self.refresh_objects(schema, "正在刷新对象列表…");
                }
            }
        }
    }

    fn update_query_database(&mut self, report: &QueryReport) {
        if let Some(schema) = report.active_database.as_ref()
            && let Some(server) = &mut self.server
        {
            if self.pending_schema.as_deref() == Some(schema.as_str()) {
                self.pending_schema = None;
            }
            server.database = Some(schema.clone());
        }
    }

    fn publish(
        &mut self,
        tab_id: u64,
        report: QueryReport,
        select_output: bool,
        batch_messages: Option<BatchMessages>,
    ) {
        let refresh_schema = self.refresh_after_query.take();
        let count = report.sets.len();
        let error = report.error.clone();
        let elapsed = report.elapsed.as_secs_f64() * 1000.0;
        let batch_summary = batch_messages.as_ref().map(BatchMessages::summary);
        let incomplete = batch_messages
            .as_ref()
            .is_some_and(|messages| messages.statements.len() < messages.total);
        let first_result = if batch_messages.is_some() {
            report.sets.iter().position(|set| !set.columns.is_empty())
        } else {
            (count > 0).then_some(0)
        };
        let output = if first_result.is_none() || error.is_some() || incomplete {
            Output::Messages
        } else {
            Output::Results
        };
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) {
            tab.report = Some(report);
            tab.batch_messages = batch_messages;
            tab.command_error = None;
            tab.profile = None;
            tab.set = first_result.unwrap_or(0);
            tab.result_views = Default::default();
            if !tab
                .editable_editor
                .as_ref()
                .is_some_and(RowEditor::has_draft_or_modal)
            {
                tab.editable_editor = None;
            }
            if select_output {
                tab.last_output = output;
            }
        }
        if select_output && self.tab().id == tab_id {
            self.output = output;
        }
        if let Some(summary) = batch_summary {
            self.status = summary;
        } else if let Some(error) = error {
            self.message(error, false);
        } else {
            self.message(
                format!("返回 {count} 个结果集，用时 {elapsed:.0} 毫秒"),
                false,
            );
            match refresh_schema {
                Some(RefreshAfterQuery::TableCreated(schema)) => {
                    self.refresh_objects(schema, "表已创建，正在刷新对象列表…");
                }
                Some(RefreshAfterQuery::DatabaseCreated(schema)) => {
                    if self.schema_is_displayed(&schema) && !self.schemas.contains(&schema) {
                        self.schemas.push(schema.clone());
                        self.schemas.sort_unstable();
                    }
                    self.send(Command::UseSchema { schema }, "数据库已创建，正在切换…");
                }
                None => {}
            }
        }
    }

    fn schema_is_displayed(&self, schema: &str) -> bool {
        let Some(profile) = &self.profile else {
            return true;
        };
        let mut names = profile
            .advanced
            .displayed_databases
            .split([';', '\n'])
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .peekable();
        names.peek().is_none() || names.any(|name| name == schema)
    }

    pub fn message(&mut self, text: impl Into<String>, error: bool) {
        self.status = text.into();
        if self.messages.len() >= 300 {
            self.messages.remove(0);
        }
        self.messages.push(self.status.clone());
        if error {
            self.output = Output::Messages;
        }
    }
}

#[cfg(test)]
mod query_cancel;

#[cfg(test)]
mod connection_tests {
    use std::time::Instant;

    use super::*;

    #[test]
    #[ignore = "需要 HARBOR_HTTP_FIXTURE_PORT 指定一次性 MariaDB"]
    fn disconnects_all_idle_workspaces_without_losing_query_drafts() {
        let port = std::env::var("HARBOR_HTTP_FIXTURE_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let drafts = ["SELECT 17", "SELECT 29"];
        let mut workspaces = drafts.map(|draft| {
            let mut workspace = Workspace::new("测试连接".into(), None);
            workspace.new_tab(String::new(), None);
            workspace.tab_mut().sql = draft.into();
            workspace.connect(
                ConnectionConfig {
                    port,
                    ..Default::default()
                },
                None,
            );
            workspace
        });
        let poll_idle = |workspaces: &mut [Workspace]| {
            let start = Instant::now();
            loop {
                for workspace in workspaces.iter_mut() {
                    workspace.poll();
                }
                if workspaces.iter().all(|workspace| !workspace.busy()) {
                    break;
                }
                assert!(start.elapsed() < Duration::from_secs(10));
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        poll_idle(&mut workspaces);
        let [a, b] = &workspaces;
        assert_ne!(
            a.server.as_ref().unwrap().connection_id,
            b.server.as_ref().unwrap().connection_id
        );
        assert!(Workspace::disconnect_all(&mut workspaces));
        poll_idle(&mut workspaces);
        for (workspace, draft) in workspaces.iter().zip(drafts) {
            assert!(workspace.server.is_none());
            assert_eq!(workspace.tabs.len(), 2);
            assert_eq!(workspace.tab().sql, draft);
            assert!(workspace.tab().saved.is_empty());
        }
    }
}
