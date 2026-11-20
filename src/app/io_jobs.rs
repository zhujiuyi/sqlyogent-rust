pub(super) mod export;
mod restore_dialog;
mod schema_html;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use eframe::egui::{self, RichText, collapsing_header::CollapsingState};
use rfd::{FileDialog, MessageButtons, MessageDialog, MessageDialogResult};

use crate::database::{
    Command, DbObject, ObjectKind,
    transfer::{DumpSettings, MAX_BULK_INSERT_KIB, MAX_HTTP_DUMP_CHUNK_ROWS, TransferSummary},
};

use super::{
    kind_label,
    preferences::{DumpCheckboxes, DumpPreferences},
    state::Workspace,
    style,
};

pub(super) fn form_width(ctx: &egui::Context, preferred: f32) -> f32 {
    (ctx.content_rect().width() - 64.0).clamp(1.0, preferred)
}

pub(super) fn form_scroll(ctx: &egui::Context, reserved_height: f32) -> egui::ScrollArea {
    egui::ScrollArea::vertical()
        .auto_shrink([false, true])
        .max_height((ctx.content_rect().height() - reserved_height).max(64.0))
}

pub(super) fn right_aligned_row<R>(
    ui: &mut egui::Ui,
    content: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    // Keep horizontal centering within a single row, even inside a vertical scroll area.
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), ui.spacing().interact_size.y),
        egui::Layout::right_to_left(egui::Align::Center),
        content,
    )
    .inner
}

pub(super) fn form_section<R>(
    ui: &mut egui::Ui,
    title: &str,
    content: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let result = style::section_card(ui, title, content);
    ui.add_space(6.0);
    result
}

fn transfer_progress(ui: &mut egui::Ui, status: &str, fraction: f32, color: egui::Color32) {
    right_aligned_row(ui, |ui| {
        ui.label(RichText::new(format!("{}%", (fraction * 100.0) as u32)).color(style::muted()));
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.add(egui::Label::new(RichText::new(status).color(color)).truncate())
                .on_hover_text(status);
        });
    });
    ui.scope(|ui| {
        ui.visuals_mut().extreme_bg_color = style::border();
        let fill = if fraction == 0.0 {
            style::border()
        } else if color == style::ink() || color == style::muted() {
            style::accent()
        } else {
            color
        };
        ui.add(
            egui::ProgressBar::new(fraction)
                .desired_height(6.0)
                .fill(fill),
        );
    });
}

pub(super) enum Job {
    Backup {
        schema: String,
        object: Option<DbObject>,
        defaults: DumpPreferences,
    },
    Restore(String),
    Export(DbObject),
    SchemaHtml(String),
}

pub(super) struct Form {
    workspace: usize,
    kind: FormKind,
}

enum FormKind {
    Backup(BackupForm),
    Restore(restore_dialog::RestoreForm),
    Export(export::TableForm),
    SchemaHtml(schema_html::Form),
}

