use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

use eframe::egui::{self, Align, Layout, TextEdit};
use harbor_sql::APP_NAME;
use harbor_sql::jobs::{
    CANCELLED_EXIT_CODE, DESKTOP_ODBC_RUN, DESKTOP_ODBC_STOP, backup_editor, data_sync_editor,
    is_odbc_job, job_lock_path, notification_editor, odbc_editor,
};
use serde::{Deserialize, Serialize};

use super::state::{Profile, Workspace, profile_path, write_atomic};
use super::style;
use crate::database::DbObject;

mod authoring;
mod backup;
mod data_sync;
mod legacy;
mod notification;
mod odbc_job;
mod schedule;

const CATALOG_LIMIT: u64 = 1024 * 1024;
const OUTPUT_LIMIT: usize = 32 * 1024;

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    name: String,
    path: PathBuf,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    version: u32,
    entries: Vec<Entry>,
}

#[derive(Clone)]
enum Operation {
    Check,
    Run,
    Schedule(schedule::Change),
    RemoveEntry,
}

impl Operation {
    fn label(&self) -> &'static str {
        match self {
            Self::Check => "检查",
            Self::Run => "运行",
            Self::Schedule(change) => change.label(),
            Self::RemoveEntry => "移除",
        }
    }
}

struct Task {
    operation: Operation,
    entry: Entry,
    result: Receiver<Outcome>,
    odbc_stop: Option<OdbcStop>,
}

enum OdbcStop {
    Ready(ChildStdin),
    Requested,
}

enum Outcome {
    OdbcRunning(ChildStdin),
    Job(Result<Output, String>),
    Schedule(Result<schedule::Status, String>),
    Removed(Result<(Vec<Entry>, Option<PathBuf>), String>),
}

struct Output {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    truncated: bool,
}

struct Finished {
    operation: Operation,
    name: String,
    result: Result<Output, String>,
}

#[derive(Default)]
pub(super) struct Manager {
    open: bool,
    entries: Vec<Entry>,
    selected: Option<PathBuf>,
    name: String,
    schedule_status: Option<schedule::Status>,
    schedule_fields: schedule::Fields,
    inspect_schedule: bool,
    task: Option<Task>,
    finished: Option<Finished>,
    error: Option<String>,
    editor: Option<Editor>,
    legacy_import: Option<legacy::Importer>,
}

enum Editor {
    Backup(backup::Editor),
    DataSync(data_sync::Editor),
    Notification(notification::Editor),
    Odbc(odbc_job::Editor),
}

impl Editor {
    fn form(&mut self) -> &mut authoring::Form {
        match self {
            Self::Backup(editor) => &mut editor.form,
            Self::DataSync(editor) => &mut editor.form,
            Self::Notification(editor) => &mut editor.form,
            Self::Odbc(editor) => &mut editor.form,
        }
    }

    fn dialog(&mut self, ctx: &egui::Context, profiles: &[Profile]) -> Option<authoring::Action> {
        match self {
            Self::Backup(editor) => editor.dialog(ctx, profiles),
            Self::DataSync(editor) => editor.dialog(ctx, profiles),
            Self::Notification(editor) => editor.dialog(ctx, profiles),
            Self::Odbc(editor) => editor.dialog(ctx, profiles),
        }
    }

    fn validated_bytes(&self, path: &Path) -> Result<Vec<u8>, String> {
        match self {
            Self::Backup(editor) => editor.definition.validated_bytes(path),
            Self::DataSync(editor) => editor.definition.validated_bytes(path),
            Self::Notification(editor) => editor.definition.validated_bytes(path),
            Self::Odbc(editor) => editor.definition.validated_bytes(path),
        }
    }

    fn verify_unchanged(&self, path: &Path) -> Result<(), String> {
        match self {
            Self::Backup(editor) => editor.definition.verify_unchanged(path),
            Self::DataSync(editor) => editor.definition.verify_unchanged(path),
            Self::Notification(editor) => editor.definition.verify_unchanged(path),
            Self::Odbc(editor) => editor.definition.verify_unchanged(path),
        }
    }
}

enum Action {
    NewBackup,
    NewDataSync,
    NewNotification,
    NewOdbc,
    ImportLegacy,
    Add,
    Select(PathBuf),
    Rename,
    Remove,
    Edit,
    EditJson,
    Start(Operation),
    StopOdbc,
    Close,
}

impl Manager {
    pub(super) fn open(&mut self) {
        self.open = true;
        self.error = None;
        match catalog_path().and_then(|path| load(&path)) {
            Ok(entries) => {
                self.entries = entries;
                let selection = self.selected_entry().or_else(|| self.entries.first());
                self.select(selection.map(|entry| entry.path.clone()));
                self.inspect_schedule = schedule::SUPPORTED && self.selected.is_some();
            }
            Err(error) => self.error = Some(error),
        }
    }

    pub(super) fn active(&self) -> bool {
        self.open
    }

    pub(super) fn busy(&self) -> bool {
        self.task.is_some()
    }

    pub(super) fn open_backup(
        &mut self,
        workspace: Option<&Workspace>,
        schema: String,
        object: Option<DbObject>,
    ) {
        self.open();
        if !self.busy() && self.error.is_none() {
            self.editor = Some(Editor::Backup(backup::Editor::new(
                workspace, schema, object,
            )));
        }
    }

    pub(super) fn open_data_sync(&mut self, workspace: Option<&Workspace>) {
        self.open();
        if !self.busy() && self.error.is_none() {
            self.editor = Some(Editor::DataSync(data_sync::Editor::new(workspace)));
        }
    }

    pub(super) fn open_notification(&mut self, workspace: Option<&Workspace>) {
        self.open();
        if !self.busy() && self.error.is_none() {
            self.editor = Some(Editor::Notification(notification::Editor::new(workspace)));
        }
    }

    pub(super) fn open_odbc(&mut self, workspace: Option<&Workspace>) {
        self.open();
        if !self.busy() && self.error.is_none() {
            self.editor = Some(Editor::Odbc(odbc_job::Editor::new(workspace)));
        }
    }

