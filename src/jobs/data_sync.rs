use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::database::{
    Command, DbObject, ObjectKind, PreparedSyncGroup, Response, SyncConflict, SyncFilter,
    SyncFilterScope, SyncMapping, SyncMode, SyncPlan, SyncPolicy, SyncRowError, SyncScriptFile,
    SyncScriptPaths, SyncScriptPlan, SyncSide, SyncWrites, Worker,
};

use super::{JobConnection, redact_values, request, request_cancellable, validate_identifier};

pub(super) const CANCELLED: &str = "已取消数据同步。";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DataSyncJob {
    pub source_schema: String,
    pub target_schema: String,
    pub target: JobConnection,
    #[serde(default)]
    pub all_tables: bool,
    #[serde(default)]
    pub tables: Vec<TablePair>,
    #[serde(default)]
    pub mode: Mode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict: Option<Conflict>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub skip_target_fk_checks: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub continue_on_row_error: bool,
    #[serde(default)]
    pub apply: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_directory: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    legacy: Option<LegacySettings>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LegacySettings {
    abort_on_error: bool,
    fkcheck: bool,
    source_transport: String,
    target_transport: String,
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
pub enum TablePair {
    SameName(String),
    Mapped(TableMapping),
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TableMapping {
    pub source: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compare_columns: Vec<String>,
    #[serde(rename = "where", skip_serializing_if = "Option::is_none")]
    pub predicate: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter_scope: Option<FilterScope>,
}

#[derive(Clone, Copy, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterScope {
    #[default]
    Source,
    Both,
}

impl TableMapping {
    fn filter(&self) -> Result<Option<SyncFilter>, String> {
        if self.predicate.is_none() && self.filter_scope.is_some() {
            return Err("filter_scope 需要同时指定 where 条件。".into());
        }
        let scope = match self.filter_scope.unwrap_or_default() {
            FilterScope::Source => SyncFilterScope::Source,
            FilterScope::Both => SyncFilterScope::Both,
        };
        self.predicate
            .as_deref()
            .map(|text| SyncFilter::new(text, scope))
            .transpose()
    }
}

impl TablePair {
    pub fn names(&self) -> (&str, &str) {
        match self {
            Self::SameName(name) => (name, name),
            Self::Mapped(pair) => (&pair.source, &pair.target),
        }
    }
}

#[derive(Clone, Copy, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    InsertMissing,
    InsertAndUpdate,
    MirrorReceiver,
    ReconcileBoth,
}

#[derive(Clone, Copy, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Conflict {
    #[default]
    KeepSeparate,
    SourceWins,
    TargetWins,
}

impl From<Conflict> for SyncConflict {
    fn from(conflict: Conflict) -> Self {
        match conflict {
            Conflict::KeepSeparate => Self::KeepSeparate,
            Conflict::SourceWins => Self::SourceWins,
            Conflict::TargetWins => Self::TargetWins,
        }
    }
}

#[derive(Clone, Copy, Default, Serialize)]
struct Writes {
    inserted: u64,
    updated: u64,
    deleted: u64,
}

impl From<SyncWrites> for Writes {
    fn from(writes: SyncWrites) -> Self {
        Self {
            inserted: writes.inserted,
            updated: writes.updated,
            deleted: writes.deleted,
        }
    }
}

impl Writes {
    fn has_changes(&self) -> bool {
        self.inserted != 0 || self.updated != 0 || self.deleted != 0
    }

    fn add(&mut self, writes: Self) {
        self.inserted += writes.inserted;
        self.updated += writes.updated;
        self.deleted += writes.deleted;
    }

    fn description(&self, endpoint: &str) -> String {
        format!(
            "{endpoint}新增 {} 行、更新 {} 行、删除 {} 行。",
            self.inserted, self.updated, self.deleted
        )
    }
}

#[derive(Default, Serialize)]
struct Counts {
    same: u64,
    source_only: u64,
    target_only: u64,
    conflicts: u64,
}

#[derive(Serialize)]
struct TableSummary {
    source: String,
    target: String,
    whole_row_match: bool,
    #[serde(flatten)]
    counts: Counts,
    planned_source: Writes,
    planned_target: Writes,
}

#[derive(Default, Serialize)]
struct UnmatchedTables {
    source_only: Vec<String>,
    target_only: Vec<String>,
}

