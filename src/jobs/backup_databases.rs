use std::collections::HashSet;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tempfile::{Builder, NamedTempFile};

use crate::database::transfer::{
    BackupDirectoryMode, ExistingOutput, SkippedDumpObject, copy_output, publish_output,
    publish_split_directory, split_generation_suffix, split_timestamp_prefix,
    validate_backup_directory, validate_output_filename, write_restore_order,
};
use crate::database::{Command, Response, Worker};

use super::{
    BACKUP_CANCELLED, BackupJob, BackupOptions, JobObject, JobObjectKind, append_suffix,
    backup_directory_path, backup_output_path, partial_backup_message, redact, request,
    validate_identifier, validate_output, write_log,
};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackupDatabasesJob {
    #[serde(default)]
    pub all_user_schemas: bool,
    pub schemas: Option<Vec<Schema>>,
    #[serde(default)]
    pub include_mysql: bool,
    pub output_file: Option<String>,
    pub output_directory: Option<String>,
    #[serde(default)]
    pub continue_on_error: bool,
    #[serde(default)]
    pub options: BackupOptions,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Schema {
    Whole(String),
    Selected(Selection),
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    pub schema: String,
    pub objects: Option<Vec<JobObject>>,
    pub object_kinds: Option<Vec<JobObjectKind>>,
}

impl Schema {
    pub fn name(&self) -> &str {
        match self {
            Self::Whole(schema) => schema,
            Self::Selected(selected) => &selected.schema,
        }
    }

    fn backup(&self, output: String, mut options: BackupOptions) -> BackupJob {
        options.append = false;
        options.overwrite = false;
        options.directory_mode = BackupDirectoryMode::PerRun;
        options.split_filename_timestamp = false;
        options.include_use.get_or_insert(true);
        options.include_create_database.get_or_insert(true);
        let (objects, kinds) = match self {
            Self::Whole(_) => (None, None),
            Self::Selected(selection) => {
                (selection.objects.clone(), selection.object_kinds.clone())
            }
        };
        BackupJob {
            schema: self.name().into(),
            output,
            objects,
            object_kinds: kinds,
            options,
        }
    }
}

#[derive(Default, Serialize)]
pub(super) struct BackupDatabasesSummary {
    requested: usize,
    completed: usize,
    failed: usize,
    objects: usize,
    rows: u64,
    bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_offset: Option<u64>,
    manifest: String,
    published: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    cleanup_warning: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    skipped_objects: Vec<SkippedDumpObject>,
    #[serde(skip)]
    pub(super) failure: Option<String>,
    #[serde(skip)]
    pub(super) cancelled: bool,
}

impl BackupDatabasesSummary {
    pub(super) fn fields(&self) -> Map<String, Value> {
        let Value::Object(fields) = json!(self) else {
            unreachable!("backup summary is a struct")
        };
        fields
    }

    pub(super) fn description(&self) -> String {
        format!(
            "已转储 {}/{} 个数据库、{} 行。{}",
            self.completed,
            self.requested,
            self.rows,
            self.cleanup_warning.as_deref().unwrap_or("")
        )
    }
}

impl BackupDatabasesJob {
    pub(super) fn validate(&self) -> Result<(), String> {
        let existing = self.options.existing_output()?;
        match (&self.output_file, &self.output_directory) {
            (Some(file), None) => {
                if !self.options.directory_mode.is_per_run() {
                    return Err("指定备份目录的命名方式需要按数据库分文件。".into());
                }
                if existing == ExistingOutput::Replace {
                    return Err("多库备份的 overwrite 需要指定所选输出目录。".into());
                }
                validate_output(file)?;
                validate_output_filename(Path::new(file))?;
                if self.options.include_use == Some(false) {
                    return Err("多库合并 SQL 需要启用 include_use 以切换每个数据库。".into());
                }
            }
            (None, Some(directory))
                if !directory.trim().is_empty() && !directory.contains('\0') =>
            {
                if existing != ExistingOutput::Reject && !self.uses_selected_directory() {
                    return Err("分文件备份须使用所选目录才能覆盖或追加。".into());
                }
            }
            _ => return Err("请为多库备份指定 output_file 或 output_directory。".into()),
        }
        self.options.validate()?;
        if self.options.file_per_object {
            return Err("多库备份的 file_per_object 须为 false。".into());
        }
        match (&self.schemas, self.all_user_schemas) {
            (None, true) => Ok(()),
            (Some(schemas), false) if !schemas.is_empty() => {
                let mut names = HashSet::new();
                for (index, schema) in schemas.iter().enumerate() {
                    let name = schema.name();
                    validate_identifier(name, "数据库名称")?;
                    if !names.insert(name) {
                        return Err("多库备份的数据库名称不能重复。".into());
                    }
                    if excluded_system_schema(name) {
                        return Err(format!("多库备份不支持系统数据库 {name}。"));
                    }
                    schema
                        .backup(file_name(index, name, "", ""), self.options.clone())
                        .validate()?;
                }
                if names.iter().any(|name| mysql_schema(name)) != self.include_mysql {
                    return Err(
                        "备份 mysql 系统库需要同时在 schemas 中选择它并设置 include_mysql。".into(),
                    );
                }
                Ok(())
            }
            _ => Err("请为多库备份选择非空 schemas 或设置 all_user_schemas。".into()),
        }
    }

    pub(super) fn run_path(&self, directory: &Path, timestamp: u64) -> Option<PathBuf> {
        if let Some(output) = &self.output_file {
            Some(backup_output_path(directory, output, timestamp))
        } else {
            self.output_directory.as_ref().map(|root| {
                let root = directory.join(root);
                let mode = self.options.directory_mode;
                if mode.is_per_run() {
                    root.join(format!("{timestamp}-{}", std::process::id()))
                } else {
                    backup_directory_path(root, mode, timestamp)
                }
            })
        }
    }

    pub(super) fn manifest_path(&self, output: &Path, timestamp: u64) -> PathBuf {
        if self.options.append || self.uses_selected_directory() {
            append_suffix(
                output,
                &format!(".{timestamp}-{}.manifest.jsonl", std::process::id()),
            )
        } else if self.output_file.is_some() {
            append_suffix(output, ".manifest.jsonl")
        } else {
            output.join("manifest.jsonl")
        }
    }

    pub(super) fn validate_append_path(&self, output: &Path) -> Result<(), String> {
        if self.output_file.is_none() || !self.options.append {
            return Ok(());
        }
        validate_output_filename(output)?;
        match fs::symlink_metadata(output) {
            Ok(metadata) if !metadata.is_file() => Err("追加备份需要普通文件。".into()),
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("无法读取多库 SQL 输出属性：{error}")),
        }
    }

    fn uses_selected_directory(&self) -> bool {
        self.output_directory.is_some()
            && self.options.directory_mode == BackupDirectoryMode::Selected
    }

    fn selected_schemas(&self, visible: &[String]) -> Vec<Schema> {
        if let Some(schemas) = &self.schemas {
            return schemas.clone();
        }
        let mut selected: Vec<_> = visible
            .iter()
            .filter(|name| {
                !excluded_system_schema(name) && (!mysql_schema(name) || self.include_mysql)
            })
            .cloned()
            .collect();
        if self.include_mysql && !selected.iter().any(|name| mysql_schema(name)) {
            selected.push("mysql".into());
        }
        selected.sort();
        selected.dedup();
        selected.into_iter().map(Schema::Whole).collect()
    }

    pub(super) fn run(
        &self,
        worker: &Worker,
        output: &Path,
        timestamp: u64,
        password: &str,
        cancel: &AtomicBool,
    ) -> Result<BackupDatabasesSummary, String> {
        let Response::Schemas(visible) = request(worker, 2, Command::ListSchemas)? else {
            return Err("数据库连接未返回可见数据库列表。".into());
        };
        check_cancelled(cancel)?;
        let schemas = self.selected_schemas(&visible);
        if schemas.is_empty() {
            return Err("没有当前账户可备份的用户数据库。".into());
        }
        let visible: HashSet<_> = visible.iter().map(String::as_str).collect();
        let combined = self.output_file.is_some();
        let selected_directory = self.uses_selected_directory();
        let existing = self.options.existing_output()?;
        let staging = if combined || selected_directory {
            let parent = output.parent().ok_or("无法定位多库 SQL 输出目录。")?;
            Some(
                Builder::new()
                    .prefix(".harbor-backup-")
                    .tempdir_in(parent)
                    .map_err(|error| format!("无法创建多库备份临时目录：{error}"))?,
            )
        } else {
            validate_backup_directory(output, self.options.directory_mode, existing)?;
            fs::create_dir(output).map_err(|error| format!("无法创建本次多库备份目录：{error}"))?;
            None
        };
        let work = staging
            .as_ref()
            .map_or(output, |directory| directory.path());
        let generation_suffix = if selected_directory {
            split_generation_suffix(staging.as_ref().unwrap(), existing)?
        } else {
            ""
        };
        let timestamp_prefix =
            split_timestamp_prefix(self.options.split_filename_timestamp.then_some(timestamp));
        let pathname = self.manifest_path(output, timestamp);
        let mut manifest = File::options()
            .write(true)
            .create_new(true)
            .open(&pathname)
            .map_err(|error| format!("无法创建多库备份清单：{error}"))?;
        write_log(
            &mut manifest,
            &json!({
                "event":"started", "version":1, "include_mysql":self.include_mysql,
                "schemas":schemas.iter().map(Schema::name).collect::<Vec<_>>(),
                "append":self.options.append,
                "overwrite":self.options.overwrite,
                "layout":if combined { "file" } else { "directory" },
            }),
        )?;
        let mut summary = BackupDatabasesSummary {
            requested: schemas.len(),
            manifest: pathname.display().to_string(),
            ..BackupDatabasesSummary::default()
        };
        let mut files = Vec::with_capacity(schemas.len());
        let publish_on_schema_failure =
            self.continue_on_error && (self.options.append || selected_directory);
        let mut publish_staging = true;
        for (index, schema) in schemas.iter().enumerate() {
            let name = schema.name();
            let filename = file_name(index, name, &timestamp_prefix, generation_suffix);
            let path = work.join(&filename);
            let result = check_cancelled(cancel).and_then(|()| {
                if !visible.contains(name) {
                    return Err("当前账户无法列出指定数据库。请检查库名和访问权限。".into());
                }
                schema
                    .backup(filename.clone(), self.options.clone())
                    .backup(worker, &path, timestamp, cancel)
            });
            let event = match result {
                Ok(dump) => {
                    let offset = summary.bytes;
                    let partial = partial_backup_message(&dump, staging.is_none());
                    if let Some(error) = &partial {
                        summary.failed += 1;
                        summary
                            .failure
                            .get_or_insert_with(|| format!("{name}：{error}"));
                    } else {
                        summary.completed += 1;
                    }
                    summary.rows += dump.rows;
                    summary.objects += dump.objects;
                    summary.bytes += dump.bytes;
                    files.push(filename.clone());
                    let mut event = if combined {
                        json!({"event":"staged","schema":name,"offset":offset,
                            "objects":dump.objects,"rows":dump.rows,"bytes":dump.bytes})
                    } else {
                        summary.published = !selected_directory;
                        json!({"event":if selected_directory { "staged" } else { "succeeded" },"schema":name,"file":filename,
                            "objects":dump.objects,"rows":dump.rows,"bytes":dump.bytes})
                    };
                    if let Some(error) = partial {
                        event["event"] = json!("partial");
                        event["error"] = json!(error);
                        event["skipped_objects"] = json!(dump.skipped_objects);
                        summary.skipped_objects.extend(dump.skipped_objects);
                    }
                    event
                }
                Err(error) if error == BACKUP_CANCELLED => {
                    summary.cancelled = true;
                    summary.failure = Some(format!("{name}：{error}"));
                    json!({"event":"cancelled","schema":name,"error":error})
                }
                Err(error) => {
                    publish_staging &= publish_on_schema_failure;
                    let error = redact(&error, password).into_owned();
                    summary.failed += 1;
                    summary
                        .failure
                        .get_or_insert_with(|| format!("{name}：{error}"));
                    json!({"event":"failed","schema":name,"error":error})
                }
            };
            if let Err(error) = write_log(&mut manifest, &event) {
                publish_staging = false;
                summary.cancelled = false;
                summary.failure = Some(format!("无法记录多库备份清单。{error}"));
                break;
            }
            if summary.cancelled || (!self.continue_on_error && summary.failure.is_some()) {
                break;
            }
        }
        if !files.is_empty() {
            let publish = if combined && !summary.cancelled && publish_staging {
                combine(work, &files, output, self.options.append, cancel).map(|offset| {
                    summary.output_offset = self.options.append.then_some(offset);
                })
            } else if selected_directory && !summary.cancelled && publish_staging {
                publish_split_directory(
                    staging.unwrap(),
                    output,
                    self.options.directory_mode,
                    existing,
                    &files,
                    || check_cancelled(cancel),
                )
                .map(|warning| summary.cleanup_warning = warning)
            } else if staging.is_none() {
                write_restore_order(output, &files)
            } else {
                Ok(())
            };
            match publish {
                Ok(()) if publish_staging && !summary.cancelled => summary.published = true,
                Ok(()) => (),
                Err(error) => {
                    summary.cancelled = error == BACKUP_CANCELLED;
                    summary.failure = Some(error);
                }
            }
        }
        let completion = match (&summary.failure, summary.cancelled) {
            (_, true) => "cancelled",
            (Some(_), false) => "failed",
            (None, false) => "succeeded",
        };
        let mut record = summary.fields();
        record.insert("event".into(), json!(completion));
        record.insert("output".into(), json!(output.display().to_string()));
        if let Some(error) = summary.failure.take() {
            record.insert("error".into(), json!(error));
            summary.failure = Some(format!("{}{}", summary.description(), error));
        }
        if let Err(error) = write_log(&mut manifest, &Value::Object(record)) {
            summary.cancelled = false;
            let failure = summary.failure.get_or_insert_with(String::new);
            failure.push_str(&format!("无法完成多库备份清单。{error}"));
        }
        Ok(summary)
    }
}

