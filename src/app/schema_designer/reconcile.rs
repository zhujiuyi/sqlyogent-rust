use std::collections::{BTreeMap, BTreeSet};

use eframe::egui::{self, ComboBox, RichText, ScrollArea, TextEdit};

use crate::{
    app::compare::{
        LayoutTableMatch, LayoutTableState, compare_layout_tables, plan_layout_creations,
    },
    database::{DbObject, ObjectKind, SchemaApply, SchemaApplyStop, SnapshotObject},
};

use super::{
    Command, Designer, Layout, SchemaSnapshot, Workspace, identifier_ok, layout_definition,
    quote_identifier, snapshot_definitions, style,
};

pub(super) struct Reconcile {
    source_schema: String,
    destination: String,
    new_name: String,
    create_database: bool,
    phase: Phase,
    notice: Option<Result<String, String>>,
}

enum Phase {
    Destination,
    ConfirmDatabase,
    CreatingDatabase(u64),
    ReadingDatabases,
    ReadingTarget,
    Review(Review),
    ConfirmTables {
        review: Review,
        sql: String,
        apply: Box<SchemaApply>,
    },
    Applying {
        stopping: bool,
    },
}

struct Review {
    target: SchemaSnapshot,
    tables: Vec<LayoutTableMatch>,
    create: BTreeSet<String>,
    choices: BTreeMap<String, DifferenceChoice>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DifferenceChoice {
    Keep,
    Skip,
}

enum Action {
    Continue,
    CreateDatabase,
    Back,
    PreviewTables,
    ApplyTables,
    Open,
    Cancel,
}

enum ResultOfDialog {
    Pending,
    Open(Review),
    Cancel,
}

impl Reconcile {
    pub(super) fn new(source_schema: String) -> Self {
        Self {
            source_schema,
            destination: String::new(),
            new_name: String::new(),
            create_database: false,
            phase: Phase::Destination,
            notice: None,
        }
    }

    pub(super) fn waiting(&self) -> bool {
        matches!(
            self.phase,
            Phase::CreatingDatabase(_)
                | Phase::ReadingDatabases
                | Phase::ReadingTarget
                | Phase::Applying { .. }
        )
    }

    fn source(&self, layout: &Layout) -> SchemaSnapshot {
        let objects = layout
            .tables
            .iter()
            .map(|card| SnapshotObject {
                object: DbObject {
                    schema: self.source_schema.clone(),
                    name: card.name.clone(),
                    kind: ObjectKind::Table,
                },
                definition: card
                    .definition
                    .as_deref()
                    .and_then(layout_definition)
                    .map(str::to_owned)
                    .ok_or_else(|| "布局没有可用的建表定义。".into()),
                context: None,
            })
            .collect();
        SchemaSnapshot {
            schema: self.source_schema.clone(),
            defaults: None,
            objects,
            omitted_objects: 0,
            context: None,
            foreign_keys: None,
            origin: None,
        }
    }

