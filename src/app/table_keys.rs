use std::collections::HashSet;

use eframe::egui::{self, Button, RichText, TextEdit};

use crate::database::{
    ColumnInfo, Command, DbObject, IndexDefinition, IndexVisibility, SchemaForeignKey, TableKeys,
    quote_identifier,
};

use super::{state::Workspace, style};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Kind {
    Index,
    Foreign,
}

pub(super) struct Manager {
    workspace: usize,
    object: DbObject,
    kind: Kind,
    selected: Option<usize>,
    draft: Option<Draft>,
}

enum Draft {
    Index(IndexDraft),
    Foreign(ForeignDraft),
    Delete(String),
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum IndexKind {
    Primary,
    Normal,
    Unique,
    Fulltext,
    Spatial,
}

impl IndexKind {
    const ALL: [Self; 5] = [
        Self::Normal,
        Self::Primary,
        Self::Unique,
        Self::Fulltext,
        Self::Spatial,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Normal => "普通索引",
            Self::Primary => "主键",
            Self::Unique => "唯一索引",
            Self::Fulltext => "全文索引",
            Self::Spatial => "空间索引",
        }
    }

    fn from(index: &IndexDefinition) -> Self {
        if primary(&index.name) {
            Self::Primary
        } else if index.method == "FULLTEXT" {
            Self::Fulltext
        } else if index.method == "SPATIAL" || index.method == "RTREE" {
            Self::Spatial
        } else if index.unique {
            Self::Unique
        } else {
            Self::Normal
        }
    }
}

struct IndexDraft {
    previous: Option<String>,
    name: String,
    kind: IndexKind,
    parts: Vec<(String, u64, bool)>,
    comment: String,
    visible: bool,
    hash: bool,
}

impl IndexDraft {
    fn new(original: Option<&IndexDefinition>) -> Self {
        let Some(index) = original else {
            return Self {
                previous: None,
                name: String::new(),
                kind: IndexKind::Normal,
                parts: vec![(String::new(), 0, false)],
                comment: String::new(),
                visible: true,
                hash: false,
            };
        };
        Self {
            previous: Some(index.name.clone()),
            name: index.name.clone(),
            kind: IndexKind::from(index),
            parts: index
                .parts
                .iter()
                .map(|part| {
                    (
                        part.column.clone().unwrap_or_default(),
                        part.prefix.unwrap_or(0),
                        part.descending,
                    )
                })
                .collect(),
            comment: index.comment.clone(),
            visible: index.visible,
            hash: index.method == "HASH",
        }
    }
}

struct ForeignDraft {
    previous: Option<String>,
    temporary_name: String,
    name: String,
    reference_schema: String,
    reference_table: String,
    parts: Vec<(String, String)>,
    on_delete: usize,
    on_update: usize,
}

const ACTIONS: [(&str, &str); 5] = [
    ("默认", ""),
    ("限制（RESTRICT）", "RESTRICT"),
    ("级联（CASCADE）", "CASCADE"),
    ("设为 NULL", "SET NULL"),
    ("无操作（NO ACTION）", "NO ACTION"),
];

impl ForeignDraft {
    fn new(schema: &str, original: Option<&SchemaForeignKey>) -> Self {
        let Some(key) = original else {
            return Self {
                previous: None,
                name: String::new(),
                reference_schema: schema.to_owned(),
                temporary_name: String::new(),
                reference_table: String::new(),
                parts: vec![(String::new(), String::new())],
                on_delete: 0,
                on_update: 0,
            };
        };
        Self {
            previous: Some(key.name.clone()),
            name: key.name.clone(),
            reference_schema: key.referenced_table.schema.clone(),
            temporary_name: format!(
                "harbor_fk_{:x}_{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
                std::process::id()
            ),
            reference_table: key.referenced_table.name.clone(),
            parts: key.columns.clone(),
            on_delete: ACTIONS
                .iter()
                .position(|(_, text)| *text == key.on_delete)
                .unwrap_or(0),
            on_update: ACTIONS
                .iter()
                .position(|(_, text)| *text == key.on_update)
                .unwrap_or(0),
        }
    }
}

