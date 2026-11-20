use std::ops::Range;

use eframe::egui::{self, Id, Key, Modifiers, RichText, TextEdit};
use unicode_width::UnicodeWidthStr;

use crate::database::Cell;

use super::super::text_search::{SearchOptions, TextSearch};
use super::super::{byte_index, style};
use super::display_cell;
use super::values::{ClipboardFormat, clipboard_table};

#[derive(Debug, Eq, PartialEq)]
struct Hit {
    row: usize,
    range: Range<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Caret {
    row: usize,
    byte: usize,
}

impl Hit {
    fn edge(&self, forward: bool) -> Caret {
        Caret {
            row: self.row,
            byte: if forward {
                self.range.end
            } else {
                self.range.start
            },
        }
    }
}

#[derive(Default)]
pub(in crate::app) struct TextRows {
    find_open: bool,
    focus_find: bool,
    needle: String,
    options: SearchOptions,
    find_up: bool,
    caret: Option<Caret>,
    hit: Option<Hit>,
    restart: bool,
    next_requested: bool,
    wrap_pending: Option<bool>,
    focus_ids: Vec<Id>,
    widths: Vec<usize>,
    measured_rows: usize,
    selected_all: bool,
}

impl TextRows {
    pub(in crate::app) fn request_find(&mut self, next: bool) {
        self.find_open = true;
        self.focus_find = true;
        self.selected_all = false;
        self.next_requested |= next;
    }

    pub(in crate::app) fn reset_match(&mut self) {
        self.widths.clear();
        self.caret = None;
        self.hit = None;
        self.restart = true;
        self.wrap_pending = None;
        self.selected_all = false;
    }

    pub(super) fn has_focus(&self, ctx: &egui::Context) -> bool {
        ctx.memory(|memory| self.focus_ids.iter().any(|&id| memory.has_focus(id)))
    }

    pub(in crate::app) fn show<'a>(
        &mut self,
        ui: &mut egui::Ui,
        row_count: usize,
        column_count: usize,
        name: impl Fn(usize) -> &'a str,
        row: impl Fn(usize) -> &'a [Cell],
        copy_headers: bool,
        keyboard_allowed: bool,
        request_focus: bool,
    ) {
        let headings = (0..column_count)
            .map(|column| {
                name(column)
                    .replace(['\r', '\n'], " ↵ ")
                    .replace('\t', "\\t")
            })
            .collect::<Vec<_>>();
        if self.widths.len() != column_count || self.measured_rows != row_count {
            self.reset_match();
            self.widths = column_widths(&headings, row_count, &row);
            self.measured_rows = row_count;
        }
        let interactive =
            keyboard_allowed && ui.is_enabled() && !egui::Popup::is_any_open(ui.ctx());
        let body_id = ui.make_persistent_id("text-result-keyboard");
        let find_id = ui.make_persistent_id("text-result-find");
        let anchor = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(1.0, 1.0));
        let body = ui.interact(anchor, body_id, egui::Sense::focusable_noninteractive());
        if interactive && request_focus {
            body.request_focus();
        }
        let body_allowed =
            interactive && (request_focus || ui.memory(|memory| memory.has_focus(body_id)));
        if body_allowed && ui.input_mut(|input| input.consume_key(Modifiers::COMMAND, Key::A)) {
            self.selected_all = true;
        }
        let copy_aligned = body_allowed
            && self.selected_all
            && ui.input_mut(|input| {
                let copy_key = input.consume_key(Modifiers::COMMAND, Key::C);
                let events = input.events.len();
                input
                    .events
                    .retain(|event| !matches!(event, egui::Event::Copy));
                copy_key || input.events.len() < events
            });
        let keyboard_allowed = interactive && (request_focus || self.has_focus(ui.ctx()));
        let escape = interactive
            && self.find_open
            && (keyboard_allowed
                || ui.memory(|memory| {
                    memory.focused().is_none()
                        && self
                            .focus_ids
                            .iter()
                            .any(|&id| memory.had_focus_last_frame(id))
                }))
            && ui.input_mut(|input| consume_exact_key(input, Modifiers::NONE, Key::Escape));
        let mut step = std::mem::take(&mut self.next_requested).then_some(!self.find_up);
        if keyboard_allowed {
            let (open, previous) = ui.input_mut(|input| {
                let find = consume_exact_key(input, Modifiers::COMMAND, Key::F);
                let find_again = consume_exact_key(input, Modifiers::NONE, Key::F3);
                let previous = consume_exact_key(input, Modifiers::SHIFT, Key::F3);
                (find || find_again, previous)
            });
            if open || (previous && !self.find_open) {
                self.request_find(false);
            }
            if previous {
                step = Some(false);
            }
        }
        let (copy, find) = ui
            .horizontal(|ui| (ui.button("复制可见行"), ui.button("查找")))
            .inner;
        if find.clicked() {
            self.request_find(false);
        }
        self.focus_ids.clear();
        self.focus_ids.extend([body_id, copy.id, find.id]);
        let copy_tsv = || {
            clipboard_table(
                0..row_count,
                0..column_count,
                &name,
                |line, column| row(line).get(column),
                copy_headers,
                ClipboardFormat::Tsv,
            )
        };
        if copy.clicked() {
            ui.ctx().copy_text(copy_tsv());
        }

