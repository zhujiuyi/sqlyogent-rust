use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use eframe::egui::{self, ComboBox, TextEdit};
use harbor_sql::jobs::{
    backup_editor::JobConnection,
    data_sync_editor::{
        self, Browse, Conflict, DataSyncJob, Definition, FilterScope, Mode, TableMapping, TablePair,
    },
};
use rfd::FileDialog;

use crate::app::{
    state::{Profile, Workspace},
    style,
};
use crate::database::ObjectKind;

use super::authoring::{self, Action, Form};

#[derive(Clone, Copy, Hash, PartialEq)]
enum Page {
    Source,
    Target,
    Tables,
    Run,
}

#[derive(Clone, Copy, PartialEq)]
enum Execution {
    Compare,
    Apply,
    Script,
    ScriptAndApply,
}

pub(super) struct Editor {
    pub definition: Definition,
    pub form: Form,
    page: Page,
    source: Catalog,
    target: Catalog,
    observed_path: Option<PathBuf>,
    previous_tables: Vec<TablePair>,
    selected_table: usize,
}

#[derive(Default)]
struct Catalog {
    schemas: Vec<String>,
    tables: BTreeMap<String, Vec<String>>,
    columns: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    pending: Option<(Browse, Receiver<Result<Vec<String>, String>>)>,
    error: Option<String>,
}

impl Editor {
    pub(super) fn new(workspace: Option<&Workspace>) -> Self {
        let profile = workspace.and_then(|workspace| workspace.profile.as_ref());
        let schema = workspace
            .and_then(|workspace| workspace.server.as_ref())
            .and_then(|server| server.database.clone())
            .or_else(|| profile.map(|profile| profile.schema.clone()))
            .unwrap_or_default();
        let mut definition = Definition::new(schema);
        if let Some(profile) = profile {
            definition.connection = authoring::from_profile(profile);
        }
        let mut editor = Self::from_definition(definition, None, None);
        if let Some(workspace) =
            workspace.filter(|workspace| workspace.profile.is_some() && workspace.server.is_some())
        {
            editor.source.schemas.clone_from(&workspace.schemas);
            for (schema, objects) in &workspace.objects {
                editor.source.tables.insert(
                    schema.clone(),
                    objects
                        .iter()
                        .filter(|object| object.kind == ObjectKind::Table)
                        .map(|object| object.name.clone())
                        .collect(),
                );
            }
        }
        editor
    }

    pub(super) fn from_definition(
        definition: Definition,
        source: Option<PathBuf>,
        trigger: Option<super::schedule::Trigger>,
    ) -> Self {
        Self {
            definition,
            observed_path: source.clone(),
            form: Form::new(source, trigger, "data-sync-job.json"),
            page: Page::Source,
            source: Catalog::default(),
            target: Catalog::default(),
            previous_tables: Vec::new(),
            selected_table: 0,
        }
    }