impl Manager {
    pub(super) fn open(workspace: usize, object: DbObject, kind: Kind, ws: &mut Workspace) -> Self {
        ws.table_keys = None;
        ws.send(
            Command::DescribeTableKeys {
                object: object.clone(),
            },
            "正在读取索引和外键…",
        );
        Self {
            workspace,
            object,
            kind,
            selected: None,
            draft: None,
        }
    }
}

pub(super) fn dialog(
    ctx: &egui::Context,
    state: &mut Option<Manager>,
    workspaces: &mut [Workspace],
) {
    let Some(manager) = state else { return };
    let ws = &mut workspaces[manager.workspace];
    let mut exit = false;
    let mut stage = None;
    let screen = ctx.content_rect();
    crate::app::style::modal(ctx, egui::Id::new("table-key-manager")).show(ctx, |ui| {
        ui.set_min_width((screen.width() - 72.0).clamp(550.0, 920.0));
        ui.heading(if manager.kind == Kind::Index {
            "管理索引"
        } else {
            "管理外键"
        });
        ui.label(format!("{}.{}", manager.object.schema, manager.object.name));
        ui.separator();
        if ws.busy() {
            ui.spinner();
        } else if let Some(keys) = ws
            .table_keys
            .as_ref()
            .filter(|keys| keys.object == manager.object)
        {
            egui::ScrollArea::vertical()
                .id_salt("key-manager-body")
                .max_height((screen.height() - 165.0).max(220.0))
                .show(ui, |ui| {
                    show_objects(ui, manager, keys);
                    if let Some(draft) = &mut manager.draft {
                        ui.separator();
                        match draft {
                            Draft::Index(draft) => show_index(ui, draft, keys),
                            Draft::Foreign(draft) => show_foreign(ui, draft, keys, &ws.schemas),
                            Draft::Delete(name) => {
                                ui.label(format!("待删除：{name}"));
                            }
                        }
                        match script(manager.kind, draft, keys) {
                            Ok(sql) => {
                                ui.collapsing("预览 SQL", |ui| {
                                    let mut preview = sql.as_str();
                                    ui.add(
                                        TextEdit::multiline(&mut preview)
                                            .code_editor()
                                            .desired_width(f32::INFINITY),
                                    );
                                });
                                if ui.add(style::primary("在编辑器中打开 SQL")).clicked() {
                                    stage = Some(sql);
                                }
                            }
                            Err(error) => {
                                ui.label(RichText::new(error).color(style::MUTED));
                            }
                        }
                    }
                });
        } else {
            ui.colored_label(style::ERROR, &ws.status);
        }
        ui.separator();
        exit = ui.button("关闭").clicked();
    });
    if let Some(sql) = stage {
        ws.new_tab(sql, None);
        *state = None;
    } else if exit {
        *state = None;
    }
}