impl Form {
    pub fn new(workspace: usize, job: Job) -> Self {
        let kind = match job {
            Job::Backup {
                schema,
                object,
                defaults,
            } => FormKind::Backup(BackupForm::new(schema, object, defaults)),
            Job::Restore(schema) => FormKind::Restore(restore_dialog::RestoreForm::new(schema)),
            Job::Export(object) => FormKind::Export(export::TableForm::new(object)),
            Job::SchemaHtml(schema) => FormKind::SchemaHtml(schema_html::Form::new(schema)),
        };
        Self { workspace, kind }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum DumpMode {
    Structure,
    Data,
    StructureAndData,
}

impl DumpMode {
    fn includes(self, object: &DbObject) -> bool {
        self != Self::Data || object.kind == ObjectKind::Table
    }
}

enum DumpRun {
    Ready,
    Running { cancelling: bool },
    Finished(Result<TransferSummary, String>),
}

struct BackupForm {
    schema: String,
    initial_kind: ObjectKind,
    selections: BTreeMap<String, HashSet<DbObject>>,
    catalog_request: Option<String>,
    catalog_errors: BTreeMap<String, String>,
    file: String,
    picked_file: Option<PathBuf>,
    overwrite_confirmed: bool,
    mode: DumpMode,
    settings: DumpSettings,
    limits: DumpPreferences,
    remembered: DumpCheckboxes,
    preferences_error: Option<String>,
    run: DumpRun,
}

pub(super) const OBJECT_KINDS: [ObjectKind; 6] = [
    ObjectKind::Table,
    ObjectKind::View,
    ObjectKind::Procedure,
    ObjectKind::Function,
    ObjectKind::Trigger,
    ObjectKind::Event,
];

impl BackupForm {
    fn new(schema: String, object: Option<DbObject>, defaults: DumpPreferences) -> Self {
        let (remembered, preferences_error) = match DumpCheckboxes::load() {
            Ok(choices) => (choices, None),
            Err(error) => (
                DumpCheckboxes::default(),
                Some(format!("无法读取上次的转储选项。{error}")),
            ),
        };
        let initial_kind = object
            .as_ref()
            .map_or(ObjectKind::Table, |object| object.kind);
        let mut selections = BTreeMap::new();
        if let Some(object) = object {
            selections.insert(schema.clone(), HashSet::from([object]));
        }
        Self {
            schema,
            initial_kind,
            selections,
            catalog_request: None,
            catalog_errors: BTreeMap::new(),
            file: String::new(),
            picked_file: None,
            overwrite_confirmed: false,
            mode: if initial_kind == ObjectKind::Table {
                DumpMode::StructureAndData
            } else {
                DumpMode::Structure
            },
            settings: remembered.into(),
            limits: defaults,
            remembered,
            preferences_error,
            run: DumpRun::Ready,
        }
    }

    fn update_catalog(&mut self, ws: &mut Workspace) {
        if ws.busy() {
            return;
        }
        if let Some(schema) = self.catalog_request.take()
            && !ws.objects.contains_key(&schema)
        {
            self.catalog_errors.insert(schema, ws.status.clone());
        }
        if self.schema.is_empty() {
            self.schema = ws
                .server
                .as_ref()
                .and_then(|server| server.database.as_ref())
                .or_else(|| ws.schemas.first())
                .cloned()
                .unwrap_or_default();
        }
        if ws.server.is_some()
            && !self.schema.is_empty()
            && !ws.objects.contains_key(&self.schema)
            && !self.catalog_errors.contains_key(&self.schema)
        {
            let schema = self.schema.clone();
            let command = Command::ListObjects {
                schema: schema.clone(),
            };
            if ws.send(command, "正在读取导出对象…") {
                self.catalog_request = Some(schema);
            } else {
                self.catalog_errors.insert(schema, ws.status.clone());
            }
        }
    }

    fn selected_objects<'a>(&self, ws: &'a Workspace) -> impl Iterator<Item = &'a DbObject> {
        let selected = self.selections.get(&self.schema);
        ws.objects
            .get(&self.schema)
            .into_iter()
            .flatten()
            .filter(move |object| {
                self.mode.includes(object) && selected.is_some_and(|set| set.contains(*object))
            })
    }

    fn destination(&self) -> Option<PathBuf> {
        self.picked_file
            .clone()
            .or_else(|| (!self.file.trim().is_empty()).then(|| PathBuf::from(&self.file)))
    }

