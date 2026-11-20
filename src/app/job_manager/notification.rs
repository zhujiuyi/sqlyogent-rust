use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use eframe::egui::{self, ComboBox, DragValue, TextEdit};
use harbor_sql::jobs::notification_editor::{
    Definition, MAX_REPORT_ROWS, Notification, Security, SubjectDate, Trigger,
};
use rfd::FileDialog;

use crate::app::{
    state::{Profile, Workspace},
    style,
};

use super::authoring::{self, Action, Form, optional_field, path_field};

const DEFAULT_REPORT: &str = "sql-report-{unix_ms}.html";

#[derive(Clone, Copy, Hash, PartialEq)]
enum Page {
    Connection,
    Sql,
    Mail,
    Run,
}

#[derive(Clone, Copy, PartialEq)]
enum Source {
    Text,
    Script,
}

#[derive(Clone, Copy, PartialEq)]
enum Authentication {
    None,
    Environment,
    ExistingUsername,
}

#[derive(Clone, Copy, PartialEq)]
enum Subject {
    Automatic,
    Fixed,
    Date(SubjectDate),
}

pub(super) struct Editor {
    pub form: Form,
    pub definition: Definition,
    page: Page,
    sql_source: Source,
    sql_text_draft: String,
    script_draft: Option<PathBuf>,
    report_draft: Option<String>,
    mail_draft: Option<Notification>,
    pending: Option<(&'static str, Receiver<Result<String, String>>)>,
    status: Option<Result<String, String>>,
}

impl Editor {
    pub(super) fn new(workspace: Option<&Workspace>) -> Self {
        let workspace = workspace.filter(|workspace| {
            workspace
                .profile
                .as_ref()
                .is_none_or(|profile| profile.http.is_none())
        });
        let profile = workspace.and_then(|workspace| workspace.profile.as_ref());
        let schema = workspace
            .and_then(|workspace| workspace.server.as_ref())
            .and_then(|server| server.database.clone())
            .or_else(|| profile.map(|profile| profile.schema.clone()))
            .filter(|schema| !schema.is_empty());
        let mut definition = Definition::new(schema);
        definition.sql.report = Some(DEFAULT_REPORT.into());
        if let Some(profile) = profile {
            definition.connection = authoring::from_profile(profile);
        }
        Self::from_definition(definition, None, None)
    }

    pub(super) fn from_definition(
        definition: Definition,
        source: Option<PathBuf>,
        trigger: Option<super::schedule::Trigger>,
    ) -> Self {
        let sql_source = if definition.sql.script.is_some() {
            Source::Script
        } else {
            Source::Text
        };
        Self {
            form: Form::new(source, trigger, "sql-job.json"),
            definition,
            page: Page::Connection,
            sql_source,
            sql_text_draft: String::new(),
            script_draft: None,
            report_draft: None,
            mail_draft: None,
            pending: None,
            status: None,
        }
    }

