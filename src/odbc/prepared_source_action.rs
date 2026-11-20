use std::sync::atomic::AtomicBool;

use zeroize::Zeroizing;

use super::{
    SourceAction, SourceActionPlan, SourceData, SourceLogin, SourceTable, execute_source_write,
    read_for_source_write,
};

/// A verified source snapshot and its login, consumed by a single import.
pub struct PreparedSourceAction {
    connection: Zeroizing<String>,
    plan: SourceActionPlan,
}

pub fn read_with_source_action(
    connection: String,
    sql: &str,
    indexes_from: Option<&SourceTable>,
    copy_foreign_keys: bool,
    action: &SourceAction,
) -> Result<(SourceData, PreparedSourceAction), String> {
    let connection = Zeroizing::new(connection);
    let cancel = AtomicBool::new(false);
    let (source, plan) = read_for_source_write(
        SourceLogin::ConnectionString(&connection),
        sql,
        indexes_from,
        copy_foreign_keys,
        action,
        &cancel,
    )?;
    Ok((source, PreparedSourceAction { connection, plan }))
}

impl PreparedSourceAction {
    pub(crate) fn plan(&self) -> &SourceActionPlan {
        &self.plan
    }

    pub(crate) fn execute(self, applied: &[bool], cancel: &AtomicBool) -> Result<usize, String> {
        execute_source_write(
            SourceLogin::ConnectionString(&self.connection),
            self.plan,
            applied,
            cancel,
        )
    }
}
