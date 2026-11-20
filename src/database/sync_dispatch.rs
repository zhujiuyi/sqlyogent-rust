use super::sync_engine::{
    Transport, apply_many_transported, apply_transported, compare_many_transported,
    compare_transported, export_transported,
};
use super::*;

#[cfg(test)]
mod workflow_test;

pub(super) fn is_sync(command: &Command) -> bool {
    matches!(
        command,
        Command::SyncEndpoint
            | Command::PlanSync { .. }
            | Command::PlanScopedSync { .. }
            | Command::PlanSyncGroup { .. }
            | Command::PlanScopedSyncGroup { .. }
            | Command::ApplySync { .. }
            | Command::ApplySyncGroup { .. }
            | Command::ExportSync { .. }
    )
}

fn endpoint(connection: &Connection) -> SyncEndpoint {
    match connection {
        Connection::Native(session) => SyncEndpoint {
            transport: Transport::Native(session.opts.clone()),
            session: Arc::downgrade(&session.identity),
            _system_roots: session.system_roots.clone(),
        },
        Connection::Http(session) => SyncEndpoint {
            transport: Transport::Http(Box::new(session.detached())),
            session: Arc::downgrade(&session.identity),
            _system_roots: None,
        },
    }
}

pub(super) fn handle(
    active: &Connection,
    command: Command,
    monitor: Option<&Mutex<OperationMonitor>>,
) -> Result<Response, String> {
    let source = endpoint(active);
    if matches!(command, Command::SyncEndpoint) {
        return Ok(Response::SyncEndpoint(source));
    }
    let monitor = monitor.ok_or("同步任务未初始化。")?;
    monitor.lock().unwrap().check_cancelled()?;
    let checkpoint = |progress: SyncProgress| {
        let finishing = progress.finishing;
        monitor
            .lock()
            .unwrap()
            .update(ProgressUpdate::Sync(progress), finishing)
    };
    match command {
        Command::ExportSync { prepared, paths } => {
            let (target, original_source) = match &prepared {
                SyncScriptPlan::Single(plan) => (&plan.target, &plan.source_session),
                SyncScriptPlan::Group(plan) => (&plan.target, &plan.source_session),
            };
            check_source(&source, original_source)?;
            let _live = verify_target(target)?;
            let files = export_transported(
                &source.transport,
                &target.transport,
                &prepared,
                &paths,
                checkpoint,
            )?;
            Ok(Response::SyncScripts { prepared, files })
        }
        Command::PlanSync {
            source: table,
            target,
            endpoint,
            mode,
        } => plan_one(
            source,
            endpoint,
            SyncMapping::all(table, target),
            mode.into(),
            checkpoint,
        ),
        Command::PlanScopedSync {
            mapping,
            endpoint,
            policy,
        } => plan_one(source, endpoint, mapping, policy, checkpoint),
        Command::PlanSyncGroup {
            pairs,
            endpoint,
            mode,
        } => plan_many(
            source,
            endpoint,
            pairs
                .into_iter()
                .map(|(from, to)| SyncMapping::all(from, to))
                .collect(),
            mode.into(),
            checkpoint,
        ),
        Command::PlanScopedSyncGroup {
            mappings,
            endpoint,
            policy,
        } => plan_many(source, endpoint, mappings, policy, checkpoint),
        Command::ApplySync { prepared } => {
            check_source(&source, &prepared.source_session)?;
            let _live = verify_target(&prepared.target)?;
            apply_transported(
                &source.transport,
                &prepared.target.transport,
                &prepared.plan,
                checkpoint,
            )
            .map(Response::SyncApplied)
        }
        Command::ApplySyncGroup { prepared } => {
            check_source(&source, &prepared.source_session)?;
            let _live = verify_target(&prepared.target)?;
            apply_many_transported(
                &source.transport,
                &prepared.target.transport,
                &prepared.plan,
                checkpoint,
            )
            .map(Response::SyncGroupApplied)
        }
        _ => Err("无法识别同步操作。".into()),
    }
}

fn verify_target(endpoint: &SyncEndpoint) -> Result<Arc<()>, String> {
    endpoint
        .session
        .upgrade()
        .ok_or_else(|| "目标连接已关闭，请重新比较。".into())
}

fn check_source(endpoint: &SyncEndpoint, original: &Weak<()>) -> Result<(), String> {
    if !Weak::ptr_eq(&endpoint.session, original) {
        return Err("源连接已变化，请重新比较。".into());
    }
    Ok(())
}

fn plan_one(
    source: SyncEndpoint,
    target: SyncEndpoint,
    mapping: SyncMapping,
    policy: SyncPolicy,
    progress: impl FnMut(SyncProgress) -> Result<(), String>,
) -> Result<Response, String> {
    let _live = verify_target(&target)?;
    if Weak::ptr_eq(&source.session, &target.session) && mapping.source == mapping.target {
        return Err("源表和目标表须不同。".into());
    }
    let plan = compare_transported(
        &source.transport,
        &target.transport,
        mapping,
        policy,
        progress,
    )?;
    Ok(Response::SyncPlan(Box::new(PreparedSync {
        plan,
        target,
        source_session: source.session,
    })))
}

fn plan_many(
    source: SyncEndpoint,
    target: SyncEndpoint,
    mappings: Vec<SyncMapping>,
    policy: SyncPolicy,
    progress: impl FnMut(SyncProgress) -> Result<(), String>,
) -> Result<Response, String> {
    let _live = verify_target(&target)?;
    if Weak::ptr_eq(&source.session, &target.session)
        && mappings
            .iter()
            .any(|mapping| mapping.source == mapping.target)
    {
        return Err("同一连接不能把表同步到它自身。".into());
    }
    let plan = compare_many_transported(
        &source.transport,
        &target.transport,
        mappings,
        policy,
        progress,
    )?;
    Ok(Response::SyncGroupPlan(Box::new(PreparedSyncGroup {
        plan,
        target,
        source_session: source.session,
    })))
}
