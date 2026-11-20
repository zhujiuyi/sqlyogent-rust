use eframe::egui::{self, Event, ImeEvent};

/// Collect text while the writable grid cell owns keyboard focus.
pub(in crate::app) fn take_grid_text(ui: &mut egui::Ui) -> Option<String> {
    if !ui.is_enabled() {
        return None;
    }
    ui.input_mut(|input| {
        if !input.focused {
            return None;
        }
        let mut value = String::new();
        let mut key_modifiers = None;
        let frame_modifiers = input.modifiers;
        input.events.retain(|event| {
            let text = match event {
                Event::Key { modifiers, .. } => {
                    key_modifiers = Some(*modifiers);
                    return true;
                }
                Event::Text(text) => {
                    let modifiers = key_modifiers.take().unwrap_or(frame_modifiers);
                    if modifiers.ctrl || modifiers.command || modifiers.mac_cmd {
                        return true;
                    }
                    text
                }
                Event::Ime(ImeEvent::Commit(text)) => text,
                _ => return true,
            };
            if text.is_empty() || text.chars().any(char::is_control) {
                return true;
            }
            value.push_str(text);
            false
        });
        (!value.is_empty()).then_some(value)
    })
}

/// Keep the native IME enabled where the focused grid cell is visible.
pub(in crate::app) fn request_grid_ime(ui: &egui::Ui, cell_rect: egui::Rect) {
    if !ui.is_enabled() || !ui.input(|input| input.focused) {
        return;
    }
    let rect = cell_rect.intersect(ui.clip_rect());
    if !rect.is_positive() || !rect.is_finite() {
        return;
    }
    let cursor_rect =
        egui::Rect::from_min_size(rect.min, egui::vec2(rect.width().min(1.0), rect.height()));
    let to_global = ui
        .ctx()
        .layer_transform_to_global(ui.layer_id())
        .unwrap_or_default();
    ui.ctx().output_mut(|output| {
        output.ime.get_or_insert(egui::output::IMEOutput {
            rect: to_global * rect,
            cursor_rect: to_global * cursor_rect,
        });
    });
}

pub(in crate::app) fn text_follows_click(ui: &egui::Ui, rect: egui::Rect) -> bool {
    let global = ui
        .ctx()
        .layer_transform_to_global(ui.layer_id())
        .unwrap_or_default()
        * rect;
    ui.input(|input| {
        input.events.iter().rposition(|event| matches!(event,
            Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, .. }
                if global.contains(*pos)
        )).is_some_and(|click| {
            !input.events[..click].iter().any(|event| matches!(event,
                Event::Text(text) | Event::Ime(ImeEvent::Commit(text)) if !text.is_empty()
            ))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_typed_and_committed_text_and_anchors_ime_at_the_cell() {
        let ctx = egui::Context::default();
        let cell = egui::Rect::from_min_size(egui::pos2(60.0, 60.0), egui::vec2(90.0, 20.0));
        let key = Event::Key {
            key: egui::Key::E,
            physical_key: Some(egui::Key::E),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::ALT,
        };
        let preedit = Event::Ime(ImeEvent::Preedit("ni".into()));
        let pasted = Event::Paste("pasted data".into());
        let mut entered = None;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(640.0, 480.0),
                )),
                events: vec![
                    key.clone(),
                    Event::Text(" € ".into()),
                    Event::Ime(ImeEvent::Enabled),
                    preedit.clone(),
                    Event::Ime(ImeEvent::Commit("你".into())),
                    Event::Copy,
                    Event::Cut,
                    pasted.clone(),
                ],
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    if let Some(text) = take_grid_text(ui) {
                        entered = Some(text);
                        ui.input(|input| {
                            assert_eq!(
                                input.events,
                                [
                                    key.clone(),
                                    Event::Ime(ImeEvent::Enabled),
                                    preedit.clone(),
                                    Event::Copy,
                                    Event::Cut,
                                    pasted.clone()
                                ]
                            );
                        });
                    }
                    request_grid_ime(ui, cell);
                });
            },
        );
        assert_eq!(entered.as_deref(), Some(" € 你"));
        let ime = output.platform_output.ime.unwrap();
        assert_eq!(ime.rect, cell);
        assert_eq!(ime.cursor_rect.min, cell.min);
    }
}
