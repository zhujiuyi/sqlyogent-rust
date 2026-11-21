use std::fmt::Write as _;
use std::fs::File;
use std::io::{Cursor, Read, Write};
use std::ops::Range;
use std::path::Path;

use eframe::egui::{
    self, Button, ColorImage, Key, Modifiers, RichText, TextEdit, TextureHandle,
    text::CCursorRange, text_edit::TextEditState,
};
use encoding_rs::{DecoderResult, Encoding, GBK, SHIFT_JIS, UTF_8};
use image::{ImageFormat, ImageReader, Limits};

use crate::database::Cell;

use super::{
    byte_index,
    editor_tools::cursors,
    preferences::EditorSettings,
    style,
    text_search::{SearchOptions, TextSearch},
};

const IMPORT_LIMIT: usize = 32 * 1024 * 1024;
const EDIT_LIMIT: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Text,
    Hex,
    Image,
}

pub(super) enum Outcome {
    Continue,
    Dismiss,
    Apply(Cell),
}

pub(super) struct Inspector {
    column: String,
    data: Vec<u8>,
    binary: bool,
    nullable: bool,
    editable: bool,
    null: bool,
    page: Page,
    encoding: &'static Encoding,
    text_available: bool,
    input: String,
    focus_text: bool,
    reset_text_state: bool,
    clipped: bool,
    changed: bool,
    error: Option<String>,
    notice: Option<&'static str>,
    search: Search,
    texture: Option<Result<TextureHandle, String>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SearchPanel {
    Find,
    Replace,
}

enum SearchAction {
    Find,
    Continue,
    Replace,
    ReplaceAll,
}

#[derive(Default)]
struct Search {
    panel: Option<SearchPanel>,
    needle: String,
    replacement: String,
    options: SearchOptions,
    up: bool,
    wrap_up: Option<bool>,
    message: Option<String>,
}

impl Search {
    fn reset_feedback(&mut self) {
        self.wrap_up = None;
        self.message = None;
    }

