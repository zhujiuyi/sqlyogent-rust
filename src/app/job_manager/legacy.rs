use std::path::{Path, PathBuf};

use eframe::egui::{self, ComboBox, DragValue, TextEdit};
use harbor_sql::database::OdbcReset;
use harbor_sql::file_source::FileSourceFormat;
use harbor_sql::jobs::legacy_import::{
    Address, Endpoint, Kind, OdbcScope, OdbcSource, Prepared, ReportSettings, Request, Source,
    Transport,
};
use rfd::FileDialog;

use crate::app::style;

use super::authoring::optional_field;

pub(super) enum Action {
    Save,
    Close,
}

pub(super) struct Importer {
    source: Source,
    request: Request,
    report_settings: ReportSettings,
    destination: Option<PathBuf>,
    prepared: Option<Prepared>,
    pub error: Option<String>,
}

impl Importer {
    pub(super) fn pick() -> Result<Option<Self>, String> {
        let Some(path) = FileDialog::new()
            .set_title("打开旧版 SQLyog 作业")
            .add_filter("SQLyog 作业 XML", &["xml"])
            .pick_file()
        else {
            return Ok(None);
        };
        let source = Source::open(&path)?;
        let request = match source.kind() {
            Kind::Backup { .. } => Request::Backup {
                output: String::new(),
                password_env: Some(String::new()),
                copy_replication_lock: false,
            },
            Kind::DataSync { source, target, .. } => Request::DataSync {
                source: endpoint(source),
                target: endpoint(target),
                use_native_rules: false,
            },
            Kind::Notification => Request::Notification {
                report: String::new(),
                password_env: String::new(),
                smtp_user_env: String::new(),
                smtp_password_env: String::new(),
                smtp_tls: false,
                smtp_port: 587,
            },
            Kind::Odbc {
                requires_connection_string,
                scope,
                source_file_format,
            } => Request::Odbc {
                create: true,
                reset: OdbcReset::None,
                use_legacy_table_strategies: false,
                target_password_env: Some(String::new()),
                source: if let Some(source) =
                    direct_file_source(source_file_format, source.mapped_file_path())
                {
                    source
                } else if requires_connection_string {
                    OdbcSource::ConnectionString(String::new())
                } else {
                    OdbcSource::Dsn {
                        user_env: None,
                        password_env: None,
                    }
                },
                source_catalog: None,
                source_schema: None,
                use_legacy_conflict: scope == OdbcScope::MappedTables,
                copy_indexes: false,
                copy_foreign_keys: false,
                preserve_source_actions: false,
            },
        };
        Ok(Some(Self {
            source,
            request,
            report_settings: ReportSettings::default(),
            destination: None,
            prepared: None,
            error: None,
        }))
    }

    pub(super) fn prepared(&self) -> Result<&Prepared, String> {
        self.prepared
            .as_ref()
            .ok_or_else(|| "请先核对迁移预览。".into())
    }

    pub(super) fn dialog(&mut self, ctx: &egui::Context) -> Option<Action> {
        let bounds = ctx.content_rect();
        let response =
            crate::app::style::modal(ctx, egui::Id::new("legacy-job-import")).show(ctx, |ui| {
                ui.set_width((bounds.width() - 48.0).clamp(380.0, 690.0));
                style::page_title(ui, "导入 SQLyog 作业");
                ui.horizontal_wrapped(|ui| {
                    ui.label(self.source.kind().label());
                    path_label(ui, self.source.path());
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .max_height((bounds.height() - 231.0).max(170.0))
                    .show(ui, |ui| {
                        if let Some(prepared) = &self.prepared {
                            review(ui, prepared);
                        } else {
                            self.settings(ui);
                        }
                    });
                if let Some(error) = &self.error {
                    ui.colored_label(style::error(), error);
                }
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    if self.prepared.is_some() {
                        if ui.add(style::primary("保存并登记")).clicked() {
                            return Some(Action::Save);
                        }
                        if ui.button("返回设置").clicked() {
                            self.prepared = None;
                            self.error = None;
                        }
                    } else {
                        let supported = !matches!(
                            self.source.kind(),
                            Kind::Odbc {
                                scope: OdbcScope::MissingMappings,
                                ..
                            }
                        );
                        if ui
                            .add_enabled(supported, style::primary("预览迁移"))
                            .clicked()
                        {
                            self.preview();
                        }
                    }
                    ui.button("取消").clicked().then_some(Action::Close)
                })
                .inner
            });
        if response.should_close() {
            Some(Action::Close)
        } else {
            response.inner
        }
    }

