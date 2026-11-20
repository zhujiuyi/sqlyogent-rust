use std::{
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    thread,
    time::{Duration, Instant},
};

use mysql::{Conn, OptsBuilder, prelude::Queryable};

use crate::app::output::HistoryTiming;
use crate::database::{Cell, Command, ConnectionConfig, HttpTunnelConfig, QueryReport};

use super::{EditableResult, Output, Workspace};

fn wait_until_idle(workspace: &mut Workspace, timeout: Duration) {
    let started = Instant::now();
    loop {
        workspace.poll();
        if !workspace.busy() {
            return;
        }
        assert!(started.elapsed() < timeout, "{}", workspace.status);
        thread::sleep(Duration::from_millis(5));
    }
}

fn send_f8(workspace: &mut Workspace, sql: impl Into<String>) {
    assert!(workspace.send_editor_query(
        Command::QueryF8 {
            sql: sql.into(),
            limit: 10,
        },
        "正在执行 F8 查询…",
    ));
}

fn successful_report(workspace: &Workspace) -> &QueryReport {
    let report = workspace
        .tab()
        .report
        .as_ref()
        .unwrap_or_else(|| panic!("{}", workspace.status));
    assert!(report.error.is_none(), "{:?}", report.error);
    report
}

#[test]
#[ignore = "需要 HARBOR_EDITABLE_FIXTURE_PORT 和 HARBOR_EDITABLE_FIXTURE_HTTP_URL 指定一次性 MariaDB 和归档 PHP 隧道"]
fn result_sets_keep_their_original_batch_statement_on_both_transports() {
    let port = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let url = std::env::var("HARBOR_EDITABLE_FIXTURE_HTTP_URL").unwrap();
    assert!(url.starts_with("http://127.0.0.1:"));
    let tunnel = HttpTunnelConfig {
        url,
        force_base64: true,
        use_utf8mb4: true,
        ..Default::default()
    };
    let batch = [
        (
            "SELECT /* 第一条查询 */ '甲;乙' AS `结果一`",
            "结果一",
            "甲;乙",
        ),
        ("SELECT\n  '丙' AS `结果二`", "结果二", "丙"),
    ];
    let timeout = Duration::from_secs(8);
    for tunnel in [None, Some(tunnel)] {
        let mut workspace = Workspace::new("查询结果来源测试".into(), None);
        workspace.connect(
            ConnectionConfig {
                port,
                database: Some("information_schema".into()),
                ..Default::default()
            },
            tunnel,
        );
        wait_until_idle(&mut workspace, timeout);
        assert!(workspace.server.is_some(), "{}", workspace.status);
        workspace.start_editor_batch(batch.iter().map(|(sql, _, _)| (*sql).into()).collect());
        wait_until_idle(&mut workspace, timeout);
        let report = successful_report(&workspace);
        assert_eq!(report.sets.len(), batch.len());
        for (set, (sql, column, value)) in report.sets.iter().zip(batch) {
            assert_eq!(set.source_sql.as_deref(), Some(sql));
            assert_eq!(set.columns, [column]);
            assert_eq!(set.rows, [vec![Cell::Text(value.into())]]);
        }
    }
}

