use eframe::egui::{
    self, Color32, FontData, FontDefinitions, FontFamily, FontId, RichText, Stroke, TextStyle,
    text::{Galley, LayoutJob, LayoutSection, TextFormat},
};
use std::{
    borrow::Cow,
    ops::Range,
    sync::{Arc, OnceLock},
};

use crate::sql_tools::{EditorTokenKind, editor_tokens};

use super::{
    completion::vocabulary,
    preferences::{EditorSettings, FontRole, LetterCase, Preferences},
};

pub const INK: Color32 = Color32::from_rgb(24, 36, 54);
pub const MUTED: Color32 = Color32::from_rgb(78, 94, 116);
pub const ACCENT: Color32 = Color32::from_rgb(46, 83, 192);
pub const ACCENT_SOFT: Color32 = Color32::from_rgb(227, 236, 255);
pub const ERROR: Color32 = Color32::from_rgb(160, 45, 54);
pub const SUCCESS: Color32 = Color32::from_rgb(30, 110, 69);
pub const WARNING: Color32 = Color32::from_rgb(128, 80, 27);
pub const SURFACE: Color32 = Color32::from_rgb(255, 255, 255);
pub const PANEL: Color32 = Color32::from_rgb(243, 246, 251);
pub const BORDER: Color32 = Color32::from_rgb(215, 224, 237);

pub fn bar(margin: egui::Margin) -> egui::Frame {
    egui::Frame::new().fill(SURFACE).inner_margin(margin)
}

pub fn pane(margin: egui::Margin) -> egui::Frame {
    egui::Frame::new().fill(PANEL).inner_margin(margin)
}

pub(super) fn modal(ctx: &egui::Context, id: egui::Id) -> egui::Modal {
    let frame = egui::Frame::window(&ctx.style()).inner_margin(egui::Margin::same(16));
    egui::Modal::new(id)
        .frame(frame)
        .backdrop_color(Color32::from_rgba_unmultiplied(15, 28, 49, 96))
}

pub(super) fn menu(ui: &mut egui::Ui, label: &str, entries: impl FnOnce(&mut egui::Ui)) {
    ui.menu_button(label, |ui| {
        let height = (ui.ctx().content_rect().height() - 108.0).max(76.0);
        egui::ScrollArea::vertical()
            .id_salt(("command-menu", label))
            .max_height(height)
            .show(ui, entries);
    });
}

pub fn primary(label: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(label).strong().color(SURFACE))
        .fill(ACCENT)
        .stroke(Stroke::NONE)
}

pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Light);
    let mut visuals = egui::Visuals::light();
    visuals.panel_fill = PANEL;
    visuals.window_fill = SURFACE;
    visuals.window_stroke = Stroke::new(1.0_f32, BORDER);
    visuals.window_corner_radius = egui::CornerRadius::same(12);
    visuals.window_shadow = egui::epaint::Shadow {
        offset: [0, 10],
        blur: 24,
        spread: 1,
        color: Color32::from_black_alpha(36),
    };
    visuals.menu_corner_radius = egui::CornerRadius::same(9);
    visuals.popup_shadow = egui::epaint::Shadow {
        offset: [0, 5],
        blur: 16,
        spread: 1,
        color: Color32::from_black_alpha(38),
    };
    visuals.extreme_bg_color = SURFACE;
    visuals.faint_bg_color = Color32::from_rgb(249, 251, 254);
    visuals.code_bg_color = PANEL;
    visuals.hyperlink_color = ACCENT;
    visuals.weak_text_color = Some(MUTED);
    visuals.warn_fg_color = WARNING;
    visuals.error_fg_color = ERROR;
    visuals.selection.bg_fill = ACCENT_SOFT;
    visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);

    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    widgets.noninteractive.fg_stroke.color = INK;
    widgets.inactive.bg_fill = SURFACE;
    widgets.inactive.weak_bg_fill = PANEL;
    widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    widgets.inactive.fg_stroke.color = INK;
    widgets.hovered.bg_fill = ACCENT_SOFT;
    widgets.hovered.weak_bg_fill = ACCENT_SOFT;
    widgets.hovered.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(169, 190, 235));
    widgets.hovered.fg_stroke.color = INK;
    widgets.active.weak_bg_fill = Color32::from_rgb(214, 227, 253);
    widgets.active.bg_fill = widgets.active.weak_bg_fill;
    widgets.active.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    widgets.active.fg_stroke.color = ACCENT;
    widgets.open.bg_fill = ACCENT_SOFT;
    widgets.open.weak_bg_fill = ACCENT_SOFT;
    widgets.open.bg_stroke = Stroke::new(1.0_f32, BORDER);
    widgets.open.fg_stroke.color = INK;
    for widget in [
        &mut widgets.noninteractive,
        &mut widgets.inactive,
        &mut widgets.hovered,
        &mut widgets.active,
        &mut widgets.open,
    ] {
        widget.corner_radius = egui::CornerRadius::same(6);
    }
    ctx.set_visuals(visuals);
    ctx.style_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 5.0);
        style.spacing.button_padding = egui::vec2(9.0, 4.0);
        style.spacing.interact_size.y = 25.0;
        style.spacing.window_margin = egui::Margin::same(14);
        style.spacing.menu_margin = egui::Margin::same(9);
        for (role, size) in [
            (TextStyle::Small, 12.0),
            (TextStyle::Body, 14.0),
            (TextStyle::Button, 14.0),
            (TextStyle::Heading, 20.0),
        ] {
            style.text_styles.insert(role, FontId::proportional(size));
        }
        style
            .text_styles
            .insert(TextStyle::Monospace, FontId::monospace(14.0));
    });
}

