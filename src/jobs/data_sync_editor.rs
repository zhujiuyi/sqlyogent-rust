use std::path::{Path, PathBuf};

use serde::{Serialize, Serializer};

use crate::database::{Command, DbObject, ObjectKind, Response};

pub use super::data_sync::{Conflict, DataSyncJob, FilterScope, Mode, TableMapping, TablePair};
use super::{
    Job, JobConnection, JobFile, Schedule, backup_editor::browse_command, mail, parse_definition,
    read_definition_bytes, validated_editor_bytes,
};

#[derive(Serialize)]
pub struct Definition {
    version: u32,
    pub connection: JobConnection,
    #[serde(serialize_with = "serialize_sync")]
    pub job: DataSyncJob,
    pub schedule: Option<Schedule>,
    pub log: Option<PathBuf>,
    notification: Option<mail::Notification>,
    #[serde(skip)]
    original: Option<Vec<u8>>,
}

impl Definition {
    pub fn new(source_schema: String) -> Self {
        Self {
            version: 1,
            connection: JobConnection::default(),
            job: DataSyncJob::new(source_schema),
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
        let Job::DataSync(job) = job else {
            return Ok(None);
        };
        Ok(Some(Self {
            version,
            connection,
            job,
            schedule,
            log,
            notification,
            original: Some(original),
        }))
    }

    pub fn verify_unchanged(&self, path: &Path) -> Result<(), String> {
        let original = self.original.as_ref().ok_or("找不到原数据同步作业内容。")?;
        if &read_definition_bytes(path)? != original {
            return Err("作业文件已在其他窗口更新，请重新打开后再编辑。".into());
        }
        Ok(())
    }

    pub fn validated_bytes(&self, destination: &Path) -> Result<Vec<u8>, String> {
        validated_editor_bytes(self, destination)
    }
}

fn serialize_sync<S: Serializer>(job: &DataSyncJob, serializer: S) -> Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum TaggedJob<'a> {
        DataSync(&'a DataSyncJob),
    }
    TaggedJob::DataSync(job).serialize(serializer)
}

#[derive(Clone)]
pub enum Browse {
    Schemas,
    Tables(String),
    Columns { schema: String, table: String },
}

pub fn browse(
    connection: &JobConnection,
    directory: &Path,
    query: Browse,
) -> Result<Vec<String>, String> {
    let command = match query {
        Browse::Schemas => Command::ListSchemas,
        Browse::Tables(schema) => Command::ListObjects { schema },
        Browse::Columns { schema, table } => Command::ExportColumns {
            object: DbObject {
                schema,
                name: table,
                kind: ObjectKind::Table,
            },
        },
    };
    match browse_command(connection, directory, command)? {
        Response::Schemas(schemas) => Ok(schemas),
        Response::Objects { objects, .. } => Ok(objects
            .into_iter()
            .filter(|object| object.kind == ObjectKind::Table)
            .map(|object| object.name)
            .collect()),
        Response::ExportColumns(detail) => Ok(detail
            .columns
            .into_iter()
            .map(|column| column.name)
            .collect()),
        _ => Err("读取数据库对象未返回结果。".into()),
    }
}