    fn settings(&mut self, ui: &mut egui::Ui) {
        match (self.source.kind(), &mut self.request) {
            (
                Kind::Backup { directory },
                Request::Backup {
                    output,
                    password_env,
                    copy_replication_lock,
                },
            ) => {
                output_path(
                    ui,
                    "备份输出",
                    output,
                    if directory {
                        Output::Directory
                    } else {
                        Output::Sql
                    },
                );
                password(ui, "数据库密码变量名", password_env);
                if self.source.uses_replication_flags() {
                    ui.checkbox(copy_replication_lock, "沿用旧作业的全局读锁")
                        .on_hover_text(
                            "将保留已确认的读锁步骤。备份读取期间，源服务器的写入会等待锁释放。",
                        );
                }
            }
            (
                Kind::DataSync {
                    source,
                    target,
                    continues_on_error,
                },
                Request::DataSync {
                    source: from,
                    target: to,
                    use_native_rules,
                },
            ) => {
                ui.push_id("legacy-source", |ui| sync_fields(ui, "源端", source, from));
                ui.push_id("legacy-target", |ui| sync_fields(ui, "目标端", target, to));
                if continues_on_error {
                    ui.checkbox(use_native_rules, "迁移后在出错时停止同步");
                }
            }
            (
                Kind::Notification,
                Request::Notification {
                    report,
                    password_env,
                    smtp_user_env,
                    smtp_password_env,
                    smtp_tls,
                    smtp_port,
                },
            ) => {
                output_path(ui, "HTML 报告", report, Output::Html);
                egui::Grid::new("legacy-mail-credentials")
                    .num_columns(2)
                    .show(ui, |ui| {
                        field(ui, "数据库密码变量名", password_env);
                        if self.source.requires_smtp_auth() {
                            smtp_credentials(ui, smtp_user_env, smtp_password_env);
                        }
                    });
                smtp_transport(ui, smtp_tls, smtp_port);
            }
            (
                Kind::Odbc {
                    scope: OdbcScope::MissingMappings,
                    ..
                },
                _,
            ) => {
                ui.label("旧作业未列出表和字段的对应关系。请在 ODBC 导入作业中填写表及列映射。");
                return;
            }
            (
                Kind::Odbc {
                    requires_connection_string,
                    scope,
                    source_file_format,
                },
                Request::Odbc {
                    create,
                    reset,
                    use_legacy_table_strategies,
                    target_password_env,
                    source,
                    source_catalog,
                    source_schema,
                    use_legacy_conflict,
                    copy_indexes,
                    copy_foreign_keys,
                    preserve_source_actions,
                },
            ) => {
                let all_tables = scope == OdbcScope::AllTables;
                if !all_tables {
                    ui.checkbox(use_legacy_table_strategies, "沿用原作业的逐表写入策略");
                }
                let mut mode = (*create, *reset);
                ui.add_enabled_ui(!*use_legacy_table_strategies, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label("写入方式");
                        ui.radio_value(&mut mode, (true, OdbcReset::None), "创建新表");
                        ui.radio_value(&mut mode, (false, OdbcReset::None), "追加到现有表");
                        ui.radio_value(&mut mode, (false, OdbcReset::Truncate), "清空后导入");
                        ui.radio_value(&mut mode, (true, OdbcReset::Recreate), "重建目标表");
                    })
                });
                (*create, *reset) = mode;
                if !*use_legacy_table_strategies && *reset == OdbcReset::Truncate {
                    ui.colored_label(style::error(), "运行新作业时，目标表的现有数据将被清空。");
                }
                if !*use_legacy_table_strategies && *reset == OdbcReset::Recreate {
                    ui.colored_label(style::error(), "运行新作业时，原目标表的结构和数据将被替换。");
                }
                password(ui, "目标库密码变量名", target_password_env);
                odbc_source(
                    ui,
                    requires_connection_string,
                    source_file_format,
                    self.source.mapped_file_path(),
                    source,
                );
                let file_source = matches!(
                    source,
                    OdbcSource::CsvPath(_)
                        | OdbcSource::ExcelPath(_)
                        | OdbcSource::AccessPath { .. }
                );
                let supports_metadata =
                    !matches!(source, OdbcSource::CsvPath(_) | OdbcSource::ExcelPath(_));
                if all_tables {
                    ui.label("运行时发现源表，并按表名和列名建立映射。");
                    if !file_source {
                        egui::Grid::new("legacy-odbc-discovery")
                            .num_columns(2)
                            .show(ui, |ui| {
                                optional_field(ui, "源目录（Catalog）", source_catalog, "可选");
                                optional_field(ui, "源架构（Schema）", source_schema, "可选");
                            });
                    }
                } else {
                    ui.checkbox(use_legacy_conflict, "沿用原作业的重复键策略");
                }
                let index_label = if all_tables {
                    "复制源索引"
                } else {
                    "复制原作业已选的源索引"
                };
                let foreign_key_label = if all_tables {
                    "复制源外键"
                } else {
                    "复制原作业已选的源外键"
                };
                let allows_creation = *create || *use_legacy_table_strategies;
                ui.add_enabled(
                    supports_metadata && allows_creation && !*copy_foreign_keys,
                    egui::Checkbox::new(copy_indexes, index_label),
                );
                if ui
                    .add_enabled(
                        supports_metadata && allows_creation,
                        egui::Checkbox::new(copy_foreign_keys, foreign_key_label),
                    )
                    .changed()
                    && *copy_foreign_keys
                {
                    *copy_indexes = true;
                }
                if !all_tables && !file_source {
                    ui.checkbox(preserve_source_actions, "保留原作业的源表更新或删除");
                    if *preserve_source_actions {
                        ui.colored_label(
                            style::error(),
                            "运行新作业时，目标写入确认后会更改源记录。",
                        );
                    }
                }
            }
            _ => unreachable!("migration request must correspond to the source kind"),
        }
        if let Some(report) = self.source.report_info() {
            ui.separator();
            ui.label("旧作业已启用邮件报告");
            if report.needs_auth {
                egui::Grid::new("legacy-report-credentials")
                    .num_columns(2)
                    .show(ui, |ui| {
                        smtp_credentials(
                            ui,
                            &mut self.report_settings.smtp_user_env,
                            &mut self.report_settings.smtp_password_env,
                        );
                    });
            }
            if report.needs_transport {
                ui.label("旧 SMTP 传输设置无法沿用，请填写已配置的加密端口。");
                smtp_transport(
                    ui,
                    &mut self.report_settings.smtp_tls,
                    &mut self.report_settings.smtp_port,
                );
            }
        }
        ui.label("此处填写变量名。作业运行时从对应的环境变量读取凭据。");
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            if ui.button("选择新作业文件…").clicked() {
                self.pick_destination();
            }
            if let Some(path) = &self.destination {
                path_label(ui, path);
            }
        });
    }

    fn pick_destination(&mut self) {
        let stem = self
            .source
            .path()
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("旧版作业");
        let mut dialog = FileDialog::new()
            .set_title("保存迁移后的作业")
            .add_filter("作业 JSON", &["json"])
            .set_file_name(format!("{stem}.json"));
        if let Some(parent) = self.destination.as_deref().and_then(Path::parent) {
            dialog = dialog.set_directory(parent);
        }
        if let Some(mut path) = dialog.save_file() {
            if path.extension().is_none() {
                path.set_extension("json");
            }
            self.destination = Some(path);
            self.error = None;
        }
    }

    fn preview(&mut self) {
        if self.destination.is_none() {
            self.pick_destination();
        }
        if let Some(destination) = &self.destination {
            match self.source.prepare_with_report(
                destination,
                &self.request,
                Some(&self.report_settings),
            ) {
                Ok(prepared) => {
                    self.prepared = Some(prepared);
                    self.error = None;
                }
                Err(error) => self.error = Some(error),
            }
        }
    }
}

