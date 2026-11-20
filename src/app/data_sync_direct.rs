use eframe::egui::{self, Button, RichText};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::database::{
    Command, DbObject, PreparedSync, PreparedSyncGroup, SyncConflict, SyncDifferenceKind,
    SyncFilter, SyncFilterScope, SyncGroupResult, SyncMapping, SyncMode, SyncPolicy, SyncResult,
    SyncRowError, SyncScriptFile, SyncScriptPaths, SyncScriptPlan, SyncSide, SyncWrites,
};

use super::Endpoint;
use crate::app::{
    state::{SyncReply, Workspace, sync_side_label, sync_write_summary},
    style,
};

enum Phase {
    Idle,
    Acquiring,
    Comparing,
    Planned(Box<PreparedSync>),
    PlannedGroup(Box<PreparedSyncGroup>),
    Exporting { apply_after: bool },
    Applying,
    Finished(SyncResult),
    FinishedGroup(SyncGroupResult),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Output {
    Execute,
    Sql,
    SqlAndExecute,
}

impl Output {
    fn label(self) -> &'static str {
        match self {
            Self::Execute => "执行同步",
            Self::Sql => "保存同步 SQL",
            Self::SqlAndExecute => "保存 SQL 并同步",
        }
    }
}

pub(super) struct Direct {
    phase: Phase,
    reverse: bool,
    mode: SyncMode,
    skip_target_fk_checks: bool,
    continue_on_row_error: bool,
    output: Output,
    files: Vec<SyncScriptFile>,
    allow_delete: bool,
    cancelling: bool,
    error: Option<String>,
    fields: BTreeMap<String, ScopeFields>,
    edited_table: String,
}

#[derive(Clone, Default, PartialEq, Eq)]
struct ScopeFields {
    predicate: String,
    comparison: String,
    both: bool,
}

impl Default for Direct {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            reverse: false,
            mode: SyncMode::InsertMissing,
            skip_target_fk_checks: false,
            continue_on_row_error: false,
            output: Output::Execute,
            files: Vec::new(),
            allow_delete: false,
            cancelling: false,
            error: None,
            fields: BTreeMap::new(),
            edited_table: String::new(),
        }
    }
}

impl Direct {
    pub(super) fn active(&self) -> bool {
        matches!(
            self.phase,
            Phase::Acquiring | Phase::Comparing | Phase::Exporting { .. } | Phase::Applying
        )
    }

    pub(super) fn clear(&mut self) {
        self.phase = Phase::Idle;
        self.error = None;
        self.allow_delete = false;
        self.files.clear();
    }

    pub(super) fn reset_endpoints(&mut self) {
        self.clear();
        self.skip_target_fk_checks = false;
        self.fields.clear();
        self.edited_table.clear();
    }

    fn mapping(
        &self,
        name: &str,
        source: DbObject,
        target: DbObject,
    ) -> Result<SyncMapping, String> {
        let mut mapping = SyncMapping {
            source,
            target,
            filter: None,
            compare_columns: Vec::new(),
        };
        let Some(fields) = self.fields.get(name) else {
            return Ok(mapping);
        };
        let where_sql = fields.predicate.trim();
        if !where_sql.is_empty() {
            let scope = if fields.both {
                SyncFilterScope::Both
            } else {
                SyncFilterScope::Source
            };
            mapping.filter = Some(SyncFilter::new(where_sql, scope)?);
        }
        mapping.compare_columns = fields
            .comparison
            .lines()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect();
        Ok(mapping)
    }

    fn scope_controls(&mut self, ui: &mut egui::Ui, tables: Option<&BTreeSet<String>>) {
        let name = if let Some(tables) = tables {
            let Some(first) = tables.first() else { return };
            if !tables.contains(&self.edited_table) {
                self.edited_table.clone_from(first);
            }
            egui::ComboBox::from_id_salt("sync-scope-table")
                .selected_text(&self.edited_table)
                .show_ui(ui, |ui| {
                    for table in tables {
                        ui.selectable_value(&mut self.edited_table, table.clone(), table);
                    }
                });
            self.edited_table.clone()
        } else {
            String::new()
        };
        let fields = self.fields.entry(name).or_default();
        let before = fields.clone();
        ui.label("行筛选条件");
        ui.add(
            egui::TextEdit::multiline(&mut fields.predicate)
                .desired_width(420.0)
                .desired_rows(2)
                .hint_text("填写 WHERE 后的条件，留空使用整表"),
        );
        ui.checkbox(&mut fields.both, "两端都使用此条件");
        if !fields.both && !fields.predicate.trim().is_empty() {
            ui.label(RichText::new("只筛选发送端时，保留接收端其他行。").color(style::muted()));
        }
        ui.label("用于判断记录是否有差异的字段");
        ui.add(
            egui::TextEdit::multiline(&mut fields.comparison)
                .desired_width(420.0)
                .desired_rows(2)
                .hint_text("每行一个字段名，留空比较全部字段"),
        );
        if before != *fields {
            self.clear();
        }
    }

