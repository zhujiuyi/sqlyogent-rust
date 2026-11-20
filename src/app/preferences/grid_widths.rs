use std::collections::BTreeMap;
use std::hash::Hash;
use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use eframe::egui;
use egui_extras::Column;
use serde_json::{Map, Value};

use super::{read_settings, settings_path, update_settings};
use crate::app::{output::display_cell, style};
use crate::database::Cell;

const FIELD: &str = "grid_column_widths";
const DEFAULT_WIDTH: f32 = 145.0;
const MIN_WIDTH: f32 = 70.0;
const MAX_AUTO_WIDTH: f32 = 720.0;
const CELL_PADDING: f32 = 16.0;
const RETRY_DELAY: Duration = Duration::from_secs(1);

type Widths = BTreeMap<String, BTreeMap<String, BTreeMap<String, f32>>>;
type ColumnWidthKey<'a> = (&'a str, &'a str, &'a str);

pub(in crate::app) struct GridWidths {
    enabled: bool,
    fit_to_data: bool,
    saved: Widths,
    pending: Widths,
    retry_at: Option<Instant>,
}

impl GridWidths {
    pub(in crate::app) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            fit_to_data: false,
            saved: Widths::new(),
            pending: Widths::new(),
            retry_at: None,
        }
    }

    pub(in crate::app) fn load(enabled: bool) -> Result<Self, String> {
        Self::load_at(&settings_path()?, enabled)
    }

    fn load_at(path: &Path, enabled: bool) -> Result<Self, String> {
        Ok(Self {
            saved: Self::decode(&read_settings(path)?.unwrap_or_default())?,
            ..Self::new(enabled)
        })
    }

    pub(in crate::app) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub(in crate::app) fn set_data_sizing(&mut self, enabled: bool) {
        self.fit_to_data = enabled;
    }

    fn decode(fields: &Map<String, Value>) -> Result<Widths, String> {
        fields.get(FIELD).map_or_else(
            || Ok(Widths::new()),
            |value| {
                serde_json::from_value(value.clone())
                    .map_err(|error| format!("无法读取已记忆的列宽：{error}"))
            },
        )
    }

    fn get(&self, (schema, table, column): ColumnWidthKey<'_>) -> Option<f32> {
        self.enabled
            .then(|| self.saved.get(schema)?.get(table)?.get(column).copied())
            .flatten()
            .filter(|width| width.is_finite() && *width >= MIN_WIDTH)
    }

    fn remember(&mut self, key: ColumnWidthKey<'_>, width: f32) {
        let (schema, table, column) = key;
        if self.get(key) == Some(width) {
            return;
        }
        for widths in [&mut self.saved, &mut self.pending] {
            widths
                .entry(schema.into())
                .or_default()
                .entry(table.into())
                .or_default()
                .insert(column.into(), width);
        }
    }

    pub(in crate::app) fn save_pending(&mut self, ctx: &egui::Context) -> Result<(), String> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let now = Instant::now();
        if let Some(retry_at) = self.retry_at
            && retry_at > now
        {
            ctx.request_repaint_after(retry_at - now);
            return Ok(());
        }
        match settings_path().and_then(|path| self.save_at(&path)) {
            Ok(()) => Ok(()),
            Err(error) => {
                let first_error = self.retry_at.is_none();
                self.retry_at = Some(now + RETRY_DELAY);
                ctx.request_repaint_after(RETRY_DELAY);
                if first_error {
                    return Err(format!("无法保存数据表格列宽：{error}"));
                }
                Ok(())
            }
        }
    }

    fn save_at(&mut self, path: &Path) -> Result<(), String> {
        let mut updated = Widths::new();
        update_settings(path, |fields| {
            updated = Self::decode(fields)?;
            for (schema, tables) in &self.pending {
                let schema_widths = updated.entry(schema.clone()).or_default();
                for (table, columns) in tables {
                    schema_widths
                        .entry(table.clone())
                        .or_default()
                        .extend(columns.iter().map(|(name, &width)| (name.clone(), width)));
                }
            }
            let value = serde_json::to_value(&updated).map_err(|error| error.to_string())?;
            fields.insert(FIELD.into(), value);
            Ok(())
        })?;
        self.saved = updated;
        self.pending.clear();
        self.retry_at = None;
        Ok(())
    }
}

#[derive(Default)]
pub(in crate::app) struct GridColumns {
    layout: Option<egui::Id>,
    local: Vec<Option<f32>>,
    drag: Option<ColumnDrag>,
    automatic: Option<DataWidths>,
}

struct DataWidths {
    font: egui::FontId,
    faces: Vec<Weak<egui::FontData>>,
    scale: u32,
    widths: Vec<Option<f32>>,
}

struct ColumnDrag {
    index: usize,
    start: f32,
    divider_x: f32,
    width: f32,
}

pub(in crate::app) struct GridColumn<'a> {
    key: Option<ColumnWidthKey<'a>>,
    width: f32,
    minimum: f32,
}