    pub(super) fn dialog(&mut self, ctx: &egui::Context, profiles: &[Profile]) -> Option<Action> {
        if self.form.path() != self.observed_path.as_deref() {
            self.observed_path = self.form.path().map(PathBuf::from);
            self.source = Catalog::default();
            self.target = Catalog::default();
        }
        self.source.poll();
        self.target.poll();
        let bounds = ctx.content_rect();
        let response =
            crate::app::style::modal(ctx, egui::Id::new("data-sync-job-editor")).show(ctx, |ui| {
                ui.set_width((bounds.width() - 48.0).clamp(400.0, 700.0));
                ui.heading(if self.form.source.is_some() {
                    "编辑数据同步作业"
                } else {
                    "新建数据同步作业"
                });
                ui.horizontal_wrapped(|ui| {
                    for (page, label) in [
                        (Page::Source, "源端连接"),
                        (Page::Target, "目标端连接"),
                        (Page::Tables, "同步范围"),
                        (Page::Run, "运行与调度"),
                    ] {
                        ui.selectable_value(&mut self.page, page, label);
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .id_salt(("sync-edit", self.page))
                    .max_height((bounds.height() - 190.0).max(220.0))
                    .show(ui, |ui| match self.page {
                        Page::Source => endpoint_fields(
                            ui,
                            &self.form,
                            &mut self.source,
                            &mut self.definition.connection,
                            &mut self.definition.job.source_schema,
                            profiles,
                        ),
                        Page::Target => endpoint_fields(
                            ui,
                            &self.form,
                            &mut self.target,
                            &mut self.definition.job.target,
                            &mut self.definition.job.target_schema,
                            profiles,
                        ),
                        Page::Tables => self.table_fields(ui),
                        Page::Run => {
                            run_fields(ui, &mut self.definition.job, &self.form);
                            self.form.controls(
                                ui,
                                &mut self.definition.schedule,
                                &mut self.definition.log,
                            );
                        }
                    });
                ui.separator();
                self.form.buttons(ui)
            });
        if response.should_close() {
            Some(Action::Close)
        } else {
            response.inner
        }
    }

    fn table_fields(&mut self, ui: &mut egui::Ui) {
        let job = &mut self.definition.job;
        let was_all = job.all_tables;
        ui.horizontal_wrapped(|ui| {
            ui.radio_value(&mut job.all_tables, true, "每次运行匹配同名表");
            ui.radio_value(&mut job.all_tables, false, "指定表映射");
        });
        if was_all != job.all_tables {
            std::mem::swap(&mut job.tables, &mut self.previous_tables);
        }
        if job.all_tables {
            ui.label("每次运行时读取源库和目标库的表。日志列出只存在于一端的表。");
            return;
        }

        let ready = !job.source_schema.is_empty() && !job.target_schema.is_empty();
        let can_browse = ready && self.source.pending.is_none() && self.target.pending.is_none();
        let browse_tables = ui
            .horizontal_wrapped(|ui| {
                let browse = ui
                    .add_enabled(can_browse, egui::Button::new("读取两端表"))
                    .clicked();
                let source = self.source.tables.get(&job.source_schema);
                let target = self.target.tables.get(&job.target_schema);
                if let Some((source, target)) = source.zip(target)
                    && ui.button("添加同名表").clicked()
                {
                    let first = job.tables.len();
                    add_matches(&mut job.tables, source, target);
                    if job.tables.len() > first {
                        self.selected_table = first;
                    }
                }
                if ui.button("手动添加映射").clicked() {
                    self.selected_table = job.tables.len();
                    job.tables.push(TablePair::Mapped(TableMapping::default()));
                }
                browse
            })
            .inner;
        if browse_tables {
            let ctx = ui.ctx();
            self.source.start(
                ctx,
                &self.form,
                &self.definition.connection,
                Browse::Tables(job.source_schema.clone()),
            );
            self.target.start(
                ctx,
                &self.form,
                &job.target,
                Browse::Tables(job.target_schema.clone()),
            );
        }
        self.source.status(ui, "源端");
        self.target.status(ui, "目标端");
        if job.tables.is_empty() {
            return;
        }
        self.selected_table = self.selected_table.min(job.tables.len() - 1);
        egui::ScrollArea::vertical()
            .id_salt("sync-table-list")
            .max_height(130.0)
            .show(ui, |ui| {
                for (index, table) in job.tables.iter().enumerate() {
                    let (source, target) = table.names();
                    let label = if source.is_empty() && target.is_empty() {
                        "待填写表映射".into()
                    } else {
                        format!("{source} → {target}")
                    };
                    ui.selectable_value(&mut self.selected_table, index, label);
                }
            });
        let selected = &mut self.selected_table;
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(*selected > 0, egui::Button::new("上移"))
                .clicked()
            {
                job.tables.swap(*selected, *selected - 1);
                *selected -= 1;
            }
            if ui
                .add_enabled(*selected + 1 < job.tables.len(), egui::Button::new("下移"))
                .clicked()
            {
                job.tables.swap(*selected, *selected + 1);
                *selected += 1;
            }
            if ui.button("移除映射").clicked() {
                job.tables.remove(*selected);
                *selected = (*selected).min(job.tables.len().saturating_sub(1));
            }
        });
        let Some(table) = job.tables.get_mut(*selected) else {
            return;
        };
        if let TablePair::SameName(name) = table {
            *table = TablePair::Mapped(TableMapping {
                source: name.clone(),
                target: name.clone(),
                ..TableMapping::default()
            });
        }
        let TablePair::Mapped(mapping) = table else {
            unreachable!("table mapping is expanded")
        };

        ui.separator();
        let source = self
            .source
            .tables
            .get(&job.source_schema)
            .map(Vec::as_slice);
        let target = self
            .target
            .tables
            .get(&job.target_schema)
            .map(Vec::as_slice);
        table_name_field(ui, "源表", &mut mapping.source, source);
        table_name_field(ui, "目标表", &mut mapping.target, target);
        let columns = self
            .source
            .columns
            .get(&job.source_schema)
            .and_then(|tables| tables.get(&mapping.source));
        let can_read = self.source.pending.is_none()
            && !job.source_schema.is_empty()
            && !mapping.source.is_empty();
        ui.label("比较列（留空时比较全部列，写入时同步整行）");
        let browse_columns = ui
            .horizontal_wrapped(|ui| {
                let browse = ui
                    .add_enabled(can_read, egui::Button::new("读取源表列"))
                    .clicked();
                if let Some(columns) = columns.filter(|columns| !columns.is_empty()) {
                    ui.menu_button("勾选比较列…", |ui| {
                        egui::ScrollArea::vertical()
                            .max_height(200.0)
                            .show(ui, |ui| {
                                for column in columns {
                                    let current = mapping.compare_columns.iter().position(|name| {
                                        name.eq_ignore_ascii_case(column)
                                            || name.to_lowercase() == column.to_lowercase()
                                    });
                                    let mut checked = current.is_some();
                                    if ui.checkbox(&mut checked, column).changed() {
                                        if let Some(index) = current {
                                            mapping.compare_columns.remove(index);
                                        } else {
                                            mapping.compare_columns.push(column.clone());
                                        }
                                    }
                                }
                            });
                    });
                }
                if ui.button("手动添加列").clicked() {
                    mapping.compare_columns.push(String::new());
                }
                browse
            })
            .inner;
        if browse_columns {
            self.source.start(
                ui.ctx(),
                &self.form,
                &self.definition.connection,
                Browse::Columns {
                    schema: job.source_schema.clone(),
                    table: mapping.source.clone(),
                },
            );
        }
        let mut remove_column = None;
        for (index, column) in mapping.compare_columns.iter_mut().enumerate() {
            ui.push_id((*selected, index), |ui| {
                ui.horizontal(|ui| {
                    ui.add(
                        TextEdit::singleline(column)
                            .hint_text("列名")
                            .desired_width(210.0),
                    );
                    if ui.small_button("移除").clicked() {
                        remove_column = Some(index);
                    }
                });
            });
        }
        if let Some(index) = remove_column {
            mapping.compare_columns.remove(index);
        }
        ui.label("行筛选条件（WHERE）");
        let mut predicate = mapping.predicate.take().unwrap_or_default();
        ui.add(
            TextEdit::multiline(&mut predicate)
                .hint_text("例如 status = 'active'")
                .desired_rows(2)
                .desired_width(350.0),
        );
        mapping.predicate = (!predicate.trim().is_empty()).then_some(predicate);
        if mapping.predicate.is_some() {
            let mut scope = mapping.filter_scope.unwrap_or_default();
            ui.horizontal_wrapped(|ui| {
                ui.radio_value(&mut scope, FilterScope::Source, "仅筛选源端");
                ui.radio_value(&mut scope, FilterScope::Both, "两端使用同一条件");
            });
            if mapping.filter_scope.is_some() || scope != FilterScope::default() {
                mapping.filter_scope = Some(scope);
            }
            if job.mode == Mode::ReconcileBoth && scope == FilterScope::Source {
                ui.colored_label(style::ERROR, "双向同步的行条件需要应用于两端。");
            }
        } else {
            mapping.filter_scope = None;
        }
    }
}

impl Catalog {
    fn start(
        &mut self,
        ctx: &egui::Context,
        form: &Form,
        connection: &JobConnection,
        query: Browse,
    ) {
        let directory = match form.metadata_directory(connection) {
            Ok(directory) => directory,
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        let connection = connection.clone();
        let request = query.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        let ctx = ctx.clone();
        match thread::Builder::new()
            .name("harbor-sync-job-catalog".into())
            .spawn(move || {
                let _ = sender.send(data_sync_editor::browse(&connection, &directory, request));
                ctx.request_repaint();
            }) {
            Ok(_) => {
                self.pending = Some((query, receiver));
                self.error = None;
            }
            Err(error) => self.error = Some(format!("无法启动数据库读取：{error}")),
        }
    }

    fn poll(&mut self) {
        let Some((query, receiver)) = self.pending.take() else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(names)) => match query {
                Browse::Schemas => self.schemas = names,
                Browse::Tables(schema) => {
                    self.tables.insert(schema, names);
                }
                Browse::Columns { schema, table } => {
                    self.columns.entry(schema).or_default().insert(table, names);
                }
            },
            Ok(Err(error)) => self.error = Some(error),
            Err(TryRecvError::Empty) => self.pending = Some((query, receiver)),
            Err(TryRecvError::Disconnected) => {
                self.error = Some("读取数据库对象未返回结果。".into())
            }
        }
    }

