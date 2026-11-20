use std::ops::Range;

use eframe::egui::{
    self, Button, Event, Key, Modifiers, TextEdit,
    text::{CCursor, CCursorRange},
    text_edit::TextEditState,
};

use crate::sql_tools::{SqlFormatSettings, format_sql_with_settings, statement_at_with_quoting};

use super::{
    byte_index,
    preferences::EditorSettings,
    state::{QueryTab, Workspace},
    style,
    text_search::{SearchOptions, TextSearch},
};

#[derive(Clone, Copy)]
pub(super) enum Action {
    Undo,
    Redo,
    Clear,
    Find,
    Replace,
    GoTo,
    Next,
    FormatAll,
    FormatCurrent,
    FormatSelection,
    Upper,
    Lower,
    Comment,
    Uncomment,
    SelectAll,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Panel {
    Find,
    Replace,
    GoTo,
}

struct WrapConfirmation {
    editor: egui::Id,
    caret: usize,
    selection: Option<Range<usize>>,
    text_len: usize,
    up: bool,
}

#[derive(Default)]
pub(super) struct EditorTools {
    panel: Option<Panel>,
    needle: String,
    replacement: String,
    search_options: SearchOptions,
    find_up: bool,
    wrap: Option<WrapConfirmation>,
    line: usize,
    focus: bool,
}

impl EditorTools {
    pub fn action(
        &mut self,
        action: Action,
        ctx: &egui::Context,
        ws: &mut Workspace,
        formatter: SqlFormatSettings,
    ) {
        self.invalidate_wrap();
        match action {
            Action::Undo | Action::Redo => restore_history(ctx, ws, matches!(action, Action::Redo)),
            Action::Clear => {
                let end = ws.tab().sql.len();
                if end != 0 {
                    change(ctx, ws, 0..end, "", false);
                }
            }
            Action::Find | Action::Replace | Action::GoTo => {
                self.panel = Some(match action {
                    Action::Find => Panel::Find,
                    Action::Replace => Panel::Replace,
                    _ => Panel::GoTo,
                });
                self.focus = true;
                if action_is_search(action)
                    && let Some(text) = selected(ws.tab())
                    && !text.is_empty()
                    && !text.contains(['\r', '\n'])
                {
                    self.needle = text.to_owned();
                }
                if matches!(action, Action::GoTo) {
                    self.line = safe_offset(ws.tab()).map_or(1, |offset| {
                        ws.tab().sql[..offset]
                            .bytes()
                            .filter(|byte| *byte == b'\n')
                            .count()
                            + 1
                    });
                }
            }
            Action::Next => {
                if let Some(search) = TextSearch::new(&self.needle, self.search_options) {
                    let up = self.panel != Some(Panel::Replace) && self.find_up;
                    self.find_next(ctx, ws, &search, up);
                }
            }
            Action::SelectAll => move_cursor(ctx, ws, 0..ws.tab().sql.len()),
            action => match edit_range(ws.tab(), action, formatter, editor_quoting(ws)) {
                Ok((range, text)) => change(ctx, ws, range, &text, true),
                Err(message) => ws.message(message, false),
            },
        }
    }

    pub(super) fn invalidate_wrap(&mut self) {
        self.wrap = None;
    }

