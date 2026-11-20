use std::collections::HashSet;
use std::fmt::Write;

use eframe::egui::{self, Button, RichText, TextEdit};

use crate::database::{Command, DbObject, ObjectKind, quote_identifier};

use super::{state::Workspace, style};

pub(super) mod table_options;

use table_options::Options;

const TYPES: &[&str] = &[
    "TINYINT",
    "SMALLINT",
    "MEDIUMINT",
    "INT",
    "BIGINT",
    "DECIMAL",
    "FLOAT",
    "DOUBLE",
    "BIT",
    "CHAR",
    "VARCHAR",
    "TINYTEXT",
    "TEXT",
    "MEDIUMTEXT",
    "LONGTEXT",
    "BINARY",
    "VARBINARY",
    "TINYBLOB",
    "BLOB",
    "MEDIUMBLOB",
    "LONGBLOB",
    "ENUM",
    "SET",
    "DATE",
    "TIME",
    "DATETIME",
    "TIMESTAMP",
    "YEAR",
    "JSON",
    "GEOMETRY",
    "POINT",
    "LINESTRING",
    "POLYGON",
];

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum DefaultValue {
    #[default]
    Implicit,
    Null,
    Literal,
    CurrentTime,
}

#[derive(Clone)]
struct Field {
    name: String,
    kind: &'static str,
    length: String,
    values: String,
    empty_value: bool,
    default: DefaultValue,
    value: String,
    primary: bool,
    binary: bool,
    required: bool,
    unsigned: bool,
    auto_increment: bool,
    zero_fill: bool,
    charset: String,
    collation: String,
    comment: String,
}

impl Default for Field {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: "INT",
            length: String::new(),
            values: String::new(),
            empty_value: false,
            default: DefaultValue::Implicit,
            value: String::new(),
            primary: false,
            binary: false,
            required: false,
            unsigned: false,
            auto_increment: false,
            zero_fill: false,
            charset: String::new(),
            collation: String::new(),
            comment: String::new(),
        }
    }
}

pub(super) struct Designer {
    workspace: usize,
    schema: String,
    on_canvas: bool,
    name: String,
    fields: Vec<Field>,
    options: Options,
    languages: bool,
    advanced: bool,
    preview: bool,
}

impl Designer {
    pub fn open(workspace: usize, schema: String, ws: &mut Workspace) -> Self {
        ws.engines.clear();
        ws.raid_default_engine = None;
        ws.send(Command::ListEngines, "正在读取存储引擎和 SQL 模式…");
        Self {
            workspace,
            schema,
            on_canvas: false,
            name: String::new(),
            fields: vec![Field::default()],
            options: Options::default(),
            languages: false,
            advanced: false,
            preview: false,
        }
    }

    pub(super) fn open_on_canvas(workspace: usize, schema: String, ws: &mut Workspace) -> Self {
        let mut form = Self::open(workspace, schema, ws);
        form.on_canvas = true;
        form
    }
}

pub(super) struct CanvasReturn {
    pub workspace: u64,
    pub creation: Option<(u64, DbObject)>,
    pub show_sql: bool,
}

