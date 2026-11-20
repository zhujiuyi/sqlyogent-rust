use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::AtomicBool};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use zeroize::Zeroizing;

use crate::database::{
    Cell, Command, DbObject, IMPORT_CANCELLED, IMPORT_MEMORY_LIMIT, MAX_IMPORT_COLUMNS, ObjectKind,
    OdbcColumnDefinition, OdbcConflict, OdbcForeignKey, OdbcImportCounts, OdbcReset, OdbcTarget,
    Response, Worker, check_import_cancelled, import_row_size, validate_odbc_create_table_sql,
    validate_odbc_engine,
};
use crate::file_source::{self, FileSource};
use crate::odbc::{
    self, DiscoveredTable, SOURCE_SERVER_ID_SQL, SourceAction, SourceActionPlan, SourceColumn,
    SourceData, SourceForeignKey, SourceLogin, SourceTable, execute_source_write,
    mapping::{key_name, map_foreign_key, map_indexes, same_source_table, source_table_name},
    read_for_source_write,
};

use super::{request, validate_identifier};

mod continuation;
mod target_strategy;
use target_strategy::ImportPlan;
pub use target_strategy::{EffectiveTableStrategy, ExistingTarget, MissingTarget, TableStrategy};

pub const MAX_TABLES: usize = 128;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OdbcJob {
    pub schema: String,
    source: Option<Source>,
    table: Option<String>,
    columns: Option<Vec<Column>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tables: Option<Vec<Table>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery: Option<Discovery>,
    #[serde(default)]
    pub create_table: bool,
    #[serde(default)]
    pub reset: OdbcReset,
    #[serde(default)]
    pub structure_only: bool,
    #[serde(default)]
    pub continue_on_error: bool,
    #[serde(default)]
    pub conflict: OdbcConflict,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Discovery {
    pub source: Source,
    pub catalog: Option<String>,
    pub schema: Option<String>,
    #[serde(default)]
    pub copy_indexes: bool,
    #[serde(default)]
    pub copy_foreign_keys: bool,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Table {
    pub source: Source,
    pub table: String,
    pub columns: Vec<Column>,
    pub conflict: Option<OdbcConflict>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<TableStrategy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_sql: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
}

struct TableSpec<'a> {
    name: &'a str,
    source: &'a Source,
    columns: Option<&'a [Column]>,
    create_sql: Option<&'a str>,
    engine: Option<&'a str>,
    conflict: OdbcConflict,
    strategy: Option<TableStrategy>,
}

struct PreparedTable {
    target: OdbcTarget,
    source_table: Option<SourceTable>,
    access_file: Option<AccessFileIdentity>,
    foreign_keys: Vec<SourceForeignKey>,
    mapping: Vec<(String, String)>,
    source_action: Option<SourceActionPlan>,
}

#[derive(PartialEq, Eq)]
struct AccessFileIdentity {
    database: same_file::Handle,
    link_folder: Option<PathBuf>,
}

fn resolve_file_paths(file: &mut FileSource, directory: &Path) {
    if file.path.is_relative() && !file.path.as_os_str().is_empty() {
        file.path = directory.join(&file.path);
    }
    if let Some(folder) = &mut file.access_link_folder
        && folder.is_relative()
        && !folder.as_os_str().is_empty()
    {
        *folder = directory.join(&*folder);
    }
}

pub(super) enum Login<'a> {
    Dsn {
        name: &'a str,
        user: Option<Zeroizing<String>>,
        password: Option<Zeroizing<String>>,
    },
    ConnectionString(Zeroizing<String>),
}

impl Login<'_> {
    pub(super) fn borrow(&self) -> SourceLogin<'_> {
        match self {
            Self::Dsn {
                name,
                user,
                password,
            } => SourceLogin::Dsn {
                name,
                user: user.as_ref().map(|value| value.as_str()),
                password: password.as_ref().map(|value| value.as_str()),
            },
            Self::ConnectionString(connection) => SourceLogin::ConnectionString(connection),
        }
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub dsn: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_dsn: Option<PathBuf>,
    pub user_env: Option<String>,
    pub password_env: Option<String>,
    pub connection_env: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sql: String,
    pub indexes_from: Option<SourceTable>,
    #[serde(default)]
    pub copy_foreign_keys: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_import: Option<SourceAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_source: Option<FileSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_table: Option<String>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Column {
    pub source: String,
    pub target: String,
    pub definition: Option<OdbcColumnDefinition>,
}

#[derive(Default)]
pub(super) struct OdbcSummary {
    counts: OdbcImportCounts,
    columns: usize,
    tables: usize,
    source_rows: Option<usize>,
    pub(super) cancelled: bool,
    halt_reason: Option<String>,
    table_results: Vec<continuation::TableResult>,
    separate_publications: bool,
}

impl OdbcSummary {
    pub(super) fn failure(&self) -> Option<&str> {
        self.halt_reason.as_deref().or_else(|| {
            if self.cancelled {
                Some("已停止导入。")
            } else if self.table_results.iter().any(|table| !table.completed()) {
                Some("部分表的导入或源表操作未完成。")
            } else {
                None
            }
        })
    }

