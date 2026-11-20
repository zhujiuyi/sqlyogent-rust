use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use eframe::egui::{self, ComboBox, DragValue, TextEdit};
use harbor_sql::jobs::backup_editor::{
    self, Backup, BackupDatabasesJob, BackupJob, BackupMode, BackupOptions, Definition, JobObject,
    JobObjectKind, Schema, Selection,
};
use harbor_sql::sql_encoding::SqlDumpCharset;
use rfd::FileDialog;

use crate::app::{
    kind_label,
    state::{Profile, Workspace},
    style,
};
use crate::database::{
    DbObject, Response,
    transfer::{
        BackupDirectoryMode, DEFAULT_BULK_INSERT_KIB, DEFAULT_DUMP_CHUNK_ROWS, MAX_BULK_INSERT_KIB,
    },
};

use super::authoring::{Action, Form, connection_fields, from_profile};
use super::notification::ReportMailForm;

const OBJECT_KINDS: [JobObjectKind; 6] = [
    JobObjectKind::Table,
    JobObjectKind::View,
    JobObjectKind::Procedure,
    JobObjectKind::Function,
    JobObjectKind::Trigger,
    JobObjectKind::Event,
];

#[derive(Clone, Copy, Hash, PartialEq)]
enum Page {
    Connection,
    Scope,
    Output,
    Settings,
    Mail,
}

pub(super) struct Editor {
    pub definition: Definition,
    pub form: Form,
    page: Page,
    other_scope: Option<Backup>,
    other_single_output: Option<String>,
    other_multi_output: Option<String>,
    previous_schemas: Option<Vec<Schema>>,
    catalog: Catalog,
    mail: ReportMailForm,
}

#[derive(Default)]
struct Catalog {
    schemas: Vec<String>,
    objects: BTreeMap<String, Vec<DbObject>>,
    pending: Option<Receiver<Result<Response, String>>>,
}

enum Browse {
    Schemas,
    Objects(String),
}

