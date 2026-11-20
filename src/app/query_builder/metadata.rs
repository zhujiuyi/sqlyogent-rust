use std::collections::{BTreeSet, HashMap, HashSet};

use crate::{
    app::{completion, state::Workspace},
    database::{ColumnInfo, Command, DbObject, ObjectKind},
};

use super::{Column, Join, JoinCondition, JoinKind, Query};

pub(super) fn column_details<'a>(
    workspace: &'a Workspace,
    object: &DbObject,
) -> Option<&'a [ColumnInfo]> {
    workspace
        .schema_designs
        .get(&object.schema)
        .and_then(|design| design.tables.iter().find(|table| table.object == *object))
        .map(|table| table.columns.as_slice())
        .filter(|columns| !columns.is_empty())
        .or_else(|| {
            workspace
                .detail
                .as_ref()
                .filter(|detail| detail.object == *object && !detail.columns.is_empty())
                .map(|detail| detail.columns.as_slice())
        })
}

#[derive(Clone, Eq, Hash, PartialEq)]
enum Read {
    Catalog(String),
    ForeignKeys(String),
    Columns(DbObject),
}

#[derive(Default)]
pub(super) struct Metadata {
    pub(super) columns: HashMap<DbObject, Vec<String>>,
    pub(super) auto_join: HashSet<usize>,
    current: Option<Read>,
    failed: HashSet<Read>,
    live: HashSet<DbObject>,
}

impl Metadata {
    pub(super) fn columns(&self, object: &DbObject) -> &[String] {
        self.columns.get(object).map_or(&[], Vec::as_slice)
    }

    pub(super) fn receive(&mut self, query: &Query, workspace: &Workspace) {
        if workspace.server.is_none() {
            self.current = None;
            self.live.clear();
            self.failed.clear();
            return;
        }
        for source in &query.sources {
            let object = &source.object;
            if self.live.contains(object) {
                continue;
            }
            let columns = column_details(workspace, object)
                .map(|columns| columns.iter().map(|column| column.name.clone()).collect())
                .or_else(|| workspace.tags.columns(object).map(<[String]>::to_vec));
            if let Some(columns) = columns {
                self.columns.insert(object.clone(), columns);
                self.live.insert(object.clone());
            }
        }
        if !workspace.busy()
            && let Some(read) = self.current.take()
            && !self.loaded(&read, workspace)
        {
            self.failed.insert(read);
        }
    }

    pub(super) fn request(&mut self, query: &mut Query, workspace: &mut Workspace) -> bool {
        if workspace.busy() || workspace.server.is_none() {
            return false;
        }
        if query.schema.is_none() {
            query.schema = workspace
                .server
                .as_ref()
                .and_then(|server| server.database.clone())
                .or_else(|| workspace.schemas.first().cloned());
        }
        let schemas = query
            .sources
            .iter()
            .filter(|source| source.object.kind == ObjectKind::Table)
            .map(|source| source.object.schema.clone())
            .collect::<BTreeSet<_>>();
        let multiple_tables = query
            .sources
            .iter()
            .filter(|source| source.object.kind == ObjectKind::Table)
            .nth(1)
            .is_some();
        let changed = multiple_tables
            && schemas
                .iter()
                .all(|schema| workspace.schema_designs.contains_key(schema))
            && self.apply_automatic_joins(query, workspace);
        let catalog = query.schema.clone().map(Read::Catalog);
        let relation_reads = schemas
            .into_iter()
            .filter(|_| multiple_tables)
            .map(Read::ForeignKeys);
        let column_reads = query
            .sources
            .iter()
            .map(|source| Read::Columns(source.object.clone()));
        if let Some(read) = catalog
            .into_iter()
            .chain(relation_reads)
            .chain(column_reads)
            .find(|read| !self.loaded(read, workspace) && !self.failed.contains(read))
        {
            let started = match &read {
                Read::Catalog(schema) => workspace.send(
                    Command::ListObjects {
                        schema: schema.clone(),
                    },
                    "正在读取表与视图…",
                ),
                Read::ForeignKeys(schema) => workspace.send(
                    Command::DescribeSchemaDesign {
                        schema: schema.clone(),
                    },
                    "正在读取外键关系…",
                ),
                Read::Columns(object) => {
                    completion::read_detail(workspace, object.clone(), "正在读取列信息…")
                }
            };
            if started {
                self.current = Some(read);
            } else {
                self.failed.insert(read);
            }
        }
        changed
    }

