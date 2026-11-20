use std::io::{self, BufWriter, Write as IoWrite};
use std::path::Path;

use super::super::row_editor::{layout_guard, load_layout};
use super::super::{SyncScriptFile, SyncScriptPaths};
use super::transport::apply::{
    delete_commands, insert_commands, keyless_receiver_lock,
    mirror::{mirror_count_condition, mirror_record_condition},
};
use super::transport::render_parameters;
use super::*;

pub(super) const SAVE_FOREIGN_KEY_CHECKS: &str =
    "SET @harbor_sync_fk_checks = @@SESSION.foreign_key_checks";
pub(super) const RESTORE_FOREIGN_KEY_CHECKS: &str =
    "SET SESSION foreign_key_checks = @harbor_sync_fk_checks";

pub(in crate::database) fn export_script(
    source_opts: &Opts,
    target_opts: &Opts,
    plan: &SyncPlan,
    paths: &SyncScriptPaths,
    progress: impl FnMut(SyncProgress) -> Result<(), String>,
) -> Result<Vec<SyncScriptFile>, String> {
    export_scope(
        source_opts,
        target_opts,
        &[plan],
        &[0],
        &[0],
        paths,
        progress,
    )
}

pub(in crate::database) fn export_many_scripts(
    source_opts: &Opts,
    target_opts: &Opts,
    plan: &SyncGroupPlan,
    paths: &SyncScriptPaths,
    mut progress: impl FnMut(SyncProgress) -> Result<(), String>,
) -> Result<Vec<SyncScriptFile>, String> {
    let policy = plan.policy();
    if plan.tables.len() != plan.mappings.len()
        || plan
            .tables
            .iter()
            .zip(&plan.mappings)
            .any(|(table, (source, target))| {
                &table.source != source
                    || &table.target != target
                    || table.mode != plan.mode
                    || table.policy() != policy
            })
    {
        return Err("选定的同步表已变更，请重新比较。".into());
    }
    let order = group_dependencies(
        source_opts,
        target_opts,
        &plan.mappings,
        policy,
        &mut progress,
    )?;
    if order.dependencies != plan.dependencies {
        return Err("所选表之间的外键关系已改变，请重新比较。".into());
    }
    let tables = plan.tables.iter().collect::<Vec<_>>();
    export_scope(
        source_opts,
        target_opts,
        &tables,
        &order.source,
        &order.target,
        paths,
        progress,
    )
}

