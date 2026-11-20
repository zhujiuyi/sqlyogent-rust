use std::ops::Range;

use eframe::egui::{self, Key, Response, Sense};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::app) struct Point {
    pub row: usize,
    pub column: usize,
}

#[derive(Clone, Copy)]
pub(in crate::app) struct Hit {
    pub point: Point,
    pub row_header: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) struct Selection {
    pub head: Point,
    pub whole_rows: bool,
    anchor: Point,
}

impl Selection {
    pub(in crate::app) fn contains(self, point: Point) -> bool {
        self.rows().contains(&point.row)
            && (self.whole_rows || self.columns(0).contains(&point.column))
    }

    pub(in crate::app) fn single_cell(self) -> Option<Point> {
        (!self.whole_rows && self.anchor == self.head).then_some(self.head)
    }

    pub(in crate::app) fn rows(self) -> Range<usize> {
        self.anchor.row.min(self.head.row)..self.anchor.row.max(self.head.row) + 1
    }

    pub(in crate::app) fn columns(self, count: usize) -> Range<usize> {
        if self.whole_rows {
            0..count
        } else {
            self.anchor.column.min(self.head.column)..self.anchor.column.max(self.head.column) + 1
        }
    }
}

#[derive(Default)]
pub(in crate::app) struct PointerInteraction {
    hit: Option<(Hit, bool)>,
    hovered: Option<Hit>,
    secondary: Option<Hit>,
}

impl PointerInteraction {
    pub(in crate::app) fn visit(&mut self, response: &Response, hit: Hit) {
        if response.contains_pointer() {
            self.hovered = Some(hit);
        }
        let drag_started = response.drag_started_by(egui::PointerButton::Primary);
        if drag_started || response.clicked() {
            self.hit = Some((hit, drag_started));
        }
        if response.secondary_clicked() {
            self.secondary = Some(hit);
        }
    }
}

#[derive(Default)]
pub(in crate::app) struct GridSelection {
    current: Option<Selection>,
    drag: bool,
    scroll_to: Option<Point>,
    focus_requested: bool,
}

impl GridSelection {
    pub(in crate::app) fn current(&self) -> Option<Selection> {
        self.current
    }

    pub(in crate::app) fn clear(&mut self) {
        self.current = None;
        self.scroll_to = None;
        self.drag = false;
    }

    pub(in crate::app) fn select(&mut self, hit: Hit, extend: bool) {
        if extend && let Some(selection) = &mut self.current {
            selection.head = hit.point;
            selection.whole_rows |= hit.row_header;
        } else {
            self.current = Some(Selection {
                anchor: hit.point,
                head: hit.point,
                whole_rows: hit.row_header,
            });
        }
    }

    pub(in crate::app) fn select_all(&mut self, rows: usize, columns: usize) {
        if rows > 0 && columns > 0 {
            self.current = Some(Selection {
                anchor: Point::default(),
                head: Point {
                    row: rows - 1,
                    column: columns - 1,
                },
                whole_rows: true,
            });
        }
    }

    pub(in crate::app) fn request_focus(&mut self) {
        self.focus_requested = true;
    }

    pub(in crate::app) fn clear_focus_request(&mut self) {
        self.focus_requested = false;
    }

