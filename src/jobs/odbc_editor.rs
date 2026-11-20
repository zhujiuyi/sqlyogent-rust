use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use serde::{Serialize, Serializer};

use crate::database::{DbObject, odbc_create_table_sql};
pub use crate::odbc::SourceTable;
use crate::odbc::{DiscoveredTable, SourceColumn, SourceIndex, mapping::map_indexes};

pub use super::odbc_import::{
    Column, Discovery, EffectiveTableStrategy, ExistingTarget, MAX_TABLES, MissingTarget, OdbcJob,
    Source, Table, TableStrategy,
};
use super::{
    Job, JobConnection, JobFile, Schedule, mail, parse_definition, read_definition_bytes,
    validated_editor_bytes,
};

#[derive(Serialize)]
pub struct Definition {
    version: u32,
    pub connection: JobConnection,
    #[serde(serialize_with = "serialize_odbc_job")]
    pub job: OdbcJob,
    pub schedule: Option<Schedule>,
    pub log: Option<PathBuf>,
    pub notification: Option<mail::Notification>,
    #[serde(skip)]
    original: Option<Vec<u8>>,
}

impl Definition {
    pub fn new(schema: String) -> Self {
        Self {
            version: 1,
            connection: JobConnection::default(),
            job: OdbcJob::new(schema),
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
        let Job::OdbcImport(mut job) = job else {
            return Ok(None);
        };
        job.normalize_for_edit()?;
        if !job.file_source_paths(Path::new("")).is_empty() {
            let original_path = path
                .canonicalize()
                .map_err(|error| format!("无法定位文件来源作业。{error}"))?;
            let directory = original_path.parent().ok_or("无法定位作业文件的目录。")?;
            job.resolve_file_source_paths(directory);
        }
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
        let original = self.original.as_ref().ok_or("找不到原 ODBC 作业内容。")?;
        if &read_definition_bytes(path)? != original {
            return Err("作业文件已在其他窗口更新，请重新打开后再编辑。".into());
        }
        Ok(())
    }

    pub fn validated_bytes(&self, destination: &Path) -> Result<Vec<u8>, String> {
        validated_editor_bytes(self, destination)
    }
}

fn serialize_odbc_job<S: Serializer>(job: &OdbcJob, serializer: S) -> Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum TaggedJob<'a> {
        OdbcImport(&'a OdbcJob),
    }
    TaggedJob::OdbcImport(job).serialize(serializer)
}

#[derive(Default)]
pub struct Description {
    pub columns: Vec<SourceColumn>,
    indexes: Vec<SourceIndex>,
    indexes_from: Option<SourceTable>,
}

pub fn describe(source: &Source) -> Result<Description, String> {
    source.validate()?;
    source
        .read(Path::new(""), &AtomicBool::new(false), true)
        .map(|data| Description {
            columns: data.columns,
            indexes: data.indexes,
            indexes_from: source.indexes_from.clone(),
        })
}

