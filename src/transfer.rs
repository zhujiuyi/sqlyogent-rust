#[path = "transfer/copy_definition.rs"]
mod copy_definition;
#[path = "transfer/deferred_copy_keys.rs"]
mod deferred_copy_keys;
#[path = "transfer/dump_output.rs"]
mod dump_output;
#[path = "transfer/result_sql.rs"]
mod result_sql;
#[path = "transfer/table_copy.rs"]
mod table_copy;

use std::borrow::Cow;
#[path = "transfer/tabular_csv.rs"]
mod tabular_csv;
#[path = "transfer/tabular_excel.rs"]
mod tabular_excel;

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, Write};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use mysql::prelude::Queryable;
use mysql::{Conn, Row, params};
use tempfile::NamedTempFile;

use crate::sql_encoding::{DUMP_ENCODING_PREFIX, SqlDumpCharset, parse_marker};
use crate::sql_tools::mysql_text_literal;

use self::dump_output::{DumpOutput, write_dump_output};
use super::http_session::{self, Session as HttpSession};
use super::{
    Cell, DbObject, Failure, MAX_RESULT_ROWS, ObjectDetail, ObjectKind, ResultSet, SchemaOrigin,
    TablePage, cell, column_is_binary, inspect_object, list_objects, mysql_error, quote_identifier,
};

pub(crate) use dump_output::{
    backup_timestamp, publish_split_directory, split_generation_suffix, split_timestamp_prefix,
    validate_backup_directory, write_restore_order,
};
pub use result_sql::{ResultColumn, ResultSqlTarget};
pub use table_copy::TableCopySettings;
pub(super) use table_copy::{copy_table_as, copy_table_as_over_http};
pub use tabular_csv::{CsvOptions, format_result_clipboard};
pub use tabular_excel::{EXCEL_CELL_TEXT_LIMIT, ExcelColumnType, ExcelOptions};

#[derive(Clone, Copy, Debug)]
pub struct DumpOptions {
    pub include_structure: bool,
    pub include_data: bool,
    pub drop_existing: bool,
}

impl Default for DumpOptions {
    fn default() -> Self {
        Self {
            include_structure: true,
            include_data: true,
            drop_existing: false,
        }
    }
}

pub const DEFAULT_BULK_INSERT_KIB: u32 = 256;
pub const MAX_BULK_INSERT_KIB: u32 = 16 * 1024;
pub const DEFAULT_DUMP_CHUNK_ROWS: u32 = 1000;
pub const MAX_HTTP_DUMP_CHUNK_ROWS: u32 = 1000;
pub(super) const DUMP_CANCELLED: &str = "已取消 SQL 转储。";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupDirectoryMode {
    #[default]
    PerRun,
    Timestamped,
    Selected,
}

impl BackupDirectoryMode {
    pub(crate) fn is_per_run(&self) -> bool {
        *self == Self::PerRun
    }
}

#[derive(Clone, Debug)]
pub struct DumpSettings {
    pub options: DumpOptions,
    pub objects: Option<Vec<DbObject>>,
    pub object_kinds: Option<Vec<ObjectKind>>,
    pub lock_source_tables: bool,
    pub lock_source_server: bool,
    pub flush_logs: bool,
    pub single_transaction: bool,
    pub chunk_rows: Option<NonZeroU32>,
    pub include_use: bool,
    pub include_create_database: bool,
    pub disable_foreign_key_checks: bool,
    pub lock_insert_tables: bool,
    pub disable_autocommit: Option<bool>,
    pub bulk_insert: bool,
    pub bulk_insert_limit_kib: Option<NonZeroU32>,
    pub complete_insert: bool,
    pub insert_delayed: bool,
    pub disable_keys: bool,
    pub overwrite: bool,
    pub append: bool,
    pub file_per_object: bool,
    pub directory_mode: BackupDirectoryMode,
    pub split_filename_timestamp: Option<u64>,
    pub continue_on_error: bool,
    pub file_charset: SqlDumpCharset,
}

