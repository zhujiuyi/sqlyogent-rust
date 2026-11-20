use eframe::egui::{self, RichText, TextEdit};

use crate::database::{
    ColumnInfo, Command, DatabaseContentsAction, DbObject, ObjectDetail, ObjectKind,
    quote_identifier,
};
use crate::maintenance::{TableAction, alter_database, create_database, drop_database};

use super::{
    copy::SelectiveCopy,
    kind_label,
    state::{BrowseAction, Workspace},
    style,
};

mod database_dialog;

#[derive(Clone)]
pub(super) enum Operation {
    CreateDatabase,
    AlterDatabase(String),
    CreateObject {
        schema: String,
        kind: ObjectKind,
        table: Option<String>,
    },
    EditDefinition(DbObject),
    PasteStatement {
        object: DbObject,
        statement: Statement,
    },
    Rename(DbObject),
    CopyTable(DbObject),
    TableEngine(DbObject),
    TableStructure(DbObject),
    ColumnOrder(DbObject),
    Index(DbObject),
    ForeignKey(DbObject),
    DatabaseContents {
        schema: String,
        action: DatabaseContentsAction,
    },
}

#[derive(Clone, Copy)]
pub(super) enum Statement {
    Insert,
    Update,
    Delete,
    Select,
}

impl Statement {
    fn label(self) -> &'static str {
        match self {
            Self::Insert => "INSERT（插入）",
            Self::Update => "UPDATE（更新）",
            Self::Delete => "DELETE（删除）",
            Self::Select => "SELECT（查询）",
        }
    }
}

pub(super) fn connection_menu(
    ui: &mut egui::Ui,
    schema: Option<&str>,
    action: &mut Option<BrowseAction>,
) {
    item(ui, "刷新对象浏览器", action, || {
        BrowseAction::RefreshAll
    });
    item(ui, "新建数据库…", action, || {
        BrowseAction::Manage(Operation::CreateDatabase)
    });
    if let Some(schema) = schema {
        item(ui, "计划备份…", action, || BrowseAction::Schedule {
            schema: schema.into(),
            object: None,
        });
        item(ui, "从外部数据源导入…", action, || {
            BrowseAction::ImportOdbcSchema(schema.into())
        });
    }
    item(ui, "从 SQL 转储文件还原…", action, || {
        BrowseAction::Restore(schema.unwrap_or_default().into())
    });
}

pub(super) fn schema_menu(ui: &mut egui::Ui, schema: &str, action: &mut Option<BrowseAction>) {
    item(ui, "多表数据同步…", action, || {
        BrowseAction::SyncDatabase(schema.into())
    });
    item(ui, "架构设计器…", action, || {
        BrowseAction::SchemaDesigner {
            schema: schema.into(),
            table: None,
        }
    });
    item(ui, "表诊断…", action, || {
        BrowseAction::TableDiagnostics {
            schema: schema.into(),
            table: None,
        }
    });
    item(ui, "设为默认数据库", action, || {
        BrowseAction::UseSchema(schema.into())
    });
    item(ui, "刷新对象列表", action, || {
        BrowseAction::RefreshObjects(schema.into())
    });
    ui.separator();
    item(
        ui,
        "复制数据库到其他连接或数据库…",
        action,
        || BrowseAction::Copy {
            schema: schema.into(),
            object: None,
        },
    );
    item(ui, "新建数据库…", action, || {
        BrowseAction::Manage(Operation::CreateDatabase)
    });
    item(ui, "修改数据库…", action, || {
        BrowseAction::Manage(Operation::AlterDatabase(schema.into()))
    });
    ui.menu_button("新建对象", |ui| {
        for kind in [
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::Procedure,
            ObjectKind::Function,
            ObjectKind::Trigger,
            ObjectKind::Event,
        ] {
            create_item(ui, kind_label(kind), schema, kind, None, action);
        }
    });
    ui.menu_button("其他数据库操作", |ui| {
        item(ui, "截断数据库中的表…", action, || {
            BrowseAction::Manage(Operation::DatabaseContents {
                schema: schema.into(),
                action: DatabaseContentsAction::Truncate,
            })
        });
        item(ui, "清空数据库…", action, || {
            BrowseAction::Manage(Operation::DatabaseContents {
                schema: schema.into(),
                action: DatabaseContentsAction::Drop,
            })
        });
        item(ui, "删除数据库…", action, || {
            BrowseAction::Script(drop_database(schema))
        });
    });
    ui.menu_button("备份与导出", |ui| {
        item(ui, "计划备份…", action, || BrowseAction::Schedule {
            schema: schema.into(),
            object: None,
        });
        item(ui, "备份数据库为 SQL 文件…", action, || {
            BrowseAction::Backup {
                schema: schema.into(),
                object: None,
            }
        });
        item(
            ui,
            "导出数据库结构说明（HTML）…",
            action,
            || BrowseAction::SchemaHtml(schema.into()),
        );
    });
    ui.menu_button("导入", |ui| {
        item(ui, "从外部数据源导入…", action, || {
            BrowseAction::ImportOdbcSchema(schema.into())
        });
        item(ui, "从 SQL 转储文件还原…", action, || {
            BrowseAction::Restore(schema.into())
        });
    });
}