    pub(super) fn fields(&self) -> Map<String, Value> {
        let mut fields: Map<String, Value> = [
            ("rows".into(), json!(self.counts.rows)),
            ("skipped".into(), json!(self.counts.skipped)),
            ("replaced".into(), json!(self.counts.replaced)),
            ("columns".into(), json!(self.columns)),
            ("tables".into(), json!(self.tables)),
        ]
        .into_iter()
        .collect();
        if !self.table_results.is_empty() {
            fields.insert("table_results".into(), json!(self.table_results));
        }
        if self.separate_publications {
            fields.insert("separate_publications".into(), json!(true));
        }
        if self.cancelled || !self.table_results.is_empty() {
            let published = if self.tables > 0 {
                Some(true)
            } else if self
                .table_results
                .iter()
                .any(|table| table.publish_unknown())
            {
                None
            } else {
                Some(false)
            };
            fields.insert("published".into(), json!(published));
            fields.insert("source_rows".into(), json!(self.source_rows));
        }
        fields
    }

    pub(super) fn description(&self) -> String {
        let mut message = format!(
            "已向 {} 张表导入 {} 行，映射 {} 列。",
            self.tables, self.counts.rows, self.columns
        );
        if self.separate_publications {
            message.push_str(" 新建或替换与追加分别提交。");
        }
        if let Some(source_rows) = self.source_rows {
            message.push_str(&format!(" 已处理 {source_rows} 行源记录。"));
        }
        for table in &self.table_results {
            message.push('\n');
            message.push_str(&table.description());
        }
        let detail = self.counts.detail();
        if !detail.is_empty() {
            message.push(' ');
            message.push_str(&detail);
        }
        for table in &self.table_results {
            if let Some(detail) = table.detail() {
                message.push('\n');
                message.push_str(&detail);
            }
        }
        message
    }
}

impl Source {
    pub(super) fn validate(&self) -> Result<(), String> {
        if let Some(file) = &self.file_source {
            self.validate_file_source(file)?;
            let table = self
                .file_table
                .as_deref()
                .filter(|name| !name.trim().is_empty() && !name.contains('\0'))
                .ok_or("请为文件来源选择一张表或工作表。")?;
            if let Some(source) = &self.indexes_from {
                if source.table != table || source.catalog.is_some() || source.schema.is_some() {
                    return Err("Access 索引和外键须与所选源表一致，无需填写目录或架构。".into());
                }
            } else if self.copy_foreign_keys {
                return Err("复制 Access 外键需要同时复制索引。".into());
            }
            return Ok(());
        }
        self.validate_login()?;
        odbc::checked_select(&self.sql)?;
        if self.copy_foreign_keys && self.indexes_from.is_none() {
            return Err("复制外键需要指定索引和外键的来源表。".into());
        }
        if let Some(table) = &self.indexes_from {
            table.validate()?;
        }
        if let Some(action) = &self.after_import {
            action.validate()?;
        }
        Ok(())
    }

    pub(super) fn validate_login(&self) -> Result<(), String> {
        if self.file_source.is_some() || self.file_table.is_some() {
            return Err("文件来源和 ODBC 连接设置须分别使用。".into());
        }
        match (&self.dsn, &self.file_dsn, &self.connection_env) {
            (Some(dsn), None, None) => odbc::validate_dsn(dsn)?,
            (None, Some(path), None) => odbc::validate_file_dsn_path(path)?,
            (None, None, Some(_)) if self.user_env.is_none() && self.password_env.is_none() => {}
            _ => return Err("请为 ODBC 源指定一种连接方式（dsn、file_dsn 或 connection_env）。user_env 和 password_env 可与 DSN 或文件 DSN 一起使用。".into()),
        }
        for name in [&self.connection_env, &self.user_env, &self.password_env]
            .into_iter()
            .flatten()
        {
            if name.trim().is_empty() || name.contains('=') || name.chars().any(char::is_control) {
                return Err("请为 ODBC 源填写有效的环境变量名。".into());
            }
        }
        Ok(())
    }

    fn validate_file_source(&self, file: &FileSource) -> Result<(), String> {
        if self.dsn.is_some()
            || self.file_dsn.is_some()
            || self.user_env.is_some()
            || self.password_env.is_some()
            || self.connection_env.is_some()
            || !self.sql.is_empty()
            || self.after_import.is_some()
        {
            return Err("文件来源不能设置 ODBC 连接凭据、SQL 或导入后回写。".into());
        }
        if !file.supports_metadata() && (self.indexes_from.is_some() || self.copy_foreign_keys) {
            return Err("CSV 和 Excel 文件没有可复制的数据库索引或外键。".into());
        }
        file.validate()
    }

    fn resolved_file_source(&self, directory: &Path) -> Option<FileSource> {
        let mut file = self.file_source.clone()?;
        resolve_file_paths(&mut file, directory);
        Some(file)
    }

    pub fn resolve_file_paths(&mut self, directory: &Path) {
        if let Some(file) = &mut self.file_source {
            resolve_file_paths(file, directory);
        }
        if let Some(path) = &mut self.file_dsn
            && !path.as_os_str().is_empty()
            && path.is_relative()
        {
            *path = directory.join(&*path);
        }
    }