    pub(super) fn dialog(
        &mut self,
        ctx: &egui::Context,
        profiles: &[Profile],
        workspace: Option<&Workspace>,
    ) {
        self.poll();
        if !self.open {
            return;
        }
        if let Some(importer) = &mut self.legacy_import {
            match importer.dialog(ctx) {
                Some(legacy::Action::Close) => self.legacy_import = None,
                Some(legacy::Action::Save) => {
                    if let Err(error) = self.save_legacy_import()
                        && let Some(importer) = self.legacy_import.as_mut()
                    {
                        importer.error = Some(error);
                    }
                }
                None => {}
            }
            return;
        }
        if let Some(editor) = &mut self.editor {
            if let Some(action) = editor.dialog(ctx, profiles) {
                if matches!(action, authoring::Action::Close) {
                    self.editor = None;
                } else if let Err(error) = self.save_editor(action, ctx) {
                    if let Some(editor) = &mut self.editor {
                        editor.form().error = Some(error);
                    } else {
                        self.error = Some(format!("作业已保存，后续操作失败：{error}"));
                    }
                }
            }
            return;
        }
        if self.inspect_schedule && !self.busy() {
            self.inspect_schedule = false;
            if let Err(error) = self.start(Operation::Schedule(schedule::Change::Inspect), ctx) {
                self.error = Some(error);
            }
        }
        let selected = self.selected_entry().cloned();
        let busy = self.busy();
        let width = (ctx.content_rect().width() - 48.0).clamp(400.0, 820.0);
        let height = ctx.content_rect().height();
        let response = style::modal(ctx, egui::Id::new("sja-job-manager")).show(ctx, |ui| {
            ui.set_width(width);
            // The modal starts layout with its previous frame's size.
            ui.set_max_height(height);
            let mut action = None;
            ui.horizontal(|ui| {
                style::page_title(ui, "作业管理");
                ui.add_space(6.0);
                style::badge(
                    ui,
                    format!("{} 个作业", self.entries.len()),
                    style::accent(),
                    style::accent_soft(),
                );
            });
            ui.add_space(8.0);
            style::subtle_card(egui::Margin::symmetric(10, 8)).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(!busy, style::primary_enabled(!busy, "新建备份作业…"))
                        .clicked()
                    {
                        action = Some(Action::NewBackup);
                    }
                    if ui
                        .add_enabled(!busy, egui::Button::new("新建数据同步作业…"))
                        .clicked()
                    {
                        action = Some(Action::NewDataSync);
                    }
                    if ui
                        .add_enabled(!busy, egui::Button::new("新建 SQL 作业…"))
                        .clicked()
                    {
                        action = Some(Action::NewNotification);
                    }
                    if ui
                        .add_enabled(!busy, egui::Button::new("新建外部数据导入作业…"))
                        .clicked()
                    {
                        action = Some(Action::NewOdbc);
                    }
                    if ui
                        .add_enabled(!busy, egui::Button::new("登记作业 JSON…"))
                        .clicked()
                    {
                        action = Some(Action::Add);
                    }
                    if ui
                        .add_enabled(!busy, egui::Button::new("导入 SQLyog 作业 XML…"))
                        .clicked()
                    {
                        action = Some(Action::ImportLegacy);
                    }
                });
            });
            ui.add_space(8.0);
            egui::ScrollArea::vertical()
                .id_salt("job-manager-body")
                .max_height((height - 190.0).max(120.0))
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    let list_panel = |ui: &mut egui::Ui,
                                      this: &Self,
                                      action: &mut Option<Action>| {
                        style::section_card(ui, "已登记作业", |ui| {
                            if this.entries.is_empty() {
                                ui.label(
                                    egui::RichText::new("暂无作业，可从上方新建或登记。")
                                        .small()
                                        .color(style::muted()),
                                );
                            } else {
                                egui::ScrollArea::vertical()
                                    .id_salt("job-manager-entries")
                                    .auto_shrink([false, false])
                                    .max_height((height * 0.36).clamp(150.0, 280.0))
                                    .show(ui, |ui| {
                                        for entry in &this.entries {
                                            let current =
                                                this.selected.as_ref() == Some(&entry.path);
                                            let label = if current {
                                                egui::RichText::new(&entry.name)
                                                    .strong()
                                                    .color(style::accent())
                                            } else {
                                                egui::RichText::new(&entry.name)
                                                    .color(style::ink())
                                            };
                                            if ui
                                                .add_sized(
                                                    [ui.available_width(), 26.0],
                                                    egui::Button::selectable(current, label)
                                                        .truncate(),
                                                )
                                                .on_hover_text(entry.path.display().to_string())
                                                .clicked()
                                            {
                                                *action = Some(Action::Select(entry.path.clone()));
                                            }
                                        }
                                    });
                            }
                        });
                    };
                    let detail_panel = |ui: &mut egui::Ui,
                                        this: &mut Self,
                                        selected: Option<Entry>,
                                        action: &mut Option<Action>| {
                        style::section_card(ui, "作业详情与调度", |ui| {
                            let Some(entry) = selected else {
                                ui.label(
                                    egui::RichText::new("在左侧选择一个作业以查看路径、执行或配置系统调度。")
                                        .small()
                                        .color(style::muted()),
                                );
                                return;
                            };
                            let path = entry.path.display().to_string();
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&path)
                                        .small()
                                        .monospace()
                                        .color(style::muted()),
                                )
                                .truncate(),
                            )
                            .on_hover_text(&path);
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("显示名称").color(style::muted()));
                                ui.add(
                                    TextEdit::singleline(&mut this.name)
                                        .char_limit(128)
                                        .desired_width((ui.available_width() - 180.0).max(120.0)),
                                );
                                let changed = !this.name.trim().is_empty()
                                    && this.name.trim() != entry.name;
                                if ui
                                    .add_enabled(changed && !busy, egui::Button::new("保存名称"))
                                    .clicked()
                                {
                                    *action = Some(Action::Rename);
                                }
                            });
                            ui.add_space(6.0);
                            ui.horizontal_wrapped(|ui| {
                                if ui
                                    .add_enabled(!busy, style::primary_enabled(!busy, "运行一次"))
                                    .clicked()
                                {
                                    *action = Some(Action::Start(Operation::Run));
                                }
                                if ui
                                    .add_enabled(!busy, egui::Button::new("编辑作业…"))
                                    .clicked()
                                {
                                    *action = Some(Action::Edit);
                                }
                                if ui
                                    .add_enabled(!busy, egui::Button::new("编辑 JSON…"))
                                    .clicked()
                                {
                                    *action = Some(Action::EditJson);
                                }
                                if ui.add_enabled(!busy, egui::Button::new("检查")).clicked() {
                                    *action = Some(Action::Start(Operation::Check));
                                }
                                if ui
                                    .add_enabled(
                                        !busy,
                                        egui::Button::new(
                                            egui::RichText::new("从列表移除").color(style::error()),
                                        ),
                                    )
                                    .clicked()
                                {
                                    *action = Some(Action::Remove);
                                }
                            });
                            if schedule::SUPPORTED {
                                ui.add_space(8.0);
                                ui.separator();
                                ui.add_space(6.0);
                                ui.label(
                                    egui::RichText::new("系统调度")
                                        .strong()
                                        .color(style::ink()),
                                );
                                if let Some(status) = &this.schedule_status {
                                    ui.label(
                                        egui::RichText::new(status.label())
                                            .small()
                                            .color(style::muted()),
                                    );
                                }
                                ui.add_enabled_ui(!busy, |ui| this.schedule_fields.show(ui));
                                ui.add_space(4.0);
                                ui.horizontal_wrapped(|ui| {
                                    let trigger = this.schedule_fields.trigger();
                                    if ui
                                        .add_enabled(
                                            !busy && trigger.is_some(),
                                            egui::Button::new("保存调度"),
                                        )
                                        .on_hover_text(format!(
                                            "关闭 {APP_NAME} 后仍由系统触发。"
                                        ))
                                        .clicked()
                                        && let Some(trigger) = trigger
                                    {
                                        *action = Some(Action::Start(Operation::Schedule(
                                            schedule::Change::Save(trigger),
                                        )));
                                    }
                                    if ui
                                        .add_enabled(!busy, egui::Button::new("刷新状态"))
                                        .clicked()
                                    {
                                        *action = Some(Action::Start(Operation::Schedule(
                                            schedule::Change::Inspect,
                                        )));
                                    }
                                    if ui
                                        .add_enabled(
                                            !busy
                                                && matches!(
                                                    this.schedule_status,
                                                    Some(schedule::Status::Registered { .. })
                                                ),
                                            egui::Button::new("取消调度"),
                                        )
                                        .clicked()
                                    {
                                        *action = Some(Action::Start(Operation::Schedule(
                                            schedule::Change::Cancel,
                                        )));
                                    }
                                });
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(schedule::ACCOUNT_NOTE)
                                            .small()
                                            .color(style::muted()),
                                    )
                                    .wrap(),
                                );
                            }
                        });
                    };
                    if width >= 620.0 {
                        let mut ctx_state = (&mut *self, &mut action);
                        style::split_two_columns_with(
                            ui,
                            0.36,
                            12.0,
                            &mut ctx_state,
                            |ui, (this, action)| list_panel(ui, this, action),
                            |ui, (this, action)| detail_panel(ui, this, selected, action),
                        );
                    } else {
                        list_panel(ui, self, &mut action);
                        ui.add_space(8.0);
                        detail_panel(ui, self, selected, &mut action);
                    }
                    if let Some(error) = &self.error {
                        ui.add_space(6.0);
                        style::banner(ui, error, style::error(), style::error_soft());
                    }
                    if let Some(task) = &self.task {
                        ui.horizontal_wrapped(|ui| {
                            ui.spinner();
                            match &task.odbc_stop {
                                Some(OdbcStop::Requested) => {
                                    ui.label(format!("正在停止导入 {}…", task.entry.name))
                                        .on_hover_text(
                                            "等待当前数据库操作返回并处理未完成的数据。",
                                        );
                                }
                                Some(OdbcStop::Ready(_)) => {
                                    ui.label(format!("正在导入 {}…", task.entry.name));
                                    if ui.button("停止导入").clicked() {
                                        action = Some(Action::StopOdbc);
                                    }
                                }
                                None => {
                                    ui.label(format!(
                                        "正在{} {}…",
                                        task.operation.label(),
                                        task.entry.name
                                    ));
                                }
                            }
                        });
                        ctx.request_repaint_after(Duration::from_millis(120));
                    } else if let Some(finished) = &self.finished {
                        show_result(ui, finished, (height * 0.23).min(240.0));
                    }
                });
            ui.separator();
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                if ui.button("关闭").clicked() {
                    action = Some(Action::Close);
                }
            });
            action
        });
        if response.should_close() {
            self.open = false;
        }
        if let Some(action) = response.inner {
            self.error = self.apply(action, ctx, workspace).err();
        }
    }

    fn selected_entry(&self) -> Option<&Entry> {
        self.selected
            .as_ref()
            .and_then(|selected| self.entries.iter().find(|entry| &entry.path == selected))
    }

    fn select(&mut self, path: Option<PathBuf>) {
        if self.selected != path {
            self.schedule_status = None;
            self.schedule_fields = schedule::Fields::default();
            self.inspect_schedule = schedule::SUPPORTED && path.is_some();
        }
        self.selected = path;
        self.name = self
            .selected_entry()
            .map_or(String::new(), |entry| entry.name.clone());
    }

    fn apply(
        &mut self,
        action: Action,
        ctx: &egui::Context,
        workspace: Option<&Workspace>,
    ) -> Result<(), String> {
        match action {
            Action::NewBackup => {
                self.editor = Some(Editor::Backup(backup::Editor::new(
                    workspace,
                    String::new(),
                    None,
                )));
            }
            Action::NewDataSync => {
                self.editor = Some(Editor::DataSync(data_sync::Editor::new(workspace)));
            }
            Action::NewNotification => {
                self.editor = Some(Editor::Notification(notification::Editor::new(workspace)));
            }
            Action::NewOdbc => {
                self.editor = Some(Editor::Odbc(odbc_job::Editor::new(workspace)));
            }
            Action::ImportLegacy => {
                self.legacy_import = legacy::Importer::pick()?;
            }
            Action::Add => {
                let Some(path) = rfd::FileDialog::new()
                    .add_filter("作业 JSON", &["json"])
                    .pick_file()
                else {
                    return Ok(());
                };
                let canonical = backing_file(&path)?;
                let matched = self.entries.iter().find(|entry| {
                    entry.path == path
                        || fs::canonicalize(&entry.path).is_ok_and(|current| current == canonical)
                });
                if let Some(entry) = matched {
                    self.select(Some(entry.path.clone()));
                    return Ok(());
                }
                let path = std::path::absolute(path)
                    .map_err(|error| format!("无法定位作业文件：{error}"))?;
                let mut entries = self.entries.clone();
                entries.push(Entry {
                    name: job_name(&path),
                    path: path.clone(),
                });
                self.commit(entries, Some(path))?;
            }
            Action::Select(path) => self.select(Some(path)),
            Action::Rename => {
                let selected = self.selected.as_ref().ok_or("请先选择作业。")?;
                let mut entries = self.entries.clone();
                let entry = entries
                    .iter_mut()
                    .find(|entry| &entry.path == selected)
                    .ok_or("作业已从列表移除。")?;
                entry.name = self.name.trim().into();
                self.commit(entries, Some(selected.clone()))?;
            }
            Action::Remove => self.start(Operation::RemoveEntry, ctx)?,
            Action::Edit => {
                let path = &self.selected_entry().ok_or("请先选择作业。")?.path;
                let canonical = backing_file(path)?;
                if let Some(definition) = backup_editor::Definition::read(&canonical)? {
                    self.editor = Some(Editor::Backup(backup::Editor::from_definition(
                        definition,
                        Some(path.clone()),
                        self.schedule_fields.trigger(),
                    )));
                } else if let Some(definition) = data_sync_editor::Definition::read(&canonical)? {
                    self.editor = Some(Editor::DataSync(data_sync::Editor::from_definition(
                        definition,
                        Some(path.clone()),
                        self.schedule_fields.trigger(),
                    )));
                } else if let Some(definition) = notification_editor::Definition::read(&canonical)?
                {
                    self.editor =
                        Some(Editor::Notification(notification::Editor::from_definition(
                            definition,
                            Some(path.clone()),
                            self.schedule_fields.trigger(),
                        )));
                } else if let Some(definition) = odbc_editor::Definition::read(&canonical)? {
                    self.editor = Some(Editor::Odbc(odbc_job::Editor::from_definition(
                        definition,
                        Some(path.clone()),
                        self.schedule_fields.trigger(),
                    )));
                } else {
                    edit_document(path)?;
                }
            }
            Action::EditJson => {
                let path = &self.selected_entry().ok_or("请先选择作业。")?.path;
                backing_file(path)?;
                edit_document(path)?;
            }
            Action::Start(operation) => self.start(operation, ctx)?,
            Action::StopOdbc => self.stop_odbc()?,
            Action::Close => self.open = false,
        }
        Ok(())
    }

    fn save_legacy_import(&mut self) -> Result<(), String> {
        let importer = self.legacy_import.as_ref().ok_or("请先选择旧版作业。")?;
        let prepared = importer.prepared()?;
        let path = prepared.destination().to_path_buf();
        let entry = Entry {
            name: job_name(&path),
            path: path.clone(),
        };
        self.entries = create_job(
            &catalog_path()?,
            &self.entries,
            entry,
            prepared.validated_bytes()?,
        )?;
        self.legacy_import = None;
        self.finished = None;
        self.error = None;
        self.select(Some(path));
        Ok(())
    }

    fn save_editor(
        &mut self,
        action: authoring::Action,
        ctx: &egui::Context,
    ) -> Result<(), String> {
        let editor = self.editor.as_mut().ok_or("请先填写作业。")?;
        let form = editor.form();
        let editing = form.source.is_some();
        let Some(chosen) = form.destination() else {
            return Ok(());
        };
        let path =
            std::path::absolute(chosen).map_err(|error| format!("无法定位作业文件：{error}"))?;
        if !is_json_path(&path) {
            return Err("请使用 .json 作为作业文件扩展名。".into());
        }
        if editing {
            update_job(&path, |path| {
                editor.verify_unchanged(path)?;
                editor.validated_bytes(path)
            })?;
        } else {
            let entry = Entry {
                name: job_name(&path),
                path: path.clone(),
            };
            let bytes = editor.validated_bytes(&path)?;
            self.entries = create_job(&catalog_path()?, &self.entries, entry, &bytes)?;
        }
        self.editor = None;
        self.finished = None;
        self.error = None;
        self.select(Some(path));
        match action {
            authoring::Action::Save => Ok(()),
            authoring::Action::Run => self.start(Operation::Run, ctx),
            authoring::Action::Schedule(trigger) => {
                self.schedule_fields.set(&trigger);
                self.inspect_schedule = false;
                self.start(Operation::Schedule(schedule::Change::Save(trigger)), ctx)
            }
            authoring::Action::Close => unreachable!("close does not save"),
        }
    }

    fn commit(&mut self, entries: Vec<Entry>, selection: Option<PathBuf>) -> Result<(), String> {
        save(&catalog_path()?, &self.entries, &entries)?;
        self.entries = entries;
        self.select(selection);
        Ok(())
    }

    fn start(&mut self, operation: Operation, ctx: &egui::Context) -> Result<(), String> {
        if self.busy() {
            return Err("作业正在执行。".into());
        }
        let entry = self.selected_entry().ok_or("请先选择作业。")?.clone();
        let path = entry.path.clone();
        let removal = matches!(operation, Operation::RemoveEntry).then(|| self.entries.clone());
        let pending_operation = operation.clone();
        let (sender, result) = mpsc::sync_channel(2);
        let ctx = ctx.clone();
        thread::Builder::new()
            .name("harbor-sql-job-manager".into())
            .spawn(move || {
                let result = match operation {
                    Operation::Check => Outcome::Job(
                        backing_file(&path).and_then(|path| run(&companion()?, operation, &path)),
                    ),
                    Operation::Run => Outcome::Job(backing_file(&path).and_then(|path| {
                        run_desktop_job(&companion()?, &path, |input| {
                            let _ = sender.send(Outcome::OdbcRunning(input));
                            ctx.request_repaint();
                        })
                    })),
                    Operation::Schedule(change) => {
                        Outcome::Schedule(change_schedule(change, &path))
                    }
                    Operation::RemoveEntry => Outcome::Removed(remove_entry(
                        &path,
                        removal.expect("catalog snapshot for removal"),
                    )),
                };
                let _ = sender.send(result);
                ctx.request_repaint();
            })
            .map_err(|error| format!("无法启动作业线程：{error}"))?;
        if matches!(pending_operation, Operation::Check | Operation::Run) {
            self.finished = None;
        }
        self.task = Some(Task {
            operation: pending_operation,
            entry,
            result,
            odbc_stop: None,
        });
        Ok(())
    }

    fn stop_odbc(&mut self) -> Result<(), String> {
        let Some(task) = &mut self.task else {
            return Ok(());
        };
        let Some(OdbcStop::Ready(input)) = &mut task.odbc_stop else {
            return Ok(());
        };
        // The dedicated pipe has no other writer and this is its only byte.
        match input.write_all(&[DESKTOP_ODBC_STOP]) {
            Ok(()) => task.odbc_stop = Some(OdbcStop::Requested),
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => task.odbc_stop = None,
            Err(error) => return Err(format!("无法发送停止请求：{error}")),
        }
        Ok(())
    }

    fn poll(&mut self) {
        let Some(task) = &mut self.task else {
            return;
        };
        let result = match task.result.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.task = None;
                self.error = Some("作业线程未返回执行结果。".into());
                return;
            }
        };
        match result {
            Outcome::OdbcRunning(input) => {
                task.odbc_stop = Some(OdbcStop::Ready(input));
                return;
            }
            Outcome::Job(result) => {
                self.finished = Some(Finished {
                    operation: task.operation.clone(),
                    name: task.entry.name.clone(),
                    result,
                });
            }
            Outcome::Schedule(result) if Some(&task.entry.path) == self.selected.as_ref() => {
                match result {
                    Ok(status) => {
                        if let Some(trigger) = status.trigger() {
                            self.schedule_fields.set(trigger);
                        }
                        self.schedule_status = Some(status);
                    }
                    Err(error) => {
                        self.schedule_status = None;
                        self.error = Some(error);
                    }
                }
            }
            Outcome::Schedule(_) => {}
            Outcome::Removed(Ok((entries, next))) => {
                let selection = self
                    .selected
                    .as_ref()
                    .filter(|path| **path != task.entry.path)
                    .cloned()
                    .or(next);
                self.entries = entries;
                self.select(selection);
            }
            Outcome::Removed(Err(error)) => {
                self.error = Some(error);
                self.inspect_schedule = schedule::SUPPORTED && self.selected.is_some();
            }
        }
        self.task = None;
    }
}