        let widths = &self.widths;
        let heading = aligned_line(widths, &headings);
        let underline = widths
            .iter()
            .map(|&width| "-".repeat(width))
            .collect::<Vec<_>>()
            .join("  ");
        let render_line = |index| match index {
            0 => heading.clone(),
            1 => underline.clone(),
            _ => {
                let cells = row(index - 2);
                aligned_line(
                    widths,
                    (0..column_count)
                        .map(|column| preview(cells.get(column).unwrap_or(&Cell::Null))),
                )
            }
        };
        if copy_aligned {
            let mut text = String::new();
            let complete = (0..row_count + 2).all(|index| {
                let line = render_line(index);
                if text.try_reserve(line.len() + 1).is_err() {
                    return false;
                }
                if index != 0 {
                    text.push('\n');
                }
                text.push_str(&line);
                true
            });
            ui.ctx().copy_text(if complete { text } else { copy_tsv() });
        }
        let mut reveal = false;
        let all_rows = 0..row_count + 2;
        let locate = |search: &TextSearch, rows, origin, forward| {
            find_match(rows, &render_line, search, origin, forward)
        };
        if self.find_open {
            ui.horizontal_wrapped(|ui| {
                let input = ui.add(
                    TextEdit::singleline(&mut self.needle)
                        .id(find_id)
                        .desired_width(160.0)
                        .hint_text("查找文字")
                        .return_key(None),
                );
                if std::mem::take(&mut self.focus_find) {
                    input.request_focus();
                }
                let match_case = ui.checkbox(&mut self.options.match_case, "区分大小写");
                let whole_word = ui.checkbox(&mut self.options.whole_word, "全字匹配");
                let (up, down) = ui
                    .horizontal(|ui| {
                        ui.label("方向");
                        (
                            ui.radio_value(&mut self.find_up, true, "向上"),
                            ui.radio_value(&mut self.find_up, false, "向下"),
                        )
                    })
                    .inner;
                if input.changed() || match_case.changed() || whole_word.changed() {
                    self.hit = None;
                    self.restart = true;
                    self.wrap_pending = None;
                } else if up.changed() || down.changed() {
                    self.wrap_pending = None;
                }
                let has_text = !self.needle.is_empty();
                let can_step = has_text && self.wrap_pending.is_none();
                let previous = ui.add_enabled(can_step, egui::Button::new("上一处"));
                let next = ui.add_enabled(can_step, egui::Button::new("下一处"));
                if previous.clicked() {
                    step = Some(false);
                }
                if next.clicked() {
                    step = Some(true);
                }
                if keyboard_allowed && input.has_focus() && can_step {
                    if ui.input_mut(|input| consume_exact_key(input, Modifiers::SHIFT, Key::Enter))
                    {
                        step = Some(false);
                    } else if ui
                        .input_mut(|input| consume_exact_key(input, Modifiers::NONE, Key::Enter))
                    {
                        step = Some(!self.find_up);
                    }
                }
                let close = ui.small_button("收起");
                self.focus_ids.extend([
                    input.id,
                    match_case.id,
                    whole_word.id,
                    up.id,
                    down.id,
                    previous.id,
                    next.id,
                    close.id,
                ]);
                if close.clicked() || (escape && self.wrap_pending.is_none()) {
                    self.find_open = false;
                    self.wrap_pending = None;
                    body.request_focus();
                } else if escape {
                    self.wrap_pending = None;
                    step = None;
                    input.request_focus();
                }
                if self.find_open {
                    let restart = std::mem::take(&mut self.restart);
                    if self.wrap_pending.is_none()
                        && (restart || step.is_some())
                        && let Some(search) = TextSearch::new(&self.needle, self.options)
                    {
                        let forward = step.unwrap_or(!self.find_up);
                        let origin = self
                            .hit
                            .as_ref()
                            .map(|hit| hit.edge(forward))
                            .or(self.caret);
                        if let Some(hit) = locate(&search, all_rows.clone(), origin, forward) {
                            self.hit = Some(hit);
                            reveal = true;
                        } else if self.hit.is_some()
                            || origin.is_some_and(|caret| {
                                // Check the full clicked row too, since a match may span the caret.
                                let wrapped = if forward {
                                    0..caret.row + 1
                                } else {
                                    caret.row..all_rows.end
                                };
                                locate(&search, wrapped, None, forward).is_some()
                            })
                        {
                            self.wrap_pending = Some(forward);
                        }
                    }
                    if has_text && self.hit.is_none() && self.wrap_pending.is_none() {
                        ui.label("未找到");
                    }
                }
            });
        }
        if let Some(forward) = self.wrap_pending {
            ui.horizontal_wrapped(|ui| {
                ui.label(if forward {
                    "已到达末尾，是否从开头继续查找？"
                } else {
                    "已到达开头，是否从末尾继续查找？"
                });
                let proceed = ui.button("继续");
                let cancel = ui.button("取消");
                self.focus_ids.extend([proceed.id, cancel.id]);
                if proceed.clicked() {
                    self.hit = TextSearch::new(&self.needle, self.options)
                        .and_then(|search| locate(&search, all_rows.clone(), None, forward));
                    reveal = self.hit.is_some();
                }
                if proceed.clicked() || cancel.clicked() {
                    self.wrap_pending = None;
                    ui.memory_mut(|memory| memory.request_focus(find_id));
                }
            });
        }

