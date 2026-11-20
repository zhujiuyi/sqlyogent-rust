use eframe::egui::{self, Rect};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum Arrangement {
    #[default]
    Maximized,
    Cascade,
    Tile,
}

#[derive(Default)]
pub(super) struct WorkspaceWindows {
    pub mode: Arrangement,
    pub minimized: BTreeSet<u64>,
    pub revision: u64,
}

impl WorkspaceWindows {
    pub(super) fn arrange(&mut self, mode: Arrangement) {
        self.mode = mode;
        self.revision = self.revision.wrapping_add(1);
    }

    pub(super) fn minimize(&mut self, key: u64) {
        self.minimized.insert(key);
        if self.mode == Arrangement::Maximized {
            self.arrange(Arrangement::Cascade);
        }
    }

    pub(super) fn restore(&mut self, key: u64) {
        self.minimized.remove(&key);
    }
}

pub(super) enum PanelHost<'a> {
    Root(&'a egui::Context),
    Local(&'a mut egui::Ui),
}

impl PanelHost<'_> {
    pub(super) fn width(&self) -> f32 {
        match self {
            Self::Root(ctx) => ctx.available_rect().width(),
            Self::Local(ui) => ui.available_width(),
        }
    }

    pub(super) fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }

    pub(super) fn bottom<R>(
        &mut self,
        panel: egui::TopBottomPanel,
        content: impl FnOnce(&mut egui::Ui) -> R,
    ) -> egui::InnerResponse<R> {
        match self {
            Self::Root(ctx) => panel.show(ctx, content),
            Self::Local(ui) => panel.show_inside(ui, content),
        }
    }

    pub(super) fn side<R>(
        &mut self,
        panel: egui::SidePanel,
        content: impl FnOnce(&mut egui::Ui) -> R,
    ) -> egui::InnerResponse<R> {
        match self {
            Self::Root(ctx) => panel.show(ctx, content),
            Self::Local(ui) => panel.show_inside(ui, content),
        }
    }

    pub(super) fn central<R>(
        &mut self,
        panel: egui::CentralPanel,
        content: impl FnOnce(&mut egui::Ui) -> R,
    ) -> egui::InnerResponse<R> {
        match self {
            Self::Root(ctx) => panel.show(ctx, content),
            Self::Local(ui) => panel.show_inside(ui, content),
        }
    }
}

pub(super) fn tile_rectangles(available: Rect, count: usize) -> Vec<Rect> {
    if count == 0 || !available.is_positive() {
        return Vec::new();
    }

    const TARGET_TILE_WIDTH: f32 = 360.0;
    let max_columns = ((available.width() / TARGET_TILE_WIDTH) as usize).max(1);
    let rows = count.div_ceil(max_columns);
    let mut rectangles = Vec::with_capacity(count);
    for row in 0..rows {
        let columns = count / rows + usize::from(row < count % rows);
        let (top, bottom) = tile_span(available.top(), available.bottom(), rows, row);
        for column in 0..columns {
            let (left, right) = tile_span(available.left(), available.right(), columns, column);
            rectangles.push(Rect::from_min_max(
                egui::pos2(left, top),
                egui::pos2(right, bottom),
            ));
        }
    }
    rectangles
}

fn tile_span(start: f32, end: f32, count: usize, index: usize) -> (f32, f32) {
    const GAP: f32 = 6.0;
    let length = end - start;
    let gap = GAP.min(length / count as f32 * 0.1);
    let step = (length + gap) / count as f32;
    let first = start + step * index as f32;
    let last = if index + 1 == count {
        end
    } else {
        start + step * (index + 1) as f32 - gap
    };
    (first, last)
}

pub(super) fn cascade_rectangles(available: Rect, count: usize) -> Vec<Rect> {
    if count == 0 || !available.is_positive() {
        return Vec::new();
    }

    const CASCADE_STEP: f32 = 26.0;
    let last = count - 1;
    let max_offset = available.width().min(available.height()) * 0.25;
    let step = CASCADE_STEP.min(max_offset / last.max(1) as f32);
    (0..count)
        .map(|index| {
            Rect::from_min_max(
                available.min + egui::Vec2::splat(step * index as f32),
                available.max - egui::Vec2::splat(step * (last - index) as f32),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiles_fill_the_workspace_in_rows_of_vertical_panels() {
        let available = Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(800.0, 500.0));
        let tiles = tile_rectangles(available, 4);

        assert_eq!(tiles.len(), 4);
        assert_eq!(tiles[0].min, available.min);
        assert_eq!(tiles[3].max, available.max);
        assert_eq!(tiles[0].top(), tiles[1].top());
        assert_eq!(tiles[0].left(), tiles[2].left());
        assert!(tiles[0].right() < tiles[1].left());
        assert!(tiles[0].bottom() < tiles[2].top());
        assert!(tiles.iter().all(|tile| available.contains_rect(*tile)));
    }

    #[test]
    fn cascaded_windows_overlap_and_stay_inside_the_workspace() {
        let available = Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(800.0, 500.0));
        let windows = cascade_rectangles(available, 3);

        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].min, available.min);
        assert_eq!(windows[2].max, available.max);
        assert!(
            windows
                .iter()
                .all(|window| available.contains_rect(*window))
        );
        for pair in windows.windows(2) {
            assert_eq!(pair[0].size(), pair[1].size());
            assert!(pair[0].intersects(pair[1]));
            assert!(pair[0].left() < pair[1].left());
            assert!(pair[0].top() < pair[1].top());
        }
    }
}