impl Default for DumpSettings {
    fn default() -> Self {
        Self {
            options: DumpOptions {
                drop_existing: true,
                ..DumpOptions::default()
            },
            objects: None,
            object_kinds: None,
            lock_source_tables: false,
            lock_source_server: false,
            flush_logs: false,
            single_transaction: false,
            chunk_rows: None,
            include_use: false,
            include_create_database: false,
            disable_foreign_key_checks: true,
            lock_insert_tables: false,
            disable_autocommit: None,
            bulk_insert: true,
            bulk_insert_limit_kib: NonZeroU32::new(DEFAULT_BULK_INSERT_KIB),
            complete_insert: true,
            insert_delayed: false,
            disable_keys: false,
            overwrite: false,
            append: false,
            file_per_object: false,
            directory_mode: BackupDirectoryMode::PerRun,
            split_filename_timestamp: None,
            continue_on_error: false,
            file_charset: SqlDumpCharset::default(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct DumpProgress {
    pub completed_objects: usize,
    pub total_objects: usize,
    pub rows: u64,
    pub object: Option<String>,
    pub finishing: bool,
}

#[derive(Clone, Debug, Default)]
pub struct TransferSummary {
    pub objects: usize,
    pub rows: u64,
    pub bytes: u64,
    pub copy_origin: Option<SchemaOrigin>,
    pub copy_dependencies: CopyDependencies,
    pub output_directory: Option<PathBuf>,
    pub skipped_objects: Vec<SkippedDumpObject>,
    pub cleanup_warning: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct CopyDependencies {
    pub external_parents: Vec<DbObject>,
    pub target_tables: Vec<String>,
    pub cyclic_overwrite: bool,
    pub cyclic_data: bool,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SkippedDumpObject {
    pub schema: String,
    pub name: String,
    pub kind: &'static str,
    pub reason: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExportKind {
    Csv,
    Html,
    Xml,
    ExcelXml,
    Sql,
    Json,
}

impl ExportKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Csv => "CSV",
            Self::Html => "HTML 网页",
            Self::Xml => "XML 数据",
            Self::ExcelXml => "Excel 2003 XML",
            Self::Sql => "SQL 语句",
            Self::Json => "JSON",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Html => "html",
            Self::Xml | Self::ExcelXml => "xml",
            Self::Sql => "sql",
            Self::Json => "json",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ExportSettings {
    pub kind: ExportKind,
    pub fields: Option<Vec<String>>,
    pub csv: CsvOptions,
    pub excel: ExcelOptions,
    pub sql: SqlExportMode,
    pub result_target: Option<ResultSqlTarget>,
}

impl Default for ExportSettings {
    fn default() -> Self {
        Self {
            kind: ExportKind::Csv,
            fields: None,
            csv: CsvOptions::default(),
            excel: ExcelOptions::default(),
            sql: SqlExportMode::default(),
            result_target: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SqlExportMode {
    #[default]
    Data,
    Structure,
    StructureAndData,
}

// Use a dedicated connection. The source session negotiates UTF-8 result text and UTC.
pub fn dump(
    conn: &mut Conn,
    schema: &str,
    object: Option<&DbObject>,
    path: &Path,
    options: DumpOptions,
) -> Result<TransferSummary, String> {
    let settings = DumpSettings {
        options,
        bulk_insert: false,
        ..DumpSettings::default()
    };
    dump_to(conn, schema, object, path, settings, true)
}

pub fn dump_with_limits(
    conn: &mut Conn,
    schema: &str,
    object: Option<&DbObject>,
    path: &Path,
    options: DumpOptions,
    chunk_rows: Option<NonZeroU32>,
    bulk_insert_limit_kib: Option<NonZeroU32>,
) -> Result<TransferSummary, String> {
    let settings = DumpSettings {
        options,
        chunk_rows,
        bulk_insert_limit_kib,
        ..DumpSettings::default()
    };
    dump_to(conn, schema, object, path, settings, false)
}

pub(super) fn copy_dump_settings(
    schema: &str,
    destination_schema: &str,
    object: Option<DbObject>,
    objects: Option<Vec<DbObject>>,
    options: DumpOptions,
    bulk_insert: bool,
) -> Result<DumpSettings, String> {
    if destination_schema.is_empty() {
        return Err("请选择目标数据库。".into());
    }
    validate_dump_mode(options)?;
    if options.drop_existing && !options.include_structure {
        return Err("替换目标端对象时必须复制结构。".into());
    }
    let create_database = object.is_none() && options.include_structure;
    let objects = match (object, objects) {
        (Some(_), Some(_)) => return Err("复制任务只能指定一个对象范围。".into()),
        (Some(object), None) => Some(vec![object]),
        (None, objects) => objects,
    };
    if let Some(objects) = &objects {
        if objects.is_empty() {
            return Err("请选择要复制的对象。".into());
        }
        for object in objects {
            if object.schema != schema {
                return Err("选择的对象不属于源数据库。".into());
            }
            if !options.include_structure && object.kind != ObjectKind::Table {
                return Err("仅复制数据时请选择基表。".into());
            }
        }
    }
    Ok(DumpSettings {
        include_use: true,
        include_create_database: create_database,
        disable_foreign_key_checks: objects.is_none(),
        options,
        objects,
        bulk_insert,
        overwrite: true,
        ..DumpSettings::default()
    })
}

pub(super) fn dump_for_copy(
    conn: &mut Conn,
    schema: &str,
    destination_schema: &str,
    path: &Path,
    mut settings: DumpSettings,
    mut progress: impl FnMut(DumpProgress) -> Result<(), String>,
) -> Result<TransferSummary, String> {
    settings.single_transaction = true;
    write_dump_output(path, &settings, |out| {
        let (summary, finishing) = with_snapshot(conn, schema, |conn| {
            let (objects, _) = selected_objects(conn, schema, &settings)?;
            let mut dump = DumpWriter::new(out, &settings, &mut progress, &objects, true);
            dump.report(None)?;
            let summary = dump.write(
                &mut DumpSource::Native(conn),
                schema,
                &objects,
                Some(destination_schema),
                true,
            )?;
            Ok((summary, dump.finishing_status()))
        })?;
        progress(finishing)?;
        Ok(summary)
    })
}

fn dump_to(
    conn: &mut Conn,
    schema: &str,
    object: Option<&DbObject>,
    path: &Path,
    mut settings: DumpSettings,
    require_innodb: bool,
) -> Result<TransferSummary, String> {
    let options = settings.options;
    validate_dump_mode(options)?;
    if options.drop_existing && !options.include_structure {
        return Err("替换已有对象时必须导出结构。".into());
    }
    if let Some(object) = object {
        if object.schema != schema {
            return Err("选择的对象不属于此数据库。".into());
        }
        if object.kind != ObjectKind::Table && !options.include_structure {
            return Err("仅数据 SQL 转储只能还原到基表。".into());
        }
    }
    settings.include_use = object.is_none();
    settings.include_create_database = object.is_none() && options.include_structure;
    settings.single_transaction = true;
    atomic_output(path, ExistingOutput::Replace, |out| {
        with_snapshot(conn, schema, |conn| {
            let objects = match object {
                Some(object) => vec![object.clone()],
                None => list_objects(conn, schema).map_err(Failure::message)?,
            };
            let mut no_progress = |_| Ok(());
            let mut dump = DumpWriter::new(out, &settings, &mut no_progress, &objects, false);
            dump.write(
                &mut DumpSource::Native(conn),
                schema,
                &objects,
                None,
                require_innodb,
            )
        })
    })
}

// Use a dedicated connection for source locks and session settings.
pub fn dump_configured(
    conn: &mut Conn,
    schema: &str,
    path: &Path,
    settings: DumpSettings,
    progress: impl FnMut(DumpProgress) -> Result<(), String>,
) -> Result<TransferSummary, String> {
    dump_configured_with_lock_tag(conn, schema, path, settings, "", progress)
}

pub(super) fn dump_configured_with_lock_tag(
    conn: &mut Conn,
    schema: &str,
    path: &Path,
    mut settings: DumpSettings,
    lock_tag: &str,
    mut progress: impl FnMut(DumpProgress) -> Result<(), String>,
) -> Result<TransferSummary, String> {
    validate_dump_mode(settings.options)?;
    if settings.lock_source_tables && settings.single_transaction {
        return Err("锁定源表与单一事务不能同时使用。".into());
    }
    if settings.lock_source_server && settings.lock_source_tables {
        return Err("全局读锁与逐表读锁不能同时使用。".into());
    }
    settings.options.drop_existing &= settings.options.include_structure;
    write_dump_output(path, &settings, |out| {
        prepare_source(conn, schema)?;
        conn.query_drop("SET SESSION autocommit = 1")
            .map_err(mysql_error)?;
        if settings.lock_source_server {
            progress(DumpProgress::default())?;
        }
        let mut run = |conn: &mut Conn| {
            if settings.lock_source_server {
                progress(DumpProgress::default())?;
            }
            let (objects, missing) = selected_objects(conn, schema, &settings)?;
            let mut dump = DumpWriter::new(out, &settings, &mut progress, &objects, false);
            dump.record_missing(&missing)?;
            dump.report(None)?;
            if settings.flush_logs {
                conn.query_drop("FLUSH LOGS")
                    .map_err(|error| format!("刷新源库日志失败：{}", mysql_error(error)))?;
            }
            let mut write = |conn: &mut Conn| {
                dump.write(&mut DumpSource::Native(conn), schema, &objects, None, false)
            };
            let summary = if settings.single_transaction {
                in_snapshot(conn, write)?
            } else {
                write(conn)?
            };
            Ok((summary, dump.finishing_status()))
        };
        let (summary, status) = if settings.lock_source_server {
            with_global_read_lock(conn, lock_tag, run)?
        } else {
            run(conn)?
        };
        progress(status)?;
        Ok(summary)
    })
}

fn validate_dump_mode(options: DumpOptions) -> Result<(), String> {
    if options.include_structure || options.include_data {
        Ok(())
    } else {
        Err("请选择结构、数据或两者。".into())
    }
}

pub(super) fn dump_over_http(
    session: &HttpSession,
    schema: &str,
    path: &Path,
    mut settings: DumpSettings,
    progress: impl FnMut(DumpProgress) -> Result<(), String>,
) -> Result<TransferSummary, String> {
    let maximum = NonZeroU32::new(MAX_HTTP_DUMP_CHUNK_ROWS).unwrap();
    settings.chunk_rows = Some(
        settings
            .chunk_rows
            .map_or(maximum, |rows| rows.min(maximum)),
    );
    dump_over_http_to(session, schema, None, path, settings, progress)
}

pub(super) fn dump_for_copy_over_http(
    session: &HttpSession,
    schema: &str,
    destination_schema: &str,
    path: &Path,
    settings: DumpSettings,
    progress: impl FnMut(DumpProgress) -> Result<(), String>,
) -> Result<TransferSummary, String> {
    dump_over_http_to(
        session,
        schema,
        Some(destination_schema),
        path,
        settings,
        progress,
    )
}

fn dump_over_http_to(
    session: &HttpSession,
    schema: &str,
    destination_schema: Option<&str>,
    path: &Path,
    mut settings: DumpSettings,
    mut progress: impl FnMut(DumpProgress) -> Result<(), String>,
) -> Result<TransferSummary, String> {
    validate_dump_mode(settings.options)?;
    if settings.single_transaction || settings.lock_source_tables || settings.lock_source_server {
        return Err(
            "HTTP 隧道无法让整个转储共享事务或读锁。请选择分批读取，或使用直连或 SSH 隧道。".into(),
        );
    }
    settings.options.drop_existing &= settings.options.include_structure;
    if schema.is_empty() {
        return Err("请先选择要备份的数据库。".into());
    }
    let (objects, missing) = validate_object_selection(session.objects(schema)?, &settings)?;
    write_dump_output(path, &settings, |out| {
        let mut dump = DumpWriter::new(
            out,
            &settings,
            &mut progress,
            &objects,
            destination_schema.is_some(),
        );
        dump.record_missing(&missing)?;
        dump.report(None)?;
        if settings.flush_logs {
            session.read_set("FLUSH LOGS")?;
        }
        let summary = dump.write(
            &mut DumpSource::Http(session),
            schema,
            &objects,
            destination_schema,
            false,
        )?;
        let status = dump.finishing_status();
        (dump.progress)(status)?;
        Ok(summary)
    })
}

#[derive(Clone, Copy)]
struct DumpDefinitionContext<'a> {
    source_sql_mode: &'a str,
    source_sql_mode_literal: &'a str,
    case_mode: u8,
    destination_schema: Option<&'a str>,
    deferred_targets: Option<&'a HashSet<String>>,
}

impl<'a> DumpDefinitionContext<'a> {
    fn renamed_schema(self, source_schema: &str) -> Option<&'a str> {
        self.destination_schema
            .filter(|destination| *destination != source_schema)
    }
}

fn selected_objects(
    conn: &mut Conn,
    schema: &str,
    settings: &DumpSettings,
) -> Result<(Vec<DbObject>, Vec<DbObject>), String> {
    let visible = list_objects(conn, schema).map_err(Failure::message)?;
    validate_object_selection(visible, settings)
}

fn validate_object_selection(
    mut visible: Vec<DbObject>,
    settings: &DumpSettings,
) -> Result<(Vec<DbObject>, Vec<DbObject>), String> {
    let mut missing = Vec::new();
    if settings.objects.is_some() || settings.object_kinds.is_some() {
        let selected = settings.objects.as_deref().unwrap_or_default();
        let kinds = settings.object_kinds.as_deref().unwrap_or_default();
        let mut remaining: HashSet<_> = selected.iter().collect();
        visible.retain(|object| remaining.remove(object) || kinds.contains(&object.kind));
        if !settings.continue_on_error
            && let Some(missing) = remaining.iter().next()
        {
            return Err(format!(
                "无法找到所选对象 {}，请刷新对象列表。",
                missing.qualified_name()
            ));
        }
        missing.extend(
            selected
                .iter()
                .filter(|object| remaining.remove(object))
                .cloned(),
        );
    }
    if !settings.options.include_structure {
        visible.retain(|object| object.kind == ObjectKind::Table);
        missing.retain(|object| object.kind == ObjectKind::Table);
    }
    if visible.is_empty() && missing.is_empty() && !settings.include_create_database {
        return Err("请选择要导出的对象。".into());
    }
    Ok((visible, missing))
}

enum DumpSource<'a> {
    Native(&'a mut Conn),
    Http(&'a HttpSession),
}

impl DumpSource<'_> {
    fn sql_mode(&mut self) -> Result<(String, u8), String> {
        match self {
            Self::Native(conn) => conn
                .query_first_opt::<(String, u8), _>(
                    "SELECT @@SESSION.sql_mode, @@lower_case_table_names",
                )
                .map_err(mysql_error)?
                .transpose()
                .map_err(|error| mysql_error(error.into()))?
                .ok_or("服务器未返回 SQL 模式。".into()),
            Self::Http(session) => session
                .snapshot_context()
                .map(|context| (context.sql_mode, context.lower_case_table_names)),
        }
    }

    fn max_packet(&mut self) -> Result<usize, String> {
        const SQL: &str = "SELECT @@SESSION.max_allowed_packet";
        match self {
            Self::Native(conn) => conn
                .query_first_opt::<usize, _>(SQL)
                .map_err(mysql_error)?
                .transpose()
                .map_err(|error| mysql_error(error.into()))?
                .ok_or("服务器未返回最大通信包大小。".into()),
            Self::Http(session) => session
                .metadata_columns::<1>(SQL, 1)
                .map_err(Failure::message)?
                .first()
                .and_then(|row| row[0].as_ref())
                .and_then(|value| value.parse().ok())
                .ok_or("服务器未返回最大通信包大小。".into()),
        }
    }

    fn create_database(&mut self, schema: &str, destination: &str) -> Result<String, String> {
        if schema != destination {
            return relocated_database_ddl(self, schema, destination);
        }
        let query = format!(
            "SHOW CREATE DATABASE IF NOT EXISTS {}",
            quote_identifier(schema)
        );
        match self {
            Self::Native(conn) => {
                required_text(&query_create_row(conn, &query)?, "Create Database")
            }
            Self::Http(session) => {
                let set = session.read_set(&query)?;
                let index = http_session::column(&set, "Create Database")
                    .ok_or("服务器未返回建库定义。")?;
                let ddl = set
                    .rows
                    .first()
                    .and_then(|row| http_session::text(row.get(index)))
                    .ok_or("服务器未返回建库定义。")?;
                Ok(ddl.to_owned())
            }
        }
    }

    fn definition(&mut self, object: &DbObject) -> Result<ObjectDefinition, String> {
        match self {
            Self::Native(conn) => read_definition(conn, object),
            Self::Http(session) => {
                let (ddl, context) = session.definition_with_context(object)?;
                Ok(ObjectDefinition {
                    ddl,
                    sql_mode: context.sql_mode,
                    collation: context
                        .collation_connection
                        .as_deref()
                        .map(setting_literal)
                        .transpose()?,
                    time_zone: context.time_zone,
                })
            }
        }
    }

    fn table_dependencies(
        &mut self,
        schema: &str,
        case_mode: u8,
        include_self_references: bool,
    ) -> Result<Vec<(String, String)>, String> {
        match self {
            Self::Native(conn) => {
                table_dependencies(conn, schema, case_mode, include_self_references)
            }
            Self::Http(session) => {
                let normalized = comparable_name(schema, case_mode);
                Ok(session
                    .foreign_keys(schema, false)
                    .map_err(Failure::message)?
                    .into_iter()
                    .filter(|key| {
                        comparable_name(&key.referenced_table.schema, case_mode) == normalized
                    })
                    .filter_map(|key| {
                        let from = comparable_name(&key.table.name, case_mode);
                        let to = comparable_name(&key.referenced_table.name, case_mode);
                        (include_self_references || from != to).then_some((from, to))
                    })
                    .collect())
            }
        }
    }

    fn external_copy_parents(
        &mut self,
        schema: &str,
        tables: &[&DbObject],
        case_mode: u8,
    ) -> Result<Vec<DbObject>, String> {
        const SQL: &str = "SELECT DISTINCT BINARY TABLE_NAME, BINARY REFERENCED_TABLE_SCHEMA,
                   BINARY REFERENCED_TABLE_NAME
            FROM information_schema.KEY_COLUMN_USAGE
            WHERE TABLE_SCHEMA = :schema
                AND BINARY IF(@@lower_case_table_names = 0, TABLE_SCHEMA, LOWER(TABLE_SCHEMA))
                    = BINARY IF(@@lower_case_table_names = 0, :schema, LOWER(:schema))
                AND REFERENCED_TABLE_NAME IS NOT NULL
                AND BINARY IF(@@lower_case_table_names = 0, REFERENCED_TABLE_SCHEMA, LOWER(REFERENCED_TABLE_SCHEMA))
                    <> BINARY IF(@@lower_case_table_names = 0, :schema, LOWER(:schema))";
        let rows: Vec<(String, String, String)> = match self {
            Self::Native(conn) => conn
                .exec_opt(SQL, params! { "schema" => schema })
                .map_err(mysql_error)?
                .into_iter()
                .collect::<Result<_, _>>()
                .map_err(|error| mysql_error(error.into()))?,
            Self::Http(session) => session
                .metadata_columns::<3>(
                    &SQL.replace(":schema", &mysql_text_literal(schema)),
                    MAX_RESULT_ROWS,
                )
                .map_err(Failure::message)?
                .into_iter()
                .map(|row| match row {
                    [Some(table), Some(parent_schema), Some(parent_name)] => {
                        Ok((table, parent_schema, parent_name))
                    }
                    _ => Err("隧道返回的外部外键元数据不完整。"),
                })
                .collect::<Result<_, _>>()?,
        };
        let table_names: HashSet<_> = tables
            .iter()
            .map(|table| comparable_name(&table.name, case_mode))
            .collect();
        let mut parents = Vec::new();
        let mut seen = HashSet::new();
        for (table, parent_schema, parent_name) in rows {
            if !table_names.contains(&comparable_name(&table, case_mode)) {
                continue;
            }
            let key = (
                comparable_name(&parent_schema, case_mode),
                comparable_name(&parent_name, case_mode),
            );
            let parent = DbObject {
                schema: parent_schema,
                name: parent_name,
                kind: ObjectKind::Table,
            };
            if seen.insert(key) {
                parents.push(parent);
            }
        }
        parents.sort_by(|left, right| {
            left.schema
                .cmp(&right.schema)
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(parents)
    }

    fn ordered_triggers<'a>(
        &mut self,
        schema: &str,
        objects: &'a [DbObject],
    ) -> Result<Vec<&'a DbObject>, String> {
        let session = match self {
            Self::Native(conn) => return ordered_triggers(conn, schema, objects),
            Self::Http(session) => session,
        };
        let mut triggers = objects
            .iter()
            .filter(|object| object.kind == ObjectKind::Trigger)
            .collect::<Vec<_>>();
        if triggers.len() > 1 {
            let sql = TRIGGER_ORDER.replace(":schema", &mysql_text_literal(schema));
            let ranks = session
                .metadata_columns::<2>(&sql, 10_000)
                .map_err(Failure::message)?
                .into_iter()
                .map(|row| match row {
                    [Some(name), Some(order)] => order
                        .parse::<u32>()
                        .map(|order| (name, order))
                        .map_err(|_| "服务器返回的触发器执行顺序无效。"),
                    _ => Err("服务器没有返回完整的触发器执行顺序。"),
                })
                .collect::<Result<HashMap<_, _>, _>>()?;
            triggers.sort_by_key(|trigger| ranks.get(&trigger.name).copied().unwrap_or_default());
        }
        Ok(triggers)
    }

    fn trigger_tables(&mut self, schema: &str) -> Result<HashMap<String, String>, String> {
        const SQL: &str = "SELECT TRIGGER_NAME, EVENT_OBJECT_TABLE FROM information_schema.TRIGGERS
            WHERE TRIGGER_SCHEMA = :schema AND EVENT_OBJECT_SCHEMA = :schema";
        match self {
            Self::Native(conn) => conn
                .exec_opt::<(String, String), _, _>(SQL, params! { "schema" => schema })
                .map_err(mysql_error)?
                .into_iter()
                .collect::<Result<_, _>>()
                .map_err(|error| mysql_error(error.into())),
            Self::Http(session) => session
                .metadata_columns::<2>(&SQL.replace(":schema", &mysql_text_literal(schema)), 10_000)
                .map_err(Failure::message)?
                .into_iter()
                .map(|row| match row {
                    [Some(name), Some(table)] => Ok((name, table)),
                    _ => Err("服务器未返回触发器所属的表。".into()),
                })
                .collect(),
        }
    }

    fn write_view_stub(
        &mut self,
        out: &mut impl Write,
        object: &DbObject,
        drop_existing: bool,
    ) -> Result<(), String> {
        match self {
            Self::Native(conn) => write_view_stub(conn, out, object, drop_existing),
            Self::Http(session) => {
                let columns = session
                    .inspect(object.clone())?
                    .columns
                    .into_iter()
                    .map(|column| format!("1 AS {}", quote_identifier(&column.name)))
                    .collect::<Vec<_>>();
                write_view_stub_definition(out, object, drop_existing, &columns)
            }
        }
    }

    fn dump_rows(
        &mut self,
        out: &mut impl Write,
        table: &DbObject,
        require_innodb: bool,
        insert: InsertOutput<'_>,
        progress: impl FnMut(u64) -> Result<(), String>,
    ) -> Result<u64, String> {
        match self {
            Self::Native(conn) => dump_rows(conn, out, table, require_innodb, insert, progress),
            Self::Http(session) => dump_http_rows(session, out, table, insert, progress),
        }
    }
}

fn prepare_source(conn: &mut Conn, schema: &str) -> Result<(), String> {
    conn.select_db(schema).map_err(mysql_error)?;
    for sql in [
        "SET NAMES utf8mb4",
        "SET SESSION time_zone = '+00:00'",
        "SET SESSION sql_quote_show_create = 1",
    ] {
        conn.query_drop(sql).map_err(mysql_error)?;
    }
    Ok(())
}

fn with_snapshot<T>(
    conn: &mut Conn,
    schema: &str,
    run: impl FnOnce(&mut Conn) -> Result<T, String>,
) -> Result<T, String> {
    prepare_source(conn, schema)?;
    in_snapshot(conn, run)
}

fn in_snapshot<T>(
    conn: &mut Conn,
    run: impl FnOnce(&mut Conn) -> Result<T, String>,
) -> Result<T, String> {
    for sql in [
        "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
        "START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY",
    ] {
        conn.query_drop(sql).map_err(mysql_error)?;
    }
    let result = run(conn);
    after_cleanup(result, conn.query_drop("ROLLBACK").map_err(mysql_error))
}

fn with_read_lock<T>(
    conn: &mut Conn,
    table: &DbObject,
    run: impl FnOnce(&mut Conn) -> Result<T, String>,
) -> Result<T, String> {
    // InnoDB uses its internal table lock with autocommit disabled. START TRANSACTION
    // would implicitly release the server-level LOCK TABLES lock.
    conn.query_drop("SET SESSION autocommit = 0")
        .map_err(mysql_error)?;
    let name = table.qualified_name();
    let result = conn
        .query_drop(format!("LOCK TABLES {name} READ"))
        .map_err(|error| format!("无法锁定源表 {name}：{}", mysql_error(error)))
        .and_then(|()| {
            let result = run(conn);
            let rollback = conn.query_drop("ROLLBACK").map_err(mysql_error);
            let unlock = conn.query_drop("UNLOCK TABLES").map_err(mysql_error);
            after_cleanup(after_cleanup(result, rollback), unlock)
        });
    after_cleanup(
        result,
        conn.query_drop("SET SESSION autocommit = 1")
            .map_err(mysql_error),
    )
}

fn with_global_read_lock<T>(
    conn: &mut Conn,
    tag: &str,
    run: impl FnOnce(&mut Conn) -> Result<T, String>,
) -> Result<T, String> {
    conn.query_drop(format!("{tag}FLUSH TABLES WITH READ LOCK"))
        .map_err(|error| format!("无法取得服务器全局读锁。{}", mysql_error(error)))?;
    let result = run(conn);
    after_cleanup(
        result,
        conn.query_drop("UNLOCK TABLES").map_err(mysql_error),
    )
}

fn after_cleanup<T>(result: Result<T, String>, cleanup: Result<(), String>) -> Result<T, String> {
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup)) => Err(format!("{error}\n清理导出会话失败：{cleanup}")),
    }
}

struct DumpWriter<'a, F> {
    out: &'a mut dyn DumpOutput,
    settings: &'a DumpSettings,
    progress: &'a mut F,
    summary: TransferSummary,
    total_objects: usize,
    foreign_key_checks: Option<bool>,
}

impl<'a, F: FnMut(DumpProgress) -> Result<(), String>> DumpWriter<'a, F> {
    fn new(
        out: &'a mut dyn DumpOutput,
        settings: &'a DumpSettings,
        progress: &'a mut F,
        objects: &[DbObject],
        copying: bool,
    ) -> Self {
        Self {
            out,
            settings,
            progress,
            summary: TransferSummary::default(),
            foreign_key_checks: (copying || settings.disable_foreign_key_checks)
                .then_some(!settings.disable_foreign_key_checks),
            total_objects: objects
                .iter()
                .filter(|object| {
                    settings.options.include_structure || object.kind == ObjectKind::Table
                })
                .count(),
        }
    }

    fn status(&self, object: Option<&str>) -> DumpProgress {
        DumpProgress {
            completed_objects: self.summary.objects + self.summary.skipped_objects.len(),
            total_objects: self.total_objects,
            rows: self.summary.rows,
            object: object.map(str::to_owned),
            finishing: false,
        }
    }

    fn finishing_status(&self) -> DumpProgress {
        DumpProgress {
            finishing: true,
            ..self.status(None)
        }
    }

    fn report(&mut self, object: Option<&str>) -> Result<(), String> {
        let status = self.status(object);
        (self.progress)(status)
    }

    fn complete(&mut self, object: &DbObject) -> Result<(), String> {
        self.summary.objects += 1;
        self.report(Some(&object.name))
    }

    fn skip(&mut self, object: &DbObject, reason: &'static str) -> Result<(), String> {
        self.summary.skipped_objects.push(SkippedDumpObject {
            schema: object.schema.clone(),
            name: object.name.clone(),
            kind: object.kind.show_create().0,
            reason,
        });
        self.report(Some(&object.name))
    }

    fn record_missing(&mut self, missing: &[DbObject]) -> Result<(), String> {
        self.total_objects += missing.len();
        for object in missing {
            self.skip(object, "无法找到待备份的对象")?;
        }
        Ok(())
    }

    fn attempt<T>(
        &mut self,
        source: &mut DumpSource<'_>,
        run: impl FnOnce(&mut Self, &mut DumpSource<'_>) -> Result<T, String>,
    ) -> Result<Option<T>, String> {
        if !self.settings.continue_on_error {
            return run(self, source).map(Some);
        }
        let mark = self.out.checkpoint().map_err(output_error)?;
        let (objects, rows) = (self.summary.objects, self.summary.rows);
        match run(self, source) {
            Ok(output) => Ok(Some(output)),
            Err(error)
                if error == DUMP_CANCELLED
                    || error.starts_with("写入导出文件失败：")
                    || error.contains("清理导出会话失败：")
                    || error.starts_with("ERROR 1205")
                    || error.starts_with("ERROR 1213") =>
            {
                Err(error)
            }
            Err(_) => {
                if let DumpSource::Native(conn) = source {
                    conn.ping().map_err(mysql_error)?;
                }
                self.out.rewind(mark).map_err(output_error)?;
                self.summary.objects = objects;
                self.summary.rows = rows;
                Ok(None)
            }
        }
    }

    fn depends_on_skipped(
        &self,
        object: &DbObject,
        dependencies: &[(String, String)],
        case_mode: u8,
    ) -> bool {
        let name = comparable_name(&object.name, case_mode);
        dependencies.iter().any(|(dependent, required)| {
            dependent == &name
                && self.summary.skipped_objects.iter().any(|skipped| {
                    matches!(skipped.kind, "TABLE" | "VIEW")
                        && comparable_name(&skipped.name, case_mode) == *required
                })
        })
    }

    fn write(
        &mut self,
        source: &mut DumpSource<'_>,
        schema: &str,
        objects: &[DbObject],
        destination_schema: Option<&str>,
        require_innodb: bool,
    ) -> Result<TransferSummary, String> {
        let options = self.settings.options;
        let copying = destination_schema.is_some();
        let copying_structure = copying && options.include_structure;
        let destination = destination_schema.unwrap_or(schema);

        let (source_mode, case_mode) = source.sql_mode()?;
        let source_mode_literal = setting_literal(&source_mode)?;

        let tables: Vec<_> = objects
            .iter()
            .filter(|object| object.kind == ObjectKind::Table)
            .collect();
        if copying_structure && !tables.is_empty() {
            self.summary.copy_dependencies.external_parents =
                source.external_copy_parents(schema, &tables, case_mode)?;
        }
        let has_external_parents = !self.summary.copy_dependencies.external_parents.is_empty();
        let batch_limit = if self.settings.bulk_insert && options.include_data && !tables.is_empty()
        {
            // Keep space for the protocol header when the server packet size is the lower limit.
            let packet_limit = source.max_packet()?.saturating_sub(16);
            self.settings
                .bulk_insert_limit_kib
                .map_or(packet_limit, |limit| {
                    packet_limit.min(limit.get().min(MAX_BULK_INSERT_KIB) as usize * 1024)
                })
        } else {
            0
        };
        let order_tables = copying_structure
            || self.foreign_key_checks != Some(false)
            || self.settings.continue_on_error;
        let plan_tables = !tables.is_empty() && (copying || order_tables);
        let dependencies = if plan_tables {
            source.table_dependencies(schema, case_mode, options.include_data)?
        } else {
            Vec::new()
        };
        let (tables, has_cycle) = if plan_tables {
            let names: Vec<_> = tables
                .iter()
                .map(|table| comparable_name(&table.name, case_mode))
                .collect();
            let (order, sorted_count) = dependency_order(&names, dependencies.clone());
            let has_cycle = sorted_count != tables.len();
            let tables = if order_tables {
                order.into_iter().map(|index| tables[index]).collect()
            } else {
                tables
            };
            (tables, has_cycle)
        } else {
            (tables, false)
        };
        let cyclic_data = has_cycle && copying && !options.include_structure;
        let defer_keys = has_cycle && copying_structure;
        let deferred_targets = defer_keys.then(|| {
            tables
                .iter()
                .map(|table| comparable_name(&table.name, case_mode))
                .collect()
        });
        let cyclic_overwrite = defer_keys && options.drop_existing;
        self.summary.copy_dependencies.cyclic_overwrite = cyclic_overwrite;
        self.summary.copy_dependencies.cyclic_data = cyclic_data;
        if has_external_parents || cyclic_overwrite || cyclic_data {
            self.summary.copy_dependencies.target_tables =
                tables.iter().map(|table| table.name.clone()).collect();
        }
        if cyclic_data {
            self.foreign_key_checks = None;
        } else if has_external_parents || defer_keys {
            self.foreign_key_checks = Some(true);
        }
        let insert = InsertOutput {
            lock_tables: !cyclic_data && self.settings.lock_insert_tables,
            // The target owns the transaction for all selected tables.
            disable_autocommit: if cyclic_data {
                Some(false)
            } else {
                self.settings.disable_autocommit
            },
            batch_limit,
            complete: self.settings.complete_insert,
            delayed: self.settings.insert_delayed
                && options.include_structure
                && !self.settings.lock_insert_tables
                && !self.settings.disable_keys
                && self.settings.disable_autocommit != Some(true),
            disable_keys: !cyclic_data && self.settings.disable_keys,
            chunk_rows: self.settings.chunk_rows,
            fields: None,
        };
        let definition_context = DumpDefinitionContext {
            source_sql_mode: &source_mode,
            source_sql_mode_literal: &source_mode_literal,
            case_mode,
            destination_schema,
            deferred_targets: deferred_targets.as_ref(),
        };
        let header = self.dump_header(source, schema, destination, &source_mode_literal)?;
        let per_object = self.out.per_object();
        if !per_object {
            self.out.write_all(&header).map_err(output_error)?;
        } else if objects.is_empty() {
            self.open_part("database", &header)?;
            self.close_part()?;
        }
        let drop_first =
            self.foreign_key_checks != Some(false) && !tables.is_empty() && options.drop_existing;
        let warn_cycle = has_cycle && !defer_keys && !cyclic_data;
        if !self.settings.continue_on_error && (warn_cycle || drop_first) {
            self.open_part("prepare", &header)?;
            if warn_cycle && dump_supports_chinese_notes(self.settings.file_charset) {
                writeln!(
                    self.out,
                    "-- 存在循环外键引用。还原时请确认目标库可按脚本顺序写入。"
                )
                .map_err(output_error)?;
            }
            if drop_first {
                for table in tables.iter().rev() {
                    write_drop(&mut self.out, table)?;
                }
            }
            self.close_part()?;
        }
        let groups = if self.settings.continue_on_error && (drop_first || has_cycle) {
            dependency_groups(&tables, &dependencies, case_mode)
        } else {
            tables.into_iter().map(|table| vec![table]).collect()
        };
        let mut deferred = Vec::new();
        for group in groups {
            if self.settings.continue_on_error
                && group
                    .iter()
                    .any(|table| self.depends_on_skipped(table, &dependencies, case_mode))
            {
                for table in group {
                    self.skip(table, "所依赖的表已跳过")?;
                }
                continue;
            }
            let prepare_group = self.settings.continue_on_error && drop_first;
            let written = self.attempt(source, |writer, source| {
                if prepare_group {
                    writer.open_part("prepare", &header)?;
                    for table in group.iter().rev() {
                        write_drop(&mut writer.out, table)?;
                    }
                    writer.close_part()?;
                }
                let mut group_keys = Vec::new();
                for table in &group {
                    group_keys.extend(writer.write_table(
                        source,
                        table,
                        &header,
                        definition_context,
                        require_innodb,
                        insert,
                        options.drop_existing && !drop_first,
                    )?);
                }
                Ok(group_keys)
            })?;
            if let Some(group_keys) = written {
                deferred.extend(group_keys);
            } else {
                for table in group {
                    self.skip(table, "未能完整导出该表或其关联表")?;
                }
            }
        }
        if defer_keys && deferred.is_empty() {
            return Err("未能从建表语句提取已检测到的循环外键。请刷新源对象后重试。".into());
        }
        if !deferred.is_empty() {
            self.open_part("foreign-keys", &header)?;
            for keys in deferred {
                self.report(Some(&keys.table))?;
                keys.write(&mut self.out)?;
            }
            self.close_part()?;
        }

        if options.include_structure {
            for object in objects.iter().filter(|object| {
                matches!(object.kind, ObjectKind::Function | ObjectKind::Procedure)
            }) {
                self.write_structure(source, object, definition_context, &header)?;
            }
            self.write_views(source, schema, objects, definition_context, &header)?;
            self.write_triggers(source, schema, objects, definition_context, &header)?;
            for event in objects
                .iter()
                .filter(|object| object.kind == ObjectKind::Event)
            {
                self.write_structure(source, event, definition_context, &header)?;
            }
        }

        if !per_object {
            self.write_footer()?;
        }
        if self.summary.objects == 0 && !self.summary.skipped_objects.is_empty() {
            return Err(format!(
                "全部 {} 个待备份对象均已跳过，未生成备份。",
                self.summary.skipped_objects.len()
            ));
        }
        Ok(self.summary.clone())
    }

    fn write_table(
        &mut self,
        source: &mut DumpSource<'_>,
        table: &DbObject,
        header: &[u8],
        context: DumpDefinitionContext<'_>,
        require_innodb: bool,
        insert: InsertOutput<'_>,
        drop_existing: bool,
    ) -> Result<Option<DeferredTableKeys>, String> {
        self.report(Some(&table.name))?;
        self.open_object(table, header)?;
        let options = self.settings.options;
        let lock_source = self.settings.lock_source_tables && options.include_data;
        let mut write = |source: &mut DumpSource<'_>| {
            let keys = if options.include_structure {
                write_object(source, &mut self.out, table, context, drop_existing)?
            } else {
                None
            };
            if !options.include_data {
                return Ok((0, keys));
            }
            let status = self.status(Some(&table.name));
            let rows = source.dump_rows(&mut self.out, table, require_innodb, insert, |rows| {
                (self.progress)(DumpProgress {
                    rows: status.rows + rows,
                    ..status.clone()
                })
            })?;
            Ok((rows, keys))
        };
        let (rows, keys) = match source {
            DumpSource::Native(conn) if lock_source => {
                with_read_lock(conn, table, |conn| write(&mut DumpSource::Native(conn)))?
            }
            _ => write(source)?,
        };
        self.close_part()?;
        self.summary.rows += rows;
        self.complete(table)?;
        Ok(keys)
    }

    fn open_part(&mut self, name: &str, header: &[u8]) -> Result<(), String> {
        if self.out.per_object() {
            self.out.open_part(name).map_err(output_error)?;
            self.out.write_all(header).map_err(output_error)?;
        }
        Ok(())
    }

    fn open_object(&mut self, object: &DbObject, header: &[u8]) -> Result<(), String> {
        if self.out.per_object() {
            let (kind, _) = object.kind.show_create();
            self.open_part(
                &format!("{}-{}", kind.to_ascii_lowercase(), object.name),
                header,
            )?;
        }
        Ok(())
    }

    fn close_part(&mut self) -> Result<(), String> {
        if self.out.per_object() {
            self.write_footer()?;
            self.out.close_part().map_err(output_error)?;
        }
        Ok(())
    }

    fn write_footer(&mut self) -> Result<(), String> {
        if self.foreign_key_checks.is_some() {
            writeln!(
                self.out,
                "SET SESSION foreign_key_checks = @harbor_dump_fk_checks;"
            )
            .map_err(output_error)?;
        }
        write!(self.out, "{DUMP_FOOTER}").map_err(output_error)
    }

    fn dump_header(
        &self,
        source: &mut DumpSource<'_>,
        schema: &str,
        destination: &str,
        source_mode: &str,
    ) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        let charset = self.settings.file_charset;
        if charset != SqlDumpCharset::Utf8Mb4 {
            writeln!(out, "{DUMP_ENCODING_PREFIX}{}", charset.mysql_name())
                .map_err(output_error)?;
        }
        if dump_supports_chinese_notes(charset) {
            writeln!(out, "-- Harbor SQL 逻辑备份，范围为所选对象。").map_err(output_error)?;
            if self.settings.insert_delayed && self.settings.options.include_data {
                writeln!(
                    out,
                    "-- 支持延迟写入的还原服务器可能将数据排队。执行脚本后请核对目标行。"
                )
                .map_err(output_error)?;
            }
            if self.settings.single_transaction {
                writeln!(
                    out,
                    "-- InnoDB 数据使用同一快照。备份期间请保持源库结构和非事务表数据稳定。"
                )
                .map_err(output_error)?;
            }
            if matches!(source, DumpSource::Http(_)) {
                writeln!(out, "-- HTTP 分批读取。备份期间请保持源库数据和结构稳定。")
                    .map_err(output_error)?;
            } else if self.settings.options.include_data
                && self.settings.chunk_rows.is_some()
                && !self.settings.single_transaction
                && !self.settings.lock_source_tables
            {
                writeln!(out, "-- 分批读取。备份期间请保持源库数据和结构稳定。")
                    .map_err(output_error)?;
            }
        }
        write_dump_session_header(&mut out, charset).map_err(output_error)?;
        if let Some(enabled) = self.foreign_key_checks {
            writeln!(
                out,
                "SET @harbor_dump_fk_checks = @@SESSION.foreign_key_checks;\nSET SESSION foreign_key_checks = {};",
                u8::from(enabled)
            )
            .map_err(output_error)?;
        }
        writeln!(out).map_err(output_error)?;
        if self.settings.include_create_database {
            let definition = source.create_database(schema, destination)?;
            writeln!(out, "SET SESSION sql_mode = {source_mode};").map_err(output_error)?;
            write_statement(&mut out, &definition)?;
        }
        if self.settings.include_use {
            writeln!(out, "USE {};\n", quote_identifier(destination)).map_err(output_error)?;
        }
        Ok(out)
    }

    fn write_structure(
        &mut self,
        source: &mut DumpSource<'_>,
        object: &DbObject,
        context: DumpDefinitionContext<'_>,
        header: &[u8],
    ) -> Result<(), String> {
        self.report(Some(&object.name))?;
        let written = self.attempt(source, |writer, source| {
            writer.open_object(object, header)?;
            write_object(
                source,
                &mut writer.out,
                object,
                context,
                writer.settings.options.drop_existing,
            )?;
            writer.close_part()
        })?;
        if written.is_some() {
            self.complete(object)
        } else {
            self.skip(object, "未能完整导出对象定义")
        }
    }

    fn write_triggers(
        &mut self,
        source: &mut DumpSource<'_>,
        schema: &str,
        objects: &[DbObject],
        context: DumpDefinitionContext<'_>,
        header: &[u8],
    ) -> Result<(), String> {
        // Triggers are created after loading rows and in their source execution order.
        let Some(triggers) =
            self.attempt(source, |_, source| source.ordered_triggers(schema, objects))?
        else {
            for trigger in objects
                .iter()
                .filter(|object| object.kind == ObjectKind::Trigger)
            {
                self.skip(trigger, "未能读取触发器执行顺序")?;
            }
            return Ok(());
        };
        let case_mode = context.case_mode;
        let skipped_tables: HashSet<_> = self
            .summary
            .skipped_objects
            .iter()
            .filter(|object| object.kind == "TABLE")
            .map(|object| comparable_name(&object.name, case_mode))
            .collect();
        let tables = if skipped_tables.is_empty() || triggers.is_empty() {
            None
        } else {
            Some(
                self.attempt(source, |_, source| source.trigger_tables(schema))?
                    .unwrap_or_default(),
            )
        };
        for trigger in triggers {
            if let Some(tables) = &tables {
                match tables.get(&trigger.name) {
                    Some(table) if skipped_tables.contains(&comparable_name(table, case_mode)) => {
                        self.skip(trigger, "触发器所属的表已跳过")?;
                        continue;
                    }
                    None => {
                        self.skip(trigger, "未能确认触发器所属的表")?;
                        continue;
                    }
                    _ => {}
                }
            }
            self.write_structure(source, trigger, context, header)?;
        }
        Ok(())
    }

    fn write_views(
        &mut self,
        source: &mut DumpSource<'_>,
        schema: &str,
        objects: &[DbObject],
        context: DumpDefinitionContext<'_>,
        header: &[u8],
    ) -> Result<(), String> {
        let mut views = Vec::new();
        for view in objects
            .iter()
            .filter(|object| object.kind == ObjectKind::View)
        {
            if let Some(definition) = self.attempt(source, |_, source| source.definition(view))? {
                views.push((view, definition));
            } else {
                self.skip(view, "未能读取视图定义")?;
            }
        }
        let case_mode = context.case_mode;
        let source_mode = context.source_sql_mode;
        let dependencies =
            if views.len() > 1 || !views.is_empty() && self.settings.continue_on_error {
                match source {
                    DumpSource::Native(conn) => {
                        view_dependencies(conn, schema, &views, source_mode, case_mode)?
                    }
                    DumpSource::Http(_) => {
                        dependencies_from_view_definitions(schema, &views, source_mode, case_mode)
                    }
                }
            } else {
                Vec::new()
            };
        let names: Vec<_> = views
            .iter()
            .map(|(view, _)| comparable_name(&view.name, case_mode))
            .collect();
        let (order, sorted_count) = dependency_order(&names, dependencies.clone());
        let drop = self.settings.options.drop_existing;
        let groups = order[..sorted_count]
            .chunks(1)
            .map(|view| (view, false))
            .chain(std::iter::once((&order[sorted_count..], true)));
        for (group, stubs) in groups.filter(|(group, _)| !group.is_empty()) {
            if self.settings.continue_on_error
                && group
                    .iter()
                    .any(|&index| self.depends_on_skipped(views[index].0, &dependencies, case_mode))
            {
                for &index in group {
                    self.skip(views[index].0, "所依赖的表或视图已跳过")?;
                }
                continue;
            }
            let written = self.attempt(source, |writer, source| {
                if stubs {
                    writer.open_part("view-stubs", header)?;
                    for &index in group {
                        source.write_view_stub(&mut writer.out, views[index].0, drop)?;
                    }
                    writer.close_part()?;
                }
                for &index in group {
                    let (view, definition) = &views[index];
                    writer.report(Some(&view.name))?;
                    writer.open_object(view, header)?;
                    definition.write(&mut writer.out, view, context, drop && !stubs, stubs)?;
                    writer.close_part()?;
                    writer.complete(view)?;
                }
                Ok(())
            })?;
            if written.is_none() {
                for &index in group {
                    self.skip(views[index].0, "未能完整导出该视图或其关联视图")?;
                }
            }
        }
        Ok(())
    }
}

const TRIGGER_ORDER: &str = "SELECT TRIGGER_NAME, ACTION_ORDER FROM information_schema.TRIGGERS
    WHERE TRIGGER_SCHEMA = :schema
        AND BINARY IF(@@lower_case_table_names = 0, TRIGGER_SCHEMA, LOWER(TRIGGER_SCHEMA))
            = BINARY IF(@@lower_case_table_names = 0, :schema, LOWER(:schema))";

fn ordered_triggers<'a>(
    conn: &mut Conn,
    schema: &str,
    objects: &'a [DbObject],
) -> Result<Vec<&'a DbObject>, String> {
    let mut triggers: Vec<_> = objects
        .iter()
        .filter(|object| object.kind == ObjectKind::Trigger)
        .collect();
    if triggers.len() > 1 {
        let order = conn
            .exec_opt::<(String, u32), _, _>(TRIGGER_ORDER, params! { "schema" => schema })
            .map_err(mysql_error)?
            .into_iter()
            .collect::<Result<HashMap<_, _>, _>>()
            .map_err(|error| mysql_error(error.into()))?;
        triggers.sort_by_key(|trigger| order.get(&trigger.name).copied().unwrap_or_default());
    }
    Ok(triggers)
}

fn table_dependencies(
    conn: &mut Conn,
    schema: &str,
    case_mode: u8,
    include_self_references: bool,
) -> Result<Vec<(String, String)>, String> {
    const SQL: &str = "SELECT TABLE_NAME, REFERENCED_TABLE_NAME
        FROM information_schema.KEY_COLUMN_USAGE
        WHERE TABLE_SCHEMA = :schema AND REFERENCED_TABLE_SCHEMA = :schema
            AND BINARY IF(@@lower_case_table_names = 0, TABLE_SCHEMA, LOWER(TABLE_SCHEMA))
                = BINARY IF(@@lower_case_table_names = 0, :schema, LOWER(:schema))
            AND BINARY IF(@@lower_case_table_names = 0, REFERENCED_TABLE_SCHEMA, LOWER(REFERENCED_TABLE_SCHEMA))
                = BINARY IF(@@lower_case_table_names = 0, :schema, LOWER(:schema))";
    let dependencies = schema_dependencies(conn, SQL, schema, case_mode).map_err(mysql_error)?;
    // A table can be created with a self reference. Loading its rows may depend on row order.
    Ok(dependencies
        .into_iter()
        .filter(|(table, parent)| include_self_references || table != parent)
        .collect())
}

fn view_dependencies(
    conn: &mut Conn,
    schema: &str,
    views: &[(&DbObject, ObjectDefinition)],
    source_mode: &str,
    case_mode: u8,
) -> Result<Vec<(String, String)>, String> {
    const SQL: &str = "SELECT VIEW_NAME, TABLE_NAME
        FROM information_schema.VIEW_TABLE_USAGE
        WHERE VIEW_SCHEMA = :schema AND TABLE_SCHEMA = :schema
            AND BINARY IF(@@lower_case_table_names = 0, VIEW_SCHEMA, LOWER(VIEW_SCHEMA))
                = BINARY IF(@@lower_case_table_names = 0, :schema, LOWER(:schema))
            AND BINARY IF(@@lower_case_table_names = 0, TABLE_SCHEMA, LOWER(TABLE_SCHEMA))
                = BINARY IF(@@lower_case_table_names = 0, :schema, LOWER(:schema))";
    match schema_dependencies(conn, SQL, schema, case_mode) {
        Ok(dependencies) => return Ok(dependencies),
        Err(mysql::Error::MySqlError(error)) if matches!(error.code, 1109 | 1146) => {}
        Err(error) => return Err(mysql_error(error)),
    }

    Ok(dependencies_from_view_definitions(
        schema,
        views,
        source_mode,
        case_mode,
    ))
}

fn dependencies_from_view_definitions(
    schema: &str,
    views: &[(&DbObject, ObjectDefinition)],
    source_mode: &str,
    case_mode: u8,
) -> Vec<(String, String)> {
    // MariaDB and older MySQL versions lack VIEW_TABLE_USAGE. Inspect the canonical
    // SELECT printed by SHOW CREATE VIEW for named relations in FROM and JOIN clauses.
    let mut dependencies = Vec::new();
    for (view, definition) in views {
        let name = comparable_name(&view.name, case_mode);
        for relation in view_relations(&definition.ddl, schema, source_mode, case_mode) {
            if relation != name {
                dependencies.push((name.clone(), relation));
            }
        }
    }
    dependencies
}

fn schema_dependencies(
    conn: &mut Conn,
    sql: &str,
    schema: &str,
    case_mode: u8,
) -> Result<Vec<(String, String)>, mysql::Error> {
    conn.exec_opt::<(String, String), _, _>(sql, params! { "schema" => schema })?
        .into_iter()
        .map(|row| {
            let (object, dependency) = row?;
            Ok((
                comparable_name(&object, case_mode),
                comparable_name(&dependency, case_mode),
            ))
        })
        .collect()
}

fn comparable_name(name: &str, case_mode: u8) -> String {
    if case_mode == 0 {
        name.to_owned()
    } else {
        name.to_lowercase()
    }
}

fn dependency_groups<'a>(
    objects: &[&'a DbObject],
    dependencies: &[(String, String)],
    case_mode: u8,
) -> Vec<Vec<&'a DbObject>> {
    let indices: HashMap<_, _> = objects
        .iter()
        .enumerate()
        .map(|(index, object)| (comparable_name(&object.name, case_mode), index))
        .collect();
    let mut links = vec![Vec::new(); objects.len()];
    for (dependent, required) in dependencies {
        if let (Some(&a), Some(&b)) = (indices.get(dependent), indices.get(required)) {
            links[a].push(b);
            links[b].push(a);
        }
    }
    let mut visited = vec![false; objects.len()];
    let mut groups = Vec::new();
    for start in 0..objects.len() {
        if std::mem::replace(&mut visited[start], true) {
            continue;
        }
        let mut group = vec![start];
        let mut next = 0;
        while let Some(&index) = group.get(next) {
            for &other in &links[index] {
                if !std::mem::replace(&mut visited[other], true) {
                    group.push(other);
                }
            }
            next += 1;
        }
        group.sort_unstable();
        groups.push(group.into_iter().map(|index| objects[index]).collect());
    }
    groups
}

