use super::super::super::row_editor::{TableLayout, http_layout as row_layout, layout_guard};
use super::super::super::{SyncScriptFile, SyncScriptPaths, SyncScriptPlan};
use super::*;

mod continue_on_error;
pub(in crate::database::sync_engine) mod mirror;
mod unique;
use mirror::{DeletePolicy, MirrorCheck};

const MAX_STAGED_DATA: usize = 32 * 1024 * 1024;

enum Mutation {
    Insert(Vec<Value>),
    Update {
        incoming: Vec<Value>,
        original: Vec<Value>,
    },
    Delete(Vec<Value>),
}

#[derive(Default)]
struct Mutations {
    source: Vec<Mutation>,
    target: Vec<Mutation>,
}

impl Mutations {
    fn for_side(&self, side: SyncSide) -> &[Mutation] {
        match side {
            SyncSide::Source => &self.source,
            SyncSide::Target => &self.target,
        }
    }

    fn counts(&self, side: SyncSide) -> SyncWrites {
        let mut counts = SyncWrites::default();
        for mutation in self.for_side(side) {
            match mutation {
                Mutation::Insert(_) => counts.inserted += 1,
                Mutation::Update { .. } => counts.updated += 1,
                Mutation::Delete(_) => counts.deleted += 1,
            }
        }
        counts
    }

    fn record(
        &mut self,
        observed: ObservedRow<'_>,
        mode: SyncMode,
        remaining: &mut usize,
    ) -> Result<(), String> {
        let mut take = |row: &[Value]| {
            let bytes = row
                .iter()
                .map(|value| match value {
                    Value::Bytes(bytes) => bytes.len() + size_of::<Value>(),
                    _ => size_of::<Value>(),
                })
                .sum::<usize>();
            *remaining = remaining
                .checked_sub(bytes)
                .ok_or("本次同步的待写入数据超过 32 MiB，请缩小范围后再比较。")?;
            Ok::<_, String>(row.to_vec())
        };
        match observed {
            ObservedRow::SourceOnly(row) => self.target.push(Mutation::Insert(take(row)?)),
            ObservedRow::TargetOnly(row) if mode == SyncMode::MirrorReceiver => {
                self.target.push(Mutation::Delete(take(row)?))
            }
            ObservedRow::TargetOnly(row) if matches!(mode, SyncMode::ReconcileBoth(_)) => {
                self.source.push(Mutation::Insert(take(row)?))
            }
            ObservedRow::Different {
                source: left,
                target: right,
            } => match mode {
                SyncMode::InsertAndUpdate
                | SyncMode::MirrorReceiver
                | SyncMode::ReconcileBoth(SyncConflict::SourceWins) => {
                    self.target.push(Mutation::Update {
                        incoming: take(left)?,
                        original: take(right)?,
                    })
                }
                SyncMode::ReconcileBoth(SyncConflict::TargetWins) => {
                    self.source.push(Mutation::Update {
                        incoming: take(right)?,
                        original: take(left)?,
                    })
                }
                _ => {}
            },
            _ => {}
        }
        Ok(())
    }
}

struct Staged<'a> {
    review: &'a SyncPlan,
    source: Layout,
    target: Layout,
    mutations: Mutations,
    mirror_check: Option<MirrorCheck>,
}

impl Staged<'_> {
    fn receiver(&self, side: SyncSide) -> (&Layout, &Review) {
        match side {
            SyncSide::Source => (&self.source, &self.review.approval.source),
            SyncSide::Target => (&self.target, &self.review.approval.target),
        }
    }
}

pub(in crate::database) fn apply_transported(
    source: &Transport,
    target: &Transport,
    plan: &SyncPlan,
    progress: impl FnMut(SyncProgress) -> Result<(), String>,
) -> Result<SyncResult, String> {
    if !plan.policy().continue_on_row_error
        && let (Transport::Native(source), Transport::Native(target)) = (source, target)
    {
        return apply(source, target, plan, progress);
    }
    apply_group(
        source,
        target,
        &[plan],
        &[0],
        &[0],
        &BTreeSet::new(),
        plan.policy(),
        progress,
    )
    .map(|(result, _)| result)
}

