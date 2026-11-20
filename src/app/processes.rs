use eframe::egui::{self, TextEdit};
use egui_extras::{Column, TableBuilder};

use crate::database::{
    Command, ProcessAction, ProcessOverview, ProcessReply, ProcessRequest, ProcessRow,
};

use super::{state::Workspace, style};

pub(super) struct Dialog {
    workspace_key: u64,
    started: bool,
    pending: bool,
    overview: Option<ProcessOverview>,
    selected: Option<u64>,
    review: Option<ProcessAction>,
    notice: String,
    error: String,
}

impl Dialog {
    pub(super) fn new(workspace_key: u64) -> Self {
        Self {
            workspace_key,
            started: false,
            pending: false,
            overview: None,
            selected: None,
            review: None,
            notice: String::new(),
            error: String::new(),
        }
    }

    fn request(&mut self, ws: &mut Workspace, request: ProcessRequest) {
        self.error.clear();
        ws.process_reply = None;
        let status = match request {
            ProcessRequest::List => "正在读取进程列表…",
            ProcessRequest::Stop { .. } => "正在核对并处理所选进程…",
        };
        self.pending = ws.send(Command::Processes(request), status);
        if !self.pending {
            self.error = ws.status.clone();
        }
    }

    fn refresh(&mut self, ws: &mut Workspace) {
        self.overview = None;
        self.selected = None;
        self.review = None;
        self.started = true;
        self.request(ws, ProcessRequest::List);
    }

    fn receive(&mut self, ws: &mut Workspace) {
        if !self.pending {
            return;
        }
        let reply = ws.process_reply.take();
        if reply.is_none() && ws.busy() {
            return;
        }
        self.pending = false;
        match reply {
            Some(Ok(ProcessReply::Listed(overview))) => self.overview = Some(overview),
            Some(Ok(ProcessReply::Stopped { id, action })) => {
                self.notice = format!("服务器已接受{}请求（连接 ID {id}）。", action.label());
                if !ws.busy() && ws.server.is_some() {
                    self.refresh(ws);
                }
            }
            Some(Err(error)) => self.error = error,
            None => self.error = ws.status.clone(),
        }
    }

    fn show(&mut self, ctx: &egui::Context, ws: &mut Workspace) -> bool {
        self.receive(ws);
        if !self.started && !ws.busy() && ws.server.is_some() {
            self.refresh(ws);
        }
        let size = ctx.content_rect().size();
        let response =
            crate::app::style::modal(ctx, egui::Id::new("server-processes")).show(ctx, |ui| {
                ui.set_width((size.x - 64.0).clamp(420.0, 960.0));
                ui.heading("进程列表");
                ui.colored_label(style::MUTED, &ws.label);
                if !self.notice.is_empty() {
                    ui.label(&self.notice);
                }
                if !self.error.is_empty() {
                    ui.colored_label(style::ERROR, &self.error);
                }
                ui.separator();
                if ws.server.is_none() {
                    ui.label("连接已断开。");
                } else if self.pending {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(&ws.status);
                    });
                } else if let Some(overview) = self.overview.as_ref() {
                    if let Some(action) = self.review {
                        let target = self
                            .selected
                            .and_then(|id| overview.rows.iter().find(|row| row.id == id));
                        if let Some(row) = target {
                            if review(ui, row, action, !ws.busy()) {
                                let request = overview.stop(row.id, action);
                                self.overview = None;
                                self.selected = None;
                                self.review = None;
                                self.notice.clear();
                                if let Some(request) = request {
                                    self.request(ws, request);
                                }
                            } else if ui.button("返回进程列表").clicked() {
                                self.review = None;
                            }
                        }
                    } else {
                        if overview.truncated {
                            ui.label("进程太多，仅显示服务器返回的一部分记录。");
                        }
                        if ws.server.as_ref().is_some_and(|server| server.http_tunnel) {
                            ui.label("终止进程需要直连或 SSH 会话。");
                        }
                        show_rows(
                            ui,
                            overview,
                            &mut self.selected,
                            (size.y * 0.44).clamp(120.0, 390.0),
                        );
                        if let Some(row) = self
                            .selected
                            .and_then(|id| overview.rows.iter().find(|row| row.id == id))
                        {
                            ui.separator();
                            show_sql(ui, row.sql.as_deref());
                            ui.horizontal(|ui| {
                                for action in [ProcessAction::StopQuery, ProcessAction::Disconnect]
                                {
                                    if ui
                                        .add_enabled(
                                            !ws.busy() && overview.can_stop(row, action),
                                            egui::Button::new(format!("{}…", action.label())),
                                        )
                                        .clicked()
                                    {
                                        self.review = Some(action);
                                    }
                                }
                                if Some(row.id) == overview.own_id {
                                    ui.colored_label(style::MUTED, "当前工作区的连接");
                                }
                            });
                        }
                    }
                } else if ws.busy() {
                    ui.label("等待当前连接任务完成…");
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!ws.busy() && ws.server.is_some(), egui::Button::new("刷新"))
                        .clicked()
                    {
                        self.notice.clear();
                        self.refresh(ws);
                    }
                    ui.button("关闭").clicked()
                })
                .inner
            });
        response.inner || response.should_close()
    }
}

