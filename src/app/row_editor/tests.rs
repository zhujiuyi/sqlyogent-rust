use std::{
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use mysql::{Conn, OptsBuilder, prelude::Queryable};

use crate::database::{
    ConnectionConfig, JoinedEditTarget, ObjectKind, Response, ResultSet, Worker,
};

use super::*;

fn wait(workspace: &mut Workspace, editor: Option<&mut RowEditor>) {
    let start = Instant::now();
    let mut editor = editor;
    loop {
        workspace.poll();
        if let Some(editor) = editor.as_deref_mut() {
            editor.poll(workspace);
        }
        if !workspace.busy()
            && editor
                .as_ref()
                .is_none_or(|editor| editor.pending.is_none())
        {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(12),
            "{}",
            workspace.status
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn input(events: Vec<egui::Event>) -> egui::RawInput {
    egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(900.0, 660.0),
        )),
        events,
        ..Default::default()
    }
}

fn render_row_editor(workspace: &mut Workspace, state: &mut Option<RowEditor>) -> egui::FullOutput {
    let ctx = egui::Context::default();
    let mut widths = GridWidths::new(false);
    let mut draw = || {
        ctx.run(input(vec![]), |ctx| {
            dialog(
                ctx,
                state,
                std::slice::from_mut(workspace),
                EditorSettings::default(),
                &mut widths,
            );
        })
    };
    let _ = draw();
    draw()
}

fn renders_text(output: &egui::FullOutput, expected: &str) -> bool {
    fn contains(shape: &egui::Shape, expected: &str) -> bool {
        match shape {
            egui::Shape::Text(text) => text.galley.text().contains(expected),
            egui::Shape::Vec(shapes) => shapes.iter().any(|shape| contains(shape, expected)),
            _ => false,
        }
    }

    output
        .shapes
        .iter()
        .any(|shape| contains(&shape.shape, expected))
}

#[test]
#[ignore = "requires a disposable MariaDB and PHP tunnel on loopback"]
fn table_data_ranges_page_show_all_and_stop_on_both_transports() {
    fn read(
        workspace: &mut Workspace,
        editor: &mut RowEditor,
        action: Navigate,
        offset: usize,
        ids: impl IntoIterator<Item = i32>,
        has_more: bool,
    ) {
        editor.navigate(workspace, action);
        wait(workspace, Some(editor));
        verify(editor, offset, ids, has_more);
    }

    fn verify(
        editor: &RowEditor,
        offset: usize,
        ids: impl IntoIterator<Item = i32>,
        has_more: bool,
    ) {
        assert!(editor.error.is_none(), "{:?}", editor.error);
        let page = editor.page.as_ref().unwrap();
        assert_eq!(page.offset, offset);
        assert_eq!(page.has_more, has_more);
        let actual: Vec<_> = page.rows.iter().map(|row| row.cells[0].clone()).collect();
        let expected: Vec<_> = ids
            .into_iter()
            .map(|id| Cell::Text(id.to_string()))
            .collect();
        assert_eq!(actual.len(), expected.len());
        assert_eq!(actual, expected);
    }

    let port = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let url = std::env::var("HARBOR_EDITABLE_FIXTURE_HTTP_URL").unwrap();
    assert!(url.starts_with("http://127.0.0.1:"));
    let schema = format!("harbor_table_range_{}", std::process::id());
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
    let result = catch_unwind(AssertUnwindSafe(|| {
        database.select_db(&schema).unwrap();
        database
            .query_drop("CREATE TABLE items (id INT PRIMARY KEY) ENGINE=InnoDB")
            .unwrap();
        let values = (1..=1105).map(|id| format!("({id})")).collect::<Vec<_>>();
        database
            .query_drop(format!("INSERT INTO items VALUES {}", values.join(",")))
            .unwrap();
        let tunnel = crate::database::HttpTunnelConfig {
            url,
            force_base64: true,
            use_utf8mb4: true,
            ..Default::default()
        };
        for tunnel in [None, Some(tunnel)] {
            let mut workspace = Workspace::new("表数据范围测试".into(), None);
            workspace.connect(
                ConnectionConfig {
                    port,
                    database: Some(schema.clone()),
                    ..Default::default()
                },
                tunnel,
            );
            wait(&mut workspace, None);
            assert!(workspace.server.is_some(), "{}", workspace.status);

            let mut editor = RowEditor::new(
                0,
                DbObject {
                    schema: schema.clone(),
                    name: "items".into(),
                    kind: ObjectKind::Table,
                },
            );
            editor.set_table_defaults(4, 7, false);
            wait(&mut workspace, Some(&mut editor));
            verify(&editor, 4, 5..=11, true);

            editor.begin_grid_draft(Some(2), 0, Some("7777".into()));
            editor.grid.checked.set(0, true, false);
            editor.grid.checked.set(2, true, false);
            let selection = editor.grid.selection.current();
            assert!(selection.is_some());
            for view in [DataView::Text, DataView::Grid] {
                editor.set_view(view);
                assert!(editor.view == view);
                assert_eq!(editor.grid.selection.current(), selection);
                assert_eq!(editor.grid.checked.rows, BTreeSet::from([0, 2]));
                let draft = editor.draft.as_ref().unwrap();
                assert_eq!(draft.row, Some(2));
                assert_eq!(draft.fields[0].text, "7777");
            }
            editor.draft = None;
            editor.set_view(DataView::Text);
            read(
                &mut workspace,
                &mut editor,
                Navigate::Next(11),
                11,
                12..=18,
                true,
            );
            assert!(editor.view == DataView::Text);
            let mut state = Some(editor);
            let rendered = render_row_editor(&mut workspace, &mut state);
            for text in ["复制可见行", "id", "18"] {
                assert!(renders_text(&rendered, text), "缺少表数据文本：{text}");
            }
            let table_info = format!("数据库 {schema} · 表 items");
            assert!(renders_text(&rendered, &table_info));
            workspace.show_data_info = false;
            let hidden_info = render_row_editor(&mut workspace, &mut state);
            assert!(!renders_text(&hidden_info, &table_info));
            assert!(renders_text(&hidden_info, "上一页"));
            assert!(renders_text(&hidden_info, "18"));
            workspace.show_data_info = true;
            let mut editor = state.take().unwrap();
            read(
                &mut workspace,
                &mut editor,
                Navigate::Previous,
                4,
                5..=11,
                true,
            );
            assert!(editor.view == DataView::Text);
            editor.set_view(DataView::Grid);

            editor.table_fields.offset = 2;
            editor.table_fields.rows = 13;
            read(
                &mut workspace,
                &mut editor,
                Navigate::Refresh,
                2,
                3..=15,
                true,
            );
            editor.table_fields.all = true;
            read(
                &mut workspace,
                &mut editor,
                Navigate::Refresh,
                0,
                1..=1105,
                false,
            );
            editor.table_fields.all = false;
            read(
                &mut workspace,
                &mut editor,
                Navigate::Refresh,
                2,
                3..=15,
                true,
            );

            editor.server_selection.predicates.push(TablePredicate {
                column: "id".into(),
                operator: TablePredicateOp::Greater,
                value: mysql::Value::Int(1000),
            });
            editor.server_selection.order.push(TableSort {
                column: "id".into(),
                descending: true,
            });
            read(
                &mut workspace,
                &mut editor,
                Navigate::First,
                0,
                (1093..=1105).rev(),
                true,
            );
            read(
                &mut workspace,
                &mut editor,
                Navigate::Next(13),
                13,
                (1080..=1092).rev(),
                true,
            );

            database.query_drop("LOCK TABLES items WRITE").unwrap();
            editor.table_fields.all = true;
            editor.navigate(&mut workspace, Navigate::Refresh);
            let stopped = workspace.cancel_table_read();
            database.query_drop("UNLOCK TABLES").unwrap();
            assert!(stopped);
            wait(&mut workspace, Some(&mut editor));
            verify(&editor, 13, (1080..=1092).rev(), true);
            assert!(!editor.table_fields.all);
        }
    }));
    database
        .query_drop(format!("DROP DATABASE {schema}"))
        .unwrap();
    if let Err(error) = result {
        resume_unwind(error);
    }
}

#[test]
#[ignore = "requires a disposable MariaDB and PHP tunnel on loopback"]
fn docked_table_data_replies_stay_with_the_originating_query_tab() {
    use super::super::state::Output;

    fn verify_page(workspace: &Workspace, tab: usize, table: &str, value: &str) {
        let editor = workspace.tabs[tab].data_editor.as_ref().unwrap();
        assert!(editor.error.is_none(), "{:?}", editor.error);
        assert!(!editor.has_pending());
        let page = editor.page.as_ref().unwrap();
        assert_eq!(page.object.name, table);
        assert_eq!(page.rows[0].cells[1], Cell::Text(value.into()));
    }

    let port = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let url = std::env::var("HARBOR_EDITABLE_FIXTURE_HTTP_URL").unwrap();
    assert!(url.starts_with("http://127.0.0.1:"));
    let schema = format!("harbor_docked_data_{}", std::process::id());
    let options = OptsBuilder::new()
        .ip_or_hostname(Some("127.0.0.1"))
        .tcp_port(port)
        .user(Some("root"))
        .prefer_socket(false);
    let mut database = Conn::new(options.clone()).unwrap();
    let mut observer = Conn::new(options).unwrap();
    database
        .query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
        .unwrap();
    let result = catch_unwind(AssertUnwindSafe(|| {
        database.select_db(&schema).unwrap();
        for table in ["first_items", "second_items"] {
            database
                .query_drop(format!(
                    "CREATE TABLE {table} (id INT PRIMARY KEY, label VARCHAR(30)) ENGINE=InnoDB"
                ))
                .unwrap();
        }
        let tunnel = crate::database::HttpTunnelConfig {
            url,
            force_base64: true,
            use_utf8mb4: true,
            ..Default::default()
        };
        for tunnel in [None, Some(tunnel)] {
            database
                .query_drop("REPLACE INTO first_items VALUES (1, '表一')")
                .unwrap();
            database
                .query_drop("REPLACE INTO second_items VALUES (1, '表二')")
                .unwrap();
            let mut workspace = Workspace::new("查询页数据隔离测试".into(), None);
            workspace.connect(
                ConnectionConfig {
                    port,
                    database: Some(schema.clone()),
                    ..Default::default()
                },
                tunnel,
            );
            wait(&mut workspace, None);
            assert!(workspace.server.is_some(), "{}", workspace.status);

            for (index, label) in ["查询一", "查询二"].into_iter().enumerate() {
                if index > 0 {
                    workspace.new_tab(String::new(), None);
                }
                assert!(workspace.send_editor_query(
                    Command::Query {
                        sql: format!("SELECT '{label}' AS source"),
                        limit: 1,
                    },
                    "正在执行查询…",
                ));
                wait(&mut workspace, None);
            }
            let table = |name: &str| {
                RowEditor::new(
                    0,
                    DbObject {
                        schema: schema.clone(),
                        name: name.into(),
                        kind: ObjectKind::Table,
                    },
                )
            };

            workspace.select_tab(0);
            workspace.tab_mut().data_editor = Some(table("first_items"));
            workspace.output = Output::Data;
            database
                .query_drop("LOCK TABLES first_items WRITE")
                .unwrap();
            workspace
                .with_data_editor(|workspace, editor| editor.as_mut().unwrap().poll(workspace));
            assert!(workspace.busy());
            let started = Instant::now();
            loop {
                let reader: Option<u32> = observer
                    .exec_first(
                        "SELECT ID FROM information_schema.PROCESSLIST
                         WHERE STATE = 'Waiting for table metadata lock'
                           AND (DB = ? OR LOCATE(?, INFO) > 0)",
                        (schema.as_str(), schema.as_str()),
                    )
                    .unwrap();
                if reader.is_some() {
                    break;
                }
                assert!(
                    started.elapsed() < Duration::from_secs(5),
                    "未观察到表数据读取等待"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            workspace.select_tab(1);
            assert!(workspace.tab().data_editor.is_none());
            assert!(workspace.output == Output::Results);
            assert!(workspace.data_tab_protected(0));
            database.query_drop("UNLOCK TABLES").unwrap();
            wait(&mut workspace, None);
            assert!(workspace.tab().data_editor.is_none());
            assert!(workspace.row_page.is_none());
            verify_page(&workspace, 0, "first_items", "表一");

            workspace.tab_mut().data_editor = Some(table("second_items"));
            workspace.output = Output::Data;
            workspace
                .with_data_editor(|workspace, editor| editor.as_mut().unwrap().poll(workspace));
            wait(&mut workspace, None);
            verify_page(&workspace, 1, "second_items", "表二");
            verify_page(&workspace, 0, "first_items", "表一");

            workspace.select_tab(0);
            assert!(workspace.output == Output::Data);
            workspace.with_data_editor(|_, editor| {
                editor
                    .as_mut()
                    .unwrap()
                    .begin_grid_draft(Some(0), 1, Some("未保存".into()));
            });
            database
                .query_drop("UPDATE first_items SET label = '已经更新' WHERE id = 1")
                .unwrap();
            workspace.select_tab(1);
            workspace.select_tab(0);
            assert!(!workspace.with_data_editor(|workspace, editor| {
                editor.as_mut().unwrap().refresh_on_focus(workspace)
            }));
            assert!(!workspace.busy());
            verify_page(&workspace, 0, "first_items", "表一");
            workspace.with_data_editor(|_, editor| {
                let editor = editor.as_mut().unwrap();
                assert_eq!(editor.draft.as_ref().unwrap().fields[1].text, "未保存");
                assert_eq!(editor.grid.form_point(), Point { row: 0, column: 1 });
                editor.draft = None;
            });
            assert!(workspace.with_data_editor(|workspace, editor| {
                editor.as_mut().unwrap().refresh_on_focus(workspace)
            }));
            wait(&mut workspace, None);
            verify_page(&workspace, 0, "first_items", "已经更新");
            verify_page(&workspace, 1, "second_items", "表二");
            assert!(workspace.row_page.is_none());

            workspace.with_data_editor(|workspace, editor| {
                let editor = editor.as_mut().unwrap();
                editor.begin_grid_draft(Some(0), 1, Some("在另一页等待保存".into()));
                let edit = editor
                    .draft
                    .as_ref()
                    .unwrap()
                    .edit(editor.page.as_ref().unwrap());
                editor.write(workspace, edit, None);
            });
            assert!(workspace.busy());
            workspace.select_tab(1);
            wait(&mut workspace, None);
            verify_page(&workspace, 0, "first_items", "在另一页等待保存");
            verify_page(&workspace, 1, "second_items", "表二");
            assert!(workspace.row_page.is_none());

            for (index, label) in ["查询一", "查询二"].into_iter().enumerate() {
                let report = workspace.tabs[index].report.as_ref().unwrap();
                assert!(report.error.is_none(), "{:?}", report.error);
                assert_eq!(report.sets[0].rows[0], [Cell::Text(label.into())]);
            }
        }
    }));
    database.query_drop("UNLOCK TABLES").unwrap();
    database
        .query_drop(format!("DROP DATABASE {schema}"))
        .unwrap();
    if let Err(error) = result {
        resume_unwind(error);
    }
}

#[test]
#[ignore = "requires a disposable MariaDB and PHP tunnel on loopback"]
fn docked_f8_keeps_its_query_tab_through_read_and_write() {
    use super::super::state::Output;
    use crate::app::output::HistoryTiming;
    use crate::database::QueryTiming;

    fn verify_joined(workspace: &Workspace, label: &str) -> QueryTiming {
        let editor = workspace.tabs[0]
            .editable_editor
            .as_ref()
            .expect("原查询页应保留可编辑结果");
        assert!(editor.error.is_none(), "{:?}", editor.error);
        assert!(!editor.has_pending());
        let joined = editor.joined.as_ref().unwrap();
        assert_eq!(joined.page().unwrap().object.name, "items");
        let response = joined.response.as_ref().unwrap();
        let result = &response.result;
        assert_eq!(result.columns, ["item_id", "label", "note_id", "note"]);
        assert_eq!(result.rows.len(), 2);
        assert_eq!(
            result.rows[0],
            [
                Cell::Text("1".into()),
                Cell::Text(label.into()),
                Cell::Text("11".into()),
                Cell::Text("明细".into()),
            ]
        );
        assert_eq!(result.rows[1][1], Cell::Text("保留行".into()));
        assert_eq!(result.rows[1][3], Cell::Null);
        response.query_timing.unwrap()
    }

    fn verify_other_query(workspace: &Workspace, label: &str) {
        assert_eq!(workspace.active_tab, 1);
        assert!(workspace.tab().editable_editor.is_none());
        let report = workspace.tab().report.as_ref().unwrap();
        assert!(report.error.is_none(), "{:?}", report.error);
        assert_eq!(report.sets[0].rows[0], [Cell::Text(label.into())]);
    }

    fn wait_for_metadata_lock(observer: &mut Conn, schema: &str) {
        let started = Instant::now();
        loop {
            let reader = observer
                .exec_first::<u32, _, _>(
                    "SELECT ID FROM information_schema.PROCESSLIST
                 WHERE STATE = 'Waiting for table metadata lock'
                   AND (DB = ? OR LOCATE(?, INFO) > 0)",
                    (schema, schema),
                )
                .unwrap();
            if reader.is_some() {
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "未观察到 F8 读取等待"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    let port = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let url = std::env::var("HARBOR_EDITABLE_FIXTURE_HTTP_URL").unwrap();
    assert!(url.starts_with("http://127.0.0.1:"));
    let schema = format!("harbor_docked_f8_{}", std::process::id());
    let options = OptsBuilder::new()
        .ip_or_hostname(Some("127.0.0.1"))
        .tcp_port(port)
        .user(Some("root"))
        .prefer_socket(false);
    let mut database = Conn::new(options.clone()).unwrap();
    let mut observer = Conn::new(options).unwrap();
    database
        .query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
        .unwrap();
    let result = catch_unwind(AssertUnwindSafe(|| {
        database.select_db(&schema).unwrap();
        database
            .query_drop("CREATE TABLE items (id INT PRIMARY KEY, label VARCHAR(30)) ENGINE=InnoDB")
            .unwrap();
        database
            .query_drop(
                "CREATE TABLE notes (id INT PRIMARY KEY, item INT, note VARCHAR(30)) ENGINE=InnoDB",
            )
            .unwrap();
        database
            .query_drop("INSERT INTO notes VALUES (11, 1, '明细')")
            .unwrap();
        let sql = format!(
            "SELECT i.id AS item_id, i.label, n.id AS note_id, n.note \
             FROM `{schema}`.`items` AS i LEFT JOIN `{schema}`.`notes` AS n ON n.item = i.id \
             ORDER BY i.id"
        );
        let tunnel = crate::database::HttpTunnelConfig {
            url,
            force_base64: true,
            use_utf8mb4: true,
            ..Default::default()
        };
        for tunnel in [None, Some(tunnel)] {
            let native = tunnel.is_none();
            database
                .query_drop("REPLACE INTO items VALUES (1, '原始行'), (2, '保留行')")
                .unwrap();
            let mut workspace = Workspace::new("查询页 F8 测试".into(), None);
            workspace.connect(
                ConnectionConfig {
                    port,
                    database: Some(schema.clone()),
                    ..Default::default()
                },
                tunnel,
            );
            wait(&mut workspace, None);
            assert!(workspace.server.is_some(), "{}", workspace.status);
            workspace.new_tab(String::new(), None);
            workspace.start_editor_batch(vec!["SELECT '查询二' AS source".into()]);
            wait(&mut workspace, None);
            verify_other_query(&workspace, "查询二");
            workspace.output = Output::History;

            workspace.select_tab(0);
            database.query_drop("LOCK TABLES items WRITE").unwrap();
            assert!(
                workspace.start_f8(0, sql.clone(), 10),
                "{}",
                workspace.status
            );
            assert!(workspace.busy());
            wait_for_metadata_lock(&mut observer, &schema);
            workspace.select_tab(1);
            database.query_drop("UNLOCK TABLES").unwrap();
            wait(&mut workspace, None);
            let timing = verify_joined(&workspace, "原始行");
            assert_eq!(timing.execution.is_some(), native);
            let expected_timing = if let Some(execution) = timing.execution {
                assert!(execution <= timing.elapsed);
                HistoryTiming::Execution(execution)
            } else {
                HistoryTiming::Total(timing.elapsed)
            };
            assert_eq!(
                workspace.history.recorded_timings().last(),
                Some(Some(expected_timing))
            );
            let messages = workspace.tabs[0].batch_messages.as_ref().unwrap();
            let statement = &messages.statements[0];
            assert_eq!(statement.sql, sql);
            assert_eq!(statement.elapsed, Some(timing.elapsed));
            assert_eq!(statement.diagnostics.execution, timing.execution);
            assert_eq!(statement.editable_result.as_ref().unwrap().rows, 2);
            verify_other_query(&workspace, "查询二");
            assert!(workspace.output == Output::History);

            workspace.select_tab(0);
            assert!(workspace.output == Output::Results);
            workspace.with_editable_editor(|_, editor| {
                let editor = editor.as_mut().unwrap();
                let target = editor.joined.as_ref().unwrap().page().unwrap();
                let column = target
                    .columns
                    .iter()
                    .position(|column| column.name == "label")
                    .unwrap();
                editor.begin_grid_draft(Some(0), column, Some("已保存".into()));
                assert!(editor.draft.is_some());
            });
            workspace.start_editor_batch(vec!["SELECT '应保留草稿' AS source".into()]);
            assert!(!workspace.busy());
            assert!(
                workspace
                    .tab()
                    .editable_editor
                    .as_ref()
                    .unwrap()
                    .draft
                    .is_some()
            );

            workspace.select_tab(1);
            workspace.start_editor_batch(vec!["SELECT '另一页仍可查询' AS source".into()]);
            assert!(workspace.busy(), "{}", workspace.status);
            wait(&mut workspace, None);
            verify_other_query(&workspace, "另一页仍可查询");

            workspace.select_tab(0);
            workspace.with_editable_editor(|workspace, editor| {
                let editor = editor.as_mut().unwrap();
                let page = editor.joined.as_ref().unwrap().page().unwrap();
                let change = editor.draft.as_ref().unwrap().edit(page);
                editor.write(workspace, change, None);
            });
            assert!(workspace.busy(), "{}", workspace.status);
            workspace.select_tab(1);
            wait(&mut workspace, None);
            let refreshed = verify_joined(&workspace, "已保存");
            assert_eq!(refreshed.execution.is_some(), native);
            verify_other_query(&workspace, "另一页仍可查询");
            assert!(
                workspace.tabs[0]
                    .editable_editor
                    .as_ref()
                    .unwrap()
                    .draft
                    .is_none()
            );
            assert_eq!(
                database
                    .query_first::<String, _>("SELECT label FROM items WHERE id = 1")
                    .unwrap()
                    .as_deref(),
                Some("已保存")
            );

            workspace.select_tab(0);
            database.query_drop("LOCK TABLES items WRITE").unwrap();
            assert!(workspace.start_f8(0, sql.clone(), 10));
            wait_for_metadata_lock(&mut observer, &schema);
            assert!(workspace.cancel_editable_read());
            database.query_drop("UNLOCK TABLES").unwrap();
            wait(&mut workspace, None);
            assert_eq!(verify_joined(&workspace, "已保存"), refreshed);

            assert!(workspace.start_f8(0, "SELECT '只读结果' AS source".into(), 10));
            wait(&mut workspace, None);
            assert!(workspace.output == Output::Results);
            assert!(workspace.tab().editable_editor.is_none());
            let report = workspace.tab().report.as_ref().unwrap();
            assert!(report.error.is_none(), "{:?}", report.error);
            assert_eq!(report.sets[0].rows[0], [Cell::Text("只读结果".into())]);
        }
    }));
    database.query_drop("UNLOCK TABLES").unwrap();
    database
        .query_drop(format!("DROP DATABASE {schema}"))
        .unwrap();
    if let Err(error) = result {
        resume_unwind(error);
    }
}

#[test]
#[ignore = "requires a disposable MariaDB and PHP tunnel on loopback"]
fn loaded_table_and_f8_exports_use_their_current_rows_on_both_transports() {
    use crate::database::transfer::{
        CsvOptions, ExcelOptions, ExportKind, ExportSettings, ResultExport, ResultSqlTarget,
        SqlExportMode,
    };

    fn assert_csv(
        data: ResultExport<'_>,
        path: &std::path::Path,
        indices: &[usize],
        expected: &str,
        row_count: u64,
    ) {
        let summary = data
            .export(path, indices, &ExportSettings::default())
            .unwrap();
        assert_eq!(summary.rows, row_count);
        assert_eq!(std::fs::read_to_string(path).unwrap(), expected);
    }

    let port = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let url = std::env::var("HARBOR_EDITABLE_FIXTURE_HTTP_URL").unwrap();
    assert!(url.starts_with("http://127.0.0.1:"));
    let schema = format!("harbor_loaded_export_{}", std::process::id());
    let mut database = Conn::new(
        OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(port)
            .user(Some("root"))
            .prefer_socket(false),
    )
    .unwrap();
    let output = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    database
        .query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
        .unwrap();
    let result = catch_unwind(AssertUnwindSafe(|| {
        database.select_db(&schema).unwrap();
        database.query_drop("CREATE TABLE items (id INT PRIMARY KEY, label VARCHAR(12), private_note VARCHAR(30), amount DECIMAL(8,3)) ENGINE=InnoDB").unwrap();
        database.query_drop("INSERT INTO items VALUES (1, '甲', '隐藏甲', 1.234), (2, '乙', '隐藏乙', 2.345), (3, '丙', '隐藏丙', 3.456)").unwrap();
        database
            .query_drop(
                "CREATE TABLE notes (id INT PRIMARY KEY, item INT, note VARCHAR(20)) ENGINE=InnoDB",
            )
            .unwrap();
        database
            .query_drop("INSERT INTO notes VALUES (11, 2, '明细')")
            .unwrap();

        let tunnel = crate::database::HttpTunnelConfig {
            url,
            force_base64: true,
            use_utf8mb4: true,
            ..Default::default()
        };
        for tunnel in [None, Some(tunnel)] {
            let mut workspace = Workspace::new("表数据和查询结果导出测试".into(), None);
            workspace.connect(
                ConnectionConfig {
                    port,
                    database: Some(schema.clone()),
                    ..Default::default()
                },
                tunnel,
            );
            wait(&mut workspace, None);
            assert!(workspace.server.is_some(), "{}", workspace.status);

            let mut editor = RowEditor::new(
                0,
                DbObject {
                    schema: schema.clone(),
                    name: "items".into(),
                    kind: ObjectKind::Table,
                },
            );
            editor.set_table_defaults(1, 1, false);
            workspace.tab_mut().data_editor = Some(editor);
            workspace
                .with_data_editor(|workspace, editor| editor.as_mut().unwrap().poll(workspace));
            wait(&mut workspace, None);
            let editor = workspace.tab().data_editor.as_ref().unwrap();
            let page = editor.page.as_ref().expect("表数据应已加载");
            assert_eq!(page.offset, 1);
            assert_eq!(page.columns[1].declared_width, Some(48));
            let data = editor.export_input().unwrap();
            assert!(data.truncated() && data.has_declared_widths());
            assert_csv(
                data,
                &output.path().join("page.csv"),
                &[1],
                "label\r\n乙\r\n",
                1,
            );

            let fixed = ExportSettings {
                csv: CsvOptions {
                    fixed_width: true,
                    declared_widths: true,
                    headers: false,
                    ..Default::default()
                },
                ..Default::default()
            };
            let fixed_path = output.path().join("page-fixed.csv");
            assert_eq!(data.export(&fixed_path, &[1], &fixed).unwrap().rows, 1);
            assert_eq!(
                std::fs::read_to_string(fixed_path).unwrap(),
                format!("乙{}\r\n", " ".repeat(45))
            );

            for (kind, filename, expected) in [
                (ExportKind::Html, "page.html", "<td>2</td>"),
                (ExportKind::Xml, "page.xml", "<row><cell>2</cell></row>"),
                (
                    ExportKind::ExcelXml,
                    "page-excel.xml",
                    "ss:Type=\"Number\">2.34</Data>",
                ),
                (
                    ExportKind::Sql,
                    "page.sql",
                    "INSERT INTO `snapshot` (`id`) VALUES (2);",
                ),
            ] {
                let settings = ExportSettings {
                    kind,
                    excel: ExcelOptions {
                        decimal_places: Some(2),
                        ..ExcelOptions::default()
                    },
                    sql: SqlExportMode::StructureAndData,
                    result_target: Some(ResultSqlTarget {
                        schema: String::new(),
                        table: "snapshot".into(),
                    }),
                    ..ExportSettings::default()
                };
                let path = output.path().join(filename);
                let column = if kind == ExportKind::ExcelXml { 3 } else { 0 };
                assert_eq!(data.export(&path, &[column], &settings).unwrap().rows, 1);
                assert!(
                    std::fs::read_to_string(path).unwrap().contains(expected),
                    "{filename}"
                );
            }

            let sql = "SELECT id AS item_id, label AS display_label FROM items WHERE id = 2";
            assert!(
                workspace.start_f8(0, sql.into(), 10),
                "{}",
                workspace.status
            );
            wait(&mut workspace, None);
            let editor = workspace
                .tab()
                .editable_editor
                .as_ref()
                .expect("F8 应返回可编辑结果");
            let page = editor.page.as_ref().unwrap();
            assert_eq!(page.columns[1].name, "label");
            assert!(
                page.rows[0]
                    .source_columns()
                    .iter()
                    .any(|column| column.name == "private_note")
            );
            let data = editor.export_input().unwrap();
            assert_eq!(data.column_count(), 2);
            assert_eq!(data.column_label(1), "display_label");
            assert_csv(
                data,
                &output.path().join("f8.csv"),
                &[1, 0],
                "display_label,item_id\r\n乙,2\r\n",
                1,
            );

            let sql = "SELECT i.id AS item_id, i.label AS item_label, n.id AS note_id, n.note AS joined_note \
                       FROM items AS i LEFT JOIN notes AS n ON n.item = i.id WHERE i.id >= 2 ORDER BY i.id";
            assert!(
                workspace.start_f8(0, sql.into(), 10),
                "{}",
                workspace.status
            );
            wait(&mut workspace, None);
            let editor = workspace
                .tab()
                .editable_editor
                .as_ref()
                .expect("F8 应返回可编辑联表结果");
            assert_eq!(
                editor.joined.as_ref().unwrap().page().unwrap().object.name,
                "items"
            );
            let data = editor.export_input().unwrap();
            assert_eq!(data.column_count(), 4);
            assert_csv(
                data,
                &output.path().join("join.csv"),
                &[3, 1],
                "joined_note,item_label\r\n明细,乙\r\nNULL,丙\r\n",
                2,
            );
        }
    }));
    database
        .query_drop(format!("DROP DATABASE {schema}"))
        .unwrap();
    if let Err(error) = result {
        resume_unwind(error);
    }
}

#[test]
#[ignore = "requires a disposable MariaDB on loopback"]
fn stopping_an_active_table_read_preserves_the_page_and_main_session() {
    let port = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let schema = format!("harbor_stop_read_{}", std::process::id());
    let user = format!("harbor_read_{}", std::process::id());
    let qualified = format!("`{schema}`.`items`");
    let options = OptsBuilder::new()
        .ip_or_hostname(Some("127.0.0.1"))
        .tcp_port(port)
        .user(Some("root"))
        .prefer_socket(false);
    let mut database = Conn::new(options.clone()).unwrap();
    let mut observer = Conn::new(options).unwrap();
    database
        .query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
        .unwrap();
    let result = catch_unwind(AssertUnwindSafe(|| {
        database.select_db(&schema).unwrap();
        database
            .query_drop(
                "CREATE TABLE items (id INT PRIMARY KEY, label VARCHAR(30) NOT NULL) ENGINE=InnoDB",
            )
            .unwrap();
        database
            .query_drop("INSERT INTO items VALUES (1,'甲'),(2,'乙')")
            .unwrap();
        database
            .query_drop(format!("CREATE USER '{user}'@'127.0.0.1'"))
            .unwrap();
        database
            .query_drop(format!(
                "GRANT SELECT, UPDATE ON {qualified} TO '{user}'@'127.0.0.1'"
            ))
            .unwrap();

        let mut workspace = Workspace::new("停止表数据读取测试".into(), None);
        let mut config = ConnectionConfig {
            port,
            user: user.clone(),
            database: Some(schema.clone()),
            ..Default::default()
        };
        config.advanced.init_sql = "SET @harbor_stop_read_marker = 'keep-session'".into();
        workspace.connect(config, None);
        wait(&mut workspace, None);
        let main_id = workspace
            .server
            .as_ref()
            .unwrap_or_else(|| panic!("{}", workspace.status))
            .connection_id;
        let mut editor = RowEditor::new(
            0,
            DbObject {
                schema: schema.clone(),
                name: "items".into(),
                kind: ObjectKind::Table,
            },
        );
        editor.set_table_defaults(0, 1, false);
        wait(&mut workspace, Some(&mut editor));
        let first = vec![Cell::Text("1".into()), Cell::Text("甲".into())];
        assert_eq!(editor.page.as_ref().unwrap().rows[0].cells, first);

        database.query_drop("LOCK TABLES items WRITE").unwrap();
        editor.table_fields.all = true;
        editor.navigate(&mut workspace, Navigate::Refresh);

        // The reader requests a metadata lock before it reads the rows. Keep the lock
        // held so a between-batches-only stop cannot make this test pass.
        let started = Instant::now();
        let read_sql = format!("%FROM {qualified} LIMIT 0%");
        let reader_id = loop {
            let reader: Option<u32> = observer
                .exec_first(
                    "SELECT ID FROM information_schema.PROCESSLIST
                     WHERE USER = ? AND STATE = 'Waiting for table metadata lock' AND INFO LIKE ?",
                    (&user, &read_sql),
                )
                .unwrap();
            if let Some(id) = reader {
                break id;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "表数据查询没有在服务器上进入等待表元数据锁的状态"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_ne!(reader_id, main_id);
        assert!(workspace.cancel_table_read());
        wait(&mut workspace, Some(&mut editor));
        assert!(editor.error.is_none(), "{:?}", editor.error);
        assert_eq!(editor.notice.as_deref(), Some(TABLE_READ_CANCELLED));
        assert_eq!(editor.page.as_ref().unwrap().rows[0].cells, first);
        assert!(!editor.table_fields.all);

        let started = Instant::now();
        loop {
            let reader: Option<u32> = observer
                .exec_first(
                    "SELECT ID FROM information_schema.PROCESSLIST WHERE ID = ?",
                    (reader_id,),
                )
                .unwrap();
            if reader.is_none() {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "停止后独立的表数据连接仍然在服务器上"
            );
            std::thread::sleep(Duration::from_millis(5));
        }

        assert!(workspace.send_editor_query(
            Command::Query {
                sql: "SELECT CONNECTION_ID(), @harbor_stop_read_marker".into(),
                limit: 1,
            },
            "核对当前 SQL 会话…",
        ));
        wait(&mut workspace, None);
        let report = workspace.tab().report.as_ref().unwrap();
        assert!(report.error.is_none(), "{:?}", report.error);
        assert_eq!(
            report.sets[0].rows[0],
            [
                Cell::Text(main_id.to_string()),
                Cell::Text("keep-session".into())
            ]
        );

        database.query_drop("UNLOCK TABLES").unwrap();
        editor.navigate(&mut workspace, Navigate::Next(1));
        wait(&mut workspace, Some(&mut editor));
        let page = editor.page.as_ref().unwrap();
        assert_eq!(page.offset, 1);
        assert_eq!(page.rows[0].cells[0], Cell::Text("2".into()));
        editor.begin_grid_draft(Some(0), 1, Some("修改后".into()));
        let edit = editor
            .draft
            .as_ref()
            .unwrap()
            .edit(editor.page.as_ref().unwrap());
        editor.write(&mut workspace, edit, None);
        wait(&mut workspace, Some(&mut editor));
        assert!(editor.error.is_none(), "{:?}", editor.error);
        assert_eq!(
            database
                .query_first::<String, _>("SELECT label FROM items WHERE id = 2")
                .unwrap(),
            Some("修改后".into())
        );
    }));
    database.query_drop("UNLOCK TABLES").unwrap();
    database
        .query_drop(format!("DROP USER IF EXISTS '{user}'@'127.0.0.1'"))
        .unwrap();
    database
        .query_drop(format!("DROP DATABASE {schema}"))
        .unwrap();
    if let Err(error) = result {
        resume_unwind(error);
    }
}

#[test]
#[ignore = "requires a disposable MariaDB and PHP tunnel on loopback"]
fn stops_loading_and_refreshing_single_and_joined_editable_results() {
    fn wait_for_blocked_reader(observer: &mut Conn, schema: &str, main_id: u32) -> u32 {
        let started = Instant::now();
        loop {
            let reader = observer
                .exec_first::<u32, _, _>(
                    "SELECT ID FROM information_schema.PROCESSLIST
                     WHERE ID <> ? AND STATE = 'Waiting for table metadata lock'
                       AND (DB = ? OR LOCATE(?, INFO) > 0)",
                    (main_id, schema, schema),
                )
                .unwrap();
            if let Some(reader) = reader {
                return reader;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "未观察到 F8 读取在等待元数据锁"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn finish_stop(
        database: &mut Conn,
        observer: &mut Conn,
        workspace: &mut Workspace,
        editor: Option<&mut RowEditor>,
        reader: u32,
        http: bool,
    ) {
        assert!(workspace.editable_read_stopping());
        if http {
            workspace.poll();
            assert!(workspace.reading_editable_result());
            assert_eq!(reader_by_id(observer, reader), Some(reader));
            database.query_drop("UNLOCK TABLES").unwrap();
            wait(workspace, editor);
        } else {
            wait(workspace, editor);
            let started = Instant::now();
            while reader_by_id(observer, reader).is_some() {
                assert!(
                    started.elapsed() < Duration::from_secs(5),
                    "停止后 F8 独立读取连接仍在服务器上"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            database.query_drop("UNLOCK TABLES").unwrap();
        }
        assert!(workspace.editable_page.is_none());
    }

    fn reader_by_id(observer: &mut Conn, id: u32) -> Option<u32> {
        observer
            .exec_first(
                "SELECT ID FROM information_schema.PROCESSLIST WHERE ID = ?",
                (id,),
            )
            .unwrap()
    }

    fn assert_label(editor: &RowEditor, label: &str) {
        let page = editor
            .joined
            .as_ref()
            .map_or(editor.page.as_ref(), joined::Joined::page)
            .unwrap();
        assert_eq!(page.rows[0].cells[1], Cell::Text(label.into()));
    }

    let port = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let url = std::env::var("HARBOR_EDITABLE_FIXTURE_HTTP_URL").unwrap();
    assert!(url.starts_with("http://127.0.0.1:"));
    let schema = format!("harbor_stop_f8_{}", std::process::id());
    let options = OptsBuilder::new()
        .ip_or_hostname(Some("127.0.0.1"))
        .tcp_port(port)
        .user(Some("root"))
        .prefer_socket(false);
    let mut database = Conn::new(options.clone()).unwrap();
    let mut observer = Conn::new(options).unwrap();
    database
        .query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
        .unwrap();
    let result = catch_unwind(AssertUnwindSafe(|| {
        database.select_db(&schema).unwrap();
        database
            .query_drop("CREATE TABLE items (id INT PRIMARY KEY, label VARCHAR(30)) ENGINE=InnoDB")
            .unwrap();
        database
            .query_drop("CREATE TABLE details (id INT PRIMARY KEY, note VARCHAR(30)) ENGINE=InnoDB")
            .unwrap();
        database
            .query_drop("INSERT INTO items VALUES (1,'原页')")
            .unwrap();
        database
            .query_drop("INSERT INTO details VALUES (1,'明细')")
            .unwrap();
        let tunnel = crate::database::HttpTunnelConfig {
            url,
            force_base64: true,
            use_utf8mb4: true,
            ..Default::default()
        };
        for tunnel in [None, Some(tunnel)] {
            let http = tunnel.is_some();
            let mut workspace = Workspace::new("停止 F8 读取测试".into(), None);
            workspace.connect(
                ConnectionConfig {
                    port,
                    database: Some(schema.clone()),
                    ..Default::default()
                },
                tunnel,
            );
            wait(&mut workspace, None);
            let main_id = workspace.server.as_ref().unwrap().connection_id;
            let queries = [
                format!("SELECT i.id, i.label FROM `{schema}`.`items` AS i"),
                format!(
                    "SELECT d.id AS detail_id, d.note, i.id AS item_id, i.label FROM `{schema}`.`items` AS i JOIN `{schema}`.`details` AS d ON d.id = i.id"
                ),
            ];
            for sql in queries {
                let open = |workspace: &mut Workspace| {
                    assert!(workspace.send_editor_query(
                        Command::QueryF8 {
                            sql: sql.clone(),
                            limit: 10
                        },
                        "正在读取可编辑查询结果…"
                    ));
                };
                database
                    .query_drop("UPDATE items SET label='原页' WHERE id=1")
                    .unwrap();
                database.query_drop("LOCK TABLES items WRITE").unwrap();
                open(&mut workspace);
                let reader = wait_for_blocked_reader(&mut observer, &schema, main_id);
                assert!(workspace.cancellable());
                workspace.stop_batch();
                finish_stop(
                    &mut database,
                    &mut observer,
                    &mut workspace,
                    None,
                    reader,
                    http,
                );
                assert_eq!(
                    workspace.take_editable_read_error().as_deref(),
                    Some(EDITABLE_READ_CANCELLED)
                );

                open(&mut workspace);
                wait(&mut workspace, None);
                let mut editor = match workspace
                    .editable_page
                    .take()
                    .unwrap_or_else(|| panic!("{}", workspace.status))
                {
                    EditableResult::Table(page) => RowEditor::from_query(0, page, sql.clone(), 10),
                    EditableResult::Joined(response) => {
                        assert_eq!(response.targets.first().unwrap().alias, "d");
                        let target = joined::Target::new(
                            response
                                .targets
                                .iter()
                                .find(|target| target.alias == "i")
                                .unwrap(),
                        );
                        let mut editor = RowEditor::from_joined(
                            0,
                            response,
                            PendingEditableQuery {
                                workspace: workspace.key,
                                tab: workspace.tab().id,
                                sql: sql.clone(),
                                limit: 10,
                            },
                        );
                        assert_eq!(editor.object.as_ref().unwrap().name, "details");
                        editor.select_joined_target(Some(target));
                        editor
                    }
                };
                let selected_target = editor
                    .joined
                    .as_ref()
                    .and_then(|data| data.selected.clone());
                let point = Point { row: 0, column: 1 };
                editor.grid.selection.select(
                    Hit {
                        point,
                        row_header: false,
                    },
                    false,
                );
                assert_label(&editor, "原页");

                database.query_drop("LOCK TABLES items WRITE").unwrap();
                database
                    .query_drop("UPDATE items SET label='更新后' WHERE id=1")
                    .unwrap();
                editor.navigate(&mut workspace, Navigate::Refresh);
                let reader = wait_for_blocked_reader(&mut observer, &schema, main_id);
                assert!(workspace.cancel_editable_read());
                finish_stop(
                    &mut database,
                    &mut observer,
                    &mut workspace,
                    Some(&mut editor),
                    reader,
                    http,
                );
                assert!(editor.error.is_none(), "{:?}", editor.error);
                assert!(
                    editor
                        .notice
                        .as_deref()
                        .is_some_and(|notice| notice.contains("已停止读取可编辑查询结果"))
                );
                assert_label(&editor, "原页");
                assert_eq!(editor.grid.selection.current().unwrap().head, point);
                assert!(
                    editor
                        .joined
                        .as_ref()
                        .and_then(|data| data.selected.as_ref())
                        == selected_target.as_ref()
                );

                editor.navigate(&mut workspace, Navigate::Refresh);
                wait(&mut workspace, Some(&mut editor));
                assert!(editor.error.is_none(), "{:?}", editor.error);
                assert_label(&editor, "更新后");
            }
        }
    }));
    database.query_drop("UNLOCK TABLES").unwrap();
    database
        .query_drop(format!("DROP DATABASE {schema}"))
        .unwrap();
    if let Err(error) = result {
        resume_unwind(error);
    }
}

fn grid_frame(
    context: &egui::Context,
    editor: &mut RowEditor,
    events: Vec<egui::Event>,
) -> (Option<GridAction>, Option<egui::Pos2>) {
    let page = editor.page.as_ref().unwrap();
    let mut widths = GridWidths::new(false);
    let mut action = None;
    let output = context.run(input(events), |context| {
        egui::CentralPanel::default().show(context, |ui| {
            if let Some(next) =
                editor
                    .grid
                    .show(ui, page, true, false, true, 320.0, None, false, &mut widths)
            {
                action = Some(next);
            }
        });
    });
    (
        action,
        output.platform_output.ime.map(|ime| ime.rect.center()),
    )
}

#[test]
#[ignore = "requires a disposable MariaDB and PHP tunnel on loopback"]
fn grid_typing_and_blank_entry_write_literal_values_and_keep_database_defaults() {
    fn start_draft(
        editor: &mut RowEditor,
        action: Option<GridAction>,
        expected_row: Option<usize>,
        expected_column: usize,
        expected_text: &str,
    ) {
        let Some(GridAction::BeginDraft { row, column, text }) = action else {
            panic!(
                "the focused grid cell did not start a draft for row {expected_row:?}, column {expected_column}, and text {expected_text:?}"
            );
        };
        assert_eq!(row, expected_row);
        assert_eq!(column, expected_column);
        assert_eq!(text.as_deref(), Some(expected_text));
        editor.begin_grid_draft(row, column, text);
    }

    fn save(editor: &mut RowEditor, workspace: &mut Workspace) {
        let edit = editor
            .draft
            .as_ref()
            .unwrap()
            .edit(editor.page.as_ref().unwrap());
        editor.write(workspace, edit, None);
        wait(workspace, Some(editor));
        assert!(editor.error.is_none(), "{:?}", editor.error);
        assert!(editor.draft.is_none());
    }

    let port = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let url = std::env::var("HARBOR_EDITABLE_FIXTURE_HTTP_URL").unwrap();
    assert!(url.starts_with("http://127.0.0.1:"));
    let schema = format!("harbor_grid_entry_{}", std::process::id());
    let mut database = Conn::new(
        OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(port)
            .user(Some("root"))
            .prefer_socket(false),
    )
    .unwrap();
    database
        .query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
        .unwrap();
    let result = catch_unwind(AssertUnwindSafe(|| {
        database.select_db(&schema).unwrap();
        let tunnel = crate::database::HttpTunnelConfig {
            url,
            force_base64: true,
            use_utf8mb4: true,
            ..Default::default()
        };
        for (table, tunnel, first, tail, inserted) in [
            ("direct_items", None, "Typed", "Tail", "newRow"),
            ("http_items", Some(tunnel), "中云", "层", "新行"),
        ] {
            let via_http = tunnel.is_some();
            database
                .query_drop(format!(
                    "CREATE TABLE {table} (
                     title VARCHAR(64) NOT NULL,
                     id INT PRIMARY KEY AUTO_INCREMENT,
                     qty INT NOT NULL DEFAULT 7,
                     title_upper VARCHAR(64) GENERATED ALWAYS AS (UPPER(title)) STORED
                 ) ENGINE=InnoDB"
                ))
                .unwrap();
            let mut workspace = Workspace::new("网格输入验证".into(), None);
            workspace.connect(
                ConnectionConfig {
                    port,
                    database: Some(schema.clone()),
                    ..Default::default()
                },
                tunnel,
            );
            wait(&mut workspace, None);
            assert!(workspace.server.is_some(), "{}", workspace.status);
            let mut editor = RowEditor::new(
                0,
                DbObject {
                    schema: schema.clone(),
                    name: table.into(),
                    kind: ObjectKind::Table,
                },
            );
            editor.load(&mut workspace, 0, None);
            wait(&mut workspace, Some(&mut editor));
            assert!(editor.page.as_ref().unwrap().rows.is_empty());
            assert!(!editor.page.as_ref().unwrap().columns[3].writable);
            let context = egui::Context::default();
            let typed = |text: &str| {
                if via_http {
                    egui::Event::Ime(egui::ImeEvent::Commit(text.into()))
                } else {
                    egui::Event::Text(text.into())
                }
            };
            editor.grid.selection.request_focus();
            grid_frame(&context, &mut editor, vec![]);
            let (entry, _) = grid_frame(&context, &mut editor, vec![typed("最初记录")]);
            start_draft(&mut editor, entry, None, 0, "最初记录");
            save(&mut editor, &mut workspace);

            let page = editor.page.as_ref().unwrap();
            editor
                .grid
                .focus_form_point(page, Point { row: 0, column: 0 });
            let (_, title_cell) = grid_frame(&context, &mut editor, vec![]);
            let title_cell = title_cell.unwrap();
            let page = editor.page.as_ref().unwrap();
            editor
                .grid
                .focus_form_point(page, Point { row: 0, column: 2 });
            grid_frame(&context, &mut editor, vec![]);
            let pointer = |pressed| egui::Event::PointerButton {
                pos: title_cell,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            grid_frame(
                &context,
                &mut editor,
                vec![egui::Event::PointerMoved(title_cell), pointer(true)],
            );
            assert_eq!(editor.grid.form_point().column, 2);
            let (entry, _) = grid_frame(&context, &mut editor, vec![pointer(false), typed(first)]);
            start_draft(&mut editor, entry, Some(0), 0, first);
            for events in [vec![], vec![egui::Event::Text(tail.into())]] {
                let page = editor.page.as_ref().unwrap();
                let draft = editor.draft.as_mut().unwrap();
                let _ = context.run(input(events), |context| {
                    egui::CentralPanel::default().show(context, |ui| {
                        draft.show(ui, page, true, false, 320.0);
                    });
                });
            }
            let edited = format!("{first}{tail}");
            assert_eq!(editor.draft.as_ref().unwrap().fields[0].text, edited);
            save(&mut editor, &mut workspace);

            let page = editor.page.as_ref().unwrap();
            editor
                .grid
                .focus_form_point(page, Point { row: 0, column: 0 });
            grid_frame(&context, &mut editor, vec![]);
            let _ = grid_frame(
                &context,
                &mut editor,
                vec![egui::Event::Key {
                    key: egui::Key::ArrowDown,
                    physical_key: Some(egui::Key::ArrowDown),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
            assert_eq!(editor.grid.blank_column, Some(0));
            let (entry, _) = grid_frame(&context, &mut editor, vec![typed(inserted)]);
            start_draft(&mut editor, entry, None, 0, inserted);
            save(&mut editor, &mut workspace);

            let rows: Vec<(i64, String, i64, String)> = database
                .query(format!(
                    "SELECT id,title,qty,title_upper FROM {table} ORDER BY id"
                ))
                .unwrap();
            assert_eq!(
                rows,
                [
                    (1, edited.clone(), 7, edited.to_uppercase()),
                    (2, inserted.into(), 7, inserted.to_uppercase()),
                ]
            );
        }
    }));
    database
        .query_drop(format!("DROP DATABASE {schema}"))
        .unwrap();
    if let Err(error) = result {
        resume_unwind(error);
    }
}

#[test]
#[ignore = "requires a disposable MariaDB and PHP tunnel on loopback"]
fn long_value_grid_typing_opens_viewer_and_saves_utf8_without_touching_other_fields() {
    fn label_center(output: &egui::FullOutput, label: &str) -> egui::Pos2 {
        fn position(shape: &egui::Shape, label: &str) -> Option<egui::Pos2> {
            match shape {
                egui::Shape::Text(text) if text.galley.text() == label => {
                    Some(text.pos + text.galley.size() * 0.5)
                }
                egui::Shape::Vec(shapes) => shapes.iter().find_map(|shape| position(shape, label)),
                _ => None,
            }
        }
        output
            .shapes
            .iter()
            .find_map(|shape| position(&shape.shape, label))
            .unwrap_or_else(|| panic!("could not find grid or inspector label {label:?}"))
    }

    fn primary(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
    }

    let port = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let url = std::env::var("HARBOR_EDITABLE_FIXTURE_HTTP_URL").unwrap();
    assert!(url.starts_with("http://127.0.0.1:"));
    let schema = format!("harbor_lob_entry_{}", std::process::id());
    let mut database = Conn::new(
        OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(port)
            .user(Some("root"))
            .prefer_socket(false),
    )
    .unwrap();
    database
        .query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
        .unwrap();
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        database.select_db(&schema).unwrap();
        let tunnel = crate::database::HttpTunnelConfig {
            url,
            force_base64: true,
            use_utf8mb4: true,
            ..Default::default()
        };
        for (table, tunnel) in [("local_lob", None), ("php_lob", Some(tunnel))] {
            let via_http = tunnel.is_some();
            database
                .query_drop(format!(
                    "CREATE TABLE {table} (
                    id INT PRIMARY KEY, note TEXT, payload LONGBLOB, tag VARCHAR(40) NOT NULL
                ) ENGINE=InnoDB"
                ))
                .unwrap();
            database
                .query_drop(format!(
                    "INSERT INTO {table} VALUES (1,NULL,NULL,'short-value')"
                ))
                .unwrap();

            let mut workspace = Workspace::new("长字段录入验证".into(), None);
            workspace.connect(
                ConnectionConfig {
                    port,
                    database: Some(schema.clone()),
                    ..Default::default()
                },
                tunnel,
            );
            wait(&mut workspace, None);
            let select = format!("SELECT id, payload, tag FROM {table}");
            assert!(workspace.send_editor_query(
                Command::Query {
                    sql: select,
                    limit: 10
                },
                "读取只读结果…"
            ));
            wait(&mut workspace, None);
            let query = workspace.tabs[workspace.active_tab]
                .report
                .as_ref()
                .unwrap();
            assert!(query.error.is_none(), "{:?}", query.error);
            let set = &query.sets[0];
            assert!(set.long_value_column(1));
            assert!(!set.long_value_column(0));
            assert!(!set.long_value_column(2));
            assert!(matches!(set.rows[0][1], Cell::Null));

            let settings = EditorSettings::default();
            let result_context = egui::Context::default();
            let mut readonly = super::super::output::ResultGrid::default();
            let mut widths = GridWidths::new(false);
            widths.set_data_sizing(true);
            let mut result_frame = |view: &mut super::super::output::ResultGrid, events| {
                result_context.run(input(events), |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        view.show(ui, set, false, true, settings, &mut widths);
                    });
                })
            };
            result_frame(&mut readonly, vec![]);
            let rendered = result_frame(&mut readonly, vec![]);
            for (label, opens) in [
                ("short-value".to_owned(), false),
                (display_cell(&Cell::Null, true), true),
            ] {
                let pos = label_center(&rendered, &label);
                result_frame(
                    &mut readonly,
                    vec![egui::Event::PointerMoved(pos), primary(pos, true)],
                );
                result_frame(&mut readonly, vec![primary(pos, false)]);
                let input = egui::Event::Ime(egui::ImeEvent::Commit("只读".into()));
                result_frame(&mut readonly, vec![input]);
                assert_eq!(readonly.has_modal(), opens);
            }

            let mut editor = if via_http {
                let sql = format!("SELECT id, note AS details, payload AS bytes FROM {table}");
                assert!(workspace.send(
                    Command::QueryEditable {
                        sql: sql.clone(),
                        limit: 10
                    },
                    "读取可编辑结果…"
                ));
                wait(&mut workspace, None);
                let Some(EditableResult::Table(page)) = workspace.editable_page.take() else {
                    panic!("expected the editable F8 projection: {}", workspace.status);
                };
                assert!(!page.can_insert);
                RowEditor::from_query(0, page, sql, 10)
            } else {
                let mut editor = RowEditor::new(
                    0,
                    DbObject {
                        schema: schema.clone(),
                        name: table.into(),
                        kind: ObjectKind::Table,
                    },
                );
                editor.load(&mut workspace, 0, None);
                wait(&mut workspace, Some(&mut editor));
                editor
            };

            let read_saved = |database: &mut Conn| -> (Option<String>, Option<Vec<u8>>, String) {
                database
                    .query_first(format!("SELECT note,payload,tag FROM {table}"))
                    .unwrap()
                    .unwrap()
            };
            let context = egui::Context::default();
            let viewer = |editor: &mut RowEditor, events| {
                context.run(input(events), |ctx| editor.show_inspection(ctx, settings))
            };
            let click_viewer = |editor: &mut RowEditor, label: &str| {
                let pos = label_center(&viewer(editor, Vec::new()), label);
                viewer(
                    editor,
                    vec![egui::Event::PointerMoved(pos), primary(pos, true)],
                );
                viewer(editor, vec![primary(pos, false)]);
            };
            let start_typing = |editor: &mut RowEditor, column, event| {
                let page = editor.page.as_ref().unwrap();
                editor.grid.focus_form_point(page, Point { row: 0, column });
                grid_frame(&context, editor, vec![]);
                let (action, _) = grid_frame(&context, editor, vec![event]);
                let Some(GridAction::BeginDraft { row, column, text }) = action else {
                    panic!("expected to open the editable large-value viewer");
                };
                editor.begin_grid_draft(row, column, text);
                assert!(editor.inspection.is_some());
                assert!(editor.draft.is_none());
                viewer(editor, vec![]);
                viewer(editor, vec![]);
            };

            start_typing(&mut editor, 2, egui::Event::Text("未提交的字节".into()));
            let cancel = label_center(&viewer(&mut editor, vec![]), "取消");
            viewer(
                &mut editor,
                vec![egui::Event::PointerMoved(cancel), primary(cancel, true)],
            );
            viewer(&mut editor, vec![primary(cancel, false)]);
            assert!(editor.inspection.is_none());
            assert!(editor.draft.is_none());
            assert_eq!(
                read_saved(&mut database),
                (None, None, "short-value".into())
            );

            for (column, first, tail) in [
                (
                    1,
                    egui::Event::Key {
                        key: egui::Key::Space,
                        physical_key: Some(egui::Key::Space),
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                    "保留前导空格",
                ),
                (
                    2,
                    egui::Event::Ime(egui::ImeEvent::Commit("中文".into())),
                    "☕",
                ),
            ] {
                start_typing(&mut editor, column, first);
                let view = viewer(&mut editor, vec![egui::Event::Text(tail.into())]);
                let apply = label_center(&view, "应用到行草稿");
                viewer(
                    &mut editor,
                    vec![egui::Event::PointerMoved(apply), primary(apply, true)],
                );
                viewer(&mut editor, vec![primary(apply, false)]);
                assert!(editor.inspection.is_none());
                let original = read_saved(&mut database);
                if column == 1 {
                    assert!(original.0.is_none());
                }
                assert!(original.1.is_none());
                let edit = editor
                    .draft
                    .as_ref()
                    .unwrap()
                    .edit(editor.page.as_ref().unwrap());
                editor.write(&mut workspace, edit, None);
                wait(&mut workspace, Some(&mut editor));
                assert!(editor.error.is_none(), "{:?}", editor.error);
            }
            assert_eq!(
                read_saved(&mut database),
                (
                    Some(" 保留前导空格".into()),
                    Some("中文☕".as_bytes().to_vec()),
                    "short-value".into()
                )
            );

            let page = editor.page.as_ref().unwrap();
            editor.inspection = Some((
                Some(0),
                2,
                Inspector::edit(
                    page.column_label(2).to_owned(),
                    &page.rows[0].cells[2],
                    true,
                    true,
                ),
            ));
            viewer(&mut editor, Vec::new());
            viewer(&mut editor, Vec::new());
            click_viewer(&mut editor, "替换…");
            viewer(&mut editor, Vec::new());
            let search = viewer(&mut editor, vec![egui::Event::Text("中文".into())]);
            label_center(&search, "中文");
            click_viewer(&mut editor, "替换为");
            let replacement = viewer(&mut editor, vec![egui::Event::Text("已替换".into())]);
            label_center(&replacement, "已替换");
            click_viewer(&mut editor, "全部替换");
            label_center(&viewer(&mut editor, Vec::new()), "已替换 1 处。");
            let initial_bytes = "中文☕".as_bytes().to_vec();
            assert_eq!(read_saved(&mut database).1, Some(initial_bytes.clone()));
            assert!(editor.draft.is_none());

            click_viewer(&mut editor, "应用到行草稿");
            assert!(editor.inspection.is_none());
            assert_eq!(read_saved(&mut database).1, Some(initial_bytes));
            let edit = editor
                .draft
                .as_ref()
                .unwrap()
                .edit(editor.page.as_ref().unwrap());
            editor.write(&mut workspace, edit, None);
            wait(&mut workspace, Some(&mut editor));
            assert!(editor.error.is_none(), "{:?}", editor.error);
            assert_eq!(
                read_saved(&mut database),
                (
                    Some(" 保留前导空格".into()),
                    Some("已替换☕".as_bytes().to_vec()),
                    "short-value".into()
                )
            );
        }
    }));
    database
        .query_drop(format!("DROP DATABASE {schema}"))
        .unwrap();
    if let Err(error) = outcome {
        resume_unwind(error);
    }
}

#[test]
#[ignore = "requires a disposable MariaDB and PHP tunnel on loopback"]
fn moving_to_another_row_saves_then_follows_the_record_after_resorting() {
    let port = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let url = std::env::var("HARBOR_EDITABLE_FIXTURE_HTTP_URL").unwrap();
    assert!(url.starts_with("http://127.0.0.1:"));
    let schema = format!("harbor_row_move_{}", std::process::id());
    let mut database = Conn::new(
        OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(port)
            .user(Some("root"))
            .prefer_socket(false),
    )
    .unwrap();
    database
        .query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
        .unwrap();
    let result = catch_unwind(AssertUnwindSafe(|| {
        database.select_db(&schema).unwrap();
        let wait = |workspace: &mut Workspace, mut editor: Option<&mut RowEditor>| {
            let started = Instant::now();
            loop {
                workspace.poll();
                if let Some(editor) = editor.as_deref_mut() {
                    editor.poll(workspace);
                }
                if !workspace.busy()
                    && editor
                        .as_ref()
                        .is_none_or(|editor| editor.pending.is_none())
                {
                    break;
                }
                assert!(
                    started.elapsed() < Duration::from_secs(12),
                    "{}",
                    workspace.status
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let tunnel = crate::database::HttpTunnelConfig {
            url,
            force_base64: true,
            use_utf8mb4: true,
            ..Default::default()
        };
        for (table, view, tunnel) in [
            ("grid_items", DataView::Grid, None),
            ("form_items", DataView::Form, Some(tunnel)),
        ] {
            database
                .query_drop(format!(
                    "CREATE TABLE {table} (id INT PRIMARY KEY, sort_position INT, label VARCHAR(30)) ENGINE=InnoDB"
                ))
                .unwrap();
            database
                .query_drop(format!(
                    "INSERT INTO {table} VALUES (1,10,'甲'),(2,20,'乙'),(3,30,'丙')"
                ))
                .unwrap();
            let query_result = tunnel.is_some();
            let mut workspace = Workspace::new("换行保存测试".into(), None);
            workspace.connect(
                ConnectionConfig {
                    port,
                    database: Some(schema.clone()),
                    ..Default::default()
                },
                tunnel,
            );
            wait(&mut workspace, None);
            assert!(workspace.server.is_some(), "{}", workspace.status);
            let mut editor = if query_result {
                let sql = format!(
                    "SELECT id AS row_id, sort_position, label FROM {schema}.{table} ORDER BY sort_position"
                );
                assert!(workspace.send(
                    Command::QueryEditable {
                        sql: sql.clone(),
                        limit: 20,
                    },
                    "正在读取测试结果…"
                ));
                wait(&mut workspace, None);
                let Some(EditableResult::Table(page)) = workspace.editable_page.take() else {
                    panic!("expected HTTP F8 editable page: {}", workspace.status);
                };
                assert_eq!(page.column_label(0), "row_id");
                RowEditor::from_query(0, page, sql, 20)
            } else {
                let mut editor = RowEditor::new(
                    0,
                    DbObject {
                        schema: schema.clone(),
                        name: table.into(),
                        kind: ObjectKind::Table,
                    },
                );
                editor.server_selection.order.push(TableSort {
                    column: "sort_position".into(),
                    descending: false,
                });
                editor.load(&mut workspace, 0, None);
                wait(&mut workspace, Some(&mut editor));
                editor
            };
            editor.view = view;
            let page = editor.page.as_ref().unwrap();
            assert_eq!(page.rows[1].cells[0], Cell::Text("2".into()));
            editor.draft = Some(Draft::new(page, Some(0), Some(1)));
            editor.move_to(&mut workspace, RowMove::Row(Point { row: 1, column: 1 }));
            assert!(!workspace.busy());
            assert!(editor.draft.is_none());
            assert_eq!(editor.grid.form_point().row, 1);

            let page = editor.page.as_ref().unwrap();
            editor
                .grid
                .focus_form_point(page, Point { row: 0, column: 1 });
            let mut draft = Draft::new(page, Some(0), Some(1));
            draft.fields[1].set_cell(Cell::Text("40".into()));
            let preview = crate::database::preview_table_update(&draft.edit(page)).unwrap();
            assert!(preview.contains("UPDATE"));
            assert!(preview.contains("`sort_position` = 40"));
            assert_eq!(
                database
                    .query_first::<i64, _>(format!("SELECT sort_position FROM {table} WHERE id=1"))
                    .unwrap(),
                Some(10)
            );
            editor.draft = Some(draft);
            editor.move_to(&mut workspace, RowMove::Row(Point { row: 1, column: 1 }));
            wait(&mut workspace, Some(&mut editor));
            assert!(editor.error.is_none(), "{:?}", editor.error);
            assert!(editor.draft.is_none());
            let page = editor.page.as_ref().unwrap();
            let focus = editor.grid.form_point();
            assert_eq!(focus.row, 0);
            assert_eq!(focus.column, 1);
            assert_eq!(page.rows[focus.row].cells[0], Cell::Text("2".into()));
            assert_eq!(
                database
                    .query_first::<i64, _>(format!("SELECT sort_position FROM {table} WHERE id=1"))
                    .unwrap(),
                Some(40)
            );
        }
    }));
    database
        .query_drop(format!("DROP DATABASE {schema}"))
        .unwrap();
    if let Err(error) = result {
        resume_unwind(error);
    }
}

#[test]
fn checked_rows_keep_delete_scope_separate_from_cell_selection() {
    let mut grid = DataGrid::new(egui::Id::new("checked-row-test"));
    grid.selection.select(
        Hit {
            point: Point { row: 8, column: 1 },
            row_header: false,
        },
        false,
    );
    assert!(matches!(
        grid.deletion(grid.single_row()),
        Some(Deletion::Row(8))
    ));

    grid.checked.set(1, true, false);
    grid.checked.set(3, true, true);
    grid.checked.set(6, true, false);
    let deletion = grid.deletion(grid.single_row()).unwrap();
    assert!(matches!(deletion, Deletion::Checked(_)));
    assert_eq!(deletion.rows(), &[1, 2, 3, 6]);
    assert_eq!(grid.single_row(), Some(8));

    grid.checked.clear();
    assert!(matches!(
        grid.deletion(grid.single_row()),
        Some(Deletion::Row(8))
    ));
}

#[test]
fn joined_target_selection_preserves_drafts_and_matches_alias_and_table_on_refresh() {
    let mut workspace = Workspace::new("测试连接".into(), None);
    let table = DbObject {
        schema: "演示库".into(),
        name: "人员".into(),
        kind: ObjectKind::Table,
    };
    let target = |alias: &str, object: DbObject| JoinedEditTarget {
        alias: alias.into(),
        result_rows: vec![None],
        result_columns: vec![None, None],
        page: TablePage {
            object,
            columns: vec![],
            column_labels: vec![],
            primary_key: vec![],
            row_key: vec![],
            rows: vec![],
            offset: 0,
            has_more: false,
            can_insert: false,
            edit_error: None,
            query_timing: None,
        },
    };
    let rows = vec![vec![Cell::Text("编制甲".into()), Cell::Null]];
    let result = |targets| JoinedEditablePage {
        result: ResultSet {
            source_sql: None,
            columns: vec!["负责人".into(), "协办人".into()],
            column_hints: vec![],
            column_origins: Vec::new(),
            excel_types: vec![],
            export_columns: vec![],
            duplicate_source: None,
            notice: None,
            rows: rows.clone(),
            affected_rows: 0,
            last_insert_id: None,
            truncated: false,
        },
        targets,
        query_timing: None,
    };
    let response = result(vec![
        target("领导", table.clone()),
        target("协办", table.clone()),
    ]);
    let first = joined::Target::new(&response.targets[0]);
    let second = joined::Target::new(&response.targets[1]);
    let mut editor = RowEditor::from_joined(
        0,
        response,
        PendingEditableQuery {
            workspace: workspace.key,
            tab: workspace.tab().id,
            sql: "SELECT * FROM 演示库.人员".into(),
            limit: 12,
        },
    );
    let data = editor.joined.as_ref().unwrap();
    assert!(data.selected.as_ref() == Some(&first));
    assert_eq!(editor.object.as_ref(), Some(&table));
    assert_eq!(data.response.as_ref().unwrap().result.rows, rows);
    editor.select_joined_target(None);
    editor.select_joined_target(Some(first.clone()));
    let first_grid = editor.grid.id;
    editor.draft = Some(Draft::new(
        editor.joined.as_ref().unwrap().page().unwrap(),
        None,
        None,
    ));
    editor.select_joined_target(Some(second.clone()));
    assert!(editor.draft.is_some());
    assert!(editor.joined.as_ref().unwrap().selected.as_ref() == Some(&first));
    editor.draft = None;
    editor.select_joined_target(Some(second.clone()));
    assert_ne!(editor.grid.id, first_grid);

    editor.pending = Some(Pending::Read(None));
    workspace.editable_page = Some(EditableResult::Joined(result(vec![
        target("协办", table.clone()),
        target("领导", table.clone()),
    ])));
    editor.poll(&mut workspace);
    assert!(editor.joined.as_ref().unwrap().selected.as_ref() == Some(&second));
    assert_eq!(editor.object.as_ref(), Some(&table));

    editor.pending = Some(Pending::Read(None));
    workspace.editable_page = Some(EditableResult::Joined(result(vec![target(
        "协办",
        DbObject {
            name: "其他源表".into(),
            ..table
        },
    )])));
    editor.poll(&mut workspace);
    let data = editor.joined.as_ref().unwrap();
    assert!(data.selected.is_none() && editor.object.is_none());
    assert_eq!(data.response.as_ref().unwrap().result.rows, rows);
}

#[test]
#[ignore = "requires a disposable MariaDB on loopback via HARBOR_EDITABLE_FIXTURE_PORT"]
fn joined_target_writeback_reloads_original_query_with_the_same_target() {
    let port = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let schema = format!("harbor_joined_editor_{}", std::process::id());
    let mut database = Conn::new(
        OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(port)
            .user(Some("root"))
            .prefer_socket(false),
    )
    .unwrap();
    database
        .query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
        .unwrap();
    let result = catch_unwind(AssertUnwindSafe(|| {
        database.select_db(&schema).unwrap();
        database
            .query_drop("CREATE TABLE people (id INT PRIMARY KEY, name VARCHAR(40)) ENGINE=InnoDB")
            .unwrap();
        database.query_drop("CREATE TABLE notes (id INT PRIMARY KEY, person INT, body VARCHAR(40)) ENGINE=InnoDB").unwrap();
        database
            .query_drop("INSERT INTO people VALUES (1, '负责人'), (2, '缺少备注')")
            .unwrap();
        database
            .query_drop("INSERT INTO notes VALUES (31, 1, '原备注')")
            .unwrap();

        let wait = |workspace: &mut Workspace, mut editor: Option<&mut RowEditor>| {
            let started = Instant::now();
            loop {
                workspace.poll();
                if let Some(editor) = editor.as_deref_mut() {
                    editor.poll(workspace);
                }
                if !workspace.busy()
                    && editor
                        .as_ref()
                        .is_none_or(|editor| editor.pending.is_none())
                {
                    break;
                }
                assert!(started.elapsed() < Duration::from_secs(10));
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let mut workspace = Workspace::new("联表测试".into(), None);
        workspace.connect(
            ConnectionConfig {
                port,
                database: Some(schema.clone()),
                ..Default::default()
            },
            None,
        );
        wait(&mut workspace, None);
        let sql = "SELECT a.id AS person_id, a.name, b.id AS note_id, b.body
                   FROM people AS a LEFT JOIN notes AS b ON b.person = a.id ORDER BY a.id";
        assert!(workspace.send(
            Command::QueryEditable {
                sql: sql.into(),
                limit: 10
            },
            "正在读取…"
        ));
        wait(&mut workspace, None);
        let Some(EditableResult::Joined(result)) = workspace.editable_page.take() else {
            panic!("expected joined query result: {}", workspace.status);
        };
        assert_eq!(result.result.rows.len(), 2);
        assert_eq!(result.result.rows[1][3], Cell::Null);
        let target = joined::Target::new(
            result
                .targets
                .iter()
                .find(|target| target.alias == "b")
                .unwrap(),
        );
        let mut editor = RowEditor::from_joined(
            0,
            result,
            PendingEditableQuery {
                workspace: workspace.key,
                tab: workspace.tab().id,
                sql: sql.into(),
                limit: 10,
            },
        );
        editor.select_joined_target(Some(target.clone()));
        let page = editor.joined.as_ref().unwrap().page().unwrap();
        assert_eq!(page.rows.len(), 1);
        let column = page
            .columns
            .iter()
            .position(|column| column.name == "body")
            .unwrap();
        let mut draft = Draft::new(page, Some(0), Some(column));
        draft.fields[column].set_cell(Cell::Text("已更新".into()));
        let change = draft.edit(page);
        editor.draft = Some(draft);
        editor.write(&mut workspace, change, None);
        wait(&mut workspace, Some(&mut editor));

        assert!(editor.error.is_none(), "{:?}", editor.error);
        assert!(editor.draft.is_none());
        let joined = editor.joined.as_ref().unwrap();
        assert!(joined.selected.as_ref() == Some(&target));
        let rows = &joined.response.as_ref().unwrap().result.rows;
        assert_eq!(rows[0][3], Cell::Text("已更新".into()));
        assert_eq!(rows[1][3], Cell::Null);
        assert_eq!(
            database
                .query_first::<String, _>("SELECT name FROM people WHERE id=1")
                .unwrap()
                .as_deref(),
            Some("负责人")
        );
    }));
    database
        .query_drop(format!("DROP DATABASE {schema}"))
        .unwrap();
    if let Err(error) = result {
        resume_unwind(error);
    }
}

#[test]
#[ignore = "requires a disposable MariaDB and PHP tunnel on loopback"]
fn joined_full_grid_writes_only_the_selected_source_on_both_transports() {
    fn count_text(shape: &egui::Shape, label: &str) -> usize {
        match shape {
            egui::Shape::Text(text) => usize::from(text.galley.text() == label),
            egui::Shape::Vec(shapes) => shapes.iter().map(|shape| count_text(shape, label)).sum(),
            _ => 0,
        }
    }

    let port = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let url = std::env::var("HARBOR_EDITABLE_FIXTURE_HTTP_URL").unwrap();
    assert!(url.starts_with("http://127.0.0.1:"));
    let schema = format!("harbor_join_full_grid_{}", std::process::id());
    let mut database = Conn::new(
        OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(port)
            .user(Some("root"))
            .prefer_socket(false),
    )
    .unwrap();
    database
        .query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
        .unwrap();
    let result = catch_unwind(AssertUnwindSafe(|| {
        database.select_db(&schema).unwrap();
        database
            .query_drop(
                "CREATE TABLE owners (
                    id INT PRIMARY KEY,
                    title VARCHAR(40) NOT NULL,
                    title_upper VARCHAR(40) GENERATED ALWAYS AS (UPPER(title)) STORED
                ) ENGINE=InnoDB",
            )
            .unwrap();
        database
            .query_drop(
                "CREATE TABLE notes (
                    id INT PRIMARY KEY, owner_id INT NOT NULL, body VARCHAR(40) NOT NULL
                ) ENGINE=InnoDB",
            )
            .unwrap();
        database
            .query_drop("INSERT INTO owners (id, title) VALUES (1, 'alice'), (2, 'bob')")
            .unwrap();
        database
            .query_drop("INSERT INTO notes VALUES (11, 1, '甲'), (12, 1, '乙')")
            .unwrap();

        let sql = "SELECT a.title AS owner_name, a.title AS owner_repeated, b.body AS note_body,
                          a.title_upper AS owner_upper, '联表常量' AS marker
                   FROM owners AS a LEFT JOIN notes AS b ON b.owner_id = a.id
                   ORDER BY a.id, b.id";
        let expected_rows = |name: &str| {
            [Some("甲"), Some("乙"), None]
                .into_iter()
                .map(|note| {
                    let title = if note.is_some() { name } else { "bob" };
                    vec![
                        Cell::Text(title.into()),
                        Cell::Text(title.into()),
                        note.map_or(Cell::Null, |value| Cell::Text(value.into())),
                        Cell::Text(title.to_ascii_uppercase()),
                        Cell::Text("联表常量".into()),
                    ]
                })
                .collect::<Vec<_>>()
        };
        let tunnel = crate::database::HttpTunnelConfig {
            url,
            force_base64: true,
            use_utf8mb4: true,
            ..Default::default()
        };
        for tunnel in [None, Some(tunnel)] {
            database
                .query_drop("UPDATE owners SET title = 'alice' WHERE id = 1")
                .unwrap();
            let mut workspace = Workspace::new("完整联表网格测试".into(), None);
            workspace.connect(
                ConnectionConfig {
                    port,
                    database: Some(schema.clone()),
                    ..Default::default()
                },
                tunnel,
            );
            wait(&mut workspace, None);
            assert!(workspace.send_editor_query(
                Command::QueryF8 {
                    sql: sql.into(),
                    limit: 10,
                },
                "正在读取可编辑查询结果…"
            ));
            wait(&mut workspace, None);
            let Some(EditableResult::Joined(reply)) = workspace.editable_page.take() else {
                panic!("expected joined query result: {}", workspace.status);
            };
            assert_eq!(
                reply.result.columns,
                [
                    "owner_name",
                    "owner_repeated",
                    "note_body",
                    "owner_upper",
                    "marker"
                ]
            );
            assert_eq!(reply.result.rows, expected_rows("alice"));
            let owners = joined::Target::new(&reply.targets[0]);
            assert_eq!(reply.targets[0].alias, "a");
            let notes = joined::Target::new(&reply.targets[1]);
            assert_eq!(reply.targets[1].alias, "b");
            let mut state = Some(RowEditor::from_joined(
                0,
                reply,
                PendingEditableQuery {
                    workspace: workspace.key,
                    tab: workspace.tab().id,
                    sql: sql.into(),
                    limit: 10,
                },
            ));
            let output = render_row_editor(&mut workspace, &mut state);
            for header in ["owner_name", "owner_repeated", "note_body", "marker"] {
                assert_eq!(
                    output
                        .shapes
                        .iter()
                        .map(|shape| count_text(&shape.shape, header))
                        .sum::<usize>(),
                    1,
                    "{header}"
                );
            }
            state.as_mut().unwrap().set_view(DataView::Text);
            let output = render_row_editor(&mut workspace, &mut state);
            for text in [
                "复制可见行",
                "owner_repeated",
                "note_body",
                "marker",
                "联表常量",
                "甲",
                "乙",
                "bob",
                "NULL",
            ] {
                assert!(renders_text(&output, text), "缺少联表结果文本：{text}");
            }
            let mut editor = state.take().unwrap();
            assert!(editor.joined.as_ref().unwrap().selected.as_ref() == Some(&owners));
            editor.set_view(DataView::Grid);
            let duplicate_cell = Point { row: 1, column: 1 };
            assert!(
                editor.joined_grid.focus_source_point(
                    &editor
                        .joined
                        .as_ref()
                        .unwrap()
                        .response
                        .as_ref()
                        .unwrap()
                        .result,
                    duplicate_cell
                )
            );
            let action = {
                let target = editor.joined.as_ref().unwrap().binding().unwrap();
                assert_eq!(target.result_rows, [Some(0), Some(0), Some(1)]);
                assert_eq!(joined_source_rows(target, vec![1, 0]), [0]);
                for column in [2, 3, 4] {
                    assert!(
                        joined_edit_action(target, Point { row: 0, column }, Some("忽略".into()))
                            .is_none()
                    );
                }
                joined_edit_action(target, duplicate_cell, Some("updated".into()))
            };
            let Some(GridAction::BeginDraft { row, column, text }) = action else {
                panic!("expected the repeated result cell to open a source draft");
            };
            assert_eq!(row, Some(0));
            assert_eq!(column, 0);
            editor.begin_grid_draft(row, column, text);
            let page = editor.joined.as_ref().unwrap().page().unwrap();
            let edit = editor.draft.as_ref().unwrap().edit(page);
            editor.write(&mut workspace, edit, None);
            wait(&mut workspace, Some(&mut editor));
            assert!(editor.error.is_none(), "{:?}", editor.error);
            assert!(editor.draft.is_none());
            assert_eq!(
                editor.joined_grid.selected_source_point(),
                Some(duplicate_cell)
            );
            let joined = editor.joined.as_ref().unwrap();
            assert!(joined.selected.as_ref() == Some(&owners));
            assert_eq!(
                joined.response.as_ref().unwrap().result.rows,
                expected_rows("updated")
            );
            assert_eq!(
                database
                    .query::<(u32, String), _>("SELECT id, title FROM owners ORDER BY id")
                    .unwrap(),
                [(1, "updated".to_owned()), (2, "bob".to_owned())]
            );
            assert_eq!(
                database
                    .query::<(u32, String), _>("SELECT id, body FROM notes ORDER BY id")
                    .unwrap(),
                [(11, "甲".to_owned()), (12, "乙".to_owned())]
            );
            editor.select_joined_target(Some(notes.clone()));
            assert_eq!(
                editor.joined_grid.selected_source_point(),
                Some(duplicate_cell)
            );
            let joined = editor.joined.as_ref().unwrap();
            assert!(joined.selected.as_ref() == Some(&notes));
            assert_eq!(
                joined.response.as_ref().unwrap().result.rows,
                expected_rows("updated")
            );
            let target = joined.binding().unwrap();
            assert_eq!(target.result_rows, [Some(0), Some(1), None]);
            assert!(
                joined_edit_action(target, Point { row: 2, column: 2 }, Some("忽略".into()))
                    .is_none()
            );
        }
    }));
    database
        .query_drop(format!("DROP DATABASE {schema}"))
        .unwrap();
    if let Err(error) = result {
        resume_unwind(error);
    }
}

#[test]
#[ignore = "requires a disposable MariaDB on loopback via HARBOR_EDITABLE_FIXTURE_PORT"]
fn duplicates_partial_query_as_a_new_typed_row() {
    let port = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let schema = format!("harbor_form_{}_{nonce:x}", std::process::id());
    let mut connection = Conn::new(
        OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(port)
            .user(Some("root"))
            .prefer_socket(false),
    )
    .unwrap();
    connection
        .query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
        .unwrap();

    let result = catch_unwind(AssertUnwindSafe(|| {
        connection.select_db(&schema).unwrap();
        connection
            .query_drop(
                "CREATE TABLE items (
                 id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
                 title VARCHAR(30) NOT NULL, amount DECIMAL(20,4) NOT NULL,
                 payload VARBINARY(16), note VARCHAR(20),
                 fingerprint VARCHAR(30) GENERATED ALWAYS AS (UPPER(title)) STORED
             ) ENGINE=InnoDB",
            )
            .unwrap();
        connection
            .query_drop(
                "INSERT INTO items (title, amount, payload, note)
             VALUES ('原样复制', 12345678901234.5678, X'00FF41', NULL)",
            )
            .unwrap();

        let worker = Worker::new();
        let mut id = 0;
        let mut request = |command| {
            id += 1;
            worker.send(id, command).unwrap();
            let event = worker
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap();
            assert_eq!(event.id, id);
            event.result.unwrap()
        };
        assert!(matches!(
            request(Command::Connect(ConnectionConfig {
                port,
                database: Some(schema.clone()),
                ..Default::default()
            })),
            Response::Connected(_)
        ));
        let Response::EditablePage(page) = request(Command::QueryEditable {
            sql: "SELECT id, title AS caption FROM items".into(),
            limit: 5,
        }) else {
            panic!("expected an editable query projection");
        };
        assert_eq!(page.column_labels, ["id", "caption"]);
        assert!(!page.can_insert);

        let mut editor =
            RowEditor::from_query(0, page, "SELECT id, title AS caption FROM items".into(), 5);
        let focus = Point { row: 0, column: 1 };
        editor
            .grid
            .focus_form_point(editor.page.as_ref().unwrap(), focus);
        editor.set_view(DataView::Form);
        editor.draft = Some(Draft::duplicate(editor.page.as_ref().unwrap(), 0).unwrap());
        editor.set_view(DataView::Grid);
        editor.set_view(DataView::Form);
        assert_eq!(editor.grid.form_point(), focus);

        let page = editor.page.as_ref().unwrap();
        let context = egui::Context::default();
        let drawings = context.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(850.0, 560.0),
                )),
                ..Default::default()
            },
            |context| {
                egui::CentralPanel::default().show(context, |ui| {
                    ui.set_width(740.0);
                    editor
                        .draft
                        .as_mut()
                        .unwrap()
                        .show(ui, page, true, false, 340.0);
                });
            },
        );
        fn shows_amount_on_one_line(shape: &egui::epaint::Shape) -> bool {
            match shape {
                egui::epaint::Shape::Text(text) => {
                    text.galley.text() == "12345678901234.5678" && text.galley.rows.len() == 1
                }
                egui::epaint::Shape::Vec(shapes) => shapes.iter().any(shows_amount_on_one_line),
                _ => false,
            }
        }
        assert!(
            drawings
                .shapes
                .iter()
                .any(|shape| shows_amount_on_one_line(&shape.shape))
        );

        let duplicate = editor.draft.as_ref().unwrap();
        assert!(duplicate.row.is_none());
        assert!(matches!(
            duplicate.fields[0].value(),
            Some(FieldValue::Default)
        ));
        assert!(duplicate.fields[5].value().is_none());
        let edit = duplicate.edit(&page);
        assert!(matches!(edit, TableEdit::Insert { .. }));
        assert_eq!(
            connection
                .query_first::<u64, _>("SELECT COUNT(*) FROM items")
                .unwrap(),
            Some(1)
        );

        assert!(matches!(
            request(Command::EditTable {
                object: page.object.clone(),
                edit,
            }),
            Response::TableEdited { affected_rows: 1 }
        ));
        let copied: (u64, String, String, Vec<u8>, Option<String>, String) = connection
            .query_first(
                "SELECT id, title, CAST(amount AS CHAR), payload, note, fingerprint
                          FROM items ORDER BY id DESC LIMIT 1",
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            copied,
            (
                2,
                "原样复制".into(),
                "12345678901234.5678".into(),
                vec![0, 255, 65],
                None,
                "原样复制".into(),
            )
        );
    }));

    connection
        .query_drop(format!("DROP DATABASE {schema}"))
        .unwrap();
    if let Err(failure) = result {
        resume_unwind(failure);
    }
}
