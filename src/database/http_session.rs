#[cfg(test)]
mod archived_https;
mod restore;
mod transfers;
pub(super) use restore::{RestoreSetup, batch_statements};
pub(super) use transfers::paging_order;
#[cfg(test)]
pub(super) mod workflow_test;

use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::{
    Url,
    blocking::{Client, ClientBuilder, RequestBuilder},
    header::CONTENT_TYPE,
    redirect::Policy,
};

use crate::{http_tunnel, sql_tools::mysql_text_literal};

use super::editable_query::{EditableRead, query_editable_http, try_query_editable_http};
use super::joined_editable::is_joined_query;
use super::joined_editable_http::{query_joined_editable_http, try_query_joined_editable_http};
use super::{
    Cell, ColumnInfo, ColumnOrigin, Command, ConnectionConfig, DatabaseContents,
    DatabaseContentsAction, DbObject, DbResult, DefinitionContext, Failure, MAX_RESULT_ROWS,
    MAX_SNAPSHOT_OBJECTS, ObjectDetail, ObjectKind, QueryReport, Response, ResultSet,
    SCHEMA_DESIGN_COLUMNS, SCHEMA_FOREIGN_KEYS, SNAPSHOT_DEFINITION_LIMIT, SchemaDesign,
    SchemaForeignKey, SchemaSnapshot, ServerInfo, SharedMonitor, SnapshotContext, SnapshotObject,
    build_design_tables, build_schema_foreign_keys, can_continue_after_server_error, f8_plan_joins,
    object_operations, quote_identifier, result_duplicate, transfer,
};

pub(super) const RESPONSE_TOO_LARGE: &str = "HTTP 隧道响应超过大小上限，请缩小查询范围。";

#[derive(Clone)]
pub struct HttpTunnelConfig {
    pub url: String,
    pub content_type: usize,
    pub force_base64: bool,
    pub use_utf8mb4: bool,
    pub web_auth: Option<(String, String)>,
    pub proxy_url: Option<String>,
    pub proxy_auth: Option<(String, String)>,
    pub timeout: u32,
}

impl Default for HttpTunnelConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            content_type: 0,
            force_base64: false,
            use_utf8mb4: false,
            web_auth: None,
            proxy_url: None,
            proxy_auth: None,
            timeout: 120,
        }
    }
}

#[derive(Clone)]
pub(super) struct Session {
    endpoint: Url,
    transport: Client,
    connection: ConnectionConfig,
    config: HttpTunnelConfig,
    case_insensitive_database_names: bool,
    legacy_table_catalog: bool,
    editable_read_monitor: Option<SharedMonitor>,
    pub(super) identity: Arc<()>,
}

impl Session {
    pub(super) fn detached(&self) -> Self {
        Self {
            editable_read_monitor: None,
            identity: Arc::new(()),
            ..self.clone()
        }
    }

    pub(super) fn database_address(&self) -> (&str, u16, &str) {
        (
            &self.connection.host,
            self.connection.port,
            &self.connection.user,
        )
    }

    pub(super) fn database_name(&self) -> Option<&str> {
        self.connection.database.as_deref()
    }

    pub(super) fn clone_settings(&self) -> (ConnectionConfig, HttpTunnelConfig) {
        (self.connection.clone(), self.config.clone())
    }

    pub(super) fn open(
        connection: ConnectionConfig,
        config: HttpTunnelConfig,
    ) -> Result<(Self, ServerInfo), String> {
        Self::open_with_builder(connection, config, Client::builder())
    }

    fn open_with_builder(
        connection: ConnectionConfig,
        config: HttpTunnelConfig,
        builder: ClientBuilder,
    ) -> Result<(Self, ServerInfo), String> {
        if connection.host.trim().is_empty()
            || connection.user.trim().is_empty()
            || connection.port == 0
        {
            return Err("HTTP 隧道需要数据库主机、端口和用户名。".into());
        }
        let endpoint = http_url(&config.url, "HTTP 隧道地址")?;
        if endpoint
            .query_pairs()
            .any(|(key, _)| key == "app" || key == "version")
        {
            return Err("隧道地址中的 app 和 version 参数由 SQLyog 协议保留。".into());
        }
        if config.content_type >= http_tunnel::CONTENT_TYPES.len() {
            return Err("请选择有效的 HTTP 内容类型。".into());
        }
        if config.timeout == 0 || connection.advanced.connect_timeout == 0 {
            return Err("HTTP 请求超时和连接超时必须大于零。".into());
        }
        let mut transport = builder
            .no_proxy()
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(
                connection.advanced.connect_timeout as u64,
            ))
            .timeout(Duration::from_secs(config.timeout as u64));
        if let Some(url) = &config.proxy_url {
            let url = http_url(url, "HTTP 代理地址")?;
            let mut proxy = reqwest::Proxy::all(url).map_err(|_| "无法配置 HTTP 代理。")?;
            if let Some((user, password)) = &config.proxy_auth {
                proxy = proxy.basic_auth(user, password);
            }
            transport = transport.proxy(proxy);
        }
        let transport = transport
            .build()
            .map_err(|error| format!("无法初始化 HTTP 连接：{}", error.without_url()))?;
        let mut active = Self {
            endpoint,
            transport,
            connection,
            config,
            case_insensitive_database_names: false,
            legacy_table_catalog: false,
            editable_read_monitor: None,
            identity: Arc::new(()),
        };
        let mut probe = active.endpoint.clone();
        let parameters: Vec<_> = probe
            .query_pairs()
            .filter(|(key, _)| key != "app" && key != "version")
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        probe
            .query_pairs_mut()
            .clear()
            .extend_pairs(parameters)
            .extend_pairs(
                http_tunnel::VERSION_QUERY
                    .split('&')
                    .filter_map(|pair| pair.split_once('=')),
            );
        let body = active.post(
            active
                .request(probe)
                .header(
                    CONTENT_TYPE,
                    http_tunnel::CONTENT_TYPES[active.config.content_type],
                )
                .body(http_tunnel::VERSION_BODY),
        )?;
        http_tunnel::check_version(&body)?;

