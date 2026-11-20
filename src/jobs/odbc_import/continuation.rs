use std::collections::HashMap;

use crate::database::IMPORT_UNCONFIRMED;
use crate::jobs::redact_values;

use super::target_strategy::TargetDecision;
use super::*;

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    NotRun,
    Completed,
    Skipped,
    Failed,
    DependencyFailed,
    SourcePending,
    Cancelled,
    Unconfirmed,
}

#[derive(Serialize)]
pub(super) struct TableResult {
    table: String,
    status: Status,
    target_published: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

impl TableResult {
    fn new(table: &str) -> Self {
        Self {
            table: table.into(),
            status: Status::NotRun,
            target_published: Some(false),
            detail: None,
        }
    }

    fn record(&mut self, status: Status, error: &str, secrets: &[&str]) {
        self.status = status;
        self.detail = Some(redact_values(error, secrets));
    }

    pub(super) fn completed(&self) -> bool {
        matches!(self.status, Status::Completed | Status::Skipped)
    }

    pub(super) fn publish_unknown(&self) -> bool {
        self.target_published.is_none()
    }

    pub(super) fn description(&self) -> String {
        let label = match (self.status, self.target_published) {
            (Status::NotRun, _) => "未执行",
            (Status::Completed, _) => "已完成",
            (Status::Skipped, _) => "按目标表策略跳过",
            (Status::Failed, Some(true)) => "目标已提交，源表操作失败",
            (Status::Failed, _) => "失败，目标未发布",
            (Status::DependencyFailed, _) => "外键依赖未就绪",
            (Status::SourcePending, _) => "目标已提交，源表操作未执行",
            (Status::Cancelled, Some(true)) => "目标已提交，源表操作已停止",
            (Status::Cancelled, _) => "已停止，目标未发布",
            (Status::Unconfirmed, Some(true)) => "目标已提交，源表结果需核对",
            (Status::Unconfirmed, _) => "导入结果需核对",
        };
        format!("{}：{label}", self.table)
    }