    pub(super) fn dialog(&mut self, ctx: &egui::Context, profiles: &[Profile]) -> Option<Action> {
        self.poll();
        let bounds = ctx.content_rect();
        let response =
            crate::app::style::modal(ctx, egui::Id::new("sql-job-editor")).show(ctx, |ui| {
                ui.set_width((bounds.width() - 48.0).clamp(400.0, 700.0));
                ui.heading(if self.form.source.is_some() {
                    "编辑 SQL 作业"
                } else {
                    "新建 SQL 作业"
                });
                ui.horizontal_wrapped(|ui| {
                    for (page, label) in [
                        (Page::Connection, "连接"),
                        (Page::Sql, "SQL 与报告"),
                        (Page::Mail, "邮件通知"),
                        (Page::Run, "保存与运行"),
                    ] {
                        ui.selectable_value(&mut self.page, page, label);
                    }
                });
                ui.separator();
                let idle = self.pending.is_none();
                egui::ScrollArea::vertical()
                    .id_salt(("sql-job-edit", self.page))
                    .max_height((bounds.height() - 210.0).max(210.0))
                    .show(ui, |ui| {
                        ui.add_enabled_ui(idle, |ui| match self.page {
                            Page::Connection => self.connection_form(ui, profiles),
                            Page::Sql => self.sql_form(ui),
                            Page::Mail => self.mail_form(ui),
                            Page::Run => self.form.controls(
                                ui,
                                &mut self.definition.schedule,
                                &mut self.definition.log,
                            ),
                        });
                    });
                ui.separator();
                if let Some((message, _)) = &self.pending {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(*message);
                    });
                }
                match &self.status {
                    Some(Ok(message)) => {
                        ui.label(message);
                    }
                    Some(Err(error)) => {
                        ui.colored_label(style::ERROR, error);
                    }
                    None => {}
                }
                ui.add_enabled_ui(idle, |ui| self.form.buttons(ui)).inner
            });
        if response.should_close() {
            Some(Action::Close)
        } else {
            response.inner
        }
    }

    fn connection_form(&mut self, ui: &mut egui::Ui, profiles: &[Profile]) {
        let connection = &mut self.definition.connection;
        if let Some(schema) = authoring::connection_fields(ui, connection, profiles, false)
            && self.definition.sql.schema.is_none()
            && !schema.is_empty()
        {
            self.definition.sql.schema = Some(schema);
        }
        egui::Grid::new("sql-job-default-database")
            .num_columns(2)
            .show(ui, |ui| {
                optional_field(
                    ui,
                    "默认数据库",
                    &mut self.definition.sql.schema,
                    "可留空或在 SQL 中使用 USE",
                );
            });
    }

    fn sql_form(&mut self, ui: &mut egui::Ui) {
        let sql = &mut self.definition.sql;
        let mut report = sql.report.is_some();
        ui.horizontal_wrapped(|ui| {
            ui.radio_value(&mut report, true, "查询报告");
            ui.radio_value(&mut report, false, "维护 SQL");
        });
        if report && sql.report.is_none() {
            sql.report = Some(
                self.report_draft
                    .take()
                    .unwrap_or_else(|| DEFAULT_REPORT.into()),
            );
        } else if !report && sql.report.is_some() {
            self.report_draft = sql.report.take();
            if let Some(mail) = &mut self.definition.notification {
                mail.include_report = false;
            }
        }

        let mut source = self.sql_source;
        ui.horizontal_wrapped(|ui| {
            ui.radio_value(&mut source, Source::Text, "输入 SQL");
            ui.radio_value(&mut source, Source::Script, "外部 SQL 脚本");
        });
        if source != self.sql_source {
            self.sql_source = source;
            match source {
                Source::Text => {
                    self.script_draft = sql.script.take();
                    sql.sql = Some(std::mem::take(&mut self.sql_text_draft));
                }
                Source::Script => {
                    self.sql_text_draft = sql.sql.take().unwrap_or_default();
                    sql.script = self.script_draft.take();
                }
            }
        }
        match source {
            Source::Text => {
                ui.add(
                    TextEdit::multiline(sql.sql.get_or_insert_with(String::new))
                        .code_editor()
                        .desired_rows(7)
                        .desired_width(f32::INFINITY)
                        .hint_text("SELECT …"),
                );
            }
            Source::Script => {
                ui.horizontal_wrapped(|ui| {
                    egui::Grid::new("sql-script-path")
                        .num_columns(2)
                        .show(ui, |ui| {
                            path_field(ui, "脚本路径", &mut sql.script, "相对路径以作业目录为准");
                        });
                    if ui.button("浏览…").clicked()
                        && let Some(path) = FileDialog::new()
                            .add_filter("SQL 脚本", &["sql"])
                            .set_directory(self.form.directory())
                            .pick_file()
                    {
                        sql.script = Some(path);
                    }
                });
            }
        }
        let check = ui
            .button("检查语句拆分")
            .on_hover_text("使用默认 MySQL 引号规则和脚本中的 DELIMITER，数据库不会执行这些语句。")
            .clicked();
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            ui.label("语句失败时");
            ui.radio_value(&mut sql.continue_on_error, false, "立即停止");
            ui.radio_value(&mut sql.continue_on_error, true, "继续下一条");
        });

        if let Some(path) = &mut sql.report {
            ui.separator();
            ui.horizontal_wrapped(|ui| {
                ui.label("HTML 报告");
                ui.add(
                    TextEdit::singleline(path)
                        .desired_width(280.0)
                        .hint_text(DEFAULT_REPORT),
                );
                if ui.button("浏览…").clicked()
                    && let Some(selected) = FileDialog::new()
                        .add_filter("HTML 报告", &["html", "htm"])
                        .set_file_name(DEFAULT_REPORT)
                        .set_directory(self.form.directory())
                        .save_file()
                {
                    let selected = if selected.extension().is_some() {
                        selected
                    } else {
                        selected.with_extension("html")
                    };
                    if let Some(selected) = selected.to_str() {
                        *path = selected.into();
                    } else {
                        self.form.error = Some("HTML 报告路径应为 Unicode 路径。".into());
                    }
                }
            });
            ui.small("相对路径以作业目录为准。文件名中的 {unix_ms} 会替换为运行时间。");
            ui.horizontal_wrapped(|ui| {
                ui.label("每个结果集最多保留");
                ui.add(DragValue::new(&mut sql.result_row_limit).range(1..=MAX_REPORT_ROWS));
                ui.label("行");
            });
            ui.checkbox(&mut sql.overwrite, "覆盖同名报告");
            ui.checkbox(&mut sql.include_sql, "报告包含 SQL 原文");
            ui.checkbox(&mut sql.include_messages, "报告包含执行消息");
        }
        if check {
            self.check_sql(ui.ctx());
        }
    }

    fn check_sql(&mut self, ctx: &egui::Context) {
        if self
            .definition
            .sql
            .script
            .as_ref()
            .is_some_and(|path| path.is_relative())
            && self.form.destination().is_none()
        {
            return;
        }
        let sql = self.definition.sql.clone();
        let directory = self.form.directory();
        self.start_check(ctx, "正在检查 SQL…", move || {
            let statements = sql.check_statements(&directory)?;
            Ok(format!("按默认 MySQL 规则拆分出 {statements} 条 SQL。"))
        });
    }

    fn mail_form(&mut self, ui: &mut egui::Ui) {
        let has_report = self.definition.sql.report.is_some();
        let mut enabled = self.definition.notification.is_some();
        if ui.checkbox(&mut enabled, "启用 SMTP 邮件通知").changed() {
            if enabled {
                let mut mail = self.mail_draft.take().unwrap_or_else(|| Notification {
                    to: vec![String::new()],
                    when: if has_report {
                        Trigger::Always
                    } else {
                        Trigger::Failure
                    },
                    include_report: has_report,
                    ..Default::default()
                });
                if !has_report {
                    mail.include_report = false;
                }
                self.definition.notification = Some(mail);
            } else {
                self.mail_draft = self.definition.notification.take();
            }
        }
        let Some(mail) = &mut self.definition.notification else {
            return;
        };

        trigger_fields(ui, &mut mail.when);
        if has_report {
            ui.checkbox(&mut mail.include_report, "成功时附加 HTML 报告");
        }
        message_fields(ui, mail);
        if ui
            .button("发送测试邮件")
            .on_hover_text("向所填地址发送一封测试邮件。")
            .clicked()
        {
            let mail = mail.clone();
            self.start_check(ui.ctx(), "正在发送测试邮件…", move || {
                mail.send_test()?;
                Ok("SMTP 服务器已接受测试邮件。".into())
            });
        }
    }

    fn start_check(
        &mut self,
        ctx: &egui::Context,
        pending: &'static str,
        task: impl FnOnce() -> Result<String, String> + Send + 'static,
    ) {
        let (sender, receiver) = mpsc::channel();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = sender.send(task());
            ctx.request_repaint();
        });
        self.status = None;
        self.pending = Some((pending, receiver));
    }

    fn poll(&mut self) {
        let Some((_, receiver)) = &self.pending else {
            return;
        };
        self.status = Some(match receiver.try_recv() {
            Ok(status) => status,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("检查任务未返回结果。".into()),
        });
        self.pending = None;
    }
}