fn review(ui: &mut egui::Ui, prepared: &Prepared) {
    ui.horizontal_wrapped(|ui| {
        ui.label("新作业文件");
        path_label(ui, prepared.destination());
    });
    for note in prepared.notes() {
        ui.add(egui::Label::new(note).wrap());
    }
}

fn path_label(ui: &mut egui::Ui, path: &Path) {
    let name = path.display().to_string();
    ui.add(egui::Label::new(&name).wrap()).on_hover_text(name);
}

fn field(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.label(label);
    ui.add(TextEdit::singleline(value).desired_width(260.0));
    ui.end_row();
}

fn smtp_credentials(ui: &mut egui::Ui, user: &mut String, password: &mut String) {
    field(ui, "SMTP 用户名变量名", user);
    field(ui, "SMTP 密码变量名", password);
}

fn smtp_transport(ui: &mut egui::Ui, tls: &mut bool, port: &mut u16) {
    ui.horizontal_wrapped(|ui| {
        ui.label("邮件加密");
        let previous = *tls;
        ui.radio_value(tls, false, "STARTTLS");
        ui.radio_value(tls, true, "直接 TLS");
        if previous != *tls && matches!(*port, 587 | 465) {
            *port = if *tls { 465 } else { 587 };
        }
        ui.label("端口");
        ui.add(DragValue::new(port).range(1..=u16::MAX));
    });
}