    pub fn show(&mut self, ui: &mut egui::Ui, ws: &mut Workspace) {
        let ctx = ui.ctx().clone();
        if let Some(panel) = self.panel {
            style::pane(egui::Margin::symmetric(14, 7)).show(ui, |ui| {
                style::paint_bottom_hairline(ui);
                ui.set_min_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    match panel {
                        Panel::Find | Panel::Replace => {
                            self.show_search(ui, ws, panel == Panel::Replace);
                        }
                        Panel::GoTo => {
                            ui.label("行号");
                            let maximum =
                                ws.tab().sql.bytes().filter(|byte| *byte == b'\n').count() + 1;
                            let input =
                                ui.add(egui::DragValue::new(&mut self.line).range(1..=maximum));
                            if std::mem::take(&mut self.focus) {
                                input.request_focus();
                            }
                            if ui.button("跳转").clicked()
                                || (input.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)))
                            {
                                let offset = line_start(&ws.tab().sql, self.line.clamp(1, maximum));
                                move_cursor(&ctx, ws, offset..offset);
                                self.panel = None;
                            }
                        }
                    }
                    if ui.small_button("关闭").clicked() {
                        self.panel = None;
                        self.invalidate_wrap();
                        ws.focus_request = true;
                    }
                });
            });
        }
        self.show_wrap(ui, ws);
    }

    fn show_search(&mut self, ui: &mut egui::Ui, ws: &mut Workspace, replacing: bool) {
        let ctx = ui.ctx().clone();
        let input = ui.add(
            TextEdit::singleline(&mut self.needle)
                .hint_text("查找文字")
                .desired_width(160.0)
                .return_key(None),
        );
        if std::mem::take(&mut self.focus) {
            input.request_focus();
        }
        let replacement = replacing.then(|| {
            ui.add(
                TextEdit::singleline(&mut self.replacement)
                    .hint_text("替换为")
                    .desired_width(160.0)
                    .return_key(None),
            )
        });
        let input_focused =
            input.has_focus() || replacement.as_ref().is_some_and(egui::Response::has_focus);
        let input_had_focus =
            input.lost_focus() || replacement.as_ref().is_some_and(egui::Response::lost_focus);
        if (input_focused || input_had_focus)
            && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
        {
            self.panel = None;
            self.invalidate_wrap();
            ws.focus_request = true;
            return;
        }
        let mut changed =
            input.changed() || replacement.as_ref().is_some_and(egui::Response::changed);
        changed |= ui
            .checkbox(&mut self.search_options.match_case, "区分大小写")
            .changed();
        changed |= ui
            .checkbox(&mut self.search_options.whole_word, "全字匹配")
            .changed();
        if !replacing {
            changed |= ui.radio_value(&mut self.find_up, false, "向下").changed();
            changed |= ui.radio_value(&mut self.find_up, true, "向上").changed();
        }
        if changed {
            self.invalidate_wrap();
        }
        let has_needle = !self.needle.is_empty();
        let enter = input_focused && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
        let find = ui
            .add_enabled(has_needle, Button::new("查找下一个"))
            .clicked()
            || enter;
        let replace = replacing && ui.add_enabled(has_needle, Button::new("替换")).clicked();
        let replace_all = replacing
            && ui
                .add_enabled(has_needle, Button::new("全部替换"))
                .clicked();
        if !has_needle || !(find || replace || replace_all) {
            return;
        }
        let Some(search) = TextSearch::new(&self.needle, self.search_options) else {
            ws.message("查找文字过长，无法搜索。", false);
            return;
        };
        self.invalidate_wrap();
        if replace_all {
            self.replace_all(&ctx, ws, &search);
        } else if replace {
            self.replace_one(&ctx, ws, &search);
        } else {
            self.find_next(&ctx, ws, &search, !replacing && self.find_up);
        }
    }

    fn find_next(
        &mut self,
        ctx: &egui::Context,
        ws: &mut Workspace,
        search: &TextSearch,
        up: bool,
    ) {
        let tab = ws.tab();
        let from = current_match(tab, search).map_or_else(
            || safe_offset(tab).unwrap_or(tab.sql.len()),
            |range| if up { range.start } else { range.end },
        );
        self.find_from(ctx, ws, search, from, up);
    }

    fn find_from(
        &mut self,
        ctx: &egui::Context,
        ws: &mut Workspace,
        search: &TextSearch,
        from: usize,
        up: bool,
    ) {
        self.invalidate_wrap();
        let sql = &ws.tab().sql;
        let scope = if up { 0..from } else { from..sql.len() };
        if let Some(range) = search.find_in(sql, scope, !up) {
            move_cursor(ctx, ws, range);
        } else {
            self.prompt_wrap(ws, search, up);
        }
    }

    fn prompt_wrap(&mut self, ws: &mut Workspace, search: &TextSearch, up: bool) {
        let tab = ws.tab();
        if search.matches(&tab.sql).next().is_some() {
            self.wrap = Some(WrapConfirmation {
                editor: editor_id(ws),
                caret: tab.focused_byte,
                selection: tab.selection.clone(),
                text_len: tab.sql.len(),
                up,
            });
        } else {
            ws.message("未找到匹配的文字。", false);
        }
    }

    fn show_wrap(&mut self, ui: &mut egui::Ui, ws: &mut Workspace) {
        let Some(wrap) = &self.wrap else { return };
        let tab = ws.tab();
        if wrap.editor != editor_id(ws)
            || wrap.caret != tab.focused_byte
            || wrap.selection != tab.selection
            || wrap.text_len != tab.sql.len()
        {
            self.invalidate_wrap();
            return;
        }
        let up = wrap.up;
        ui.horizontal_wrapped(|ui| {
            ui.label(if up {
                "已查找到开头，从末尾继续查找？"
            } else {
                "已查找到末尾，从开头继续查找？"
            });
            if ui.button("继续").clicked() {
                self.continue_find(ui.ctx(), ws, up);
            }
            if ui.button("取消").clicked() {
                self.invalidate_wrap();
                ws.focus_request = true;
            }
        });
    }

    fn continue_find(&mut self, ctx: &egui::Context, ws: &mut Workspace, up: bool) {
        self.invalidate_wrap();
        let tab = ws.tab();
        if let Some(range) = TextSearch::new(&self.needle, self.search_options)
            .and_then(|search| search.find_in(&tab.sql, 0..tab.sql.len(), !up))
        {
            move_cursor(ctx, ws, range);
        } else {
            ws.message("未找到匹配的文字。", false);
        }
    }

    fn replace_one(&mut self, ctx: &egui::Context, ws: &mut Workspace, search: &TextSearch) {
        let tab = ws.tab();
        let Some(range) = current_match(tab, search) else {
            let from = tab.selection.as_ref().map_or_else(
                || safe_offset(tab).unwrap_or(tab.sql.len()),
                |range| range.start,
            );
            self.find_from(ctx, ws, search, from, false);
            return;
        };
        let from = range.start + self.replacement.len();
        if tab.sql[range.clone()] == self.replacement {
            move_cursor(ctx, ws, from..from);
            self.find_from(ctx, ws, search, from, false);
            return;
        }
        replace_and_select(ctx, ws, range, &self.replacement, |sql| {
            search
                .find_in(sql, from..sql.len(), true)
                .unwrap_or(from..from)
        });
        if ws.tab().selection.is_none() {
            self.prompt_wrap(ws, search, false);
        }
    }

    fn replace_all(&self, ctx: &egui::Context, ws: &mut Workspace, search: &TextSearch) {
        let original_len = ws.tab().sql.len();
        let Some((text, total)) = search.replace_all(&ws.tab().sql, &self.replacement) else {
            ws.message("未找到匹配的文字。", false);
            return;
        };
        change(ctx, ws, 0..original_len, &text, true);
        ws.message(format!("已替换 {total} 处。"), false);
    }
}

