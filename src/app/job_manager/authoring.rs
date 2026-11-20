use std::path::{Path, PathBuf};

use eframe::egui::{self, ComboBox, DragValue, TextEdit};
use harbor_sql::jobs::backup_editor::{JobConnection, JobHttp, JobSsh, JobTls, Schedule};
use rfd::FileDialog;

use crate::app::{state::Profile, style};
use crate::database::HTTP_CONTENT_TYPES;

use super::schedule;

#[derive(Clone, Copy, PartialEq)]
enum Finish {
    Save,
    Run,
    Schedule,
}

pub(super) enum Action {
    Save,
    Run,
    Schedule(schedule::Trigger),
    Close,
}

pub(super) struct Form {
    pub source: Option<PathBuf>,
    destination: Option<PathBuf>,
    pub error: Option<String>,
    default_file_name: &'static str,
    finish: Finish,
    schedule_fields: schedule::Fields,
}

impl Form {
    pub(super) fn new(
        source: Option<PathBuf>,
        trigger: Option<schedule::Trigger>,
        default_file_name: &'static str,
    ) -> Self {
        Self {
            source,
            destination: None,
            error: None,
            default_file_name,
            finish: Finish::Save,
            schedule_fields: schedule::Fields::for_authoring(trigger),
        }
    }

    pub(super) fn path(&self) -> Option<&Path> {
        self.source.as_deref().or(self.destination.as_deref())
    }

    pub(super) fn directory(&self) -> PathBuf {
        let Some(path) = self.path() else {
            return PathBuf::new();
        };
        let resolved = path.canonicalize().ok();
        resolved
            .as_deref()
            .unwrap_or(path)
            .parent()
            .map_or_else(PathBuf::new, |directory| {
                directory
                    .canonicalize()
                    .unwrap_or_else(|_| directory.to_path_buf())
            })
    }

    pub(super) fn metadata_directory(&self, connection: &JobConnection) -> Result<PathBuf, String> {
        let relative = connection
            .tls
            .iter()
            .flat_map(|tls| [&tls.ca_path, &tls.client_certificate, &tls.client_key])
            .chain(
                connection
                    .ssh
                    .iter()
                    .flat_map(|ssh| [&ssh.identity_file, &ssh.known_hosts_path]),
            )
            .flatten()
            .any(|path| path.is_relative());
        if relative && self.path().is_none() {
            return Err("请先选择作业文件，以确定证书或密钥的相对路径。".into());
        }
        Ok(self.directory())
    }

    pub(super) fn destination(&mut self) -> Option<&Path> {
        if self.path().is_none() {
            self.pick_path();
        }
        self.path()
    }

    fn pick_path(&mut self) {
        let mut dialog = FileDialog::new()
            .set_title("保存作业")
            .set_file_name(self.default_file_name)
            .add_filter("作业 JSON", &["json"]);
        if let Some(directory) = self.path().and_then(Path::parent) {
            dialog = dialog.set_directory(directory);
        }
        if let Some(path) = dialog.save_file() {
            self.destination = Some(if path.extension().is_none() {
                path.with_extension("json")
            } else {
                path
            });
        }
    }

    pub(super) fn controls(
        &mut self,
        ui: &mut egui::Ui,
        schedule: &mut Option<Schedule>,
        log: &mut Option<PathBuf>,
    ) {
        egui::Grid::new("job-log").num_columns(2).show(ui, |ui| {
            path_field(ui, "运行日志", log, "默认写在作业文件旁");
        });
        ui.separator();
        let mut interval = schedule.is_some();
        if ui.checkbox(&mut interval, "设置循环运行间隔").changed() {
            *schedule = interval.then_some(Schedule {
                interval_seconds: 3600,
                run_immediately: true,
            });
        }
        if let Some(schedule) = schedule {
            ui.horizontal_wrapped(|ui| {
                ui.label("每");
                ui.add(DragValue::new(&mut schedule.interval_seconds).range(1..=u64::MAX));
                ui.label("秒运行一次");
                ui.checkbox(&mut schedule.run_immediately, "启动后立即运行首轮");
            });
            ui.label("此设置用于独立作业程序的 watch 命令。");
        }
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            if self.source.is_none() {
                if ui.button("选择作业文件…").clicked() {
                    self.pick_path();
                }
            } else {
                ui.label("作业文件");
            }
            if let Some(path) = self.path() {
                let path = path.display().to_string();
                ui.add(egui::Label::new(&path).truncate())
                    .on_hover_text(&path);
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.radio_value(&mut self.finish, Finish::Save, "仅保存");
            ui.radio_value(&mut self.finish, Finish::Run, "立即运行一次");
            if schedule::SUPPORTED {
                ui.radio_value(&mut self.finish, Finish::Schedule, "设置系统调度");
            }
        });
        if self.finish == Finish::Schedule {
            self.schedule_fields.show(ui);
            ui.label(schedule::ACCOUNT_NOTE);
        }
    }

    pub(super) fn buttons(&self, ui: &mut egui::Ui) -> Option<Action> {
        if let Some(error) = &self.error {
            ui.colored_label(style::ERROR, error);
        }
        ui.horizontal(|ui| {
            let (label, action) = match self.finish {
                Finish::Save => ("保存作业", Some(Action::Save)),
                Finish::Run => ("保存并运行", Some(Action::Run)),
                Finish::Schedule => (
                    "保存并调度",
                    self.schedule_fields.trigger().map(Action::Schedule),
                ),
            };
            if ui
                .add_enabled(action.is_some(), style::primary(label))
                .clicked()
            {
                action
            } else if ui.button("取消").clicked() {
                Some(Action::Close)
            } else {
                None
            }
        })
        .inner
    }
}

