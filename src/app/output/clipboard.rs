use std::ops::Range;

use eframe::egui;

use crate::database::{
    ResultSet,
    transfer::{CsvOptions, format_result_clipboard},
};

use super::super::style;

pub(super) struct Dialog {
    title: &'static str,
    rows: Vec<usize>,
    columns: Range<usize>,
    options: CsvOptions,
    error: Option<String>,
}

pub(super) enum Outcome {
    Continue,
    Cancel,
    Copied(CsvOptions),
}

impl Dialog {
    pub(super) fn new(
        title: &'static str,
        rows: Vec<usize>,
        columns: Range<usize>,
        headers: bool,
        previous: Option<&CsvOptions>,
    ) -> Self {
        let options = previous.cloned().unwrap_or_else(|| CsvOptions {
            field_end: r"\t".into(),
            row_end: r"\n".into(),
            enclosure: String::new(),
            escape: r"\\".into(),
            null: r"\N".into(),
            headers,
            ..CsvOptions::default()
        });
        Self {
            title,
            rows,
            columns,
            options,
            error: None,
        }
    }

    pub(super) fn show(&mut self, ctx: &egui::Context, set: &ResultSet) -> Outcome {
        let declared_widths = self.columns.clone().all(|index| {
            set.export_columns
                .get(index)
                .is_some_and(|column| column.has_declared_width())
        });
        self.options.declared_widths &= declared_widths;
        let mut copy = false;
        let mut cancel = false;
        let response = style::modal(ctx, egui::Id::new("result-clipboard")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 60.0).clamp(360.0, 480.0));
            ui.horizontal(|ui| {
                style::page_title(ui, self.title);
                ui.add_space(6.0);
                style::badge(
                    ui,
                    format!("{} 行 × {} 列", self.rows.len(), self.columns.len()),
                    style::accent(),
                    style::accent_soft(),
                );
            });
            ui.add_space(8.0);

            let options = &mut self.options;
            style::section_card(ui, "格式设置", |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.radio_value(&mut options.fixed_width, false, "分隔字段");
                    ui.radio_value(&mut options.fixed_width, true, "固定宽度");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("应用 Excel 格式").clicked() {
                            apply_excel_format(options);
                        }
                    });
                });
                if options.fixed_width {
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new("定宽依据").color(style::muted()));
                        ui.radio_value(&mut options.declared_widths, false, "按内容");
                        ui.add_enabled_ui(declared_widths, |ui| {
                            ui.radio_value(&mut options.declared_widths, true, "按声明宽度");
                        })
                        .response
                        .on_disabled_hover_text("所选结果列未提供完整的声明宽度");
                    });
                }
                ui.add_space(6.0);
                egui::Grid::new("result-clipboard-settings")
                    .num_columns(2)
                    .spacing([14.0, 7.0])
                    .show(ui, |ui| {
                        if !options.fixed_width {
                            field(ui, "字段分隔符", &mut options.field_end);
                            field(ui, "封闭符", &mut options.enclosure);
                        }
                        field(ui, "行分隔符", &mut options.row_end);
                        field(ui, "转义符", &mut options.escape);
                        if !options.fixed_width {
                            field(ui, "NULL 表示", &mut options.null);
                        }
                    });
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    ui.checkbox(&mut options.headers, "首行复制字段名");
                    if !options.fixed_width {
                        ui.add_enabled_ui(!options.enclosure.is_empty(), |ui| {
                            ui.checkbox(&mut options.enclose_all, "封闭全部非 NULL 字段");
                        });
                    }
                });
                let hint = if options.fixed_width {
                    "按 UTF-8 字节数补齐空格，NULL 留空。"
                } else {
                    r"分隔符支持 \t、\r、\n 和 \\。留空则省略该项。"
                };
                ui.add_space(4.0);
                ui.label(egui::RichText::new(hint).small().color(style::muted()));
            });
            let validation = options.validate_clipboard();
            if let Err(error) = &validation {
                ui.add_space(6.0);
                style::banner(ui, error, style::error(), style::error_soft());
            } else if let Some(error) = &self.error {
                ui.add_space(6.0);
                style::banner(ui, error, style::error(), style::error_soft());
            }
            let ok = validation.is_ok();
            style::dialog_footer(ui, |ui| {
                copy = ui
                    .add_enabled(ok, style::primary_enabled(ok, "复制到剪贴板"))
                    .clicked();
                cancel = ui.button("取消").clicked();
            });
        });
        if copy {
            match format_result_clipboard(set, &self.rows, self.columns.clone(), &self.options) {
                Ok(text) => {
                    ctx.copy_text(text);
                    return Outcome::Copied(self.options.clone());
                }
                Err(error) => self.error = Some(error),
            }
        }
        if cancel || response.should_close() {
            Outcome::Cancel
        } else {
            Outcome::Continue
        }
    }
}