#[derive(Default)]
pub(super) struct ReportMailForm {
    draft: Option<Notification>,
    test: Option<Receiver<Result<(), String>>>,
    test_result: Option<Result<(), String>>,
}

impl ReportMailForm {
    pub(super) fn show(
        &mut self,
        ui: &mut egui::Ui,
        notification: &mut Option<Notification>,
        toggle_label: &str,
    ) {
        if let Some(test) = &self.test {
            let result = match test.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err("邮件测试未返回结果。".into())),
            };
            if let Some(result) = result {
                self.test = None;
                self.test_result = Some(result);
            }
        }
        ui.add_enabled_ui(self.test.is_none(), |ui| {
            let mut enabled = notification.is_some();
            if ui.checkbox(&mut enabled, toggle_label).changed() {
                self.test_result = None;
                if enabled {
                    *notification = Some(self.draft.take().unwrap_or_else(|| Notification {
                        to: vec![String::new()],
                        when: Trigger::Failure,
                        ..Notification::default()
                    }));
                } else {
                    self.draft = notification.take();
                }
            }
            let Some(mail) = notification.as_mut() else {
                return;
            };
            trigger_fields(ui, &mut mail.when);
            message_fields(ui, mail);
            if ui
                .button("发送测试邮件")
                .on_hover_text("向所填地址发送一封测试邮件。")
                .clicked()
            {
                let mail = mail.clone();
                let ctx = ui.ctx().clone();
                let (sender, receiver) = mpsc::channel();
                match thread::Builder::new()
                    .name("report-mail-test".into())
                    .spawn(move || {
                        let _ = sender.send(mail.send_test());
                        ctx.request_repaint();
                    }) {
                    Ok(_) => {
                        self.test = Some(receiver);
                        self.test_result = None;
                    }
                    Err(error) => {
                        self.test_result = Some(Err(format!("无法启动邮件测试。{error}")))
                    }
                }
            }
        });
        if self.test.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("正在发送测试邮件…");
            });
        } else if let Some(result) = &self.test_result {
            match result {
                Ok(()) => ui.label("SMTP 服务器已接受上次测试邮件。"),
                Err(error) => ui.colored_label(style::ERROR, error),
            };
        }
    }
}

