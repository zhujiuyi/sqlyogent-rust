use eframe::egui::{self, Key, Modifiers, TextEdit};

use super::{
    Discard, Workbench, editor_tools,
    state::{Output, Workspace},
    style,
};

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(super) enum Document {
    Query(u64),
    Design(u64),
    Builder(u64),
}

#[derive(Clone, Copy)]
pub(super) enum Step {
    Previous,
    Next,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Pane {
    Explorer,
    Output,
    Editor,
}

#[derive(Clone, Copy)]
pub(super) enum Action {
    Connection(Step),
    ConnectionNumber(usize),
    Document(Step),
    RenameQuery,
    CloseDocument,
    Toggle(Pane),
}

#[derive(Clone, Copy)]
pub(super) enum Shortcut {
    Navigate(Action),
    Focus(Pane),
}

pub(super) struct RenameQuery {
    workspace: u64,
    tab: u64,
    name: String,
    focus: bool,
}

fn key_shortcut(key: Key, physical_key: Option<Key>, modifiers: Modifiers) -> Option<Shortcut> {
    use Action::*;
    use Step::{Next, Previous};

    if modifiers.mac_cmd {
        return None;
    }
    if modifiers.matches_exact(Modifiers::CTRL) {
        return match key {
            Key::Tab => Some(Connection(Next)),
            Key::PageUp => Some(Document(Previous)),
            Key::PageDown => Some(Document(Next)),
            Key::Num1 => Some(Toggle(Pane::Explorer)),
            Key::Num2 => Some(Toggle(Pane::Output)),
            Key::Num3 => Some(Toggle(Pane::Editor)),
            Key::B => return Some(Shortcut::Focus(Pane::Explorer)),
            Key::R => return Some(Shortcut::Focus(Pane::Output)),
            Key::E => return Some(Shortcut::Focus(Pane::Editor)),
            _ => None,
        }
        .map(Shortcut::Navigate);
    }
    if modifiers.matches_exact(Modifiers::CTRL | Modifiers::SHIFT) {
        return match key {
            Key::Tab => Some(Connection(Previous)),
            _ => [
                Key::Num1,
                Key::Num2,
                Key::Num3,
                Key::Num4,
                Key::Num5,
                Key::Num6,
                Key::Num7,
                Key::Num8,
                Key::Num9,
            ]
            .iter()
            .position(|&number| number == key || Some(number) == physical_key)
            .map(|index| ConnectionNumber(index + 1)),
        }
        .map(Shortcut::Navigate);
    }
    if modifiers.matches_exact(Modifiers::ALT) {
        return match key {
            Key::F2 => Some(RenameQuery),
            Key::L => Some(CloseDocument),
            _ => None,
        }
        .map(Shortcut::Navigate);
    }
    None
}

pub(super) fn shortcut(ctx: &egui::Context) -> Option<Shortcut> {
    if ctx.memory(|memory| memory.top_modal_layer().is_some()) {
        return None;
    }
    let (action, consumed_tab) = ctx.input_mut(|input| {
        let mut action = None;
        let mut consumed_tab = false;
        let mut consume_text = false;
        input.events.retain(|event| {
            if std::mem::take(&mut consume_text) && matches!(event, egui::Event::Text(_)) {
                return false;
            }
            let egui::Event::Key {
                key,
                physical_key,
                pressed: true,
                repeat,
                modifiers,
            } = *event
            else {
                return true;
            };
            let Some(pressed) = key_shortcut(key, physical_key, modifiers) else {
                return true;
            };
            consumed_tab |= key == Key::Tab;
            // The window backend can also emit text for Alt+L.
            consume_text = matches!(pressed, Shortcut::Navigate(Action::CloseDocument));
            if action.is_none()
                && (!repeat
                    || matches!(
                        pressed,
                        Shortcut::Navigate(Action::Connection(_) | Action::Document(_))
                    ))
            {
                action = Some(pressed);
            }
            false
        });
        (action, consumed_tab)
    });
    if consumed_tab {
        // egui schedules focus traversal for Tab before the app handles shortcuts.
        ctx.memory_mut(|memory| memory.move_focus(egui::FocusDirection::None));
    }
    action
}

impl Workbench {
    pub(super) fn poll_workspaces(&mut self) -> Option<(u64, u64)> {
        let mut completed = None;
        for (index, workspace) in self.workspaces.iter_mut().enumerate() {
            let connected = workspace.server.is_some();
            if let Some(tab) = workspace.poll()
                && index == self.active
            {
                completed = Some((workspace.key, tab));
            }
            if !connected && workspace.server.is_some() {
                self.design_tabs
                    .initialize_connection(workspace, &self.preferences);
            }
        }
        completed
    }