    fn access_file_identity(&self, directory: &Path) -> Result<Option<AccessFileIdentity>, String> {
        self.resolved_file_source(directory)
            .filter(|file| file.supports_metadata())
            .map(|file| {
                if !std::fs::metadata(&file.path)
                    .map_err(|error| format!("无法读取 Access 源文件属性。{error}"))?
                    .is_file()
                {
                    return Err("请选择普通的 Access 数据库文件。".into());
                }
                let database = same_file::Handle::from_path(file.path)
                    .map_err(|error| format!("无法确认 Access 源文件。{error}"))?;
                let link_folder = file
                    .access_link_folder
                    .map(|folder| {
                        folder
                            .canonicalize()
                            .map_err(|error| format!("无法确认 Access 链接表文件夹。{error}"))
                    })
                    .transpose()?;
                Ok(AccessFileIdentity {
                    database,
                    link_folder,
                })
            })
            .transpose()
    }

    pub(super) fn discover(&self, directory: &Path) -> Result<Vec<DiscoveredTable>, String> {
        if let Some(file) = self.resolved_file_source(directory) {
            self.validate_file_source(&file)?;
            return Ok(file_source::discover(&file)?
                .into_iter()
                .map(|table| DiscoveredTable {
                    source: SourceTable {
                        catalog: None,
                        schema: None,
                        table,
                    },
                    select_sql: String::new(),
                })
                .collect());
        }
        self.validate_login()?;
        odbc::discover_tables_for_login(self.login(directory)?.borrow())
    }

    fn login(&self, directory: &Path) -> Result<Login<'_>, String> {
        fn variable(name: &str) -> Result<Zeroizing<String>, String> {
            std::env::var(name)
                .map(Zeroizing::new)
                .map_err(|_| format!("无法读取 ODBC 源环境变量 {name}。"))
        }

        let user = self.user_env.as_deref().map(variable).transpose()?;
        let password = self.password_env.as_deref().map(variable).transpose()?;
        if let Some(dsn) = &self.dsn {
            Ok(Login::Dsn {
                name: dsn,
                user,
                password,
            })
        } else if let Some(path) = &self.file_dsn {
            let connection = odbc::file_dsn_connection(
                &directory.join(path),
                user.as_ref().map(|value| value.as_str()),
                password.as_ref().map(|value| value.as_str()),
            )?;
            Ok(Login::ConnectionString(Zeroizing::new(connection)))
        } else {
            let name = self
                .connection_env
                .as_deref()
                .ok_or("ODBC 源缺少连接定义。")?;
            let connection = variable(name)?;
            if !connection.contains('=') {
                return Err("connection_env 须指向完整的 ODBC 连接字符串。".into());
            }
            Ok(Login::ConnectionString(connection))
        }
    }

    pub(super) fn read(
        &self,
        directory: &Path,
        cancel: &AtomicBool,
        columns_only: bool,
    ) -> Result<SourceData, String> {
        if let Some(file) = self.resolved_file_source(directory) {
            let table = self.file_table.as_deref().ok_or("请先选择文件中的表。")?;
            if file.supports_metadata() {
                return file_source::read_with_metadata_and_cancel(
                    &file,
                    table,
                    columns_only,
                    self.indexes_from.is_some(),
                    self.copy_foreign_keys,
                    cancel,
                );
            }
            return file_source::read_with_cancel(&file, table, columns_only, cancel);
        }
        odbc::read_query(
            self.login(directory)?.borrow(),
            &self.sql,
            self.indexes_from.as_ref(),
            self.copy_foreign_keys,
            Some(cancel),
            columns_only,
        )
    }

    fn same_odbc_connection(&self, other: &Self) -> bool {
        self.file_source.is_none()
            && other.file_source.is_none()
            && self.dsn == other.dsn
            && self.file_dsn == other.file_dsn
            && self.user_env == other.user_env
            && self.password_env == other.password_env
            && self.connection_env == other.connection_env
    }
}

impl Discovery {
    fn validate(&self, create: bool) -> Result<(), String> {
        if !create && (self.copy_indexes || self.copy_foreign_keys) {
            return Err("复制源索引或外键需要选择创建或重建目标表。".into());
        }
        if let Some(file) = &self.source.file_source {
            self.source.validate_file_source(file)?;
            if self.source.file_table.is_some()
                || self.source.indexes_from.is_some()
                || self.source.copy_foreign_keys
                || self.catalog.is_some()
                || self.schema.is_some()
            {
                return Err(
                    "动态导入文件时，source 只填写文件配置。Access 索引和外键可在 discovery 中设置。"
                        .into(),
                );
            }
            if !file.supports_metadata() && (self.copy_indexes || self.copy_foreign_keys) {
                return Err("CSV 和 Excel 文件没有可复制的数据库索引或外键。".into());
            }
            return Ok(());
        }
        self.source.validate_login()?;
        if !self.source.sql.trim().is_empty()
            || self.source.indexes_from.is_some()
            || self.source.copy_foreign_keys
            || self.source.after_import.is_some()
        {
            return Err(
                "动态导入全部表的 source 只填写连接信息。请在 discovery 中设置表范围和复制选项。"
                    .into(),
            );
        }
        for value in [self.catalog.as_deref(), self.schema.as_deref()]
            .into_iter()
            .flatten()
        {
            if value.trim().is_empty() || value.contains('\0') || value.encode_utf16().count() > 255
            {
                return Err("请为动态导入选择有效的源目录或架构。".into());
            }
        }
        Ok(())
    }

