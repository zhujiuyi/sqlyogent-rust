use eframe::egui::{self, Button, RichText, Sense};

use super::{
    Cell, GRID_ROW_HEIGHT, GridAction, Navigate, Point, TablePage, cell_filter_menu, cell_text,
    copy_menu, display_cell, inspectable, preset_menu, style,
};

pub(super) struct Controls {
    pub interactive: bool,
    pub editable: bool,
    pub paged: bool,
    pub previous_page: bool,
    pub next_page: bool,
    pub server_filter: bool,
    pub headers: bool,
    pub height: f32,
}

pub(super) fn show(
    ui: &mut egui::Ui,
    page: &TablePage,
    current: Point,
    controls: Controls,
) -> Option<GridAction> {
    let row = page.rows.get(current.row)?;
    let mut action = None;
    let width = ui.available_width();
    let row_height = style::grid_row_height(ui, GRID_ROW_HEIGHT);
    egui::ScrollArea::vertical()
        .id_salt("table-row-form")
        .max_height((controls.height - 31.0).max(68.0))
        .show(ui, |ui| {
            style::apply_grid_font(ui);
            egui::Grid::new("current-row-fields")
                .striped(true)
                .spacing([12.0, 2.0])
                .show(ui, |ui| {
                    for (index, (column, cell)) in page.columns.iter().zip(&row.cells).enumerate() {
                        ui.add_sized(
                            [152.0, row_height],
                            egui::Label::new(
                                RichText::new(page.column_label(index)).color(style::muted()),
                            )
                            .truncate(),
                        )
                        .on_hover_text(format!("{} · {}", column.name, column.data_type));
                        let point = Point {
                            column: index,
                            ..current
                        };
                        ui.horizontal(|ui| {
                            let text = RichText::new(display_cell(cell, true)).monospace();
                            let text = if index == current.column {
                                text.strong().color(style::accent())
                            } else {
                                text
                            };
                            let text = if matches!(cell, Cell::Null) {
                                text.color(style::muted()).italics()
                            } else {
                                text
                            };
                            let inspect = inspectable(column);
                            let response = ui.add_sized(
                                [
                                    (width - if inspect { 274.0 } else { 194.0 }).max(115.0),
                                    row_height,
                                ],
                                egui::Label::new(text)
                                    .truncate()
                                    .selectable(false)
                                    .sense(Sense::click()),
                            );
                            if controls.interactive {
                                let writable = controls.editable && column.writable;
                                if response.double_clicked() && (writable || inspect) {
                                    action = Some(GridAction::Open(point));
                                } else if response.clicked() {
                                    action = Some(GridAction::Focus(point));
                                }
                                if inspect && ui.small_button("查看内容…").clicked() {
                                    action = Some(GridAction::Open(point));
                                }
                            }
                            response.context_menu(|ui| {
                                if ui.button("复制单元格").clicked() {
                                    ui.ctx().copy_text(cell_text(cell));
                                    ui.close();
                                }
                                ui.menu_button("复制", |ui| {
                                    copy_menu(ui, page, None, Some(point), controls.headers);
                                });
                                if controls.interactive && controls.editable {
                                    ui.separator();
                                    if column.writable
                                        && !inspect
                                        && ui.button("编辑此值…").clicked()
                                    {
                                        action = Some(GridAction::Open(point));
                                        ui.close();
                                    }
                                    if let Some(preset) = preset_menu(ui, column) {
                                        action = Some(GridAction::SetValue(point, preset));
                                    }
                                    if ui.button("复制为新行").clicked() {
                                        action = Some(GridAction::Duplicate(current.row));
                                        ui.close();
                                    }
                                    if ui.button("删除当前行…").clicked() {
                                        action = Some(GridAction::Delete(current.row));
                                        ui.close();
                                    }
                                }
                                if controls.server_filter
                                    && let Some(value) = row.original().get(index)
                                {
                                    ui.separator();
                                    if ui.button("按输入值筛选全表…").clicked() {
                                        action = Some(GridAction::NewFilter(index));
                                        ui.close();
                                    }
                                    if let Some(predicate) =
                                        cell_filter_menu(ui, column, cell, value)
                                    {
                                        action = Some(GridAction::Filter(predicate));
                                    }
                                }
                            });
                        });
                        ui.end_row();
                    }
                });
        });

    navigation(ui, page, current, &controls).or(action)
}

pub(super) fn adjacent(
    page: &TablePage,
    current: Point,
    controls: &Controls,
    next: bool,
) -> Option<GridAction> {
    let last = page.rows.len().checked_sub(1)?;
    let focus = |row| GridAction::Focus(Point { row, ..current });
    if next {
        if current.row < last {
            Some(focus(current.row + 1))
        } else if controls.paged && controls.next_page {
            Some(GridAction::Navigate(Navigate::Next(
                page.offset + page.rows.len(),
            )))
        } else {
            None
        }
    } else if current.row > 0 {
        Some(focus(current.row - 1))
    } else if controls.paged && controls.previous_page {
        Some(GridAction::Navigate(Navigate::Previous))
    } else {
        None
    }
}

pub(super) fn navigation(
    ui: &mut egui::Ui,
    page: &TablePage,
    current: Point,
    controls: &Controls,
) -> Option<GridAction> {
    let last = page.rows.len().checked_sub(1)?;
    let focus = |row| GridAction::Focus(Point { row, ..current });
    let mut action = None;
    ui.horizontal_wrapped(|ui| {
        for (label, command) in [
            (
                if controls.paged {
                    "本页首行"
                } else {
                    "首行"
                },
                (current.row > 0).then(|| focus(0)),
            ),
            ("上一行", adjacent(page, current, controls, false)),
            ("下一行", adjacent(page, current, controls, true)),
            (
                if controls.paged {
                    "本页末行"
                } else {
                    "末行"
                },
                (current.row < last).then(|| focus(last)),
            ),
        ] {
            if ui
                .add_enabled(
                    controls.interactive && command.is_some(),
                    Button::new(label),
                )
                .clicked()
            {
                action = command;
            }
        }
        ui.label(
            RichText::new(format!("第 {} 行", page.offset + current.row + 1))
                .small()
                .color(style::muted()),
        );
    });
    action
}
