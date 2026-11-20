use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{BuildHasher, RandomState};
use std::io::{self, BufRead, BufReader};
use std::net::{Ipv4Addr, Ipv6Addr, TcpListener};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::process::{Child, ChildStderr, Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use mysql::consts::ColumnType;
use mysql::prelude::Queryable;
use mysql::{
    Column, Conn, LocalInfileHandler, Opts, OptsBuilder, Row, SslOpts, TxOpts, Value, params,
};

mod accounts;
mod connection_clone;
#[cfg(test)]
mod connection_smoke;
mod copy_cycle_apply;
mod copy_cycle_data;
mod copy_cycle_target;
mod copy_restore;
mod design_indexes;
#[cfg(test)]
mod design_smoke;
#[cfg(test)]
mod dump_smoke;
pub(crate) mod editable_query;
mod http_session;
mod joined_editable;
mod joined_editable_http;
mod object_operations;
mod odbc_import;
mod odbc_source_action;
mod odbc_target;
mod password_ssh;
mod processes;
mod profiler;
mod result_duplicate;
mod row_editor;
mod schema_apply;
mod sync_dispatch;
mod sync_engine;
#[cfg(test)]
mod sync_smoke;
mod text_import;

pub use crate::restore::{RESTORE_CANCELLED, RestoreProgress};
pub use crate::sql_encoding::SqlDumpCharset;
pub use accounts::{
    AccountApplyResult, AccountCommand, AccountName, AccountReply, GrantScope, GrantSnapshot,
    PrivilegeChanges,
};
pub use http_session::HttpTunnelConfig;
pub use object_operations::{
    DatabaseContents, DatabaseContentsAction, DatabaseContentsPlan, DatabaseContentsResult,
    DatabaseDefaults,
};
pub use odbc_import::{
    OdbcColumnDefinition, odbc_create_table_sql, validate_odbc_create_table_sql,
};
pub(crate) use odbc_target::odbc_identifier_key;
pub use odbc_target::validate_odbc_engine;
pub use odbc_target::{
    OdbcConflict, OdbcForeignKey, OdbcImportCounts, OdbcIndex, OdbcReferentialAction, OdbcReset,
    OdbcTarget,
};
pub use processes::{ProcessAction, ProcessOverview, ProcessReply, ProcessRequest, ProcessRow};
pub use profiler::{ProfileOptions, QueryProfile};
pub use result_duplicate::ResultDuplicateSource;
pub use row_editor::{
    DuplicateChoice, DuplicateScope, FieldValue, TableColumn, TableEdit, TablePage, TablePredicate,
    TablePredicateOp, TableReadRange, TableRow, TableSelection, TableSort, preview_table_update,
};
pub use schema_apply::{
    SchemaApply, SchemaApplyProgress, SchemaApplyResult, SchemaApplyStop, SchemaChange,
    SchemaOrigin,
};
pub use sync_engine::{
    SyncConflict, SyncDifferenceKind, SyncFilter, SyncFilterScope, SyncGroupPlan, SyncGroupResult,
    SyncMapping, SyncMode, SyncPlan, SyncPolicy, SyncProgress, SyncResult, SyncRowError, SyncSide,
    SyncWrites,
};
pub use text_import::{
    LoadLocalConflict, LoadLocalFormat, LoadLocalImport, LoadLocalPriority, LoadLocalReport,
    LoadLocalWarning, TextImport,
};
pub const HTTP_CONTENT_TYPES: [&str; 4] = crate::http_tunnel::CONTENT_TYPES;

const MAX_RESULT_ROWS: usize = 10_000;
const MAX_RESULT_SETS: usize = 128;
const RESULT_MEMORY_LIMIT: usize = 32 * 1024 * 1024;
const MAX_SNAPSHOT_OBJECTS: usize = 512;
const SNAPSHOT_DEFINITION_LIMIT: usize = 8 * 1024 * 1024;
pub(crate) const MAX_IMPORT_ROWS: usize = 20_000;
pub const MAX_IMPORT_COLUMNS: usize = 4_096;
pub(crate) const IMPORT_MEMORY_LIMIT: usize = 32 * 1024 * 1024;
pub(crate) const IMPORT_CANCELLED: &str = "已取消数据导入。";
pub(crate) const IMPORT_UNCONFIRMED: &str = "导入结果需核对。";
pub const TABLE_READ_CANCELLED: &str = "已停止读取表数据。";
pub const EDITABLE_READ_CANCELLED: &str = "已停止读取可编辑查询结果。";
const SSH_TIMEOUT: Duration = Duration::from_secs(16);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(4);
const SESSION_ATTRIBUTE: &str = "harbor_sql_session";

#[derive(Clone)]
pub struct ConnectionConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: Option<String>,
    pub tls: Option<TlsConfig>,
    pub ssh: Option<SshConfig>,
    pub advanced: AdvancedConnection,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 3306,
            user: "root".into(),
            password: String::new(),
            database: None,
            tls: None,
            ssh: None,
            advanced: AdvancedConnection::default(),
        }
    }
}

#[derive(Clone, Default)]
pub struct TlsConfig {
    pub ca_path: Option<PathBuf>,
    pub client_identity: Option<TlsClientIdentity>,
}

#[derive(Clone)]
pub struct TlsClientIdentity {
    pub certificate: PathBuf,
    pub private_key: PathBuf,
}

#[derive(Clone)]
pub struct AdvancedConnection {
    pub compression: bool,
    pub wait_timeout: Option<u32>,
    pub keep_alive: Option<u32>,
    pub connect_timeout: u32,
    pub read_timeout: u32,
    pub sql_mode: Option<String>,
    pub init_sql: String,
}

impl Default for AdvancedConnection {
    fn default() -> Self {
        Self {
            compression: false,
            wait_timeout: None,
            keep_alive: None,
            connect_timeout: 10,
            read_timeout: 120,
            sql_mode: None,
            init_sql: String::new(),
        }
    }
}

#[derive(Clone)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub identity_file: Option<PathBuf>,
    pub password: Option<String>,
    pub key_passphrase: Option<String>,
    pub known_hosts_path: Option<PathBuf>,
    pub local_port: Option<u16>,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 22,
            user: String::new(),
            identity_file: None,
            password: None,
            key_passphrase: None,
            known_hosts_path: None,
            local_port: None,
        }
    }
}

pub enum Command {
    Connect(ConnectionConfig),
    ConnectHttp(ConnectionConfig, HttpTunnelConfig),
    CloneConnection,
    TestConnection {
        config: ConnectionConfig,
        http: Option<HttpTunnelConfig>,
    },
    Disconnect,
    ListSchemas,
    ListEngines,
    ListObjects {
        schema: String,
    },
    ReadDatabaseDefaults {
        schema: Option<String>,
    },
    ReadDatabaseContents {
        schema: String,
        action: DatabaseContentsAction,
    },
    ApplyDatabaseContents {
        plan: DatabaseContentsPlan,
    },
    Accounts(AccountCommand),
    Processes(ProcessRequest),
    ServerValues(crate::maintenance::ServerView),
    SnapshotSchema {
        schema: String,
    },
    ApplySchema {
        apply: Box<SchemaApply>,
    },
    DescribeSchemaDesign {
        schema: String,
    },
    DescribeTableKeys {
        object: DbObject,
    },
    SnapshotDesignTables {
        schema: String,
        names: Vec<String>,
    },
    SyncEndpoint,
    PlanSync {
        source: DbObject,
        target: DbObject,
        endpoint: SyncEndpoint,
        mode: SyncMode,
    },
    PlanScopedSync {
        mapping: SyncMapping,
        endpoint: SyncEndpoint,
        policy: SyncPolicy,
    },
    ApplySync {
        prepared: Box<PreparedSync>,
    },
    PlanSyncGroup {
        pairs: Vec<(DbObject, DbObject)>,
        endpoint: SyncEndpoint,
        mode: SyncMode,
    },
    PlanScopedSyncGroup {
        mappings: Vec<SyncMapping>,
        endpoint: SyncEndpoint,
        policy: SyncPolicy,
    },
    ApplySyncGroup {
        prepared: Box<PreparedSyncGroup>,
    },
    ExportSync {
        prepared: SyncScriptPlan,
        paths: SyncScriptPaths,
    },
    Dump {
        schema: String,
        object: Option<DbObject>,
        path: PathBuf,
        options: transfer::DumpOptions,
        chunk_rows: Option<NonZeroU32>,
        bulk_insert_limit_kib: Option<NonZeroU32>,
    },
    DumpConfigured {
        schema: String,
        path: PathBuf,
        settings: transfer::DumpSettings,
    },
    DumpForCopy {
        schema: String,
        object: Option<DbObject>,
        objects: Option<Vec<DbObject>>,
        destination_schema: String,
        path: PathBuf,
        options: transfer::DumpOptions,
        bulk_insert: bool,
    },
    CopyTableAs {
        object: DbObject,
        settings: transfer::TableCopySettings,
    },
    SchemaHtml {
        schema: String,
        path: PathBuf,
    },
    SchemaHtmlTables {
        schema: String,
        tables: Vec<String>,
        path: PathBuf,
    },
    Export {
        object: DbObject,
        path: PathBuf,
        settings: transfer::ExportSettings,
    },
    ExportColumns {
        object: DbObject,
    },
    Restore {
        schema: String,
        path: PathBuf,
    },
    RestoreForCopy {
        schema: String,
        path: PathBuf,
        source_schema: String,
        source: SchemaOrigin,
        dependencies: transfer::CopyDependencies,
    },
    RestoreEncoded {
        schema: String,
        path: PathBuf,
        charset: SqlDumpCharset,
    },
    UseSchema {
        schema: String,
    },
    Inspect {
        object: DbObject,
    },
    Preview {
        object: DbObject,
        limit: usize,
    },
    TablePage {
        object: DbObject,
        limit: usize,
        offset: usize,
    },
    TablePageSelected {
        object: DbObject,
        limit: usize,
        offset: usize,
        selection: TableSelection,
    },
    ReadTableData {
        object: DbObject,
        selection: TableSelection,
        range: TableReadRange,
    },
    EditTable {
        object: DbObject,
        edit: TableEdit,
    },
    ConfirmTableEdit {
        object: DbObject,
        edit: TableEdit,
        choice: DuplicateChoice,
    },
    DeleteTableRows {
        object: DbObject,
        originals: Vec<TableRow>,
    },
    Query {
        sql: String,
        limit: usize,
    },
    QueryInSeparateSession {
        sql: String,
        limit: usize,
    },
    FlushResources(crate::maintenance::FlushPlan),
    QueryProfiled {
        sql: String,
        limit: usize,
        options: ProfileOptions,
        open_profile: bool,
    },
    QueryEditable {
        sql: String,
        limit: usize,
    },
    QueryF8 {
        sql: String,
        limit: usize,
    },
    DuplicateResultRow {
        source: ResultDuplicateSource,
        cells: Vec<Cell>,
    },
    ImportRows {
        object: DbObject,
        columns: Vec<String>,
        rows: Vec<Vec<Option<String>>>,
    },
    ImportRowsControlled {
        object: DbObject,
        columns: Vec<String>,
        rows: Vec<Vec<Option<String>>>,
        cancel: Arc<AtomicBool>,
    },
    ImportDelimitedText(TextImport),
    ImportLocalFile(LoadLocalImport),
    ImportOdbcTables {
        tables: Vec<OdbcTarget>,
        cancel: Arc<AtomicBool>,
    },
    ImportOdbcTableWithSourceAction {
        table: OdbcTarget,
        source: crate::odbc::PreparedSourceAction,
        cancel: Arc<AtomicBool>,
    },
}

pub struct Event {
    pub id: u64,
    pub result: Result<Response, String>,
    pub connected: bool,
}

pub struct CancelEvent {
    pub id: u64,
    pub result: Result<(), String>,
}

