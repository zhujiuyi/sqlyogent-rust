use eframe::egui::{self, TextEdit};
use egui_extras::{Column, TableBuilder};

use crate::database::{Command, ServerValue};
use crate::maintenance::ServerView;

use super::{error_details::ErrorDetails, output::display_cell, state::Workspace, style};

pub(super) struct Dialog {
    workspace: u64,
    view: ServerView,
    started: bool,
    pending: bool,
    values: Vec<ServerValue>,
    selected: Option<usize>,
    details: bool,
    error: Option<String>,
}

impl Dialog {
    pub(super) fn new(workspace: u64, view: ServerView) -> Self {
        Self {
            workspace,
            view,
            started: false,
            pending: false,
            values: Vec::new(),
            selected: None,
            details: false,
            error: None,
        }
    }

    fn refresh(&mut self, workspace: &mut Workspace) {
        workspace.server_values_reply = None;
        self.started = true;
        self.error = None;
        self.pending = workspace.send(
            Command::ServerValues(self.view),
            &format!("正在读取{}…", self.view.label()),
        );
        if !self.pending {
            self.error = Some(workspace.status.clone());
        }
    }

    fn receive(&mut self, workspace: &mut Workspace) {
        if !self.pending {
            return;
        }
        if let Some((view, values)) = workspace.server_values_reply.take() {
            self.pending = false;
            if view != self.view {
                self.error = Some("服务器返回了其他类型的列表。".into());
                return;
            }
            match values {
                Ok(values) => {
                    self.values = values;
                    self.selected = None;
                    self.details = false;
                }
                Err(error) => self.error = Some(error),
            }
        } else if !workspace.busy() {
            self.pending = false;
            self.error = Some(workspace.status.clone());
        }
    }

    fn show(&mut self, ctx: &egui::Context, workspace: &mut Workspace) -> bool {
        self.receive(workspace);
        let ready = workspace.server.is_some() && !workspace.busy();
        if ready && !self.started {
            self.refresh(workspace);
        }
        let response =
            crate::app::style::modal(ctx, egui::Id::new("server-value-list")).show(ctx, |ui| {
                let size = ctx.content_rect().size();
                ui.set_width((size.x - 60.0).clamp(380.0, 880.0));
                style::page_title(ui, self.view.label());
                ui.label(&workspace.label);
                if self.view.is_session()
                    && workspace
                        .server
                        .as_ref()
                        .is_some_and(|server| server.http_tunnel)
                {
                    ui.small("会话范围为本次 HTTP 请求。");
                }
                if let Some(error) = &self.error {
                    ui.colored_label(style::error(), error);
                    if ui.button("查看错误详情…").clicked() {
                        workspace.error_dialog =
                            Some(Box::new(ErrorDetails::new(self.view.sql(), error)));
                    }
                }
                if self.pending {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(&workspace.status);
                    });
                }
                if workspace.server.is_none() {
                    ui.label("连接已断开。");
                }
                if self.details {
                    let value = &self.values[self.selected.expect("selected server value")];
                    ui.strong(&value.name);
                    let text = display_cell(&value.value, false);
                    let mut readonly = text.as_str();
                    egui::ScrollArea::both()
                        .max_height(size.y * 0.5)
                        .show(ui, |ui| {
                            ui.add(
                                TextEdit::multiline(&mut readonly)
                                    .font(egui::TextStyle::Monospace)
                                    .desired_width(f32::INFINITY)
                                    .desired_rows(6),
                            );
                        });
                    if ui.button("返回列表").clicked() {
                        self.details = false;
                    }
                } else {
                    let height = (size.y * 0.46).clamp(150.0, 420.0);
                    TableBuilder::new(ui)
                        .striped(true)
                        .sense(egui::Sense::click())
                        .column(Column::initial(240.0).resizable(true).clip(true))
                        .column(Column::remainder().clip(true))
                        .max_scroll_height(height)
                        .header(24.0, |mut header| {
                            header.col(|ui| {
                                ui.strong("名称");
                            });
                            header.col(|ui| {
                                ui.strong("值");
                            });
                        })
                        .body(|body| {
                            body.rows(24.0, self.values.len(), |mut row| {
                                let index = row.index();
                                let value = &self.values[index];
                                row.set_selected(self.selected == Some(index));
                                row.col(|ui| {
                                    ui.label(&value.name);
                                });
                                row.col(|ui| {
                                    ui.add(
                                        egui::Label::new(display_cell(&value.value, true))
                                            .truncate(),
                                    );
                                });
                                let response = row.response();
                                if response.clicked() || response.double_clicked() {
                                    self.selected = Some(index);
                                    self.details = response.double_clicked();
                                }
                            });
                        });
                }
                style::dialog_footer_split(
                    ui,
                    |ui| {
                        if ui
                            .add_enabled(ready && !self.pending, egui::Button::new("刷新"))
                            .clicked()
                        {
                            self.refresh(workspace);
                        }
                        if !self.details
                            && ui
                                .add_enabled(
                                    self.selected.is_some(),
                                    egui::Button::new("查看完整值"),
                                )
                                .clicked()
                        {
                            self.details = true;
                        }
                    },
                    |ui| ui.button("关闭").clicked(),
                )
                .1
            });
        response.inner || response.should_close()
    }
}

pub(super) fn dialog(
    ctx: &egui::Context,
    state: &mut Option<Dialog>,
    workspaces: &mut [Workspace],
) {
    let Some(dialog) = state else { return };
    let Some(workspace) = workspaces.iter_mut().find(|ws| ws.key == dialog.workspace) else {
        *state = None;
        return;
    };
    if dialog.show(ctx, workspace) {
        *state = None;
    }
}