pub(super) fn folder_menu(
    ui: &mut egui::Ui,
    schema: &str,
    kind: ObjectKind,
    action: &mut Option<BrowseAction>,
) {
    create_item(ui, create_label(kind), schema, kind, None, action);
    if kind == ObjectKind::Table {
        item(ui, "表诊断…", action, || {
            BrowseAction::TableDiagnostics {
                schema: schema.into(),
                table: None,
            }
        });
    }
    item(ui, "刷新对象列表", action, || {
        BrowseAction::RefreshObjects(schema.into())
    });
}

pub(super) fn object_menu(ui: &mut egui::Ui, object: &DbObject, action: &mut Option<BrowseAction>) {
    let is_table = object.kind == ObjectKind::Table;
    let is_view = object.kind == ObjectKind::View;
    if is_table {
        item(ui, "打开表数据…", action, || {
            BrowseAction::EditRows(object.clone())
        });
        item(ui, "在架构设计器中打开…", action, || {
            BrowseAction::SchemaDesigner {
                schema: object.schema.clone(),
                table: Some(object.clone()),
            }
        });
    }
    if is_table || is_view {
        item(ui, "查看数据", action, || {
            BrowseAction::Preview(object.clone())
        });
    }
    item(ui, "查看对象定义", action, || {
        BrowseAction::Inspect(object.clone())
    });
    if is_table {
        item(
            ui,
            "复制表到其他连接或数据库…",
            action,
            || BrowseAction::Copy {
                schema: object.schema.clone(),
                object: Some(object.clone()),
            },
        );
        item(ui, "比较并同步表数据…", action, || {
            BrowseAction::SyncData(object.clone())
        });
        ui.menu_button("生成 SQL", |ui| {
            for statement in [
                Statement::Insert,
                Statement::Update,
                Statement::Delete,
                Statement::Select,
            ] {
                item(ui, statement.label(), action, || {
                    BrowseAction::Manage(Operation::PasteStatement {
                        object: object.clone(),
                        statement,
                    })
                });
            }
        });
        create_item(
            ui,
            "新建表…",
            &object.schema,
            ObjectKind::Table,
            None,
            action,
        );
        table_operation(ui, "修改表字段…", object, action, Operation::TableStructure);
        table_operation(ui, "管理索引…", object, action, Operation::Index);
        table_operation(ui, "管理外键…", object, action, Operation::ForeignKey);
        ui.menu_button("其他表操作", |ui| {
            table_operation(ui, "重命名表…", object, action, Operation::Rename);
            table_operation(ui, "调整字段顺序…", object, action, Operation::ColumnOrder);
            table_operation(
                ui,
                "复制表结构或数据…",
                object,
                action,
                Operation::CopyTable,
            );
            table_operation(ui, "更改存储引擎…", object, action, Operation::TableEngine);
            item(ui, "查看表属性", action, || {
                BrowseAction::RunSql(table_status_sql(object))
            });
            for table_action in [TableAction::Truncate, TableAction::Drop] {
                item(ui, table_action.label(), action, || {
                    BrowseAction::Script(table_action.sql(&object.schema, &object.name))
                });
            }
        });
        item(ui, "表诊断…", action, || {
            BrowseAction::TableDiagnostics {
                schema: object.schema.clone(),
                table: Some(object.name.clone()),
            }
        });
        ui.menu_button("备份与导出", |ui| {
            item(ui, "计划备份…", action, || BrowseAction::Schedule {
                schema: object.schema.clone(),
                object: Some(object.clone()),
            });
            item(ui, "备份表为 SQL 文件…", action, || {
                BrowseAction::Backup {
                    schema: object.schema.clone(),
                    object: Some(object.clone()),
                }
            });
            item(ui, "导出表数据…", action, || {
                BrowseAction::ExportPreview(object.clone())
            });
        });
        ui.menu_button("导入", |ui| {
            item(ui, "导入文本文件…", action, || {
                BrowseAction::Import(object.clone())
            });
            item(ui, "从外部数据源导入…", action, || {
                BrowseAction::ImportOdbc(object.clone())
            });
        });
        create_item(
            ui,
            "新建触发器…",
            &object.schema,
            ObjectKind::Trigger,
            Some(object.name.as_str()),
            action,
        );
    } else {
        create_item(
            ui,
            create_label(object.kind),
            &object.schema,
            object.kind,
            None,
            action,
        );
        table_operation(
            ui,
            if is_view {
                "修改视图…"
            } else {
                "在编辑器中打开定义…"
            },
            object,
            action,
            Operation::EditDefinition,
        );
        if is_view {
            table_operation(ui, "重命名视图…", object, action, Operation::Rename);
            item(ui, "导出视图定义为 SQL 文件…", action, || {
                BrowseAction::Backup {
                    schema: object.schema.clone(),
                    object: Some(object.clone()),
                }
            });
            item(ui, "导出视图查询结果…", action, || {
                BrowseAction::ExportPreview(object.clone())
            });
        }
        item(ui, drop_label(object.kind), action, || {
            BrowseAction::Script(drop_object(object))
        });
    }
    if is_table || is_view {
        item(ui, "构建 SELECT 查询…", action, || {
            BrowseAction::Builder(object.clone())
        });
    }
}