fn apply_excel_format(options: &mut CsvOptions) {
    options.fixed_width = false;
    options.declared_widths = false;
    options.field_end = r"\t".into();
    options.row_end = r"\r\n".into();
    options.enclosure = "\"".into();
    options.escape.clear();
    options.enclose_all = false;
    options.null = r"\N".into();
}

fn field(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.label(label);
    ui.add(
        egui::TextEdit::singleline(value)
            .desired_width(145.0)
            .char_limit(128),
    );
    ui.end_row();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Cell;

    #[test]
    fn copies_the_selected_rows_in_excel_clipboard_format() {
        let set = ResultSet {
            source_sql: None,
            columns: vec!["编号".into(), "内容".into(), "标记".into()],
            column_hints: Vec::new(),
            column_origins: Vec::new(),
            excel_types: Vec::new(),
            export_columns: Vec::new(),
            duplicate_source: None,
            notice: None,
            rows: vec![
                vec![
                    Cell::Text("1".into()),
                    Cell::Text("乙".into()),
                    Cell::Binary(vec![0, 255]),
                ],
                vec![
                    Cell::Text("2".into()),
                    Cell::Text("甲\t\"引号\"\n丙".into()),
                    Cell::Null,
                ],
                vec![
                    Cell::Text("3".into()),
                    Cell::Text("其他行".into()),
                    Cell::Null,
                ],
            ],
            affected_rows: 0,
            last_insert_id: None,
            truncated: false,
        };
        let ctx = egui::Context::default();
        let mut dialog = Dialog::new("复制选中行", vec![1, 0], 0..3, true, None);
        let mut frame = |events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(820.0, 590.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    let _ = dialog.show(ctx, &set);
                },
            )
        };
        let button_position = |frame: &egui::FullOutput, text| {
            frame
                .shapes
                .iter()
                .find_map(|shape| {
                    if let egui::Shape::Text(label) = &shape.shape
                        && label.galley.text() == text
                    {
                        return Some(label.pos + label.galley.size() * 0.5);
                    }
                    None
                })
                .expect("dialog button is visible")
        };
        let event = |pos, pressed| egui::Event::PointerButton {
            pos,
            pressed,
            button: egui::PointerButton::Primary,
            modifiers: egui::Modifiers::NONE,
        };
        frame(vec![]);
        let opening = frame(vec![]);
        let excel = button_position(&opening, "应用 Excel 格式");
        frame(vec![egui::Event::PointerMoved(excel), event(excel, true)]);
        frame(vec![event(excel, false)]);

        let formatted = frame(vec![]);
        let copy = button_position(&formatted, "复制到剪贴板");
        frame(vec![egui::Event::PointerMoved(copy), event(copy, true)]);
        let output = frame(vec![event(copy, false)]);
        assert!(output.platform_output.commands.iter().any(|command| {
            matches!(command, egui::OutputCommand::CopyText(text)
                if text == "编号\t内容\t标记\r\n2\t\"甲\t\"\"引号\"\"\n丙\"\t\\N\r\n1\t乙\t0x00FF")
        }));
    }
}
