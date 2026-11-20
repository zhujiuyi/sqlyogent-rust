use std::{
    collections::{HashSet, VecDeque},
    fmt::Write as _,
    io,
    path::Path,
    time::Duration,
};

use chrono::{Local, NaiveTime};
use eframe::egui::{self, Button, Checkbox, Event, Key, Modifiers, RichText};
use sha2::{Digest, Sha256};

use super::super::{
    preferences::TextFont,
    state::{Workspace, write_atomic},
    style,
};

#[derive(Default)]
pub(in crate::app) struct History {
    entries: VecDeque<Entry>,
    next_id: u64,
    selected: HashSet<u64>,
    removal: Option<Removal>,
    focus: Option<egui::Id>,
}

struct Entry {
    id: u64,
    content: Content,
}

enum Content {
    Sql {
        sql: String,
        completion: Option<Completion>,
    },
    Note {
        time: NaiveTime,
        text: &'static str,
    },
}

impl Content {
    fn sql(&self) -> Option<&str> {
        match self {
            Self::Sql { sql, .. } => Some(sql),
            Self::Note { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::app) enum HistoryTiming {
    Execution(Duration),
    Total(Duration),
}

struct Completion {
    time: NaiveTime,
    timing: Option<HistoryTiming>,
}

impl Completion {
    fn label(&self) -> String {
        let time = self.time.format("%H:%M:%S");
        let (kind, elapsed) = match self.timing {
            Some(HistoryTiming::Execution(elapsed)) => ("执行耗时", elapsed),
            Some(HistoryTiming::Total(elapsed)) => ("总耗时", elapsed),
            None => return time.to_string(),
        };
        format!("{time} · {kind} {} 毫秒", elapsed.as_millis())
    }
}

fn note_label(time: NaiveTime, text: &str) -> String {
    format!(
        "{} · {}",
        time.format("%H:%M:%S"),
        text.replace(['\r', '\n'], " ")
    )
}

#[derive(Clone, Copy)]
enum Scope {
    Entry(u64),
    Selected,
    All,
}

impl Scope {
    fn includes(self, id: u64, selected: &HashSet<u64>) -> bool {
        match self {
            Self::Entry(entry) => entry == id,
            Self::Selected => selected.contains(&id),
            Self::All => true,
        }
    }
}

struct Removal {
    ids: HashSet<u64>,
    all: bool,
}

enum Action {
    Copy(Scope),
    CopyLog,
    Remove(Scope),
    Open(u64),
    Save,
}

impl History {
    pub(in crate::app) fn record(&mut self, sql: String) -> u64 {
        self.append(Content::Sql {
            sql,
            completion: None,
        })
    }

    pub(in crate::app) fn note(&mut self, text: &'static str) {
        self.append(Content::Note {
            time: Local::now().time(),
            text,
        });
    }

    fn append(&mut self, content: Content) -> u64 {
        self.next_id += 1;
        self.entries.push_front(Entry {
            id: self.next_id,
            content,
        });
        self.next_id
    }

    pub(in crate::app) fn finish(&mut self, id: u64, timing: Option<HistoryTiming>) {
        if let Some(index) = self.index_of(id) {
            let entry = &mut self.entries[index];
            let Content::Sql { completion, .. } = &mut entry.content else {
                return;
            };
            *completion = Some(Completion {
                time: Local::now().time(),
                timing,
            });
        }
    }

    #[cfg(test)]
    pub(in crate::app) fn recorded_sql(&self) -> impl Iterator<Item = &str> {
        self.matching(Scope::All)
            .filter_map(|entry| entry.content.sql())
    }

    #[cfg(test)]
    pub(in crate::app) fn recorded_timings(&self) -> impl Iterator<Item = Option<HistoryTiming>> {
        self.matching(Scope::All)
            .filter_map(|entry| match &entry.content {
                Content::Sql { completion, .. } => {
                    Some(completion.as_ref().and_then(|completion| completion.timing))
                }
                Content::Note { .. } => None,
            })
    }

    #[cfg(test)]
    pub(in crate::app) fn recorded_notes(&self) -> impl Iterator<Item = &str> {
        self.matching(Scope::All)
            .filter_map(|entry| match &entry.content {
                Content::Note { text, .. } => Some(*text),
                Content::Sql { .. } => None,
            })
    }

    pub(in crate::app) fn has_focus(&self, ctx: &egui::Context) -> bool {
        self.focus
            .is_some_and(|id| ctx.memory(|memory| memory.has_focus(id)))
    }

    fn matching(&self, scope: Scope) -> impl Iterator<Item = &Entry> {
        self.entries
            .iter()
            .rev()
            .filter(move |entry| scope.includes(entry.id, &self.selected))
    }

    fn index_of(&self, id: u64) -> Option<usize> {
        self.entries
            .binary_search_by(|entry| id.cmp(&entry.id))
            .ok()
    }

    fn sql(&self, id: u64) -> Option<&str> {
        self.entries[self.index_of(id)?].content.sql()
    }

    fn has_selected_sql(&self) -> bool {
        self.selected.iter().any(|&id| self.sql(id).is_some())
    }

    fn text(&self, scope: Scope) -> String {
        if let Scope::Entry(id) = scope {
            return self.sql(id).unwrap_or_default().to_owned();
        }
        self.matching(scope)
            .filter_map(|entry| entry.content.sql())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    fn script(&self) -> String {
        // History entries have already been separated from their delimiters. Reframe
        // each stored statement so semicolons in routines do not split the export.
        let mut hash = Sha256::new();
        for sql in self
            .matching(Scope::All)
            .filter_map(|entry| entry.content.sql())
        {
            hash.update(sql.as_bytes());
        }
        let digest = format!("{:x}", hash.finalize());
        let mut separator = format!("__h{}__", &digest[..8]);
        while self
            .entries
            .iter()
            .filter_map(|entry| entry.content.sql())
            .any(|sql| sql.contains(&separator))
        {
            separator.push('_');
        }
        let mut script = format!("DELIMITER {separator}\n");
        let mut after_comment = false;
        for entry in self.matching(Scope::All) {
            match &entry.content {
                Content::Sql { sql, completion } => {
                    if after_comment {
                        // Keep the preceding log comments outside the next statement.
                        writeln!(script, "DELIMITER {separator}").unwrap();
                    }
                    writeln!(script, "{sql}\n{separator}").unwrap();
                    after_comment = completion.is_some();
                    if let Some(completion) = completion {
                        writeln!(script, "-- {}", completion.label()).unwrap();
                    }
                }
                Content::Note { time, text } => {
                    writeln!(script, "-- {}", note_label(*time, text)).unwrap();
                    after_comment = true;
                }
            }
        }
        script.push_str("DELIMITER ;\n");
        script
    }

    fn export_text(&self) -> String {
        let mut text = String::new();
        for (index, entry) in self.matching(Scope::All).enumerate() {
            if index != 0 {
                text.push_str("\n\n");
            }
            match &entry.content {
                Content::Sql { sql, completion } => {
                    text.push_str(sql);
                    if let Some(completion) = completion {
                        write!(text, "\n-- {}", completion.label()).unwrap();
                    }
                }
                Content::Note { time, text: note } => {
                    write!(text, "-- {}", note_label(*time, note)).unwrap();
                }
            }
        }
        text
    }

    fn save(&self, path: &Path) -> io::Result<()> {
        let is_sql = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("sql"));
        let content = if is_sql {
            self.script()
        } else {
            self.export_text()
        };
        write_atomic(path, content.as_bytes())
    }

    fn request_removal(&mut self, scope: Scope) {
        self.removal = Some(Removal {
            ids: self.matching(scope).map(|entry| entry.id).collect(),
            all: matches!(scope, Scope::All),
        });
    }

    fn remove_confirmed(&mut self) -> usize {
        let Some(removal) = self.removal.take() else {
            return 0;
        };
        let before = self.entries.len();
        self.entries.retain(|entry| {
            if !removal.ids.contains(&entry.id) {
                return true;
            }
            self.selected.remove(&entry.id);
            false
        });
        before - self.entries.len()
    }
}

pub(super) fn panel(
    ui: &mut egui::Ui,
    workspace: &mut Workspace,
    keyboard_allowed: bool,
    font: TextFont,
    request_focus: bool,
) {
    let history = &mut workspace.history;
    if let Some(removal) = &history.removal {
        let prompt = if removal.all {
            format!("清空当前的 {} 条查询历史？", removal.ids.len())
        } else {
            format!("删除 {} 条查询历史？", removal.ids.len())
        };
        let (apply, cancel) = ui
            .horizontal_wrapped(|ui| {
                ui.label(prompt);
                let label = if removal.all {
                    "确认清空"
                } else {
                    "确认删除"
                };
                let apply = ui
                    .button(RichText::new(label).color(style::ERROR))
                    .clicked();
                let cancel = ui.button("取消").clicked();
                (apply, cancel)
            })
            .inner;
        if apply {
            let count = history.remove_confirmed();
            workspace.message(format!("已删除 {count} 条查询历史。"), false);
            return;
        } else if cancel {
            history.removal = None;
        }
    }

    if history.entries.is_empty() {
        ui.label(RichText::new("暂无执行记录").color(style::MUTED));
        return;
    }

    let mut action = None;
    if keyboard_allowed && history.removal.is_none() && history.has_focus(ui.ctx()) {
        let has_selected_sql = history.has_selected_sql();
        action = ui.input_mut(|input| {
            if consume_key(input, Key::S) {
                return Some(Action::Save);
            }
            if !has_selected_sql {
                return None;
            }
            let shortcut = consume_key(input, Key::C);
            let count = input.events.len();
            input.events.retain(|event| !matches!(event, Event::Copy));
            (shortcut || input.events.len() != count).then_some(Action::Copy(Scope::Selected))
        });
    }

    let focus_row = (request_focus && history.removal.is_none()).then(|| {
        history
            .selected
            .iter()
            .max()
            .and_then(|&id| history.index_of(id))
            .unwrap_or(0)
    });
    ui.add_enabled_ui(history.removal.is_none(), |ui| {
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(history.has_selected_sql(), Button::new("复制选中"))
                .clicked()
            {
                action = Some(Action::Copy(Scope::Selected));
            }
            let has_sql = history
                .entries
                .iter()
                .any(|entry| entry.content.sql().is_some());
            if ui.add_enabled(has_sql, Button::new("复制全部")).clicked() {
                action = Some(Action::Copy(Scope::All));
            }
            if ui.button("复制日志").clicked() {
                action = Some(Action::CopyLog);
            }
            if ui.button("保存历史记录…").clicked() {
                action = Some(Action::Save);
            }
            if ui
                .add_enabled(!history.selected.is_empty(), Button::new("删除选中…"))
                .clicked()
            {
                action = Some(Action::Remove(Scope::Selected));
            }
            if ui.button("清空历史…").clicked() {
                action = Some(Action::Remove(Scope::All));
            }
        });
        let font = font.id();
        let font_height = ui.fonts_mut(|fonts| fonts.row_height(&font));
        let row_height = ui
            .spacing()
            .interact_size
            .y
            .max(font_height + 2.0 * ui.spacing().button_padding.y);
        let mut area = egui::ScrollArea::vertical().id_salt(("query-history", workspace.key));
        if let Some(row) = focus_row {
            let y = row as f32 * (row_height + ui.spacing().item_spacing.y);
            area = area.vertical_scroll_offset((y - 0.4 * ui.available_height()).max(0.0));
        }
        area.show_rows(ui, row_height, history.entries.len(), |ui, rows| {
            for row in rows {
                let entry = &history.entries[row];
                ui.push_id(entry.id, |ui| {
                    ui.horizontal(|ui| {
                        ui.set_min_height(row_height);
                        let mut selected = history.selected.contains(&entry.id);
                        let choose = ui
                            .add(Checkbox::without_text(&mut selected))
                            .on_hover_text("选择此记录");
                        if choose.changed() {
                            if selected {
                                history.selected.insert(entry.id);
                            } else {
                                history.selected.remove(&entry.id);
                            }
                            choose.request_focus();
                        }
                        if focus_row == Some(row) {
                            choose.request_focus();
                        }
                        if choose.has_focus() {
                            history.focus = Some(choose.id);
                        }
                        let response = match &entry.content {
                            Content::Sql { sql, completion } => {
                                if let Some(completion) = completion {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(completion.label()).color(style::MUTED),
                                        )
                                        .truncate(),
                                    );
                                }
                                let response = ui
                                    .add(
                                        Button::new(RichText::new(preview(sql)).font(font.clone()))
                                            .truncate(),
                                    )
                                    .on_hover_text("在新查询标签中打开");
                                if response.clicked() {
                                    action = Some(Action::Open(entry.id));
                                }
                                response
                            }
                            Content::Note { time, text } => ui.add(
                                egui::Label::new(
                                    RichText::new(note_label(*time, text)).color(style::MUTED),
                                )
                                .selectable(false)
                                .sense(egui::Sense::click())
                                .truncate(),
                            ),
                        };
                        response.context_menu(|ui| {
                            if entry.content.sql().is_some() && ui.button("复制本条 SQL").clicked()
                            {
                                action = Some(Action::Copy(Scope::Entry(entry.id)));
                                ui.close();
                            }
                            if ui.button("删除本条记录…").clicked() {
                                action = Some(Action::Remove(Scope::Entry(entry.id)));
                                ui.close();
                            }
                        });
                    });
                });
            }
        });
    });

    match action {
        Some(Action::Open(id)) => {
            let sql = history.sql(id).map(str::to_owned);
            if let Some(sql) = sql {
                workspace.new_tab(sql, None);
            }
        }
        Some(Action::Copy(scope)) => ui.ctx().copy_text(history.text(scope)),
        Some(Action::CopyLog) => ui.ctx().copy_text(history.export_text()),
        Some(Action::Remove(scope)) => history.request_removal(scope),
        Some(Action::Save) => {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("保存查询历史记录")
                .set_file_name("查询历史.sql")
                .add_filter("SQL 文件", &["sql"])
                .add_filter("文本文件", &["txt"])
                .save_file()
            {
                let result = history.save(&path);
                match result {
                    Ok(()) => {
                        workspace.message(format!("已保存查询历史：{}", path.display()), false)
                    }
                    Err(error) => workspace.message(format!("无法保存查询历史：{error}"), true),
                }
            }
        }
        None => {}
    }
}