        let highlight = |index, line: &str| {
            if self.selected_all {
                Some(0..line.len())
            } else {
                self.hit
                    .as_ref()
                    .filter(|hit| self.find_open && index == hit.row)
                    .map(|hit| hit.range.clone())
            }
        };
        let mut columns = egui::ScrollArea::horizontal()
            .id_salt("text-result-columns")
            .auto_shrink([false, false]);
        if reveal && let Some(hit) = &self.hit {
            let line = render_line(hit.row);
            if let Some(prefix) = line.get(..hit.range.start) {
                let font = style::grid_font(ui)
                    .unwrap_or_else(|| egui::TextStyle::Monospace.resolve(ui.style()));
                let x = ui.fonts_mut(|fonts| {
                    fonts
                        .layout_no_wrap(prefix.to_owned(), font, egui::Color32::WHITE)
                        .size()
                        .x
                });
                columns =
                    columns.horizontal_scroll_offset((x - 0.3 * ui.available_width()).max(0.0));
            }
        }
        let mut clicked = None;
        let text = columns.show(ui, |ui| {
            let row_height = style::grid_row_height(ui, 19.0);
            style::apply_grid_font(ui);
            let mut draw_line = |ui: &mut egui::Ui, index, line: &str| {
                if let Some(byte) = row_label(ui, line, highlight(index, line)) {
                    clicked = Some(Caret { row: index, byte });
                }
            };
            draw_line(ui, 0, &heading);
            draw_line(ui, 1, &underline);
            let mut body = egui::ScrollArea::vertical()
                .id_salt("result-text-body")
                .auto_shrink([false, false]);
            if reveal && let Some(hit) = &self.hit {
                let y =
                    hit.row.saturating_sub(2) as f32 * (row_height + ui.spacing().item_spacing.y);
                body = body.vertical_scroll_offset((y - 0.4 * ui.available_height()).max(0.0));
            }
            body.show_rows(ui, row_height, row_count, |ui, range| {
                for index in range {
                    let line = index + 2;
                    let text = render_line(line);
                    draw_line(ui, line, &text);
                }
            });
        });
        if interactive && let Some(caret) = clicked {
            self.caret = Some(caret);
            self.hit = None;
            self.wrap_pending = None;
        }
        // A label click can surrender the separate keyboard anchor on release.
        if interactive
            && ui.input(|input| {
                (input.pointer.primary_pressed() || input.pointer.primary_clicked())
                    && input
                        .pointer
                        .interact_pos()
                        .is_some_and(|pos| text.inner_rect.contains(pos))
            })
        {
            self.selected_all = false;
            body.request_focus();
        }
    }
}

