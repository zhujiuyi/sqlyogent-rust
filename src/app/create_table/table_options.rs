use eframe::egui::{self, ComboBox, TextEdit};

use crate::sql_tools::{EditorTokenKind, editor_tokens};

use super::{literal, option_identifier};

const INNODB_FORMATS: &[&str] = &["DYNAMIC", "COMPACT", "COMPRESSED", "REDUNDANT"];
const MYISAM_FORMATS: &[&str] = &["DYNAMIC", "FIXED"];
const ARIA_FORMATS: &[&str] = &["PAGE", "DYNAMIC", "FIXED"];
const COMMON_FORMATS: &[&str] = &["DYNAMIC", "FIXED", "COMPACT", "COMPRESSED", "REDUNDANT"];
// MyISAM stores the chunk size in bytes in a four-byte field.
const MAX_RAID_CHUNKSIZE_KIB: u32 = u32::MAX / 1024;

#[derive(Default)]
pub(in crate::app) struct Options {
    pub engine: String,
    pub charset: String,
    pub collation: String,
    pub row_format: Option<&'static str>,
    pub auto_increment: String,
    pub average_row_length: String,
    pub min_rows: String,
    pub max_rows: String,
    pub checksum: Option<bool>,
    pub delayed_key_write: Option<bool>,
    pub raid_chunks: String,
    pub raid_chunksize: String,
    pub comment: Option<String>,
}