fn export_scope(
    source_opts: &Opts,
    target_opts: &Opts,
    plans: &[&SyncPlan],
    source_order: &[usize],
    target_order: &[usize],
    paths: &SyncScriptPaths,
    mut progress: impl FnMut(SyncProgress) -> Result<(), String>,
) -> Result<Vec<SyncScriptFile>, String> {
    validate_script_paths(plans, paths)?;
    let mut status = SyncProgress::default();
    progress(status)?;
    let (mut source_connection, source_endpoint) = connect(source_opts, "源端")?;
    let (mut target_connection, target_endpoint) = connect(target_opts, "目标端")?;
    let mut source = snapshot(&mut source_connection)?;
    let mut target = snapshot(&mut target_connection)?;
    let mut tables = Vec::with_capacity(plans.len());
    for &plan in plans {
        progress(status)?;
        let approval = &plan.approval;
        if plan.source != approval.source.object
            || plan.target != approval.target.object
            || plan.mode != approval.mode
        {
            return Err("同步配置已变更，请重新比较。".into());
        }
        let mut source_layout = Layout::read(&mut source, &plan.source)?;
        let mut target_layout = Layout::read(&mut target, &plan.target)?;
        approval
            .scope
            .select_rows(&mut source_layout, &mut target_layout);
        verify_layout(
            &approval.source,
            &source_endpoint,
            &mut source_layout,
            "源端",
        )?;
        verify_layout(
            &approval.target,
            &target_endpoint,
            &mut target_layout,
            "目标端",
        )?;
        let actual = inspect_pair(
            &mut source,
            &source_layout,
            &mut target,
            &target_layout,
            &approval.scope,
            false,
            |source_rows, target_rows| {
                progress(SyncProgress {
                    source_rows: status.source_rows + source_rows,
                    target_rows: status.target_rows + target_rows,
                    ..status
                })
            },
        )?;
        verify_snapshot(&approval.source, actual.source, "源端")?;
        verify_snapshot(&approval.target, actual.target, "目标端")?;
        if actual.boundary != approval.boundary {
            return Err("两端筛选范围边界的行匹配已变化，请重新比较。".into());
        }
        status.source_rows += approval.source.snapshot.rows;
        status.target_rows += approval.target.snapshot.rows;
        tables.push(CheckedTable {
            plan,
            source: source_layout,
            target: target_layout,
        });
    }

    // Stage every requested file before publishing any of the scripts.
    let mut staged = Vec::new();
    let skip_target_fk_checks = plans
        .first()
        .is_some_and(|plan| plan.policy().skip_target_fk_checks);
    for (side, path) in [
        (SyncSide::Target, &paths.target),
        (SyncSide::Source, &paths.source),
    ] {
        let Some(path) = path else { continue };
        let (peer, receiver, endpoint, order) = match side {
            SyncSide::Source => (&mut target, &mut source, &source_endpoint, source_order),
            SyncSide::Target => (&mut source, &mut target, &target_endpoint, target_order),
        };
        status.side = Some(side);
        let guarded = tables.iter().any(|table| {
            let layout = match side {
                SyncSide::Source => &table.source,
                SyncSide::Target => &table.target,
            };
            needs_keyless_guards(table, side) || !layout.filter_tables.is_empty()
        });
        let skip_fk_checks = side == SyncSide::Target && skip_target_fk_checks;
        let temporary = stage_file(path, side, endpoint, guarded, skip_fk_checks, |out| {
            write_endpoint(
                out,
                side,
                peer,
                receiver,
                skip_fk_checks,
                order.iter().map(|&index| &tables[index]),
                || progress(status),
            )
        })?;
        staged.push((
            temporary,
            SyncScriptFile {
                side,
                path: path.clone(),
            },
        ));
    }
    status.finishing = true;
    progress(status)?;
    publish_scripts(staged, paths.replace)
}

pub(super) fn validate_script_paths(
    plans: &[&SyncPlan],
    paths: &SyncScriptPaths,
) -> Result<(), String> {
    let writes_source = plans.iter().any(|plan| plan.planned_source().total() != 0);
    let writes_target = plans.iter().any(|plan| plan.planned_target().total() != 0);
    if (!writes_source && !writes_target)
        || writes_source != paths.source.is_some()
        || writes_target != paths.target.is_some()
        || (writes_source && writes_target && (paths.replace || paths.source == paths.target))
    {
        return Err("请为每个有改动的连接分别选择同步脚本的保存位置。".into());
    }
    Ok(())
}

pub(super) fn publish_scripts(
    staged: Vec<(tempfile::NamedTempFile, SyncScriptFile)>,
    replace: bool,
) -> Result<Vec<SyncScriptFile>, String> {
    let mut saved: Vec<SyncScriptFile> = Vec::new();
    for (temporary, file) in staged {
        let result = if replace {
            temporary.persist(&file.path)
        } else {
            temporary.persist_noclobber(&file.path)
        };
        if let Err(error) = result {
            let existing = saved
                .iter()
                .map(|file| file.path.display().to_string())
                .collect::<Vec<_>>();
            let saved_note = if existing.is_empty() {
                String::new()
            } else {
                format!(" 已保存：{}。", existing.join("、"))
            };
            return Err(format!(
                "无法保存 {}：{}。{saved_note}",
                file.path.display(),
                error.error
            ));
        }
        saved.push(file);
    }
    Ok(saved)
}

