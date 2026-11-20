use std::ffi::OsString;
use std::fs::{self, File, TryLockError};
use std::io::{Read, Write};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::database::transfer::{
    BackupDirectoryMode, DumpOptions, DumpSettings, ExistingOutput, TransferSummary,
    backup_timestamp as sja_timestamp, validate_backup_directory,
};
use crate::database::{
    Command, ConnectionConfig, DbObject, HttpTunnelConfig, IMPORT_CANCELLED, ObjectKind, Response,
    SshConfig, TlsClientIdentity, TlsConfig, Worker,
};
use crate::restore::RESTORE_MANIFEST_NAME;

mod backup_databases;
pub mod backup_editor;
mod data_sync;
pub mod data_sync_editor;
#[cfg(test)]
mod data_sync_script_test;
mod legacy;
pub mod legacy_import;
mod legacy_notify;
mod legacy_odbc;
mod legacy_report;
mod legacy_sync;
mod mail;
pub mod notification_editor;
pub mod odbc_editor;
mod odbc_import;
mod sql;

const USAGE: &str = "用法：harbor-sql-job check|run|watch <作业文件.json>\n    check  检查定义\n    run    立即运行一次\n    watch  按定义中的间隔在前台运行\n    run-once-at <作业文件.json> <YYYY-MM-DDTHH:mm>  在计划当天执行一次\n    import-legacy <旧作业.xml> <新作业.json> <备份文件.sql 或输出目录> --password-env <变量名>\n    import-legacy <旧作业.xml> <新作业.json> <备份文件.sql 或输出目录> --passwordless";
const JOB_FILE_LIMIT: u64 = 1024 * 1024;
const TIMESTAMP_TOKEN: &str = "{unix_ms}";
const SJA_TIMESTAMP_TOKEN: &str = "{sja_timestamp}";
const BACKUP_CANCELLED: &str = "已取消 SQL 转储。";
pub const DESKTOP_ODBC_RUN: &str = "run-desktop-odbc";
pub const DESKTOP_ODBC_STOP: u8 = b'S';
pub const CANCELLED_EXIT_CODE: i32 = 130;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JobFile {
    version: u32,
    connection: JobConnection,
    job: Job,
    schedule: Option<Schedule>,
    log: Option<PathBuf>,
    notification: Option<mail::Notification>,
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JobConnection {
    pub host: String,
    #[serde(default = "mysql_port")]
    pub port: u16,
    pub user: String,
    pub password_env: Option<String>,
    pub tls: Option<JobTls>,
    pub ssh: Option<JobSsh>,
    pub http: Option<JobHttp>,
}

fn mysql_port() -> u16 {
    3306
}

#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JobTls {
    pub ca_path: Option<PathBuf>,
    pub client_certificate: Option<PathBuf>,
    pub client_key: Option<PathBuf>,
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JobSsh {
    pub host: String,
    #[serde(default = "ssh_port")]
    pub port: u16,
    pub user: String,
    pub identity_file: Option<PathBuf>,
    pub password_env: Option<String>,
    pub key_passphrase_env: Option<String>,
    pub known_hosts_path: Option<PathBuf>,
    pub local_port: Option<u16>,
}

#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JobHttp {
    pub url: String,
    pub content_type: Option<usize>,
    pub timeout: Option<u32>,
    #[serde(default)]
    pub force_base64: bool,
    #[serde(default)]
    pub utf8mb4: bool,
    pub web_user_env: Option<String>,
    pub web_password_env: Option<String>,
    pub proxy_url: Option<String>,
    pub proxy_user_env: Option<String>,
    pub proxy_password_env: Option<String>,
}

impl JobHttp {
    pub fn can_store_url(raw: &str) -> bool {
        reqwest::Url::parse(raw).is_ok_and(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
        })
    }

    fn validate(&self) -> Result<(), String> {
        if !Self::can_store_url(&self.url)
            || self
                .proxy_url
                .as_deref()
                .is_some_and(|proxy| !Self::can_store_url(proxy))
            || self
                .timeout
                .is_some_and(|timeout| !(1..=3600).contains(&timeout))
            || self
                .content_type
                .is_some_and(|index| index >= crate::database::HTTP_CONTENT_TYPES.len())
        {
            return Err("请填写有效的 HTTP 隧道地址、代理地址、内容类型和超时。保存的地址不能含认证信息、查询参数或片段。".into());
        }
        if self.web_user_env.is_some() != self.web_password_env.is_some()
            || self.proxy_user_env.is_some() != self.proxy_password_env.is_some()
            || (self.proxy_user_env.is_some() && self.proxy_url.is_none())
        {
            return Err(
                "HTTP 网页或代理认证需成对填写用户名和密码环境变量。代理认证还需代理地址。".into(),
            );
        }
        if [
            &self.web_user_env,
            &self.web_password_env,
            &self.proxy_user_env,
            &self.proxy_password_env,
        ]
        .into_iter()
        .flatten()
        .any(|name| {
            name.trim().is_empty() || name.contains('=') || name.chars().any(char::is_control)
        }) {
            return Err("HTTP 认证的环境变量名无效。".into());
        }
        Ok(())
    }

    fn resolve(&self) -> Result<HttpTunnelConfig, String> {
        let auth = |user: &Option<String>, password: &Option<String>| {
            user.as_deref()
                .zip(password.as_deref())
                .map(|(user, password)| {
                    let get = |name: &str| {
                        std::env::var(name)
                            .map_err(|_| format!("无法读取 HTTP 认证环境变量 {name}。"))
                    };
                    Ok::<_, String>((get(user)?, get(password)?))
                })
                .transpose()
        };
        Ok(HttpTunnelConfig {
            url: self.url.clone(),
            content_type: self.content_type.unwrap_or(0),
            force_base64: self.force_base64,
            use_utf8mb4: self.utf8mb4,
            web_auth: auth(&self.web_user_env, &self.web_password_env)?,
            proxy_url: self.proxy_url.clone(),
            proxy_auth: auth(&self.proxy_user_env, &self.proxy_password_env)?,
            timeout: self.timeout.unwrap_or(120),
        })
    }
}

fn http_passwords(config: &Option<HttpTunnelConfig>) -> impl Iterator<Item = &str> {
    config
        .iter()
        .flat_map(|http| [http.web_auth.as_ref(), http.proxy_auth.as_ref()])
        .flatten()
        .map(|(_, password)| password.as_str())
}

fn connect_command(config: ConnectionConfig, http: Option<HttpTunnelConfig>) -> Command {
    match http {
        Some(http) => Command::ConnectHttp(config, http),
        None => Command::Connect(config),
    }
}

fn ssh_port() -> u16 {
    22
}

impl JobConnection {
    fn http_config(&self) -> Result<Option<HttpTunnelConfig>, String> {
        self.http.as_ref().map(JobHttp::resolve).transpose()
    }