impl Options {
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        engines: &[String],
        original: Option<&Self>,
        raid_default_engine: Option<&str>,
    ) {
        let defaults = Self::default();
        let declared = original.unwrap_or(&defaults);
        let omit = if original.is_some() {
            "保持当前"
        } else {
            "服务器默认"
        };
        egui::Grid::new("table-options")
            .num_columns(4)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("存储引擎");
                let initial_engine = current_label(omit, &declared.engine);
                ComboBox::from_id_salt("engine")
                    .selected_text(if self.engine.is_empty() {
                        &initial_engine
                    } else {
                        &self.engine
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.engine, String::new(), initial_engine);
                        for engine in engines {
                            ui.selectable_value(&mut self.engine, engine.clone(), engine);
                        }
                    });
                ui.label("行格式");
                let initial_format = current_label(omit, declared.row_format.unwrap_or_default());
                ComboBox::from_id_salt("row-format")
                    .selected_text(match self.row_format {
                        Some("DEFAULT") => "恢复引擎默认",
                        Some(format) => format,
                        None => &initial_format,
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.row_format, None, initial_format);
                        if original.is_some() {
                            ui.selectable_value(
                                &mut self.row_format,
                                Some("DEFAULT"),
                                "恢复引擎默认",
                            );
                        }
                        for &format in row_formats(effective_engine(&self.engine, &declared.engine))
                        {
                            ui.selectable_value(&mut self.row_format, Some(format), format);
                        }
                    });
                ui.end_row();

                text_field(ui, "默认字符集", &mut self.charset, &declared.charset);
                text_field(ui, "默认排序规则", &mut self.collation, &declared.collation);
                ui.end_row();

                text_field(
                    ui,
                    "下一个自增值",
                    &mut self.auto_increment,
                    &declared.auto_increment,
                );
                text_field(
                    ui,
                    "平均行长度",
                    &mut self.average_row_length,
                    &declared.average_row_length,
                );
                ui.end_row();

                text_field(ui, "预计最少行数", &mut self.min_rows, &declared.min_rows);
                text_field(ui, "预计最多行数", &mut self.max_rows, &declared.max_rows);
                ui.end_row();

                bool_field(
                    ui,
                    "表校验和",
                    "MyISAM 和 MariaDB 的 Aria 支持随写入更新表校验和。",
                    &mut self.checksum,
                    omit,
                    declared.checksum,
                );
                bool_field(
                    ui,
                    "延迟写入索引",
                    "适用于 MyISAM 和 MariaDB 的 Aria，是否生效由服务器的 delay_key_write 设置决定。",
                    &mut self.delayed_key_write,
                    omit,
                    declared.delayed_key_write,
                );
                ui.end_row();

                if self.raid_available(original, raid_default_engine) {
                    text_field(ui, "RAID 分块数", &mut self.raid_chunks, &declared.raid_chunks);
                    text_field(
                        ui,
                        "RAID 分块大小（KiB）",
                        &mut self.raid_chunksize,
                        &declared.raid_chunksize,
                    );
                    ui.end_row();
                }

                if let Some(original) = original {
                    let mut change = self.comment.is_some();
                    if ui.checkbox(&mut change, "修改表备注").changed() {
                        self.comment = change.then(|| original.comment.clone().unwrap_or_default());
                    }
                    if let Some(comment) = &mut self.comment {
                        ui.add(TextEdit::singleline(comment).desired_width(200.0));
                    } else {
                        let comment = original.comment.as_deref().unwrap_or_default();
                        ui.add(egui::Label::new(comment).truncate())
                            .on_hover_text(comment);
                    }
                } else {
                    ui.label("表备注");
                    let mut comment = self.comment.take().unwrap_or_default();
                    ui.add(TextEdit::singleline(&mut comment).desired_width(200.0));
                    self.comment = (!comment.is_empty()).then_some(comment);
                }
                ui.end_row();
            });
    }

    pub fn clauses(
        &self,
        original: Option<&Self>,
        no_backslash_escapes: bool,
        raid_default_engine: Option<&str>,
    ) -> Result<Vec<String>, String> {
        let mut clauses = Vec::new();
        for (name, value) in [
            ("ENGINE", &self.engine),
            ("DEFAULT CHARACTER SET", &self.charset),
            ("COLLATE", &self.collation),
        ] {
            if let Some(value) = option_identifier(name, value)? {
                clauses.push(format!("{name} = {value}"));
            }
        }
        if let Some(format) = self.row_format {
            let engine =
                effective_engine(&self.engine, original.map_or("", |options| &options.engine));
            if format != "DEFAULT" && !row_formats(engine).contains(&format) {
                if engine.is_empty() {
                    return Err(format!("请选择支持 {format} 行格式的存储引擎。"));
                }
                return Err(format!(
                    "{engine} 不支持设置行格式 {format}，请选择其他行格式。"
                ));
            }
            clauses.push(format!("ROW_FORMAT = {format}"));
        }
        for (name, value) in [
            ("AUTO_INCREMENT", &self.auto_increment),
            ("AVG_ROW_LENGTH", &self.average_row_length),
            ("MIN_ROWS", &self.min_rows),
            ("MAX_ROWS", &self.max_rows),
        ] {
            let value = value.trim();
            if !value.is_empty() {
                let number = value
                    .parse::<u64>()
                    .map_err(|_| format!("{name} 必须是非负整数。"))?;
                clauses.push(format!("{name} = {number}"));
            }
        }
        for (name, value) in [
            ("CHECKSUM", self.checksum),
            ("DELAY_KEY_WRITE", self.delayed_key_write),
        ] {
            if let Some(value) = value {
                clauses.push(format!("{name} = {}", u8::from(value)));
            }
        }
        if self.raid_available(original, raid_default_engine)
            && !(self.raid_chunks.trim().is_empty() && self.raid_chunksize.trim().is_empty())
        {
            let chunks = numeric_change(&self.raid_chunks, original.map(|o| &o.raid_chunks))
                .parse::<u8>()
                .ok()
                .filter(|&count| count > 0)
                .ok_or("RAID_CHUNKS 必须是 1 到 255 之间的整数。")?;
            let chunk_size =
                numeric_change(&self.raid_chunksize, original.map(|o| &o.raid_chunksize))
                    .parse::<u32>()
                    .ok()
                    .filter(|size| (1..=MAX_RAID_CHUNKSIZE_KIB).contains(size))
                    .ok_or_else(|| {
                        format!(
                            "RAID_CHUNKSIZE 必须是 1 到 {MAX_RAID_CHUNKSIZE_KIB} 之间的 KiB 整数。"
                        )
                    })?;
            clauses.push(format!(
                "RAID_TYPE = STRIPED RAID_CHUNKS = {chunks} RAID_CHUNKSIZE = {chunk_size}"
            ));
        }
        if let Some(comment) = &self.comment {
            clauses.push(format!(
                "COMMENT = {}",
                literal(comment, no_backslash_escapes)
            ));
        }
        Ok(clauses)
    }

    fn raid_available(&self, original: Option<&Self>, raid_default_engine: Option<&str>) -> bool {
        raid_default_engine.is_some_and(|default_engine| {
            let inherited_engine = original.map_or(default_engine, |options| &options.engine);
            effective_engine(&self.engine, inherited_engine).eq_ignore_ascii_case("MyISAM")
        })
    }

    pub fn read_clause(clause: &str) -> Self {
        // SHOW CREATE TABLE supplies the current values for display. Leave anything in
        // executable comments, partition definitions or unrecognized options opaque.
        use EditorTokenKind::{Protected, Symbol, Word};

        let mut options = Self::default();
        // Both servers quote table option strings with backslash escapes in SHOW CREATE TABLE.
        let mut tokens = editor_tokens(clause, false, false).peekable();
        let mut depth = 0;
        while let Some(token) = tokens.next() {
            match token.kind {
                Symbol('(') => {
                    depth += 1;
                    continue;
                }
                Symbol(')') if depth > 0 => {
                    depth -= 1;
                    continue;
                }
                Symbol(')' | ';') => break,
                Word if depth == 0 => {}
                _ => continue,
            }
            let key = token.text.to_ascii_uppercase();
            if key == "PARTITION" {
                break;
            }
            let key = if key == "CHARACTER"
                && tokens
                    .next_if(|token| token.kind == Word && token.text.eq_ignore_ascii_case("SET"))
                    .is_some()
            {
                "CHARSET"
            } else {
                &key
            };
            if tokens.next_if(|token| token.kind == Symbol('=')).is_none() {
                continue;
            }
            let Some(value) =
                tokens.next_if(|token| matches!(token.kind, Word | Protected { closed: true }))
            else {
                continue;
            };
            match (key, value.kind) {
                ("ENGINE" | "TYPE", Word) => options.engine = value.text.into(),
                ("CHARSET", Word) => options.charset = value.text.into(),
                ("COLLATE", Word) => options.collation = value.text.into(),
                ("ROW_FORMAT", Word) => {
                    options.row_format = COMMON_FORMATS
                        .iter()
                        .copied()
                        .chain(["PAGE", "DEFAULT"])
                        .find(|format| value.text.eq_ignore_ascii_case(format));
                }
                ("AUTO_INCREMENT", Word) => options.auto_increment = value.text.into(),
                ("AVG_ROW_LENGTH", Word) => options.average_row_length = value.text.into(),
                ("MIN_ROWS", Word) => options.min_rows = value.text.into(),
                ("MAX_ROWS", Word) => options.max_rows = value.text.into(),
                ("CHECKSUM" | "TABLE_CHECKSUM", Word) => options.checksum = parse_bool(value.text),
                ("DELAY_KEY_WRITE", Word) => options.delayed_key_write = parse_bool(value.text),
                ("RAID_CHUNKS", Word) => options.raid_chunks = value.text.into(),
                ("RAID_CHUNKSIZE", Word) => options.raid_chunksize = value.text.into(),
                ("COMMENT", Protected { closed: true }) => {
                    options.comment = decode_comment(value.text);
                }
                _ => {}
            }
        }
        options
    }
}