    fn tables(&self, directory: &Path, cancel: &AtomicBool) -> Result<Vec<Table>, String> {
        check_import_cancelled(Some(cancel))?;
        let discovered = self.source.discover(directory)?;
        check_import_cancelled(Some(cancel))?;
        let scoped = discovered
            .into_iter()
            .filter(|table| {
                self.catalog
                    .as_deref()
                    .is_none_or(|catalog| table.source.catalog.as_deref() == Some(catalog))
                    && self
                        .schema
                        .as_deref()
                        .is_none_or(|schema| table.source.schema.as_deref() == Some(schema))
            })
            .collect::<Vec<_>>();
        let first = scoped.first().ok_or("选定的源范围没有可导入的数据表。")?;
        if self.catalog.is_none()
            && self.schema.is_none()
            && scoped.iter().any(|table| {
                table.source.catalog != first.source.catalog
                    || table.source.schema != first.source.schema
            })
        {
            return Err("ODBC 源返回了多个目录或架构，请指定本次动态导入的范围。".into());
        }
        if scoped.len() > MAX_TABLES {
            return Err(format!("每个 ODBC 作业最多导入 {MAX_TABLES} 张表。"));
        }

        let mut names = HashSet::new();
        scoped
            .into_iter()
            .map(|discovered| {
                check_import_cancelled(Some(cancel))?;
                let table = discovered.source.table.clone();
                validate_detected_name(&table, "表名")?;
                if !names.insert(table.to_lowercase()) {
                    return Err(format!("源范围内的多个表映射到了同一个目标表「{table}」。"));
                }
                let mut source = self.source.clone();
                if source.file_source.is_some() {
                    source.file_table = Some(table.clone());
                } else {
                    source.sql = discovered.select_sql;
                }
                source.indexes_from =
                    (self.copy_indexes || self.copy_foreign_keys).then_some(discovered.source);
                source.copy_foreign_keys = self.copy_foreign_keys;
                Ok(Table {
                    source,
                    table,
                    columns: Vec::new(),
                    conflict: None,
                    strategy: None,
                    create_sql: None,
                    engine: None,
                })
            })
            .collect()
    }
}

impl OdbcJob {
    pub(super) fn new(schema: String) -> Self {
        Self {
            schema,
            source: None,
            table: None,
            columns: None,
            tables: Some(vec![Table::default()]),
            discovery: None,
            create_table: false,
            reset: OdbcReset::None,
            structure_only: false,
            continue_on_error: false,
            conflict: OdbcConflict::default(),
        }
    }

    pub(super) fn normalize_for_edit(&mut self) -> Result<(), String> {
        if self.discovery.is_some() {
            return self.validate_discovery_shape();
        }
        match (
            self.source.take(),
            self.table.take(),
            self.columns.take(),
            &self.tables,
        ) {
            (Some(source), Some(table), Some(columns), None) => {
                self.tables = Some(vec![Table {
                    source,
                    table,
                    columns,
                    conflict: None,
                    strategy: None,
                    create_sql: None,
                    engine: None,
                }]);
                Ok(())
            }
            (None, None, None, Some(_)) => Ok(()),
            _ => Err("ODBC 作业请使用完整的单表定义或 tables 列表。".into()),
        }
    }

    pub(super) fn file_source_paths(&self, directory: &Path) -> Vec<PathBuf> {
        self.source
            .iter()
            .chain(self.tables.iter().flatten().map(|table| &table.source))
            .chain(self.discovery.iter().map(|discovery| &discovery.source))
            .flat_map(|source| {
                source
                    .file_dsn
                    .iter()
                    .chain(source.file_source.iter().flat_map(|file| {
                        std::iter::once(&file.path).chain(file.access_link_folder.iter())
                    }))
            })
            .map(|path| directory.join(path))
            .collect()
    }

    pub(super) fn resolve_file_source_paths(&mut self, directory: &Path) {
        for source in self
            .source
            .iter_mut()
            .chain(
                self.tables
                    .iter_mut()
                    .flatten()
                    .map(|table| &mut table.source),
            )
            .chain(
                self.discovery
                    .iter_mut()
                    .map(|discovery| &mut discovery.source),
            )
        {
            source.resolve_file_paths(directory);
        }
    }

    fn validate_discovery_shape(&self) -> Result<(), String> {
        if self.source.is_some()
            || self.table.is_some()
            || self.columns.is_some()
            || self.tables.is_some()
        {
            return Err("动态导入全部表不能与单表映射或 tables 列表同时使用。".into());
        }
        Ok(())
    }