    fn validate(&self) -> Result<(), String> {
        if self.host.trim().is_empty() || self.user.trim().is_empty() || self.port == 0 {
            return Err("请填写数据库主机、有效端口和用户名。".into());
        }
        if self
            .password_env
            .as_ref()
            .is_some_and(|name| name.is_empty() || name.contains(['=', '\0']))
        {
            return Err("password_env 必须为有效的环境变量名。".into());
        }
        if let Some(tls) = &self.tls {
            if tls.client_certificate.is_some() != tls.client_key.is_some()
                || [&tls.ca_path, &tls.client_certificate, &tls.client_key]
                    .into_iter()
                    .flatten()
                    .any(|path| path.as_os_str().is_empty())
            {
                return Err("TLS 客户端证书和私钥需成对配置，证书路径不能为空。".into());
            }
        }
        if let Some(http) = &self.http {
            if self.tls.is_some() || self.ssh.is_some() {
                return Err("HTTP 隧道不能与数据库 TLS 或 SSH 同时启用。".into());
            }
            http.validate()?;
        }
        if self.ssh.as_ref().is_some_and(|ssh| {
            ssh.host.trim().is_empty() || ssh.user.trim().is_empty() || ssh.port == 0
        }) {
            return Err("请填写 SSH 主机、有效端口和用户名。".into());
        }
        if let Some(ssh) = &self.ssh {
            if ssh.identity_file.is_some() && ssh.password_env.is_some() {
                return Err("请选择 SSH 私钥或 password_env。".into());
            }
            if ssh.key_passphrase_env.is_some() && ssh.identity_file.is_none() {
                return Err("SSH 私钥口令环境变量需要同时指定 identity_file。".into());
            }
            if [&ssh.password_env, &ssh.key_passphrase_env]
                .into_iter()
                .flatten()
                .any(|name| {
                    name.trim().is_empty()
                        || name.contains('=')
                        || name.chars().any(char::is_control)
                })
                || ssh.local_port == Some(0)
                || ssh
                    .known_hosts_path
                    .as_ref()
                    .is_some_and(|path| path.as_os_str().is_empty())
            {
                return Err(
                    "请填写有效的 SSH 密码环境变量名、本地端口和 known_hosts 路径。".into(),
                );
            }
        }
        Ok(())
    }