#[test]
#[ignore = "需要 HARBOR_EDITABLE_FIXTURE_PORT 和 HARBOR_EDITABLE_FIXTURE_HTTP_URL 指定一次性 MariaDB 和归档 PHP 隧道"]
fn editor_batches_keep_results_and_continue_after_server_errors_on_both_transports() {
    let port = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let url = std::env::var("HARBOR_EDITABLE_FIXTURE_HTTP_URL").unwrap();
    assert!(url.starts_with("http://127.0.0.1:"));
    let schema = format!("harbor_editor_batch_{}", std::process::id());
    let item = format!("`{schema}`.`items`");
    let mut fixture = Conn::new(
        OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(port)
            .user(Some("root"))
            .prefer_socket(false),
    )
    .unwrap();
    fixture
        .query_drop(format!("CREATE DATABASE `{schema}` CHARACTER SET utf8mb4"))
        .unwrap();
    let result = catch_unwind(AssertUnwindSafe(|| {
        fixture
            .query_drop(format!(
                "CREATE TABLE {item} (id INT PRIMARY KEY, runs INT NOT NULL) ENGINE=InnoDB"
            ))
            .unwrap();
        fixture
            .query_drop(format!("INSERT INTO {item} (id, runs) VALUES (1, 0)"))
            .unwrap();
        let batch = vec![
            format!("SELECT /* 初始值 */ runs AS before_update\nFROM {item} WHERE id = 1"),
            format!("SELECT /* 找不到列 */ missing_batch_column\nFROM {item}"),
            format!("UPDATE {item} SET runs = runs + 1 WHERE id = 1"),
            format!("INSERT /* 主键重复 */ INTO {item}\nVALUES (1, 99)"),
            format!("USE `{schema}_missing`"),
            format!("SELECT runs AS after_update\nFROM {item} WHERE id = 1"),
        ];
        let admin_failure = format!("SELECT missing_admin_column FROM {item}");
        let admin_update = format!("UPDATE {item} SET runs = runs + 100 WHERE id = 1");
        let timeout = Duration::from_secs(8);
        let tunnel = HttpTunnelConfig {
            url,
            force_base64: true,
            use_utf8mb4: true,
            ..Default::default()
        };
        for tunnel in [None, Some(tunnel)] {
            fixture
                .query_drop(format!("UPDATE {item} SET runs = 0 WHERE id = 1"))
                .unwrap();
            let mut workspace = Workspace::new("编辑器批处理测试".into(), None);
            workspace.connect(
                ConnectionConfig {
                    port,
                    database: Some(schema.clone()),
                    ..Default::default()
                },
                tunnel,
            );
            wait_until_idle(&mut workspace, timeout);
            assert!(workspace.server.is_some(), "{}", workspace.status);

            let origin = workspace.active_tab;
            workspace.start_editor_batch(batch.clone());
            workspace.new_tab("SELECT '另一个页签'".into(), None);
            let other = workspace.active_tab;
            wait_until_idle(&mut workspace, timeout);
            assert_eq!(workspace.active_tab, other);
            assert!(matches!(workspace.output, Output::Results));
            assert!(workspace.tab().report.is_none());
            assert!(workspace.tab().batch_messages.is_none());

            workspace.select_tab(origin);
            assert!(matches!(workspace.output, Output::Messages));
            let tab = workspace.tab();
            let messages = tab.batch_messages.as_ref().unwrap();
            assert_eq!(
                (messages.statements.len(), messages.total),
                (batch.len(), batch.len()),
                "HTTP 隧道：{}，最后一条消息：{:?}",
                workspace.server.as_ref().unwrap().http_tunnel,
                messages
                    .statements
                    .last()
                    .and_then(|statement| statement.error.as_deref())
            );
            let errors = [None, Some("1054"), None, Some("1062"), Some("1049"), None];
            let ranges = [0..1, 1..1, 1..2, 2..2, 2..2, 2..3];
            for (((message, sql), code), range) in messages
                .statements
                .iter()
                .zip(&batch)
                .zip(errors)
                .zip(ranges)
            {
                assert_eq!(&message.sql, sql);
                assert_eq!(message.result_sets, range);
                assert!(message.elapsed.is_some());
                match (message.error.as_deref(), code) {
                    (Some(error), Some(code)) => assert!(error.contains(code), "{error}"),
                    (None, None) => {}
                    _ => panic!(
                        "错误与预期不符。SQL {sql}，预期 {code:?}，实际 {:?}",
                        message.error
                    ),
                }
            }
            assert_eq!(
                workspace.server.as_ref().unwrap().database.as_deref(),
                Some(schema.as_str())
            );
            let report = tab.report.as_ref().unwrap();
            assert_eq!(
                report
                    .sets
                    .iter()
                    .map(|set| set.source_sql.as_deref())
                    .collect::<Vec<_>>(),
                [0, 2, 5].map(|index| Some(batch[index].as_str()))
            );
            assert_eq!(report.sets[0].columns, ["before_update"]);
            assert_eq!(report.sets[0].rows, [vec![Cell::Text("0".into())]]);
            assert_eq!(report.sets[1].affected_rows, 1);
            assert_eq!(report.sets[2].columns, ["after_update"]);
            assert_eq!(report.sets[2].rows, [vec![Cell::Text("1".into())]]);
            let mut expected_history: Vec<_> = batch.iter().map(String::as_str).collect();
            assert_eq!(
                workspace.history.recorded_sql().collect::<Vec<_>>(),
                expected_history
            );

            workspace.select_tab(other);
            assert!(workspace.start_batch(vec![admin_failure.clone(), admin_update.clone()]));
            wait_until_idle(&mut workspace, timeout);
            let tab = workspace.tab();
            assert!(tab.batch_messages.is_none());
            let error = tab.report.as_ref().unwrap().error.as_deref().unwrap();
            assert!(error.contains("1054"), "{error}");
            expected_history.push(admin_failure.as_str());
            assert_eq!(
                workspace.history.recorded_sql().collect::<Vec<_>>(),
                expected_history
            );
            assert_eq!(
                fixture
                    .query_first::<u32, _>(format!("SELECT runs FROM {item} WHERE id = 1"))
                    .unwrap(),
                Some(1)
            );

            let no_rows = format!("UPDATE {item} SET runs = 1 WHERE id = 1");
            workspace.start_editor_batch(vec![no_rows.clone()]);
            wait_until_idle(&mut workspace, timeout);
            assert!(matches!(workspace.output, Output::Messages));
            assert!(successful_report(&workspace).sets[0].columns.is_empty());

            workspace.start_editor_batch(vec![no_rows, batch[5].clone()]);
            wait_until_idle(&mut workspace, timeout);
            assert!(matches!(workspace.output, Output::Results));
            assert_eq!(workspace.tab().set, 1);
            assert_eq!(
                successful_report(&workspace).sets[1].columns,
                ["after_update"]
            );
        }
    }));
    fixture
        .query_drop(format!("DROP DATABASE `{schema}`"))
        .unwrap();
    if let Err(error) = result {
        resume_unwind(error);
    }
}