    fn status(&self, ui: &mut egui::Ui, endpoint: &str) {
        if self.pending.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!("正在读取{endpoint}数据库对象…"));
            });
        }
        if let Some(error) = &self.error {
            ui.colored_label(style::ERROR, format!("{endpoint}读取失败。{error}"));
        }
    }
}

fn endpoint_fields(
    ui: &mut egui::Ui,
    form: &Form,
    catalog: &mut Catalog,
    connection: &mut JobConnection,
    schema: &mut String,
    profiles: &[Profile],
) {
    let original = connection.clone();
    if let Some(profile_schema) = authoring::connection_fields(ui, connection, profiles, true)
        && !profile_schema.is_empty()
    {
        *schema = profile_schema;
    }
    if *connection != original {
        *catalog = Catalog::default();
    }
    ui.separator();
    ui.horizontal_wrapped(|ui| {
        ui.label("数据库");
        ui.add(TextEdit::singleline(schema).desired_width(210.0));
        if ui
            .add_enabled(
                catalog.pending.is_none(),
                egui::Button::new("读取数据库列表"),
            )
            .clicked()
        {
            catalog.start(ui.ctx(), form, connection, Browse::Schemas);
        } else {
            choose_name(ui, "sync-schema-list", schema, &catalog.schemas);
        }
    });
    catalog.status(ui, "当前端点");
}

