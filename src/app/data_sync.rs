use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use eframe::egui::{self, RichText};
use mysql::Value;

use crate::database::{Command, DbObject, ObjectKind, TableColumn, TablePage, quote_identifier};

use super::{output::display_cell, state::Workspace, style};

#[path = "data_sync_direct.rs"]
mod direct;

const MAX_ROWS: usize = 1_000;
const MAX_SCRIPT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq)]
struct Endpoint {
    workspace: u64,
    schema: String,
    table: String,
}

impl Endpoint {
    fn from_workspace(workspace: &Workspace, table: String) -> Self {
        let schema = workspace
            .server
            .as_ref()
            .and_then(|server| server.database.clone())
            .or_else(|| workspace.schemas.first().cloned())
            .unwrap_or_default();
        Self {
            workspace: workspace.key,
            schema,
            table,
        }
    }

    fn index(&self, workspaces: &[Workspace]) -> Option<usize> {
        workspaces
            .iter()
            .position(|workspace| workspace.key == self.workspace)
    }

    fn object(&self) -> DbObject {
        DbObject {
            schema: self.schema.clone(),
            name: self.table.clone(),
            kind: ObjectKind::Table,
        }
    }

    fn ready(&self, workspaces: &[Workspace]) -> bool {
        !self.schema.is_empty()
            && !self.table.is_empty()
            && self.index(workspaces).is_some_and(|index| {
                workspaces[index].server.is_some() && !workspaces[index].busy()
            })
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Route {
    ToTarget,
    ToSource,
    Both,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Conflict {
    Skip,
    Source,
    Target,
}

enum ReadState {
    Idle,
    Source,
    Target(TablePage),
}

pub(super) struct DataSync {
    source: Endpoint,
    target: Endpoint,
    reading: ReadState,
    compared: Option<Compared>,
    route: Route,
    conflict: Conflict,
    error: Option<String>,
    direct: direct::Direct,
    use_sync_wizard: bool,
    tables: Option<BTreeSet<String>>,
}

impl DataSync {
    pub(super) fn new(index: usize, selected_table: DbObject, workspaces: &[Workspace]) -> Self {
        let source = Endpoint {
            workspace: workspaces[index].key,
            schema: selected_table.schema.clone(),
            table: selected_table.name.clone(),
        };
        let peer = workspaces
            .iter()
            .enumerate()
            .find(|(candidate, workspace)| *candidate != index && workspace.server.is_some())
            .map_or(index, |(candidate, _)| candidate);
        let mut target = Endpoint::from_workspace(&workspaces[peer], selected_table.name);
        if target == source
            && let Some(schema) = workspaces[peer]
                .schemas
                .iter()
                .find(|schema| **schema != source.schema)
        {
            target.schema = schema.clone();
        }
        Self {
            source,
            target,
            reading: ReadState::Idle,
            compared: None,
            route: Route::ToTarget,
            conflict: Conflict::Skip,
            error: None,
            direct: direct::Direct::default(),
            use_sync_wizard: true,
            tables: None,
        }
    }

    pub(super) fn for_schema(index: usize, schema: String, workspaces: &[Workspace]) -> Self {
        let mut sync = Self::new(
            index,
            DbObject {
                schema,
                name: String::new(),
                kind: ObjectKind::Table,
            },
            workspaces,
        );
        sync.tables = Some(BTreeSet::new());
        sync
    }

    fn poll(&mut self, workspaces: &mut [Workspace]) {
        let endpoint = match &self.reading {
            ReadState::Idle => return,
            ReadState::Source => &self.source,
            ReadState::Target(_) => &self.target,
        };
        let Some(index) = endpoint.index(workspaces) else {
            self.reading = ReadState::Idle;
            self.error = Some("所选连接已关闭。".into());
            return;
        };
        if workspaces[index].busy() {
            return;
        }
        let state = std::mem::replace(&mut self.reading, ReadState::Idle);
        let page = workspaces[index].row_page.take();
        let Some(page) = page.filter(|page| page.object == endpoint.object() && page.offset == 0)
        else {
            self.error = Some(format!("读取表数据失败：{}", workspaces[index].status));
            return;
        };
        if page.has_more {
            self.error = Some(format!(
                "{} 未完整读入。数据比较最多读取每端 {MAX_ROWS} 行。",
                page.object.name
            ));
            return;
        }
        match state {
            ReadState::Source => match request_page(&self.target, workspaces) {
                Ok(()) => self.reading = ReadState::Target(page),
                Err(error) => self.error = Some(error),
            },
            ReadState::Target(source) => match Compared::new(source, page) {
                Ok(compared) => self.compared = Some(compared),
                Err(error) => self.error = Some(error),
            },
            ReadState::Idle => {}
        }
    }

    fn stage(&self, workspaces: &[Workspace]) -> Result<Vec<(usize, String)>, String> {
        let compared = self.compared.as_ref().ok_or("请先比较数据。")?;
        let mut plans = Vec::new();
        for (forward, endpoint) in [(true, &self.target), (false, &self.source)] {
            let allowed = match self.route {
                Route::ToTarget => forward,
                Route::ToSource => !forward,
                Route::Both => true,
            };
            if !allowed {
                continue;
            }
            let index = endpoint.index(workspaces).ok_or("所选连接已关闭。")?;
            if workspaces[index].server.is_none() || workspaces[index].busy() {
                return Err("请在两端连接可用时生成 SQL。".into());
            }
            let updates = self.conflict
                == if forward {
                    Conflict::Source
                } else {
                    Conflict::Target
                };
            if let Some(sql) = compared.script(forward, updates)? {
                plans.push((index, sql));
            }
        }
        if plans.is_empty() {
            return Err("当前方向和冲突选项没有可生成 SQL 的差异。".into());
        }
        Ok(plans)
    }
}

fn request_page(endpoint: &Endpoint, workspaces: &mut [Workspace]) -> Result<(), String> {
    let index = endpoint.index(workspaces).ok_or("所选连接已关闭。")?;
    let workspace = &mut workspaces[index];
    if workspace.server.is_none() || workspace.busy() {
        return Err("所选连接当前不可用。".into());
    }
    workspace.row_page = None;
    workspace
        .send(
            Command::TablePage {
                object: endpoint.object(),
                limit: MAX_ROWS,
                offset: 0,
            },
            "正在读取对比数据…",
        )
        .then_some(())
        .ok_or_else(|| format!("读取表数据失败：{}", workspace.status))
}

pub(super) fn dialog(
    ctx: &egui::Context,
    state: &mut Option<DataSync>,
    workspaces: &mut [Workspace],
) -> Option<usize> {
    let sync = state.as_mut()?;
    sync.poll(workspaces);
    let active = !matches!(sync.reading, ReadState::Idle) || sync.direct.active();
    let mut start = false;
    let mut stage = false;
    let mut close = false;
    crate::app::style::modal(ctx, egui::Id::new("table-data-sync")).show(ctx, |ui| {
        ui.set_width((ctx.content_rect().width() - 60.0).clamp(540.0, 880.0));
        ui.horizontal(|ui| {
            ui.heading(if sync.tables.is_some() {
                "多表数据同步"
            } else {
                "比较与同步表数据"
            });
            if sync.tables.is_none() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_enabled_ui(!active, |ui| {
                        style::subtle_card(egui::Margin::symmetric(4, 2)).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.selectable_value(
                                    &mut sync.use_sync_wizard,
                                    true,
                                    "整表同步向导",
                                );
                                ui.selectable_value(
                                    &mut sync.use_sync_wizard,
                                    false,
                                    "小表逐行对照",
                                );
                            });
                        });
                    });
                });
            }
        });
        ui.add_space(8.0);
        let previous = (sync.source.clone(), sync.target.clone());
        ui.add_enabled_ui(!active, |ui| {
            let select_table = sync.tables.is_none();
            style::split_two_columns(
                ui,
                0.5,
                12.0,
                |ui| {
                    style::section_card(ui, "源端", |ui| {
                        endpoint_fields(ui, "源端", &mut sync.source, select_table, workspaces);
                    });
                },
                |ui| {
                    style::section_card(ui, "目标端", |ui| {
                        endpoint_fields(ui, "目标端", &mut sync.target, select_table, workspaces);
                    });
                },
            );
        });
        ui.add_space(8.0);
        if previous != (sync.source.clone(), sync.target.clone()) {
            sync.compared = None;
            sync.error = None;
            sync.direct.reset_endpoints();
            if let Some(tables) = &mut sync.tables { tables.clear(); }
        }
        if let Some(tables) = &mut sync.tables {
            let before = tables.clone();
            ui.add_enabled_ui(!active, |ui| select_tables(ui, &sync.source, &sync.target, tables, workspaces));
            if before != *tables { sync.direct.clear(); }
        }
        if sync.use_sync_wizard {
            sync.direct.show(ui, &sync.source, &sync.target, sync.tables.as_ref(), workspaces);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = ui.add_enabled(!sync.direct.active(), egui::Button::new("关闭")).clicked();
            });
            return;
        }
        ui.horizontal(|ui| {
            let ready = sync.source != sync.target && sync.source.ready(workspaces) && sync.target.ready(workspaces);
            start = ui.add_enabled(ready && !active, style::primary("比较数据")).clicked();
            if active {
                ui.spinner();
                ui.label("正在读取两端数据…");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = ui.button("关闭").clicked();
            });
        });
        if let Some(compared) = &sync.compared {
            ui.separator();
            compared.show(ui, ctx.content_rect().height());
            let previous = (sync.route, sync.conflict);
            ui.horizontal_wrapped(|ui| {
                ui.label("方向");
                ui.selectable_value(&mut sync.route, Route::ToTarget, "源 → 目标");
                ui.selectable_value(&mut sync.route, Route::ToSource, "目标 → 源");
                ui.selectable_value(&mut sync.route, Route::Both, "双向补齐");
            });
            if (sync.route == Route::ToSource && sync.conflict == Conflict::Source)
                || (sync.route == Route::ToTarget && sync.conflict == Conflict::Target)
            {
                sync.conflict = Conflict::Skip;
            }
            ui.horizontal_wrapped(|ui| {
                ui.label("同键差异");
                ui.selectable_value(&mut sync.conflict, Conflict::Skip, "保留两端原值");
                ui.add_enabled_ui(sync.route != Route::ToSource, |ui| {
                    ui.selectable_value(&mut sync.conflict, Conflict::Source, "使用源端值");
                });
                ui.add_enabled_ui(sync.route != Route::ToTarget, |ui| {
                    ui.selectable_value(&mut sync.conflict, Conflict::Target, "使用目标端值");
                });
            });
            if previous != (sync.route, sync.conflict) {
                sync.error = None;
            }
            if let Some(reason) = compared.staging_blocker() {
                ui.label(RichText::new(reason).color(style::muted()));
            } else {
                ui.label(RichText::new("SQL 仅包含新增与选定的更新。请在对应连接中分别审阅和执行，两端提交相互独立。").color(style::muted()));
                stage = ui.add_enabled(!compared.differences.is_empty(), style::primary("在编辑器中打开同步 SQL")).clicked();
            }
        }
        if let Some(error) = &sync.error {
            ui.colored_label(style::error(), error);
        }
    });
    if close {
        *state = None;
    } else if start {
        sync.compared = None;
        sync.error = None;
        match request_page(&sync.source, workspaces) {
            Ok(()) => sync.reading = ReadState::Source,
            Err(error) => sync.error = Some(error),
        }
    } else if stage {
        match sync.stage(workspaces) {
            Ok(plans) => {
                let next_active = plans.first().map(|(index, _)| *index);
                for (index, sql) in plans {
                    workspaces[index].new_tab(sql, None);
                }
                *state = None;
                return next_active;
            }
            Err(error) => sync.error = Some(error),
        }
    }
    None
}