    fn loaded(&self, read: &Read, workspace: &Workspace) -> bool {
        match read {
            Read::Catalog(schema) => workspace.objects.contains_key(schema),
            Read::ForeignKeys(schema) => workspace.schema_designs.contains_key(schema),
            Read::Columns(object) => self.live.contains(object),
        }
    }

    pub(super) fn loading(&self) -> bool {
        self.current.is_some()
    }

    pub(super) fn failed(&self, query: &Query, workspace: &Workspace) -> bool {
        self.failed
            .iter()
            .filter(|read| !self.loaded(read, workspace))
            .any(|read| match read {
                Read::Catalog(schema) => query.schema.as_ref() == Some(schema),
                Read::ForeignKeys(schema) => query
                    .sources
                    .iter()
                    .any(|source| source.object.schema == *schema),
                Read::Columns(object) => {
                    query.sources.iter().any(|source| source.object == *object)
                }
            })
    }

    pub(super) fn retry(&mut self) {
        self.failed.clear();
    }

    fn apply_automatic_joins(&mut self, query: &mut Query, workspace: &Workspace) -> bool {
        if self.auto_join.is_empty() {
            return false;
        }
        let mut by_table = HashMap::<(usize, DbObject), Vec<Suggestion>>::new();
        for suggestion in suggestions(query, workspace) {
            let join = &suggestion.join;
            let newest = join.left.max(join.right);
            let other = join.left.min(join.right);
            if self.auto_join.contains(&newest)
                && !query.joins.iter().any(|join| join.contains(newest))
            {
                let object = query
                    .source(other)
                    .expect("foreign key source")
                    .object
                    .clone();
                by_table
                    .entry((newest, object))
                    .or_default()
                    .push(suggestion);
            }
        }
        self.auto_join.clear();
        let mut joins = by_table
            .into_values()
            .filter_map(|mut candidates| {
                (candidates.len() == 1).then(|| candidates.pop().expect("one foreign key").join)
            })
            .collect::<Vec<_>>();
        joins.sort_by_key(|join| (join.left.max(join.right), join.left.min(join.right)));
        let changed = !joins.is_empty();
        query.joins.extend(joins);
        changed
    }
}

pub(super) struct Suggestion {
    pub(super) label: String,
    pub(super) join: Join,
}

pub(super) fn suggestions(query: &Query, workspace: &Workspace) -> Vec<Suggestion> {
    let mut tables: HashMap<&DbObject, Vec<_>> = HashMap::new();
    for source in &query.sources {
        tables.entry(&source.object).or_default().push(source);
    }
    let existing = query
        .joins
        .iter()
        .map(|join| (join.left.min(join.right), join.left.max(join.right)))
        .collect::<HashSet<_>>();
    let schemas = tables
        .keys()
        .map(|object| object.schema.as_str())
        .collect::<BTreeSet<_>>();
    let mut choices = Vec::new();
    for key in schemas
        .into_iter()
        .filter_map(|schema| workspace.schema_designs.get(schema))
        .flat_map(|design| &design.foreign_keys)
    {
        let (Some(left), Some(right)) = (tables.get(&key.table), tables.get(&key.referenced_table))
        else {
            continue;
        };
        if key.columns.is_empty() {
            continue;
        }
        for from in left {
            for to in right {
                if from.id == to.id || existing.contains(&(from.id.min(to.id), from.id.max(to.id)))
                {
                    continue;
                }
                choices.push(Suggestion {
                    label: format!("{} → {} · {}", from.label(), to.label(), key.name),
                    join: Join {
                        left: from.id,
                        right: to.id,
                        kind: JoinKind::Inner,
                        conditions: key
                            .columns
                            .iter()
                            .map(|(left, right)| JoinCondition {
                                left: Some(Column {
                                    table: from.id,
                                    name: left.clone(),
                                }),
                                right: Some(Column {
                                    table: to.id,
                                    name: right.clone(),
                                }),
                            })
                            .collect(),
                    },
                });
            }
        }
    }
    choices
}
