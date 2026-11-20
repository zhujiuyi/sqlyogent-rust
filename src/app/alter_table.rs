use std::collections::{HashMap, HashSet};

use eframe::egui::{self, Align, Button, Layout, TextEdit};
use egui_extras::{Column, TableBuilder};

use crate::database::{Command, DbObject, ObjectDetail, quote_identifier};

use super::{create_table::table_options::Options, state::Workspace, style};

pub(super) struct Editor {
    workspace: usize,
    object: DbObject,
    loading: Load,
}

enum Load {
    Settings,
    Definition,
    Ready(Draft),
    Failed(String),
}

#[derive(Clone)]
struct Field {
    id: usize,
    name: String,
    definition: String,
}

struct Draft {
    original: Vec<Field>,
    fields: Vec<Field>,
    next_id: usize,
    no_backslash_escapes: bool,
    original_options: Options,
    options: Options,
    advanced: bool,
    preview: bool,
}

impl Editor {
    pub(super) fn open(workspace: usize, object: DbObject, ws: &mut Workspace) -> Self {
        ws.engines.clear();
        ws.raid_default_engine = None;
        ws.detail = None;
        let loading = if ws.send(Command::ListEngines, "正在读取存储引擎和 SQL 模式…")
        {
            Load::Settings
        } else {
            Load::Failed(ws.status.clone())
        };
        Self {
            workspace,
            object,
            loading,
        }
    }

    fn poll(&mut self, ws: &mut Workspace) {
        if ws.busy() {
            return;
        }
        match &self.loading {
            Load::Settings if !ws.engines.is_empty() => {
                self.loading = if ws.send(
                    Command::Inspect {
                        object: self.object.clone(),
                    },
                    "正在读取表结构…",
                ) {
                    Load::Definition
                } else {
                    Load::Failed(ws.status.clone())
                };
            }
            Load::Definition => {
                let draft = ws
                    .detail
                    .as_ref()
                    .filter(|detail| detail.object == self.object)
                    .ok_or_else(|| ws.status.clone())
                    .and_then(|detail| Draft::new(detail, ws.no_backslash_escapes));
                self.loading = match draft {
                    Ok(draft) => Load::Ready(draft),
                    Err(error) => Load::Failed(error),
                };
            }
            Load::Settings => self.loading = Load::Failed(ws.status.clone()),
            Load::Ready(_) | Load::Failed(_) => {}
        }
    }
}

impl Draft {
    fn new(detail: &ObjectDetail, no_backslash_escapes: bool) -> Result<Self, String> {
        let (original, options_clause) = read_fields(detail, no_backslash_escapes)?;
        Ok(Self {
            next_id: original.len(),
            fields: original.clone(),
            original,
            no_backslash_escapes,
            original_options: options_clause.map(Options::read_clause).unwrap_or_default(),
            options: Options::default(),
            advanced: false,
            preview: true,
        })
    }

    fn add_field(&mut self) {
        self.fields.push(Field {
            id: self.next_id,
            name: String::new(),
            definition: "INT".into(),
        });
        self.next_id += 1;
    }
}

