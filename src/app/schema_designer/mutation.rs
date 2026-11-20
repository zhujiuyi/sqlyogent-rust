use super::{Designer, SchemaForeignKey, Workspace, drop_key};
use crate::{
    database::{Command, DbObject},
    sql_tools::split_script,
};

pub(super) enum Effect {
    CreateTable(DbObject),
    SaveKey(SchemaForeignKey),
    DeleteKey { table: String, name: String },
}

pub(super) struct KeyChange {
    pub sql: String,
    pub previous: Option<SchemaForeignKey>,
    pub effect: Effect,
}

impl KeyChange {
    pub(super) fn delete(key: &SchemaForeignKey) -> Self {
        Self {
            sql: drop_key(key),
            previous: Some(key.clone()),
            effect: Effect::DeleteKey {
                table: key.table.name.clone(),
                name: key.name.clone(),
            },
        }
    }
}

pub(super) enum Mutation {
    Verify(KeyChange),
    Execute {
        tab: u64,
        effect: Effect,
    },
    Refresh {
        effect: Effect,
        result: Result<(), String>,
    },
}

impl Designer {
    pub(in crate::app) fn return_from_create(
        &mut self,
        workspace: u64,
        creation: Option<(u64, DbObject)>,
    ) {
        if workspace != self.workspace {
            return;
        }
        if let Some((tab, object)) = creation
            && object.schema == self.layout.schema
        {
            self.status = None;
            self.mutation = Some(Mutation::Execute {
                tab,
                effect: Effect::CreateTable(object),
            });
        }
    }

    pub(super) fn apply_key_change(&mut self, ws: &mut Workspace, request: KeyChange) {
        if ws.busy() || ws.server.is_none() {
            self.status = Some(Err("当前连接暂时无法执行外键操作。".into()));
            return;
        }
        self.status = None;
        if let Some(previous) = &request.previous {
            if ws.send(
                Command::DescribeTableKeys {
                    object: previous.table.clone(),
                },
                "正在核对外键定义…",
            ) {
                ws.table_keys = None;
                self.mutation = Some(Mutation::Verify(request));
            } else {
                self.status = Some(Err(ws.status.clone()));
            }
        } else {
            self.run_key_change(ws, request);
        }
    }

    fn run_key_change(&mut self, ws: &mut Workspace, change: KeyChange) {
        let statements = split_script(&change.sql)
            .into_iter()
            .map(|statement| statement.sql.to_owned())
            .collect();
        ws.new_tab(change.sql, None);
        self.mutation = Some(Mutation::Execute {
            tab: ws.tab().id,
            effect: change.effect,
        });
        ws.start_batch(statements);
    }

    fn refresh_after_mutation(
        &mut self,
        ws: &mut Workspace,
        effect: Effect,
        result: Result<(), String>,
    ) {
        let schema = self.layout.schema.clone();
        if ws.send(
            Command::DescribeSchemaDesign {
                schema: schema.clone(),
            },
            "正在读取操作后的表结构…",
        ) {
            ws.schema_designs.remove(&schema);
            self.mutation = Some(Mutation::Refresh { effect, result });
        } else {
            self.status = Some(Err(result
                .err()
                .unwrap_or_else(|| format!("SQL 已执行，刷新结构失败。{}", ws.status))));
        }
    }

