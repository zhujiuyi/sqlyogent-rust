use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use eframe::egui::{self, ComboBox, TextEdit};
use harbor_sql::database::{
    DbObject, ObjectKind, OdbcColumnDefinition, OdbcConflict, quote_identifier,
};
use harbor_sql::file_source::FileSource;
use harbor_sql::jobs::odbc_editor::{
    self, Column, Definition, Description, Discovery, EffectiveTableStrategy, ExistingTarget,
    MAX_TABLES, MissingTarget, Source, Table,
};
use harbor_sql::odbc::{self, SourceColumn, SourceTable};
use rfd::{MessageButtons, MessageDialog, MessageDialogResult};

use crate::app::{
    odbc_file_dsn,
    odbc_import::{
        TableMode, confirm_import, create_sql_editor, engine_fields, file_fields,
        metadata_checkboxes, metadata_fields, source_action_effect, source_action_fields,
        source_query_file_button, target_name,
    },
    state::{Profile, Workspace},
    style,
};

use super::authoring::{self, Action, Form, optional_field};
use super::notification::ReportMailForm;

mod catalog;

#[derive(Clone, Copy, Hash, PartialEq)]
enum Page {
    Target,
    Tables,
    Mail,
    Run,
}

enum Read {
    DataSources,
    Columns,
    Tables,
}

enum Metadata {
    DataSources(Vec<String>),
    Columns(usize, Description),
    Discovered(Source, Vec<odbc::DiscoveredTable>),
    Mapped(Option<usize>, Vec<(Table, Description)>),
}

#[derive(Default)]
struct TableFields {
    description: Description,
    selected_column: usize,
}

pub(super) struct Editor {
    pub form: Form,
    pub definition: Definition,
    page: Page,
    selected_table: usize,
    table_fields: Vec<TableFields>,
    data_sources: Vec<String>,
    catalog: Option<catalog::Catalog>,
    inactive_tables: Option<Vec<Table>>,
    inactive_discovery: Option<Discovery>,
    inactive_odbc_scope: (Option<String>, Option<String>),
    mail: ReportMailForm,
    pending: Option<(&'static str, Receiver<Result<Metadata, String>>)>,
    status: Option<Result<String, String>>,
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
        Self::from_definition(definition, None, None)
    }

    pub(super) fn from_definition(
        mut definition: Definition,
        source: Option<PathBuf>,
        trigger: Option<super::schedule::Trigger>,
    ) -> Self {
        let default_strategy = definition.job.default_table_strategy();
        let table_count = if definition.job.discovery.is_some() {
            definition.job.tables = None;
            0
        } else {
            let tables = definition.job.tables.get_or_insert_with(Vec::new);
            if tables.is_empty() {
                tables.push(Table::default());
            }
            for table in tables.iter_mut() {
                if default_strategy
                    .with_override(table.strategy)
                    .structure_only
                {
                    table.source.after_import = None;
                }
            }
            tables.len()
        };
        let table_fields = (0..table_count).map(|_| TableFields::default()).collect();
        Self {
            form: Form::new(source, trigger, "odbc-import-job.json"),
            definition,
            page: Page::Target,
            selected_table: 0,
            table_fields,
            data_sources: Vec::new(),
            catalog: None,
            inactive_tables: None,
            inactive_discovery: None,
            inactive_odbc_scope: (None, None),
            mail: ReportMailForm::default(),
            pending: None,
            status: None,
        }
    }

    pub(super) fn dialog(&mut self, ctx: &egui::Context, profiles: &[Profile]) -> Option<Action> {
        self.poll();
        let bounds = ctx.content_rect();
        let response =
            crate::app::style::modal(ctx, egui::Id::new("odbc-job-editor")).show(ctx, |ui| {
                ui.set_width((bounds.width() - 48.0).clamp(400.0, 700.0));
                style::page_title(ui, if self.form.source.is_some() {
                    "编辑外部数据导入作业"
                } else {
                    "新建外部数据导入作业"
                });
                style::page_tabs(
                    ui,
                    &mut self.page,
                    &[
                        (Page::Target, "目标连接"),
                        (Page::Tables, "来源与映射"),
                        (Page::Mail, "邮件通知"),
                        (Page::Run, "保存与运行"),
                    ],
                );
                ui.add_space(8.0);
                let idle = self.pending.is_none();
                egui::ScrollArea::vertical()
                    .id_salt(("odbc-job-fields", self.page))
                    .max_height((bounds.height() - 230.0).max(210.0))
                    .show(ui, |ui| {
                        ui.add_enabled_ui(idle, |ui| match self.page {
                            Page::Target => self.target_fields(ui, profiles),
                            Page::Tables => self.tables(ui),
                            Page::Mail => self.mail.show(
                                ui,
                                &mut self.definition.notification,
                                "发送导入报告邮件",
                            ),
                            Page::Run => {
                                self.show_job_impact(ui);
                                self.form.controls(
                                    ui,
                                    &mut self.definition.schedule,
                                    &mut self.definition.log,
                                );
                            }
                        });
                    });
                ui.separator();
                if let Some(message) = self.pending.as_ref().map(|(message, _)| *message) {
                    ui.horizontal_wrapped(|ui| {
                        ui.spinner();
                        ui.label(message);
                        if ui.button("停止等待").clicked() {
                            self.pending = None;
                        }
                    });
                }
                if let Some(status) = &self.status {
                    match status {
                        Ok(message) => ui.label(message),
                        Err(error) => ui.colored_label(style::error(), error),
                    };
                }
                ui.add_enabled_ui(self.pending.is_none(), |ui| self.form.buttons(ui))
                    .inner
            });
        if response.should_close() {
            Some(Action::Close)
        } else if matches!(response.inner.as_ref(), Some(Action::Run)) && !self.confirm_run() {
            None
        } else {
            response.inner
        }
    }

    fn table_mode(&self) -> TableMode {
        TableMode::from_settings(self.definition.job.create_table, self.definition.job.reset)
    }

    fn affected_tables(&self) -> Vec<String> {
        let job = &self.definition.job;
        job.tables
            .iter()
            .flatten()
            .filter(|table| !table.table.trim().is_empty())
            .map(|table| target_name(&job.schema, &table.table))
            .collect()
    }

    fn show_job_impact(&self, ui: &mut egui::Ui) {
        if self.definition.job.discovery.is_some() {
            if let Some(impact) = self.discovery_impact() {
                ui.colored_label(style::error(), impact);
            }
            return;
        }
        let job = &self.definition.job;
        if job.has_table_strategies() && !job.continue_on_error {
            ui.label(
                "同时新建或替换表以及向已有表追加时，先发布新建或替换的表，再单独提交追加的数据。",
            );
        }
        for impact in self.target_impacts() {
            ui.colored_label(style::error(), impact);
        }
        let source = self.source_impacts();
        if !source.is_empty() {
            ui.colored_label(style::error(), "作业运行时将对以下源表执行导入后的操作。");
            for action in source {
                ui.label(action);
            }
        }
    }