// The resolved portion comes first. A remaining tail represents cyclic dependencies.
pub(super) fn dependency_order<T: Eq + std::hash::Hash>(
    names: &[T],
    dependencies: Vec<(T, T)>,
) -> (Vec<usize>, usize) {
    let indices: HashMap<_, _> = names
        .iter()
        .enumerate()
        .map(|(index, name)| (name, index))
        .collect();
    let mut required = vec![0; names.len()];
    let mut edges = vec![Vec::new(); names.len()];
    let mut pairs = HashSet::new();
    for (name, dependency) in dependencies {
        if let (Some(&target), Some(&source)) = (indices.get(&name), indices.get(&dependency)) {
            if pairs.insert((source, target)) {
                edges[source].push(target);
                required[target] += 1;
            }
        }
    }
    let mut available: VecDeque<_> = required
        .iter()
        .enumerate()
        .filter_map(|(index, &count)| (count == 0).then_some(index))
        .collect();
    let mut order = Vec::with_capacity(names.len());
    while let Some(source) = available.pop_front() {
        order.push(source);
        for &target in &edges[source] {
            required[target] -= 1;
            if required[target] == 0 {
                available.push_back(target);
            }
        }
    }
    let sorted_count = order.len();
    order.extend(
        required
            .into_iter()
            .enumerate()
            .filter_map(|(index, count)| (count > 0).then_some(index)),
    );
    (order, sorted_count)
}