impl GridColumn<'_> {
    pub(in crate::app) fn column(&self) -> Column {
        Column::exact(self.width)
    }
}

impl GridColumns {
    pub(in crate::app) fn invalidate_automatic(&mut self) {
        self.automatic = None;
    }

    pub(in crate::app) fn measure_data<'a>(
        &mut self,
        ui: &mut egui::Ui,
        remembered: &GridWidths,
        columns: impl ExactSizeIterator<Item = bool>,
        cells: impl Iterator<Item = (usize, &'a Cell)>,
    ) {
        if !remembered.fit_to_data || columns.len() == 0 {
            self.invalidate_automatic();
            return;
        }
        let mut widths = columns
            .map(|use_data| use_data.then_some(MIN_WIDTH))
            .collect::<Vec<_>>();
        let font = style::grid_font(ui)
            .or_else(|| ui.style().override_font_id.clone())
            .unwrap_or_else(|| egui::TextStyle::Monospace.resolve(ui.style()));
        let scale = ui.ctx().pixels_per_point().to_bits();
        let faces = ui.fonts(|fonts| {
            let definitions = fonts.definitions();
            definitions
                .families
                .get(&font.family)
                .into_iter()
                .flatten()
                .filter_map(|name| definitions.font_data.get(name).map(Arc::downgrade))
                .collect::<Vec<_>>()
        });
        if self.automatic.as_ref().is_some_and(|old| {
            old.font == font
                && old.scale == scale
                && old.widths.len() == widths.len()
                && old
                    .widths
                    .iter()
                    .zip(&widths)
                    .all(|(old, new)| old.is_some() == new.is_some())
                && old.faces.len() == faces.len()
                && old
                    .faces
                    .iter()
                    .zip(&faces)
                    .all(|(old, new)| Weak::ptr_eq(old, new))
        }) {
            return;
        }
        ui.fonts_mut(|fonts| {
            for (column, cell) in cells {
                let Some(Some(width)) = widths.get_mut(column) else {
                    continue;
                };
                if *width >= MAX_AUTO_WIDTH {
                    continue;
                }
                let mut measured = CELL_PADDING;
                for character in display_cell(cell, true).chars() {
                    measured += fonts.glyph_width(&font, character);
                    if measured >= MAX_AUTO_WIDTH {
                        break;
                    }
                }
                let measured = measured.ceil().clamp(MIN_WIDTH, MAX_AUTO_WIDTH);
                *width = width.max(measured);
            }
        });
        self.automatic = Some(DataWidths {
            font,
            faces,
            scale,
            widths,
        });
    }