impl Editor {
    pub(super) fn new(
        workspace: Option<&Workspace>,
        schema: String,
        object: Option<DbObject>,
    ) -> Self {
        let profile = workspace.and_then(|workspace| workspace.profile.as_ref());
        let schema = if schema.is_empty() {
            workspace
                .and_then(|workspace| workspace.server.as_ref())
                .and_then(|server| server.database.clone())
                .or_else(|| profile.map(|profile| profile.schema.clone()))
                .unwrap_or_default()
        } else {
            schema
        };
        let mut definition = Definition::new(schema, object);
        *definition.backup.options_mut() = BackupOptions {
            mode: BackupMode::StructureAndData,
            drop_existing: true,
            lock_source_tables: false,
            flush_logs: false,
            single_transaction: Some(false),
            chunk_rows: None,
            include_use: Some(true),
            include_create_database: Some(true),
            disable_foreign_key_checks: true,
            lock_insert_tables: false,
            disable_autocommit: Some(false),
            bulk_insert: true,
            bulk_insert_limit_kib: None,
            complete_insert: false,
            insert_delayed: false,
            disable_keys: false,
            ..BackupOptions::default()
        };
        let mut editor = Self::from_definition(definition, None, None);
        if let Some(profile) = profile {
            editor.definition.connection = from_profile(profile);
            if let Some(workspace) = workspace.filter(|workspace| workspace.server.is_some()) {
                editor.catalog.schemas = workspace.schemas.clone();
                editor.catalog.objects = workspace.objects.clone();
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
            form: Form::new(source, trigger, "backup-job.json"),
            page: Page::Connection,
            other_scope: None,
            other_single_output: None,
            other_multi_output: None,
            previous_schemas: None,
            catalog: Catalog::default(),
            mail: ReportMailForm::default(),
        }
    }

    pub(super) fn dialog(&mut self, ctx: &egui::Context, profiles: &[Profile]) -> Option<Action> {
        self.poll();
        let bounds = ctx.content_rect();
        let response =
            crate::app::style::modal(ctx, egui::Id::new("backup-job-editor")).show(ctx, |ui| {
                ui.set_width((bounds.width() - 48.0).clamp(400.0, 700.0));
                style::page_title(ui, if self.form.source.is_some() {
                    "编辑备份作业"
                } else {
                    "新建备份作业"
                });
                style::page_tabs(
                    ui,
                    &mut self.page,
                    &[
                        (Page::Connection, "连接"),
                        (Page::Scope, "备份范围"),
                        (Page::Output, "输出与运行"),
                        (Page::Settings, "SQL 选项"),
                        (Page::Mail, "邮件通知"),
                    ],
                );
                ui.add_space(8.0);
                let browse = egui::ScrollArea::vertical()
                    .id_salt(("backup-edit", self.page))
                    .max_height((bounds.height() - 205.0).max(210.0))
                    .show(ui, |ui| match self.page {
                        Page::Connection => {
                            self.connection_form(ui, profiles);
                            None
                        }
                        Page::Scope => self.scope_form(ui),
                        Page::Output => {
                            self.output_form(ui);
                            None
                        }
                        Page::Settings => {
                            let multi =
                                matches!(self.definition.backup, Backup::BackupDatabases(_));
                            let merged = matches!(
                                &self.definition.backup,
                                Backup::BackupDatabases(job) if job.output_file.is_some()
                            );
                            let http = self.definition.connection.http.is_some();
                            settings_form(
                                ui,
                                self.definition.backup.options_mut(),
                                multi,
                                merged,
                                http,
                            );
                            None
                        }
                        Page::Mail => {
                            self.mail.show(
                                ui,
                                &mut self.definition.notification,
                                "发送备份通知邮件",
                            );
                            None
                        }
                    })
                    .inner;
                if let Some(browse) = browse {
                    self.query(ctx, browse);
                }
                ui.separator();
                if self.catalog.pending.is_some() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("正在读取数据库对象…");
                    });
                }
                self.form.buttons(ui)
            });
        if response.should_close() {
            Some(Action::Close)
        } else {
            response.inner
        }
    }

    fn connection_form(&mut self, ui: &mut egui::Ui, profiles: &[Profile]) {
        let connection = &mut self.definition.connection;
        let original = connection.clone();
        if let Some(schema) = connection_fields(ui, connection, profiles, true)
            && let Backup::Backup(job) = &mut self.definition.backup
            && job.schema.is_empty()
        {
            job.schema = schema;
        }
        if original.http.is_none() && connection.http.is_some() {
            configure_http(self.definition.backup.options_mut());
            if let Some(other) = &mut self.other_scope {
                configure_http(other.options_mut());
            }
        }
        if *connection != original {
            self.catalog = Catalog::default();
            self.form.error = None;
        }
    }

    fn scope_form(&mut self, ui: &mut egui::Ui) -> Option<Browse> {
        let is_multi = matches!(self.definition.backup, Backup::BackupDatabases(_));
        let mut multi = is_multi;
        ui.horizontal(|ui| {
            ui.selectable_value(&mut multi, false, "单个数据库");
            ui.selectable_value(&mut multi, true, "多个数据库");
        });
        if multi != is_multi {
            let backup = self
                .other_scope
                .take()
                .unwrap_or_else(|| other_scope(&self.definition.backup));
            self.other_scope = Some(std::mem::replace(&mut self.definition.backup, backup));
        }
        let ready = self.catalog.pending.is_none();
        let mut browse = ui
            .add_enabled(ready, egui::Button::new("读取数据库列表"))
            .clicked()
            .then_some(Browse::Schemas);
        match &mut self.definition.backup {
            Backup::Backup(job) => {
                ui.horizontal(|ui| {
                    ui.label("数据库");
                    ui.add(TextEdit::singleline(&mut job.schema).desired_width(220.0));
                    choose_schema(ui, &self.catalog.schemas, |name| job.schema = name.into());
                });
                selectable_objects(
                    ui,
                    &job.schema,
                    &mut job.objects,
                    &mut job.object_kinds,
                    &self.catalog,
                    &mut browse,
                );
            }
            Backup::BackupDatabases(job) => {
                let old_all = job.all_user_schemas;
                ui.horizontal(|ui| {
                    ui.radio_value(&mut job.all_user_schemas, false, "指定数据库");
                    ui.radio_value(&mut job.all_user_schemas, true, "所有可见用户数据库");
                });
                if old_all != job.all_user_schemas {
                    if job.all_user_schemas {
                        self.previous_schemas = job.schemas.take();
                        job.include_mysql = false;
                    } else {
                        job.schemas = self.previous_schemas.take();
                    }
                }
                if job.all_user_schemas {
                    ui.checkbox(&mut job.include_mysql, "包含 mysql 系统库")
                        .on_hover_text("转储可能包含数据库账户和认证数据。");
                } else {
                    let schemas = job.schemas.get_or_insert_with(Vec::new);
                    ui.horizontal(|ui| {
                        if ui.button("手动添加数据库").clicked() {
                            schemas.push(Schema::Whole(String::new()));
                        }
                        choose_schema(ui, &self.catalog.schemas, |name| {
                            if !schemas.iter().any(|schema| schema.name() == name) {
                                schemas.push(Schema::Whole(name.into()));
                            }
                        });
                    });
                    let mut remove = None;
                    for (index, schema) in schemas.iter_mut().enumerate() {
                        ui.push_id(index, |ui| {
                            let label = if schema.name().is_empty() {
                                "待填写数据库"
                            } else {
                                schema.name()
                            };
                            egui::CollapsingHeader::new(label)
                                .id_salt("backup-schema")
                                .default_open(schema.name().is_empty())
                                .show(ui, |ui| {
                                    let mut selected = match schema {
                                        Schema::Whole(name) => Selection {
                                            schema: name.clone(),
                                            objects: None,
                                            object_kinds: None,
                                        },
                                        Schema::Selected(selected) => selected.clone(),
                                    };
                                    ui.horizontal(|ui| {
                                        ui.label("数据库");
                                        ui.add(
                                            TextEdit::singleline(&mut selected.schema)
                                                .desired_width(220.0),
                                        );
                                        if ui.button("移除此数据库").clicked() {
                                            remove = Some(index);
                                        }
                                    });
                                    selectable_objects(
                                        ui,
                                        &selected.schema,
                                        &mut selected.objects,
                                        &mut selected.object_kinds,
                                        &self.catalog,
                                        &mut browse,
                                    );
                                    *schema = if selected.objects.is_none()
                                        && selected.object_kinds.is_none()
                                    {
                                        Schema::Whole(selected.schema)
                                    } else {
                                        Schema::Selected(selected)
                                    };
                                });
                        });
                    }
                    if let Some(index) = remove {
                        schemas.remove(index);
                    }
                    job.include_mysql = schemas
                        .iter()
                        .any(|schema| schema.name().eq_ignore_ascii_case("mysql"));
                }
            }
        }
        browse
    }

    fn output_form(&mut self, ui: &mut egui::Ui) {
        match &mut self.definition.backup {
            Backup::Backup(job) => {
                let split = job.options.file_per_object;
                ui.horizontal(|ui| {
                    ui.radio_value(&mut job.options.file_per_object, false, "单个 SQL 文件");
                    ui.radio_value(&mut job.options.file_per_object, true, "每个对象单独文件");
                });
                if split != job.options.file_per_object {
                    let switched = self
                        .other_single_output
                        .take()
                        .unwrap_or_else(|| default_output(job.options.file_per_object).into());
                    self.other_single_output = Some(std::mem::replace(&mut job.output, switched));
                    job.options.overwrite = false;
                    job.options.append = false;
                    if !job.options.file_per_object {
                        job.options.directory_mode = BackupDirectoryMode::PerRun;
                        job.options.split_filename_timestamp = false;
                    }
                }
                output_path(ui, &mut job.output, job.options.file_per_object);
                if job.options.file_per_object {
                    backup_directory_options(ui, &mut job.options);
                } else {
                    existing_backup_options(ui, &mut job.options, "文件已存在时", "覆盖", "追加");
                }
            }
            Backup::BackupDatabases(job) => {
                let mut merged = job.output_file.is_some();
                let prior = merged;
                ui.horizontal(|ui| {
                    ui.radio_value(&mut merged, false, "每个数据库单独文件");
                    ui.radio_value(&mut merged, true, "合并为一个 SQL 文件");
                });
                if merged != prior {
                    let old = if prior {
                        job.output_file.take()
                    } else {
                        job.output_directory.take()
                    };
                    let switched = self
                        .other_multi_output
                        .take()
                        .unwrap_or_else(|| default_output(!merged).into());
                    self.other_multi_output = old;
                    job.options.overwrite = false;
                    job.options.append = false;
                    if merged {
                        job.output_file = Some(switched);
                        job.options.include_use = Some(true);
                        job.options.directory_mode = BackupDirectoryMode::PerRun;
                        job.options.split_filename_timestamp = false;
                    } else {
                        job.output_directory = Some(switched);
                    }
                }
                if merged {
                    output_path(ui, job.output_file.get_or_insert_with(String::new), false);
                    ui.checkbox(&mut job.options.append, "追加到已有 SQL 文件");
                } else {
                    output_path(
                        ui,
                        job.output_directory.get_or_insert_with(String::new),
                        true,
                    );
                    backup_directory_options(ui, &mut job.options);
                }
                ui.checkbox(
                    &mut job.continue_on_error,
                    "一个数据库失败后继续备份其余数据库",
                );
            }
        }
        let options = self.definition.backup.options_mut();
        ui.horizontal(|ui| {
            ui.label("SQL 文件字符集");
            ComboBox::from_id_salt("backup-file-charset")
                .selected_text(options.file_charset.label())
                .show_ui(ui, |ui| {
                    for charset in SqlDumpCharset::ALL {
                        ui.selectable_value(&mut options.file_charset, charset, charset.label());
                    }
                })
                .response
                .on_hover_text("决定 SQL 文件的编码和文件内的 SET NAMES 设置。");
        });
        ui.checkbox(&mut options.continue_on_error, "对象出错后继续备份")
            .on_hover_text(
                "遇到对象错误时保留可导出的内容。作业会报告备份不完整，并列出跳过的对象和原因。",
            );
        self.form
            .controls(ui, &mut self.definition.schedule, &mut self.definition.log);
    }

    fn query(&mut self, ctx: &egui::Context, browse: Browse) {
        let connection = self.definition.connection.clone();
        let directory = match self.form.metadata_directory(&connection) {
            Ok(directory) => directory,
            Err(error) => {
                self.form.error = Some(error);
                return;
            }
        };
        let schema = match browse {
            Browse::Schemas => None,
            Browse::Objects(schema) => Some(schema),
        };
        let ctx = ctx.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        match thread::Builder::new()
            .name("harbor-backup-catalog".into())
            .spawn(move || {
                let _ = sender.send(backup_editor::browse(&connection, &directory, schema));
                ctx.request_repaint();
            }) {
            Ok(_) => {
                self.catalog.pending = Some(receiver);
                self.form.error = None;
            }
            Err(error) => self.form.error = Some(format!("无法启动数据库读取：{error}")),
        }
    }

    fn poll(&mut self) {
        let Some(pending) = &self.catalog.pending else {
            return;
        };
        match pending.try_recv() {
            Ok(Ok(Response::Schemas(schemas))) => self.catalog.schemas = schemas,
            Ok(Ok(Response::Objects { schema, objects })) => {
                self.catalog.objects.insert(schema, objects);
            }
            Ok(Err(error)) => self.form.error = Some(error),
            Ok(Ok(_)) | Err(TryRecvError::Disconnected) => {
                self.form.error = Some("读取数据库对象未返回结果。".into())
            }
            Err(TryRecvError::Empty) => return,
        }
        self.catalog.pending = None;
    }
}