pub(super) fn dialog(
    ctx: &egui::Context,
    state: &mut Option<Editor>,
    workspaces: &mut [Workspace],
) {
    let Some(editor) = state else { return };
    let Some(ws) = workspaces.get_mut(editor.workspace) else {
        *state = None;
        return;
    };
    editor.poll(ws);
    let viewport = ctx.content_rect();
    let mut close = false;
    let mut retry = false;
    let mut open = None;
    crate::app::style::modal(ctx, egui::Id::new("alter-table-fields")).show(ctx, |ui| {
        ui.set_width((viewport.width() - 58.0).clamp(1.0, 1000.0));
        ui.horizontal(|ui| {
            style::page_title(ui, "修改表结构");
            ui.add_space(6.0);
            style::badge(
                ui,
                qualified(&editor.object),
                style::accent(),
                style::accent_soft(),
            );
        });
        ui.add_space(8.0);
        let mut ready_sql = None;
        match &mut editor.loading {
            Load::Settings | Load::Definition => {
                style::subtle_card(egui::Margin::same(14)).show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal_wrapped(|ui| {
                        ui.spinner();
                        ui.label(&ws.status);
                    });
                });
            }
            Load::Failed(error) => {
                style::banner(ui, error.as_str(), style::error(), style::error_soft());
                ui.add_space(6.0);
                retry = ui
                    .add_enabled(!ws.busy() && ws.server.is_some(), Button::new("重新读取"))
                    .clicked();
            }
            Load::Ready(draft) => {
                style::subtle_card(egui::Margin::symmetric(10, 6)).show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("添加字段").clicked() {
                            draft.add_field();
                        }
                        if ui.button("恢复原定义").clicked() {
                            draft.fields.clone_from(&draft.original);
                            draft.options = Options::default();
                        }
                        ui.separator();
                        ui.checkbox(&mut draft.advanced, "高级表属性");
                        ui.checkbox(&mut draft.preview, "预览 SQL");
                    });
                });
                ui.add_space(8.0);
                style::card(egui::Margin::same(8)).show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    field_grid(
                        ui,
                        &mut draft.fields,
                        (viewport.height() * 0.31).clamp(100.0, 265.0),
                    );
                });
                if draft.advanced {
                    ui.add_space(8.0);
                    style::section_card(ui, "高级表属性", |ui| {
                        egui::ScrollArea::both()
                            .id_salt("alter-table-properties")
                            .auto_shrink([false, false])
                            .max_height((viewport.height() * 0.24).clamp(106.0, 200.0))
                            .show(ui, |ui| {
                                draft.options.show(
                                    ui,
                                    &ws.engines,
                                    Some(&draft.original_options),
                                    ws.raid_default_engine.as_deref(),
                                );
                            });
                    });
                }
                match alter_sql(draft, &editor.object, ws.raid_default_engine.as_deref()) {
                    Ok(Some(sql)) => {
                        if draft.preview {
                            ui.add_space(8.0);
                            style::section_card(ui, "SQL 预览", |ui| {
                                let mut query = sql.as_str();
                                egui::ScrollArea::both()
                                    .id_salt("alter-sql-preview")
                                    .auto_shrink([false, false])
                                    .max_height((viewport.height() * 0.20).clamp(84.0, 165.0))
                                    .show(ui, |ui| {
                                        ui.add(
                                            TextEdit::multiline(&mut query)
                                                .code_editor()
                                                .desired_width(f32::INFINITY)
                                                .frame(false),
                                        );
                                    });
                            });
                        }
                        ready_sql = Some(sql);
                    }
                    Ok(None) => {
                        ui.add_space(6.0);
                        ui.colored_label(style::muted(), "尚无更改。");
                    }
                    Err(error) => {
                        ui.add_space(6.0);
                        style::banner(ui, &error, style::error(), style::error_soft());
                    }
                }
            }
        }
        style::dialog_footer(ui, |ui| {
            let has_sql = ready_sql.is_some();
            if ui
                .add_enabled(has_sql, style::primary_enabled(has_sql, "在编辑器中打开 SQL"))
                .clicked()
            {
                open = ready_sql;
            }
            close = ui.button("取消").clicked();
        });
    });
    if let Some(sql) = open {
        ws.new_tab(sql, None);
        *state = None;
    } else if close {
        *state = None;
    } else if retry {
        *editor = Editor::open(editor.workspace, editor.object.clone(), ws);
    }
}

enum FieldAction {
    Move(usize, usize),
    Remove(usize),
}