        const PROBE: &str =
            "SELECT VERSION(), @@character_set_connection, DATABASE(), @@lower_case_table_names";
        let probe = active.checked_query(PROBE, 1)?;
        let values = probe
            .sets
            .first()
            .and_then(|set| set.rows.first())
            .ok_or("隧道未返回服务器信息。")?;
        let version = text(values.first())
            .ok_or("隧道未返回数据库服务器版本。")?
            .to_owned();
        if active.config.use_utf8mb4 && text(values.get(1)) != Some("utf8mb4") {
            return Err("服务器没有启用 utf8mb4。请关闭该选项或调整服务端字符集。".into());
        }
        let database = text(values.get(2)).map(str::to_owned);
        active.case_insensitive_database_names = matches!(text(values.get(3)), Some("1" | "2"));
        active.legacy_table_catalog = version.starts_with("4.1.");
        if active
            .connection
            .database
            .as_deref()
            .is_some_and(|name| !active.same_database(name, database.as_deref()))
        {
            return Err("隧道未能选择指定的默认数据库。请核对数据库名与账号权限。".into());
        }
        Ok((
            active,
            ServerInfo {
                version,
                connection_id: 0,
                database,
                http_tunnel: true,
            },
        ))
    }

    fn same_database(&self, expected: &str, actual: Option<&str>) -> bool {
        actual.is_some_and(|actual| {
            actual == expected
                || (self.case_insensitive_database_names
                    && actual.to_lowercase() == expected.to_lowercase())
        })
    }

    fn request(&self, url: Url) -> RequestBuilder {
        let mut request = self.transport.post(url);
        if let Some((user, password)) = &self.config.web_auth {
            request = request.basic_auth(user, Some(password));
        }
        request
    }

    fn post(&self, request: RequestBuilder) -> Result<Vec<u8>, String> {
        let response = request
            .send()
            .map_err(|error| format!("HTTP 隧道请求失败：{}", error.without_url()))?;
        let code = response.status();
        if !code.is_success() {
            return Err(format!("HTTP 隧道返回状态 {}。", code.as_u16()));
        }
        let limit = http_tunnel::MAX_RESPONSE_BYTES;
        let mut body = Vec::new();
        response
            .take(limit as u64 + 1)
            .read_to_end(&mut body)
            .map_err(|error| format!("无法读取 HTTP 隧道响应：{error}"))?;
        if body.len() > limit {
            return Err(RESPONSE_TOO_LARGE.into());
        }
        Ok(body)
    }

    fn exchange(&self, sql: &str, batch: bool, limit: usize) -> Result<http_tunnel::Reply, String> {
        self.exchange_in(
            sql,
            batch,
            limit,
            self.connection.database.as_deref().unwrap_or(""),
        )
    }

    fn exchange_in(
        &self,
        sql: &str,
        batch: bool,
        limit: usize,
        database: &str,
    ) -> Result<http_tunnel::Reply, String> {
        self.execute_payload(self.prepare_payload(sql, batch, database)?, limit)
    }

    pub(super) fn prepare_payload(
        &self,
        sql: &str,
        batch: bool,
        database: &str,
    ) -> Result<http_tunnel::RequestBody, String> {
        let info = http_tunnel::ConnectInfo {
            host: &self.connection.host,
            port: self.connection.port,
            user: &self.connection.user,
            password: &self.connection.password,
            database,
            charset: if self.config.use_utf8mb4 {
                "utf8mb4"
            } else {
                "utf8"
            },
        };
        http_tunnel::encode_query(&info, sql, batch, self.config.force_base64)
    }

    pub(super) fn execute_payload(
        &self,
        payload: http_tunnel::RequestBody,
        limit: usize,
    ) -> Result<http_tunnel::Reply, String> {
        let mut request = self
            .request(self.endpoint.clone())
            .header(
                CONTENT_TYPE,
                http_tunnel::CONTENT_TYPES[self.config.content_type],
            )
            .body(payload.bytes);
        if payload.base64_header {
            request = request.header("Base64", "yes");
        }
        self.check_editable_read()?;
        let response = self.post(request);
        self.check_editable_read()?;
        http_tunnel::parse_response(&response?, limit)
    }

    fn check_editable_read(&self) -> Result<(), String> {
        if let Some(monitor) = &self.editable_read_monitor {
            monitor.lock().unwrap().check_cancelled()?;
        }
        Ok(())
    }

    fn query(&self, sql: &str, limit: usize, attach_source: bool) -> Result<QueryReport, String> {
        let started = Instant::now();
        let reply = self.exchange(sql, false, limit.min(MAX_RESULT_ROWS))?;
        let source_sql = attach_source.then(|| Arc::from(sql));
        let single_result =
            reply.error.is_none() && reply.omitted_sets == 0 && reply.sets.len() == 1;
        let sets = reply
            .sets
            .into_iter()
            .map(|set| {
                // The legacy packet only supplies display names and table hints. Require
                // a single verified projection before using it to remember widths.
                let source = single_result
                    .then(|| {
                        result_duplicate::http_source_projection(
                            sql,
                            self.database_name(),
                            &set.columns,
                        )
                    })
                    .flatten();
                let column_origins = source.as_ref().map_or_else(Vec::new, |(object, names)| {
                    names
                        .iter()
                        .map(|name| Some(ColumnOrigin::new(&object.schema, &object.name, name)))
                        .collect()
                });
                let duplicate_source =
                    source
                        .filter(|_| attach_source)
                        .and_then(|(object, names)| {
                            result_duplicate::bind_http(
                                sql,
                                object,
                                names,
                                &set.columns,
                                &self.identity,
                            )
                        });
                let excel_types = set
                    .columns
                    .iter()
                    .map(|field| transfer::ExcelColumnType::from_type_name(&field.data_type))
                    .collect();
                let export_columns = set
                    .columns
                    .iter()
                    .map(transfer::ResultColumn::from_http)
                    .collect();
                let (columns, column_hints) = set
                    .columns
                    .into_iter()
                    .map(|field| {
                        let mut info = Vec::new();
                        if !field.table.is_empty() {
                            info.push(format!("源表：{}", field.table));
                        }
                        if !field.data_type.is_empty() {
                            info.push(format!("数据类型：{}", field.data_type));
                        }
                        if let Some(length) = field.max_length {
                            info.push(format!("最大长度：{length}"));
                        }
                        if let Some(value) = field.default_text {
                            info.push(format!("默认值：{value}"));
                        }
                        (field.name, info.join("\n"))
                    })
                    .unzip();
                let notice = set.info.map(|info| {
                    if set.server_version.is_empty() {
                        info
                    } else {
                        format!("{}：{info}", set.server_version)
                    }
                });
                ResultSet {
                    source_sql: source_sql.clone(),
                    columns,
                    column_hints,
                    column_origins,
                    excel_types,
                    export_columns,
                    duplicate_source,
                    notice,
                    rows: set
                        .rows
                        .into_iter()
                        .map(|row| row.into_iter().map(http_cell).collect())
                        .collect(),
                    affected_rows: set.affected_rows,
                    last_insert_id: set.last_insert_id,
                    truncated: set.truncated,
                }
            })
            .collect();
        Ok(QueryReport {
            sets,
            elapsed: started.elapsed(),
            // The legacy XML carries no execution timing or warning counts.
            diagnostics: Default::default(),
            continue_after_error: reply
                .error
                .as_ref()
                .is_some_and(|error| can_continue_after_server_error(error.code, None, false)),
            error: reply
                .error
                .map(|error| format!("ERROR {}: {}", error.code, error.message)),
            omitted_sets: reply.omitted_sets,
            active_database: None,
        })
    }

    pub(super) fn checked_query(&self, sql: &str, limit: usize) -> Result<QueryReport, String> {
        let report = self.query(sql, limit, false)?;
        if let Some(error) = report.error {
            Err(error)
        } else {
            Ok(report)
        }
    }

    fn editor_query(&mut self, sql: &str, limit: usize) -> Result<Response, String> {
        if let Some(schema) = select_database_command(sql)? {
            self.switch_schema(&schema).map(Response::Queried)
        } else {
            self.query(sql, limit, true).map(Response::Queried)
        }
    }

    fn editable_read_session(&self, monitor: Option<&SharedMonitor>) -> Self {
        Self {
            editable_read_monitor: monitor.cloned(),
            ..self.clone()
        }
    }

    fn query_f8(
        &mut self,
        sql: &str,
        limit: usize,
        monitor: Option<&SharedMonitor>,
    ) -> Result<Response, String> {
        if let Some(joined) = f8_plan_joins(sql) {
            let session = self.editable_read_session(monitor);
            session.check_editable_read()?;
            let response = if joined {
                match try_query_joined_editable_http(&session, sql, limit)? {
                    EditableRead::Editable(page) if page.targets.is_empty() => {
                        Some(Response::Queried(QueryReport::from_read_only_result(
                            page.result,
                            page.query_timing,
                        )))
                    }
                    EditableRead::Editable(page) => Some(Response::JoinedEditablePage(page)),
                    EditableRead::ReadOnly(_) => None,
                }
            } else {
                match try_query_editable_http(&session, sql, limit)? {
                    EditableRead::Editable(page) => Some(Response::EditablePage(page)),
                    EditableRead::ReadOnly(_) => None,
                }
            };
            if let Some(response) = response {
                session.check_editable_read()?;
                return Ok(response);
            }
        }
        if let Some(monitor) = monitor {
            monitor.lock().unwrap().begin_editor_query()?;
        }
        self.editor_query(sql, limit)
    }

    fn flush_resources(&self, plan: crate::maintenance::FlushPlan) -> Result<Response, String> {
        const MARKER: &str = "harbor_flush_completed";
        let completed = format!("SELECT 1 AS {MARKER}");
        let mut statements: Vec<_> = plan.statements().iter().map(String::as_str).collect();
        statements.push(&completed);
        // The gateway only returns the final result from a batch. Keep the lock and unlock here.
        let batch = batch_statements(&statements)?;
        let reply = self
            .exchange(&batch, true, 1)
            .map_err(|error| format!("无法确认服务器刷新是否完成。{error}"))?;
        if let Some(error) = reply.error {
            return Err(format!(
                "服务器刷新中止。ERROR {}: {}",
                error.code, error.message
            ));
        }
        let confirmed = reply.sets.last().is_some_and(|set| {
            set.columns.len() == 1
                && set.columns[0].name == MARKER
                && set
                    .rows
                    .first()
                    .and_then(|row| row.first())
                    .and_then(Option::as_deref)
                    == Some(b"1".as_slice())
        });
        if !confirmed {
            return Err("无法确认服务器刷新是否完成，隧道未返回完成标记。".into());
        }
        Ok(Response::ServerResourcesFlushed(plan.targets()))
    }

    pub(super) fn handle(&mut self, command: Command) -> Result<Response, String> {
        self.handle_with_monitor(command, None)
    }

    pub(super) fn handle_with_monitor(
        &mut self,
        command: Command,
        monitor: Option<&SharedMonitor>,
    ) -> Result<Response, String> {
        match command {
            Command::QueryInSeparateSession { sql, limit } => {
                self.query(&sql, limit, false).map(Response::Queried)
            }
            Command::FlushResources(plan) => self.flush_resources(plan),
            Command::Query { sql, limit } => self.editor_query(&sql, limit),
            Command::QueryF8 { sql, limit } => self.query_f8(&sql, limit, monitor),
            Command::ListSchemas => self.read_set("SHOW DATABASES").map(|set| {
                Response::Schemas(
                    set.rows
                        .into_iter()
                        .filter_map(|row| text(row.first()).map(str::to_owned))
                        .collect(),
                )
            }),
            Command::ListObjects { schema } => {
                let objects = self.objects(&schema)?;
                Ok(Response::Objects { schema, objects })
            }
            Command::ReadDatabaseDefaults { schema } => {
                object_operations::read_defaults(schema.as_deref(), |sql| self.metadata_rows(sql))
                    .map(Response::DatabaseDefaults)
                    .map_err(Failure::message)
            }
            Command::ReadDatabaseContents { schema, action } => self
                .database_contents(schema, action)
                .map(Response::DatabaseContents),
            Command::ApplyDatabaseContents { plan } => {
                if plan.suspend_foreign_key_checks {
                    return Err("所选表互相引用。请使用直连或 SSH 隧道截断。".into());
                }
                let schema = plan.schema().to_owned();
                let current = self.database_contents(schema.clone(), plan.action())?;
                plan.verify(&current)?;
                let mut result = object_operations::run_contents(plan, |sql| {
                    self.checked_query(sql, 1).map(|_| ())
                });
                result.objects = self.objects(&schema).ok();
                Ok(Response::DatabaseContentsApplied(result))
            }
            Command::UseSchema { schema } => {
                if let Some(error) = self.switch_schema(&schema)?.error {
                    return Err(error);
                }
                Ok(Response::SchemaChanged(schema))
            }
            Command::Inspect { object } => self.inspect(object).map(Response::Inspected),
            Command::ExportColumns { object } => self.inspect(object).map(Response::ExportColumns),
            Command::Preview { object, limit } if object.kind.is_tabular() => self
                .query(
                    &format!(
                        "SELECT * FROM {} LIMIT {}",
                        object.qualified_name(),
                        limit.min(MAX_RESULT_ROWS) + 1
                    ),
                    limit,
                    false,
                )
                .map(Response::Previewed),
            Command::SnapshotSchema { schema } => {
                let objects = self.objects(&schema)?;
                let mut snapshot = self.snapshot(schema, objects)?;
                snapshot.defaults =
                    object_operations::read_current_defaults(Some(&snapshot.schema), |sql| {
                        self.metadata_rows(sql)
                    })
                    .ok();
                snapshot.context = self.snapshot_context().ok();
                snapshot.foreign_keys = self.foreign_keys(&snapshot.schema, true).ok();
                snapshot.origin = super::SchemaOrigin::http(self).ok();
                Ok(Response::Snapshot(snapshot))
            }
            Command::DescribeSchemaDesign { schema } => self
                .schema_design(schema)
                .map(Response::SchemaDesign)
                .map_err(Failure::message),
            Command::DescribeTableKeys { object } => self
                .table_keys(object)
                .map(Response::TableKeys)
                .map_err(Failure::message),
            Command::SnapshotDesignTables { schema, names } => {
                let objects = names
                    .into_iter()
                    .map(|name| DbObject {
                        schema: schema.clone(),
                        name,
                        kind: ObjectKind::Table,
                    })
                    .collect();
                self.snapshot(schema, objects).map(Response::Snapshot)
            }
            Command::ListEngines => self.engines(),
            Command::Export {
                object,
                path,
                settings,
            } => self
                .export(&object, &path, &settings)
                .map(Response::Transferred),
            Command::Dump {
                schema,
                object,
                path,
                options,
                chunk_rows,
                bulk_insert_limit_kib,
            } => {
                let settings = super::transfer::DumpSettings {
                    include_use: object.is_none(),
                    include_create_database: object.is_none() && options.include_structure,
                    options,
                    objects: object.map(|object| vec![object]),
                    bulk_insert: true,
                    chunk_rows,
                    bulk_insert_limit_kib,
                    overwrite: true,
                    ..Default::default()
                };
                super::transfer::dump_over_http(self, &schema, &path, settings, |_| Ok(()))
                    .map(Response::Transferred)
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
                let check_cancelled =
                    || monitor.map_or(Ok(()), |monitor| monitor.lock().unwrap().check_cancelled());
                check_cancelled()?;
                let settings = super::transfer::copy_dump_settings(
                    &schema,
                    &destination_schema,
                    object,
                    objects,
                    options,
                    bulk_insert,
                )?;
                let origin = super::SchemaOrigin::http(self)?;
                check_cancelled()?;
                let mut summary = super::transfer::dump_for_copy_over_http(
                    self,
                    &schema,
                    &destination_schema,
                    &path,
                    settings,
                    |progress| {
                        monitor.map_or(Ok(()), |monitor| {
                            monitor.lock().unwrap().update_dump(progress)
                        })
                    },
                )?;
                summary.copy_origin = Some(origin);
                Ok(Response::Transferred(summary))
            }
            Command::CopyTableAs { object, settings } => {
                super::transfer::copy_table_as_over_http(self, &object, &settings)
                    .map(Response::Transferred)
            }
            Command::TablePage {
                object,
                limit,
                offset,
            } => {
                super::row_editor::http_table_page(self, object, limit, offset, &Default::default())
                    .map(Response::TablePage)
            }
            Command::TablePageSelected {
                object,
                limit,
                offset,
                selection,
            } => super::row_editor::http_table_page(self, object, limit, offset, &selection)
                .map(Response::TablePage),
            Command::ReadTableData {
                object,
                selection,
                range,
            } => {
                let monitor = monitor.ok_or("读取表数据任务未初始化。")?;
                super::row_editor::http_table_data(self, object, &selection, range, || {
                    monitor.lock().unwrap().check_cancelled()
                })
                .map(Response::TablePage)
            }
            Command::EditTable { object, edit } => {
                super::row_editor::http_edit_table_with_confirmation(self, object, edit, None)
                    .map(super::table_edit_response)
            }
            Command::ConfirmTableEdit {
                object,
                edit,
                choice,
            } => super::row_editor::http_edit_table_with_confirmation(
                self,
                object,
                edit,
                Some(choice),
            )
            .map(super::table_edit_response),
            Command::DeleteTableRows { object, originals } => {
                super::row_editor::http_delete_table_rows(self, object, originals)
                    .map(|affected_rows| Response::TableEdited { affected_rows })
            }
            Command::QueryEditable { sql, limit } => {
                // Keep the operation monitor on this temporary view of the tunnel only.
                let session = self.editable_read_session(monitor);
                session.check_editable_read()?;
                let result = if is_joined_query(&sql) {
                    query_joined_editable_http(&session, &sql, limit)
                        .map(Response::JoinedEditablePage)
                } else {
                    query_editable_http(&session, &sql, limit).map(Response::EditablePage)
                };
                session.check_editable_read()?;
                result
            }
            Command::DuplicateResultRow { source, cells } => {
                super::result_duplicate::load_http(self, source, cells)
                    .map(Response::ResultRowDuplicate)
            }
            Command::SchemaHtml { schema, path } => super::transfer::schema_html_for(
                &schema,
                &path,
                self.objects(&schema)?,
                None,
                |object| self.inspect(object),
            )
            .map(Response::Transferred),
            Command::SchemaHtmlTables {
                schema,
                tables,
                path,
            } => super::transfer::schema_html_for(
                &schema,
                &path,
                self.objects(&schema)?,
                Some(&tables),
                |object| self.inspect(object),
            )
            .map(Response::Transferred),
            _ => Err("这个操作目前需要 MySQL 直连或 SSH 隧道连接。".into()),
        }
    }

    fn switch_schema(&mut self, schema: &str) -> Result<QueryReport, String> {
        if schema.contains('\x60') {
            return Err("SQLyog 8.05 隧道无法直接切换到名称含反引号的数据库。".into());
        }
        let started = Instant::now();
        let original = self.connection.database.replace(schema.to_owned());
        let check = self.query("SELECT DATABASE()", 1, false);
        let actual = check
            .as_ref()
            .ok()
            .filter(|report| report.error.is_none())
            .and_then(|report| report.sets.first())
            .and_then(|set| set.rows.first())
            .and_then(|row| text(row.first()));
        if self.same_database(schema, actual) {
            return Ok(QueryReport {
                elapsed: started.elapsed(),
                active_database: Some(schema.to_owned()),
                ..QueryReport::default()
            });
        }

        self.connection.database = original;
        let mut report = check?;
        if report.error.is_none() {
            // SQLyog 8.05 ignores a failed schema selection in the request header.
            // An explicit USE exposes the server error on this temporary connection.
            report = self.query(&format!("USE {}", quote_identifier(schema)), 1, false)?;
        }
        let Some(error) = report.error else {
            return Err("隧道未能切换到指定数据库。请核对数据库名与账号权限。".into());
        };
        Ok(QueryReport {
            elapsed: started.elapsed(),
            error: Some(error),
            continue_after_error: report.continue_after_error,
            ..QueryReport::default()
        })
    }

    pub(super) fn read_set(&self, sql: &str) -> Result<ResultSet, String> {
        let set = self
            .checked_query(sql, MAX_RESULT_ROWS)?
            .sets
            .into_iter()
            .next()
            .ok_or("隧道没有返回结果集。")?;
        if set.truncated {
            return Err("隧道返回的元数据超过限制，无法完整读取。".into());
        }
        Ok(set)
    }

    fn metadata_rows(&self, sql: &str) -> DbResult<object_operations::MetadataRows> {
        let set = self.read_set(sql).map_err(Failure::Message)?;
        object_operations::http_rows(set.rows)
    }

    fn database_contents(
        &self,
        schema: String,
        action: DatabaseContentsAction,
    ) -> Result<DatabaseContents, String> {
        let objects = self.objects(&schema)?;
        object_operations::read_contents(schema, objects, action, |sql| self.metadata_rows(sql))
            .map_err(Failure::message)
    }

    pub(super) fn objects(&self, schema: &str) -> Result<Vec<DbObject>, String> {
        let mut objects = Vec::new();
        let show_tables = if self.legacy_table_catalog {
            "SHOW TABLES"
        } else {
            "SHOW FULL TABLES"
        };
        let set = self.read_set(&format!("{show_tables} FROM {}", quote_identifier(schema)))?;
        for row in set.rows {
            let Some(name) = text(row.first()) else {
                continue;
            };
            let kind = match text(row.get(1)) {
                Some("VIEW" | "SYSTEM VIEW") => ObjectKind::View,
                Some(_) => ObjectKind::Table,
                None if self.legacy_table_catalog => ObjectKind::Table,
                None => continue,
            };
            objects.push(DbObject {
                schema: schema.into(),
                name: name.into(),
                kind,
            });
        }
        if !self.legacy_table_catalog {
            let encoded = mysql_text_literal(schema);
            let sql = super::SCHEMA_OBJECTS.replace(":schema", &encoded);
            for row in self.read_set(&sql)?.rows {
                let Some(name) = text(row.first()) else {
                    continue;
                };
                let kind = match text(row.get(1)) {
                    Some("PROCEDURE") => ObjectKind::Procedure,
                    Some("FUNCTION") => ObjectKind::Function,
                    Some("TRIGGER") => ObjectKind::Trigger,
                    Some("EVENT") => ObjectKind::Event,
                    _ => continue,
                };
                objects.push(DbObject {
                    schema: schema.into(),
                    name: name.into(),
                    kind,
                });
            }
        }
        objects.sort_by(|left, right| {
            left.kind
                .cmp(&right.kind)
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(objects)
    }

    pub(super) fn inspect(&self, object: DbObject) -> Result<ObjectDetail, String> {
        let mut columns = Vec::new();
        if object.kind.is_tabular() {
            let set = self.read_set(&format!(
                "SHOW FULL COLUMNS FROM {}",
                object.qualified_name()
            ))?;
            let indexes =
                ["Field", "Type", "Null", "Key", "Default", "Extra"].map(|name| column(&set, name));
            for row in &set.rows {
                let get = |index: usize| indexes[index].and_then(|index| text(row.get(index)));
                let (Some(name), Some(data_type)) = (get(0), get(1)) else {
                    continue;
                };
                columns.push(ColumnInfo {
                    name: name.into(),
                    data_type: data_type.into(),
                    nullable: get(2) == Some("YES"),
                    key: get(3).unwrap_or("").into(),
                    default_value: get(4).map(str::to_owned),
                    extra: get(5).unwrap_or("").into(),
                });
            }
        }
        let (ddl, ddl_error) = match self.definition(&object) {
            Ok(ddl) => (ddl, None),
            Err(error) => (String::new(), Some(error)),
        };
        Ok(ObjectDetail {
            object,
            columns,
            ddl,
            ddl_error,
        })
    }

    fn definition(&self, object: &DbObject) -> Result<String, String> {
        self.definition_with_context(object)
            .map(|(definition, _)| definition)
    }

    pub(super) fn definition_with_context(
        &self,
        object: &DbObject,
    ) -> Result<(String, DefinitionContext), String> {
        let (keyword, field) = object.kind.show_create();
        let set = self.read_set(&format!(
            "SHOW CREATE {keyword} {}",
            object.qualified_name()
        ))?;
        let index = column(&set, field).ok_or("服务器未返回预期的对象定义字段。")?;
        let row = set.rows.first().ok_or("服务器未返回对象定义。")?;
        let ddl = text(row.get(index)).ok_or("服务器未返回对象定义。")?;
        let field = |name| {
            column(&set, name)
                .and_then(|index| text(row.get(index)))
                .map(str::to_owned)
        };
        let trigger_action_order = (object.kind == ObjectKind::Trigger).then(|| {
            self.read_set(&format!(
                "SELECT ACTION_ORDER FROM information_schema.TRIGGERS WHERE BINARY TRIGGER_SCHEMA = BINARY {} AND BINARY TRIGGER_NAME = BINARY {}",
                mysql_text_literal(&object.schema), mysql_text_literal(&object.name),
            )).ok().and_then(|set| text(set.rows.first()?.first())?.parse::<u64>().ok())
        }).flatten();
        Ok((
            ddl.to_owned(),
            DefinitionContext {
                sql_mode: field("sql_mode"),
                character_set_client: field("character_set_client"),
                collation_connection: field("collation_connection"),
                time_zone: field("time_zone"),
                trigger_action_order,
            },
        ))
    }

    fn snapshot(&self, schema: String, objects: Vec<DbObject>) -> Result<SchemaSnapshot, String> {
        let omitted_objects = objects.len().saturating_sub(MAX_SNAPSHOT_OBJECTS);
        let mut remaining = SNAPSHOT_DEFINITION_LIMIT;
        let objects = objects
            .into_iter()
            .take(MAX_SNAPSHOT_OBJECTS)
            .map(|object| {
                let mut context = None;
                let definition = if remaining == 0 {
                    Err("结构快照已达到容量上限。".into())
                } else {
                    self.definition_with_context(&object)
                        .and_then(|(definition, metadata)| {
                            if definition.len() > remaining {
                                return Err("此对象的定义超出了结构快照的剩余容量。".into());
                            }
                            remaining -= definition.len();
                            context = Some(metadata);
                            Ok(definition)
                        })
                };
                SnapshotObject {
                    object,
                    definition,
                    context,
                }
            })
            .collect();
        Ok(SchemaSnapshot {
            schema,
            defaults: None,
            objects,
            omitted_objects,
            context: None,
            foreign_keys: None,
            origin: None,
        })
    }

    pub(super) fn snapshot_context(&self) -> Result<SnapshotContext, String> {
        let sql = "SELECT @@lower_case_table_names, @@SESSION.sql_mode, @@SESSION.character_set_client, @@SESSION.collation_connection";
        let mut rows = self
            .metadata_columns::<4>(sql, 1)
            .map_err(Failure::message)?;
        let [
            Some(mode),
            Some(sql_mode),
            Some(character_set_client),
            Some(collation_connection),
        ] = rows.pop().ok_or("服务器未返回隧道会话属性。")?
        else {
            return Err("服务器返回的隧道会话属性不完整。".into());
        };
        let lower_case_table_names = mode
            .parse()
            .map_err(|_| "服务器返回的表名大小写模式无效。")?;
        Ok(SnapshotContext {
            lower_case_table_names,
            sql_mode,
            character_set_client,
            collation_connection,
        })
    }

    fn schema_design(&self, schema: String) -> DbResult<SchemaDesign> {
        let sql = SCHEMA_DESIGN_COLUMNS.replace(":schema", &mysql_text_literal(&schema));
        let columns = self.metadata_columns::<7>(&sql, 16_385)?;
        let mut tables = build_design_tables(
            &schema,
            columns.into_iter().map(|row| match row {
                [
                    Some(table),
                    Some(name),
                    Some(data_type),
                    Some(nullable),
                    Some(key),
                    default_value,
                    Some(extra),
                ] => Ok((table, name, data_type, nullable, key, default_value, extra)),
                _ => Err(Failure::Message("隧道返回的表字段元数据不完整。".into())),
            }),
        )?;
        super::design_indexes::read_http(self, &schema, &mut tables);
        let foreign_keys = self.foreign_keys(&schema, false)?;
        Ok(SchemaDesign {
            schema,
            tables,
            foreign_keys,
        })
    }

    pub(super) fn foreign_keys(
        &self,
        schema: &str,
        include_inbound: bool,
    ) -> DbResult<Vec<SchemaForeignKey>> {
        let sql = SCHEMA_FOREIGN_KEYS
            .replace(":schema", &mysql_text_literal(schema))
            .replace(":include_inbound", if include_inbound { "1" } else { "0" });
        let keys = self.metadata_columns::<9>(&sql, 8_193)?;
        build_schema_foreign_keys(keys.into_iter().map(|row| match row {
            [
                Some(schema),
                Some(table),
                Some(name),
                Some(referenced_schema),
                Some(referenced_table),
                Some(column),
                Some(referenced_column),
                on_delete,
                on_update,
            ] => Ok((
                schema,
                table,
                name,
                referenced_schema,
                referenced_table,
                column,
                referenced_column,
                on_delete,
                on_update,
            )),
            _ => Err(Failure::Message("隧道返回的外键元数据不完整。".into())),
        }))
    }

    fn table_keys(&self, object: DbObject) -> DbResult<super::TableKeys> {
        use super::IndexVisibility;
        if object.kind != ObjectKind::Table {
            return Err(Failure::Message("只有表支持管理索引和外键。".into()));
        }
        let detail = self.inspect(object.clone()).map_err(Failure::Message)?;
        let set = self
            .read_set(&format!("SHOW INDEX FROM {}", object.qualified_name()))
            .map_err(Failure::Message)?;
        let indexes = super::group_table_indexes(&detail, &set.rows, |row, name| {
            column(&set, name)
                .and_then(|index| text(row.get(index)))
                .map(str::to_owned)
        })?;
        let meta = self.metadata_columns::<2>("SELECT VERSION(), @@SESSION.sql_mode", 1)?;
        let [Some(version), Some(mode)] = meta
            .first()
            .ok_or_else(|| Failure::Message("服务器未返回版本和 SQL 模式。".into()))?
        else {
            return Err(Failure::Message("服务器返回的索引设置不完整。".into()));
        };
        let maria = version.to_ascii_lowercase().contains("mariadb");
        let version = if maria {
            version.strip_prefix("5.5.5-").unwrap_or(version)
        } else {
            version
        };
        let mut parts = version
            .split(|ch: char| !ch.is_ascii_digit())
            .take(2)
            .map(|part| part.parse::<u32>().unwrap_or(0));
        let pair = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
        let visibility = if maria && pair >= (10, 6) {
            Some(IndexVisibility::MariaDb)
        } else if !maria && pair >= (8, 0) {
            Some(IndexVisibility::MySql)
        } else {
            None
        };
        let foreign_keys = self
            .foreign_keys(&object.schema, false)?
            .into_iter()
            .filter(|key| {
                key.table.name == object.name
                    || self.case_insensitive_database_names
                        && key.table.name.to_lowercase() == object.name.to_lowercase()
            })
            .collect();
        let no_backslash_escapes = mode
            .split(',')
            .any(|value| value.eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES"));
        Ok(super::TableKeys {
            object,
            columns: detail.columns,
            indexes,
            foreign_keys,
            visibility,
            no_backslash_escapes,
        })
    }

    pub(super) fn metadata_columns<const N: usize>(
        &self,
        sql: &str,
        limit: usize,
    ) -> DbResult<Vec<[Option<String>; N]>> {
        let set = self.read_raw_set(sql, limit).map_err(Failure::Message)?;
        if set.columns.len() != N {
            return Err(Failure::Message("隧道返回的元数据列数不符。".into()));
        }
        set.rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|value| value.map(String::from_utf8).transpose())
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| Failure::Message("隧道返回了无法解码的元数据。".into()))?
                    .try_into()
                    .map_err(|_| Failure::Message("隧道返回的元数据列数不符。".into()))
            })
            .collect()
    }

    fn engines(&self) -> Result<Response, String> {
        let set = self.read_set("SHOW ENGINES")?;
        let engine = column(&set, "Engine").ok_or("服务器未返回存储引擎名称。")?;
        let support = column(&set, "Support").ok_or("服务器未返回存储引擎的支持情况。")?;
        let names: Vec<String> = set
            .rows
            .iter()
            .filter_map(|row| {
                matches!(text(row.get(support)), Some("YES" | "DEFAULT"))
                    .then(|| text(row.get(engine)).map(str::to_owned))
                    .flatten()
            })
            .collect();
        let mode = self.read_set("SELECT @@SESSION.sql_mode")?;
        let no_backslash_escapes = mode
            .rows
            .first()
            .and_then(|row| text(row.first()))
            .is_some_and(|mode| {
                mode.split(',')
                    .any(|value| value.eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES"))
            });
        let raid_default_engine = if self.legacy_table_catalog
            && names.iter().any(|name| name.eq_ignore_ascii_case("MyISAM"))
            && self
                .read_set(super::LEGACY_RAID_VARIABLE)?
                .rows
                .iter()
                .any(|row| {
                    text(row.first()).is_some_and(|name| name.eq_ignore_ascii_case("have_raid"))
                        && text(row.get(1)).is_some_and(|value| value.eq_ignore_ascii_case("YES"))
                }) {
            Some(
                set.rows
                    .iter()
                    .find(|row| text(row.get(support)) == Some("DEFAULT"))
                    .and_then(|row| text(row.get(engine)))
                    .unwrap_or_default()
                    .to_owned(),
            )
        } else {
            None
        };
        Ok(Response::Engines {
            names,
            no_backslash_escapes,
            raid_default_engine,
        })
    }
}