    fn source_impacts(&self) -> Vec<String> {
        let defaults = self.definition.job.default_table_strategy();
        self.definition
            .job
            .tables
            .iter()
            .flatten()
            .filter(|table| defaults.with_override(table.strategy).imports_rows())
            .filter_map(|table| {
                let action = table.source.after_import.as_ref()?;
                let connection = table
                    .source
                    .dsn
                    .as_deref()
                    .filter(|name| !name.trim().is_empty())
                    .map(|dsn| format!("DSN {dsn}"))
                    .or_else(|| {
                        table
                            .source
                            .file_dsn
                            .as_ref()
                            .map(|path| format!("文件 DSN {}", path.display()))
                    })
                    .or_else(|| {
                        table
                            .source
                            .connection_env
                            .as_deref()
                            .filter(|name| !name.trim().is_empty())
                            .map(|variable| format!("连接变量 {variable}"))
                    })
                    .unwrap_or_else(|| "待填写数据源".into());
                Some(format!("{connection} · {}", source_action_effect(action)))
            })
            .collect()
    }

    fn confirm_run(&self) -> bool {
        if self.definition.job.discovery.is_some() {
            return self.discovery_impact().is_none_or(|impact| {
                MessageDialog::new()
                    .set_title("确认运行外部数据导入作业")
                    .set_description(format!("{impact}\n\n是否继续？"))
                    .set_buttons(MessageButtons::YesNo)
                    .show()
                    == MessageDialogResult::Yes
            });
        }
        if self.definition.job.has_table_strategies() {
            let mut impacts = self.target_impacts();
            let source = self.source_impacts();
            if !source.is_empty() {
                impacts.push(format!(
                    "源表操作仅处理已确认写入目标库的记录。\n{}",
                    source.join("\n")
                ));
            }
            return impacts.is_empty()
                || MessageDialog::new()
                    .set_title("确认运行外部数据导入作业")
                    .set_description(format!("{}\n\n是否继续？", impacts.join("\n\n")))
                    .set_buttons(MessageButtons::YesNo)
                    .show()
                    == MessageDialogResult::Yes;
        }
        confirm_import(
            "确认运行外部数据导入作业",
            self.table_mode(),
            &self.affected_tables(),
            &self.source_impacts(),
        )
    }

    fn target_impacts(&self) -> Vec<String> {
        let job = &self.definition.job;
        let defaults = job.default_table_strategy();
        [
            (ExistingTarget::Truncate, TableMode::Truncate),
            (ExistingTarget::Recreate, TableMode::Recreate),
        ]
        .into_iter()
        .filter_map(|(policy, mode)| {
            let tables = job
                .tables
                .iter()
                .flatten()
                .filter(|table| {
                    !table.table.trim().is_empty()
                        && defaults.with_override(table.strategy).if_exists == policy
                })
                .map(|table| target_name(&job.schema, &table.table))
                .collect::<Vec<_>>();
            (!tables.is_empty())
                .then(|| format!("{}\n{}", mode.impact().unwrap(), tables.join("\n")))
        })
        .collect()
    }

    fn discovery_impact(&self) -> Option<String> {
        let schema = quote_identifier(self.definition.job.schema.trim());
        match self.table_mode() {
            TableMode::Truncate => Some(format!(
                "将清空目标库 {schema} 中与本次发现的源表同名的现有表，再导入数据。保留原表结构。本次源表清单在运行时确定。"
            )),
            TableMode::Recreate => Some(format!(
                "将重建目标库 {schema} 中与本次发现的源表同名的现有表。已有表结构和数据会被替换。本次源表清单在运行时确定。"
            )),
            _ => None,
        }
    }

    fn set_discovery(&mut self, discovery: bool) {
        let job = &mut self.definition.job;
        if discovery == job.discovery.is_some() {
            return;
        }
        if discovery {
            let tables = job.tables.take().unwrap_or_default();
            let draft = self.inactive_discovery.take().unwrap_or_else(|| {
                let mut source = tables
                    .get(self.selected_table)
                    .map(|table| table.source.clone())
                    .unwrap_or_default();
                source.sql.clear();
                source.file_table = None;
                clear_source_options(&mut source);
                Discovery {
                    source,
                    ..Discovery::default()
                }
            });
            job.discovery = Some(draft);
            self.inactive_tables = Some(tables);
        } else if let Some(active) = job.discovery.take() {
            let tables = self
                .inactive_tables
                .take()
                .filter(|tables| !tables.is_empty())
                .unwrap_or_else(|| {
                    let mut source = active.source.clone();
                    source.sql.clear();
                    source.file_table = None;
                    clear_source_options(&mut source);
                    vec![Table {
                        source,
                        ..Default::default()
                    }]
                });
            if self.table_fields.len() != tables.len() {
                self.table_fields = (0..tables.len()).map(|_| TableFields::default()).collect();
            }
            self.selected_table = self.selected_table.min(tables.len().saturating_sub(1));
            job.tables = Some(tables);
            self.inactive_discovery = Some(active);
        }
        self.catalog = None;
        self.pending = None;
        self.status = None;
    }

    fn apply_mode(&mut self, mode: TableMode, target_changed: bool) {
        let job = &mut self.definition.job;
        let previous = job.default_table_strategy();
        job.create_table = mode.defines_table();
        job.reset = mode.reset();
        if !job.create_table {
            job.structure_only = false;
            for discovery in job
                .discovery
                .iter_mut()
                .chain(self.inactive_discovery.iter_mut())
            {
                discovery.copy_indexes = false;
                discovery.copy_foreign_keys = false;
            }
        }
        let current = job.default_table_strategy();
        let tables = job.tables.as_mut().or(self.inactive_tables.as_mut());
        for (table, fields) in tables.into_iter().flatten().zip(&mut self.table_fields) {
            let strategy = current.with_override(table.strategy);
            if strategy.structure_only {
                table.source.after_import = None;
            }
            if target_changed
                || previous.with_override(table.strategy).defines_table()
                    != strategy.defines_table()
            {
                if !strategy.defines_table() {
                    clear_creation_options(table);
                }
                fields.invalidate(table);
                self.status = None;
            }
        }
    }