#[test]
#[ignore = "需要 HARBOR_EDITABLE_FIXTURE_PORT 和 HARBOR_EDITABLE_FIXTURE_HTTP_URL 指定一次性 MariaDB 和归档 PHP 隧道"]
fn editor_batch_reports_native_diagnostics_and_http_wall_time() {
    let port = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let url = std::env::var("HARBOR_EDITABLE_FIXTURE_HTTP_URL").unwrap();
    let endpoint = reqwest::Url::parse(&url).unwrap();
    assert_eq!(endpoint.scheme(), "http");
    assert_eq!(endpoint.host_str(), Some("127.0.0.1"));
    let schema = format!("harbor_editor_diagnostics_{}", std::process::id());
    let mut fixture = Conn::new(
        OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(port)
            .user(Some("root"))
            .prefer_socket(false),
    )
    .unwrap();
    fixture
        .query_drop(format!("CREATE DATABASE `{schema}`"))
        .unwrap();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let timeout = Duration::from_secs(8);
        let tunnel = HttpTunnelConfig {
            url,
            force_base64: true,
            use_utf8mb4: true,
            ..Default::default()
        };
        for (name, tunnel) in [("native_items", None), ("http_items", Some(tunnel))] {
            let native = tunnel.is_none();
            let item = format!("`{schema}`.`{name}`");
            fixture
                .query_drop(format!(
                    "CREATE TABLE {item} (id INT PRIMARY KEY, runs INT NOT NULL) ENGINE=InnoDB"
                ))
                .unwrap();
            fixture
                .query_drop(format!("INSERT INTO {item} VALUES (1, 10)"))
                .unwrap();

            let mut workspace = Workspace::new("编辑器查询诊断测试".into(), None);
            let config = ConnectionConfig {
                port,
                database: Some(schema.clone()),
                ..Default::default()
            };
            workspace.connect(config.clone(), tunnel.clone());
            wait_until_idle(&mut workspace, timeout);
            assert!(workspace.server.is_some(), "{}", workspace.status);

            let mut batch = vec![format!("INSERT IGNORE INTO {item} VALUES (1, 99), (2, 20)")];
            if native {
                batch.push("SHOW WARNINGS".into());
            }
            let select = format!("SELECT id, runs FROM {item} ORDER BY id");
            batch.extend([select.clone(), select]);
            let count = batch.len();
            workspace.start_editor_batch(batch);
            wait_until_idle(&mut workspace, timeout);

            let report = successful_report(&workspace);
            let messages = workspace.tab().batch_messages.as_ref().unwrap();
            assert_eq!(messages.statements.len(), count);
            assert_eq!(report.sets.len(), count);
            assert!(report.sets[0].columns.is_empty());
            assert_eq!(report.sets[0].affected_rows, 1);
            assert_eq!(
                report.sets.last().unwrap().rows,
                [
                    vec![Cell::Text("1".into()), Cell::Text("10".into())],
                    vec![Cell::Text("2".into()), Cell::Text("20".into())],
                ]
            );
            let history: Vec<_> = workspace.history.recorded_timings().collect();
            assert_eq!(history.len(), count);
            for (index, (statement, timing)) in messages.statements.iter().zip(history).enumerate()
            {
                assert_eq!(statement.result_sets, index..index + 1);
                let elapsed = statement.elapsed.unwrap();
                if native {
                    let execution = statement.diagnostics.execution.unwrap();
                    assert_eq!(statement.diagnostics.results.len(), 1);
                    let result = &statement.diagnostics.results[0];
                    assert!(execution <= result.elapsed && result.elapsed <= elapsed);
                    assert_eq!(timing, Some(HistoryTiming::Execution(execution)));
                } else {
                    assert!(statement.diagnostics.execution.is_none());
                    assert!(statement.diagnostics.results.is_empty());
                    assert_eq!(timing, Some(HistoryTiming::Total(elapsed)));
                }
            }
            if native {
                assert_eq!(
                    messages.statements[0].diagnostics.results[0].warning_count,
                    Some(1)
                );
                assert_eq!(report.sets[1].columns, ["Level", "Code", "Message"]);
                assert_eq!(report.sets[1].rows.len(), 1);
                assert_eq!(report.sets[1].rows[0][1], Cell::Text("1062".into()));

                let procedure = format!("`{schema}`.`timed_results`");
                fixture
                    .query_drop(format!(
                        "CREATE PROCEDURE {procedure}() BEGIN
                            SELECT 11 AS before_pause;
                            DO SLEEP(0.16);
                            SELECT 22 AS after_pause;
                        END"
                    ))
                    .unwrap();
                workspace.start_editor_batch(vec![format!("CALL {procedure}()")]);
                wait_until_idle(&mut workspace, timeout);

                let report = successful_report(&workspace);
                assert_eq!(report.sets.len(), 3);
                assert_eq!(report.sets[0].columns, ["before_pause"]);
                assert_eq!(report.sets[0].rows, [vec![Cell::Text("11".into())]]);
                assert_eq!(report.sets[1].columns, ["after_pause"]);
                assert_eq!(report.sets[1].rows, [vec![Cell::Text("22".into())]]);
                assert!(report.sets[2].columns.is_empty());

                let messages = workspace.tab().batch_messages.as_ref().unwrap();
                let [statement] = messages.statements.as_slice() else {
                    panic!("CALL 的结果应对应一条语句消息");
                };
                assert_eq!(statement.result_sets, 0..3);
                let [first, second, completed] = statement.diagnostics.results.as_slice() else {
                    panic!("CALL 的三个结果应分别有计时");
                };
                let execution = statement.diagnostics.execution.unwrap();
                let elapsed = statement.elapsed.unwrap();
                assert!(execution <= first.elapsed && first.elapsed <= second.elapsed);
                assert!(second.elapsed <= completed.elapsed && completed.elapsed <= elapsed);
                assert!(
                    second.elapsed - first.elapsed >= Duration::from_millis(80),
                    "首个结果用时 {:?}，第二个结果用时 {:?}",
                    first.elapsed,
                    second.elapsed,
                );
                assert_eq!(
                    workspace.history.recorded_timings().last(),
                    Some(Some(HistoryTiming::Execution(execution)))
                );
            }

            let summary_sql = format!("SELECT COUNT(*) AS result_total FROM {item}");
            assert!(workspace.start_f8(0, summary_sql.clone(), 20));
            wait_until_idle(&mut workspace, timeout);
            let report = successful_report(&workspace);
            assert_eq!(report.sets[0].rows, [vec![Cell::Text("2".into())]]);
            let expected_timing = if native {
                report.diagnostics.execution.map(HistoryTiming::Execution)
            } else {
                Some(HistoryTiming::Total(report.elapsed))
            };
            assert!(expected_timing.is_some());
            assert_eq!(
                workspace.history.recorded_timings().last(),
                Some(expected_timing)
            );
            assert_eq!(
                workspace.history.recorded_sql().last(),
                Some(summary_sql.as_str())
            );

            if native {
                let sql = "SELECT 37 AS profile_history";
                workspace.start_profiled_query(
                    sql.into(),
                    crate::database::ProfileOptions::default(),
                    false,
                );
                wait_until_idle(&mut workspace, timeout);
                let report = successful_report(&workspace);
                let timing = report.diagnostics.execution.map(HistoryTiming::Execution);
                assert!(timing.is_some());
                assert_eq!(workspace.history.recorded_timings().last(), Some(timing));
                assert_eq!(workspace.history.recorded_sql().last(), Some(sql));
            }

            let editable_sql = format!("SELECT id, runs FROM {item} WHERE id = 1");
            assert!(workspace.start_f8(0, editable_sql.clone(), 20));
            wait_until_idle(&mut workspace, timeout);
            assert!(
                workspace.tab().editable_editor.is_some(),
                "{}",
                workspace.status
            );
            let messages = workspace.tab().batch_messages.as_ref().unwrap();
            let message = &messages.statements[0];
            let elapsed = message.elapsed.unwrap();
            assert_eq!(message.sql, editable_sql);
            let editable = message.editable_result.as_ref().unwrap();
            assert_eq!(editable.rows, 1);
            assert!(!editable.truncated);
            let expected_timing = if native {
                let execution = message.diagnostics.execution.unwrap();
                assert!(execution <= elapsed);
                HistoryTiming::Execution(execution)
            } else {
                assert!(message.diagnostics.execution.is_none());
                HistoryTiming::Total(elapsed)
            };
            assert_eq!(
                workspace.history.recorded_timings().last(),
                Some(Some(expected_timing))
            );
            assert_eq!(
                workspace.history.recorded_sql().last(),
                Some(editable_sql.as_str())
            );

            let update_sql = format!("UPDATE {item} SET runs = runs + 1 WHERE id = 1");
            workspace.output = Output::Results;
            assert!(workspace.start_f8(0, update_sql.clone(), 20));
            wait_until_idle(&mut workspace, timeout);
            assert!(matches!(workspace.output, Output::Messages));
            assert!(workspace.tab().editable_editor.is_none());
            let report = successful_report(&workspace);
            assert_eq!(report.sets[0].affected_rows, 1);
            let message = &workspace.tab().batch_messages.as_ref().unwrap().statements[0];
            assert_eq!(message.sql, update_sql);
            assert_eq!(message.result_sets, 0..1);

            let tracked = workspace.history.recorded_sql().count();
            workspace.disconnect();
            wait_until_idle(&mut workspace, timeout);
            assert!(workspace.server.is_none());
            workspace.connect(config, tunnel);
            wait_until_idle(&mut workspace, timeout);
            assert!(workspace.server.is_some(), "{}", workspace.status);
            assert_eq!(workspace.history.recorded_sql().count(), tracked);
            assert_eq!(
                workspace.history.recorded_notes().collect::<Vec<_>>(),
                if native {
                    &["重新连接成功"][..]
                } else {
                    &[][..]
                }
            );
        }
    }));
    fixture
        .query_drop(format!("DROP DATABASE `{schema}`"))
        .unwrap();
    if let Err(error) = result {
        resume_unwind(error);
    }
}

