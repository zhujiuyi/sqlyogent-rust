use std::collections::HashSet;
use std::path::PathBuf;

use eframe::egui::{self, RichText, TextEdit};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::database::{
    Command, DbObject, ObjectKind, quote_identifier,
    transfer::{DumpOptions, TableCopySettings, TransferSummary},
};

use super::{
    io_jobs::{OBJECT_KINDS, object_checkbox_group},
    kind_label,
    preferences::{CopyChoices, SavedCopyDestination},
    state::{Profile, Workspace, load_profiles},
    style,
};

#[derive(PartialEq, Eq)]
enum TableCopyPhase {
    Configure,
    Copying,
    Done,
}

pub(super) struct SelectiveCopy {
    object: DbObject,
    settings: TableCopySettings,
    fields: Option<Vec<(String, bool)>>,
    all_fields: bool,
    phase: TableCopyPhase,
    status: String,
}

impl SelectiveCopy {
    pub(super) fn new(object: DbObject, workspace: &mut Workspace) -> Self {
        let mut copy = Self {
            settings: TableCopySettings {
                schema: object.schema.clone(),
                name: format!("{}_副本", object.name.chars().take(61).collect::<String>()),
                columns: None,
                include_data: false,
                include_indexes: true,
                include_triggers: false,
            },
            object,
            fields: None,
            all_fields: true,
            phase: TableCopyPhase::Configure,
            status: String::new(),
        };
        copy.load_fields(workspace);
        copy
    }

    fn load_fields(&mut self, workspace: &mut Workspace) {
        workspace.detail = None;
        if !workspace.send(
            Command::Inspect {
                object: self.object.clone(),
            },
            "正在读取源表字段…",
        ) {
            self.status.clone_from(&workspace.status);
        }
    }