pub(in crate::database) fn apply_many_transported(
    source: &Transport,
    target: &Transport,
    group: &SyncGroupPlan,
    mut progress: impl FnMut(SyncProgress) -> Result<(), String>,
) -> Result<SyncGroupResult, String> {
    let policy = group.policy();
    if !policy.continue_on_row_error
        && let (Transport::Native(source), Transport::Native(target)) = (source, target)
    {
        return apply_many(source, target, group, progress);
    }
    if group.tables.len() != group.mappings.len()
        || group
            .tables
            .iter()
            .zip(&group.mappings)
            .any(|(plan, (source, target))| {
                plan.source != *source
                    || plan.target != *target
                    || plan.mode != group.mode
                    || plan.policy() != policy
            })
    {
        return Err("选定的同步表已变更，请重新比较。".into());
    }
    let order = transported_dependencies(source, target, &group.mappings, policy, &mut progress)?;
    if order.dependencies != group.dependencies {
        return Err("所选表之间的外键关系已改变，请重新比较。".into());
    }
    let plans = group.tables.iter().collect::<Vec<_>>();
    let (result, completed) = apply_group(
        source,
        target,
        &plans,
        &order.source,
        &order.target,
        &order.dependencies.target,
        policy,
        progress,
    )?;
    let completed = completed.unwrap_or_else(|| {
        plans
            .iter()
            .filter(|table| {
                (table.planned_source().total() == 0 || result.source.total() > 0)
                    && (table.planned_target().total() == 0 || result.target.total() > 0)
            })
            .count()
    });
    Ok(SyncGroupResult {
        completed,
        source: result.source,
        target: result.target,
        stopped: result.stopped,
        uncertain: result.uncertain,
        skipped_rows: result.skipped_rows,
        row_errors: result.row_errors,
    })
}