    fn target_fields(&mut self, ui: &mut egui::Ui, profiles: &[Profile]) {
        let old_connection = self.definition.connection.clone();
        let old_schema = self.definition.job.schema.clone();
        let mut mode = self.table_mode();
        let job = &mut self.definition.job;
        if let Some(schema) =
            authoring::connection_fields(ui, &mut self.definition.connection, profiles, true)
            && !schema.is_empty()
        {
            job.schema = schema;
        }
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            ui.label("目标数据库");
            ui.add(TextEdit::singleline(&mut job.schema).desired_width(260.0));
        });
        TableMode::picker(ui, "odbc-job-import-mode", &mut mode, true);
        ui.checkbox(&mut job.continue_on_error, "表导入失败后继续其他表")
            .on_hover_text("复制外键时，关联表一起提交。若提交或回滚结果无法确认，作业会停止。");
        if mode.defines_table() {
            ui.checkbox(&mut job.structure_only, "仅导入表结构");
        }
        if !job.structure_only
            || !mode.defines_table()
            || job.tables.iter().flatten().any(|table| {
                table
                    .strategy
                    .is_some_and(|strategy| strategy.structure_only == Some(false))
            })
        {
            ui.horizontal_wrapped(|ui| {
                ui.label("默认重复键处理");
                let mut conflict = Some(job.conflict);
                conflict_field(ui, &mut conflict, None);
                job.conflict = conflict.unwrap_or_default();
            });
        }
        let connection = &self.definition.connection;
        let target_changed = old_schema != job.schema
            || old_connection.host != connection.host
            || old_connection.port != connection.port
            || old_connection.user != connection.user
            || old_connection.http.as_ref().map(|http| &http.url)
                != connection.http.as_ref().map(|http| &http.url)
            || old_connection.ssh.as_ref().map(|ssh| (&ssh.host, ssh.port))
                != connection.ssh.as_ref().map(|ssh| (&ssh.host, ssh.port));
        self.apply_mode(mode, target_changed);
        self.show_job_impact(ui);
    }

    fn tables(&mut self, ui: &mut egui::Ui) {
        let mut discovery = self.definition.job.discovery.is_some();
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut discovery, false, "指定要导入的表");
            ui.selectable_value(&mut discovery, true, "运行时发现全部源表");
        });
        self.set_discovery(discovery);
        if discovery {
            self.discovery_fields(ui);
            return;
        }
        if let Some(catalog) = &mut self.catalog {
            match catalog.show(ui, self.definition.job.create_table) {
                Some(catalog::Action::Back) => {
                    self.catalog = None;
                    self.status = None;
                }
                Some(catalog::Action::Add) => self.map_selected_tables(ui.ctx()),
                None => {}
            }
            return;
        }
        let job = &mut self.definition.job;
        let defaults = job.default_table_strategy();
        let tables = job
            .tables
            .as_mut()
            .expect("ODBC job editor uses a table list");
        let selected = &mut self.selected_table;
        let previous = *selected;
        ui.horizontal_wrapped(|ui| {
            ui.label("导入表");
            ComboBox::from_id_salt("odbc-job-tables")
                .selected_text(table_label(&tables[*selected], *selected))
                .show_ui(ui, |ui| {
                    for (index, table) in tables.iter().enumerate() {
                        ui.selectable_value(selected, index, table_label(table, index));
                    }
                });
            if ui
                .add_enabled(tables.len() < MAX_TABLES, egui::Button::new("添加表"))
                .clicked()
            {
                let mut source = tables[*selected].source.clone();
                source.sql.clear();
                source.file_table = None;
                clear_source_options(&mut source);
                *selected = tables.len();
                tables.push(Table {
                    source,
                    ..Table::default()
                });
                self.table_fields.push(TableFields::default());
                self.status = None;
            }
            if ui
                .add_enabled(tables.len() > 1, egui::Button::new("移除表"))
                .clicked()
            {
                tables.remove(*selected);
                self.table_fields.remove(*selected);
                *selected = (*selected).min(tables.len() - 1);
                self.status = None;
            }
        });
        if previous != *selected {
            self.status = None;
        }
        let index = *selected;
        let table = &mut tables[index];
        let fields = &mut self.table_fields[index];
        let read = ui
            .push_id(("odbc-job-table", index), |ui| {
                let strategy = table_strategy_fields(ui, table, defaults);
                let target_changed = ui
                    .horizontal_wrapped(|ui| {
                        ui.label("目标表名");
                        let changed = ui
                            .add(TextEdit::singleline(&mut table.table).desired_width(170.0))
                            .changed();
                        if strategy.imports_rows() {
                            ui.label("重复键");
                            conflict_field(ui, &mut table.conflict, Some(job.conflict));
                        }
                        changed
                    })
                    .inner;
                let reset = match strategy.if_exists {
                    ExistingTarget::Truncate => Some(TableMode::Truncate),
                    ExistingTarget::Recreate => Some(TableMode::Recreate),
                    _ => None,
                };
                if let Some(impact) = reset.and_then(TableMode::impact)
                    && !table.table.trim().is_empty()
                {
                    ui.colored_label(
                        style::error(),
                        format!("{impact}\n{}", target_name(&job.schema, &table.table)),
                    );
                }
                ui.separator();
                let (changed, credentials_changed, read, file_error) =
                    source_fields(ui, &mut table.source, &self.data_sources);
                if changed {
                    clear_source_options(&mut table.source);
                }
                if changed || target_changed || matches!(read, Some(Read::Columns)) {
                    fields.invalidate(table);
                    self.status = None;
                } else if credentials_changed {
                    fields.description = Description::default();
                    fields.selected_column = 0;
                    self.status = None;
                }
                if let Some(error) = file_error {
                    self.status = Some(Err(error));
                }
                if strategy.defines_table() {
                    fixed_metadata_fields(ui, &mut table.source);
                }
                if strategy.imports_rows() && table.source.file_source.is_none() {
                    source_action_fields(ui, &mut table.source.after_import, None);
                }
                ui.separator();
                fields.mapping(ui, &mut table.columns, strategy.defines_table());
                if strategy.defines_table() && !table.table.trim().is_empty() {
                    engine_fields(ui, &mut table.engine, &[]);
                    let object = DbObject {
                        schema: job.schema.clone(),
                        name: table.table.clone(),
                        kind: ObjectKind::Table,
                    };
                    let mut sql = table.create_sql.take();
                    create_sql_editor(ui, &object, &mut sql, || {
                        fields.description.create_sql(&object, table)
                    });
                    table.create_sql = sql;
                }
                read
            })
            .inner;
        match read {
            Some(Read::DataSources) => self.load_data_sources(ui.ctx()),
            Some(Read::Columns) => {
                let mut source = preview_source(&table.source, &self.form.directory());
                source.after_import = None;
                self.start(ui.ctx(), "正在读取源列…", move || {
                    odbc_editor::describe(&source)
                        .map(|description| Metadata::Columns(index, description))
                });
            }
            Some(Read::Tables) => {
                let mut login = table.source.clone();
                clear_source_options(&mut login);
                login.sql.clear();
                login.file_table = None;
                let source = preview_source(&login, &self.form.directory());
                self.start(ui.ctx(), "正在读取源数据表…", move || {
                    odbc_editor::discover(&source).map(|tables| Metadata::Discovered(login, tables))
                });
            }
            None => {}
        }
    }

    fn discovery_fields(&mut self, ui: &mut egui::Ui) {
        let job = &mut self.definition.job;
        let Some(discovery) = &mut job.discovery else {
            return;
        };
        ui.label("每次运行重新读取数据表列表，并按同名目标表和同名列导入。");
        if source_kind_fields(ui, &mut discovery.source) {
            switch_discovery_source(discovery, &mut self.inactive_odbc_scope);
            self.status = None;
        }
        if let Some(file) = &mut discovery.source.file_source {
            let (file_changed, credentials_changed) = file_fields::show_job(ui, file);
            if file_changed || credentials_changed {
                self.status = None;
            }
            discovery.catalog = None;
            discovery.schema = None;
            if job.create_table && file.supports_metadata() {
                metadata_checkboxes(
                    ui,
                    &mut discovery.copy_indexes,
                    &mut discovery.copy_foreign_keys,
                );
            } else {
                discovery.copy_indexes = false;
                discovery.copy_foreign_keys = false;
            }
            return;
        }
        let (changed, read, error) =
            source_login_fields(ui, &mut discovery.source, &self.data_sources);
        if changed {
            self.status = None;
        }
        if let Some(error) = error {
            self.status = Some(Err(error));
        }
        egui::Grid::new("odbc-job-discovery-scope")
            .num_columns(2)
            .show(ui, |ui| {
                optional_field(ui, "源目录", &mut discovery.catalog, "留空则不按目录筛选");
                optional_field(ui, "源架构", &mut discovery.schema, "留空则不按架构筛选");
            });
        ui.label("数据源返回多个目录或架构时，请至少填写一项筛选范围。");
        if job.create_table {
            metadata_checkboxes(
                ui,
                &mut discovery.copy_indexes,
                &mut discovery.copy_foreign_keys,
            );
            if discovery.copy_foreign_keys {
                ui.label("复制跨表外键时，发现范围须包含关联的源表。");
            }
        }
        if matches!(read, Some(Read::DataSources)) {
            self.load_data_sources(ui.ctx());
        }
    }

    fn load_data_sources(&mut self, ctx: &egui::Context) {
        self.start(ctx, "正在读取已注册的 DSN…", || {
            odbc::registered_data_sources().map(Metadata::DataSources)
        });
    }

    fn map_selected_tables(&mut self, ctx: &egui::Context) {
        let Some(catalog) = &self.catalog else {
            return;
        };
        let tables = self.definition.job.tables.as_ref().unwrap();
        let current = &tables[self.selected_table];
        let unused = (current.table.trim().is_empty() || current.source.file_source.is_some())
            && current.columns.is_empty()
            && current.source.sql.trim().is_empty()
            && current.source.file_table.is_none()
            && current.source.indexes_from.is_none()
            && current.source.after_import.is_none();
        let replace = unused.then_some(self.selected_table);
        let selected: Vec<_> = catalog
            .selection()
            .map(|(source, target)| (source.clone(), target.to_owned()))
            .collect();
        if tables.len() + selected.len() - usize::from(unused) > MAX_TABLES {
            self.status = Some(Err(format!("每个导入作业最多可加入 {MAX_TABLES} 张表。")));
            return;
        }
        let mut used: HashSet<_> = tables
            .iter()
            .enumerate()
            .filter(|(index, _)| Some(*index) != replace)
            .map(|(_, table)| table.table.trim().to_lowercase())
            .collect();
        if selected.iter().any(|(_, target)| {
            target.is_empty()
                || target.chars().count() > 64
                || target.contains('\0')
                || !used.insert(target.to_lowercase())
        }) {
            self.status = Some(Err("请为所有选中的源表填写有效且不重复的目标表名。".into()));
            return;
        }
        let login = catalog.login.clone();
        let source = preview_source(&login, &self.form.directory());
        let (indexes, foreign_keys) = catalog.copy_metadata(self.definition.job.create_table);
        self.start(ctx, "正在读取所选表的列映射…", move || {
            let tables = selected
                .into_iter()
                .map(|(discovered, target)| {
                    let name = discovered.source.table.clone();
                    odbc_editor::map_discovered_table(
                        &source,
                        discovered,
                        target,
                        indexes,
                        foreign_keys,
                    )
                    .map(|(mut table, columns)| {
                        table.source.file_source.clone_from(&login.file_source);
                        table.source.file_dsn.clone_from(&login.file_dsn);
                        (table, columns)
                    })
                    .map_err(|error| format!("无法读取源表「{name}」。{error}"))
                })
                .collect::<Result<_, _>>()?;
            Ok(Metadata::Mapped(replace, tables))
        });
    }

    fn start(
        &mut self,
        ctx: &egui::Context,
        label: &'static str,
        read: impl FnOnce() -> Result<Metadata, String> + Send + 'static,
    ) {
        self.status = None;
        let (sender, receiver) = mpsc::sync_channel(1);
        let ctx = ctx.clone();
        match thread::Builder::new()
            .name("harbor-odbc-job-metadata".into())
            .spawn(move || {
                let _ = sender.send(read());
                ctx.request_repaint();
            }) {
            Ok(_) => self.pending = Some((label, receiver)),
            Err(error) => self.status = Some(Err(format!("无法启动源数据读取。{error}"))),
        }
    }

    fn poll(&mut self) {
        let Some((label, receiver)) = self.pending.take() else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(Metadata::DataSources(names))) => {
                self.status = Some(Ok(format!("已读到 {} 个已注册的 DSN。", names.len())));
                self.data_sources = names;
            }
            Ok(Ok(Metadata::Columns(index, mut description))) => {
                let total = description.columns.len();
                let mut counts = HashMap::new();
                for column in &description.columns {
                    *counts.entry(column.name.to_lowercase()).or_insert(0) += 1;
                }
                let fields = &mut self.table_fields[index];
                description.columns.retain(|column| {
                    !column.name.trim().is_empty()
                        && !column.name.contains('\0')
                        && counts[&column.name.to_lowercase()] == 1
                });
                let table = &mut self.definition.job.tables.as_mut().unwrap()[index];
                table.columns = description.columns.iter().map(initial_mapping).collect();
                fields.selected_column = 0;
                let detected = description.columns.len();
                fields.description = description;
                self.status = Some(if detected == total && detected > 0 {
                    Ok(format!("已读取 {detected} 个源列。"))
                } else {
                    let tip = if table.source.file_source.is_some() {
                        "请检查文件中的列名是否为空或重复。"
                    } else {
                        "请给空白或重名列设置唯一的 SELECT 别名。"
                    };
                    Err(format!("读到 {detected}/{total} 个可映射的源列。{tip}"))
                });
            }
            Ok(Ok(Metadata::Discovered(login, tables))) => {
                self.status = Some(Ok(format!("已读取 {} 张源数据表。", tables.len())));
                self.catalog = Some(catalog::Catalog::new(login, tables));
            }
            Ok(Ok(Metadata::Mapped(replace, mapped))) => {
                let total = mapped.len();
                let tables = self.definition.job.tables.as_mut().unwrap();
                self.selected_table = replace.unwrap_or(tables.len());
                let mut slot = replace;
                for (table, description) in mapped {
                    let fields = TableFields {
                        description,
                        selected_column: 0,
                    };
                    if let Some(index) = slot.take() {
                        tables[index] = table;
                        self.table_fields[index] = fields;
                    } else {
                        tables.push(table);
                        self.table_fields.push(fields);
                    }
                }
                self.catalog = None;
                self.status = Some(Ok(format!("已加入 {total} 张表及同名列映射。")));
            }
            Ok(Err(error)) => self.status = Some(Err(error)),
            Err(TryRecvError::Empty) => self.pending = Some((label, receiver)),
            Err(TryRecvError::Disconnected) => {
                self.status = Some(Err("源数据读取没有返回结果。".into()));
            }
        }
    }
}

