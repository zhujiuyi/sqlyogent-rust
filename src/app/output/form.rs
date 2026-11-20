use eframe::egui::{self, Align, Layout, RichText, Sense};
use egui_extras::{Column, TableBuilder};

use crate::database::{Cell, ResultSet};

use super::super::style;
use super::{grid, values};

#[derive(Default)]
pub(super) struct Record {
    source_row: Option<usize>,
}

impl Record {
    pub(super) fn select(&mut self, source_row: usize) {
        self.source_row = Some(source_row);
    }

    fn current(&mut self, visible: &[usize]) -> Option<(usize, usize)> {
        let position = self
            .source_row
            .and_then(|source| visible.iter().position(|row| *row == source))
            .unwrap_or(0);
        let source = *visible.get(position)?;
        self.select(source);
        Some((position, source))
    }
}

pub(super) fn show(
    ui: &mut egui::Ui,
    set: &ResultSet,
    view: &mut grid::View,
    record: &mut Record,
    copy_headers: bool,
    request_focus: bool,
) {
    let Some((_, source)) = record.current(view.visible()) else {
        ui.label("没有符合条件的记录。");
        return;
    };
    let row_height = style::grid_row_height(ui, grid::GRID_ROW_HEIGHT);
    let header_height = style::grid_row_height(ui, grid::GRID_HEADER_HEIGHT);
    let body_height = (ui.available_height()
        - header_height
        - ui.spacing().interact_size.y
        - ui.spacing().item_spacing.y * 2.0)
        .max(0.0);
    let label_width = (ui.available_width() * 0.35).clamp(82.0, 220.0);
    let original_style = ui.style().clone();
    style::apply_grid_font(ui);
    ui.spacing_mut().item_spacing.y = grid::GRID_ROW_GAP;
    grid::paint_grid_header(ui, header_height);
    TableBuilder::new(ui)
        .id_salt("result-form-fields")
        .striped(true)
        .sense(Sense::click())
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(
            Column::initial(label_width)
                .at_least(66.0)
                .clip(true)
                .resizable(true),
        )
        .column(Column::remainder().clip(true))
        .min_scrolled_height(0.0)
        .max_scroll_height(body_height)
        .header(header_height, |mut header| {
            header.col(|ui| {
                grid::grid_label(ui, RichText::new("字段").strong().color(style::ink()), false);
            });
            header.col(|ui| {
                grid::grid_label(ui, RichText::new("值").strong().color(style::ink()), false);
            });
        })
        .body(|body| {
            body.rows(row_height, set.columns.len(), |mut row| {
                let column = row.index();
                let name = &set.columns[column];
                let (_, field) = row.col(|ui| {
                    grid::grid_label(ui, RichText::new(name).color(style::muted()), false);
                });
                field.on_hover_text(name);
                let cell = set.rows[source].get(column).unwrap_or(&Cell::Null);
                let (_, response) = row.col(|ui| {
                    grid::grid_cell(ui, cell, false, false);
                });
                if response.double_clicked() && !matches!(cell, Cell::Null) {
                    view.inspect_cell(set, source, column);
                }
                response.context_menu(|ui| {
                    grid::form_cell_menu(ui, set, view, source, column, copy_headers);
                });
            });
        });
    ui.set_style(original_style);

    view.prepare(set);
    let visible = view.visible();
    let Some((position, source)) = record.current(visible) else {
        return;
    };
    let last = visible.len() - 1;
    ui.horizontal_wrapped(|ui| {
        for (label, destination) in [
            ("首行", 0),
            ("上一行", position.saturating_sub(1)),
            ("下一行", (position + 1).min(last)),
            ("末行", last),
        ] {
            if ui
                .add_enabled(destination != position, egui::Button::new(label))
                .clicked()
            {
                record.select(visible[destination]);
            }
        }
        ui.label(
            RichText::new(format!("第 {} 行，共 {} 行", position + 1, visible.len()))
                .small()
                .color(style::muted()),
        );
        let copy = ui.button("复制本行");
        if request_focus {
            copy.request_focus();
        }
        if copy.clicked() {
            ui.ctx().copy_text(values::clipboard_rows(
                set,
                &[source],
                0..set.columns.len(),
                copy_headers,
                values::ClipboardFormat::Tsv,
            ));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigates_filtered_records_and_copies_the_current_row() {
        let set = ResultSet {
            source_sql: None,
            columns: vec!["entry".into(), "payload".into()],
            column_hints: Vec::new(),
            column_origins: Vec::new(),
            excel_types: Vec::new(),
            export_columns: Vec::new(),
            duplicate_source: None,
            notice: None,
            rows: vec![
                vec![Cell::Text("match first".into()), Cell::Null],
                vec![Cell::Text("filtered out".into()), Cell::Null],
                vec![Cell::Text("match last".into()), Cell::Binary(vec![0, 255])],
            ],
            affected_rows: 0,
            last_insert_id: None,
            truncated: false,
        };
        let ctx = egui::Context::default();
        let mut view = grid::View::default();
        view.search = "match".into();
        view.prepare(&set);
        let mut record = Record::default();
        let mut frame = |events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(650.0, 310.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        show(ui, &set, &mut view, &mut record, true, false);
                    });
                },
            )
        };
        frame(Vec::new());
        let mut click = |label: &str| {
            let painted = frame(Vec::new());
            let pos = painted
                .shapes
                .iter()
                .find_map(|shape| {
                    if let egui::Shape::Text(text) = &shape.shape
                        && text.galley.text() == label
                    {
                        Some(text.pos + text.galley.size() * 0.5)
                    } else {
                        None
                    }
                })
                .expect("form control is visible");
            let button = |pressed| egui::Event::PointerButton {
                pos,
                pressed,
                button: egui::PointerButton::Primary,
                modifiers: egui::Modifiers::NONE,
            };
            frame(vec![egui::Event::PointerMoved(pos), button(true)]);
            frame(vec![button(false)])
        };
        let copied = |output: egui::FullOutput, expected: &str| {
            assert!(output.platform_output.commands.iter().any(|command| {
                matches!(command, egui::OutputCommand::CopyText(text) if text == expected)
            }));
        };
        copied(click("复制本行"), "entry\tpayload\nmatch first\t\\N");
        click("末行");
        copied(click("复制本行"), "entry\tpayload\nmatch last\t\\x00ff");
    }
}