fn numeric_change<'a>(edited: &'a str, original: Option<&'a String>) -> &'a str {
    let edited = edited.trim();
    if edited.is_empty() {
        original.map_or("", |value| value.trim())
    } else {
        edited
    }
}

fn row_formats(engine: &str) -> &'static [&'static str] {
    if engine.eq_ignore_ascii_case("MyISAM") {
        MYISAM_FORMATS
    } else if engine.eq_ignore_ascii_case("InnoDB") {
        INNODB_FORMATS
    } else if engine.eq_ignore_ascii_case("Aria") {
        ARIA_FORMATS
    } else {
        COMMON_FORMATS
    }
}

fn effective_engine<'a>(chosen: &'a str, original: &'a str) -> &'a str {
    let chosen = chosen.trim();
    if chosen.is_empty() { original } else { chosen }
}

fn current_label(omit: &str, value: &str) -> String {
    if value.is_empty() {
        omit.into()
    } else {
        format!("{omit}（{value}）")
    }
}

fn text_field(ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str) {
    ui.label(label);
    ui.add(
        TextEdit::singleline(value)
            .desired_width(155.0)
            .hint_text(hint),
    );
}

fn bool_field(
    ui: &mut egui::Ui,
    label: &str,
    help: &str,
    value: &mut Option<bool>,
    omit: &str,
    declared: Option<bool>,
) {
    ui.label(label).on_hover_text(help);
    let initial = current_label(omit, declared.map(on_off).unwrap_or_default());
    ComboBox::from_id_salt(label)
        .selected_text(match value {
            Some(setting) => on_off(*setting),
            None => &initial,
        })
        .show_ui(ui, |ui| {
            ui.selectable_value(value, None, initial);
            for state in [true, false] {
                ui.selectable_value(value, Some(state), on_off(state));
            }
        });
}

fn on_off(value: bool) -> &'static str {
    if value { "启用" } else { "关闭" }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

fn decode_comment(value: &str) -> Option<String> {
    let mut content = value.strip_prefix('\'')?.strip_suffix('\'')?.chars();
    let mut comment = String::new();
    while let Some(character) = content.next() {
        let character = match character {
            '\'' if content.next()? == '\'' => '\'',
            '\'' => return None,
            '\\' => match content.next()? {
                '0' => '\0',
                'n' => '\n',
                'r' => '\r',
                '\\' => '\\',
                _ => return None,
            },
            character => character,
        };
        comment.push(character);
    }
    Some(comment)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_raid_edit_keeps_the_other_chunk_setting() {
        let original = Options::read_clause(
            "TYPE=MyISAM RAID_TYPE=STRIPED RAID_CHUNKS=2 RAID_CHUNKSIZE=123 COMMENT='旧表'",
        );
        assert_eq!(original.raid_chunks, "2");
        assert_eq!(original.raid_chunksize, "123");

        let changes = Options {
            raid_chunks: "3".into(),
            ..Options::default()
        };
        assert_eq!(
            changes
                .clauses(Some(&original), false, Some("InnoDB"))
                .unwrap(),
            ["RAID_TYPE = STRIPED RAID_CHUNKS = 3 RAID_CHUNKSIZE = 123"]
        );
        assert!(
            changes
                .clauses(Some(&original), false, None)
                .unwrap()
                .is_empty()
        );
    }
}