    pub(super) fn show(&mut self, ctx: &egui::Context, workspace: &mut Workspace) -> bool {
        if self.phase == TableCopyPhase::Copying
            && let Some(result) = workspace.transfer_result.take()
        {
            match result {
                Ok(()) => {
                    self.phase = TableCopyPhase::Done;
                    self.status = format!("已创建 {}.{}", self.settings.schema, self.settings.name);
                    workspace
                        .refresh_objects(self.settings.schema.clone(), "正在刷新目标数据库对象…");
                }
                Err(error) => {
                    self.phase = TableCopyPhase::Configure;
                    self.status = error;
                }
            }
        }
        if self.fields.is_none()
            && let Some(detail) = workspace
                .detail
                .as_ref()
                .filter(|detail| detail.object == self.object)
            && !detail.columns.is_empty()
        {
            self.fields = Some(
                detail
                    .columns
                    .iter()
                    .map(|column| (column.name.clone(), true))
                    .collect(),
            );
            self.status.clear();
        }

        let mut start = false;
        let mut reload = false;
        let mut close = false;
        let modal = crate::app::style::modal(ctx, egui::Id::new("copy-selected-table-fields"))
            .show(ctx, |ui| {
                ui.set_min_width(460.0);
                ui.heading("复制表");
                ui.monospace(format!(
                    "{}.{}",
                    quote_identifier(&self.object.schema),
                    quote_identifier(&self.object.name)
                ));

                match self.phase {
                    TableCopyPhase::Copying => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("正在复制表…");
                        });
                    }
                    TableCopyPhase::Done => {
                        ui.label(&self.status);
                        close = ui.button("关闭").clicked();
                    }
                    TableCopyPhase::Configure => {
                        ui.horizontal(|ui| {
                            ui.label("目标数据库");
                            ui.add(
                                TextEdit::singleline(&mut self.settings.schema)
                                    .desired_width(235.0),
                            );
                            egui::ComboBox::from_id_salt("selective-copy-schema")
                                .selected_text("选择")
                                .show_ui(ui, |ui| {
                                    for schema in &workspace.schemas {
                                        ui.selectable_value(
                                            &mut self.settings.schema,
                                            schema.clone(),
                                            schema,
                                        );
                                    }
                                });
                        });
                        ui.horizontal(|ui| {
                            ui.label("新表名");
                            ui.add(
                                TextEdit::singleline(&mut self.settings.name)
                                    .char_limit(64)
                                    .desired_width(270.0),
                            );
                        });
                        ui.horizontal_wrapped(|ui| {
                            ui.radio_value(&mut self.settings.include_data, false, "仅结构");
                            ui.radio_value(&mut self.settings.include_data, true, "结构和数据");
                            ui.checkbox(&mut self.settings.include_indexes, "复制索引");
                            let allows_triggers = self.settings.schema.trim() == self.object.schema;
                            self.settings.include_triggers &= allows_triggers;
                            ui.add_enabled(
                                allows_triggers,
                                egui::Checkbox::new(
                                    &mut self.settings.include_triggers,
                                    "复制触发器",
                                ),
                            )
                            .on_disabled_hover_text("复制触发器需要在源数据库中创建新表。");
                        });
                        ui.separator();
                        ui.checkbox(&mut self.all_fields, "包括全部字段");
                        if let Some(fields) = &mut self.fields {
                            egui::ScrollArea::vertical()
                                .max_height(215.0)
                                .show(ui, |ui| {
                                    for (name, selected) in fields {
                                        if self.all_fields {
                                            let mut checked = true;
                                            ui.add_enabled(
                                                false,
                                                egui::Checkbox::new(&mut checked, name.as_str()),
                                            );
                                        } else {
                                            ui.checkbox(selected, name.as_str());
                                        }
                                    }
                                });
                        } else if workspace.busy() {
                            ui.spinner();
                        } else {
                            ui.label(if self.status.is_empty() {
                                workspace.status.as_str()
                            } else {
                                self.status.as_str()
                            });
                            reload = ui.button("重新读取字段").clicked();
                        }
                        if !self.status.is_empty() && self.fields.is_some() {
                            ui.colored_label(style::ERROR, &self.status);
                        }
                        let schema = self.settings.schema.trim();
                        let name = self.settings.name.trim();
                        let has_fields = self.fields.as_ref().is_some_and(|fields| {
                            !fields.is_empty()
                                && (self.all_fields || fields.iter().any(|(_, selected)| *selected))
                        });
                        let ready = has_fields
                            && !schema.is_empty()
                            && !name.is_empty()
                            && (schema != self.object.schema || name != self.object.name)
                            && workspace.server.is_some()
                            && !workspace.busy();
                        ui.horizontal(|ui| {
                            start = ui.add_enabled(ready, style::primary("复制")).clicked();
                            close = ui.button("取消").clicked();
                        });
                    }
                }
            });
        if reload {
            self.load_fields(workspace);
        } else if start {
            self.settings.name = self.settings.name.trim().to_owned();
            self.settings.schema = self.settings.schema.trim().to_owned();
            self.settings.columns = if self.all_fields {
                None
            } else {
                self.fields.as_ref().map(|fields| {
                    fields
                        .iter()
                        .filter(|(_, selected)| *selected)
                        .map(|(column, _)| column.clone())
                        .collect()
                })
            };
            if workspace.send(
                Command::CopyTableAs {
                    object: self.object.clone(),
                    settings: self.settings.clone(),
                },
                "正在准备新表…",
            ) {
                self.phase = TableCopyPhase::Copying;
                self.status.clear();
            } else {
                self.status.clone_from(&workspace.status);
            }
        }
        close || (self.phase != TableCopyPhase::Copying && modal.should_close())
    }
}

enum Phase {
    Configure,
    Dumping {
        directory: TempDir,
        cancelling: bool,
    },
    Restoring {
        directory: TempDir,
        summary: TransferSummary,
    },
    Done(TransferSummary),
}

enum Catalog {
    Waiting,
    Loading,
    Ready,
    Failed(String),
}

enum Action {
    Start,
    Replace,
    Back,
    Stop,
    Close,
}

pub(super) struct Destination {
    workspace: u64,
    connection: u32,
    schema: String,
}

type Endpoint<'a> = (&'a str, u16, Option<(&'a str, u16)>);

fn endpoint(profile: &Profile) -> Option<Endpoint<'_>> {
    // Free-form HTTP and proxy URLs can contain access credentials and routing data.
    profile.http.is_none().then_some((
        profile.host.trim(),
        profile.port,
        profile
            .ssh
            .then_some((profile.ssh_host.trim(), profile.ssh_port)),
    ))
}

fn profile_key(profile: &Profile) -> Option<String> {
    let endpoint = endpoint(profile)?;
    let descriptor = serde_json::to_vec(&(&profile.label, endpoint, profile.tls)).ok()?;
    Some(format!("{:x}", Sha256::digest(descriptor)))
}

fn saved_profile<'a>(key: &str, profiles: &'a [Profile]) -> Option<&'a Profile> {
    let mut matching = profiles
        .iter()
        .filter(|profile| profile_key(profile).as_deref() == Some(key));
    let profile = matching.next()?;
    matching.next().is_none().then_some(profile)
}