fn endpoint_fields(
    ui: &mut egui::Ui,
    label: &str,
    endpoint: &mut Endpoint,
    select_table: bool,
    workspaces: &[Workspace],
) {
    ui.horizontal_wrapped(|ui| {
        ui.strong(label);
        let selected = endpoint
            .index(workspaces)
            .map_or("请选择连接", |index| workspaces[index].label.as_str());
        egui::ComboBox::from_id_salt((label, "connection"))
            .selected_text(selected)
            .show_ui(ui, |ui| {
                for workspace in workspaces
                    .iter()
                    .filter(|workspace| workspace.server.is_some())
                {
                    if ui
                        .selectable_label(endpoint.workspace == workspace.key, &workspace.label)
                        .clicked()
                    {
                        *endpoint = Endpoint::from_workspace(workspace, endpoint.table.clone());
                    }
                }
            });
        let Some(index) = endpoint.index(workspaces) else {
            return;
        };
        let workspace = &workspaces[index];
        let schemas = &workspace.schemas;
        if schemas.is_empty() {
            ui.add(
                egui::TextEdit::singleline(&mut endpoint.schema)
                    .desired_width(145.0)
                    .hint_text("数据库名"),
            );
        } else {
            egui::ComboBox::from_id_salt((label, "schema"))
                .selected_text(if endpoint.schema.is_empty() {
                    "请选择数据库"
                } else {
                    &endpoint.schema
                })
                .show_ui(ui, |ui| {
                    for schema in schemas {
                        ui.selectable_value(&mut endpoint.schema, schema.clone(), schema);
                    }
                });
        }
        if !select_table {
            return;
        }
        ui.add(
            egui::TextEdit::singleline(&mut endpoint.table)
                .desired_width(170.0)
                .hint_text("表名"),
        );
        if let Some(objects) = workspace.objects.get(&endpoint.schema) {
            egui::ComboBox::from_id_salt((label, "table"))
                .selected_text("已加载的表")
                .show_ui(ui, |ui| {
                    for object in objects
                        .iter()
                        .filter(|object| object.kind == ObjectKind::Table)
                    {
                        ui.selectable_value(&mut endpoint.table, object.name.clone(), &object.name);
                    }
                });
        }
    });
}

