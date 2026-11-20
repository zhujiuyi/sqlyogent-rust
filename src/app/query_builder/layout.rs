use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::Read as _,
    path::{Path, PathBuf},
};

use rfd::FileDialog;
use serde::{Deserialize, Serialize};

use crate::app::state::write_atomic;

use super::{
    Builder, Column, Comparison, DbObject, FieldInput, ObjectKind, Predicate, Query, Target,
};

const FORMAT: &str = "harbor-sql-query";
const VERSION: u32 = 1;
const MAX_BYTES: u64 = 16 * 1024 * 1024;
pub(super) const TABLE_LIMIT: usize = 128;
pub(super) const ID_LIMIT: usize = 1_000_000;

#[derive(Deserialize, Serialize)]
#[serde(remote = "ObjectKind")]
enum StoredKind {
    Table,
    View,
    Procedure,
    Function,
    Trigger,
    Event,
}

#[derive(Deserialize, Serialize)]
#[serde(remote = "DbObject")]
pub(super) struct StoredObject {
    schema: String,
    name: String,
    #[serde(with = "StoredKind")]
    kind: ObjectKind,
}

#[derive(Deserialize, Serialize)]
#[serde(remote = "Comparison")]
pub(super) enum StoredComparison {
    Equal,
    NotEqual,
    LessThan,
    LessOrEqual,
    GreaterThan,
    GreaterOrEqual,
    Like,
    IsNull,
    IsNotNull,
}

#[derive(Deserialize, Serialize)]
struct Document {
    format: String,
    version: u32,
    query: Query,
    #[serde(default)]
    columns: Vec<ColumnSnapshot>,
}

#[derive(Deserialize, Serialize)]
struct ColumnSnapshot {
    #[serde(with = "StoredObject")]
    object: DbObject,
    names: Vec<String>,
}

pub(in crate::app) fn pick_layout() -> Option<PathBuf> {
    FileDialog::new()
        .set_title("打开查询布局")
        .add_filter("港湾查询布局", &["query"])
        .pick_file()
}

impl Builder {
    pub(in crate::app) fn load_layout(path: PathBuf) -> Result<Self, String> {
        let document = read_document(&path)?;
        let mut builder = Self::new_blank(document.query.schema.clone());
        builder.query = document.query;
        builder.metadata.columns = document
            .columns
            .into_iter()
            .map(|entry| (entry.object, entry.names))
            .collect();
        builder.path = Some(path);
        Ok(builder)
    }

    pub(in crate::app) fn save_layout(&mut self, save_as: bool) -> bool {
        let path = if !save_as && self.path.is_some() {
            self.path.clone()
        } else {
            FileDialog::new()
                .set_title("保存查询布局")
                .add_filter("港湾查询布局", &["query"])
                .set_file_name("查询布局.query")
                .save_file()
                .map(|path| {
                    if path.extension().is_none() {
                        path.with_extension("query")
                    } else {
                        path
                    }
                })
        };
        let Some(path) = path else { return false };
        match self.write_layout(&path) {
            Ok(()) => {
                self.path = Some(path);
                self.dirty = false;
                self.status = Some(Ok("查询布局已保存。".into()));
                true
            }
            Err(error) => {
                self.status = Some(Err(format!("保存查询布局失败。{error}")));
                false
            }
        }
    }

    fn write_layout(&self, path: &Path) -> Result<(), String> {
        if !path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("query"))
        {
            return Err("请将文件扩展名设为 .query。".into());
        }
        self.query.validate()?;
        let mut seen = HashSet::new();
        let columns = self
            .query
            .sources
            .iter()
            .filter(|source| seen.insert(&source.object))
            .filter_map(|source| {
                Some(ColumnSnapshot {
                    object: source.object.clone(),
                    names: self.metadata.columns.get(&source.object)?.clone(),
                })
            })
            .collect();
        let document = Document {
            format: FORMAT.into(),
            version: VERSION,
            query: self.query.clone(),
            columns,
        };
        let bytes = serde_json::to_vec_pretty(&document).map_err(|error| error.to_string())?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("查询布局超过 16 MiB。".into());
        }
        write_atomic(path, &bytes).map_err(|error| error.to_string())
    }
}