impl Description {
    pub fn create_sql(&self, object: &DbObject, table: &Table) -> Result<String, String> {
        if table.columns.is_empty() {
            return Err("请先设置列映射。".into());
        }
        let definitions = table
            .columns
            .iter()
            .map(|column| {
                column
                    .definition
                    .clone()
                    .or_else(|| {
                        self.columns
                            .iter()
                            .find(|source| {
                                source.name.to_lowercase() == column.source.to_lowercase()
                            })
                            .map(|source| source.definition.clone())
                    })
                    .ok_or_else(|| format!("请读取源列或为「{}」指定类型。", column.target))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let names = table
            .columns
            .iter()
            .map(|column| column.target.clone())
            .collect::<Vec<_>>();
        let mut indexes = Vec::new();
        if let Some(locator) = &table.source.indexes_from {
            if self.indexes_from.as_ref() != Some(locator) {
                return Err("请重新读取源列，以获取用于生成建表 SQL 的索引。".into());
            }
            let mapping = table
                .columns
                .iter()
                .map(|column| (column.source.clone(), column.target.clone()))
                .collect::<Vec<_>>();
            indexes = map_indexes(&self.indexes, &mapping)?;
        }
        odbc_create_table_sql(
            object,
            &names,
            &definitions,
            &indexes,
            table.engine.as_deref(),
        )
    }
}

pub fn discover(source: &Source) -> Result<Vec<DiscoveredTable>, String> {
    source.discover(Path::new(""))
}

pub fn map_discovered_table(
    source: &Source,
    discovered: DiscoveredTable,
    target: String,
    copy_indexes: bool,
    copy_foreign_keys: bool,
) -> Result<(Table, Description), String> {
    super::validate_identifier(&target, "目标表名称")?;
    let mut source = source.clone();
    let from_file = source.file_source.is_some();
    if let Some(file) = &source.file_source {
        if !file.supports_metadata() && (copy_indexes || copy_foreign_keys) {
            return Err("CSV 和 Excel 文件没有可复制的数据库索引或外键。".into());
        }
        source.file_table = Some(discovered.source.table.clone());
    } else {
        source.sql = discovered.select_sql;
    }
    source.after_import = None;
    source.indexes_from = (copy_indexes || copy_foreign_keys).then_some(discovered.source);
    source.copy_foreign_keys = copy_foreign_keys;
    let detected = describe(&source)?;
    let detected_columns = &detected.columns;
    let mut unique = HashSet::new();
    if detected_columns.is_empty()
        || detected_columns.iter().any(|column| {
            column.name.trim().is_empty()
                || column.name.contains('\0')
                || !unique.insert(column.name.to_lowercase())
        })
    {
        return Err(if from_file {
            "文件表没有可区分的列名。请检查表头。".into()
        } else {
            "源表没有可区分的列名。可用源查询为各列指定别名。".into()
        });
    }
    let columns = detected_columns
        .iter()
        .map(|column| Column {
            source: column.name.clone(),
            target: if column.name.chars().count() <= 64 && !column.name.ends_with(' ') {
                column.name.clone()
            } else {
                String::new()
            },
            definition: None,
        })
        .collect();
    Ok((
        Table {
            source,
            table: target,
            columns,
            conflict: None,
            strategy: None,
            create_sql: None,
            engine: None,
        },
        detected,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_access_link_folder_when_saving_a_job_elsewhere() {
        let executable = std::env::current_exe().unwrap();
        let fixture = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let original = fixture.path().join("来源");
        let links = original.join("链接数据");
        let exports = fixture.path().join("另存");
        std::fs::create_dir_all(&links).unwrap();
        std::fs::create_dir(&exports).unwrap();
        let database = original.join("前端.accdb");
        std::fs::write(&database, "配置路径样例").unwrap();
        let path = original.join("迁移.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "connection": {"host": "127.0.0.1", "user": "root"},
                "job": {
                    "type": "odbc_import", "schema": "target", "create_table": true,
                    "discovery": {"source": {"file_source": {
                        "format": "access", "path": "前端.accdb", "access_link_folder": "链接数据"
                    }}}
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let definition = Definition::read(&path).unwrap().unwrap();
        let saved = exports.join("新作业.json");
        let bytes = definition.validated_bytes(&saved).unwrap();
        let job: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let source = &job["job"]["discovery"]["source"]["file_source"];
        let path = |key: &str| {
            PathBuf::from(source[key].as_str().unwrap())
                .canonicalize()
                .unwrap()
        };
        assert_eq!(path("path"), database.canonicalize().unwrap());
        assert_eq!(path("access_link_folder"), links.canonicalize().unwrap());
    }

    #[test]
    fn describes_and_keeps_relative_file_source_when_editing_a_job() {
        let executable = std::env::current_exe().unwrap();
        let folder = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let job_path = folder.path().join("file-import.json");
        let csv_path = folder.path().join("inventory.csv");
        std::fs::write(&csv_path, "id,label\n1,stock\n").unwrap();
        std::fs::write(&job_path, serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "connection": {"host":"127.0.0.1","user":"fixture"},
            "job": {
                "type": "odbc_import", "schema": "warehouse", "table": "stock",
                "source": {"file_source":{"format":"csv", "path":"inventory.csv"}, "file_table":"inventory"},
                "columns": [{"source":"id", "target":"id"}]
            }
        })).unwrap()).unwrap();

        let mut definition = Definition::read(&job_path).unwrap().unwrap();
        let table = &mut definition.job.tables.as_mut().unwrap()[0];
        let file = table.source.file_source.as_ref().unwrap();
        assert_eq!(file.path, csv_path.canonicalize().unwrap());
        let discovery = discover(&table.source).unwrap();
        assert_eq!(discovery.len(), 1);
        assert_eq!(discovery[0].source.table, "inventory");
        let (mapped, columns) = map_discovered_table(
            &table.source,
            discovery.into_iter().next().unwrap(),
            "stock".into(),
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            columns
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            ["id", "label"]
        );
        assert_eq!(mapped.source.file_table.as_deref(), Some("inventory"));
        *table = mapped;
        let other = folder.path().join("separate-job-dir");
        std::fs::create_dir(&other).unwrap();
        let saved = other.join("new-import.json");
        std::fs::write(&saved, definition.validated_bytes(&saved).unwrap()).unwrap();
        let reopened = Definition::read(&saved).unwrap().unwrap();
        let input = &reopened.job.tables.as_ref().unwrap()[0].source;
        assert_eq!(
            input.file_source.as_ref().unwrap().path,
            csv_path.canonicalize().unwrap()
        );
        assert_eq!(describe(input).unwrap().columns.len(), 2);
    }

    #[test]
    fn edits_a_single_table_odbc_job_for_the_normal_runner() {
        let root = std::env::current_exe().unwrap();
        let folder = tempfile::tempdir_in(root.parent().unwrap()).unwrap();
        let path = folder.path().join("导入物流.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "connection": {"host": "127.0.0.1", "user": "fixture"},
                "job": {
                    "type": "odbc_import",
                    "schema": "warehouse",
                    "table": "stock",
                    "source": {"connection_env": "HARBOR_FIXTURE_ODBC", "sql": "SELECT id FROM stock"},
                    "columns": [{"source": "id", "target": "id"}]
                },
            }))
            .unwrap(),
        )
        .unwrap();
        let mut definition = Definition::read(&path).unwrap().unwrap();
        definition.job.create_table = true;
        let table = &mut definition.job.tables.as_mut().unwrap()[0];
        assert_eq!(table.table, "stock");
        table.columns[0].target = "stock_id".into();
        table.create_sql = Some("CREATE TABLE stock (stock_id INT) COMMENT='物流库存';".into());
        definition.verify_unchanged(&path).unwrap();
        std::fs::write(&path, definition.validated_bytes(&path).unwrap()).unwrap();
        assert_eq!(
            crate::jobs::cli(["check".into(), path.clone().into_os_string()]),
            0
        );
        let reopened = Definition::read(&path).unwrap().unwrap();
        let table = &reopened.job.tables.as_ref().unwrap()[0];
        assert_eq!(table.columns[0].target, "stock_id");
        assert_eq!(
            table.create_sql.as_deref(),
            Some("CREATE TABLE stock (stock_id INT) COMMENT='物流库存';")
        );
        assert_eq!(
            table.source.connection_env.as_deref(),
            Some("HARBOR_FIXTURE_ODBC")
        );
    }
}