pub(super) fn stage_guarded_file(
    path: &Path,
    side: SyncSide,
    commands: &[String],
    expected: u64,
    report_unique_preflight: bool,
) -> Result<tempfile::NamedTempFile, String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(script_io)?;
    {
        let mut out = BufWriter::new(file.as_file_mut());
        writeln!(
            out,
            "-- HarborSQL 数据同步计划 · {}\n\
             -- 请在没有其他事务的专用连接中执行，整个文件需使用同一会话。\n\
             -- SQL 文件中的值来自已复核的预览。请核对数据来源、目标及执行结果。\n",
            side.label()
        )
        .map_err(script_io)?;
        for sql in commands {
            writeln!(out, "{sql};").map_err(script_io)?;
        }
        let preflight_result = if report_unique_preflight {
            ", @harbor_sync_unique_failed AS mirror_unique_preflight_failed"
        } else {
            ""
        };
        writeln!(
            out,
            "\nSELECT @harbor_sync_ok AS ready_to_commit, {expected} AS planned_rows{preflight_result};\n\
             -- 仅当无错误且 ready_to_commit = 1 时，才在同一连接提交。\n\
             -- COMMIT;\n-- ROLLBACK;"
        )
        .map_err(script_io)?;
        out.flush().map_err(script_io)?;
    }
    file.as_file().sync_all().map_err(script_io)?;
    Ok(file)
}

fn stage_file(
    path: &Path,
    side: SyncSide,
    endpoint: &EndpointIdentity,
    guarded: bool,
    skip_fk_checks: bool,
    write_body: impl FnOnce(&mut dyn IoWrite) -> Result<SyncWrites, String>,
) -> Result<tempfile::NamedTempFile, String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(script_io)?;
    {
        let mut out = BufWriter::new(file.as_file_mut());
        write_header(&mut out, side, endpoint, skip_fk_checks)?;
        let planned = write_body(&mut out)?;
        writeln!(
            out,
            "\n{RESTORE_FOREIGN_KEY_CHECKS};\nSELECT {} AS planned_rows, @harbor_sync_rows AS changed_rows;",
            planned.total()
        )
        .map_err(script_io)?;
        if guarded {
            writeln!(
                out,
                "SET @harbor_sync_ok = IF(@harbor_sync_ok = 1 AND @harbor_sync_rows = {}, 1, 0);\n\
                 SELECT @harbor_sync_ok AS ready_to_commit;\n\
                 -- 仅当无错误且 ready_to_commit = 1 时，才可在同一连接提交。",
                planned.total()
            )
            .map_err(script_io)?;
        } else {
            writeln!(
                out,
                "-- 检查错误、警告和受影响行数后，在同一连接手动执行 COMMIT 或 ROLLBACK。"
            )
            .map_err(script_io)?;
        }
        writeln!(out, "-- COMMIT;\n-- ROLLBACK;").map_err(script_io)?;
        out.flush().map_err(script_io)?;
    }
    file.as_file().sync_all().map_err(script_io)?;
    Ok(file)
}

fn write_header(
    out: &mut dyn IoWrite,
    side: SyncSide,
    endpoint: &EndpointIdentity,
    skip_fk_checks: bool,
) -> Result<(), String> {
    let condition = endpoint_condition(endpoint)?;
    let foreign_key_checks = u8::from(!skip_fk_checks);
    let host = endpoint
        .server
        .get("hostname")
        .map_or(endpoint.host.as_str(), String::as_str);
    let port = endpoint
        .server
        .get("port")
        .map(String::as_str)
        .unwrap_or("未知");
    writeln!(out,
        "-- HarborSQL 数据同步计划 · {}\n\
         -- 生成时的 MySQL 服务器 {}:{}\n\
         -- 请在对应服务器的专用连接中审阅和执行。脚本记录比较时的行，需人工核对源端是否有新变化。\n\
         -- 发生错误、警告、行数或结构不符时，执行 ROLLBACK。可在 HarborSQL 重新比较后运行内置同步。\n\
         SET NAMES utf8mb4;\n\
         {SAVE_FOREIGN_KEY_CHECKS};\n\
         SET SESSION foreign_key_checks = {foreign_key_checks}, SESSION unique_checks = 1, SESSION time_zone = '+00:00';\n\
         SET SESSION sql_mode = CONCAT_WS(',', NULLIF(TRIM(BOTH ',' FROM REPLACE(\
             CONCAT(',', @@SESSION.sql_mode, ','), ',PAD_CHAR_TO_FULL_LENGTH,', ',')), ''), 'NO_AUTO_VALUE_ON_ZERO');\n\
         SET @harbor_sync_endpoint_ok = 0, @harbor_sync_ok = 0, @harbor_sync_rows = 0;\n\
         SET @harbor_sync_endpoint_ok = ({condition}) AND @@SESSION.foreign_key_checks = {foreign_key_checks};\n\
         SET @harbor_sync_ok = @harbor_sync_endpoint_ok;\n\
         SET TRANSACTION ISOLATION LEVEL REPEATABLE READ;\n\
         START TRANSACTION;",
        side.label(), host.escape_debug(), port.escape_debug(),
    ).map_err(script_io)
}