    pub(super) fn poll(&mut self, workspace: &mut Workspace, layout: &Layout) {
        if workspace.busy() {
            return;
        }
        match self.phase {
            Phase::CreatingDatabase(tab) => {
                let report = workspace
                    .tabs
                    .iter()
                    .find(|query| query.id == tab)
                    .and_then(|query| query.report.as_ref());
                match report {
                    Some(report) if report.error.is_none() => {
                        self.notice = Some(Ok(format!("已创建数据库 {}。", self.destination)));
                        self.create_database = false;
                        if workspace.send(Command::ListSchemas, "正在刷新数据库列表…") {
                            self.phase = Phase::ReadingDatabases;
                        } else {
                            self.read_target(workspace);
                        }
                    }
                    report => {
                        let error = report
                            .and_then(|report| report.error.clone())
                            .unwrap_or_else(|| workspace.status.clone());
                        self.notice = Some(Err(format!("无法确认数据库创建成功。{error}")));
                        self.phase = Phase::Destination;
                    }
                }
            }
            Phase::ReadingDatabases => self.read_target(workspace),
            Phase::ReadingTarget => {
                if let Some(target) = workspace.snapshots.remove(&self.destination) {
                    let tables = compare_layout_tables(&self.source(layout), &target);
                    self.phase = Phase::Review(Review {
                        target,
                        tables,
                        create: BTreeSet::new(),
                        choices: BTreeMap::new(),
                    });
                } else {
                    let prior = self.notice.take();
                    self.notice = Some(Err(format!(
                        "{}无法读取目标库 {} 的结构。{}",
                        prior.as_ref().map_or("", |notice| match notice {
                            Ok(message) | Err(message) => message.as_str(),
                        }),
                        self.destination,
                        workspace.status
                    )));
                    self.phase = Phase::Destination;
                }
            }
            Phase::Applying { .. } => {
                let result = workspace.schema_apply_result.take();
                self.notice = Some(match result {
                    Some(Ok(result)) if result.schema == self.destination => match result.stopped {
                        None => Ok(format!("已执行全部 {} 条建表及外键语句。", result.total)),
                        Some(stop) => {
                            let detail = match stop {
                                SchemaApplyStop::Cancelled => "操作已停止。".to_owned(),
                                SchemaApplyStop::Failed {
                                    sql,
                                    error,
                                    uncertain,
                                } => {
                                    let uncertainty = if uncertain {
                                        "当前语句是否生效无法确认。"
                                    } else {
                                        ""
                                    };
                                    format!("{error} {uncertainty}\n{sql}")
                                }
                            };
                            Err(format!(
                                "已确认 {} / {} 条。{detail}",
                                result.completed, result.total
                            ))
                        }
                    },
                    Some(Err(error)) => Err(error),
                    _ => Err("未能确认建表结果。请先检查目标库。".into()),
                });
                self.read_target(workspace);
            }
            _ => {}
        }
    }

    fn read_target(&mut self, workspace: &mut Workspace) {
        let schema = self.destination.clone();
        if workspace.send(
            Command::SnapshotSchema {
                schema: schema.clone(),
            },
            "正在核对目标库结构…",
        ) {
            workspace.snapshots.remove(&schema);
            self.phase = Phase::ReadingTarget;
        } else {
            let failure = workspace.status.clone();
            self.notice = Some(Err(match self.notice.take() {
                Some(Ok(message) | Err(message)) => {
                    format!("{message} 无法读取目标结构。{failure}")
                }
                None => format!("无法读取目标结构。{failure}"),
            }));
            self.phase = Phase::Destination;
        }
    }

    fn create_database_sql(&self) -> String {
        format!("CREATE DATABASE {}", quote_identifier(self.new_name.trim()))
    }