#[derive(Serialize)]
pub(super) struct SyncSummary {
    source_schema: String,
    target_schema: String,
    mode: Mode,
    #[serde(skip_serializing_if = "Option::is_none")]
    conflict: Option<Conflict>,
    #[serde(skip_serializing_if = "is_false")]
    skip_target_fk_checks: bool,
    #[serde(skip_serializing_if = "is_false")]
    continue_on_row_error: bool,
    tables: Vec<TableSummary>,
    unmatched_tables: UnmatchedTables,
    #[serde(skip_serializing_if = "Option::is_none")]
    legacy: Option<LegacySettings>,
    #[serde(flatten)]
    counts: Counts,
    apply: bool,
    completed: usize,
    planned_source: Writes,
    planned_target: Writes,
    source_writes: Writes,
    target_writes: Writes,
    skipped_rows: u64,
    #[serde(skip)]
    row_errors: Vec<SyncRowError>,
    #[serde(skip)]
    script_files: Option<Vec<SyncScriptFile>>,
    stopped: Option<String>,
    uncertain: Option<&'static str>,
    #[serde(skip)]
    pub(super) failure: Option<String>,
    #[serde(skip)]
    pub(super) cancelled: bool,
}

impl SyncSummary {
    pub(super) fn fields(&self) -> Map<String, Value> {
        let Value::Object(mut fields) = json!(self) else {
            unreachable!("sync summary is a struct")
        };
        // Keep the earlier one-way job totals as aliases of confirmed target writes.
        fields.insert("inserted".into(), json!(self.target_writes.inserted));
        fields.insert("updated".into(), json!(self.target_writes.updated));
        if !self.row_errors.is_empty() {
            let errors = self
                .row_errors
                .iter()
                .map(|error| {
                    json!({
                        "side": side_name(error.side),
                        "schema": error.table.schema,
                        "table": error.table.name,
                        "operation": error.operation,
                        "code": error.code,
                    })
                })
                .collect::<Vec<_>>();
            fields.insert("row_errors".into(), Value::Array(errors));
        }
        if let Some(files) = &self.script_files {
            let files = files
                .iter()
                .map(|file| json!({"side": side_name(file.side), "path": file.path.to_string_lossy()}))
                .collect::<Vec<_>>();
            fields.insert("script_files".into(), Value::Array(files));
        }
        fields
    }

    pub(super) fn description(&self) -> String {
        let mut message = if self.apply {
            format!(
                "已处理 {} 张表。{}{}",
                self.completed,
                self.source_writes.description("源端确认"),
                self.target_writes.description("目标端确认"),
            )
        } else {
            format!(
                "已比较 {} 张表。相同 {} 行，源端独有 {} 行，目标端独有 {} 行，冲突 {} 行。{}{}",
                self.tables.len(),
                self.counts.same,
                self.counts.source_only,
                self.counts.target_only,
                self.counts.conflicts,
                self.planned_source.description("源端预计"),
                self.planned_target.description("目标端预计"),
            )
        };
        if !self.unmatched_tables.source_only.is_empty()
            || !self.unmatched_tables.target_only.is_empty()
        {
            message.push_str(&format!(
                "源端 {} 张表、目标端 {} 张表没有同名对应。",
                self.unmatched_tables.source_only.len(),
                self.unmatched_tables.target_only.len()
            ));
        }
        if self.tables.iter().any(|table| table.whole_row_match) {
            message.push_str("整行匹配的表按不同的行值计数。");
        }
        if self.skip_target_fk_checks {
            message.push_str("本次作业配置为在写入目标端时暂停外键检查。");
        }
        if self.skipped_rows != 0 {
            message.push_str(&format!("跳过 {} 行。", self.skipped_rows));
            for error in self.row_errors.iter().take(3) {
                let endpoint = match error.side {
                    SyncSide::Source => "源端",
                    SyncSide::Target => "目标端",
                };
                let operation = match error.operation {
                    "insert" => "新增",
                    "update" => "更新",
                    "delete" => "删除",
                    _ => "写入",
                };
                message.push_str(&format!(
                    "{endpoint} {}.{} 的{operation}失败，错误编号 {}。",
                    error.table.schema, error.table.name, error.code,
                ));
            }
        }
        if let Some(files) = &self.script_files {
            for file in files {
                let (endpoint, written) = match file.side {
                    SyncSide::Source => ("源端", &self.planned_source),
                    SyncSide::Target => ("目标端", &self.planned_target),
                };
                message.push_str(&format!(
                    "已保存{endpoint}同步 SQL 到 {}。",
                    file.path.display()
                ));
                if self.apply {
                    message.push_str(&written.description(&format!("{endpoint}脚本预计")));
                }
            }
            if !self.tables.is_empty()
                && !self.planned_source.has_changes()
                && !self.planned_target.has_changes()
            {
                message.push_str("没有需要写入的行，无需生成同步 SQL。");
            }
        }
        message
    }

