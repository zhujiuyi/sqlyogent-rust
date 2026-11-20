mod accounts;

use eframe::egui::{self, ComboBox, RichText, TextEdit};

use crate::database::{Command, ObjectKind};
use crate::maintenance::{
    FlushCapabilities, FlushPlan, FlushTarget, Privilege, PrivilegeScope, alter_database,
    copy_table_structure, create_database, drop_database, grant_privileges, kill_process,
    rename_table, revoke_privileges, show_grants,
};

use super::{state::Workspace, style};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Operation {
    CreateDatabase,
    AlterDatabase,
    DropDatabase,
    RenameTable,
    CopyTableStructure,
    ShowGrants,
    Grant,
    Revoke,
    Flush,
    KillQuery,
    KillConnection,
}

const OPERATIONS: [(Operation, &str); 11] = [
    (Operation::CreateDatabase, "新建数据库"),
    (Operation::AlterDatabase, "修改数据库默认设置"),
    (Operation::DropDatabase, "删除数据库"),
    (Operation::RenameTable, "重命名表"),
    (Operation::CopyTableStructure, "复制表结构"),
    (Operation::ShowGrants, "查看账户权限"),
    (Operation::Grant, "授予权限"),
    (Operation::Revoke, "撤销权限"),
    (Operation::Flush, "刷新服务器资源"),
    (Operation::KillQuery, "终止查询"),
    (Operation::KillConnection, "断开连接"),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Global,
    Database,
    Table,
    Columns,
}

const SCOPES: [(Scope, &str); 4] = [
    (Scope::Global, "全局"),
    (Scope::Database, "数据库"),
    (Scope::Table, "表"),
    (Scope::Columns, "列"),
];

struct FlushOptions {
    selected: [bool; FlushTarget::ALL.len()],
    all: bool,
    no_write_to_binlog: bool,
}

impl Default for FlushOptions {
    fn default() -> Self {
        let mut selected = [false; FlushTarget::ALL.len()];
        selected[FlushTarget::Logs as usize] = true;
        Self {
            selected,
            all: false,
            no_write_to_binlog: false,
        }
    }
}

impl FlushOptions {
    fn show(&mut self, ui: &mut egui::Ui, server: FlushCapabilities) {
        use FlushTarget::*;
        let groups: [&[FlushTarget]; 2] = [
            &[Logs, Privileges, Tables, DesKeyFile, UserResources],
            &[Hosts, Status, ReadLock, QueryCache],
        ];
        ui.label("刷新作用于当前连接的整个服务器。");
        ui.columns(2, |columns| {
            for (column, targets) in columns.iter_mut().zip(groups) {
                for &target in targets {
                    let unavailable = server.unavailable(target);
                    let selected = &mut self.selected[target as usize];
                    let mut checked = unavailable.is_none() && (self.all || *selected);
                    let response = column.add_enabled(
                        !self.all && unavailable.is_none(),
                        egui::Checkbox::new(&mut checked, target.label()),
                    );
                    if response.changed() {
                        *selected = checked;
                    }
                    if let Some(reason) = unavailable {
                        response.on_disabled_hover_text(reason);
                    } else if target == ReadLock {
                        let hint = "读锁会短暂阻止写入，刷新完成后自动释放。";
                        response.on_hover_text(hint).on_disabled_hover_text(hint);
                    }
                }
            }
        });
        ui.checkbox(&mut self.all, "全部可用项目");
        ui.add_enabled(
            server.supports_no_write_to_binlog(),
            egui::Checkbox::new(
                &mut self.no_write_to_binlog,
                "跳过二进制日志（NO_WRITE_TO_BINLOG）",
            ),
        )
        .on_hover_text("在刷新语句中加入跳过二进制日志的选项，由服务器解释。")
        .on_disabled_hover_text("MySQL 4.1 起支持此选项。");
    }
}

pub(super) struct Admin {
    workspace_key: u64,
    account_manager: Option<accounts::Manager>,
    initialized: bool,
    flush_only: bool,
    operation: Operation,
    database: String,
    table: String,
    new_database: String,
    target_database: String,
    target_table: String,
    charset: String,
    collation: String,
    user: String,
    host: String,
    scope: Scope,
    columns: String,
    privileges: Vec<Privilege>,
    flush: FlushOptions,
    process: String,
}

impl Admin {
    pub(super) fn new(workspace_key: u64) -> Self {
        Self {
            workspace_key,
            account_manager: None,
            initialized: false,
            flush_only: false,
            operation: Operation::CreateDatabase,
            database: String::new(),
            table: String::new(),
            new_database: String::new(),
            target_database: String::new(),
            target_table: String::new(),
            charset: String::new(),
            collation: String::new(),
            user: String::new(),
            host: "localhost".into(),
            scope: Scope::Database,
            columns: String::new(),
            privileges: Vec::new(),
            flush: FlushOptions::default(),
            process: String::new(),
        }
    }

    pub(super) fn accounts(workspace_key: u64) -> Self {
        Self {
            account_manager: Some(accounts::Manager::default()),
            ..Self::new(workspace_key)
        }
    }

