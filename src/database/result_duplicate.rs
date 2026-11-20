use std::collections::HashSet;
use std::sync::{Arc, Weak};

use mysql::{AccessMode, Column, Conn, TxOpts, prelude::Queryable};

use crate::http_tunnel::Field;
use crate::sql_tools::mysql_text_literal;

use super::editable_query::{DuplicateSelect, duplicate_select};
use super::http_session::{Session as HttpSession, http_cell};
use super::row_editor::{
    TableLayout, fingerprint_column, http_bounded_select, http_layout, http_page_from_raw,
    layout_guard, load_layout, native_fingerprint_column, page_from_rows,
};
use super::{
    Cell, DbObject, ObjectKind, QueryReport, SchemaOrigin, Session, TablePage, cell,
    column_is_binary, mysql_error, quote_identifier,
};

/// Proof established from the statement and the actual result packet. Result operations
/// use the packet's original row index, never the visible sort/filter position.
#[derive(Clone, Debug)]
pub struct ResultDuplicateSource {
    sql: Arc<str>,
    object: DbObject,
    projection: Vec<String>,
    evidence: ColumnEvidence,
    session: Option<Weak<()>>,
}

#[derive(Clone, Debug)]
enum ColumnEvidence {
    Native(Vec<Column>),
    Http(Vec<String>),
}

impl ResultDuplicateSource {
    pub fn is_bound(&self) -> bool {
        !self.sql.is_empty()
            && self
                .session
                .as_ref()
                .is_some_and(|pin| pin.strong_count() > 0)
    }

    fn check_session(&self, session: &Arc<()>, cells: &[Cell]) -> Result<(), String> {
        if cells.len() != self.projection.len()
            || self
                .session
                .as_ref()
                .is_none_or(|pin| !pin.ptr_eq(&Arc::downgrade(session)))
        {
            return Err("查询结果的行或连接已变化，请重新执行原查询。".into());
        }
        Ok(())
    }