fn mysql_schema(name: &str) -> bool {
    name.eq_ignore_ascii_case("mysql")
}

fn excluded_system_schema(name: &str) -> bool {
    ["information_schema", "performance_schema", "sys", "ndbinfo"]
        .iter()
        .any(|system| name.eq_ignore_ascii_case(system))
}

fn file_name(index: usize, name: &str, timestamp_prefix: &str, generation_suffix: &str) -> String {
    let label: String = name
        .chars()
        .take(32)
        .map(|ch| {
            if ch.is_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "{timestamp_prefix}{:04}-{label}{generation_suffix}.sql",
        index + 1
    )
}

fn check_cancelled(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        Err(BACKUP_CANCELLED.into())
    } else {
        Ok(())
    }
}

fn combine(
    directory: &Path,
    files: &[String],
    output: &Path,
    append: bool,
    cancel: &AtomicBool,
) -> Result<u64, String> {
    fn error(error: std::io::Error) -> String {
        format!("无法保存合并的多库 SQL：{error}")
    }
    let check = || check_cancelled(cancel);
    check()?;
    let parent = output.parent().ok_or("无法定位多库 SQL 输出目录。")?;
    let mut sql_file = NamedTempFile::new_in(parent).map_err(error)?;
    for name in files {
        let mut file = File::open(directory.join(name)).map_err(error)?;
        copy_output(&mut file, &mut sql_file, &check)?;
    }
    let mode = if append {
        ExistingOutput::Append
    } else {
        ExistingOutput::Reject
    };
    publish_output(sql_file, output, mode, check)
}