fn show_objects(ui: &mut egui::Ui, manager: &mut Manager, keys: &TableKeys) {
    egui::ScrollArea::vertical()
        .id_salt("key-manager-list")
        .max_height(132.0)
        .show(ui, |ui| match manager.kind {
            Kind::Index => {
                for (row, index) in keys.indexes.iter().enumerate() {
                    let parts = index
                        .parts
                        .iter()
                        .map(|part| {
                            part.column
                                .as_deref()
                                .or(part.expression.as_deref())
                                .unwrap_or("未知字段")
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    ui.selectable_value(
                        &mut manager.selected,
                        Some(row),
                        format!(
                            "{}  ·  {}  ·  {parts}",
                            index.name,
                            IndexKind::from(index).label()
                        ),
                    );
                }
            }
            Kind::Foreign => {
                for (row, key) in keys.foreign_keys.iter().enumerate() {
                    let target = &key.referenced_table;
                    ui.selectable_value(
                        &mut manager.selected,
                        Some(row),
                        format!("{}  ·  {}.{}", key.name, target.schema, target.name),
                    );
                }
            }
        });
    let index = manager.selected.and_then(|index| keys.indexes.get(index));
    let foreign = manager
        .selected
        .and_then(|index| keys.foreign_keys.get(index));
    let editable = match manager.kind {
        Kind::Index => index.is_some_and(|index| {
            index.standard_options
                && index
                    .parts
                    .iter()
                    .all(|part| part.column.is_some() && part.expression.is_none())
        }),
        Kind::Foreign => foreign.is_some_and(|key| {
            [&key.on_delete, &key.on_update]
                .into_iter()
                .all(|rule| ACTIONS[1..].iter().any(|(_, action)| *action == rule))
        }),
    };
    let name = match manager.kind {
        Kind::Index => index.map(|index| index.name.as_str()),
        Kind::Foreign => foreign.map(|key| key.name.as_str()),
    };
    ui.horizontal(|ui| {
        if ui.button("新建").clicked() {
            manager.draft = Some(match manager.kind {
                Kind::Index => Draft::Index(IndexDraft::new(None)),
                Kind::Foreign => Draft::Foreign(ForeignDraft::new(&manager.object.schema, None)),
            });
        }
        if ui
            .add_enabled(editable, Button::new("修改"))
            .on_disabled_hover_text("存在未识别的原始属性或表达式。")
            .clicked()
        {
            manager.draft = Some(match manager.kind {
                Kind::Index => Draft::Index(IndexDraft::new(index)),
                Kind::Foreign => Draft::Foreign(ForeignDraft::new(&manager.object.schema, foreign)),
            });
        }
        if ui
            .add_enabled(name.is_some(), Button::new("删除"))
            .clicked()
            && let Some(name) = name
        {
            manager.draft = Some(Draft::Delete(name.to_owned()));
        }
    });
}

fn show_index(ui: &mut egui::Ui, draft: &mut IndexDraft, keys: &TableKeys) {
    ui.horizontal_wrapped(|ui| {
        egui::ComboBox::from_id_salt("index-type")
            .selected_text(draft.kind.label())
            .show_ui(ui, |ui| {
                for kind in IndexKind::ALL {
                    ui.selectable_value(&mut draft.kind, kind, kind.label());
                }
            });
        if draft.kind != IndexKind::Primary {
            field_text(ui, "索引名称", &mut draft.name, 180.0);
            if keys.visibility.is_some() {
                ui.checkbox(&mut draft.visible, "供优化器使用");
            }
        }
        if matches!(
            draft.kind,
            IndexKind::Primary | IndexKind::Normal | IndexKind::Unique
        ) {
            ui.checkbox(&mut draft.hash, "HASH 索引");
        }
    });
    let mut swap = None;
    let mut delete = None;
    let limit = draft.parts.len();
    for (row, (column, prefix, descending)) in draft.parts.iter_mut().enumerate() {
        ui.push_id(row, |ui| {
            ui.horizontal_wrapped(|ui| {
                field_choice(ui, column, &keys.columns);
                ui.label("前缀长度");
                ui.add(egui::DragValue::new(prefix).range(0..=u32::MAX as u64))
                    .on_hover_text("0 表示索引整个字段");
                ui.checkbox(descending, "降序");
                if ui.add_enabled(row > 0, Button::new("上移")).clicked() {
                    swap = Some((row, row - 1));
                }
                if ui
                    .add_enabled(row + 1 < limit, Button::new("下移"))
                    .clicked()
                {
                    swap = Some((row, row + 1));
                }
                if ui.small_button("移除").clicked() {
                    delete = Some(row);
                }
            });
        });
    }
    if let Some((a, b)) = swap {
        draft.parts.swap(a, b);
    }
    if let Some(row) = delete {
        draft.parts.remove(row);
    }
    if ui.button("添加字段").clicked() {
        draft.parts.push((String::new(), 0, false));
    }
    ui.horizontal(|ui| {
        field_text(ui, "备注", &mut draft.comment, 340.0);
    });
}

fn show_foreign(ui: &mut egui::Ui, draft: &mut ForeignDraft, keys: &TableKeys, schemas: &[String]) {
    ui.horizontal_wrapped(|ui| {
        field_text(ui, "外键名称", &mut draft.name, 165.0);
        ui.label("引用数据库");
        egui::ComboBox::from_id_salt("reference-schema")
            .selected_text(&draft.reference_schema)
            .show_ui(ui, |ui| {
                for schema in schemas {
                    ui.selectable_value(&mut draft.reference_schema, schema.clone(), schema);
                }
            });
        field_text(ui, "引用表", &mut draft.reference_table, 180.0);
    });
    let mut delete = None;
    for (row, (column, reference)) in draft.parts.iter_mut().enumerate() {
        ui.push_id(row, |ui| {
            ui.horizontal_wrapped(|ui| {
                field_choice(ui, column, &keys.columns);
                ui.label("→");
                field_text(ui, "引用字段", reference, 180.0);
                if ui.small_button("移除").clicked() {
                    delete = Some(row);
                }
            });
        });
    }
    if let Some(row) = delete {
        draft.parts.remove(row);
    }
    if ui.button("添加字段映射").clicked() {
        draft.parts.push((String::new(), String::new()));
    }
    ui.horizontal_wrapped(|ui| {
        action_choice(ui, "删除引用行时", &mut draft.on_delete);
        action_choice(ui, "更新引用键时", &mut draft.on_update);
    });
}

fn field_text(ui: &mut egui::Ui, label: &str, value: &mut String, width: f32) {
    ui.label(label);
    ui.add(TextEdit::singleline(value).desired_width(width));
}

fn field_choice(ui: &mut egui::Ui, value: &mut String, columns: &[ColumnInfo]) {
    egui::ComboBox::from_id_salt("selected-column")
        .selected_text(if value.is_empty() {
            "请选择字段"
        } else {
            value.as_str()
        })
        .show_ui(ui, |ui| {
            for column in columns {
                ui.selectable_value(value, column.name.clone(), &column.name);
            }
        });
}

fn action_choice(ui: &mut egui::Ui, label: &str, value: &mut usize) {
    ui.label(label);
    egui::ComboBox::from_id_salt(label)
        .selected_text(ACTIONS[*value].0)
        .show_ui(ui, |ui| {
            for (index, (label, _)) in ACTIONS.iter().enumerate() {
                ui.selectable_value(value, index, *label);
            }
        });
}

fn script(kind: Kind, draft: &Draft, keys: &TableKeys) -> Result<String, String> {
    let table = format!(
        "{}.{}",
        quote_identifier(&keys.object.schema),
        quote_identifier(&keys.object.name)
    );
    let mut clauses = Vec::new();
    match draft {
        Draft::Delete(name) => clauses.push(match kind {
            Kind::Index => drop_index(name),
            Kind::Foreign => format!("DROP FOREIGN KEY {}", quote_identifier(name)),
        }),
        Draft::Index(draft) => {
            if let Some(name) = &draft.previous {
                clauses.push(drop_index(name));
            }
            clauses.push(add_index(draft, keys)?);
        }
        Draft::Foreign(draft) => {
            let add = add_foreign(draft, keys, None)?;
            if let Some(name) = &draft.previous {
                let drop = format!("DROP FOREIGN KEY {}", quote_identifier(name));
                if name.eq_ignore_ascii_case(draft.name.trim()) {
                    // Some engines cannot drop/recreate the same FK name in one ALTER.
                    // Validate the replacement before removing either enforcing key.
                    let temporary = add_foreign(draft, keys, Some(&draft.temporary_name))?;
                    let remove_temporary = format!(
                        "DROP FOREIGN KEY {}",
                        quote_identifier(&draft.temporary_name)
                    );
                    return Ok(format!(
                        "-- 先用临时名称验证新外键，再重建原名称。若中途报错，请检查仍保留的约束。\n\
                         ALTER TABLE {table} {temporary};\nALTER TABLE {table} {drop};\n\
                         ALTER TABLE {table} {add};\nALTER TABLE {table} {remove_temporary};"
                    ));
                }
                clauses.push(drop);
            }
            clauses.push(add);
        }
    }
    Ok(format!(
        "ALTER TABLE {table}\n    {};",
        clauses.join(",\n    ")
    ))
}

fn primary(name: &str) -> bool {
    name.eq_ignore_ascii_case("PRIMARY")
}

fn drop_index(name: &str) -> String {
    if primary(name) {
        "DROP PRIMARY KEY".into()
    } else {
        format!("DROP INDEX {}", quote_identifier(name))
    }
}

fn names_unique<'a>(names: impl Iterator<Item = &'a str>) -> bool {
    let mut seen = HashSet::new();
    names
        .into_iter()
        .all(|name| !name.trim().is_empty() && seen.insert(name.trim().to_lowercase()))
}

fn identifier(value: &str, message: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() || value.contains('\0') || value.chars().count() > 64 {
        Err(message.to_owned())
    } else {
        Ok(quote_identifier(value))
    }
}

fn add_index(draft: &IndexDraft, keys: &TableKeys) -> Result<String, String> {
    if draft.parts.is_empty() || !names_unique(draft.parts.iter().map(|(col, _, _)| col.as_str())) {
        return Err("请选择至少一个字段，同一字段只能添加一次。".into());
    }
    let keyword = match draft.kind {
        IndexKind::Primary => "PRIMARY KEY",
        IndexKind::Normal => "INDEX",
        IndexKind::Unique => "UNIQUE INDEX",
        IndexKind::Fulltext => "FULLTEXT INDEX",
        IndexKind::Spatial => "SPATIAL INDEX",
    };
    let unnamed = draft.kind == IndexKind::Primary;
    let name = if unnamed {
        "PRIMARY"
    } else {
        draft.name.trim()
    };
    if !unnamed && primary(name) {
        return Err("PRIMARY 是主键的名称。".into());
    }
    if keys.indexes.iter().any(|index| {
        index.name.eq_ignore_ascii_case(name)
            && !draft
                .previous
                .as_ref()
                .is_some_and(|previous| index.name == *previous)
    }) {
        return Err("已有同名索引或主键。".into());
    }
    let mut sql = format!("ADD {keyword}");
    if !unnamed {
        sql.push(' ');
        sql.push_str(&identifier(name, "请填写有效的索引名称。")?);
    }
    if draft.hash
        && matches!(
            draft.kind,
            IndexKind::Primary | IndexKind::Normal | IndexKind::Unique
        )
    {
        sql.push_str(" USING HASH");
    }
    let parts = draft
        .parts
        .iter()
        .map(|(name, prefix, descending)| {
            let mut column = quote_identifier(name);
            if matches!(draft.kind, IndexKind::Fulltext | IndexKind::Spatial)
                && (*prefix != 0 || *descending)
            {
                return Err("全文索引和空间索引不能指定字段前缀或排序方向。");
            }
            if *prefix != 0 {
                column.push_str(&format!("({prefix})"));
            }
            if *descending {
                column.push_str(" DESC");
            }
            Ok(column)
        })
        .collect::<Result<Vec<_>, _>>()?;
    sql.push_str(&format!(" ({})", parts.join(", ")));
    if !draft.comment.is_empty() {
        let escaped = if keys.no_backslash_escapes {
            draft.comment.clone()
        } else {
            draft.comment.replace('\\', "\\\\")
        };
        sql.push_str(&format!(" COMMENT '{}'", escaped.replace('\'', "''")));
    }
    if !unnamed && !draft.visible {
        match keys.visibility {
            Some(IndexVisibility::MySql) => sql.push_str(" INVISIBLE"),
            Some(IndexVisibility::MariaDb) => sql.push_str(" IGNORED"),
            None => {}
        }
    }
    Ok(sql)
}

fn add_foreign(
    draft: &ForeignDraft,
    keys: &TableKeys,
    constraint_name: Option<&str>,
) -> Result<String, String> {
    if draft.parts.is_empty()
        || !names_unique(draft.parts.iter().map(|(column, _)| column.as_str()))
        || !names_unique(draft.parts.iter().map(|(_, reference)| reference.as_str()))
    {
        return Err("请填写至少一项字段映射，两侧字段均不能重复。".into());
    }
    let name = identifier(
        constraint_name.unwrap_or(&draft.name),
        "请填写有效的外键名称。",
    )?;
    if keys.foreign_keys.iter().any(|key| {
        key.name.eq_ignore_ascii_case(draft.name.trim())
            && draft.previous.as_ref() != Some(&key.name)
    }) {
        return Err("已有同名外键。".into());
    }
    if (draft.on_delete == 3 || draft.on_update == 3)
        && draft.parts.iter().any(|(name, _)| {
            !keys
                .columns
                .iter()
                .any(|column| &column.name == name && column.nullable)
        })
    {
        return Err("选择 SET NULL 时，本表的每个外键字段必须允许 NULL。".into());
    }
    let schema = identifier(&draft.reference_schema, "请选择引用数据库。")?;
    let table = identifier(&draft.reference_table, "请填写有效的引用表名。")?;
    let local = draft
        .parts
        .iter()
        .map(|(name, _)| quote_identifier(name))
        .collect::<Vec<_>>();
    let reference = draft
        .parts
        .iter()
        .map(|(_, name)| identifier(name, "请填写有效的引用字段名。"))
        .collect::<Result<Vec<_>, _>>()?;
    let mut sql = format!(
        "ADD CONSTRAINT {name} FOREIGN KEY ({}) REFERENCES {schema}.{table} ({})",
        local.join(", "),
        reference.join(", ")
    );
    if draft.on_delete != 0 {
        sql.push_str(&format!(" ON DELETE {}", ACTIONS[draft.on_delete].1));
    }
    if draft.on_update != 0 {
        sql.push_str(&format!(" ON UPDATE {}", ACTIONS[draft.on_update].1));
    }
    Ok(sql)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::{ConnectionConfig, ObjectKind, Response, Worker};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    #[ignore = "在一次性本机 MariaDB 中运行，需要 HARBOR_KEYS_TEST_PORT"]
    fn modifies_index_parts_and_foreign_key_actions() {
        let port = std::env::var("HARBOR_KEYS_TEST_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let worker = Worker::new();
        let mut id = 0;
        let mut request = |command| {
            id += 1;
            worker.send(id, command)?;
            let event = worker.recv()?;
            assert_eq!(event.id, id);
            event.result
        };
        request(Command::Connect(ConnectionConfig {
            port,
            ..Default::default()
        }))
        .unwrap();
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let schema = format!("harbor_keys_{suffix:x}");
        let db = quote_identifier(&schema);
        let object = DbObject {
            schema,
            name: "child".into(),
            kind: ObjectKind::Table,
        };
        let sql = |sql| Command::Query { sql, limit: 20 };
        let handle = |result: Response| {
            if let Response::Queried(report) = result {
                if let Some(error) = report.error {
                    return Err(error);
                }
                Ok(())
            } else {
                Err("未返回 SQL 结果。".to_owned())
            }
        };
        handle(request(sql(format!("CREATE DATABASE {db}"))).unwrap()).unwrap();
        let result = (|| -> Result<TableKeys, String> {
            handle(request(sql(format!(
                "CREATE TABLE {db}.parent(id INT PRIMARY KEY) ENGINE=InnoDB;
                 CREATE TABLE {db}.child(id INT PRIMARY KEY, parent_id INT, code VARCHAR(64) NOT NULL,
                 INDEX idx_code(code(8), parent_id), CONSTRAINT fk_parent FOREIGN KEY(parent_id) REFERENCES {db}.parent(id)) ENGINE=InnoDB;"
            )))?)?;
            let Response::TableKeys(keys) = request(Command::DescribeTableKeys {
                object: object.clone(),
            })?
            else {
                return Err("没有返回索引和外键。".into());
            };
            let index = keys
                .indexes
                .iter()
                .find(|index| index.name == "idx_code")
                .unwrap();
            assert!(index.standard_options);
            assert_eq!(index.parts[0].prefix, Some(8));
            let mut draft = IndexDraft::new(Some(index));
            draft.name = "idx_code_updated".into();
            draft.parts[0].1 = 16;
            draft.parts[1].2 = true;
            handle(request(sql(script(
                Kind::Index,
                &Draft::Index(draft),
                &keys,
            )?))?)?;
            let mut draft = ForeignDraft::new(&object.schema, keys.foreign_keys.first());
            draft.on_delete = 2;
            draft.on_update = 1;
            handle(request(sql(script(
                Kind::Foreign,
                &Draft::Foreign(draft),
                &keys,
            )?))?)?;
            let Response::TableKeys(keys) = request(Command::DescribeTableKeys { object })? else {
                return Err("没有返回修改后的索引和外键。".into());
            };
            Ok(keys)
        })();
        handle(request(sql(format!("DROP DATABASE {db}"))).unwrap()).unwrap();
        let keys = result.unwrap();
        let index = keys
            .indexes
            .iter()
            .find(|index| index.name == "idx_code_updated")
            .unwrap();
        assert_eq!(index.parts[0].prefix, Some(16));
        assert!(index.parts[1].descending);
        assert_eq!(keys.foreign_keys[0].on_delete, "CASCADE");
        assert_eq!(keys.foreign_keys[0].on_update, "RESTRICT");
    }
}