fn table_label(table: &Table, index: usize) -> String {
    if table.table.is_empty() {
        format!("待填写表 {}", index + 1)
    } else {
        table.table.clone()
    }
}

fn table_strategy_fields(
    ui: &mut egui::Ui,
    table: &mut Table,
    defaults: EffectiveTableStrategy,
) -> EffectiveTableStrategy {
    let prior = defaults.with_override(table.strategy);
    let mut strategy = table.strategy.unwrap_or_default();
    ui.horizontal_wrapped(|ui| {
        ui.label("目标表不存在时");
        strategy_choice(
            ui,
            "odbc-target-missing",
            &mut strategy.if_missing,
            defaults.if_missing,
            &[
                (MissingTarget::Create, "按映射新建"),
                (MissingTarget::Error, "报告错误"),
                (MissingTarget::Skip, "跳过"),
            ],
        );
        ui.label("目标表已存在时");
        strategy_choice(
            ui,
            "odbc-target-exists",
            &mut strategy.if_exists,
            defaults.if_exists,
            &[
                (ExistingTarget::Append, "保留表结构并追加数据"),
                (ExistingTarget::Skip, "保留表并跳过导入"),
                (ExistingTarget::Error, "报告错误"),
                (ExistingTarget::Truncate, "清空后导入"),
                (ExistingTarget::Recreate, "按映射重建"),
            ],
        );
        ui.label("导入内容");
        strategy_choice(
            ui,
            "odbc-target-content",
            &mut strategy.structure_only,
            defaults.structure_only,
            &[(false, "导入数据"), (true, "仅导入表结构")],
        );
    });
    table.strategy = (!strategy.is_empty()).then_some(strategy);
    let effective = defaults.with_override(table.strategy);
    if prior.defines_table() && !effective.defines_table() {
        clear_creation_options(table);
    }
    if !effective.imports_rows() {
        table.source.after_import = None;
    }
    effective
}