fn base_fonts() -> &'static FontDefinitions {
    static FONTS: OnceLock<FontDefinitions> = OnceLock::new();
    FONTS.get_or_init(|| {
        #[cfg(windows)]
        let mut font_candidates = std::env::var_os("WINDIR")
            .or_else(|| std::env::var_os("SystemRoot"))
            .map(|dir| std::path::PathBuf::from(dir).join("Fonts"))
            .into_iter()
            .flat_map(|dir| {
                ["msyh.ttc", "deng.ttf", "simhei.ttf", "simsun.ttc"].map(|file| dir.join(file))
            });
        #[cfg(target_os = "macos")]
        let mut font_candidates = [
            "/System/Library/Fonts/PingFang.ttc",
            // Recent macOS versions store system PingFang in a private outline format.
            "/System/Library/Fonts/Hiragino Sans GB.ttc",
            "/System/Library/Fonts/STHeiti Medium.ttc",
            "/System/Library/Fonts/STHeiti Light.ttc",
        ]
        .into_iter()
        .map(std::path::PathBuf::from);
        #[cfg(not(any(windows, target_os = "macos")))]
        let mut font_candidates = std::iter::empty::<std::path::PathBuf>();

        let mut fonts = FontDefinitions::default();
        #[cfg(windows)]
        let ui_font = std::env::var_os("WINDIR")
            .or_else(|| std::env::var_os("SystemRoot"))
            .map(|dir| std::path::PathBuf::from(dir).join("Fonts/segoeui.ttf"));
        #[cfg(target_os = "macos")]
        let ui_font = Some(std::path::PathBuf::from("/System/Library/Fonts/SFNS.ttf"));
        #[cfg(not(any(windows, target_os = "macos")))]
        let ui_font = None::<std::path::PathBuf>;
        if let Some(bytes) = ui_font
            .and_then(|path| std::fs::read(path).ok())
            .filter(|bytes| ttf_parser::Face::parse(bytes, 0).is_ok())
        {
            let name = "System UI".to_owned();
            fonts
                .font_data
                .insert(name.clone(), FontData::from_owned(bytes).into());
            fonts
                .families
                .entry(FontFamily::Proportional)
                .or_default()
                .insert(0, name);
        }
        if let Some(bytes) = font_candidates.find_map(|path| {
            let bytes = std::fs::read(path).ok()?;
            ttf_parser::Face::parse(&bytes, 0).ok()?;
            Some(bytes)
        }) {
            let name = "System CJK".to_owned();
            fonts
                .font_data
                .insert(name.clone(), FontData::from_owned(bytes).into());
            for family in [FontFamily::Proportional, FontFamily::Monospace] {
                fonts.families.entry(family).or_default().push(name.clone());
            }
        }
        // The preference dialog may queue a font change at the end of a frame.
        // Reserve these families in the live atlas before any selection uses them.
        let fallback = fonts
            .families
            .get(&FontFamily::Monospace)
            .cloned()
            .unwrap_or_default();
        for role in FontRole::ALL {
            fonts
                .families
                .insert(FontFamily::Name(role.font_name().into()), fallback.clone());
        }
        fonts
    })
}