pub(super) fn dialog(
    ctx: &egui::Context,
    state: &mut Option<Designer>,
    workspaces: &mut [Workspace],
) -> Option<CanvasReturn> {
    let form = state.as_mut()?;
    let ws = &mut workspaces[form.workspace];
    let rect = ctx.content_rect();
    let mut close = false;
    let mut open = false;
    let mut create = false;
    let mut sql = Err(String::new());
    style::modal(ctx, egui::Id::new("create-table-fields")).show(ctx, |ui| {
        ui.set_width((rect.width() - 58.0).clamp(1.0, 1170.0));
        ui.horizontal(|ui| {
            style::page_title(ui, "新建表");
            ui.add_space(6.0);
            style::badge(ui, &form.schema, style::accent(), style::accent_soft());
        });
        ui.add_space(8.0);
        egui::ScrollArea::vertical()
            .id_salt("create-table-body")
            .auto_shrink([false, false])
            .max_height((rect.height() - 160.0).max(170.0))
            .show(ui, |ui| {
                style::subtle_card(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("表名").color(style::muted()));
                        ui.add(
                            TextEdit::singleline(&mut form.name)
                                .desired_width(240.0)
                                .hint_text("输入新表名称"),
                        );
                        ui.separator();
                        if ui.button("添加字段").clicked() {
                            form.fields.push(Field::default());
                        }
                        ui.checkbox(&mut form.languages, "显示字符集和排序规则");
                        ui.checkbox(&mut form.advanced, "高级表属性");
                        ui.checkbox(&mut form.preview, "预览 SQL");
                    });
                });
                ui.add_space(8.0);
                let mut remove = None;
                egui::ScrollArea::both()
                    .id_salt("field-grid")
                    .auto_shrink([false, true])
                    .max_height((rect.height() * 0.32).clamp(130.0, 285.0))
                    .show(ui, |ui| {
                        style::apply_grid_font(ui);
                        egui::Grid::new("field-cells")
                            .striped(true)
                            .min_col_width(34.0)
                            .spacing(egui::vec2(8.0, 6.0))
                            .show(ui, |ui| {
                                for label in [
                                    "字段名",
                                    "数据类型",
                                    "长度 / 精度",
                                    "默认值",
                                    "主键",
                                    "二进制排序",
                                    "非空",
                                    "无符号",
                                    "自增",
                                    "补零",
                                ] {
                                    ui.strong(label);
                                }
                                if form.languages {
                                    ui.strong("字符集");
                                    ui.strong("排序规则");
                                }
                                ui.strong("备注");
                                ui.label("");
                                ui.end_row();
                                for (index, field) in form.fields.iter_mut().enumerate() {
                                    ui.add(
                                        TextEdit::singleline(&mut field.name).desired_width(100.0),
                                    );
                                    egui::ComboBox::from_id_salt(("data-type", index))
                                        .selected_text(field.kind)
                                        .show_ui(ui, |ui| {
                                            for &kind in TYPES {
                                                ui.selectable_value(&mut field.kind, kind, kind);
                                            }
                                        });
                                    if matches!(field.kind, "ENUM" | "SET") {
                                        ui.menu_button("编辑选项", |ui| {
                                            ui.label("每行一个值。");
                                            ui.add(
                                                TextEdit::multiline(&mut field.values)
                                                    .desired_width(240.0)
                                                    .desired_rows(7),
                                            );
                                            ui.checkbox(&mut field.empty_value, "包含空字符串");
                                        });
                                    } else {
                                        ui.add_enabled(
                                            has_length(field.kind),
                                            TextEdit::singleline(&mut field.length)
                                                .desired_width(69.0),
                                        );
                                    }
                                    ui.horizontal(|ui| {
                                        const DEFAULTS: [(DefaultValue, &str); 4] = [
                                            (DefaultValue::Implicit, "不指定"),
                                            (DefaultValue::Null, "空值（NULL）"),
                                            (DefaultValue::Literal, "固定值"),
                                            (DefaultValue::CurrentTime, "当前时间"),
                                        ];
                                        let selected = DEFAULTS
                                            .iter()
                                            .find(|(value, _)| *value == field.default)
                                            .map_or("", |(_, label)| *label);
                                        egui::ComboBox::from_id_salt(("field-default", index))
                                            .selected_text(selected)
                                            .show_ui(ui, |ui| {
                                                for (value, label) in DEFAULTS {
                                                    ui.selectable_value(
                                                        &mut field.default,
                                                        value,
                                                        label,
                                                    );
                                                }
                                            });
                                        if field.default == DefaultValue::Literal {
                                            ui.add(
                                                TextEdit::singleline(&mut field.value)
                                                    .desired_width(86.0),
                                            );
                                        }
                                    });
                                    ui.checkbox(&mut field.primary, "");
                                    checkbox(ui, is_text(field.kind), &mut field.binary);
                                    checkbox(
                                        ui,
                                        !field.primary
                                            && !(is_integer(field.kind) && field.auto_increment),
                                        &mut field.required,
                                    );
                                    checkbox(ui, is_numeric(field.kind), &mut field.unsigned);
                                    checkbox(ui, is_integer(field.kind), &mut field.auto_increment);
                                    checkbox(ui, is_numeric(field.kind), &mut field.zero_fill);
                                    if form.languages {
                                        ui.add_enabled(
                                            is_text(field.kind),
                                            TextEdit::singleline(&mut field.charset)
                                                .desired_width(82.0),
                                        );
                                        ui.add_enabled(
                                            is_text(field.kind),
                                            TextEdit::singleline(&mut field.collation)
                                                .desired_width(120.0),
                                        );
                                    }
                                    ui.add(
                                        TextEdit::singleline(&mut field.comment)
                                            .desired_width(102.0),
                                    );
                                    if ui.small_button("删除").clicked() {
                                        remove = Some(index);
                                    }
                                    ui.end_row();
                                }
                            });
                    });
                if let Some(index) = remove {
                    form.fields.remove(index);
                }
                if form.advanced {
                    ui.separator();
                    egui::ScrollArea::both()
                        .id_salt("table-properties")
                        .max_height(235.0)
                        .show(ui, |ui| {
                            form.options.show(
                                ui,
                                &ws.engines,
                                None,
                                ws.raid_default_engine.as_deref(),
                            );
                        });
                }
                sql = design_sql(
                    form,
                    ws.no_backslash_escapes,
                    ws.raid_default_engine.as_deref(),
                );
                if form.preview
                    && let Ok(query) = &sql
                {
                    ui.separator();
                    let mut text = query.as_str();
                    egui::ScrollArea::both()
                        .id_salt("create-table-preview")
                        .max_height(185.0)
                        .show(ui, |ui| {
                            ui.add(
                                TextEdit::multiline(&mut text)
                                    .code_editor()
                                    .desired_width(f32::INFINITY),
                            );
                        });
                }
                if let Err(error) = &sql {
                    ui.label(RichText::new(error).color(style::muted()));
                }
            });
        let can_create = sql.is_ok() && !ws.busy() && ws.server.is_some();
        style::dialog_footer(ui, |ui| {
            create = ui
                .add_enabled(can_create, style::primary_enabled(can_create, "创建表"))
                .clicked();
            open = ui
                .add_enabled(sql.is_ok() && !ws.busy(), Button::new("打开 SQL"))
                .clicked();
            close = ui.button("取消").clicked();
        });
    });
    let mut outcome = None;
    if close {
        if form.on_canvas {
            outcome = Some(CanvasReturn {
                workspace: ws.key,
                creation: None,
                show_sql: false,
            });
        }
        *state = None;
    } else if (open || create)
        && let Ok(sql) = sql
    {
        ws.new_tab(sql.clone(), None);
        if create {
            ws.start_create_table(sql, form.schema.clone());
            if form.on_canvas {
                outcome = Some(CanvasReturn {
                    workspace: ws.key,
                    creation: Some((
                        ws.tab().id,
                        DbObject {
                            schema: form.schema.clone(),
                            name: form.name.trim().to_owned(),
                            kind: ObjectKind::Table,
                        },
                    )),
                    show_sql: false,
                });
            }
        } else if form.on_canvas {
            outcome = Some(CanvasReturn {
                workspace: ws.key,
                creation: None,
                show_sql: true,
            });
        }
        *state = None;
    }
    outcome
}