fn http_url(input: &str, label: &str) -> Result<Url, String> {
    let url = Url::parse(input.trim()).map_err(|_| format!("请输入有效的{label}。"))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(format!(
            "{label}须为 HTTP 或 HTTPS，且不能含嵌入的用户名、密码或片段。"
        ));
    }
    Ok(url)
}

fn select_database_command(sql: &str) -> Result<Option<String>, String> {
    let sql = without_comments(sql);
    let Some(first) = sql.get(..3) else {
        return Ok(None);
    };
    if !first.eq_ignore_ascii_case("USE")
        || !sql
            .as_bytes()
            .get(3)
            .is_none_or(|byte| byte.is_ascii_whitespace() || *byte == b'\x60')
    {
        return Ok(None);
    }
    let invalid = "无法识别 USE 中的数据库名称。可从对象树切换默认数据库。";
    let tail = sql[3..].trim_ascii_start();
    let (name, rest) = if let Some(tail) = tail.strip_prefix('\x60') {
        let end = tail.find('\x60').ok_or(invalid)?;
        (&tail[..end], &tail[end + 1..])
    } else {
        let end = tail
            .find(|ch: char| !(ch.is_alphanumeric() || matches!(ch, '_' | '$')))
            .unwrap_or(tail.len());
        (&tail[..end], &tail[end..])
    };
    let rest = without_comments(rest);
    let rest = rest.strip_prefix(';').map_or(rest, without_comments);
    if name.is_empty() || !rest.is_empty() {
        return Err(invalid.into());
    }
    Ok(Some(name.to_owned()))
}