pub(super) fn apply_font_preferences(
    ctx: &egui::Context,
    preferences: &mut Preferences,
) -> Vec<String> {
    let mut definitions = base_fonts().clone();
    let mut errors = Vec::new();
    for (role, font, face) in [
        (
            FontRole::Editor,
            &mut preferences.editor.font,
            &preferences.fonts.editor,
        ),
        (
            FontRole::History,
            &mut preferences.editor.history_font,
            &preferences.fonts.history,
        ),
        (
            FontRole::Blob,
            &mut preferences.editor.blob_font,
            &preferences.fonts.blob,
        ),
        (
            FontRole::Grid,
            &mut preferences.editor.grid_font,
            &preferences.fonts.grid,
        ),
    ] {
        font.system_role = None;
        let Some(face) = face else {
            continue;
        };
        let data = match face.data() {
            Ok(data) => data,
            Err(error) => {
                errors.push(format!(
                    "{}字体「{}」加载失败，已使用内置字体。{error}",
                    role.label(),
                    face.label()
                ));
                continue;
            }
        };
        let name = role.font_name().to_owned();
        let mut family = definitions
            .families
            .get(&font.builtin_family())
            .cloned()
            .unwrap_or_default();
        family.insert(0, name.clone());
        definitions.font_data.insert(name.clone(), data.into());
        definitions
            .families
            .insert(FontFamily::Name(name.into()), family);
        font.system_role = Some(role);
    }
    ctx.set_fonts(definitions);
    ctx.style_mut(|style| {
        style
            .text_styles
            .insert(grid_font_style(), preferences.editor.grid_font.id());
    });
    errors
}

fn grid_font_style() -> TextStyle {
    TextStyle::Name(FontRole::Grid.font_name().into())
}

pub(super) fn grid_font(ui: &egui::Ui) -> Option<FontId> {
    ui.style().text_styles.get(&grid_font_style()).cloned()
}

pub(super) fn apply_grid_font(ui: &mut egui::Ui) {
    if let Some(font) = grid_font(ui) {
        ui.style_mut().override_font_id = Some(font);
    }
}

pub(super) fn grid_row_height(ui: &mut egui::Ui, default: f32) -> f32 {
    grid_font(ui).map_or(default, |font| {
        default.max(ui.fonts_mut(|fonts| fonts.row_height(&font)) + 6.0)
    })
}

pub(super) const KEYWORDS: &[&str] = &[
    "ADD",
    "ALTER",
    "ANALYZE",
    "AND",
    "AS",
    "ASC",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CALL",
    "CASE",
    "CHECK",
    "COMMIT",
    "CREATE",
    "DATABASE",
    "DECLARE",
    "DEFAULT",
    "DELETE",
    "DELIMITER",
    "DESC",
    "DESCRIBE",
    "DISTINCT",
    "DROP",
    "ELSE",
    "END",
    "EVENT",
    "EXISTS",
    "EXPLAIN",
    "FALSE",
    "FOREIGN",
    "FROM",
    "FUNCTION",
    "GRANT",
    "GROUP",
    "HAVING",
    "IF",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NOT",
    "NULL",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "PRIMARY",
    "PROCEDURE",
    "REFERENCES",
    "REPLACE",
    "REVOKE",
    "RIGHT",
    "ROLLBACK",
    "SELECT",
    "SET",
    "SHOW",
    "TABLE",
    "THEN",
    "TRIGGER",
    "TRUE",
    "TRUNCATE",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USE",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WITH",
];

pub fn highlight(sql: &str, width: f32) -> LayoutJob {
    sql_layout_job(sql, width, EditorSettings::default(), (false, false))
}

pub(super) fn sql_galley(
    ui: &egui::Ui,
    sql: &str,
    width: f32,
    settings: EditorSettings,
    quoting: (bool, bool),
) -> Arc<Galley> {
    let mut job = sql_layout_job(sql, width, settings, quoting);
    ui.fonts_mut(|fonts| {
        if job.text.contains('\t') {
            layout_tabs(fonts, &mut job, &settings.font.id(), settings.tab_width);
        }
        fonts.layout_job(job)
    })
}

pub(super) fn sql_file_text(
    sql: &str,
    settings: EditorSettings,
    quoting: (bool, bool),
) -> Cow<'_, str> {
    if settings.changes_case() {
        Cow::Owned(sql_layout_job(sql, f32::INFINITY, settings, quoting).text)
    } else {
        Cow::Borrowed(sql)
    }
}