#[test]
#[ignore = "需要 HARBOR_EDITABLE_FIXTURE_PORT 和 HARBOR_EDITABLE_FIXTURE_HTTP_URL 指定一次性 MariaDB 和归档 PHP 隧道"]
fn f8_uses_the_original_query_for_read_only_results() {
    let port = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let url = std::env::var("HARBOR_EDITABLE_FIXTURE_HTTP_URL").unwrap();
    assert!(url.starts_with("http://127.0.0.1:"));
    let schema = format!("harbor_f8_read_only_{}", std::process::id());
    let item = format!("`{schema}`.`items`");
    let view = format!("`{schema}`.`item_labels`");
    let mut fixture = Conn::new(
        OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(port)
            .user(Some("root"))
            .prefer_socket(false),
    )
    .unwrap();
    fixture
        .query_drop(format!("CREATE DATABASE `{schema}` CHARACTER SET utf8mb4"))
        .unwrap();
    let result = catch_unwind(AssertUnwindSafe(|| {
        fixture.query_drop(format!(
            "CREATE TABLE {item} (id INT PRIMARY KEY, label VARCHAR(30) NOT NULL, runs INT NOT NULL DEFAULT 0) ENGINE=InnoDB"
        )).unwrap();
        fixture
            .query_drop(format!(
                "INSERT INTO {item} (id, label) VALUES (1, '甲'), (2, '乙')"
            ))
            .unwrap();
        fixture
            .query_drop(format!(
                "CREATE VIEW {view} AS SELECT id, label FROM {item}"
            ))
            .unwrap();
        let timeout = Duration::from_secs(8);
        let tunnel = HttpTunnelConfig {
            url,
            force_base64: true,
            use_utf8mb4: true,
            ..Default::default()
        };
        for tunnel in [None, Some(tunnel)] {
            let native = tunnel.is_none();
            let mut workspace = Workspace::new("F8 只读查询测试".into(), None);
            workspace.connect(
                ConnectionConfig {
                    port,
                    database: Some(schema.clone()),
                    ..Default::default()
                },
                tunnel,
            );
            wait_until_idle(&mut workspace, timeout);
            assert!(workspace.server.is_some(), "{}", workspace.status);
            workspace.start_editor_batch(vec!["SELECT '上一次结果' AS label".into()]);
            wait_until_idle(&mut workspace, timeout);
            let mut previous = successful_report(&workspace).sets[0].rows.clone();

            send_f8(
                &mut workspace,
                format!("SELECT id, label FROM {item} WHERE id = 1"),
            );
            wait_until_idle(&mut workspace, timeout);
            let Some(EditableResult::Table(page)) = &workspace.editable_page else {
                panic!("{}", workspace.status);
            };
            assert_eq!(
                page.rows[0].cells,
                [Cell::Text("1".into()), Cell::Text("甲".into())]
            );

            for (sql, values) in [
                (format!("SELECT COUNT(*) AS total FROM {item}"), vec!["2"]),
                (format!("SELECT label FROM {view} WHERE id = 1"), vec!["甲"]),
                (
                    "SELECT (SELECT '并集甲') AS label UNION ALL SELECT '并集乙'".into(),
                    vec!["并集甲", "并集乙"],
                ),
            ] {
                send_f8(&mut workspace, sql);
                assert_eq!(successful_report(&workspace).sets[0].rows, previous);
                wait_until_idle(&mut workspace, timeout);
                assert!(workspace.editable_page.is_none());
                assert!(matches!(workspace.output, Output::Results));
                let expected: Vec<_> = values
                    .into_iter()
                    .map(|value| vec![Cell::Text(value.into())])
                    .collect();
                assert_eq!(successful_report(&workspace).sets[0].rows, expected);
                previous = expected;
            }

            let empty_join_sql = format!(
                "SELECT l.id AS left_id, r.id AS right_id FROM {item} l JOIN {item} r ON l.id = r.id WHERE l.id = 999"
            );
            send_f8(&mut workspace, empty_join_sql.clone());
            wait_until_idle(&mut workspace, timeout);
            assert!(workspace.editable_page.is_none());
            assert!(matches!(workspace.output, Output::Results));
            let empty_join = &successful_report(&workspace).sets[0];
            assert_eq!(empty_join.columns, ["left_id", "right_id"]);
            assert!(empty_join.rows.is_empty());
            assert_eq!(
                empty_join.source_sql.as_deref(),
                Some(empty_join_sql.as_str())
            );
            let report = successful_report(&workspace);
            assert_eq!(report.diagnostics.execution.is_some(), native);
            if let Some(execution) = report.diagnostics.execution {
                assert!(execution <= report.elapsed);
                assert_eq!(report.diagnostics.results[0].elapsed, report.elapsed);
            }

            fixture
                .query_drop(format!("UPDATE {item} SET runs = 0 WHERE id = 1"))
                .unwrap();
            send_f8(
                &mut workspace,
                format!("UPDATE {item} SET runs = runs + 1 WHERE id = 1"),
            );
            wait_until_idle(&mut workspace, timeout);
            assert!(workspace.editable_page.is_none());
            assert_eq!(successful_report(&workspace).sets[0].affected_rows, 1);
            if native {
                send_f8(&mut workspace, "SELECT ROW_COUNT() AS changed");
                wait_until_idle(&mut workspace, timeout);
                assert!(workspace.editable_page.is_none());
                assert_eq!(
                    successful_report(&workspace).sets[0].rows,
                    [vec![Cell::Text("1".into())]]
                );
            }
            assert_eq!(
                fixture
                    .query_first::<u32, _>(format!("SELECT runs FROM {item} WHERE id = 1"))
                    .unwrap(),
                Some(1)
            );

            if native {
                let temporary = format!("`{schema}`.`f8_session_only`");
                workspace.start_editor_batch(vec![
                    format!("CREATE TEMPORARY TABLE {temporary} (id INT PRIMARY KEY, label VARCHAR(30) NOT NULL)"),
                    format!("INSERT INTO {temporary} VALUES (7, '临时记录')"),
                ]);
                wait_until_idle(&mut workspace, timeout);
                successful_report(&workspace);
                send_f8(
                    &mut workspace,
                    format!("SELECT id, label FROM {temporary} WHERE id = 7"),
                );
                wait_until_idle(&mut workspace, timeout);
                assert!(workspace.editable_page.is_none());
                assert_eq!(
                    successful_report(&workspace).sets[0].rows,
                    [vec![Cell::Text("7".into()), Cell::Text("临时记录".into())]]
                );

                workspace.start_editor_batch(vec!["SET @harbor_f8_runs = 40".into()]);
                wait_until_idle(&mut workspace, timeout);
                successful_report(&workspace);
                send_f8(
                    &mut workspace,
                    "SELECT (@harbor_f8_runs := @harbor_f8_runs + 1) AS runs",
                );
                wait_until_idle(&mut workspace, timeout);
                assert_eq!(
                    successful_report(&workspace).sets[0].rows,
                    [vec![Cell::Text("41".into())]]
                );
                workspace.start_editor_batch(vec!["SELECT @harbor_f8_runs".into()]);
                wait_until_idle(&mut workspace, timeout);
                assert_eq!(
                    successful_report(&workspace).sets[0].rows,
                    [vec![Cell::Text("41".into())]]
                );

                workspace.start_editor_batch(vec!["SET SESSION sql_mode = 'ANSI_QUOTES'".into()]);
                wait_until_idle(&mut workspace, timeout);
                successful_report(&workspace);
                send_f8(
                    &mut workspace,
                    r#"SELECT "id", "label" FROM "items" WHERE "id" = 1"#,
                );
                wait_until_idle(&mut workspace, timeout);
                let Some(EditableResult::Table(page)) = &workspace.editable_page else {
                    panic!("{}", workspace.status);
                };
                assert_eq!(page.rows[0].cells[1], Cell::Text("甲".into()));
            }
        }
    }));
    fixture
        .query_drop(format!("DROP DATABASE `{schema}`"))
        .unwrap();
    if let Err(error) = result {
        resume_unwind(error);
    }
}