fn consume_exact_key(input: &mut egui::InputState, modifiers: Modifiers, key: Key) -> bool {
    let count = input.events.len();
    input.events.retain(|event| {
        !matches!(event, egui::Event::Key { key: pressed_key, pressed: true, modifiers: pressed_modifiers, .. }
            if *pressed_key == key && pressed_modifiers.matches_exact(modifiers))
    });
    input.events.len() != count
}

fn preview(cell: &Cell) -> String {
    match cell {
        Cell::Null => "(NULL)".into(),
        cell => display_cell(cell, true).replace('\t', "\\t"),
    }
}

fn column_widths<'a>(
    headings: &[String],
    count: usize,
    row: &impl Fn(usize) -> &'a [Cell],
) -> Vec<usize> {
    let mut widths = headings
        .iter()
        .map(|heading| heading.as_str().width().max(1))
        .collect::<Vec<_>>();
    for index in 0..count {
        let cells = row(index);
        for (column, width) in widths.iter_mut().enumerate() {
            let value = preview(cells.get(column).unwrap_or(&Cell::Null));
            *width = (*width).max(value.as_str().width());
        }
    }
    widths
}

fn aligned_line(widths: &[usize], fields: impl IntoIterator<Item = impl AsRef<str>>) -> String {
    let mut line = String::new();
    for (column, (field, &width)) in fields.into_iter().zip(widths).enumerate() {
        let field = field.as_ref();
        line.push_str(field);
        if column + 1 < widths.len() {
            line.extend(std::iter::repeat_n(
                ' ',
                width.saturating_sub(field.width()) + 2,
            ));
        }
    }
    line
}

fn find_match(
    rows: Range<usize>,
    render_row: &impl Fn(usize) -> String,
    search: &TextSearch,
    origin: Option<Caret>,
    forward: bool,
) -> Option<Hit> {
    if rows.is_empty() {
        return None;
    }
    let origin = origin.filter(|caret| rows.contains(&caret.row));
    let start_row = origin.map_or(if forward { rows.start } else { rows.end - 1 }, |caret| {
        caret.row
    });
    let locate = |text: &str, range| search.find_in(text, range, forward);
    let first = render_row(start_row);
    let partial = origin
        .filter(|caret| first.is_char_boundary(caret.byte))
        .map_or(0..first.len(), |caret| {
            if forward {
                caret.byte..first.len()
            } else {
                0..caret.byte
            }
        });
    if let Some(range) = locate(&first, partial) {
        return Some(Hit {
            row: start_row,
            range,
        });
    }
    let locate_row = |row| {
        let line = render_row(row);
        locate(&line, 0..line.len()).map(|range| Hit { row, range })
    };
    if forward {
        (start_row + 1..rows.end).find_map(locate_row)
    } else {
        (rows.start..start_row).rev().find_map(locate_row)
    }
}