    pub(super) fn flush(workspace_key: u64) -> Self {
        Self {
            operation: Operation::Flush,
            flush_only: true,
            ..Self::new(workspace_key)
        }
    }

    fn fill_workspace_context(&mut self, workspace: &Workspace) {
        if self.initialized {
            return;
        }
        self.initialized = true;
        if let Some(object) = &workspace.selected {
            self.database = object.schema.clone();
            if object.kind == ObjectKind::Table {
                self.table = object.name.clone();
            }
        } else if let Some(database) = workspace
            .server
            .as_ref()
            .and_then(|server| server.database.as_ref())
        {
            self.database = database.clone();
        }
        self.target_database = self.database.clone();
    }

    fn fields(&mut self, ui: &mut egui::Ui, server: FlushCapabilities) {
        if !self.flush_only {
            choose(ui, ("Operation", "操作"), &mut self.operation, &OPERATIONS);
        }
        match self.operation {
            Operation::CreateDatabase => {
                field(ui, "新数据库名称", &mut self.new_database);
                self.defaults_fields(ui);
            }
            Operation::AlterDatabase => {
                field(ui, "数据库", &mut self.database);
                self.defaults_fields(ui);
            }
            Operation::DropDatabase => field(ui, "数据库", &mut self.database),
            Operation::RenameTable | Operation::CopyTableStructure => {
                field(ui, "源数据库", &mut self.database);
                field(ui, "源表", &mut self.table);
                if self.operation == Operation::CopyTableStructure {
                    field(ui, "目标数据库", &mut self.target_database);
                }
                field(ui, "目标表", &mut self.target_table);
            }
            Operation::ShowGrants | Operation::Grant | Operation::Revoke => {
                field(ui, "用户名", &mut self.user);
                field(ui, "账户主机", &mut self.host);
                if self.operation != Operation::ShowGrants {
                    self.privilege_fields(ui);
                }
            }
            Operation::Flush => self.flush.show(ui, server),
            Operation::KillQuery | Operation::KillConnection => {
                field(ui, "连接 ID", &mut self.process);
            }
        }
    }

    fn defaults_fields(&mut self, ui: &mut egui::Ui) {
        field(ui, "字符集", &mut self.charset);
        field(ui, "排序规则", &mut self.collation);
    }

    fn privilege_fields(&mut self, ui: &mut egui::Ui) {
        choose(ui, ("Scope", "权限范围"), &mut self.scope, &SCOPES);
        if self.scope != Scope::Global {
            field(ui, "数据库", &mut self.database);
        }
        if matches!(self.scope, Scope::Table | Scope::Columns) {
            field(ui, "表名", &mut self.table);
        }
        if self.scope == Scope::Columns {
            ui.label("列名（每行一个）");
            ui.add(
                TextEdit::multiline(&mut self.columns)
                    .desired_width(472.0)
                    .desired_rows(2),
            );
        }
        ui.label("权限");
        ui.horizontal_wrapped(|ui| {
            for privilege in Privilege::ALL {
                let mut selected = self.privileges.contains(&privilege);
                if ui.checkbox(&mut selected, privilege.label()).changed() {
                    if selected {
                        self.privileges.push(privilege);
                    } else {
                        self.privileges.retain(|value| *value != privilege);
                    }
                }
            }
        });
    }

    fn sql(&self, flush_plan: Option<&FlushPlan>) -> Result<String, &'static str> {
        let database = || required(&self.database, "请输入数据库名称");
        let table = || required(&self.table, "请输入表名");
        let target_table = || required(&self.target_table, "请输入目标表名");
        let charset = optional(&self.charset);
        let collation = optional(&self.collation);
        match self.operation {
            Operation::CreateDatabase => {
                let name = required(&self.new_database, "请输入数据库名称")?;
                Ok(create_database(name, charset, collation))
            }
            Operation::AlterDatabase => alter_database(database()?, charset, collation),
            Operation::DropDatabase => Ok(drop_database(database()?)),
            Operation::RenameTable => Ok(rename_table(database()?, table()?, target_table()?)),
            Operation::CopyTableStructure => {
                let target_database = required(&self.target_database, "请输入目标数据库名称")?;
                Ok(copy_table_structure(
                    database()?,
                    table()?,
                    target_database,
                    target_table()?,
                ))
            }
            Operation::ShowGrants | Operation::Grant | Operation::Revoke => {
                let user = required(&self.user, "请输入用户名")?;
                let host = required(&self.host, "请输入账户主机")?;
                if self.operation == Operation::ShowGrants {
                    return Ok(show_grants(user, host));
                }
                let columns = self
                    .columns
                    .lines()
                    .filter(|name| !name.trim().is_empty())
                    .collect::<Vec<_>>();
                let scope = match self.scope {
                    Scope::Global => PrivilegeScope::Global,
                    Scope::Database => PrivilegeScope::Database(database()?),
                    Scope::Table => PrivilegeScope::Table {
                        database: database()?,
                        table: table()?,
                    },
                    Scope::Columns => PrivilegeScope::Columns {
                        database: database()?,
                        table: table()?,
                        columns: &columns,
                    },
                };
                if self.operation == Operation::Grant {
                    grant_privileges(user, host, scope, &self.privileges)
                } else {
                    revoke_privileges(user, host, scope, &self.privileges)
                }
            }
            Operation::Flush => flush_plan.map(FlushPlan::sql).ok_or("请选择要刷新的项目。"),
            Operation::KillQuery | Operation::KillConnection => {
                let id = self
                    .process
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| "请输入有效的连接 ID（正整数）")?;
                if id == 0 {
                    return Err("连接 ID 必须大于 0");
                }
                Ok(kill_process(id, self.operation == Operation::KillQuery))
            }
        }
    }
}