    pub(super) fn focus_after_query(
        &mut self,
        ctx: &egui::Context,
        origin: (u64, u64),
    ) -> Option<Pane> {
        let workspace = &mut self.workspaces[self.active];
        if origin != (workspace.key, workspace.tab().id)
            || self.design_tabs.active(workspace.key).is_some()
            || ctx.memory(|memory| memory.top_modal_layer().is_some())
            || egui::Popup::is_any_open(ctx)
            || ctx.input(|input| {
                !input.focused
                    || input.pointer.any_pressed()
                    || input.pointer.any_released()
                    || input.events.iter().any(|event| {
                        matches!(
                            event,
                            egui::Event::Key { pressed: true, .. }
                                | egui::Event::Text(_)
                                | egui::Event::Paste(_)
                                | egui::Event::Cut
                                | egui::Event::Copy
                                | egui::Event::Ime(_)
                        )
                    })
            })
        {
            return None;
        }
        if self.preferences.keep_focus_after_query {
            self.show_editor.then_some(Pane::Editor)
        } else {
            self.show_output = true;
            workspace.focus_request = false;
            Some(Pane::Output)
        }
    }

    pub(super) fn navigate(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::Connection(step) => {
                let target = match step {
                    Step::Previous => self
                        .connections()
                        .rev()
                        .find(|index| *index < self.active)
                        .or_else(|| self.connections().next_back()),
                    Step::Next => self
                        .connections()
                        .find(|index| *index > self.active)
                        .or_else(|| self.connections().next()),
                };
                if let Some(index) = target {
                    self.activate_workspace(ctx, index);
                }
            }
            Action::ConnectionNumber(number) => {
                let target = if number == 9 {
                    self.connections().next_back()
                } else {
                    self.connections().nth(number - 1)
                };
                if let Some(index) = target {
                    self.activate_workspace(ctx, index);
                }
            }
            Action::Document(step) => self.step_document(ctx, step),
            Action::RenameQuery => {
                let workspace = &self.workspaces[self.active];
                if self.design_tabs.active(workspace.key).is_none() {
                    self.begin_query_rename(self.active, workspace.tab().id);
                }
            }
            Action::CloseDocument => {
                if self.show_editor {
                    match self.active_layout() {
                        Some(Document::Design(id)) => {
                            if let Some(designer) = self.design_tabs.schema_mut(id) {
                                designer.request_close(self.preferences.confirm_tab_close);
                            }
                        }
                        Some(Document::Builder(id)) => self.close_query_design(ctx, id),
                        _ => self.close_query_tab(
                            ctx,
                            self.active,
                            self.workspaces[self.active].active_tab,
                        ),
                    }
                }
            }
            Action::Toggle(pane) => match pane {
                Pane::Explorer => self.show_explorer = !self.show_explorer,
                Pane::Output => self.show_output = !self.show_output,
                Pane::Editor => self.show_editor = !self.show_editor,
            },
        }
        ctx.request_repaint();
    }