fn checkbox(ui: &mut egui::Ui, enabled: bool, value: &mut bool) {
    ui.add_enabled(enabled, egui::Checkbox::without_text(value));
}

fn design_sql(
    form: &Designer,
    no_backslash_escapes: bool,
    raid_default_engine: Option<&str>,
) -> Result<String, String> {
    let name = required_name(&form.name, "请输入表名。")?;
    if form.fields.is_empty() {
        return Err("至少添加一个字段。".into());
    }
    let mut fields = Vec::with_capacity(form.fields.len() + 1);
    let mut primary = Vec::new();
    let mut names = HashSet::new();
    let mut increments = 0;
    for field in &form.fields {
        let name = required_name(&field.name, "请输入每个字段名。")?;
        if !names.insert(field.name.trim().to_lowercase()) {
            return Err(format!("字段名「{}」重复。", field.name));
        }
        if field.primary {
            primary.push(name.clone());
        }
        increments += usize::from(field.auto_increment && is_integer(field.kind));
        fields.push(column_sql(field, name, no_backslash_escapes)?);
    }
    if increments > 1 {
        return Err("一张表只能有一个自增字段。".into());
    }
    if !primary.is_empty() {
        fields.push(format!("PRIMARY KEY ({})", primary.join(", ")));
    }
    let mut sql = format!(
        "CREATE TABLE {}.{name} (\n    {}\n)",
        quote_identifier(&form.schema),
        fields.join(",\n    ")
    );
    for clause in form
        .options
        .clauses(None, no_backslash_escapes, raid_default_engine)?
    {
        sql.push('\n');
        sql.push_str(&clause);
    }
    sql.push(';');
    Ok(sql)
}