    fn show(
        &mut self,
        ui: &mut egui::Ui,
        id: egui::Id,
        text: &mut String,
        can_replace: bool,
        requested: Option<SearchPanel>,
    ) -> Option<(CCursorRange, bool)> {
        let ctx = ui.ctx().clone();
        let editor_focused = ctx.memory(|memory| memory.has_focus(id));
        let opened = if requested.is_some() || ui.is_sizing_pass() {
            requested
        } else {
            ui.input_mut(|input| {
                if can_replace && editor_focused && input.consume_key(Modifiers::CTRL, Key::H) {
                    Some(SearchPanel::Replace)
                } else if input.consume_key(Modifiers::COMMAND, Key::F) {
                    Some(SearchPanel::Find)
                } else {
                    None
                }
            })
        };
        let replacing = opened.or(self.panel)? == SearchPanel::Replace && can_replace;
        self.panel = Some(if replacing {
            SearchPanel::Replace
        } else {
            SearchPanel::Find
        });
        let mut state = TextEdit::load_state(&ctx, id).unwrap_or_default();
        let cursor = state.cursor.char_range().unwrap_or_default();
        let chars = cursor.as_sorted_char_range();
        let selection = byte_index(text, chars.start)..byte_index(text, chars.end);
        if opened.is_some() {
            let selected = &text[selection.clone()];
            if !selected.is_empty() && !selected.contains(['\r', '\n']) {
                self.needle = selected.to_owned();
            }
            self.up = false;
            self.reset_feedback();
        }
        if !ui.is_sizing_pass()
            && ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape))
            && self.wrap_up.take().is_none()
        {
            self.panel = None;
            self.reset_feedback();
            return None;
        }
        let action = self.controls(ui, opened.is_some(), replacing);
        if self.panel.is_none() {
            ctx.request_repaint();
        }
        let action = action?;
        ctx.request_repaint();
        let Some(search) = TextSearch::new(&self.needle, self.options) else {
            self.message = Some("查找文字过长，无法搜索。".into());
            return None;
        };
        let up = !replacing && self.up;
        self.reset_feedback();
        let mut previous = None;
        let range = match action {
            SearchAction::Find => {
                let from = if up { selection.start } else { selection.end };
                self.find_from(&search, text, from, up)?
            }
            SearchAction::Continue => search.find_in(text, 0..text.len(), !up)?,
            SearchAction::Replace => {
                if search.find_in(text, selection.clone(), true).as_ref() == Some(&selection) {
                    let after = selection.start + self.replacement.len();
                    if text[selection.clone()] != self.replacement {
                        previous = Some(text.clone());
                        text.replace_range(selection, &self.replacement);
                    }
                    self.find_from(&search, text, after, false)
                        .unwrap_or(after..after)
                } else {
                    self.find_from(&search, text, selection.start, false)?
                }
            }
            SearchAction::ReplaceAll => {
                let Some((replaced, count)) = search.replace_all(text, &self.replacement) else {
                    self.message = Some("未找到".into());
                    return None;
                };
                if replaced != *text {
                    previous = Some(std::mem::replace(text, replaced));
                }
                self.message = Some(format!("已替换 {count} 处。"));
                let at = byte_index(text, cursor.primary.index);
                at..at
            }
        };
        let selected = cursors(text, range);
        let changed = previous.is_some();
        if let Some(previous) = previous {
            let mut undo = state.undoer();
            undo.add_undo(&(cursor, previous));
            let current = (selected, text.clone());
            undo.feed_state(ui.input(|input| input.time), &current);
            undo.add_undo(&current);
            state.set_undoer(undo);
        }
        state.cursor.set_char_range(Some(selected));
        state.store(&ctx, id);
        ctx.memory_mut(|memory| memory.request_focus(id));
        Some((selected, changed))
    }

    fn controls(
        &mut self,
        ui: &mut egui::Ui,
        focus: bool,
        replacing: bool,
    ) -> Option<SearchAction> {
        let action = ui
            .horizontal_wrapped(|ui| {
                let query = ui.add(
                    TextEdit::singleline(&mut self.needle)
                        .hint_text("查找文字")
                        .desired_width(150.0)
                        .return_key(None),
                );
                if focus {
                    query.request_focus();
                }
                let replacement = replacing.then(|| {
                    ui.add(
                        TextEdit::singleline(&mut self.replacement)
                            .hint_text("替换为")
                            .desired_width(150.0)
                            .return_key(None),
                    )
                });
                let mut changed =
                    query.changed() || replacement.as_ref().is_some_and(egui::Response::changed);
                changed |= ui
                    .checkbox(&mut self.options.match_case, "区分大小写")
                    .changed();
                changed |= ui
                    .checkbox(&mut self.options.whole_word, "全字匹配")
                    .changed();
                if !replacing {
                    changed |= ui.radio_value(&mut self.up, false, "向下").changed();
                    changed |= ui.radio_value(&mut self.up, true, "向上").changed();
                }
                if changed {
                    self.reset_feedback();
                }
                let enabled = !self.needle.is_empty() && self.wrap_up.is_none();
                let input_focused = query.has_focus()
                    || replacement.as_ref().is_some_and(egui::Response::has_focus);
                let enter = enabled
                    && input_focused
                    && ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Enter));
                let find = ui.add_enabled(enabled, Button::new("查找下一个")).clicked() || enter;
                let replace = replacing && ui.add_enabled(enabled, Button::new("替换")).clicked();
                let replace_all =
                    replacing && ui.add_enabled(enabled, Button::new("全部替换")).clicked();
                if ui.small_button("收起").clicked() {
                    self.panel = None;
                    self.reset_feedback();
                    None
                } else if replace_all {
                    Some(SearchAction::ReplaceAll)
                } else if replace {
                    Some(SearchAction::Replace)
                } else if find {
                    Some(SearchAction::Find)
                } else {
                    None
                }
            })
            .inner;
        if let Some(up) = self.wrap_up {
            ui.horizontal_wrapped(|ui| {
                ui.label(if up {
                    "已到达开头，是否从末尾继续查找？"
                } else {
                    "已到达末尾，是否从开头继续查找？"
                });
                if ui.button("继续").clicked() {
                    return Some(SearchAction::Continue);
                }
                if ui.button("取消").clicked() {
                    self.reset_feedback();
                }
                None
            })
            .inner
        } else {
            if let Some(message) = &self.message {
                ui.label(message);
            }
            action
        }
    }

    fn find_from(
        &mut self,
        search: &TextSearch,
        text: &str,
        from: usize,
        up: bool,
    ) -> Option<Range<usize>> {
        let scope = if up { 0..from } else { from..text.len() };
        if let Some(range) = search.find_in(text, scope, !up) {
            Some(range)
        } else {
            if search.matches(text).next().is_some() {
                self.wrap_up = Some(up);
            } else {
                self.message = Some("未找到".into());
            }
            None
        }
    }
}

impl Inspector {
    pub(super) fn read(column: String, cell: &Cell) -> Self {
        Self::new(column, cell, matches!(cell, Cell::Binary(_)), false, false)
    }

    pub(super) fn edit(column: String, cell: &Cell, binary: bool, nullable: bool) -> Self {
        Self::new(column, cell, binary, nullable, true)
    }