fn strategy_choice<T: Copy + Eq>(
    ui: &mut egui::Ui,
    id: &'static str,
    value: &mut Option<T>,
    default: T,
    choices: &[(T, &str)],
) {
    let label = |value| {
        choices
            .iter()
            .find(|(item, _)| *item == value)
            .map(|(_, label)| *label)
            .unwrap_or_default()
    };
    let inherited = format!("沿用作业设置（{}）", label(default));
    ComboBox::from_id_salt(id)
        .selected_text(value.map_or(inherited.as_str(), label))
        .show_ui(ui, |ui| {
            ui.selectable_value(value, None, inherited);
            for &(item, label) in choices {
                ui.selectable_value(value, Some(item), label);
            }
        });
}

fn clear_creation_options(table: &mut Table) {
    table.source.indexes_from = None;
    table.source.copy_foreign_keys = false;
    table.create_sql = None;
    table.engine = None;
    for column in &mut table.columns {
        column.definition = None;
    }
}

fn conflict_label(conflict: OdbcConflict) -> &'static str {
    match conflict {
        OdbcConflict::Abort => "终止本次导入",
        OdbcConflict::KeepTarget => "保留目标行",
        OdbcConflict::Replace => "用源数据替换整行",
    }
}

fn conflict_field(
    ui: &mut egui::Ui,
    value: &mut Option<OdbcConflict>,
    default: Option<OdbcConflict>,
) {
    let inherited = default.map(|default| format!("沿用作业设置（{}）", conflict_label(default)));
    let label = match *value {
        Some(policy) => conflict_label(policy),
        None => inherited.as_deref().unwrap_or_default(),
    };
    ComboBox::from_id_salt("odbc-job-conflict")
        .selected_text(label)
        .show_ui(ui, |ui| {
            if let Some(inherited) = inherited {
                ui.selectable_value(value, None, inherited);
            }
            for policy in [
                OdbcConflict::Abort,
                OdbcConflict::KeepTarget,
                OdbcConflict::Replace,
            ] {
                ui.selectable_value(value, Some(policy), conflict_label(policy));
            }
        });
}

fn source_login_fields(
    ui: &mut egui::Ui,
    source: &mut Source,
    dsns: &[String],
) -> (bool, Option<Read>, Option<String>) {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum LoginMode {
        Registered,
        FileDsn,
        ConnectionEnv,
    }

    let original = (
        source.dsn.clone(),
        source.user_env.clone(),
        source.password_env.clone(),
        source.connection_env.clone(),
        source.file_dsn.clone(),
    );
    let previous_mode = if source.file_dsn.is_some() {
        LoginMode::FileDsn
    } else if source.connection_env.is_some() {
        LoginMode::ConnectionEnv
    } else {
        LoginMode::Registered
    };
    let mut mode = previous_mode;
    ui.horizontal_wrapped(|ui| {
        ui.radio_value(&mut mode, LoginMode::Registered, "已注册 DSN");
        ui.radio_value(&mut mode, LoginMode::FileDsn, "文件 DSN");
        ui.radio_value(&mut mode, LoginMode::ConnectionEnv, "连接字符串变量");
    });
    if previous_mode != mode {
        source.connection_env = (mode == LoginMode::ConnectionEnv).then(String::new);
        source.file_dsn = (mode == LoginMode::FileDsn).then(PathBuf::new);
        source.dsn = None;
        source.user_env = None;
        source.password_env = None;
    }
    let mut read = None;
    let mut error = None;
    if mode != LoginMode::ConnectionEnv {
        if let Some(path) = &mut source.file_dsn {
            error = odbc_file_dsn::fields(ui, path).err();
        }
        egui::Grid::new("odbc-job-dsn-fields")
            .num_columns(2)
            .show(ui, |ui| {
                if mode == LoginMode::Registered {
                    optional_field(ui, "DSN 名称", &mut source.dsn, "输入已注册的数据源名称");
                }
                optional_field(ui, "源用户名变量名", &mut source.user_env, "可选");
                optional_field(ui, "源密码变量名", &mut source.password_env, "可选");
            });
    }
    if mode == LoginMode::Registered {
        ui.horizontal_wrapped(|ui| {
            if ui.button("读取本机 DSN").clicked() {
                read = Some(Read::DataSources);
            }
            if !dsns.is_empty() {
                ComboBox::from_id_salt("odbc-job-dsn-list")
                    .selected_text("选择已注册 DSN…")
                    .show_ui(ui, |ui| {
                        for dsn in dsns {
                            if ui
                                .selectable_label(source.dsn.as_ref() == Some(dsn), dsn)
                                .clicked()
                            {
                                source.dsn = Some(dsn.clone());
                            }
                        }
                    });
            }
        });
    } else if mode == LoginMode::ConnectionEnv {
        ui.horizontal_wrapped(|ui| {
            ui.label("连接字符串的环境变量名");
            if let Some(variable) = &mut source.connection_env {
                ui.add(TextEdit::singleline(variable).hint_text("ODBC_SOURCE_CONNECTION"));
            }
        });
    }
    (
        source.dsn != original.0
            || source.user_env != original.1
            || source.password_env != original.2
            || source.connection_env != original.3
            || source.file_dsn != original.4,
        read,
        error,
    )
}