pub(super) fn connection_fields(
    ui: &mut egui::Ui,
    connection: &mut JobConnection,
    profiles: &[Profile],
    allow_http: bool,
) -> Option<String> {
    let mut selected_schema = None;
    if profiles
        .iter()
        .any(|profile| allow_http || profile.http.is_none())
    {
        ComboBox::from_id_salt("job-profiles")
            .selected_text("使用已保存连接…")
            .show_ui(ui, |ui| {
                for profile in profiles
                    .iter()
                    .filter(|profile| allow_http || profile.http.is_none())
                {
                    if ui.selectable_label(false, &profile.label).clicked() {
                        *connection = from_profile(profile);
                        selected_schema = Some(profile.schema.clone());
                    }
                }
            });
    }
    egui::Grid::new("job-connection-fields")
        .num_columns(2)
        .spacing([14.0, 6.0])
        .show(ui, |ui| {
            field(ui, "数据库主机", &mut connection.host, "127.0.0.1");
            ui.label("端口");
            ui.add(DragValue::new(&mut connection.port).range(1..=u16::MAX));
            ui.end_row();
            field(ui, "用户名", &mut connection.user, "");
            optional_field(
                ui,
                "数据库密码变量名",
                &mut connection.password_env,
                "如 HARBOR_DB_PASSWORD",
            );
        });
    ui.label("填写环境变量名。使用无密码账户时留空。");
    if allow_http {
        let mut tunneled = connection.http.is_some();
        if ui.checkbox(&mut tunneled, "使用 HTTP 隧道").changed() {
            connection.http = tunneled.then(JobHttp::default);
            if tunneled {
                connection.tls = None;
                connection.ssh = None;
            }
        }
    }
    if let Some(http) = &mut connection.http {
        http_fields(ui, http);
    } else {
        native_fields(ui, connection);
    }
    selected_schema
}

pub(super) fn from_profile(profile: &Profile) -> JobConnection {
    let mut connection = JobConnection::default();
    connection.host = profile.host.trim().into();
    connection.port = profile.port;
    connection.user = profile.user.trim().into();
    connection.http = profile.http.as_ref().map(|http| {
        let url = http.url.trim();
        let proxy = http.proxy_url.trim();
        let proxy_url = JobHttp::can_store_url(proxy).then(|| proxy.to_owned());
        let portable = JobHttp::can_store_url(url) && (proxy.is_empty() || proxy_url.is_some());
        JobHttp {
            url: if portable { url.into() } else { String::new() },
            content_type: Some(http.content_type),
            timeout: http.timeout,
            force_base64: http.force_base64,
            utf8mb4: http.utf8mb4,
            proxy_url,
            ..Default::default()
        }
    });
    let path = |value: &str| {
        let value = value.trim();
        (!value.is_empty())
            .then(|| std::path::absolute(value).unwrap_or_else(|_| PathBuf::from(value)))
    };
    connection.tls = (profile.tls && profile.http.is_none()).then(|| JobTls {
        ca_path: path(&profile.ca),
        client_certificate: path(&profile.client_identity),
        client_key: path(&profile.client_private_key),
    });
    connection.ssh = (profile.ssh && profile.http.is_none()).then(|| JobSsh {
        host: profile.ssh_host.trim().into(),
        port: profile.ssh_port,
        user: profile.ssh_user.trim().into(),
        identity_file: if profile.ssh_password_auth {
            None
        } else {
            path(&profile.ssh_key)
        },
        known_hosts_path: path(&profile.ssh_known_hosts),
        local_port: profile.ssh_local_port,
        ..Default::default()
    });
    connection
}