    pub(in crate::app) fn focus(&mut self, ui: &mut egui::Ui) -> Response {
        let id = ui.make_persistent_id("grid-keyboard");
        let rect = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(1.0, 1.0));
        let focus = ui.interact(rect, id, Sense::focusable_noninteractive());
        if std::mem::take(&mut self.focus_requested) {
            focus.request_focus();
            ui.ctx().request_repaint();
        }
        ui.memory_mut(|memory| {
            memory.set_focus_lock_filter(
                id,
                egui::EventFilter {
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                    ..Default::default()
                },
            )
        });
        focus
    }

    pub(in crate::app) fn apply_pointer(
        &mut self,
        ui: &egui::Ui,
        focus: &Response,
        pointer: PointerInteraction,
    ) {
        let before = self.current;
        let (shift, pressed) =
            ui.input(|input| (input.modifiers.shift, input.pointer.primary_down()));
        if let Some((hit, drag)) = pointer.hit {
            self.select(hit, shift);
            self.drag = drag;
            self.scroll_to = None;
            focus.request_focus();
        } else if let Some(hit) = pointer.secondary {
            if self.current.is_none_or(|selection| {
                !selection.contains(hit.point) || hit.row_header && !selection.whole_rows
            }) {
                self.select(hit, false);
            }
            focus.request_focus();
        }
        if self.drag
            && let Some(mut hit) = pointer.hovered
        {
            hit.row_header = self.current.is_some_and(|selection| selection.whole_rows);
            self.select(hit, true);
        }
        if !pressed {
            self.drag = false;
        }
        if self.current != before {
            ui.ctx().request_repaint();
        }
    }

    pub(in crate::app) fn scroll_target(&self) -> Option<Point> {
        self.scroll_to
    }

    pub(in crate::app) fn finish_scroll(&mut self, ui: &egui::Ui, did_scroll: bool) {
        if did_scroll {
            self.scroll_to = None;
        } else if self.scroll_to.is_some() {
            ui.ctx().request_repaint();
        }
    }

    pub(in crate::app) fn shortcuts(
        &mut self,
        ui: &egui::Ui,
        rows: usize,
        columns: usize,
        page_rows: usize,
        f2_opens_cell: bool,
    ) -> Vec<Shortcut> {
        if rows == 0 || columns == 0 || egui::Popup::is_any_open(ui.ctx()) {
            return Vec::new();
        }
        let commands = ui.input_mut(|input| {
            let mut commands = Vec::new();
            input.events.retain(|event| {
                let Some(action) = Keyboard::from_event(event, f2_opens_cell) else {
                    return true;
                };
                if matches!(action, Keyboard::Clear) && self.current.is_none() {
                    return true;
                }
                commands.push(action);
                false
            });
            commands
        });
        if commands.iter().any(|action| {
            matches!(
                action,
                Keyboard::Move {
                    key: Key::ArrowUp | Key::ArrowDown | Key::ArrowLeft | Key::ArrowRight,
                    ..
                }
            )
        }) {
            // egui can queue focus traversal before a newly focused grid consumes the arrow.
            ui.memory_mut(|memory| memory.move_focus(egui::FocusDirection::None));
        }
        let max = Point {
            row: rows - 1,
            column: columns - 1,
        };
        let mut output = Vec::new();
        for action in commands {
            let head = self
                .current
                .map_or(Point::default(), |selection| selection.head);
            match action {
                Keyboard::Copy => {
                    if let Some(selection) = self.current {
                        output.push(Shortcut::Copy(selection));
                    }
                }
                Keyboard::Open(key) => {
                    output.push(Shortcut::Open(head, key));
                    break;
                }
                Keyboard::SelectAll => self.select_all(rows, columns),
                Keyboard::SelectRow => self.select(
                    Hit {
                        point: head,
                        row_header: true,
                    },
                    false,
                ),
                Keyboard::Clear => self.clear(),
                Keyboard::Move { key, extend, edge } => {
                    let point = if self.current.is_none() {
                        Point::default()
                    } else {
                        Keyboard::destination(key, head, max, page_rows, edge)
                    };
                    self.select(
                        Hit {
                            point,
                            row_header: false,
                        },
                        extend,
                    );
                    self.scroll_to = Some(point);
                }
            }
        }
        output
    }
}

pub(in crate::app) enum Shortcut {
    Copy(Selection),
    Open(Point, Key),
}

enum Keyboard {
    Copy,
    Open(Key),
    SelectAll,
    SelectRow,
    Clear,
    Move { key: Key, extend: bool, edge: bool },
}

impl Keyboard {
    fn from_event(event: &egui::Event, f2_opens_cell: bool) -> Option<Self> {
        if matches!(event, egui::Event::Copy) {
            return Some(Self::Copy);
        }
        let egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } = *event
        else {
            return None;
        };
        if modifiers.alt {
            return None;
        }
        let command = modifiers.command || modifiers.ctrl;
        match key {
            Key::C | Key::Insert if command && !modifiers.shift => Some(Self::Copy),
            Key::A if command && !modifiers.shift => Some(Self::SelectAll),
            Key::Space if command || modifiers.shift => Some(Self::SelectRow),
            Key::Enter | Key::Space if !command && !modifiers.shift => Some(Self::Open(key)),
            Key::F2 if f2_opens_cell && !command && !modifiers.shift => Some(Self::Open(key)),
            Key::Escape if !command => Some(Self::Clear),
            Key::ArrowUp
            | Key::ArrowDown
            | Key::ArrowLeft
            | Key::ArrowRight
            | Key::PageUp
            | Key::PageDown
            | Key::Home
            | Key::End => Some(Self::Move {
                key,
                extend: modifiers.shift,
                edge: command,
            }),
            _ => None,
        }
    }

    fn destination(key: Key, point: Point, max: Point, page: usize, edge: bool) -> Point {
        let Point {
            mut row,
            mut column,
        } = point;
        match key {
            Key::ArrowUp => row = if edge { 0 } else { row.saturating_sub(1) },
            Key::ArrowDown => {
                row = if edge {
                    max.row
                } else {
                    (row + 1).min(max.row)
                }
            }
            Key::ArrowLeft => column = if edge { 0 } else { column.saturating_sub(1) },
            Key::ArrowRight => {
                column = if edge {
                    max.column
                } else {
                    (column + 1).min(max.column)
                }
            }
            Key::PageUp => row = row.saturating_sub(page),
            Key::PageDown => row = row.saturating_add(page).min(max.row),
            Key::Home => {
                column = 0;
                if edge {
                    row = 0;
                }
            }
            Key::End => {
                column = max.column;
                if edge {
                    row = max.row;
                }
            }
            _ => {}
        }
        Point { row, column }
    }
}