fn consume_key(input: &mut egui::InputState, key: Key) -> bool {
    let count = input.events.len();
    input.events.retain(|event| {
        !matches!(event, Event::Key { key: pressed_key, modifiers, pressed: true, .. }
            if *pressed_key == key && modifiers.matches_exact(Modifiers::COMMAND))
    });
    input.events.len() != count
}

fn preview(sql: &str) -> String {
    let end = sql.char_indices().nth(240).map(|(index, _)| index);
    let mut preview = sql[..end.unwrap_or(sql.len())]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if end.is_some() {
        preview.push('…');
    }
    preview
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql_tools::split_script;

    #[test]
    fn copies_selected_history_and_removes_the_confirmed_entries() {
        let mut history = History::default();
        let first = "SELECT\n  '原值\t保留'";
        let second = "SELECT 2";
        let first_id = history.record(first.into());
        let second_id = history.record(second.into());
        let repeated = history.record(first.into());
        let timing = HistoryTiming::Execution(Duration::from_millis(7));
        history.finish(repeated, Some(timing));
        history.selected.extend([second_id, repeated]);
        assert_eq!(history.text(Scope::Entry(first_id)), first);
        assert_eq!(
            history.recorded_timings().collect::<Vec<_>>(),
            vec![None, None, Some(timing)]
        );
        assert_eq!(
            history.text(Scope::Selected),
            format!("{second}\n\n{first}")
        );
        assert_eq!(
            history.text(Scope::All),
            format!("{first}\n\n{second}\n\n{first}")
        );

        history.request_removal(Scope::Selected);
        history.record("SELECT 4".into());
        assert_eq!(history.remove_confirmed(), 2);
        assert_eq!(history.text(Scope::All), format!("{first}\n\nSELECT 4"));

        history.request_removal(Scope::All);
        history.record("SELECT 5".into());
        assert_eq!(history.remove_confirmed(), 2);
        assert_eq!(history.text(Scope::All), "SELECT 5");
    }

    #[test]
    fn saves_history_in_execution_order_preserving_utf8_compound_sql_and_notes() {
        let fixture = tempfile::tempdir_in("target").unwrap();
        let mut history = History::default();
        let routine = "CREATE PROCEDURE p()\nBEGIN\n  SELECT '中文';\n  SELECT 2;\nEND";
        let call = "CALL p() -- 保留行注释";
        let untimed = "SELECT 3";
        let note = "连接已恢复";
        let routine_id = history.record(routine.into());
        history.finish(
            routine_id,
            Some(HistoryTiming::Execution(Duration::from_millis(7))),
        );
        history.note(note);
        let call_id = history.record(call.into());
        history.finish(
            call_id,
            Some(HistoryTiming::Total(Duration::from_millis(9))),
        );
        history.record(untimed.into());

        assert_eq!(history.recorded_notes().collect::<Vec<_>>(), vec![note]);
        assert_eq!(
            history.text(Scope::All),
            format!("{routine}\n\n{call}\n\n{untimed}")
        );

        let text_path = fixture.path().join("历史.txt");
        history.save(&text_path).unwrap();
        let text = std::fs::read_to_string(text_path).unwrap();
        assert_eq!(text, history.export_text());
        let annotations = ["执行耗时 7 毫秒", note, "总耗时 9 毫秒"];
        let matching_comments = |document: &str| {
            document
                .lines()
                .filter_map(|line| line.strip_prefix("-- ")?.split_once(" · "))
                .map(|(time, comment)| {
                    NaiveTime::parse_from_str(time, "%H:%M:%S").unwrap();
                    comment.to_owned()
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(matching_comments(&text), annotations);

        let sql_path = fixture.path().join("历史.sql");
        history.save(&sql_path).unwrap();
        let script = std::fs::read_to_string(sql_path).unwrap();
        assert_eq!(matching_comments(&script), annotations);
        assert_eq!(
            split_script(&script)
                .into_iter()
                .map(|part| part.sql)
                .collect::<Vec<_>>(),
            vec![routine, call, untimed],
        );
    }

    #[test]
    fn history_keeps_more_than_a_hundred_entries_and_preserves_long_sql() {
        let mut history = History::default();
        for index in 0..120 {
            history.record(format!("SELECT {index}"));
        }
        assert_eq!(history.recorded_sql().count(), 120);

        let long = format!("SELECT '{}'", "a".repeat(5000));
        let len = long.len();
        let id = history.record(long);
        assert_eq!(history.sql(id).unwrap().len(), len);
        assert_eq!(history.recorded_sql().count(), 121);

        history.record("SELECT 120".into());
        assert_eq!(history.recorded_sql().count(), 122);
        assert_eq!(history.recorded_sql().next(), Some("SELECT 0"));
        assert_eq!(history.recorded_sql().last(), Some("SELECT 120"));
    }
}