fn table_name_field(ui: &mut egui::Ui, label: &str, value: &mut String, names: Option<&[String]>) {
    ui.horizontal_wrapped(|ui| {
        ui.label(label);
        ui.add(TextEdit::singleline(value).desired_width(210.0));
        if let Some(names) = names {
            choose_name(ui, label, value, names);
        }
    });
}

fn choose_name(ui: &mut egui::Ui, id: &str, value: &mut String, names: &[String]) {
    if names.is_empty() {
        return;
    }
    ComboBox::from_id_salt(id)
        .selected_text("从列表选择…")
        .show_ui(ui, |ui| {
            for name in names {
                if ui.selectable_label(value == name, name).clicked() {
                    value.clone_from(name);
                }
            }
        });
}

fn add_matches(tables: &mut Vec<TablePair>, source: &[String], target: &[String]) {
    let (used_sources, used_targets): (HashSet<_>, HashSet<_>) =
        tables.iter().map(TablePair::names).unzip();
    let target: HashSet<_> = target.iter().map(String::as_str).collect();
    let matches = source
        .iter()
        .filter(|name| {
            let name = name.as_str();
            target.contains(name) && !used_sources.contains(name) && !used_targets.contains(name)
        })
        .cloned()
        .map(TablePair::SameName)
        .collect::<Vec<_>>();
    tables.extend(matches);
}