    pub(super) fn edit_typed(column: String, text: String, binary: bool, nullable: bool) -> Self {
        let mut inspector = Self::edit(column, &Cell::Text(text), binary, nullable);
        if inspector.page != Page::Text {
            inspector.prepare(Page::Text);
        }
        inspector.focus_text = true;
        inspector
    }

    fn new(column: String, cell: &Cell, binary: bool, nullable: bool, editable: bool) -> Self {
        let data = match cell {
            Cell::Text(text) => text.as_bytes().to_vec(),
            Cell::Binary(bytes) => bytes.clone(),
            Cell::Null => Vec::new(),
        };
        let mut inspector = Self {
            column,
            data,
            binary,
            nullable,
            editable,
            null: matches!(cell, Cell::Null) && (!editable || nullable),
            page: Page::Text,
            encoding: UTF_8,
            text_available: false,
            input: String::new(),
            focus_text: false,
            reset_text_state: false,
            clipped: false,
            changed: false,
            error: None,
            notice: None,
            search: Search::default(),
            texture: None,
        };
        inspector.prepare(inspector.default_page());
        inspector
    }

    fn default_page(&self) -> Page {
        if !self.binary {
            Page::Text
        } else if image_format(&self.data).is_some() {
            Page::Image
        } else if std::str::from_utf8(&self.data).is_ok_and(|text| {
            text.chars()
                .all(|ch| !ch.is_control() || matches!(ch, '\n' | '\r' | '\t'))
        }) {
            Page::Text
        } else {
            Page::Hex
        }
    }

    fn prepare(&mut self, page: Page) {
        self.page = page;
        self.changed = false;
        self.reset_text_state = true;
        self.search.reset_feedback();
        self.clipped = self.data.len() > EDIT_LIMIT;
        self.text_available = false;
        self.input = match self.page {
            Page::Text => {
                let preview = text_preview(&self.data, self.encoding);
                self.text_available = preview.is_some();
                preview.unwrap_or_default()
            }
            Page::Hex => {
                let preview = &self.data[..self.data.len().min(EDIT_LIMIT)];
                let mut hex = String::with_capacity(preview.len() * 3);
                for (index, byte) in preview.iter().enumerate() {
                    if index > 0 {
                        hex.push(if index % 16 == 0 { '\n' } else { ' ' });
                    }
                    let _ = write!(hex, "{byte:02X}");
                }
                hex
            }
            Page::Image => String::new(),
        };
    }