fn trigger_fields(ui: &mut egui::Ui, when: &mut Trigger) {
    ui.horizontal_wrapped(|ui| {
        ui.label("发送时机");
        ui.radio_value(when, Trigger::Always, "每次完成后");
        ui.radio_value(when, Trigger::Success, "仅成功");
        ui.radio_value(when, Trigger::Failure, "仅失败");
    });
}

fn message_fields(ui: &mut egui::Ui, mail: &mut Notification) {
    ui.separator();
    smtp_fields(ui, mail);
    ui.separator();
    egui::Grid::new("sql-mail-sender")
        .num_columns(2)
        .spacing([14.0, 6.0])
        .show(ui, |ui| {
            ui.label("发件人");
            ui.add(
                TextEdit::singleline(&mut mail.from)
                    .hint_text("显示名 <sender@example.com>")
                    .desired_width(300.0),
            );
            ui.end_row();
            optional_field(ui, "回复地址", &mut mail.reply_to, "可选");
        });
    addresses(ui, "收件人", &mut mail.to);
    addresses(ui, "抄送", &mut mail.cc);
    addresses(ui, "密送", &mut mail.bcc);
    subject_field(ui, mail);
}

fn smtp_fields(ui: &mut egui::Ui, mail: &mut Notification) {
    egui::Grid::new("sql-mail-smtp")
        .num_columns(2)
        .spacing([14.0, 6.0])
        .show(ui, |ui| {
            ui.label("SMTP 主机");
            ui.add(
                TextEdit::singleline(&mut mail.smtp_host)
                    .hint_text("smtp.example.com")
                    .desired_width(300.0),
            );
            ui.end_row();

            ui.label("传输加密");
            ComboBox::from_id_salt("sql-smtp-security")
                .selected_text(security_label(mail.security))
                .show_ui(ui, |ui| {
                    for security in [Security::Tls, Security::StartTls, Security::LocalRelay] {
                        ui.selectable_value(&mut mail.security, security, security_label(security));
                    }
                });
            ui.end_row();

            ui.label("端口");
            ui.horizontal(|ui| {
                let mut custom = mail.port.is_some();
                let default = mail.security.default_port();
                if ui.checkbox(&mut custom, "自定义").changed() {
                    mail.port = custom.then_some(default);
                }
                if let Some(port) = &mut mail.port {
                    ui.add(DragValue::new(port).range(1..=u16::MAX));
                } else {
                    ui.label(default.to_string());
                }
            });
            ui.end_row();

            let current = if mail.username.is_some() && mail.username_env.is_none() {
                Authentication::ExistingUsername
            } else if mail.username_env.is_some() || mail.password_env.is_some() {
                Authentication::Environment
            } else {
                Authentication::None
            };
            let mut authentication = current;
            ui.label("SMTP 认证");
            ui.horizontal_wrapped(|ui| {
                if current == Authentication::ExistingUsername {
                    ui.radio_value(
                        &mut authentication,
                        Authentication::ExistingUsername,
                        "现有用户名",
                    );
                }
                ui.radio_value(&mut authentication, Authentication::None, "无需认证");
                ui.radio_value(&mut authentication, Authentication::Environment, "环境变量");
            });
            ui.end_row();
            if authentication != current {
                mail.username = None;
                mail.username_env =
                    (authentication == Authentication::Environment).then(String::new);
                if authentication == Authentication::None {
                    mail.password_env = None;
                }
            }
            if authentication == Authentication::Environment {
                ui.label("用户名变量名");
                ui.add(
                    TextEdit::singleline(mail.username_env.get_or_insert_with(String::new))
                        .desired_width(300.0)
                        .hint_text("HARBOR_SMTP_USER"),
                );
                ui.end_row();
            }
            if authentication != Authentication::None {
                ui.label("密码变量名");
                ui.add(
                    TextEdit::singleline(mail.password_env.get_or_insert_with(String::new))
                        .desired_width(300.0)
                        .hint_text("HARBOR_SMTP_PASSWORD"),
                );
                ui.end_row();
            }
        });
    if mail.security == Security::LocalRelay {
        ui.small("本机中继使用回环 IP 地址（如 127.0.0.1），无需认证。");
    }
}

