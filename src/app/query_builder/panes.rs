use eframe::egui::{self, CursorIcon, Id, PointerButton, Rect, Sense, vec2};

use crate::app::style;

pub(super) const DIVIDER_HEIGHT: f32 = 10.0;
const MINIMUM_HEIGHTS: [f32; 3] = [130.0, 80.0, 60.0];
// Keep some room to resize in short windows. The outer scroll area can reveal the rest.
const MINIMUM_EXTRA_HEIGHT: f32 = 64.0;

#[derive(Clone, Copy, Hash)]
pub(super) enum Divider {
    Canvas,
    Fields,
}

impl Divider {
    pub(super) fn id(self, id: Id) -> Id {
        id.with(self)
    }

    fn index_and_hint(self) -> (usize, &'static str) {
        match self {
            Self::Canvas => (0, "拖动调整画布和设置区的高度"),
            Self::Fields => (1, "拖动调整设置区和 SQL 预览的高度"),
        }
    }
}

pub(super) struct PaneSizes {
    shares: [f32; 3],
}

impl Default for PaneSizes {
    fn default() -> Self {
        Self {
            shares: [0.52, 0.28, 0.20],
        }
    }
}

impl PaneSizes {
    fn extra_height(available: f32) -> f32 {
        (available - MINIMUM_HEIGHTS.iter().sum::<f32>()).max(MINIMUM_EXTRA_HEIGHT)
    }

    pub(super) fn heights(&self, available: f32) -> [f32; 3] {
        let extra = Self::extra_height(available);
        std::array::from_fn(|index| MINIMUM_HEIGHTS[index] + self.shares[index] * extra)
    }

    pub(super) fn splitter(&mut self, ui: &mut egui::Ui, id: Id, divider: Divider, available: f32) {
        let (index, hint) = divider.index_and_hint();
        let (_, rect) = ui.allocate_space(vec2(ui.available_width(), DIVIDER_HEIGHT));
        let response = ui
            .interact(rect, divider.id(id), Sense::drag())
            .on_hover_and_drag_cursor(CursorIcon::ResizeVertical)
            .on_hover_text(hint);
        let center = rect.center();
        let active = response.hovered() || response.dragged();
        if active {
            ui.painter().rect_filled(rect, 4, style::ACCENT_SOFT);
        }
        ui.painter().rect_filled(
            Rect::from_center_size(center, vec2(28.0, 3.0)),
            2,
            if active { style::ACCENT } else { style::MUTED },
        );

        let grab_id = response.id.with("grab-offset");
        if response.drag_started_by(PointerButton::Primary) {
            let offset = ui.input(|input| {
                input
                    .pointer
                    .press_origin()
                    .map_or(0.0, |point| point.y - center.y)
            });
            ui.data_mut(|data| data.insert_temp(grab_id, offset));
        }
        let released = response.drag_stopped_by(PointerButton::Primary);
        if (response.dragged_by(PointerButton::Primary) || released)
            && let Some(pointer) = response.interact_pointer_pos()
        {
            let delta = pointer.y
                - center.y
                - ui.data(|data| data.get_temp::<f32>(grab_id).unwrap_or_default());
            let total = self.shares[index] + self.shares[index + 1];
            let change = delta / Self::extra_height(available);
            let above = (self.shares[index] + change).clamp(0.0, total);
            if above != self.shares[index] {
                self.shares[index] = above;
                self.shares[index + 1] = total - above;
                ui.ctx().request_repaint();
            }
        }
        if released {
            ui.data_mut(|data| data.remove::<f32>(grab_id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{query_builder::Builder, state::Workspace};

    const TALL_WINDOW: f32 = 800.0;

    fn render(
        ctx: &egui::Context,
        builder: &mut Builder,
        workspace: &Workspace,
        tab: u64,
        height: f32,
        events: Vec<egui::Event>,
    ) -> [egui::Pos2; 2] {
        let mut splitter_id = Id::NULL;
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    vec2(1024.0, height),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.push_id(("query-design-tab", tab), |ui| {
                        splitter_id = ui.id().with("builder-splitters");
                        builder.design_view(ui, workspace, true, false);
                    });
                });
            },
        );
        [Divider::Canvas, Divider::Fields].map(|divider| {
            ctx.read_response(divider.id(splitter_id))
                .expect("query design divider")
                .rect
                .center()
        })
    }

    #[test]
    fn dragging_both_splitters_keeps_sizes_in_the_current_query_design() {
        let ctx = egui::Context::default();
        style::apply(&ctx);
        let workspace = Workspace::new("测试连接".into(), None);
        let mut first = Builder::new_blank(None);
        let mut second = Builder::new_blank(None);
        let initial = render(&ctx, &mut first, &workspace, 0, TALL_WINDOW, vec![]);
        let drag = |builder: &mut Builder, from: egui::Pos2, delta: f32| {
            let to = from + vec2(0.0, delta);
            let button = |pos, pressed| egui::Event::PointerButton {
                pos,
                pressed,
                button: PointerButton::Primary,
                modifiers: egui::Modifiers::NONE,
            };
            render(
                &ctx,
                builder,
                &workspace,
                0,
                TALL_WINDOW,
                vec![egui::Event::PointerMoved(from), button(from, true)],
            );
            render(
                &ctx,
                builder,
                &workspace,
                0,
                TALL_WINDOW,
                vec![egui::Event::PointerMoved(to)],
            );
            render(
                &ctx,
                builder,
                &workspace,
                0,
                TALL_WINDOW,
                vec![button(to, false)],
            );
            render(&ctx, builder, &workspace, 0, TALL_WINDOW, vec![])
        };
        let close = |actual: f32, expected: f32| {
            assert!((actual - expected).abs() < 1.0, "{actual} vs {expected}");
        };

        let first_drag = drag(&mut first, initial[0], 38.0);
        close(first_drag[0].y, initial[0].y + 38.0);
        close(first_drag[1].y, initial[1].y);
        let second_drag = drag(&mut first, first_drag[1], 24.0);
        close(second_drag[0].y, first_drag[0].y);
        close(second_drag[1].y, first_drag[1].y + 24.0);

        let other = render(&ctx, &mut second, &workspace, 1, TALL_WINDOW, vec![]);
        close(other[0].y, initial[0].y);
        close(other[1].y, initial[1].y);
        let restored = render(&ctx, &mut first, &workspace, 0, TALL_WINDOW, vec![]);
        close(restored[0].y, second_drag[0].y);
        close(restored[1].y, second_drag[1].y);
        assert!(!first.dirty());

        let short_window = 360.0;
        render(&ctx, &mut first, &workspace, 0, short_window, vec![]);
        let small = render(&ctx, &mut first, &workspace, 0, short_window, vec![]);
        assert!(small[1].y - small[0].y >= MINIMUM_HEIGHTS[1]);
        render(
            &ctx,
            &mut first,
            &workspace,
            0,
            short_window,
            vec![
                egui::Event::PointerMoved(small[0]),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: vec2(0.0, -160.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        render(&ctx, &mut first, &workspace, 0, short_window, vec![]);
        let scrolled = render(&ctx, &mut first, &workspace, 0, short_window, vec![]);
        assert!(scrolled[1].y < small[1].y, "{scrolled:?} vs {small:?}");
    }
}