    pub(super) fn poll_mutation(&mut self, ws: &mut Workspace) {
        let Some(mutation) = self.mutation.take() else {
            return;
        };
        match mutation {
            Mutation::Verify(change) => {
                let original = change.previous.as_ref().expect("foreign key to verify");
                let Some(keys) = ws
                    .table_keys
                    .as_ref()
                    .filter(|keys| keys.object == original.table)
                else {
                    self.status = Some(Err(ws.status.clone()));
                    return;
                };
                if keys.foreign_keys.iter().any(|key| same_key(original, key)) {
                    self.run_key_change(ws, change);
                } else {
                    self.refresh_after_mutation(
                        ws,
                        change.effect,
                        Err("外键已变更。画布已重新读取，请核对后再操作。".into()),
                    );
                }
            }
            Mutation::Execute { tab, effect } => {
                let result = ws
                    .tabs
                    .iter()
                    .find(|item| item.id == tab)
                    .and_then(|item| item.report.as_ref())
                    .map_or_else(
                        || Err(ws.status.clone()),
                        |report| report.error.clone().map_or(Ok(()), Err),
                    );
                self.refresh_after_mutation(ws, effect, result);
            }
            Mutation::Refresh { effect, result } => {
                let Some(design) = ws.schema_designs.get(&self.layout.schema) else {
                    self.catalog_failed = Some(self.layout.schema.clone());
                    self.status = Some(Err(result
                        .err()
                        .unwrap_or_else(|| format!("SQL 已执行，刷新结构失败。{}", ws.status))));
                    return;
                };
                self.check_layout = self
                    .layout
                    .tables
                    .iter()
                    .any(|card| card.definition.is_some());
                if let Err(error) = result {
                    self.status = Some(Err(error));
                    return;
                }
                let table_for =
                    |object: &DbObject| design.tables.iter().find(|table| &table.object == object);
                self.status = Some(match effect {
                    Effect::CreateTable(object) => {
                        if let Some(table) = table_for(&object) {
                            if self.layout.tables.len() == super::TABLE_LIMIT
                                && self
                                    .layout
                                    .tables
                                    .iter()
                                    .all(|card| card.name != object.name)
                            {
                                self.status = Some(Ok("表已创建，画布已达到表数量上限。".into()));
                                return;
                            }
                            self.add_card(&object.name, table.columns.len());
                            self.selected_card = Some(object.name);
                            self.selected_key = None;
                            self.fit = true;
                            Ok("表已创建并添加到画布。".into())
                        } else {
                            Err("SQL 已执行，刷新后未找到新表。".into())
                        }
                    }
                    Effect::SaveKey(expected) => {
                        self.dirty = true;
                        if let Some(key) = design
                            .foreign_keys
                            .iter()
                            .find(|key| created_key(&expected, key))
                        {
                            self.selected_card = None;
                            self.selected_key = Some((expected.table.name, key.name.clone()));
                            Ok("外键已保存，画布已刷新。".into())
                        } else {
                            Err("SQL 已执行，刷新后未找到外键。".into())
                        }
                    }
                    Effect::DeleteKey { table, name } => {
                        self.dirty = true;
                        self.selected_key = None;
                        if design.foreign_keys.iter().any(|key| {
                            key.table.name == table && key.name.eq_ignore_ascii_case(&name)
                        }) {
                            Err("SQL 已执行，刷新后仍能读取该外键。".into())
                        } else {
                            Ok("外键已删除，画布已刷新。".into())
                        }
                    }
                });
            }
        }
    }
}

fn same_key(original: &SchemaForeignKey, live: &SchemaForeignKey) -> bool {
    live.table == original.table
        && live.name == original.name
        && live.referenced_table == original.referenced_table
        && live.columns == original.columns
        && live.on_delete == original.on_delete
        && live.on_update == original.on_update
}

