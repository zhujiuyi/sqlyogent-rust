use std::collections::BTreeSet;

use eframe::egui::{self, ComboBox, TextEdit};

use crate::database::{
    AccountCommand, AccountName, AccountReply, Command, GrantScope, GrantSnapshot, ObjectKind,
    PrivilegeChanges,
};

use super::{Workspace, choose, field, style};

#[derive(Default)]
pub(super) struct Manager {
    started: bool,
    pending: bool,
    accounts: Vec<AccountName>,
    create_privileges: BTreeSet<String>,
    selected: Option<AccountName>,
    snapshot: Option<GrantSnapshot>,
    draft: Option<PrivilegeChanges>,
    scope_form: Option<ScopeForm>,
    form: Option<AccountForm>,
    review: Option<Review>,
    notice: String,
    error: String,
}

impl Manager {
    pub(super) fn show(&mut self, ctx: &egui::Context, ws: &mut Workspace) -> bool {
        self.receive(ws);
        let available = ws.server.is_some();
        if available && !self.started && !ws.busy() {
            self.started = true;
            self.send(ws, AccountCommand::List);
        }

        let modal =
            crate::app::style::modal(ctx, egui::Id::new("account-manager")).show(ctx, |ui| {
                let frame = ctx.content_rect();
                ui.set_width((frame.width() - 64.0).clamp(480.0, 720.0));
                ui.horizontal(|ui| {
                    style::page_title(ui, "用户与权限");
                    ui.add_space(6.0);
                    style::badge(ui, &ws.label, style::muted(), style::chrome());
                });
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .max_height((frame.height() * 0.62).clamp(170.0, 500.0))
                    .show(ui, |ui| {
                        if !self.notice.is_empty() {
                            style::banner(ui, &self.notice, style::success(), style::success_soft());
                            ui.add_space(6.0);
                        }
                        if !self.error.is_empty() {
                            style::banner(ui, &self.error, style::error(), style::error_soft());
                            ui.add_space(6.0);
                        }
                        if !available {
                            ui.label(
                                egui::RichText::new("请先连接数据库服务器。").color(style::muted()),
                            );
                        } else if ws.busy() {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(&ws.status);
                            });
                        } else if self.review.is_some() {
                            self.review(ui, ws);
                        } else if self.form.is_some() {
                            self.account_form(ui, ws);
                        } else {
                            self.browse(ui, ws);
                        }
                    });
                style::dialog_footer(ui, |ui| {
                    ui.add_enabled(!self.pending, egui::Button::new("关闭"))
                        .clicked()
                })
            });
        modal.inner || (!self.pending && modal.should_close())
    }

    fn send(&mut self, ws: &mut Workspace, command: AccountCommand) {
        let status = match &command {
            AccountCommand::List => "正在读取账户列表…",
            AccountCommand::ListPrivileges => "正在读取服务器的权限目录…",
            AccountCommand::Read(_) => "正在读取账户权限…",
            _ => "正在处理账户管理请求…",
        };
        ws.account_reply = None;
        self.pending = ws.send(Command::Accounts(command), status);
        if !self.pending {
            self.report_error(&ws.status);
        }
    }

    fn report_error(&mut self, error: &str) {
        if !self.error.is_empty() {
            self.error.push(' ');
        }
        self.error.push_str(error);
    }

    fn receive(&mut self, ws: &mut Workspace) {
        if !self.pending || ws.busy() {
            return;
        }
        self.pending = false;
        match ws.account_reply.take() {
            Some(Ok(AccountReply::Listed(accounts))) => {
                self.accounts = accounts;
                let account = self
                    .selected
                    .as_ref()
                    .and_then(|selected| {
                        self.accounts.iter().find(|account| {
                            account.user == selected.user
                                && account.host.eq_ignore_ascii_case(&selected.host)
                        })
                    })
                    .or_else(|| self.accounts.first())
                    .cloned();
                if let Some(account) = account {
                    self.select_account(ws, account);
                } else {
                    self.selected = None;
                    self.snapshot = None;
                    self.draft = None;
                }
            }
            Some(Ok(AccountReply::Privileges(privileges))) => {
                self.create_privileges = privileges;
            }
            Some(Ok(AccountReply::Read(snapshot))) => self.load_snapshot(snapshot),
            Some(Ok(AccountReply::Changed {
                selected,
                message,
                warning,
            })) => {
                self.selected = selected;
                self.snapshot = None;
                self.draft = None;
                self.notice = message.into();
                self.error = warning.unwrap_or_default();
                self.send(ws, AccountCommand::List);
            }
            Some(Ok(AccountReply::Applied(result))) => {
                let scope = self.draft.as_ref().map(|draft| draft.scope.clone());
                self.draft = None;
                self.snapshot = result.refreshed;
                if let Some(scope) = scope {
                    self.select_scope(scope);
                }
                self.notice = if result.error.is_none() {
                    format!("已执行 {} 条语句并核对服务器权限。", result.completed)
                } else if result.total > 0 {
                    format!(
                        "服务器已确认执行 {} / {} 条语句。",
                        result.completed, result.total
                    )
                } else {
                    String::new()
                };
                self.error = result.error.unwrap_or_default();
            }
            Some(Err(error)) => self.report_error(&error),
            None => self.report_error("未收到服务器响应，请重新读取账户和权限。"),
        }
    }

    fn select_account(&mut self, ws: &mut Workspace, account: AccountName) {
        self.selected = Some(account.clone());
        self.snapshot = None;
        self.draft = None;
        self.scope_form = None;
        self.send(ws, AccountCommand::Read(account));
    }

    fn load_snapshot(&mut self, snapshot: GrantSnapshot) {
        let scope = self
            .draft
            .as_ref()
            .map_or(GrantScope::Global, |draft| draft.scope.clone());
        self.snapshot = Some(snapshot);
        self.select_scope(scope);
    }

    fn select_scope(&mut self, scope: GrantScope) {
        if let Some(snapshot) = &self.snapshot {
            let grants = snapshot.scopes.get(&scope).cloned().unwrap_or_default();
            self.draft = Some(PrivilegeChanges {
                scope,
                names: grants.names,
                grant_option: grants.grant_option,
            });
        }
    }

    fn browse(&mut self, ui: &mut egui::Ui, ws: &mut Workspace) {
        let picked = style::subtle_card(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label("账户");
                    let selected = self
                        .selected
                        .as_ref()
                        .map_or_else(|| "选择账户".into(), AccountName::label);
                    let picked = ComboBox::from_id_salt("account-picker")
                        .selected_text(selected)
                        .width(280.0)
                        .show_ui(ui, |ui| {
                            let mut picked = None;
                            for account in &self.accounts {
                                if ui
                                    .selectable_label(
                                        self.selected.as_ref() == Some(account),
                                        account.label(),
                                    )
                                    .clicked()
                                {
                                    picked = Some(account.clone());
                                }
                            }
                            if self.accounts.is_empty() {
                                ui.label("暂无可列出的账户");
                            }
                            picked
                        })
                        .inner
                        .flatten();
                    if ui.button("指定账户…").clicked() {
                        self.open_form(
                            AccountAction::Read,
                            AccountName {
                                user: String::new(),
                                host: "localhost".into(),
                            },
                        );
                    }
                    if ui.button("刷新账户列表").clicked() {
                        self.error.clear();
                        self.notice.clear();
                        self.send(ws, AccountCommand::List);
                    }
                    picked
                })
                .inner
            })
            .inner;
        if let Some(account) = picked {
            self.error.clear();
            self.notice.clear();
            self.select_account(ws, account);
            return;
        }

        style::subtle_card(egui::Margin::symmetric(10, 8)).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                if ui.button("新建账户…").clicked() {
                    self.open_form(
                        AccountAction::Create,
                        AccountName {
                            user: String::new(),
                            host: "localhost".into(),
                        },
                    );
                    self.create_privileges.clear();
                    self.send(ws, AccountCommand::ListPrivileges);
                }
                if let Some(account) = self.selected.clone() {
                    if ui.button("改名或主机…").clicked() {
                        self.open_form(AccountAction::Rename, account.clone());
                    }
                    if ui.button("修改密码…").clicked() {
                        self.open_form(AccountAction::Password, account.clone());
                    }
                    if style::menu_item_danger(ui, "删除账户…").clicked() {
                        self.error.clear();
                        self.notice.clear();
                        self.review = Some(Review {
                            label: "删除账户",
                            details: vec![format!(
                                "删除 {} 及其直接授权。此操作无法撤销。",
                                account.label()
                            )],
                            command: AccountCommand::Drop(account),
                        });
                    }
                }
            });
        });
        if self.form.is_some() || self.review.is_some() {
            return;
        }

        if self.snapshot.is_some() {
            ui.separator();
            self.privilege_editor(ui, ws);
        } else if let Some(account) = self.selected.clone()
            && ui.button("读取账户权限").clicked()
        {
            self.error.clear();
            self.select_account(ws, account);
        }
    }

    fn open_form(&mut self, action: AccountAction, target: AccountName) {
        self.notice.clear();
        self.error.clear();
        self.form = Some(AccountForm {
            action,
            target,
            password: String::new(),
            confirmation: String::new(),
            allow_empty: false,
            privileges: BTreeSet::new(),
            grant_option: false,
        });
    }

    fn account_form(&mut self, ui: &mut egui::Ui, ws: &mut Workspace) {
        let Some(form) = &mut self.form else { return };
        ui.strong(form.action.label());
        if form.action == AccountAction::Rename
            && let Some(account) = &self.selected
        {
            ui.monospace(account.label());
        }
        if form.action == AccountAction::Password {
            ui.monospace(form.target.label());
        } else {
            field(ui, "用户名", &mut form.target.user);
            field(ui, "账户主机", &mut form.target.host);
            if form.target.user.is_empty() && form.action != AccountAction::Read {
                ui.colored_label(style::muted(), "用户名为空表示匿名账户。");
            }
        }
        if form.action.uses_password() {
            password_field(ui, "新密码", "account-password", &mut form.password);
            password_field(
                ui,
                "重复密码",
                "account-password-confirmation",
                &mut form.confirmation,
            );
            if form.password.is_empty() && form.confirmation.is_empty() {
                ui.checkbox(&mut form.allow_empty, "为账户设置空密码");
            }
        }
        if form.action == AccountAction::Create && !self.create_privileges.is_empty() {
            ui.separator();
            ui.strong("全局权限");
            privilege_checks(
                ui,
                self.create_privileges.iter().collect(),
                &mut form.privileges,
                None,
                true,
            );
            ui.checkbox(
                &mut form.grant_option,
                "允许转授全局静态权限（GRANT OPTION）",
            );
        }
        let validation = form.validate(self.selected.as_ref());
        if let Err(error) = validation {
            ui.colored_label(style::muted(), error);
        }
        let (submit, back) = ui
            .horizontal(|ui| {
                let label = if form.action == AccountAction::Read {
                    "读取权限"
                } else {
                    "核对操作"
                };
                let submit = ui
                    .add_enabled(validation.is_ok(), style::primary(label))
                    .clicked();
                let back = ui.button("返回").clicked();
                (submit, back)
            })
            .inner;
        if back {
            self.form = None;
        } else if submit && let Some(form) = self.form.take() {
            if form.action == AccountAction::Read {
                self.select_account(ws, form.target);
            } else {
                self.review = form.review(self.selected.clone());
            }
        }
    }

    fn privilege_editor(&mut self, ui: &mut egui::Ui, ws: &mut Workspace) {
        let (Some(snapshot), Some(draft)) = (&self.snapshot, &mut self.draft) else {
            return;
        };
        let mut selection = draft.scope.clone();
        ui.horizontal(|ui| {
            ui.label("权限范围");
            ComboBox::from_id_salt("account-grant-scope")
                .selected_text(draft.scope.label())
                .width(280.0)
                .show_ui(ui, |ui| {
                    for scope in snapshot.scopes.keys() {
                        ui.selectable_value(&mut selection, scope.clone(), scope.label());
                    }
                });
            if ui.button("指定对象…").clicked() {
                self.scope_form = Some(ScopeForm::from_workspace(ws));
            }
        });
        if let Some(form) = &mut self.scope_form {
            let (pick, cancel) = form.show(ui);
            if cancel {
                self.scope_form = None;
            }
            if let Some(scope) = pick {
                self.scope_form = None;
                self.select_scope(snapshot.literal_scope(scope));
            }
            return;
        }
        if selection != draft.scope {
            self.select_scope(selection);
            return;
        }
        ui.colored_label(
            style::muted(),
            "显示该范围的直接授权。上级范围和角色也可能提供访问权限。",
        );
        if snapshot.other_rules > 0 {
            ui.colored_label(
                style::muted(),
                format!(
                    "此账户还有 {} 条角色、代理或其它授权规则。",
                    snapshot.other_rules
                ),
            );
        }
        let editable = snapshot.editable(&draft.scope);
        let current = snapshot.scopes.get(&draft.scope);
        ui.add_enabled_ui(editable.is_ok(), |ui| {
            privilege_checks(
                ui,
                snapshot.choices(&draft.scope).collect(),
                &mut draft.names,
                current.map(|grants| &grants.delegated),
                draft.scope == GrantScope::Global,
            );
            if !matches!(draft.scope, GrantScope::Column { .. }) {
                ui.checkbox(
                    &mut draft.grant_option,
                    "允许转授此范围的静态权限（GRANT OPTION）",
                );
            }
        });
        let changes = snapshot.statements(draft);
        if let Err(error) = &changes {
            ui.colored_label(style::error(), *error);
        }
        let (review, reload) = ui
            .horizontal(|ui| {
                let review = ui
                    .add_enabled(
                        changes.as_ref().is_ok_and(|sql| !sql.is_empty()),
                        style::primary("核对权限变更"),
                    )
                    .clicked();
                (review, ui.button("重新读取权限").clicked())
            })
            .inner;
        if review && let Ok(statements) = changes {
            self.error.clear();
            self.notice.clear();
            self.review = Some(Review {
                label: "应用权限变更",
                details: statements,
                command: AccountCommand::Apply {
                    original: snapshot.clone(),
                    changes: PrivilegeChanges {
                        scope: draft.scope.clone(),
                        names: draft.names.clone(),
                        grant_option: draft.grant_option,
                    },
                },
            });
        } else if reload {
            let account = snapshot.account.clone();
            self.notice.clear();
            self.error.clear();
            self.send(ws, AccountCommand::Read(account));
        }
    }

    fn review(&mut self, ui: &mut egui::Ui, ws: &mut Workspace) {
        let Some(review) = &self.review else { return };
        ui.strong(review.label);
        for detail in &review.details {
            ui.monospace(detail);
        }
        let (apply, back) = ui
            .horizontal(|ui| {
                let apply = ui.button(format!("确认{}", review.label)).clicked();
                let back = ui.button("返回").clicked();
                (apply, back)
            })
            .inner;
        if back {
            self.review = None;
        } else if apply && let Some(review) = self.review.take() {
            self.send(ws, review.command);
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum AccountAction {
    Read,
    Create,
    Rename,
    Password,
}

impl AccountAction {
    fn label(self) -> &'static str {
        match self {
            Self::Read => "指定账户",
            Self::Create => "创建账户",
            Self::Rename => "更改账户名称或主机",
            Self::Password => "更新账户密码",
        }
    }

    fn uses_password(self) -> bool {
        matches!(self, Self::Create | Self::Password)
    }
}

struct AccountForm {
    action: AccountAction,
    target: AccountName,
    password: String,
    confirmation: String,
    allow_empty: bool,
    privileges: BTreeSet<String>,
    grant_option: bool,
}

impl AccountForm {
    fn validate(&self, original: Option<&AccountName>) -> Result<(), &'static str> {
        self.target.validate()?;
        if self.action == AccountAction::Rename && original == Some(&self.target) {
            return Err("请修改用户名或账户主机。");
        }
        if self.action.uses_password() {
            if self.password != self.confirmation {
                return Err("两次输入的密码必须一致。");
            }
            if self.password.is_empty() && !self.allow_empty {
                return Err("请输入密码，或明确选择空密码。");
            }
            if self.password.contains('\0') {
                return Err("密码不能包含 NUL 字符。");
            }
        }
        Ok(())
    }

    fn review(self, original: Option<AccountName>) -> Option<Review> {
        let label = self.action.label();
        let target = self.target.label();
        let empty = self.password.is_empty();
        let (mut details, command) = match self.action {
            AccountAction::Read => return None,
            AccountAction::Create => {
                let mut details = vec![target];
                if !self.privileges.is_empty() {
                    details.push(format!(
                        "授予全局权限 {}",
                        self.privileges
                            .iter()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join("、")
                    ));
                }
                if self.grant_option {
                    details.push("允许转授全局静态权限（GRANT OPTION）。".into());
                }
                (
                    details,
                    AccountCommand::Create {
                        account: self.target,
                        password: self.password,
                        privileges: self.privileges,
                        grant_option: self.grant_option,
                    },
                )
            }
            AccountAction::Rename => {
                let account = original?;
                let details = vec![format!("{} → {target}", account.label())];
                (
                    details,
                    AccountCommand::Rename {
                        account,
                        target: self.target,
                    },
                )
            }
            AccountAction::Password => (
                vec![target, "下一次登录使用新密码。".into()],
                AccountCommand::Password {
                    account: self.target,
                    password: self.password,
                },
            ),
        };
        if empty && self.action.uses_password() {
            details.push("将设置空密码。".into());
        }
        Some(Review {
            label,
            details,
            command,
        })
    }
}