fn show_result(ui: &mut egui::Ui, finished: &Finished, height: f32) {
    ui.separator();
    let label = format!("{} {}", finished.operation.label(), finished.name);
    match &finished.result {
        Err(error) => {
            ui.colored_label(style::error(), format!("{label}：{error}"));
        }
        Ok(output) => {
            match output.code {
                Some(0) => {
                    ui.label(format!("{label}成功"));
                }
                Some(CANCELLED_EXIT_CODE) => {
                    ui.label(format!("{label}已停止"));
                }
                Some(code) => {
                    ui.colored_label(style::error(), format!("{label}退出，退出码 {code}"));
                }
                None => {
                    ui.colored_label(style::error(), format!("{label}结束，未返回退出码"));
                }
            }
            egui::ScrollArea::vertical()
                .max_height(height)
                .show(ui, |ui| {
                    for text in [&output.stdout, &output.stderr] {
                        if !text.is_empty() {
                            ui.add(egui::Label::new(text).selectable(true).wrap());
                        }
                    }
                });
            if output.truncated {
                ui.label("已截断较长的进程输出。");
            }
        }
    }
}

fn catalog_path() -> Result<PathBuf, String> {
    profile_path()
        .map(|path| path.with_file_name("job-manager.json"))
        .ok_or_else(|| "无法找到本机的配置目录。".into())
}