fn column_sql(field: &Field, name: String, no_backslash_escapes: bool) -> Result<String, String> {
    let mut out = format!("{name} {}", field.kind);
    let auto = field.auto_increment && is_integer(field.kind);
    if auto && field.default != DefaultValue::Implicit {
        return Err(format!("自增字段 {name} 不可指定默认值。"));
    }
    if matches!(field.kind, "ENUM" | "SET") {
        let mut values = field
            .values
            .lines()
            .map(|value| literal(value, no_backslash_escapes))
            .collect::<Vec<_>>();
        if field.empty_value && !values.iter().any(|value| value == "''") {
            values.push("''".into());
        }
        if values.is_empty() {
            return Err(format!("请为 {name} 设置 {} 可选值。", field.kind));
        }
        let _ = write!(&mut out, "({})", values.join(", "));
    } else if has_length(field.kind) && !field.length.trim().is_empty() {
        let parts = field
            .length
            .split([',', '，'])
            .map(str::trim)
            .collect::<Vec<_>>();
        let maximum = if matches!(field.kind, "DECIMAL" | "FLOAT" | "DOUBLE") {
            2
        } else {
            1
        };
        if parts.len() > maximum || parts.iter().any(|part| part.parse::<u32>().is_err()) {
            return Err(format!("请为 {name} 输入数字长度或精度。"));
        }
        let _ = write!(&mut out, "({})", parts.join(","));
    } else if matches!(field.kind, "VARCHAR" | "VARBINARY") {
        return Err(format!("请为 {name} 指定长度。"));
    }
    if is_numeric(field.kind) {
        if field.unsigned {
            out.push_str(" UNSIGNED");
        }
        if field.zero_fill {
            out.push_str(" ZEROFILL");
        }
    }
    if is_text(field.kind) {
        append_identifier(&mut out, "CHARACTER SET", &field.charset)?;
        append_identifier(&mut out, "COLLATE", &field.collation)?;
        if field.binary {
            out.push_str(" BINARY");
        }
    }
    let required = field.required || field.primary || auto;
    if required {
        out.push_str(" NOT NULL");
    }
    match field.default {
        DefaultValue::Implicit => {}
        DefaultValue::Null if required => {
            return Err(format!("{name} 不可为空，不能使用 NULL 作为默认值。"));
        }
        DefaultValue::Null => out.push_str(" DEFAULT NULL"),
        DefaultValue::Literal => {
            let _ = write!(
                &mut out,
                " DEFAULT {}",
                literal(&field.value, no_backslash_escapes)
            );
        }
        DefaultValue::CurrentTime if matches!(field.kind, "TIMESTAMP" | "DATETIME") => {
            out.push_str(" DEFAULT CURRENT_TIMESTAMP")
        }
        DefaultValue::CurrentTime => return Err(format!("{name} 的类型不能设置当前时间默认值。")),
    }
    if auto {
        out.push_str(" AUTO_INCREMENT");
    }
    if !field.comment.is_empty() {
        let _ = write!(
            &mut out,
            " COMMENT {}",
            literal(&field.comment, no_backslash_escapes)
        );
    }
    Ok(out)
}