    fn ends<'a>(&self, left: &'a Endpoint, right: &'a Endpoint) -> (&'a Endpoint, &'a Endpoint) {
        if self.reverse && !matches!(self.mode, SyncMode::ReconcileBoth(_)) {
            (right, left)
        } else {
            (left, right)
        }
    }

    fn ready(
        &self,
        left: &Endpoint,
        right: &Endpoint,
        tables: Option<&BTreeSet<String>>,
        workspaces: &[Workspace],
    ) -> bool {
        let complete = tables.map_or_else(
            || !left.table.is_empty() && !right.table.is_empty(),
            |tables| !tables.is_empty(),
        );
        left != right
            && complete
            && [left, right].into_iter().all(|endpoint| {
                !endpoint.schema.is_empty()
                    && endpoint.index(workspaces).is_some_and(|index| {
                        !workspaces[index].busy() && workspaces[index].server.is_some()
                    })
            })
    }

    fn request(&mut self, target: &Endpoint, workspaces: &mut [Workspace]) {
        let Some(index) = target.index(workspaces) else {
            return;
        };
        self.clear();
        self.cancelling = false;
        let workspace = &mut workspaces[index];
        if workspace.send(Command::SyncEndpoint, "正在获取目标连接…") {
            self.phase = Phase::Acquiring;
        } else {
            self.error = Some(workspace.status.clone());
        }
    }

    fn poll(
        &mut self,
        left: &Endpoint,
        right: &Endpoint,
        tables: Option<&BTreeSet<String>>,
        workspaces: &mut [Workspace],
    ) {
        if !self.active() {
            return;
        }
        let (source, target) = self.ends(left, right);
        let (Some(source_index), Some(target_index)) =
            (source.index(workspaces), target.index(workspaces))
        else {
            self.clear();
            self.error = Some("所选连接已关闭，请重新比较。".into());
            return;
        };
        match self.phase {
            Phase::Acquiring if !workspaces[target_index].busy() => {
                let Some(endpoint) = workspaces[target_index].sync_endpoint.take() else {
                    self.phase = Phase::Idle;
                    self.error = Some(format!(
                        "无法取得目标连接：{}",
                        workspaces[target_index].status
                    ));
                    return;
                };
                let policy = SyncPolicy {
                    mode: self.mode,
                    skip_target_fk_checks: self.skip_target_fk_checks,
                    continue_on_row_error: self.continue_on_row_error,
                };
                let command = if let Some(tables) = tables {
                    let mappings = tables
                        .iter()
                        .map(|name| {
                            self.mapping(
                                name,
                                DbObject {
                                    name: name.clone(),
                                    ..source.object()
                                },
                                DbObject {
                                    name: name.clone(),
                                    ..target.object()
                                },
                            )
                        })
                        .collect::<Result<Vec<_>, _>>();
                    mappings.map(|mappings| Command::PlanScopedSyncGroup {
                        mappings,
                        endpoint,
                        policy,
                    })
                } else {
                    self.mapping("", source.object(), target.object())
                        .map(|mapping| Command::PlanScopedSync {
                            mapping,
                            endpoint,
                            policy,
                        })
                };
                let command = match command {
                    Ok(command) => command,
                    Err(error) => {
                        self.phase = Phase::Idle;
                        self.error = Some(error);
                        return;
                    }
                };
                let workspace = &mut workspaces[source_index];
                if workspace.send(command, "正在扫描两端的数据…") {
                    self.phase = Phase::Comparing;
                } else {
                    self.phase = Phase::Idle;
                    self.error = Some(format!("无法开始同步比较：{}", workspace.status));
                }
            }
            Phase::Comparing | Phase::Exporting { .. } | Phase::Applying
                if !workspaces[source_index].busy() =>
            {
                let workspace = &mut workspaces[source_index];
                self.cancelling = false;
                match workspace.sync_reply.take() {
                    Some(SyncReply::Prepared(prepared)) => self.phase = Phase::Planned(prepared),
                    Some(SyncReply::Applied(result)) => self.phase = Phase::Finished(result),
                    Some(SyncReply::GroupPrepared(prepared)) => {
                        self.phase = Phase::PlannedGroup(prepared)
                    }
                    Some(SyncReply::GroupApplied(result)) => {
                        self.phase = Phase::FinishedGroup(result)
                    }
                    Some(SyncReply::Scripts(prepared, files)) => {
                        let execute = matches!(self.phase, Phase::Exporting { apply_after: true });
                        self.files = files;
                        if execute {
                            self.execute(prepared, workspace);
                        } else {
                            self.phase = match prepared {
                                SyncScriptPlan::Single(plan) => Phase::Planned(plan),
                                SyncScriptPlan::Group(plan) => Phase::PlannedGroup(plan),
                            };
                        }
                    }
                    Some(SyncReply::Failed(error)) => {
                        self.phase = Phase::Idle;
                        self.error = Some(error);
                    }
                    None => {
                        self.phase = Phase::Idle;
                        self.error = Some("同步任务没有返回结果，请检查连接后再试。".into());
                    }
                }
            }
            _ => {}
        }
    }

    fn execute(&mut self, prepared: SyncScriptPlan, workspace: &mut Workspace) {
        let command = match prepared {
            SyncScriptPlan::Single(prepared) => Command::ApplySync { prepared },
            SyncScriptPlan::Group(prepared) => Command::ApplySyncGroup { prepared },
        };
        self.phase = Phase::Idle;
        if workspace.send(command, "正在同步数据…") {
            self.phase = Phase::Applying;
        } else {
            self.error = Some(format!("无法开始同步：{}", workspace.status));
        }
    }

    pub(super) fn show(
        &mut self,
        ui: &mut egui::Ui,
        left: &Endpoint,
        right: &Endpoint,
        tables: Option<&BTreeSet<String>>,
        workspaces: &mut [Workspace],
    ) {
        self.poll(left, right, tables, workspaces);
        let old = (
            self.reverse,
            self.mode,
            self.skip_target_fk_checks,
            self.continue_on_row_error,
        );
        let active = self.active();
        ui.add_enabled_ui(!active, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(&mut self.mode, SyncMode::InsertMissing, "补齐缺少的行");
                ui.selectable_value(&mut self.mode, SyncMode::InsertAndUpdate, "补齐并更新");
                ui.selectable_value(&mut self.mode, SyncMode::MirrorReceiver, "镜像到接收端");
                if ui
                    .selectable_label(matches!(self.mode, SyncMode::ReconcileBoth(_)), "双向同步")
                    .clicked()
                    && !matches!(self.mode, SyncMode::ReconcileBoth(_))
                {
                    self.mode = SyncMode::ReconcileBoth(SyncConflict::KeepSeparate);
                }
            });
            ui.horizontal_wrapped(|ui| {
                if let SyncMode::ReconcileBoth(conflict) = &mut self.mode {
                    ui.label("同键记录有差异");
                    ui.selectable_value(conflict, SyncConflict::KeepSeparate, "保留两端原值");
                    ui.selectable_value(conflict, SyncConflict::SourceWins, "采用源端");
                    ui.selectable_value(conflict, SyncConflict::TargetWins, "采用目标端");
                } else {
                    ui.label("方向");
                    ui.selectable_value(&mut self.reverse, false, "源 → 目标");
                    ui.selectable_value(&mut self.reverse, true, "目标 → 源");
                }
            });
            let two_way = matches!(self.mode, SyncMode::ReconcileBoth(_));
            let label = if self.reverse && !two_way {
                "暂停源端接收数据时的外键检查"
            } else {
                "暂停目标端外键检查"
            };
            ui.checkbox(&mut self.skip_target_fk_checks, label)
                .on_hover_text(if two_way {
                    "只在目标端写入时暂停外键检查，源端仍会检查。目标端可能留下缺少父记录的数据。"
                } else {
                    "只在接收端写入时暂停外键检查，可能留下缺少父记录的数据。"
                });
            ui.checkbox(&mut self.continue_on_row_error, "写入出错时跳过该行并继续")
                .on_hover_text(
                    "直接同步时，跳过因数据或约束被数据库拒绝的行。比对失效或无法确认提交时停止。",
                );
            ui.collapsing("按行或字段比较", |ui| {
                self.scope_controls(ui, tables)
            });
        });
        if old
            != (
                self.reverse,
                self.mode,
                self.skip_target_fk_checks,
                self.continue_on_row_error,
            )
        {
            self.clear();
        }
        let (source, target) = self.ends(left, right);
        let ready = !active && self.ready(left, right, tables, workspaces);
        let uses_http = |endpoint: &Endpoint| {
            endpoint.index(workspaces).is_some_and(|index| {
                workspaces[index]
                    .server
                    .as_ref()
                    .is_some_and(|server| server.http_tunnel)
            })
        };
        let target_http = uses_http(target);
        let all_native = !uses_http(source) && !target_http;
        let mut start = false;
        let mut run = None;
        let mut cancel = false;
        ui.horizontal_wrapped(|ui| {
            start = ui
                .add_enabled(
                    ready,
                    style::primary(if tables.is_some() {
                        "比较所选表"
                    } else {
                        "比较整表"
                    }),
                )
                .clicked();
            if active {
                ui.spinner();
                ui.label(if self.cancelling {
                    "正在取消…"
                } else {
                    match self.phase {
                        Phase::Applying => "正在复核差异并同步…",
                        Phase::Exporting { .. } => "正在复核并保存同步 SQL…",
                        Phase::Acquiring => "正在取得接收端连接…",
                        _ => "正在读取两端完整数据…",
                    }
                });
                if let Some(index) = source.index(workspaces)
                    && let Some(progress) = &workspaces[index].sync_progress
                {
                    let read = format!(
                        "已读 {} / {} 行",
                        progress.source_rows, progress.target_rows
                    );
                    if matches!(self.phase, Phase::Applying) {
                        let side = progress.side.map_or(String::new(), |side| {
                            format!("（{}）", sync_side_label(side))
                        });
                        ui.label(format!("{read}，已写入事务 {} 行{side}", progress.written));
                    } else {
                        ui.label(read);
                    }
                }
                if !matches!(self.phase, Phase::Acquiring) {
                    let finishing = source
                        .index(workspaces)
                        .and_then(|index| workspaces[index].sync_progress.as_ref())
                        .is_some_and(|progress| progress.finishing);
                    cancel = ui
                        .add_enabled(!self.cancelling && !finishing, Button::new("取消操作"))
                        .clicked();
                }
            }
        });
        match &self.phase {
            Phase::Planned(prepared) => {
                let counts = &prepared.plan.counts;
                ui.separator();
                if prepared.plan.matches_whole_row() {
                    ui.label("按整行匹配。完全相同的多条记录按一种行值计数。");
                    if prepared.plan.planned_target().deleted > 0 {
                        ui.label("镜像会删除目标端独有行值的全部副本。");
                    }
                }
                ui.label(format!(
                    "相同 {} 行 · 发送端独有 {} 行 · 接收端独有 {} 行 · 同键记录有差异 {} 行",
                    counts.same, counts.source_only, counts.target_only, counts.conflicts
                ));
                if !prepared.plan.samples.is_empty() {
                    egui::ScrollArea::vertical()
                        .max_height(132.0)
                        .show(ui, |ui| {
                            for sample in &prepared.plan.samples {
                                let kind = match sample.kind {
                                    SyncDifferenceKind::SourceOnly => "发送端独有",
                                    SyncDifferenceKind::TargetOnly => "接收端独有",
                                    SyncDifferenceKind::Conflict => "同键记录有差异",
                                    SyncDifferenceKind::OutsideFilter => {
                                        "另一端的同键行在筛选范围外"
                                    }
                                };
                                let key = sample
                                    .key
                                    .iter()
                                    .map(|value| value.as_sql(true))
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                ui.add(egui::Label::new(format!("{kind} · {key}")).truncate());
                            }
                        });
                }
                if prepared.plan.outside_filter != 0 {
                    ui.label(format!(
                        "已跳过 {} 个筛选范围外的同键记录。",
                        prepared.plan.outside_filter
                    ));
                }
                run = Some((
                    prepared.plan.planned_source(),
                    prepared.plan.planned_target(),
                ));
            }
            Phase::Finished(result) => {
                show_commit(
                    ui,
                    result.source,
                    result.target,
                    result.stopped.as_deref(),
                    result.uncertain,
                );
                show_row_errors(ui, result.skipped_rows, &result.row_errors);
            }
            Phase::PlannedGroup(prepared) => {
                let mut from = SyncWrites::default();
                let mut to = SyncWrites::default();
                ui.separator();
                if prepared
                    .plan
                    .tables
                    .iter()
                    .any(|table| table.matches_whole_row())
                {
                    ui.label("整行匹配的表按不同的行值计数，镜像删除会移除独有行值的全部副本。");
                }
                egui::ScrollArea::vertical()
                    .max_height(160.0)
                    .show(ui, |ui| {
                        for table in &prepared.plan.tables {
                            let counts = &table.counts;
                            accumulate(&mut from, table.planned_source());
                            accumulate(&mut to, table.planned_target());
                            ui.add(
                                egui::Label::new(format!(
                                    "{} → {} · 发送端独有 {} · 接收端独有 {} · 同键记录有差异 {}{}",
                                    table.source.name,
                                    table.target.name,
                                    counts.source_only,
                                    counts.target_only,
                                    counts.conflicts,
                                    if table.matches_whole_row() {
                                        " · 整行匹配"
                                    } else {
                                        ""
                                    },
                                ))
                                .truncate(),
                            );
                        }
                    });
                run = Some((from, to));
            }
            Phase::FinishedGroup(result) => {
                ui.label(format!("处理 {} 张表。", result.completed));
                show_commit(
                    ui,
                    result.source,
                    result.target,
                    result.stopped.as_deref(),
                    result.uncertain,
                );
                show_row_errors(ui, result.skipped_rows, &result.row_errors);
            }
            _ => {}
        }
        let grouped = matches!(self.phase, Phase::PlannedGroup(_));
        let output = run.and_then(|(from, to)| {
            if target_http && to.deleted > 0 && !self.skip_target_fk_checks {
                ui.label(
                    RichText::new(
                        "HTTP 目标执行镜像删除时，需要账户拥有全局 SELECT 权限，以核对外键。",
                    )
                    .color(style::muted()),
                );
            }
            self.confirm_plan(ui, from, to, ready, all_native, grouped)
                .then_some((from, to))
        });
        for file in &self.files {
            let endpoint = if file.side == SyncSide::Source {
                source
            } else {
                target
            };
            ui.label(format!(
                "{} SQL 已保存到 {}",
                endpoint
                    .index(workspaces)
                    .map_or(sync_side_label(file.side), |index| workspaces[index]
                        .label
                        .as_str()),
                file.path.display()
            ));
        }
        if let Some(error) = &self.error {
            ui.colored_label(style::error(), error);
        }
        if start {
            self.request(target, workspaces);
        } else if let Some((from, to)) = output
            && let Some(index) = source.index(workspaces)
        {
            let paths = if self.output == Output::Execute {
                None
            } else {
                let Some(paths) = pick_paths(from, to) else {
                    return;
                };
                Some(paths)
            };
            let prepared = match std::mem::replace(&mut self.phase, Phase::Idle) {
                Phase::Planned(prepared) => SyncScriptPlan::Single(prepared),
                Phase::PlannedGroup(prepared) => SyncScriptPlan::Group(prepared),
                _ => return,
            };
            self.error = None;
            let workspace = &mut workspaces[index];
            if let Some(paths) = paths {
                if workspace.send(
                    Command::ExportSync { prepared, paths },
                    "正在复核并保存同步 SQL…",
                ) {
                    self.phase = Phase::Exporting {
                        apply_after: self.output == Output::SqlAndExecute,
                    };
                } else {
                    self.error = Some(format!("无法开始生成同步 SQL：{}", workspace.status));
                }
            } else {
                self.execute(prepared, workspace);
            }
        } else if cancel && let Some(index) = source.index(workspaces) {
            self.cancelling = workspaces[index].cancel_sync();
        }
    }
    fn confirm_plan(
        &mut self,
        ui: &mut egui::Ui,
        from: SyncWrites,
        to: SyncWrites,
        ready: bool,
        all_native: bool,
        grouped: bool,
    ) -> bool {
        ui.label(sync_write_summary(from, to));
        if from.total() + to.total() == 0 {
            return false;
        }
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut self.output, Output::Execute, "只执行");
            ui.selectable_value(&mut self.output, Output::Sql, "只生成 SQL");
            ui.selectable_value(&mut self.output, Output::SqlAndExecute, "生成 SQL 并执行");
        });
        if self.output != Output::Execute {
            ui.label(
                RichText::new(
                    "SQL 按写入端分别保存。单独回放脚本时，请使用能保持会话与事务的连接，核对结果后手动提交。",
                )
                .color(style::muted()),
            );
            if self.continue_on_row_error {
                ui.label(
                    RichText::new("跳过出错行的设置在程序直接执行同步时生效。").color(style::muted()),
                );
            }
        }
        if self.output != Output::Sql {
            let note = if self.continue_on_row_error {
                Some("结果会列出跳过的数量和错误编号。")
            } else {
                match self.mode {
                    SyncMode::ReconcileBoth(_) => {
                        Some("两端分别提交。如果第二端失败，第一端的已确认改动会保留。")
                    }
                    SyncMode::InsertMissing | SyncMode::InsertAndUpdate
                        if grouped && all_native =>
                    {
                        Some("各表分别提交。中途停止时，已提交的表会保留。")
                    }
                    _ => None,
                }
            };
            if let Some(note) = note {
                ui.label(RichText::new(note).color(style::muted()));
            }
        }
        let deletions = from.deleted + to.deleted;
        if deletions != 0 {
            ui.checkbox(
                &mut self.allow_delete,
                format!(
                    "确认计划包含删除接收端独有的 {deletions} 行。数据库触发器可能随写入执行。"
                ),
            );
        }
        ui.add_enabled(
            ready && (deletions == 0 || self.allow_delete),
            style::primary(self.output.label()),
        )
        .clicked()
    }
}