fn source_fields(
    ui: &mut egui::Ui,
    source: &mut Source,
    dsns: &[String],
) -> (bool, bool, Option<Read>, Option<String>) {
    let kind_changed = source_kind_fields(ui, source);
    if let Some(file) = &mut source.file_source {
        let initial = file.clone();
        let (file_changed, credentials_changed) = file_fields::show_job(ui, file);
        if initial.format != file.format
            || initial.path != file.path
            || initial.access_link_folder != file.access_link_folder
            || initial.excel_named_ranges != file.excel_named_ranges
        {
            source.file_table = None;
        }
        if let Some(table) = &source.file_table {
            ui.label(format!("源数据表  {table}"));
        }
        let read = ui
            .horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(
                        !file.path.as_os_str().is_empty(),
                        egui::Button::new("从文件选择数据表…"),
                    )
                    .clicked()
                {
                    return Some(Read::Tables);
                }
                if source.file_table.is_some() && ui.button("读取源列").clicked() {
                    return Some(Read::Columns);
                }
                None
            })
            .inner;
        return (
            kind_changed || file_changed,
            credentials_changed,
            read,
            None,
        );
    }
    let (login_changed, mut read, login_error) = source_login_fields(ui, source, dsns);
    let loaded = ui
        .horizontal_wrapped(|ui| {
            ui.label("源端 SELECT");
            source_query_file_button(ui, &mut source.sql)
        })
        .inner;
    let sql_changed = ui
        .add(
            TextEdit::multiline(&mut source.sql)
                .code_editor()
                .desired_rows(4)
                .desired_width(f32::INFINITY)
                .hint_text("SELECT id, name FROM source_table"),
        )
        .changed();
    let has_connection = source
        .dsn
        .as_ref()
        .or(source.connection_env.as_ref())
        .is_some_and(|value| !value.trim().is_empty())
        || source
            .file_dsn
            .as_ref()
            .is_some_and(|path| !path.as_os_str().is_empty());
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(
                has_connection && !source.sql.trim().is_empty(),
                egui::Button::new("读取查询列"),
            )
            .on_hover_text("根据当前查询重新建立列映射。")
            .clicked()
        {
            read = Some(Read::Columns);
        }
        if ui
            .add_enabled(has_connection, egui::Button::new("从数据源选择数据表…"))
            .clicked()
        {
            read = Some(Read::Tables);
        }
    });
    (
        kind_changed || sql_changed || login_changed || matches!(loaded, Ok(true)),
        false,
        read,
        login_error.or_else(|| loaded.err()),
    )
}

fn source_kind_fields(ui: &mut egui::Ui, source: &mut Source) -> bool {
    let was_file = source.file_source.is_some();
    let mut file = was_file;
    ui.horizontal_wrapped(|ui| {
        ui.label("导入来源");
        ui.radio_value(&mut file, false, "ODBC 数据源");
        ui.radio_value(&mut file, true, "从文件导入");
    });
    if file == was_file {
        return false;
    }
    *source = Source {
        file_source: file.then(FileSource::default),
        ..Source::default()
    };
    true
}

fn switch_discovery_source(
    discovery: &mut Discovery,
    inactive_odbc_scope: &mut (Option<String>, Option<String>),
) {
    if discovery.source.file_source.is_some() {
        *inactive_odbc_scope = (discovery.catalog.take(), discovery.schema.take());
    } else {
        (discovery.catalog, discovery.schema) = std::mem::take(inactive_odbc_scope);
    }
    discovery.copy_indexes = false;
    discovery.copy_foreign_keys = false;
}

fn fixed_metadata_fields(ui: &mut egui::Ui, source: &mut Source) {
    let file_table = if let Some(file) = &source.file_source {
        let Some(table) = source
            .file_table
            .as_ref()
            .filter(|_| file.supports_metadata())
        else {
            return;
        };
        Some(SourceTable {
            table: table.clone(),
            ..Default::default()
        })
    } else {
        None
    };
    metadata_fields(
        ui,
        "odbc-job-source-metadata",
        &mut source.indexes_from,
        &mut source.copy_foreign_keys,
        file_table.as_ref(),
    );
    if source.copy_foreign_keys {
        ui.label(if file_table.is_some() {
            "跨表外键涉及的 Access 表须来自同一个文件，并加入当前作业。"
        } else {
            "跨表外键要求父表也在当前作业中，使用相同的数据源。请为父表勾选复制索引或外键，并填写源表位置。"
        });
    }
}

fn preview_source(configured: &Source, job_directory: &Path) -> Source {
    let mut source = configured.clone();
    source.resolve_file_paths(job_directory);
    source
}

fn clear_source_options(source: &mut Source) {
    source.indexes_from = None;
    source.copy_foreign_keys = false;
    source.after_import = None;
}

impl TableFields {
    fn invalidate(&mut self, table: &mut Table) {
        self.description = Description::default();
        self.selected_column = 0;
        table.columns.clear();
        table.create_sql = None;
    }

    fn mapping(&mut self, ui: &mut egui::Ui, mappings: &mut Vec<Column>, create: bool) {
        ui.horizontal_wrapped(|ui| {
            if !mappings.is_empty() {
                ui.label("列映射");
                ComboBox::from_id_salt("odbc-job-column-map")
                    .selected_text(mapping_label(
                        &mappings[self.selected_column],
                        self.selected_column,
                    ))
                    .show_ui(ui, |ui| {
                        for (index, mapping) in mappings.iter().enumerate() {
                            ui.selectable_value(
                                &mut self.selected_column,
                                index,
                                mapping_label(mapping, index),
                            );
                        }
                    });
            }
            if ui.button("添加列映射").clicked() {
                self.selected_column = mappings.len();
                mappings.push(Column::default());
            }
            if ui
                .add_enabled(!mappings.is_empty(), egui::Button::new("移除列映射"))
                .clicked()
            {
                mappings.remove(self.selected_column);
                self.selected_column = self.selected_column.min(mappings.len().saturating_sub(1));
            }
        });
        let Some(mapping) = mappings.get_mut(self.selected_column) else {
            return;
        };
        ui.push_id(self.selected_column, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("源列");
                ui.add(TextEdit::singleline(&mut mapping.source).desired_width(170.0));
                if !self.description.columns.is_empty() {
                    ComboBox::from_id_salt("odbc-job-source-columns")
                        .selected_text("选择源列…")
                        .show_ui(ui, |ui| {
                            for column in &self.description.columns {
                                if ui
                                    .selectable_label(mapping.source == column.name, &column.name)
                                    .clicked()
                                {
                                    mapping.source.clone_from(&column.name);
                                    if mapping.target.is_empty() {
                                        mapping.target = initial_mapping(column).target;
                                    }
                                }
                            }
                        });
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("目标列");
                ui.add(TextEdit::singleline(&mut mapping.target).desired_width(170.0));
            });
            if create {
                let wanted = mapping.source.to_lowercase();
                let suggested = self
                    .description
                    .columns
                    .iter()
                    .find(|column| column.name.to_lowercase() == wanted);
                definition_fields(ui, &mut mapping.definition, suggested);
            }
        });
    }
}

fn initial_mapping(column: &SourceColumn) -> Column {
    let target = if column.name.chars().count() <= 64 && !column.name.ends_with(' ') {
        column.name.clone()
    } else {
        String::new()
    };
    Column {
        source: column.name.clone(),
        target,
        definition: None,
    }
}

fn mapping_label(mapping: &Column, index: usize) -> String {
    if mapping.source.is_empty() && mapping.target.is_empty() {
        format!("待填写列 {}", index + 1)
    } else {
        format!("{} → {}", mapping.source, mapping.target)
    }
}