    fn specifications(&self) -> Result<Vec<TableSpec<'_>>, String> {
        match (&self.source, &self.table, &self.columns, &self.tables) {
            (Some(source), Some(table), Some(columns), None) => Ok(vec![TableSpec {
                name: table,
                source,
                columns: Some(columns),
                create_sql: None,
                engine: None,
                conflict: self.conflict,
                strategy: None,
            }]),
            (None, None, None, Some(tables)) if (1..=MAX_TABLES).contains(&tables.len()) => {
                Ok(tables
                    .iter()
                    .map(|table| TableSpec {
                        name: &table.table,
                        source: &table.source,
                        columns: Some(&table.columns),
                        create_sql: table.create_sql.as_deref(),
                        engine: table.engine.as_deref(),
                        conflict: table.conflict.unwrap_or(self.conflict),
                        strategy: table.strategy.filter(|strategy| !strategy.is_empty()),
                    })
                    .collect())
            }
            _ => Err(format!(
                "请填写单表的 source、table、columns，或使用 tables 指定 1 到 {MAX_TABLES} 张表。"
            )),
        }
    }

    pub(super) fn validate(&self) -> Result<(), String> {
        validate_identifier(&self.schema, "目标数据库名称")?;
        match (self.reset, self.create_table, self.structure_only) {
            (OdbcReset::Recreate, false, _) => {
                return Err("重建目标表需要提供新表定义。".into());
            }
            (OdbcReset::Truncate, true, _) => {
                return Err("清空后导入需要使用现有表定义。".into());
            }
            (_, false, true) => {
                return Err("仅导入表结构需要选择创建或重建目标表。".into());
            }
            _ => {}
        }
        if let Some(discovery) = &self.discovery {
            self.validate_discovery_shape()?;
            return discovery.validate(self.create_table);
        }
        let specifications = self.specifications()?;
        let overrides = specifications.iter().any(|spec| spec.strategy.is_some());
        let mut names = HashSet::new();
        for spec in specifications {
            validate_identifier(spec.name, "目标表名称")?;
            let key = if overrides {
                spec.name.to_owned()
            } else {
                spec.name.to_lowercase()
            };
            if !names.insert(key) {
                return Err("一个 ODBC 作业只能为同一目标表设置一份映射。".into());
            }
            let strategy = spec.effective_strategy(self);
            strategy.validate()?;
            let defines = strategy.defines_table();
            spec.validate(defines)?;
            if let Some(engine) = spec.engine {
                if !defines {
                    return Err("选择存储引擎需要新建或重建目标表。".into());
                }
                validate_odbc_engine(engine)?;
            }
            if let Some(sql) = spec.create_sql {
                if !defines {
                    return Err("编辑建表 SQL 需要选择创建或重建目标表。".into());
                }
                validate_odbc_create_table_sql(
                    &DbObject {
                        schema: self.schema.clone(),
                        name: spec.name.to_owned(),
                        kind: ObjectKind::Table,
                    },
                    sql,
                )?;
            }
            if strategy.structure_only && spec.source.after_import.is_some() {
                return Err("仅导入表结构时不能更新或删除源记录。".into());
            }
        }
        Ok(())
    }

    pub(super) fn run(
        &self,
        worker: &Worker,
        directory: &Path,
        secrets: &[&str],
        cancel: &Arc<AtomicBool>,
    ) -> Result<OdbcSummary, String> {
        check_import_cancelled(Some(cancel))?;
        let discovered = self
            .discovery
            .as_ref()
            .map(|discovery| {
                self.validate_discovery_shape()?;
                discovery.validate(self.create_table)?;
                discovery.tables(directory, cancel)
            })
            .transpose()?;
        let specifications = if let Some(tables) = &discovered {
            tables
                .iter()
                .map(|table| TableSpec {
                    name: &table.table,
                    source: &table.source,
                    columns: None,
                    create_sql: None,
                    engine: None,
                    conflict: self.conflict,
                    strategy: None,
                })
                .collect()
        } else {
            self.specifications()?
        };
        let presence = specifications
            .iter()
            .any(|spec| spec.strategy.is_some())
            .then(|| target_strategy::read_presence(worker, &self.schema, &specifications, cancel))
            .transpose()?;
        if self.continue_on_error || presence.is_some() {
            return Ok(continuation::run(
                self,
                worker,
                directory,
                &specifications,
                presence.as_deref(),
                secrets,
                cancel,
            ));
        }
        let mut prepared = Vec::new();
        let mut column_count = 0;
        let mut remaining = IMPORT_MEMORY_LIMIT;
        for spec in &specifications {
            let table = spec
                .prepare(
                    self,
                    self.default_import_plan(),
                    directory,
                    &mut remaining,
                    cancel,
                )
                .map_err(|error| {
                    if error == IMPORT_CANCELLED {
                        error
                    } else {
                        format!("无法准备目标表 {} 的导入数据。 {error}", spec.name)
                    }
                })?;
            column_count += table.target.columns.len();
            prepared.push(table);
        }
        resolve_foreign_keys(&specifications, &mut prepared, cancel)?;
        verify_source_destinations(worker, &self.schema, &specifications, prepared.iter())?;
        let (tables, source_actions): (Vec<_>, Vec<_>) = prepared
            .into_iter()
            .map(|prepared| (prepared.target, prepared.source_action))
            .unzip();
        let table_count = tables.len();
        match request(
            worker,
            2,
            Command::ImportOdbcTables {
                tables,
                cancel: Arc::clone(cancel),
            },
        )? {
            Response::OdbcImported { counts, .. } => {
                let mut summary = OdbcSummary {
                    counts,
                    columns: column_count,
                    tables: table_count,
                    ..OdbcSummary::default()
                };
                run_source_actions(
                    &specifications,
                    source_actions,
                    &mut summary,
                    directory,
                    cancel,
                )?;
                Ok(summary)
            }
            _ => Err("数据库连接未返回 ODBC 导入结果。".into()),
        }
    }
}