fn configure_http(options: &mut BackupOptions) {
    if options.single_transaction == Some(true) {
        options.single_transaction = None;
    }
    options.lock_source_tables = false;
}

fn other_scope(backup: &Backup) -> Backup {
    match backup {
        Backup::Backup(job) => {
            let schemas = if job.schema.is_empty() {
                Vec::new()
            } else if job.objects.is_none() && job.object_kinds.is_none() {
                vec![Schema::Whole(job.schema.clone())]
            } else {
                vec![Schema::Selected(Selection {
                    schema: job.schema.clone(),
                    objects: job.objects.clone(),
                    object_kinds: job.object_kinds.clone(),
                })]
            };
            let include_mysql = job.schema.eq_ignore_ascii_case("mysql");
            Backup::BackupDatabases(BackupDatabasesJob {
                all_user_schemas: false,
                schemas: Some(schemas),
                include_mysql,
                output_file: None,
                output_directory: Some(default_output(true).into()),
                continue_on_error: false,
                options: BackupOptions {
                    file_per_object: false,
                    overwrite: false,
                    append: false,
                    ..job.options.clone()
                },
            })
        }
        Backup::BackupDatabases(job) => {
            let (schema, objects, object_kinds) =
                match job.schemas.as_ref().and_then(|schemas| schemas.first()) {
                    Some(Schema::Whole(name)) => (name.clone(), None, None),
                    Some(Schema::Selected(selection)) => (
                        selection.schema.clone(),
                        selection.objects.clone(),
                        selection.object_kinds.clone(),
                    ),
                    None => (String::new(), None, None),
                };
            Backup::Backup(BackupJob {
                schema,
                output: default_output(false).into(),
                objects,
                object_kinds,
                options: BackupOptions {
                    directory_mode: BackupDirectoryMode::PerRun,
                    split_filename_timestamp: false,
                    overwrite: false,
                    ..job.options.clone()
                },
            })
        }
    }
}

