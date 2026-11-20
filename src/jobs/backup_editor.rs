use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::database::{Command, ConnectionConfig, DbObject, ObjectKind, Response, Worker};

pub use super::backup_databases::{BackupDatabasesJob, Schema, Selection};
pub use super::{BackupJob, BackupMode, BackupOptions, JobConnection, JobObject, JobObjectKind};
use super::{
    Job, JobFile, connect_command, mail, parse_definition, read_definition_bytes, redact_values,
    request, validated_editor_bytes,
};
pub use super::{JobHttp, JobSsh, JobTls, Schedule};

#[derive(Serialize)]
pub struct Definition {
    version: u32,
    pub connection: JobConnection,
    #[serde(rename = "job")]
    pub backup: Backup,
    pub schedule: Option<Schedule>,
    pub log: Option<PathBuf>,
    pub notification: Option<mail::Notification>,
    #[serde(skip)]
    original: Option<Vec<u8>>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Backup {
    Backup(BackupJob),
    BackupDatabases(BackupDatabasesJob),
}

impl Backup {
    pub fn options_mut(&mut self) -> &mut BackupOptions {
        match self {
            Self::Backup(job) => &mut job.options,
            Self::BackupDatabases(job) => &mut job.options,
        }
    }
}

impl Definition {
    pub fn new(schema: String, object: Option<DbObject>) -> Self {
        let objects = object.map(|object| {
            vec![JobObject {
                kind: object.kind.into(),
                name: object.name,
            }]
        });
        Self {
            version: 1,
            connection: JobConnection::default(),
            backup: Backup::Backup(BackupJob {
                schema,
                output: "backups/backup-{unix_ms}.sql".into(),
                objects,
                object_kinds: None,
                options: BackupOptions::default(),
            }),
            schedule: None,
            log: None,
            notification: None,
            original: None,
        }
    }

    pub fn read(path: &Path) -> Result<Option<Self>, String> {
        let original = read_definition_bytes(path)?;
        let JobFile {
            version,
            connection,
            job,
            schedule,
            log,
            notification,
        } = parse_definition(&original)?;
        let backup = match job {
            Job::Backup(job) => Backup::Backup(job),
            Job::BackupDatabases(job) => Backup::BackupDatabases(job),
            _ => return Ok(None),
        };
        Ok(Some(Self {
            version,
            connection,
            backup,
            schedule,
            log,
            notification,
            original: Some(original),
        }))
    }

    pub fn verify_unchanged(&self, path: &Path) -> Result<(), String> {
        let original = self.original.as_ref().ok_or("找不到原备份作业内容。")?;
        if &read_definition_bytes(path)? != original {
            return Err("作业文件已在其他窗口更新，请重新打开后再编辑。".into());
        }
        Ok(())
    }

    pub fn validated_bytes(&self, destination: &Path) -> Result<Vec<u8>, String> {
        validated_editor_bytes(self, destination)
    }
}

impl Default for JobConnection {
    fn default() -> Self {
        let config = ConnectionConfig::default();
        Self {
            host: config.host,
            port: config.port,
            user: config.user,
            password_env: None,
            tls: None,
            ssh: None,
            http: None,
        }
    }
}

impl Default for JobSsh {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: super::ssh_port(),
            user: String::new(),
            identity_file: None,
            password_env: None,
            key_passphrase_env: None,
            known_hosts_path: None,
            local_port: None,
        }
    }
}

impl From<ObjectKind> for JobObjectKind {
    fn from(kind: ObjectKind) -> Self {
        match kind {
            ObjectKind::Table => Self::Table,
            ObjectKind::View => Self::View,
            ObjectKind::Procedure => Self::Procedure,
            ObjectKind::Function => Self::Function,
            ObjectKind::Trigger => Self::Trigger,
            ObjectKind::Event => Self::Event,
        }
    }
}

/// Read metadata using the same connection properties and credential references as the runner.
pub fn browse(
    connection: &JobConnection,
    directory: &Path,
    schema: Option<String>,
) -> Result<Response, String> {
    let command = schema.map_or(Command::ListSchemas, |schema| Command::ListObjects {
        schema,
    });
    browse_command(connection, directory, command)
}

pub(super) fn browse_command(
    connection: &JobConnection,
    directory: &Path,
    command: Command,
) -> Result<Response, String> {
    connection.validate()?;
    let config = connection.connection_config(directory, None)?;
    let http = connection.http_config()?;
    let ssh = config.ssh.as_ref();
    let secrets: Vec<_> = [
        config.password.as_str(),
        ssh.and_then(|ssh| ssh.password.as_deref()).unwrap_or(""),
        ssh.and_then(|ssh| ssh.key_passphrase.as_deref())
            .unwrap_or(""),
    ]
    .into_iter()
    .chain(
        http.iter()
            .flat_map(|http| http.web_auth.iter().chain(&http.proxy_auth))
            .flat_map(|(user, password)| [user.as_str(), password.as_str()]),
    )
    .collect();
    let worker = Worker::new();
    request(&worker, 1, connect_command(config.clone(), http.clone()))
        .and_then(|_| request(&worker, 2, command))
        .map_err(|error| redact_values(&error, &secrets))
}
