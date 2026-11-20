use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui::{self, RichText};

use crate::database::{Command, DbObject, transfer::DumpOptions};

use super::{preferences::DumpPreferences, state::Workspace, style};

#[derive(Default)]
pub(super) struct Schedule {
    jobs: Vec<Job>,
    form: Option<Form>,
    next_id: usize,
}

struct Form {
    workspace: u64,
    schema: String,
    object: Option<DbObject>,
    folder: Option<PathBuf>,
    interval: u64,
    first_now: bool,
}

struct Job {
    id: usize,
    workspace: u64,
    schema: String,
    object: Option<DbObject>,
    folder: PathBuf,
    interval: Duration,
    due: Instant,
    running: bool,
    last: String,
}

impl Schedule {
    pub fn open(
        &mut self,
        workspace: Option<&Workspace>,
        schema: String,
        object: Option<DbObject>,
    ) {
        self.form = Some(Form {
            workspace: workspace.map_or(0, |workspace| workspace.key),
            schema,
            object,
            folder: None,
            interval: 60,
            first_now: false,
        });
    }

    pub fn open_now(&self) -> bool {
        self.form.is_some()
    }

    pub fn tick(
        &mut self,
        ctx: &egui::Context,
        workspaces: &mut [Workspace],
        allow_start: bool,
        defaults: DumpPreferences,
    ) {
        if self.jobs.is_empty() {
            return;
        }
        for job in &mut self.jobs {
            let Some(ws) = workspaces.iter_mut().find(|ws| ws.key == job.workspace) else {
                job.running = false;
                job.last = "工作区已关闭，请移除此计划。".into();
                continue;
            };
            if job.running
                && let Some(result) = ws.backup_result.take()
            {
                job.running = false;
                job.last = result.unwrap_or_else(|error| format!("备份失败：{error}"));
            }
        }
        ctx.request_repaint_after(Duration::from_secs(1));
        if !allow_start {
            return;
        }

        let now = Instant::now();
        for job in &mut self.jobs {
            let Some(ws) = workspaces.iter_mut().find(|ws| ws.key == job.workspace) else {
                continue;
            };
            if !job.running && job.due <= now && ws.server.is_some() && !ws.busy() {
                let epoch = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                let name = clean_name(
                    job.object
                        .as_ref()
                        .map_or(job.schema.as_str(), |object| object.name.as_str()),
                );
                let path = job.folder.join(format!(
                    "备份_{name}_{epoch}_{}_{}.sql",
                    std::process::id(),
                    job.id
                ));
                let command = Command::Dump {
                    schema: job.schema.clone(),
                    object: job.object.clone(),
                    path,
                    options: DumpOptions::default(),
                    chunk_rows: defaults.chunk_rows(),
                    bulk_insert_limit_kib: defaults.bulk_insert_limit_kib(),
                };
                job.running = ws.send(command, "正在执行计划备份…");
                job.due = now + job.interval;
                if !job.running {
                    job.last = format!("无法开始备份：{}", ws.status);
                }
            }
        }
    }