    fn switch_view(&mut self, page: Page, encoding: &'static Encoding) {
        if self.page == page && self.encoding == encoding {
            return;
        }
        match self.commit_input() {
            Ok(()) => {
                self.encoding = encoding;
                self.prepare(page);
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
    }

    fn commit_input(&mut self) -> Result<(), String> {
        if self.changed && !self.null {
            let bytes = match self.page {
                Page::Text => {
                    let encoding = self.encoding;
                    let input = self.input.as_str();
                    // Distinct byte sequences can decode to the same text.
                    if encoding
                        .decode_without_bom_handling_and_without_replacement(&self.data)
                        .as_deref()
                        == Some(input)
                    {
                        self.changed = false;
                        return Ok(());
                    }
                    let (bytes, _, had_errors) = encoding.encode(input);
                    if had_errors
                        || encoding
                            .decode_without_bom_handling_and_without_replacement(&bytes)
                            .as_deref()
                            != Some(input)
                    {
                        return Err(format!(
                            "当前文本含有无法按 {} 原样保存的字符。",
                            encoding_label(encoding)
                        ));
                    }
                    bytes.into_owned()
                }
                Page::Hex => {
                    let digits = self
                        .input
                        .bytes()
                        .filter(|byte| !byte.is_ascii_whitespace())
                        .collect::<Vec<_>>();
                    if !digits.len().is_multiple_of(2) {
                        return Err("每个字节需要两个十六进制字符。".into());
                    }
                    digits
                        .chunks_exact(2)
                        .map(|pair| {
                            let high = char::from(pair[0]).to_digit(16)?;
                            let low = char::from(pair[1]).to_digit(16)?;
                            Some((high * 16 + low) as u8)
                        })
                        .collect::<Option<Vec<_>>>()
                        .ok_or("十六进制只接受数字 0–9 和字母 A–F。")?
                }
                Page::Image => return Ok(()),
            };
            self.data = bytes;
            self.changed = false;
            self.texture = None;
        }
        Ok(())
    }

    fn import(&mut self, path: &Path) -> Result<(), String> {
        let mut bytes = Vec::new();
        File::open(path)
            .and_then(|file| file.take((IMPORT_LIMIT + 1) as u64).read_to_end(&mut bytes))
            .map_err(|error| format!("无法读取文件。{error}"))?;
        if bytes.len() > IMPORT_LIMIT {
            return Err("单个导入文件最大为 32 MiB。".into());
        }
        if !self.binary && std::str::from_utf8(&bytes).is_err() {
            return Err("文本列需要 UTF-8 文件。".into());
        }
        self.data = bytes;
        self.texture = None;
        self.null = false;
        self.encoding = UTF_8;
        self.prepare(self.default_page());
        self.notice = Some("已载入文件");
        Ok(())
    }

    fn export(&mut self, path: &Path) -> Result<(), String> {
        self.commit_input()?;
        let parent = path.parent().ok_or("请选择有效的文件路径。")?;
        let mut output = tempfile::NamedTempFile::new_in(parent)
            .map_err(|error| format!("无法创建临时文件。{error}"))?;
        output
            .write_all(&self.data)
            .and_then(|()| output.as_file().sync_all())
            .map_err(|error| format!("无法写入文件。{error}"))?;
        output
            .persist_noclobber(path)
            .map_err(|error| format!("无法保存文件。{}", error.error))?;
        self.notice = Some("已保存到文件");
        Ok(())
    }

    fn value(&mut self) -> Result<Cell, String> {
        self.commit_input()?;
        if self.null {
            Ok(Cell::Null)
        } else if self.binary {
            Ok(Cell::Binary(std::mem::take(&mut self.data)))
        } else {
            String::from_utf8(self.data.clone())
                .map(Cell::Text)
                .map_err(|_| "文本列需要 UTF-8 内容。".into())
        }
    }

    pub(super) fn show(&mut self, ctx: &egui::Context, settings: EditorSettings) -> Outcome {
        let available = ctx.content_rect().size();
        let mut outcome = Outcome::Continue;
        let modal = style::modal(ctx, egui::Id::new("cell-value")).show(ctx, |ui| {
            ui.set_width((available.x - 60.0).clamp(360.0, 680.0));
            ui.horizontal(|ui| {
                style::page_title(ui, if self.editable {
                    "编辑单元格内容"
                } else {
                    "查看单元格内容"
                });
                ui.add_space(6.0);
                style::badge(ui, &self.column, style::accent(), style::accent_soft());
            });
            ui.add_space(6.0);
            let (text_available, search_requested) = style::subtle_card(egui::Margin::symmetric(10, 6))
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal_wrapped(|ui| {
                        if self.editable
                            && self.nullable
                            && ui.checkbox(&mut self.null, "设为 NULL").changed()
                        {
                            self.search.reset_feedback();
                        }
                        if !self.null && self.binary {
                            for page in [Page::Text, Page::Hex, Page::Image] {
                                let label = match page {
                                    Page::Text => "文本",
                                    Page::Hex => "十六进制",
                                    Page::Image => "图片",
                                };
                                let selected = self.page == page;
                                let label = if selected {
                                    RichText::new(label).strong().color(style::accent())
                                } else {
                                    RichText::new(label).color(style::muted())
                                };
                                if style::tab_button(ui, selected, label, true).clicked() {
                                    self.switch_view(page, self.encoding);
                                }
                            }
                            if self.page == Page::Text {
                                ui.separator();
                                ui.label(RichText::new("编码").small().color(style::muted()));
                                let mut encoding = self.encoding;
                                egui::ComboBox::from_id_salt("cell-value-encoding")
                                    .width(96.0)
                                    .selected_text(encoding_label(encoding))
                                    .show_ui(ui, |ui| {
                                        for candidate in [UTF_8, GBK, SHIFT_JIS] {
                                            ui.selectable_value(
                                                &mut encoding,
                                                candidate,
                                                encoding_label(candidate),
                                            );
                                        }
                                    });
                                self.switch_view(Page::Text, encoding);
                            }
                        }
                        let text_available = !self.null && self.text_available;
                        if self.binary && text_available {
                            ui.separator();
                        }
                        let find = text_available && ui.small_button("查找…").clicked();
                        let replace = text_available
                            && self.editable
                            && !self.clipped
                            && ui.small_button("替换…").clicked();
                        let requested = if replace {
                            Some(SearchPanel::Replace)
                        } else {
                            find.then_some(SearchPanel::Find)
                        };
                        (text_available, requested)
                    })
                    .inner
                })
                .inner;
            ui.add_space(6.0);
            let height = (available.y * 0.50).clamp(120.0, 430.0);
            if self.null {
                ui.label(
                    RichText::new("NULL")
                        .monospace()
                        .italics()
                        .color(style::muted()),
                );
            } else if self.page == Page::Image {
                let preview = self.texture.get_or_insert_with(|| {
                    decode_image(&self.data).map(|image| {
                        ctx.load_texture("cell-preview", image, egui::TextureOptions::LINEAR)
                    })
                });
                match preview {
                    Ok(texture) => {
                        ui.add(
                            egui::Image::new(&*texture)
                                .max_size(egui::vec2(ui.available_width(), height))
                                .shrink_to_fit(),
                        );
                    }
                    Err(error) => {
                        ui.colored_label(style::warning(), error.as_str());
                    }
                }
            } else if self.page == Page::Text && !text_available {
                ui.colored_label(
                    style::warning(),
                    format!("无法按 {} 解读为文本。", encoding_label(self.encoding)),
                );
            } else {
                let can_edit = self.editable && !self.clipped;
                let id = ui.make_persistent_id("cell-value-text");
                if !ui.is_sizing_pass() && std::mem::take(&mut self.reset_text_state) {
                    TextEditState::default().store(ctx, id);
                }
                let mut range = None;
                if !ui.is_sizing_pass() && std::mem::take(&mut self.focus_text) {
                    let end = self.input.len();
                    let cursor = cursors(&self.input, end..end);
                    let mut state = TextEdit::load_state(ctx, id).unwrap_or_default();
                    state.cursor.set_char_range(Some(cursor));
                    state.store(ctx, id);
                    ctx.memory_mut(|memory| memory.request_focus(id));
                    range = Some(cursor);
                }
                let search_was_open = self.search.panel.is_some();
                if let Some((selected, changed)) = self.search.show(
                    ui,
                    id,
                    &mut self.input,
                    can_edit && self.page == Page::Text,
                    search_requested,
                ) {
                    range = Some(selected);
                    self.changed |= changed;
                }
                let focus_editor =
                    range.is_some() || (search_was_open && self.search.panel.is_none());
                let width = ui.available_width();
                let wrap = self.page != Page::Text || settings.word_wrap;
                let font = settings.blob_font.id();
                if self.clipped {
                    ui.colored_label(style::warning(), "仅预览前 64 KiB。可将完整内容保存到文件。");
                }
                egui::ScrollArea::new([!wrap, true])
                    .id_salt("cell-value-data")
                    .max_height(height)
                    .show(ui, |ui| {
                        let edit = |ui: &mut egui::Ui, buffer: &mut dyn egui::TextBuffer| {
                            let mut layouter =
                                |ui: &egui::Ui, text: &dyn egui::TextBuffer, width: f32| {
                                    let job = egui::text::LayoutJob::simple(
                                        text.as_str().to_owned(),
                                        font.clone(),
                                        style::ink(),
                                        if wrap { width } else { f32::INFINITY },
                                    );
                                    ui.fonts_mut(|fonts| fonts.layout_job(job))
                                };
                            TextEdit::multiline(buffer)
                                .id(id)
                                .font(font.clone())
                                .background_color(style::panel())
                                .margin(egui::Margin::symmetric(9, 7))
                                .layouter(&mut layouter)
                                .cursor_at_end(self.page != Page::Text)
                                .desired_rows(10)
                                .desired_width(width)
                                .show(ui)
                        };
                        let rendered = if can_edit {
                            edit(ui, &mut self.input)
                        } else {
                            edit(ui, &mut self.input.as_str())
                        };
                        if can_edit && rendered.response.changed() {
                            self.changed = true;
                            self.search.reset_feedback();
                        }
                        if focus_editor {
                            rendered.response.request_focus();
                            ctx.request_repaint();
                        }
                        if let Some(range) = range {
                            let cursor = rendered.galley.pos_from_cursor(range.primary);
                            ui.scroll_to_rect(
                                cursor.translate(rendered.galley_pos.to_vec2()),
                                None,
                            );
                        }
                    });
            }
            if let Some(error) = &self.error {
                ui.colored_label(style::error(), error);
            } else if let Some(notice) = self.notice {
                ui.colored_label(style::success(), notice);
            }
            let editable = self.editable;
            let writable = !self.null;
            let (_, (dismiss, apply)) = style::dialog_footer_split(
                ui,
                |ui| {
                    if editable
                        && ui.button("从文件载入…").clicked()
                        && let Some(path) =
                            rfd::FileDialog::new().set_title("选择内容文件").pick_file()
                    {
                        self.error = self.import(&path).err();
                    }
                    if ui
                        .add_enabled(writable, egui::Button::new("保存到新文件…"))
                        .clicked()
                        && let Some(path) = rfd::FileDialog::new()
                            .set_title("保存单元格内容")
                            .save_file()
                    {
                        self.error = self.export(&path).err();
                    }
                },
                |ui| {
                    let dismiss = ui.button(if editable { "取消" } else { "关闭" }).clicked();
                    let apply = editable && ui.add(style::primary("应用到行草稿")).clicked();
                    (dismiss, apply)
                },
            );
            if dismiss {
                outcome = Outcome::Dismiss;
            }
            if apply {
                match self.value() {
                    Ok(cell) => outcome = Outcome::Apply(cell),
                    Err(error) => self.error = Some(error),
                }
            }
        });
        if !self.editable && modal.should_close() {
            Outcome::Dismiss
        } else {
            outcome
        }
    }
}