fn load(path: &Path) -> Result<Vec<Entry>, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("无法打开作业列表：{error}")),
    };
    let mut bytes = Vec::new();
    file.take(CATALOG_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("无法读取作业列表：{error}"))?;
    if bytes.len() as u64 > CATALOG_LIMIT {
        return Err("作业列表超过 1 MiB。".into());
    }
    let catalog: Catalog = serde_json::from_slice(&bytes).map_err(|_| "作业列表文件格式无效。")?;
    if catalog.version != 1
        || catalog
            .entries
            .iter()
            .any(|entry| !entry.path.is_absolute() || entry.name.trim().is_empty())
    {
        return Err("作业列表文件版本或条目无效。".into());
    }
    Ok(catalog.entries)
}

fn save(path: &Path, current: &[Entry], entries: &[Entry]) -> Result<(), String> {
    let _lock = lock_catalog(path)?;
    verify_catalog(path, current)?;
    write_catalog(path, entries)
}

fn create_job(
    catalog: &Path,
    current: &[Entry],
    entry: Entry,
    bytes: &[u8],
) -> Result<Vec<Entry>, String> {
    let _lock = lock_catalog(catalog)?;
    verify_catalog(catalog, current)?;
    if current.iter().any(|existing| existing.path == entry.path) {
        return Err("此路径已登记，请使用列表中的作业，或为新作业选择另一文件名。".into());
    }
    let folder = entry.path.parent().ok_or("无法定位作业文件目录。")?;
    let mut file = tempfile::NamedTempFile::new_in(folder)
        .map_err(|error| format!("无法创建作业临时文件：{error}"))?;
    file.write_all(bytes)
        .and_then(|()| file.as_file().sync_all())
        .map_err(|error| format!("无法写入作业文件：{error}"))?;
    file.persist_noclobber(&entry.path)
        .map_err(|error| format!("无法保存新作业，请选择尚未使用的文件名：{}", error.error))?;
    let path = entry.path.clone();
    let mut entries = current.to_vec();
    entries.push(entry);
    if let Err(error) = write_catalog(catalog, &entries) {
        if fs::read(&path).is_ok_and(|current| current == bytes) && fs::remove_file(&path).is_ok() {
            return Err(error);
        }
        return Err(format!(
            "{error}。请检查已生成的作业文件 {}，可用作业管理重新登记。",
            path.display()
        ));
    }
    Ok(entries)
}