#[test]
#[ignore = "需要 HARBOR_EDITABLE_FIXTURE_PORT 指定一次性 MariaDB"]
fn f8_uses_a_session_temporary_table_when_it_shadows_a_permanent_table() {
    let port = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let schema = format!("harbor_f8_shadow_{}", std::process::id());
    let items = format!("`{schema}`.`items`");
    let categories = format!("`{schema}`.`categories`");
    let mut observer = Conn::new(
        OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(port)
            .user(Some("root"))
            .prefer_socket(false),
    )
    .unwrap();
    observer
        .query_drop(format!("CREATE DATABASE `{schema}` CHARACTER SET utf8mb4"))
        .unwrap();
    let result = catch_unwind(AssertUnwindSafe(|| {
        observer
            .query_drop(format!(
                "CREATE TABLE {items} (id INT PRIMARY KEY, label VARCHAR(40) NOT NULL) ENGINE=InnoDB"
            ))
            .unwrap();
        observer
            .query_drop(format!("INSERT INTO {items} VALUES (1, '永久记录')"))
            .unwrap();
        observer
            .query_drop(format!(
                "CREATE TABLE {categories} (id INT PRIMARY KEY, title VARCHAR(40) NOT NULL) ENGINE=InnoDB"
            ))
            .unwrap();
        observer
            .query_drop(format!("INSERT INTO {categories} VALUES (1, '类别')"))
            .unwrap();

        let timeout = Duration::from_secs(8);
        let mut workspace = Workspace::new("F8 同名临时表测试".into(), None);
        workspace.connect(
            ConnectionConfig {
                port,
                database: Some(schema.clone()),
                ..Default::default()
            },
            None,
        );
        wait_until_idle(&mut workspace, timeout);
        let editor_id = workspace.server.as_ref().unwrap().connection_id;
        workspace.start_editor_batch(vec![
            format!("CREATE TEMPORARY TABLE {items} (id INT PRIMARY KEY, label VARCHAR(40) NOT NULL) ENGINE=InnoDB"),
            format!("INSERT INTO {items} VALUES (1, '会话临时记录')"),
        ]);
        wait_until_idle(&mut workspace, timeout);
        successful_report(&workspace);

        for (sql, expected) in [
            (
                "SELECT id, label FROM `items` WHERE id = 1".to_owned(),
                vec!["1", "会话临时记录"],
            ),
            (
                format!(
                    "SELECT i.id AS item_id, i.label AS item_label, c.title AS category\n                     FROM {items} AS i JOIN {categories} AS c ON c.id = i.id WHERE i.id = 1"
                ),
                vec!["1", "会话临时记录", "类别"],
            ),
        ] {
            send_f8(&mut workspace, sql);
            wait_until_idle(&mut workspace, timeout);
            assert!(workspace.editable_page.is_none(), "{}", workspace.status);
            assert!(matches!(workspace.output, Output::Results));
            let set = &successful_report(&workspace).sets[0];
            let row: Vec<_> = expected
                .into_iter()
                .map(|value| Cell::Text(value.into()))
                .collect();
            assert_eq!(set.rows, [row]);
            assert!(set.duplicate_source.is_none());
        }
        assert_eq!(
            observer
                .query_first::<String, _>(format!("SELECT label FROM {items} WHERE id = 1"))
                .unwrap()
                .as_deref(),
            Some("永久记录")
        );

        workspace.start_editor_batch(vec![format!("DROP TEMPORARY TABLE {items}")]);
        wait_until_idle(&mut workspace, timeout);
        successful_report(&workspace);
        send_f8(
            &mut workspace,
            format!("SELECT id, label FROM {items} WHERE id = 1"),
        );
        wait_until_idle(&mut workspace, timeout);
        let Some(EditableResult::Table(page)) = &workspace.editable_page else {
            panic!("{}", workspace.status);
        };
        assert_eq!(page.rows[0].cells[1], Cell::Text("永久记录".into()));
        assert_eq!(workspace.server.as_ref().unwrap().connection_id, editor_id);
    }));
    observer
        .query_drop(format!("DROP DATABASE `{schema}`"))
        .unwrap();
    if let Err(error) = result {
        resume_unwind(error);
    }
}