pub(super) fn apply_case_to_clipboard(
    ui: &egui::Ui,
    first_command: usize,
    sql: &str,
    selection: Range<usize>,
    settings: EditorSettings,
    quoting: (bool, bool),
) {
    let selected = &sql[selection.clone()];
    ui.output_mut(|output| {
        let mut converted = None;
        for command in &mut output.commands[first_command..] {
            if let egui::OutputCommand::CopyText(text) = command
                && text == selected
            {
                let changed = converted.get_or_insert_with(|| {
                    sql_file_text(sql, settings, quoting)[selection.clone()].to_owned()
                });
                text.clone_from(changed);
            }
        }
    });
}

fn sql_layout_job(
    sql: &str,
    width: f32,
    settings: EditorSettings,
    (no_backslash_escapes, ansi_quotes): (bool, bool),
) -> LayoutJob {
    let colors = settings.colors;
    let normal = rgb(colors.normal);
    let font = settings.font.id();
    let mut job = LayoutJob {
        text: sql.to_owned(),
        ..LayoutJob::default()
    };
    if settings.word_wrap {
        job.wrap.max_width = width;
    }
    let mut cursor = 0;
    let mut previous_is_prefix = false;
    let mut expressions = vec![false];
    let mut tokens = editor_tokens(sql, no_backslash_escapes, ansi_quotes).peekable();
    while let Some(token) = tokens.next() {
        section(&mut job, cursor..token.range.start, &font, normal);
        let next = tokens.peek().map(|token| token.kind);
        let unqualified = !previous_is_prefix && next != Some(EditorTokenKind::Symbol('.'));
        let (color, case) = match token.kind {
            EditorTokenKind::Protected { .. } => {
                let color = if token.text.starts_with(['\'', '"']) {
                    colors.string
                } else {
                    colors.comment
                };
                (rgb(color), LetterCase::AsTyped)
            }
            EditorTokenKind::Word if token.text.bytes().all(|byte| byte.is_ascii_digit()) => {
                (Color32::from_rgb(104, 67, 144), LetterCase::AsTyped)
            }
            EditorTokenKind::Word
                if unqualified
                    && next == Some(EditorTokenKind::Symbol('('))
                    && expressions.last() == Some(&true)
                    && vocabulary::parameters(token.text).is_some() =>
            {
                (rgb(colors.function), settings.function_case)
            }
            EditorTokenKind::Word if unqualified && vocabulary::is_keyword(token.text) => {
                let case = if is_reserved_keyword(token.text) {
                    settings.keyword_case
                } else {
                    LetterCase::AsTyped
                };
                (rgb(colors.keyword), case)
            }
            _ => (normal, LetterCase::AsTyped),
        };
        // Draw case changes on the same byte and character boundaries as the source text.
        match case {
            LetterCase::AsTyped => {}
            LetterCase::Upper => job.text[token.range.clone()].make_ascii_uppercase(),
            LetterCase::Lower => job.text[token.range.clone()].make_ascii_lowercase(),
        }
        cursor = token.range.end;
        section(&mut job, token.range, &font, color);
        if matches!(token.kind, EditorTokenKind::Protected { .. })
            && !token.text.starts_with(['\'', '"'])
        {
            continue;
        }
        previous_is_prefix = matches!(token.kind, EditorTokenKind::Symbol('.' | '@' | ':'));
        match token.kind {
            EditorTokenKind::Symbol('(') => {
                expressions.push(expressions.last() == Some(&true));
            }
            EditorTokenKind::Symbol(')') if expressions.len() > 1 => {
                expressions.pop();
            }
            EditorTokenKind::Symbol(';') => {
                expressions.truncate(1);
                expressions[0] = false;
            }
            EditorTokenKind::Word if unqualified => {
                let expression = expressions
                    .last_mut()
                    .expect("at least the outer statement");
                if sql_word_in(
                    token.text,
                    &[
                        "SELECT", "WHERE", "HAVING", "ON", "GROUP", "ORDER", "VALUES", "SET",
                        "RETURN", "THEN",
                    ],
                ) {
                    *expression = true;
                } else if sql_word_in(token.text, &["FROM", "JOIN", "INTO", "USE"]) {
                    *expression = false;
                }
            }
            _ => {}
        }
    }
    section(&mut job, cursor..sql.len(), &font, normal);
    job
}