fn action_is_search(action: Action) -> bool {
    matches!(action, Action::Find | Action::Replace)
}

fn selected(tab: &QueryTab) -> Option<&str> {
    tab.selection.clone().and_then(|range| tab.sql.get(range))
}

pub(super) fn has_selection(ws: &Workspace) -> bool {
    selected(ws.tab()).is_some_and(|text| !text.is_empty())
}

fn safe_offset(tab: &QueryTab) -> Option<usize> {
    tab.sql
        .is_char_boundary(tab.focused_byte)
        .then_some(tab.focused_byte)
}

fn current_match(tab: &QueryTab, search: &TextSearch) -> Option<Range<usize>> {
    let selected = tab.selection.clone()?;
    search
        .find_in(&tab.sql, selected.clone(), true)
        .filter(|range| *range == selected)
}

fn line_start(sql: &str, line: usize) -> usize {
    if line <= 1 {
        0
    } else {
        sql.match_indices('\n')
            .nth(line - 2)
            .map_or(sql.len(), |(offset, _)| offset + 1)
    }
}

fn edit_range(
    tab: &QueryTab,
    action: Action,
    formatter: SqlFormatSettings,
    modes: (bool, bool),
) -> Result<(Range<usize>, String), &'static str> {
    let selection = tab
        .selection
        .clone()
        .filter(|range| tab.sql.get(range.clone()).is_some());
    let cursor = safe_offset(tab).unwrap_or(tab.sql.len());
    let range = match action {
        Action::FormatAll => 0..tab.sql.len(),
        Action::FormatCurrent => {
            statement_at_with_quoting(&tab.sql, cursor, modes)
                .ok_or("光标处没有 SQL 语句。")?
                .range
        }
        Action::FormatSelection | Action::Upper | Action::Lower => {
            selection.ok_or("请先选择要编辑的文字。")?
        }
        Action::Comment | Action::Uncomment => {
            lines_in_range(&tab.sql, selection.unwrap_or(cursor..cursor))
        }
        _ => return Err("请选择编辑命令。"),
    };
    let value = &tab.sql[range.clone()];
    let replacement = match action {
        Action::FormatAll | Action::FormatCurrent | Action::FormatSelection => {
            format_sql_with_settings(&tab.sql, range.clone(), formatter, modes)
        }
        Action::Upper => value.to_uppercase(),
        Action::Lower => value.to_lowercase(),
        Action::Comment => toggle_comments(value, true),
        Action::Uncomment => toggle_comments(value, false),
        _ => unreachable!("not a text transformation"),
    };
    Ok((range, replacement))
}