fn without_comments(mut sql: &str) -> &str {
    loop {
        sql = sql.trim_ascii_start();
        if sql.starts_with('#')
            || (sql.starts_with("--") && sql.as_bytes().get(2).is_none_or(u8::is_ascii_whitespace))
        {
            sql = sql.find(['\r', '\n']).map_or("", |end| &sql[end..]);
        } else if sql.starts_with("/*") && !sql.starts_with("/*!") && !sql.starts_with("/*M!") {
            let Some(end) = sql[2..].find("*/") else {
                return sql;
            };
            sql = &sql[end + 4..];
        } else {
            return sql;
        }
    }
}

pub(super) fn http_cell(raw: Option<Vec<u8>>) -> Cell {
    match raw {
        None => Cell::Null,
        Some(raw) if raw.contains(&0) => Cell::Binary(raw),
        Some(raw) => match String::from_utf8(raw) {
            Ok(value) => Cell::Text(value),
            Err(error) => Cell::Binary(error.into_bytes()),
        },
    }
}

pub(super) fn text(value: Option<&Cell>) -> Option<&str> {
    match value {
        Some(Cell::Text(value)) => Some(value),
        _ => None,
    }
}

pub(super) fn column(set: &ResultSet, name: &str) -> Option<usize> {
    set.columns
        .iter()
        .position(|field| field.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};

    use base64::Engine as _;

    use super::*;

    #[test]
    fn uses_legacy_handshake_and_separate_requests_for_metadata() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let mock = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let (head, body) = read_request(&mut stream);
            assert!(head.starts_with("POST /tunnel.php?app=sqlyog&version=8.05 "));
            assert!(
                head.to_ascii_lowercase()
                    .contains("content-type: text/plain\r\n")
            );
            assert_eq!(body, b"garbagedata");
            send_reply(&mut stream, b"TunnelVersion:8.05");

            let (mut stream, _) = listener.accept().unwrap();
            let (head, body) = read_request(&mut stream);
            assert!(head.starts_with("POST /tunnel.php "));
            assert!(head.to_ascii_lowercase().contains("base64: yes\r\n"));
            assert!(head.to_ascii_lowercase().contains("authorization: basic "));
            assert!(String::from_utf8(body).unwrap().contains(
                &base64::engine::general_purpose::STANDARD.encode("SELECT VERSION(), @@character_set_connection, DATABASE(), @@lower_case_table_names"),
            ));
            let response = xml_row(
                &["version", "charset", "database", "case_mode"],
                &["5.1.40", "utf8", "demo", "0"],
            );
            send_reply(&mut stream, response.as_bytes());

            let (mut stream, _) = listener.accept().unwrap();
            let (_, body) = read_request(&mut stream);
            assert!(
                String::from_utf8(body)
                    .unwrap()
                    .contains(&base64::engine::general_purpose::STANDARD.encode("SHOW DATABASES"),)
            );
            let response = xml_row(&["Database"], &["演示库"]);
            send_reply(&mut stream, response.as_bytes());

            let (mut stream, _) = listener.accept().unwrap();
            let (_, body) = read_request(&mut stream);
            let request = String::from_utf8(body).unwrap();
            assert!(
                request.contains(
                    &base64::engine::general_purpose::STANDARD.encode("SELECT DATABASE()")
                )
            );
            assert!(request.contains(&base64::engine::general_purpose::STANDARD.encode("演示库")));
            let response = xml_row(&["DATABASE()"], &["演示库"]);
            send_reply(&mut stream, response.as_bytes());
        });
        let (mut session, info) = Session::open(
            ConnectionConfig {
                host: "127.0.0.1".into(),
                user: "test_account".into(),
                database: Some("demo".into()),
                ..Default::default()
            },
            HttpTunnelConfig {
                url: format!("http://{address}/tunnel.php"),
                content_type: 2,
                force_base64: true,
                web_auth: Some(("local-test".into(), "synthetic".into())),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(info.http_tunnel);
        assert_eq!(info.version, "5.1.40");
        assert_eq!(info.database.as_deref(), Some("demo"));
        assert!(
            matches!(session.handle(Command::ListSchemas).unwrap(), Response::Schemas(schemas) if schemas == ["演示库"])
        );
        let changed = session
            .handle(Command::Query {
                sql: "-- 注释\nUSE \x60演示库\x60".into(),
                limit: 1,
            })
            .unwrap();
        assert!(
            matches!(changed, Response::Queried(report) if report.active_database.as_deref() == Some("演示库"))
        );
        mock.join().unwrap();
    }

    pub(super) fn read_request(stream: &mut TcpStream) -> (String, Vec<u8>) {
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        read_http_request(stream)
    }

    pub(super) fn read_http_request(stream: &mut impl Read) -> (String, Vec<u8>) {
        let mut buffer = BufReader::new(stream);
        let mut header = String::new();
        loop {
            let mut line = String::new();
            assert_ne!(buffer.read_line(&mut line).unwrap(), 0);
            header.push_str(&line);
            if line == "\r\n" {
                break;
            }
        }
        let length: usize = header
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
            .unwrap()
            .1
            .trim()
            .parse()
            .unwrap();
        let mut body = vec![0; length];
        buffer.read_exact(&mut body).unwrap();
        (header, body)
    }

    pub(super) fn send_reply(stream: &mut TcpStream, body: &[u8]) {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
    }

    fn xml_row(columns: &[&str], values: &[&str]) -> String {
        let fields = columns
            .iter()
            .map(|name| format!("<f><n>{name}</n><t/><m>0</m><d/><ty>varchar</ty></f>"))
            .collect::<String>();
        let cells = values
            .iter()
            .map(|value| {
                format!(
                    "<c l=\"{}\">{}</c>",
                    value.len(),
                    base64::engine::general_purpose::STANDARD.encode(value)
                )
            })
            .collect::<String>();
        format!(
            "<xml><result v=\"8.05\"><e_i/><s_v>5.1.40</s_v><m_i/><a_r>0</a_r><i_i>0</i_i><f_i c=\"{}\">{fields}</f_i><r_i c=\"1\"><r>{cells}</r></r_i></result></xml>",
            columns.len()
        )
    }
}