fn choose_schema(ui: &mut egui::Ui, schemas: &[String], mut select: impl FnMut(&str)) {
    if schemas.is_empty() {
        return;
    }
    ComboBox::from_id_salt("backup-visible-schemas")
        .selected_text("选择数据库…")
        .show_ui(ui, |ui| {
            for schema in schemas {
                if ui.selectable_label(false, schema).clicked() {
                    select(schema);
                }
            }
        });
}

fn selectable_objects(
    ui: &mut egui::Ui,
    schema: &str,
    objects: &mut Option<Vec<JobObject>>,
    kinds: &mut Option<Vec<JobObjectKind>>,
    catalog: &Catalog,
    browse: &mut Option<Browse>,
) {
    let all = objects.is_none() && kinds.is_none();
    let mut entire = all;
    ui.horizontal(|ui| {
        ui.radio_value(&mut entire, true, "全部对象");
        ui.radio_value(&mut entire, false, "指定对象");
    });
    if entire != all {
        *objects = (!entire).then(Vec::new);
        *kinds = None;
    }
    if entire {
        return;
    }
    ui.label("运行时包含所选类型的全部对象");
    ui.horizontal_wrapped(|ui| {
        for kind in OBJECT_KINDS {
            let mut include = kinds.as_ref().is_some_and(|kinds| kinds.contains(&kind));
            if ui
                .checkbox(&mut include, kind_label(kind.database_kind()))
                .changed()
            {
                let kinds = kinds.get_or_insert_with(Vec::new);
                if include {
                    kinds.push(kind);
                } else {
                    kinds.retain(|prior| *prior != kind);
                }
            }
        }
    });
    if ui
        .add_enabled(
            !schema.is_empty() && catalog.pending.is_none(),
            egui::Button::new("读取该库对象"),
        )
        .clicked()
    {
        *browse = Some(Browse::Objects(schema.into()));
    }
    if let Some(visible) = catalog.objects.get(schema) {
        ui.collapsing(format!("可见对象（{}）", visible.len()), |ui| {
            egui::ScrollArea::vertical().max_height(180.0).show_rows(
                ui,
                ui.spacing().interact_size.y,
                visible.len(),
                |ui, range| {
                    for object in &visible[range] {
                        let kind = object.kind.into();
                        let listed =
                            |item: &JobObject| item.kind == kind && item.name == object.name;
                        let mut selected = objects
                            .as_ref()
                            .is_some_and(|items| items.iter().any(listed));
                        if ui
                            .checkbox(
                                &mut selected,
                                format!("{} · {}", kind_label(object.kind), object.name),
                            )
                            .changed()
                        {
                            let items = objects.get_or_insert_with(Vec::new);
                            if selected {
                                items.push(JobObject {
                                    kind,
                                    name: object.name.clone(),
                                });
                            } else {
                                items.retain(|item| !listed(item));
                            }
                        }
                    }
                },
            );
        });
    }
    let count = objects.as_ref().map_or(0, Vec::len);
    egui::CollapsingHeader::new(format!("指定名称的对象（{count}）"))
        .id_salt("backup-explicit-objects")
        .show(ui, |ui| {
            if ui.button("手动添加对象").clicked() {
                objects.get_or_insert_with(Vec::new).push(JobObject {
                    kind: JobObjectKind::Table,
                    name: String::new(),
                });
            }
            if let Some(objects) = objects {
                let mut remove = None;
                for (index, object) in objects.iter_mut().enumerate() {
                    ui.push_id(index, |ui| {
                        ui.horizontal(|ui| {
                            ComboBox::from_id_salt("backup-explicit-object-kind")
                                .selected_text(kind_label(object.kind.database_kind()))
                                .show_ui(ui, |ui| {
                                    for kind in OBJECT_KINDS {
                                        ui.selectable_value(
                                            &mut object.kind,
                                            kind,
                                            kind_label(kind.database_kind()),
                                        );
                                    }
                                });
                            ui.add(
                                TextEdit::singleline(&mut object.name)
                                    .desired_width(215.0)
                                    .hint_text("对象名称"),
                            );
                            if ui.button("移除").clicked() {
                                remove = Some(index);
                            }
                        });
                    });
                }
                if let Some(index) = remove {
                    objects.remove(index);
                }
            }
        });
}