fn privilege_checks(
    ui: &mut egui::Ui,
    choices: Vec<&String>,
    names: &mut BTreeSet<String>,
    delegated: Option<&BTreeSet<String>>,
    bulk: bool,
) {
    if bulk {
        ui.horizontal(|ui| {
            if ui.button("全选权限").clicked() {
                names.extend(choices.iter().copied().cloned());
            }
            if ui.button("清空所选权限").clicked() {
                for name in &choices {
                    names.remove(*name);
                }
            }
        });
    }
    ui.horizontal_wrapped(|ui| {
        for &name in &choices {
            let label = if delegated.is_some_and(|names| names.contains(name)) {
                format!("{name}（可转授）")
            } else {
                name.clone()
            };
            let mut checked = names.contains(name);
            if ui.checkbox(&mut checked, label).changed() {
                if checked {
                    names.insert(name.clone());
                } else {
                    names.remove(name);
                }
            }
        }
        for name in names.iter().filter(|name| !choices.contains(name)) {
            ui.add_enabled(false, egui::Checkbox::new(&mut true, name));
        }
    });
}

fn password_field(ui: &mut egui::Ui, label: &str, id: &str, password: &mut String) {
    ui.horizontal(|ui| {
        ui.add_sized([130.0, 24.0], egui::Label::new(label));
        let mut widget = TextEdit::singleline(password)
            .id_salt(id)
            .desired_width(330.0)
            .password(true)
            .show(ui);
        widget.state.clear_undoer();
        TextEdit::store_state(ui.ctx(), widget.response.id, widget.state);
    });
}