fn run_fields(ui: &mut egui::Ui, job: &mut DataSyncJob, form: &Form) {
    ui.label("同步方式");
    ui.horizontal_wrapped(|ui| {
        ui.radio_value(&mut job.mode, Mode::InsertMissing, "单向补齐");
        ui.radio_value(&mut job.mode, Mode::InsertAndUpdate, "单向补齐并更新");
        ui.radio_value(&mut job.mode, Mode::MirrorReceiver, "镜像目标端");
        ui.radio_value(&mut job.mode, Mode::ReconcileBoth, "双向合并");
    });
    if job.mode == Mode::ReconcileBoth {
        let mut conflict = job.conflict.unwrap_or_default();
        ui.horizontal_wrapped(|ui| {
            ui.label("同键记录有冲突时");
            ui.radio_value(&mut conflict, Conflict::KeepSeparate, "保留各自的数据");
            ui.radio_value(&mut conflict, Conflict::SourceWins, "以源端为准");
            ui.radio_value(&mut conflict, Conflict::TargetWins, "以目标端为准");
        });
        if job.conflict.is_some() || conflict != Conflict::default() {
            job.conflict = Some(conflict);
        }
    } else {
        job.conflict = None;
    }
    ui.checkbox(&mut job.skip_target_fk_checks, "暂停目标端外键检查")
        .on_hover_text(if job.mode == Mode::ReconcileBoth {
            "只在目标端写入时暂停外键检查，源端仍会检查。目标端可能留下缺少父记录的数据。"
        } else {
            "只在目标端写入时暂停外键检查，可能留下缺少父记录的数据。"
        });
    let mut execution = match (job.apply, job.script_directory.is_some()) {
        (false, false) => Execution::Compare,
        (true, false) => Execution::Apply,
        (false, true) => Execution::Script,
        (true, true) => Execution::ScriptAndApply,
    };
    ui.label("执行方式");
    ui.horizontal_wrapped(|ui| {
        ui.radio_value(&mut execution, Execution::Compare, "仅比较并记录差异");
        ui.radio_value(&mut execution, Execution::Apply, "比较后写入数据库");
        ui.radio_value(&mut execution, Execution::Script, "仅生成同步 SQL");
        ui.radio_value(
            &mut execution,
            Execution::ScriptAndApply,
            "保存 SQL 后写入数据库",
        );
    });
    job.apply = matches!(execution, Execution::Apply | Execution::ScriptAndApply);
    ui.add_enabled(
        job.apply,
        egui::Checkbox::new(&mut job.continue_on_row_error, "写入出错时跳过该行并继续"),
    )
    .on_hover_text("直接同步时，跳过因数据或约束被数据库拒绝的行，并把错误编号写入作业日志。")
    .on_disabled_hover_text("此设置在程序直接执行同步时生效。");
    if matches!(execution, Execution::Script | Execution::ScriptAndApply) {
        let directory = job.script_directory.get_or_insert_with(PathBuf::new);
        script_directory_field(ui, directory, form);
        if job.continue_on_row_error {
            ui.label(
                egui::RichText::new("跳过出错行的设置在程序直接执行同步时生效。")
                    .color(style::MUTED),
            );
        }
    } else {
        job.script_directory = None;
    }
    if job.apply {
        ui.label(match job.mode {
            Mode::InsertMissing => "目标端将新增缺少的行。",
            Mode::InsertAndUpdate => "目标端将新增缺少的行并更新有差异的行。",
            Mode::MirrorReceiver => "目标端将新增、更新行，并删除同步范围内源端没有的行。",
            Mode::ReconcileBoth => "源端和目标端都会写入，冲突按上述规则处理。",
        });
    }
    ui.separator();
}

fn script_directory_field(ui: &mut egui::Ui, directory: &mut PathBuf, form: &Form) {
    ui.horizontal_wrapped(|ui| {
        ui.label("同步 SQL 输出目录");
        let mut text = directory.display().to_string();
        if ui
            .add(
                TextEdit::singleline(&mut text)
                    .hint_text("输入或选择目录")
                    .desired_width(310.0),
            )
            .changed()
        {
            *directory = PathBuf::from(text);
        }
        if ui.button("浏览…").clicked() {
            let mut dialog = FileDialog::new().set_title("选择同步 SQL 输出目录");
            let current = if directory.is_absolute() {
                Some(directory.clone())
            } else {
                form.path().map(|_| form.directory().join(&*directory))
            };
            if let Some(folder) = current
                .as_deref()
                .and_then(|path| path.ancestors().find(|parent| parent.is_dir()))
            {
                dialog = dialog.set_directory(folder);
            }
            if let Some(path) = dialog.pick_folder() {
                *directory = path;
            }
        }
    });
    ui.label("相对路径以作业文件所在目录为准。");
}