#[test]
#[ignore = "需要 HARBOR_EDITABLE_FIXTURE_PORT 指定一次性 MariaDB"]
fn stopping_f8_while_the_editor_checks_a_table_definition() {
    fn wait_for_metadata_lock(observer: &mut Conn, id: u32, sql: &str, timeout: Duration) {
        let started = Instant::now();
        loop {
            let blocked = observer
                .exec_first::<u8, _, _>(
                    "SELECT 1 FROM information_schema.PROCESSLIST
                     WHERE ID = ? AND STATE = 'Waiting for table metadata lock'
                     AND LOCATE(?, UPPER(INFO)) > 0",
                    (id, sql),
                )
                .unwrap();
            if blocked.is_some() {
                return;
            }
            assert!(started.elapsed() < timeout, "未观察到 {sql} 在等待元数据锁");
            thread::sleep(Duration::from_millis(5));
        }
    }

    let port = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let schema = format!("harbor_f8_definition_stop_{}", std::process::id());
    let options = OptsBuilder::new()
        .ip_or_hostname(Some("127.0.0.1"))
        .tcp_port(port)
        .user(Some("root"))
        .prefer_socket(false);
    let mut observer = Conn::new(options.clone()).unwrap();
    observer
        .query_drop(format!("CREATE DATABASE `{schema}` CHARACTER SET utf8mb4"))
        .unwrap();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let items = format!("`{schema}`.`a_items`");
        let guard = format!("`{schema}`.`z_guard`");
        for table in [&items, &guard] {
            observer
                .query_drop(format!(
                    "CREATE TABLE {table} (id INT PRIMARY KEY) ENGINE=InnoDB"
                ))
                .unwrap();
        }

        let timeout = Duration::from_secs(5);
        let mut workspace = Workspace::new("停止 F8 表定义检查测试".into(), None);
        workspace.connect(
            ConnectionConfig {
                port,
                database: Some(schema.clone()),
                ..Default::default()
            },
            None,
        );
        wait_until_idle(&mut workspace, timeout);
        let editor_id = workspace.server.as_ref().unwrap().connection_id;
        workspace.start_editor_batch(vec![
            "SET SESSION lock_wait_timeout = 20".into(),
            "SET @harbor_f8_definition_probe = '保留会话'".into(),
        ]);
        wait_until_idle(&mut workspace, timeout);
        successful_report(&workspace);

        let mut holder = Conn::new(options.clone()).unwrap();
        let mut renamer = Conn::new(options).unwrap();
        let renamer_id = renamer.connection_id();
        renamer
            .query_drop("SET SESSION lock_wait_timeout = 20")
            .unwrap();
        let rename = format!(
            "RENAME TABLE {items} TO `{schema}`.`b_items`, {guard} TO `{schema}`.`y_guard`"
        );
        holder
            .query_drop(format!("LOCK TABLES {guard} WRITE"))
            .unwrap();
        let rename = thread::spawn(move || renamer.query_drop(rename));

        let stopped = catch_unwind(AssertUnwindSafe(|| {
            wait_for_metadata_lock(&mut observer, renamer_id, "RENAME TABLE", timeout);
            send_f8(&mut workspace, format!("SELECT id FROM {items}"));
            wait_for_metadata_lock(&mut observer, editor_id, "SHOW CREATE TABLE", timeout);
            assert!(workspace.cancellable());
            workspace.stop_batch();
            assert!(workspace.editable_read_stopping());
            wait_until_idle(&mut workspace, timeout);
            assert!(workspace.editable_page.is_none());
            assert_eq!(workspace.server.as_ref().unwrap().connection_id, editor_id);

            workspace.start_editor_batch(vec![
                "SELECT CONNECTION_ID(), @harbor_f8_definition_probe".into(),
            ]);
            wait_until_idle(&mut workspace, timeout);
            assert_eq!(
                successful_report(&workspace).sets[0].rows,
                [vec![
                    Cell::Text(editor_id.to_string()),
                    Cell::Text("保留会话".into())
                ]]
            );
        }));

        let unlocked = holder.query_drop("UNLOCK TABLES");
        drop(holder);
        let renamed = rename.join();
        if let Err(error) = stopped {
            resume_unwind(error);
        }
        unlocked.unwrap();
        renamed.unwrap().unwrap();
        assert_eq!(
            observer
                .query_first::<u32, _>(format!("SELECT COUNT(*) FROM `{schema}`.`b_items`"))
                .unwrap(),
            Some(0)
        );
    }));
    observer
        .query_drop(format!("DROP DATABASE `{schema}`"))
        .unwrap();
    if let Err(error) = result {
        resume_unwind(error);
    }
}