fn item(
    ui: &mut egui::Ui,
    label: &str,
    action: &mut Option<BrowseAction>,
    next: impl FnOnce() -> BrowseAction,
) {
    if ui.button(label).clicked() {
        *action = Some(next());
        ui.close();
    }
}

fn table_operation(
    ui: &mut egui::Ui,
    label: &str,
    object: &DbObject,
    action: &mut Option<BrowseAction>,
    make: fn(DbObject) -> Operation,
) {
    item(ui, label, action, || {
        BrowseAction::Manage(make(object.clone()))
    });
}

fn create_item(
    ui: &mut egui::Ui,
    label: &str,
    schema: &str,
    kind: ObjectKind,
    table: Option<&str>,
    action: &mut Option<BrowseAction>,
) {
    item(ui, label, action, || {
        BrowseAction::Manage(Operation::CreateObject {
            schema: schema.into(),
            kind,
            table: table.map(str::to_owned),
        })
    });
}

fn create_label(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Table => "新建表…",
        ObjectKind::View => "新建视图…",
        ObjectKind::Procedure => "新建存储过程…",
        ObjectKind::Function => "新建函数…",
        ObjectKind::Trigger => "新建触发器…",
        ObjectKind::Event => "新建事件…",
    }
}

fn drop_label(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Table => "删除表…",
        ObjectKind::View => "删除视图…",
        ObjectKind::Procedure => "删除存储过程…",
        ObjectKind::Function => "删除函数…",
        ObjectKind::Trigger => "删除触发器…",
        ObjectKind::Event => "删除事件…",
    }
}

fn keyword(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Table => "TABLE",
        ObjectKind::View => "VIEW",
        ObjectKind::Procedure => "PROCEDURE",
        ObjectKind::Function => "FUNCTION",
        ObjectKind::Trigger => "TRIGGER",
        ObjectKind::Event => "EVENT",
    }
}

fn qualified(schema: &str, object: &str) -> String {
    format!("{}.{}", quote_identifier(schema), quote_identifier(object))
}

fn drop_object(object: &DbObject) -> String {
    format!(
        "DROP {} {};",
        keyword(object.kind),
        qualified(&object.schema, &object.name)
    )
}

fn table_status_sql(object: &DbObject) -> String {
    let hex = object
        .name
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!(
        "SHOW TABLE STATUS FROM {} WHERE Name = CONVERT(X'{hex}' USING utf8mb4);",
        quote_identifier(&object.schema)
    )
}

pub(super) struct Plan {
    workspace: usize,
    op: Operation,
    name: String,
    second: String,
    third: String,
    fourth: String,
    fifth: String,
    include_data: bool,
    action: usize,
    contents: Option<database_dialog::ContentsDialog>,
    selective_copy: Option<SelectiveCopy>,
}