    fn pick_destination(&mut self, ws: &Workspace) {
        if self.settings.file_per_object {
            let mut dialog = FileDialog::new().set_title("选择分文件备份目录");
            if let Some(path) = self.destination() {
                if path.is_dir() {
                    dialog = dialog.set_directory(path);
                } else if let Some(parent) = path.parent().filter(|parent| parent.is_dir()) {
                    dialog = dialog.set_directory(parent);
                }
            }
            if let Some(path) = dialog.pick_folder() {
                self.file = path.display().to_string();
                self.overwrite_confirmed = false;
                self.picked_file = Some(path);
            }
            return;
        }
        let target = {
            let mut objects = self.selected_objects(ws);
            let first = objects.next();
            if objects.next().is_none() {
                target_name(&self.schema, first)
            } else {
                self.schema.clone()
            }
        };
        let mut dialog = FileDialog::new()
            .set_title("保存 SQL 转储文件")
            .add_filter("SQL 转储文件", &["sql"])
            .set_file_name(format!("备份_{}.sql", file_stem(&target)));
        if let Some(path) = self.destination() {
            if let Some(directory) = path.parent().filter(|parent| parent.is_dir()) {
                dialog = dialog.set_directory(directory);
            }
            if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                dialog = dialog.set_file_name(name);
            }
        }
        if let Some(path) = dialog.save_file() {
            self.file = path.display().to_string();
            self.overwrite_confirmed = existing_file(&path);
            self.picked_file = Some(path);
        }
    }

    fn source_fields(&mut self, ui: &mut egui::Ui, ws: &Workspace) {
        let current = if self.schema.is_empty() {
            "选择数据库"
        } else {
            &self.schema
        };
        egui::ComboBox::from_id_salt("dump-schema")
            .width(ui.available_width().min(280.0))
            .truncate()
            .selected_text(current)
            .show_ui(ui, |ui| {
                for schema in &ws.schemas {
                    if ui
                        .selectable_value(&mut self.schema, schema.clone(), schema)
                        .changed()
                    {
                        self.overwrite_confirmed = false;
                        ui.ctx().request_repaint();
                    }
                }
            });
        ui.horizontal_wrapped(|ui| {
            ui.visuals_mut().selection.bg_fill = style::accent_soft();
            ui.selectable_value(&mut self.mode, DumpMode::Structure, "仅结构");
            ui.selectable_value(&mut self.mode, DumpMode::Data, "仅数据");
            ui.selectable_value(&mut self.mode, DumpMode::StructureAndData, "结构和数据");
        });
    }

    fn destination_fields(&mut self, ui: &mut egui::Ui, ws: &Workspace) {
        if ui
            .checkbox(&mut self.settings.file_per_object, "按对象分文件")
            .on_hover_text("每次导出在目标目录下创建独立的备份目录，并保存还原顺序。")
            .changed()
        {
            self.overwrite_confirmed = false;
        }
        let per_object = self.settings.file_per_object;
        right_aligned_row(ui, |ui| {
            if ui
                .button(if per_object {
                    "选择目录…"
                } else {
                    "选择文件…"
                })
                .clicked()
            {
                self.pick_destination(ws);
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut self.file)
                            .desired_width(f32::INFINITY)
                            .hint_text(if per_object {
                                "输入目录路径"
                            } else {
                                "输入 SQL 文件路径"
                            }),
                    )
                    .on_hover_text(&self.file)
                    .changed()
                {
                    self.picked_file = None;
                    self.overwrite_confirmed = false;
                }
            });
        });
    }

    fn object_tree(&mut self, ui: &mut egui::Ui, ws: &Workspace, height: f32) {
        form_section(ui, "导出对象", |ui| {
            let Some(available) = ws.objects.get(&self.schema) else {
                if ws.server.is_none() {
                    ui.colored_label(style::muted(), "请先连接数据库。");
                } else if self.schema.is_empty() {
                    ui.colored_label(style::muted(), "请选择数据库。");
                } else if let Some(error) = self.catalog_errors.get(&self.schema) {
                    ui.colored_label(style::error(), error);
                    if ui.button("重新读取").clicked() {
                        self.catalog_errors.remove(&self.schema);
                        ui.ctx().request_repaint();
                    }
                } else if ws.busy() && self.catalog_request.as_deref() != Some(self.schema.as_str())
                {
                    ui.colored_label(style::muted(), "等待当前连接任务完成…");
                } else {
                    ui.colored_label(style::muted(), "正在读取导出对象…");
                }
                return;
            };

            let selected = self
                .selections
                .entry(self.schema.clone())
                .or_insert_with(|| available.iter().cloned().collect());
            let eligible = available.iter().filter(|object| self.mode.includes(object));
            let total = eligible.clone().count();
            let chosen = eligible.filter(|object| selected.contains(*object)).count();
            ui.label(
                RichText::new(format!("已选 {chosen} / {total}"))
                    .small()
                    .color(style::muted()),
            );
            ui.separator();
            egui::ScrollArea::vertical()
                .id_salt(("dump-tree", ws.key, &self.schema))
                .auto_shrink([false, false])
                .max_height(height)
                .show(ui, |ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    for kind in OBJECT_KINDS {
                        let objects: Vec<_> = available
                            .iter()
                            .filter(|object| object.kind == kind)
                            .collect();
                        let eligible = self.mode != DumpMode::Data || kind == ObjectKind::Table;
                        object_checkbox_group(
                            ui,
                            kind,
                            &objects,
                            selected,
                            eligible,
                            kind == self.initial_kind,
                        );
                    }
                });
        });
    }

    fn source_options(&mut self, ui: &mut egui::Ui, http_tunnel: bool) {
        let has_data = self.mode != DumpMode::Structure;
        let (chunk_label, chunk_help, max_chunk_rows) = if http_tunnel {
            (
                "指定每批行数",
                "按指定行数发出多次 SELECT。响应超过隧道大小上限时会自动缩小批次。",
                MAX_HTTP_DUMP_CHUNK_ROWS,
            )
        } else {
            (
                "分批读取源表",
                "按指定行数发出多次 SELECT。使用单事务快照或源表读锁可避免源数据变化影响分批结果。",
                u32::MAX,
            )
        };
        ui.horizontal_wrapped(|ui| {
            dump_option(ui, has_data, &mut self.limits.chunked, chunk_label)
                .on_hover_text(chunk_help)
                .on_disabled_hover_text("导出数据时可用。");
            if self.limits.chunked {
                ui.add_enabled(
                    has_data,
                    egui::DragValue::new(&mut self.limits.chunk_rows)
                        .range(1..=max_chunk_rows)
                        .suffix(" 行/批"),
                );
            }
        });
        if http_tunnel {
            ui.colored_label(
                style::muted(),
                format!("HTTP 隧道每批最多读取 {MAX_HTTP_DUMP_CHUNK_ROWS} 行。备份期间请保持源表数据和结构稳定。"),
            );
            ui.checkbox(&mut self.settings.flush_logs, "读取前刷新日志")
                .on_hover_text("执行 FLUSH LOGS。");
            return;
        }
        if has_data {
            ui.add_enabled(
                !self.settings.single_transaction,
                egui::Checkbox::new(&mut self.settings.lock_source_tables, "读取数据时锁表"),
            )
            .on_disabled_hover_text("先关闭单事务一致性快照。")
            .on_hover_text("读取每张表的数据时持有该表的读锁。");
        } else {
            ui.add_enabled(false, egui::Checkbox::new(&mut false, "读取数据时锁表"))
                .on_disabled_hover_text("导出数据时可用。");
        }
        ui.checkbox(&mut self.settings.flush_logs, "读取前刷新日志")
            .on_hover_text("执行 FLUSH LOGS。");
        let snapshot = ui
            .add_enabled(
                !has_data || !self.settings.lock_source_tables,
                egui::Checkbox::new(&mut self.settings.single_transaction, "单事务一致性快照"),
            )
            .on_disabled_hover_text("先关闭读取数据时锁表。")
            .on_hover_text("InnoDB 表使用同一事务快照。");
        if snapshot.changed() && self.settings.single_transaction {
            self.settings.lock_source_tables = false;
        }
    }

    fn sql_options(&mut self, ui: &mut egui::Ui) {
        ui.checkbox(&mut self.settings.include_use, "写入 USE")
            .on_hover_text("执行该文件时切换到与源库同名的数据库。");
        dump_option(
            ui,
            self.mode != DumpMode::Data,
            &mut self.settings.include_create_database,
            "写入 CREATE DATABASE",
        )
        .on_hover_text("还原时创建与源库同名的数据库，已有同名库时跳过创建。")
        .on_disabled_hover_text("导出结构时可用。");
        dump_option(
            ui,
            self.mode != DumpMode::Data,
            &mut self.settings.options.drop_existing,
            "写入 DROP IF EXISTS",
        );
        ui.checkbox(
            &mut self.settings.disable_foreign_key_checks,
            "还原时关闭外键检查",
        );
        ui.separator();
        let has_data = self.mode != DumpMode::Structure;
        ui.add_enabled_ui(has_data, |ui| {
            let options = [
                (None, "自动管理事务"),
                (Some(true), "关闭自动提交"),
                (Some(false), "沿用还原会话"),
            ];
            let selected = match self.settings.disable_autocommit {
                None => options[0].1,
                Some(true) => options[1].1,
                Some(false) => options[2].1,
            };
            egui::ComboBox::from_id_salt("dump-restore-autocommit")
                .width(ui.available_width())
                .truncate()
                .selected_text(selected)
                .show_ui(ui, |ui| {
                    for (value, label) in options {
                        ui.selectable_value(&mut self.settings.disable_autocommit, value, label);
                    }
                })
                .response
                .on_hover_text("自动管理事务和关闭自动提交都会按表提交数据。沿用还原会话时，由目标会话的设置决定何时提交。");
        });
        dump_option(
            ui,
            has_data,
            &mut self.settings.lock_insert_tables,
            "还原时在 INSERT 前锁表",
        );
        dump_option(
            ui,
            has_data,
            &mut self.settings.complete_insert,
            "INSERT 写入列名",
        )
        .on_hover_text("省略列名时按字段顺序写入。含生成列或不可见列的表始终写明列名。");
        dump_option(
            ui,
            has_data,
            &mut self.settings.disable_keys,
            "还原时暂缓更新索引",
        )
        .on_hover_text("写入期间暂停更新 MyISAM 和 Aria 的非唯一索引，写完后重建。");
        dump_option(
            ui,
            has_data,
            &mut self.settings.bulk_insert,
            "合并多行 INSERT",
        );
        ui.indent("dump-bulk-options", |ui| {
            ui.add_enabled_ui(has_data && self.settings.bulk_insert, |ui| {
                ui.checkbox(&mut self.limits.use_source_packet_limit, "按源库通信包大小")
                    .on_hover_text("采用源库的 max_allowed_packet。");
                if !self.limits.use_source_packet_limit {
                    ui.horizontal_wrapped(|ui| {
                        ui.label("每条上限");
                        ui.add(
                            egui::DragValue::new(&mut self.limits.bulk_insert_kib)
                                .range(1..=MAX_BULK_INSERT_KIB)
                                .suffix(" KiB"),
                        )
                        .on_hover_text(
                            "也受源库通信包大小限制。超过上限的单行单独写入一条 INSERT。",
                        );
                    });
                }
            });
        });
    }

    fn show_progress(&self, ui: &mut egui::Ui, ws: &Workspace) {
        let progress = ws.dump_progress.as_ref();
        let fraction = progress
            .filter(|item| item.total_objects > 0)
            .map_or(0.0, |item| {
                (item.completed_objects as f32 / item.total_objects as f32).min(0.99)
            });
        let (fraction, status, color) = match &self.run {
            DumpRun::Ready if ws.busy() && self.catalog_request.is_none() => {
                (0.0, "当前连接正在执行其他任务。".into(), style::muted())
            }
            DumpRun::Ready => (0.0, "等待导出".into(), style::muted()),
            DumpRun::Running { cancelling } => {
                let status = if *cancelling {
                    "正在取消导出…".into()
                } else if let Some(item) = progress {
                    if item.finishing {
                        "正在保存 SQL 文件…".into()
                    } else if let Some(object) = &item.object {
                        format!(
                            "正在导出 {object}，已完成 {}/{} 个对象，{} 行",
                            item.completed_objects, item.total_objects, item.rows
                        )
                    } else {
                        format!(
                            "已完成 {}/{} 个对象，{} 行",
                            item.completed_objects, item.total_objects, item.rows
                        )
                    }
                } else {
                    "正在准备导出…".into()
                };
                (fraction, status, style::ink())
            }
            DumpRun::Finished(Ok(summary)) => (
                1.0,
                {
                    let mut status = format!(
                        "上次导出完成，共 {} 个对象、{} 行、{} 字节。",
                        summary.objects, summary.rows, summary.bytes
                    );
                    if let Some(path) = &summary.output_directory {
                        status.push_str(&format!("输出目录为 {}。", path.display()));
                    }
                    status
                },
                style::success(),
            ),
            DumpRun::Finished(Err(error)) if error == "已取消 SQL 转储。" => {
                (fraction, error.clone(), style::muted())
            }
            DumpRun::Finished(Err(error)) => (fraction, format!("导出失败。{error}"), style::error()),
        };
        transfer_progress(ui, &status, fraction, color);
    }

    fn start(&mut self, ws: &mut Workspace, path: PathBuf) {
        let existing = !self.settings.file_per_object && existing_file(&path);
        let approved_in_picker =
            self.overwrite_confirmed && self.picked_file.as_ref() == Some(&path);
        if existing && !approved_in_picker && !confirm_overwrite(&path) {
            return;
        }
        let mut settings = self.settings.clone();
        settings.options.include_structure = self.mode != DumpMode::Data;
        settings.options.include_data = self.mode != DumpMode::Structure;
        settings.options.drop_existing &= settings.options.include_structure;
        settings.include_create_database &= settings.options.include_structure;
        let http_tunnel = ws.server.as_ref().is_some_and(|server| server.http_tunnel);
        settings.lock_source_tables &= settings.options.include_data && !http_tunnel;
        settings.single_transaction &= !http_tunnel;
        settings.lock_insert_tables &= settings.options.include_data;
        settings.bulk_insert &= settings.options.include_data;
        settings.bulk_insert_limit_kib = self.limits.bulk_insert_limit_kib();
        settings.chunk_rows = self.limits.chunk_rows();
        settings.objects = Some(self.selected_objects(ws).cloned().collect());
        settings.overwrite = existing;
        let command = Command::DumpConfigured {
            schema: self.schema.clone(),
            path,
            settings,
        };
        self.overwrite_confirmed = false;
        if ws.send(command, "正在生成 SQL 转储文件…") {
            self.run = DumpRun::Running { cancelling: false };
        } else {
            self.run = DumpRun::Finished(Err(ws.status.clone()));
        }
    }

    fn dialog(&mut self, ctx: &egui::Context, ws: &mut Workspace) -> bool {
        if matches!(self.run, DumpRun::Running { .. })
            && let Some(result) = &ws.dump_result
        {
            self.run = DumpRun::Finished(result.clone());
        }
        let running = matches!(self.run, DumpRun::Running { .. });
        let completed = matches!(self.run, DumpRun::Finished(Ok(_)));
        if !running {
            self.update_catalog(ws);
        }
        let width = form_width(ctx, 890.0);
        let tree_height = ((ctx.content_rect().height() - 260.0) * 0.6).clamp(136.0, 300.0);
        // Leave room for the footer when the first visible frame uses the prior area size.
        let reserved_height = if self.preferences_error.is_some() {
            204.0
        } else {
            180.0
        };
        let http_tunnel = ws.server.as_ref().is_some_and(|server| server.http_tunnel);
        let mut close = false;
        let mut start_path = None;
        let modal = style::modal(ctx, egui::Id::new("dump-form")).show(ctx, |ui| {
            ui.set_width(width);
            style::page_title(ui, "备份为 SQL 转储文件");
            ui.add_space(4.0);
            form_scroll(ctx, reserved_height).show(ui, |ui| {
                ui.add_enabled_ui(!running, |ui| {
                    let source = |ui: &mut egui::Ui, form: &mut Self| {
                        form_section(ui, "源数据库", |ui| form.source_fields(ui, ws));
                    };
                    let destination = |ui: &mut egui::Ui, form: &mut Self| {
                        form_section(ui, "保存位置", |ui| form.destination_fields(ui, ws));
                    };
                    if ui.available_width() >= 590.0 {
                        ui.columns(2, |columns| {
                            source(&mut columns[0], self);
                            destination(&mut columns[1], self);
                        });
                    } else {
                        source(ui, self);
                        destination(ui, self);
                    }
                    let read_options = |ui: &mut egui::Ui, form: &mut Self| {
                        form_section(ui, "读取源数据库", |ui| {
                            form.source_options(ui, http_tunnel)
                        });
                    };
                    let write_options = |ui: &mut egui::Ui, form: &mut Self| {
                        form_section(ui, "SQL 语句", |ui| form.sql_options(ui));
                    };
                    if ui.available_width() >= 720.0 {
                        ui.columns(3, |columns| {
                            self.object_tree(&mut columns[0], ws, tree_height);
                            read_options(&mut columns[1], self);
                            write_options(&mut columns[2], self);
                        });
                    } else {
                        self.object_tree(ui, ws, tree_height);
                        read_options(ui, self);
                        write_options(ui, self);
                    }
                });
            });
            style::pane(egui::Margin::symmetric(12, 8))
                .corner_radius(egui::CornerRadius::same(10))
                .show(ui, |ui| {
                    self.show_progress(ui, ws);
                    if let Some(error) = &self.preferences_error {
                        ui.add(
                            egui::Label::new(RichText::new(error).color(style::error())).truncate(),
                        )
                        .on_hover_text(error);
                    }
                    right_aligned_row(ui, |ui| {
                        if let DumpRun::Running { cancelling } = &mut self.run {
                            let cancellable = !*cancelling
                                && ws.dump_progress.as_ref().is_none_or(|item| !item.finishing);
                            if ui
                                .add_enabled(cancellable, egui::Button::new("取消导出"))
                                .clicked()
                                && ws.cancel_dump()
                            {
                                *cancelling = true;
                            }
                        } else {
                            if completed {
                                close = ui.add(style::primary("完成")).clicked();
                            }
                            let path = self.destination();
                            let can_start = path.is_some()
                                && ws.server.is_some()
                                && !ws.busy()
                                && ws.objects.contains_key(&self.schema)
                                && ((self.mode != DumpMode::Data
                                    && self.settings.include_create_database)
                                    || self.selected_objects(ws).next().is_some());
                            let export = if completed {
                                egui::Button::new("导出")
                            } else {
                                style::primary_enabled(can_start, "导出")
                            };
                            if ui.add_enabled(can_start, export).clicked() {
                                start_path = path;
                            }
                        }
                        if !completed {
                            close = ui
                                .add_enabled(!running, egui::Button::new("关闭"))
                                .clicked();
                        }
                    });
                });
        });

        close |= !running && modal.should_close();
        if close {
            let choices = DumpCheckboxes::from(&self.settings);
            if choices != self.remembered
                && let Err(error) = choices.save()
            {
                ws.message(format!("无法保存本次的转储选项。{error}"), true);
            }
        } else if let Some(path) = start_path {
            self.start(ws, path);
        }
        close
    }
}