fn select_tables(
    ui: &mut egui::Ui,
    source: &Endpoint,
    target: &Endpoint,
    selected: &mut BTreeSet<String>,
    workspaces: &mut [Workspace],
) {
    let sides = [(source, "读取源端表"), (target, "读取目标端表")];
    let mut pending = None;
    let mut load_needed = false;
    ui.horizontal_wrapped(|ui| {
        for (endpoint, label) in sides {
            let Some(index) = endpoint.index(workspaces) else {
                continue;
            };
            let workspace = &workspaces[index];
            if endpoint.schema.is_empty() || workspace.objects.contains_key(&endpoint.schema) {
                continue;
            }
            load_needed = true;
            if ui
                .add_enabled(
                    workspace.server.is_some() && !workspace.busy(),
                    egui::Button::new(label),
                )
                .clicked()
            {
                pending = Some((index, endpoint.schema.clone()));
            } else if workspace.busy() {
                ui.spinner();
            }
        }
    });
    if let Some((index, schema)) = pending {
        workspaces[index].send(Command::ListObjects { schema }, "正在读取数据库表…");
    }
    if load_needed {
        return;
    }
    let tables = |endpoint: &Endpoint| -> BTreeSet<String> {
        endpoint
            .index(workspaces)
            .and_then(|index| workspaces[index].objects.get(&endpoint.schema))
            .into_iter()
            .flatten()
            .filter(|object| object.kind == ObjectKind::Table)
            .map(|object| object.name.clone())
            .collect()
    };
    let left = tables(source);
    let right = tables(target);
    let shared: BTreeSet<_> = left.intersection(&right).cloned().collect();
    selected.retain(|name| shared.contains(name));
    if shared.is_empty() {
        ui.label(RichText::new("两端没有可同步的同名表。").color(style::muted()));
        return;
    }
    ui.separator();
    ui.horizontal(|ui| {
        ui.label(format!(
            "选中 {} / {} 张同名表",
            selected.len(),
            shared.len()
        ));
        if ui.button("全选").clicked() {
            selected.clone_from(&shared);
        }
        if ui.button("清空选择").clicked() {
            selected.clear();
        }
    });
    egui::ScrollArea::vertical()
        .id_salt("bulk-sync-tables")
        .max_height(132.0)
        .show(ui, |ui| {
            for name in shared {
                let mut choice = selected.contains(&name);
                if ui.checkbox(&mut choice, &name).changed() {
                    if choice {
                        selected.insert(name);
                    } else {
                        selected.remove(&name);
                    }
                }
            }
        });
}