fn backup_directory_options(ui: &mut egui::Ui, options: &mut BackupOptions) {
    let mode = &mut options.directory_mode;
    ui.horizontal_wrapped(|ui| {
        ui.label("目录命名");
        ui.radio_value(mode, BackupDirectoryMode::PerRun, "每次新建子目录");
        ui.radio_value(mode, BackupDirectoryMode::Timestamped, "时间戳子目录");
        ui.radio_value(mode, BackupDirectoryMode::Selected, "使用所选目录");
    });
    if *mode == BackupDirectoryMode::Selected {
        ui.checkbox(
            &mut options.split_filename_timestamp,
            "文件名添加本地时间前缀",
        );
        existing_backup_options(
            ui,
            options,
            "所选目录已有内容时",
            "覆盖已有备份",
            "追加到恢复顺序",
        );
    } else {
        options.overwrite = false;
        options.append = false;
        options.split_filename_timestamp = false;
    }
}

fn existing_backup_options(
    ui: &mut egui::Ui,
    options: &mut BackupOptions,
    context: &str,
    overwrite: &str,
    append: &str,
) {
    let mut existing = if options.append {
        2
    } else {
        u8::from(options.overwrite)
    };
    ui.horizontal_wrapped(|ui| {
        ui.label(context);
        ui.radio_value(&mut existing, 0, "结束本次备份");
        ui.radio_value(&mut existing, 1, overwrite);
        ui.radio_value(&mut existing, 2, append);
    });
    options.overwrite = existing == 1;
    options.append = existing == 2;
}