fn same_source_database(
    source: &Workspace,
    source_schema: &str,
    target: &Workspace,
    target_schema: &str,
) -> bool {
    if !source_schema.eq_ignore_ascii_case(target_schema) {
        return false;
    }
    let source_endpoint = source.profile.as_ref().and_then(endpoint);
    source.key == target.key
        || (source_endpoint.is_some()
            && source_endpoint == target.profile.as_ref().and_then(endpoint))
}

fn remember_destination(
    workspace: &Workspace,
    schema: &str,
    profiles: &[Profile],
) -> Option<SavedCopyDestination> {
    workspace.server.as_ref()?;
    let profile = workspace.profile.as_ref()?;
    let profile_key = profile_key(profile)?;
    (saved_profile(&profile_key, profiles)? == profile).then(|| SavedCopyDestination {
        profile_key,
        schema: schema.to_owned(),
    })
}

fn recall_destination(
    workspaces: &[Workspace],
    source: usize,
    schema: &str,
    choice: &SavedCopyDestination,
    profiles: &[Profile],
) -> Option<(usize, String)> {
    let profile = saved_profile(&choice.profile_key, profiles)?;
    let mut matching = workspaces.iter().enumerate().filter(|(_, workspace)| {
        workspace.server.is_some() && workspace.profile.as_ref() == Some(profile)
    });
    let (index, target) = matching.next()?;
    if matching.next().is_some()
        || !target.schemas.contains(&choice.schema)
        || same_source_database(&workspaces[source], schema, target, &choice.schema)
    {
        return None;
    }
    Some((index, choice.schema.clone()))
}

pub(super) struct Copy {
    source: usize,
    schema: String,
    initial_object: Option<DbObject>,
    destination: usize,
    destination_schema: String,
    choices: CopyChoices,
    catalog: Catalog,
    checked: Option<HashSet<DbObject>>,
    replacement: Option<Vec<DbObject>>,
    phase: Phase,
    error: String,
    preferences_error: Option<String>,
    show_help: bool,
}

impl Copy {
    pub fn new(
        source: usize,
        schema: String,
        initial_object: Option<DbObject>,
        workspaces: &[Workspace],
        remembered: Option<&Destination>,
    ) -> Self {
        let (choices, preferences_error) = match CopyChoices::load() {
            Ok(choices) => (choices, None),
            Err(error) => (
                CopyChoices::default(),
                Some(format!("无法读取复制选项。{error}")),
            ),
        };
        let (destination, destination_schema) = remembered
            .and_then(|choice| {
                workspaces
                    .iter()
                    .position(|workspace| {
                        workspace.key == choice.workspace
                            && workspace
                                .server
                                .as_ref()
                                .is_some_and(|server| server.connection_id == choice.connection)
                            && !same_source_database(
                                &workspaces[source],
                                &schema,
                                workspace,
                                &choice.schema,
                            )
                    })
                    .map(|index| (index, choice.schema.clone()))
            })
            .or_else(|| {
                let choice = choices.destination.as_ref()?;
                let profiles = load_profiles().ok()?;
                recall_destination(workspaces, source, &schema, choice, &profiles)
            })
            .unwrap_or_else(|| {
                let destination = workspaces
                    .iter()
                    .enumerate()
                    .find(|(index, workspace)| {
                        *index != source
                            && workspace.server.is_some()
                            && !same_source_database(
                                &workspaces[source],
                                &schema,
                                workspace,
                                &schema,
                            )
                    })
                    .map_or(source, |(index, _)| index);
                let target_schema = if source == destination {
                    String::new()
                } else {
                    schema.clone()
                };
                (destination, target_schema)
            });
        Self {
            source,
            schema,
            initial_object,
            destination,
            destination_schema,
            choices,
            catalog: Catalog::Waiting,
            checked: None,
            replacement: None,
            phase: Phase::Configure,
            error: String::new(),
            preferences_error,
            show_help: false,
        }
    }

    fn poll_catalog(&mut self, workspace: &mut Workspace) {
        if workspace.busy()
            || workspace.server.is_none()
            || matches!(self.catalog, Catalog::Failed(_))
        {
            return;
        }
        if let Some(objects) = workspace.objects.get(&self.schema) {
            if !matches!(self.catalog, Catalog::Ready) {
                if let Some(checked) = &mut self.checked {
                    // A refresh on the shared connection can invalidate the source list.
                    let available: HashSet<_> = objects.iter().collect();
                    checked.retain(|object| available.contains(object));
                } else {
                    self.checked = Some(
                        objects
                            .iter()
                            .filter(|object| {
                                self.initial_object
                                    .as_ref()
                                    .is_none_or(|initial| initial == *object)
                            })
                            .cloned()
                            .collect(),
                    );
                }
                self.catalog = Catalog::Ready;
            }
        } else if matches!(self.catalog, Catalog::Loading)
            || !workspace.send(
                Command::ListObjects {
                    schema: self.schema.clone(),
                },
                "正在读取源数据库对象…",
            )
        {
            self.catalog = Catalog::Failed(workspace.status.clone());
        } else {
            self.catalog = Catalog::Loading;
        }
    }