    pub fn dialog(&mut self, ctx: &egui::Context, workspaces: &[Workspace]) {
        let Some(form) = &mut self.form else { return };
        let mut create = false;
        let mut close = false;
        let mut remove = None;
        crate::app::style::modal(ctx, egui::Id::new("backup-schedule")).show(ctx, |ui| {
            let width = (ctx.content_rect().width() - 48.0).clamp(400.0, 680.0);
            ui.set_width(width);
            style::page_title(ui, "连接内间隔备份");
            ui.label(
                RichText::new("备份在应用打开并保持连接时按设定间隔自动运行。")
                    .small()
                    .color(style::muted()),
            );
            ui.add_space(10.0);
            let jobs_panel = |ui: &mut egui::Ui, remove: &mut Option<usize>| {
                style::section_card_with_header(
                    ui,
                    "当前计划",
                    |ui| {
                        style::badge(
                            ui,
                            format!("{} 项", self.jobs.len()),
                            style::accent(),
                            style::accent_soft(),
                        );
                    },
                    |ui| {
                        if self.jobs.is_empty() {
                            ui.label(
                                RichText::new("暂未添加连接内定时备份任务。")
                                    .small()
                                    .color(style::muted()),
                            );
                        } else {
                            egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .max_height(210.0)
                                .show(ui, |ui| {
                                    for job in &self.jobs {
                                        let name = target_label(&job.schema, job.object.as_ref());
                                        let workspace =
                                            workspaces.iter().find(|ws| ws.key == job.workspace);
                                        let suffix = if job.running {
                                            "正在备份".to_owned()
                                        } else if workspace.is_some_and(|ws| ws.server.is_some()) {
                                            format!(
                                                "约 {} 分钟后",
                                                job.due
                                                    .saturating_duration_since(Instant::now())
                                                    .as_secs()
                                                    .div_ceil(60)
                                            )
                                        } else {
                                            "等待连接".into()
                                        };
                                        style::subtle_card(egui::Margin::symmetric(10, 7)).show(
                                            ui,
                                            |ui| {
                                                ui.set_min_width(ui.available_width());
                                                ui.horizontal(|ui| {
                                                    ui.label(
                                                        RichText::new(name)
                                                            .strong()
                                                            .color(style::ink()),
                                                    );
                                                    ui.with_layout(
                                                        egui::Layout::right_to_left(
                                                            egui::Align::Center,
                                                        ),
                                                        |ui| {
                                                            if ui
                                                                .add_enabled(
                                                                    !job.running,
                                                                    egui::Button::new("移除"),
                                                                )
                                                                .clicked()
                                                            {
                                                                *remove = Some(job.id);
                                                            }
                                                        },
                                                    );
                                                });
                                                ui.label(
                                                    RichText::new(format!(
                                                        "每 {} 分钟 · {suffix}",
                                                        job.interval.as_secs() / 60
                                                    ))
                                                    .small()
                                                    .color(style::muted()),
                                                );
                                                if !job.last.is_empty() {
                                                    ui.add(
                                                        egui::Label::new(
                                                            RichText::new(&job.last)
                                                                .small()
                                                                .color(style::muted()),
                                                        )
                                                        .truncate(),
                                                    );
                                                }
                                            },
                                        );
                                        ui.add_space(5.0);
                                    }
                                });
                        }
                    },
                );
            };
            if form.workspace != 0 && width >= 540.0 {
                style::split_two_columns(
                    ui,
                    0.48,
                    12.0,
                    |ui| jobs_panel(ui, &mut remove),
                    |ui| {
                        style::section_card(ui, "新增备份计划", |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("目标").small().color(style::muted()));
                                style::badge(
                                    ui,
                                    target_label(&form.schema, form.object.as_ref()),
                                    style::accent(),
                                    style::accent_soft(),
                                );
                            });
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                ui.label("每");
                                ui.add(
                                    egui::DragValue::new(&mut form.interval).range(1..=10_080),
                                );
                                ui.label("分钟备份一次");
                            });
                            ui.checkbox(&mut form.first_now, "立即备份一次");
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                if ui.button("选择保存目录…").clicked()
                                    && let Some(folder) = rfd::FileDialog::new()
                                        .set_title("选择备份文件保存目录")
                                        .pick_folder()
                                {
                                    form.folder = Some(folder);
                                }
                            });
                            if let Some(folder) = &form.folder {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(folder.display().to_string())
                                            .small()
                                            .monospace()
                                            .color(style::ink()),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(folder.display().to_string());
                            } else {
                                ui.label(
                                    RichText::new("尚未选择保存目录")
                                        .small()
                                        .color(style::muted()),
                                );
                            }
                            ui.add_space(6.0);
                            ui.label(
                                RichText::new("按常规偏好中的 SQL 转储设置生成备份。")
                                    .small()
                                    .color(style::muted()),
                            );
                            if workspaces
                                .iter()
                                .find(|workspace| workspace.key == form.workspace)
                                .and_then(|workspace| workspace.server.as_ref())
                                .is_some_and(|server| !server.http_tunnel)
                            {
                                ui.label(
                                    RichText::new(
                                        "InnoDB 使用同一快照。备份期间请保持非事务表数据稳定。",
                                    )
                                    .small()
                                    .color(style::muted()),
                                );
                            }
                        });
                    },
                );
            } else {
                jobs_panel(ui, &mut remove);
                if form.workspace != 0 {
                    ui.add_space(8.0);
                    style::section_card(ui, "新增备份计划", |ui| {
                        ui.label(target_label(&form.schema, form.object.as_ref()));
                        ui.horizontal(|ui| {
                            ui.label("每");
                            ui.add(egui::DragValue::new(&mut form.interval).range(1..=10_080));
                            ui.label("分钟备份一次");
                            ui.checkbox(&mut form.first_now, "立即备份一次");
                        });
                        ui.horizontal(|ui| {
                            if ui.button("选择保存目录…").clicked()
                                && let Some(folder) = rfd::FileDialog::new()
                                    .set_title("选择备份文件保存目录")
                                    .pick_folder()
                            {
                                form.folder = Some(folder);
                            }
                            if let Some(folder) = &form.folder {
                                ui.add(egui::Label::new(folder.display().to_string()).truncate());
                            }
                        });
                    });
                }
            }
            style::dialog_footer(ui, |ui| {
                if form.workspace != 0 {
                    let ready = form.folder.is_some();
                    create = ui
                        .add_enabled(ready, style::primary_enabled(ready, "添加计划"))
                        .clicked();
                }
                close = ui.button("关闭").clicked();
            });
        });
        if let Some(id) = remove {
            self.jobs.retain(|job| job.id != id);
        }
        if create && let Some(folder) = form.folder.take() {
            let interval = Duration::from_secs(form.interval * 60);
            self.next_id += 1;
            self.jobs.push(Job {
                id: self.next_id,
                workspace: form.workspace,
                schema: form.schema.clone(),
                object: form.object.clone(),
                folder,
                interval,
                running: false,
                last: String::new(),
                due: if form.first_now {
                    Instant::now()
                } else {
                    Instant::now() + interval
                },
            });
            self.form = None;
        } else if close {
            self.form = None;
        }
    }
}

fn target_label(schema: &str, object: Option<&DbObject>) -> String {
    object.map_or_else(|| schema.to_owned(), |obj| format!("{schema}.{}", obj.name))
}

fn clean_name(name: &str) -> String {
    name.chars()
        .take(80)
        .map(|ch| {
            if ch.is_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}