impl Plan {
    pub fn new(workspace: usize, op: Operation, target: &mut Workspace) -> Self {
        let mut contents = None;
        let mut selective_copy = None;
        match &op {
            Operation::CreateDatabase | Operation::AlterDatabase(_) => load_defaults(&op, target),
            Operation::CopyTable(object) => {
                selective_copy = Some(SelectiveCopy::new(object.clone(), target));
            }
            Operation::EditDefinition(object)
            | Operation::PasteStatement { object, .. }
            | Operation::ColumnOrder(object)
            | Operation::TableStructure(object) => {
                target.detail = None;
                target.send(
                    Command::Inspect {
                        object: object.clone(),
                    },
                    "正在读取对象定义…",
                );
            }
            Operation::TableEngine(_) => {
                target.engines.clear();
                target.send(Command::ListEngines, "正在读取可用存储引擎…");
            }
            Operation::DatabaseContents { schema, action } => {
                contents = Some(database_dialog::ContentsDialog::open(
                    target,
                    schema.clone(),
                    *action,
                ));
            }
            _ => {}
        }
        Self {
            workspace,
            op,
            name: String::new(),
            second: String::new(),
            third: String::new(),
            fourth: String::new(),
            fifth: String::new(),
            include_data: false,
            action: 0,
            contents,
            selective_copy,
        }
    }
}

fn load_defaults(op: &Operation, ws: &mut Workspace) {
    let schema = match op {
        Operation::CreateDatabase => None,
        Operation::AlterDatabase(schema) => Some(schema.clone()),
        _ => return,
    };
    ws.database_defaults = None;
    ws.send(
        Command::ReadDatabaseDefaults { schema },
        "正在读取字符集和排序规则…",
    );
}

pub(super) fn dialog(ctx: &egui::Context, plan: &mut Option<Plan>, workspaces: &mut [Workspace]) {
    let Some(form) = plan else { return };
    let Some(ws) = workspaces.get_mut(form.workspace) else {
        *plan = None;
        return;
    };
    if let Some(contents) = &mut form.contents {
        if contents.show(ctx, ws) {
            *plan = None;
        }
        return;
    }
    if let Some(copy) = &mut form.selective_copy {
        if copy.show(ctx, ws) {
            *plan = None;
        }
        return;
    }
    let mut close = false;
    let mut sql = None;
    let mut apply_now = false;
    let mut reload_defaults = false;
    let defaults = matches!(
        form.op,
        Operation::CreateDatabase | Operation::AlterDatabase(_)
    );
    let modal = crate::app::style::modal(ctx, egui::Id::new("object-action")).show(ctx, |ui| {
        ui.set_width((ctx.content_rect().width() - 64.0).clamp(400.0, 560.0));
        ui.horizontal(|ui| {
            ui.heading(heading(&form.op));
            if let Some(object) = target(&form.op) {
                ui.add_space(6.0);
                style::badge(
                    ui,
                    qualified(&object.schema, &object.name),
                    style::accent(),
                    style::accent_soft(),
                );
            }
        });
        ui.add_space(8.0);
        style::card(egui::Margin::same(12)).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            form_fields(ui, form, ws);
            if ws.busy() && needs_metadata(&form.op) {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("正在读取对象信息…").color(style::muted()));
                });
            }
            if defaults && ws.database_defaults.is_none() && !ws.busy() && ws.server.is_some() {
                reload_defaults = ui.button("重新读取字符集和排序规则").clicked();
            }
        });
        ui.add_space(8.0);
        let candidate = statement_for(form, ws);
        match &candidate {
            Ok(query) => {
                style::section_card(ui, "SQL 预览", |ui| {
                    let mut preview = query.as_str();
                    egui::ScrollArea::both()
                        .auto_shrink([false, false])
                        .max_height(210.0)
                        .show(ui, |ui| {
                            ui.add(
                                TextEdit::multiline(&mut preview)
                                    .code_editor()
                                    .desired_width(f32::INFINITY)
                                    .frame(false),
                            );
                        });
                });
            }
            Err(error) if !error.is_empty() => {
                style::banner(ui, error, style::muted(), style::panel());
            }
            _ => {}
        }
        style::dialog_footer(ui, |ui| {
            let label = if matches!(form.op, Operation::PasteStatement { .. }) {
                "插入当前查询"
            } else {
                "在编辑器中打开 SQL"
            };
            let ok = candidate.is_ok();
            if defaults {
                let apply_label = if matches!(form.op, Operation::CreateDatabase) {
                    "创建数据库"
                } else {
                    "应用修改"
                };
                let can_apply = ok && ws.server.is_some() && !ws.busy();
                if ui
                    .add_enabled(can_apply, style::primary_enabled(can_apply, apply_label))
                    .clicked()
                {
                    sql = candidate.as_ref().ok().cloned();
                    apply_now = true;
                }
                if ui.add_enabled(ok, egui::Button::new(label)).clicked() {
                    sql = candidate.ok();
                }
            } else if ui
                .add_enabled(ok, style::primary_enabled(ok, label))
                .clicked()
            {
                sql = candidate.ok();
            }
            close = ui.button("关闭").clicked();
        });
    });
    if reload_defaults {
        load_defaults(&form.op, ws);
    }
    if let Some(sql) = sql {
        if matches!(form.op, Operation::PasteStatement { .. }) {
            let tab = ws.tab_mut();
            let start = tab.focused_byte.min(tab.sql.len());
            let start = if tab.sql.is_char_boundary(start) {
                start
            } else {
                tab.sql.len()
            };
            let range = tab
                .selection
                .take()
                .filter(|range| tab.sql.get(range.clone()).is_some())
                .unwrap_or(start..start);
            tab.focused_byte = range.start + sql.len();
            tab.sql.replace_range(range, &sql);
            ws.focus_request = true;
        } else if apply_now {
            ws.new_tab(sql.clone(), None);
            if matches!(form.op, Operation::CreateDatabase) {
                ws.start_create_database(sql, form.name.trim().into());
            } else {
                ws.start_batch(vec![sql]);
            }
        } else {
            ws.new_tab(sql, None);
        }
        *plan = None;
    } else if close || modal.should_close() {
        *plan = None;
    }
}