fn created_key(expected: &SchemaForeignKey, live: &SchemaForeignKey) -> bool {
    let rule_matches = |left: &str, right: &str| {
        left == right
            || ["NO ACTION", "RESTRICT"].contains(&left)
                && ["NO ACTION", "RESTRICT"].contains(&right)
    };
    live.table == expected.table
        && (expected.name.is_empty() || live.name.eq_ignore_ascii_case(&expected.name))
        && live.referenced_table == expected.referenced_table
        && live.columns == expected.columns
        && rule_matches(&live.on_delete, &expected.on_delete)
        && rule_matches(&live.on_update, &expected.on_update)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::schema_designer::{Rule, qualified, tables_by_name};
    use crate::database::{ConnectionConfig, ObjectKind, quote_identifier};
    use std::{
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    fn settle(ws: &mut Workspace, mut designer: Option<&mut Designer>) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            ws.poll();
            if let Some(designer) = designer.as_deref_mut() {
                designer.update(ws);
            }
            if !ws.busy()
                && designer
                    .as_ref()
                    .is_none_or(|designer| designer.mutation.is_none())
            {
                return;
            }
            assert!(Instant::now() < deadline, "操作超时：{}", ws.status);
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    #[ignore = "在一次性本机 MariaDB 中运行，需要 HARBOR_KEYS_TEST_PORT"]
    fn applies_canvas_table_and_foreign_key_changes() {
        let port = std::env::var("HARBOR_KEYS_TEST_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let mut ws = Workspace::new("架构验证".into(), None);
        ws.connect(
            ConnectionConfig {
                port,
                ..Default::default()
            },
            None,
        );
        settle(&mut ws, None);
        assert!(ws.server.is_some(), "{}", ws.status);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let schema = format!("harbor_canvas_{nonce:x}");
        let parent = qualified(&schema, "parent");
        let child = qualified(&schema, "child");
        ws.start_batch(vec![
            format!("CREATE DATABASE {}", quote_identifier(&schema)),
            format!("CREATE TABLE {parent} (area INT NOT NULL, id INT NOT NULL, PRIMARY KEY (area, id)) ENGINE=InnoDB"),
        ]);
        settle(&mut ws, None);
        assert!(
            ws.tab().report.as_ref().unwrap().error.is_none(),
            "{}",
            ws.status
        );
        assert!(ws.send(Command::ListSchemas, "正在读取数据库…"));
        settle(&mut ws, None);

        let mut designer = Designer::new(ws.key, schema.clone(), None);
        designer.add_card("parent", 2);
        let sql = format!("CREATE TABLE {child} (area INT NULL, parent_id INT NULL) ENGINE=InnoDB");
        ws.new_tab(sql.clone(), None);
        ws.start_create_table(sql, schema.clone());
        designer.return_from_create(
            ws.key,
            Some((
                ws.tab().id,
                DbObject {
                    schema: schema.clone(),
                    name: "child".into(),
                    kind: ObjectKind::Table,
                },
            )),
        );
        settle(&mut ws, Some(&mut designer));
        assert!(
            matches!(designer.status, Some(Ok(_))),
            "{:?}",
            designer.status
        );
        assert_eq!(designer.layout.tables[1].name, "child");

        designer.new_key(1, 0, "area", "area");
        let mut draft = designer.draft_key.take().unwrap();
        draft.name = "fk_parent".into();
        draft.columns.push(("parent_id".into(), "id".into()));
        let tables = tables_by_name(ws.schema_designs.get(&schema));
        let create = draft.statement(&schema, &tables).unwrap();
        designer.apply_key_change(&mut ws, draft.change(&schema, create));
        settle(&mut ws, Some(&mut designer));
        assert!(
            matches!(designer.status, Some(Ok(_))),
            "{:?}",
            designer.status
        );
        let created = &ws.schema_designs[&schema].foreign_keys[0];
        assert_eq!(created.name, "fk_parent");
        assert_eq!(created.columns.len(), 2);

        designer.edit_key(created);
        let mut draft = designer.draft_key.take().unwrap();
        draft.on_update = Rule::Cascade;
        draft.on_delete = Rule::SetNull;
        let tables = tables_by_name(ws.schema_designs.get(&schema));
        let update = draft.statement(&schema, &tables).unwrap();
        designer.apply_key_change(&mut ws, draft.change(&schema, update));
        settle(&mut ws, Some(&mut designer));
        assert!(
            matches!(designer.status, Some(Ok(_))),
            "{:?}",
            designer.status
        );
        let keys = &ws.schema_designs[&schema].foreign_keys;
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].name, "fk_parent");
        assert_eq!(keys[0].on_update, "CASCADE");
        assert_eq!(keys[0].on_delete, "SET NULL");
        assert_eq!(keys[0].columns.len(), 2);

        let delete = KeyChange::delete(&keys[0]);
        designer.apply_key_change(&mut ws, delete);
        settle(&mut ws, Some(&mut designer));
        assert!(
            matches!(designer.status, Some(Ok(_))),
            "{:?}",
            designer.status
        );
        assert!(ws.schema_designs[&schema].foreign_keys.is_empty());
        ws.start_batch(vec![format!("DROP DATABASE {}", quote_identifier(&schema))]);
        settle(&mut ws, None);
        assert!(
            ws.tab().report.as_ref().unwrap().error.is_none(),
            "{}",
            ws.status
        );
    }
}