fn append_identifier(sql: &mut String, name: &str, value: &str) -> Result<(), String> {
    if let Some(value) = option_identifier(name, value)? {
        let _ = write!(sql, " {name} {value}");
    }
    Ok(())
}

fn option_identifier<'a>(name: &str, value: &'a str) -> Result<Option<&'a str>, String> {
    let value = value.trim();
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(format!("{name} 只能使用字母、数字和下划线。"));
    }
    Ok((!value.is_empty()).then_some(value))
}

fn required_name(name: &str, message: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(message.to_owned());
    }
    if name.chars().count() > 64 || name.contains('\0') {
        return Err("库、表和字段名称最长 64 个字符，不可包含空字符。".into());
    }
    Ok(quote_identifier(name))
}

fn literal(input: &str, no_backslash_escapes: bool) -> String {
    let input = if no_backslash_escapes {
        input.to_owned()
    } else {
        input.replace('\\', "\\\\")
    };
    format!("'{}'", input.replace('\'', "''"))
}

fn is_integer(kind: &str) -> bool {
    matches!(
        kind,
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "BIGINT"
    )
}

fn is_numeric(kind: &str) -> bool {
    is_integer(kind) || matches!(kind, "DECIMAL" | "FLOAT" | "DOUBLE")
}

fn is_text(kind: &str) -> bool {
    matches!(
        kind,
        "CHAR" | "VARCHAR" | "TINYTEXT" | "TEXT" | "MEDIUMTEXT" | "LONGTEXT" | "ENUM" | "SET"
    )
}

fn has_length(kind: &str) -> bool {
    is_numeric(kind)
        || matches!(
            kind,
            "BIT" | "CHAR" | "VARCHAR" | "BINARY" | "VARBINARY" | "TIME" | "DATETIME" | "TIMESTAMP"
        )
}

#[cfg(test)]
mod tests {
    use mysql::{Conn, OptsBuilder, prelude::Queryable};

    use super::*;

    fn fixture() -> Conn {
        let port = std::env::var("HARBOR_DUMP_TEST_PORT")
            .unwrap()
            .parse()
            .unwrap();
        Conn::new(
            OptsBuilder::default()
                .ip_or_hostname(Some("127.0.0.1"))
                .tcp_port(port)
                .prefer_socket(false)
                .user(Some("root")),
        )
        .unwrap()
    }

