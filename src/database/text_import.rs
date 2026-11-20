use std::sync::atomic::AtomicBool;

use mysql::{Conn, Opts};

use super::{
    Cell, DbObject, OdbcConflict, OdbcImportCounts, OdbcReset, OdbcTarget, check_import_cancelled,
    import_transaction, mysql_error, validate_import_rows,
};

mod local;
pub(super) use local::run as run_local;
pub use local::{
    LoadLocalConflict, LoadLocalFormat, LoadLocalImport, LoadLocalPriority, LoadLocalReport,
    LoadLocalWarning,
};

pub struct TextImport {
    pub object: DbObject,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    pub conflict: OdbcConflict,
    pub expected_ddl: Option<String>,
}

impl TextImport {
    pub(super) fn into_target(self) -> OdbcTarget {
        OdbcTarget {
            object: self.object,
            columns: self.columns,
            rows: self
                .rows
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|value| value.map_or(Cell::Null, Cell::Text))
                        .collect()
                })
                .collect(),
            create: None,
            create_sql: None,
            engine: None,
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            track_applied_rows: false,
            conflict: self.conflict,
            reset: OdbcReset::None,
            expected_ddl: self.expected_ddl,
        }
    }
}

pub(super) fn run(
    opts: &Opts,
    import: TextImport,
    cancel: Option<&AtomicBool>,
) -> Result<OdbcImportCounts, String> {
    check_import_cancelled(cancel)?;
    let target = import.into_target();
    validate_import_rows(&target.object, &target.columns, &target.rows)?;
    if target.rows.is_empty() && cancel.is_none() {
        return Ok(OdbcImportCounts::default());
    }
    let mut conn = Conn::new(opts.clone()).map_err(mysql_error)?;
    import_transaction(&mut conn, vec![target], cancel)
}