enum ViewToken<'a> {
    Word(&'a str),
    Identifier(String),
    Symbol(u8),
}

impl ViewToken<'_> {
    fn keyword(&self, keyword: &str) -> bool {
        matches!(self, Self::Word(value) if value.eq_ignore_ascii_case(keyword))
    }

    fn name(&self) -> Option<&str> {
        match self {
            Self::Word(value) => Some(value),
            Self::Identifier(value) => Some(value),
            Self::Symbol(_) => None,
        }
    }
}

fn view_token<'a>(
    remaining: &mut &'a str,
    ansi_quotes: bool,
    backslash: bool,
) -> Option<ViewToken<'a>> {
    let sql = remaining.trim_ascii_start();
    let first = *sql.as_bytes().first()?;
    let (length, token) = match first {
        b'\'' | b'"' | b'`' => {
            let identifier = first == b'`' || first == b'"' && ansi_quotes;
            let end = quoted_end(sql.as_bytes(), 0, first, backslash && !identifier)?;
            let quote = char::from(first).to_string();
            let token = if identifier {
                ViewToken::Identifier(sql[1..end - 1].replace(&quote.repeat(2), &quote))
            } else {
                ViewToken::Symbol(first)
            };
            (end, token)
        }
        byte if identifier_byte(byte) => {
            let end = sql
                .bytes()
                .take_while(|&byte| identifier_byte(byte))
                .count();
            (end, ViewToken::Word(&sql[..end]))
        }
        other => (1, ViewToken::Symbol(other)),
    };
    *remaining = &sql[length..];
    Some(token)
}

#[derive(Default)]
struct ViewScope {
    in_select: bool,
    in_from: bool,
    ctes: Vec<String>,
}

fn view_relations(ddl: &str, schema: &str, mode: &str, case_mode: u8) -> Vec<String> {
    let ansi_quotes = mode.split(',').any(|flag| flag == "ANSI_QUOTES");
    let backslash = !mode.split(',').any(|flag| flag == "NO_BACKSLASH_ESCAPES");
    let schema = comparable_name(schema, case_mode);
    let mut remaining = ddl;
    let tokens: Vec<_> =
        std::iter::from_fn(|| view_token(&mut remaining, ansi_quotes, backslash)).collect();
    let mut scopes = vec![ViewScope::default()];
    let mut relations = Vec::new();
    let mut relation_expected = false;
    let mut pos = 0;
    while let Some(token) = tokens.get(pos) {
        pos += 1;
        let scope = scopes.last_mut().expect("root view scope");
        if token.keyword("WITH") {
            scope.ctes = cte_names(&tokens[pos..], case_mode);
        } else if token.keyword("SELECT") {
            scope.in_select = true;
            scope.in_from = false;
        } else if token.keyword("FROM") && scope.in_select {
            scope.in_from = true;
            relation_expected = true;
            continue;
        } else if scope.in_from
            && (token.keyword("JOIN")
                || token.keyword("STRAIGHT_JOIN")
                || matches!(token, ViewToken::Symbol(b',')))
        {
            relation_expected = true;
            continue;
        } else if matches!(token, ViewToken::Symbol(b'(')) {
            scopes.push(ViewScope {
                in_from: relation_expected,
                ..ViewScope::default()
            });
            continue;
        } else if matches!(token, ViewToken::Symbol(b')')) {
            if scopes.len() > 1 {
                scopes.pop();
            }
        } else if relation_expected {
            if let Some(name) = token.name() {
                let name = comparable_name(name, case_mode);
                if matches!(tokens.get(pos), Some(ViewToken::Symbol(b'.'))) {
                    if let Some(table) = tokens.get(pos + 1).and_then(ViewToken::name) {
                        if name == schema {
                            relations.push(comparable_name(table, case_mode));
                        }
                        pos += 2;
                    }
                } else if !scopes.iter().any(|scope| scope.ctes.contains(&name))
                    && !matches!(tokens.get(pos), Some(ViewToken::Symbol(b'(')))
                {
                    relations.push(name);
                }
            }
        } else if [
            "WHERE",
            "GROUP",
            "ORDER",
            "HAVING",
            "UNION",
            "INTERSECT",
            "EXCEPT",
            "LIMIT",
            "WINDOW",
        ]
        .into_iter()
        .any(|keyword| token.keyword(keyword))
        {
            scope.in_from = false;
        }
        relation_expected = false;
    }
    relations
}

fn cte_names(mut tokens: &[ViewToken<'_>], case_mode: u8) -> Vec<String> {
    if tokens
        .first()
        .is_some_and(|token| token.keyword("RECURSIVE"))
    {
        tokens = &tokens[1..];
    }
    let mut names = Vec::new();
    while let Some((name, rest)) = tokens
        .split_first()
        .and_then(|(token, rest)| Some((token.name()?, rest)))
    {
        let Some(rest) = (if matches!(rest.first(), Some(ViewToken::Symbol(b'('))) {
            after_parentheses(rest)
        } else {
            Some(rest)
        }) else {
            break;
        };
        if !rest.first().is_some_and(|token| token.keyword("AS")) {
            break;
        }
        let Some(rest) = after_parentheses(&rest[1..]) else {
            break;
        };
        names.push(comparable_name(name, case_mode));
        let [ViewToken::Symbol(b','), next @ ..] = rest else {
            break;
        };
        tokens = next;
    }
    names
}

fn after_parentheses<'tokens, 'sql>(
    tokens: &'tokens [ViewToken<'sql>],
) -> Option<&'tokens [ViewToken<'sql>]> {
    if !matches!(tokens.first(), Some(ViewToken::Symbol(b'('))) {
        return None;
    }
    let mut depth = 1;
    for (index, token) in tokens.iter().enumerate().skip(1) {
        match token {
            ViewToken::Symbol(b'(') => depth += 1,
            ViewToken::Symbol(b')') => {
                depth -= 1;
                if depth == 0 {
                    return Some(&tokens[index + 1..]);
                }
            }
            _ => {}
        }
    }
    None
}

pub(crate) const DUMP_CLIENT_STATE: &str = "SET @harbor_dump_sql_mode = @@SESSION.sql_mode,\n\
        @harbor_dump_time_zone = @@SESSION.time_zone,\n\
        @harbor_dump_charset_client = @@SESSION.character_set_client,\n\
        @harbor_dump_charset_results = @@SESSION.character_set_results,\n\
        @harbor_dump_collation = @@SESSION.collation_connection;\n";