fn update_job(
    path: &Path,
    prepare: impl FnOnce(&Path) -> Result<Vec<u8>, String>,
) -> Result<(), String> {
    let target = backing_file(path)?;
    let _lock = lock_job(&target)?;
    let bytes = prepare(&target)?;
    write_atomic(&target, &bytes).map_err(|error| format!("无法保存作业：{error}"))
}

fn lock_job(canonical: &Path) -> Result<File, String> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(job_lock_path(canonical))
        .map_err(|error| format!("无法打开作业锁：{error}"))?;
    lock.try_lock().map_err(|error| match error {
        fs::TryLockError::WouldBlock => "作业正在运行，请在运行结束后保存。".into(),
        fs::TryLockError::Error(error) => format!("无法锁定作业：{error}"),
    })?;
    Ok(lock)
}

fn job_name(path: &Path) -> String {
    path.file_stem()
        .map(|name| name.to_string_lossy().trim().chars().take(128).collect())
        .filter(|name: &String| !name.is_empty())
        .unwrap_or_else(|| "作业".into())
}

fn change_schedule(change: schedule::Change, job: &Path) -> Result<schedule::Status, String> {
    if matches!(change, schedule::Change::Inspect) {
        return schedule::apply(change, job);
    }
    let catalog = catalog_path()?;
    let _lock = lock_catalog(&catalog)?;
    if !load(&catalog)?.iter().any(|entry| entry.path == job) {
        return Err("作业已从列表移除，请重新打开作业管理。".into());
    }
    schedule::apply(change, job)
}