    fn connection_config(
        &self,
        directory: &Path,
        database: Option<String>,
    ) -> Result<ConnectionConfig, String> {
        let password = self
            .password_env
            .as_ref()
            .map(|name| {
                std::env::var(name).map_err(|_| format!("无法读取数据库密码环境变量 {name}。"))
            })
            .transpose()?
            .unwrap_or_default();
        let ssh = self
            .ssh
            .as_ref()
            .map(|ssh| -> Result<SshConfig, String> {
                let password = ssh
                    .password_env
                    .as_deref()
                    .map(|name| {
                        std::env::var(name)
                            .map_err(|_| format!("无法读取 SSH 密码环境变量 {name}。"))
                    })
                    .transpose()?;
                let key_passphrase = ssh
                    .key_passphrase_env
                    .as_deref()
                    .map(|name| {
                        std::env::var(name)
                            .map_err(|_| format!("无法读取 SSH 私钥口令环境变量 {name}。"))
                    })
                    .transpose()?;
                Ok(SshConfig {
                    host: ssh.host.clone(),
                    port: ssh.port,
                    user: ssh.user.clone(),
                    identity_file: ssh.identity_file.as_ref().map(|path| directory.join(path)),
                    known_hosts_path: ssh
                        .known_hosts_path
                        .as_ref()
                        .map(|path| directory.join(path)),
                    local_port: ssh.local_port,
                    password,
                    key_passphrase,
                    ..SshConfig::default()
                })
            })
            .transpose()?;
        Ok(ConnectionConfig {
            host: self.host.clone(),
            port: self.port,
            user: self.user.clone(),
            password,
            database,
            tls: self.tls.as_ref().map(|tls| TlsConfig {
                ca_path: tls.ca_path.as_ref().map(|path| directory.join(path)),
                client_identity: tls
                    .client_certificate
                    .as_ref()
                    .zip(tls.client_key.as_ref())
                    .map(|(certificate, key)| TlsClientIdentity {
                        certificate: directory.join(certificate),
                        private_key: directory.join(key),
                    }),
            }),
            ssh,
            ..ConnectionConfig::default()
        })
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Job {
    Backup(BackupJob),
    BackupDatabases(backup_databases::BackupDatabasesJob),
    Sql(sql::SqlJob),
    OdbcImport(odbc_import::OdbcJob),
    DataSync(data_sync::DataSyncJob),
}

impl Job {
    fn cancel_message(&self) -> Option<&'static str> {
        match self {
            Self::OdbcImport(_) => Some(IMPORT_CANCELLED),
            Self::DataSync(_) => Some(data_sync::CANCELLED),
            Self::Backup(_) | Self::BackupDatabases(_) => Some(BACKUP_CANCELLED),
            Self::Sql(_) => None,
        }
    }

    fn output_path(&self, directory: &Path, timestamp: u64) -> Option<PathBuf> {
        let template = match self {
            Self::Backup(backup) => {
                let output = backup_output_path(directory, &backup.output, timestamp);
                return Some(backup_directory_path(
                    output,
                    backup.options.directory_mode,
                    timestamp,
                ));
            }
            Self::BackupDatabases(job) => return job.run_path(directory, timestamp),
            Self::Sql(script) => script.report.as_deref(),
            Self::OdbcImport(_) | Self::DataSync(_) => None,
        };
        template.map(|path| directory.join(path.replace(TIMESTAMP_TOKEN, &timestamp.to_string())))
    }

    fn input_paths(&self, directory: &Path) -> Vec<PathBuf> {
        match self {
            Self::Backup(_) | Self::BackupDatabases(_) | Self::DataSync(_) => Vec::new(),
            Self::Sql(script) => script
                .script
                .iter()
                .map(|path| directory.join(path))
                .collect(),
            Self::OdbcImport(job) => job.file_source_paths(directory),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackupJob {
    pub schema: String,
    pub output: String,
    pub objects: Option<Vec<JobObject>>,
    pub object_kinds: Option<Vec<JobObjectKind>>,
    #[serde(default)]
    pub options: BackupOptions,
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JobObject {
    pub kind: JobObjectKind,
    pub name: String,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobObjectKind {
    Table,
    View,
    Procedure,
    Function,
    Trigger,
    Event,
}

impl JobObjectKind {
    pub fn database_kind(&self) -> ObjectKind {
        match self {
            Self::Table => ObjectKind::Table,
            Self::View => ObjectKind::View,
            Self::Procedure => ObjectKind::Procedure,
            Self::Function => ObjectKind::Function,
            Self::Trigger => ObjectKind::Trigger,
            Self::Event => ObjectKind::Event,
        }
    }
}

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupMode {
    #[default]
    StructureAndData,
    Structure,
    Data,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackupOptions {
    pub mode: BackupMode,
    pub drop_existing: bool,
    pub lock_source_tables: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub lock_source_server: bool,
    pub flush_logs: bool,
    pub single_transaction: Option<bool>,
    pub chunk_rows: Option<NonZeroU32>,
    pub include_use: Option<bool>,
    pub include_create_database: Option<bool>,
    pub disable_foreign_key_checks: bool,
    pub lock_insert_tables: bool,
    pub disable_autocommit: Option<bool>,
    pub bulk_insert: bool,
    pub bulk_insert_limit_kib: Option<NonZeroU32>,
    pub complete_insert: bool,
    pub insert_delayed: bool,
    pub disable_keys: bool,
    pub overwrite: bool,
    pub append: bool,
    pub file_per_object: bool,
    #[serde(skip_serializing_if = "BackupDirectoryMode::is_per_run")]
    pub directory_mode: BackupDirectoryMode,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub split_filename_timestamp: bool,
    pub continue_on_error: bool,
    pub file_charset: crate::sql_encoding::SqlDumpCharset,
}

impl Default for BackupOptions {
    fn default() -> Self {
        let settings = DumpSettings::default();
        Self {
            mode: BackupMode::default(),
            drop_existing: settings.options.drop_existing,
            lock_source_tables: settings.lock_source_tables,
            lock_source_server: settings.lock_source_server,
            flush_logs: settings.flush_logs,
            single_transaction: None,
            chunk_rows: settings.chunk_rows,
            include_use: None,
            include_create_database: None,
            disable_foreign_key_checks: settings.disable_foreign_key_checks,
            lock_insert_tables: settings.lock_insert_tables,
            disable_autocommit: settings.disable_autocommit,
            bulk_insert: settings.bulk_insert,
            bulk_insert_limit_kib: settings.bulk_insert_limit_kib,
            complete_insert: settings.complete_insert,
            insert_delayed: settings.insert_delayed,
            disable_keys: settings.disable_keys,
            overwrite: settings.overwrite,
            append: settings.append,
            file_per_object: settings.file_per_object,
            directory_mode: settings.directory_mode,
            split_filename_timestamp: false,
            continue_on_error: settings.continue_on_error,
            file_charset: settings.file_charset,
        }
    }
}

impl BackupOptions {
    pub fn starts_transaction(&self) -> bool {
        self.single_transaction
            .unwrap_or(!self.lock_source_tables && !self.lock_source_server)
    }

    fn prepare_http(&mut self) -> Result<(), String> {
        if self.lock_source_tables
            || self.lock_source_server
            || self.single_transaction == Some(true)
        {
            return Err("HTTP 备份请使用分批读取。单一事务和读锁需要直连或 SSH 隧道。".into());
        }
        self.single_transaction = Some(false);
        Ok(())
    }

    fn existing_output(&self) -> Result<ExistingOutput, String> {
        ExistingOutput::from_flags(self.overwrite, self.append)
    }

    fn validate(&self) -> Result<(), String> {
        self.existing_output()?;
        if self.split_filename_timestamp && self.directory_mode != BackupDirectoryMode::Selected {
            Err("分文件名时间戳需要使用所选目录。".into())
        } else if self.lock_source_server && self.lock_source_tables {
            Err("全局读锁与逐表读锁不能同时使用。".into())
        } else if self.lock_source_tables && self.starts_transaction() {
            Err("锁定源表与单一事务不能同时使用。".into())
        } else {
            Ok(())
        }
    }
}

impl BackupJob {
    fn dump_settings(&self) -> DumpSettings {
        let options = &self.options;
        DumpSettings {
            options: DumpOptions {
                include_structure: !matches!(options.mode, BackupMode::Data),
                include_data: !matches!(options.mode, BackupMode::Structure),
                drop_existing: options.drop_existing,
            },
            objects: self.objects.as_ref().map(|objects| {
                objects
                    .iter()
                    .map(|object| DbObject {
                        schema: self.schema.clone(),
                        name: object.name.clone(),
                        kind: object.kind.database_kind(),
                    })
                    .collect()
            }),
            object_kinds: self
                .object_kinds
                .as_ref()
                .map(|kinds| kinds.iter().map(|kind| kind.database_kind()).collect()),
            lock_source_tables: options.lock_source_tables,
            lock_source_server: options.lock_source_server,
            flush_logs: options.flush_logs,
            single_transaction: options.starts_transaction(),
            chunk_rows: options.chunk_rows,
            include_use: options.include_use.unwrap_or(false),
            include_create_database: options.include_create_database.unwrap_or(false),
            disable_foreign_key_checks: options.disable_foreign_key_checks,
            lock_insert_tables: options.lock_insert_tables,
            disable_autocommit: options.disable_autocommit,
            bulk_insert: options.bulk_insert,
            bulk_insert_limit_kib: options.bulk_insert_limit_kib,
            complete_insert: options.complete_insert,
            insert_delayed: options.insert_delayed,
            disable_keys: options.disable_keys,
            overwrite: options.overwrite,
            append: options.append,
            file_per_object: options.file_per_object,
            directory_mode: options.directory_mode,
            split_filename_timestamp: None,
            continue_on_error: options.continue_on_error,
            file_charset: options.file_charset,
        }
    }

    fn validate(&self) -> Result<(), String> {
        validate_identifier(&self.schema, "数据库名称")?;
        if self.options.file_per_object {
            if self.output.trim().is_empty() || self.output.contains('\0') {
                return Err("请指定有效的备份输出目录。".into());
            }
            if self.options.directory_mode != BackupDirectoryMode::Selected
                && self.options.existing_output()? != ExistingOutput::Reject
            {
                return Err("分文件备份须使用所选目录才能覆盖或追加。".into());
            }
        } else {
            if !self.options.directory_mode.is_per_run() {
                return Err("指定备份目录的命名方式需要按对象分文件。".into());
            }
            validate_output(&self.output)?;
        }
        self.options.validate()?;
        let settings = self.dump_settings();
        if let Some(objects) = &settings.objects {
            for object in objects {
                validate_identifier(&object.name, "对象名称")?;
            }
        }
        let allowed =
            |kind: ObjectKind| settings.options.include_structure || kind == ObjectKind::Table;
        let explicit = settings
            .objects
            .as_ref()
            .is_some_and(|objects| objects.iter().any(|object| allowed(object.kind)));
        let dynamic = settings
            .object_kinds
            .as_ref()
            .is_some_and(|kinds| kinds.iter().any(|kind| allowed(*kind)));
        let entire_schema = self.objects.is_none() && self.object_kinds.is_none();
        if !settings.include_create_database && !entire_schema && !explicit && !dynamic {
            return Err("请为备份选择适用于当前导出模式的对象。".into());
        }
        Ok(())
    }

    fn backup(
        &self,
        worker: &Worker,
        output: &Path,
        timestamp: u64,
        cancel: &AtomicBool,
    ) -> Result<TransferSummary, String> {
        if cancel.load(Ordering::Acquire) {
            return Err(BACKUP_CANCELLED.into());
        }
        let mut settings = self.dump_settings();
        settings.split_filename_timestamp =
            self.options.split_filename_timestamp.then_some(timestamp);
        match request_cancellable(
            worker,
            3,
            Command::DumpConfigured {
                schema: self.schema.clone(),
                path: output.to_path_buf(),
                settings,
            },
            cancel,
            Worker::cancel_dump,
        )? {
            Response::Transferred(summary) => Ok(summary),
            _ => Err("数据库连接未返回备份结果。".into()),
        }
    }
}

fn validate_output(path: &str) -> Result<(), String> {
    if path.is_empty() || path.contains('\0') || Path::new(path).file_name().is_none() {
        return Err("请指定有效的输出文件路径。".into());
    }
    Ok(())
}

fn validate_identifier(name: &str, label: &str) -> Result<(), String> {
    if name.is_empty() || name.contains('\0') {
        Err(format!("请填写有效的{label}。"))
    } else {
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    pub interval_seconds: u64,
    #[serde(default = "run_immediately")]
    pub run_immediately: bool,
}

fn run_immediately() -> bool {
    true
}

impl Schedule {
    fn interval(&self) -> Result<Duration, String> {
        let interval = Duration::from_secs(self.interval_seconds);
        if interval.is_zero() || Instant::now().checked_add(interval).is_none() {
            Err("请指定有效的正数作业间隔（秒）。".into())
        } else {
            Ok(interval)
        }
    }
}

fn read_definition_bytes(path: &Path) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|error| format!("无法打开作业文件：{error}"))?;
    let mut bytes = Vec::new();
    file.take(JOB_FILE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("无法读取作业文件：{error}"))?;
    if bytes.len() as u64 > JOB_FILE_LIMIT {
        return Err("作业定义最多支持 1 MiB。".into());
    }
    Ok(bytes)
}

fn parse_definition(bytes: &[u8]) -> Result<JobFile, String> {
    // serde_json errors can echo values. Report the location without printing the input.
    let definition: JobFile = serde_json::from_slice(
        bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes),
    )
    .map_err(|error| {
        format!(
            "JSON 作业定义无效（第 {} 行，第 {} 列）。",
            error.line(),
            error.column()
        )
    })?;
    if definition.version != 1 {
        return Err("作业定义须使用 version 1。".into());
    }
    Ok(definition)
}

pub fn is_odbc_job(path: &Path) -> bool {
    read_definition_bytes(path)
        .and_then(|bytes| parse_definition(&bytes))
        .is_ok_and(|definition| matches!(definition.job, Job::OdbcImport(_)))
}

fn validated_editor_bytes(
    definition: &impl Serialize,
    destination: &Path,
) -> Result<Vec<u8>, String> {
    let name = destination.file_name().ok_or("请选择作业 JSON 文件。")?;
    let parent = destination.parent().ok_or("无法定位作业文件目录。")?;
    let directory =
        fs::canonicalize(parent).map_err(|error| format!("无法定位作业目录：{error}"))?;
    let path = directory.join(name);
    let mut bytes = serde_json::to_vec_pretty(definition).map_err(|_| "无法生成作业 JSON。")?;
    bytes.push(b'\n');
    if bytes.len() as u64 > JOB_FILE_LIMIT {
        return Err("作业定义最多支持 1 MiB。".into());
    }
    LoadedJob::from_definition(parse_definition(&bytes)?, path)?;
    Ok(bytes)
}

struct LoadedJob {
    definition: JobFile,
    path: PathBuf,
    directory: PathBuf,
    log: PathBuf,
    lock: PathBuf,
}

impl LoadedJob {
    fn read(path: &Path) -> Result<Self, String> {
        let path = fs::canonicalize(path).map_err(|error| format!("无法定位作业文件：{error}"))?;
        if !fs::metadata(&path)
            .map_err(|error| format!("无法读取作业文件属性：{error}"))?
            .is_file()
        {
            return Err("请指定普通 JSON 作业文件。".into());
        }
        let definition = parse_definition(&read_definition_bytes(&path)?)?;
        Self::from_definition(definition, path)
    }

    fn from_definition(mut definition: JobFile, path: PathBuf) -> Result<Self, String> {
        definition.connection.validate()?;
        if definition.connection.http.is_some() {
            match &mut definition.job {
                Job::Backup(job) => job.options.prepare_http()?,
                Job::BackupDatabases(job) => job.options.prepare_http()?,
                Job::Sql(_) => return Err("独立 SQL 作业需要直连或 SSH 隧道。".into()),
                Job::DataSync(_) | Job::OdbcImport(_) => {}
            }
        }
        if let Some(schedule) = &definition.schedule {
            schedule.interval()?;
        }
        let directory = path
            .parent()
            .ok_or("无法定位作业文件所在的目录。")?
            .to_path_buf();
        match &definition.job {
            Job::Backup(backup) => backup.validate()?,
            Job::BackupDatabases(job) => job.validate()?,
            Job::Sql(script) => script.validate(&directory)?,
            Job::OdbcImport(job) => job.validate()?,
            Job::DataSync(job) => {
                job.validate()?;
                if job
                    .script_directory
                    .as_ref()
                    .is_some_and(|path| directory.join(path).is_file())
                {
                    return Err("同步 SQL 保存位置必须是目录。".into());
                }
            }
        }
        if let Some(notification) = &definition.notification {
            notification.validate()?;
            if notification.include_report
                && !matches!(&definition.job, Job::Sql(script) if script.report.is_some())
            {
                return Err("邮件附件需要 SQL 作业指定 report 文件。".into());
            }
        }
        let log = definition.log.as_ref().map_or_else(
            || append_suffix(&path, ".log.jsonl"),
            |log| directory.join(log),
        );
        if log.file_name().is_none()
            || definition
                .log
                .as_ref()
                .is_some_and(|path| path.as_os_str().is_empty())
        {
            return Err("请指定日志文件路径。".into());
        }
        let lock = job_lock_path(&path);
        let started = unix_ms();
        let output = definition.job.output_path(&directory, started);
        let loaded = Self {
            definition,
            path,
            directory,
            log,
            lock,
        };
        loaded.validate_paths(output.as_deref(), started)?;
        Ok(loaded)
    }

    fn validate_paths(&self, output: Option<&Path>, started: u64) -> Result<(), String> {
        let inputs = self.definition.job.input_paths(&self.directory);
        if paths_match(&self.log, &self.path)
            || paths_match(&self.log, &self.lock)
            || inputs.iter().any(|input| {
                [&self.path, &self.log, &self.lock]
                    .into_iter()
                    .any(|path| conflicts_with_input(path, input))
            })
            || output.is_some_and(|output| {
                [&self.path, &self.log, &self.lock]
                    .into_iter()
                    .any(|path| paths_match(output, path))
                    || inputs
                        .iter()
                        .any(|input| conflicts_with_input(output, input))
            })
        {
            return Err("作业定义、锁文件、日志、输入与输出必须使用不同路径。".into());
        }
        if let (Job::BackupDatabases(job), Some(output)) = (&self.definition.job, output) {
            let manifest = job.manifest_path(output, started);
            if [self.path.as_path(), self.log.as_path(), self.lock.as_path()]
                .into_iter()
                .any(|path| paths_match(path, &manifest))
            {
                return Err("请为作业定义、锁文件、日志和多库备份清单指定不同路径。".into());
            }
            job.validate_append_path(output)?;
            if job.output_directory.is_some() {
                validate_backup_directory(
                    output,
                    job.options.directory_mode,
                    job.options.existing_output()?,
                )?;
            } else if !job.options.append && output.exists() {
                return Err("本次多库备份的输出或清单已经存在。".into());
            }
            if manifest.exists() {
                return Err("本次多库备份的输出或清单已经存在。".into());
            }
        }
        if self.log.is_dir() {
            return Err("日志路径必须指向文件。".into());
        }
        if let Some(output) = output {
            let output_is_directory = match &self.definition.job {
                Job::Backup(backup) => {
                    let mode = backup.options.directory_mode;
                    if backup.options.file_per_object && !mode.is_per_run() {
                        validate_backup_directory(output, mode, backup.options.existing_output()?)?;
                    }
                    backup.options.file_per_object
                }
                Job::BackupDatabases(job) => job.output_directory.is_some(),
                _ => false,
            };
            if output_is_directory && output.is_file() {
                return Err("按对象分文件的输出路径必须指向目录。".into());
            }
            let writes_restore_order = output_is_directory
                && !matches!(&self.definition.job, Job::Backup(backup) if backup.options.directory_mode.is_per_run());
            if writes_restore_order
                && [&self.path, &self.log, &self.lock]
                    .into_iter()
                    .any(|path| paths_match(path, &output.join(RESTORE_MANIFEST_NAME)))
            {
                return Err("请为作业定义、锁文件、日志和恢复顺序文件指定不同路径。".into());
            }
            if !output_is_directory && output.is_dir() {
                return Err("输出路径必须指向文件。".into());
            }
        }
        Ok(())
    }

    fn run_once(&self, cancel: Option<&Arc<AtomicBool>>) -> Result<(), RunError> {
        let started = unix_ms();
        let run_id = format!("{started}-{}", std::process::id());
        let output = self.definition.job.output_path(&self.directory, started);
        self.validate_paths(output.as_deref(), started)
            .map_err(RunError::Fatal)?;
        create_parent(&self.log).map_err(RunError::Fatal)?;
        self.validate_paths(output.as_deref(), started)
            .map_err(RunError::Fatal)?;
        let mut log = File::options()
            .create(true)
            .append(true)
            .open(&self.log)
            .map_err(|error| RunError::Fatal(format!("无法打开作业日志：{error}")))?;
        let path = output.as_ref().map(|path| path.display().to_string());
        write_log(
            &mut log,
            &json!({"event":"started","run_id":run_id,"unix_ms":started,"output":path}),
        )
        .map_err(RunError::Fatal)?;
        let result = output
            .as_deref()
            .map(create_parent)
            .transpose()
            .and_then(|_| self.validate_paths(output.as_deref(), started))
            .and_then(|_| self.execute(output.as_deref(), started, cancel));
        let partial_backup = match &result {
            Ok(JobSummary::Backup(summary)) => partial_backup_message(summary, true),
            _ => None,
        };
        let incomplete_odbc = match &result {
            Ok(JobSummary::OdbcImport(summary)) => summary
                .failure()
                .map(|failure| format!("{failure} {}", summary.description())),
            _ => None,
        };
        let (failure, cancelled) = match &result {
            Ok(JobSummary::Backup(_)) => (partial_backup.as_deref(), false),
            Ok(JobSummary::BackupDatabases(summary)) => {
                (summary.failure.as_deref(), summary.cancelled)
            }
            Ok(JobSummary::DataSync(summary)) => (summary.failure.as_deref(), summary.cancelled),
            Ok(JobSummary::OdbcImport(summary)) => (incomplete_odbc.as_deref(), summary.cancelled),
            Ok(_) => (None, false),
            Err(error) => (
                Some(error.as_str()),
                self.definition.job.cancel_message() == Some(error.as_str()),
            ),
        };
        let event = match (failure, cancelled) {
            (_, true) => "cancelled",
            (Some(_), false) => "failed",
            (None, false) => "succeeded",
        };
        let mut fields = result
            .as_ref()
            .map_or_else(|_| Default::default(), JobSummary::fields);
        fields.extend([
            ("event".into(), json!(event)),
            ("run_id".into(), json!(run_id)),
            ("unix_ms".into(), json!(unix_ms())),
            ("output".into(), json!(path)),
        ]);
        if let Some(error) = failure {
            fields.insert("error".into(), json!(error));
        }
        write_log(&mut log, &Value::Object(fields)).map_err(|logging| {
            RunError::Fatal(match failure {
                Some(error) => format!("{error}\n{logging}"),
                None => logging,
            })
        })?;
        if cancelled {
            return Err(RunError::Cancelled(
                failure.unwrap_or("已停止作业。").into(),
            ));
        }
        let body = match failure {
            Some(error) => format!("作业失败。{error}"),
            None => {
                let summary = result.as_ref().expect("a successful run has a summary");
                let description = summary.description();
                println!("{description}");
                if let Some(path) = path {
                    println!("{path}");
                }
                description
            }
        };
        if let Some(notification) = &self.definition.notification
            && notification.should_send(failure.is_none())
        {
            let attachment = if failure.is_none() && notification.include_report {
                output.as_deref()
            } else {
                None
            };
            match notification.send(failure.is_none(), body, attachment) {
                Ok(()) => {
                    write_log(
                        &mut log,
                        &json!({
                            "event":"notification_sent","run_id":run_id,"unix_ms":unix_ms(),
                        }),
                    )
                    .map_err(|error| RunError::Fatal(format!("SMTP 已接受通知邮件。{error}")))?;
                    println!("SMTP 已接受通知邮件。");
                }
                Err(error) => {
                    let message = match failure {
                        None => format!("作业已完成，通知发送失败。{error}"),
                        Some(execution) => format!("{execution}\n通知发送失败。{error}"),
                    };
                    write_log(&mut log, &json!({
                        "event":"notification_failed","run_id":run_id,"unix_ms":unix_ms(),"error":error,
                    })).map_err(|logging| RunError::Fatal(format!("{message}\n{logging}")))?;
                    return Err(RunError::Execution(message));
                }
            }
        }
        failure.map_or(Ok(()), |error| Err(RunError::Execution(error.into())))
    }

    fn execute(
        &self,
        output: Option<&Path>,
        started: u64,
        cancel: Option<&Arc<AtomicBool>>,
    ) -> Result<JobSummary, String> {
        let cancel_message = self.definition.job.cancel_message();
        let check_cancelled = || match (cancel, cancel_message) {
            (Some(cancel), Some(message)) if cancel.load(Ordering::Acquire) => {
                Err(message.to_owned())
            }
            _ => Ok(()),
        };
        check_cancelled()?;
        let database = match &self.definition.job {
            Job::Backup(_) | Job::BackupDatabases(_) => None,
            Job::Sql(script) => script.schema.clone(),
            Job::OdbcImport(job) => Some(job.schema.clone()),
            Job::DataSync(job) => Some(job.source_schema.clone()),
        };
        let connection = self
            .definition
            .connection
            .connection_config(&self.directory, database)?;
        let password = connection.password.clone();
        let ssh_password = connection
            .ssh
            .as_ref()
            .and_then(|ssh| ssh.password.clone())
            .unwrap_or_default();
        let ssh_key_passphrase = connection
            .ssh
            .as_ref()
            .and_then(|ssh| ssh.key_passphrase.clone())
            .unwrap_or_default();
        let http = self.definition.connection.http_config()?;
        let http_secrets = http_passwords(&http).map(str::to_owned).collect::<Vec<_>>();
        let secret_values = [
            password.as_str(),
            ssh_password.as_str(),
            ssh_key_passphrase.as_str(),
        ]
        .into_iter()
        .chain(http_secrets.iter().map(String::as_str))
        .collect::<Vec<_>>();
        let result = (|| {
            let worker = Worker::new();
            let response = request(&worker, 1, connect_command(connection, http));
            check_cancelled()?;
            if !matches!(response?, Response::Connected(_)) {
                return Err("数据库连接未返回连接状态。".into());
            }
            match &self.definition.job {
                Job::Backup(backup) => backup
                    .backup(
                        &worker,
                        output.ok_or("备份作业缺少输出路径。")?,
                        started,
                        cancel.ok_or("备份作业未初始化取消控制。")?,
                    )
                    .map(JobSummary::Backup),
                Job::BackupDatabases(job) => job
                    .run(
                        &worker,
                        output.ok_or("多库备份作业缺少输出路径。")?,
                        started,
                        &password,
                        cancel.ok_or("备份作业未初始化取消控制。")?,
                    )
                    .map(JobSummary::BackupDatabases),
                Job::Sql(job) => job
                    .run(&worker, &self.directory, output, &password)
                    .map(JobSummary::Sql),
                Job::OdbcImport(job) => job
                    .run(
                        &worker,
                        &self.directory,
                        &secret_values,
                        cancel.ok_or("ODBC 作业未初始化取消控制。")?,
                    )
                    .map(JobSummary::OdbcImport),
                Job::DataSync(job) => job
                    .run(
                        &worker,
                        &self.directory,
                        &secret_values,
                        cancel.ok_or("数据同步作业未初始化取消控制。")?,
                    )
                    .map(JobSummary::DataSync),
            }
        })();
        result.map_err(|error: String| {
            if cancel_message == Some(error.as_str()) {
                error
            } else {
                redact_values(&error, &secret_values)
            }
        })
    }
}

enum JobSummary {
    Backup(TransferSummary),
    BackupDatabases(backup_databases::BackupDatabasesSummary),
    Sql(sql::SqlSummary),
    OdbcImport(odbc_import::OdbcSummary),
    DataSync(data_sync::SyncSummary),
}

impl JobSummary {
    fn fields(&self) -> serde_json::Map<String, Value> {
        match self {
            Self::Backup(summary) => {
                let mut fields: Map<String, Value> = [
                    ("objects".into(), json!(summary.objects)),
                    ("rows".into(), json!(summary.rows)),
                    ("bytes".into(), json!(summary.bytes)),
                ]
                .into_iter()
                .collect();
                if !summary.skipped_objects.is_empty() {
                    fields.insert("skipped_objects".into(), json!(summary.skipped_objects));
                    fields.insert("published".into(), json!(true));
                }
                if let Some(path) = &summary.output_directory {
                    fields.insert("output_directory".into(), json!(path.display().to_string()));
                }
                if let Some(warning) = &summary.cleanup_warning {
                    fields.insert("cleanup_warning".into(), json!(warning));
                }
                fields
            }
            Self::BackupDatabases(summary) => summary.fields(),
            Self::OdbcImport(summary) => summary.fields(),
            Self::DataSync(summary) => summary.fields(),
            Self::Sql(summary) => [
                ("statements".into(), json!(summary.statements)),
                ("result_sets".into(), json!(summary.result_sets)),
                ("rows".into(), json!(summary.rows)),
                ("affected_rows".into(), json!(summary.affected_rows)),
                ("truncated_sets".into(), json!(summary.truncated_sets)),
                ("omitted_sets".into(), json!(summary.omitted_sets)),
            ]
            .into_iter()
            .collect(),
        }
    }

    fn description(&self) -> String {
        match self {
            Self::Backup(summary) => {
                let mut result =
                    format!("已备份 {} 个对象、{} 行。", summary.objects, summary.rows);
                if let Some(path) = &summary.output_directory {
                    result.push_str(&format!("输出目录为 {}。", path.display()));
                }
                if let Some(warning) = &summary.cleanup_warning {
                    result.push_str(warning);
                }
                result
            }
            Self::BackupDatabases(summary) => summary.description(),
            Self::Sql(summary) => format!(
                "已执行 {} 条 SQL，返回 {} 个结果集，影响 {} 行。",
                summary.statements, summary.result_sets, summary.affected_rows
            ),
            Self::OdbcImport(summary) => summary.description(),
            Self::DataSync(summary) => summary.description(),
        }
    }
}

fn partial_backup_message(summary: &TransferSummary, published: bool) -> Option<String> {
    if summary.skipped_objects.is_empty() {
        return None;
    }
    let status = if published {
        "已发布部分备份"
    } else {
        "部分备份已暂存"
    };
    let mut result = format!(
        "{status}，导出 {} 个对象、{} 行，跳过 {} 个对象。",
        summary.objects,
        summary.rows,
        summary.skipped_objects.len()
    );
    if let Some(path) = &summary.output_directory {
        result.push_str(&format!("输出目录为 {}。", path.display()));
    }
    Some(result)
}

fn redact<'a>(message: &'a str, password: &str) -> std::borrow::Cow<'a, str> {
    if password.is_empty() || !message.contains(password) {
        std::borrow::Cow::Borrowed(message)
    } else {
        std::borrow::Cow::Owned(message.replace(password, "[已隐藏]"))
    }
}

fn redact_values(message: &str, secrets: &[&str]) -> String {
    let mut ordered: Vec<_> = secrets
        .iter()
        .copied()
        .filter(|secret| !secret.is_empty())
        .collect();
    ordered.sort_unstable_by_key(|secret| std::cmp::Reverse(secret.len()));
    let mut message = std::borrow::Cow::Borrowed(message);
    for secret in ordered {
        if let std::borrow::Cow::Owned(hidden) = redact(&message, secret) {
            message = std::borrow::Cow::Owned(hidden);
        }
    }
    message.into_owned()
}

fn request(worker: &Worker, id: u64, command: Command) -> Result<Response, String> {
    worker.send(id, command)?;
    let event = worker.recv()?;
    if event.id != id {
        return Err("数据库作业收到不匹配的处理结果。".into());
    }
    event.result
}

fn request_cancellable(
    worker: &Worker,
    id: u64,
    command: Command,
    cancel: &AtomicBool,
    notify: fn(&Worker, u64) -> bool,
) -> Result<Response, String> {
    worker.send(id, command)?;
    loop {
        if cancel.load(Ordering::Acquire) {
            notify(worker, id);
        }
        if let Some(event) = worker.recv_timeout(Duration::from_millis(50))? {
            if event.id != id {
                return Err("数据库作业收到不匹配的处理结果。".into());
            }
            return event.result;
        }
    }
}

pub fn job_lock_path(canonical_path: &Path) -> PathBuf {
    append_suffix(canonical_path, ".lock")
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn create_parent(path: &Path) -> Result<(), String> {
    let parent = path.parent().ok_or("无法定位输出目录。")?;
    fs::create_dir_all(parent).map_err(|error| format!("无法创建输出目录：{error}"))
}

fn paths_match(left: &Path, right: &Path) -> bool {
    fn resolved(path: &Path) -> PathBuf {
        fs::canonicalize(path)
            .or_else(|_| {
                let parent = path.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
                Ok::<_, std::io::Error>(
                    fs::canonicalize(parent)?
                        .join(path.file_name().ok_or(std::io::ErrorKind::InvalidInput)?),
                )
            })
            .unwrap_or_else(|_| path.to_path_buf())
    }
    let left = resolved(left);
    let right = resolved(right);
    if cfg!(windows) {
        left.to_string_lossy().to_lowercase() == right.to_string_lossy().to_lowercase()
    } else {
        left == right
    }
}

fn conflicts_with_input(output: &Path, input: &Path) -> bool {
    if paths_match(output, input)
        || output.is_file()
            && input.is_file()
            && same_file::is_same_file(output, input).unwrap_or(false)
    {
        return true;
    }
    if !input.is_dir() {
        return false;
    }
    let output = fs::canonicalize(output).unwrap_or_else(|_| output.to_path_buf());
    // File-source folders contribute their direct CSV or Access files at run
    // time. An output there could replace an input or appear as a new source.
    output
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            ["csv", "mdb", "accdb"]
                .iter()
                .any(|known| ext.eq_ignore_ascii_case(known))
        })
        && output
            .parent()
            .is_some_and(|parent| paths_match(parent, input))
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn backup_output_path(directory: &Path, template: &str, timestamp: u64) -> PathBuf {
    let mut path = template.replace(TIMESTAMP_TOKEN, &timestamp.to_string());
    if path.contains(SJA_TIMESTAMP_TOKEN) {
        path = path.replace(SJA_TIMESTAMP_TOKEN, &sja_timestamp(timestamp));
    }
    directory.join(path)
}

fn backup_directory_path(root: PathBuf, mode: BackupDirectoryMode, timestamp: u64) -> PathBuf {
    if mode != BackupDirectoryMode::Timestamped {
        return root;
    }
    let name = sja_timestamp(timestamp);
    let output = root.join(&name);
    if output.exists() {
        root.join(format!("{name}-{:03}", timestamp % 1000))
    } else {
        output
    }
}

fn write_log(log: &mut File, entry: &Value) -> Result<(), String> {
    let mut line =
        serde_json::to_vec(entry).map_err(|error| format!("无法生成作业日志：{error}"))?;
    line.push(b'\n');
    log.write_all(&line)
        .and_then(|()| log.sync_data())
        .map_err(|error| format!("无法写入作业日志：{error}"))
}

enum RunError {
    Execution(String),
    Fatal(String),
    Cancelled(String),
}

#[cfg(test)]
pub(crate) fn run_fixture(path: &Path) -> Result<(), String> {
    let job = LoadedJob::read(path)?;
    job.run_once(Some(&Arc::new(AtomicBool::new(false))))
        .map_err(|error| match error {
            RunError::Execution(message)
            | RunError::Fatal(message)
            | RunError::Cancelled(message) => message,
        })
}

fn watch(
    job: &LoadedJob,
    schedule: &Schedule,
    cancel: Option<&Arc<AtomicBool>>,
) -> Result<(), RunError> {
    let interval = schedule.interval().map_err(RunError::Fatal)?;
    let now = Instant::now();
    let mut due = if schedule.run_immediately {
        now
    } else {
        next_due(now, interval).map_err(RunError::Fatal)?
    };
    loop {
        if let Some(cancel) = cancel {
            loop {
                if cancel.load(Ordering::Acquire) {
                    return Err(RunError::Cancelled("已停止作业。".into()));
                }
                let wait = due.saturating_duration_since(Instant::now());
                if wait.is_zero() {
                    break;
                }
                thread::park_timeout(wait);
            }
        } else {
            thread::sleep(due.saturating_duration_since(Instant::now()));
        }
        match job.run_once(cancel) {
            Ok(()) => {}
            Err(RunError::Execution(error)) => eprintln!("{error}"),
            Err(error) => return Err(error),
        }
        let now = Instant::now();
        due = next_due(due, interval).map_err(RunError::Fatal)?;
        if due <= now {
            due = next_due(now, interval).map_err(RunError::Fatal)?;
        }
    }
}

fn next_due(time: Instant, interval: Duration) -> Result<Instant, String> {
    time.checked_add(interval)
        .ok_or_else(|| "作业间隔超出系统计时范围。".into())
}

fn parse_once_time(value: &std::ffi::OsStr) -> Option<chrono::NaiveDateTime> {
    use chrono::Datelike as _;

    let text = value.to_str()?;
    let due = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M").ok()?;
    ((1..=9999).contains(&due.year()) && due.format("%Y-%m-%dT%H:%M").to_string() == text)
        .then_some(due)
}

fn once_is_due(due: chrono::NaiveDateTime) -> bool {
    let now = chrono::Local::now().naive_local();
    now.date() == due.date() && now >= due
}

fn claim_once(job: &LoadedJob, due: chrono::NaiveDateTime) -> Result<bool, String> {
    let path = append_suffix(&job.path, &format!(".once-{}", due.format("%Y%m%d%H%M")));
    match File::options().write(true).create_new(true).open(&path) {
        Ok(claim) => {
            // Claim before SQL execution so a later invocation cannot repeat an attempted job.
            claim
                .sync_all()
                .map_err(|error| format!("无法同步一次性作业的执行记录：{error}"))?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if fs::symlink_metadata(&path).is_ok_and(|entry| entry.is_file()) {
                Ok(false)
            } else {
                Err("一次性作业的执行记录路径已被占用。".into())
            }
        }
        Err(error) => Err(format!("无法创建一次性作业的执行记录：{error}")),
    }
}

fn read_desktop_stop(mut input: impl Read, cancel: &AtomicBool) {
    let mut command = [0];
    if input.read_exact(&mut command).is_ok() && command[0] == DESKTOP_ODBC_STOP {
        cancel.store(true, Ordering::Release);
    }
}

#[cfg(test)]
#[test]
fn desktop_stop_request_sets_the_shared_cancellation_flag() {
    let cancel = AtomicBool::new(false);
    let (input, mut output) = std::io::pipe().unwrap();
    output.write_all(&[DESKTOP_ODBC_STOP]).unwrap();
    drop(output);
    read_desktop_stop(input, &cancel);
    assert!(cancel.load(Ordering::Acquire));
}

/// Dispatch the headless command, returning an exit code for the caller.
pub fn cli(args: impl IntoIterator<Item = OsString>) -> i32 {
    let args: Vec<_> = args.into_iter().collect();
    if matches!(args.as_slice(), [arg] if arg == "--help" || arg == "-h") {
        println!(
            "{USAGE}\n    {}\n    {}\n    {}",
            legacy_sync::USAGE,
            legacy_notify::USAGE,
            legacy_odbc::USAGE
        );
        return 0;
    }
    if args.first().is_some_and(|arg| arg == "import-legacy") {
        return match legacy::import(&args[1..]) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("{error}");
                2
            }
        };
    }
    if args.first().is_some_and(|arg| arg == "import-legacy-sync") {
        return match legacy_sync::import(&args[1..]) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("{error}");
                2
            }
        };
    }
    if args.first().is_some_and(|arg| arg == "import-legacy-odbc") {
        return match legacy_odbc::import(&args[1..]) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("{error}");
                2
            }
        };
    }
    if args
        .first()
        .is_some_and(|arg| arg == "import-legacy-notify")
    {
        return match legacy_notify::import(&args[1..]) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("{error}");
                2
            }
        };
    }
    let (operation, file, once_at) = match args.as_slice() {
        [operation, file] if matches!(operation.to_str(), Some("check" | "run" | "watch")) => {
            (operation.to_str().unwrap(), file, None)
        }
        [operation, file] if operation == DESKTOP_ODBC_RUN => (DESKTOP_ODBC_RUN, file, None),
        [operation, file, value] if operation == "run-once-at" => {
            let Some(due) = parse_once_time(value) else {
                eprintln!("仅一次调度须使用 YYYY-MM-DDTHH:mm 的本地日期及时间。");
                return 2;
            };
            if !once_is_due(due) {
                return 0;
            }
            ("run", file, Some(due))
        }
        _ => {
            eprintln!("{USAGE}");
            return 2;
        }
    };
    let job = match LoadedJob::read(Path::new(file)) {
        Ok(job) => job,
        Err(error) => {
            eprintln!("{error}");
            return 2;
        }
    };
    let desktop_odbc = operation == DESKTOP_ODBC_RUN;
    if desktop_odbc && !matches!(job.definition.job, Job::OdbcImport(_)) {
        eprintln!("此运行入口需要 ODBC 导入作业。");
        return 2;
    }
    if operation == "check" {
        println!("作业定义有效。");
        return 0;
    }
    let schedule = if operation == "watch" {
        match job.definition.schedule.as_ref() {
            Some(schedule) => Some(schedule),
            None => {
                eprintln!("watch 需要在作业文件中设置 schedule。");
                return 2;
            }
        }
    } else {
        None
    };
    let lock = match File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&job.lock)
    {
        Ok(lock) => lock,
        Err(error) => {
            eprintln!("无法打开作业锁文件 {}：{error}", job.lock.display());
            return 1;
        }
    };
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            eprintln!("此作业正在运行。");
            return 3;
        }
        Err(TryLockError::Error(error)) => {
            eprintln!("无法锁定作业：{error}");
            return 1;
        }
    }
    if let Some(due) = once_at {
        if !once_is_due(due) {
            return 0;
        }
        match claim_once(&job, due) {
            Ok(true) => {}
            Ok(false) => return 0,
            Err(error) => {
                eprintln!("{error}");
                return 1;
            }
        }
    }
    let cancel = if job.definition.job.cancel_message().is_some() {
        let cancel = Arc::new(AtomicBool::new(false));
        let requested = Arc::clone(&cancel);
        if desktop_odbc {
            if let Err(error) = thread::Builder::new()
                .name("harbor-odbc-stop".into())
                .spawn(move || read_desktop_stop(std::io::stdin(), &requested))
            {
                eprintln!("无法监听桌面停止请求：{error}");
                return 1;
            }
        } else {
            let main = thread::current();
            if let Err(error) = ctrlc::set_handler(move || {
                requested.store(true, Ordering::Release);
                main.unpark();
            }) {
                eprintln!("无法监听作业的取消请求：{error}");
                return 1;
            }
        }
        Some(cancel)
    } else {
        None
    };
    let result = match schedule {
        Some(schedule) => watch(&job, schedule, cancel.as_ref()),
        None => job.run_once(cancel.as_ref()),
    };
    match result {
        Ok(()) => 0,
        Err(RunError::Fatal(error) | RunError::Execution(error)) => {
            eprintln!("{error}");
            1
        }
        Err(RunError::Cancelled(message)) => {
            if desktop_odbc && message == IMPORT_CANCELLED {
                eprintln!("{message} 本次导入的数据未发布到目标表。");
            } else {
                eprintln!("{message}");
            }
            CANCELLED_EXIT_CODE
        }
    }
}