fn output_path(ui: &mut egui::Ui, output: &mut String, directory: bool) {
    ui.horizontal_wrapped(|ui| {
        ui.label(if directory {
            "输出目录"
        } else {
            "SQL 文件"
        });
        ui.add(TextEdit::singleline(output).desired_width(330.0));
        if ui.button("浏览…").clicked() {
            let mut dialog = FileDialog::new();
            let path = Path::new(output);
            let folder = if directory && path.is_dir() {
                Some(path)
            } else {
                path.parent()
            };
            if let Some(folder) = folder.filter(|folder| folder.is_dir()) {
                dialog = dialog.set_directory(folder);
            }
            let selected = if directory {
                dialog.pick_folder()
            } else {
                let filename = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("backup.sql");
                dialog
                    .set_file_name(filename)
                    .add_filter("SQL 文件", &["sql"])
                    .save_file()
            };
            if let Some(path) = selected {
                *output = path.display().to_string();
            }
        }
        if !directory
            && !output.contains("{unix_ms}")
            && !output.contains("{sja_timestamp}")
            && let Some(name) = Path::new(output).file_name()
            && ui.button("添加时间前缀").clicked()
        {
            *output = Path::new(output)
                .with_file_name(format!("{{sja_timestamp}}_{}", name.to_string_lossy()))
                .display()
                .to_string();
        }
    });
    if directory {
        ui.label("每次运行在输出目录下创建新的备份目录。");
    } else {
        ui.label("文件名可用 {unix_ms} 或 {sja_timestamp} 插入运行时间，避免下次备份时重名。");
    }
}