fn encoding_label(encoding: &'static Encoding) -> &'static str {
    if encoding == GBK {
        "GBK / GB2312"
    } else {
        encoding.name()
    }
}

fn text_preview(data: &[u8], encoding: &'static Encoding) -> Option<String> {
    if encoding == UTF_8 {
        let text = std::str::from_utf8(data).ok()?;
        return Some(text[..text.floor_char_boundary(EDIT_LIMIT)].to_owned());
    }
    let end = data.len().min(EDIT_LIMIT);
    let mut decoder = encoding.new_decoder_without_bom_handling();
    let mut text = String::with_capacity(decoder.max_utf8_buffer_length_without_replacement(end)?);
    let (result, _) =
        decoder.decode_to_string_without_replacement(&data[..end], &mut text, end == data.len());
    matches!(result, DecoderResult::InputEmpty).then_some(text)
}

fn image_format(bytes: &[u8]) -> Option<ImageFormat> {
    image::guess_format(bytes).ok().filter(|format| {
        matches!(
            format,
            ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Bmp
        )
    })
}

fn decode_image(bytes: &[u8]) -> Result<ColorImage, String> {
    let format = image_format(bytes).ok_or("可预览 PNG、JPEG 和 BMP 图片。")?;
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|error| format!("无法预览图片。{error}"))?
        .into_rgba8();
    Ok(ColorImage::from_rgba_unmultiplied(
        [image.width() as usize, image.height() as usize],
        image.as_raw(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::{
        Command, ConnectionConfig, DbObject, FieldValue, ObjectKind, Response, TableEdit, Worker,
    };
    use mysql::prelude::Queryable;

    fn frame(
        ctx: &egui::Context,
        inspector: &mut Inspector,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 660.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                assert!(matches!(
                    inspector.show(ctx, EditorSettings::default()),
                    Outcome::Continue
                ));
            },
        )
    }

    fn click(ctx: &egui::Context, inspector: &mut Inspector, label: &str) {
        fn find(shape: &egui::Shape, label: &str) -> Option<egui::Pos2> {
            match shape {
                egui::Shape::Text(text) if text.galley.text() == label => {
                    Some(text.pos + text.galley.rect.size() / 2.0)
                }
                egui::Shape::Vec(shapes) => shapes.iter().find_map(|shape| find(shape, label)),
                _ => None,
            }
        }

        let pos = frame(ctx, inspector, Vec::new())
            .shapes
            .iter()
            .find_map(|shape| find(&shape.shape, label))
            .unwrap_or_else(|| panic!("未显示 {label}"));
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        frame(
            ctx,
            inspector,
            vec![egui::Event::PointerMoved(pos), button(true)],
        );
        frame(ctx, inspector, vec![button(false)]);
    }

    fn key(key: Key, modifiers: Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn legacy_blob_text_preserves_original_bytes_and_saves_edits_in_its_encoding() {
        let cases = [
            (
                GBK,
                b"\xd6\xd0\xce\xc4",
                "中文",
                "测",
                b"\xd6\xd0\xce\xc4\xb2\xe2",
            ),
            (
                SHIFT_JIS,
                b"\x93\xfa\x96\x7b",
                "日本",
                "語",
                b"\x93\xfa\x96\x7b\x8c\xea",
            ),
        ];
        for (encoding, original, text, suffix, expected) in cases {
            let cell = Cell::Binary(original.to_vec());
            let mut inspector = Inspector::edit("内容".into(), &cell, true, false);
            assert_eq!(inspector.page, Page::Hex);
            inspector.switch_view(Page::Text, encoding);
            assert_eq!(inspector.input, text);
            assert_eq!(inspector.value().unwrap(), cell);

            let mut inspector = Inspector::edit("内容".into(), &cell, true, false);
            inspector.switch_view(Page::Text, encoding);
            inspector.input.push_str(suffix);
            inspector.changed = true;
            let edited = inspector.value().unwrap();
            assert_eq!(edited, Cell::Binary(expected.to_vec()));

            let mut viewer = Inspector::read("内容".into(), &edited);
            viewer.switch_view(Page::Text, encoding);
            assert_eq!(viewer.input, format!("{text}{suffix}"));
        }
    }

    #[test]
    fn typed_cell_opens_non_null_with_focus_and_appends_at_the_end() {
        for binary in [false, true] {
            let ctx = egui::Context::default();
            let mut inspector = Inspector::edit_typed("内容".into(), "初始".into(), binary, true);
            assert!(!inspector.null);
            assert_eq!(inspector.page, Page::Text);

            frame(&ctx, &mut inspector, Vec::new());
            frame(&ctx, &mut inspector, Vec::new());
            let focused = ctx.memory(|memory| memory.focused()).unwrap();
            let cursor = TextEdit::load_state(&ctx, focused)
                .unwrap()
                .cursor
                .char_range()
                .unwrap();
            assert_eq!(cursor.primary.index, 2);
            assert_eq!(cursor.secondary.index, 2);
            frame(&ctx, &mut inspector, vec![egui::Event::Text("追加".into())]);
            let expected = if binary {
                Cell::Binary("初始追加".as_bytes().to_vec())
            } else {
                Cell::Text("初始追加".into())
            };
            assert_eq!(inspector.value().unwrap(), expected);
        }
    }

    #[test]
    fn readonly_text_finds_whole_words_by_case_and_confirms_backward_wrap() {
        let ctx = egui::Context::default();
        let source = "甲 CAT catalog cat cat";
        let mut inspector = Inspector::read("内容".into(), &Cell::Text(source.into()));
        frame(&ctx, &mut inspector, Vec::new());
        click(&ctx, &mut inspector, "查找…");
        frame(&ctx, &mut inspector, vec![egui::Event::Text("cat".into())]);
        click(&ctx, &mut inspector, "区分大小写");
        click(&ctx, &mut inspector, "全字匹配");
        click(&ctx, &mut inspector, "查找下一个");

        let id = ctx.memory(|memory| memory.focused()).unwrap();
        let selection = || {
            TextEdit::load_state(&ctx, id)
                .unwrap()
                .cursor
                .char_range()
                .unwrap()
                .as_sorted_char_range()
        };
        assert_eq!(selection(), 14..17);
        click(&ctx, &mut inspector, "向上");
        click(&ctx, &mut inspector, "查找下一个");
        assert_eq!(inspector.search.wrap_up, Some(true));
        assert_eq!(selection(), 14..17);
        click(&ctx, &mut inspector, "继续");
        assert_eq!(selection(), 18..21);

        frame(&ctx, &mut inspector, vec![key(Key::H, Modifiers::CTRL)]);
        assert!(matches!(inspector.search.panel, Some(SearchPanel::Find)));
        assert!(!inspector.changed);
        assert_eq!(inspector.input, source);
    }

    #[test]
    fn text_replacement_stays_in_the_inspector_and_all_undoes_in_one_step() {
        use egui::os::OperatingSystem;

        for os in [OperatingSystem::Mac, OperatingSystem::Windows] {
            let ctx = egui::Context::default();
            ctx.set_os(os);
            let source = "猫 cat catalog cat cat";
            let mut inspector = Inspector::edit_typed("内容".into(), source.into(), false, true);
            // 先完成对话框布局及焦点过滤器注册，再发送导航键。
            for _ in 0..3 {
                frame(&ctx, &mut inspector, Vec::new());
            }
            let id = ctx.memory(|memory| memory.focused()).unwrap();
            assert!(
                TextEdit::load_state(&ctx, id).is_some(),
                "{os:?} 文本框应已取得焦点"
            );
            let (home, modifiers) = if os == OperatingSystem::Mac {
                (Key::ArrowUp, Modifiers::MAC_CMD | Modifiers::COMMAND)
            } else {
                (Key::Home, Modifiers::CTRL | Modifiers::COMMAND)
            };
            frame(&ctx, &mut inspector, vec![key(home, modifiers)]);
            assert_eq!(
                ctx.memory(|memory| memory.focused()),
                Some(id),
                "{os:?} 导航不应离开文本框"
            );
            assert_eq!(
                TextEdit::load_state(&ctx, id)
                    .unwrap()
                    .cursor
                    .char_range()
                    .unwrap()
                    .primary
                    .index,
                0,
                "{os:?} 应使用本机快捷键移动到文本开头"
            );
            frame(&ctx, &mut inspector, vec![key(Key::H, Modifiers::CTRL)]);
            frame(&ctx, &mut inspector, vec![egui::Event::Text("cat".into())]);
            click(&ctx, &mut inspector, "替换为");
            frame(&ctx, &mut inspector, vec![egui::Event::Text("犬".into())]);
            click(&ctx, &mut inspector, "全字匹配");
            click(&ctx, &mut inspector, "替换");
            assert_eq!(inspector.input, source);
            click(&ctx, &mut inspector, "替换");
            let one_replaced = "猫 犬 catalog cat cat";
            assert_eq!(inspector.input, one_replaced);
            click(&ctx, &mut inspector, "全部替换");
            assert_eq!(inspector.input, "猫 犬 catalog 犬 犬");
            assert_eq!(inspector.data, source.as_bytes());
            assert_eq!(inspector.search.message.as_deref(), Some("已替换 2 处。"));

            frame(&ctx, &mut inspector, vec![key(Key::Z, modifiers)]);
            assert_eq!(inspector.input, one_replaced);
            assert_eq!(inspector.value().unwrap(), Cell::Text(one_replaced.into()));
        }
    }

    #[test]
    #[ignore = "requires a disposable MariaDB on loopback via HARBOR_EDITABLE_FIXTURE_PORT"]
    fn imports_previews_exports_and_saves_an_image_without_altering_bytes() {
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("图像.png");
        image::RgbaImage::from_pixel(3, 2, image::Rgba([16, 40, 88, 255]))
            .save(&source)
            .unwrap();
        let mut inspector = Inspector::edit("附件".into(), &Cell::Null, true, true);
        inspector.import(&source).unwrap();
        assert_eq!(inspector.page, Page::Image);
        assert_eq!(decode_image(&inspector.data).unwrap().size, [3, 2]);
        let output = fixture.path().join("附件.png");
        inspector.export(&output).unwrap();
        let stored = std::fs::read(source).unwrap();
        assert_eq!(std::fs::read(output).unwrap(), stored);
        let Cell::Binary(draft) = inspector.value().unwrap() else {
            panic!("expected a binary draft");
        };
        assert_eq!(draft, stored);

        let port = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let mut sql = mysql::Conn::new(
            mysql::OptsBuilder::new()
                .ip_or_hostname(Some("127.0.0.1"))
                .tcp_port(port)
                .user(Some("root"))
                .prefer_socket(false),
        )
        .unwrap();
        let schema = format!("harbor_blob_{}", std::process::id());
        sql.query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
            .unwrap();
        sql.select_db(&schema).unwrap();
        sql.query_drop(
            "CREATE TABLE attachments (
                id INT PRIMARY KEY, payload LONGBLOB, note VARCHAR(32) NOT NULL
            ) ENGINE=InnoDB",
        )
        .unwrap();
        sql.query_drop("INSERT INTO attachments VALUES (1, NULL, 'retained')")
            .unwrap();
        let object = DbObject {
            schema: schema.clone(),
            name: "attachments".into(),
            kind: ObjectKind::Table,
        };
        let worker = Worker::new();
        let request = |id, command| {
            worker.send(id, command).unwrap();
            let event = worker
                .recv_timeout(std::time::Duration::from_secs(8))
                .unwrap()
                .unwrap();
            assert_eq!(event.id, id);
            event.result.unwrap()
        };
        assert!(matches!(
            request(
                1,
                Command::Connect(ConnectionConfig {
                    port,
                    ..ConnectionConfig::default()
                })
            ),
            Response::Connected(_)
        ));
        let Response::EditablePage(page) = request(
            2,
            Command::QueryEditable {
                sql: format!(
                    "SELECT a.payload AS attachment, a.id AS row_key
                     FROM {schema}.attachments AS a WHERE a.id = 1 ORDER BY row_key"
                ),
                limit: 10,
            },
        ) else {
            panic!("expected the attachment page");
        };
        assert!(!page.can_insert);
        assert_eq!(page.column_labels, ["attachment", "row_key"]);
        assert_eq!(page.primary_key, [1]);
        assert_eq!(page.rows[0].cells[0], Cell::Null);
        assert_eq!(
            page.rows[0].original()[2],
            mysql::Value::Bytes(b"retained".to_vec())
        );
        let edit = TableEdit::Update {
            original: page.rows[0].clone(),
            values: vec![("payload".into(), FieldValue::Binary(draft))],
        };
        assert!(matches!(
            request(3, Command::EditTable { object, edit }),
            Response::TableEdited { affected_rows: 1 }
        ));
        let (persisted, note) = sql
            .query_first::<(Vec<u8>, String), _>(
                "SELECT payload, note FROM attachments WHERE id = 1",
            )
            .unwrap()
            .unwrap();
        sql.query_drop(format!("DROP DATABASE {schema}")).unwrap();
        assert_eq!(persisted, stored);
        assert_eq!(note, "retained");
    }
}