pub(crate) const DUMP_CLIENT_ENCODING_TAG: &str = " -- Harbor SQL dump client encoding";
const DUMP_SESSION_SETTINGS: &str = "SET @harbor_dump_utf8_collation = @@SESSION.collation_connection;\n\
    SET SESSION sql_mode = 'NO_AUTO_VALUE_ON_ZERO';\n";
const DUMP_SESSION_UTC: &str = "SET SESSION time_zone = '+00:00';\n";

fn write_dump_session_header(out: &mut impl Write, charset: SqlDumpCharset) -> io::Result<()> {
    let tag = if charset == SqlDumpCharset::Utf8Mb4 {
        ""
    } else {
        DUMP_CLIENT_ENCODING_TAG
    };
    write!(
        out,
        "{DUMP_CLIENT_STATE}SET NAMES {};{tag}\n{DUMP_SESSION_SETTINGS}{DUMP_SESSION_UTC}",
        charset.mysql_name()
    )
}

const DUMP_FOOTER: &str = "SET SESSION time_zone = @harbor_dump_time_zone;\n\
    SET SESSION sql_mode = @harbor_dump_sql_mode;\n\
    SET SESSION character_set_client = @harbor_dump_charset_client;\n\
    SET SESSION character_set_results = @harbor_dump_charset_results;\n\
    SET SESSION collation_connection = @harbor_dump_collation;\n";

fn dump_supports_chinese_notes(charset: SqlDumpCharset) -> bool {
    matches!(
        charset,
        SqlDumpCharset::Utf8Mb4
            | SqlDumpCharset::Utf8Mb3
            | SqlDumpCharset::Gbk
            | SqlDumpCharset::Gb18030
    )
}

fn query_create_row(conn: &mut Conn, query: &str) -> Result<Row, String> {
    conn.query_first(query)
        .map_err(mysql_error)?
        .ok_or_else(|| "服务器未返回请求的对象定义。".into())
}

fn text_field(row: &Row, field: &str) -> Result<Option<String>, String> {
    let index = row
        .columns_ref()
        .iter()
        .position(|column| column.name_str().eq_ignore_ascii_case(field));
    index
        .and_then(|index| row.get_opt::<Option<String>, _>(index))
        .transpose()
        .map(Option::flatten)
        .map_err(|error| mysql_error(error.into()))
}

fn required_text(row: &Row, field: &str) -> Result<String, String> {
    text_field(row, field)?.ok_or_else(|| "此连接账户无法读取对象定义，请检查元数据权限。".into())
}

fn setting_literal(value: &str) -> Result<String, String> {
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b','))
    {
        return Err("服务器返回了不支持的 SQL 模式或排序规则名称。".into());
    }
    Ok(format!("'{value}'"))
}

fn relocated_database_ddl(
    source: &mut DumpSource<'_>,
    schema: &str,
    destination: &str,
) -> Result<String, String> {
    const SQL: &str = "SELECT DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME
        FROM information_schema.SCHEMATA
        WHERE SCHEMA_NAME = :schema
            AND BINARY IF(@@lower_case_table_names = 0, SCHEMA_NAME, LOWER(SCHEMA_NAME))
                = BINARY IF(@@lower_case_table_names = 0, :schema, LOWER(:schema))";
    let (charset, collation) = match source {
        DumpSource::Native(conn) => conn
            .exec_first_opt::<(String, String), _, _>(SQL, params! { "schema" => schema })
            .map_err(mysql_error)?
            .transpose()
            .map_err(|error| mysql_error(error.into()))?,
        DumpSource::Http(session) => {
            let query = SQL.replace(":schema", &mysql_text_literal(schema));
            match session
                .metadata_columns::<2>(&query, 1)
                .map_err(Failure::message)?
                .pop()
            {
                Some([Some(charset), Some(collation)]) => Some((charset, collation)),
                _ => None,
            }
        }
    }
    .ok_or("无法读取源库的字符集和排序规则。")?;
    Ok(format!(
        "CREATE DATABASE IF NOT EXISTS {} DEFAULT CHARACTER SET {} DEFAULT COLLATE {}",
        quote_identifier(destination),
        quote_identifier(&charset),
        quote_identifier(&collation)
    ))
}

fn write_object(
    source: &mut DumpSource<'_>,
    out: &mut impl Write,
    object: &DbObject,
    context: DumpDefinitionContext<'_>,
    drop_existing: bool,
) -> Result<Option<DeferredTableKeys>, String> {
    let definition = source.definition(object)?;
    definition.write(out, object, context, drop_existing, false)
}

struct ObjectDefinition {
    ddl: String,
    sql_mode: Option<String>,
    collation: Option<String>,
    time_zone: Option<String>,
}

struct DeferredTableKeys {
    table: String,
    statement: String,
    sql_mode: String,
    collation: Option<String>,
}

impl DeferredTableKeys {
    fn write(&self, out: &mut impl Write) -> Result<(), String> {
        write_definition_settings(out, &self.sql_mode, self.collation.as_deref())?;
        write_statement(out, &self.statement)?;
        write!(out, "{DUMP_COLLATION_RESET}").map_err(output_error)
    }
}

const DUMP_COLLATION_RESET: &str =
    "SET SESSION collation_connection = @harbor_dump_utf8_collation;\n\n";

fn write_definition_settings(
    out: &mut impl Write,
    sql_mode: &str,
    collation: Option<&str>,
) -> Result<(), String> {
    writeln!(out, "SET SESSION sql_mode = {sql_mode};").map_err(output_error)?;
    if let Some(collation) = collation {
        writeln!(out, "SET SESSION collation_connection = {collation};").map_err(output_error)?;
    }
    Ok(())
}

fn read_definition(conn: &mut Conn, object: &DbObject) -> Result<ObjectDefinition, String> {
    let (kind, field) = object.kind.show_create();
    let name = object.qualified_name();
    let row = query_create_row(conn, &format!("SHOW CREATE {kind} {name}"))?;
    Ok(ObjectDefinition {
        ddl: required_text(&row, field).map_err(|error| format!("{kind} {name}：{error}"))?,
        sql_mode: text_field(&row, "sql_mode")?,
        collation: text_field(&row, "collation_connection")?
            .map(|name| setting_literal(&name))
            .transpose()?,
        time_zone: text_field(&row, "time_zone")?,
    })
}

impl ObjectDefinition {
    fn write(
        &self,
        out: &mut impl Write,
        object: &DbObject,
        context: DumpDefinitionContext<'_>,
        drop_existing: bool,
        replace_view_stub: bool,
    ) -> Result<Option<DeferredTableKeys>, String> {
        let object_mode = self.sql_mode.as_deref();
        let object_mode_literal = object_mode.map(setting_literal).transpose()?;
        let raw_mode = object_mode.unwrap_or(context.source_sql_mode);
        let mode = object_mode_literal
            .as_deref()
            .unwrap_or(context.source_sql_mode_literal);
        let mut ddl = if let Some(destination) = context.renamed_schema(&object.schema) {
            Cow::Owned(
                copy_definition::relocate(
                    &self.ddl,
                    object,
                    destination,
                    raw_mode,
                    context.case_mode,
                )
                .map_err(|error| {
                    format!(
                        "{} 的定义无法复制到目标数据库。{error}",
                        object.qualified_name()
                    )
                })?,
            )
        } else {
            Cow::Borrowed(self.ddl.as_str())
        };
        let keys = if object.kind == ObjectKind::Table
            && let (Some(destination), Some(selected)) =
                (context.destination_schema, context.deferred_targets)
        {
            let (create, clauses) = deferred_copy_keys::defer_selected(
                &ddl,
                destination,
                selected,
                raw_mode,
                context.case_mode,
            )
            .map_err(|error| format!("{} 的外键无法延后添加。{error}", object.qualified_name()))?;
            ddl = Cow::Owned(create);
            (!clauses.is_empty()).then(|| DeferredTableKeys {
                table: object.name.clone(),
                statement: format!(
                    "ALTER TABLE {}.{} {}",
                    quote_identifier(destination),
                    quote_identifier(&object.name),
                    clauses.join(", ")
                ),
                sql_mode: mode.to_owned(),
                collation: self.collation.clone(),
            })
        } else {
            None
        };
        write_definition_settings(out, mode, self.collation.as_deref())?;
        let event_time_zone =
            if context.destination_schema.is_some() && object.kind == ObjectKind::Event {
                self.time_zone.as_deref()
            } else {
                None
            };
        if let Some(time_zone) = event_time_zone {
            writeln!(
                out,
                "SET SESSION time_zone = {};",
                mysql_text_literal(time_zone)
            )
            .map_err(output_error)?;
        }
        if drop_existing {
            write_drop(out, object)?;
        }
        if replace_view_stub {
            let definition = ddl
                .strip_prefix("CREATE ")
                .ok_or("服务器返回了无法识别的视图定义。")?;
            write_statement(out, &format!("CREATE OR REPLACE {definition}"))?;
        } else {
            write_statement(out, &ddl)?;
        }
        if event_time_zone.is_some() {
            write!(out, "{DUMP_SESSION_UTC}").map_err(output_error)?;
        }
        write!(out, "{DUMP_COLLATION_RESET}").map_err(output_error)?;
        Ok(keys)
    }
}

fn write_drop(out: &mut impl Write, object: &DbObject) -> Result<(), String> {
    let (kind, _) = object.kind.show_create();
    let name = quote_identifier(&object.name);
    writeln!(out, "DROP {kind} IF EXISTS {name};").map_err(output_error)
}

fn identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$' | 0x80..=0xff)
}

fn quoted_end(bytes: &[u8], start: usize, quote: u8, backslash: bool) -> Option<usize> {
    let mut pos = start + 1;
    while let Some(&byte) = bytes.get(pos) {
        if byte == quote {
            if bytes.get(pos + 1) != Some(&quote) {
                return Some(pos + 1);
            }
            pos += 2;
        } else if backslash && byte == b'\\' {
            pos += 2;
        } else {
            pos += 1;
        }
    }
    None
}

fn write_statement(out: &mut impl Write, sql: &str) -> Result<(), String> {
    if !sql.contains([';', '\n', '#']) && !sql.contains("--") {
        return writeln!(out, "{sql};").map_err(output_error);
    }
    let mut delimiter = "$$".to_owned();
    while sql.contains(&delimiter) {
        delimiter.push('$');
    }
    writeln!(
        out,
        "DELIMITER {delimiter}\n{sql}\n{delimiter}\nDELIMITER ;"
    )
    .map_err(output_error)
}

fn write_view_stub(
    conn: &mut Conn,
    out: &mut impl Write,
    object: &DbObject,
    drop_existing: bool,
) -> Result<(), String> {
    let columns = conn
        .query_opt::<Row, _>(format!("SHOW COLUMNS FROM {}", object.qualified_name()))
        .map_err(mysql_error)?
        .into_iter()
        .map(|row| {
            let row = row.map_err(|error| mysql_error(error.into()))?;
            required_text(&row, "Field").map(|name| format!("1 AS {}", quote_identifier(&name)))
        })
        .collect::<Result<Vec<_>, String>>()?;
    write_view_stub_definition(out, object, drop_existing, &columns)
}

fn write_view_stub_definition(
    out: &mut impl Write,
    object: &DbObject,
    drop_existing: bool,
    columns: &[String],
) -> Result<(), String> {
    let name = quote_identifier(&object.name);
    if columns.is_empty() {
        return Err(format!("服务器未返回视图 {name} 的列。"));
    }
    if drop_existing {
        write_drop(out, object)?;
    }
    writeln!(out, "CREATE VIEW {name} AS SELECT {};", columns.join(", ")).map_err(output_error)
}

#[derive(Clone, Copy)]
enum DataKind {
    Number,
    Binary,
    Text,
    Geometry,
}

struct DumpColumn {
    name: String,
    kind: DataKind,
    expressions: String,
}

struct TableData {
    columns: Vec<DumpColumn>,
    needs_column_list: bool,
    supports_disable_keys: bool,
    supports_delayed: bool,
}

impl TableData {
    fn select_fields(&mut self, fields: Option<&[String]>) -> Result<(), String> {
        let Some(fields) = fields else {
            return Ok(());
        };
        if fields.is_empty() {
            return Err("请选择至少一个可写列。".into());
        }
        let mut available = std::mem::take(&mut self.columns);
        for field in fields {
            let name = quote_identifier(field);
            let index = available
                .iter()
                .position(|column| column.name == name)
                .ok_or_else(|| {
                    format!("无法导出可写列「{field}」，该列不存在、是生成列或已选择。")
                })?;
            self.columns.push(available.swap_remove(index));
        }
        self.needs_column_list = true;
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct InsertOutput<'a> {
    lock_tables: bool,
    disable_autocommit: Option<bool>,
    batch_limit: usize,
    complete: bool,
    delayed: bool,
    disable_keys: bool,
    chunk_rows: Option<NonZeroU32>,
    fields: Option<&'a [String]>,
}

const COLUMNS_SQL: &str = "SELECT COLUMN_NAME, DATA_TYPE, EXTRA FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = :schema AND TABLE_NAME = :object
        AND BINARY IF(@@lower_case_table_names = 0, TABLE_SCHEMA, LOWER(TABLE_SCHEMA))
            = BINARY IF(@@lower_case_table_names = 0, :schema, LOWER(:schema))
        AND BINARY IF(@@lower_case_table_names = 0, TABLE_NAME, LOWER(TABLE_NAME))
            = BINARY IF(@@lower_case_table_names = 0, :object, LOWER(:object))
    ORDER BY ORDINAL_POSITION";

const TABLE_SQL: &str = "SELECT ENGINE, TABLE_TYPE, CREATE_OPTIONS FROM information_schema.TABLES
    WHERE TABLE_SCHEMA = :schema AND TABLE_NAME = :object
        AND BINARY IF(@@lower_case_table_names = 0, TABLE_SCHEMA, LOWER(TABLE_SCHEMA))
            = BINARY IF(@@lower_case_table_names = 0, :schema, LOWER(:schema))
        AND BINARY IF(@@lower_case_table_names = 0, TABLE_NAME, LOWER(TABLE_NAME))
            = BINARY IF(@@lower_case_table_names = 0, :object, LOWER(:object))";

const DELAYED_TABLE_SQL: &str = "SELECT NOT EXISTS (
        SELECT 1 FROM information_schema.TRIGGERS
        WHERE EVENT_OBJECT_SCHEMA = :schema AND EVENT_OBJECT_TABLE = :object
            AND (@@lower_case_table_names <> 0 OR BINARY EVENT_OBJECT_SCHEMA = BINARY :schema)
            AND (@@lower_case_table_names <> 0 OR BINARY EVENT_OBJECT_TABLE = BINARY :object)
    ) AND NOT EXISTS (
        SELECT 1 FROM information_schema.TABLE_CONSTRAINTS
        WHERE TABLE_SCHEMA = :schema AND TABLE_NAME = :object AND CONSTRAINT_TYPE = 'CHECK'
            AND (@@lower_case_table_names <> 0 OR BINARY TABLE_SCHEMA = BINARY :schema)
            AND (@@lower_case_table_names <> 0 OR BINARY TABLE_NAME = BINARY :object)
    )";

fn native_table_data(
    conn: &mut Conn,
    object: &DbObject,
    require_innodb: bool,
    delayed: bool,
) -> Result<TableData, String> {
    let args = params! { "schema" => object.schema.as_str(), "object" => object.name.as_str() };
    let (engine, table_type, create_options) = conn
        .exec_first_opt::<(Option<String>, String, Option<String>), _, _>(TABLE_SQL, args.clone())
        .map_err(mysql_error)?
        .transpose()
        .map_err(|error| mysql_error(error.into()))?
        .ok_or_else(|| format!("无法读取 {} 的表属性。", object.qualified_name()))?;
    check_table_type(object, &table_type, create_options.as_deref())?;
    if require_innodb
        && !engine
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case("InnoDB"))
    {
        let engine = engine.as_deref().unwrap_or("未知引擎");
        return Err(format!(
            "{} 使用 {engine}。一致性数据备份目前要求 InnoDB，可以仅备份该对象的结构。",
            object.qualified_name()
        ));
    }
    let supports_delayed = delayed
        && engine_allows_delayed(engine.as_deref(), create_options.as_deref())
        && conn
            .exec_first_opt::<u8, _, _>(DELAYED_TABLE_SQL, args.clone())
            .map_err(mysql_error)?
            .transpose()
            .map_err(|error| mysql_error(error.into()))?
            == Some(1);
    let rows = conn
        .exec_opt::<(String, String, String), _, _>(COLUMNS_SQL, args)
        .map_err(mysql_error)?;
    table_data(
        object,
        engine.as_deref(),
        supports_delayed,
        rows.into_iter()
            .map(|row| row.map_err(|error| mysql_error(error.into()))),
    )
}