fn heading(op: &Operation) -> &'static str {
    match op {
        Operation::CreateDatabase => "新建数据库",
        Operation::AlterDatabase(_) => "修改数据库默认设置",
        Operation::CreateObject { kind, .. } => create_label(*kind).trim_end_matches('…'),
        Operation::EditDefinition(_) => "修改对象定义",
        Operation::PasteStatement { statement, .. } => statement.label(),
        Operation::Rename(_) => "重命名对象",
        Operation::CopyTable(_) => "复制表结构或数据",
        Operation::TableEngine(_) => "更改表的存储引擎",
        Operation::TableStructure(_) => "修改表字段",
        Operation::ColumnOrder(_) => "调整字段顺序",
        Operation::Index(_) => "管理索引",
        Operation::ForeignKey(_) => "管理外键",
        Operation::DatabaseContents { .. } => unreachable!("handled by database contents dialog"),
    }
}

fn target(op: &Operation) -> Option<&DbObject> {
    match op {
        Operation::EditDefinition(object)
        | Operation::PasteStatement { object, .. }
        | Operation::Rename(object)
        | Operation::CopyTable(object)
        | Operation::TableEngine(object)
        | Operation::TableStructure(object)
        | Operation::ColumnOrder(object)
        | Operation::Index(object)
        | Operation::ForeignKey(object) => Some(object),
        _ => None,
    }
}

fn needs_metadata(op: &Operation) -> bool {
    matches!(
        op,
        Operation::CreateDatabase
            | Operation::AlterDatabase(..)
            | Operation::EditDefinition(..)
            | Operation::PasteStatement { .. }
            | Operation::CopyTable(..)
            | Operation::ColumnOrder(..)
            | Operation::TableStructure(..)
            | Operation::TableEngine(..)
            | Operation::DatabaseContents { .. }
    )
}

fn text(ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.add(
            TextEdit::singleline(value)
                .desired_width(270.0)
                .hint_text(hint),
        );
    });
}