    fn connections(&self) -> impl DoubleEndedIterator<Item = usize> + '_ {
        self.workspaces
            .iter()
            .enumerate()
            .filter_map(|(index, workspace)| {
                (workspace.profile.is_some() || workspace.server.is_some()).then_some(index)
            })
    }

    pub(super) fn activate_workspace(&mut self, ctx: &egui::Context, index: usize) {
        let key = self.workspaces[index].key;
        let minimized = self.workspace_windows.minimized.contains(&key);
        self.workspace_windows.restore(key);
        if self.active != index || minimized {
            self.active = index;
            self.completion.close();
            self.focus_active_query(ctx);
            ctx.request_repaint();
        }
    }

    pub(super) fn focus_active_query(&self, ctx: &egui::Context) {
        let workspace = &self.workspaces[self.active];
        if self.show_editor && self.design_tabs.active(workspace.key).is_none() {
            ctx.memory_mut(|memory| memory.request_focus(editor_tools::editor_id(workspace)));
        }
    }

    fn step_document(&mut self, ctx: &egui::Context, step: Step) {
        let workspace = &mut self.workspaces[self.active];
        let key = workspace.key;
        let sql_tabs = workspace.tabs.len();
        let designs: Vec<_> = self.design_tabs.documents(key).collect();
        let total = sql_tabs + designs.len();
        if total < 2 {
            return;
        }
        let current = self
            .design_tabs
            .active(key)
            .and_then(|document| designs.iter().position(|candidate| *candidate == document))
            .map_or(workspace.active_tab, |position| sql_tabs + position);
        let next = match step {
            Step::Previous if current == 0 => total - 1,
            Step::Previous => current - 1,
            Step::Next => (current + 1) % total,
        };
        if next < sql_tabs {
            workspace.select_tab(next);
            self.design_tabs.select_query(key);
        } else {
            self.design_tabs.select(key, designs[next - sql_tabs]);
        }
        self.show_editor = true;
        self.completion.close();
        self.focus_active_query(ctx);
    }

    pub(super) fn close_query_tab(&mut self, ctx: &egui::Context, workspace: usize, tab: usize) {
        let current = &mut self.workspaces[workspace];
        if current.row_editor_tab_protected(tab) {
            current.select_tab(tab);
            current.output = if current.editable_result_protected(tab) {
                Output::Results
            } else {
                Output::Data
            };
            self.show_output = true;
            self.design_tabs.select_query(current.key);
            current.message(
                "请先结束数据编辑操作，或保存、取消草稿再关闭查询页。",
                false,
            );
            return;
        }
        let document = &current.tabs[tab];
        if document.sql != document.saved
            && (self.preferences.confirm_tab_close || document.path.is_some())
        {
            self.discard = Some(Discard::Tab { workspace, tab });
        } else {
            current.close_tab(tab);
            self.focus_active_query(ctx);
        }
    }

    pub(super) fn close_query_design(&mut self, ctx: &egui::Context, id: u64) {
        let Some(builder) = self.design_tabs.builder_mut(id) else {
            return;
        };
        if builder.dirty() && (self.preferences.confirm_tab_close || builder.path().is_some()) {
            self.discard = Some(Discard::QueryDesign(id));
        } else {
            self.design_tabs.remove_builder(id);
            self.focus_active_query(ctx);
        }
    }

    pub(super) fn begin_query_rename(&mut self, index: usize, tab: u64) {
        let workspace = &self.workspaces[index];
        if let Some(tab) = workspace.tabs.iter().find(|candidate| candidate.id == tab) {
            self.query_rename = Some(RenameQuery {
                workspace: workspace.key,
                tab: tab.id,
                name: tab.title(),
                focus: true,
            });
            self.completion.close();
        }
    }

    pub(super) fn query_rename_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut rename) = self.query_rename.take() else {
            return;
        };
        let result =
            crate::app::style::modal(ctx, egui::Id::new("rename-query-tab")).show(ctx, |ui| {
                ui.heading("重命名查询页");
                ui.label("查询页名称");
                let mut output = TextEdit::singleline(&mut rename.name)
                    .desired_width(260.0)
                    .show(ui);
                if std::mem::take(&mut rename.focus) {
                    output.response.request_focus();
                    output
                        .state
                        .cursor
                        .set_char_range(Some(editor_tools::cursors(
                            &rename.name,
                            0..rename.name.len(),
                        )));
                    output.state.store(ui.ctx(), output.response.id);
                }
                let entered =
                    output.response.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter));
                ui.horizontal(|ui| {
                    let cancel = ui.button("取消").clicked();
                    let nonempty = !rename.name.trim().is_empty();
                    let apply = ui.add_enabled(nonempty, style::primary("确定")).clicked();
                    (nonempty && (apply || entered), cancel)
                })
                .inner
            });
        let (applied, cancelled) = result.inner;
        if applied {
            if let Some(tab) = self
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.key == rename.workspace)
                .and_then(|workspace| workspace.tabs.iter_mut().find(|tab| tab.id == rename.tab))
            {
                tab.rename(rename.name.trim().to_owned());
            }
            self.visible_document = None;
        } else if !cancelled && !result.should_close() {
            self.query_rename = Some(rename);
            return;
        }
        self.focus_active_query(ctx);
    }
}