fn show_rows(
    ui: &mut egui::Ui,
    overview: &ProcessOverview,
    selected: &mut Option<u64>,
    max_height: f32,
) {
    if overview.rows.is_empty() {
        ui.label("当前账户看不到可列出的进程。");
        return;
    }
    let headers = [
        "连接 ID",
        "用户",
        "来源",
        "数据库",
        "命令",
        "秒数",
        "状态",
        "SQL",
    ];
    TableBuilder::new(ui)
        .id_salt("process-rows")
        .striped(true)
        .resizable(true)
        .sense(egui::Sense::click())
        .max_scroll_height(max_height)
        .columns(Column::initial(72.0).clip(true), 2)
        .column(Column::initial(132.0).clip(true))
        .columns(Column::initial(84.0).clip(true), 2)
        .column(Column::initial(45.0).clip(true))
        .column(Column::initial(120.0).clip(true))
        .column(Column::remainder().at_least(90.0).clip(true))
        .header(23.0, |mut header| {
            for label in headers {
                header.col(|ui| {
                    ui.strong(label);
                });
            }
        })
        .body(|body| {
            body.rows(24.0, overview.rows.len(), |mut row| {
                let process = &overview.rows[row.index()];
                row.set_selected(Some(process.id) == *selected);
                let id = process.id.to_string();
                let time = process.seconds.map(|value| value.to_string());
                for text in [
                    id.as_str(),
                    &process.user,
                    &process.host,
                    process.database.as_deref().unwrap_or("—"),
                    &process.command,
                    time.as_deref().unwrap_or("—"),
                    process.state.as_deref().unwrap_or("—"),
                    process.sql.as_deref().unwrap_or("—"),
                ] {
                    row.col(|ui| {
                        ui.add(egui::Label::new(text).truncate().selectable(false));
                    });
                }
                if row.response().clicked() {
                    *selected = Some(process.id);
                }
            });
        });
}

fn review(ui: &mut egui::Ui, row: &ProcessRow, action: ProcessAction, available: bool) -> bool {
    ui.strong(format!("{}连接 ID {}", action.label(), row.id));
    ui.label(format!("{}  ·  {}", row.user, row.host));
    if let Some(database) = &row.database {
        ui.monospace(database);
    }
    ui.label(match action {
        ProcessAction::StopQuery => "中断当前 SQL，保留此连接及其事务。",
        ProcessAction::Disconnect => "断开此连接，服务器将回滚未提交的事务。",
    });
    show_sql(ui, row.sql.as_deref());
    let label = format!("确认{}", action.label());
    ui.add_enabled(available, style::primary(&label)).clicked()
}

fn show_sql(ui: &mut egui::Ui, sql: Option<&str>) {
    if let Some(mut sql) = sql {
        egui::ScrollArea::vertical()
            .id_salt("process-sql")
            .max_height(92.0)
            .show(ui, |ui| {
                ui.add(
                    TextEdit::multiline(&mut sql)
                        .font(egui::TextStyle::Monospace)
                        .desired_width(f32::INFINITY)
                        .desired_rows(2),
                );
            });
    } else {
        ui.colored_label(style::MUTED, "此进程没有可显示的 SQL。");
    }
}