fn security_label(security: Security) -> &'static str {
    match security {
        Security::Tls => "直接 TLS",
        Security::StartTls => "STARTTLS",
        Security::LocalRelay => "本机中继",
    }
}

fn addresses(ui: &mut egui::Ui, label: &str, addresses: &mut Vec<String>) {
    ui.push_id(label, |ui| {
        ui.horizontal(|ui| {
            ui.label(label);
            if ui.small_button("添加").clicked() {
                addresses.push(String::new());
            }
        });
        let mut remove = None;
        for (index, address) in addresses.iter_mut().enumerate() {
            ui.push_id(index, |ui| {
                ui.horizontal(|ui| {
                    ui.add(
                        TextEdit::singleline(address)
                            .desired_width(300.0)
                            .hint_text("name@example.com"),
                    );
                    if ui.small_button("移除").clicked() {
                        remove = Some(index);
                    }
                });
            });
        }
        if let Some(index) = remove {
            addresses.remove(index);
        }
    });
}

fn subject_field(ui: &mut egui::Ui, mail: &mut Notification) {
    let current = mail.subject_date.map_or_else(
        || {
            if mail.subject.is_some() {
                Subject::Fixed
            } else {
                Subject::Automatic
            }
        },
        Subject::Date,
    );
    let mut subject = current;
    ui.horizontal_wrapped(|ui| {
        ui.label("邮件主题");
        for (choice, label) in [
            (Subject::Automatic, "按执行结果"),
            (Subject::Fixed, "固定文本"),
            (Subject::Date(SubjectDate::Date), "YYYY-MM-DD"),
            (Subject::Date(SubjectDate::DateTime), "YYYY-MM-DD HH:MM:SS"),
        ] {
            ui.radio_value(&mut subject, choice, label);
        }
    });
    if subject != current {
        mail.subject = (subject == Subject::Fixed).then(String::new);
        mail.subject_date = match subject {
            Subject::Date(date) => Some(date),
            _ => None,
        };
    }
    if let Some(text) = &mut mail.subject {
        ui.add(
            TextEdit::singleline(text)
                .desired_width(300.0)
                .hint_text("自定义邮件主题"),
        );
    }
}