#[test]
#[ignore = "需要 HARBOR_EDITABLE_FIXTURE_PORT 和 HARBOR_EDITABLE_FIXTURE_HTTP_URL 指定一次性 MariaDB 和归档 PHP 隧道"]
fn stopping_an_http_f8_write_reports_what_the_server_committed() {
    let port = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let url = std::env::var("HARBOR_EDITABLE_FIXTURE_HTTP_URL").unwrap();
    assert!(url.starts_with("http://127.0.0.1:"));
    let schema = format!("harbor_http_f8_stop_write_{}", std::process::id());
    let table = format!("`{schema}`.`items`");
    let opts = OptsBuilder::new()
        .ip_or_hostname(Some("127.0.0.1"))
        .tcp_port(port)
        .user(Some("root"))
        .prefer_socket(false);
    let mut observer = Conn::new(opts.clone()).unwrap();
    observer
        .query_drop(format!("CREATE DATABASE `{schema}`"))
        .unwrap();
    let result = catch_unwind(AssertUnwindSafe(|| {
        observer
            .query_drop(format!(
                "CREATE TABLE {table} (id INT PRIMARY KEY, runs INT NOT NULL) ENGINE=InnoDB"
            ))
            .unwrap();
        observer
            .query_drop(format!("INSERT INTO {table} VALUES (1, 0)"))
            .unwrap();

        let timeout = Duration::from_secs(5);
        let mut workspace = Workspace::new("停止 HTTP F8 写入测试".into(), None);
        workspace.connect(
            ConnectionConfig {
                port,
                database: Some(schema.clone()),
                ..Default::default()
            },
            Some(HttpTunnelConfig {
                url,
                force_base64: true,
                use_utf8mb4: true,
                ..Default::default()
            }),
        );
        wait_until_idle(&mut workspace, timeout);
        assert!(
            workspace
                .server
                .as_ref()
                .is_some_and(|server| server.http_tunnel),
            "{}",
            workspace.status
        );

        let mut holder = Conn::new(opts).unwrap();
        holder
            .query_drop(format!("LOCK TABLES {table} WRITE"))
            .unwrap();
        send_f8(
            &mut workspace,
            format!("UPDATE {table} SET runs = runs + 1 WHERE id = 1"),
        );
        let started = Instant::now();
        loop {
            let blocked = observer
                .exec_first::<u8, _, _>(
                    "SELECT 1 FROM information_schema.PROCESSLIST
                 WHERE STATE = 'Waiting for table metadata lock'
                 AND INFO LIKE 'UPDATE %' AND LOCATE(?, INFO) > 0",
                    (&schema,),
                )
                .unwrap();
            if blocked.is_some() {
                break;
            }
            assert!(
                started.elapsed() < timeout,
                "未观察到 HTTP F8 更新在等待表锁"
            );
            thread::sleep(Duration::from_millis(5));
        }

        assert!(workspace.cancellable());
        workspace.stop_batch();
        assert!(workspace.editable_read_stopping());
        holder.query_drop("UNLOCK TABLES").unwrap();
        wait_until_idle(&mut workspace, timeout);
        assert!(workspace.editable_page.is_none());
        assert_eq!(successful_report(&workspace).sets[0].affected_rows, 1);
        assert_eq!(
            observer
                .query_first::<u32, _>(format!("SELECT runs FROM {table} WHERE id = 1"))
                .unwrap(),
            Some(1)
        );
    }));
    observer
        .query_drop(format!("DROP DATABASE `{schema}`"))
        .unwrap();
    if let Err(error) = result {
        resume_unwind(error);
    }
}