fn lines_in_range(sql: &str, range: Range<usize>) -> Range<usize> {
    let start = sql[..range.start]
        .rfind('\n')
        .map_or(0, |offset| offset + 1);
    let end = if range.end > range.start && sql.as_bytes().get(range.end - 1) == Some(&b'\n') {
        range.end
    } else {
        sql[range.end..]
            .find('\n')
            .map_or(sql.len(), |offset| offset + range.end + 1)
    };
    start..end
}

fn toggle_comments(sql: &str, comment: bool) -> String {
    let mut output = String::with_capacity(sql.len());
    for line in sql.split_inclusive('\n') {
        let contents = line.trim_start_matches([' ', '\t']);
        output.push_str(&line[..line.len() - contents.len()]);
        if comment {
            if !contents.trim_end_matches(['\r', '\n']).is_empty() {
                output.push_str("-- ");
            }
            output.push_str(contents);
        } else if let Some(rest) = contents.strip_prefix("--")
            && rest.as_bytes().first().is_none_or(u8::is_ascii_whitespace)
        {
            output.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        } else {
            output.push_str(contents.strip_prefix('#').unwrap_or(contents));
        }
    }
    output
}

pub(super) fn insert_text(
    ctx: &egui::Context,
    ws: &mut Workspace,
    text: &str,
    select_replacement: bool,
) {
    let tab = ws.tab();
    let range = tab
        .selection
        .clone()
        .filter(|range| tab.sql.get(range.clone()).is_some())
        .unwrap_or_else(|| {
            let offset = safe_offset(tab).unwrap_or(tab.sql.len());
            offset..offset
        });
    change(ctx, ws, range, text, select_replacement);
}

pub(super) fn editor_id(workspace: &Workspace) -> egui::Id {
    egui::Id::new(("sql-editor", workspace.key, workspace.tab().id))
}

pub(super) fn editor_quoting(workspace: &Workspace) -> (bool, bool) {
    let sql_mode = workspace
        .profile
        .as_ref()
        .and_then(|profile| profile.advanced.sql_mode.as_deref())
        .unwrap_or("");
    let has_mode = |name| {
        sql_mode
            .split(',')
            .any(|part| part.trim().eq_ignore_ascii_case(name))
    };
    (
        workspace.no_backslash_escapes || has_mode("NO_BACKSLASH_ESCAPES"),
        has_mode("ANSI_QUOTES"),
    )
}