fn row_label(ui: &mut egui::Ui, line: &str, highlight: Option<Range<usize>>) -> Option<usize> {
    let label = if let Some(range) = highlight
        && let Some(matching) = line.get(range.clone())
    {
        let normal = egui::TextFormat::simple(
            style::grid_font(ui).unwrap_or_else(|| egui::TextStyle::Monospace.resolve(ui.style())),
            ui.visuals().text_color(),
        );
        let selected = egui::TextFormat {
            background: ui.visuals().selection.bg_fill,
            color: ui.visuals().selection.stroke.color,
            ..normal.clone()
        };
        let mut job = egui::text::LayoutJob::default();
        job.append(&line[..range.start], 0.0, normal.clone());
        job.append(matching, 0.0, selected);
        job.append(&line[range.end..], 0.0, normal);
        egui::Label::new(job)
    } else {
        egui::Label::new(RichText::new(line).monospace())
    };
    let (origin, galley, response) = label.extend().selectable(true).layout_in_ui(ui);
    response
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, ui.is_enabled(), line));
    if !ui.is_rect_visible(response.rect) {
        return None;
    }
    let caret = response
        .interact_pointer_pos()
        .filter(|&pos| {
            response.clicked_by(egui::PointerButton::Primary)
                && response.interact_rect.contains(pos)
        })
        .map(|pos| byte_index(galley.text(), galley.cursor_from_pos(pos - origin).index));
    let color = ui.visuals().text_color();
    let underline = if response.has_focus() || response.highlighted() {
        egui::Stroke::new(1.0_f32, color)
    } else {
        egui::Stroke::NONE
    };
    egui::text_selection::LabelSelectionState::label_text_selection(
        ui, &response, origin, galley, color, underline,
    );
    caret
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_table_aligns_wide_text_and_preserves_display_formats() {
        let headings = ["名".into(), "内容".into(), "值".into()];
        let rows = [
            [
                Cell::Text("中文".into()),
                Cell::Null,
                Cell::Binary(vec![0, 255]),
            ],
            [
                Cell::Text("A".into()),
                Cell::Text("含\t制表".into()),
                Cell::Null,
            ],
        ];
        let widths = column_widths(&headings, rows.len(), &|row| rows[row].as_slice());
        assert_eq!(widths, [4, 8, 6]);
        assert_eq!(
            aligned_line(&widths, rows[0].iter().map(preview)),
            "中文  (NULL)    0x00ff"
        );
        assert_eq!(
            aligned_line(&widths, rows[1].iter().map(preview)),
            "A     含\\t制表  (NULL)"
        );
    }

    #[test]
    fn text_find_steps_through_unicode_occurrences_in_display_order() {
        let rendered = ["甲\tNULL", "乙\t甲和甲", "0x00ff"];
        let row = |index: usize| rendered[index].to_owned();
        let positions = [
            Hit {
                row: 0,
                range: 0.."甲".len(),
            },
            Hit {
                row: 1,
                range: "乙\t".len().."乙\t甲".len(),
            },
            Hit {
                row: 1,
                range: "乙\t甲和".len().."乙\t甲和甲".len(),
            },
        ];
        let search = TextSearch::new("甲", SearchOptions::default()).unwrap();
        let step = |current: Option<&Hit>, forward| {
            find_match(
                0..rendered.len(),
                &row,
                &search,
                current.map(|hit| hit.edge(forward)),
                forward,
            )
        };
        let first = step(None, true);
        assert_eq!(first.as_ref(), Some(&positions[0]));
        let second = step(first.as_ref(), true);
        assert_eq!(second.as_ref(), Some(&positions[1]));
        let third = step(second.as_ref(), true);
        assert_eq!(third.as_ref(), Some(&positions[2]));
        assert_eq!(step(third.as_ref(), true), None);
        assert_eq!(step(first.as_ref(), false), None);
        assert_eq!(step(None, false), third);
        assert_eq!(step(third.as_ref(), false), second);
        let absent = TextSearch::new("丙", SearchOptions::default()).unwrap();
        assert_eq!(
            find_match(0..rendered.len(), &row, &absent, None, true),
            None
        );
    }

    #[test]
    fn text_find_uses_the_clicked_character_and_wraps_in_the_same_row() {
        let ctx = egui::Context::default();
        let mut view = TextRows::default();
        let line = "甲中甲尾";
        let data = [[Cell::Text("另一行".into())], [Cell::Text(line.into())]];
        let frame = |view: &mut TextRows, events| {
            ctx.run(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        view.show(
                            ui,
                            data.len(),
                            1,
                            |_| "字段",
                            |row| &data[row],
                            false,
                            true,
                            false,
                        );
                    });
                },
            )
        };
        let position = |output: &egui::FullOutput, text, character| {
            output
                .shapes
                .iter()
                .find_map(|clipped| match &clipped.shape {
                    egui::Shape::Text(shape) if shape.galley.text() == text => {
                        let cursor = shape
                            .galley
                            .pos_from_cursor(egui::text::CCursor::new(character));
                        Some(shape.pos + cursor.center().to_vec2())
                    }
                    _ => None,
                })
                .unwrap()
        };
        let click = |view: &mut TextRows, pos| {
            let event = |pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: Modifiers::NONE,
            };
            frame(view, vec![egui::Event::PointerMoved(pos), event(true)]);
            frame(view, vec![event(false)])
        };

        let output = frame(&mut view, vec![]);
        click(&mut view, position(&output, line, 2));
        assert_eq!(
            view.caret,
            Some(Caret {
                row: 3,
                byte: "甲中".len()
            })
        );
        click(&mut view, position(&output, "查找", 1));
        let output = frame(&mut view, vec![egui::Event::Text("甲".into())]);
        let later = Hit {
            row: 3,
            range: "甲中".len().."甲中甲".len(),
        };
        assert_eq!(view.hit.as_ref(), Some(&later));

        let output = click(&mut view, position(&output, line, 3));
        let output = click(&mut view, position(&output, "下一处", 1));
        assert!(view.hit.is_none());
        assert_eq!(view.wrap_pending, Some(true));
        click(&mut view, position(&output, "继续", 1));
        assert_eq!(
            view.hit,
            Some(Hit {
                row: 3,
                range: 0.."甲".len()
            })
        );
        let output = frame(&mut view, vec![]);
        click(&mut view, position(&output, "下一处", 1));
        assert_eq!(view.hit, Some(later));
    }

    #[test]
    fn selecting_all_and_copying_keeps_aligned_text() {
        let ctx = egui::Context::default();
        let mut view = TextRows::default();
        let data = [Cell::Text("值".into()), Cell::Null];
        let output = ctx.run(
            egui::RawInput {
                events: vec![
                    egui::Event::Key {
                        key: Key::A,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: Modifiers::COMMAND,
                    },
                    egui::Event::Copy,
                ],
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    view.show(
                        ui,
                        1,
                        2,
                        |column| ["甲", "乙"][column],
                        |_| &data,
                        false,
                        true,
                        true,
                    );
                });
            },
        );
        assert!(
            output
                .platform_output
                .commands
                .iter()
                .any(|command| matches!(command,
                    egui::OutputCommand::CopyText(text) if text == "甲  乙\n--  ------\n值  (NULL)"
                ))
        );
    }
}