    #[test]
    #[ignore = "在一次性本机 MariaDB 中运行，需要 HARBOR_DUMP_TEST_PORT"]
    fn table_design_creates_table_with_selected_properties() {
        let mut db = fixture();
        let schema = format!("harbor_create_smoke_{}", std::process::id());
        let schema_sql = quote_identifier(&schema);
        db.query_drop(format!(
            "CREATE DATABASE {schema_sql} CHARACTER SET utf8mb4"
        ))
        .unwrap();
        let form = Designer {
            workspace: 0,
            schema,
            on_canvas: false,
            name: "订单".into(),
            fields: vec![
                Field {
                    name: "编号".into(),
                    kind: "BIGINT",
                    primary: true,
                    unsigned: true,
                    auto_increment: true,
                    ..Default::default()
                },
                Field {
                    name: "分类".into(),
                    kind: "ENUM",
                    values: "普通\n含引号'\n有\\反斜杠".into(),
                    default: DefaultValue::Literal,
                    value: "普通".into(),
                    required: true,
                    comment: "分类\\说明".into(),
                    ..Default::default()
                },
                Field {
                    name: "名称".into(),
                    kind: "VARCHAR",
                    length: "30".into(),
                    binary: true,
                    charset: "utf8mb4".into(),
                    ..Default::default()
                },
            ],
            options: Options {
                engine: "InnoDB".into(),
                row_format: Some("DYNAMIC"),
                charset: "utf8mb4".into(),
                collation: "utf8mb4_bin".into(),
                auto_increment: "23".into(),
                comment: Some("订单'备注".into()),
                ..Default::default()
            },
            languages: false,
            advanced: false,
            preview: false,
        };
        let ddl = design_sql(&form, false, None).unwrap();
        let table = format!("{schema_sql}.{}", quote_identifier(&form.name));
        let result = (|| -> mysql::Result<_> {
            db.query_drop(&ddl)?;
            db.query_drop(format!(
                "INSERT INTO {table} ({}) VALUES ('样品')",
                quote_identifier("名称")
            ))?;
            db.query_first::<(u64, String), _>(format!(
                "SELECT {}, {} FROM {table}",
                quote_identifier("编号"),
                quote_identifier("分类")
            ))
        })();
        db.query_drop(format!("DROP DATABASE {schema_sql}"))
            .unwrap();
        assert_eq!(
            result.unwrap_or_else(|error| panic!("{error}\n{ddl}")),
            Some((23, "普通".into()))
        );
    }

    #[test]
    #[ignore = "在一次性本机 MariaDB 中运行，需要 HARBOR_DUMP_TEST_PORT"]
    fn advanced_properties_apply_to_myisam_and_aria_tables() {
        let mut db = fixture();
        let schema = format!("harbor_create_options_{}", std::process::id());
        let schema_sql = quote_identifier(&schema);
        db.query_drop(format!(
            "CREATE DATABASE {schema_sql} CHARACTER SET utf8mb4"
        ))
        .unwrap();
        let mut form = Designer {
            workspace: 0,
            schema,
            on_canvas: false,
            name: "myisam_rows".into(),
            fields: vec![Field {
                name: "id".into(),
                primary: true,
                ..Default::default()
            }],
            options: Options {
                engine: "MyISAM".into(),
                row_format: Some("FIXED"),
                average_row_length: "48".into(),
                min_rows: "25".into(),
                max_rows: "200".into(),
                checksum: Some(true),
                delayed_key_write: Some(true),
                ..Default::default()
            },
            languages: false,
            advanced: true,
            preview: true,
        };
        let result = (|| -> Result<_, Box<dyn std::error::Error>> {
            db.query_drop(design_sql(&form, false, None)?)?;
            let (_, myisam) = db
                .query_first::<(String, String), _>(format!(
                    "SHOW CREATE TABLE {schema_sql}.myisam_rows"
                ))?
                .ok_or("读取 MyISAM 表失败。")?;
            form.name = "aria_pages".into();
            form.options = Options {
                engine: "Aria".into(),
                row_format: Some("PAGE"),
                ..Default::default()
            };
            db.query_drop(design_sql(&form, false, None)?)?;
            let (_, aria) = db
                .query_first::<(String, String), _>(format!(
                    "SHOW CREATE TABLE {schema_sql}.aria_pages"
                ))?
                .ok_or("读取 Aria 表失败。")?;
            Ok((myisam, aria))
        })();
        db.query_drop(format!("DROP DATABASE {schema_sql}"))
            .unwrap();
        let (myisam, aria) = result.unwrap();
        for option in [
            "ENGINE=MyISAM",
            "ROW_FORMAT=FIXED",
            "AVG_ROW_LENGTH=48",
            "MIN_ROWS=25",
            "MAX_ROWS=200",
            "CHECKSUM=1",
            "DELAY_KEY_WRITE=1",
        ] {
            assert!(myisam.contains(option), "{myisam}");
        }
        assert!(
            aria.contains("ENGINE=Aria") && aria.contains("ROW_FORMAT=PAGE"),
            "{aria}"
        );
    }
}