fn check_table_type(object: &DbObject, kind: &str, options: Option<&str>) -> Result<(), String> {
    if kind != "BASE TABLE"
        || options.is_some_and(|options| options.to_ascii_uppercase().contains("SYSTEM VERSION"))
    {
        return Err(format!(
            "{} 的表类型或选项无法用当前逻辑备份完整还原，可以仅备份结构。",
            object.qualified_name()
        ));
    }
    Ok(())
}

fn engine_allows_delayed(engine: Option<&str>, options: Option<&str>) -> bool {
    let Some(engine) = engine else { return false };
    let options = options.unwrap_or_default();
    if options
        .split_ascii_whitespace()
        .any(|option| option.eq_ignore_ascii_case("partitioned"))
    {
        return false;
    }
    ["MyISAM", "MEMORY", "ARCHIVE", "BLACKHOLE"]
        .iter()
        .any(|name| engine.eq_ignore_ascii_case(name))
        || engine.eq_ignore_ascii_case("Aria")
            && options
                .split_ascii_whitespace()
                .any(|option| option.eq_ignore_ascii_case("transactional=0"))
}

fn table_data(
    object: &DbObject,
    engine: Option<&str>,
    supports_delayed: bool,
    rows: impl Iterator<Item = Result<(String, String, String), String>>,
) -> Result<TableData, String> {
    let mut columns = Vec::new();
    let mut needs_column_list = false;
    for row in rows {
        let (name, ty, extra) = row?;
        let extra = extra.to_ascii_uppercase();
        if extra.contains("ROW START") || extra.contains("ROW END") {
            return Err(format!(
                "系统版本表 {} 的数据需要支持历史版本的备份工具。",
                object.qualified_name()
            ));
        }
        let mut extra_words = extra.split_ascii_whitespace();
        if extra_words
            .clone()
            .any(|word| matches!(word, "VIRTUAL" | "STORED" | "PERSISTENT"))
        {
            needs_column_list = true;
            continue;
        }
        // The implicit INSERT list excludes invisible fields, even though we export their values.
        needs_column_list |= extra_words.any(|word| word == "INVISIBLE");
        let name = quote_identifier(&name);
        let (kind, expressions) = match ty.as_str() {
            "bit" => (DataKind::Number, format!("CAST({name} AS UNSIGNED)")),
            "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "decimal"
            | "numeric" | "float" | "double" | "real" => (DataKind::Number, name.clone()),
            "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" => {
                (DataKind::Binary, name.clone())
            }
            "geometry" | "point" | "linestring" | "polygon" | "multipoint" | "multilinestring"
            | "multipolygon" | "geometrycollection" => (
                DataKind::Geometry,
                format!("ST_AsWKB({name}), ST_SRID({name})"),
            ),
            "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" | "json"
            | "enum" | "set" | "date" | "datetime" | "timestamp" | "time" | "year" | "inet4"
            | "inet6" | "uuid" => (DataKind::Text, name.clone()),
            _ => {
                return Err(format!(
                    "暂不支持备份 {}.{name} 的数据类型 {ty}。",
                    object.qualified_name()
                ));
            }
        };
        columns.push(DumpColumn {
            name,
            kind,
            expressions,
        });
    }
    if columns.is_empty() {
        return Err(format!("{} 没有可见的可写列。", object.qualified_name()));
    }
    Ok(TableData {
        columns,
        needs_column_list,
        supports_disable_keys: engine.is_some_and(|engine| {
            engine.eq_ignore_ascii_case("MyISAM") || engine.eq_ignore_ascii_case("Aria")
        }),
        supports_delayed,
    })
}

fn dump_rows(
    conn: &mut Conn,
    out: &mut impl Write,
    table: &DbObject,
    require_innodb: bool,
    insert: InsertOutput<'_>,
    progress: impl FnMut(u64) -> Result<(), String>,
) -> Result<u64, String> {
    let mut data = native_table_data(conn, table, require_innodb, insert.delayed)?;
    data.select_fields(insert.fields)?;
    let mut query = data_query(table, &data.columns);
    if insert.chunk_rows.is_some() {
        let detail = inspect_object(conn, table.clone()).map_err(Failure::message)?;
        let order = http_session::paging_order(
            table,
            &detail.columns,
            detail.columns.iter().map(|column| column.name.as_str()),
        );
        query.push_str(" ORDER BY ");
        query.push_str(&order);
    }
    write_data_rows(out, table, &data, insert, progress, |write_row| {
        let mut offset = 0_u64;
        loop {
            let page = match insert.chunk_rows {
                Some(limit) => Cow::Owned(format!("{query} LIMIT {limit} OFFSET {offset}")),
                None => Cow::Borrowed(query.as_str()),
            };
            let mut count = 0;
            for row in conn.query_iter(page.as_ref()).map_err(mysql_error)? {
                let mut row = row.map_err(mysql_error)?;
                let values = (0..row.len())
                    .map(|index| read_field::<Vec<u8>>(&mut row, index))
                    .collect::<Result<_, _>>()?;
                write_row(values)?;
                count += 1;
            }
            if insert
                .chunk_rows
                .is_none_or(|limit| count < u64::from(limit.get()))
            {
                break;
            }
            offset = offset.checked_add(count).ok_or("导出行数超过上限。")?;
        }
        Ok(())
    })
}

fn dump_http_rows(
    session: &HttpSession,
    out: &mut impl Write,
    table: &DbObject,
    insert: InsertOutput<'_>,
    progress: impl FnMut(u64) -> Result<(), String>,
) -> Result<u64, String> {
    let format_query = |sql: &str| {
        sql.replace(":schema", &mysql_text_literal(&table.schema))
            .replace(":object", &mysql_text_literal(&table.name))
    };
    let properties = session
        .metadata_columns::<3>(&format_query(TABLE_SQL), 1)
        .map_err(Failure::message)?;
    let [engine, Some(kind), options] = properties.first().ok_or("服务器未返回表属性。")?
    else {
        return Err("服务器返回的表属性不完整。".into());
    };
    check_table_type(table, kind, options.as_deref())?;
    let supports_delayed = insert.delayed
        && engine_allows_delayed(engine.as_deref(), options.as_deref())
        && session
            .metadata_columns::<1>(&format_query(DELAYED_TABLE_SQL), 1)
            .map_err(Failure::message)?
            .first()
            .and_then(|row| row[0].as_deref())
            == Some("1");

    let metadata = session
        .metadata_columns::<3>(&format_query(COLUMNS_SQL), 10_000)
        .map_err(Failure::message)?;
    let mut data = table_data(
        table,
        engine.as_deref(),
        supports_delayed,
        metadata.into_iter().map(|row| match row {
            [Some(name), Some(kind), Some(extra)] => Ok((name, kind, extra)),
            _ => Err("服务器返回的列属性不完整。".into()),
        }),
    )?;
    data.select_fields(insert.fields)?;
    let order_meta = session.inspect(table.clone())?;
    let order = http_session::paging_order(
        table,
        &order_meta.columns,
        order_meta.columns.iter().map(|column| column.name.as_str()),
    );
    let query = format!("{} ORDER BY {order}", data_query(table, &data.columns));
    write_data_rows(out, table, &data, insert, progress, |write_row| {
        for row in session.paged_rows_with_limit(query, None, insert.chunk_rows) {
            write_row(row?)?;
        }
        Ok(())
    })
}

fn data_query(table: &DbObject, columns: &[DumpColumn]) -> String {
    let query_cols = columns
        .iter()
        .map(|column| column.expressions.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!("SELECT {query_cols} FROM {}", table.qualified_name())
}

type DumpRow = Vec<Option<Vec<u8>>>;

fn write_data_rows(
    out: &mut impl Write,
    table: &DbObject,
    data: &TableData,
    insert: InsertOutput<'_>,
    mut progress: impl FnMut(u64) -> Result<(), String>,
    read_rows: impl FnOnce(&mut dyn FnMut(DumpRow) -> Result<(), String>) -> Result<(), String>,
) -> Result<u64, String> {
    let name = quote_identifier(&table.name);
    let verb = if data.supports_delayed {
        "INSERT DELAYED"
    } else {
        "INSERT"
    };
    let prefix = if insert.complete || data.needs_column_list {
        let names = data
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!("{verb} INTO {name} ({names}) VALUES ")
    } else {
        format!("{verb} INTO {name} VALUES ")
    };
    let disable_keys = insert.disable_keys && data.supports_disable_keys;
    let disable_autocommit = insert.disable_autocommit.unwrap_or(insert.lock_tables);
    let start_transaction =
        !data.supports_delayed && insert.disable_autocommit.is_none() && !insert.lock_tables;
    let progress_every = u64::from(insert.chunk_rows.map_or(250, |limit| limit.get().min(250)));
    let mut rows = 0;
    let mut statement_len = 0;
    let mut tuple = Vec::new();
    read_rows(&mut |row| {
        if rows == 0 {
            writeln!(out, "SET SESSION sql_mode = 'NO_AUTO_VALUE_ON_ZERO';")
                .map_err(output_error)?;
            if disable_autocommit {
                writeln!(out, "SET @harbor_dump_autocommit = @@SESSION.autocommit;\nSET SESSION autocommit = 0;")
                    .map_err(output_error)?;
            }
            if insert.lock_tables {
                writeln!(out, "LOCK TABLES {name} WRITE;").map_err(output_error)?;
            }
            if disable_keys {
                writeln!(out, "/*!40000 ALTER TABLE {name} DISABLE KEYS */;")
                    .map_err(output_error)?;
            }
            if start_transaction {
                writeln!(out, "START TRANSACTION;").map_err(output_error)?;
            }
        }
        if insert.batch_limit == 0 {
            write!(out, "{prefix}").map_err(output_error)?;
            write_tuple(out, &data.columns, &row)?;
            writeln!(out, ";").map_err(output_error)?;
        } else {
            tuple.clear();
            write_tuple(&mut tuple, &data.columns, &row)?;
            if statement_len > 0 && statement_len + tuple.len() + 4 > insert.batch_limit {
                writeln!(out, ";").map_err(output_error)?;
                statement_len = 0;
            }
            if statement_len == 0 {
                write!(out, "{prefix}").map_err(output_error)?;
                statement_len = prefix.len();
            } else {
                writeln!(out, ",").map_err(output_error)?;
                statement_len += 2;
            }
            out.write_all(&tuple).map_err(output_error)?;
            statement_len += tuple.len();
        }
        rows += 1;
        if rows % progress_every == 0 {
            progress(rows)?;
        }
        Ok(())
    })?;
    if rows > 0 {
        if statement_len > 0 {
            writeln!(out, ";").map_err(output_error)?;
        }
        if !data.supports_delayed && insert.disable_autocommit != Some(false) {
            writeln!(out, "COMMIT;").map_err(output_error)?;
        }
        if disable_keys {
            writeln!(out, "/*!40000 ALTER TABLE {name} ENABLE KEYS */;").map_err(output_error)?;
        }
        if insert.lock_tables {
            writeln!(out, "UNLOCK TABLES;").map_err(output_error)?;
        }
        if disable_autocommit {
            writeln!(out, "SET SESSION autocommit = @harbor_dump_autocommit;")
                .map_err(output_error)?;
        }
        writeln!(out).map_err(output_error)?;
    }
    Ok(rows)
}

fn write_tuple(
    out: &mut impl Write,
    columns: &[DumpColumn],
    row: &[Option<Vec<u8>>],
) -> Result<(), String> {
    write!(out, "(").map_err(output_error)?;
    let mut offset = 0;
    for (index, column) in columns.iter().enumerate() {
        if index != 0 {
            write!(out, ", ").map_err(output_error)?;
        }
        let bytes = row
            .get(offset)
            .ok_or("服务器返回的数据列数不符。")?
            .as_deref();
        offset += 1;
        let srid = if matches!(column.kind, DataKind::Geometry) {
            let srid = row
                .get(offset)
                .ok_or("服务器返回的数据列数不符。")?
                .as_deref();
            offset += 1;
            srid.map(|bytes| {
                std::str::from_utf8(bytes)
                    .ok()
                    .and_then(|value| value.parse::<u32>().ok())
                    .ok_or("服务器返回的空间参考标识无效。")
            })
            .transpose()?
        } else {
            None
        };
        write_value(out, column.kind, bytes, srid)?;
    }
    if offset != row.len() {
        return Err("服务器返回的数据列数不符。".into());
    }
    write!(out, ")").map_err(output_error)
}

fn read_field<T: mysql::prelude::FromValue>(
    row: &mut Row,
    offset: usize,
) -> Result<Option<T>, String> {
    row.take_opt::<Option<T>, _>(offset)
        .ok_or("服务器返回的数据列数不符。")?
        .map_err(|error| mysql_error(error.into()))
}

fn write_value(
    out: &mut impl Write,
    kind: DataKind,
    bytes: Option<&[u8]>,
    srid: Option<u32>,
) -> Result<(), String> {
    let Some(bytes) = bytes else {
        return write!(out, "NULL").map_err(output_error);
    };
    match kind {
        DataKind::Number => {
            if !valid_number(bytes) {
                return Err("服务器返回了无效的数值。".into());
            }
            out.write_all(bytes).map_err(output_error)
        }
        DataKind::Binary => write_hex(out, bytes).map_err(output_error),
        DataKind::Text => {
            std::str::from_utf8(bytes).map_err(|_| "服务器返回了无效的 UTF-8 文本。")?;
            write!(out, "CONVERT(").map_err(output_error)?;
            write_hex(out, bytes).map_err(output_error)?;
            write!(out, " USING utf8mb4)").map_err(output_error)
        }
        DataKind::Geometry => {
            let srid = srid.ok_or("服务器返回的空间数据缺少 SRID。")?;
            write!(out, "ST_GeomFromWKB(").map_err(output_error)?;
            write_hex(out, bytes).map_err(output_error)?;
            write!(out, ", {srid})").map_err(output_error)
        }
    }
}

fn valid_number(bytes: &[u8]) -> bool {
    fn consume_digits(bytes: &[u8]) -> (&[u8], usize) {
        let count = bytes
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        (&bytes[count..], count)
    }
    let bytes = if matches!(bytes.first(), Some(b'+' | b'-')) {
        &bytes[1..]
    } else {
        bytes
    };
    let (mut rest, integer) = consume_digits(bytes);
    let mut fractional = 0;
    if let Some(after_dot) = rest.strip_prefix(b".") {
        (rest, fractional) = consume_digits(after_dot);
    }
    if integer == 0 && fractional == 0 {
        return false;
    }
    if matches!(rest.first(), Some(b'e' | b'E')) {
        let after_e = &rest[1..];
        let after_sign = if matches!(after_e.first(), Some(b'+' | b'-')) {
            &after_e[1..]
        } else {
            after_e
        };
        let (remainder, exponent) = consume_digits(after_sign);
        return exponent != 0 && remainder.is_empty();
    }
    rest.is_empty()
}

fn write_hex(out: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    out.write_all(b"X'")?;
    write_hex_digits(out, bytes)?;
    out.write_all(b"'")
}

const HEX_DIGITS: &[u8; 16] = b"0123456789ABCDEF";

fn write_hex_digits(out: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    let mut encoded = [0; 2048];
    for chunk in bytes.chunks(encoded.len() / 2) {
        for (index, byte) in chunk.iter().enumerate() {
            encoded[2 * index] = HEX_DIGITS[usize::from(byte >> 4)];
            encoded[2 * index + 1] = HEX_DIGITS[usize::from(byte & 15)];
        }
        out.write_all(&encoded[..2 * chunk.len()])?;
    }
    Ok(())
}

pub fn export(
    conn: &mut Conn,
    object: &DbObject,
    path: &Path,
    settings: &ExportSettings,
) -> Result<TransferSummary, String> {
    if !object.kind.is_tabular() {
        return Err("只能从表或视图导出数据。".into());
    }
    if settings.kind == ExportKind::Sql {
        if object.kind != ObjectKind::Table {
            return Err("SQL 语句导出需要使用基表。".into());
        }
        if settings.fields.is_some() || settings.sql != SqlExportMode::Data {
            return atomic_output(path, ExistingOutput::Replace, |out| {
                with_snapshot(conn, &object.schema, |conn| {
                    write_table_sql(out, &mut DumpSource::Native(conn), object, settings)
                })
            });
        }
        return dump(
            conn,
            &object.schema,
            Some(object),
            path,
            DumpOptions {
                include_structure: false,
                include_data: true,
                drop_existing: false,
            },
        );
    }
    let projection = export_projection(settings.fields.as_deref())?;
    atomic_output(path, ExistingOutput::Replace, |out| {
        with_snapshot(conn, &object.schema, |conn| {
            let query = format!("SELECT {projection} FROM {}", object.qualified_name());
            let result = conn.query_iter(query).map_err(mysql_error)?;
            let names: Vec<_> = result
                .columns()
                .as_ref()
                .iter()
                .map(|col| col.name_str().into_owned())
                .collect();
            let binary: Vec<_> = result
                .columns()
                .as_ref()
                .iter()
                .map(column_is_binary)
                .collect();
            let types = result
                .columns()
                .as_ref()
                .iter()
                .map(ExcelColumnType::from_native)
                .collect::<Vec<_>>();
            let widths = if settings.csv.fixed_width && settings.csv.declared_widths {
                result
                    .columns()
                    .as_ref()
                    .iter()
                    .map(ResultColumn::native_width)
                    .collect()
            } else {
                Vec::new()
            };
            let rows = result.map(|row| {
                let row = row.map_err(mysql_error)?.unwrap();
                if row.len() != binary.len() {
                    return Err("服务器返回的数据列数不符。".into());
                }
                Ok(row
                    .into_iter()
                    .zip(&binary)
                    .map(|(value, binary)| cell(value, *binary))
                    .collect())
            });
            let columns = TabularColumns {
                names: &names,
                types: &types,
                widths: &widths,
            };
            write_export(out, path, columns, rows, settings, false)
        })
    })
}

pub(super) fn export_sql_over_http(
    session: &HttpSession,
    object: &DbObject,
    path: &Path,
    settings: &ExportSettings,
) -> Result<TransferSummary, String> {
    if object.kind != ObjectKind::Table {
        return Err("SQL 语句导出需要使用基表。".into());
    }
    if settings.fields.is_some() || settings.sql != SqlExportMode::Data {
        return atomic_output(path, ExistingOutput::Replace, |out| {
            write_table_sql(out, &mut DumpSource::Http(session), object, settings)
        });
    }
    dump_over_http(
        session,
        &object.schema,
        path,
        DumpSettings {
            options: DumpOptions {
                include_structure: false,
                include_data: true,
                drop_existing: false,
            },
            objects: Some(vec![object.clone()]),
            overwrite: true,
            bulk_insert: false,
            ..Default::default()
        },
        |_| Ok(()),
    )
}

fn write_table_sql(
    out: &mut impl Write,
    source: &mut DumpSource<'_>,
    object: &DbObject,
    settings: &ExportSettings,
) -> Result<TransferSummary, String> {
    let title = match settings.sql {
        SqlExportMode::Data => "所选字段的插入语句。",
        SqlExportMode::Structure => "所选字段的简化建表语句。",
        SqlExportMode::StructureAndData => "所选字段的简化建表与插入语句。",
    };
    writeln!(out, "-- Harbor SQL {title}").map_err(output_error)?;
    if matches!(source, DumpSource::Http(_)) {
        writeln!(out, "-- HTTP 分批读取。导出期间请保持源数据和结构稳定。")
            .map_err(output_error)?;
    }
    write_dump_session_header(out, SqlDumpCharset::Utf8Mb4).map_err(output_error)?;
    let fields = settings.fields.as_deref();
    if settings.sql != SqlExportMode::Data {
        write_simple_create(out, source, object, fields)?;
    }
    let rows = if settings.sql == SqlExportMode::Structure {
        0
    } else {
        writeln!(
            out,
            "SET @harbor_dump_fk_checks = @@SESSION.foreign_key_checks;\nSET SESSION foreign_key_checks = 0;"
        )
        .map_err(output_error)?;
        let rows = source.dump_rows(
            out,
            object,
            false,
            InsertOutput {
                lock_tables: false,
                disable_autocommit: None,
                batch_limit: 0,
                complete: true,
                delayed: false,
                disable_keys: false,
                chunk_rows: None,
                fields,
            },
            |_| Ok(()),
        )?;
        writeln!(
            out,
            "SET SESSION foreign_key_checks = @harbor_dump_fk_checks;"
        )
        .map_err(output_error)?;
        rows
    };
    write!(out, "{DUMP_FOOTER}").map_err(output_error)?;
    Ok(TransferSummary {
        objects: 1,
        rows,
        ..Default::default()
    })
}

pub fn sql_export_writable(extra: &str) -> bool {
    !extra.split_ascii_whitespace().any(|flag| {
        ["VIRTUAL", "STORED", "PERSISTENT"]
            .iter()
            .any(|generated| flag.eq_ignore_ascii_case(generated))
    })
}

fn write_simple_create(
    out: &mut impl Write,
    source: &mut DumpSource<'_>,
    object: &DbObject,
    fields: Option<&[String]>,
) -> Result<(), String> {
    let detail = match source {
        DumpSource::Native(conn) => inspect_object(conn, object.clone()).map_err(Failure::message),
        DumpSource::Http(session) => session.inspect(object.clone()),
    }?;
    let mut columns = detail
        .columns
        .into_iter()
        .filter(|column| sql_export_writable(&column.extra))
        .collect::<Vec<_>>();
    if let Some(fields) = fields {
        let mut available = std::mem::take(&mut columns);
        for field in fields {
            let index = available
                .iter()
                .position(|column| column.name == *field)
                .ok_or_else(|| format!("所选字段「{field}」已不存在、不可写或重复。"))?;
            columns.push(available.swap_remove(index));
        }
    }
    if columns.is_empty() {
        return Err("请选择至少一个可写字段。".into());
    }
    let table = quote_identifier(&object.name);
    writeln!(out, "CREATE TABLE {table} (").map_err(output_error)?;
    for (index, column) in columns.iter().enumerate() {
        let separator = if index + 1 == columns.len() { "" } else { "," };
        let name = quote_identifier(&column.name);
        writeln!(out, "  {name} {}{separator}", column.data_type).map_err(output_error)?;
    }
    writeln!(out, ");").map_err(output_error)
}

pub(super) fn export_projection(fields: Option<&[String]>) -> Result<String, String> {
    let Some(fields) = fields else {
        return Ok("*".into());
    };
    let mut unique = HashSet::new();
    if fields.is_empty()
        || fields
            .iter()
            .any(|name| name.is_empty() || !unique.insert(name))
    {
        return Err("请至少选择一个字段，字段不能重复。".into());
    }
    Ok(fields
        .iter()
        .map(|name| quote_identifier(name))
        .collect::<Vec<_>>()
        .join(", "))
}

pub(super) fn export_rows(
    path: &Path,
    names: &[String],
    types: &[ExcelColumnType],
    rows: impl Iterator<Item = Result<Vec<Cell>, String>>,
    settings: &ExportSettings,
) -> Result<TransferSummary, String> {
    atomic_output(path, ExistingOutput::Replace, |out| {
        let columns = TabularColumns {
            names,
            types,
            widths: &[],
        };
        write_export(out, path, columns, rows, settings, false)
    })
}

pub fn export_result(
    path: &Path,
    result: &ResultSet,
    indices: &[usize],
    settings: &ExportSettings,
) -> Result<TransferSummary, String> {
    ResultExport::from(result).export(path, indices, settings)
}

/// Borrows the loaded data without reading more rows or duplicating cell values.
#[derive(Clone, Copy)]
pub enum ResultExport<'a> {
    Result(&'a ResultSet),
    Table(&'a TablePage),
}

impl<'a> From<&'a ResultSet> for ResultExport<'a> {
    fn from(result: &'a ResultSet) -> Self {
        Self::Result(result)
    }
}

impl<'a> From<&'a TablePage> for ResultExport<'a> {
    fn from(page: &'a TablePage) -> Self {
        Self::Table(page)
    }
}