fn form_fields(ui: &mut egui::Ui, form: &mut Plan, ws: &Workspace) {
    match &form.op {
        Operation::CreateDatabase => database_dialog::default_fields(ui, form, ws, false),
        Operation::AlterDatabase(schema) => {
            ui.monospace(schema);
            database_dialog::default_fields(ui, form, ws, true);
        }
        Operation::CreateObject {
            schema,
            kind,
            table,
        } => {
            ui.label(format!("数据库：{schema}"));
            text(ui, "对象名称", &mut form.name, "");
            if *kind == ObjectKind::Trigger && table.is_none() {
                text(ui, "所属表", &mut form.second, "");
            }
        }
        Operation::Rename(_) => text(ui, "新名称", &mut form.name, ""),
        Operation::CopyTable(_) => unreachable!("handled by selective copy dialog"),
        Operation::TableEngine(_) => {
            ui.horizontal(|ui| {
                ui.label("存储引擎");
                egui::ComboBox::from_id_salt("storage-engine")
                    .selected_text(if form.name.is_empty() {
                        "请选择"
                    } else {
                        form.name.as_str()
                    })
                    .show_ui(ui, |ui| {
                        for engine in &ws.engines {
                            ui.selectable_value(&mut form.name, engine.clone(), engine);
                        }
                    });
            });
        }
        Operation::TableStructure(_) => {
            let previous_mode = form.action;
            modes(ui, &mut form.action, &["添加字段", "修改字段", "删除字段"]);
            if form.action != previous_mode {
                form.name.clear();
                form.second.clear();
            }
            if form.action == 0 {
                text(ui, "字段名", &mut form.name, "");
            } else if let Ok(detail) = detail_for(ws, &form.op) {
                let previous_name = form.name.clone();
                select_column(ui, "字段名", &detail.columns, &mut form.name, false);
                if form.action == 1 && previous_name != form.name {
                    let quoted = quote_identifier(&form.name);
                    form.second = column_definition(&detail.ddl, &form.name)
                        .as_deref()
                        .and_then(|definition| definition.strip_prefix(&quoted))
                        .map_or_else(String::new, |definition| definition.trim().to_owned());
                }
            }
            if form.action != 2 {
                text(
                    ui,
                    "字段定义",
                    &mut form.second,
                    "例如 VARCHAR(100) NOT NULL",
                );
            }
        }
        Operation::ColumnOrder(_) => {
            let detail = detail_for(ws, &form.op);
            if let Ok(detail) = detail {
                select_column(ui, "移动字段", &detail.columns, &mut form.name, false);
                select_column(ui, "放在此字段后", &detail.columns, &mut form.second, true);
            }
        }
        Operation::Index(_) => {
            modes(ui, &mut form.action, &["查看索引", "添加索引", "删除索引"]);
            if form.action != 0 {
                text(ui, "索引名", &mut form.name, "");
            }
            if form.action == 1 {
                text(ui, "索引字段", &mut form.second, "用逗号分隔");
                ui.checkbox(&mut form.include_data, "唯一索引");
            }
        }
        Operation::ForeignKey(object) => {
            modes(ui, &mut form.action, &["查看定义", "添加外键", "删除外键"]);
            if form.action != 0 {
                text(ui, "外键名", &mut form.name, "");
            }
            if form.action == 1 {
                text(ui, "本表字段", &mut form.second, "用逗号分隔");
                text(ui, "引用数据库", &mut form.third, &object.schema);
                text(ui, "引用表", &mut form.fourth, "");
                text(ui, "引用字段", &mut form.fifth, "用逗号分隔");
            }
        }
        Operation::EditDefinition(_)
        | Operation::PasteStatement { .. }
        | Operation::DatabaseContents { .. } => {}
    }
}

fn modes(ui: &mut egui::Ui, chosen: &mut usize, names: &[&str]) {
    ui.horizontal(|ui| {
        for (index, name) in names.iter().enumerate() {
            ui.selectable_value(chosen, index, *name);
        }
    });
}

fn select_column(
    ui: &mut egui::Ui,
    label: &str,
    columns: &[ColumnInfo],
    selected: &mut String,
    first: bool,
) {
    ui.horizontal(|ui| {
        ui.label(label);
        egui::ComboBox::from_id_salt(label)
            .selected_text(if selected.is_empty() {
                if first { "移到首位" } else { "请选择" }
            } else {
                selected.as_str()
            })
            .show_ui(ui, |ui| {
                if first {
                    ui.selectable_value(selected, String::new(), "移到首位");
                }
                for column in columns {
                    ui.selectable_value(selected, column.name.clone(), &column.name);
                }
            });
    });
}

fn detail_for<'a>(ws: &'a Workspace, op: &Operation) -> Result<&'a ObjectDetail, String> {
    ws.detail
        .as_ref()
        .filter(|detail| Some(&detail.object) == target(op))
        .ok_or_else(|| {
            if ws.busy() {
                "正在读取对象定义…".into()
            } else {
                format!("无法读取对象定义：{}", ws.status)
            }
        })
}

fn required(value: &str, label: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("请填写{label}。"));
    }
    Ok(quote_identifier(value))
}

fn identifiers(value: &str) -> Result<Vec<String>, String> {
    value
        .split([',', '，'])
        .map(|value| required(value, "字段名"))
        .collect()
}

fn text_option(text: &str) -> Option<&str> {
    let text = text.trim();
    (!text.is_empty()).then_some(text)
}