fn field_grid(ui: &mut egui::Ui, fields: &mut Vec<Field>, height: f32) {
    let mut action = None;
    let count = fields.len();
    let header_height = style::grid_row_height(ui, 25.0);
    let row_height = style::grid_row_height(ui, 30.0);
    let definition_font =
        style::grid_font(ui).unwrap_or_else(|| egui::TextStyle::Monospace.resolve(ui.style()));
    let original_style = ui.style().clone();
    style::apply_grid_font(ui);
    TableBuilder::new(ui)
        .id_salt("alter-fields")
        .striped(true)
        .resizable(true)
        .cell_layout(Layout::left_to_right(Align::Center))
        .max_scroll_height(height)
        .column(Column::initial(150.0).at_least(95.0))
        .column(Column::remainder().at_least(185.0))
        .column(Column::auto().at_least(128.0))
        .header(header_height, |mut header| {
            for label in ["字段名", "类型和属性", "字段操作"] {
                header.col(|ui| {
                    ui.strong(label);
                });
            }
        })
        .body(|body| {
            body.rows(row_height, count, |mut row| {
                let index = row.index();
                let field = &mut fields[index];
                row.col(|ui| {
                    ui.add(
                        TextEdit::singleline(&mut field.name)
                            .id_salt(("name", field.id))
                            .desired_width(f32::INFINITY),
                    );
                });
                row.col(|ui| {
                    ui.add(
                        TextEdit::singleline(&mut field.definition)
                            .id_salt(("definition", field.id))
                            .font(definition_font.clone())
                            .desired_width(f32::INFINITY),
                    )
                    .on_hover_text(&field.definition);
                });
                row.col(|ui| {
                    if ui.add_enabled(index > 0, Button::new("上移")).clicked() {
                        action = Some(FieldAction::Move(index, index - 1));
                    }
                    if ui
                        .add_enabled(index + 1 < count, Button::new("下移"))
                        .clicked()
                    {
                        action = Some(FieldAction::Move(index, index + 1));
                    }
                    if ui.button("删除").clicked() {
                        action = Some(FieldAction::Remove(index));
                    }
                });
            });
        });
    ui.set_style(original_style);
    match action {
        Some(FieldAction::Move(index, target)) => fields.swap(index, target),
        Some(FieldAction::Remove(index)) => {
            fields.remove(index);
        }
        None => {}
    }
}

fn qualified(object: &DbObject) -> String {
    format!(
        "{}.{}",
        quote_identifier(&object.schema),
        quote_identifier(&object.name)
    )
}

fn read_fields(
    detail: &ObjectDetail,
    no_backslash_escapes: bool,
) -> Result<(Vec<Field>, Option<&str>), String> {
    if let Some(error) = &detail.ddl_error {
        return Err(error.clone());
    }
    let read_error = || "无法从服务器返回的建表语句完整读取字段。".to_owned();
    let table = detail
        .ddl
        .trim_ascii_start()
        .strip_prefix("CREATE TABLE ")
        .ok_or_else(read_error)?;
    let quoted_table = quote_identifier(&detail.object.name);
    let qualified_table = qualified(&detail.object);
    let mut body = [&qualified_table, &quoted_table, &detail.object.name]
        .into_iter()
        .find_map(|name| {
            table
                .strip_prefix(name)?
                .trim_ascii_start()
                .strip_prefix('(')
        })
        .ok_or_else(read_error)?;

    let mut fields = Vec::with_capacity(detail.columns.len());
    for column in &detail.columns {
        let Ok(Some((end, separator))) = definition_separator(body, no_backslash_escapes) else {
            return Err(read_error());
        };
        let entry = body[..end].trim_ascii();
        let quoted = quote_identifier(&column.name);
        let definition = [&quoted, &column.name]
            .into_iter()
            .find_map(|name| {
                entry
                    .strip_prefix(name)
                    .filter(|definition| definition.starts_with(char::is_whitespace))
            })
            .map(str::trim_ascii)
            .filter(|definition| !definition.is_empty())
            .ok_or_else(read_error)?;
        fields.push(Field {
            id: fields.len(),
            name: column.name.clone(),
            definition: definition.to_owned(),
        });
        if separator == b')' {
            if fields.len() != detail.columns.len() {
                return Err(read_error());
            }
            return Ok((fields, Some(&body[end + 1..])));
        }
        body = &body[end + 1..];
    }
    if fields.is_empty() {
        return Err(read_error());
    }
    // Skip index and constraint definitions after the columns. The field editor
    // can still use the recovered columns if the suffix cannot be located.
    while let Ok(Some((end, separator))) = definition_separator(body, no_backslash_escapes) {
        body = &body[end + 1..];
        if separator == b')' {
            return Ok((fields, Some(body)));
        }
    }
    Ok((fields, None))
}

