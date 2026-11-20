use eframe::egui::{self, TextEdit};

use super::{state::write_atomic, style};

#[derive(Clone)]
pub(super) struct ErrorDetails {
    sql: String,
    message: String,
    code: Option<u16>,
    save_error: Option<String>,
}

impl ErrorDetails {
    pub(super) fn new(sql: &str, message: &str) -> Self {
        let code = message.strip_prefix("ERROR ").and_then(|rest| {
            let length = rest.bytes().take_while(u8::is_ascii_digit).count();
            rest[..length].parse().ok()
        });
        Self {
            sql: sql.into(),
            message: message.into(),
            code,
            save_error: None,
        }
    }

    fn document(&self) -> String {
        let code = self
            .code
            .map_or("未提供".to_owned(), |code| code.to_string());
        format!(
            "SQL\n{}\n\n错误编号\n{code}\n\n服务器消息\n{}\n",
            self.sql, self.message
        )
    }
}

pub(super) fn dialog(ctx: &egui::Context, state: &mut Option<Box<ErrorDetails>>) {
    let Some(detail) = state else { return };
    let response = style::modal(ctx, egui::Id::new("database-error-details")).show(ctx, |ui| {
        let size = ctx.content_rect().size();
        ui.set_width((size.x - 60.0).clamp(320.0, 750.0));
        ui.horizontal(|ui| {
            style::page_title(ui, "数据库错误详情");
            if let Some(code) = detail.code {
                style::badge(
                    ui,
                    format!("ERROR {code}"),
                    style::error(),
                    style::error_soft(),
                );
            }
        });
        ui.add_space(6.0);
        style::section_card(ui, "服务器消息", |ui| {
            detail_text(ui, "server-message", &detail.message, size.y * 0.21);
        });
        if !detail.sql.is_empty() {
            ui.add_space(6.0);
            style::section_card(ui, "执行的 SQL", |ui| {
                detail_text(ui, "failed-statement", &detail.sql, size.y * 0.30);
            });
        }
        if let Some(error) = &detail.save_error {
            style::banner(ui, error, style::error(), style::error_soft());
        }
        style::dialog_footer_split(
            ui,
            |ui| {
                if ui.button("复制详情").clicked() {
                    ctx.copy_text(detail.document());
                }
                if ui.button("另存错误详情…").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .set_title("另存错误详情")
                        .add_filter("文本文件", &["txt"])
                        .set_file_name("数据库错误.txt")
                        .save_file()
                {
                    detail.save_error = write_atomic(&path, detail.document().as_bytes())
                        .err()
                        .map(|error| format!("无法保存错误详情。{error}"));
                }
            },
            |ui| ui.button("关闭").clicked(),
        )
        .1
    });
    if response.inner || response.should_close() {
        *state = None;
    }
}

fn detail_text(ui: &mut egui::Ui, id: &str, mut content: &str, height: f32) {
    egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(height)
        .show(ui, |ui| {
            ui.add(
                TextEdit::multiline(&mut content)
                    .font(egui::TextStyle::Monospace)
                    .background_color(style::panel())
                    .margin(egui::Margin::symmetric(8, 6))
                    .desired_width(f32::INFINITY)
                    .desired_rows(2),
            );
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_document_keeps_original_sql_and_server_code() {
        let sql = "CREATE DATABASE 测试库";
        let message = "ERROR 1007 (HY000): 数据库已存在";
        let detail = ErrorDetails::new(sql, message);
        assert_eq!(detail.code, Some(1007));
        assert!(detail.document().contains(sql));
        assert!(detail.document().contains(message));
    }
}