fn pick_paths(from: SyncWrites, to: SyncWrites) -> Option<SyncScriptPaths> {
    let source = from.total() > 0;
    let target = to.total() > 0;
    if source && target {
        let directory = rfd::FileDialog::new()
            .set_title("选择两端 SQL 的保存文件夹")
            .pick_folder()?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis();
        return Some(SyncScriptPaths {
            source: Some(directory.join(format!("harbor-sync-source-{stamp}.sql"))),
            target: Some(directory.join(format!("harbor-sync-target-{stamp}.sql"))),
            replace: false,
        });
    }
    let side = if source { "source" } else { "target" };
    let path = rfd::FileDialog::new()
        .set_title("保存数据同步 SQL")
        .add_filter("SQL", &["sql"])
        .set_file_name(format!("harbor-sync-{side}.sql"))
        .save_file()?;
    let mut paths = SyncScriptPaths {
        replace: true,
        ..Default::default()
    };
    if source {
        paths.source = Some(path);
    } else {
        paths.target = Some(path);
    }
    Some(paths)
}

fn show_commit(
    ui: &mut egui::Ui,
    from: SyncWrites,
    to: SyncWrites,
    stopped: Option<&str>,
    uncertain: Option<SyncSide>,
) {
    ui.label(format!("已确认提交。{}", sync_write_summary(from, to)));
    if let Some(error) = stopped {
        ui.colored_label(style::error(), error);
    }
    if let Some(side) = uncertain {
        ui.colored_label(
            style::error(),
            format!("无法确认{}提交结果。", sync_side_label(side)),
        );
    }
}

fn show_row_errors(ui: &mut egui::Ui, skipped_rows: u64, errors: &[SyncRowError]) {
    if skipped_rows == 0 {
        return;
    }
    ui.colored_label(style::error(), format!("跳过 {skipped_rows} 行。"));
    if errors.is_empty() {
        return;
    }
    ui.collapsing("查看跳过的原因", |ui| {
        egui::ScrollArea::vertical()
            .max_height(130.0)
            .show(ui, |ui| {
                for error in errors {
                    let operation = match error.operation {
                        "insert" => "新增",
                        "update" => "更新",
                        "delete" => "删除",
                        _ => "写入",
                    };
                    ui.label(format!(
                        "{} {}.{} · {operation}失败 · 错误 {}",
                        sync_side_label(error.side),
                        error.table.schema,
                        error.table.name,
                        error.code,
                    ));
                }
            });
        if (errors.len() as u64) < skipped_rows {
            ui.label(format!("已列出其中 {} 条错误记录。", errors.len()));
        }
    });
}

fn accumulate(total: &mut SyncWrites, one: SyncWrites) {
    total.inserted += one.inserted;
    total.updated += one.updated;
    total.deleted += one.deleted;
}