impl<'a> ResultExport<'a> {
    pub fn column_count(self) -> usize {
        match self {
            Self::Result(result) => result.columns.len(),
            Self::Table(page) => page.columns.len(),
        }
    }

    pub fn column_label(self, index: usize) -> &'a str {
        match self {
            Self::Result(result) => &result.columns[index],
            Self::Table(page) => page.column_label(index),
        }
    }

    pub fn row_count(self) -> usize {
        match self {
            Self::Result(result) => result.rows.len(),
            Self::Table(page) => page.rows.len(),
        }
    }

    pub fn truncated(self) -> bool {
        match self {
            Self::Result(result) => result.truncated,
            Self::Table(page) => page.offset != 0 || page.has_more,
        }
    }

    pub fn has_declared_widths(self) -> bool {
        (0..self.column_count()).any(|index| self.declared_width(index).is_some())
    }

    fn declared_width(self, index: usize) -> Option<usize> {
        match self {
            Self::Result(result) => result.export_columns.get(index)?.declared_width,
            Self::Table(page) => page.columns.get(index)?.declared_width,
        }
    }

    fn excel_type(self, index: usize) -> ExcelColumnType {
        match self {
            Self::Result(result) => result.excel_types.get(index).copied().unwrap_or_default(),
            Self::Table(page) => ExcelColumnType::from_type_name(&page.columns[index].data_type),
        }
    }

    fn export_column(self, index: usize) -> ResultColumn {
        match self {
            Self::Result(result) => result
                .export_columns
                .get(index)
                .copied()
                .unwrap_or_default(),
            Self::Table(page) => ResultColumn::from_table_column(&page.columns[index]),
        }
    }

    fn rows(self) -> impl ExactSizeIterator<Item = &'a [Cell]> {
        (0..self.row_count()).map(move |index| match self {
            Self::Result(result) => result.rows[index].as_slice(),
            Self::Table(page) => page.rows[index].cells.as_slice(),
        })
    }

    pub fn export(
        self,
        path: &Path,
        indices: &[usize],
        settings: &ExportSettings,
    ) -> Result<TransferSummary, String> {
        if indices.iter().any(|&index| index >= self.column_count()) {
            return Err("选择的字段已不存在。".into());
        }
        let names = indices
            .iter()
            .map(|&index| self.column_label(index).to_owned())
            .collect::<Vec<_>>();
        if settings.kind == ExportKind::Sql {
            let target = settings
                .result_target
                .as_ref()
                .ok_or("请输入 SQL 导出的目标表名。")?;
            return atomic_output(path, ExistingOutput::Replace, |out| {
                result_sql::write_result_sql(out, self, indices, &names, target, settings.sql)
            });
        }
        let types = indices
            .iter()
            .map(|&index| self.excel_type(index))
            .collect::<Vec<_>>();
        let widths = if settings.csv.fixed_width && settings.csv.declared_widths {
            indices
                .iter()
                .map(|&index| self.declared_width(index))
                .collect()
        } else {
            Vec::new()
        };
        let rows = self.rows().map(|row| {
            indices
                .iter()
                .map(|&index| {
                    row.get(index)
                        .ok_or_else(|| "查询结果中的列数不符。".into())
                })
                .collect()
        });
        atomic_output(path, ExistingOutput::Replace, |out| {
            let columns = TabularColumns {
                names: &names,
                types: &types,
                widths: &widths,
            };
            write_export(out, path, columns, rows, settings, self.truncated())
        })
    }
}

struct TabularColumns<'a> {
    names: &'a [String],
    types: &'a [ExcelColumnType],
    widths: &'a [Option<usize>],
}

fn write_export<C: Borrow<Cell>>(
    out: &mut impl Write,
    path: &Path,
    columns: TabularColumns<'_>,
    rows: impl Iterator<Item = Result<Vec<C>, String>>,
    settings: &ExportSettings,
    truncated: bool,
) -> Result<TransferSummary, String> {
    let TabularColumns {
        names,
        types,
        widths,
    } = columns;
    if names.is_empty() {
        return Err("请选择至少一个导出字段。".into());
    }
    let rows = rows.map(|row| {
        let row = row?;
        if row.len() != names.len() {
            return Err("服务器返回的数据列数不符。".into());
        }
        Ok(row)
    });
    let count = match settings.kind {
        ExportKind::Csv => {
            let directory = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            tabular_csv::export_csv(out, names, rows, &settings.csv, widths, directory)?
        }
        ExportKind::ExcelXml => {
            tabular_excel::export_excel(out, names, types, rows, settings.excel)?
        }
        ExportKind::Sql => return Err("SQL 转储需要使用对象定义和列类型。".into()),
        ExportKind::Json => export_json(out, names, rows, truncated)?,
        kind => export_markup(out, names, rows, kind, truncated)?,
    };
    Ok(TransferSummary {
        objects: 1,
        rows: count,
        ..Default::default()
    })
}

fn export_json<C: Borrow<Cell>>(
    out: &mut impl Write,
    names: &[String],
    rows: impl Iterator<Item = Result<Vec<C>, String>>,
    truncated: bool,
) -> Result<u64, String> {
    write!(out, "{{\n  \"columns\": ").map_err(output_error)?;
    serde_json::to_writer(&mut *out, names).map_err(|error| error.to_string())?;
    write!(out, ",\n  \"rows\": [").map_err(output_error)?;
    let mut count = 0;
    for row in rows {
        let row = row?;
        let fields: Vec<_> = row
            .iter()
            .map(|cell| match cell.borrow() {
                Cell::Null => serde_json::Value::Null,
                Cell::Text(text) => serde_json::Value::String(text.clone()),
                Cell::Binary(bytes) => {
                    serde_json::json!({ "binary_hex": hex_text(bytes).to_ascii_lowercase() })
                }
            })
            .collect();
        if count > 0 {
            write!(out, ",").map_err(output_error)?;
        }
        write!(out, "\n    ").map_err(output_error)?;
        serde_json::to_writer(&mut *out, &fields).map_err(|error| error.to_string())?;
        count += 1;
    }
    writeln!(out, "\n  ],\n  \"truncated\": {truncated}\n}}").map_err(output_error)?;
    Ok(count)
}

pub(crate) fn display_value(cell: &Cell) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    match cell {
        Cell::Null => Cow::Borrowed("NULL"),
        Cell::Text(text) => Cow::Borrowed(text),
        Cell::Binary(bytes) => Cow::Owned(format!("0x{}", hex_text(bytes))),
    }
}

fn hex_text(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(HEX_DIGITS[usize::from(byte & 15)]));
    }
    output
}

fn export_markup<C: Borrow<Cell>>(
    out: &mut impl Write,
    names: &[String],
    rows: impl Iterator<Item = Result<Vec<C>, String>>,
    kind: ExportKind,
    truncated: bool,
) -> Result<u64, String> {
    match kind {
        ExportKind::Html => {
            writeln!(out, "<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\"><title>数据导出</title>{HTML_STYLE}<table><thead><tr>").map_err(output_error)?;
            for name in names {
                writeln!(out, "<th scope=\"col\">{}</th>", html_text(name))
                    .map_err(output_error)?;
            }
            writeln!(out, "</tr></thead><tbody>").map_err(output_error)?;
        }
        ExportKind::Xml => {
            writeln!(
                out,
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<result truncated=\"{truncated}\"><columns>"
            )
            .map_err(output_error)?;
            for name in names {
                write_xml_text(out, "column", name).map_err(output_error)?;
            }
            writeln!(out, "</columns><rows>").map_err(output_error)?;
        }
        _ => unreachable!("only markup formats enter export_markup"),
    }
    let mut count = 0;
    for row in rows {
        write_markup_row(out, &row?, kind).map_err(output_error)?;
        count += 1;
    }
    let end = match kind {
        ExportKind::Html => "</tbody></table></html>",
        ExportKind::Xml => "</rows></result>",
        _ => unreachable!("only markup formats enter export_markup"),
    };
    writeln!(out, "{end}").map_err(output_error)?;
    Ok(count)
}