pub(super) fn dialog(
    ctx: &egui::Context,
    state: &mut Option<Dialog>,
    workspaces: &mut [Workspace],
) {
    let Some(dialog) = state else { return };
    let Some(ws) = workspaces
        .iter_mut()
        .find(|ws| ws.key == dialog.workspace_key)
    else {
        *state = None;
        return;
    };
    if dialog.show(ctx, ws) {
        *state = None;
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::{Duration, Instant};

    use mysql::{Conn, OptsBuilder, prelude::Queryable};

    use crate::database::ConnectionConfig;

    use super::*;

    #[test]
    #[ignore = "在一次性本机 MariaDB 中运行，需要 HARBOR_PROCESS_TEST_PORT"]
    fn process_actions_stop_a_query_then_disconnect_the_same_connection() {
        let port = std::env::var("HARBOR_PROCESS_TEST_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let mut workspace = Workspace::new("本机测试".into(), None);
        workspace.connect(
            ConnectionConfig {
                port,
                ..Default::default()
            },
            None,
        );
        let mut dialog = Dialog::new(workspace.key);
        wait_for_reply(&mut dialog, &mut workspace);
        let server = workspace.server.as_ref().expect("test connection");
        let own_id = u64::from(server.connection_id);

        let opts = OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(port)
            .user(Some("root"))
            .prefer_socket(false);
        let mut target = Conn::new(opts).unwrap();
        let target_id = u64::from(target.connection_id());
        let sleeping = thread::spawn(move || {
            let _ = target.query_first::<u64, _>("SELECT SLEEP(30)");
            target
        });
        let deadline = Instant::now() + Duration::from_secs(8);
        let request = loop {
            dialog.refresh(&mut workspace);
            wait_for_reply(&mut dialog, &mut workspace);
            let overview = dialog.overview.as_ref().expect("process overview");
            assert_eq!(overview.own_id, Some(own_id));
            if let Some(row) = overview.rows.iter().find(|row| row.id == target_id)
                && row.sql.as_deref() == Some("SELECT SLEEP(30)")
            {
                assert!(row.user == "root" && !row.host.is_empty());
                break overview.stop(target_id, ProcessAction::StopQuery).unwrap();
            }
            assert!(Instant::now() < deadline, "sleeping query did not appear");
        };
        dialog.request(&mut workspace, request);
        wait_for_reply(&mut dialog, &mut workspace);
        assert!(dialog.notice.contains("终止查询"), "{}", dialog.error);
        while !sleeping.is_finished() {
            assert!(Instant::now() < deadline, "query was not stopped");
            thread::sleep(Duration::from_millis(10));
        }
        let mut target = sleeping.join().unwrap();
        assert_eq!(
            target
                .query_first::<u64, _>("SELECT CONNECTION_ID()")
                .unwrap(),
            Some(target_id)
        );

        dialog.refresh(&mut workspace);
        wait_for_reply(&mut dialog, &mut workspace);
        let overview = dialog
            .overview
            .as_ref()
            .expect("refreshed process overview");
        let request = overview.stop(target_id, ProcessAction::Disconnect).unwrap();
        dialog.request(&mut workspace, request);
        wait_for_reply(&mut dialog, &mut workspace);
        assert!(dialog.notice.contains("断开连接"), "{}", dialog.error);
        assert!(target.query_first::<u64, _>("SELECT 1").is_err());
        assert_eq!(
            workspace.server.as_ref().unwrap().connection_id as u64,
            own_id
        );
    }

    fn wait_for_reply(dialog: &mut Dialog, workspace: &mut Workspace) {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            workspace.poll();
            dialog.receive(workspace);
            if !workspace.busy() {
                return;
            }
            assert!(Instant::now() < deadline, "{}", workspace.status);
            thread::sleep(Duration::from_millis(10));
        }
    }
}