pub(super) fn dialog(ctx: &egui::Context, state: &mut Option<Admin>, workspaces: &mut [Workspace]) {
    let Some(admin) = state else { return };
    let Some(workspace) = workspaces
        .iter_mut()
        .find(|workspace| workspace.key == admin.workspace_key)
    else {
        *state = None;
        return;
    };
    if let Some(manager) = &mut admin.account_manager {
        if manager.show(ctx, workspace) {
            *state = None;
        }
        return;
    }
    admin.fill_workspace_context(workspace);
    let flush_server = FlushCapabilities::from_version(
        workspace
            .server
            .as_ref()
            .map_or("", |server| &server.version),
    );
    let response = crate::app::style::modal(ctx, egui::Id::new("administration")).show(ctx, |ui| {
        ui.set_width(490.0);
        ui.heading(if admin.flush_only {
            "刷新服务器资源"
        } else {
            "服务器管理"
        });
        ui.label(RichText::new(&workspace.label).color(style::MUTED));
        ui.separator();
        egui::ScrollArea::vertical()
            .max_height(365.0)
            .show(ui, |ui| admin.fields(ui, flush_server));
        ui.separator();
        let flush_plan = if admin.operation == Operation::Flush {
            FlushPlan::new(
                flush_server,
                &admin.flush.selected,
                admin.flush.all,
                admin.flush.no_write_to_binlog,
            )
        } else {
            None
        };
        let sql = admin.sql(flush_plan.as_ref());
        match &sql {
            Ok(sql) => {
                let mut preview = sql.as_str();
                ui.add(
                    TextEdit::multiline(&mut preview)
                        .font(egui::TextStyle::Monospace)
                        .desired_width(472.0)
                        .desired_rows(3),
                );
            }
            Err(error) => {
                ui.label(RichText::new(*error).color(style::MUTED));
            }
        }
        let (open, execute, cancel) = ui
            .horizontal(|ui| {
                let execute = admin.operation == Operation::Flush
                    && ui
                        .add_enabled(
                            flush_plan.is_some() && workspace.server.is_some() && !workspace.busy(),
                            style::primary("执行刷新"),
                        )
                        .clicked();
                let open = ui
                    .add_enabled(
                        sql.is_ok(),
                        if admin.operation == Operation::Flush {
                            egui::Button::new("在编辑器中打开")
                        } else {
                            style::primary("在编辑器中打开")
                        },
                    )
                    .clicked();
                let cancel = ui.button("取消").clicked();
                (open, execute, cancel)
            })
            .inner;
        (
            if open { sql.ok() } else { None },
            if execute { flush_plan } else { None },
            cancel,
        )
    });
    let should_close = response.should_close();
    let (sql, execute, cancel) = response.inner;
    if let Some(plan) = execute {
        let sql = plan.sql();
        if workspace.send(Command::FlushResources(plan), "正在刷新服务器资源…") {
            workspace.record_pending_history(sql);
            *state = None;
        }
    } else if let Some(sql) = sql {
        workspace.new_tab(sql, None);
        *state = None;
    } else if cancel || should_close {
        *state = None;
    }
}

fn field(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.horizontal(|ui| {
        ui.add_sized([130.0, 24.0], egui::Label::new(label));
        ui.add(TextEdit::singleline(value).desired_width(330.0));
    });
}

fn choose<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    (id, label): (&str, &str),
    selected: &mut T,
    choices: &[(T, &str)],
) {
    ui.horizontal(|ui| {
        ui.add_sized([130.0, 24.0], egui::Label::new(label));
        let text = choices
            .iter()
            .find(|(value, _)| *value == *selected)
            .map_or("", |(_, text)| *text);
        ComboBox::from_id_salt(id)
            .selected_text(text)
            .show_ui(ui, |ui| {
                for &(value, text) in choices {
                    ui.selectable_value(selected, value, text);
                }
            });
    });
}

fn optional(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn required<'a>(value: &'a str, missing: &'static str) -> Result<&'a str, &'static str> {
    if value.trim().is_empty() {
        Err(missing)
    } else if value.contains('\0') {
        Err("名称中不能包含 NUL 字符")
    } else {
        Ok(value)
    }
}