fn definition_fields(
    ui: &mut egui::Ui,
    definition: &mut Option<OdbcColumnDefinition>,
    suggested: Option<&SourceColumn>,
) {
    let mut specified = definition.is_some();
    if ui.checkbox(&mut specified, "自定义目标列定义").changed() {
        *definition = specified.then(|| {
            suggested.map_or_else(OdbcColumnDefinition::default, |column| {
                column.definition.clone()
            })
        });
    }
    let Some(definition) = definition else {
        if let Some(column) = suggested {
            ui.label(format!("推断类型 {}", column.definition.sql_type));
        }
        return;
    };
    ui.horizontal_wrapped(|ui| {
        ui.label("MySQL 类型");
        ui.add(TextEdit::singleline(&mut definition.sql_type).hint_text("如 VARCHAR(80) 或 BLOB"));
    });
    ui.horizontal_wrapped(|ui| {
        ui.checkbox(&mut definition.primary, "主键");
        ui.checkbox(&mut definition.not_null, "非空");
        ui.checkbox(&mut definition.unsigned, "无符号");
        ui.checkbox(&mut definition.auto_increment, "自增");
        ui.checkbox(&mut definition.binary, "二进制排序规则");
        ui.checkbox(&mut definition.zerofill, "ZEROFILL");
    });
    ui.horizontal_wrapped(|ui| {
        let mut enabled = definition.default.is_some();
        if ui.checkbox(&mut enabled, "使用默认值").changed() {
            definition.default = enabled.then(String::new);
        }
        if let Some(default) = &mut definition.default {
            ui.add(TextEdit::singleline(default).hint_text("文本值"));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::odbc_import::AfterMode;
    use harbor_sql::database::OdbcReset;
    use harbor_sql::file_source::FileSourceFormat;
    use harbor_sql::odbc::{SourceAction, SourceAssignment, SourceChange, SourceTable};

    #[test]
    fn dynamic_odbc_authoring_uses_one_job_shape_and_preserves_the_explicit_draft() {
        let mut editor = Editor::from_definition(Definition::new("目标库".into()), None, None);
        editor.apply_mode(TableMode::Recreate, false);
        let table = &mut editor.definition.job.tables.as_mut().unwrap()[0];
        table.table = "手工目标".into();
        table.columns.push(Column {
            source: "id".into(),
            target: "编号".into(),
            definition: None,
        });
        table.source.dsn = Some("示例来源".into());
        table.source.sql = "SELECT id FROM 手工来源".into();
        table.source.after_import = Some(SourceAction {
            table: SourceTable {
                table: "手工来源".into(),
                ..Default::default()
            },
            change: SourceChange::Delete,
        });

        editor.set_discovery(true);
        let job = &mut editor.definition.job;
        assert!(job.tables.is_none());
        let discovery = job.discovery.as_mut().unwrap();
        assert_eq!(discovery.source.dsn.as_deref(), Some("示例来源"));
        assert!(discovery.source.sql.is_empty() && discovery.source.after_import.is_none());
        discovery.schema = Some("限定范围".into());
        let saved = serde_json::to_value(&editor.definition).unwrap();
        assert!(saved["job"].get("tables").is_none());
        assert_eq!(saved["job"]["discovery"]["schema"], "限定范围");
        let impact = editor.discovery_impact().unwrap();
        assert!(impact.contains("目标库") && impact.contains("运行时确定"));
        assert!(editor.affected_tables().is_empty() && editor.source_impacts().is_empty());

        editor.set_discovery(false);
        let table = &editor.definition.job.tables.as_ref().unwrap()[0];
        assert_eq!(table.columns[0].target, "编号");
        assert!(matches!(
            table
                .source
                .after_import
                .as_ref()
                .map(|action| &action.change),
            Some(SourceChange::Delete)
        ));
        assert!(
            serde_json::to_value(&editor.definition).unwrap()["job"]
                .get("discovery")
                .is_none()
        );

        editor.set_discovery(true);
        assert_eq!(
            editor
                .definition
                .job
                .discovery
                .as_ref()
                .unwrap()
                .schema
                .as_deref(),
            Some("限定范围")
        );
        editor.definition.job.structure_only = true;
        editor.apply_mode(TableMode::Recreate, false);
        editor.set_discovery(false);
        let table = &editor.definition.job.tables.as_ref().unwrap()[0];
        assert!(table.source.after_import.is_none());
        assert_eq!(table.columns[0].target, "编号");
    }

    #[test]
    fn reopening_dynamic_odbc_job_shows_runtime_scope_and_no_stored_tables() {
        let mut definition = Definition::new("目标库".into());
        definition.job.tables = None;
        definition.job.discovery = Some(Discovery {
            source: Source {
                connection_env: Some("ODBC_TEST_SOURCE".into()),
                ..Default::default()
            },
            catalog: Some("数据库目录".into()),
            ..Default::default()
        });
        let mut editor = Editor::from_definition(definition, None, None);
        assert!(editor.definition.job.tables.is_none() && editor.table_fields.is_empty());
        assert_eq!(
            editor
                .definition
                .job
                .discovery
                .as_ref()
                .unwrap()
                .catalog
                .as_deref(),
            Some("数据库目录")
        );
        editor.set_discovery(false);
        let tables = editor.definition.job.tables.as_ref().unwrap();
        assert_eq!(tables.len(), 1);
        assert!(tables[0].columns.is_empty() && tables[0].source.after_import.is_none());
        assert_eq!(
            tables[0].source.connection_env.as_deref(),
            Some("ODBC_TEST_SOURCE")
        );
    }

    #[test]
    fn odbc_file_job_switches_modes_without_losing_the_explicit_file_settings() {
        let mut definition = Definition::new("目标库".into());
        let fixed = &mut definition.job.tables.as_mut().unwrap()[0];
        fixed.table = "库存".into();
        fixed.source.file_table = Some("现有库存".into());
        fixed.source.file_source = Some(FileSource {
            path: "现有库存.csv".into(),
            delimiter: "|".into(),
            headers: false,
            ..FileSource::default()
        });
        let mut editor = Editor::from_definition(definition, None, None);

        editor.set_discovery(true);
        let discovery = editor.definition.job.discovery.as_ref().unwrap();
        assert!(discovery.source.file_table.is_none());
        let file = discovery.source.file_source.as_ref().unwrap();
        assert_eq!(file.path, Path::new("现有库存.csv"));
        assert_eq!(file.delimiter, "|");
        assert!(!file.headers);
        let preview = preview_source(&discovery.source, Path::new("作业目录"));
        assert_eq!(
            preview.file_source.unwrap().path,
            Path::new("作业目录").join("现有库存.csv")
        );
        assert!(
            serde_json::to_value(&editor.definition).unwrap()["job"]
                .get("tables")
                .is_none()
        );

        editor.set_discovery(false);
        let fixed = &editor.definition.job.tables.as_ref().unwrap()[0];
        let file = fixed.source.file_source.as_ref().unwrap();
        assert_eq!(file.delimiter, "|");
        assert!(!file.headers);
        assert_eq!(fixed.source.file_table.as_deref(), Some("现有库存"));
        assert_eq!(fixed.table, "库存");
    }

    #[test]
    fn odbc_access_job_binds_copied_keys_to_the_exact_file_table() {
        let mut source = Source {
            file_source: Some(FileSource {
                format: FileSourceFormat::Access,
                path: "仓库.accdb".into(),
                access_password_env: Some("TEST_ACCESS_FILE_PASSWORD".into()),
                access_link_folder: Some("库存来源".into()),
                ..FileSource::default()
            }),
            file_table: Some("出库明细".into()),
            copy_foreign_keys: true,
            ..Source::default()
        };
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| fixed_metadata_fields(ui, &mut source));
        });

        let locator = source.indexes_from.as_ref().unwrap();
        assert_eq!(locator.table, "出库明细");
        assert!(locator.catalog.is_none() && locator.schema.is_none());
        assert!(source.copy_foreign_keys);
        let stored = serde_json::to_value(&source).unwrap();
        assert_eq!(
            stored["file_source"]["access_password_env"],
            "TEST_ACCESS_FILE_PASSWORD"
        );
        assert_eq!(stored["file_source"]["access_link_folder"], "库存来源");
        let preview = preview_source(&source, Path::new("作业目录"));
        assert_eq!(
            preview.file_source.unwrap().access_link_folder,
            Some(Path::new("作业目录").join("库存来源"))
        );
    }

    #[test]
    fn odbc_access_discovery_preserves_scope_when_switching_back_to_odbc() {
        let mut discovery = Discovery {
            source: Source {
                file_source: Some(FileSource {
                    format: FileSourceFormat::Access,
                    path: "仓库.mdb".into(),
                    ..FileSource::default()
                }),
                ..Source::default()
            },
            catalog: Some("业务库".into()),
            schema: Some("仓储".into()),
            copy_indexes: true,
            ..Discovery::default()
        };
        let mut parked = (None, None);
        switch_discovery_source(&mut discovery, &mut parked);
        assert!(discovery.catalog.is_none() && discovery.schema.is_none());
        assert!(!discovery.copy_indexes && !discovery.copy_foreign_keys);

        discovery.source = Source::default();
        switch_discovery_source(&mut discovery, &mut parked);
        assert_eq!(discovery.catalog.as_deref(), Some("业务库"));
        assert_eq!(discovery.schema.as_deref(), Some("仓储"));
    }

    #[test]
    fn odbc_discovery_adds_tables_alongside_existing_mapping() {
        let mut editor = Editor::from_definition(Definition::new("目标库".into()), None, None);
        let existing = &mut editor.definition.job.tables.as_mut().unwrap()[0];
        existing.table = "已有映射".into();
        existing.columns.push(Column {
            source: "id".into(),
            target: "编号".into(),
            definition: None,
        });
        let mapped = ["客户", "订单"]
            .into_iter()
            .map(|name| {
                let column = SourceColumn {
                    name: "id".into(),
                    definition: OdbcColumnDefinition::default(),
                };
                let table = Table {
                    table: name.into(),
                    columns: vec![initial_mapping(&column)],
                    ..Default::default()
                };
                let mut description = Description::default();
                description.columns.push(column);
                (table, description)
            })
            .collect();
        let (sender, receiver) = mpsc::sync_channel(1);
        sender.send(Ok(Metadata::Mapped(None, mapped))).unwrap();
        editor.pending = Some(("正在读取所选表的列映射…", receiver));
        editor.poll();

        let tables = editor.definition.job.tables.as_ref().unwrap();
        assert_eq!(
            tables
                .iter()
                .map(|table| table.table.as_str())
                .collect::<Vec<_>>(),
            ["已有映射", "客户", "订单"]
        );
        assert_eq!(tables[0].columns[0].target, "编号");
        assert_eq!(editor.selected_table, 1);
        assert_eq!(editor.table_fields.len(), tables.len());
    }

    #[test]
    fn odbc_after_import_reports_source_rows_and_clears_for_structure_only() {
        let mut editor = Editor::from_definition(Definition::new("目标库".into()), None, None);
        editor.apply_mode(TableMode::Recreate, false);
        let table = &mut editor.definition.job.tables.as_mut().unwrap()[0];
        table.table = "目标记录".into();
        let source = &mut table.source;
        source.dsn = Some("演示数据源".into());
        source.indexes_from = Some(SourceTable {
            table: "源记录".into(),
            ..Default::default()
        });
        source.copy_foreign_keys = true;
        AfterMode::Update.apply(&mut source.after_import);
        let action = source.after_import.as_mut().unwrap();
        action.table = SourceTable {
            table: "源记录".into(),
            schema: Some("源库".into()),
            ..Default::default()
        };
        let SourceChange::Update { assignments } = &mut action.change else {
            panic!("expected update assignments");
        };
        assignments.push(SourceAssignment {
            column: "导入状态".into(),
            expression: "'完成'".into(),
        });

        assert_eq!(editor.affected_tables(), ["`目标库`.`目标记录`"]);
        assert_eq!(
            editor.source_impacts(),
            ["DSN 演示数据源 · `源库`.`源记录` · 更新列 `导入状态`"]
        );
        let source = &mut editor.definition.job.tables.as_mut().unwrap()[0].source;
        AfterMode::Delete.apply(&mut source.after_import);
        assert_eq!(
            editor.source_impacts(),
            ["DSN 演示数据源 · `源库`.`源记录` · 删除已导入的行"]
        );

        editor.definition.job.structure_only = true;
        editor.apply_mode(TableMode::Recreate, false);
        let source = &editor.definition.job.tables.as_ref().unwrap()[0].source;
        assert!(source.after_import.is_none());
        assert!(source.copy_foreign_keys && source.indexes_from.is_some());
        assert!(editor.source_impacts().is_empty());
    }

    #[test]
    fn odbc_target_modes_job_preserve_compatible_mapping_and_invalidate_old_definitions() {
        let mut editor = Editor::from_definition(Definition::new("导入库".into()), None, None);
        let table = &mut editor.definition.job.tables.as_mut().unwrap()[0];
        table.table = "目标表".into();
        table.columns.push(Column {
            source: "id".into(),
            target: "编号".into(),
            definition: None,
        });
        assert!(editor.table_mode() == TableMode::Append);
        editor.apply_mode(TableMode::Truncate, false);
        assert!(editor.definition.job.reset == OdbcReset::Truncate);
        assert!(!editor.definition.job.create_table);
        assert_eq!(
            editor.definition.job.tables.as_ref().unwrap()[0].columns[0].target,
            "编号"
        );
        assert_eq!(editor.affected_tables(), ["`导入库`.`目标表`"]);

        editor.apply_mode(TableMode::Recreate, false);
        assert!(editor.definition.job.reset == OdbcReset::Recreate);
        assert!(editor.definition.job.create_table);
        assert!(
            editor.definition.job.tables.as_ref().unwrap()[0]
                .columns
                .is_empty()
        );
        let source = &mut editor.definition.job.tables.as_mut().unwrap()[0].source;
        source.indexes_from = Some(odbc::SourceTable {
            table: "源商品".into(),
            ..Default::default()
        });
        source.copy_foreign_keys = true;
        editor.definition.job.structure_only = true;
        editor.apply_mode(TableMode::Append, false);
        assert!(editor.definition.job.reset == OdbcReset::None);
        assert!(!editor.definition.job.create_table && !editor.definition.job.structure_only);
        let source = &editor.definition.job.tables.as_ref().unwrap()[0].source;
        assert!(source.indexes_from.is_none() && !source.copy_foreign_keys);
    }
}