fn sql_word_in(word: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| word.eq_ignore_ascii_case(candidate))
}

fn is_reserved_keyword(word: &str) -> bool {
    // Nonreserved vocabulary may spell a table or column name. Only fold common
    // reserved words whose use as identifiers requires quoting in both dialects.
    sql_word_in(
        word,
        &[
            "ADD",
            "ALL",
            "ALTER",
            "ANALYZE",
            "AND",
            "AS",
            "ASC",
            "BETWEEN",
            "CASE",
            "CHECK",
            "CREATE",
            "CROSS",
            "DECLARE",
            "DEFAULT",
            "DELETE",
            "DESC",
            "DESCRIBE",
            "DISTINCT",
            "DROP",
            "ELSE",
            "EXISTS",
            "EXPLAIN",
            "FALSE",
            "FOR",
            "FOREIGN",
            "FROM",
            "GRANT",
            "GROUP",
            "HAVING",
            "IF",
            "IN",
            "INDEX",
            "INNER",
            "INSERT",
            "INTO",
            "IS",
            "JOIN",
            "KEY",
            "LEFT",
            "LIKE",
            "LIMIT",
            "NOT",
            "NULL",
            "ON",
            "OR",
            "ORDER",
            "OUTER",
            "PRIMARY",
            "REFERENCES",
            "REPLACE",
            "REVOKE",
            "RIGHT",
            "SELECT",
            "SET",
            "SHOW",
            "TABLE",
            "THEN",
            "TRIGGER",
            "TRUE",
            "UNION",
            "UNIQUE",
            "UPDATE",
            "USE",
            "USING",
            "VALUES",
            "WHEN",
            "WHERE",
        ],
    )
}

fn rgb([r, g, b]: [u8; 3]) -> Color32 {
    Color32::from_rgb(r, g, b)
}

fn section(job: &mut LayoutJob, range: Range<usize>, font: &FontId, color: Color32) {
    if range.is_empty() {
        return;
    }
    if let Some(previous) = job.sections.last_mut()
        && previous.format.color == color
    {
        previous.byte_range.end = range.end;
    } else {
        job.sections.push(LayoutSection {
            leading_space: 0.0,
            byte_range: range,
            format: TextFormat::simple(font.clone(), color),
        });
    }
}

fn tab_advance(x: f32, stop: f32) -> f32 {
    ((x / stop + 0.0001).floor() + 1.0) * stop - x
}

pub(super) fn spaces_to_tab_stop(ui: &egui::Ui, prefix: &str, settings: EditorSettings) -> usize {
    let font = settings.font.id();
    ui.fonts_mut(|fonts| {
        let space = fonts.glyph_width(&font, ' ');
        let tab = space * f32::from(settings.tab_width);
        if tab <= 0.0 {
            return usize::from(settings.tab_width);
        }
        let mut x = 0.0;
        for ch in prefix.chars() {
            x += if ch == '\t' {
                tab_advance(x, tab)
            } else {
                fonts.glyph_width(&font, ch)
            };
        }
        (tab_advance(x, tab) / space)
            .round()
            .clamp(1.0, f32::from(settings.tab_width)) as usize
    })
}