    fn stop(
        &mut self,
        step: &str,
        error: &str,
        cancel: &AtomicBool,
        sanitize: &impl Fn(&str) -> String,
    ) {
        self.cancelled = self.uncertain.is_none() && was_cancelled(cancel, error);
        self.stopped = Some(sanitize(error));
        self.failure = Some(sanitize(&format!("{}{step}{error}", self.description())));
    }
}

impl DataSyncJob {
    pub(super) fn new(source_schema: String) -> Self {
        Self {
            source_schema,
            target_schema: String::new(),
            target: JobConnection::default(),
            all_tables: true,
            tables: Vec::new(),
            mode: Mode::default(),
            conflict: None,
            skip_target_fk_checks: false,
            continue_on_row_error: false,
            apply: false,
            script_directory: None,
            legacy: None,
        }
    }

    pub(super) fn validate(&self) -> Result<(), String> {
        self.target.validate()?;
        validate_identifier(&self.source_schema, "源数据库名称")?;
        validate_identifier(&self.target_schema, "目标数据库名称")?;
        if self.conflict.is_some() && !matches!(self.mode, Mode::ReconcileBoth) {
            return Err("conflict 选项用于 reconcile_both 同步模式。".into());
        }
        if self.all_tables != self.tables.is_empty() {
            return Err("请设置 all_tables 或为数据同步作业选择至少一张表。".into());
        }
        if self
            .script_directory
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            return Err("请选择同步 SQL 的保存目录。".into());
        }
        let mut sources = HashSet::new();
        let mut targets = HashSet::new();
        for pair in &self.tables {
            let (source, target) = pair.names();
            validate_identifier(source, "源表名称")?;
            validate_identifier(target, "目标表名称")?;
            if !sources.insert(source) || !targets.insert(target) {
                return Err("数据同步的源表和目标表各自只能映射一次。".into());
            }
            if let TablePair::Mapped(mapping) = pair {
                let filter = mapping.filter()?;
                if matches!(self.mode, Mode::ReconcileBoth)
                    && filter
                        .as_ref()
                        .is_some_and(|filter| filter.scope() == SyncFilterScope::Source)
                {
                    return Err("双向同步的 WHERE 条件需要设置 filter_scope 为 both。".into());
                }
                let mut names = HashSet::new();
                for column in &mapping.compare_columns {
                    validate_identifier(column, "比较列名称")?;
                    if !names.insert(column.to_lowercase()) {
                        return Err("同步表的比较列不能重复。".into());
                    }
                }
            }
        }
        Ok(())
    }

    fn engine_mode(&self) -> SyncMode {
        match self.mode {
            Mode::InsertMissing => SyncMode::InsertMissing,
            Mode::InsertAndUpdate => SyncMode::InsertAndUpdate,
            Mode::MirrorReceiver => SyncMode::MirrorReceiver,
            Mode::ReconcileBoth => {
                SyncMode::ReconcileBoth(self.conflict.unwrap_or_default().into())
            }
        }
    }

    fn mappings(
        &self,
        source: &Worker,
        target: &Worker,
    ) -> Result<(Vec<SyncMapping>, UnmatchedTables), String> {
        if self.all_tables {
            fn names(worker: &Worker, schema: &str) -> Result<BTreeSet<String>, String> {
                let Response::Objects { objects, .. } = request(
                    worker,
                    3,
                    Command::ListObjects {
                        schema: schema.into(),
                    },
                )?
                else {
                    return Err("数据库未返回同步表列表。".into());
                };
                Ok(objects
                    .into_iter()
                    .filter(|object| object.kind == ObjectKind::Table)
                    .map(|object| object.name)
                    .collect())
            }
            let source = names(source, &self.source_schema)?;
            let target = names(target, &self.target_schema)?;
            let mappings = source
                .intersection(&target)
                .map(|name| self.mapping(name, name, None, Vec::new()))
                .collect();
            let omitted = UnmatchedTables {
                source_only: source.difference(&target).cloned().collect(),
                target_only: target.difference(&source).cloned().collect(),
            };
            return Ok((mappings, omitted));
        }
        let mappings = self
            .tables
            .iter()
            .map(|pair| {
                let (source, target) = pair.names();
                let (filter, columns) = match pair {
                    TablePair::Mapped(mapping) => {
                        (mapping.filter()?, mapping.compare_columns.clone())
                    }
                    TablePair::SameName(_) => (None, Vec::new()),
                };
                Ok(self.mapping(source, target, filter, columns))
            })
            .collect::<Result<_, String>>()?;
        Ok((mappings, UnmatchedTables::default()))
    }

    fn mapping(
        &self,
        source: &str,
        target: &str,
        filter: Option<SyncFilter>,
        compare_columns: Vec<String>,
    ) -> SyncMapping {
        let table = |schema: &str, name: &str| DbObject {
            schema: schema.into(),
            name: name.into(),
            kind: ObjectKind::Table,
        };
        SyncMapping {
            source: table(&self.source_schema, source),
            target: table(&self.target_schema, target),
            filter,
            compare_columns,
        }
    }

    fn summarize(&self, plans: &[SyncPlan], unmatched_tables: UnmatchedTables) -> SyncSummary {
        let mut counts = Counts::default();
        let mut planned_source = Writes::default();
        let mut planned_target = Writes::default();
        let tables = plans
            .iter()
            .map(|plan| {
                counts.same += plan.counts.same;
                counts.source_only += plan.counts.source_only;
                counts.target_only += plan.counts.target_only;
                counts.conflicts += plan.counts.conflicts;
                let source = Writes::from(plan.planned_source());
                let target = Writes::from(plan.planned_target());
                planned_source.add(source);
                planned_target.add(target);
                TableSummary {
                    source: plan.source.name.clone(),
                    target: plan.target.name.clone(),
                    whole_row_match: plan.matches_whole_row(),
                    counts: Counts {
                        same: plan.counts.same,
                        source_only: plan.counts.source_only,
                        target_only: plan.counts.target_only,
                        conflicts: plan.counts.conflicts,
                    },
                    planned_source: source,
                    planned_target: target,
                }
            })
            .collect();
        SyncSummary {
            source_schema: self.source_schema.clone(),
            target_schema: self.target_schema.clone(),
            mode: self.mode,
            conflict: matches!(self.mode, Mode::ReconcileBoth)
                .then(|| self.conflict.unwrap_or_default()),
            skip_target_fk_checks: self.skip_target_fk_checks,
            continue_on_row_error: self.continue_on_row_error,
            tables,
            unmatched_tables,
            legacy: self.legacy.clone(),
            counts,
            apply: self.apply,
            completed: 0,
            planned_source,
            planned_target,
            source_writes: Writes::default(),
            target_writes: Writes::default(),
            skipped_rows: 0,
            row_errors: Vec::new(),
            script_files: self.script_directory.as_ref().map(|_| Vec::new()),
            stopped: None,
            uncertain: None,
            failure: None,
            cancelled: false,
        }
    }

    pub(super) fn run(
        &self,
        source: &Worker,
        directory: &Path,
        source_secrets: &[&str],
        cancel: &AtomicBool,
    ) -> Result<SyncSummary, String> {
        let connection = self
            .target
            .connection_config(directory, Some(self.target_schema.clone()))?;
        let target_password = connection.password.clone();
        let target_ssh_password = connection
            .ssh
            .as_ref()
            .and_then(|ssh| ssh.password.clone())
            .unwrap_or_default();
        let target_key_passphrase = connection
            .ssh
            .as_ref()
            .and_then(|ssh| ssh.key_passphrase.clone())
            .unwrap_or_default();
        let http = self.target.http_config()?;
        let http_secrets = super::http_passwords(&http)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let secrets = source_secrets
            .iter()
            .copied()
            .chain([
                target_password.as_str(),
                target_ssh_password.as_str(),
                target_key_passphrase.as_str(),
            ])
            .chain(http_secrets.iter().map(String::as_str))
            .collect::<Vec<_>>();
        let sanitize = |error: &str| redact_values(error, &secrets);
        let result = (|| {
            let target = Worker::new();
            let response = request(&target, 1, super::connect_command(connection, http));
            check_cancelled(cancel)?;
            if !matches!(response?, Response::Connected(_)) {
                return Err("数据库连接未返回数据同步的目标连接状态。".into());
            }
            let Response::SyncEndpoint(endpoint) = request(&target, 2, Command::SyncEndpoint)?
            else {
                return Err("数据库连接未返回数据同步的目标端点。".into());
            };
            let (mappings, unmatched) = self.mappings(source, &target)?;
            if mappings.is_empty() {
                let mut summary = self.summarize(&[], unmatched);
                summary.failure = Some("两端没有可同步的同名数据表。".into());
                return Ok(summary);
            }
            let Response::SyncGroupPlan(mut prepared) = request_sync(
                source,
                4,
                Command::PlanScopedSyncGroup {
                    mappings,
                    endpoint,
                    policy: SyncPolicy {
                        mode: self.engine_mode(),
                        skip_target_fk_checks: self.skip_target_fk_checks,
                        continue_on_row_error: self.continue_on_row_error,
                    },
                },
                cancel,
            )?
            else {
                return Err("数据库连接未返回数据同步的比较结果。".into());
            };
            let mut summary = self.summarize(&prepared.plan.tables, unmatched);
            if let Some(script_directory) = &self.script_directory
                && (summary.planned_source.has_changes() || summary.planned_target.has_changes())
            {
                match save_scripts(
                    source,
                    &directory.join(script_directory),
                    prepared,
                    &summary,
                    cancel,
                ) {
                    Ok((reviewed, files)) => {
                        prepared = reviewed;
                        summary.script_files = Some(files);
                    }
                    Err(error) => {
                        summary.stop("保存同步 SQL 未完成。", &error, cancel, &sanitize);
                        return Ok(summary);
                    }
                }
            }
            if self.apply {
                let applied =
                    match request_sync(source, 6, Command::ApplySyncGroup { prepared }, cancel) {
                        Ok(Response::SyncGroupApplied(applied)) => Ok(applied),
                        Ok(_) => Err("数据库连接未返回数据同步的写入结果。".into()),
                        Err(error) => Err(error),
                    };
                let applied = match applied {
                    Ok(applied) => applied,
                    Err(error) if self.script_directory.is_some() => {
                        summary.stop("后续同步停止。", &error, cancel, &sanitize);
                        return Ok(summary);
                    }
                    Err(error) => return Err(error),
                };
                summary.completed = applied.completed;
                summary.source_writes = applied.source.into();
                summary.target_writes = applied.target.into();
                summary.skipped_rows = applied.skipped_rows;
                summary.row_errors = applied.row_errors;
                summary.uncertain = applied.uncertain.map(side_name);
                if let Some(error) = applied.stopped {
                    summary.stop("后续同步停止。", &error, cancel, &sanitize);
                } else if summary.skipped_rows != 0 {
                    summary.failure = Some(sanitize(&summary.description()));
                }
            }
            Ok(summary)
        })();
        result.map_err(|error: String| {
            if was_cancelled(cancel, &error) {
                CANCELLED.into()
            } else {
                sanitize(&error)
            }
        })
    }
}