fn native_fields(ui: &mut egui::Ui, connection: &mut JobConnection) {
    let mut tls = connection.tls.is_some();
    if ui.checkbox(&mut tls, "使用 TLS").changed() {
        connection.tls = tls.then(JobTls::default);
    }
    if let Some(tls) = &mut connection.tls {
        egui::Grid::new("job-tls").num_columns(2).show(ui, |ui| {
            path_field(ui, "CA 证书", &mut tls.ca_path, "可选路径");
            path_field(ui, "客户端证书", &mut tls.client_certificate, "可选路径");
            path_field(ui, "客户端私钥", &mut tls.client_key, "可选路径");
        });
    }
    let mut ssh = connection.ssh.is_some();
    if ui.checkbox(&mut ssh, "使用 SSH 隧道").changed() {
        connection.ssh = ssh.then(JobSsh::default);
    }
    if let Some(ssh) = &mut connection.ssh {
        egui::Grid::new("job-ssh").num_columns(2).show(ui, |ui| {
            field(ui, "SSH 主机", &mut ssh.host, "");
            ui.label("SSH 端口");
            ui.add(DragValue::new(&mut ssh.port).range(1..=u16::MAX));
            ui.end_row();
            field(ui, "SSH 用户名", &mut ssh.user, "");
            path_field(ui, "SSH 私钥", &mut ssh.identity_file, "私钥或 SSH 密码");
            optional_field(ui, "SSH 密码变量名", &mut ssh.password_env, "");
            optional_field(
                ui,
                "SSH 私钥口令变量名",
                &mut ssh.key_passphrase_env,
                "加密私钥时填写",
            );
            path_field(ui, "已知主机文件", &mut ssh.known_hosts_path, "可选路径");
            ui.label("SSH 本地端口");
            let mut port = ssh.local_port.unwrap_or_default();
            if ui
                .add(DragValue::new(&mut port))
                .on_hover_text("0 表示自动分配")
                .changed()
            {
                ssh.local_port = (port != 0).then_some(port);
            }
            ui.end_row();
        });
    }
}

fn http_fields(ui: &mut egui::Ui, http: &mut JobHttp) {
    egui::Grid::new("job-http").num_columns(2).show(ui, |ui| {
        field(
            ui,
            "隧道地址",
            &mut http.url,
            "https://主机/SQLyogTunnel.php",
        );
        ui.label("内容类型");
        let mut content_type = http.content_type.unwrap_or_default();
        ComboBox::from_id_salt("job-http-content-type")
            .selected_text(
                HTTP_CONTENT_TYPES
                    .get(content_type)
                    .copied()
                    .unwrap_or_default(),
            )
            .show_ui(ui, |ui| {
                for (index, value) in HTTP_CONTENT_TYPES.iter().enumerate() {
                    if ui
                        .selectable_value(&mut content_type, index, *value)
                        .changed()
                    {
                        http.content_type = Some(content_type);
                    }
                }
            });
        ui.end_row();
        ui.label("请求超时（秒）");
        let mut timeout = http.timeout.unwrap_or(120);
        if ui
            .add(DragValue::new(&mut timeout).range(1..=3600))
            .changed()
        {
            http.timeout = Some(timeout);
        }
        ui.end_row();
    });
    ui.horizontal_wrapped(|ui| {
        ui.checkbox(&mut http.force_base64, "始终使用 Base64 编码");
        ui.checkbox(&mut http.utf8mb4, "使用 utf8mb4");
    });
    ui.collapsing("网页认证和代理", |ui| {
        egui::Grid::new("job-http-auth")
            .num_columns(2)
            .show(ui, |ui| {
                optional_field(ui, "网页用户名变量名", &mut http.web_user_env, "");
                optional_field(ui, "网页密码变量名", &mut http.web_password_env, "");
                optional_field(ui, "代理地址", &mut http.proxy_url, "");
                optional_field(ui, "代理用户名变量名", &mut http.proxy_user_env, "");
                optional_field(ui, "代理密码变量名", &mut http.proxy_password_env, "");
            });
    });
    ui.label("数据库主机和端口需从隧道服务器访问。");
    ui.label("地址不含用户名、密码、查询参数或锚点。");
}

fn field(ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str) -> bool {
    ui.label(label);
    let changed = ui
        .add(
            TextEdit::singleline(value)
                .desired_width(325.0)
                .hint_text(hint),
        )
        .changed();
    ui.end_row();
    changed
}

pub(super) fn optional_field(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut Option<String>,
    hint: &str,
) {
    let mut text = value.clone().unwrap_or_default();
    if field(ui, label, &mut text, hint) {
        *value = (!text.trim().is_empty()).then(|| text.trim().into());
    }
}

pub(super) fn path_field(ui: &mut egui::Ui, label: &str, value: &mut Option<PathBuf>, hint: &str) {
    let mut text = value
        .as_ref()
        .map_or(String::new(), |path| path.display().to_string());
    if field(ui, label, &mut text, hint) {
        *value = (!text.is_empty()).then(|| PathBuf::from(text));
    }
}