    fn selected_objects<'a>(
        &'a self,
        workspace: &'a Workspace,
    ) -> impl Iterator<Item = &'a DbObject> {
        workspace
            .objects
            .get(&self.schema)
            .into_iter()
            .flatten()
            .filter(move |object| {
                matches!(self.catalog, Catalog::Ready)
                    && self
                        .checked
                        .as_ref()
                        .is_some_and(|checked| checked.contains(*object))
                    && (self.choices.include_structure
                        || (self.choices.include_data && object.kind == ObjectKind::Table))
            })
    }

    fn dump_command(&self, objects: Vec<DbObject>, path: PathBuf) -> Command {
        Command::DumpForCopy {
            schema: self.schema.clone(),
            object: None,
            objects: Some(objects),
            bulk_insert: self.choices.bulk_insert && self.choices.include_data,
            destination_schema: self.destination_schema.trim().to_owned(),
            path,
            options: DumpOptions {
                include_structure: self.choices.include_structure,
                include_data: self.choices.include_data,
                drop_existing: self.choices.include_structure && self.choices.drop_existing,
            },
        }
    }

    fn start(
        &mut self,
        workspaces: &mut [Workspace],
        objects: Vec<DbObject>,
    ) -> Result<(), String> {
        let directory =
            tempfile::tempdir().map_err(|error| format!("无法创建临时转储目录：{error}"))?;
        let cmd = self.dump_command(objects, directory.path().join("copy.sql"));
        let source = &mut workspaces[self.source];
        if !source.send(cmd, "正在读取源端数据库…") {
            return Err(source.status.clone());
        }
        self.phase = Phase::Dumping {
            directory,
            cancelling: false,
        };
        self.error.clear();
        let target = &workspaces[self.destination];
        let target_schema = self.destination_schema.trim();
        self.choices.destination = if same_source_database(
            &workspaces[self.source],
            &self.schema,
            target,
            target_schema,
        ) {
            None
        } else {
            load_profiles()
                .ok()
                .and_then(|profiles| remember_destination(target, target_schema, &profiles))
        };
        self.preferences_error = self
            .choices
            .save()
            .err()
            .map(|error| format!("无法保存复制选项。{error}"));
        Ok(())
    }

    fn advance(&mut self, workspaces: &mut [Workspace]) {
        match self.phase {
            Phase::Dumping { .. } => {
                let Some(result) = workspaces[self.source].dump_result.take() else {
                    return;
                };
                let Phase::Dumping { directory, .. } =
                    std::mem::replace(&mut self.phase, Phase::Configure)
                else {
                    unreachable!()
                };
                let summary = match result {
                    Ok(summary) => summary,
                    Err(error) if error == "已取消 SQL 转储。" => {
                        self.error = "已停止复制。".into();
                        return;
                    }
                    Err(error) => {
                        self.error = format!("读取源数据库失败。{error}");
                        return;
                    }
                };
                let Some(origin) = summary.copy_origin.clone() else {
                    self.error = "无法核实复制来源，请重新连接源数据库后再试。".into();
                    return;
                };
                let destination = &mut workspaces[self.destination];
                let cmd = Command::RestoreForCopy {
                    schema: self.destination_schema.trim().to_owned(),
                    path: directory.path().join("copy.sql"),
                    source_schema: self.schema.clone(),
                    source: origin,
                    dependencies: summary.copy_dependencies.clone(),
                };
                if destination.send(cmd, "正在写入目标端数据库…") {
                    self.phase = Phase::Restoring { directory, summary };
                } else {
                    self.error = format!("无法提交目标端还原。{}", destination.status);
                }
            }
            Phase::Restoring { .. } => {
                let target = &mut workspaces[self.destination];
                let Some(result) = target.transfer_result.take() else {
                    return;
                };
                let Phase::Restoring { directory, summary } =
                    std::mem::replace(&mut self.phase, Phase::Configure)
                else {
                    unreachable!()
                };
                drop(directory);
                let schema = self.destination_schema.trim();
                target.objects.remove(schema);
                target.snapshots.remove(schema);
                target.schema_designs.remove(schema);
                target.tags.invalidate(Some(schema));
                if target
                    .detail
                    .as_ref()
                    .is_some_and(|detail| detail.object.schema == schema)
                {
                    target.detail = None;
                }
                if !target.busy() && target.server.is_some() {
                    target.send(Command::ListSchemas, "正在刷新数据库列表…");
                }
                match result {
                    Ok(()) => self.phase = Phase::Done(summary),
                    Err(error) if error == crate::database::RESTORE_CANCELLED => {
                        self.error = "已停止复制。目标端已提交的更改可能保留。".into();
                    }
                    Err(error) => {
                        self.error = format!("写入目标数据库失败。{error}");
                    }
                }
            }
            Phase::Configure | Phase::Done(_) => {}
        }
    }

    fn ready(&self, workspaces: &[Workspace]) -> bool {
        let source = &workspaces[self.source];
        let target = &workspaces[self.destination];
        let destination_schema = self.destination_schema.trim();
        source.server.is_some()
            && target.server.is_some()
            && !source.busy()
            && !target.busy()
            && !destination_schema.is_empty()
            && !(self.source == self.destination && self.schema == destination_schema)
            && (self.choices.include_structure
                || target
                    .schemas
                    .iter()
                    .any(|schema| schema == destination_schema))
            && self.replacement.as_ref().map_or_else(
                || self.selected_objects(source).next().is_some(),
                |objects| !objects.is_empty(),
            )
    }

    fn object_tree(&mut self, ui: &mut egui::Ui, source: &Workspace, height: f32) {
        ui.strong("要复制的对象");
        let mut retry = false;
        style::pane(egui::Margin::same(6)).show(ui, |ui| {
            match (
                &self.catalog,
                source.objects.get(&self.schema),
                &mut self.checked,
            ) {
                (Catalog::Ready, Some(objects), _) if objects.is_empty() => {
                    ui.label("源数据库没有可复制的对象。");
                }
                (Catalog::Ready, Some(objects), Some(selected)) => {
                    egui::ScrollArea::vertical()
                        .id_salt(("copy-objects", source.key, &self.schema))
                        .max_height(height)
                        .show(ui, |ui| {
                            for kind in OBJECT_KINDS {
                                let group: Vec<_> = objects
                                    .iter()
                                    .filter(|object| object.kind == kind)
                                    .collect();
                                let eligible = self.choices.include_structure
                                    || (self.choices.include_data && kind == ObjectKind::Table);
                                object_checkbox_group(
                                    ui,
                                    kind,
                                    &group,
                                    selected,
                                    eligible,
                                    kind == ObjectKind::Table,
                                );
                            }
                        });
                }
                (Catalog::Failed(error), _, _) => {
                    ui.colored_label(style::ERROR, error.as_str());
                    retry = ui.button("重新读取对象").clicked();
                }
                _ if source.server.is_none() => {
                    ui.label("请先连接源数据库。");
                }
                _ => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("正在读取源数据库对象…");
                    });
                }
            }
        });
        if retry {
            self.catalog = Catalog::Waiting;
            ui.ctx().request_repaint();
        }
    }

    fn configure(
        &mut self,
        ui: &mut egui::Ui,
        workspaces: &[Workspace],
        tree_height: f32,
    ) -> Option<Action> {
        ui.horizontal(|ui| {
            ui.label("目标连接");
            egui::ComboBox::from_id_salt("copy-destination")
                .selected_text(&workspaces[self.destination].label)
                .show_ui(ui, |ui| {
                    for (index, workspace) in workspaces.iter().enumerate() {
                        if workspace.server.is_some() {
                            ui.selectable_value(&mut self.destination, index, &workspace.label);
                        }
                    }
                });
        });
        ui.horizontal(|ui| {
            ui.label("目标数据库");
            ui.add(
                TextEdit::singleline(&mut self.destination_schema)
                    .hint_text("选择已有数据库或输入新库名"),
            );
            egui::ComboBox::from_id_salt("copy-schema")
                .selected_text("选择")
                .show_ui(ui, |ui| {
                    for schema in &workspaces[self.destination].schemas {
                        ui.selectable_value(&mut self.destination_schema, schema.clone(), schema);
                    }
                });
        });
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut self.choices.include_structure, "复制对象结构");
            ui.checkbox(&mut self.choices.include_data, "复制表数据");
        });
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled(
                self.choices.include_data,
                egui::Checkbox::new(&mut self.choices.bulk_insert, "批量插入数据"),
            );
            ui.add_enabled(
                self.choices.include_structure,
                egui::Checkbox::new(&mut self.choices.drop_existing, "覆盖目标中的同名对象"),
            );
        });
        let source = &workspaces[self.source];
        self.object_tree(ui, source, tree_height);
        let destination_schema = self.destination_schema.trim();
        if !destination_schema.is_empty()
            && self.schema != destination_schema
            && self.choices.include_structure
        {
            ui.label(
                RichText::new("目标库名与源库不同时，可确认的源库引用会改为指向目标库。")
                    .color(style::MUTED),
            );
        }
        if source
            .server
            .as_ref()
            .is_some_and(|server| server.http_tunnel)
        {
            ui.label(
                RichText::new("HTTP 隧道按批读取。复制期间请保持源库数据和结构稳定。")
                    .color(style::MUTED),
            );
        }
        if !self.error.is_empty() {
            ui.colored_label(style::ERROR, &self.error);
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.ready(workspaces), style::primary("开始复制"))
                .clicked()
            {
                Some(Action::Start)
            } else if ui.button("取消").clicked() {
                Some(Action::Close)
            } else {
                None
            }
        })
        .inner
    }

    fn confirm_replacement(
        &self,
        ui: &mut egui::Ui,
        workspaces: &[Workspace],
        objects: &[DbObject],
        height: f32,
    ) -> Option<Action> {
        ui.strong("确认覆盖目标对象");
        let target = &workspaces[self.destination];
        let label = target.profile.as_ref().map_or_else(
            || target.label.clone(),
            |profile| format!("{}（{}:{}）", target.label, profile.host, profile.port),
        );
        ui.label(format!("目标连接 {label}"));
        ui.label(format!(
            "目标数据库 {}",
            quote_identifier(self.destination_schema.trim())
        ));
        ui.label(format!(
            "以下 {} 个对象如果在目标库中同名，将先删除再复制。目标表中原有的数据也会被删除。",
            objects.len()
        ));
        if objects
            .iter()
            .any(|object| object.kind == ObjectKind::Table)
        {
            ui.label("覆盖表也会删除目标表的触发器。本次复制会带入勾选的源触发器。");
        }
        egui::ScrollArea::vertical()
            .id_salt("replace-copy-objects")
            .max_height(height)
            .show_rows(
                ui,
                ui.spacing().interact_size.y,
                objects.len(),
                |ui, range| {
                    for object in &objects[range] {
                        let label = format!("{}  {}", kind_label(object.kind), object.name);
                        ui.add(egui::Label::new(&label).truncate())
                            .on_hover_text(label);
                    }
                },
            );
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.ready(workspaces), style::primary("覆盖并复制"))
                .clicked()
            {
                Some(Action::Replace)
            } else if ui.button("返回修改").clicked() {
                Some(Action::Back)
            } else {
                None
            }
        })
        .inner
    }

    fn progress(&self, ui: &mut egui::Ui, workspaces: &[Workspace]) -> Option<Action> {
        let (status, fraction, cancellable, target_phase) = match &self.phase {
            Phase::Dumping { cancelling, .. } => {
                let source = &workspaces[self.source];
                let progress = source.dump_progress.as_ref();
                let status = if *cancelling {
                    "正在停止复制…".into()
                } else if let Some(progress) = progress {
                    let object = progress.object.as_deref().unwrap_or("源数据库");
                    format!(
                        "正在读取 {object}，已完成 {}/{} 个对象、{} 行数据",
                        progress.completed_objects, progress.total_objects, progress.rows
                    )
                } else {
                    "正在读取源数据库…".into()
                };
                let fraction = progress
                    .filter(|item| item.total_objects > 0)
                    .map_or(0.0, |item| {
                        (item.completed_objects as f32 / item.total_objects as f32).min(0.99)
                    });
                (
                    status,
                    fraction,
                    !*cancelling && progress.is_none_or(|item| !item.finishing),
                    false,
                )
            }
            Phase::Restoring { .. } => {
                let target = &workspaces[self.destination];
                let status = target
                    .restore_status()
                    .unwrap_or_else(|| "正在写入目标数据库…".into());
                let fraction = target
                    .restore_progress
                    .filter(|item| item.total_bytes > 0)
                    .map_or(0.0, |item| {
                        (item.bytes as f64 / item.total_bytes as f64).clamp(0.0, 1.0) as f32
                    });
                (status, fraction, target.can_stop_restore(), true)
            }
            _ => unreachable!(),
        };
        ui.add(egui::Label::new(&status).truncate())
            .on_hover_text(status);
        ui.add(egui::ProgressBar::new(fraction).show_percentage());
        let mut button = ui.add_enabled(cancellable, egui::Button::new("停止复制"));
        if target_phase {
            button = button.on_hover_text("停止提交后续 SQL。目标端已提交的更改可能保留。");
        }
        button.clicked().then_some(Action::Stop)
    }
}