// Find a column or table-definition boundary without cutting quoted values or expressions.
fn definition_separator(sql: &str, no_backslash_escapes: bool) -> Result<Option<(usize, u8)>, ()> {
    let bytes = sql.as_bytes();
    let mut index = 0;
    let mut depth = 0_u32;
    while let Some(&byte) = bytes.get(index) {
        let following = bytes.get(index + 1);
        match byte {
            b'\'' | b'"' | b'`' => {
                let backslash = byte != b'`' && !no_backslash_escapes;
                index += 1;
                loop {
                    match bytes.get(index).ok_or(())? {
                        b'\\' if backslash => index += 2,
                        quote if *quote == byte && bytes.get(index + 1) == Some(&byte) => {
                            index += 2
                        }
                        quote if *quote == byte => break,
                        _ => index += 1,
                    }
                }
            }
            b'/' if following == Some(&b'*') => {
                let end = bytes[index + 2..]
                    .windows(2)
                    .position(|pair| pair == b"*/")
                    .ok_or(())?;
                index += end + 3;
            }
            b'#' => return Err(()),
            b'-' if following == Some(&b'-')
                && bytes
                    .get(index + 2)
                    .is_none_or(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control()) =>
            {
                return Err(());
            }
            b'(' => depth += 1,
            b')' | b',' if depth == 0 => return Ok(Some((index, byte))),
            b')' => depth -= 1,
            b';' | 0 => return Err(()),
            _ => {}
        }
        index += 1;
    }
    if depth == 0 { Ok(None) } else { Err(()) }
}

fn alter_sql(
    draft: &Draft,
    object: &DbObject,
    raid_default_engine: Option<&str>,
) -> Result<Option<String>, String> {
    if draft.fields.is_empty() {
        return Err("表必须保留至少一个字段。".into());
    }
    let mut names = HashSet::with_capacity(draft.fields.len());
    let original_names: HashMap<_, _> = draft
        .original
        .iter()
        .map(|field| (field.name.to_lowercase(), field.id))
        .collect();
    for field in &draft.fields {
        let name = field.name.to_lowercase();
        if field.name.trim().is_empty()
            || field.name.chars().count() > 64
            || field.name.contains('\0')
        {
            return Err("请输入不超过 64 字符的有效字段名。".into());
        }
        if original_names.get(&name).is_some_and(|id| *id != field.id) || !names.insert(name) {
            return Err(format!(
                "字段名「{}」与其他原有或新增字段重名。",
                field.name
            ));
        }
        if field.definition.trim_ascii().is_empty()
            || definition_separator(&field.definition, draft.no_backslash_escapes) != Ok(None)
        {
            return Err(format!("请为「{}」填写一项完整字段定义。", field.name));
        }
    }

    let ids: HashSet<_> = draft.fields.iter().map(|field| field.id).collect();
    let mut order = Vec::with_capacity(draft.fields.len());
    let mut clauses = Vec::new();
    for field in &draft.original {
        if ids.contains(&field.id) {
            order.push(field.id);
        } else {
            clauses.push(format!("DROP COLUMN {}", quote_identifier(&field.name)));
        }
    }
    for (index, field) in draft.fields.iter().enumerate() {
        let original = draft.original.get(field.id);
        let position = order.iter().position(|id| *id == field.id);
        let moving = position != Some(index);
        let name = quote_identifier(&field.name);
        let definition = field.definition.trim_ascii();
        let mut clause = match original {
            Some(original) if original.name != field.name => {
                format!(
                    "CHANGE COLUMN {} {name} {definition}",
                    quote_identifier(&original.name)
                )
            }
            Some(original) if moving || original.definition != definition => {
                format!("MODIFY COLUMN {name} {definition}")
            }
            Some(_) => continue,
            None => format!("ADD COLUMN {name} {definition}"),
        };
        if moving {
            if let Some(position) = position {
                order.remove(position);
            }
            order.insert(index, field.id);
            if let Some(previous) = index
                .checked_sub(1)
                .and_then(|index| draft.fields.get(index))
            {
                clause.push_str(&format!(" AFTER {}", quote_identifier(&previous.name)));
            } else {
                clause.push_str(" FIRST");
            }
        }
        clauses.push(clause);
    }
    clauses.extend(draft.options.clauses(
        Some(&draft.original_options),
        draft.no_backslash_escapes,
        raid_default_engine,
    )?);
    Ok((!clauses.is_empty()).then(|| {
        format!(
            "ALTER TABLE {}\n    {};",
            qualified(object),
            clauses.join(",\n    ")
        )
    }))
}