pub(super) fn indent_from_keyboard(
    ui: &egui::Ui,
    ws: &mut Workspace,
    id: egui::Id,
    settings: EditorSettings,
) {
    if !ui.memory(|memory| memory.has_focus(id)) || !ui.input(|input| input.focused) {
        return;
    }
    let multiline = selected(ws.tab()).is_some_and(|text| text.contains('\n'));
    let mut directions = Vec::new();
    ui.input_mut(|input| {
        input.events.retain(|event| {
            if let Event::Key {
                key: Key::Tab,
                modifiers,
                pressed: true,
                ..
            } = event
            {
                if *modifiers == Modifiers::SHIFT {
                    directions.push(true);
                    return false;
                }
                if *modifiers == Modifiers::NONE && (settings.indent_with_spaces || multiline) {
                    directions.push(false);
                    return false;
                }
            }
            true
        });
    });
    for outdent in directions {
        change_indentation(ui, ws, settings, outdent);
    }
}

fn change_indentation(ui: &egui::Ui, ws: &mut Workspace, settings: EditorSettings, outdent: bool) {
    let tab = ws.tab();
    let caret = safe_offset(tab).unwrap_or(tab.sql.len());
    let selection = tab
        .selection
        .clone()
        .filter(|range| tab.sql.get(range.clone()).is_some())
        .unwrap_or(caret..caret);
    let line_width = usize::from(settings.tab_width);
    if !outdent && !tab.sql[selection.clone()].contains('\n') {
        let indentation = if settings.indent_with_spaces {
            let prefix = tab.sql[..selection.start].rsplit('\n').next().unwrap_or("");
            " ".repeat(style::spaces_to_tab_stop(ui, prefix, settings))
        } else {
            "\t".to_owned()
        };
        change(ui.ctx(), ws, selection, &indentation, false);
        return;
    }

    let lines = lines_in_range(&tab.sql, selection.clone());
    let base = lines.start;
    let source = &tab.sql[lines.clone()];
    let prefix = if settings.indent_with_spaces {
        " ".repeat(line_width)
    } else {
        "\t".to_owned()
    };
    let mut output = String::new();
    for line in source.split_inclusive('\n') {
        if outdent {
            output.push_str(&line[indentation_bytes(line, line_width)..]);
        } else {
            output.push_str(&prefix);
            output.push_str(line);
        }
    }
    if source == output {
        return;
    }
    let range_after = if selection.is_empty() {
        let at = caret
            .saturating_sub(indentation_bytes(source, line_width))
            .max(base);
        at..at
    } else {
        base..base + output.len()
    };
    replace_and_select(ui.ctx(), ws, lines, &output, |_| range_after);
}

fn indentation_bytes(line: &str, width: usize) -> usize {
    let spaces = line.bytes().take(width).take_while(|&b| b == b' ').count();
    spaces + usize::from(spaces < width && line.as_bytes().get(spaces) == Some(&b'\t'))
}

pub(super) fn cursors(text: &str, range: Range<usize>) -> CCursorRange {
    CCursorRange::two(
        CCursor::new(text[..range.start].chars().count()),
        CCursor::new(text[..range.end].chars().count()),
    )
}

fn move_cursor(ctx: &egui::Context, ws: &mut Workspace, range: Range<usize>) {
    let id = editor_id(ws);
    let tab = ws.tab_mut();
    let mut state = TextEdit::load_state(ctx, id).unwrap_or_default();
    state
        .cursor
        .set_char_range(Some(cursors(&tab.sql, range.clone())));
    state.store(ctx, id);
    tab.focused_byte = range.end;
    tab.selection = (range.start != range.end).then_some(range);
    ws.focus_request = true;
}

fn history_state(tab: &QueryTab, state: &TextEditState) -> (CCursorRange, String) {
    let cursor = state.cursor.char_range().unwrap_or_else(|| {
        let point = safe_offset(tab).unwrap_or(tab.sql.len());
        let range = tab
            .selection
            .clone()
            .filter(|range| tab.sql.get(range.clone()).is_some())
            .unwrap_or(point..point);
        cursors(&tab.sql, range)
    });
    (cursor, tab.sql.clone())
}

pub(super) fn history_availability(ctx: &egui::Context, ws: &Workspace) -> (bool, bool) {
    let Some(state) = TextEdit::load_state(ctx, editor_id(ws)) else {
        return (false, false);
    };
    let current = history_state(ws.tab(), &state);
    let history = state.undoer();
    (history.has_undo(&current), history.has_redo(&current))
}