fn statement_for(form: &Plan, ws: &Workspace) -> Result<String, String> {
    let obj = target(&form.op);
    let table = || {
        obj.map(|obj| qualified(&obj.schema, &obj.name))
            .unwrap_or_default()
    };
    let name = || required(&form.name, "名称");
    match &form.op {
        Operation::CreateDatabase => {
            name()?;
            let (charset, collation) = database_dialog::selected_defaults(form, ws)?;
            Ok(create_database(form.name.trim(), charset, collation))
        }
        Operation::AlterDatabase(schema) => {
            let (charset, collation) = database_dialog::selected_defaults(form, ws)?;
            alter_database(schema, charset, collation)
                .map_err(|_| "请选择要修改的字符集或排序规则。".to_owned())
        }
        Operation::CreateObject {
            schema,
            kind,
            table,
        } => {
            name()?;
            let table = table.as_deref().or_else(|| text_option(&form.second));
            if *kind == ObjectKind::Trigger && table.is_none() {
                return Err("请填写触发器所属表。".into());
            }
            Ok(create_object(schema, *kind, form.name.trim(), table))
        }
        Operation::EditDefinition(object) => {
            let detail = detail_for(ws, &form.op)?;
            if detail.ddl.is_empty() {
                return Err(detail
                    .ddl_error
                    .clone()
                    .unwrap_or_else(|| "服务器未返回对象定义。".into()));
            }
            if object.kind == ObjectKind::View {
                let ddl = detail.ddl.replacen("CREATE ", "ALTER ", 1);
                Ok(format!("USE {};\n{ddl};", quote_identifier(&object.schema)))
            } else {
                Ok(delimited(
                    format!("USE {};\n", quote_identifier(&object.schema)),
                    &detail.ddl,
                ))
            }
        }
        Operation::PasteStatement { statement, .. } => {
            sql_template(*statement, &table(), &detail_for(ws, &form.op)?.columns)
        }
        Operation::Rename(object) => {
            name()?;
            Ok(format!(
                "RENAME TABLE {} TO {};",
                table(),
                qualified(&object.schema, form.name.trim())
            ))
        }
        Operation::CopyTable(_) => unreachable!("handled by selective copy dialog"),
        Operation::TableEngine(_) => {
            let engine = form.name.trim();
            if engine.is_empty()
                || !engine
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            {
                return Err("请输入有效的存储引擎名称。".into());
            }
            Ok(format!("ALTER TABLE {} ENGINE = {engine};", table()))
        }
        Operation::TableStructure(_) => {
            let column = name()?;
            let table = table();
            if form.action == 2 {
                return Ok(format!("ALTER TABLE {table} DROP COLUMN {column};"));
            }
            let definition = text_option(&form.second).ok_or("请填写字段定义。")?;
            let verb = if form.action == 0 { "ADD" } else { "MODIFY" };
            Ok(format!(
                "ALTER TABLE {table} {verb} COLUMN {column} {definition};"
            ))
        }
        Operation::ColumnOrder(_) => {
            name()?;
            if form.name == form.second {
                return Err("请选择其他参照字段，或将字段移到首位。".into());
            }
            let detail = detail_for(ws, &form.op)?;
            let definition = column_definition(&detail.ddl, form.name.trim())
                .ok_or("无法从表定义提取完整字段声明。")?;
            let placement = text_option(&form.second).map_or_else(
                || "FIRST".into(),
                |after| format!("AFTER {}", quote_identifier(after)),
            );
            Ok(format!(
                "ALTER TABLE {} MODIFY COLUMN {definition} {placement};",
                table()
            ))
        }
        Operation::Index(_) => match form.action {
            0 => Ok(format!("SHOW INDEX FROM {};", table())),
            1 => {
                let unique = if form.include_data { "UNIQUE " } else { "" };
                Ok(format!(
                    "ALTER TABLE {} ADD {unique}INDEX {} ({});",
                    table(),
                    name()?,
                    identifiers(&form.second)?.join(", ")
                ))
            }
            _ => Ok(format!("ALTER TABLE {} DROP INDEX {};", table(), name()?)),
        },
        Operation::ForeignKey(object) => match form.action {
            0 => Ok(format!("SHOW CREATE TABLE {};", table())),
            1 => {
                let database = text_option(&form.third).unwrap_or(&object.schema);
                let key = name()?;
                let parent = qualified(database, form.fourth.trim());
                required(&form.fourth, "引用表")?;
                let child_cols = identifiers(&form.second)?;
                let parent_cols = identifiers(&form.fifth)?;
                if child_cols.len() != parent_cols.len() {
                    return Err("本表和引用表的字段数必须一致。".into());
                }
                Ok(format!(
                    "ALTER TABLE {} ADD CONSTRAINT {key} FOREIGN KEY ({}) REFERENCES {parent} ({});",
                    table(),
                    child_cols.join(", "),
                    parent_cols.join(", ")
                ))
            }
            _ => Ok(format!(
                "ALTER TABLE {} DROP FOREIGN KEY {};",
                table(),
                name()?
            )),
        },
        Operation::DatabaseContents { .. } => unreachable!("handled by database contents dialog"),
    }
}