#[derive(Default)]
struct Pair {
    source: Option<usize>,
    target: Option<usize>,
}

struct Compared {
    source: TablePage,
    target: TablePage,
    differences: Vec<Pair>,
    source_only: usize,
    target_only: usize,
    conflicts: usize,
    same: usize,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum KeyValue {
    Bytes(Vec<u8>),
    Int(i64),
    UInt(u64),
    Float(u32),
    Double(u64),
    Temporal(String),
}

impl Compared {
    fn new(source: TablePage, target: TablePage) -> Result<Self, String> {
        if source.offset != 0 || target.offset != 0 || source.has_more || target.has_more {
            return Err("需要两端完整表数据才能比较。".into());
        }
        if source.columns != target.columns || source.primary_key != target.primary_key {
            return Err("两端的列、类型、排序规则或主键不同，请先同步结构。".into());
        }
        if source.primary_key.is_empty() {
            return Err(
                "小表逐行对照需要完整主键。可用「整表同步向导」按两端共有的非空唯一索引匹配。"
                    .into(),
            );
        }
        let mut indexed = BTreeMap::<Vec<KeyValue>, Pair>::new();
        for (page, forward) in [(&source, true), (&target, false)] {
            for (index, row) in page.rows.iter().enumerate() {
                let values = row.original();
                if values.len() != page.columns.len() {
                    return Err("读取的行与列数不一致，请刷新数据。".into());
                }
                let key = page
                    .primary_key
                    .iter()
                    .map(|&part| key_value(&values[part]))
                    .collect::<Result<_, _>>()?;
                let pair = indexed.entry(key).or_default();
                let slot = if forward {
                    &mut pair.source
                } else {
                    &mut pair.target
                };
                if slot.replace(index).is_some() {
                    return Err("数据中存在重复主键，请刷新后重试。".into());
                }
            }
        }
        let mut differences = Vec::new();
        let (mut source_only, mut target_only, mut conflicts, mut same) = (0, 0, 0, 0);
        for pair in indexed.into_values() {
            match (pair.source, pair.target) {
                (Some(left), Some(right))
                    if source.rows[left]
                        .original()
                        .iter()
                        .zip(target.rows[right].original())
                        .all(|(left, right)| equal_value(left, right)) =>
                {
                    same += 1;
                    continue;
                }
                (Some(_), Some(_)) => conflicts += 1,
                (Some(_), None) => source_only += 1,
                (None, Some(_)) => target_only += 1,
                (None, None) => continue,
            }
            differences.push(pair);
        }
        Ok(Self {
            source,
            target,
            differences,
            source_only,
            target_only,
            conflicts,
            same,
        })
    }