fn read_document(path: &Path) -> Result<Document, String> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(MAX_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|error| format!("读取查询布局失败。{error}"))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("查询布局超过 16 MiB。".into());
    }
    if path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("queryxml"))
    {
        return Err("SQLyog 的 .queryxml 暂不支持导入。请选择港湾的 .query 文件。".into());
    }
    let mut document: Document =
        serde_json::from_slice(&bytes).map_err(|error| format!("无法读取查询布局。{error}"))?;
    if document.format != FORMAT || document.version != VERSION {
        return Err("此查询布局格式或版本不受支持。".into());
    }
    document.query.validate()?;
    let sources: HashSet<_> = document
        .query
        .sources
        .iter()
        .map(|source| &source.object)
        .collect();
    document.columns.retain(|entry| {
        sources.contains(&entry.object) && entry.names.iter().all(|name| identifier(name))
    });
    Ok(document)
}

impl Query {
    fn validate(&self) -> Result<(), String> {
        if self.sources.len() > TABLE_LIMIT
            || !(1..=10_000).contains(&self.limit)
            || !self.camera.valid()
            || !self.schema.as_deref().is_none_or(identifier)
        {
            return Err("查询布局的数据库、表数量、画布或行数限制无效。".into());
        }
        let sources: HashMap<_, _> = self
            .sources
            .iter()
            .map(|source| (source.id, source))
            .collect();
        if sources.len() != self.sources.len()
            || self.next_table == 0
            || self.next_table > ID_LIMIT
            || self.sources.iter().any(|source| {
                source.id == 0
                    || source.id >= self.next_table
                    || !matches!(source.object.kind, ObjectKind::Table | ObjectKind::View)
                    || !identifier(&source.object.schema)
                    || !identifier(&source.object.name)
                    || !super::canvas::valid_card(source.position, source.size)
            })
        {
            return Err("查询布局中的数据源无效。".into());
        }
        let mut relations = HashSet::new();
        let column_ok =
            |column: &Column| sources.contains_key(&column.table) && identifier(&column.name);
        if self.joins.iter().any(|join| {
            join.left == join.right
                || !sources.contains_key(&join.left)
                || !sources.contains_key(&join.right)
                || !relations.insert((join.left.min(join.right), join.left.max(join.right)))
                || join.conditions.is_empty()
                || join.conditions.iter().any(|condition| {
                    !condition
                        .left
                        .as_ref()
                        .is_none_or(|col| col.table == join.left && column_ok(col))
                        || !condition
                            .right
                            .as_ref()
                            .is_none_or(|col| col.table == join.right && column_ok(col))
                })
        }) {
            return Err("查询布局中的连接关系无效。".into());
        }
        let fields: HashSet<_> = self.fields.iter().map(|field| field.id).collect();
        if fields.len() != self.fields.len()
            || self.next_field == 0
            || self.next_field > ID_LIMIT
            || self.fields.iter().any(|field| {
                field.id == 0
                    || field.id >= self.next_field
                    || matches!(&field.input, Some(FieldInput::Column(column)) if !column_ok(column))
            })
        {
            return Err("查询布局中的结果字段无效。".into());
        }
        let target_ok = |target: &Target| match target {
            Target::Column(column) => column_ok(column),
            Target::Field(id) => fields.contains(id),
        };
        if !self.groups.iter().all(target_ok)
            || !self.sorts.iter().all(|sort| target_ok(&sort.target))
            || self.filters.iter().flatten().any(|filter| {
                matches!(filter, Predicate::Compare { target: Some(target), .. } if !target_ok(target))
            })
        {
            return Err("查询布局中的筛选、分组或排序引用了无效字段。".into());
        }
        Ok(())
    }
}

fn identifier(value: &str) -> bool {
    !value.is_empty() && !value.contains('\0')
}

#[cfg(test)]
mod tests {
    use super::super::{Aggregate, JoinKind, Sort};
    use super::*;
    use crate::{
        app::{Document, Edit, Workbench, state::Workspace},
        database::{ColumnInfo, SchemaDesign, SchemaDesignTable, SchemaForeignKey, ServerInfo},
    };