fn write_markup_row<C: Borrow<Cell>>(
    out: &mut impl Write,
    row: &[C],
    kind: ExportKind,
) -> io::Result<()> {
    let cells = row.iter().map(Borrow::borrow);
    match kind {
        ExportKind::Html => {
            write!(out, "<tr>")?;
            for cell in cells {
                write!(out, "<td>{}</td>", html_text(&display_value(cell)))?;
            }
            writeln!(out, "</tr>")
        }
        ExportKind::Xml => {
            write!(out, "<row>")?;
            for cell in cells {
                match cell {
                    Cell::Null => write!(out, "<cell null=\"true\"/>")?,
                    Cell::Text(text) => write_xml_text(out, "cell", text)?,
                    Cell::Binary(bytes) => write!(
                        out,
                        "<cell encoding=\"binary-hex\">{}</cell>",
                        hex_text(bytes)
                    )?,
                }
            }
            writeln!(out, "</row>")
        }
        _ => unreachable!("only markup formats enter write_markup_row"),
    }
}

fn xml_text_is_valid(value: &str) -> bool {
    value.chars().all(|char| matches!(char, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}'))
}

fn write_xml_text(out: &mut impl Write, tag: &str, value: &str) -> io::Result<()> {
    if xml_text_is_valid(value) {
        write!(
            out,
            "<{tag}>{}</{tag}>",
            html_text(value).replace('\r', "&#xD;")
        )
    } else {
        write!(
            out,
            "<{tag} encoding=\"utf8-hex\">{}</{tag}>",
            hex_text(value.as_bytes())
        )
    }
}

pub fn schema_html(conn: &mut Conn, schema: &str, path: &Path) -> Result<TransferSummary, String> {
    schema_html_with_tables(conn, schema, path, None)
}

pub(super) fn schema_html_with_tables(
    conn: &mut Conn,
    schema: &str,
    path: &Path,
    tables: Option<&[String]>,
) -> Result<TransferSummary, String> {
    atomic_output(path, ExistingOutput::Replace, |out| {
        with_snapshot(conn, schema, |conn| {
            let objects = schema_html_objects(
                list_objects(conn, schema).map_err(Failure::message)?,
                tables,
            )?;
            write_schema_html(out, schema, objects, |object| {
                inspect_object(conn, object).map_err(Failure::message)
            })
        })
    })
}

pub(super) fn schema_html_for(
    schema: &str,
    path: &Path,
    objects: Vec<DbObject>,
    tables: Option<&[String]>,
    inspect: impl FnMut(DbObject) -> Result<ObjectDetail, String>,
) -> Result<TransferSummary, String> {
    let objects = schema_html_objects(objects, tables)?;
    atomic_output(path, ExistingOutput::Replace, |out| {
        write_schema_html(out, schema, objects, inspect)
    })
}

fn schema_html_objects(
    objects: Vec<DbObject>,
    tables: Option<&[String]>,
) -> Result<Vec<DbObject>, String> {
    let Some(tables) = tables else {
        return Ok(objects);
    };
    if tables.is_empty() {
        return Err("请至少选择一张数据表。".into());
    }
    let names: HashSet<_> = tables.iter().map(String::as_str).collect();
    let selected: Vec<_> = objects
        .into_iter()
        .filter(|object| object.kind == ObjectKind::Table && names.contains(object.name.as_str()))
        .collect();
    if selected.len() != names.len() {
        return Err("所选数据表已变化，请重新读取数据表后再选择。".into());
    }
    Ok(selected)
}

fn write_schema_html(
    out: &mut impl Write,
    schema: &str,
    objects: Vec<DbObject>,
    mut inspect: impl FnMut(DbObject) -> Result<ObjectDetail, String>,
) -> Result<TransferSummary, String> {
    let title = html_text(schema);
    writeln!(out, "<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\">\n<title>{title} 数据库架构</title>\n{HTML_STYLE}\n<h1>{title}</h1>\n<nav><ul>")
                .map_err(output_error)?;
    for (index, object) in objects.iter().enumerate() {
        let kind = kind_text(object.kind);
        writeln!(
            out,
            "<li><a href=\"#object-{index}\">{}</a> <small>{kind}</small></li>",
            html_text(&object.name)
        )
        .map_err(output_error)?;
    }
    writeln!(out, "</ul></nav><main>").map_err(output_error)?;
    let summary = TransferSummary {
        objects: objects.len(),
        ..Default::default()
    };
    for (index, object) in objects.into_iter().enumerate() {
        let name = html_text(&object.name);
        let kind = kind_text(object.kind);
        let detail = inspect(object)?;
        writeln!(
            out,
            "<section id=\"object-{index}\"><h2>{name} <small>{kind}</small></h2>"
        )
        .map_err(output_error)?;
        if !detail.columns.is_empty() {
            writeln!(out, "<table><thead><tr><th>列</th><th>类型</th><th>可为 NULL</th><th>键</th><th>默认值</th><th>附加属性</th></tr></thead><tbody>").map_err(output_error)?;
            for column in detail.columns {
                let nullable = if column.nullable { "是" } else { "否" };
                writeln!(out, "<tr><td>{}</td><td>{}</td><td>{nullable}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                            html_text(&column.name), html_text(&column.data_type), html_text(&column.key),
                            html_text(column.default_value.as_deref().unwrap_or("—")), html_text(&column.extra),
                        ).map_err(output_error)?;
            }
            writeln!(out, "</tbody></table>").map_err(output_error)?;
        }
        if let Some(error) = detail.ddl_error {
            writeln!(out, "<p>{}</p>", html_text(&error)).map_err(output_error)?;
        } else {
            writeln!(out, "<pre>{}</pre>", html_text(&detail.ddl)).map_err(output_error)?;
        }
        writeln!(out, "</section>").map_err(output_error)?;
    }
    writeln!(out, "</main></html>").map_err(output_error)?;
    Ok(summary)
}

fn kind_text(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Table => "表",
        ObjectKind::View => "视图",
        ObjectKind::Procedure => "存储过程",
        ObjectKind::Function => "函数",
        ObjectKind::Trigger => "触发器",
        ObjectKind::Event => "事件",
    }
}

const HTML_STYLE: &str =
    "<style>body{margin:2rem auto;max-width:72rem;padding:0 1rem;font:16px system-ui}
    section{margin:2rem 0}small{color:#53616e}table{border-collapse:collapse;width:100%}
    td,th{border-bottom:1px solid #d3dce4;text-align:left;padding:.4rem;vertical-align:top;white-space:pre-wrap}
    pre{background:#f4f6f8;padding:1rem;overflow:auto}li{margin:.2rem 0}</style>";

pub(crate) fn html_text(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#39;"),
            _ => output.push(character),
        }
    }
    output
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ExistingOutput {
    Reject,
    Replace,
    Append,
}

impl ExistingOutput {
    pub(crate) fn from_flags(overwrite: bool, append: bool) -> Result<Self, String> {
        match (overwrite, append) {
            (true, true) => Err("覆盖和追加只能选择其中一种。".into()),
            (true, false) => Ok(Self::Replace),
            (false, true) => Ok(Self::Append),
            (false, false) => Ok(Self::Reject),
        }
    }
}

const OUTPUT_LOCK_NAME: &str = ".harbor-sql-output.lck";

pub(crate) fn validate_output_filename(path: &Path) -> Result<(), String> {
    if path
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case(OUTPUT_LOCK_NAME))
    {
        return Err("请选择与导出锁文件不同的文件名。".into());
    }
    Ok(())
}

fn output_directory(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn atomic_output(
    path: &Path,
    mode: ExistingOutput,
    write: impl FnOnce(&mut BufWriter<&mut File>) -> Result<TransferSummary, String>,
) -> Result<TransferSummary, String> {
    validate_output_filename(path)?;
    let directory = output_directory(path);
    let mut temporary = NamedTempFile::new_in(directory).map_err(output_error)?;
    let mut summary = {
        let mut out = BufWriter::new(temporary.as_file_mut());
        let summary = write(&mut out)?;
        out.flush().map_err(output_error)?;
        summary
    };
    summary.bytes = temporary.as_file().metadata().map_err(output_error)?.len();
    publish_output(temporary, path, mode, || Ok(()))?;
    Ok(summary)
}

/// Returns the offset of the newly written bytes in the published file.
pub(crate) fn publish_output(
    mut temporary: NamedTempFile,
    path: &Path,
    mode: ExistingOutput,
    check_cancelled: impl Fn() -> Result<(), String>,
) -> Result<u64, String> {
    validate_output_filename(path)?;
    let directory = output_directory(path);
    let replace = match mode {
        ExistingOutput::Replace => match fs::symlink_metadata(path) {
            Ok(_) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(output_error(error)),
        },
        _ => false,
    };
    // Keep the lock file in place so every publisher acquires the same filesystem lock.
    // This allows generation to run in parallel while an append reads and replaces the SQL file.
    let _lock = (replace || mode == ExistingOutput::Append)
        .then(|| lock_output_directory(directory))
        .transpose()
        .map_err(output_error)?;
    check_cancelled()?;
    let mut offset = 0;
    let replace = if mode == ExistingOutput::Append
        && let Some((combined, start)) =
            combine_with_existing(path, directory, &temporary, &check_cancelled)?
    {
        temporary = combined;
        offset = start;
        true
    } else {
        replace
    };
    temporary.as_file().sync_all().map_err(output_error)?;
    check_cancelled()?;
    let saved = if replace {
        temporary.persist(path)
    } else {
        temporary.persist_noclobber(path)
    };
    saved.map_err(|error| output_error(error.error))?;
    Ok(offset)
}

fn lock_output_directory(directory: &Path) -> io::Result<File> {
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(OUTPUT_LOCK_NAME))?;
    lock.lock()?;
    Ok(lock)
}

fn combine_with_existing(
    path: &Path,
    directory: &Path,
    new_dump: &NamedTempFile,
    check_cancelled: &impl Fn() -> Result<(), String>,
) -> Result<Option<(NamedTempFile, u64)>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return Err("追加备份需要普通文件。".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(output_error(error)),
    };
    let mut original = File::options()
        .read(true)
        .write(true)
        .open(path)
        .map_err(output_error)?;
    let mut appended = new_dump.reopen().map_err(output_error)?;
    if metadata.len() != 0 && append_charset(&mut original)? != append_charset(&mut appended)? {
        return Err("现有 SQL 文件与本次转储的字符集不同，请选择新文件。".into());
    }
    let mut combined = NamedTempFile::new_in(directory).map_err(output_error)?;
    let mut offset = copy_output(&mut original, &mut combined, check_cancelled)?;
    if offset > 0 {
        combined.write_all(b"\n").map_err(output_error)?;
        offset += 1;
    }
    copy_output(&mut appended, &mut combined, check_cancelled)?;
    combined
        .as_file()
        .set_permissions(metadata.permissions())
        .map_err(output_error)?;
    Ok(Some((combined, offset)))
}

fn append_charset(file: &mut File) -> Result<SqlDumpCharset, String> {
    let mut prefix = Vec::new();
    Read::take(&mut *file, 128)
        .read_to_end(&mut prefix)
        .map_err(output_error)?;
    file.rewind().map_err(output_error)?;
    let line = prefix
        .split_inclusive(|&byte| byte == b'\n')
        .next()
        .unwrap_or(&[]);
    Ok(parse_marker(line)?.unwrap_or_default())
}

pub(crate) fn copy_output(
    input: &mut File,
    out: &mut impl Write,
    check_cancelled: &impl Fn() -> Result<(), String>,
) -> Result<u64, String> {
    let mut buffer = [0; 64 * 1024];
    let mut copied = 0;
    loop {
        check_cancelled()?;
        let length = match input.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result.map_err(output_error)?,
        };
        if length == 0 {
            return Ok(copied);
        }
        out.write_all(&buffer[..length]).map_err(output_error)?;
        copied += length as u64;
    }
}

fn output_error(error: io::Error) -> String {
    format!("写入导出文件失败：{error}")
}

#[cfg(test)]
mod delayed_tests {
    use std::time::{Duration, Instant};

    use super::super::http_session::workflow_test::fixture_database;
    use super::*;

    #[test]
    #[ignore = "需要一次性 MariaDB，端口由 HARBOR_HTTP_FIXTURE_PORT 指定"]
    fn restores_legacy_delayed_inserts_for_eligible_tables() {
        let (_, opts) = fixture_database();
        let mut conn = Conn::new(opts).unwrap();
        let schema = format!("harbor_delayed_{}", std::process::id());
        let restored = format!("{schema}_restored");
        for database in [&schema, &restored] {
            conn.query_drop(format!("CREATE DATABASE {database} CHARACTER SET utf8mb4"))
                .unwrap();
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            conn.select_db(&schema).unwrap();
            let cases = [
                ("myisam_ok", "ENGINE=MyISAM", "", true),
                ("aria_ok", "ENGINE=Aria TRANSACTIONAL=0", "", true),
                ("aria_tx", "ENGINE=Aria TRANSACTIONAL=1", "", false),
                ("innodb", "ENGINE=InnoDB", "", false),
                ("checked", "ENGINE=MyISAM", " CHECK (id > 0)", false),
                ("has_trigger", "ENGINE=MyISAM", "", false),
                (
                    "partitioned",
                    "ENGINE=MyISAM PARTITION BY HASH(id) PARTITIONS 2",
                    "",
                    false,
                ),
            ];
            for (name, engine, constraint, _) in cases {
                conn.query_drop(format!(
                    "CREATE TABLE {name} (id INT PRIMARY KEY{constraint}, val VARCHAR(30)) {engine}"
                ))
                .unwrap();
                conn.query_drop(format!("INSERT INTO {name} VALUES (1,'测试数据')"))
                    .unwrap();
            }
            conn.query_drop(
                "CREATE TRIGGER reads_as_regular BEFORE INSERT ON has_trigger FOR EACH ROW SET NEW.val = TRIM(NEW.val)",
            ).unwrap();

            let output =
                tempfile::tempdir_in(Path::new(env!("CARGO_MANIFEST_DIR")).join("target")).unwrap();
            let path = output.path().join("delayed.sql");
            let settings = DumpSettings {
                insert_delayed: true,
                ..DumpSettings::default()
            };
            let summary =
                dump_configured(&mut conn, &schema, &path, settings.clone(), |_| Ok(())).unwrap();
            assert_eq!(summary.rows, cases.len() as u64);
            let script = fs::read_to_string(&path).unwrap();
            for (name, _, _, delayed) in cases {
                let verb = if delayed { "INSERT DELAYED" } else { "INSERT" };
                assert!(
                    script
                        .lines()
                        .any(|line| line.starts_with(&format!("{verb} INTO `{name}`"))),
                    "{name}"
                );
            }
            assert_eq!(
                script
                    .lines()
                    .filter(|line| line.starts_with("INSERT DELAYED INTO"))
                    .count(),
                2
            );
            let delayed_writes = |conn: &mut Conn| {
                let status: (String, u64) = conn
                    .query_first("SHOW GLOBAL STATUS LIKE 'Delayed_writes'")
                    .unwrap()
                    .unwrap();
                status.1
            };
            let previous = delayed_writes(&mut conn);
            crate::restore::restore(&mut conn, &restored, &path).unwrap();
            for (name, _, _, _) in cases {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    let rows = conn
                        .query::<(u8, String), _>(format!("SELECT id,val FROM {restored}.{name}"))
                        .unwrap();
                    if rows == [(1, "测试数据".into())] {
                        break;
                    }
                    assert!(Instant::now() < deadline, "target table {name}: {rows:?}");
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            assert!(delayed_writes(&mut conn) >= previous + 2);

            let locked = output.path().join("locked.sql");
            dump_configured(
                &mut conn,
                &schema,
                &locked,
                DumpSettings {
                    lock_insert_tables: true,
                    ..settings
                },
                |_| Ok(()),
            )
            .unwrap();
            let script = fs::read_to_string(locked).unwrap();
            assert!(script.contains("LOCK TABLES `myisam_ok` WRITE"));
            assert!(
                !script
                    .lines()
                    .any(|line| line.starts_with("INSERT DELAYED INTO"))
            );
        }));
        for database in [&schema, &restored] {
            conn.query_drop(format!("DROP DATABASE IF EXISTS {database}"))
                .unwrap();
        }
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }
}