fn password(ui: &mut egui::Ui, label: &str, variable: &mut Option<String>) {
    let mut passwordless = variable.is_none();
    if ui.checkbox(&mut passwordless, "数据库无需密码").changed() {
        *variable = (!passwordless).then(String::new);
    }
    if let Some(variable) = variable {
        egui::Grid::new("legacy-db-credentials")
            .num_columns(2)
            .show(ui, |ui| field(ui, label, variable));
    }
}

fn endpoint(transport: Transport) -> Endpoint {
    let address = match transport {
        Transport::Direct => Address::Original,
        Transport::Http => Address::Http(String::new()),
        Transport::Tunnel => Address::Direct {
            host: String::new(),
            port: 3306,
        },
    };
    Endpoint {
        address,
        ..Default::default()
    }
}

fn sync_fields(ui: &mut egui::Ui, label: &str, transport: Transport, endpoint: &mut Endpoint) {
    ui.group(|ui| {
        ui.strong(label);
        let choice = match &endpoint.address {
            Address::Original => "沿用旧直连地址",
            Address::Http(_) => "填写 PHP 隧道地址",
            Address::Direct { .. } => "填写直连地址",
        };
        ComboBox::from_id_salt("legacy-sync-transport")
            .selected_text(choice)
            .show_ui(ui, |ui| {
                if transport == Transport::Direct
                    && ui
                        .selectable_label(
                            matches!(endpoint.address, Address::Original),
                            "沿用旧直连地址",
                        )
                        .clicked()
                {
                    endpoint.address = Address::Original;
                }
                let uses_direct = matches!(endpoint.address, Address::Direct { .. });
                if ui.selectable_label(uses_direct, "填写直连地址").clicked() && !uses_direct
                {
                    endpoint.address = Address::Direct {
                        host: String::new(),
                        port: 3306,
                    };
                }
                let uses_http = matches!(endpoint.address, Address::Http(_));
                if transport == Transport::Http
                    && ui
                        .selectable_label(uses_http, "填写 PHP 隧道地址")
                        .clicked()
                    && !uses_http
                {
                    endpoint.address = Address::Http(String::new());
                }
            });
        egui::Grid::new("legacy-sync-endpoint")
            .num_columns(2)
            .show(ui, |ui| {
                field(ui, "数据库密码变量名", &mut endpoint.password_env);
                match &mut endpoint.address {
                    Address::Original => {}
                    Address::Direct { host, port } => {
                        field(ui, "MySQL 主机", host);
                        ui.label("端口");
                        ui.add(DragValue::new(port).range(1..=u16::MAX));
                        ui.end_row();
                    }
                    Address::Http(address) => field(ui, "PHP 隧道地址", address),
                }
            });
    });
}

fn direct_file_source(
    format: Option<FileSourceFormat>,
    default_path: Option<&Path>,
) -> Option<OdbcSource> {
    let path = default_path.map(Path::to_path_buf).unwrap_or_default();
    match format {
        Some(FileSourceFormat::Csv) => Some(OdbcSource::CsvPath(path)),
        Some(FileSourceFormat::Excel) => Some(OdbcSource::ExcelPath(path)),
        Some(FileSourceFormat::Access) => Some(OdbcSource::AccessPath {
            path,
            password_env: None,
        }),
        None => None,
    }
}