    #[test]
    fn saves_and_reopens_query_with_automatically_joined_composite_foreign_key() {
        let schema = "图书馆";
        let object = |name: &str| DbObject {
            schema: schema.into(),
            name: name.into(),
            kind: ObjectKind::Table,
        };
        let parent = object("书目");
        let child = object("借阅");
        let column = |name: &str| ColumnInfo {
            name: name.into(),
            data_type: "int".into(),
            nullable: false,
            key: String::new(),
            default_value: None,
            extra: String::new(),
        };
        let mut workspace = Workspace::new("测试连接".into(), None);
        workspace.server = Some(ServerInfo {
            version: "fixture".into(),
            connection_id: 1,
            database: Some(schema.into()),
            http_tunnel: false,
        });
        workspace
            .objects
            .insert(schema.into(), vec![parent.clone(), child.clone()]);
        workspace.schema_designs.insert(
            schema.into(),
            SchemaDesign {
                schema: schema.into(),
                tables: vec![
                    SchemaDesignTable {
                        object: parent.clone(),
                        columns: vec![column("分馆"), column("编号")],
                        unique_indexes: None,
                    },
                    SchemaDesignTable {
                        object: child.clone(),
                        columns: vec![column("图书分馆"), column("图书编号"), column("日期")],
                        unique_indexes: None,
                    },
                ],
                foreign_keys: vec![SchemaForeignKey {
                    table: child.clone(),
                    name: "fk_books".into(),
                    referenced_table: parent.clone(),
                    columns: vec![
                        ("图书分馆".into(), "分馆".into()),
                        ("图书编号".into(), "编号".into()),
                    ],
                    on_update: "RESTRICT".into(),
                    on_delete: "RESTRICT".into(),
                }],
            },
        );
        let mut builder = Builder::new_blank(Some(schema.into()));
        builder.add_source(parent, Some([25.0, 8.0]));
        builder.add_source(child.clone(), Some([305.0, 24.0]));
        assert!(builder.load_metadata(&mut workspace));
        assert_eq!(builder.query.joins.len(), 1);
        assert_eq!(builder.query.joins[0].conditions.len(), 2);
        builder.query.joins[0].kind = JoinKind::Right;
        let date = Column {
            table: 2,
            name: "日期".into(),
        };
        builder
            .query
            .add_field(Some(FieldInput::Column(date.clone())), Aggregate::None);
        builder.query.sorts.push(Sort {
            target: Target::Column(date),
            descending: true,
        });
        let expected = builder.query.sql(false).unwrap();
        assert!(expected.contains("LEFT JOIN `图书馆`.`借阅` AS `t2`"));
        assert!(
            expected
                .contains("ON `t1`.`分馆` = `t2`.`图书分馆`\n  AND `t1`.`编号` = `t2`.`图书编号`")
        );
        assert!(expected.contains("ORDER BY `t2`.`日期` DESC"));
        let binary = std::env::current_exe().unwrap();
        let folder = tempfile::tempdir_in(binary.parent().unwrap()).unwrap();
        let path = folder.path().join("借阅查询.query");
        builder.write_layout(&path).unwrap();
        let ctx = eframe::egui::Context::default();
        let mut app = Workbench::new(&ctx);
        let workspace = app.ws().key;
        app.edit(&ctx, Edit::NewQueryDesign);
        let Some(Document::Builder(blank)) = app.active_layout() else {
            panic!("missing blank layout");
        };
        app.open_query_design(path.clone());
        let Some(Document::Builder(saved)) = app.active_layout() else {
            panic!("missing opened layout");
        };
        assert_ne!(saved, blank);
        let reopened = app.design_tabs.builder_mut(saved).unwrap();
        assert!(builder.query == reopened.query);
        assert_eq!(
            reopened.metadata.columns(&child),
            ["图书分馆", "图书编号", "日期"]
        );
        assert_eq!(reopened.query.sql(false).unwrap(), expected);
        assert!(
            app.design_tabs
                .builder_mut(blank)
                .unwrap()
                .query
                .sources
                .is_empty()
        );
        app.open_query_design(path);
        assert_eq!(app.active_layout(), Some(Document::Builder(saved)));
        assert_eq!(app.design_tabs.documents(workspace).count(), 2);
    }
}