struct Review {
    label: &'static str,
    details: Vec<String>,
    command: AccountCommand,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ScopeKind {
    Database,
    Table,
    Column,
    Procedure,
    Function,
}

struct ScopeForm {
    kind: ScopeKind,
    database: String,
    object: String,
    column: String,
}

impl ScopeForm {
    fn from_workspace(ws: &Workspace) -> Self {
        if let Some(object) = &ws.selected {
            let kind = match object.kind {
                ObjectKind::Table | ObjectKind::View => ScopeKind::Table,
                ObjectKind::Procedure => ScopeKind::Procedure,
                ObjectKind::Function => ScopeKind::Function,
                _ => ScopeKind::Database,
            };
            Self {
                kind,
                database: object.schema.clone(),
                object: object.name.clone(),
                column: String::new(),
            }
        } else {
            let database = ws
                .server
                .as_ref()
                .and_then(|server| server.database.clone())
                .unwrap_or_default();
            Self {
                kind: ScopeKind::Database,
                database,
                object: String::new(),
                column: String::new(),
            }
        }
    }

    fn show(&mut self, ui: &mut egui::Ui) -> (Option<GrantScope>, bool) {
        choose(
            ui,
            ("account-scope-kind", "对象类型"),
            &mut self.kind,
            &[
                (ScopeKind::Database, "数据库"),
                (ScopeKind::Table, "表或视图"),
                (ScopeKind::Column, "表列"),
                (ScopeKind::Procedure, "存储过程"),
                (ScopeKind::Function, "存储函数"),
            ],
        );
        field(ui, "数据库", &mut self.database);
        if self.kind != ScopeKind::Database {
            field(ui, "对象名称", &mut self.object);
        }
        if self.kind == ScopeKind::Column {
            field(ui, "列名", &mut self.column);
        }
        let valid = |value: &str| !value.trim().is_empty() && !value.contains('\0');
        let ready = valid(&self.database)
            && (self.kind == ScopeKind::Database || valid(&self.object))
            && (self.kind != ScopeKind::Column || valid(&self.column));
        ui.horizontal(|ui| {
            let pick = ui
                .add_enabled(ready, style::primary("选择范围"))
                .clicked()
                .then(|| self.scope());
            (pick, ui.button("取消").clicked())
        })
        .inner
    }

    fn scope(&self) -> GrantScope {
        let database = self.database.clone();
        match self.kind {
            ScopeKind::Database => GrantScope::Database(database),
            ScopeKind::Table => GrantScope::Table {
                database,
                table: self.object.clone(),
            },
            ScopeKind::Column => GrantScope::Column {
                database,
                table: self.object.clone(),
                column: self.column.clone(),
            },
            ScopeKind::Procedure | ScopeKind::Function => GrantScope::Routine {
                database,
                name: self.object.clone(),
                function: self.kind == ScopeKind::Function,
            },
        }
    }
}