pub(super) fn dialog(
    ctx: &egui::Context,
    form: &mut Option<Copy>,
    workspaces: &mut [Workspace],
    remembered: &mut Option<Destination>,
) {
    let Some(copy) = form else { return };
    copy.advance(workspaces);
    if matches!(copy.phase, Phase::Configure) && copy.replacement.is_none() {
        copy.poll_catalog(&mut workspaces[copy.source]);
    }
    let viewport = ctx.content_rect();
    let tree_height = (viewport.height() - 320.0).clamp(115.0, 300.0);
    let modal = crate::app::style::modal(ctx, egui::Id::new("copy-database-objects")).show(ctx, |ui| {
        ui.set_width((viewport.width() - 48.0).clamp(380.0, 560.0));
        ui.horizontal(|ui| {
            ui.heading("复制数据库对象");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("帮助").on_hover_text("F1").clicked()
                    || ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::F1))
                {
                    copy.show_help = !copy.show_help;
                }
            });
        });
        let source = &workspaces[copy.source];
        let schema = quote_identifier(&copy.schema);
        let user = source.profile.as_ref().filter(|profile| !profile.user.is_empty())
            .map_or_else(String::new, |profile| format!("（用户 {}）", profile.user));
        ui.label(format!("来源  {} / {schema}{user}", source.label));
        if copy.show_help {
            ui.label("勾选对象后选择目标连接和数据库。仅复制数据时，目标表须已存在。覆盖同名对象前会再次确认。");
            ui.separator();
        }
        if let Some(error) = &copy.preferences_error {
            ui.colored_label(style::ERROR, error);
        }
        match &copy.phase {
            Phase::Configure => {
                if let Some(objects) = &copy.replacement {
                    copy.confirm_replacement(ui, workspaces, objects, tree_height)
                } else {
                    copy.configure(ui, workspaces, tree_height)
                }
            }
            Phase::Done(summary) => {
                ui.strong("复制概要");
                ui.label(format!("已复制 {} 个对象，共 {} 行数据。", summary.objects, summary.rows));
                ui.label(format!("传输 SQL {} 字节", summary.bytes));
                ui.add(style::primary("完成")).clicked().then_some(Action::Close)
            }
            Phase::Dumping { .. } | Phase::Restoring { .. } => copy.progress(ui, workspaces),
        }
    });
    let dismissed = modal.should_close();
    let action = modal.inner.or_else(|| {
        if !dismissed {
            return None;
        }
        match copy.phase {
            Phase::Configure if copy.replacement.is_some() => Some(Action::Back),
            Phase::Configure | Phase::Done(_) => Some(Action::Close),
            Phase::Dumping { .. } | Phase::Restoring { .. } => None,
        }
    });
    let objects = match action {
        Some(Action::Start) => {
            let objects = copy
                .selected_objects(&workspaces[copy.source])
                .cloned()
                .collect();
            if copy.choices.include_structure && copy.choices.drop_existing {
                copy.replacement = Some(objects);
                None
            } else {
                Some(objects)
            }
        }
        Some(Action::Replace) => copy.replacement.take(),
        Some(Action::Back) => {
            copy.replacement = None;
            None
        }
        Some(Action::Stop) => {
            match &mut copy.phase {
                Phase::Dumping { cancelling, .. } => {
                    *cancelling = workspaces[copy.source].cancel_dump()
                }
                Phase::Restoring { .. } => workspaces[copy.destination].stop_restore(),
                _ => {}
            }
            None
        }
        Some(Action::Close) => {
            *form = None;
            return;
        }
        None => None,
    };
    if let Some(objects) = objects {
        match copy.start(workspaces, objects) {
            Ok(()) => {
                let target = &workspaces[copy.destination];
                *remembered = target.server.as_ref().map(|server| Destination {
                    workspace: target.key,
                    connection: server.connection_id,
                    schema: copy.destination_schema.trim().to_owned(),
                });
            }
            Err(error) => copy.error = error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::ServerInfo;

    #[test]
    fn saved_copy_destination_reopens_with_the_connected_profile() {
        let profile = Profile {
            label: "归档服务器".into(),
            host: "192.0.2.20".into(),
            ..Default::default()
        };
        let profiles = [profile.clone()];
        let connect = || {
            let mut workspace = Workspace::new(profile.label.clone(), Some(profile.clone()));
            workspace.server = Some(ServerInfo {
                version: "MariaDB".into(),
                connection_id: 2,
                database: Some("stock_archive".into()),
                http_tunnel: false,
            });
            workspace.schemas.push("stock_archive".into());
            workspace
        };
        let first = connect();
        let preferences = CopyChoices {
            destination: remember_destination(&first, "stock_archive", &profiles),
            ..Default::default()
        };
        let stored = serde_json::to_string(&preferences).unwrap();
        let reloaded: CopyChoices = serde_json::from_str(&stored).unwrap();
        let target = reloaded.destination.as_ref().unwrap();
        assert_eq!(target.schema, "stock_archive");
        assert_eq!(target.profile_key.len(), 64);
        let source = Workspace::new("来源".into(), Some(Profile::default()));
        let workspaces = [source, connect()];
        assert_ne!(workspaces[1].key, first.key);
        assert_eq!(
            recall_destination(&workspaces, 0, "stock", target, &profiles),
            Some((1, "stock_archive".into()))
        );
    }

    #[test]
    fn cross_database_copy_selection_preserves_table_entry_and_mode() {
        let mut source = Workspace::new("来源".into(), None);
        source.server = Some(ServerInfo {
            version: "MariaDB".into(),
            connection_id: 1,
            database: Some("stock".into()),
            http_tunnel: false,
        });
        let object = |name: &str, kind| DbObject {
            schema: "stock".into(),
            name: name.into(),
            kind,
        };
        let inventory = object("inventory", ObjectKind::Table);
        let warehouse = object("warehouse", ObjectKind::Table);
        let inventory_view = object("current_inventory", ObjectKind::View);
        let refresh_procedure = object("refresh_inventory", ObjectKind::Procedure);
        source.objects.insert(
            "stock".into(),
            vec![
                inventory.clone(),
                warehouse.clone(),
                inventory_view.clone(),
                refresh_procedure.clone(),
            ],
        );
        let mut workspaces = [source];
        let mut copy = Copy::new(
            0,
            "stock".into(),
            Some(inventory.clone()),
            &workspaces,
            None,
        );
        copy.choices = CopyChoices::default();
        copy.destination_schema = "stock_archive".into();
        copy.poll_catalog(&mut workspaces[0]);
        assert_eq!(
            copy.selected_objects(&workspaces[0]).collect::<Vec<_>>(),
            [&inventory]
        );

        let checked = copy.checked.as_mut().expect("expected source objects");
        checked.extend([
            warehouse.clone(),
            inventory_view.clone(),
            refresh_procedure.clone(),
        ]);
        assert!(copy.ready(&workspaces));
        let selected = copy.selected_objects(&workspaces[0]).cloned().collect();
        let command = copy.dump_command(selected, PathBuf::from("copy.sql"));
        assert!(matches!(
            command,
            Command::DumpForCopy { object: None, objects: Some(objects), schema, destination_schema, options, .. }
                if objects.iter().eq([&inventory, &warehouse, &inventory_view, &refresh_procedure])
                    && schema == "stock" && destination_schema == "stock_archive"
                    && options.include_structure && options.include_data
        ));

        workspaces[0].schemas.push("stock_archive".into());
        copy.choices.include_structure = false;
        assert!(copy.ready(&workspaces));
        let selected = copy.selected_objects(&workspaces[0]).cloned().collect();
        let command = copy.dump_command(selected, PathBuf::from("copy.sql"));
        assert!(matches!(
            command,
            Command::DumpForCopy { object: None, objects: Some(objects), destination_schema, bulk_insert: true, options, .. }
                if objects.iter().eq([&inventory, &warehouse]) && destination_schema == "stock_archive"
                    && !options.include_structure && options.include_data
        ));

        // The refreshed server catalog keeps the table selection and leaves new objects unchecked.
        copy.catalog = Catalog::Loading;
        workspaces[0].objects.insert(
            "stock".into(),
            vec![
                inventory.clone(),
                warehouse.clone(),
                object("new_table", ObjectKind::Table),
            ],
        );
        copy.poll_catalog(&mut workspaces[0]);
        assert_eq!(
            copy.selected_objects(&workspaces[0]).collect::<Vec<_>>(),
            [&inventory, &warehouse]
        );
    }
}