fn restore_history(ctx: &egui::Context, ws: &mut Workspace, redo: bool) {
    let id = editor_id(ws);
    let Some(mut state) = TextEdit::load_state(ctx, id) else {
        return;
    };
    let tab = ws.tab_mut();
    let current = history_state(tab, &state);
    let mut history = state.undoer();
    let restored = if redo {
        history.redo(&current)
    } else {
        history.undo(&current)
    }
    .cloned();
    state.set_undoer(history);
    let Some((cursor, sql)) = restored else {
        return;
    };

    tab.sql = sql;
    let primary = byte_index(&tab.sql, cursor.primary.index);
    let secondary = byte_index(&tab.sql, cursor.secondary.index);
    tab.focused_byte = primary;
    tab.selection =
        (primary != secondary).then_some(primary.min(secondary)..primary.max(secondary));
    state.cursor.set_char_range(Some(cursor));
    state.store(ctx, id);
    ws.focus_request = true;
}

pub(super) fn change(
    ctx: &egui::Context,
    ws: &mut Workspace,
    range: Range<usize>,
    replacement: &str,
    select_replacement: bool,
) {
    if &ws.tab().sql[range.clone()] == replacement {
        if !select_replacement {
            move_cursor(ctx, ws, range.end..range.end);
        }
        return;
    }
    let end = range.start + replacement.len();
    let selection = if select_replacement {
        range.start..end
    } else {
        end..end
    };
    replace_and_select(ctx, ws, range, replacement, |_| selection);
}