fn remove_entry(
    job: &Path,
    mut entries: Vec<Entry>,
) -> Result<(Vec<Entry>, Option<PathBuf>), String> {
    let catalog = catalog_path()?;
    let _lock = lock_catalog(&catalog)?;
    verify_catalog(&catalog, &entries)?;
    let index = entries
        .iter()
        .position(|entry| entry.path == job)
        .ok_or("作业已从列表移除。")?;
    if schedule::SUPPORTED
        && matches!(
            schedule::apply(schedule::Change::Inspect, job)?,
            schedule::Status::Registered { .. }
        )
    {
        return Err("请先取消系统调度，再从列表移除作业。".into());
    }
    entries.remove(index);
    let next = entries
        .get(index)
        .or_else(|| entries.last())
        .map(|entry| entry.path.clone());
    write_catalog(&catalog, &entries)?;
    Ok((entries, next))
}

fn lock_catalog(path: &Path) -> Result<File, String> {
    let parent = path.parent().ok_or("无法定位作业列表目录。")?;
    fs::create_dir_all(parent).map_err(|error| format!("无法创建作业列表目录：{error}"))?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))
        .map_err(|error| format!("无法打开作业列表锁：{error}"))?;
    lock.try_lock()
        .map_err(|error| format!("作业列表正在使用：{error}"))?;
    Ok(lock)
}

fn verify_catalog(path: &Path, current: &[Entry]) -> Result<(), String> {
    if load(path)? != current {
        return Err("作业列表已在其他窗口更新，请重新打开列表。".into());
    }
    Ok(())
}

fn write_catalog(path: &Path, entries: &[Entry]) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(&Catalog {
        version: 1,
        entries: entries.to_vec(),
    })
    .map_err(|_| "作业列表无法保存为 JSON。")?;
    if bytes.len() as u64 > CATALOG_LIMIT {
        return Err("作业列表超过 1 MiB。".into());
    }
    write_atomic(path, &bytes).map_err(|error| format!("无法保存作业列表：{error}"))
}

fn backing_file(path: &Path) -> Result<PathBuf, String> {
    let canonical = fs::canonicalize(path).map_err(|error| format!("无法找到作业文件：{error}"))?;
    if !canonical.is_file() || !is_json_path(path) {
        return Err("请选择作业 JSON 文件。".into());
    }
    Ok(canonical)
}

fn is_json_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|value| value.eq_ignore_ascii_case("json"))
}

fn companion() -> Result<PathBuf, String> {
    let executable =
        std::env::current_exe().map_err(|error| format!("无法定位 {APP_NAME}：{error}"))?;
    let directory = executable
        .parent()
        .ok_or_else(|| format!("无法定位 {APP_NAME} 的程序目录。"))?;
    #[cfg(windows)]
    let names = ["HarborSQL-Job.exe", "harbor-sql-job.exe"];
    #[cfg(not(windows))]
    let names = ["harbor-sql-job"];
    names
        .into_iter()
        .map(|name| directory.join(name))
        .find(|path| path.is_file())
        .ok_or_else(|| format!("程序目录中找不到 {APP_NAME} 作业程序。"))
}

fn edit_document(path: &Path) -> Result<(), String> {
    #[cfg(windows)]
    let mut editor = Command::new(
        PathBuf::from(std::env::var_os("SystemRoot").ok_or("无法定位 Windows 系统目录。")?)
            .join("System32/notepad.exe"),
    );
    #[cfg(target_os = "macos")]
    let mut editor = {
        let mut editor = Command::new("/usr/bin/open");
        editor.arg("-t");
        editor
    };
    #[cfg(not(any(windows, target_os = "macos")))]
    let mut editor = Command::new("/usr/bin/xdg-open");
    let child = editor
        .arg(path)
        .spawn()
        .map_err(|error| format!("无法打开 JSON 编辑器：{error}"))?;
    #[cfg(not(windows))]
    thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    #[cfg(windows)]
    drop(child);
    Ok(())
}

fn run(executable: &Path, operation: Operation, path: &Path) -> Result<Output, String> {
    let argument = match operation {
        Operation::Check => "check",
        Operation::Run => "run",
        _ => unreachable!("expected an independent job command"),
    };
    run_command(Command::new(executable).arg(argument).arg(path))
}

fn run_desktop_job(
    executable: &Path,
    path: &Path,
    on_odbc_started: impl FnOnce(ChildStdin),
) -> Result<Output, String> {
    if !is_odbc_job(path) {
        return run(executable, Operation::Run, path);
    }
    let mut command = Command::new(executable);
    command
        .arg(DESKTOP_ODBC_RUN)
        .arg(path)
        .stdin(Stdio::piped());
    capture_command(&mut command, |input| {
        on_odbc_started(input.expect("piped ODBC stop control"));
    })
}

fn run_command(command: &mut Command) -> Result<Output, String> {
    capture_command(command.stdin(Stdio::null()), |_| {})
}

fn capture_command(
    command: &mut Command,
    on_started: impl FnOnce(Option<ChildStdin>),
) -> Result<Output, String> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW.
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("无法启动进程：{error}"))?;
    on_started(child.stdin.take());
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_reader = thread::spawn(move || capture(stdout));
    let stderr = capture(stderr);
    let status = child
        .wait()
        .map_err(|error| format!("无法读取作业退出状态：{error}"))?;
    let (stdout, stdout_truncated) = stdout_reader
        .join()
        .map_err(|_| "无法读取作业标准输出。")?
        .map_err(|error| format!("无法读取作业标准输出：{error}"))?;
    let (stderr, stderr_truncated) =
        stderr.map_err(|error| format!("无法读取作业错误输出：{error}"))?;
    Ok(Output {
        code: status.code(),
        stdout,
        stderr,
        truncated: stdout_truncated || stderr_truncated,
    })
}