impl TableSpec<'_> {
    fn validate(&self, create: bool) -> Result<(), String> {
        self.source.validate()?;
        if self.source.indexes_from.is_some() && !create {
            return Err("indexes_from 须与 create_table 一起使用。".into());
        }
        let columns = self.columns.unwrap_or_default();
        if !(1..=MAX_IMPORT_COLUMNS).contains(&columns.len()) {
            return Err(format!("请设置 1 到 {MAX_IMPORT_COLUMNS} 个 ODBC 列映射。"));
        }
        let mut sources = HashSet::new();
        let mut targets = HashSet::new();
        for column in columns {
            if self.source.file_source.is_some() {
                if column.source.trim().is_empty() || column.source.contains('\0') {
                    return Err("请填写有效的文件源列名称。".into());
                }
            } else {
                validate_identifier(&column.source, "源列名称")?;
            }
            validate_identifier(&column.target, "目标列名称")?;
            if !sources.insert(column.source.to_lowercase())
                || !targets.insert(column.target.to_lowercase())
            {
                return Err("每个源列和目标列只能映射一次。".into());
            }
            if let Some(definition) = &column.definition {
                if !create {
                    return Err("列定义须与 create_table 一起使用。".into());
                }
                definition.validate()?;
            }
        }
        Ok(())
    }

    fn source_positions(&self, columns: &[SourceColumn]) -> Result<Vec<usize>, String> {
        let Some(mapping) = self.columns else {
            return Ok((0..columns.len()).collect());
        };
        let names: Vec<_> = columns
            .iter()
            .map(|column| column.name.to_lowercase())
            .collect();
        mapping
            .iter()
            .enumerate()
            .map(|(offset, column)| {
                let wanted = column.source.to_lowercase();
                let mut matches = names
                    .iter()
                    .enumerate()
                    .filter(|(_, name)| **name == wanted);
                let mapping = offset + 1;
                let Some((position, _)) = matches.next() else {
                    return Err(format!("第 {mapping} 个映射的源列未出现在读取结果中。"));
                };
                if matches.next().is_some() {
                    return Err(format!(
                        "第 {mapping} 个映射匹配到多个源列，请在 ODBC 查询中指定不同的别名。"
                    ));
                }
                Ok(position)
            })
            .collect()
    }

    fn prepare(
        &self,
        job: &OdbcJob,
        plan: ImportPlan,
        directory: &Path,
        remaining: &mut usize,
        cancel: &AtomicBool,
    ) -> Result<PreparedTable, String> {
        check_import_cancelled(Some(cancel))?;
        let access_file = self.source.access_file_identity(directory)?;
        let without_metadata;
        let read_source = if !plan.create && self.source.indexes_from.is_some() {
            without_metadata = Source {
                indexes_from: None,
                copy_foreign_keys: false,
                ..self.source.clone()
            };
            &without_metadata
        } else {
            self.source
        };
        let (source, source_action) = match &read_source.after_import {
            Some(action) => {
                let login = read_source.login(directory)?;
                let (source, plan) = read_for_source_write(
                    login.borrow(),
                    &read_source.sql,
                    read_source.indexes_from.as_ref(),
                    read_source.copy_foreign_keys,
                    action,
                    cancel,
                )?;
                (source, Some(plan))
            }
            None => (
                read_source.read(directory, cancel, plan.structure_only)?,
                None,
            ),
        };
        if let Some(file) = &access_file
            && self.source.access_file_identity(directory)?.as_ref() != Some(file)
        {
            return Err("读取期间 Access 源文件已被替换，请重新运行。".into());
        }
        let positions = self.source_positions(&source.columns)?;
        let inferred;
        let columns = if let Some(columns) = self.columns {
            columns
        } else {
            inferred = inferred_columns(&source.columns)?;
            &inferred
        };
        let mapping: Vec<_> = columns
            .iter()
            .map(|column| (column.source.clone(), column.target.clone()))
            .collect();
        let indexes = map_indexes(&source.indexes, &mapping)?;
        let definitions = plan.create.then(|| {
            columns
                .iter()
                .zip(&positions)
                .map(|(column, &position)| {
                    column
                        .definition
                        .clone()
                        .unwrap_or_else(|| source.columns[position].definition.clone())
                })
                .collect()
        });
        let rows = source
            .rows
            .into_iter()
            .map(|mut row| {
                check_import_cancelled(Some(cancel))?;
                let row: Vec<_> = positions
                    .iter()
                    .map(|&index| std::mem::replace(&mut row[index], Cell::Null))
                    .collect();
                *remaining = remaining
                    .checked_sub(import_row_size(&row))
                    .ok_or("本次 ODBC 作业的数据总量超过 32 MiB。")?;
                Ok(row)
            })
            .collect::<Result<Vec<_>, String>>()?;
        let target = OdbcTarget {
            object: DbObject {
                schema: job.schema.clone(),
                name: self.name.to_owned(),
                kind: ObjectKind::Table,
            },
            columns: columns.iter().map(|column| column.target.clone()).collect(),
            rows,
            create: definitions,
            create_sql: self.create_sql.filter(|_| plan.create).map(str::to_owned),
            engine: self.engine.filter(|_| plan.create).map(str::to_owned),
            indexes,
            foreign_keys: Vec::new(),
            track_applied_rows: source_action.is_some(),
            conflict: self.conflict,
            reset: plan.reset,
            expected_ddl: plan.expected_ddl,
        };
        Ok(PreparedTable {
            target,
            source_table: source.source_table,
            access_file,
            foreign_keys: source.foreign_keys,
            mapping,
            source_action,
        })
    }
}