fn save_scripts(
    worker: &Worker,
    directory: &Path,
    prepared: Box<PreparedSyncGroup>,
    summary: &SyncSummary,
    cancel: &AtomicBool,
) -> Result<(Box<PreparedSyncGroup>, Vec<SyncScriptFile>), String> {
    check_cancelled(cancel)?;
    fs::create_dir_all(directory).map_err(|error| {
        format!(
            "无法创建同步 SQL 的保存目录 {}。{error}",
            directory.display()
        )
    })?;
    let run_directory = tempfile::Builder::new()
        .prefix("同步-")
        .tempdir_in(directory)
        .map_err(|error| format!("无法在 {} 创建本次同步的目录。{error}", directory.display()))?;
    let paths = SyncScriptPaths {
        source: summary
            .planned_source
            .has_changes()
            .then(|| run_directory.path().join("源端.sql")),
        target: summary
            .planned_target
            .has_changes()
            .then(|| run_directory.path().join("目标端.sql")),
        replace: false,
    };
    let Response::SyncScripts {
        prepared: SyncScriptPlan::Group(prepared),
        files,
    } = request_sync(
        worker,
        5,
        Command::ExportSync {
            prepared: SyncScriptPlan::Group(prepared),
            paths,
        },
        cancel,
    )?
    else {
        return Err("数据库连接未返回同步 SQL 的保存结果。".into());
    };
    let _ = run_directory.keep();
    Ok((prepared, files))
}

fn side_name(side: SyncSide) -> &'static str {
    match side {
        SyncSide::Source => "source",
        SyncSide::Target => "target",
    }
}

fn check_cancelled(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        Err(CANCELLED.into())
    } else {
        Ok(())
    }
}

fn was_cancelled(cancel: &AtomicBool, error: &str) -> bool {
    cancel.load(Ordering::Acquire) && error.ends_with(CANCELLED)
}

fn request_sync(
    worker: &Worker,
    id: u64,
    command: Command,
    cancel: &AtomicBool,
) -> Result<Response, String> {
    check_cancelled(cancel)?;
    request_cancellable(worker, id, command, cancel, Worker::cancel_sync)
}