pub(super) fn endpoint_condition(endpoint: &EndpointIdentity) -> Result<String, String> {
    let tests = endpoint
        .server
        .iter()
        .filter_map(|(name, value)| {
            matches!(
                name.as_str(),
                "hostname" | "port" | "version" | "server_id" | "server_uuid" | "server_uid"
            )
            .then(|| (name, value))
        })
        .map(|(name, value)| {
            render_parameters(
                &format!("CAST(@@GLOBAL.{name} AS BINARY) = ?"),
                &[Value::Bytes(value.as_bytes().to_owned())],
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    if tests.is_empty() {
        return Err("服务器未返回可用于标识脚本写入端的信息。".into());
    }
    Ok(tests.join(" AND "))
}

fn write_endpoint<'a>(
    out: &mut dyn IoWrite,
    side: SyncSide,
    peer: &mut impl ReadTransport,
    receiver: &mut Transaction<'_>,
    skip_fk_checks: bool,
    tables: impl Iterator<Item = &'a CheckedTable<'a>> + Clone,
    mut checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<SyncWrites, String> {
    let mut total = SyncWrites::default();
    let mut deletions = Vec::new();
    for table in tables
        .clone()
        .filter(|table| needs_keyless_guards(table, side))
    {
        checkpoint()?;
        let (layout, review) = match side {
            SyncSide::Source => (&table.source, &table.plan.approval.source),
            SyncSide::Target => (&table.target, &table.plan.approval.target),
        };
        let metadata = load_layout(receiver, review.object.clone(), &layout.qualified)?;
        for pin in transport::filter_metadata_pins(layout) {
            writeln!(out, "{pin};").map_err(script_io)?;
        }
        let dependencies = filter_guard_suffix(layout);
        writeln!(
            out,
            "{};\nSET @harbor_sync_ok = (@harbor_sync_ok = 1 AND {} AND ({}){dependencies});",
            keyless_receiver_lock(layout),
            layout_guard(&metadata),
            transport::sync_key_condition(layout),
        )
        .map_err(script_io)?;
    }
    for table in tables.clone() {
        checkpoint()?;
        let (from, to, review, expected) = match side {
            SyncSide::Source => (
                &table.target,
                &table.source,
                &table.plan.approval.source,
                table.plan.planned_source(),
            ),
            SyncSide::Target => (
                &table.source,
                &table.target,
                &table.plan.approval.target,
                table.plan.planned_target(),
            ),
        };
        if expected.total() == 0 {
            continue;
        }
        receiver.check_filter_tables(&to.filter_tables, Some(&to.table))?;
        if expected.inserted + expected.updated > 0 {
            let keyless_writer = to.is_keyless().then(|| WriteStatements::new(to));
            if keyless_writer.is_none() {
                table_guard(out, to)?;
            }
            let mut actual = SyncWrites::default();
            walk(peer, from, |peer, rows, _| {
                checkpoint()?;
                visit_pairs(peer, from, receiver, to, rows, |_, incoming, original| {
                    if let Some(original) = original {
                        if expected.updated == 0
                            || table.plan.approval.scope.equivalent(incoming, original)
                        {
                            return Ok(());
                        }
                        checkpoint()?;
                        write_update(out, to, incoming, original)?;
                        actual.updated += 1;
                    } else {
                        checkpoint()?;
                        if let Some(writer) = &keyless_writer {
                            write_keyless_change(out, insert_commands(to, writer, incoming)?)?;
                        } else {
                            write_insert(out, to, incoming)?;
                        }
                        actual.inserted += 1;
                    }
                    Ok(())
                })
                .map(|_| ())
            })?;
            if actual.inserted != expected.inserted || actual.updated != expected.updated {
                return Err(format!("{}的新增或更新行数与预览不一致。", to.qualified));
            }
            total.add(actual);
        }
        if expected.deleted > 0 {
            deletions.push((from, to, review, expected.deleted));
        }
    }
    for (from, to, review, expected) in deletions.into_iter().rev() {
        checkpoint()?;
        let keyless_writer = to.is_keyless().then(|| WriteStatements::new(to));
        if keyless_writer.is_none() {
            table_guard(out, to)?;
        }
        let (guards, catalog) = if skip_fk_checks {
            (Vec::new(), None)
        } else {
            (
                delete_guards(receiver, &review.object, to)?,
                Some(delete_catalog_check(receiver, to)?),
            )
        };
        if let (Some(_), Some(catalog)) = (&keyless_writer, &catalog) {
            writeln!(
                out,
                "SET @harbor_sync_ok = (@harbor_sync_ok = 1 AND ({catalog}));"
            )
            .map_err(script_io)?;
        }
        let mut deleted = 0;
        walk(receiver, to, |_, rows, _| {
            checkpoint()?;
            let matched = from.probe_scoped(peer, to, rows, false, |_, _| Ok(()))?;
            for (row, state) in rows.iter().zip(matched) {
                if state == PeerState::Missing {
                    checkpoint()?;
                    if let Some(writer) = &keyless_writer {
                        write_keyless_change(out, delete_commands(to, writer, &guards, row)?)?;
                    } else {
                        write_delete(out, to, catalog.as_deref(), &guards, row)?;
                    }
                    deleted += 1;
                }
            }
            Ok(())
        })?;
        if deleted != expected {
            return Err(format!("{}的删除行数与预览不一致。", to.qualified));
        }
        total.deleted += deleted;
    }
    if side == SyncSide::Target {
        for table in tables.filter(|table| {
            (table.target.is_keyless() || !table.target.filter_tables.is_empty())
                && table.plan.mode == SyncMode::MirrorReceiver
        }) {
            write_mirror_check(out, peer, receiver, table, &mut checkpoint)?;
        }
    }
    Ok(total)
}

fn needs_keyless_guards(table: &CheckedTable<'_>, side: SyncSide) -> bool {
    match side {
        SyncSide::Source => table.source.is_keyless() && table.plan.planned_source().total() != 0,
        SyncSide::Target => {
            table.target.is_keyless()
                && (table.plan.planned_target().total() != 0
                    || table.plan.mode == SyncMode::MirrorReceiver)
        }
    }
}

fn write_keyless_change(out: &mut dyn IoWrite, commands: Vec<String>) -> Result<(), String> {
    for sql in commands {
        writeln!(out, "{sql};").map_err(script_io)?;
    }
    writeln!(
        out,
        "SET @harbor_sync_rows = @harbor_sync_rows + IF(@harbor_sync_ok = 1, 1, 0);"
    )
    .map_err(script_io)
}

fn write_mirror_check(
    out: &mut dyn IoWrite,
    peer: &mut impl ReadTransport,
    receiver: &mut Transaction<'_>,
    table: &CheckedTable<'_>,
    checkpoint: &mut impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let scope = &table.plan.approval.scope;
    if !table.target.is_keyless() {
        table_guard(out, &table.target)?;
    }
    let mut count = 0;
    inspect_pair_record(
        peer,
        &table.source,
        receiver,
        &table.target,
        scope,
        false,
        |observed| {
            if let Some(condition) = mirror_record_condition(&table.target, observed)? {
                writeln!(
                    out,
                    "SET @harbor_sync_ok = (@harbor_sync_ok = 1 AND {condition});"
                )
                .map_err(script_io)?;
                count += 1;
            }
            Ok(())
        },
        |_, _| checkpoint(),
    )?;
    if !scope.source_only() {
        let condition = mirror_count_condition(&table.target, count);
        writeln!(
            out,
            "SET @harbor_sync_ok = (@harbor_sync_ok = 1 AND {condition});"
        )
        .map_err(script_io)?;
    }
    Ok(())
}

fn table_guard(out: &mut dyn IoWrite, layout: &Layout) -> Result<(), String> {
    let (schema, table) = &layout.table;
    let check = render_parameters(
        "SELECT 1 FROM information_schema.TABLES
         WHERE CAST(IF(@@lower_case_table_names = 0, TABLE_SCHEMA, LOWER(TABLE_SCHEMA)) AS BINARY) = ?
           AND CAST(IF(@@lower_case_table_names = 0, TABLE_NAME, LOWER(TABLE_NAME)) AS BINARY) = ?
           AND TABLE_TYPE = 'BASE TABLE' AND ENGINE = 'InnoDB'",
        &[Value::Bytes(schema.as_bytes().to_owned()), Value::Bytes(table.as_bytes().to_owned())],
    )?;
    let key = transport::sync_key_condition(layout);
    for pin in transport::filter_metadata_pins(layout) {
        writeln!(out, "{pin};").map_err(script_io)?;
    }
    let dependencies = filter_guard_suffix(layout);
    // A metadata lock holds the checked table and its matching index throughout replay.
    writeln!(
        out,
        "\nSELECT 1 FROM {} LIMIT 0;\n\
        SET @harbor_sync_table_ok = 0;\n\
        SET @harbor_sync_table_ok = @harbor_sync_ok AND EXISTS({check}) AND ({key}){dependencies};",
        layout.qualified
    )
    .map_err(script_io)?;
    if !layout.filter_tables.is_empty() {
        writeln!(
            out,
            "SET @harbor_sync_ok = (@harbor_sync_ok = 1 AND @harbor_sync_table_ok = 1);"
        )
        .map_err(script_io)?;
    }
    Ok(())
}

fn filter_guard_suffix(layout: &Layout) -> String {
    transport::filter_catalog_checks(layout)
        .map(|check| format!(" AND {check}"))
        .collect()
}

fn write_insert(out: &mut dyn IoWrite, layout: &Layout, incoming: &[Value]) -> Result<(), String> {
    let columns = layout
        .columns
        .iter()
        .zip(incoming)
        .filter(|(column, _)| column.generation.is_none());
    let names = columns
        .clone()
        .map(|(column, _)| column.quoted.as_str())
        .collect::<Vec<_>>();
    let values = columns
        .map(|(column, value)| bind_value(&column.bind, value))
        .collect::<Result<Vec<_>, _>>()?;
    writeln!(
        out,
        "INSERT INTO {} ({}) SELECT {} FROM DUAL WHERE @harbor_sync_table_ok = 1;",
        layout.qualified,
        names.join(", "),
        values.join(", ")
    )
    .map_err(script_io)?;
    count_write(out)?;
    verify_scoped_write(out, layout, incoming)
}

fn write_update(
    out: &mut dyn IoWrite,
    layout: &Layout,
    incoming: &[Value],
    original: &[Value],
) -> Result<(), String> {
    let changes = layout
        .columns
        .iter()
        .zip(incoming)
        .filter(|(column, _)| column.generation.is_none())
        .map(|(column, value)| {
            Ok(format!(
                "{} = {}",
                column.quoted,
                bind_value(&column.bind, value)?
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let original = original_predicate(layout, original)?;
    writeln!(
        out,
        "UPDATE {} SET {} WHERE @harbor_sync_table_ok = 1 AND {original};",
        layout.qualified,
        changes.join(", ")
    )
    .map_err(script_io)?;
    count_write(out)?;
    verify_scoped_write(out, layout, incoming)
}

fn verify_scoped_write(
    out: &mut dyn IoWrite,
    layout: &Layout,
    incoming: &[Value],
) -> Result<(), String> {
    if layout.filter_tables.is_empty() {
        return Ok(());
    }
    let stored = original_predicate(layout, incoming)?;
    writeln!(
        out,
        "SET @harbor_sync_ok = (@harbor_sync_ok = 1 AND EXISTS (SELECT 1 FROM {} WHERE {stored}));",
        layout.qualified,
    )
    .map_err(script_io)
}

fn write_delete(
    out: &mut dyn IoWrite,
    layout: &Layout,
    cascade_catalog: Option<&str>,
    guards: &[DeleteGuard],
    original: &[Value],
) -> Result<(), String> {
    let key = layout.key_values(original);
    let original = original_predicate(layout, original)?;
    let cascade_check = cascade_catalog
        .map(|condition| format!(" AND ({condition})"))
        .unwrap_or_default();
    writeln!(
        out,
        "SET @harbor_sync_delete_ok = 0;\n\
        SET @harbor_sync_delete_ok = @harbor_sync_table_ok{cascade_check}"
    )
    .map_err(script_io)?;
    for guard in guards {
        writeln!(
            out,
            "    AND NOT EXISTS({})",
            render_parameters(&guard.query, &key)?
        )
        .map_err(script_io)?;
    }
    writeln!(
        out,
        ";\nDELETE FROM {} WHERE @harbor_sync_delete_ok = 1 AND {original};",
        layout.qualified
    )
    .map_err(script_io)?;
    count_write(out)
}

fn delete_catalog_check(
    transaction: &mut Transaction<'_>,
    layout: &Layout,
) -> Result<String, String> {
    // Recheck the visible automatic referential actions before each exported deletion.
    let (schema, table) = &layout.table;
    let catalog = render_parameters(
        "FROM information_schema.KEY_COLUMN_USAGE AS k
         JOIN information_schema.REFERENTIAL_CONSTRAINTS AS r
           ON CAST(k.CONSTRAINT_SCHEMA AS BINARY) = CAST(r.CONSTRAINT_SCHEMA AS BINARY)
          AND CAST(k.CONSTRAINT_NAME AS BINARY) = CAST(r.CONSTRAINT_NAME AS BINARY)
          AND CAST(k.TABLE_NAME AS BINARY) = CAST(r.TABLE_NAME AS BINARY)
         WHERE CAST(IF(@@lower_case_table_names = 0, k.REFERENCED_TABLE_SCHEMA, LOWER(k.REFERENCED_TABLE_SCHEMA)) AS BINARY) = ?
           AND CAST(IF(@@lower_case_table_names = 0, k.REFERENCED_TABLE_NAME, LOWER(k.REFERENCED_TABLE_NAME)) AS BINARY) = ?
           AND r.DELETE_RULE NOT IN ('RESTRICT', 'NO ACTION')",
        &[Value::Bytes(schema.as_bytes().to_owned()), Value::Bytes(table.as_bytes().to_owned())],
    )?;
    let digest = "BIT_XOR(CAST(CONV(LEFT(SHA2(CONCAT_WS(':', HEX(k.TABLE_SCHEMA), HEX(k.TABLE_NAME),
        HEX(k.CONSTRAINT_NAME), HEX(k.COLUMN_NAME), HEX(k.REFERENCED_COLUMN_NAME), HEX(r.DELETE_RULE)), 256), 16), 16, 10) AS UNSIGNED))";
    let (count, signature) = transaction
        .query_first::<(u64, u64), _>(format!("SELECT COUNT(*), {digest} {catalog}"))
        .map_err(mysql_error)?
        .ok_or("服务器未返回删除依赖信息。")?;
    Ok(format!(
        "SELECT COUNT(*) = {count} AND {digest} = {signature} {catalog}"
    ))
}

fn original_predicate(layout: &Layout, row: &[Value]) -> Result<String, String> {
    let mut predicates = layout
        .key
        .iter()
        .map(|part| {
            let column = &layout.columns[part.column];
            Ok(format!(
                "{} = {}",
                column.quoted,
                bind_value(&column.key_bind(), &row[part.column])?
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    for (column, value) in layout.columns.iter().zip(row) {
        let bind = if column.read.starts_with("CAST(") && column.read.ends_with(" AS BINARY)") {
            "?"
        } else {
            &column.bind
        };
        predicates.push(format!("{} <=> {}", column.read, bind_value(bind, value)?));
    }
    if let Some(selection) = &layout.selection {
        predicates.push(format!("({selection})"));
    }
    Ok(predicates.join(" AND "))
}

fn bind_value(template: &str, value: &Value) -> Result<String, String> {
    render_parameters(template, std::slice::from_ref(value))
}

fn count_write(out: &mut dyn IoWrite) -> Result<(), String> {
    writeln!(
        out,
        "SET @harbor_sync_rows = @harbor_sync_rows + ROW_COUNT();"
    )
    .map_err(script_io)
}

fn script_io(error: io::Error) -> String {
    format!("写入同步 SQL 失败。 {error}")
}