fn dump_option(
    ui: &mut egui::Ui,
    available: bool,
    checked: &mut bool,
    label: &str,
) -> egui::Response {
    if available {
        ui.checkbox(checked, label)
    } else {
        ui.add_enabled(false, egui::Checkbox::new(&mut false, label))
    }
}

pub(super) fn object_checkbox_group(
    ui: &mut egui::Ui,
    kind: ObjectKind,
    objects: &[&DbObject],
    selected: &mut HashSet<DbObject>,
    eligible: bool,
    default_open: bool,
) {
    let count = if eligible {
        objects
            .iter()
            .copied()
            .filter(|object| selected.contains(*object))
            .count()
    } else {
        0
    };
    let mut all = !objects.is_empty() && count == objects.len();
    let label = format!("{}（{count}/{}）", kind_label(kind), objects.len());
    let id = ui.make_persistent_id(kind);
    CollapsingState::load_with_default_open(ui.ctx(), id, default_open)
        .show_header(ui, |ui| {
            let checkbox = egui::Checkbox::new(&mut all, label)
                .indeterminate(count > 0 && count < objects.len());
            if ui
                .add_enabled(eligible && !objects.is_empty(), checkbox)
                .changed()
            {
                for &object in objects {
                    if all {
                        selected.insert(object.clone());
                    } else {
                        selected.remove(object);
                    }
                }
            }
        })
        .body(|ui| {
            for &object in objects {
                let mut checked = eligible && selected.contains(object);
                let checkbox = egui::Checkbox::new(&mut checked, &object.name);
                if ui
                    .add_enabled(eligible, checkbox)
                    .on_hover_text(&object.name)
                    .changed()
                {
                    if checked {
                        selected.insert(object.clone());
                    } else {
                        selected.remove(object);
                    }
                }
            }
        });
}