pub(super) fn workspace_label(workspace: &Workspace) -> String {
    let icon = if workspace.busy() {
        "◌"
    } else if workspace.server.is_some() {
        "●"
    } else {
        "○"
    };
    format!("{icon}  {}", workspace.label)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::app::{
        Edit, output,
        preferences::EditorSettings,
        state::{BrowseAction, Output, Profile},
    };
    use crate::database::{
        Cell, ConnectionConfig, DbObject, ObjectKind, QueryReport, ResultSet, ServerInfo,
    };

    fn press(key: Key, physical_key: Key, modifiers: Modifiers) -> egui::RawInput {
        egui::RawInput {
            modifiers,
            events: vec![egui::Event::Key {
                key,
                physical_key: Some(physical_key),
                pressed: true,
                repeat: false,
                modifiers,
            }],
            ..Default::default()
        }
    }

    fn navigate(
        app: &mut Workbench,
        ctx: &egui::Context,
        key: Key,
        physical_key: Key,
        modifiers: Modifiers,
    ) {
        let _ = ctx.run(press(key, physical_key, modifiers), |ctx| {
            let Some(Shortcut::Navigate(action)) = shortcut(ctx) else {
                panic!("expected a navigation shortcut");
            };
            app.edit(ctx, Edit::Navigate(action));
        });
    }

    fn focus_frame(
        ctx: &egui::Context,
        key: Key,
        pane: Pane,
        mut draw: impl FnMut(&egui::Context),
    ) {
        let mut input = press(key, key, Modifiers::CTRL);
        input.events.push(egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: false,
            repeat: false,
            modifiers: Modifiers::CTRL,
        });
        input.screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(960.0, 640.0),
        ));
        let mut received = false;
        let _ = ctx.run(input, |ctx| {
            if let Some(action) = shortcut(ctx) {
                assert!(matches!(action, Shortcut::Focus(target) if target == pane));
                received = true;
            }
            draw(ctx);
        });
        assert!(received);
    }

    fn wait_for_query(app: &mut Workbench) -> Option<(u64, u64)> {
        let started = Instant::now();
        loop {
            let finished = app.poll_workspaces();
            if finished.is_some() || app.workspaces.iter().all(|workspace| !workspace.busy()) {
                return finished;
            }
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    #[ignore = "需要 HARBOR_EDITABLE_FIXTURE_PORT 指定一次性 MariaDB"]
    fn opens_selected_table_in_docked_data_and_refreshes_on_activation_only_when_enabled() {
        use mysql::{Conn, OptsBuilder, prelude::Queryable};

        let port = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let schema = format!("harbor_docked_activation_{}", std::process::id());
        let mut database = Conn::new(
            OptsBuilder::new()
                .ip_or_hostname(Some("127.0.0.1"))
                .tcp_port(port)
                .user(Some("root"))
                .prefer_socket(false),
        )
        .unwrap();
        database
            .query_drop(format!("CREATE DATABASE {schema}"))
            .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            database
                .query_drop(format!(
                    "CREATE TABLE {schema}.items (id INT PRIMARY KEY) ENGINE=InnoDB"
                ))
                .unwrap();
            database
                .query_drop(format!("INSERT INTO {schema}.items VALUES (7)"))
                .unwrap();
            database
                .query_drop(format!(
                    "CREATE TABLE {schema}.inventory (id INT PRIMARY KEY) ENGINE=InnoDB"
                ))
                .unwrap();
            let ctx = egui::Context::default();
            let mut app = Workbench::new(&ctx);
            app.preferences.table_data_default_offset = 0;
            app.preferences.table_data_default_limit = 50;
            app.preferences.table_data_default_all = false;
            app.preferences.refresh_table_data_on_focus = false;
            app.ws().connect(
                ConnectionConfig {
                    port,
                    database: Some(schema.clone()),
                    ..Default::default()
                },
                None,
            );
            wait_for_query(&mut app);
            app.ws().selected = Some(DbObject {
                schema: schema.clone(),
                name: "items".into(),
                kind: ObjectKind::Table,
            });
            app.ws().set_result_mode(output::ResultMode::Text);
            app.edit(&ctx, Edit::OpenTableData);
            assert!(app.show_output && app.row_editor.is_none());
            assert!(app.ws().output == Output::Data);
            assert_eq!(app.ws().result_mode(), output::ResultMode::Grid);
            assert!(app.ws().tab().data_editor.is_some());
            wait_for_query(&mut app);

            for refresh in [false, true] {
                app.preferences.refresh_table_data_on_focus = refresh;
                app.ws().output = Output::Results;
                app.sync_visible_data_tab(false, false);
                app.ws().output = Output::Data;
                app.sync_visible_data_tab(false, false);
                assert_eq!(app.ws().busy(), refresh);
                wait_for_query(&mut app);
                app.sync_visible_data_tab(false, false);
                assert!(!app.ws().busy());
            }

            let inventory = DbObject {
                schema: schema.clone(),
                name: "inventory".into(),
                kind: ObjectKind::Table,
            };
            let items = app.ws().selected.clone().unwrap();
            app.browse(&ctx, BrowseAction::Inspect(inventory.clone()));
            wait_for_query(&mut app);
            assert!(app.ws().output == Output::Definition);
            assert_eq!(
                app.ws().tab().data_editor.as_ref().unwrap().object(),
                Some(&items)
            );
            app.sync_visible_data_tab(false, false);
            app.ws().output = Output::Data;
            app.ws().data_entry_selection = Some(app.ws().tab().id);
            app.sync_visible_data_tab(false, false);
            wait_for_query(&mut app);
            assert_eq!(
                app.ws().tab().data_editor.as_ref().unwrap().object(),
                Some(&inventory)
            );

            app.browse(&ctx, BrowseAction::SelectObject(items.clone()));
            wait_for_query(&mut app);
            assert!(app.ws().output == Output::Data);
            assert_eq!(
                app.ws().tab().data_editor.as_ref().unwrap().object(),
                Some(&items)
            );

            app.ws().tab_mut().sql = format!("SELECT id FROM {schema}.items");
            app.edit(&ctx, Edit::Editable);
            assert!(app.show_output && app.row_editor.is_none());
            assert!(app.ws().starting_f8_on_active_tab());
            wait_for_query(&mut app);
            assert!(app.ws().output == Output::Results && app.row_editor.is_none());
            assert_eq!(
                app.ws().tab().editable_editor.as_ref().unwrap().object(),
                Some(&items)
            );
            app.edit(&ctx, Edit::ExportDisplayedRows);
            assert!(app.ws().tab().editable_editor.as_ref().unwrap().has_modal());
            assert_eq!(
                app.ws().tab().data_editor.as_ref().unwrap().object(),
                Some(&items)
            );
            assert!(!app.ws().tab().data_editor.as_ref().unwrap().has_modal());
        }));
        database
            .query_drop(format!("DROP DATABASE {schema}"))
            .unwrap();
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }

    #[test]
    #[ignore = "需要 HARBOR_QUERY_FOCUS_FIXTURE_PORT 指定一次性 MariaDB"]
    fn editor_execution_uses_the_highlight_caret_or_document() {
        let port = std::env::var("HARBOR_QUERY_FOCUS_FIXTURE_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let ctx = egui::Context::default();
        let mut app = Workbench::new(&ctx);
        app.preferences.profile_selects = false;
        app.ws().connect(
            ConnectionConfig {
                port,
                ..Default::default()
            },
            None,
        );
        assert!(wait_for_query(&mut app).is_none());
        assert!(app.ws().server.is_some(), "{}", app.ws().status);

        let first = ("SELECT /* 查询一 */ '甲;乙' AS label", "甲;乙");
        let second = ("SELECT\n  '丙' AS label", "丙");
        let third = ("SELECT '丁' AS label", "丁");
        let partial_second = ("SELECT\n  '丙'", "丙");
        let script = format!("{};\n{};\n{};", first.0, second.0, third.0);
        let selection_end = script.find(partial_second.0).unwrap() + partial_second.0.len();
        let selection = 0..selection_end;
        let caret = script.find(third.0).unwrap() + "SELECT ".len();
        app.ws().tab_mut().sql = script;

        let selected = [first, partial_second];
        let document = [first, second, third];
        let current = [third];
        let origin = (app.ws().key, app.ws().tab().id);
        for (action, range, expected) in [
            (Edit::Current, Some(selection.clone()), selected.as_slice()),
            (
                Edit::Selection,
                Some(selection.clone()),
                selected.as_slice(),
            ),
            (Edit::Editable, Some(selection.clone()), selected.as_slice()),
            (Edit::Batch, Some(selection), selected.as_slice()),
            (Edit::Batch, None, document.as_slice()),
            (Edit::Current, None, current.as_slice()),
        ] {
            let expect_batch_button = range.is_some() || matches!(action, Edit::Batch);
            let tab = app.ws().tab_mut();
            tab.focused_byte = range.as_ref().map_or(caret, |range| range.end);
            tab.selection = range;
            app.edit(&ctx, action);
            assert_eq!(app.ws().batch_stop_button(), expect_batch_button);
            assert_eq!(wait_for_query(&mut app), Some(origin));
            assert!(!app.ws().batch_stop_button());

            let report = app.ws().tab().report.as_ref().unwrap();
            assert!(report.error.is_none(), "{:?}", report.error);
            assert_eq!(report.sets.len(), expected.len());
            for (set, &(sql, value)) in report.sets.iter().zip(expected) {
                assert_eq!(set.source_sql.as_deref(), Some(sql));
                assert_eq!(set.rows, [vec![Cell::Text(value.into())]]);
            }
            assert!(app.ws().tab().editable_editor.is_none());
        }

        app.edit(&ctx, Edit::Selection);
        assert!(!app.ws().busy());
        let report = app.ws().tab().report.as_ref().unwrap();
        assert_eq!(report.sets[0].source_sql.as_deref(), Some(third.0));

        app.ws()
            .objects
            .insert("information_schema".into(), Vec::new());
        app.edit(&ctx, Edit::RefreshExplorer);
        assert!(app.ws().busy());
        assert!(wait_for_query(&mut app).is_none());
        assert!(!app.ws().objects.contains_key("information_schema"));
        let report = app.ws().tab().report.as_ref().unwrap();
        assert_eq!(report.sets[0].source_sql.as_deref(), Some(third.0));

        let selected_script = "DELIMITER $$\nSELECT '甲;乙' AS label$$";
        let tab = app.ws().tab_mut();
        tab.sql = format!("{selected_script}\nDELIMITER ;\n");
        tab.selection = Some(0..selected_script.len());
        app.edit(&ctx, Edit::Batch);
        assert_eq!(wait_for_query(&mut app), Some(origin));
        let report = app.ws().tab().report.as_ref().unwrap();
        assert_eq!(report.sets[0].rows, [vec![Cell::Text("甲;乙".into())]]);
        let tab = app.ws().tab_mut();
        tab.selection = None;
        tab.focused_byte = tab.sql.rfind("DELIMITER").unwrap() + 2;
        app.edit(&ctx, Edit::Current);
        assert!(!app.ws().busy());
    }

    #[test]
    #[ignore = "需要 HARBOR_QUERY_FOCUS_FIXTURE_PORT 指定一次性 MariaDB"]
    fn query_completion_focus_is_scoped_to_the_visible_origin() {
        let port = std::env::var("HARBOR_QUERY_FOCUS_FIXTURE_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let ctx = egui::Context::default();
        let mut app = Workbench::new(&ctx);
        app.preferences.keep_focus_after_query = true;
        app.ws().connect(
            ConnectionConfig {
                port,
                ..Default::default()
            },
            None,
        );
        assert!(wait_for_query(&mut app).is_none());
        assert!(app.ws().server.is_some());

        let source = (app.ws().key, app.ws().tab().id);
        app.ws()
            .start_editor_batch(vec!["SELECT 1".into(), "SELECT 2".into()]);
        app.ws().new_tab("SELECT '另一个草稿';".into(), None);
        app.ws().output = Output::History;
        let finished = wait_for_query(&mut app).unwrap();
        assert_eq!(finished, source);
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            assert!(app.focus_after_query(ctx, finished).is_none());
        });
        assert!(app.ws().output == Output::History);
        assert!(app.ws().tab().report.is_none());
        assert_eq!(app.ws().tabs[0].report.as_ref().unwrap().sets.len(), 2);

        let foreground = (app.ws().key, app.ws().tab().id);
        app.ws().start_editor_batch(vec!["SELECT 3".into()]);
        assert_eq!(wait_for_query(&mut app), Some(foreground));
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            assert!(matches!(
                app.focus_after_query(ctx, foreground),
                Some(Pane::Editor)
            ));
            app.focus_active_query(ctx);
            app.editor(ctx, true);
        });
        let editor = editor_tools::editor_id(app.ws());
        assert!(ctx.memory(|memory| memory.has_focus(editor)));

        app.preferences.keep_focus_after_query = false;
        app.show_output = false;
        app.ws().start_editor_batch(vec!["SELECT 4".into()]);
        assert_eq!(wait_for_query(&mut app), Some(foreground));
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            assert!(matches!(
                app.focus_after_query(ctx, foreground),
                Some(Pane::Output)
            ));
        });
        assert!(app.show_output && app.ws().output == Output::Results);

        let mut second = Workspace::new("另一个工作区".into(), None);
        second.connect(
            ConnectionConfig {
                port,
                ..Default::default()
            },
            None,
        );
        app.workspaces.push(second);
        assert!(wait_for_query(&mut app).is_none());
        let first_key = app.workspaces[0].key;
        let second_origin = (app.workspaces[1].key, app.workspaces[1].tab().id);
        app.edit(
            &ctx,
            Edit::ArrangeWorkspaces(super::super::workspace_windows::Arrangement::Tile),
        );
        app.ws()
            .start_editor_batch(vec!["SELECT 5 + SLEEP(0.10)".into()]);
        app.edit(&ctx, Edit::MinimizeWorkspace);
        assert!(app.workspace_windows.minimized.contains(&first_key));
        assert_eq!(app.active, 1);
        app.ws().start_editor_batch(vec!["SELECT 6".into()]);
        assert_eq!(wait_for_query(&mut app), Some(second_origin));
        assert!(wait_for_query(&mut app).is_none());
        assert_eq!(
            app.ws().tab().report.as_ref().unwrap().sets[0].rows[0],
            vec![Cell::Text("6".into())],
        );
        app.edit(&ctx, Edit::SelectWorkspace(0));
        assert!(!app.workspace_windows.minimized.contains(&first_key));
        assert_eq!(
            app.workspaces[0].tab().report.as_ref().unwrap().sets[0].rows[0],
            vec![Cell::Text("5".into())],
        );
    }

    #[test]
    fn query_completion_leaves_a_foreground_dialog_in_control() {
        let ctx = egui::Context::default();
        let mut app = Workbench::new(&ctx);
        app.preferences.keep_focus_after_query = false;
        app.show_output = false;
        let origin = (app.ws().key, app.ws().tab().id);
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::Modal::new(egui::Id::new("query-focus-test-dialog")).show(ctx, |ui| {
                ui.label("连接设置");
            });
        });
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            assert!(app.focus_after_query(ctx, origin).is_none());
        });
        assert!(!app.show_output);
    }

    #[test]
    fn focus_shortcuts_target_the_selected_object_and_original_editor_caret() {
        let ctx = egui::Context::default();
        let mut app = Workbench::new(&ctx);
        let table = |name: &str| DbObject {
            schema: "仓库".into(),
            name: name.into(),
            kind: ObjectKind::Table,
        };
        let first = table("入库单");
        let second = table("出库单");
        let workspace = app.ws();
        workspace.server = Some(ServerInfo {
            version: "11.4.13-MariaDB".into(),
            connection_id: 1,
            database: Some("仓库".into()),
            http_tunnel: false,
        });
        workspace.schemas.push("仓库".into());
        workspace
            .objects
            .insert("仓库".into(), vec![first.clone(), second.clone()]);
        workspace.selected = Some(first);
        workspace.tab_mut().sql = "SELECT 1;".into();

        let editor_id = editor_tools::editor_id(workspace);
        let mut editor = TextEdit::load_state(&ctx, editor_id).unwrap_or_default();
        editor
            .cursor
            .set_char_range(Some(editor_tools::cursors(&workspace.tab().sql, 7..7)));
        editor.store(&ctx, editor_id);

        focus_frame(&ctx, Key::B, Pane::Explorer, |ctx| app.sidebar(ctx, true));
        let first_focus = ctx.memory(|memory| memory.focused()).unwrap();
        app.ws().selected = Some(second.clone());
        focus_frame(&ctx, Key::B, Pane::Explorer, |ctx| app.sidebar(ctx, true));
        let second_focus = ctx.memory(|memory| memory.focused()).unwrap();
        assert_ne!(first_focus, second_focus);
        assert_eq!(app.ws().selected.as_ref(), Some(&second));

        focus_frame(&ctx, Key::E, Pane::Editor, |ctx| {
            app.focus_active_query(ctx);
            app.editor(ctx, true);
        });
        assert!(ctx.memory(|memory| memory.has_focus(editor_id)));
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Text("7".into())],
                ..Default::default()
            },
            |ctx| app.editor(ctx, true),
        );
        assert_eq!(app.ws().tab().sql, "SELECT 71;");
        assert_eq!(editor_tools::editor_id(app.ws()), editor_id);
    }

    #[test]
    fn focus_current_output_keeps_grid_navigation_and_history_copy_available() {
        let ctx = egui::Context::default();
        let mut workspace = Workspace::new("测试工作区".into(), None);
        workspace.tab_mut().report = Some(QueryReport {
            sets: vec![ResultSet {
                source_sql: None,
                columns: vec!["记录".into()],
                column_hints: Vec::new(),
                column_origins: Vec::new(),
                excel_types: Vec::new(),
                export_columns: Vec::new(),
                duplicate_source: None,
                notice: None,
                rows: vec![vec![Cell::Text("第一行".into())]],
                affected_rows: 0,
                last_insert_id: None,
                truncated: false,
            }],
            elapsed: Duration::ZERO,
            error: None,
            continue_after_error: false,
            diagnostics: Default::default(),
            omitted_sets: 0,
            active_database: None,
        });
        let settings = EditorSettings::default();
        let mut widths = crate::app::preferences::GridWidths::new(false);
        focus_frame(&ctx, Key::R, Pane::Output, |ctx| {
            output::panel(ctx, &mut workspace, true, true, settings, true, &mut widths);
        });
        let input = press(Key::ArrowDown, Key::ArrowDown, Modifiers::NONE);
        let _ = ctx.run(input, |ctx| {
            output::panel(
                ctx,
                &mut workspace,
                true,
                true,
                settings,
                false,
                &mut widths,
            );
        });
        let copied = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Copy],
                ..Default::default()
            },
            |ctx| {
                output::panel(
                    ctx,
                    &mut workspace,
                    true,
                    true,
                    settings,
                    false,
                    &mut widths,
                )
            },
        );
        assert!(copied.platform_output.commands.iter().any(
            |command| matches!(command, egui::OutputCommand::CopyText(text) if text == "第一行"),
        ));
        assert!(workspace.output == Output::Results);

        workspace.output = Output::History;
        workspace.history.record("SELECT 1;".into());
        focus_frame(&ctx, Key::R, Pane::Output, |ctx| {
            output::panel(ctx, &mut workspace, true, true, settings, true, &mut widths);
        });
        assert!(workspace.history.has_focus(&ctx));
        assert!(workspace.output == Output::History);
        let _ = ctx.run(press(Key::Space, Key::Space, Modifiers::NONE), |ctx| {
            output::panel(
                ctx,
                &mut workspace,
                true,
                true,
                settings,
                false,
                &mut widths,
            );
        });
        let copied = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Copy],
                ..Default::default()
            },
            |ctx| {
                output::panel(
                    ctx,
                    &mut workspace,
                    true,
                    true,
                    settings,
                    false,
                    &mut widths,
                )
            },
        );
        assert!(copied.platform_output.commands.iter().any(
            |command| matches!(command, egui::OutputCommand::CopyText(text) if text == "SELECT 1;"),
        ));
    }

    #[test]
    fn navigation_switches_named_connections_and_keeps_the_pane_shortcuts() {
        let ctx = egui::Context::default();
        let mut app = Workbench::new(&ctx);
        let shifted = Modifiers::CTRL | Modifiers::SHIFT;
        for number in 1..=10 {
            let mut workspace = Workspace::new(format!("连接 {number}"), Some(Profile::default()));
            workspace.tab_mut().sql = format!("SELECT {number}");
            app.workspaces.push(workspace);
        }
        navigate(&mut app, &ctx, Key::Exclamationmark, Key::Num1, shifted);
        assert_eq!(app.active, 1);
        navigate(&mut app, &ctx, Key::Tab, Key::Tab, Modifiers::CTRL);
        assert_eq!(app.active, 2);
        navigate(&mut app, &ctx, Key::Tab, Key::Tab, shifted);
        assert_eq!(app.active, 1);
        navigate(&mut app, &ctx, Key::Num9, Key::Num9, shifted);
        assert_eq!(app.active, 10);
        assert_eq!(app.ws().tab().sql, "SELECT 10");
        navigate(&mut app, &ctx, Key::Tab, Key::Tab, Modifiers::CTRL);
        assert_eq!(app.active, 1);
        navigate(&mut app, &ctx, Key::Num1, Key::Num1, Modifiers::CTRL);
        navigate(&mut app, &ctx, Key::Num2, Key::Num2, Modifiers::CTRL);
        navigate(&mut app, &ctx, Key::Num3, Key::Num3, Modifiers::CTRL);
        assert_eq!(app.active, 1);
        assert!(!app.show_explorer && !app.show_output && !app.show_editor);
    }

    #[test]
    fn navigation_preserves_renamed_queries_and_unsaved_designs() {
        let ctx = egui::Context::default();
        let mut app = Workbench::new(&ctx);
        let workspace = app.ws();
        let path = PathBuf::from("盘点.sql");
        workspace.new_tab("SELECT 编号 FROM 库存;".into(), Some(path.clone()));
        let first_query = workspace.tab().id;
        let first_editor = editor_tools::editor_id(workspace);
        editor_tools::change(&ctx, workspace, 0..6, "select", true);
        let layout = app.design_tabs.new_design(
            app.workspaces[0].key,
            "仓库".into(),
            Some(crate::database::DbObject {
                schema: "仓库".into(),
                kind: crate::database::ObjectKind::Table,
                name: "库存".into(),
            }),
        );

        navigate(&mut app, &ctx, Key::PageUp, Key::PageUp, Modifiers::CTRL);
        assert_eq!(app.ws().tab().id, first_query);
        navigate(&mut app, &ctx, Key::F2, Key::F2, Modifiers::ALT);
        assert_eq!(app.query_rename.as_ref().unwrap().name, "盘点.sql");
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            app.query_rename_dialog(ctx)
        });
        app.query_rename.as_mut().unwrap().name = "月末盘点".into();
        let _ = ctx.run(press(Key::Enter, Key::Enter, Modifiers::NONE), |ctx| {
            app.query_rename_dialog(ctx);
        });
        assert!(app.query_rename.is_none());
        let _ = ctx.run(egui::RawInput::default(), |_| {});

        navigate(
            &mut app,
            &ctx,
            Key::PageDown,
            Key::PageDown,
            Modifiers::CTRL,
        );
        assert_eq!(
            app.design_tabs.active(app.workspaces[0].key),
            Some(Document::Design(layout))
        );
        assert!(app.design_tabs.dirty(Some(app.workspaces[0].key)));
        navigate(&mut app, &ctx, Key::PageUp, Key::PageUp, Modifiers::CTRL);
        let workspace = &app.workspaces[0];
        let query = workspace.tab();
        assert_eq!(editor_tools::editor_id(workspace), first_editor);
        assert_eq!(query.title(), "月末盘点");
        assert_eq!(query.path.as_deref(), Some(path.as_path()));
        assert_eq!(query.saved, "SELECT 编号 FROM 库存;");
        assert_eq!(query.sql, "select 编号 FROM 库存;");
        assert_eq!(query.selection, Some(0..6));

        app.preferences.confirm_tab_close = false;
        navigate(&mut app, &ctx, Key::L, Key::L, Modifiers::ALT);
        assert!(matches!(
            app.discard,
            Some(Discard::Tab {
                workspace: 0,
                tab: 1
            })
        ));
        assert_eq!(app.ws().tab().id, first_query);
    }

    #[test]
    fn tab_close_preference_controls_dirty_query_and_design_drafts() {
        let ctx = egui::Context::default();
        let close = |app: &mut Workbench| {
            navigate(app, &ctx, Key::L, Key::L, Modifiers::ALT);
            let mut released = press(Key::L, Key::L, Modifiers::ALT);
            if let egui::Event::Key { pressed, .. } = &mut released.events[0] {
                *pressed = false;
            }
            let _ = ctx.run(released, |_| {});
        };
        let mut app = Workbench::new(&ctx);
        app.preferences.confirm_tab_close = true;
        app.ws().new_tab(String::new(), None);
        app.ws().tab_mut().sql = "SELECT 1;".into();
        let query = app.ws().tab().id;
        close(&mut app);
        assert!(matches!(app.discard, Some(Discard::Tab { .. })));
        assert_eq!(app.ws().tab().id, query);

        let _ = ctx.run(egui::RawInput::default(), |ctx| app.confirm_discard(ctx));
        let _ = ctx.run(press(Key::Escape, Key::Escape, Modifiers::NONE), |ctx| {
            app.confirm_discard(ctx);
        });
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        assert!(app.discard.is_none());
        assert_eq!(app.ws().tab().sql, "SELECT 1;");

        app.preferences.confirm_tab_close = false;
        close(&mut app);
        assert!(!app.ws().tabs.iter().any(|tab| tab.id == query));
        assert!(app.discard.is_none());

        let key = app.ws().key;
        let design = app.design_tabs.new_design(
            key,
            "仓库".into(),
            Some(crate::database::DbObject {
                schema: "仓库".into(),
                kind: crate::database::ObjectKind::Table,
                name: "库存".into(),
            }),
        );
        assert!(app.design_tabs.dirty(Some(key)));
        close(&mut app);
        assert!(app.design_tabs.active(key).is_none());
        assert!(
            !app.design_tabs
                .documents(key)
                .any(|document| document == Document::Design(design))
        );
    }
}