#[cfg(test)]
mod tests {
    use mysql::{Conn, OptsBuilder, Value, prelude::Queryable};

    use super::*;
    use crate::database::{ColumnInfo, ObjectKind};

    fn table() -> ObjectDetail {
        let object = DbObject {
            schema: "test".into(),
            name: "fields".into(),
            kind: ObjectKind::Table,
        };
        let columns = ["id", "title", "amount", "obsolete"]
            .into_iter()
            .map(|name| ColumnInfo {
                name: name.into(),
                data_type: String::new(),
                nullable: false,
                key: String::new(),
                default_value: None,
                extra: String::new(),
            })
            .collect();
        let ddl = "CREATE TABLE `fields` (
  `id` int unsigned NOT NULL AUTO_INCREMENT,
  `title` varchar(24) NOT NULL DEFAULT 'a,b' COMMENT 'value,()',
  `amount` decimal(8,2) NOT NULL DEFAULT 0.00,
  `obsolete` int DEFAULT NULL,
  PRIMARY KEY (`id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4";
        ObjectDetail {
            object,
            columns,
            ddl: ddl.into(),
            ddl_error: None,
        }
    }

    #[test]
    fn existing_fields_keep_server_definition_bytes() {
        let mut detail = table();
        detail.ddl = detail.ddl.replace(
            "varchar(24) NOT NULL DEFAULT 'a,b' COMMENT 'value,()'",
            "enum('a,b','it''s') COLLATE utf8mb4_bin NOT NULL COMMENT 'line 1\nline 2'",
        );
        detail.ddl.push_str(
            " /*!50100 PARTITION BY HASH (`id`) (PARTITION p0 COMMENT='分区备注' ENGINE=InnoDB) */",
        );
        let draft = Draft::new(&detail, false).unwrap();
        assert_eq!(
            draft.fields[1].definition,
            "enum('a,b','it''s') COLLATE utf8mb4_bin NOT NULL COMMENT 'line 1\nline 2'"
        );
        assert_eq!(draft.original_options.engine, "InnoDB");
        assert_eq!(draft.original_options.charset, "utf8mb4");
        assert_eq!(draft.original_options.comment, None);
        assert_eq!(alter_sql(&draft, &detail.object, None).unwrap(), None);
    }

    #[test]
    fn stages_field_changes_and_reordering_in_one_statement() {
        let detail = table();
        let mut draft = Draft::new(&detail, false).unwrap();
        draft.fields.remove(3);
        draft.fields[0].name = "sku".into();
        draft.fields[1].definition = draft.fields[1]
            .definition
            .replace("varchar(24)", "varchar(32)");
        draft.add_field();
        draft.fields[3].name = "tag".into();
        draft.fields[3].definition = "INT NOT NULL DEFAULT 7".into();
        draft.fields.rotate_right(2);
        draft.fields.swap(0, 1);
        draft.options.charset = "utf8mb4".into();
        draft.options.collation = "utf8mb4_unicode_ci".into();
        draft.options.comment = Some("verified".into());
        assert_eq!(
            alter_sql(&draft, &detail.object, None).unwrap().unwrap(),
            "ALTER TABLE `test`.`fields`
    DROP COLUMN `obsolete`,
    ADD COLUMN `tag` INT NOT NULL DEFAULT 7 FIRST,
    MODIFY COLUMN `amount` decimal(8,2) NOT NULL DEFAULT 0.00 AFTER `tag`,
    CHANGE COLUMN `id` `sku` int unsigned NOT NULL AUTO_INCREMENT,
    MODIFY COLUMN `title` varchar(32) NOT NULL DEFAULT 'a,b' COMMENT 'value,()',
    DEFAULT CHARACTER SET = utf8mb4,
    COLLATE = utf8mb4_unicode_ci,
    COMMENT = 'verified';"
        );
    }

    #[test]
    #[ignore = "在一次性本机 MariaDB 中运行，需要 HARBOR_DUMP_TEST_PORT"]
    fn changes_advanced_properties_without_resending_table_definition() {
        let port = std::env::var("HARBOR_DUMP_TEST_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let mut db = Conn::new(
            OptsBuilder::default()
                .ip_or_hostname(Some("127.0.0.1"))
                .tcp_port(port)
                .prefer_socket(false)
                .user(Some("root")),
        )
        .unwrap();
        let mut detail = table();
        detail.object.schema = format!("harbor_alter_options_{}", std::process::id());
        let schema = quote_identifier(&detail.object.schema);
        let qualified = qualified(&detail.object);
        db.query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
            .unwrap();
        let note = "路径 \\ 和引号 '";
        let result = (|| -> Result<_, Box<dyn std::error::Error>> {
            db.query_drop("SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES'")?;
            db.query_drop(format!(
                "CREATE TABLE {qualified} (id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
                    title VARCHAR(24), amount DECIMAL(8,2), obsolete INT)
                 ENGINE=MyISAM AUTO_INCREMENT=23 ROW_FORMAT=FIXED AVG_ROW_LENGTH=64
                 MIN_ROWS=30 MAX_ROWS=150 CHECKSUM=1 DELAY_KEY_WRITE=1 PACK_KEYS=1 COMMENT={}",
                Value::from(note).as_sql(true)
            ))?;
            (_, detail.ddl) = db
                .query_first::<(String, String), _>(format!("SHOW CREATE TABLE {qualified}"))?
                .ok_or("读取建表语句失败。")?;
            let mut draft = Draft::new(&detail, true)?;
            let initial = alter_sql(&draft, &detail.object, None)?;
            draft.options = Options {
                row_format: Some("DEFAULT"),
                auto_increment: "71".into(),
                average_row_length: "96".into(),
                min_rows: "0".into(),
                max_rows: "401".into(),
                checksum: Some(false),
                delayed_key_write: Some(false),
                comment: Some(String::new()),
                ..Default::default()
            };
            let sql = alter_sql(&draft, &detail.object, None)?.ok_or("没有生成改表语句。")?;
            db.query_drop(&sql)?;
            let (_, after) = db
                .query_first::<(String, String), _>(format!("SHOW CREATE TABLE {qualified}"))?
                .ok_or("读取修改后的表失败。")?;
            db.query_drop(format!("INSERT INTO {qualified} (title) VALUES ('样品')"))?;
            let row =
                db.query_first::<(u64, String), _>(format!("SELECT id, title FROM {qualified}"))?;
            Ok((draft.original_options, initial, sql, after, row))
        })();
        db.query_drop(format!("DROP DATABASE {schema}")).unwrap();
        let (original, initial, sql, after, row) = result.unwrap();
        assert_eq!(original.engine, "MyISAM");
        assert_eq!(original.row_format, Some("FIXED"));
        assert_eq!(original.auto_increment, "23");
        assert_eq!(original.average_row_length, "64");
        assert_eq!(original.min_rows, "30");
        assert_eq!(original.max_rows, "150");
        assert_eq!(original.checksum, Some(true));
        assert_eq!(original.delayed_key_write, Some(true));
        assert_eq!(original.comment.as_deref(), Some(note));
        assert_eq!(initial, None);
        assert_eq!(
            sql,
            format!(
                "ALTER TABLE {qualified}
    ROW_FORMAT = DEFAULT,
    AUTO_INCREMENT = 71,
    AVG_ROW_LENGTH = 96,
    MIN_ROWS = 0,
    MAX_ROWS = 401,
    CHECKSUM = 0,
    DELAY_KEY_WRITE = 0,
    COMMENT = '';"
            )
        );
        assert!(
            after.contains("MAX_ROWS=401 AVG_ROW_LENGTH=96 PACK_KEYS=1"),
            "{after}"
        );
        assert!(
            !after.contains("ROW_FORMAT=")
                && !after.contains("MIN_ROWS=")
                && !after.contains("CHECKSUM=")
                && !after.contains("DELAY_KEY_WRITE=")
                && !after.contains("COMMENT="),
            "{after}"
        );
        assert_eq!(row, Some((71, "样品".into())));
    }
}