fn confirm_overwrite(path: &Path) -> bool {
    MessageDialog::new()
        .set_title("覆盖 SQL 转储文件")
        .set_description(format!("以下文件已存在。是否覆盖？\n{}", path.display()))
        .set_buttons(MessageButtons::YesNo)
        .show()
        == MessageDialogResult::Yes
}

fn existing_file(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|metadata| !metadata.file_type().is_dir())
}

fn file_stem(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
            {
                '_'
            } else {
                ch
            }
        })
        .collect::<String>()
        .trim_matches(['.', ' '])
        .to_owned()
}

pub(super) fn dialog(ctx: &egui::Context, state: &mut Option<Form>, workspaces: &mut [Workspace]) {
    let Some(form) = state else { return };
    let Some(ws) = workspaces.get_mut(form.workspace) else {
        *state = None;
        return;
    };
    let close = match &mut form.kind {
        FormKind::Backup(backup) => backup.dialog(ctx, ws),
        FormKind::Restore(restore) => restore.dialog(ctx, ws),
        FormKind::Export(export) => export.dialog(ctx, ws),
        FormKind::SchemaHtml(schema_html) => schema_html.dialog(ctx, ws),
    };
    if close {
        *state = None;
    }
}

fn target_name(schema: &str, object: Option<&DbObject>) -> String {
    object.map_or_else(
        || schema.to_owned(),
        |object| format!("{schema}.{}", object.name),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::preferences::Preferences;
    use crate::database::ServerInfo;

    fn text_bounds(shape: &egui::Shape, label: &str) -> Option<egui::Rect> {
        match shape {
            egui::Shape::Text(text) if text.galley.text() == label => {
                Some(egui::Rect::from_min_size(text.pos, text.galley.size()))
            }
            egui::Shape::Vec(shapes) => shapes.iter().find_map(|shape| text_bounds(shape, label)),
            _ => None,
        }
    }

    #[test]
    fn dump_dialog_keeps_title_and_actions_in_the_viewport() {
        let schema = "备份库".to_owned();
        let mut workspace = Workspace::new("备份测试".into(), None);
        workspace.server = Some(ServerInfo {
            version: "11.4.13-MariaDB".into(),
            connection_id: 1,
            database: Some(schema.clone()),
            http_tunnel: false,
        });
        workspace.schemas.push(schema.clone());
        workspace.objects.insert(
            schema.clone(),
            ["商品", "订单"]
                .into_iter()
                .map(|name| DbObject {
                    schema: schema.clone(),
                    name: name.into(),
                    kind: ObjectKind::Table,
                })
                .collect(),
        );
        let remembered = DumpCheckboxes::default();
        let mut form = BackupForm {
            schema,
            initial_kind: ObjectKind::Table,
            selections: BTreeMap::new(),
            catalog_request: None,
            catalog_errors: BTreeMap::new(),
            file: "backup.sql".into(),
            picked_file: None,
            overwrite_confirmed: false,
            mode: DumpMode::StructureAndData,
            settings: remembered.into(),
            limits: DumpPreferences::default(),
            remembered,
            preferences_error: None,
            run: DumpRun::Ready,
        };

        let minimum_window = egui::vec2(850.0, 560.0);
        for size in [
            egui::vec2(1223.0, 821.0),
            minimum_window,
            minimum_window / 1.5,
        ] {
            let ctx = egui::Context::default();
            style::apply(&ctx);
            let _ = style::apply_font_preferences(&ctx, &mut Preferences::default());
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
            let mut render = || {
                ctx.run(
                    egui::RawInput {
                        screen_rect: Some(screen),
                        ..Default::default()
                    },
                    |ctx| assert!(!form.dialog(ctx, &mut workspace)),
                )
            };
            let _ = render();
            let output = render();
            for label in ["备份为 SQL 转储文件", "等待导出", "导出", "关闭"] {
                let (bounds, clip) = output
                    .shapes
                    .iter()
                    .find_map(|shape| {
                        let bounds = text_bounds(&shape.shape, label)?;
                        Some((bounds, shape.clip_rect.intersect(screen)))
                    })
                    .unwrap_or_else(|| panic!("{label} 未显示在 {size:?} 的视口内"));
                assert!(
                    clip.contains_rect(bounds),
                    "{label} 在 {size:?} 的视口内被裁剪。文字区域 {bounds:?}，可见区域 {clip:?}"
                );
            }
        }
    }
}