    fn show(&self, ui: &mut egui::Ui, height: f32) {
        ui.label(format!(
            "一致 {} 行   ·   仅源端 {} 行   ·   仅目标端 {} 行   ·   同键数据不同 {} 行",
            self.same, self.source_only, self.target_only, self.conflicts
        ));
        if self.differences.is_empty() {
            return;
        }
        egui::ScrollArea::vertical()
            .max_height((height * 0.32).clamp(90.0, 250.0))
            .show_rows(ui, 23.0, self.differences.len(), |ui, visible| {
                for index in visible {
                    let pair = &self.differences[index];
                    let (page, row, status) = match (pair.source, pair.target) {
                        (Some(row), Some(_)) => (&self.source, row, "同键数据不同"),
                        (Some(row), None) => (&self.source, row, "仅源端"),
                        (None, Some(row)) => (&self.target, row, "仅目标端"),
                        (None, None) => continue,
                    };
                    let key = page
                        .primary_key
                        .iter()
                        .map(|&column| {
                            format!(
                                "{} = {}",
                                page.columns[column].name,
                                display_cell(&page.rows[row].cells[column], true)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("，");
                    ui.add(egui::Label::new(format!("{status}   {key}")).truncate());
                }
            });
    }

    fn staging_blocker(&self) -> Option<String> {
        if let Some(reason) = self
            .source
            .edit_error
            .as_ref()
            .or(self.target.edit_error.as_ref())
        {
            return Some(reason.clone());
        }
        let column = self
            .source
            .columns
            .iter()
            .find(|column| column_kind(column).is_none())?;
        if column.data_type.starts_with("timestamp") {
            Some(format!(
                "列「{}」按各端会话时区读取，包含 TIMESTAMP 的表暂仅支持比较。",
                column.name
            ))
        } else {
            Some(format!(
                "列「{}」的类型 {} 暂仅支持比较。",
                column.name, column.data_type
            ))
        }
    }

    fn script(&self, forward: bool, updates: bool) -> Result<Option<String>, String> {
        if let Some(reason) = self.staging_blocker() {
            return Err(reason);
        }
        let (origin, destination) = if forward {
            (&self.source, &self.target)
        } else {
            (&self.target, &self.source)
        };
        let qualified = format!(
            "{}.{}",
            quote_identifier(&destination.object.schema),
            quote_identifier(&destination.object.name)
        );
        let columns = destination
            .columns
            .iter()
            .map(|column| {
                Ok(SqlColumn {
                    name: quote_identifier(&column.name),
                    category: column_kind(column).ok_or("列类型不支持同步 SQL。")?,
                    writable: column.writable,
                    auto_increment: column.auto_increment,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let mut sql = String::new();
        for pair in &self.differences {
            let (origin_row, dest_row) = if forward {
                (pair.source, pair.target)
            } else {
                (pair.target, pair.source)
            };
            let Some(origin_row) = origin_row else {
                continue;
            };
            let incoming = origin.rows[origin_row].original();
            let statement = match dest_row {
                None => Some(insert_statement(&qualified, &columns, incoming)?),
                Some(row) if updates => update_statement(
                    &qualified,
                    &columns,
                    &destination.primary_key,
                    incoming,
                    destination.rows[row].original(),
                )?,
                Some(_) => None,
            };
            if let Some(statement) = statement {
                if sql.len() + statement.len() > MAX_SCRIPT_BYTES {
                    return Err("同步 SQL 超过 8 MiB，请分批处理这些差异。".into());
                }
                sql.push_str(&statement);
            }
        }
        if sql.is_empty() {
            return Ok(None);
        }
        let mut complete =
            "-- 请在对应连接中审阅和执行。更新仅匹配读取时的整行，完整主键缺失才会安排新增。\n\
            -- 新增时若已出现同键，服务器会报错。更新影响 0 行时，请刷新快照核对差异。\n\n"
                .to_owned();
        complete.push_str(&sql);
        Ok(Some(complete))
    }
}

fn key_value(value: &Value) -> Result<KeyValue, String> {
    Ok(match value {
        Value::NULL => return Err("主键中读取到了 NULL。".into()),
        Value::Bytes(bytes) => KeyValue::Bytes(bytes.clone()),
        Value::Int(value) => KeyValue::Int(*value),
        Value::UInt(value) => KeyValue::UInt(*value),
        Value::Float(value) => KeyValue::Float(value.to_bits()),
        Value::Double(value) => KeyValue::Double(value.to_bits()),
        value @ (Value::Date(..) | Value::Time(..)) => KeyValue::Temporal(value.as_sql(true)),
    })
}

fn equal_value(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Float(left), Value::Float(right)) => left.to_bits() == right.to_bits(),
        (Value::Double(left), Value::Double(right)) => left.to_bits() == right.to_bits(),
        _ => left == right,
    }
}

#[derive(Clone, Copy)]
enum Category {
    Integer,
    Decimal,
    Float,
    Temporal,
    Bit,
    Binary,
    Text,
    Enum,
}

struct SqlColumn {
    name: String,
    category: Category,
    writable: bool,
    auto_increment: bool,
}

fn column_kind(column: &TableColumn) -> Option<Category> {
    let base = column.data_type.split(['(', ' ']).next()?;
    Some(match base {
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "year" => {
            Category::Integer
        }
        "decimal" | "numeric" => Category::Decimal,
        "float" | "double" | "real" => Category::Float,
        "date" | "datetime" | "time" => Category::Temporal,
        "bit" => Category::Bit,
        "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" => {
            Category::Binary
        }
        "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" | "set" | "json" => {
            Category::Text
        }
        "enum" => Category::Enum,
        _ => return None,
    })
}

fn literal(column: &SqlColumn, value: &Value) -> Result<String, String> {
    let sql = match (column.category, value) {
        (_, Value::NULL) => "NULL".into(),
        (Category::Integer | Category::Bit, Value::Int(value)) => value.to_string(),
        (Category::Integer | Category::Bit, Value::UInt(value)) => value.to_string(),
        (Category::Decimal, Value::Bytes(bytes)) => decimal_literal(bytes)
            .ok_or("无法读取准确的 DECIMAL 值。")?
            .into(),
        (Category::Float, Value::Float(value)) if value.is_finite() => {
            format!("{:e}", f64::from(*value))
        }
        (Category::Float, Value::Double(value)) if value.is_finite() => format!("{value:e}"),
        (Category::Temporal, Value::Date(..) | Value::Time(..)) => value.as_sql(true),
        (Category::Bit, Value::Bytes(bytes)) if bytes.len() <= 8 => bytes
            .iter()
            .fold(0u64, |previous, &byte| (previous << 8) | u64::from(byte))
            .to_string(),
        (Category::Binary, Value::Bytes(bytes)) => hex_literal(bytes),
        (Category::Text | Category::Enum, Value::Bytes(bytes)) => {
            if matches!(column.category, Category::Enum) && bytes.is_empty() {
                return Err("空 ENUM 值无法区分空字符串和无效枚举项，请手动处理。".into());
            }
            std::str::from_utf8(bytes)
                .map_err(|_| "服务器返回了非 UTF-8 文本，无法生成无损的同步 SQL。")?;
            format!("CONVERT({} USING utf8mb4)", hex_literal(bytes))
        }
        _ => return Err(format!("列 {} 的值与字段类型不匹配。", column.name)),
    };
    Ok(sql)
}

fn decimal_literal(bytes: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(bytes).ok()?;
    let number = text.strip_prefix('-').unwrap_or(text);
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    (!whole.is_empty()
        && whole.bytes().all(|byte| byte.is_ascii_digit())
        && fraction.bytes().all(|byte| byte.is_ascii_digit()))
    .then_some(text)
}

fn hex_literal(bytes: &[u8]) -> String {
    let mut sql = String::with_capacity(bytes.len() * 2 + 3);
    sql.push_str("X'");
    for byte in bytes {
        let _ = write!(sql, "{byte:02X}");
    }
    sql.push('\'');
    sql
}

fn insert_statement(
    qualified: &str,
    columns: &[SqlColumn],
    incoming: &[Value],
) -> Result<String, String> {
    let mut names = Vec::new();
    let mut values = Vec::new();
    for (column, value) in columns
        .iter()
        .zip(incoming)
        .filter(|(column, _)| column.writable)
    {
        if column.auto_increment && matches!(value, Value::Int(0) | Value::UInt(0)) {
            return Err("源端存在自增列值 0，目标的 SQL 模式可能重新分配编号，请手动处理。".into());
        }
        names.push(column.name.as_str());
        values.push(literal(column, value)?);
    }
    if names.is_empty() {
        return Err("当前表没有可用于写入的列。".into());
    }
    Ok(format!(
        "INSERT INTO {qualified} ({}) VALUES ({});\n",
        names.join(", "),
        values.join(", ")
    ))
}

fn update_statement(
    qualified: &str,
    columns: &[SqlColumn],
    key: &[usize],
    incoming: &[Value],
    original: &[Value],
) -> Result<Option<String>, String> {
    let mut assignments = Vec::new();
    for (index, column) in columns.iter().enumerate() {
        if column.writable
            && !key.contains(&index)
            && !equal_value(&incoming[index], &original[index])
        {
            assignments.push(format!(
                "{} = {}",
                column.name,
                literal(column, &incoming[index])?
            ));
        }
    }
    if assignments.is_empty() {
        return Ok(None);
    }
    // Explicitly retain unchanged DATETIME values with ON UPDATE defaults (including in keys).
    for (index, column) in columns.iter().enumerate() {
        if column.writable
            && matches!(column.category, Category::Temporal)
            && equal_value(&incoming[index], &original[index])
        {
            assignments.push(format!("{} = {}", column.name, column.name));
        }
    }
    let mut guards = Vec::new();
    // An indexed primary-key predicate limits the candidates, including on collated text keys.
    for &index in key {
        let column = &columns[index];
        guards.push(format!(
            "{} <=> {}",
            column.name,
            literal(column, &original[index])?
        ));
    }
    for (column, value) in columns.iter().zip(original) {
        guards.push(exact_guard(column, value)?);
    }
    Ok(Some(format!(
        "UPDATE {qualified} SET {} WHERE {} LIMIT 1;\n",
        assignments.join(", "),
        guards.join(" AND ")
    )))
}

fn exact_guard(column: &SqlColumn, value: &Value) -> Result<String, String> {
    let name = &column.name;
    if matches!(value, Value::NULL) {
        return Ok(format!("{name} IS NULL"));
    }
    if let (Category::Text | Category::Enum, Value::Bytes(bytes)) = (column.category, value) {
        std::str::from_utf8(bytes).map_err(|_| "服务器返回了非 UTF-8 文本，无法检查原始值。")?;
        return Ok(format!(
            "CAST(CONVERT({name} USING utf8mb4) AS BINARY) <=> {}",
            hex_literal(bytes)
        ));
    }
    let value = literal(column, value)?;
    if matches!(column.category, Category::Bit) {
        Ok(format!("CAST({name} AS UNSIGNED) <=> {value}"))
    } else {
        Ok(format!("{name} <=> {value}"))
    }
}