fn validate_detected_name(name: &str, kind: &str) -> Result<(), String> {
    if name.trim().is_empty()
        || name.contains('\0')
        || name.ends_with(' ')
        || name.chars().count() > 64
    {
        return Err(format!(
            "源{kind}「{name}」无法在目标端使用同名，请改用指定映射。"
        ));
    }
    Ok(())
}

fn inferred_columns(columns: &[SourceColumn]) -> Result<Vec<Column>, String> {
    let mut names = HashSet::new();
    columns
        .iter()
        .map(|column| {
            validate_detected_name(&column.name, "列名")?;
            if !names.insert(column.name.to_lowercase()) {
                return Err("源表包含无法区分的列名，请改用指定映射。".into());
            }
            Ok(Column {
                source: column.name.clone(),
                target: column.name.clone(),
                definition: None,
            })
        })
        .collect()
}

fn resolve_foreign_keys(
    specifications: &[TableSpec<'_>],
    prepared: &mut [PreparedTable],
    cancel: &AtomicBool,
) -> Result<(), String> {
    for (child_pos, spec) in specifications.iter().enumerate() {
        let keys = mapped_foreign_keys(
            spec,
            &prepared[child_pos],
            prepared.iter().zip(specifications),
            cancel,
        )?;
        prepared[child_pos].target.foreign_keys = keys;
    }
    Ok(())
}

fn mapped_foreign_keys<'a, 'source: 'a>(
    spec: &TableSpec<'_>,
    child: &PreparedTable,
    candidates: impl Iterator<Item = (&'a PreparedTable, &'a TableSpec<'source>)> + Clone,
    cancel: &AtomicBool,
) -> Result<Vec<OdbcForeignKey>, String> {
    check_import_cancelled(Some(cancel))?;
    let mut keys = Vec::with_capacity(child.foreign_keys.len());
    for key in &child.foreign_keys {
        let mut parents = candidates.clone().filter(|(parent, parent_spec)| {
            let same_source = match (&child.access_file, &parent.access_file) {
                (Some(child), Some(parent)) => child == parent,
                (None, None) => spec.source.same_odbc_connection(parent_spec.source),
                _ => false,
            };
            same_source
                && parent.target.create.is_some()
                && parent
                    .source_table
                    .as_ref()
                    .is_some_and(|source| same_source_table(source, &key.referenced))
        });
        let Some((parent, _)) = parents.next() else {
            let hint = if child.access_file.is_some() {
                "请在同一作业中创建或重建该 Access 表，并启用复制索引。"
            } else {
                "请在同一作业中创建或重建该源表，并填写它的索引来源。"
            };
            return Err(format!(
                "表 {} 的外键 {} 引用 {}。{hint}",
                spec.name,
                key_name(key),
                source_table_name(&key.referenced)
            ));
        };
        if parents.next().is_some() {
            return Err(format!(
                "表 {} 的外键 {} 匹配了多个目标表。请为被引用表保留一份映射。",
                spec.name,
                key_name(key)
            ));
        }
        keys.push(map_foreign_key(
            key,
            &child.mapping,
            &parent.target.object.name,
            &parent.mapping,
        )?);
    }
    Ok(keys)
}

fn verify_source_destinations<'a>(
    worker: &Worker,
    target_schema: &str,
    specifications: &[TableSpec<'_>],
    prepared: impl Iterator<Item = &'a PreparedTable>,
) -> Result<(), String> {
    let actions: Vec<_> = prepared
        .filter_map(|table| table.source_action.as_ref())
        .collect();
    let mut seen = HashSet::new();
    for action in &actions {
        let (schema, table) = action.table_identity();
        if !seen.insert((
            action.server_identity(),
            schema.to_lowercase(),
            table.to_lowercase(),
        )) {
            return Err("同一张源表每次只能设置一项导入后的更新或删除操作。".into());
        }
    }
    let overlaps = |action: &SourceActionPlan| {
        let (schema, table) = action.table_identity();
        target_schema.to_lowercase() == schema.to_lowercase()
            && specifications
                .iter()
                .any(|spec| spec.name.to_lowercase() == table.to_lowercase())
    };
    if !actions.iter().any(|action| overlaps(action)) {
        return Ok(());
    }
    let Response::Queried(report) = request(
        worker,
        3,
        Command::Query {
            sql: SOURCE_SERVER_ID_SQL.into(),
            limit: 1,
        },
    )
    .map_err(|error| format!("无法核对同名目标表所在的服务器。{error}"))?
    else {
        return Err("目标连接未返回服务器标识。".into());
    };
    let identity = report
        .sets
        .first()
        .and_then(|set| set.rows.first())
        .filter(|row| report.error.is_none() && row.len() == 5)
        .and_then(|row| {
            row.iter()
                .map(|cell| match cell {
                    Cell::Text(value) => Some(value.as_str()),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
        })
        .ok_or("无法核对同名目标表所在的服务器。")?;
    if actions.iter().any(|action| {
        overlaps(action)
            && action
                .server_identity()
                .iter()
                .map(String::as_str)
                .eq(identity.iter().copied())
    }) {
        return Err("作业目标包括同一服务器上的源表，不能同时执行导入后的源表更新或删除。".into());
    }
    Ok(())
}

fn run_source_actions(
    specifications: &[TableSpec<'_>],
    plans: Vec<Option<SourceActionPlan>>,
    summary: &mut OdbcSummary,
    directory: &Path,
    cancel: &AtomicBool,
) -> Result<(), String> {
    if plans.iter().all(Option::is_none) {
        return Ok(());
    }
    let confirmed = take_confirmed_source_rows(&plans, &mut summary.counts)?;
    let mut total = 0;
    for ((specification, plan), rows) in specifications.iter().zip(plans).zip(confirmed) {
        let (Some(plan), Some(rows)) = (plan, rows) else {
            continue;
        };
        match execute_source_plan(specification, plan, &rows, directory, cancel) {
            Ok(applied) => total += applied,
            Err(error) if error == IMPORT_CANCELLED => {
                summary.cancelled = true;
                summary.halt_reason = Some("目标已提交，后续源表操作已停止。".into());
                break;
            }
            Err(error) => {
                return Err(format!(
                    "目标已提交 {} 行，此前已确认处理 {total} 行源数据。目标表 {} 对应的源表操作未完成。{error}",
                    summary.counts.rows, specification.name
                ));
            }
        }
    }
    summary.source_rows = Some(total);
    Ok(())
}

fn take_confirmed_source_rows(
    plans: &[Option<SourceActionPlan>],
    counts: &mut OdbcImportCounts,
) -> Result<Vec<Option<Vec<bool>>>, String> {
    if plans.iter().all(Option::is_none) {
        return Ok(Vec::new());
    }
    let confirmed = std::mem::take(&mut counts.applied_rows);
    if plans.len() != confirmed.len()
        || plans
            .iter()
            .zip(&confirmed)
            .any(|(plan, rows)| match (plan, rows) {
                (Some(plan), Some(rows)) => rows.len() != plan.len(),
                (None, None) => false,
                _ => true,
            })
    {
        return Err("目标数据已提交，但逐行确认信息不完整，源表操作无法执行。".into());
    }
    Ok(confirmed)
}

fn execute_source_plan(
    specification: &TableSpec<'_>,
    plan: SourceActionPlan,
    rows: &[bool],
    directory: &Path,
    cancel: &AtomicBool,
) -> Result<usize, String> {
    if !rows.iter().any(|&row| row) {
        return Ok(0);
    }
    check_import_cancelled(Some(cancel))?;
    let login = specification.source.login(directory)?;
    execute_source_write(login.borrow(), plan, rows, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn definition() -> Value {
        json!({
            "source": {"dsn": "Example", "sql": "SELECT name, number FROM contacts"},
            "schema": "target_db", "table": "contacts",
            "columns": [{"source": "number", "target": "id"}, {"source": "name", "target": "label"}],
        })
    }

    #[test]
    fn preserves_discovery_in_editor_without_materializing_table_mappings() {
        let mut job: OdbcJob = serde_json::from_value(json!({
            "schema": "target_catalog",
            "create_table": true,
            "structure_only": true,
            "discovery": {
                "source": {"connection_env": "HARBOR_DISCOVERY_FIXTURE_CONNECTION"},
                "catalog": "source_catalog",
                "copy_foreign_keys": true
            }
        }))
        .unwrap();
        job.validate().unwrap();
        job.normalize_for_edit().unwrap();
        let serialized = serde_json::to_value(job).unwrap();
        assert!(serialized.get("tables").is_none());
        assert!(serialized["discovery"]["source"].get("sql").is_none());
        assert_eq!(serialized["discovery"]["catalog"], "source_catalog");
        assert_eq!(serialized["discovery"]["copy_foreign_keys"], true);
        assert_eq!(serialized["structure_only"], true);

        let names = ["编号", "Title"];
        let source_columns = names
            .into_iter()
            .map(|name| SourceColumn {
                name: name.into(),
                definition: OdbcColumnDefinition::default(),
            })
            .collect::<Vec<_>>();
        let mappings = inferred_columns(&source_columns).unwrap();
        assert_eq!(
            mappings
                .iter()
                .map(|mapping| (mapping.source.as_str(), mapping.target.as_str()))
                .collect::<Vec<_>>(),
            [("编号", "编号"), ("Title", "Title")],
        );
    }

    #[test]
    fn resolves_query_columns_in_mapping_order_without_ambiguity() {
        let job: OdbcJob = serde_json::from_value(definition()).unwrap();
        job.validate().unwrap();
        let specifications = job.specifications().unwrap();
        let spec = &specifications[0];
        let input = |names: &[&str]| {
            names
                .iter()
                .map(|name| SourceColumn {
                    name: (*name).to_owned(),
                    definition: OdbcColumnDefinition::default(),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            spec.source_positions(&input(&["NAME", "Number"])).unwrap(),
            [1, 0]
        );
        assert!(
            spec.source_positions(&input(&["NAME", "Number", "Name"]))
                .is_err()
        );
        assert!(spec.source_positions(&input(&["Number"])).is_err());
    }

    #[test]
    fn checks_definition_without_reading_credentials() {
        let mut config = definition();
        config["source"]["password_env"] = json!("HARBOR_ODBC_FIXTURE_UNSET");
        let valid: OdbcJob = serde_json::from_value(config.clone()).unwrap();
        valid.validate().unwrap();

        config["source"]["connection_env"] = json!("HARBOR_ODBC_FIXTURE_CONNECTION");
        let conflicting: OdbcJob = serde_json::from_value(config.clone()).unwrap();
        assert!(conflicting.validate().is_err());

        config["source"]["password"] = json!("fixture-data");
        assert!(serde_json::from_value::<OdbcJob>(config).is_err());
    }
}