    fn locate(&self, layout: &mut TableLayout, cells: &[Cell]) -> Result<RowLookup, String> {
        if let Some(error) = &layout.edit_error {
            return Err(error.clone());
        }
        let positions = self
            .projection
            .iter()
            .map(|name| {
                layout
                    .columns
                    .iter()
                    .position(|column| column.name.eq_ignore_ascii_case(name))
                    .ok_or("查询结果的源字段已变化，请重新执行原查询。")
            })
            .collect::<Result<Vec<_>, _>>()?;
        let by_key =
            layout.select_projected_key(&positions).is_ok() && !layout.row_key().is_empty();
        if !by_key && positions.len() != layout.columns.len() {
            return Err(
                "请在原查询结果中包含完整主键、完整的非空唯一键，或源表的全部字段。".into(),
            );
        }
        if !by_key
            && layout
                .columns
                .iter()
                .any(|column| !has_exact_text_identity(&column.data_type))
        {
            return Err(
                "这个无键结果含有无法仅凭显示值确定原值的字段。请用 F8 捕获原值后复制。".into(),
            );
        }

        let filters = if !by_key {
            // Fully displayed rows may be duplicates. Using exact bytes prevents a
            // case-insensitive near match from supplying an unrelated row.
            positions
                .iter()
                .zip(cells)
                .map(|(&index, cell)| {
                    let name = quote_identifier(&layout.columns[index].name);
                    format!("CAST({name} AS BINARY) <=> {}", raw_literal(cell))
                })
                .collect::<Vec<_>>()
        } else {
            layout
                .row_key()
                .iter()
                .map(|&index| {
                    let column = &layout.columns[index];
                    let position = positions
                        .iter()
                        .position(|&item| item == index)
                        .ok_or("结果集缺少源表的唯一键字段。")?;
                    let value = match &cells[position] {
                        Cell::Text(text) => mysql_text_literal(text),
                        Cell::Binary(_) if column.is_binary() => raw_literal(&cells[position]),
                        _ => return Err("唯一键的原值无法按当前字符集精确定位。"),
                    };
                    Ok(format!("{} <=> {value}", quote_identifier(&column.name)))
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        Ok(RowLookup {
            positions,
            by_key,
            filter: filters.join(" AND "),
        })
    }
}

struct RowLookup {
    positions: Vec<usize>,
    by_key: bool,
    filter: String,
}

fn has_exact_text_identity(sql_type: &str) -> bool {
    // Text-protocol FLOAT, DOUBLE, ENUM/SET and localized TIMESTAMP values
    // can hide distinct physical values. Do not use those values as a key.
    match sql_type
        .split(['(', ' '])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "bit" | "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "decimal"
        | "numeric" | "date" | "datetime" | "time" | "year" | "char" | "varchar" | "binary"
        | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" | "tinytext" | "text"
        | "mediumtext" | "longtext" => true,
        _ => false,
    }
}

fn raw_literal(cell: &Cell) -> String {
    use std::fmt::Write;

    let bytes = match cell {
        Cell::Null => return "NULL".into(),
        Cell::Text(value) => value.as_bytes(),
        Cell::Binary(bytes) => bytes,
    };
    let mut sql = String::with_capacity(bytes.len() * 2 + 3);
    sql.push_str("X'");
    for byte in bytes {
        let _ = write!(sql, "{byte:02X}");
    }
    sql.push('\'');
    sql
}

pub(super) fn capture_native(sql: &str, fields: &[Column]) -> Option<ResultDuplicateSource> {
    let parsed = duplicate_select(sql)?;
    let first = fields.first()?;
    let schema = std::str::from_utf8(first.schema_ref()).ok()?;
    let table = std::str::from_utf8(first.org_table_ref()).ok()?;
    if schema.is_empty()
        || table.is_empty()
        || !parsed.table.eq_ignore_ascii_case(table)
        || parsed
            .schema
            .as_deref()
            .is_some_and(|expected| !expected.eq_ignore_ascii_case(schema))
        || fields.iter().any(|column| {
            column.schema_ref() != first.schema_ref()
                || column.org_table_ref() != first.org_table_ref()
        })
    {
        return None;
    }
    let projection = fields
        .iter()
        .map(|column| {
            std::str::from_utf8(column.org_name_ref())
                .map(str::to_owned)
                .ok()
        })
        .collect::<Option<Vec<_>>>()?;
    unique_projection(&projection).then(|| ResultDuplicateSource {
        sql: Arc::from(sql),
        object: DbObject {
            schema: schema.into(),
            name: table.into(),
            kind: ObjectKind::Table,
        },
        projection,
        evidence: ColumnEvidence::Native(fields.to_vec()),
        session: None,
    })
}

pub(super) fn bind_native(report: &mut QueryReport, session: &Arc<()>) {
    let single = report.error.is_none() && report.omitted_sets == 0 && report.sets.len() == 1;
    for set in &mut report.sets {
        if single {
            if let Some(source) = &mut set.duplicate_source {
                source.session = Some(Arc::downgrade(session));
            }
        } else {
            set.duplicate_source = None;
        }
    }
}

pub(super) fn http_source_projection(
    sql: &str,
    database: Option<&str>,
    fields: &[Field],
) -> Option<(DbObject, Vec<String>)> {
    let parsed = duplicate_select(sql)?;
    let schema = parsed.schema.as_deref().or(database)?;
    let alias = parsed.alias.as_deref().unwrap_or(&parsed.table);
    if schema.is_empty()
        || fields.is_empty()
        || fields.iter().any(|field| {
            !field.table.eq_ignore_ascii_case(alias)
                && !field.table.eq_ignore_ascii_case(&parsed.table)
        })
    {
        return None;
    }
    let projection = http_projection(&parsed, fields)?;
    Some((
        DbObject {
            schema: schema.into(),
            name: parsed.table,
            kind: ObjectKind::Table,
        },
        projection,
    ))
}

pub(super) fn bind_http(
    sql: &str,
    object: DbObject,
    projection: Vec<String>,
    fields: &[Field],
    session: &Arc<()>,
) -> Option<ResultDuplicateSource> {
    unique_projection(&projection).then(|| ResultDuplicateSource {
        sql: Arc::from(sql),
        object,
        projection,
        evidence: ColumnEvidence::Http(
            fields.iter().map(|field| field.data_type.clone()).collect(),
        ),
        session: Some(Arc::downgrade(session)),
    })
}

fn http_projection(query: &DuplicateSelect, fields: &[Field]) -> Option<Vec<String>> {
    let stars = query
        .projection
        .iter()
        .filter(|(name, _)| name.is_none())
        .count();
    let explicit = query.projection.len() - stars;
    let star_width = fields.len().checked_sub(explicit)?;
    if stars > 1 || (stars == 0) != (star_width == 0) {
        return None;
    }
    let mut actual = fields.iter();
    let mut names = Vec::with_capacity(fields.len());
    for (name, label) in &query.projection {
        if let Some(name) = name {
            let field = actual.next()?;
            if !field
                .name
                .eq_ignore_ascii_case(label.as_deref().unwrap_or(name))
            {
                return None;
            }
            names.push(name.clone());
        } else {
            names.extend(
                actual
                    .by_ref()
                    .take(star_width)
                    .map(|field| field.name.clone()),
            );
        }
    }
    names.iter().all(|name| !name.is_empty()).then_some(names)
}

fn unique_projection(names: &[String]) -> bool {
    let mut unique = HashSet::new();
    names
        .iter()
        .all(|name| !name.is_empty() && unique.insert(name.to_lowercase()))
}

pub(super) fn load_native(
    active: &mut Session,
    source: ResultDuplicateSource,
    cells: Vec<Cell>,
) -> Result<TablePage, String> {
    source.check_session(&active.identity, &cells)?;
    let ColumnEvidence::Native(evidence) = &source.evidence else {
        return Err("查询结果的连接类型已变化。".into());
    };
    let active_server = SchemaOrigin::native(&mut active.conn, &active.identity)?;
    let mut connection = Conn::new(active.opts.clone()).map_err(mysql_error)?;
    let lookup_server = SchemaOrigin::native(&mut connection, &active.identity)?;
    if !active_server.same_server(&lookup_server) {
        return Err("独立连接未指向原查询所在的服务器。请重新连接后再试。".into());
    }
    let mut transaction = connection
        .start_transaction(TxOpts::default().set_access_mode(Some(AccessMode::ReadOnly)))
        .map_err(mysql_error)?;
    let qualified = source.object.qualified_name();
    let mut layout = load_layout(&mut transaction, source.object.clone(), &qualified)?;
    let lookup = source.locate(&mut layout, &cells)?;
    let keyless = layout.is_keyless();
    let mut columns = layout.projection();
    if keyless {
        columns.push_str(", ");
        columns.push_str(&native_fingerprint_column(
            &layout,
            None,
            "__harbor_duplicate_fingerprint",
        ));
    }
    let sql = format!(
        "SELECT {columns} FROM {qualified} WHERE {} LIMIT 2",
        lookup.filter
    );
    let mut result = transaction.query_iter(sql).map_err(mysql_error)?;
    let metadata = result.columns();
    let current = metadata
        .as_ref()
        .get(..layout.columns.len())
        .ok_or_else(source_changed)?;
    if !lookup
        .positions
        .iter()
        .zip(evidence)
        .all(|(&index, original)| {
            let observed = &current[index];
            observed.schema_ref() == original.schema_ref()
                && observed.org_table_ref() == original.org_table_ref()
                && observed.org_name_ref() == original.org_name_ref()
                && observed.column_type() == original.column_type()
                && observed.character_set() == original.character_set()
                && observed.column_length() == original.column_length()
                && observed.decimals() == original.decimals()
                && observed.flags() == original.flags()
        })
    {
        return Err(source_changed());
    }
    let binary = current.iter().map(column_is_binary).collect::<Vec<_>>();
    let mut page = page_from_rows(
        layout,
        &binary,
        None,
        result.iter().into_iter().flatten(),
        keyless,
        2,
        0,
    )?;
    if page.rows.is_empty()
        || lookup.by_key && (page.has_more || page.rows.len() != 1)
        || !page.rows.iter().all(|row| {
            lookup
                .positions
                .iter()
                .zip(&cells)
                .all(|(&index, observed)| {
                    cell(row.original()[index].clone(), binary[index]) == *observed
                })
        })
    {
        return Err(row_changed());
    }
    page.rows.truncate(1);
    page.has_more = false;
    Ok(page)
}

pub(super) fn load_http(
    session: &HttpSession,
    source: ResultDuplicateSource,
    cells: Vec<Cell>,
) -> Result<TablePage, String> {
    source.check_session(&session.identity, &cells)?;
    let ColumnEvidence::Http(evidence) = &source.evidence else {
        return Err("查询结果的连接类型已变化。".into());
    };
    let qualified = source.object.qualified_name();
    let schema = source.object.schema.clone();
    let mut layout = http_layout(session, source.object.clone())?;
    let lookup = source.locate(&mut layout, &cells)?;
    let columns = layout.projection();
    let fingerprint = fingerprint_column(&layout, None, "__harbor_duplicate_fingerprint");
    let guard = layout_guard(&layout);
    let (set, _) = http_bounded_select(session, &schema, 2, |_| {
        format!(
            "SELECT {columns}, {fingerprint} FROM {qualified} WHERE {} AND ({guard}) LIMIT 2",
            lookup.filter,
        )
    })?;
    if set.columns.len() != layout.columns.len() + 1
        || !lookup
            .positions
            .iter()
            .zip(evidence)
            .all(|(&index, expected)| set.columns[index].data_type.eq_ignore_ascii_case(expected))
    {
        return Err(source_changed());
    }
    if set.rows.is_empty()
        || lookup.by_key && (set.truncated || set.rows.len() != 1)
        || !set.rows.iter().all(|row| {
            lookup
                .positions
                .iter()
                .zip(&cells)
                .all(|(&index, observed)| {
                    row.get(index)
                        .is_some_and(|value| http_cell(value.clone()) == *observed)
                })
        })
    {
        return Err(row_changed());
    }
    let width = layout.columns.len();
    let mut page = http_page_from_raw(layout, set, &(0..width).collect::<Vec<_>>(), true, 2, 0)?;
    if page.rows.is_empty() || page.edit_error.is_some() {
        return Err(row_changed());
    }
    page.rows.truncate(1);
    page.has_more = false;
    Ok(page)
}

fn source_changed() -> String {
    "查询结果的源字段已变化，请重新执行原查询。".into()
}

fn row_changed() -> String {
    "找不到与查询结果原值一致的记录。请重新执行原查询后再复制。".into()
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use mysql::Opts;

    use super::*;
    use crate::database::http_session::workflow_test::{
        TestGateway, archived_tunnel, fixture_database,
    };
    use crate::database::{Command, ConnectionConfig, Response, Worker};

    struct Databases {
        options: Opts,
        source: String,
        other: String,
    }

    impl Drop for Databases {
        fn drop(&mut self) {
            if let Ok(mut connection) = Conn::new(self.options.clone()) {
                for schema in [&self.source, &self.other] {
                    let _ = connection.query_drop(format!("DROP DATABASE IF EXISTS {schema}"));
                }
            }
        }
    }

    #[test]
    #[ignore = "requires disposable MariaDB via HARBOR_HTTP_FIXTURE_PORT"]
    fn duplicates_a_regular_result_row_from_its_recorded_schema_and_key() {
        let (port, options) = fixture_database();
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let databases = Databases {
            options: options.clone(),
            source: format!("harbor_http_duplicate_a_{stamp:x}"),
            other: format!("harbor_http_duplicate_b_{stamp:x}"),
        };
        let mut setup = Conn::new(options.clone()).unwrap();
        for schema in [&databases.source, &databases.other] {
            setup
                .query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
                .unwrap();
            setup
                .query_drop(format!(
                    "CREATE TABLE {schema}.records (id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
                    label VARCHAR(40), detail DECIMAL(26,4), raw VARBINARY(8), optional VARCHAR(40))
                    ENGINE=InnoDB"
                ))
                .unwrap();
        }
        setup
            .query_drop(format!(
                "INSERT INTO {}.records VALUES (1,'第一行',601.3125,X'FF00',NULL),
                (2,'第二行',9007199254740992.0042,X'00FE','来源')",
                databases.source,
            ))
            .unwrap();
        setup
            .query_drop(format!(
                "INSERT INTO {}.records VALUES (2,'第二行',0.1000,X'AA','另一数据库')",
                databases.other,
            ))
            .unwrap();
        setup
            .query_drop(format!(
            "CREATE TABLE {}.keyless(note VARCHAR(40) COLLATE utf8mb4_general_ci, raw VARBINARY(8),
              amount DECIMAL(10,2)) ENGINE=InnoDB", databases.source,
        ))
            .unwrap();
        setup
            .query_drop(format!(
                "INSERT INTO {}.keyless VALUES ('重复a',X'00FF',7.00),('重复a',X'00FF',7.00),
              ('重复A',X'00FF',7.00)",
                databases.source,
            ))
            .unwrap();
        let gateway = TestGateway::new(options);
        let config = ConnectionConfig {
            port,
            database: Some(databases.source.clone()),
            ..Default::default()
        };
        let mut transports = vec![
            Command::Connect(config.clone()),
            Command::ConnectHttp(config.clone(), gateway.tunnel.clone()),
        ];
        if let Some(tunnel) = archived_tunnel() {
            transports.push(Command::ConnectHttp(config, tunnel));
        }

        let query = "SELECT r.id AS code, r.label AS memo FROM records AS r ORDER BY r.id";
        assert!(duplicate_select(query).is_some(), "fixture SELECT shape");
        for (transport, connect) in transports.into_iter().enumerate() {
            let worker = Worker::new();
            let mut id = 0_u64;
            let mut request = |command| {
                id += 1;
                worker.send(id, command).unwrap();
                let event = worker
                    .recv_timeout(Duration::from_secs(15))
                    .unwrap()
                    .unwrap();
                assert_eq!(event.id, id);
                event.result.unwrap()
            };
            assert!(matches!(request(connect), Response::Connected(_)));
            let Response::Queried(report) = request(Command::Query {
                sql: query.into(),
                limit: 10,
            }) else {
                panic!("expected ordinary query result")
            };
            assert!(report.error.is_none());
            let set = &report.sets[0];
            assert_eq!(set.columns, ["code", "memo"]);
            let source = set.duplicate_source.clone().unwrap_or_else(|| {
                panic!(
                    "ordinary SELECT source proof, transport {transport}: {:?}",
                    set.column_hints
                )
            });
            let reordered = [1, 0];
            let cells = set.rows[reordered[0]].clone();
            assert_eq!(cells[0], Cell::Text("2".into()));
            assert!(matches!(
                request(Command::UseSchema {
                    schema: databases.other.clone()
                }),
                Response::SchemaChanged(_)
            ));
            let Response::ResultRowDuplicate(page) =
                request(Command::DuplicateResultRow { source, cells })
            else {
                panic!("expected a source row for the insert draft")
            };
            assert_eq!(page.object.schema, databases.source);
            assert_eq!(page.rows.len(), 1);
            let row = &page.rows[0];
            assert!(row.source_columns()[0].auto_increment);
            assert_eq!(
                row.source_cell(2),
                Some(Cell::Text("9007199254740992.0042".into()))
            );
            assert_eq!(row.source_cell(3), Some(Cell::Binary(vec![0, 254])));
            assert_eq!(row.source_cell(4), Some(Cell::Text("来源".into())));
            let count = setup
                .query_first::<u64, _>(format!("SELECT COUNT(*) FROM {}.records", databases.source))
                .unwrap();
            assert_eq!(count, Some(2));

            let Response::Queried(report) = request(Command::Query {
                sql: format!("SELECT * FROM {}.keyless", databases.source),
                limit: 10,
            }) else {
                panic!("expected complete keyless query result")
            };
            let set = &report.sets[0];
            let source = set
                .duplicate_source
                .clone()
                .expect("complete keyless source proof");
            let cells = set
                .rows
                .iter()
                .find(|row| row[0] == Cell::Text("重复a".into()))
                .unwrap()
                .clone();
            let Response::ResultRowDuplicate(page) =
                request(Command::DuplicateResultRow { source, cells })
            else {
                panic!("expected the identical keyless values")
            };
            assert_eq!(page.rows.len(), 1);
            assert_eq!(
                page.rows[0].source_cell(0),
                Some(Cell::Text("重复a".into()))
            );
            assert_eq!(
                page.rows[0].source_cell(1),
                Some(Cell::Binary(vec![0, 255]))
            );
        }

        gateway.stop();
    }
}