    fn show(
        &mut self,
        ui: &mut egui::Ui,
        workspace: &mut Workspace,
        layout: &Layout,
    ) -> ResultOfDialog {
        if self.destination.is_empty() && matches!(self.phase, Phase::Destination) {
            self.destination = workspace
                .schemas
                .iter()
                .find(|schema| *schema == &self.source_schema)
                .cloned()
                .or_else(|| workspace.server.as_ref()?.database.clone())
                .unwrap_or_default();
        }
        let ctx = ui.ctx();
        let mut action = None;
        let waiting = self.waiting();
        let connected = workspace.server.is_some() && !workspace.busy();
        let modal = style::modal(ctx, ui.id().with("layout-reconciliation")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 38.0).clamp(350.0, 720.0));
            ui.heading("打开架构布局");
            ui.label(format!("当前连接 · {}", workspace.label));
            if let Some(notice) = &self.notice {
                match notice {
                    Ok(message) => {
                        ui.label(message);
                    }
                    Err(error) => {
                        ScrollArea::vertical()
                            .id_salt("layout-status")
                            .max_height(72.0)
                            .show(ui, |ui| ui.colored_label(style::error(), error));
                    }
                }
            }
            match &mut self.phase {
                Phase::Destination => {
                    ui.horizontal_wrapped(|ui| {
                        ui.radio_value(&mut self.create_database, false, "选择现有数据库");
                        ui.radio_value(&mut self.create_database, true, "创建新数据库");
                    });
                    let ready = if self.create_database {
                        ui.add(TextEdit::singleline(&mut self.new_name).hint_text("数据库名称"));
                        identifier_ok(self.new_name.trim())
                    } else {
                        ComboBox::from_id_salt("layout-destination")
                            .selected_text(if self.destination.is_empty() {
                                "选择数据库"
                            } else {
                                &self.destination
                            })
                            .show_ui(ui, |ui| {
                                for schema in &workspace.schemas {
                                    ui.selectable_value(
                                        &mut self.destination,
                                        schema.clone(),
                                        schema,
                                    );
                                }
                            });
                        workspace.schemas.contains(&self.destination)
                    };
                    if ui
                        .add_enabled(connected && ready, style::primary("继续"))
                        .clicked()
                    {
                        action = Some(Action::Continue);
                    }
                }
                Phase::ConfirmDatabase => {
                    ui.label("确认在当前连接中创建新数据库后，再读取其中的表结构。");
                    ui.monospace(self.create_database_sql());
                    if ui
                        .add_enabled(connected, style::primary("确认创建数据库"))
                        .clicked()
                    {
                        action = Some(Action::CreateDatabase);
                    }
                }
                Phase::CreatingDatabase(_) | Phase::ReadingDatabases | Phase::ReadingTarget => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(&workspace.status);
                    });
                }
                Phase::Review(review) => {
                    review.show(ui, connected, &mut action);
                }
                Phase::ConfirmTables { sql, apply, .. } => {
                    ui.label(format!(
                        "在数据库 {} 中执行 {} 条建表及外键语句。",
                        self.destination,
                        apply.changes.len()
                    ));
                    ui.label("先创建表，再添加外键。若中途停止，已执行的结构变更会保留。");
                    ScrollArea::both()
                        .max_height(scroll_height(ctx))
                        .show(ui, |ui| {
                            ui.add(
                                TextEdit::multiline(&mut sql.as_str())
                                    .font(egui::TextStyle::Monospace)
                                    .desired_width(f32::INFINITY),
                            );
                        });
                    if ui
                        .add_enabled(connected, style::primary("确认创建选中的表"))
                        .clicked()
                    {
                        action = Some(Action::ApplyTables);
                    }
                }
                Phase::Applying { stopping } => {
                    if let Some(progress) =
                        workspace.schema_apply_progress.filter(|p| !p.validating)
                    {
                        ui.label(format!(
                            "已确认 {} / {} 条语句。",
                            progress.completed, progress.total
                        ));
                    } else {
                        ui.label("正在重新核对目标库和连接标识…");
                    }
                    if ui
                        .add_enabled(!*stopping, egui::Button::new("停止后续语句"))
                        .clicked()
                        && workspace.cancel_schema_apply()
                    {
                        *stopping = true;
                    }
                }
            }
            if !waiting {
                ui.horizontal(|ui| {
                    if matches!(
                        self.phase,
                        Phase::Review(_) | Phase::ConfirmDatabase | Phase::ConfirmTables { .. }
                    ) && ui.button("返回").clicked()
                    {
                        action = Some(Action::Back);
                    }
                    if ui.button("关闭布局").clicked() {
                        action = Some(Action::Cancel);
                    }
                });
            }
        });
        if action.is_none() && !waiting && modal.should_close() {
            action = Some(Action::Cancel);
        }
        let Some(action) = action else {
            return ResultOfDialog::Pending;
        };
        self.act(workspace, layout, action)
    }

    fn act(
        &mut self,
        workspace: &mut Workspace,
        layout: &Layout,
        action: Action,
    ) -> ResultOfDialog {
        match action {
            Action::Continue => {
                self.notice = None;
                if self.create_database {
                    self.phase = Phase::ConfirmDatabase;
                } else {
                    self.read_target(workspace);
                }
            }
            Action::CreateDatabase => {
                let sql = self.create_database_sql();
                self.destination = self.new_name.trim().to_owned();
                workspace.new_tab(sql.clone(), None);
                if workspace.send(
                    Command::QueryInSeparateSession { sql, limit: 1 },
                    "正在创建数据库…",
                ) {
                    self.phase = Phase::CreatingDatabase(workspace.tab().id);
                } else {
                    self.notice = Some(Err(workspace.status.clone()));
                }
            }
            Action::Back => {
                if let Phase::ConfirmTables { review, .. } =
                    std::mem::replace(&mut self.phase, Phase::Destination)
                {
                    self.phase = Phase::Review(review);
                }
            }
            Action::PreviewTables => {
                if let Phase::Review(review) =
                    std::mem::replace(&mut self.phase, Phase::Destination)
                {
                    match plan_layout_creations(
                        &self.source(layout),
                        &review.target,
                        &review.create,
                    ) {
                        Ok((sql, apply)) => {
                            self.phase = Phase::ConfirmTables {
                                review,
                                sql,
                                apply: Box::new(apply),
                            }
                        }
                        Err(errors) => {
                            self.notice = Some(Err(errors.join("\n")));
                            self.phase = Phase::Review(review);
                        }
                    }
                }
            }
            Action::ApplyTables => {
                if let Phase::ConfirmTables { review, apply, .. } =
                    std::mem::replace(&mut self.phase, Phase::Destination)
                {
                    if workspace.send(Command::ApplySchema { apply }, "正在创建布局中的表…")
                    {
                        self.notice = None;
                        self.phase = Phase::Applying { stopping: false };
                    } else {
                        self.notice = Some(Err(workspace.status.clone()));
                        self.phase = Phase::Review(review);
                    }
                }
            }
            Action::Open => {
                if let Phase::Review(review) =
                    std::mem::replace(&mut self.phase, Phase::Destination)
                {
                    return ResultOfDialog::Open(review);
                }
            }
            Action::Cancel => return ResultOfDialog::Cancel,
        }
        ResultOfDialog::Pending
    }
}