fn odbc_source(
    ui: &mut egui::Ui,
    requires_connection_string: bool,
    source_file_format: Option<FileSourceFormat>,
    default_path: Option<&Path>,
    source: &mut OdbcSource,
) {
    if let Some(file_source) = direct_file_source(source_file_format, default_path) {
        ui.horizontal_wrapped(|ui| {
            ui.label("导入来源");
            let mut file = matches!(
                source,
                OdbcSource::CsvPath(_) | OdbcSource::ExcelPath(_) | OdbcSource::AccessPath { .. }
            );
            let before = file;
            let label = match source_file_format {
                Some(FileSourceFormat::Access) => "直接读取 Access 文件",
                Some(FileSourceFormat::Excel) => "直接读取 Excel 工作簿",
                _ => "直接读取 CSV 文件或文件夹",
            };
            ui.radio_value(&mut file, true, label);
            ui.radio_value(&mut file, false, "从环境变量读取 ODBC 连接字符串");
            if file != before {
                *source = if file {
                    file_source
                } else {
                    OdbcSource::ConnectionString(String::new())
                };
            }
        });
    } else if !requires_connection_string {
        ui.horizontal_wrapped(|ui| {
            ui.label("ODBC 来源");
            let mut dsn = matches!(source, OdbcSource::Dsn { .. });
            let before = dsn;
            ui.radio_value(&mut dsn, true, "沿用旧 DSN");
            ui.radio_value(&mut dsn, false, "从环境变量读取连接字符串");
            if before != dsn {
                *source = if dsn {
                    OdbcSource::Dsn {
                        user_env: None,
                        password_env: None,
                    }
                } else {
                    OdbcSource::ConnectionString(String::new())
                };
            }
        });
    }
    egui::Grid::new("legacy-odbc-source")
        .num_columns(2)
        .show(ui, |ui| match source {
            OdbcSource::Dsn {
                user_env,
                password_env,
            } => {
                optional_field(ui, "源用户名变量名", user_env, "可选");
                optional_field(ui, "源密码变量名", password_env, "可选");
            }
            OdbcSource::ConnectionString(environment) => {
                field(ui, "ODBC 连接字符串变量名", environment)
            }
            OdbcSource::CsvPath(path) => {
                ui.label("CSV 路径");
                ui.horizontal_wrapped(|ui| {
                    if ui.button("选 CSV 文件…").clicked()
                        && let Some(file) = FileDialog::new()
                            .add_filter("CSV 文件", &["csv"])
                            .pick_file()
                    {
                        *path = file;
                    }
                    if ui.button("选文件夹…").clicked()
                        && let Some(folder) = FileDialog::new().pick_folder()
                    {
                        *path = folder;
                    }
                    if !path.as_os_str().is_empty() {
                        path_label(ui, path);
                    }
                });
                ui.end_row();
            }
            OdbcSource::ExcelPath(path) => {
                file_path(ui, "Excel 工作簿", &["xls", "xlsx", "xlsm", "xlsb"], path);
            }
            OdbcSource::AccessPath { path, password_env } => {
                file_path(ui, "Access 数据库", &["mdb", "accdb"], path);
                optional_field(ui, "Access 密码变量名", password_env, "可选");
            }
        });
}

fn file_path(ui: &mut egui::Ui, label: &str, extensions: &[&str], path: &mut PathBuf) {
    ui.label(label);
    ui.horizontal_wrapped(|ui| {
        if ui.button("选择文件…").clicked()
            && let Some(file) = FileDialog::new().add_filter(label, extensions).pick_file()
        {
            *path = file;
        }
        if !path.as_os_str().is_empty() {
            path_label(ui, path);
        }
    });
    ui.end_row();
}

#[derive(Clone, Copy)]
enum Output {
    Directory,
    Sql,
    Html,
}

fn output_path(ui: &mut egui::Ui, label: &str, value: &mut String, output: Output) {
    ui.horizontal_wrapped(|ui| {
        ui.label(label);
        let hint = match output {
            Output::Directory => "输出目录",
            Output::Sql => "SQL 文件",
            Output::Html => "HTML 文件",
        };
        ui.add(
            TextEdit::singleline(value)
                .hint_text(hint)
                .desired_width(275.0),
        );
        if ui.button("浏览…").clicked() {
            let dialog = FileDialog::new().set_title(label);
            let path = match output {
                Output::Directory => dialog.pick_folder(),
                Output::Sql => dialog
                    .add_filter("SQL 文件", &["sql"])
                    .set_file_name("备份.sql")
                    .save_file(),
                Output::Html => dialog
                    .add_filter("HTML 报告", &["html", "htm"])
                    .set_file_name("报告.html")
                    .save_file(),
            };
            if let Some(path) = path {
                *value = path.to_string_lossy().into_owned();
            }
        }
    });
    ui.label("相对路径按新作业文件所在目录解释。");
}