    pub(super) fn detail(&self) -> Option<String> {
        self.detail
            .as_ref()
            .map(|detail| format!("{}：{detail}", self.table))
    }
}

pub(super) fn run(
    job: &OdbcJob,
    worker: &Worker,
    directory: &Path,
    specs: &[TableSpec<'_>],
    presence: Option<&[Option<String>]>,
    secrets: &[&str],
    cancel: &Arc<AtomicBool>,
) -> OdbcSummary {
    let mut summary = OdbcSummary {
        table_results: specs
            .iter()
            .map(|spec| TableResult::new(spec.name))
            .collect(),
        ..OdbcSummary::default()
    };
    let mut prepared = Vec::with_capacity(specs.len());
    let mut remaining = IMPORT_MEMORY_LIMIT;
    for (index, spec) in specs.iter().enumerate() {
        let mut available = remaining;
        let plan = spec.resolve_target(job, worker, presence.map(|tables| &tables[index]), cancel);
        let table = plan.and_then(|plan| match plan {
            TargetDecision::Import(plan) => spec
                .prepare(job, plan, directory, &mut available, cancel)
                .map(Some),
            TargetDecision::Skip(reason) => {
                summary.table_results[index].record(Status::Skipped, reason, secrets);
                Ok(None)
            }
        });
        match table {
            Ok(table) => {
                remaining = available;
                prepared.push(table);
            }
            Err(error) => {
                let stopped = error == IMPORT_CANCELLED;
                let status = if stopped {
                    Status::Cancelled
                } else {
                    Status::Failed
                };
                summary.table_results[index].record(status, &error, secrets);
                if stopped || !job.continue_on_error {
                    summary.cancelled = stopped;
                    return summary;
                }
                prepared.push(None);
            }
        }
    }
    if let Err(error) =
        resolve_links(specs, &mut prepared, &mut summary, secrets, cancel).and_then(|()| {
            verify_source_destinations(worker, &job.schema, specs, prepared.iter().flatten())
        })
    {
        summary.cancelled = error == IMPORT_CANCELLED;
        summary.halt_reason = Some(redact_values(&error, secrets));
        return summary;
    }
    if !job.continue_on_error
        && summary
            .table_results
            .iter()
            .any(|result| result.status == Status::DependencyFailed)
    {
        return summary;
    }

    let groups = if job.continue_on_error {
        groups(&prepared)
    } else {
        publication_groups(&prepared)
    };
    summary.separate_publications = !job.continue_on_error && groups.len() > 1;
    let mut pending_source_actions = Vec::new();

    for indexes in groups {
        if check_import_cancelled(Some(cancel)).is_err() {
            summary.cancelled = true;
            break;
        }
        let mut columns = 0;
        let (targets, plans): (Vec<_>, Vec<_>) = indexes
            .iter()
            .map(|&index| {
                let table = prepared[index].take().expect("prepared import group");
                columns += table.target.columns.len();
                (table.target, table.source_action)
            })
            .unzip();
        let mut counts = match import_group(worker, targets, cancel) {
            Ok(counts) => counts,
            Err(error) => {
                let status = error_status(&error);
                for &index in &indexes {
                    let result = &mut summary.table_results[index];
                    if status == Status::Unconfirmed {
                        result.target_published = None;
                    }
                    result.record(status, &error, secrets);
                }
                if status == Status::Failed && job.continue_on_error {
                    continue;
                }
                summary.cancelled = status == Status::Cancelled;
                break;
            }
        };
        summary.tables += indexes.len();
        summary.columns += columns;
        summary.counts.rows += counts.rows;
        summary.counts.skipped += counts.skipped;
        summary.counts.replaced += counts.replaced;
        summary.counts.warnings.extend(
            counts
                .warnings
                .drain(..)
                .map(|warning| redact_values(&warning, secrets)),
        );
        for (&index, plan) in indexes.iter().zip(&plans) {
            let result = &mut summary.table_results[index];
            result.target_published = Some(true);
            result.status = if plan.is_some() {
                Status::SourcePending
            } else {
                Status::Completed
            };
        }
        let confirmed = match take_confirmed_source_rows(&plans, &mut counts) {
            Ok(confirmed) => confirmed,
            Err(error) => {
                summary.halt_reason = Some(redact_values(&error, secrets));
                break;
            }
        };
        if !confirmed.is_empty() {
            summary.source_rows.get_or_insert(0);
        }
        let mut group_actions = Vec::new();
        for ((index, plan), rows) in indexes.into_iter().zip(plans).zip(confirmed) {
            let (Some(plan), Some(rows)) = (plan, rows) else {
                continue;
            };
            group_actions.push((index, plan, rows));
        }
        if job.continue_on_error {
            if !complete_source_actions(
                specs,
                group_actions,
                directory,
                &mut summary,
                secrets,
                cancel,
                true,
            ) {
                break;
            }
        } else {
            pending_source_actions.extend(group_actions);
        }
    }
    if !job.continue_on_error
        && !summary.cancelled
        && summary.halt_reason.is_none()
        && summary
            .table_results
            .iter()
            .all(|result| result.completed() || result.status == Status::SourcePending)
    {
        // Follow the stop-on-error contract for source writes after all target groups confirm publication.
        pending_source_actions.sort_by_key(|(index, _, _)| *index);
        complete_source_actions(
            specs,
            pending_source_actions,
            directory,
            &mut summary,
            secrets,
            cancel,
            false,
        );
    }
    summary
}

fn complete_source_actions(
    specs: &[TableSpec<'_>],
    actions: Vec<(usize, SourceActionPlan, Vec<bool>)>,
    directory: &Path,
    summary: &mut OdbcSummary,
    secrets: &[&str],
    cancel: &AtomicBool,
    continue_on_error: bool,
) -> bool {
    for (index, plan, rows) in actions {
        let result = &mut summary.table_results[index];
        match execute_source_plan(&specs[index], plan, &rows, directory, cancel) {
            Ok(rows) => {
                *summary.source_rows.get_or_insert(0) += rows;
                result.status = Status::Completed;
            }
            Err(error) => {
                let status = error_status(&error);
                result.record(status, &error, secrets);
                if status != Status::Failed || !continue_on_error {
                    summary.cancelled = status == Status::Cancelled;
                    return false;
                }
            }
        }
    }
    true
}

fn error_status(error: &str) -> Status {
    if error == IMPORT_CANCELLED {
        Status::Cancelled
    } else if error.starts_with(IMPORT_UNCONFIRMED) {
        Status::Unconfirmed
    } else {
        Status::Failed
    }
}

fn import_group(
    worker: &Worker,
    tables: Vec<OdbcTarget>,
    cancel: &Arc<AtomicBool>,
) -> Result<OdbcImportCounts, String> {
    let event = worker
        .send(
            2,
            Command::ImportOdbcTables {
                tables,
                cancel: Arc::clone(cancel),
            },
        )
        .and_then(|()| worker.recv())
        .map_err(|error| format!("{IMPORT_UNCONFIRMED}{error}"))?;
    if event.id == 2
        && let Response::OdbcImported { counts, .. } = event.result?
    {
        return Ok(counts);
    }
    Err(format!(
        "{IMPORT_UNCONFIRMED}数据库连接未返回当前表组的导入结果。"
    ))
}

fn resolve_links(
    specs: &[TableSpec<'_>],
    prepared: &mut [Option<PreparedTable>],
    summary: &mut OdbcSummary,
    secrets: &[&str],
    cancel: &AtomicBool,
) -> Result<(), String> {
    for index in 0..prepared.len() {
        let Some(child) = prepared[index].as_ref() else {
            continue;
        };
        let candidates = prepared
            .iter()
            .zip(specs)
            .filter_map(|(table, spec)| table.as_ref().map(|table| (table, spec)));
        match mapped_foreign_keys(&specs[index], child, candidates, cancel) {
            Ok(keys) => prepared[index].as_mut().unwrap().target.foreign_keys = keys,
            Err(error) if error == IMPORT_CANCELLED => return Err(error),
            Err(error) => {
                summary.table_results[index].record(Status::DependencyFailed, &error, secrets);
                prepared[index] = None;
            }
        }
    }
    loop {
        check_import_cancelled(Some(cancel))?;
        let present = target_positions(prepared);
        let missing = prepared
            .iter()
            .enumerate()
            .filter_map(|(index, table)| {
                table
                    .as_ref()?
                    .target
                    .foreign_keys
                    .iter()
                    .find(|key| !present.contains_key(key.referenced_table.as_str()))
                    .map(|key| (index, key.referenced_table.clone()))
            })
            .collect::<Vec<_>>();
        if missing.is_empty() {
            return Ok(());
        }
        for (index, parent) in missing {
            summary.table_results[index].record(
                Status::DependencyFailed,
                &format!("被引用表 {parent} 未能准备导入。"),
                secrets,
            );
            prepared[index] = None;
        }
    }
}

fn target_positions(prepared: &[Option<PreparedTable>]) -> HashMap<&str, usize> {
    prepared
        .iter()
        .enumerate()
        .filter_map(|(index, table)| {
            table
                .as_ref()
                .map(|table| (table.target.object.name.as_str(), index))
        })
        .collect()
}

fn groups(prepared: &[Option<PreparedTable>]) -> Vec<Vec<usize>> {
    let positions = target_positions(prepared);
    let mut owners = (0..prepared.len()).collect::<Vec<_>>();
    for (index, table) in prepared.iter().enumerate() {
        let Some(table) = table else { continue };
        for key in &table.target.foreign_keys {
            let parent = positions[key.referenced_table.as_str()];
            let current = owners[index];
            let other = owners[parent];
            if current != other {
                let first = current.min(other);
                let last = current.max(other);
                for owner in &mut owners {
                    if *owner == last {
                        *owner = first;
                    }
                }
            }
        }
    }
    let mut groups = vec![Vec::new(); prepared.len()];
    for (index, owner) in owners.into_iter().enumerate() {
        if prepared[index].is_some() {
            groups[owner].push(index);
        }
    }
    groups
        .into_iter()
        .filter(|group| !group.is_empty())
        .collect()
}

fn publication_groups(prepared: &[Option<PreparedTable>]) -> Vec<Vec<usize>> {
    let mut staged = Vec::new();
    let mut existing = Vec::new();
    for (index, table) in prepared.iter().enumerate() {
        if let Some(table) = table {
            if table.target.create.is_some() || table.target.reset != OdbcReset::None {
                staged.push(index);
            } else {
                existing.push(index);
            }
        }
    }
    [staged, existing]
        .into_iter()
        .filter(|group| !group.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::OdbcReferentialAction;

    #[test]
    fn keeps_copied_foreign_key_groups_atomic_and_imports_independent_tables_separately() {
        let table = |name: &str, parent: Option<&str>| {
            Some(PreparedTable {
                target: OdbcTarget {
                    object: DbObject {
                        schema: "fixture".into(),
                        name: name.into(),
                        kind: ObjectKind::Table,
                    },
                    columns: vec!["id".into()],
                    rows: Vec::new(),
                    create: Some(vec![OdbcColumnDefinition::default()]),
                    create_sql: None,
                    engine: None,
                    indexes: Vec::new(),
                    foreign_keys: parent
                        .map(|parent| OdbcForeignKey {
                            name: None,
                            columns: vec!["id".into()],
                            referenced_table: parent.into(),
                            referenced_columns: vec!["id".into()],
                            on_update: OdbcReferentialAction::NoAction,
                            on_delete: OdbcReferentialAction::NoAction,
                        })
                        .into_iter()
                        .collect(),
                    track_applied_rows: false,
                    conflict: OdbcConflict::Abort,
                    reset: OdbcReset::None,
                    expected_ddl: None,
                },
                source_table: None,
                access_file: None,
                foreign_keys: Vec::new(),
                mapping: Vec::new(),
                source_action: None,
            })
        };
        let mut prepared = [
            table("line", Some("invoice")),
            table("warehouse", None),
            table("customer", None),
            table("invoice", Some("customer")),
        ];
        assert_eq!(groups(&prepared), vec![vec![0, 2, 3], vec![1]]);
        prepared[1].as_mut().unwrap().target.create = None;
        assert_eq!(publication_groups(&prepared), vec![vec![0, 2, 3], vec![1]]);
    }
}