fn scroll_height(ctx: &egui::Context) -> f32 {
    (ctx.content_rect().height() - 250.0).clamp(105.0, 420.0)
}

impl Review {
    fn show(&mut self, ui: &mut egui::Ui, connected: bool, action: &mut Option<Action>) {
        use LayoutTableState::{Blocked, Different, Missing, Same, Unverified};
        ui.label(format!("目标数据库 · {}", self.target.schema));
        let missing = self
            .tables
            .iter()
            .filter(|table| table.state == Missing)
            .count();
        let needs_choice = |state| matches!(state, Different | Unverified);
        let unresolved = self
            .tables
            .iter()
            .filter(|table| needs_choice(table.state) && !self.choices.contains_key(&table.name))
            .count();
        if self.tables.is_empty() {
            ui.label("布局中没有表。");
        }
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    missing > 0,
                    egui::Button::new(if self.create.len() == missing {
                        "取消全选缺表"
                    } else {
                        "选择所有缺表"
                    }),
                )
                .clicked()
            {
                if self.create.len() == missing {
                    self.create.clear();
                } else {
                    self.create = self
                        .tables
                        .iter()
                        .filter(|table| table.state == Missing)
                        .map(|table| table.name.clone())
                        .collect();
                }
            }
            if ui
                .add_enabled(unresolved != 0, egui::Button::new("跳过未处理的差异表"))
                .clicked()
            {
                for table in self.tables.iter().filter(|table| needs_choice(table.state)) {
                    self.choices
                        .entry(table.name.clone())
                        .or_insert(DifferenceChoice::Skip);
                }
            }
        });
        ScrollArea::vertical()
            .max_height(scroll_height(ui.ctx()))
            .show(ui, |ui| {
                egui::Grid::new("layout-table-status")
                    .num_columns(3)
                    .striped(true)
                    .show(ui, |ui| {
                        ui.strong("数据表");
                        ui.strong("比较结果");
                        ui.strong("处理方式");
                        ui.end_row();
                        for table in &self.tables {
                            ui.label(&table.name);
                            let label = match table.state {
                                Same => "一致",
                                Different => "结构不同",
                                Unverified => "现有表，定义无法核对",
                                Missing => "目标库缺表",
                                Blocked => "无法载入",
                            };
                            ui.label(label);
                            match table.state {
                                Same => {
                                    ui.label("使用现有表");
                                }
                                Different | Unverified => {
                                    let previous = self.choices.get(&table.name).copied();
                                    let mut choice = previous;
                                    ComboBox::from_id_salt(&table.name)
                                        .selected_text(match choice {
                                            Some(DifferenceChoice::Keep) => "使用现有表",
                                            Some(DifferenceChoice::Skip) => "跳过表",
                                            None => "请选择",
                                        })
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(
                                                &mut choice,
                                                Some(DifferenceChoice::Keep),
                                                "使用现有表",
                                            );
                                            ui.selectable_value(
                                                &mut choice,
                                                Some(DifferenceChoice::Skip),
                                                "跳过表",
                                            );
                                        });
                                    if let Some(choice) = choice
                                        && Some(choice) != previous
                                    {
                                        self.choices.insert(table.name.clone(), choice);
                                    }
                                }
                                Missing => {
                                    let mut checked = self.create.contains(&table.name);
                                    if ui.checkbox(&mut checked, "创建").changed() {
                                        if checked {
                                            self.create.insert(table.name.clone());
                                        } else {
                                            self.create.remove(&table.name);
                                        }
                                    }
                                }
                                Blocked => {
                                    ui.label(
                                        RichText::new(
                                            table.problem.unwrap_or("请检查保存的表定义。"),
                                        )
                                        .color(style::muted()),
                                    );
                                }
                            }
                            ui.end_row();
                        }
                    });
            });
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    connected && !self.create.is_empty(),
                    style::primary("预览建表 SQL"),
                )
                .clicked()
            {
                *action = Some(Action::PreviewTables);
            }
            let resolved = !self
                .tables
                .iter()
                .any(|table| needs_choice(table.state) && !self.choices.contains_key(&table.name));
            if ui
                .add_enabled(
                    connected && resolved,
                    egui::Button::new(if missing == 0 {
                        "打开布局"
                    } else {
                        "不创建缺少的表，打开布局"
                    }),
                )
                .clicked()
            {
                *action = Some(Action::Open);
            }
        });
    }
}