fn default_output(directory: bool) -> &'static str {
    if directory {
        "backups"
    } else {
        "backups/backup-{unix_ms}.sql"
    }
}

fn settings_form(
    ui: &mut egui::Ui,
    options: &mut BackupOptions,
    multi: bool,
    merged: bool,
    http: bool,
) {
    ui.horizontal_wrapped(|ui| {
        ui.label("导出内容");
        ui.radio_value(
            &mut options.mode,
            BackupMode::StructureAndData,
            "结构和数据",
        );
        ui.radio_value(&mut options.mode, BackupMode::Structure, "仅结构");
        ui.radio_value(&mut options.mode, BackupMode::Data, "仅数据");
    });
    ui.separator();
    ui.strong("源库");
    ui.horizontal_wrapped(|ui| {
        if !http {
            let locked = options.lock_source_tables;
            ui.checkbox(&mut options.lock_source_tables, "读取时锁定源表");
            if locked != options.lock_source_tables && options.lock_source_tables {
                options.lock_source_server = false;
                options.single_transaction = Some(false);
            }
            let mut transaction = options.starts_transaction();
            if ui.checkbox(&mut transaction, "单一事务").changed() {
                options.single_transaction = Some(transaction);
                if transaction {
                    options.lock_source_tables = false;
                }
            }
        }
        if !http || options.lock_source_server {
            if ui
                .checkbox(&mut options.lock_source_server, "读取时锁定整个源服务器")
                .on_hover_text("备份读取期间，源服务器的写入会等待全局读锁释放。")
                .changed()
                && options.lock_source_server
            {
                options.lock_source_tables = false;
            }
        }
        ui.checkbox(&mut options.flush_logs, "转储前刷新日志");
    });
    if http {
        configure_http(options);
        ui.label("HTTP 隧道分批读取，每批使用独立连接。");
        if options.lock_source_server {
            ui.colored_label(
                style::error(),
                "请关闭全局读锁，或将连接方式改为直连或 SSH 隧道。",
            );
        }
    }
    limit_field(
        ui,
        "每批读取行数",
        &mut options.chunk_rows,
        DEFAULT_DUMP_CHUNK_ROWS,
        u32::MAX,
        if http {
            "按隧道默认行数"
        } else {
            "流式读取"
        },
    );

    ui.separator();
    ui.strong("生成的 SQL");
    ui.horizontal_wrapped(|ui| {
        let mut create = options.include_create_database.unwrap_or(multi);
        if ui.checkbox(&mut create, "CREATE DATABASE").changed() {
            options.include_create_database = Some(create);
        }
        let mut use_schema = options.include_use.unwrap_or(multi);
        if ui
            .add_enabled(
                !merged,
                egui::Checkbox::new(&mut use_schema, "USE DATABASE"),
            )
            .changed()
        {
            options.include_use = Some(use_schema);
        }
        ui.checkbox(&mut options.drop_existing, "DROP 对象");
    });
    ui.horizontal_wrapped(|ui| {
        ui.checkbox(&mut options.disable_foreign_key_checks, "暂停外键检查");
        ui.checkbox(&mut options.lock_insert_tables, "写入时锁表");
        ui.checkbox(&mut options.disable_keys, "暂停更新非唯一索引");
    });
    ui.horizontal_wrapped(|ui| {
        ui.checkbox(&mut options.complete_insert, "写明 INSERT 列名");
        ui.checkbox(&mut options.bulk_insert, "生成批量 INSERT");
        ui.checkbox(&mut options.insert_delayed, "生成延迟 INSERT")
            .on_hover_text("随结构一起导出时，兼容的非事务表使用 INSERT DELAYED。支持该语法的服务器可将数据排队写入，新版 MySQL 按普通 INSERT 执行。");
    });
    if options.bulk_insert {
        limit_field(
            ui,
            "单条 INSERT 上限（KiB）",
            &mut options.bulk_insert_limit_kib,
            DEFAULT_BULK_INSERT_KIB,
            MAX_BULK_INSERT_KIB,
            "按源库通信包",
        );
    }
    ui.horizontal_wrapped(|ui| {
        ui.label("自动提交");
        ui.radio_value(&mut options.disable_autocommit, None, "默认");
        ui.radio_value(&mut options.disable_autocommit, Some(true), "暂时关闭");
        ui.radio_value(
            &mut options.disable_autocommit,
            Some(false),
            "沿用还原会话设置",
        );
    });
}