pub enum Response {
    Connected(ServerInfo),
    ClonedConnection {
        worker: Worker,
        request: u64,
    },
    ConnectionTest(ConnectionProbe),
    Disconnected,
    Schemas(Vec<String>),
    Engines {
        names: Vec<String>,
        no_backslash_escapes: bool,
        raid_default_engine: Option<String>,
    },
    Objects {
        schema: String,
        objects: Vec<DbObject>,
    },
    DatabaseDefaults(DatabaseDefaults),
    DatabaseContents(DatabaseContents),
    DatabaseContentsApplied(DatabaseContentsResult),
    Accounts(Result<AccountReply, String>),
    Processes(Result<ProcessReply, String>),
    ServerValues {
        view: crate::maintenance::ServerView,
        values: Result<Vec<ServerValue>, String>,
    },
    ServerResourcesFlushed(usize),
    Snapshot(SchemaSnapshot),
    SchemaApplied(SchemaApplyResult),
    SchemaDesign(SchemaDesign),
    TableKeys(TableKeys),
    SyncEndpoint(SyncEndpoint),
    SyncPlan(Box<PreparedSync>),
    SyncApplied(SyncResult),
    SyncGroupPlan(Box<PreparedSyncGroup>),
    SyncGroupApplied(SyncGroupResult),
    SyncScripts {
        prepared: SyncScriptPlan,
        files: Vec<SyncScriptFile>,
    },
    Transferred(transfer::TransferSummary),
    Restored(usize),
    SchemaChanged(String),
    Inspected(ObjectDetail),
    ExportColumns(ObjectDetail),
    Previewed(QueryReport),
    TablePage(TablePage),
    EditablePage(TablePage),
    JoinedEditablePage(JoinedEditablePage),
    ResultRowDuplicate(TablePage),
    TableEdited {
        affected_rows: u64,
    },
    TableEditConfirmation {
        matches: u64,
    },
    Queried(QueryReport),
    Profiled {
        report: QueryReport,
        profile: Box<QueryProfile>,
        open_profile: bool,
    },
    Imported(u64),
    TextImported(OdbcImportCounts),
    LocalFileImported(LoadLocalReport),
    OdbcImported {
        counts: OdbcImportCounts,
        created_in: Option<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerInfo {
    pub version: String,
    pub connection_id: u32,
    pub database: Option<String>,
    pub http_tunnel: bool,
}

pub struct ConnectionProbe {
    pub version: String,
    pub database: Option<String>,
    pub elapsed: Duration,
    pub transport: String,
    pub compression: Option<bool>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DbObject {
    pub schema: String,
    pub name: String,
    pub kind: ObjectKind,
}

impl DbObject {
    fn qualified_name(&self) -> String {
        format!(
            "{}.{}",
            quote_identifier(&self.schema),
            quote_identifier(&self.name)
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ObjectKind {
    Table,
    View,
    Procedure,
    Function,
    Trigger,
    Event,
}

impl ObjectKind {
    fn is_tabular(self) -> bool {
        matches!(self, Self::Table | Self::View)
    }

    fn show_create(self) -> (&'static str, &'static str) {
        match self {
            Self::Table => ("TABLE", "Create Table"),
            Self::View => ("VIEW", "Create View"),
            Self::Procedure => ("PROCEDURE", "Create Procedure"),
            Self::Function => ("FUNCTION", "Create Function"),
            Self::Trigger => ("TRIGGER", "SQL Original Statement"),
            Self::Event => ("EVENT", "Create Event"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ObjectDetail {
    pub object: DbObject,
    pub columns: Vec<ColumnInfo>,
    pub ddl: String,
    pub ddl_error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SchemaSnapshot {
    pub schema: String,
    pub defaults: Option<SchemaDefaults>,
    pub objects: Vec<SnapshotObject>,
    pub omitted_objects: usize,
    pub context: Option<SnapshotContext>,
    pub foreign_keys: Option<Vec<SchemaForeignKey>>,
    pub origin: Option<SchemaOrigin>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaDefaults {
    pub charset: String,
    pub collation: String,
}

#[derive(Clone, Debug)]
pub struct SnapshotObject {
    pub object: DbObject,
    pub definition: Result<String, String>,
    pub context: Option<DefinitionContext>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotContext {
    pub lower_case_table_names: u8,
    pub sql_mode: String,
    pub character_set_client: String,
    pub collation_connection: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DefinitionContext {
    pub sql_mode: Option<String>,
    pub character_set_client: Option<String>,
    pub collation_connection: Option<String>,
    pub time_zone: Option<String>,
    pub trigger_action_order: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub key: String,
    pub default_value: Option<String>,
    pub extra: String,
}

#[derive(Clone, Debug)]
pub struct SchemaDesign {
    pub schema: String,
    pub tables: Vec<SchemaDesignTable>,
    pub foreign_keys: Vec<SchemaForeignKey>,
}

#[derive(Clone, Debug)]
pub struct SchemaDesignTable {
    pub object: DbObject,
    pub columns: Vec<ColumnInfo>,
    pub unique_indexes: Option<Vec<SchemaUniqueIndex>>,
}

#[derive(Clone, Debug)]
pub struct SchemaUniqueIndex {
    pub name: String,
    pub columns: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaForeignKey {
    pub table: DbObject,
    pub name: String,
    pub referenced_table: DbObject,
    pub columns: Vec<(String, String)>,
    pub on_delete: String,
    pub on_update: String,
}

#[derive(Clone, Debug)]
pub struct TableKeys {
    pub object: DbObject,
    pub columns: Vec<ColumnInfo>,
    pub indexes: Vec<IndexDefinition>,
    pub foreign_keys: Vec<SchemaForeignKey>,
    pub visibility: Option<IndexVisibility>,
    pub no_backslash_escapes: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum IndexVisibility {
    MySql,
    MariaDb,
}

#[derive(Clone, Debug)]
pub struct IndexDefinition {
    pub name: String,
    pub unique: bool,
    pub method: String,
    pub parts: Vec<IndexPart>,
    pub comment: String,
    pub visible: bool,
    pub standard_options: bool,
}

#[derive(Clone, Debug)]
pub struct IndexPart {
    pub column: Option<String>,
    pub expression: Option<String>,
    pub prefix: Option<u64>,
    pub descending: bool,
}

#[derive(Clone)]
pub struct SyncEndpoint {
    transport: sync_engine::Transport,
    session: Weak<()>,
    _system_roots: Option<Arc<tempfile::TempPath>>,
}

pub struct PreparedSync {
    pub plan: SyncPlan,
    target: SyncEndpoint,
    source_session: Weak<()>,
}

pub struct PreparedSyncGroup {
    pub plan: SyncGroupPlan,
    target: SyncEndpoint,
    source_session: Weak<()>,
}

pub enum SyncScriptPlan {
    Single(Box<PreparedSync>),
    Group(Box<PreparedSyncGroup>),
}

#[derive(Default)]
pub struct SyncScriptPaths {
    pub source: Option<PathBuf>,
    pub target: Option<PathBuf>,
    pub replace: bool,
}

pub struct SyncScriptFile {
    pub side: SyncSide,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub struct QueryDiagnostics {
    /// Client time through receipt of the first result metadata or server status.
    pub execution: Option<Duration>,
    /// Diagnostics for retained results in the same order as `QueryReport::sets`.
    pub results: Vec<ResultDiagnostics>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueryTiming {
    /// Client time through receipt of the first result metadata on a direct connection.
    pub execution: Option<Duration>,
    /// Client time through receipt of the selected result rows.
    pub elapsed: Duration,
}

#[derive(Clone, Debug)]
pub struct ResultDiagnostics {
    /// Cumulative client time through reading this result.
    pub elapsed: Duration,
    /// Server warning count from an immediate OK packet, when present.
    pub warning_count: Option<u16>,
}

#[derive(Clone, Debug, Default)]
pub struct QueryReport {
    pub sets: Vec<ResultSet>,
    pub elapsed: Duration,
    pub diagnostics: QueryDiagnostics,
    pub error: Option<String>,
    /// A server error that permits continuing an editor batch on the same session.
    pub continue_after_error: bool,
    pub omitted_sets: usize,
    pub active_database: Option<String>,
}

impl QueryReport {
    fn from_read_only_result(result: ResultSet, timing: Option<QueryTiming>) -> Self {
        let mut report = Self {
            sets: vec![result],
            ..Self::default()
        };
        if let Some(timing) = timing {
            report.elapsed = timing.elapsed;
            report.diagnostics.execution = timing.execution;
            if timing.execution.is_some() {
                report.diagnostics.results.push(ResultDiagnostics {
                    elapsed: timing.elapsed,
                    warning_count: None,
                });
            }
        }
        report
    }
}

#[derive(Clone, Debug)]
pub struct ResultSet {
    pub source_sql: Option<Arc<str>>,
    pub columns: Vec<String>,
    pub column_hints: Vec<String>,
    /// Original column identifiers by result position, when the source is known.
    pub column_origins: Vec<Option<ColumnOrigin>>,
    pub excel_types: Vec<transfer::ExcelColumnType>,
    pub export_columns: Vec<transfer::ResultColumn>,
    pub duplicate_source: Option<ResultDuplicateSource>,
    pub notice: Option<String>,
    pub rows: Vec<Vec<Cell>>,
    pub affected_rows: u64,
    pub last_insert_id: Option<u64>,
    pub truncated: bool,
}

pub struct ServerValue {
    pub name: String,
    pub value: Cell,
}

fn server_values(set: ResultSet) -> Result<Vec<ServerValue>, String> {
    if set.truncated || set.columns.len() != 2 {
        return Err("服务器返回的变量或状态列表不完整。".into());
    }
    set.rows
        .into_iter()
        .map(|row| {
            let [name, value] =
                <[Cell; 2]>::try_from(row).map_err(|_| "服务器返回了无效的变量或状态行。")?;
            let name = match name {
                Cell::Text(name) => name,
                Cell::Binary(name) => {
                    String::from_utf8(name).map_err(|_| "服务器返回了无效的变量或状态名称。")?
                }
                Cell::Null => return Err("服务器返回了空的变量或状态名称。".into()),
            };
            Ok(ServerValue { name, value })
        })
        .collect()
}

impl ResultSet {
    /// Reports the server's BLOB/TEXT type independently of this column's values.
    pub fn long_value_column(&self, index: usize) -> bool {
        self.export_columns
            .get(index)
            .is_some_and(transfer::ResultColumn::is_long_value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColumnOrigin {
    schema: String,
    table: String,
    column: String,
}

impl ColumnOrigin {
    fn new(schema: &str, table: &str, column: &str) -> Self {
        Self {
            schema: schema.into(),
            table: table.into(),
            column: column.into(),
        }
    }

    fn from_native(field: &Column) -> Option<Self> {
        let schema = std::str::from_utf8(field.schema_ref()).ok()?;
        let table = std::str::from_utf8(field.org_table_ref()).ok()?;
        let column = std::str::from_utf8(field.org_name_ref()).ok()?;
        if schema.is_empty() || table.is_empty() || column.is_empty() {
            return None;
        }
        Some(Self::new(schema, table, column))
    }

    pub fn identifiers(&self) -> (&str, &str, &str) {
        (&self.schema, &self.table, &self.column)
    }
}

pub struct JoinedEditablePage {
    pub result: ResultSet,
    pub targets: Vec<JoinedEditTarget>,
    pub query_timing: Option<QueryTiming>,
}

pub struct JoinedEditTarget {
    pub alias: String,
    pub page: TablePage,
    /// Map original result rows to deduplicated rows in `page`.
    pub result_rows: Vec<Option<usize>>,
    /// Map original result columns to the corresponding columns in `page`.
    pub result_columns: Vec<Option<usize>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Cell {
    Null,
    Text(String),
    Binary(Vec<u8>),
}

pub fn quote_identifier(identifier: &str) -> String {
    format!("`{}`", identifier.replace('`', "``"))
}

#[derive(Default)]
struct Lifetime {
    stopped: bool,
    ssh: Option<Child>,
    password_ssh: Option<tokio::sync::oneshot::Sender<()>>,
}

type SharedLifetime = Arc<Mutex<Lifetime>>;

struct QueryTarget {
    id: u64,
    connection_id: u32,
    token: String,
    opts: Opts,
    identity: Option<QueryConnectionIdentity>,
}

impl QueryTarget {
    fn fallback_tag(&self) -> Option<String> {
        self.identity
            .is_none()
            .then(|| format!("/* harbor_sql_query_{} */ ", self.token))
    }
}

#[derive(Clone)]
struct QueryConnectionIdentity {
    server_id: (String, String),
    user: String,
    source: String,
}

impl QueryConnectionIdentity {
    fn read(conn: &mut Conn) -> mysql::Result<Option<Self>> {
        let Some(server_id) = query_server_id(conn)? else {
            return Ok(None);
        };
        let id = conn.connection_id();
        let Some(row) = query_process_row(conn, id)? else {
            return Ok(None);
        };
        let identity = process_source(&row).and_then(|(user, source)| {
            // MySQL labels TCP peers as host:port. Do not use generic endpoint names.
            let (_, port) = source.rsplit_once(':')?;
            if port.parse::<u16>().ok()? == 0 || user.is_empty() {
                return None;
            }
            Some(Self {
                server_id,
                user,
                source,
            })
        });
        Ok(identity)
    }
}

#[derive(Default)]
struct CancellationState {
    active: Option<Arc<QueryTarget>>,
    requested: bool,
    acknowledged: bool,
    retire_session: bool,
}

impl CancellationState {
    fn is_current(&self, target: &Arc<QueryTarget>) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, target))
    }
}

enum ProgressUpdate {
    Dump(transfer::DumpProgress),
    Sync(SyncProgress),
    Restore(RestoreProgress),
    SchemaApply(SchemaApplyProgress),
}

struct OperationMonitor {
    progress: Option<ProgressUpdate>,
    cancel_requested: bool,
    finishing: bool,
    editor_query_started: bool,
    cancel_message: &'static str,
    table_read: Option<Arc<TableReadTarget>>,
}

impl OperationMonitor {
    fn new(cancel_message: &'static str) -> Self {
        Self {
            progress: None,
            cancel_requested: false,
            finishing: false,
            editor_query_started: false,
            cancel_message,
            table_read: None,
        }
    }

    fn request_cancel(&mut self) -> bool {
        if self.finishing || self.cancel_requested {
            return false;
        }
        self.cancel_requested = true;
        true
    }

    fn begin_editor_query(&mut self) -> Result<(), String> {
        self.check_cancelled()?;
        // From here on an ordinary statement might change data. Publish its actual outcome.
        self.editor_query_started = true;
        Ok(())
    }

    fn check_cancelled(&self) -> Result<(), String> {
        if self.cancel_requested {
            Err(self.cancel_message.into())
        } else {
            Ok(())
        }
    }

    fn update(&mut self, progress: ProgressUpdate, finishing: bool) -> Result<(), String> {
        self.check_cancelled()?;
        self.finishing = finishing;
        self.progress = Some(progress);
        Ok(())
    }

    fn update_dump(&mut self, progress: transfer::DumpProgress) -> Result<(), String> {
        let finishing = progress.finishing;
        self.update(ProgressUpdate::Dump(progress), finishing)
    }
}

type SharedMonitor = Arc<Mutex<OperationMonitor>>;
type Request = (u64, Command, Option<SharedMonitor>);

type SocketShutdown = Box<dyn Fn() -> io::Result<()> + Send + Sync>;

struct TableReadTarget {
    connection_id: u32,
    tag: String,
    opts: Opts,
    shutdown: Option<SocketShutdown>,
}

impl TableReadTarget {
    fn shutdown(&self) {
        if let Some(shutdown) = &self.shutdown {
            let _ = shutdown();
        }
    }
}

struct RegisteredTableRead<'a> {
    monitor: &'a Mutex<OperationMonitor>,
    target: Arc<TableReadTarget>,
}

impl<'a> RegisteredTableRead<'a> {
    fn new(
        monitor: &'a Mutex<OperationMonitor>,
        connection: &Conn,
        opts: &Opts,
    ) -> Result<Self, String> {
        let target = Arc::new(TableReadTarget {
            connection_id: connection.connection_id(),
            tag: format!(
                "/* harbor_table_read_{:016x} */ ",
                RandomState::new().hash_one(SESSION_ATTRIBUTE)
            ),
            opts: opts.clone(),
            shutdown: connection
                .tcp_shutdown_handle()
                .ok()
                .map(|shutdown| Box::new(shutdown) as SocketShutdown),
        });
        let mut status = monitor.lock().unwrap();
        status.check_cancelled()?;
        status.table_read = Some(Arc::clone(&target));
        Ok(Self { monitor, target })
    }

    fn tag(&self) -> &str {
        &self.target.tag
    }
}

impl Drop for RegisteredTableRead<'_> {
    fn drop(&mut self) {
        self.monitor.lock().unwrap().table_read = None;
    }
}

pub struct Worker {
    commands: mpsc::SyncSender<Request>,
    events: mpsc::Receiver<Event>,
    lifetime: SharedLifetime,
    cancellation: Arc<Mutex<CancellationState>>,
    cancel_requests: mpsc::SyncSender<Arc<QueryTarget>>,
    cancel_events: mpsc::Receiver<CancelEvent>,
    monitored: Mutex<Option<(u64, SharedMonitor)>>,
}

impl Worker {
    pub fn new() -> Self {
        Self::spawn(None)
    }

    fn spawn(initial: Option<(u64, connection_clone::Settings)>) -> Self {
        let (commands, requests) = mpsc::sync_channel::<Request>(8);
        let (responses, events) = mpsc::sync_channel(4);
        let (cancel_requests, cancellations) = mpsc::sync_channel(1);
        let (cancel_responses, cancel_events) = mpsc::sync_channel(4);
        let lifetime = SharedLifetime::default();
        let cancellation = Arc::new(Mutex::new(CancellationState::default()));
        let worker_lifetime = Arc::clone(&lifetime);
        let worker_cancellation = Arc::clone(&cancellation);
        thread::spawn(move || {
            let mut session: Option<Connection> = None;
            if let Some((id, settings)) = initial {
                if worker_lifetime.lock().unwrap().stopped {
                    return;
                }
                let result = settings.open(&worker_lifetime).map(|(connection, info)| {
                    session = Some(connection);
                    Response::Connected(info)
                });
                if responses
                    .send(Event {
                        id,
                        result,
                        connected: session.is_some(),
                    })
                    .is_err()
                {
                    return;
                }
            }
            loop {
                let keep_alive = match &session {
                    Some(Connection::Native(session)) => session.keep_alive,
                    _ => None,
                };
                let request = match keep_alive {
                    Some(delay) => requests.recv_timeout(delay),
                    None => requests
                        .recv()
                        .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                };
                let (id, command, monitor) = match request {
                    Ok(request) => request,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if let Some(Connection::Native(connection)) = session.as_mut() {
                            if connection.conn.ping().is_ok() {
                                connection.last_activity = Instant::now();
                            } else if let Err(error) = reconnect_native(connection, &worker_lifetime)
                            {
                                session = None;
                                if responses
                                    .send(Event {
                                        id: 0,
                                        connected: false,
                                        result: Err(format!("连接保活失败且自动重连失败：{error}")),
                                    })
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                        continue;
                    }
                };
                if worker_lifetime.lock().unwrap().stopped {
                    break;
                }
                let monitored_read = matches!(
                    &command,
                    Command::ReadTableData { .. }
                        | Command::QueryEditable { .. }
                        | Command::QueryF8 { .. }
                );
                let mut result = handle_command(
                    &mut session,
                    id,
                    command,
                    &worker_lifetime,
                    &worker_cancellation,
                    monitor.as_ref(),
                );
                if let Some(monitor) = monitor {
                    let mut monitor = monitor.lock().unwrap();
                    // An accepted stop must take precedence before publishing a page.
                    if monitored_read
                        && !monitor.editor_query_started
                        && let Err(error) = monitor.check_cancelled()
                    {
                        result = Err(error);
                    }
                    monitor.finishing = true;
                }
                if responses
                    .send(Event {
                        id,
                        result,
                        connected: session.is_some(),
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let control_cancellation = Arc::clone(&cancellation);
        thread::spawn(move || {
            while let Ok(target) = cancellations.recv() {
                let result = attempt_cancel(&target, &control_cancellation);
                {
                    let mut state = control_cancellation.lock().unwrap();
                    if state.is_current(&target) {
                        state.requested = false;
                    }
                }
                if cancel_responses
                    .send(CancelEvent {
                        id: target.id,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            commands,
            events,
            lifetime,
            cancellation,
            cancel_requests,
            cancel_events,
            monitored: Mutex::default(),
        }
    }

    pub fn send(&self, id: u64, command: Command) -> Result<(), String> {
        let cancellation = match &command {
            Command::DumpConfigured { .. } | Command::DumpForCopy { .. } => {
                Some(transfer::DUMP_CANCELLED)
            }
            Command::Restore { .. }
            | Command::RestoreEncoded { .. }
            | Command::RestoreForCopy { .. } => Some(crate::restore::RESTORE_CANCELLED),
            Command::ApplySchema { .. } => Some(schema_apply::CANCELLED),
            Command::PlanSync { .. }
            | Command::PlanScopedSync { .. }
            | Command::ApplySync { .. }
            | Command::PlanSyncGroup { .. }
            | Command::PlanScopedSyncGroup { .. }
            | Command::ApplySyncGroup { .. } => Some("已取消数据同步。"),
            Command::ExportSync { .. } => Some("已取消生成同步 SQL。"),
            Command::ReadTableData { .. } => Some(TABLE_READ_CANCELLED),
            Command::QueryEditable { .. } | Command::QueryF8 { .. } => {
                Some(EDITABLE_READ_CANCELLED)
            }
            _ => None,
        };
        let monitor =
            cancellation.map(|message| Arc::new(Mutex::new(OperationMonitor::new(message))));
        self.commands
            .try_send((id, command, monitor.clone()))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => "当前连接的待处理请求过多。".into(),
                mpsc::TrySendError::Disconnected(_) => "数据库连接服务已停止。".into(),
            })
            .map(|()| {
                if let Some(monitor) = monitor {
                    *self.monitored.lock().unwrap() = Some((id, monitor));
                }
            })
    }

    pub fn try_recv(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    pub fn recv(&self) -> Result<Event, String> {
        self.events
            .recv()
            .map_err(|_| "数据库连接服务已停止。".to_owned())
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<Event>, String> {
        match self.events.recv_timeout(timeout) {
            Ok(event) => Ok(Some(event)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err("数据库连接服务已停止。".into()),
        }
    }

    pub fn cancel(&self, id: u64) -> Result<(), String> {
        let mut state = self
            .cancellation
            .try_lock()
            .map_err(|_| "正在处理取消请求。")?;
        if state.requested || state.acknowledged {
            return Err("正在处理取消请求。".into());
        }
        let target = state
            .active
            .as_ref()
            .filter(|target| target.id == id)
            .ok_or("查询尚未开始或已经结束。")?;
        self.cancel_requests
            .try_send(Arc::clone(target))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => "已有其他取消请求正在处理。",
                mpsc::TrySendError::Disconnected(_) => "查询取消服务已停止。",
            })?;
        state.requested = true;
        Ok(())
    }

    pub fn try_recv_cancel(&self) -> Option<CancelEvent> {
        self.cancel_events.try_recv().ok()
    }

    pub fn take_dump_progress(&self, id: u64) -> Option<transfer::DumpProgress> {
        match self.take_progress(id)? {
            ProgressUpdate::Dump(progress) => Some(progress),
            _ => None,
        }
    }

    pub fn take_sync_progress(&self, id: u64) -> Option<SyncProgress> {
        match self.take_progress(id)? {
            ProgressUpdate::Sync(progress) => Some(progress),
            _ => None,
        }
    }

    pub fn take_restore_progress(&self, id: u64) -> Option<RestoreProgress> {
        match self.take_progress(id)? {
            ProgressUpdate::Restore(progress) => Some(progress),
            _ => None,
        }
    }

    pub fn take_schema_apply_progress(&self, id: u64) -> Option<SchemaApplyProgress> {
        match self.take_progress(id)? {
            ProgressUpdate::SchemaApply(progress) => Some(progress),
            _ => None,
        }
    }

    fn take_progress(&self, id: u64) -> Option<ProgressUpdate> {
        let operation = self.monitored.lock().unwrap();
        let (_, monitor) = operation.as_ref().filter(|(request, _)| *request == id)?;
        monitor.lock().unwrap().progress.take()
    }

    pub fn cancel_dump(&self, id: u64) -> bool {
        self.cancel_detached_read(id, transfer::DUMP_CANCELLED)
    }

    pub fn cancel_sync(&self, id: u64) -> bool {
        self.cancel_operation(id)
    }

    pub fn cancel_restore(&self, id: u64) -> bool {
        self.cancel_operation(id)
    }

    pub fn cancel_schema_apply(&self, id: u64) -> bool {
        self.cancel_operation(id)
    }

    pub fn cancel_table_read(&self, id: u64) -> bool {
        self.cancel_detached_read(id, TABLE_READ_CANCELLED)
    }

    pub fn cancel_editable_read(&self, id: u64) -> bool {
        if !self.cancel_detached_read(id, EDITABLE_READ_CANCELLED) {
            return false;
        }
        // When F8 runs an ordinary statement, its target is the persistent SQL session.
        let _ = self.cancel(id);
        true
    }

    fn cancel_detached_read(&self, id: u64, message: &str) -> bool {
        let Some(monitor) = self
            .monitored
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(request, _)| *request == id)
            .map(|(_, monitor)| Arc::clone(monitor))
        else {
            return false;
        };
        let target = {
            let mut status = monitor.lock().unwrap();
            if status.cancel_message != message || !status.request_cancel() {
                return false;
            }
            status.table_read.clone()
        };
        if let Some(target) = target {
            thread::spawn(move || {
                let _ = stop_native_detached_query(&target, &monitor);
                target.shutdown();
            });
        }
        true
    }

    fn cancel_operation(&self, id: u64) -> bool {
        let operation = self.monitored.lock().unwrap();
        let Some((_, monitor)) = operation.as_ref().filter(|(request, _)| *request == id) else {
            return false;
        };
        monitor.lock().unwrap().request_cancel()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some((_, monitor)) = self.monitored.get_mut().unwrap().as_ref() {
            let mut monitor = monitor.lock().unwrap();
            monitor.cancel_requested = true;
            if let Some(target) = &monitor.table_read {
                target.shutdown();
            }
        }
        let ssh = {
            let mut lifetime = self.lifetime.lock().unwrap();
            lifetime.stopped = true;
            if let Some(stop) = lifetime.password_ssh.take() {
                let _ = stop.send(());
            }
            lifetime.ssh.take()
        };
        terminate_ssh(ssh);
    }
}

struct Session {
    conn: Conn,
    config: ConnectionConfig,
    mariadb: bool,
    opts: Opts,
    token: String,
    query_identity: Option<QueryConnectionIdentity>,
    identity: Arc<()>,
    keep_alive: Option<Duration>,
    current_db: Option<String>,
    last_activity: Instant,
    system_roots: Option<Arc<tempfile::TempPath>>,
    _tunnel: Option<SshTunnel>,
}

const DEFAULT_KEEP_ALIVE_SECS: u64 = 30;

fn reconnect_native(session: &mut Session, lifetime: &SharedLifetime) -> Result<(), String> {
    let mut config = session.config.clone();
    if let Some(db) = session.current_db.clone() {
        config.database = Some(db);
    }
    let keep_alive = session.keep_alive;
    let (mut fresh, _) = connect(config, lifetime, false)?;
    fresh.keep_alive = keep_alive;
    *session = fresh;
    Ok(())
}

enum Connection {
    Native(Session),
    Http(http_session::Session),
}

enum Failure {
    Driver(mysql::Error),
    Message(String),
}

impl From<mysql::Error> for Failure {
    fn from(error: mysql::Error) -> Self {
        Self::Driver(error)
    }
}

impl Failure {
    fn message(self) -> String {
        match self {
            Self::Driver(error) => mysql_error(error),
            Self::Message(message) => message,
        }
    }

    fn lost_connection(&self) -> bool {
        matches!(self, Self::Driver(error) if error.is_connectivity_error())
    }
}

type DbResult<T> = Result<T, Failure>;

fn table_edit_response(outcome: row_editor::TableEditOutcome) -> Response {
    match outcome {
        row_editor::TableEditOutcome::Applied(affected_rows) => {
            Response::TableEdited { affected_rows }
        }
        row_editor::TableEditOutcome::NeedsConfirmation { matches } => {
            Response::TableEditConfirmation { matches }
        }
    }
}

fn mysql_error(error: mysql::Error) -> String {
    match error {
        mysql::Error::MySqlError(error) => error.to_string(),
        mysql::Error::IoError(error) => error.to_string(),
        error => error.to_string(),
    }
}

fn can_continue_after_server_error(code: u32, sql_state: Option<&str>, mariadb: bool) -> bool {
    let server_code =
        (1000..=u32::from(u16::MAX)).contains(&code) && !(2000..=2999).contains(&code);
    let halting_state = sql_state.is_some_and(|state| {
        state.starts_with("08") || state.starts_with("40") || state == "70100"
    });
    // The legacy HTTP tunnel supplies only an error number, including client errors.
    let session_error = matches!(code, 1152..=1161)
        || matches!(
            code,
            1040 | 1042 | 1043 | 1045 | 1047 | 1053 | 1080 | 1081 | 1129 | 1130
                | 1184 | 1251 | 1698 | 1810 | 1835 | 1862 | 1927 | 3032 | 3168 | 3169
        )
        // 4031 means an idle disconnect in MySQL and a missing trigger in MariaDB.
        || (!mariadb && code == 4031);
    let interrupted = matches!(code, 1317 | 1969 | 3024);
    let transaction_error = matches!(code, 1180 | 1181 | 1205 | 1206 | 1213 | 1402 | 1613 | 1614);
    server_code && !halting_state && !session_error && !interrupted && !transaction_error
}

fn handle_command(
    session: &mut Option<Connection>,
    id: u64,
    command: Command,
    lifetime: &SharedLifetime,
    cancellation: &Mutex<CancellationState>,
    shared_monitor: Option<&SharedMonitor>,
) -> Result<Response, String> {
    let operation_monitor = shared_monitor.map(Arc::as_ref);
    match command {
        Command::Connect(config) => {
            *session = None;
            let (connected, info) = connect(config, lifetime, true)?;
            *session = Some(Connection::Native(connected));
            return Ok(Response::Connected(info));
        }
        Command::ConnectHttp(config, tunnel) => {
            *session = None;
            if config.tls.is_some() || config.ssh.is_some() {
                return Err("HTTP 隧道不能与 MySQL 直连 TLS 或 SSH 转发同时启用。".into());
            }
            let (connected, info) = http_session::Session::open(config, tunnel)?;
            *session = Some(Connection::Http(connected));
            return Ok(Response::Connected(info));
        }
        Command::CloneConnection => return connection_clone::start(session),
        Command::TestConnection { config, http } => {
            return test_connection(config, http).map(Response::ConnectionTest);
        }
        Command::Disconnect => {
            *session = None;
            return Ok(Response::Disconnected);
        }
        _ => {}
    }

    let active = session.as_mut().ok_or("请先连接数据库服务器。")?;
    if let Connection::Native(native) = active
        && native.last_activity.elapsed() >= Duration::from_secs(15)
        && native.conn.ping().is_err()
        && let Err(error) = reconnect_native(native, lifetime)
    {
        *session = None;
        return Err(format!("数据库连接已中断且自动重连失败：{error}"));
    }
    if let Connection::Native(native) = active {
        native.last_activity = Instant::now();
    }
    if sync_dispatch::is_sync(&command) {
        return sync_dispatch::handle(active, command, operation_monitor);
    }
    if let Command::ApplySchema { apply } = command {
        return schema_apply::apply(active, *apply, operation_monitor).map(Response::SchemaApplied);
    }
    if let Command::RestoreForCopy {
        schema,
        path,
        source_schema,
        source,
        dependencies,
    } = command
    {
        return copy_restore::run(
            active,
            &schema,
            &path,
            &source_schema,
            &source,
            &dependencies,
            operation_monitor,
        )
        .map(Response::Restored);
    }
    if let Command::ImportOdbcTableWithSourceAction {
        table,
        source,
        cancel,
    } = command
    {
        return odbc_source_action::run(active, table, source, &cancel);
    }
    let active = match active {
        Connection::Native(active) => active,
        Connection::Http(active) => {
            return match command {
                Command::ImportOdbcTables { tables, cancel } => {
                    let created_in = tables
                        .first()
                        .filter(|table| table.create.is_some())
                        .map(|table| table.object.schema.clone());
                    odbc_import::run_http(active, tables, &cancel)
                        .map(|counts| Response::OdbcImported { counts, created_in })
                }
                Command::ImportDelimitedText(import) => odbc_import::run_http(
                    active,
                    vec![import.into_target()],
                    &AtomicBool::new(false),
                )
                .map(Response::TextImported),
                Command::ImportLocalFile(_) => Err("服务器导入需要直连或 SSH 会话。".into()),
                Command::QueryProfiled { .. } => Err("查询分析需要直连或 SSH 会话。".into()),
                command @ (Command::Restore { .. } | Command::RestoreEncoded { .. }) => {
                    let (schema, path, charset) = restore_args(command);
                    let monitor = operation_monitor.ok_or("SQL 还原任务未初始化。")?;
                    active
                        .restore_with_progress(&schema, &path, charset, |progress| {
                            monitor
                                .lock()
                                .unwrap()
                                .update(ProgressUpdate::Restore(progress), progress.finishing)
                        })
                        .map(Response::Restored)
                }
                Command::Accounts(command) => {
                    Ok(Response::Accounts(accounts::handle_http(active, command)))
                }
                Command::Processes(request) => {
                    Ok(Response::Processes(processes::http(active, request)))
                }
                Command::ServerValues(view) if view.is_variable_or_status() => {
                    let values = active.read_set(view.sql()).and_then(server_values);
                    Ok(Response::ServerValues { view, values })
                }
                Command::DumpConfigured {
                    schema,
                    path,
                    settings,
                } => {
                    let monitor = operation_monitor.ok_or("SQL 转储任务未初始化。")?;
                    monitor.lock().unwrap().check_cancelled()?;
                    transfer::dump_over_http(active, &schema, &path, settings, |progress| {
                        monitor.lock().unwrap().update_dump(progress)
                    })
                    .map(Response::Transferred)
                }
                command => active.handle_with_monitor(command, shared_monitor),
            };
        }
    };
    match command {
        Command::SnapshotSchema { schema } => {
            let result = snapshot_schema(&mut active.conn, schema).map(|mut snapshot| {
                snapshot.origin = SchemaOrigin::native(&mut active.conn, &active.identity).ok();
                Response::Snapshot(snapshot)
            });
            if result.as_ref().is_err_and(|error| error.lost_connection()) {
                *session = None;
            }
            result.map_err(Failure::message)
        }
        Command::Accounts(command) => {
            let (reply, retire) = accounts::handle(&mut active.conn, command);
            if retire {
                *session = None;
            }
            Ok(Response::Accounts(reply))
        }
        Command::Processes(request) => {
            let (reply, lost) = processes::native(active, request);
            if lost {
                *session = None;
            }
            Ok(Response::Processes(reply))
        }
        Command::ServerValues(view) => {
            if !view.is_variable_or_status() {
                return Err("请选择变量或状态列表。".into());
            }
            let (report, lost) = execute_sql(&mut active.conn, view.sql(), MAX_RESULT_ROWS);
            if lost {
                *session = None;
            }
            let values = if let Some(error) = report.error {
                Err(error)
            } else {
                report
                    .sets
                    .into_iter()
                    .next()
                    .ok_or_else(|| "服务器没有返回变量或状态列表。".to_owned())
                    .and_then(server_values)
            };
            Ok(Response::ServerValues { view, values })
        }
        Command::Query { sql, limit } => {
            let (report, disconnected) =
                run_editor_query(active, id, &sql, limit, cancellation, None);
            if disconnected {
                *session = None;
            } else {
                if let Some(db) = &report.active_database {
                    active.current_db = Some(db.clone());
                }
                active.last_activity = Instant::now();
            }
            Ok(Response::Queried(report))
        }
        Command::QueryInSeparateSession { sql, limit } => {
            // Administrative table statements can implicitly commit the editor transaction.
            let mut conn = Conn::new(active.opts.clone())
                .map_err(|error| format!("无法建立独立数据库会话：{}", mysql_error(error)))?;
            let (report, _) = execute_sql(&mut conn, &sql, limit);
            Ok(Response::Queried(report))
        }
        Command::FlushResources(plan) => {
            // Keep implicit commits and the temporary read lock out of the editor session.
            let mut conn = Conn::new(active.opts.clone())
                .map_err(|error| format!("无法建立独立数据库会话：{}", mysql_error(error)))?;
            for sql in plan.statements() {
                conn.query_drop(sql)
                    .map_err(|error| format!("服务器刷新中止（{sql}）。{}", mysql_error(error)))?;
            }
            Ok(Response::ServerResourcesFlushed(plan.targets()))
        }
        Command::QueryProfiled {
            sql,
            limit,
            options,
            open_profile,
        } => {
            if !crate::sql_tools::is_single_profile_select(&sql) {
                return Err("查询分析需要一条 SELECT 语句。".into());
            }
            let (report, profile, disconnected) =
                profiler::run(active, id, sql, limit, options, cancellation);
            if disconnected {
                *session = None;
            } else {
                if let Some(db) = &report.active_database {
                    active.current_db = Some(db.clone());
                }
                active.last_activity = Instant::now();
            }
            Ok(Response::Profiled {
                report,
                profile: Box::new(profile),
                open_profile,
            })
        }
        command @ (Command::QueryEditable { .. } | Command::QueryF8 { .. }) => {
            use editable_query::EditableRead;

            let (sql, limit, initial) = match command {
                Command::QueryEditable { sql, limit } => (sql, limit, false),
                Command::QueryF8 { sql, limit } => (sql, limit, true),
                _ => unreachable!(),
            };
            let monitor = operation_monitor.ok_or("F8 查询任务未初始化。")?;
            monitor.lock().unwrap().check_cancelled()?;
            let plan = if initial {
                f8_plan_joins(&sql)
            } else {
                Some(joined_editable::is_joined_query(&sql))
            };
            let mut read_only_reason = None;
            if let Some(joined) = plan {
                let sources =
                    editable_query_options(active, id, &sql, joined, cancellation, monitor);
                match sources {
                    Err((error, disconnected)) => {
                        if disconnected {
                            *session = None;
                        }
                        return Err(error);
                    }
                    Ok(EditableRead::ReadOnly(reason)) if !initial => return Err(reason),
                    Ok(EditableRead::ReadOnly(reason)) => read_only_reason = Some(reason),
                    Ok(EditableRead::Editable(opts)) if !initial => {
                        return if joined {
                            joined_editable::query_joined_editable(
                                &opts,
                                &sql,
                                limit,
                                Some(monitor),
                            )
                            .map(Response::JoinedEditablePage)
                        } else {
                            editable_query::query_editable(&opts, &sql, limit, Some(monitor))
                                .map(Response::EditablePage)
                        };
                    }
                    Ok(EditableRead::Editable(opts)) => {
                        if let Some(response) =
                            try_f8_editable(&opts, &sql, limit, joined, monitor)?
                        {
                            return Ok(response);
                        }
                    }
                }
            }
            let (mut report, disconnected) =
                run_editor_query(active, id, &sql, limit, cancellation, Some(monitor));
            if let Some(reason) = read_only_reason {
                for set in &mut report.sets {
                    set.duplicate_source = None;
                    set.notice.get_or_insert_with(|| reason.clone());
                }
            }
            if disconnected {
                *session = None;
            } else {
                if let Some(db) = &report.active_database {
                    active.current_db = Some(db.clone());
                }
                active.last_activity = Instant::now();
            }
            Ok(Response::Queried(report))
        }
        Command::DuplicateResultRow { source, cells } => {
            result_duplicate::load_native(active, source, cells).map(Response::ResultRowDuplicate)
        }
        Command::Preview { object, limit } => {
            if !object.kind.is_tabular() {
                return Err("只有表和视图支持数据预览。".into());
            }
            let limit = limit.min(MAX_RESULT_ROWS);
            let sql = format!(
                "SELECT * FROM {} LIMIT {}",
                object.qualified_name(),
                limit + 1
            );
            let (report, disconnected, _) =
                execute_cancellable(active, id, &sql, limit, cancellation, None);
            if disconnected {
                *session = None;
            }
            Ok(Response::Previewed(report))
        }
        Command::TablePage {
            object,
            limit,
            offset,
        } => row_editor::table_page(&active.opts, object, limit, offset).map(Response::TablePage),
        Command::TablePageSelected {
            object,
            limit,
            offset,
            selection,
        } => row_editor::table_page_selected(&active.opts, object, limit, offset, &selection)
            .map(Response::TablePage),
        Command::ReadTableData {
            object,
            selection,
            range,
        } => {
            let monitor = operation_monitor.ok_or("读取表数据任务未初始化。")?;
            row_editor::table_data(&active.opts, object, &selection, range, monitor)
                .map(Response::TablePage)
        }
        Command::EditTable { object, edit } => {
            row_editor::edit_table_with_confirmation(&active.opts, object, edit, None)
                .map(table_edit_response)
        }
        Command::ConfirmTableEdit {
            object,
            edit,
            choice,
        } => row_editor::edit_table_with_confirmation(&active.opts, object, edit, Some(choice))
            .map(table_edit_response),
        Command::DeleteTableRows { object, originals } => {
            row_editor::delete_table_rows(&active.opts, object, originals)
                .map(|affected_rows| Response::TableEdited { affected_rows })
        }
        Command::ImportRows {
            object,
            columns,
            rows,
        } => import_rows(&active.opts, object, columns, rows, None).map(Response::Imported),
        Command::ImportRowsControlled {
            object,
            columns,
            rows,
            cancel,
        } => {
            import_rows(&active.opts, object, columns, rows, Some(&cancel)).map(Response::Imported)
        }
        Command::ImportDelimitedText(import) => {
            text_import::run(&active.opts, import, None).map(Response::TextImported)
        }
        Command::ImportLocalFile(import) => {
            text_import::run_local(&active.opts, import).map(Response::LocalFileImported)
        }
        Command::ImportOdbcTables { tables, cancel } => {
            let created_in = tables
                .first()
                .filter(|table| table.create.is_some())
                .map(|table| table.object.schema.clone());
            odbc_import::run(&active.opts, tables, &cancel)
                .map(|counts| Response::OdbcImported { counts, created_in })
        }
        Command::Dump {
            schema,
            object,
            path,
            options,
            chunk_rows,
            bulk_insert_limit_kib,
        } => {
            let mut conn = transfer_connection(&active.opts)?;
            transfer::dump_with_limits(
                &mut conn,
                &schema,
                object.as_ref(),
                &path,
                options,
                chunk_rows,
                bulk_insert_limit_kib,
            )
            .map(Response::Transferred)
        }
        Command::DumpConfigured {
            schema,
            path,
            settings,
        } => {
            let monitor = operation_monitor.ok_or("SQL 转储任务未初始化。")?;
            monitor.lock().unwrap().check_cancelled()?;
            let mut conn = transfer_connection(&active.opts)?;
            let registered = settings
                .lock_source_server
                .then(|| RegisteredTableRead::new(monitor, &conn, &active.opts))
                .transpose()?;
            monitor.lock().unwrap().check_cancelled()?;
            let lock_tag = registered.as_ref().map_or("", RegisteredTableRead::tag);
            let result = transfer::dump_configured_with_lock_tag(
                &mut conn,
                &schema,
                &path,
                settings,
                lock_tag,
                |progress| monitor.lock().unwrap().update_dump(progress),
            );
            if result.is_err() && registered.is_some() {
                monitor.lock().unwrap().check_cancelled()?;
            }
            result.map(Response::Transferred)
        }
        Command::DumpForCopy {
            schema,
            object,
            objects,
            destination_schema,
            path,
            options,
            bulk_insert,
        } => {
            let monitor = operation_monitor.ok_or("复制任务未初始化。")?;
            monitor.lock().unwrap().check_cancelled()?;
            let settings = transfer::copy_dump_settings(
                &schema,
                &destination_schema,
                object,
                objects,
                options,
                bulk_insert,
            )?;
            let mut conn = transfer_connection(&active.opts)?;
            let origin = SchemaOrigin::native(&mut conn, &active.identity)?;
            monitor.lock().unwrap().check_cancelled()?;
            let mut summary = transfer::dump_for_copy(
                &mut conn,
                &schema,
                &destination_schema,
                &path,
                settings,
                |progress| monitor.lock().unwrap().update_dump(progress),
            )?;
            summary.copy_origin = Some(origin);
            Ok(Response::Transferred(summary))
        }
        Command::CopyTableAs { object, settings } => {
            let mut conn = transfer_connection(&active.opts)?;
            transfer::copy_table_as(&mut conn, &object, &settings).map(Response::Transferred)
        }
        Command::SchemaHtml { schema, path } => {
            let mut conn = transfer_connection(&active.opts)?;
            transfer::schema_html(&mut conn, &schema, &path).map(Response::Transferred)
        }
        Command::SchemaHtmlTables {
            schema,
            tables,
            path,
        } => {
            let mut conn = transfer_connection(&active.opts)?;
            transfer::schema_html_with_tables(&mut conn, &schema, &path, Some(&tables))
                .map(Response::Transferred)
        }
        Command::Export {
            object,
            path,
            settings,
        } => {
            let mut conn = transfer_connection(&active.opts)?;
            transfer::export(&mut conn, &object, &path, &settings).map(Response::Transferred)
        }
        command @ (Command::Restore { .. } | Command::RestoreEncoded { .. }) => {
            let (schema, path, charset) = restore_args(command);
            let monitor = operation_monitor.ok_or("SQL 还原任务未初始化。")?;
            monitor.lock().unwrap().check_cancelled()?;
            let mut conn = transfer_connection(&active.opts)?;
            crate::restore::restore_with_progress(&mut conn, &schema, &path, charset, |progress| {
                monitor
                    .lock()
                    .unwrap()
                    .update(ProgressUpdate::Restore(progress), progress.finishing)
            })
            .map(Response::Restored)
        }
        Command::ApplyDatabaseContents { plan } => {
            // DDL implicitly commits. Run it separately from the SQL editor's session.
            let mut conn = Conn::new(active.opts.clone()).map_err(|error| {
                format!("无法建立用于处理数据库对象的连接。{}", mysql_error(error))
            })?;
            let schema = plan.schema().to_owned();
            let current = read_database_contents(&mut conn, schema.clone(), plan.action())
                .map_err(Failure::message)?;
            plan.verify(&current)?;
            if plan.suspend_foreign_key_checks
                && !object_operations::native_has_global_visibility(&mut conn)
                    .map_err(Failure::message)?
            {
                return Err("无法确认当前账号能读取全部引用表。请使用有全局读取权限的账号，或先处理相关外键。".into());
            }
            let foreign_key_checks = u8::from(!plan.suspend_foreign_key_checks);
            conn.query_drop(format!(
                "SET SESSION foreign_key_checks = {foreign_key_checks}"
            ))
            .map_err(mysql_error)?;
            let mut result = object_operations::run_contents(plan, |sql| {
                conn.query_drop(sql).map_err(|error| {
                    if error.is_connectivity_error() {
                        format!("无法确认这项操作的结果。{}", mysql_error(error))
                    } else {
                        mysql_error(error)
                    }
                })
            });
            result.objects = list_objects(&mut conn, &schema).ok();
            Ok(Response::DatabaseContentsApplied(result))
        }
        command => {
            let result = metadata_command(&mut active.conn, command);
            if let Ok(Response::SchemaChanged(schema)) = &result {
                active.current_db = Some(schema.clone());
            }
            active.last_activity = Instant::now();
            if result.as_ref().is_err_and(|error| error.lost_connection()) {
                *session = None;
            }
            result.map_err(Failure::message)
        }
    }
}

fn restore_args(command: Command) -> (String, PathBuf, Option<SqlDumpCharset>) {
    match command {
        Command::Restore { schema, path } => (schema, path, None),
        Command::RestoreEncoded {
            schema,
            path,
            charset,
        } => (schema, path, Some(charset)),
        _ => unreachable!("only restore commands have restore arguments"),
    }
}

fn transfer_connection(opts: &Opts) -> Result<Conn, String> {
    Conn::new(opts.clone())
        .map_err(|error| format!("无法建立用于导入导出的独立连接：{}", mysql_error(error)))
}

#[path = "transfer.rs"]
pub mod transfer;

fn connect(
    config: ConnectionConfig,
    lifetime: &SharedLifetime,
    run_init: bool,
) -> Result<(Session, ServerInfo), String> {
    let host = normalize_host(&config.host, "数据库主机")?;
    if config.port == 0 {
        return Err("请输入 1 到 65535 之间的数据库端口。".into());
    }
    if config.advanced.connect_timeout == 0
        || config.advanced.read_timeout == 0
        || config.advanced.keep_alive == Some(0)
        || config.advanced.wait_timeout == Some(0)
    {
        return Err("连接超时、会话空闲超时和保活间隔必须大于零。".into());
    }

    let tunnel = config
        .ssh
        .as_ref()
        .map(|ssh| SshTunnel::open(ssh, &host, config.port, lifetime))
        .transpose()?;
    let (dial_host, dial_port) = match &tunnel {
        Some(tunnel) => (Ipv4Addr::LOCALHOST.to_string(), tunnel.port),
        None => (bracket_ipv6(&host), config.port),
    };

    let system_roots = config
        .tls
        .as_ref()
        .filter(|tls| tls.ca_path.is_none())
        .and_then(|_| export_system_trust())
        .map(Arc::new);
    let tls_host = tunnel.as_ref().map(|_| bracket_ipv6(&host));
    let tls = config
        .tls
        .as_ref()
        .map(|tls| {
            let identity = tls.client_identity.as_ref().map(|client| {
                mysql::ClientIdentity::new(client.certificate.clone(), client.private_key.clone())
            });
            let trust = tls
                .ca_path
                .clone()
                .or_else(|| system_roots.as_ref().map(|roots| roots.to_path_buf()));
            SslOpts::default()
                .with_root_cert_path(trust)
                .with_client_identity(identity)
                .with_server_name(tls_host.as_deref())
                .map_err(|_| "数据库主机名无法用于 TLS 证书验证。".to_owned())
        })
        .transpose()?;
    let opts = OptsBuilder::new()
        .ip_or_hostname(Some(dial_host))
        .tcp_port(dial_port)
        .user(Some(config.user.clone()))
        .pass(Some(config.password.clone()))
        .prefer_socket(false)
        .tcp_connect_timeout(Some(Duration::from_secs(
            config.advanced.connect_timeout as u64,
        )))
        .read_timeout(Some(Duration::from_secs(
            config.advanced.read_timeout as u64,
        )))
        .write_timeout(Some(Duration::from_secs(30)))
        .tcp_keepalive_time_ms(Some(30_000))
        .compress(
            config
                .advanced
                .compression
                .then(mysql::Compression::default),
        )
        .local_infile_handler(Some(LocalInfileHandler::new(|_, _| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "已禁止服务器请求读取本地文件。",
            ))
        })))
        .ssl_opts(tls);
    let opts = Opts::from(opts);
    let token = format!("{:016x}", RandomState::new().hash_one(SESSION_ATTRIBUTE));
    let editor_opts = OptsBuilder::from_opts(opts.clone())
        .connect_attrs(Some(HashMap::from([(SESSION_ATTRIBUTE, token.clone())])));
    let mut conn = Conn::new(editor_opts).map_err(|error| match &tunnel {
        Some(tunnel) => tunnel.error(mysql_error(error)),
        None => mysql_error(error),
    })?;
    let query_identity = QueryConnectionIdentity::read(&mut conn).ok().flatten();
    if let Some(database) = config.database.as_ref().filter(|name| !name.is_empty()) {
        conn.select_db(database).map_err(mysql_error)?;
    }
    configure_session(&mut conn, &config.advanced, run_init)?;
    let info = conn
        .query_first_opt::<(String, Option<String>), _>("SELECT VERSION(), DATABASE()")
        .map_err(mysql_error)?
        .transpose()
        .map_err(|error| mysql_error(error.into()))?
        .ok_or("服务器未返回版本和当前数据库。")?;
    let info = ServerInfo {
        version: info.0,
        connection_id: conn.connection_id(),
        database: info.1,
        http_tunnel: false,
    };
    let session = Session {
        conn,
        keep_alive: Some(Duration::from_secs(
            config
                .advanced
                .keep_alive
                .map_or(DEFAULT_KEEP_ALIVE_SECS, u64::from),
        )),
        current_db: info.database.clone(),
        last_activity: Instant::now(),
        config,
        mariadb: info.version.to_ascii_lowercase().contains("mariadb"),
        opts,
        token,
        query_identity,
        identity: Arc::new(()),
        system_roots,
        _tunnel: tunnel,
    };
    Ok((session, info))
}

fn export_system_trust() -> Option<tempfile::TempPath> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use std::io::Write;
    let certs = rustls_native_certs::load_native_certs().certs;
    if certs.is_empty() {
        return None;
    }
    let mut file = tempfile::Builder::new()
        .prefix("harbor-public-ca-")
        .suffix(".pem")
        .tempfile()
        .ok()?;
    let mut store = rustls::RootCertStore::empty();
    for cert in certs {
        if store.add(cert.clone()).is_err() {
            continue;
        }
        file.write_all(b"-----BEGIN CERTIFICATE-----\n").ok()?;
        let encoded = STANDARD.encode(cert.as_ref());
        for line in encoded.as_bytes().chunks(64) {
            file.write_all(line).ok()?;
            file.write_all(b"\n").ok()?;
        }
        file.write_all(b"-----END CERTIFICATE-----\n").ok()?;
    }
    if store.is_empty() {
        return None;
    }
    file.flush().ok()?;
    Some(file.into_temp_path())
}

fn configure_session(
    conn: &mut Conn,
    config: &AdvancedConnection,
    run_init: bool,
) -> Result<(), String> {
    if let Some(seconds) = config.wait_timeout {
        conn.exec_drop("SET SESSION wait_timeout = ?", (seconds,))
            .map_err(mysql_error)?;
        let actual = conn
            .query_first::<u32, _>("SELECT @@SESSION.wait_timeout")
            .map_err(mysql_error)?;
        if actual != Some(seconds) {
            return Err(format!("服务器未采用指定的会话空闲超时 {seconds} 秒。"));
        }
    }
    if let Some(mode) = &config.sql_mode {
        conn.exec_drop("SET SESSION sql_mode = ?", (mode,))
            .map_err(mysql_error)?;
    }
    if run_init && !config.init_sql.is_empty() {
        let mut script = crate::sql_tools::StatementReader::new(&config.init_sql);
        let read_mode = |conn: &mut Conn| {
            conn.query_first::<String, _>("SELECT @@SESSION.sql_mode")
                .map_err(mysql_error)
                .map(Option::unwrap_or_default)
        };
        script.set_sql_mode(&read_mode(conn)?);
        let mut position = 0;
        while let Some(statement) = script.next_statement() {
            position += 1;
            crate::restore::run(conn, statement.sql).map_err(|error| match error {
                mysql::Error::MySqlError(error) => format!(
                    "第 {position} 条连接初始化语句失败（数据库错误 {}）。",
                    error.code
                ),
                _ => format!("第 {position} 条连接初始化语句未执行完成，请检查连接和脚本。"),
            })?;
            if crate::restore::changes_sql_mode(statement.sql) {
                script.set_sql_mode(&read_mode(conn)?);
            }
        }
    }
    Ok(())
}

fn test_connection(
    config: ConnectionConfig,
    http: Option<HttpTunnelConfig>,
) -> Result<ConnectionProbe, String> {
    let started = Instant::now();
    if let Some(http) = http {
        let secure = http.url.trim().to_ascii_lowercase().starts_with("https:");
        let (_session, info) = http_session::Session::open(config, http)?;
        return Ok(ConnectionProbe {
            version: info.version,
            database: info.database,
            elapsed: started.elapsed(),
            transport: if secure {
                "HTTPS 隧道"
            } else {
                "HTTP 隧道"
            }
            .into(),
            compression: None,
        });
    }
    let ssh = config.ssh.is_some();
    let tls_requested = config.tls.is_some();
    let lifetime = SharedLifetime::default();
    let (mut session, info) = connect(config, &lifetime, false)?;
    let status = |connection: &mut Conn, sql: &str| -> Result<Option<String>, String> {
        Ok(connection
            .query_first::<(String, String), _>(sql)
            .map_err(mysql_error)?
            .map(|(_, value)| value))
    };
    let cipher = if tls_requested {
        status(&mut session.conn, "SHOW SESSION STATUS LIKE 'Ssl_cipher'")?
            .filter(|value| !value.is_empty())
    } else {
        None
    };
    let transport = if ssh {
        "SSH 隧道".to_owned()
    } else if let Some(cipher) = cipher {
        format!("MySQL TLS（{cipher}）")
    } else if tls_requested {
        "MySQL TLS".to_owned()
    } else {
        "MySQL 直连".to_owned()
    };
    let compression = status(&mut session.conn, "SHOW SESSION STATUS LIKE 'Compression'")?
        .map(|value| value.eq_ignore_ascii_case("ON"));
    Ok(ConnectionProbe {
        version: info.version,
        database: info.database,
        elapsed: started.elapsed(),
        transport,
        compression,
    })
}

const LEGACY_RAID_VARIABLE: &str = "SHOW VARIABLES LIKE 'have_raid'";

fn metadata_command(conn: &mut Conn, command: Command) -> DbResult<Response> {
    match command {
        Command::ListEngines => {
            let rows = conn.query::<mysql::Row, _>("SHOW ENGINES")?;
            let engines: Vec<String> = rows
                .iter()
                .filter_map(|row| {
                    let support: String = row.get("Support")?;
                    matches!(support.as_str(), "DEFAULT" | "YES")
                        .then(|| row.get::<String, _>("Engine"))
                        .flatten()
                })
                .collect();
            let mode: String = conn
                .query_first("SELECT @@SESSION.sql_mode")?
                .unwrap_or_default();
            let raid_default_engine = if matches!(conn.server_version(), (4, 1, _))
                && engines
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case("MyISAM"))
                && conn
                    .query_first::<(String, String), _>(LEGACY_RAID_VARIABLE)?
                    .is_some_and(|(name, value)| {
                        name.eq_ignore_ascii_case("have_raid") && value.eq_ignore_ascii_case("YES")
                    }) {
                Some(
                    rows.iter()
                        .find(|row| row.get::<String, _>("Support").as_deref() == Some("DEFAULT"))
                        .and_then(|row| row.get("Engine"))
                        .unwrap_or_default(),
                )
            } else {
                None
            };
            Ok(Response::Engines {
                names: engines,
                no_backslash_escapes: mode
                    .split(',')
                    .any(|name| name.eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES")),
                raid_default_engine,
            })
        }
        Command::ListSchemas => {
            let schemas = conn.query_opt::<String, _>("SHOW DATABASES")?;
            let schemas = schemas
                .into_iter()
                .collect::<Result<_, _>>()
                .map_err(mysql::Error::from)?;
            Ok(Response::Schemas(schemas))
        }
        Command::ListObjects { schema } => {
            let objects = list_objects(conn, &schema)?;
            Ok(Response::Objects { schema, objects })
        }
        Command::ReadDatabaseDefaults { schema } => {
            object_operations::read_defaults(schema.as_deref(), |sql| {
                object_operations::native_rows(conn, sql)
            })
            .map(Response::DatabaseDefaults)
        }
        Command::ReadDatabaseContents { schema, action } => {
            read_database_contents(conn, schema, action).map(Response::DatabaseContents)
        }
        Command::DescribeSchemaDesign { schema } => {
            describe_schema_design(conn, schema).map(Response::SchemaDesign)
        }
        Command::DescribeTableKeys { object } => {
            describe_table_keys(conn, object).map(Response::TableKeys)
        }
        Command::SnapshotDesignTables { schema, names } => {
            let mut seen = HashSet::new();
            let objects = names
                .into_iter()
                .filter(|name| seen.insert(name.clone()))
                .map(|name| DbObject {
                    schema: schema.clone(),
                    name,
                    kind: ObjectKind::Table,
                })
                .collect();
            snapshot_objects(conn, schema, objects).map(Response::Snapshot)
        }
        Command::UseSchema { schema } => {
            conn.select_db(&schema)?;
            Ok(Response::SchemaChanged(schema))
        }
        Command::Inspect { object } => inspect_object(conn, object).map(Response::Inspected),
        Command::ExportColumns { object } => {
            inspect_object(conn, object).map(Response::ExportColumns)
        }
        _ => unreachable!("non-metadata command"),
    }
}

fn read_database_contents(
    conn: &mut Conn,
    schema: String,
    action: DatabaseContentsAction,
) -> DbResult<DatabaseContents> {
    let objects = list_objects(conn, &schema)?;
    object_operations::read_contents(schema, objects, action, |sql| {
        object_operations::native_rows(conn, sql)
    })
}

fn describe_table_keys(conn: &mut Conn, object: DbObject) -> DbResult<TableKeys> {
    if object.kind != ObjectKind::Table {
        return Err(Failure::Message("只有表支持管理索引和外键。".into()));
    }
    let detail = inspect_object(conn, object.clone())?;
    let rows = conn.query::<Row, _>(format!("SHOW INDEX FROM {}", object.qualified_name()))?;
    let mode: String = conn
        .query_first("SELECT @@SESSION.sql_mode")?
        .unwrap_or_default();
    let version: String = conn.query_first("SELECT VERSION()")?.unwrap_or_default();
    let visibility = if version.contains("MariaDB") && conn.server_version() >= (10, 6, 0) {
        Some(IndexVisibility::MariaDb)
    } else if !version.contains("MariaDB") && conn.server_version() >= (8, 0, 0) {
        Some(IndexVisibility::MySql)
    } else {
        None
    };
    let indexes = group_table_indexes(&detail, rows, |row, name| {
        row.columns_ref()
            .iter()
            .position(|column| column.name_str().eq_ignore_ascii_case(name))
            .and_then(|index| row.get_opt::<Option<String>, _>(index))
            .and_then(Result::ok)
            .flatten()
    })?;
    let foreign_keys = read_schema_foreign_keys(conn, &object.schema, false)?
        .into_iter()
        .filter(|key| key.table.name == object.name)
        .collect();
    let no_backslash_escapes = mode
        .split(',')
        .any(|value| value.eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES"));
    Ok(TableKeys {
        object,
        columns: detail.columns,
        indexes,
        foreign_keys,
        visibility,
        no_backslash_escapes,
    })
}

fn group_table_indexes<T>(
    detail: &ObjectDetail,
    rows: impl IntoIterator<Item = T>,
    get: impl Fn(&T, &str) -> Option<String>,
) -> DbResult<Vec<IndexDefinition>> {
    let mut definitions: std::collections::BTreeMap<
        String,
        (IndexDefinition, Vec<(u64, IndexPart)>),
    > = std::collections::BTreeMap::new();
    for row in rows {
        let text = |name| get(&row, name);
        let number = |name| text(name).and_then(|value| value.parse::<u64>().ok());
        let name =
            text("Key_name").ok_or_else(|| Failure::Message("服务器未返回索引名称。".into()))?;
        let sequence = number("Seq_in_index")
            .ok_or_else(|| Failure::Message("服务器未返回索引字段顺序。".into()))?;
        let (metadata, parts) = definitions.entry(name.clone()).or_insert_with(|| {
            let quoted = quote_identifier(&name);
            let index_token = format!("KEY {quoted} ");
            let primary = name.eq_ignore_ascii_case("PRIMARY");
            let ddl = detail.ddl.lines().find(|line| {
                let line = line.trim_start();
                if primary {
                    line.starts_with("PRIMARY KEY ")
                } else {
                    line.starts_with(&index_token)
                        || ["UNIQUE ", "FULLTEXT ", "SPATIAL "].iter().any(|prefix| {
                            line.strip_prefix(prefix)
                                .is_some_and(|tail| tail.starts_with(&index_token))
                        })
                }
            });
            let standard_options = ddl.is_some_and(|line| {
                let line = line.to_ascii_uppercase();
                ![
                    "WITH PARSER",
                    "KEY_BLOCK_SIZE",
                    "ENGINE_ATTRIBUTE",
                    "SECONDARY_ENGINE_ATTRIBUTE",
                    "WITHOUT OVERLAPS",
                ]
                .iter()
                .any(|token| line.contains(token))
            });
            (
                IndexDefinition {
                    name,
                    unique: number("Non_unique") == Some(0),
                    method: text("Index_type").unwrap_or_default(),
                    parts: Vec::new(),
                    comment: text("Index_comment").unwrap_or_default(),
                    visible: text("Visible").as_deref() != Some("NO")
                        && text("Ignored").as_deref() != Some("YES"),
                    standard_options,
                },
                Vec::new(),
            )
        });
        if metadata.method.is_empty() {
            metadata.standard_options = false;
        }
        parts.push((
            sequence,
            IndexPart {
                column: text("Column_name"),
                expression: text("Expression"),
                prefix: number("Sub_part"),
                descending: text("Collation").as_deref() == Some("D"),
            },
        ));
    }
    Ok(definitions
        .into_values()
        .map(|(mut definition, mut parts)| {
            parts.sort_by_key(|(sequence, _)| *sequence);
            definition.parts = parts.into_iter().map(|(_, part)| part).collect();
            definition
        })
        .collect())
}

const SCHEMA_DESIGN_COLUMNS: &str = "
        SELECT c.TABLE_NAME, c.COLUMN_NAME, c.COLUMN_TYPE, c.IS_NULLABLE, c.COLUMN_KEY, c.COLUMN_DEFAULT, c.EXTRA
        FROM information_schema.COLUMNS c
        JOIN information_schema.TABLES t
            ON BINARY c.TABLE_SCHEMA = BINARY t.TABLE_SCHEMA AND BINARY c.TABLE_NAME = BINARY t.TABLE_NAME
        WHERE c.TABLE_SCHEMA = :schema AND t.TABLE_TYPE IN ('BASE TABLE', 'SYSTEM VERSIONED')
            AND (@@lower_case_table_names <> 0 OR BINARY c.TABLE_SCHEMA = BINARY :schema)
        ORDER BY BINARY c.TABLE_NAME, c.ORDINAL_POSITION
        LIMIT 16385";
type DesignColumnRow = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    String,
);

fn describe_schema_design(conn: &mut Conn, schema: String) -> DbResult<SchemaDesign> {
    let mut tables = build_design_tables(
        &schema,
        conn.exec_iter(SCHEMA_DESIGN_COLUMNS, params! { "schema" => &schema })?
            .map(|row| {
                mysql::from_row_opt(row?)
                    .map_err(mysql::Error::from)
                    .map_err(Failure::from)
            }),
    )?;
    design_indexes::read_native(conn, &schema, &mut tables);
    let foreign_keys = read_schema_foreign_keys(conn, &schema, false)?;
    Ok(SchemaDesign {
        schema,
        tables,
        foreign_keys,
    })
}

fn build_design_tables(
    schema: &str,
    rows: impl Iterator<Item = DbResult<DesignColumnRow>>,
) -> DbResult<Vec<SchemaDesignTable>> {
    let mut tables: Vec<SchemaDesignTable> = Vec::new();
    for (offset, row) in rows.enumerate() {
        if offset == 16_384 {
            return Err(Failure::Message(
                "当前数据库的字段超过结构设计器的加载上限（16,384 个）。".into(),
            ));
        }
        let (table, name, data_type, nullable, key, default_value, extra) = row?;
        if tables.last().is_none_or(|item| item.object.name != table) {
            tables.push(SchemaDesignTable {
                object: DbObject {
                    schema: schema.to_owned(),
                    name: table,
                    kind: ObjectKind::Table,
                },
                columns: Vec::new(),
                unique_indexes: None,
            });
        }
        if let Some(table) = tables.last_mut() {
            table.columns.push(ColumnInfo {
                name,
                data_type,
                nullable: nullable == "YES",
                key,
                default_value,
                extra,
            });
        }
    }
    Ok(tables)
}

const SCHEMA_FOREIGN_KEYS: &str = "
        SELECT k.TABLE_SCHEMA, k.TABLE_NAME, k.CONSTRAINT_NAME, k.REFERENCED_TABLE_SCHEMA, k.REFERENCED_TABLE_NAME,
               k.COLUMN_NAME, k.REFERENCED_COLUMN_NAME, r.DELETE_RULE, r.UPDATE_RULE
        FROM information_schema.KEY_COLUMN_USAGE k
        LEFT JOIN information_schema.REFERENTIAL_CONSTRAINTS r
            ON BINARY r.CONSTRAINT_SCHEMA = BINARY k.CONSTRAINT_SCHEMA
            AND BINARY r.CONSTRAINT_NAME = BINARY k.CONSTRAINT_NAME
            AND BINARY r.TABLE_NAME = BINARY k.TABLE_NAME
        WHERE k.REFERENCED_TABLE_NAME IS NOT NULL AND (
            (k.TABLE_SCHEMA = :schema
                AND (@@lower_case_table_names <> 0 OR BINARY k.TABLE_SCHEMA = BINARY :schema))
            OR (:include_inbound AND k.REFERENCED_TABLE_SCHEMA = :schema
                AND (@@lower_case_table_names <> 0 OR BINARY k.REFERENCED_TABLE_SCHEMA = BINARY :schema))
        )
        ORDER BY BINARY k.TABLE_SCHEMA, BINARY k.TABLE_NAME, BINARY k.CONSTRAINT_NAME, k.ORDINAL_POSITION
        LIMIT 8193";
type ForeignKeyRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
);

fn read_schema_foreign_keys(
    conn: &mut Conn,
    schema: &str,
    include_inbound: bool,
) -> DbResult<Vec<SchemaForeignKey>> {
    build_schema_foreign_keys(
        conn.exec_iter(
            SCHEMA_FOREIGN_KEYS,
            params! { "schema" => schema, "include_inbound" => include_inbound },
        )?
        .map(|row| {
            mysql::from_row_opt(row?)
                .map_err(mysql::Error::from)
                .map_err(Failure::from)
        }),
    )
}

fn build_schema_foreign_keys(
    rows: impl Iterator<Item = DbResult<ForeignKeyRow>>,
) -> DbResult<Vec<SchemaForeignKey>> {
    let mut foreign_keys: Vec<SchemaForeignKey> = Vec::new();
    for (offset, row) in rows.enumerate() {
        if offset == 8_192 {
            return Err(Failure::Message(
                "当前数据库的外键字段超过结构设计器的加载上限（8,192 个）。".into(),
            ));
        }
        let (
            table_schema,
            table,
            name,
            referenced_schema,
            referenced_table,
            column,
            referenced_column,
            on_delete,
            on_update,
        ) = row?;
        if foreign_keys.last().is_none_or(|item| {
            item.table.schema != table_schema || item.table.name != table || item.name != name
        }) {
            foreign_keys.push(SchemaForeignKey {
                table: DbObject {
                    schema: table_schema,
                    name: table,
                    kind: ObjectKind::Table,
                },
                name,
                referenced_table: DbObject {
                    schema: referenced_schema,
                    name: referenced_table,
                    kind: ObjectKind::Table,
                },
                columns: Vec::new(),
                on_delete: on_delete.unwrap_or_default(),
                on_update: on_update.unwrap_or_default(),
            });
        }
        if let Some(key) = foreign_keys.last_mut() {
            key.columns.push((column, referenced_column));
        }
    }
    Ok(foreign_keys)
}

const SCHEMA_OBJECTS: &str = "
        SELECT ROUTINE_NAME, ROUTINE_TYPE FROM information_schema.ROUTINES
        WHERE ROUTINE_SCHEMA = :schema
            AND CAST(IF(@@lower_case_table_names = 0, ROUTINE_SCHEMA, LOWER(ROUTINE_SCHEMA)) AS BINARY)
                = CAST(IF(@@lower_case_table_names = 0, :schema, LOWER(:schema)) AS BINARY)
        UNION ALL
        SELECT TRIGGER_NAME, 'TRIGGER' FROM information_schema.TRIGGERS
        WHERE TRIGGER_SCHEMA = :schema
            AND CAST(IF(@@lower_case_table_names = 0, TRIGGER_SCHEMA, LOWER(TRIGGER_SCHEMA)) AS BINARY)
                = CAST(IF(@@lower_case_table_names = 0, :schema, LOWER(:schema)) AS BINARY)
        UNION ALL
        SELECT EVENT_NAME, 'EVENT' FROM information_schema.EVENTS
        WHERE EVENT_SCHEMA = :schema
            AND CAST(IF(@@lower_case_table_names = 0, EVENT_SCHEMA, LOWER(EVENT_SCHEMA)) AS BINARY)
                = CAST(IF(@@lower_case_table_names = 0, :schema, LOWER(:schema)) AS BINARY)";

fn list_objects(conn: &mut Conn, schema: &str) -> DbResult<Vec<DbObject>> {
    if matches!(conn.server_version(), (4, 1, _)) {
        // Views and the Information Schema catalog arrived in MySQL 5.0.
        let mut tables = Vec::new();
        for name in
            conn.query_opt::<String, _>(format!("SHOW TABLES FROM {}", quote_identifier(schema)))?
        {
            tables.push(DbObject {
                schema: schema.into(),
                name: name.map_err(mysql::Error::from)?,
                kind: ObjectKind::Table,
            });
        }
        tables.sort_by(|left, right| left.name.cmp(&right.name));
        return Ok(tables);
    }
    let tables = conn.query_opt::<(String, String), _>(format!(
        "SHOW FULL TABLES FROM {}",
        quote_identifier(schema)
    ))?;
    let mut objects = Vec::new();
    for table in tables {
        let (name, kind) = table.map_err(mysql::Error::from)?;
        let kind = match kind.as_str() {
            "VIEW" | "SYSTEM VIEW" => ObjectKind::View,
            _ => ObjectKind::Table,
        };
        objects.push(DbObject {
            schema: schema.into(),
            name,
            kind,
        });
    }

    let requests = params! { "schema" => schema };
    for object in conn.exec_opt::<(String, String), _, _>(SCHEMA_OBJECTS, requests)? {
        let (name, kind) = object.map_err(mysql::Error::from)?;
        let kind = match kind.as_str() {
            "PROCEDURE" => ObjectKind::Procedure,
            "FUNCTION" => ObjectKind::Function,
            "TRIGGER" => ObjectKind::Trigger,
            "EVENT" => ObjectKind::Event,
            _ => continue,
        };
        objects.push(DbObject {
            schema: schema.into(),
            name,
            kind,
        });
    }
    objects.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(objects)
}

fn inspect_object(conn: &mut Conn, object: DbObject) -> DbResult<ObjectDetail> {
    let mut columns = Vec::new();
    if object.kind.is_tabular() && matches!(conn.server_version(), (4, 1, _)) {
        for row in conn.query::<Row, _>(format!(
            "SHOW FULL COLUMNS FROM {}",
            object.qualified_name()
        ))? {
            let (Some(name), Some(data_type)) = (row.get("Field"), row.get("Type")) else {
                continue;
            };
            columns.push(ColumnInfo {
                name,
                data_type,
                nullable: row.get::<String, _>("Null").as_deref() == Some("YES"),
                key: row.get("Key").unwrap_or_default(),
                default_value: row.get::<Option<String>, _>("Default").flatten(),
                extra: row.get("Extra").unwrap_or_default(),
            });
        }
    } else if object.kind.is_tabular() {
        const COLUMNS: &str = "
            SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY, COLUMN_DEFAULT, EXTRA
            FROM information_schema.COLUMNS
            WHERE TABLE_SCHEMA = :schema AND TABLE_NAME = :table
                AND CAST(IF(@@lower_case_table_names = 0, TABLE_SCHEMA, LOWER(TABLE_SCHEMA)) AS BINARY)
                    = CAST(IF(@@lower_case_table_names = 0, :schema, LOWER(:schema)) AS BINARY)
                AND CAST(IF(@@lower_case_table_names = 0, TABLE_NAME, LOWER(TABLE_NAME)) AS BINARY)
                    = CAST(IF(@@lower_case_table_names = 0, :table, LOWER(:table)) AS BINARY)
            ORDER BY ORDINAL_POSITION";
        type ColumnMetadata = (String, String, String, String, Option<String>, String);
        let args = params! { "schema" => object.schema.as_str(), "table" => object.name.as_str() };
        for row in conn.exec_opt::<ColumnMetadata, _, _>(COLUMNS, args)? {
            let (name, data_type, nullable, key, default_value, extra) =
                row.map_err(mysql::Error::from)?;
            columns.push(ColumnInfo {
                name,
                data_type,
                nullable: nullable == "YES",
                key,
                default_value,
                extra,
            });
        }
    }

    let ddl = show_create(conn, &object);
    let (ddl, ddl_error) = match ddl {
        Ok(ddl) => (ddl, None),
        Err(error) if !columns.is_empty() && !error.lost_connection() => {
            (String::new(), Some(error.message()))
        }
        Err(error) => return Err(error),
    };
    Ok(ObjectDetail {
        object,
        columns,
        ddl,
        ddl_error,
    })
}

fn snapshot_schema(conn: &mut Conn, schema: String) -> DbResult<SchemaSnapshot> {
    let objects = list_objects(conn, &schema)?;
    let defaults = match object_operations::read_current_defaults(Some(&schema), |sql| {
        object_operations::native_rows(conn, sql)
    }) {
        Ok(defaults) => Some(defaults),
        Err(error) if error.lost_connection() => return Err(error),
        Err(_) => None,
    };
    let context = match conn.query_first_opt::<(u8, String, String, String), _>(
        "SELECT @@lower_case_table_names, @@SESSION.sql_mode, @@SESSION.character_set_client, @@SESSION.collation_connection",
    ) {
        Ok(Some(Ok((lower_case_table_names, sql_mode, character_set_client, collation_connection)))) => {
            Some(SnapshotContext { lower_case_table_names, sql_mode, character_set_client, collation_connection })
        }
        Err(error) if error.is_connectivity_error() => return Err(error.into()),
        _ => None,
    };
    let foreign_keys = match read_schema_foreign_keys(conn, &schema, true) {
        Ok(keys) => Some(keys),
        Err(error) if error.lost_connection() => return Err(error),
        Err(_) => None,
    };
    let mut snapshot = snapshot_objects(conn, schema, objects)?;
    snapshot.defaults = defaults;
    snapshot.context = context;
    snapshot.foreign_keys = foreign_keys;
    Ok(snapshot)
}

fn snapshot_objects(
    conn: &mut Conn,
    schema: String,
    objects: Vec<DbObject>,
) -> DbResult<SchemaSnapshot> {
    let omitted_objects = objects.len().saturating_sub(MAX_SNAPSHOT_OBJECTS);
    let mut remaining = SNAPSHOT_DEFINITION_LIMIT;
    let mut definitions = Vec::with_capacity(objects.len().min(MAX_SNAPSHOT_OBJECTS));
    for object in objects.into_iter().take(MAX_SNAPSHOT_OBJECTS) {
        let mut context = None;
        let definition = if remaining == 0 {
            Err("结构快照已达到容量上限。".into())
        } else {
            match show_create_with_context(conn, &object) {
                Ok((ddl, metadata)) if ddl.len() <= remaining => {
                    remaining -= ddl.len();
                    context = Some(metadata);
                    Ok(ddl)
                }
                Ok(_) => Err("此对象的定义超出了结构快照的剩余容量。".into()),
                Err(error) if error.lost_connection() => return Err(error),
                Err(error) => Err(error.message()),
            }
        };
        definitions.push(SnapshotObject {
            object,
            definition,
            context,
        });
    }
    Ok(SchemaSnapshot {
        schema,
        defaults: None,
        objects: definitions,
        omitted_objects,
        context: None,
        foreign_keys: None,
        origin: None,
    })
}

fn show_create(conn: &mut Conn, object: &DbObject) -> DbResult<String> {
    let (row, name) = show_create_row(conn, object)?;
    read_definition(&row, name)
}

fn show_create_row(conn: &mut Conn, object: &DbObject) -> DbResult<(Row, &'static str)> {
    let (keyword, name) = object.kind.show_create();
    let query = format!("SHOW CREATE {keyword} {}", object.qualified_name());
    let row = conn
        .query_first::<Row, _>(query)?
        .ok_or_else(|| Failure::Message("服务器未返回此对象的定义。".into()))?;
    Ok((row, name))
}

fn read_definition(row: &Row, name: &str) -> DbResult<String> {
    let ddl_index = row
        .columns_ref()
        .iter()
        .position(|column| column.name_str().eq_ignore_ascii_case(name));
    let ddl = ddl_index
        .and_then(|index| row.get_opt::<Option<String>, _>(index))
        .transpose()
        .map_err(mysql::Error::from)?
        .flatten();
    ddl.ok_or_else(|| Failure::Message("服务器未向当前账户提供此对象的定义。".into()))
}

fn show_create_with_context(
    conn: &mut Conn,
    object: &DbObject,
) -> DbResult<(String, DefinitionContext)> {
    let (row, expected) = show_create_row(conn, object)?;
    let definition = read_definition(&row, expected)?;
    let field = |name: &str| {
        row.columns_ref()
            .iter()
            .position(|column| column.name_str().eq_ignore_ascii_case(name))
            .and_then(|index| row.get_opt::<Option<String>, _>(index))
            .and_then(Result::ok)
            .flatten()
    };
    let mut context = DefinitionContext {
        sql_mode: field("sql_mode"),
        character_set_client: field("character_set_client"),
        collation_connection: field("collation_connection"),
        time_zone: field("time_zone"),
        trigger_action_order: None,
    };
    if object.kind == ObjectKind::Trigger {
        match conn.exec_first_opt::<u64, _, _>(
            "SELECT ACTION_ORDER FROM information_schema.TRIGGERS WHERE BINARY TRIGGER_SCHEMA = BINARY :schema AND BINARY TRIGGER_NAME = BINARY :name",
            params! { "schema" => &object.schema, "name" => &object.name },
        ) {
            Ok(Some(Ok(order))) => context.trigger_action_order = Some(order),
            Err(error) if error.is_connectivity_error() => return Err(error.into()),
            _ => {}
        }
    }
    Ok((definition, context))
}

fn f8_plan_joins(sql: &str) -> Option<bool> {
    if crate::sql_tools::needs_unmodified_query_diagnostics(sql) {
        return None;
    }
    if joined_editable::is_joined_query(sql) {
        joined_editable::can_plan_join(sql).then_some(true)
    } else {
        editable_query::can_plan_single(sql).then_some(false)
    }
}

fn editable_query_options(
    session: &mut Session,
    id: u64,
    sql: &str,
    joined: bool,
    cancellation: &Mutex<CancellationState>,
    monitor: &Mutex<OperationMonitor>,
) -> Result<editable_query::EditableRead<Opts>, (String, bool)> {
    use editable_query::EditableRead;

    let (schema, sql_mode) = session
        .conn
        .query_first_opt::<(Option<String>, String), _>("SELECT DATABASE(), @@SESSION.sql_mode")
        .map_err(|error| {
            let disconnected = error.is_connectivity_error();
            (mysql_error(error), disconnected)
        })?
        .transpose()
        .map_err(|error| (mysql_error(error.into()), false))?
        .ok_or_else(|| ("服务器未返回当前数据库和 SQL 模式。".into(), false))?;
    let sources = if joined {
        joined_editable::editable_sources(sql, &sql_mode)
    } else {
        editable_query::editable_source(sql, &sql_mode).map(|source| vec![source])
    };
    let unknown = || EditableRead::ReadOnly("无法核实查询的永久表来源。".into());
    let Some(sources) = sources else {
        return Ok(unknown());
    };
    let mut checked = std::collections::HashSet::new();
    for (source_schema, table) in sources {
        let Some(database) = source_schema.as_deref().or(schema.as_deref()) else {
            return Ok(unknown());
        };
        let qualified = format!(
            "{}.{}",
            quote_identifier(database),
            quote_identifier(&table)
        );
        if !checked.insert(qualified.clone()) {
            continue;
        }
        // Both PREPARE and its column definitions can disguise a session-local shadow.
        // SHOW CREATE resolves the name on the editor connection and identifies TEMPORARY.
        let (report, disconnected, _) = execute_cancellable(
            session,
            id,
            &format!("SHOW CREATE TABLE {qualified}"),
            1,
            cancellation,
            Some(MonitoredQuery::Metadata(monitor)),
        );
        monitor
            .lock()
            .unwrap()
            .check_cancelled()
            .map_err(|error| (error, disconnected))?;
        if disconnected {
            let error = report
                .error
                .unwrap_or_else(|| "核实查询源表时连接已断开。".into());
            return Err((error, true));
        }
        let definition = report
            .sets
            .first()
            .filter(|set| {
                set.columns
                    .get(1)
                    .is_some_and(|name| name.eq_ignore_ascii_case("Create Table"))
            })
            .and_then(|set| set.rows.first())
            .and_then(|row| row.get(1));
        match definition {
            Some(Cell::Text(ddl)) if ddl.starts_with("CREATE TEMPORARY TABLE ") => {
                return Ok(EditableRead::ReadOnly(
                    "结果来自当前会话的临时表，以只读方式显示。".into(),
                ));
            }
            Some(Cell::Text(ddl)) if ddl.starts_with("CREATE TABLE ") => {}
            _ => return Ok(unknown()),
        }
    }
    let mode = crate::sql_tools::mysql_text_literal(&sql_mode);
    Ok(EditableRead::Editable(
        OptsBuilder::from_opts(session.opts.clone())
            .db_name(schema)
            .init(vec![format!("SET SESSION sql_mode = {mode}")])
            .into(),
    ))
}

fn try_f8_editable(
    opts: &Opts,
    sql: &str,
    limit: usize,
    joined: bool,
    monitor: &Mutex<OperationMonitor>,
) -> Result<Option<Response>, String> {
    use editable_query::EditableRead;

    if joined {
        Ok(
            match joined_editable::try_query_joined_editable(opts, sql, limit, Some(monitor))? {
                EditableRead::Editable(page) if page.targets.is_empty() => Some(Response::Queried(
                    QueryReport::from_read_only_result(page.result, page.query_timing),
                )),
                EditableRead::Editable(page) => Some(Response::JoinedEditablePage(page)),
                EditableRead::ReadOnly(_) => None,
            },
        )
    } else {
        Ok(
            match editable_query::try_query_editable(opts, sql, limit, Some(monitor))? {
                EditableRead::Editable(page) => Some(Response::EditablePage(page)),
                EditableRead::ReadOnly(_) => None,
            },
        )
    }
}

fn run_editor_query(
    session: &mut Session,
    id: u64,
    sql: &str,
    limit: usize,
    cancellation: &Mutex<CancellationState>,
    monitor: Option<&Mutex<OperationMonitor>>,
) -> (QueryReport, bool) {
    let (mut report, disconnected, _) = execute_cancellable(
        session,
        id,
        sql,
        limit,
        cancellation,
        monitor.map(MonitoredQuery::Statement),
    );
    if !disconnected {
        result_duplicate::bind_native(&mut report, &session.identity);
    }
    (report, disconnected)
}

enum MonitoredQuery<'a> {
    Metadata(&'a Mutex<OperationMonitor>),
    Statement(&'a Mutex<OperationMonitor>),
}

impl<'a> MonitoredQuery<'a> {
    fn start(self) -> Result<std::sync::MutexGuard<'a, OperationMonitor>, String> {
        match self {
            Self::Metadata(monitor) => {
                let guard = monitor.lock().unwrap();
                guard.check_cancelled()?;
                Ok(guard)
            }
            Self::Statement(monitor) => {
                let mut guard = monitor.lock().unwrap();
                guard.begin_editor_query()?;
                Ok(guard)
            }
        }
    }
}

fn execute_cancellable(
    session: &mut Session,
    id: u64,
    sql: &str,
    limit: usize,
    cancellation: &Mutex<CancellationState>,
    monitor: Option<MonitoredQuery<'_>>,
) -> (QueryReport, bool, Option<String>) {
    let target = Arc::new(QueryTarget {
        id,
        connection_id: session.conn.connection_id(),
        token: session.token.clone(),
        opts: session.opts.clone(),
        identity: session.query_identity.clone(),
    });
    let tagged_sql = target.fallback_tag().map(|tag| {
        let (bom, body) = sql
            .strip_prefix('\u{feff}')
            .map_or(("", sql), |body| ("\u{feff}", body));
        format!("{bom}{tag}{body}")
    });
    {
        let _guard = match monitor.map(MonitoredQuery::start).transpose() {
            Ok(guard) => guard,
            Err(error) => {
                return (
                    QueryReport {
                        error: Some(error),
                        ..QueryReport::default()
                    },
                    false,
                    None,
                );
            }
        };
        // Publish the target before releasing the monitor so Stop sees it when the mode changes.
        cancellation.lock().unwrap().active = Some(target);
    }
    let (mut report, disconnected) = execute_sql_wire(
        &mut session.conn,
        sql,
        tagged_sql.as_deref().unwrap_or(sql),
        limit,
        session.mariadb,
    );
    let state = std::mem::take(&mut *cancellation.lock().unwrap());
    if disconnected || state.requested || state.acknowledged || state.retire_session {
        report.continue_after_error = false;
    }
    if state.retire_session {
        report
            .error
            .get_or_insert_with(|| "无法确认取消指令是否送达，已关闭当前会话。".into());
    } else if state.acknowledged && report.error.is_none() {
        report.error = Some("服务器已接受停止查询的请求。".into());
    }
    (report, disconnected || state.retire_session, tagged_sql)
}

fn attempt_cancel(
    target: &Arc<QueryTarget>,
    cancellation: &Mutex<CancellationState>,
) -> Result<(), String> {
    const FINISHED: &str = "查询在取消指令发出前已经结束。";
    if !cancellation.lock().unwrap().is_current(target) {
        return Err(FINISHED.into());
    }
    let mut control = open_query_control(&target.opts)
        .map_err(|error| format!("无法建立用于取消查询的连接。 {}", mysql_error(error)))?;

    let verified = verify_query_connection(&mut control, target).unwrap_or(false)
        || verify_query_attribute(&mut control, target)?;

    // Keep the editor from advancing until the server acknowledges the KILL command.
    let mut state = cancellation.lock().unwrap();
    if !state.is_current(target) {
        return Err(FINISHED.into());
    }
    if !verified {
        return Err("无法在服务器上确认当前会话有正在执行的查询。".into());
    }
    match control.query_drop(format!("KILL QUERY {}", target.connection_id)) {
        Ok(()) => {
            state.acknowledged = true;
            Ok(())
        }
        Err(error) if error.is_connectivity_error() => {
            state.retire_session = true;
            Err(format!(
                "无法确认取消指令是否送达，当前会话将关闭。 {}",
                mysql_error(error)
            ))
        }
        Err(error) => Err(mysql_error(error)),
    }
}

fn query_server_id(conn: &mut Conn) -> mysql::Result<Option<(String, String)>> {
    let id = conn
        .query_first_opt::<(String, String), _>(
            "SHOW GLOBAL VARIABLES WHERE Variable_name IN ('server_uid', 'server_uuid')",
        )?
        .transpose()?;
    Ok(
        id.filter(|(_, value)| {
            !value.is_empty() && value != "00000000-0000-0000-0000-000000000000"
        }),
    )
}

fn query_process_row(conn: &mut Conn, id: u32) -> mysql::Result<Option<Row>> {
    let mut listing = conn.query_iter("SHOW FULL PROCESSLIST")?;
    for row in listing.iter().into_iter().flatten() {
        let row = row?;
        if row.get_opt::<u32, _>("Id").and_then(Result::ok) == Some(id) {
            return Ok(Some(row));
        }
    }
    Ok(None)
}

fn process_source(row: &Row) -> Option<(String, String)> {
    Some((
        row.get_opt::<String, _>("User")?.ok()?,
        row.get_opt::<String, _>("Host")?.ok()?,
    ))
}

fn process_info(row: &Row) -> Option<String> {
    let command = row.get_opt::<String, _>("Command")?.ok()?;
    if matches!(command.as_str(), "Sleep" | "Killed") {
        return None;
    }
    row.get_opt::<String, _>("Info")?.ok()
}

fn verify_query_connection(conn: &mut Conn, target: &QueryTarget) -> mysql::Result<bool> {
    if let Some(identity) = &target.identity
        && query_server_id(conn)?.as_ref() != Some(&identity.server_id)
    {
        return Ok(false);
    }
    let Some(row) = query_process_row(conn, target.connection_id)? else {
        return Ok(false);
    };
    let Some(info) = process_info(&row) else {
        return Ok(false);
    };
    if let Some(identity) = &target.identity {
        // Verify the originating TCP endpoint and account on the same database instance.
        // The active SQL text may be a different statement inside a stored procedure.
        return Ok(process_source(&row)
            .is_some_and(|(user, source)| user == identity.user && source == identity.source));
    }
    Ok(target
        .fallback_tag()
        .is_some_and(|tag| info.trim_start_matches('\u{feff}').starts_with(&tag)))
}

fn verify_query_attribute(conn: &mut Conn, target: &QueryTarget) -> Result<bool, String> {
    // The numeric connection ID alone can match a different session behind a proxy.
    const VERIFY_TARGET: &str = "
        SELECT 1 FROM performance_schema.session_account_connect_attrs a
        JOIN information_schema.PROCESSLIST p ON p.ID = a.PROCESSLIST_ID
        WHERE a.PROCESSLIST_ID = ? AND a.ATTR_NAME = ? AND a.ATTR_VALUE = ?
            AND p.COMMAND NOT IN ('Sleep', 'Killed')
        LIMIT 1";
    let args = (
        target.connection_id,
        SESSION_ATTRIBUTE,
        target.token.as_str(),
    );
    let verified = conn
        .exec_first_opt::<u64, _, _>(VERIFY_TARGET, args)
        .map_err(|error| format!("无法核实要取消的查询。 {}", mysql_error(error)))?
        .transpose()
        .map_err(|error| mysql_error(error.into()))?
        .is_some();

    Ok(verified)
}

fn open_query_control(opts: &Opts) -> mysql::Result<Conn> {
    Conn::new(
        OptsBuilder::from_opts(opts.clone())
            .tcp_connect_timeout(Some(CONTROL_TIMEOUT))
            .read_timeout(Some(CONTROL_TIMEOUT))
            .write_timeout(Some(CONTROL_TIMEOUT)),
    )
}

fn stop_native_detached_query(
    target: &Arc<TableReadTarget>,
    monitor: &Mutex<OperationMonitor>,
) -> mysql::Result<()> {
    let mut control = open_query_control(&target.opts)?;
    let verified = query_process_row(&mut control, target.connection_id)?
        .and_then(|row| process_info(&row))
        .is_some_and(|info| {
            info.starts_with(&target.tag)
                || info
                    .strip_prefix("SELECT ")
                    .is_some_and(|sql| sql.starts_with(&target.tag))
        });

    // Keep the detached reader registered until the control round-trip finishes. The SQL
    // marker binds its numeric ID to this request, even if a proxy routes the control connection.
    let status = monitor.lock().unwrap();
    if verified
        && status
            .table_read
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, target))
    {
        control.query_drop(format!("KILL CONNECTION {}", target.connection_id))?;
    }
    Ok(())
}

fn execute_sql(conn: &mut Conn, sql: &str, limit: usize) -> (QueryReport, bool) {
    execute_sql_wire(conn, sql, sql, limit, false)
}

fn execute_sql_wire(
    conn: &mut Conn,
    sql: &str,
    wire_sql: &str,
    limit: usize,
    mariadb: bool,
) -> (QueryReport, bool) {
    let source_sql = Arc::<str>::from(sql);
    let capture_origins = crate::sql_tools::split_script(sql)
        .into_iter()
        .all(|statement| {
            use crate::sql_tools::{EditorTokenKind, editor_tokens};

            // SHOW can return information_schema metadata for synthesized columns.
            editor_tokens(statement.sql, false, false)
                .find(|token| {
                    !matches!(
                        token.kind,
                        EditorTokenKind::Protected { .. } | EditorTokenKind::Symbol('(')
                    )
                })
                .is_some_and(|token| {
                    ["SELECT", "WITH", "TABLE"]
                        .iter()
                        .any(|word| token.text.eq_ignore_ascii_case(word))
                })
        });
    let mut report = QueryReport::default();
    let started = Instant::now();
    let result = conn.query_iter(wire_sql);
    let first_response = started.elapsed();
    if matches!(&result, Ok(_) | Err(mysql::Error::MySqlError(_))) {
        report.diagnostics.execution = Some(first_response);
    }
    let (error, disconnected) = match result {
        Ok(mut result) => {
            let mut remaining = RESULT_MEMORY_LIMIT;
            let mut error = None;
            while let Some(mut source) = result.iter() {
                let result_columns = source.columns();
                // The driver's warning count is available in the immediate OK packet.
                let warning_count = result_columns
                    .as_ref()
                    .is_empty()
                    .then(|| source.warnings());
                let (columns, binary_columns): (Vec<_>, Vec<_>) = result_columns
                    .as_ref()
                    .iter()
                    .map(|column| (column.name_str().into_owned(), column_is_binary(column)))
                    .unzip();
                let excel_types = result_columns
                    .as_ref()
                    .iter()
                    .map(transfer::ExcelColumnType::from_native)
                    .collect();
                let export_columns = result_columns
                    .as_ref()
                    .iter()
                    .map(transfer::ResultColumn::from_native)
                    .collect();
                let record = report.sets.len() < MAX_RESULT_SETS;
                let column_origins = if capture_origins && record {
                    result_columns
                        .as_ref()
                        .iter()
                        .map(ColumnOrigin::from_native)
                        .collect()
                } else {
                    Vec::new()
                };
                let mut set = ResultSet {
                    source_sql: Some(Arc::clone(&source_sql)),
                    columns,
                    column_hints: Vec::new(),
                    column_origins,
                    excel_types,
                    export_columns,
                    duplicate_source: if record {
                        result_duplicate::capture_native(sql, result_columns.as_ref())
                    } else {
                        None
                    },
                    notice: None,
                    rows: Vec::new(),
                    affected_rows: source.affected_rows(),
                    last_insert_id: source.last_insert_id(),
                    truncated: false,
                };
                let mut result_finished = started;
                while let Some(row) = source.next_with_boundary(Some(&mut result_finished)) {
                    match row {
                        Ok(row)
                            if record
                                && !set.truncated
                                && set.rows.len() < limit.min(MAX_RESULT_ROWS) =>
                        {
                            let cells: Vec<_> = row
                                .unwrap()
                                .into_iter()
                                .zip(&binary_columns)
                                .map(|(value, &binary)| cell(value, binary))
                                .collect();
                            let size = cells.len() * size_of::<Cell>()
                                + size_of::<Vec<Cell>>()
                                + cells
                                    .iter()
                                    .map(|cell| match cell {
                                        Cell::Null => 0,
                                        Cell::Text(value) => value.len(),
                                        Cell::Binary(value) => value.len(),
                                    })
                                    .sum::<usize>();
                            if size <= remaining {
                                remaining -= size;
                                set.rows.push(cells);
                            } else {
                                set.truncated = true;
                            }
                        }
                        Ok(_) => set.truncated = true,
                        Err(problem) => {
                            error = Some(problem);
                            break;
                        }
                    }
                }
                // Later statement errors are exposed as an iterator with no result metadata.
                if error.is_none() || !set.columns.is_empty() {
                    if record {
                        report.sets.push(set);
                        report.diagnostics.results.push(ResultDiagnostics {
                            elapsed: result_finished.duration_since(started),
                            warning_count,
                        });
                    } else {
                        report.omitted_sets += 1;
                    }
                }
                if error.is_some() {
                    break;
                }
            }
            let disconnected = error
                .as_ref()
                .is_some_and(mysql::Error::is_connectivity_error);
            (error, disconnected)
        }
        Err(error) => {
            let disconnected = error.is_connectivity_error();
            (Some(error), disconnected)
        }
    };
    report.elapsed = started.elapsed();
    report.continue_after_error = matches!(
        &error,
        Some(mysql::Error::MySqlError(error))
            if can_continue_after_server_error(u32::from(error.code), Some(&error.state), mariadb)
    );
    report.error = error.map(mysql_error);
    (report, disconnected)
}

fn column_is_binary(column: &Column) -> bool {
    use ColumnType::*;
    matches!(column.column_type(), MYSQL_TYPE_BIT | MYSQL_TYPE_GEOMETRY)
        || column.character_set() == 63
            && matches!(
                column.column_type(),
                MYSQL_TYPE_VARCHAR
                    | MYSQL_TYPE_VAR_STRING
                    | MYSQL_TYPE_STRING
                    | MYSQL_TYPE_TINY_BLOB
                    | MYSQL_TYPE_MEDIUM_BLOB
                    | MYSQL_TYPE_LONG_BLOB
                    | MYSQL_TYPE_BLOB
            )
}

fn cell(value: Value, binary: bool) -> Cell {
    match value {
        Value::NULL => Cell::Null,
        Value::Bytes(bytes) if binary => Cell::Binary(bytes),
        Value::Bytes(bytes) => match String::from_utf8(bytes) {
            Ok(value) => Cell::Text(value),
            Err(error) => Cell::Binary(error.into_bytes()),
        },
        // Text-protocol rows normally contain only NULL or bytes.
        value => Cell::Text(value.as_sql(true)),
    }
}

pub(crate) fn check_import_cancelled(cancel: Option<&AtomicBool>) -> Result<(), String> {
    if cancel.is_some_and(|cancel| cancel.load(Ordering::Acquire)) {
        Err(IMPORT_CANCELLED.to_owned())
    } else {
        Ok(())
    }
}

fn import_rows(
    opts: &Opts,
    object: DbObject,
    columns: Vec<String>,
    rows: Vec<Vec<Option<String>>>,
    cancel: Option<&AtomicBool>,
) -> Result<u64, String> {
    text_import::run(
        opts,
        TextImport {
            object,
            columns,
            rows,
            conflict: OdbcConflict::Abort,
            expected_ddl: None,
        },
        cancel,
    )
    .map(|counts| counts.rows)
}

fn validate_import_rows(
    object: &DbObject,
    columns: &[String],
    rows: &[Vec<Cell>],
) -> Result<usize, String> {
    if object.kind != ObjectKind::Table {
        return Err("只能导入到表中。".into());
    }
    let distinct: HashSet<_> = columns.iter().map(|name| name.to_lowercase()).collect();
    if columns.is_empty()
        || columns.len() > MAX_IMPORT_COLUMNS
        || distinct.len() != columns.len()
        || columns.iter().any(String::is_empty)
    {
        return Err(format!(
            "请选择 1 到 {MAX_IMPORT_COLUMNS} 个不重复的目标列。"
        ));
    }
    if rows.len() > MAX_IMPORT_ROWS {
        return Err(format!("每次最多导入 {MAX_IMPORT_ROWS} 行数据。"));
    }
    if rows.iter().any(|row| row.len() != columns.len()) {
        return Err("每行数据都必须与目标列一一对应。".into());
    }
    let size = rows.iter().map(|row| import_row_size(row)).sum::<usize>();
    if size > IMPORT_MEMORY_LIMIT {
        return Err("导入数据超过 32 MiB 上限。".into());
    }
    Ok(size)
}

pub(crate) fn import_row_size(row: &[Cell]) -> usize {
    size_of::<Vec<Cell>>()
        + size_of_val(row)
        + row
            .iter()
            .map(|cell| match cell {
                Cell::Null => 0,
                Cell::Text(text) => text.len(),
                Cell::Binary(bytes) => bytes.len(),
            })
            .sum::<usize>()
}

fn lock_import_target(
    tx: &mut mysql::Transaction<'_>,
    object: &DbObject,
    staged_engine: Option<&str>,
) -> Result<(), String> {
    let qualified = object.qualified_name();
    // Hold a metadata lock so the storage engine cannot change before the insert commits.
    tx.query_drop(format!("SELECT 1 FROM {qualified} LIMIT 0"))
        .map_err(mysql_error)?;
    const TRANSACTIONAL_TABLE: &str = "
        SELECT e.TRANSACTIONS, t.ENGINE FROM information_schema.TABLES t
        JOIN information_schema.ENGINES e ON e.ENGINE = t.ENGINE
        WHERE t.TABLE_SCHEMA = :schema AND t.TABLE_NAME = :table
            AND CAST(IF(@@lower_case_table_names = 0, t.TABLE_SCHEMA, LOWER(t.TABLE_SCHEMA)) AS BINARY)
                = CAST(IF(@@lower_case_table_names = 0, :schema, LOWER(:schema)) AS BINARY)
            AND CAST(IF(@@lower_case_table_names = 0, t.TABLE_NAME, LOWER(t.TABLE_NAME)) AS BINARY)
                = CAST(IF(@@lower_case_table_names = 0, :table, LOWER(:table)) AS BINARY)
            AND t.TABLE_TYPE IN ('BASE TABLE', 'SYSTEM VERSIONED')";
    let args = params! { "schema" => object.schema.as_str(), "table" => object.name.as_str() };
    let (transactions, engine) = tx
        .exec_first_opt::<(Option<String>, String), _, _>(TRANSACTIONAL_TABLE, args)
        .map_err(mysql_error)?
        .transpose()
        .map_err(|error| mysql_error(error.into()))?
        .ok_or_else(|| format!("无法核对目标表 {qualified} 的存储引擎。"))?;
    if let Some(expected) = staged_engine
        && !engine.eq_ignore_ascii_case(expected)
    {
        return Err(format!(
            "建表实际使用 {engine}，与选择的 {expected} 不一致。请核对建表 SQL 与服务器支持。"
        ));
    }
    let disposable =
        staged_engine.is_some() && odbc_target::supports_staged_nontransactional_engine(&engine);
    if transactions.as_deref() != Some("YES") && !disposable {
        return Err("导入要求目标表的存储引擎支持事务。".into());
    }
    Ok(())
}

fn import_transaction(
    conn: &mut Conn,
    tables: Vec<OdbcTarget>,
    cancel: Option<&AtomicBool>,
) -> Result<OdbcImportCounts, String> {
    let interrupted = || check_import_cancelled(cancel).is_err();
    let mut tx = conn
        .start_transaction(TxOpts::default())
        .map_err(mysql_error)?;
    let mut counts = OdbcImportCounts::default();
    let track_any = tables.iter().any(|target| target.track_applied_rows);
    if track_any {
        counts.applied_rows.reserve(tables.len());
    }
    for mut target in tables {
        let staged_engine = target
            .create
            .is_some()
            .then(|| target.engine.take().unwrap_or_else(|| "InnoDB".into()));
        let OdbcTarget {
            object,
            columns,
            rows,
            conflict,
            expected_ddl,
            track_applied_rows,
            ..
        } = target;
        if interrupted() {
            return cancel_import(tx);
        }
        let qualified = object.qualified_name();
        let statement =
            lock_import_target(&mut tx, &object, staged_engine.as_deref()).and_then(|()| {
                odbc_target::prepare(
                    &mut tx,
                    &object,
                    &columns,
                    &rows,
                    conflict,
                    expected_ddl.as_deref(),
                )
            });
        let statement = match statement {
            Ok(statement) => statement,
            Err(message) => {
                return Err(match tx.rollback() {
                    Ok(()) => message,
                    Err(error) => format!(
                        "{IMPORT_UNCONFIRMED}{message} 无法确认导入事务已回滚。{}",
                        mysql_error(error)
                    ),
                });
            }
        };
        let mut applied = track_applied_rows.then(|| Vec::with_capacity(rows.len()));
        for (index, row) in rows.into_iter().enumerate() {
            if interrupted() {
                return cancel_import(tx);
            }
            match odbc_target::write_row(&mut tx, &statement, row, conflict, &mut counts) {
                Ok(written) => {
                    if let Some(applied) = &mut applied {
                        applied.push(written);
                    }
                }
                Err(message) => {
                    let row = index + 1;
                    if staged_engine
                        .as_deref()
                        .is_some_and(odbc_target::supports_staged_nontransactional_engine)
                    {
                        return Err(format!(
                            "导入暂存表 {qualified} 第 {row} 行时失败。{message}"
                        ));
                    }
                    return match tx.rollback() {
                        Ok(()) => Err(format!(
                            "导入 {qualified} 第 {row} 行数据时失败，事务已回滚。 {message}"
                        )),
                        Err(error) => Err(format!(
                            "{IMPORT_UNCONFIRMED}导入 {qualified} 第 {row} 行数据时失败，无法确认是否已回滚。 {message} ({})",
                            mysql_error(error)
                        )),
                    };
                }
            }
        }
        if track_any {
            counts.applied_rows.push(applied);
        }
    }
    if interrupted() {
        return cancel_import(tx);
    }
    tx.commit().map_err(|error| {
        format!(
            "{IMPORT_UNCONFIRMED}无法确认导入事务是否已提交。 {}",
            mysql_error(error)
        )
    })?;
    Ok(counts)
}

fn cancel_import(transaction: mysql::Transaction<'_>) -> Result<OdbcImportCounts, String> {
    match transaction.rollback() {
        Ok(()) => Err(IMPORT_CANCELLED.to_owned()),
        Err(error) => Err(format!(
            "{IMPORT_UNCONFIRMED}已请求取消导入，但无法确认事务是否回滚：{}",
            mysql_error(error)
        )),
    }
}

fn check_write_warnings(transaction: &mut mysql::Transaction<'_>) -> Result<(), String> {
    if transaction.warnings() == 0 {
        return Ok(());
    }
    let warnings = transaction
        .query::<(String, u16, String), _>("SHOW WARNINGS LIMIT 3")
        .map_err(mysql_error)?
        .into_iter()
        .map(|(level, code, message)| format!("{level} {code} {message}"))
        .collect::<Vec<_>>()
        .join(" / ");
    Err(format!("数据库返回警告。 {warnings}"))
}

fn normalize_host(input: &str, field: &str) -> Result<String, String> {
    let host = input.trim();
    let ipv6 = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    if ipv6.parse::<Ipv6Addr>().is_ok() {
        return Ok(ipv6.into());
    }
    if !host.is_empty()
        && !host.starts_with('-')
        && host
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Ok(host.into());
    }
    Err(format!("{field}应为主机名或 IP 地址。请单独填写端口。"))
}

fn bracket_ipv6(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.into()
    }
}

struct SshTunnel {
    lifetime: SharedLifetime,
    port: u16,
    diagnostics: Arc<Mutex<VecDeque<String>>>,
}

impl SshTunnel {
    fn open(
        config: &SshConfig,
        target: &str,
        target_port: u16,
        lifetime: &SharedLifetime,
    ) -> Result<Self, String> {
        let host = normalize_host(&config.host, "SSH 主机")?;
        if config.port == 0 {
            return Err("请输入 1 到 65535 之间的 SSH 端口。".into());
        }
        if config
            .user
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        {
            return Err("请输入有效的 SSH 用户名。".into());
        }
        if config.local_port == Some(0) {
            return Err("固定本地端口必须大于零。".into());
        }
        if config.password.is_some() && config.identity_file.is_some() {
            return Err("请选择密码或 SSH 私钥认证。".into());
        }
        if config.key_passphrase.is_some() && config.identity_file.is_none() {
            return Err("填写私钥口令时，请同时选择 SSH 私钥文件。".into());
        }
        if config.password.is_some() && config.user.is_empty() {
            return Err("SSH 密码认证需要用户名。".into());
        }
        let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, config.local_port.unwrap_or(0)))
            .map_err(|error| format!("无法为 SSH 隧道绑定本地端口。 {error}"))?;
        let port = socket
            .local_addr()
            .map_err(|error| format!("无法获取 SSH 隧道的本地端口。 {error}"))?
            .port();
        let native_key = config.key_passphrase.is_some()
            || config
                .identity_file
                .as_deref()
                .is_some_and(password_ssh::is_putty_key);
        if config.password.is_some() || native_key {
            return password_ssh::open(config, host, target, target_port, socket, port, lifetime);
        }
        let forward = format!("127.0.0.1:{port}:{}:{target_port}", bracket_ipv6(target));

        let mut command = ProcessCommand::new("ssh");
        command.args([
            "-F",
            "none",
            "-v",
            "-N",
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "GatewayPorts=no",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ServerAliveInterval=30",
            "-o",
            "ServerAliveCountMax=3",
        ]);
        command.arg("-p").arg(config.port.to_string());
        if let Some(path) = &config.known_hosts_path {
            if !path.is_file() {
                return Err(format!("找不到 SSH 主机密钥文件 {}。", path.display()));
            }
            let path = path
                .to_str()
                .ok_or("SSH 主机密钥文件路径无法表示为 Unicode。")?
                .replace('\\', "/")
                .replace('%', "%%");
            if path.contains(['\r', '\n', '\x00', '"']) {
                return Err("SSH 主机密钥文件路径包含无效字符。".into());
            }
            command
                .arg("-o")
                .arg(format!("UserKnownHostsFile=\"{path}\""));
        }
        if !config.user.is_empty() {
            command.arg("-l").arg(&config.user);
        }
        if let Some(path) = &config.identity_file {
            if !path.is_file() {
                return Err(format!("找不到 SSH 私钥文件 {}。", path.display()));
            }
            command.args(["-o", "IdentitiesOnly=yes", "-i"]).arg(path);
        }
        command.arg("-L").arg(forward).arg("--").arg(host);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }

        let stderr = {
            let mut state = lifetime.lock().unwrap();
            if state.stopped {
                return Err("数据库连接服务已停止。".into());
            }
            drop(socket);
            let mut child = command.spawn().map_err(|error| {
                format!("无法启动 OpenSSH。请确认系统中的 ssh 命令可用。 {error}")
            })?;
            let stderr = child.stderr.take().expect("piped SSH stderr");
            state.ssh = Some(child);
            stderr
        };

        let diagnostics = Arc::new(Mutex::new(VecDeque::new()));
        let (ready, open) = mpsc::channel();
        let stderr_diagnostics = Arc::clone(&diagnostics);
        thread::spawn(move || read_ssh_logs(stderr, port, ready, &stderr_diagnostics));

        let tunnel = Self {
            lifetime: Arc::clone(lifetime),
            port,
            diagnostics,
        };
        match open.recv_timeout(SSH_TIMEOUT) {
            Ok(()) => Ok(tunnel),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(tunnel.error("OpenSSH 未能建立隧道。"))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Err(tunnel.error("等待 SSH 隧道建立时超时。")),
        }
    }

    fn error(&self, message: impl AsRef<str>) -> String {
        let diagnostics = self.diagnostics.lock().unwrap();
        if diagnostics.is_empty() {
            message.as_ref().into()
        } else {
            format!(
                "{} {}",
                message.as_ref(),
                diagnostics.iter().cloned().collect::<Vec<_>>().join(" ")
            )
        }
    }
}

impl Drop for SshTunnel {
    fn drop(&mut self) {
        let ssh = {
            let mut lifetime = self.lifetime.lock().unwrap();
            if let Some(stop) = lifetime.password_ssh.take() {
                let _ = stop.send(());
            }
            lifetime.ssh.take()
        };
        terminate_ssh(ssh);
    }
}

fn terminate_ssh(ssh: Option<Child>) {
    if let Some(mut child) = ssh {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn read_ssh_logs(
    stderr: ChildStderr,
    port: u16,
    ready: mpsc::Sender<()>,
    diagnostics: &Mutex<VecDeque<String>>,
) {
    let listen_message = format!("Local forwarding listening on 127.0.0.1 port {port}.");
    let mut matching_listener = false;
    let mut reader = BufReader::new(stderr);
    let mut buffer = Vec::new();
    // Drain stderr for the lifetime of the child even after signaling readiness.
    while reader
        .read_until(b'\n', &mut buffer)
        .is_ok_and(|bytes| bytes > 0)
    {
        let line = String::from_utf8_lossy(&buffer);
        let line = line.trim();
        if line.starts_with("debug1:") && line.ends_with(&listen_message) {
            matching_listener = true;
        } else if matching_listener
            && line.starts_with("debug1: channel ")
            && line.contains("new port-listener")
        {
            let _ = ready.send(());
            matching_listener = false;
        } else if !line.is_empty() && !line.starts_with("debug") && !line.starts_with("OpenSSH_") {
            let mut diagnostics = diagnostics.lock().unwrap();
            if diagnostics.len() == 6 {
                diagnostics.pop_front();
            }
            diagnostics.push_back(line.chars().take(600).collect());
        }
        buffer.clear();
    }
}