fn capture(mut pipe: impl Read) -> io::Result<(String, bool)> {
    let mut output = Vec::new();
    let mut truncated = false;
    let mut buffer = [0; 8192];
    loop {
        let read = match pipe.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if read == 0 {
            break;
        }
        let retained = read.min(OUTPUT_LIMIT - output.len());
        output.extend_from_slice(&buffer[..retained]);
        truncated |= retained < read;
    }
    Ok((String::from_utf8_lossy(&output).into_owned(), truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::ObjectKind;
    use harbor_sql::jobs::{
        backup_editor::{Backup, Definition},
        cli,
    };
    use std::io::Cursor;

    #[test]
    #[ignore = "需要 HARBOR_LEGACY_JOB_FIXTURES 指向随 SQLyog 安装的 sjaschemas 目录"]
    fn registers_jobs_from_the_installed_sqlyog_xml_examples() {
        use harbor_sql::file_source::FileSourceFormat;
        use harbor_sql::jobs::legacy_import::{
            Address, Endpoint, Kind, OdbcScope, OdbcSource, Request, Source, Transport,
        };

        let samples = PathBuf::from(std::env::var_os("HARBOR_LEGACY_JOB_FIXTURES").unwrap());
        let executable = std::env::current_exe().unwrap();
        let fixture = tempfile::Builder::new()
            .prefix("harbor-legacy-import-")
            .tempdir_in(executable.parent().unwrap())
            .unwrap();
        let catalog = fixture.path().join("job-manager.json");
        let csv = fixture.path().join("旧作业样例.csv");
        fs::write(&csv, "id,label\n1,样例\n").unwrap();
        let mut entries = Vec::new();
        let sync_endpoint = |transport: Transport, name: &str| Endpoint {
            password_env: name.into(),
            address: match transport {
                Transport::Direct => Address::Original,
                Transport::Http => Address::Http("https://migration.invalid/tunnel.php".into()),
                Transport::Tunnel => Address::Direct {
                    host: "127.0.0.1".into(),
                    port: 3306,
                },
            },
        };

        for (sample, job_type) in [
            ("exportjob-alltables.xml", "backup"),
            ("exportjob-seltables.xml", "backup"),
            ("exportjobs-alldbs.xml", "backup_databases"),
            ("datasync-alltables.xml", "data_sync"),
            ("datasync-seltables.xml", "data_sync"),
            ("notification.xml", "sql"),
            ("odbcimport-general.xml", "odbc_import"),
            ("odbcimport-dsnless.xml", "odbc_import"),
            ("odbcimport-withtrigger.xml", "odbc_import"),
            ("odbcimport-withwhere.xml", "odbc_import"),
        ] {
            let input = samples.join(sample);
            let original = fs::read(&input).unwrap();
            let source = Source::open(&input).unwrap_or_else(|error| panic!("{sample}: {error}"));
            let path = fixture
                .path()
                .join(input.with_extension("json").file_name().unwrap());
            let request = match source.kind() {
                Kind::Backup { directory } => Request::Backup {
                    output: if directory {
                        "逐表备份"
                    } else {
                        "数据库备份.sql"
                    }
                    .into(),
                    password_env: Some("HARBOR_XML_BACKUP_PASSWORD".into()),
                    copy_replication_lock: false,
                },
                Kind::DataSync {
                    source,
                    target,
                    continues_on_error,
                } => {
                    assert_eq!(continues_on_error, sample == "datasync-alltables.xml");
                    Request::DataSync {
                        source: sync_endpoint(source, "HARBOR_XML_SOURCE_PASSWORD"),
                        target: sync_endpoint(target, "HARBOR_XML_TARGET_PASSWORD"),
                        use_native_rules: false,
                    }
                }
                Kind::Notification => Request::Notification {
                    report: "通知报告.html".into(),
                    password_env: "HARBOR_XML_DATABASE_PASSWORD".into(),
                    smtp_user_env: "HARBOR_XML_SMTP_USER".into(),
                    smtp_password_env: "HARBOR_XML_SMTP_PASSWORD".into(),
                    smtp_tls: false,
                    smtp_port: 587,
                },
                Kind::Odbc {
                    scope: OdbcScope::MappedTables | OdbcScope::AllTables,
                    requires_connection_string,
                    source_file_format,
                } => Request::Odbc {
                    create: true,
                    reset: if sample == "odbcimport-general.xml" {
                        harbor_sql::database::OdbcReset::Recreate
                    } else {
                        harbor_sql::database::OdbcReset::None
                    },
                    use_legacy_table_strategies: false,
                    target_password_env: None,
                    source: if source_file_format == Some(FileSourceFormat::Csv) {
                        OdbcSource::CsvPath(csv.clone())
                    } else if requires_connection_string {
                        OdbcSource::ConnectionString("HARBOR_XML_ODBC_CONNECTION".into())
                    } else {
                        OdbcSource::Dsn {
                            user_env: Some("HARBOR_XML_ODBC_USER".into()),
                            password_env: Some("HARBOR_XML_ODBC_PASSWORD".into()),
                        }
                    },
                    source_catalog: None,
                    source_schema: None,
                    use_legacy_conflict: true,
                    copy_indexes: sample != "odbcimport-dsnless.xml",
                    copy_foreign_keys: sample == "odbcimport-general.xml",
                    preserve_source_actions: sample == "odbcimport-withtrigger.xml",
                },
                _ => panic!("unexpected job type for {sample}"),
            };
            let prepared = source
                .prepare(&path, &request)
                .unwrap_or_else(|error| panic!("{sample}: {error}"));
            assert!(!path.exists(), "preparing {sample} wrote a file");
            assert!(!prepared.notes().is_empty());
            let bytes = prepared.validated_bytes().unwrap();
            let definition: serde_json::Value = serde_json::from_slice(bytes).unwrap();
            assert_eq!(definition["job"]["type"], job_type);
            if job_type == "data_sync" {
                let job = &definition["job"];
                let continue_on_row_error = sample == "datasync-alltables.xml";
                assert_eq!(job["apply"], false);
                assert_eq!(job["legacy"]["abort_on_error"], !continue_on_row_error);
                if continue_on_row_error {
                    assert_eq!(job["continue_on_row_error"], true);
                } else {
                    assert!(job.get("continue_on_row_error").is_none());
                }
                let skip_target_fk_checks = sample == "datasync-seltables.xml";
                assert_eq!(job["legacy"]["fkcheck"], skip_target_fk_checks);
                if skip_target_fk_checks {
                    assert_eq!(job["skip_target_fk_checks"], true);
                } else {
                    assert!(job.get("skip_target_fk_checks").is_none());
                }
            }
            if let Kind::DataSync {
                source: from,
                target: to,
                continues_on_error: true,
            } = source.kind()
            {
                let overridden = source
                    .prepare(
                        &path,
                        &Request::DataSync {
                            source: sync_endpoint(from, "HARBOR_XML_SOURCE_PASSWORD"),
                            target: sync_endpoint(to, "HARBOR_XML_TARGET_PASSWORD"),
                            use_native_rules: true,
                        },
                    )
                    .unwrap();
                let overridden: serde_json::Value =
                    serde_json::from_slice(overridden.validated_bytes().unwrap()).unwrap();
                assert_eq!(overridden["job"]["legacy"]["abort_on_error"], false);
                assert!(overridden["job"].get("continue_on_row_error").is_none());
            }
            if sample == "odbcimport-general.xml" {
                assert_eq!(definition["job"]["reset"], "recreate");
                assert!(
                    definition["job"]["tables"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|table| {
                            table["source"]["copy_foreign_keys"] == true
                                && table["source"]["indexes_from"]["table"].is_string()
                        })
                );
            }
            if sample == "odbcimport-withtrigger.xml" {
                let change = &definition["job"]["tables"][0]["source"]["after_import"]["change"];
                assert_eq!(change["kind"], "update");
                assert_eq!(change["assignments"][0]["column"], "col1");
                assert_eq!(change["assignments"][0]["expression"], "'1'");
            }
            if sample == "odbcimport-dsnless.xml" {
                let job = &definition["job"];
                let source = &job["discovery"]["source"];
                assert_eq!(source["file_source"]["format"], "csv");
                let saved = PathBuf::from(source["file_source"]["path"].as_str().unwrap());
                assert_eq!(saved.canonicalize().unwrap(), csv.canonicalize().unwrap());
                assert!(source["connection_env"].is_null());
                assert!(job.get("tables").is_none());
                assert_eq!(job["discovery"]["copy_indexes"], false);
                assert_eq!(job["discovery"]["copy_foreign_keys"], false);
            }
            entries = create_job(
                &catalog,
                &entries,
                Entry {
                    name: job_name(&path),
                    path: path.clone(),
                },
                bytes,
            )
            .unwrap();
            assert_eq!(cli(["check".into(), path.into_os_string()]), 0, "{sample}");
            assert!(
                original == fs::read(input).unwrap(),
                "changed old XML: {sample}"
            );
        }
        assert_eq!(entries.len(), 10);
        assert!(load(&catalog).unwrap() == entries);
        assert!(matches!(
            Source::open(&samples.join("odbcimport-dsnless.xml"))
                .unwrap()
                .kind(),
            Kind::Odbc {
                requires_connection_string: true,
                scope: OdbcScope::AllTables,
                source_file_format: Some(FileSourceFormat::Csv),
            },
        ));
    }

    #[test]
    fn creates_and_edits_a_registered_backup_definition() {
        let executable = std::env::current_exe().unwrap();
        let fixture = tempfile::Builder::new()
            .prefix("harbor-backup-editor-")
            .tempdir_in(executable.parent().unwrap())
            .unwrap();
        let catalog = fixture.path().join("job-manager.json");
        let path = fixture.path().join("inventory-backup.json");
        let entry = Entry {
            name: job_name(&path),
            path: path.clone(),
        };
        let mut definition = Definition::new(
            "inventory".into(),
            Some(DbObject {
                schema: "inventory".into(),
                name: "stock".into(),
                kind: ObjectKind::Table,
            }),
        );
        definition.connection.password_env = Some("HARBOR_TEST_DB_PASSWORD".into());
        let bytes = definition.validated_bytes(&path).unwrap();
        let entries = create_job(&catalog, &[], entry, &bytes).unwrap();
        let mut definition = Definition::read(&path).unwrap().unwrap();
        assert_eq!(
            definition.connection.password_env.as_deref(),
            Some("HARBOR_TEST_DB_PASSWORD")
        );
        let Backup::Backup(job) = &mut definition.backup else {
            panic!("expected single-schema backup")
        };
        assert_eq!(job.objects.as_ref().unwrap()[0].name, "stock");
        job.output = "inventory_{sja_timestamp}.sql".into();
        definition.log = Some("inventory.jsonl".into());
        update_job(&path, |target| {
            definition.verify_unchanged(target)?;
            definition.validated_bytes(target)
        })
        .unwrap();
        assert!(load(&catalog).unwrap() == entries);
        assert_eq!(cli(["check".into(), path.clone().into_os_string()]), 0);
        let definition = Definition::read(&path).unwrap().unwrap();
        assert_eq!(
            definition.log.as_deref(),
            Some(Path::new("inventory.jsonl"))
        );
    }

    #[test]
    fn manages_synthetic_catalog_without_overwriting_concurrent_changes() {
        let executable = std::env::current_exe().unwrap();
        let fixture = tempfile::Builder::new()
            .prefix("harbor-job-manager-")
            .tempdir_in(executable.parent().unwrap())
            .unwrap();
        let path = fixture.path().join("job-manager.json");
        let entry = Entry {
            name: "物流".into(),
            path: fixture.path().join("sample.json"),
        };
        assert!(load(&path).unwrap().is_empty());
        save(&path, &[], std::slice::from_ref(&entry)).unwrap();
        let mut entries = load(&path).unwrap();
        assert_eq!(entries[0].name, entry.name);
        entries[0].name = "新名称".into();
        save(&path, &[entry], &entries).unwrap();
        assert!(save(&path, &[], &[]).is_err());
        assert_eq!(load(&path).unwrap()[0].name, "新名称");
        save(&path, &entries, &[]).unwrap();
        assert!(load(&path).unwrap().is_empty());
    }

    #[test]
    fn drains_output_after_reaching_capture_limit() {
        let bytes = vec![b'a'; OUTPUT_LIMIT * 2];
        let mut pipe = Cursor::new(&bytes);
        let (output, truncated) = capture(&mut pipe).unwrap();
        assert_eq!(output.len(), OUTPUT_LIMIT);
        assert_eq!(pipe.position(), bytes.len() as u64);
        assert!(truncated);
    }
}