fn limit_field(
    ui: &mut egui::Ui,
    label: &str,
    limit: &mut Option<NonZeroU32>,
    default: u32,
    max: u32,
    off: &str,
) {
    ui.horizontal_wrapped(|ui| {
        ui.label(label);
        let mut enabled = limit.is_some();
        if ui.checkbox(&mut enabled, "指定数值").changed() {
            *limit = if enabled {
                NonZeroU32::new(default)
            } else {
                None
            };
        }
        if let Some(current) = limit {
            let mut value = current.get();
            if ui.add(DragValue::new(&mut value).range(1..=max)).changed() {
                *limit = NonZeroU32::new(value);
            }
        } else {
            ui.label(off);
        }
    });
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn saves_installed_wizard_defaults_for_new_backup_jobs() {
        let executable = std::env::current_exe().unwrap();
        let directory = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let path = directory.path().join("backup-job.json");
        let wizard = Editor::new(None, "demo".into(), None);
        let bytes = wizard.definition.validated_bytes(&path).unwrap();
        let mut saved: Value = serde_json::from_slice(&bytes).unwrap();
        let options = &saved["job"]["options"];
        let defaults = json!({
            "mode": "structure_and_data",
            "drop_existing": true,
            "lock_source_tables": false,
            "flush_logs": false,
            "continue_on_error": false,
            "file_charset": "utf8mb4",
            "single_transaction": false,
            "chunk_rows": null,
            "include_use": true,
            "include_create_database": true,
            "disable_foreign_key_checks": true,
            "lock_insert_tables": false,
            "disable_autocommit": false,
            "bulk_insert": true,
            "bulk_insert_limit_kib": null,
            "complete_insert": false,
            "disable_keys": false,
        });
        for (key, expected) in defaults.as_object().unwrap() {
            assert_eq!(options.get(key), Some(expected), "{key}");
        }

        saved["job"]["options"]["include_create_database"] = json!(false);
        saved["job"]["options"]["single_transaction"] = json!(true);
        saved["job"]["options"]["continue_on_error"] = json!(true);
        saved["job"]["options"]["file_charset"] = json!("gb18030");
        saved["job"]["options"]["bulk_insert_limit_kib"] = json!(512);
        std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
        let definition = Definition::read(&path).unwrap().unwrap();
        let reopened = Editor::from_definition(definition, Some(path.clone()), None);
        let reopened: Value =
            serde_json::from_slice(&reopened.definition.validated_bytes(&path).unwrap()).unwrap();
        assert_eq!(reopened["job"]["options"], saved["job"]["options"]);
    }

    #[test]
    fn backup_notification_settings_roundtrip() {
        use harbor_sql::jobs::notification_editor::{Notification, Security, Trigger};

        let executable = std::env::current_exe().unwrap();
        let directory = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let path = directory.path().join("backup-mail.json");
        let mut wizard = Editor::new(None, "demo".into(), None);
        assert!(wizard.definition.notification.is_none());
        wizard.definition.notification = Some(Notification {
            smtp_host: "smtp.example.com".into(),
            security: Security::StartTls,
            username_env: Some("HARBOR_FIXTURE_MAIL_USER".into()),
            password_env: Some("HARBOR_FIXTURE_MAIL_PASSWORD".into()),
            from: "Backup <backup@example.com>".into(),
            to: vec!["recipient@example.com".into()],
            cc: vec!["reviewer@example.com".into()],
            bcc: vec!["archive@example.com".into()],
            reply_to: Some("support@example.com".into()),
            subject: Some("计划备份报告".into()),
            when: Trigger::Failure,
            ..Notification::default()
        });

        let bytes = wizard.definition.validated_bytes(&path).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let definition = Definition::read(&path).unwrap().unwrap();
        let reopened = Editor::from_definition(definition, Some(path.clone()), None);
        assert_eq!(reopened.definition.validated_bytes(&path).unwrap(), bytes);
    }
}