#[test]
#[ignore = "需要 HARBOR_EDITABLE_FIXTURE_PORT 指定一次性 MariaDB"]
fn stopping_f8_read_only_queries_keeps_the_editor_session() {
    let port = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let mut observer = Conn::new(
        OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(port)
            .user(Some("root"))
            .prefer_socket(false),
    )
    .unwrap();
    let timeout = Duration::from_secs(5);
    let mut workspace = Workspace::new("停止 F8 只读查询测试".into(), None);
    workspace.connect(
        ConnectionConfig {
            port,
            ..Default::default()
        },
        None,
    );
    wait_until_idle(&mut workspace, timeout);
    let main_id = workspace.server.as_ref().unwrap().connection_id;
    workspace.start_editor_batch(vec!["SET @harbor_f8_keep_session = '保留会话'".into()]);
    wait_until_idle(&mut workspace, timeout);
    successful_report(&workspace);

    send_f8(
        &mut workspace,
        "SELECT 1 + SLEEP(15) AS harbor_cancel_f8_read_only",
    );
    let started = Instant::now();
    loop {
        let executing: Option<u8> = observer
            .exec_first(
                "SELECT 1 FROM information_schema.PROCESSLIST
             WHERE ID = ? AND STATE = 'User sleep' AND INFO LIKE '%harbor_cancel_f8_read_only%'",
                (main_id,),
            )
            .unwrap();
        if executing.is_some() {
            break;
        }
        assert!(
            started.elapsed() < timeout,
            "未观察到 F8 查询在编辑器会话中执行"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(workspace.cancellable());
    workspace.stop_batch();
    wait_until_idle(&mut workspace, timeout);
    assert!(workspace.editable_page.is_none());

    workspace.start_editor_batch(vec![
        "SELECT CONNECTION_ID(), @harbor_f8_keep_session".into(),
    ]);
    wait_until_idle(&mut workspace, timeout);
    assert_eq!(
        successful_report(&workspace).sets[0].rows,
        [vec![
            Cell::Text(main_id.to_string()),
            Cell::Text("保留会话".into())
        ]]
    );
}

#[test]
#[ignore = "需要 HARBOR_EDITABLE_FIXTURE_PORT 指定未启用 Performance Schema 的一次性 MariaDB"]
fn stopping_editor_query_keeps_the_session_and_open_transaction() {
    let port = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let suffix = std::process::id();
    let schema = format!("harbor_stop_editor_{suffix}");
    let user = format!("harbor_stop_query_{suffix}");
    let table = format!("`{schema}`.`items`");
    let procedure = format!("`{schema}`.`pause_before_result`");
    let mut fixture = Conn::new(
        OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(port)
            .user(Some("root"))
            .prefer_socket(false),
    )
    .unwrap();
    assert_eq!(
        fixture
            .query_first::<u8, _>("SELECT @@GLOBAL.performance_schema")
            .unwrap(),
        Some(0)
    );
    fixture
        .query_drop(format!("CREATE DATABASE `{schema}`"))
        .unwrap();
    let result = catch_unwind(AssertUnwindSafe(|| {
        fixture
            .query_drop(format!(
                "CREATE TABLE {table} (id INT PRIMARY KEY) ENGINE=InnoDB"
            ))
            .unwrap();
        fixture
            .query_drop(format!(
                "CREATE PROCEDURE {procedure}() SQL SECURITY INVOKER
                 SELECT 1 + SLEEP(15) AS harbor_cancel_inside_call"
            ))
            .unwrap();
        fixture
            .query_drop(format!("CREATE USER '{user}'@'127.0.0.1'"))
            .unwrap();
        fixture
            .query_drop(format!(
                "GRANT SELECT, INSERT ON {table} TO '{user}'@'127.0.0.1'"
            ))
            .unwrap();
        fixture
            .query_drop(format!(
                "GRANT EXECUTE ON PROCEDURE {procedure} TO '{user}'@'127.0.0.1'"
            ))
            .unwrap();

        let normal_timeout = Duration::from_secs(8);
        let mut workspace = Workspace::new("停止编辑器查询测试".into(), None);
        workspace.connect(
            ConnectionConfig {
                port,
                user: user.clone(),
                database: Some(schema.clone()),
                ..Default::default()
            },
            None,
        );
        wait_until_idle(&mut workspace, normal_timeout);
        let main_id = workspace.server.as_ref().unwrap().connection_id;
        workspace.start_editor_batch(vec![
            "SET @harbor_stop_query_marker = '保持会话'".into(),
            "START TRANSACTION".into(),
            format!("INSERT INTO {table} VALUES (1)"),
        ]);
        wait_until_idle(&mut workspace, normal_timeout);
        let setup_report = workspace.tab().report.as_ref().unwrap();
        assert!(setup_report.error.is_none(), "{:?}", setup_report.error);

        for (statement, observed_query) in [
            (
                "SELECT 1 + SLEEP(15) AS harbor_cancel_wait".into(),
                "%harbor_cancel_wait%",
            ),
            (
                "SELECT SLEEP(15) AS harbor_cancel_standalone".into(),
                "%harbor_cancel_standalone%",
            ),
            (format!("CALL {procedure}()"), "%harbor_cancel_inside_call%"),
        ] {
            workspace
                .start_editor_batch(vec![statement, format!("INSERT INTO {table} VALUES (2)")]);
            let started = Instant::now();
            loop {
                let executing: Option<u8> = fixture
                    .exec_first(
                        "SELECT 1 FROM information_schema.PROCESSLIST
                         WHERE ID = ? AND USER = ? AND STATE = 'User sleep' AND INFO LIKE ?",
                        (main_id, &user, observed_query),
                    )
                    .unwrap();
                if executing.is_some() {
                    break;
                }
                assert!(
                    started.elapsed() < Duration::from_secs(5),
                    "未观察到测试查询执行：{observed_query}"
                );
                thread::sleep(Duration::from_millis(5));
            }
            assert!(workspace.cancellable());
            workspace.stop_batch();
            wait_until_idle(&mut workspace, Duration::from_secs(5));
            assert!(workspace.tab().report.as_ref().unwrap().error.is_some());
        }

        assert!(workspace.send_editor_query(
            Command::Query {
                sql: format!("SELECT CONNECTION_ID(), @harbor_stop_query_marker, (SELECT SUM(id) FROM {table})"),
                limit: 1,
            },
            "正在核对会话及事务…",
        ));
        wait_until_idle(&mut workspace, normal_timeout);
        let report = workspace.tab().report.as_ref().unwrap();
        assert!(report.error.is_none(), "{:?}", report.error);
        assert_eq!(
            report.sets[0].rows[0],
            [
                Cell::Text(main_id.to_string()),
                Cell::Text("保持会话".into()),
                Cell::Text("1".into()),
            ]
        );
        assert_eq!(
            fixture
                .query_first::<u64, _>(format!("SELECT COUNT(*) FROM {table}"))
                .unwrap(),
            Some(0)
        );
        workspace.start_editor_batch(vec!["ROLLBACK".into()]);
        wait_until_idle(&mut workspace, normal_timeout);
        assert!(workspace.tab().report.as_ref().unwrap().error.is_none());
        assert_eq!(
            fixture
                .query_first::<u64, _>(format!("SELECT COUNT(*) FROM {table}"))
                .unwrap(),
            Some(0)
        );
    }));
    fixture
        .query_drop(format!("DROP USER IF EXISTS '{user}'@'127.0.0.1'"))
        .unwrap();
    fixture
        .query_drop(format!("DROP DATABASE `{schema}`"))
        .unwrap();
    if let Err(error) = result {
        resume_unwind(error);
    }
}