fn layout_tabs(
    fonts: &mut egui::epaint::text::FontsView<'_>,
    job: &mut LayoutJob,
    font: &FontId,
    width: u8,
) {
    let tab = fonts.glyph_width(font, ' ') * f32::from(width);
    let native_tab = fonts.glyph_width(font, '\t');
    if tab <= 0.0 || native_tab <= 0.0 {
        return;
    }
    let height = fonts.row_height(font);
    let mut layouts = Vec::new();
    let mut line_start = 0;
    for line in job.text.split_inclusive('\n') {
        if let Some(last_tab) = line.rfind('\t') {
            let mut x = 0.0;
            for (offset, ch) in line[..=last_tab].char_indices() {
                if ch == '\t' {
                    // Size only the invisible glyph, preserving one cursor position per source
                    // character and preserving the row height of the regular editor font.
                    let mut tab_font = font.clone();
                    tab_font.size *= tab_advance(x, tab) / native_tab;
                    x += fonts.glyph_width(&tab_font, '\t');
                    layouts.push((line_start + offset, tab_font));
                } else {
                    x += fonts.glyph_width(font, ch);
                }
            }
        }
        line_start += line.len();
    }

    let mut tabs = layouts.into_iter().peekable();
    for mut section in std::mem::take(&mut job.sections) {
        let end = section.byte_range.end;
        while let Some(&(offset, _)) = tabs.peek().filter(|(offset, _)| *offset < end) {
            let (_, tab_font) = tabs.next().expect("peeked tab");
            if section.byte_range.start < offset {
                job.sections.push(LayoutSection {
                    byte_range: section.byte_range.start..offset,
                    ..section.clone()
                });
            }
            job.sections.push(LayoutSection {
                byte_range: offset..offset + 1,
                format: TextFormat {
                    font_id: tab_font,
                    line_height: Some(height),
                    ..section.format.clone()
                },
                ..section.clone()
            });
            section.byte_range.start = offset + 1;
        }
        if !section.byte_range.is_empty() {
            job.sections.push(section);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_and_saves_sql_with_the_same_safe_case_as_the_editor() {
        let original = "select CONCAT('原文 from', t.order) from t;";
        let shown = "SELECT concat('原文 from', t.order) FROM t;";
        let mut sql = original.to_owned();
        let id = egui::Id::new("editor-case");
        let settings = EditorSettings {
            keyword_case: LetterCase::Upper,
            function_case: LetterCase::Lower,
            ..EditorSettings::default()
        };
        let ctx = egui::Context::default();
        let mut initialize = true;
        let mut draw = |ctx: &egui::Context| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let mut layout = |ui: &egui::Ui, text: &dyn egui::TextBuffer, width| {
                    sql_galley(ui, text.as_str(), width, settings, (false, false))
                };
                let first = ui.output(|output| output.commands.len());
                let mut output = egui::TextEdit::multiline(&mut sql)
                    .id(id)
                    .layouter(&mut layout)
                    .show(ui);
                assert_eq!(output.galley.text(), shown);
                let color_at = |index| {
                    output
                        .galley
                        .job
                        .sections
                        .iter()
                        .find(|section| section.byte_range.contains(&index))
                        .unwrap()
                        .format
                        .color
                };
                assert_eq!(color_at(0), rgb(settings.colors.keyword));
                assert_eq!(color_at(7), rgb(settings.colors.function));
                assert_eq!(color_at(14), rgb(settings.colors.string));
                apply_case_to_clipboard(ui, first, &sql, 0..sql.len(), settings, (false, false));
                if initialize {
                    let range = super::super::editor_tools::cursors(&sql, 0..sql.len());
                    output.state.cursor.set_char_range(Some(range));
                    output.state.store(ctx, id);
                    output.response.request_focus();
                    initialize = false;
                }
            });
        };
        let _ = ctx.run(egui::RawInput::default(), &mut draw);
        let output = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Copy],
                ..egui::RawInput::default()
            },
            draw,
        );
        assert!(output.platform_output.commands.iter().any(|command| {
            matches!(command, egui::OutputCommand::CopyText(copied) if copied == shown)
        }));
        assert_eq!(sql, original);
        assert_eq!(
            sql_file_text(
                "create table CoNcAt (data int); select CONCAT(data) from CoNcAt;",
                settings,
                (false, false),
            ),
            "CREATE TABLE CoNcAt (data int); SELECT concat(data) FROM CoNcAt;"
        );
    }

    #[test]
    fn aligns_tabs_to_the_custom_stop_and_only_wraps_when_enabled() {
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let settings = EditorSettings {
                    tab_width: 6,
                    word_wrap: false,
                    ..EditorSettings::default()
                };
                let tabbed = sql_galley(ui, "a\tb", 100.0, settings, (false, false));
                let spaces = sql_galley(ui, "a     b", 100.0, settings, (false, false));
                assert_eq!(tabbed.text(), "a\tb");
                let x = |galley: &Galley, character| {
                    galley
                        .pos_from_cursor(egui::text::CCursor::new(character))
                        .left()
                };
                assert!((x(&tabbed, 2) - x(&spaces, 6)).abs() <= 1.0);
                assert_eq!(tabbed.rows[0].height(), spaces.rows[0].height());

                let long = "SELECT title FROM books WHERE visible = 1;";
                assert_eq!(
                    sql_galley(ui, long, 110.0, settings, (false, false))
                        .rows
                        .len(),
                    1
                );
                let wrapping = EditorSettings {
                    word_wrap: true,
                    ..settings
                };
                assert!(
                    sql_galley(ui, long, 110.0, wrapping, (false, false))
                        .rows
                        .len()
                        > 1
                );
            });
        });
    }
}