fn writable_columns(columns: &[ColumnInfo]) -> Vec<String> {
    columns
        .iter()
        .filter(|column| {
            !column
                .extra
                .split_whitespace()
                .any(|word| word.eq_ignore_ascii_case("GENERATED"))
        })
        .map(|column| quote_identifier(&column.name))
        .collect()
}

fn sql_template(
    statement: Statement,
    table: &str,
    columns: &[ColumnInfo],
) -> Result<String, String> {
    let all = columns
        .iter()
        .map(|col| quote_identifier(&col.name))
        .collect::<Vec<_>>();
    let writable = writable_columns(columns);
    if all.is_empty() {
        return Err("未读到字段定义。".into());
    }
    match statement {
        Statement::Select => Ok(format!("SELECT {} FROM {table};", all.join(", "))),
        Statement::Delete => Ok(format!(
            "DELETE FROM {table}\nWHERE 1 = 0; -- 请填写筛选条件"
        )),
        Statement::Insert if !writable.is_empty() => Ok(format!(
            "INSERT INTO {table} ({})\nVALUES ({});",
            writable.join(", "),
            vec!["DEFAULT"; writable.len()].join(", ")
        )),
        Statement::Update if !writable.is_empty() => {
            let pairs = writable
                .iter()
                .map(|name| format!("{name} = {name}"))
                .collect::<Vec<_>>();
            Ok(format!(
                "UPDATE {table}\nSET {}\nWHERE 1 = 0; -- 请填写筛选条件",
                pairs.join(", ")
            ))
        }
        _ => Err("没有可写字段。".into()),
    }
}

pub(super) fn create_view_draft(schema: &str, name: &str, select: &str) -> String {
    let object = qualified(schema, name);
    format!(
        "CREATE
    /* 可在这里填写视图选项
       ALGORITHM = UNDEFINED | MERGE | TEMPTABLE
       DEFINER = CURRENT_USER 或 '用户'@'主机'
       SQL SECURITY DEFINER | INVOKER
    */
    VIEW {object} AS
{select}"
    )
}

fn create_object(schema: &str, kind: ObjectKind, name: &str, table: Option<&str>) -> String {
    let object = qualified(schema, name);
    match kind {
        ObjectKind::Table => format!(
            "CREATE TABLE {object} (\n    id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,\n    PRIMARY KEY (id)\n);"
        ),
        ObjectKind::View => create_view_draft(schema, name, "SELECT 1 AS value;"),
        ObjectKind::Procedure => delimited(
            String::new(),
            &format!("CREATE PROCEDURE {object}()\nBEGIN\n    SELECT 1;\nEND"),
        ),
        ObjectKind::Function => delimited(
            String::new(),
            &format!(
                "CREATE FUNCTION {object}()\nRETURNS INT DETERMINISTIC\nBEGIN\n    RETURN 1;\nEND"
            ),
        ),
        ObjectKind::Trigger => {
            let table = qualified(schema, table.unwrap_or_default());
            delimited(
                String::new(),
                &format!(
                    "CREATE TRIGGER {object} BEFORE INSERT ON {table}\nFOR EACH ROW\nBEGIN\n    -- 在这里编写触发器逻辑\nEND"
                ),
            )
        }
        ObjectKind::Event => delimited(
            String::new(),
            &format!(
                "CREATE EVENT {object}\nON SCHEDULE EVERY 1 DAY\nDISABLE\nDO\nBEGIN\n    -- 在这里编写事件任务\nEND"
            ),
        ),
    }
}

fn delimited(prefix: String, body: &str) -> String {
    let mut delimiter = "$$".to_owned();
    while body.contains(&delimiter) {
        delimiter.push('$');
    }
    format!("{prefix}DELIMITER {delimiter}\n{body}{delimiter}\nDELIMITER ;\n")
}

fn column_definition(ddl: &str, name: &str) -> Option<String> {
    let quoted = quote_identifier(name);
    ddl.lines().map(str::trim).find_map(|line| {
        line.strip_prefix(&quoted)
            .filter(|suffix| suffix.starts_with(char::is_whitespace))
            .map(|_| line.trim_end_matches(',').trim_end().to_owned())
    })
}