impl Designer {
    pub(super) fn reconcile_view(
        &mut self,
        ui: &mut egui::Ui,
        workspace: &mut Workspace,
        interactive: bool,
    ) {
        ui.heading("架构设计器");
        ui.label("核对布局与目标数据库");
        if !interactive {
            return;
        }
        let Some(mut flow) = self.reconciliation.take() else {
            return;
        };
        match flow.show(ui, workspace, &self.layout) {
            ResultOfDialog::Pending => self.reconciliation = Some(flow),
            ResultOfDialog::Open(review) => self.finish_reconciliation(workspace, review),
            ResultOfDialog::Cancel => self.closed = true,
        }
    }

    fn finish_reconciliation(&mut self, workspace: &mut Workspace, review: Review) {
        let Review {
            target,
            tables,
            choices,
            ..
        } = review;
        let destination = target.schema.clone();
        let same_schema = self.layout.schema == destination;
        let lower_case = target
            .context
            .as_ref()
            .is_some_and(|context| context.lower_case_table_names != 0);
        let normalize = |name: &str| {
            if lower_case {
                name.to_lowercase()
            } else {
                name.to_owned()
            }
        };
        let present = target
            .objects
            .iter()
            .filter(|item| item.object.kind == ObjectKind::Table)
            .map(|item| (normalize(&item.object.name), item))
            .collect::<BTreeMap<_, _>>();
        let mut altered = !same_schema;
        self.layout.tables = std::mem::take(&mut self.layout.tables)
            .into_iter()
            .zip(tables)
            .filter_map(|(mut card, matched)| {
                if choices.get(&card.name) == Some(&DifferenceChoice::Skip) {
                    altered = true;
                    return None;
                }
                if matches!(
                    matched.state,
                    LayoutTableState::Same
                        | LayoutTableState::Different
                        | LayoutTableState::Unverified
                ) && let Some(item) = present.get(&normalize(&card.name))
                {
                    let actual = item
                        .definition
                        .as_deref()
                        .ok()
                        .and_then(layout_definition)
                        .map(str::to_owned);
                    altered |= card.name != item.object.name
                        || (matched.state != LayoutTableState::Same && card.definition != actual);
                    card.name.clone_from(&item.object.name);
                    card.definition = actual;
                }
                Some(card)
            })
            .collect();
        if !same_schema {
            self.change_schema(destination.clone());
        }
        self.dirty |= altered;
        self.live_definitions = snapshot_definitions(&target);
        self.check_layout = false;
        if workspace.send(
            Command::DescribeSchemaDesign {
                schema: destination.clone(),
            },
            "正在读取布局中的表和外键…",
        ) {
            workspace.schema_designs.remove(&destination);
            self.catalog_loading = Some(destination);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{app::schema_designer::encode_layout, database::ConnectionConfig};
    use mysql::{Conn, OptsBuilder, prelude::Queryable};
    use std::{
        fs, thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    fn settle(workspace: &mut Workspace, mut designer: Option<&mut Designer>) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            workspace.poll();
            if let Some(designer) = &mut designer {
                designer.advance(workspace, false);
            }
            if !workspace.busy()
                && designer
                    .as_ref()
                    .is_none_or(|designer| !designer.has_pending_work())
            {
                return;
            }
            assert!(Instant::now() < deadline, "操作超时：{}", workspace.status);
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn act(designer: &mut Designer, workspace: &mut Workspace, action: Action) {
        let result =
            designer
                .reconciliation
                .as_mut()
                .unwrap()
                .act(workspace, &designer.layout, action);
        if let ResultOfDialog::Open(review) = result {
            designer.reconciliation = None;
            designer.finish_reconciliation(workspace, review);
        }
        settle(workspace, Some(designer));
    }

    struct TestSchemas {
        connection: Conn,
        names: [String; 2],
    }

    impl Drop for TestSchemas {
        fn drop(&mut self) {
            for name in &self.names {
                let _ = self.connection.query_drop(format!(
                    "DROP DATABASE IF EXISTS {}",
                    quote_identifier(name)
                ));
            }
        }
    }

    #[test]
    #[ignore = "在一次性本机 MariaDB 中运行，需要 HARBOR_SCHEMA_RECONCILE_PORT"]
    fn reconciles_saved_layout_into_existing_and_new_databases() {
        let port = std::env::var("HARBOR_SCHEMA_RECONCILE_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let existing = format!("harbor_reconcile_old_{nonce:x}");
        let new = format!("harbor_reconcile_new_{nonce:x}");
        let mut fixture = TestSchemas {
            connection: Conn::new(
                OptsBuilder::new()
                    .ip_or_hostname(Some("127.0.0.1"))
                    .tcp_port(port)
                    .user(Some("root")),
            )
            .unwrap(),
            names: [existing.clone(), new.clone()],
        };
        let qualified = quote_identifier(&existing);
        fixture
            .connection
            .query_drop(format!("CREATE DATABASE {qualified}"))
            .unwrap();
        for sql in [
            format!(
                "CREATE TABLE {qualified}.`matches` (`id` int(11) NOT NULL, PRIMARY KEY (`id`)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci"
            ),
            format!("CREATE TABLE {qualified}.`keep` (`live_value` int(11)) ENGINE=InnoDB"),
            format!("CREATE TABLE {qualified}.`skip` (`live_value` int(11)) ENGINE=InnoDB"),
            format!("INSERT INTO {qualified}.`keep` VALUES (123)"),
        ] {
            fixture.connection.query_drop(sql).unwrap();
        }

        let mut workspace = Workspace::new("布局验收".into(), None);
        workspace.connect(
            ConnectionConfig {
                port,
                database: Some(existing.clone()),
                ..Default::default()
            },
            None,
        );
        settle(&mut workspace, None);
        assert!(workspace.server.is_some(), "{}", workspace.status);

        let mut seed = Designer::new(workspace.key, "legacy_layout".into(), None);
        for (name, definition) in [
            (
                "child",
                "CREATE TABLE `legacy_layout`.`child` (`id` int(11) NOT NULL, `parent_id` int(11) NOT NULL, PRIMARY KEY (`id`), KEY `parent_id` (`parent_id`), CONSTRAINT `fk_parent` FOREIGN KEY (`parent_id`) REFERENCES `legacy_layout`.`parent` (`id`)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci",
            ),
            (
                "parent",
                "CREATE TABLE `legacy_layout`.`parent` (`id` int(11) NOT NULL, PRIMARY KEY (`id`)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci",
            ),
            (
                "matches",
                "CREATE TABLE `matches` (`id` int(11) NOT NULL, PRIMARY KEY (`id`)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci",
            ),
            (
                "keep",
                "CREATE TABLE `keep` (`old_value` int(11)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci",
            ),
            (
                "skip",
                "CREATE TABLE `skip` (`old_value` int(11)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci",
            ),
        ] {
            seed.add_card(name, 2);
            seed.layout.tables.last_mut().unwrap().definition = Some(definition.into());
        }
        let executable = std::env::current_exe().unwrap();
        let directory = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let path = directory.path().join("布局.schemaxml");
        let bytes = encode_layout(&seed.layout, &path).unwrap();
        fs::write(&path, &bytes).unwrap();

        let mut designer = Designer::from_file(workspace.key, path.clone()).unwrap();
        designer.reconciliation.as_mut().unwrap().destination = existing.clone();
        act(&mut designer, &mut workspace, Action::Continue);
        let Phase::Review(review) = &mut designer.reconciliation.as_mut().unwrap().phase else {
            panic!("未收到目标库结构：{}", workspace.status);
        };
        let state = |name| {
            review
                .tables
                .iter()
                .find(|table| table.name == name)
                .unwrap()
                .state
        };
        assert!(state("matches") == LayoutTableState::Same);
        assert!(state("keep") == LayoutTableState::Different);
        assert!(state("parent") == LayoutTableState::Missing);
        assert!(state("child") == LayoutTableState::Missing);
        review.create.extend(["child".into(), "parent".into()]);

        act(&mut designer, &mut workspace, Action::PreviewTables);
        let flow = designer.reconciliation.as_ref().unwrap();
        let Phase::ConfirmTables { apply, .. } = &flow.phase else {
            panic!("未生成确认 SQL：{:?}", flow.notice);
        };
        assert_eq!(apply.changes.len(), 3);
        assert!(
            apply.changes[..2]
                .iter()
                .all(|change| change.sql.starts_with("CREATE TABLE"))
        );
        assert!(apply.changes[2].sql.contains("fk_parent"));
        act(&mut designer, &mut workspace, Action::ApplyTables);
        let flow = designer.reconciliation.as_mut().unwrap();
        assert!(matches!(flow.notice, Some(Ok(_))), "{:?}", flow.notice);
        let Phase::Review(review) = &mut flow.phase else {
            panic!("未重新读取已创建的表");
        };
        review.choices.insert("keep".into(), DifferenceChoice::Keep);
        review.choices.insert("skip".into(), DifferenceChoice::Skip);
        act(&mut designer, &mut workspace, Action::Open);
        assert!(designer.reconciliation.is_none());
        assert_eq!(designer.layout.schema, existing);
        assert!(
            !designer
                .layout
                .tables
                .iter()
                .any(|table| table.name == "skip")
        );
        let kept = designer
            .layout
            .tables
            .iter()
            .find(|table| table.name == "keep")
            .unwrap();
        assert!(kept.definition.as_ref().unwrap().contains("live_value"));
        let keys = &workspace.schema_designs[&existing].foreign_keys;
        assert_eq!(keys[0].referenced_table.schema, existing);
        assert_eq!(keys[0].referenced_table.name, "parent");
        let untouched: Option<u32> = fixture
            .connection
            .query_first(format!("SELECT live_value FROM {qualified}.`keep`"))
            .unwrap();
        assert_eq!(untouched, Some(123));

        let mut second = Designer::from_file(workspace.key, path.clone()).unwrap();
        let flow = second.reconciliation.as_mut().unwrap();
        flow.new_name = new.clone();
        flow.create_database = true;
        act(&mut second, &mut workspace, Action::Continue);
        assert!(matches!(
            second.reconciliation.as_ref().unwrap().phase,
            Phase::ConfirmDatabase
        ));
        act(&mut second, &mut workspace, Action::CreateDatabase);
        let flow = second.reconciliation.as_ref().unwrap();
        let Phase::Review(review) = &flow.phase else {
            panic!("新建数据库未进入布局核对：{:?}", flow.notice);
        };
        assert_eq!(review.target.schema, new);
        assert!(review.target.objects.is_empty());
        assert_eq!(
            workspace.server.as_ref().unwrap().database.as_deref(),
            Some(existing.as_str())
        );
        act(&mut second, &mut workspace, Action::Open);
        assert_eq!(second.layout.schema, new);
        assert_eq!(second.layout.tables.len(), 5);
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}