fn replace_and_select(
    ctx: &egui::Context,
    ws: &mut Workspace,
    range: Range<usize>,
    replacement: &str,
    select: impl FnOnce(&str) -> Range<usize>,
) {
    let id = editor_id(ws);
    let tab = ws.tab_mut();
    let mut state = TextEdit::load_state(ctx, id).unwrap_or_default();
    let mut undo = state.undoer();
    undo.add_undo(&history_state(tab, &state));
    tab.sql.replace_range(range, replacement);
    let selection = select(&tab.sql);
    let new_cursor = cursors(&tab.sql, selection.clone());
    let new_state = (new_cursor, tab.sql.clone());
    undo.feed_state(ctx.input(|input| input.time), &new_state);
    undo.add_undo(&new_state);
    state.set_undoer(undo);
    state.cursor.set_char_range(Some(new_cursor));
    state.store(ctx, id);
    tab.focused_byte = selection.end;
    tab.selection = (selection.start != selection.end).then_some(selection);
    ws.focus_request = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_whole_words_upward_and_wraps_after_confirmation() {
        let mut workspace = Workspace::new("测试".into(), None);
        let sql = "甲 SELECT select_id select 乙";
        let first = sql.find("SELECT").unwrap();
        let last = sql.rfind("select").unwrap();
        workspace.tab_mut().sql = sql.into();
        workspace.tab_mut().focused_byte = sql.len();
        let ctx = egui::Context::default();
        let mut tools = EditorTools {
            needle: "select".into(),
            search_options: SearchOptions {
                whole_word: true,
                ..SearchOptions::default()
            },
            find_up: true,
            ..EditorTools::default()
        };
        let formatter = SqlFormatSettings::default();

        tools.action(Action::Next, &ctx, &mut workspace, formatter);
        assert_eq!(workspace.tab().selection, Some(last..last + 6));
        tools.action(Action::Next, &ctx, &mut workspace, formatter);
        assert_eq!(workspace.tab().selection, Some(first..first + 6));
        tools.action(Action::Next, &ctx, &mut workspace, formatter);
        assert_eq!(workspace.tab().selection, Some(first..first + 6));
        let up = tools.wrap.as_ref().unwrap().up;
        tools.continue_find(&ctx, &mut workspace, up);
        assert_eq!(workspace.tab().selection, Some(last..last + 6));
        assert!(tools.wrap.is_none());
    }

    #[test]
    fn replacing_once_selects_first_and_then_replaces_and_advances() {
        let mut workspace = Workspace::new("测试".into(), None);
        let original = "甲 SELECT select_id select 乙";
        let first = original.find("SELECT").unwrap();
        workspace.tab_mut().sql = original.into();
        let ctx = egui::Context::default();
        move_cursor(
            &ctx,
            &mut workspace,
            first..first + "SELECT select_id".len(),
        );
        let mut tools = EditorTools {
            needle: "select".into(),
            replacement: "内容项".into(),
            search_options: SearchOptions {
                whole_word: true,
                ..SearchOptions::default()
            },
            ..EditorTools::default()
        };
        let search = TextSearch::new(&tools.needle, tools.search_options).unwrap();

        tools.replace_one(&ctx, &mut workspace, &search);
        assert_eq!(workspace.tab().sql, original);
        assert_eq!(workspace.tab().selection, Some(first..first + 6));
        tools.replace_one(&ctx, &mut workspace, &search);
        let changed = "甲 内容项 select_id select 乙";
        let next = changed.rfind("select").unwrap();
        assert_eq!(workspace.tab().sql, changed);
        assert_eq!(workspace.tab().selection, Some(next..next + 6));
        tools.action(
            Action::Undo,
            &ctx,
            &mut workspace,
            SqlFormatSettings::default(),
        );
        assert_eq!(workspace.tab().sql, original);
        assert_eq!(workspace.tab().selection, Some(first..first + 6));
    }

    #[test]
    fn replacing_all_uses_both_options_and_undoes_as_a_single_edit() {
        let mut workspace = Workspace::new("测试".into(), None);
        let original = "甲 SELECT SELECT_id select SELECT; 乙";
        let start = original.rfind("SELECT").unwrap();
        let selection = start..start + 6;
        workspace.tab_mut().sql = original.into();
        let ctx = egui::Context::default();
        move_cursor(&ctx, &mut workspace, selection.clone());
        let mut tools = EditorTools {
            needle: "SELECT".into(),
            replacement: "投影".into(),
            search_options: SearchOptions {
                match_case: true,
                whole_word: true,
            },
            ..EditorTools::default()
        };
        let search = TextSearch::new(&tools.needle, tools.search_options).unwrap();

        tools.replace_all(&ctx, &mut workspace, &search);
        assert_eq!(workspace.tab().sql, "甲 投影 SELECT_id select 投影; 乙");
        assert_eq!(workspace.status, "已替换 2 处。");
        tools.action(
            Action::Undo,
            &ctx,
            &mut workspace,
            SqlFormatSettings::default(),
        );
        assert_eq!(workspace.tab().sql, original);
        assert_eq!(workspace.tab().selection, Some(selection));
        assert_eq!(history_availability(&ctx, &workspace), (false, true));
    }

    #[test]
    fn clears_entire_query_and_restores_utf8_selection_through_undo_and_redo() {
        let mut workspace = Workspace::new("测试".into(), None);
        let original = "SELECT '甲乙';";
        let start = original.find('乙').unwrap();
        let selection = start..start + '乙'.len_utf8();
        let ctx = egui::Context::default();
        let id = editor_id(&workspace);
        let mut state = TextEditState::default();
        let mut reverse = cursors(original, selection.clone());
        std::mem::swap(&mut reverse.primary, &mut reverse.secondary);
        state.cursor.set_char_range(Some(reverse));
        state.store(&ctx, id);
        let tab = workspace.tab_mut();
        tab.sql = original.into();
        tab.selection = Some(selection.clone());
        tab.focused_byte = start;

        let mut tools = EditorTools::default();
        let mut apply = |action, workspace: &mut Workspace| {
            tools.action(action, &ctx, workspace, SqlFormatSettings::default());
        };
        assert_eq!(history_availability(&ctx, &workspace), (false, false));
        assert!(has_selection(&workspace));
        apply(Action::Clear, &mut workspace);
        assert!(workspace.tab().sql.is_empty());
        assert_eq!(workspace.tab().focused_byte, 0);
        assert!(!has_selection(&workspace));
        assert_eq!(history_availability(&ctx, &workspace), (true, false));

        apply(Action::Undo, &mut workspace);
        assert_eq!(workspace.tab().sql, original);
        assert_eq!(workspace.tab().selection, Some(selection));
        assert_eq!(workspace.tab().focused_byte, start);
        assert_eq!(
            TextEdit::load_state(&ctx, id).unwrap().cursor.char_range(),
            Some(reverse)
        );
        assert_eq!(history_availability(&ctx, &workspace), (false, true));

        apply(Action::Redo, &mut workspace);
        assert!(workspace.tab().sql.is_empty());
        assert_eq!(workspace.tab().focused_byte, 0);
        assert!(!has_selection(&workspace));
        assert_eq!(history_availability(&ctx, &workspace), (true, false));

        apply(Action::Undo, &mut workspace);
        apply(Action::Clear, &mut workspace);
        assert_eq!(history_availability(&ctx, &workspace), (true, false));
    }

    #[test]
    fn comments_only_selected_lines_and_preserves_crlf_and_chinese() {
        let mut workspace = Workspace::new("测试".into(), None);
        workspace.tab_mut().sql = "  SELECT '甲';\r\nSELECT '乙';\r\n".into();
        let first_end = workspace.tab().sql.find('\n').unwrap() + 1;
        workspace.tab_mut().selection = Some(2..first_end);
        let ctx = egui::Context::default();
        let mut tools = EditorTools::default();
        tools.action(
            Action::Comment,
            &ctx,
            &mut workspace,
            SqlFormatSettings::default(),
        );
        assert_eq!(workspace.tab().sql, "  -- SELECT '甲';\r\nSELECT '乙';\r\n");
        tools.action(
            Action::Uncomment,
            &ctx,
            &mut workspace,
            SqlFormatSettings::default(),
        );
        assert_eq!(workspace.tab().sql, "  SELECT '甲';\r\nSELECT '乙';\r\n");
    }

    #[test]
    fn formatting_one_statement_is_undoable_without_touching_neighbors() {
        let mut workspace = Workspace::new("测试".into(), None);
        let original = "select 1; select 编号, 名称 from 订单;";
        workspace.tab_mut().sql = original.into();
        workspace.tab_mut().focused_byte = original.find("编号").unwrap();
        let ctx = egui::Context::default();
        let mut tools = EditorTools::default();
        let formatter = SqlFormatSettings {
            comma_first: true,
            indent_spaces: 3,
            ..SqlFormatSettings::default()
        };
        tools.action(Action::FormatCurrent, &ctx, &mut workspace, formatter);
        let tab = workspace.tab();
        assert_ne!(tab.sql, original);
        assert!(tab.sql.starts_with("select 1; "));
        assert!(tab.sql.ends_with(';'));
        assert!(tab.sql.contains("select\n   编号\n   ,名称"), "{}", tab.sql);
        let state = TextEdit::load_state(&ctx, editor_id(&workspace)).unwrap();
        let range = state.cursor.char_range().unwrap();
        let mut undo = state.undoer();
        let (_, restored) = undo.undo(&(range, tab.sql.clone())).unwrap();
        assert_eq!(restored, original);
    }

    #[test]
    fn indenting_uses_the_chosen_tab_width_and_keeps_selected_sql() {
        let mut workspace = Workspace::new("测试".into(), None);
        workspace.tab_mut().sql = "  SELECT 编号;\nSELECT 名称;".into();
        workspace.tab_mut().focused_byte = 2;
        let settings = EditorSettings {
            tab_width: 6,
            indent_with_spaces: true,
            ..EditorSettings::default()
        };
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                change_indentation(ui, &mut workspace, settings, false);
                assert_eq!(workspace.tab().sql, "      SELECT 编号;\nSELECT 名称;");

                let full = 0..workspace.tab().sql.len();
                move_cursor(ctx, &mut workspace, full);
                change_indentation(ui, &mut workspace, settings, false);
                assert_eq!(
                    selected(workspace.tab()),
                    Some("            SELECT 编号;\n      SELECT 名称;")
                );
                change_indentation(ui, &mut workspace, settings, true);
                assert_eq!(
                    selected(workspace.tab()),
                    Some("      SELECT 编号;\nSELECT 名称;")
                );
            });
        });
    }
}