    pub(in crate::app) fn prepare<'a>(
        &mut self,
        labels: impl Hash,
        keys: impl Iterator<Item = Option<ColumnWidthKey<'a>>>,
        gutter: (f32, f32),
        remembered: &GridWidths,
    ) -> Vec<GridColumn<'a>> {
        let keys: Vec<_> = std::iter::once(None).chain(keys).collect();
        let layout = egui::Id::new((labels, &keys, remembered.enabled));
        if self.layout != Some(layout) {
            self.layout = Some(layout);
            self.local = vec![None; keys.len()];
            self.drag = None;
        }
        keys.into_iter()
            .enumerate()
            .map(|(index, key)| {
                let (default, minimum) = if index == 0 {
                    gutter
                } else {
                    (DEFAULT_WIDTH, MIN_WIDTH)
                };
                let width = self
                    .drag
                    .as_ref()
                    .filter(|drag| drag.index == index)
                    .map(|drag| drag.width)
                    .or_else(|| key.and_then(|key| remembered.get(key)))
                    .or(self.local[index])
                    .or_else(|| {
                        let index = index.checked_sub(1)?;
                        if remembered.fit_to_data {
                            return self
                                .automatic
                                .as_ref()?
                                .widths
                                .get(index)
                                .copied()
                                .flatten();
                        }
                        None
                    })
                    .unwrap_or(default);
                GridColumn {
                    key,
                    width,
                    minimum,
                }
            })
            .collect()
    }

    pub(in crate::app) fn resize_handles(
        &mut self,
        ui: &egui::Ui,
        id: egui::Id,
        top: egui::Pos2,
        columns: &[GridColumn<'_>],
        remembered: &mut GridWidths,
    ) {
        let id = id.with(self.layout);
        let spacing = ui.spacing().item_spacing.x;
        let mut x = top.x - spacing / 2.0;
        let bottom = ui.min_rect().bottom();
        let visuals = &ui.visuals().widgets;
        let mut still_dragging = false;
        for (index, column) in columns.iter().enumerate() {
            x += column.width + spacing;
            let line = [egui::pos2(x, top.y), egui::pos2(x, bottom)];
            let rect = egui::Rect::from_min_max(line[0], line[1])
                .expand(ui.style().interaction.resize_grab_radius_side);
            let response = ui.interact(rect, id.with(index), egui::Sense::click_and_drag());
            let dragging = response.dragged_by(egui::PointerButton::Primary);
            if response.hovered() || dragging {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeColumn);
            }
            let stroke = if dragging {
                visuals.active.bg_stroke
            } else if response.hovered() {
                visuals.hovered.bg_stroke
            } else {
                visuals.noninteractive.bg_stroke
            };
            ui.painter().line_segment(line, stroke);

            let stopped = response.drag_stopped_by(egui::PointerButton::Primary);
            if (dragging || stopped)
                && let Some(pointer) = response.interact_pointer_pos()
            {
                let drag = self.drag.get_or_insert(ColumnDrag {
                    index,
                    start: column.width,
                    divider_x: x,
                    width: column.width,
                });
                drag.width = (drag.start + pointer.x - drag.divider_x)
                    .max(column.minimum)
                    .round();
                still_dragging = dragging;
                if stopped && (drag.start - drag.width).abs() >= 1.0 {
                    if let Some(key) = column.key.filter(|_| remembered.enabled) {
                        remembered.remember(key, drag.width);
                    } else {
                        self.local[index] = Some(drag.width);
                    }
                }
                ui.ctx().request_repaint();
            }
        }
        if !still_dragging {
            self.drag = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dragging_persists_only_the_source_column_and_restores_it_after_reordering() {
        let ctx = egui::Context::default();
        let origin = ("work", "orders", "customer");
        let mut grid = GridColumns::default();
        let mut remembered = GridWidths::new(true);
        let mut dividers = [egui::Pos2::ZERO; 2];
        let mut render = |events| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(700.0, 400.0),
                )),
                events,
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let columns = grid.prepare(
                        ["customer_alias", "calculation"],
                        [Some(origin), None].into_iter(),
                        (55.0, 44.0),
                        &remembered,
                    );
                    let top = ui.cursor().min;
                    let spacing = ui.spacing().item_spacing.x;
                    dividers[0] = egui::pos2(
                        top.x + columns[0].width + columns[1].width + spacing * 1.5,
                        top.y + 10.0,
                    );
                    dividers[1] = dividers[0] + egui::vec2(columns[2].width + spacing, 0.0);
                    let id = ui.id().with("resize-test");
                    let table = columns.iter().fold(
                        egui_extras::TableBuilder::new(ui)
                            .drag_to_scroll(false)
                            .sense(
                                egui::Sense::click_and_drag()
                                    - egui::Sense::focusable_noninteractive(),
                            ),
                        |table, column| table.column(column.column()),
                    );
                    table
                        .header(24.0, |mut row| {
                            for name in ["#", "customer_alias", "calculation"] {
                                row.col(|ui| {
                                    ui.label(name);
                                });
                            }
                        })
                        .body(|_| {});
                    grid.resize_handles(ui, id, top, &columns, &mut remembered);
                });
            });
            dividers
        };
        let press = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        for (index, delta) in [110.0, 50.0].into_iter().enumerate() {
            let divider = render(Vec::new())[index];
            let next = divider + egui::vec2(delta, 0.0);
            render(vec![
                egui::Event::PointerMoved(divider),
                press(divider, true),
            ]);
            render(vec![egui::Event::PointerMoved(next)]);
            render(vec![press(next, false)]);
        }

        let executable = std::env::current_exe().unwrap();
        let folder = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let path = folder.path().join("preferences.json");
        remembered.save_at(&path).unwrap();
        let mut restored = GridWidths::load_at(&path, true).unwrap();
        assert_eq!(
            restored.saved["work"]["orders"],
            BTreeMap::from([("customer".into(), 255.0)])
        );

        let mut second_grid = GridColumns::default();
        let before = |grid: &mut GridColumns, remembered: &GridWidths| {
            grid.prepare(
                ["calculation", "renamed"],
                [None, Some(origin)].into_iter(),
                (55.0, 44.0),
                remembered,
            )
            .into_iter()
            .map(|column| column.width)
            .collect::<Vec<_>>()
        };
        assert_eq!(before(&mut second_grid, &restored), [55.0, 145.0, 255.0]);
        restored.set_data_sizing(true);
        let value = Cell::Text("x".into());
        let measure = |grid: &mut GridColumns, remembered: &GridWidths, types: [bool; 2]| {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    grid.measure_data(
                        ui,
                        remembered,
                        types.into_iter(),
                        [(0, &value), (1, &value)].into_iter(),
                    );
                });
            });
        };
        measure(&mut second_grid, &restored, [true, true]);
        assert_eq!(before(&mut second_grid, &restored), [55.0, 70.0, 255.0]);
        restored.set_enabled(false);
        measure(&mut second_grid, &restored, [true, true]);
        assert_eq!(before(&mut second_grid, &restored), [55.0, 70.0, 70.0]);
        measure(&mut second_grid, &restored, [true, false]);
        assert_eq!(before(&mut second_grid, &restored), [55.0, 70.0, 145.0]);
        restored.set_data_sizing(false);
        assert_eq!(before(&mut second_grid, &restored), [55.0, 145.0, 145.0]);
    }
}