pub(in crate::database) fn export_transported(
    source: &Transport,
    target: &Transport,
    prepared: &SyncScriptPlan,
    paths: &SyncScriptPaths,
    mut progress: impl FnMut(SyncProgress) -> Result<(), String>,
) -> Result<Vec<SyncScriptFile>, String> {
    let requires_unique_preflight = match prepared {
        SyncScriptPlan::Single(plan) => plan.plan.requires_unique_preflight(),
        SyncScriptPlan::Group(plan) => plan
            .plan
            .tables
            .iter()
            .any(SyncPlan::requires_unique_preflight),
    };
    if !requires_unique_preflight
        && let (Transport::Native(source), Transport::Native(target)) = (source, target)
    {
        return match prepared {
            SyncScriptPlan::Single(plan) => {
                script::export_script(source, target, &plan.plan, paths, progress)
            }
            SyncScriptPlan::Group(plan) => {
                script::export_many_scripts(source, target, &plan.plan, paths, progress)
            }
        };
    }
    let (plans, source_order, target_order, target_dependencies, policy) = match prepared {
        SyncScriptPlan::Single(plan) => (
            vec![&plan.plan],
            vec![0],
            vec![0],
            BTreeSet::new(),
            plan.plan.policy(),
        ),
        SyncScriptPlan::Group(plan) => {
            let group = &plan.plan;
            let policy = group.policy();
            if group.tables.len() != group.mappings.len()
                || group
                    .tables
                    .iter()
                    .zip(&group.mappings)
                    .any(|(plan, (source, target))| {
                        plan.source != *source
                            || plan.target != *target
                            || plan.mode != group.mode
                            || plan.policy() != policy
                    })
            {
                return Err("选定的同步表已变更，请重新比较。".into());
            }
            let order =
                transported_dependencies(source, target, &group.mappings, policy, &mut progress)?;
            if order.dependencies != group.dependencies {
                return Err("所选表之间的外键关系已改变，请重新比较。".into());
            }
            (
                group.tables.iter().collect::<Vec<_>>(),
                order.source,
                order.target,
                order.dependencies.target,
                policy,
            )
        }
    };
    script::validate_script_paths(&plans, paths)?;
    progress(SyncProgress::default())?;
    source.with_reader(SyncSide::Source, |mut src, source_id| {
        target.with_reader(SyncSide::Target, |mut dst, target_id| {
            let (tables, mut status) = capture_reviewed(
                &mut src,
                &mut dst,
                &source_id,
                &target_id,
                &plans,
                true,
                &mut progress,
            )?;
            let mut staged = Vec::new();
            let early_deletions = unique::early_deletions(&tables, &target_dependencies);
            for (side, receiver, path, order) in [
                (SyncSide::Target, target, &paths.target, &target_order),
                (SyncSide::Source, source, &paths.source, &source_order),
            ] {
                let Some(path) = path else { continue };
                status.side = Some(side);
                let skip_fk_checks = side == SyncSide::Target && policy.skip_target_fk_checks;
                let (commands, count) = receiver.with_reader(side, |mut reader, identity| {
                    receiver_commands(
                        &mut reader,
                        &identity,
                        side,
                        &tables,
                        order,
                        &early_deletions,
                        skip_fk_checks,
                        |written| progress(SyncProgress { written, ..status }),
                    )
                })?;
                let temporary = script::stage_guarded_file(
                    path,
                    side,
                    &commands,
                    count,
                    requires_unique_preflight && side == SyncSide::Target,
                )?;
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
            script::publish_scripts(staged, paths.replace)
        })
    })
}

pub(in crate::database::sync_engine) fn apply_group(
    source: &Transport,
    target: &Transport,
    plans: &[&SyncPlan],
    source_order: &[usize],
    target_order: &[usize],
    target_dependencies: &BTreeSet<ForeignKeyEdge>,
    policy: SyncPolicy,
    mut progress: impl FnMut(SyncProgress) -> Result<(), String>,
) -> Result<(SyncResult, Option<usize>), String> {
    if policy.continue_on_row_error
        && plans
            .iter()
            .any(|plan| plan.requires_unique_preflight() && plan.approval.target_writes.deleted > 0)
    {
        return Err(
            "无可用键的镜像需要在同一事务中删除旧行并新增记录。请关闭行错误后继续，再运行。".into(),
        );
    }
    let status = SyncProgress::default();
    progress(status)?;
    source.with_reader(SyncSide::Source, |mut src, source_identity| {
        target.with_reader(SyncSide::Target, |mut dst, target_identity| {
            let (tables, mut progress_state) = capture_reviewed(
                &mut src,
                &mut dst,
                &source_identity,
                &target_identity,
                plans,
                matches!(target, Transport::Http(_)) && !policy.continue_on_row_error,
                &mut progress,
            )?;

            if policy.continue_on_row_error {
                let mut run =
                    continue_on_error::RowSync::new(&tables, progress_state, &mut progress);
                let mut complete = run.receiver(
                    target,
                    &mut dst,
                    &target_identity,
                    SyncSide::Target,
                    target_order,
                    policy.skip_target_fk_checks,
                )?;
                if complete {
                    complete = run.receiver(
                        source,
                        &mut src,
                        &source_identity,
                        SyncSide::Source,
                        source_order,
                        false,
                    )?;
                }
                let (result, completed) = run.finish();
                debug_assert!(complete || result.stopped.is_some());
                return Ok((result, Some(completed)));
            }
            let mut result = SyncResult::default();
            let early_deletions = unique::early_deletions(&tables, target_dependencies);
            for (side, receiver, order) in [
                (SyncSide::Target, target, target_order),
                (SyncSide::Source, source, source_order),
            ] {
                let mut writes = SyncWrites::default();
                for table in &tables {
                    writes.add(table.mutations.counts(side));
                }
                if writes.total() == 0 {
                    continue;
                }
                progress_state.side = Some(side);
                let skip_fk_checks = side == SyncSide::Target && policy.skip_target_fk_checks;
                let outcome = match receiver {
                    Transport::Native(opts) => commit_native(
                        opts,
                        side,
                        &tables,
                        order,
                        &early_deletions,
                        skip_fk_checks,
                        &mut src,
                        |written, finishing| {
                            progress(SyncProgress {
                                written: progress_state.written + written,
                                finishing,
                                ..progress_state
                            })
                        },
                    ),
                    Transport::Http(session) => commit_http(
                        session,
                        side,
                        &tables,
                        order,
                        &early_deletions,
                        skip_fk_checks,
                        |written, finishing| {
                            progress(SyncProgress {
                                written: progress_state.written + written,
                                finishing,
                                ..progress_state
                            })
                        },
                    ),
                };
                match outcome {
                    Ok(()) => {
                        match side {
                            SyncSide::Source => result.source = writes,
                            SyncSide::Target => result.target = writes,
                        }
                        progress_state.written += writes.total();
                    }
                    Err(CommitFailure::BeforeCommit(error)) if result.target.total() == 0 => {
                        return Err(error);
                    }
                    Err(CommitFailure::BeforeCommit(error)) => {
                        result.stopped = Some(format!("{}未提交。{error}", side.label()));
                        break;
                    }
                    Err(CommitFailure::Uncertain(error)) => {
                        result.uncertain = Some(side);
                        result.stopped =
                            Some(format!("无法确认{}事务是否已经提交。{error}", side.label()));
                        break;
                    }
                }
            }
            Ok((result, None))
        })
    })
}

fn capture_reviewed<'a>(
    src: &mut Reader<'_, '_>,
    dst: &mut Reader<'_, '_>,
    source_identity: &EndpointIdentity,
    target_identity: &EndpointIdentity,
    plans: &[&'a SyncPlan],
    verify_mirror_status: bool,
    progress: &mut impl FnMut(SyncProgress) -> Result<(), String>,
) -> Result<(Vec<Staged<'a>>, SyncProgress), String> {
    let mut progress_state = SyncProgress::default();
    let mut remaining = MAX_STAGED_DATA;
    let mut tables = Vec::with_capacity(plans.len());
    for &plan in plans {
        let approval = &plan.approval;
        if approval.source.object != plan.source
            || approval.target.object != plan.target
            || approval.mode != plan.mode
        {
            return Err("同步配置已变更，请重新比较。".into());
        }
        let mut source_layout = src.layout(&plan.source)?;
        let mut target_layout = dst.layout(&plan.target)?;
        verify_layout(
            &approval.source,
            source_identity,
            &mut source_layout,
            SyncSide::Source.label(),
        )?;
        verify_layout(
            &approval.target,
            target_identity,
            &mut target_layout,
            SyncSide::Target.label(),
        )?;
        source_layout.ensure_compatible(&target_layout)?;
        approval
            .scope
            .select_rows(&mut source_layout, &mut target_layout);
        let mut mutations = Mutations::default();
        let mut mirror_check = (plan.mode == SyncMode::MirrorReceiver && verify_mirror_status)
            .then(|| MirrorCheck::new(approval.scope.source_only()));
        let inspected = inspect_pair_record(
            src,
            &source_layout,
            dst,
            &target_layout,
            &approval.scope,
            false,
            |observed| {
                mutations.record(observed, plan.mode, &mut remaining)?;
                if let Some(check) = &mut mirror_check {
                    check.record(&target_layout, observed, &mut remaining)?;
                }
                Ok(())
            },
            |source_rows, target_rows| {
                progress(group_progress(
                    progress_state,
                    SyncProgress {
                        source_rows,
                        target_rows,
                        ..Default::default()
                    },
                ))
            },
        )?;
        if inspection_changes(&inspected, approval, &mutations) {
            return Err("同步范围或写入计划已变化，请重新比较。".into());
        }
        verify_snapshot(&approval.source, inspected.source, SyncSide::Source.label())?;
        verify_snapshot(&approval.target, inspected.target, SyncSide::Target.label())?;
        progress_state.source_rows += approval.source.snapshot.rows;
        progress_state.target_rows += approval.target.snapshot.rows;
        tables.push(Staged {
            review: plan,
            source: source_layout,
            target: target_layout,
            mutations,
            mirror_check,
        });
    }
    Ok((tables, progress_state))
}

fn inspection_changes(inspection: &Inspection, approval: &Approval, mutations: &Mutations) -> bool {
    inspection.boundary != approval.boundary
        || mutations.counts(SyncSide::Source) != approval.source_writes
        || mutations.counts(SyncSide::Target) != approval.target_writes
}

enum CommitFailure {
    BeforeCommit(String),
    Uncertain(String),
}

impl From<String> for CommitFailure {
    fn from(error: String) -> Self {
        Self::BeforeCommit(error)
    }
}

fn commit_native(
    opts: &Opts,
    side: SyncSide,
    tables: &[Staged<'_>],
    order: &[usize],
    early_deletions: &BTreeSet<CatalogTable>,
    skip_fk_checks: bool,
    source_reader: &mut Reader<'_, '_>,
    mut progress: impl FnMut(u64, bool) -> Result<(), String>,
) -> Result<(), CommitFailure> {
    let (mut connection, identity) = open_writer(opts, side, skip_fk_checks)?;
    let locks_keyless = |item: &Staged<'_>| {
        writes_keyless(item, side)
            || side == SyncSide::Target
                && item.review.mode == SyncMode::MirrorReceiver
                && item.target.is_keyless()
    };
    let lock_ranges = tables
        .iter()
        .any(|item| item.mutations.counts(side).deleted > 0 || locks_keyless(item));
    let mut tx = write_transaction(&mut connection, lock_ranges)?;
    for &index in order {
        let item = &tables[index];
        if locks_keyless(item) {
            lock_keyless_receiver(&mut tx, item.receiver(side).0)?;
        }
    }
    let mut written = 0;
    let mut ready = Vec::new();
    for &index in order {
        let item = &tables[index];
        let mutations = item.mutations.for_side(side);
        if mutations.is_empty() {
            continue;
        }
        let (expected, review) = item.receiver(side);
        let mut layout = Layout::read(&mut tx, &review.object)?;
        verify_layout(review, &identity, &mut layout, side.label())?;
        layout.set_scope_from(expected);
        tx.check_filter_tables(&layout.filter_tables, Some(&layout.table))?;
        let writer = WriteStatements::new(&layout);
        ready.push((index, layout, writer, review, mutations));
    }
    if side == SyncSide::Target
        && ready
            .iter()
            .any(|(index, ..)| tables[*index].review.requires_unique_preflight())
    {
        tx.query_drop("SET @harbor_sync_ok = 1, @harbor_sync_unique_failed = 0")
            .map_err(mysql_error)?;
        for (index, layout, writer, _, mutations) in &ready {
            if !tables[*index].review.requires_unique_preflight() {
                continue;
            }
            unique::preflight(*index, layout, writer, mutations, |statement| {
                progress(written, false)?;
                tx.query_drop(statement).map_err(mysql_error)
            })
            .map_err(|error| format!("{} 的镜像预检失败。{error}", layout.qualified))?;
            let allowed: Option<u8> = tx
                .query_first("SELECT @harbor_sync_ok")
                .map_err(mysql_error)?;
            if allowed != Some(1) {
                return Err(format!("{} {}", layout.qualified, unique::REJECTED).into());
            }
        }
    }
    let early: Vec<_> = ready
        .iter()
        .map(|(_, layout, ..)| side == SyncSide::Target && early_deletions.contains(&layout.table))
        .collect();
    for (index, deleting) in unique::mutation_order(&early) {
        let (_, layout, writer, review, mutations) = &ready[index];
        let mut changes = mutations
            .iter()
            .filter(|change| matches!(change, Mutation::Delete(_)) == deleting)
            .peekable();
        let guards = if deleting && !skip_fk_checks && changes.peek().is_some() {
            delete_guards(&mut tx, &review.object, layout)?
        } else {
            Vec::new()
        };
        for change in changes {
            progress(written, false)?;
            match change {
                Mutation::Insert(row) => writer.insert(&mut tx, layout, row)?,
                Mutation::Update { incoming, original } => {
                    writer.update(&mut tx, layout, incoming, original)?
                }
                Mutation::Delete(row) => writer.delete(&mut tx, layout, &guards, row)?,
            }
            written += 1;
        }
    }
    if side == SyncSide::Target {
        for item in tables
            .iter()
            .filter(|item| item.review.mode == SyncMode::MirrorReceiver)
        {
            let approval = &item.review.approval;
            let mut layout = Layout::read(&mut tx, &item.review.target)?;
            verify_layout(&approval.target, &identity, &mut layout, side.label())?;
            layout.set_scope_from(&item.target);
            let final_state = inspect_pair(
                source_reader,
                &item.source,
                &mut tx,
                &layout,
                &approval.scope,
                false,
                |_, _| progress(written, false),
            )?;
            if final_state.source != approval.source.snapshot
                || final_state.boundary != approval.boundary
                || final_state.counts.source_only != 0
                || final_state.counts.target_only != 0
                || final_state.counts.conflicts != 0
            {
                return Err("镜像暂存结果与已核对的源端数据不一致，事务已取消。"
                    .to_owned()
                    .into());
            }
        }
    }
    progress(written, true)?;
    tx.commit()
        .map_err(|error| CommitFailure::Uncertain(mysql_error(error)))?;
    Ok(())
}

fn commit_http(
    session: &Session,
    side: SyncSide,
    tables: &[Staged<'_>],
    order: &[usize],
    early_deletions: &BTreeSet<CatalogTable>,
    skip_fk_checks: bool,
    mut progress: impl FnMut(u64, bool) -> Result<(), String>,
) -> Result<(), CommitFailure> {
    let transport = Transport::Http(Box::new(session.detached()));
    let (mut commands, written) = transport.with_reader(side, |mut reader, identity| {
        receiver_commands(
            &mut reader,
            &identity,
            side,
            tables,
            order,
            early_deletions,
            skip_fk_checks,
            |written| progress(written, false),
        )
    })?;
    commands.extend(FINISH_RECEIVER.map(str::to_owned));
    let borrowed = commands.iter().map(String::as_str).collect::<Vec<_>>();
    let batch = batch_statements(&borrowed)?;
    let payload = session.prepare_payload(&batch, true, "")?;
    progress(written, true)?;
    let reply = session
        .execute_payload(payload, 1)
        .map_err(CommitFailure::Uncertain)?;
    let set = single_set(reply).map_err(CommitFailure::Uncertain)?;
    match receiver_status(&set) {
        Some(1) => Ok(()),
        Some(0) => Err("接收端的结构、原始行或写入结果已变化，事务已回滚。"
            .to_owned()
            .into()),
        Some(2) => Err(unique::REJECTED.to_owned().into()),
        _ => Err(CommitFailure::Uncertain(
            "隧道未返回可验证的提交状态。".into(),
        )),
    }
}

const FINISH_RECEIVER: [&str; 6] = [
    "SET @harbor_finish = IF(@harbor_sync_ok = 1, 'COMMIT AND NO CHAIN NO RELEASE', 'DO 0')",
    "PREPARE harbor_sync_finish FROM @harbor_finish",
    "EXECUTE harbor_sync_finish",
    "DEALLOCATE PREPARE harbor_sync_finish",
    "ROLLBACK AND NO CHAIN NO RELEASE",
    "SELECT IF(@harbor_sync_unique_failed = 1, 2, @harbor_sync_ok) AS harbor_sync_committed",
];

fn receiver_confirmed(set: &http_tunnel::ResultSet) -> Option<bool> {
    match receiver_status(set) {
        Some(0) => Some(false),
        Some(1) => Some(true),
        _ => None,
    }
}

fn receiver_status(set: &http_tunnel::ResultSet) -> Option<u8> {
    let fields = set.columns.as_slice();
    let rows = set.rows.as_slice();
    if let ([field], [row]) = (fields, rows)
        && field.name == "harbor_sync_committed"
        && let [Some(value)] = row.as_slice()
    {
        match value.as_slice() {
            b"0" => return Some(0),
            b"1" => return Some(1),
            b"2" => return Some(2),
            _ => {}
        }
    }
    None
}

fn receiver_start(
    identity: &EndpointIdentity,
    skip_fk_checks: bool,
    lock_ranges: bool,
) -> Result<Vec<String>, String> {
    let foreign_key_checks = u8::from(!skip_fk_checks);
    let mut commands = vec![
        "SET NAMES utf8mb4".into(),
        "SET SESSION time_zone = '+00:00', SESSION autocommit = 1, SESSION sql_quote_show_create = 1".into(),
        "SET SESSION sql_mode = 'STRICT_ALL_TABLES,NO_ENGINE_SUBSTITUTION,NO_AUTO_VALUE_ON_ZERO'".into(),
        script::SAVE_FOREIGN_KEY_CHECKS.into(),
        format!("SET SESSION foreign_key_checks = {foreign_key_checks}, SESSION unique_checks = 1"),
    ];
    if lock_ranges {
        commands.push("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ".into());
    }
    let endpoint = script::endpoint_condition(identity)?;
    commands.extend([
        "START TRANSACTION".into(),
        format!("SET @harbor_sync_ok = ({endpoint}) AND @@SESSION.foreign_key_checks = {foreign_key_checks}, @harbor_sync_unique_failed = 0"),
    ]);
    Ok(commands)
}

fn receiver_layout_condition(
    reader: &mut Reader<'_, '_>,
    identity: &EndpointIdentity,
    side: SyncSide,
    layout: &Layout,
    review: &Review,
) -> Result<String, String> {
    reader.check_filter_tables(&layout.filter_tables, Some(&layout.table))?;
    let metadata = match reader {
        Reader::Http(reader) => row_layout(reader.0, review.object.clone())?,
        Reader::Native(tx) => super::super::super::row_editor::load_layout(
            tx,
            review.object.clone(),
            &layout.qualified,
        )?,
    };
    let mut current = reader.layout(&review.object)?;
    verify_layout(review, identity, &mut current, side.label())?;
    if metadata.edit_error.is_some() || !same_columns(layout, &metadata) {
        return Err("接收表的字段或事务引擎已变化，请重新比较。".into());
    }
    Ok([
        format!("({})", layout_guard(&metadata)),
        format!("({})", sync_key_condition(layout)),
        base_table_condition(&review.object),
    ]
    .into_iter()
    .chain(filter_catalog_checks(layout))
    .collect::<Vec<_>>()
    .join(" AND "))
}

fn receiver_commands(
    reader: &mut Reader<'_, '_>,
    identity: &EndpointIdentity,
    side: SyncSide,
    tables: &[Staged<'_>],
    order: &[usize],
    early_deletions: &BTreeSet<CatalogTable>,
    skip_fk_checks: bool,
    mut progress: impl FnMut(u64) -> Result<(), String>,
) -> Result<(Vec<String>, u64), String> {
    let has_mirror =
        side == SyncSide::Target && tables.iter().any(|item| item.mirror_check.is_some());
    if !skip_fk_checks
        && tables
            .iter()
            .any(|item| item.mutations.counts(side).deleted > 0)
    {
        mirror::require_grant_visibility(reader)?;
    }
    let lock_ranges = has_mirror || tables.iter().any(|item| writes_keyless(item, side));
    let mut commands = receiver_start(identity, skip_fk_checks, lock_ranges)?;
    let mut written = 0;
    let mut estimated = commands.iter().map(String::len).sum::<usize>();
    let mut validation = Vec::new();
    let mut ready = Vec::new();
    for &index in order {
        let item = &tables[index];
        let mutations = item.mutations.for_side(side);
        let mirror = item
            .mirror_check
            .as_ref()
            .filter(|_| side == SyncSide::Target);
        if mutations.is_empty() && mirror.is_none() {
            continue;
        }
        let (layout, review) = item.receiver(side);
        let condition = receiver_layout_condition(reader, identity, side, layout, review)?;
        let lock = if layout.is_keyless() {
            keyless_receiver_lock(layout)
        } else {
            mirror
                .and_then(|check| check.lock(layout))
                .unwrap_or_else(|| format!("SELECT 1 FROM {} LIMIT 0", layout.qualified))
        };
        add_commands(
            &mut commands,
            &mut estimated,
            filter_metadata_pins(layout).chain([lock]),
        )?;
        let guard = format!("SET @harbor_sync_ok = (@harbor_sync_ok = 1 AND {condition})");
        add_commands(&mut validation, &mut estimated, [guard])?;
        let delete = if !skip_fk_checks
            && mutations
                .iter()
                .any(|mutation| matches!(mutation, Mutation::Delete(_)))
        {
            let policy = DeletePolicy::read(reader, &review.object, layout)?;
            add_commands(
                &mut validation,
                &mut estimated,
                [format!(
                    "SET @harbor_sync_ok = (@harbor_sync_ok = 1 AND {})",
                    policy.metadata_condition
                )],
            )?;
            Some(policy)
        } else {
            None
        };
        ready.push((
            index,
            layout,
            mutations,
            mirror,
            delete,
            WriteStatements::new(layout),
        ));
    }
    // Establish the approved key-range locks before any consistent reads start.
    commands.extend(validation);
    if side == SyncSide::Target {
        for &(index, layout, mutations, _, _, ref writer) in &ready {
            if tables[index].review.requires_unique_preflight() {
                unique::preflight(index, layout, writer, mutations, |statement| {
                    progress(written)?;
                    add_commands(&mut commands, &mut estimated, [statement])
                })?;
            }
        }
    }
    let early: Vec<_> = ready
        .iter()
        .map(|(_, layout, ..)| side == SyncSide::Target && early_deletions.contains(&layout.table))
        .collect();
    for (index, deleting) in unique::mutation_order(&early) {
        let (_, layout, mutations, _, policy, writer) = &ready[index];
        let guards = policy
            .as_ref()
            .map_or(&[][..], |policy| policy.guards.as_slice());
        for mutation in mutations
            .iter()
            .filter(|mutation| matches!(mutation, Mutation::Delete(_)) == deleting)
        {
            progress(written)?;
            add_commands(
                &mut commands,
                &mut estimated,
                mutation_sql(layout, writer, mutation, guards)?,
            )?;
            written += 1;
        }
    }
    for (_, layout, _, mirror, _, _) in &ready {
        if let Some(check) = mirror {
            add_commands(&mut commands, &mut estimated, check.checks(layout))?;
        }
    }
    add_commands(
        &mut commands,
        &mut estimated,
        [script::RESTORE_FOREIGN_KEY_CHECKS.into()],
    )?;
    Ok((commands, written))
}

fn writes_keyless(item: &Staged<'_>, side: SyncSide) -> bool {
    item.receiver(side).0.is_keyless() && !item.mutations.for_side(side).is_empty()
}

pub(in crate::database::sync_engine) fn keyless_receiver_lock(layout: &Layout) -> String {
    format!(
        "SELECT COUNT(*) INTO @harbor_sync_locked FROM {} WHERE @harbor_sync_ok = 1 FOR UPDATE",
        layout.qualified
    )
}

fn add_commands(
    commands: &mut Vec<String>,
    estimated: &mut usize,
    added: impl IntoIterator<Item = String>,
) -> Result<(), String> {
    for sql in added {
        *estimated += sql.len() + 32;
        if *estimated > MAX_STAGED_DATA {
            return Err("本次同步生成的 SQL 批次超过 32 MiB，请缩小范围。".into());
        }
        commands.push(sql);
    }
    Ok(())
}

fn same_columns(expected: &Layout, actual: &TableLayout) -> bool {
    expected.columns.len() == actual.columns.len()
        && expected.columns.iter().all(|column| {
            actual
                .columns
                .iter()
                .find(|other| other.name == column.name)
                .is_some_and(|other| {
                    column.declaration == type_shape(&column.kind, other.data_type.clone())
                        && column.nullable == other.nullable
                        && column.generation.as_deref()
                            == other
                                .generation_expression
                                .as_deref()
                                .filter(|value| !value.is_empty())
                        && column.charset == other.charset
                        && column.collation == other.collation
                })
        })
}

fn mutation_sql(
    layout: &Layout,
    writer: &WriteStatements,
    mutation: &Mutation,
    delete_guards: &[DeleteGuard],
) -> Result<Vec<String>, String> {
    match mutation {
        Mutation::Insert(row) => insert_commands(layout, writer, row),
        Mutation::Delete(original) => delete_commands(layout, writer, delete_guards, original),
        Mutation::Update { incoming, original } => {
            if layout.is_keyless() {
                return Err("缺少可用键的表只能添加或删除整行。".into());
            }
            let mut result = Vec::new();
            lock_original_row(
                &mut result,
                layout,
                original,
                !layout.filter_tables.is_empty(),
            )?;
            let mut values = writer.values(incoming);
            values.extend(layout.key_values(original));
            let selection = dependency_selection(layout);
            let sql = format!(
                "{} AND @harbor_sync_ok = 1{selection} LIMIT 1",
                writer.update
            );
            result.push(render_parameters(&sql, &values)?);
            check_stored_row(&mut result, layout, incoming, "BETWEEN 0 AND 1")?;
            Ok(result)
        }
    }
}

pub(in crate::database::sync_engine) fn insert_commands(
    layout: &Layout,
    writer: &WriteStatements,
    row: &[Value],
) -> Result<Vec<String>, String> {
    let mut result = Vec::new();
    if layout.is_keyless() {
        let original = existing_row(layout, row, false)?;
        result.push(format!(
            "SET @harbor_sync_ok = (@harbor_sync_ok = 1 AND NOT EXISTS (SELECT 1 FROM {} WHERE {original} LIMIT 1 FOR UPDATE))",
            layout.qualified
        ));
    }
    let (names, slots): (Vec<_>, Vec<_>) = writer
        .writable
        .iter()
        .map(|&index| &layout.columns[index])
        .map(|column| (column.quoted.as_str(), column.bind.as_str()))
        .unzip();
    let sql = format!(
        "INSERT INTO {} ({}) SELECT {} WHERE @harbor_sync_ok = 1",
        layout.qualified,
        names.join(", "),
        slots.join(", ")
    );
    result.push(render_parameters(&sql, &writer.values(row))?);
    check_stored_row(&mut result, layout, row, "= 1")?;
    Ok(result)
}

pub(in crate::database::sync_engine) fn delete_commands(
    layout: &Layout,
    writer: &WriteStatements,
    delete_guards: &[DeleteGuard],
    original: &[Value],
) -> Result<Vec<String>, String> {
    let mut result = Vec::new();
    let key = layout.key_values(original);
    let (sql, expected, locking) = if layout.is_keyless() {
        let predicate = existing_row(layout, original, true)?;
        let table = &layout.qualified;
        result.push(format!(
            "SELECT COUNT(*) INTO @harbor_sync_reviewed FROM {table} WHERE @harbor_sync_ok = 1 AND {predicate} FOR UPDATE"
        ));
        result.push(
            "SET @harbor_sync_ok = (@harbor_sync_ok = 1 AND @harbor_sync_reviewed > 0)".into(),
        );
        (
            format!("DELETE FROM {table} WHERE @harbor_sync_ok = 1 AND {predicate}"),
            "@harbor_sync_reviewed",
            " LIMIT 1 FOR UPDATE",
        )
    } else {
        lock_original_row(&mut result, layout, original, true)?;
        let selection = dependency_selection(layout);
        (
            render_parameters(
                &format!(
                    "{} AND @harbor_sync_ok = 1{selection} LIMIT 1",
                    writer.delete
                ),
                &key,
            )?,
            "1",
            "",
        )
    };
    for guard in delete_guards {
        let query =
            guard
                .query
                .replacen("SELECT 1 FROM ", "SELECT 1 INTO @harbor_related FROM ", 1);
        result.push("SET @harbor_related = 0".into());
        result.push(render_parameters(&query, &key)?);
        result.push("SET @harbor_sync_ok = (@harbor_sync_ok = 1 AND @harbor_related = 0)".into());
    }
    result.push(sql);
    result.push(WRITE_DIAGNOSTICS.into());
    let key_filter = render_parameters(&layout.predicate, &key)?;
    result.push(format!(
        "SET @harbor_sync_ok = (@harbor_sync_ok = 1 AND @harbor_sync_affected = {expected} AND @harbor_sync_warnings = 0 AND NOT EXISTS (SELECT 1 FROM {} WHERE {key_filter}{locking}))",
        layout.qualified
    ));
    Ok(result)
}

fn dependency_selection(layout: &Layout) -> String {
    if !layout.filter_tables.is_empty()
        && let Some(selection) = &layout.selection
    {
        return format!(" AND ({selection})");
    }
    String::new()
}

const WRITE_DIAGNOSTICS: &str =
    "SET @harbor_sync_affected = ROW_COUNT(), @harbor_sync_warnings = @@warning_count";

fn check_stored_row(
    result: &mut Vec<String>,
    layout: &Layout,
    incoming: &[Value],
    expected: &str,
) -> Result<(), String> {
    result.push(WRITE_DIAGNOSTICS.into());
    let stored = existing_row(layout, incoming, true)?;
    let locking = if layout.is_keyless() {
        " LIMIT 1 FOR UPDATE"
    } else {
        ""
    };
    result.push(format!(
        "SET @harbor_sync_ok = (@harbor_sync_ok = 1 AND @harbor_sync_affected {expected} AND @harbor_sync_warnings = 0 AND EXISTS (SELECT 1 FROM {} WHERE {stored}{locking}))",
        layout.qualified
    ));
    Ok(())
}

fn lock_original_row(
    result: &mut Vec<String>,
    layout: &Layout,
    original: &[Value],
    selected: bool,
) -> Result<(), String> {
    let predicate = existing_row(layout, original, selected)?;
    result.push("SET @harbor_sync_match = 0".into());
    result.push(format!("SELECT 1 INTO @harbor_sync_match FROM {} WHERE @harbor_sync_ok = 1 AND {predicate} LIMIT 1 FOR UPDATE", layout.qualified));
    result.push("SET @harbor_sync_ok = (@harbor_sync_ok = 1 AND @harbor_sync_match = 1)".into());
    Ok(())
}

fn existing_row(layout: &Layout, row: &[Value], selected: bool) -> Result<String, String> {
    row_condition(layout, row, selected, false)
}

fn row_condition(
    layout: &Layout,
    row: &[Value],
    selected: bool,
    compact: bool,
) -> Result<String, String> {
    let mut terms = vec![render_parameters(
        &layout.predicate,
        &layout.key_values(row),
    )?];
    if !layout.is_keyless() {
        for (column, value) in layout.columns.iter().zip(row) {
            if compact && let Value::Bytes(bytes) = value {
                let digest = Sha256::digest(bytes);
                terms.push(format!(
                    "SHA2(CAST({} AS BINARY),256) = '{digest:x}'",
                    column.read
                ));
                continue;
            }
            let (left, right, bound) = match value {
                Value::Float(value) => (
                    format!("({} + 0e0)", column.quoted),
                    "(? + 0e0)".to_owned(),
                    Value::Double(f64::from(*value)),
                ),
                Value::Double(_) => (column.read.clone(), "(? + 0e0)".into(), value.clone()),
                _ => (column.read.clone(), column.bind.clone(), value.clone()),
            };
            terms.push(render_parameters(&format!("{left} <=> {right}"), &[bound])?);
        }
    }
    if selected && let Some(selection) = &layout.selection {
        terms.push(format!("({selection})"));
    }
    Ok(terms.join(" AND "))
}
