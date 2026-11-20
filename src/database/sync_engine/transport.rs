use std::borrow::Cow;

use crate::http_tunnel;
use crate::sql_tools::{mysql_binary_literal, mysql_text_literal};

use super::super::http_session::{RESPONSE_TOO_LARGE, Session, batch_statements};
use super::*;

pub(super) mod apply;
pub(in crate::database) use apply::{
    apply_many_transported, apply_transported, export_transported,
};

#[derive(Clone)]
pub(in crate::database) enum Transport {
    Native(Opts),
    Http(Box<Session>),
}

impl Transport {
    fn with_reader<T>(
        &self,
        side: SyncSide,
        run: impl FnOnce(Reader<'_, '_>, EndpointIdentity) -> Result<T, String>,
    ) -> Result<T, String> {
        match self {
            Self::Native(opts) => {
                let (mut connection, identity) = connect(opts, side.label())?;
                let mut transaction = snapshot(&mut connection)?;
                run(Reader::Native(&mut transaction), identity)
            }
            Self::Http(session) => {
                let (host, port, user) = session.database_address();
                let server = read_metadata::<2>(session,
                    "SHOW GLOBAL VARIABLES WHERE Variable_name IN ('server_uuid','server_uid','hostname','port','version','server_id')", 12)?
                    .into_iter().map(|row| match row {
                        [Some(name), Some(value)] => Ok((name.to_ascii_lowercase(), value)),
                        _ => Err("隧道返回的服务器标识不完整。"),
                    }).collect::<Result<_, _>>()?;
                let identity = EndpointIdentity {
                    host: host.to_owned(),
                    port,
                    socket: None,
                    user: Some(user.to_owned()),
                    server,
                };
                run(Reader::Http(HttpReader(session)), identity)
            }
        }
    }
}

enum Reader<'a, 'c> {
    Native(&'a mut Transaction<'c>),
    Http(HttpReader<'a>),
}

impl Reader<'_, '_> {
    fn layout(&mut self, object: &DbObject) -> Result<Layout, String> {
        match self {
            Self::Native(tx) => Layout::read(tx, object),
            Self::Http(reader) => http_layout(reader.0, object),
        }
    }

    fn catalog(&mut self, object: &DbObject) -> Result<CatalogTable, String> {
        match self {
            Self::Native(tx) => read_catalog(*tx, object),
            Self::Http(reader) => http_catalog(reader.0, object),
        }
    }

    fn parents(&mut self, object: &DbObject) -> Result<Vec<CatalogTable>, String> {
        let sql = foreign_key_parent_sql();
        match self {
            Self::Native(tx) => tx
                .exec_opt::<CatalogTable, _, _>(
                    &sql,
                    params! { "schema" => object.schema.as_str(), "table" => object.name.as_str() },
                )
                .map_err(mysql_error)?
                .into_iter()
                .map(|row| row.map_err(|e| mysql_error(e.into())))
                .collect(),
            Self::Http(reader) => read_metadata::<2>(reader.0, &bind_object(sql, object), 8193)?
                .into_iter()
                .map(|row| match row {
                    [Some(schema), Some(name)] => Ok((schema, name)),
                    _ => Err("隧道返回的外键目标不完整。".into()),
                })
                .collect(),
        }
    }
}

impl ReadTransport for Reader<'_, '_> {
    fn check_filter_tables(
        &mut self,
        tables: &[DbObject],
        writing: Option<&CatalogTable>,
    ) -> Result<(), String> {
        match self {
            Self::Native(tx) => tx.check_filter_tables(tables, writing),
            Self::Http(reader) => reader.check_filter_tables(tables, writing),
        }
    }

    fn projection<'a>(&self, layout: &'a Layout) -> Cow<'a, str> {
        match self {
            Self::Native(tx) => tx.projection(layout),
            Self::Http(reader) => reader.projection(layout),
        }
    }

    fn for_rows(
        &mut self,
        sql: &str,
        args: Vec<Value>,
        columns: &[ColumnLayout],
        prefix: RowPrefix,
        limit: usize,
        visit: impl FnMut(Vec<Value>) -> Result<bool, String>,
    ) -> Result<(), String> {
        match self {
            Self::Native(tx) => tx.for_rows(sql, args, columns, prefix, limit, visit),
            Self::Http(reader) => reader.for_rows(sql, args, columns, prefix, limit, visit),
        }
    }
}

pub(in crate::database) fn compare_transported(
    source: &Transport,
    target: &Transport,
    mapping: SyncMapping,
    policy: SyncPolicy,
    mut progress: impl FnMut(SyncProgress) -> Result<(), String>,
) -> Result<SyncPlan, String> {
    if let (Transport::Native(source), Transport::Native(target)) = (source, target) {
        return compare_filtered(source, target, mapping, policy, progress);
    }
    progress(SyncProgress::default())?;
    source.with_reader(SyncSide::Source, |mut src, source_endpoint| {
        target.with_reader(SyncSide::Target, |mut dst, target_endpoint| {
            let SyncMapping {
                source,
                target,
                filter,
                compare_columns,
            } = mapping;
            let mut source_layout = src.layout(&source)?;
            let mut target_layout = dst.layout(&target)?;
            if source_layout.table == target_layout.table
                && maybe_same_server(&source_endpoint, &target_endpoint)
            {
                return Err("请选择两个不同的表。".into());
            }
            source_layout.choose_shared_key(&mut target_layout)?;
            let scope = Scope::resolve(
                filter,
                compare_columns,
                policy.mode,
                &source_layout,
                &target_layout,
                &source_endpoint,
                &target_endpoint,
            )?;
            scope.select_rows(&mut source_layout, &mut target_layout);
            let inspection = inspect_pair(
                &mut src,
                &source_layout,
                &mut dst,
                &target_layout,
                &scope,
                true,
                |source_rows, target_rows| {
                    progress(SyncProgress {
                        source_rows,
                        target_rows,
                        ..Default::default()
                    })
                },
            )?;
            Ok(build_plan(
                source,
                target,
                policy,
                source_endpoint,
                target_endpoint,
                source_layout,
                target_layout,
                scope,
                inspection,
            ))
        })
    })
}

pub(in crate::database) fn compare_many_transported(
    source: &Transport,
    target: &Transport,
    mappings: Vec<SyncMapping>,
    policy: SyncPolicy,
    mut progress: impl FnMut(SyncProgress) -> Result<(), String>,
) -> Result<SyncGroupPlan, String> {
    if let (Transport::Native(source), Transport::Native(target)) = (source, target) {
        return compare_many_filtered(source, target, mappings, policy, progress);
    }
    let pairs = mappings
        .iter()
        .map(|mapping| (mapping.source.clone(), mapping.target.clone()))
        .collect::<Vec<_>>();
    let order = transported_dependencies(source, target, &pairs, policy, &mut progress)?;
    let mut selected = mappings.into_iter().map(Some).collect::<Vec<_>>();
    let mut tables = Vec::new();
    let mut pairs_ordered = Vec::new();
    let mut completed = SyncProgress::default();
    for index in order.target {
        let pair = &pairs[index];
        let plan = compare_transported(
            source,
            target,
            selected[index]
                .take()
                .expect("dependency order contains each table once"),
            policy,
            |current| progress(group_progress(completed, current)),
        )?;
        completed.source_rows += plan.approval.source.snapshot.rows;
        completed.target_rows += plan.approval.target.snapshot.rows;
        tables.push(plan);
        pairs_ordered.push(pair.clone());
    }
    Ok(SyncGroupPlan {
        tables,
        mappings: pairs_ordered,
        mode: policy.mode,
        skip_target_fk_checks: policy.skip_target_fk_checks,
        continue_on_row_error: policy.continue_on_row_error,
        dependencies: order.dependencies,
    })
}

fn transported_dependencies(
    source: &Transport,
    target: &Transport,
    pairs: &[(DbObject, DbObject)],
    policy: SyncPolicy,
    progress: &mut impl FnMut(SyncProgress) -> Result<(), String>,
) -> Result<OrderedTables, String> {
    if pairs.is_empty() {
        return Err("请选择至少一对表。".into());
    }
    progress(SyncProgress::default())?;
    source.with_reader(SyncSide::Source, |mut src, source_id| {
        target.with_reader(SyncSide::Target, |mut dst, target_id| {
            let mut sources = Vec::new();
            let mut targets = Vec::new();
            for (from, to) in pairs {
                progress(SyncProgress::default())?;
                sources.push(src.catalog(from)?);
                targets.push(dst.catalog(to)?);
            }
            let source_set = sources.iter().cloned().collect::<BTreeSet<_>>();
            let target_set = targets.iter().cloned().collect::<BTreeSet<_>>();
            if source_set.len() != pairs.len() || target_set.len() != pairs.len() {
                return Err("每端一个数据表只能出现在一对映射中。".into());
            }
            if maybe_same_server(&source_id, &target_id) && !source_set.is_disjoint(&target_set) {
                return Err("同一服务器中的源表和目标表不能重叠。".into());
            }
            let (target, target_edges) = if policy.skip_target_fk_checks {
                ((0..pairs.len()).collect(), BTreeSet::new())
            } else {
                reader_order(
                    &mut dst,
                    SyncSide::Target,
                    pairs.iter().map(|pair| &pair.1),
                    &targets,
                    &target_set,
                    progress,
                )?
            };
            let (source, source_edges) = if matches!(policy.mode, SyncMode::ReconcileBoth(_)) {
                reader_order(
                    &mut src,
                    SyncSide::Source,
                    pairs.iter().map(|pair| &pair.0),
                    &sources,
                    &source_set,
                    progress,
                )?
            } else {
                (Vec::new(), BTreeSet::new())
            };
            Ok(OrderedTables {
                source,
                target,
                dependencies: Dependencies {
                    source: source_edges,
                    target: target_edges,
                },
            })
        })
    })
}

fn reader_order<'a>(
    reader: &mut Reader<'_, '_>,
    side: SyncSide,
    objects: impl Iterator<Item = &'a DbObject>,
    catalog: &[CatalogTable],
    selected: &BTreeSet<CatalogTable>,
    progress: &mut impl FnMut(SyncProgress) -> Result<(), String>,
) -> Result<(Vec<usize>, BTreeSet<ForeignKeyEdge>), String> {
    let mut edges = BTreeSet::new();
    for (child, object) in catalog.iter().zip(objects) {
        progress(SyncProgress::default())?;
        for parent in reader.parents(object)? {
            if selected.contains(&parent) {
                edges.insert((child.clone(), parent));
            }
        }
    }
    order_catalogs(catalog, edges, side)
}

fn maybe_same_server(source: &EndpointIdentity, target: &EndpointIdentity) -> bool {
    source.same_server(target) || !source.server.is_empty() && source.server == target.server
}

fn bind_object(sql: String, object: &DbObject) -> String {
    sql.replace(":schema", &mysql_text_literal(&object.schema))
        .replace(":table", &mysql_text_literal(&object.name))
}

fn base_table_condition(object: &DbObject) -> String {
    let filter = bind_object(MATCH_TABLE.to_owned(), object);
    format!(
        "EXISTS (SELECT 1 FROM information_schema.TABLES t WHERE {filter} AND t.TABLE_TYPE = 'BASE TABLE' AND t.ENGINE = 'InnoDB')"
    )
}

pub(super) fn filter_catalog_checks(layout: &Layout) -> impl Iterator<Item = String> + '_ {
    layout.filter_tables.iter().map(base_table_condition)
}

pub(super) fn filter_metadata_pins(layout: &Layout) -> impl Iterator<Item = String> + '_ {
    layout.filter_tables.iter().map(|table| {
        // Keep the dependency definition pinned in the write transaction without returning a set.
        format!("DO (SELECT 1 FROM {} LIMIT 0)", table.qualified_name())
    })
}

pub(super) fn sync_key_condition(layout: &Layout) -> String {
    let (schema, table) = &layout.table;
    let table_filter = MATCH_TABLE
        .replace(":schema", &mysql_binary_literal(schema.as_bytes()))
        .replace(":table", &mysql_binary_literal(table.as_bytes()));
    if layout.is_keyless() {
        return format!(
            "NOT EXISTS (
                SELECT 1 FROM information_schema.STATISTICS t
                WHERE {table_filter} AND t.NON_UNIQUE = 0
                  AND (t.INDEX_NAME = 'PRIMARY' OR (
                    t.INDEX_TYPE = 'BTREE' AND NOT EXISTS (
                        SELECT 1 FROM information_schema.STATISTICS t
                        WHERE {table_filter} AND t.INDEX_NAME = 'PRIMARY' AND t.NON_UNIQUE = 0
                    )
                  ))
                GROUP BY CAST(t.INDEX_NAME AS BINARY)
                HAVING COUNT(*) =
                    COALESCE(SUM(t.COLUMN_NAME IS NOT NULL AND t.SUB_PART IS NULL AND t.NULLABLE = ''), 0)
            )"
        );
    }
    let index_name = mysql_binary_literal(layout.key_index.as_bytes());
    let count = layout.key.len();
    let parts = layout
        .key
        .iter()
        .enumerate()
        .map(|(position, part)| {
            let column = mysql_binary_literal(layout.columns[part.column].name.as_bytes());
            let descending = u8::from(part.descending);
            format!(
                "WHEN {} THEN (CAST(t.COLUMN_NAME AS BINARY) = {column} AND t.SUB_PART IS NULL
                 AND t.NULLABLE = '' AND (t.COLLATION <=> 'D') = {descending})",
                position + 1,
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "(SELECT COUNT(*) = {count}
         AND COALESCE(SUM(CASE t.SEQ_IN_INDEX {parts} ELSE 0 END), 0) = {count}
         FROM information_schema.STATISTICS t
         WHERE {table_filter} AND CAST(t.INDEX_NAME AS BINARY) = {index_name}
           AND t.NON_UNIQUE = 0 AND (t.INDEX_NAME = 'PRIMARY' OR t.INDEX_TYPE = 'BTREE'))"
    )
}

fn read_metadata<const N: usize>(
    session: &Session,
    sql: &str,
    limit: usize,
) -> Result<Vec<[Option<String>; N]>, String> {
    let set = http_query(session, sql, limit)?;
    if set.columns.len() != N {
        return Err("隧道返回的同步元数据列数不符。".into());
    }
    set.rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| value.map(String::from_utf8).transpose())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| "隧道返回了无法解码的同步元数据。")?
                .try_into()
                .map_err(|_| "隧道返回的同步元数据列数不符。".into())
        })
        .collect()
}

fn http_catalog(session: &Session, object: &DbObject) -> Result<CatalogTable, String> {
    if object.kind != ObjectKind::Table {
        return Err("只能同步数据表。".into());
    }
    let mut rows = read_metadata::<4>(session, &bind_object(catalog_sql(), object), 1)?;
    let [Some(schema), Some(name), Some(kind), engine] = rows.pop().ok_or("找不到同步表。")?
    else {
        return Err("隧道返回的同步表属性不完整。".into());
    };
    checked_catalog(object, (schema, name, kind, engine))
}

fn http_layout(session: &Session, object: &DbObject) -> Result<Layout, String> {
    let table = http_catalog(session, object)?;
    let columns = read_metadata::<9>(session, &bind_object(columns_sql(), object), 10_000)?
        .into_iter()
        .map(|row| match row {
            [
                Some(name),
                Some(kind),
                Some(declaration),
                Some(nullable),
                generation,
                charset,
                collation,
                precision,
                scale,
            ] => Ok((
                name,
                kind,
                declaration,
                nullable,
                generation,
                charset,
                collation,
                number(precision)?,
                number(scale)?,
            )),
            _ => Err("隧道返回的同步列属性不完整。".into()),
        });
    let keys = read_metadata::<6>(session, &bind_object(key_sql(), object), 10_000)?
        .into_iter()
        .map(|[index, name, position, prefix, direction, index_type]| {
            Ok((
                index.ok_or("隧道未返回索引名称。")?,
                name,
                number(position)?.ok_or("隧道未返回索引字段顺序。")?,
                number(prefix)?,
                direction,
                index_type.ok_or("隧道未返回索引类型。")?,
            ))
        });
    let qualified = object.qualified_name();
    let mut fields = read_metadata::<2>(session, &format!("SHOW CREATE TABLE {qualified}"), 1)?;
    let [_, Some(definition)] = fields.pop().ok_or("隧道未返回表定义。")? else {
        return Err("隧道返回的表定义不完整。".into());
    };
    Layout::from_metadata(
        table,
        qualified,
        stable_definition(definition),
        columns,
        keys,
    )
}

fn number<T: std::str::FromStr>(text: Option<String>) -> Result<Option<T>, String> {
    text.map(|text| {
        text.parse()
            .map_err(|_| "隧道返回的同步元数据不是有效数字。".into())
    })
    .transpose()
}

pub(super) trait ReadTransport {
    fn check_filter_tables(
        &mut self,
        tables: &[DbObject],
        writing: Option<&CatalogTable>,
    ) -> Result<(), String>;

    fn projection<'a>(&self, layout: &'a Layout) -> Cow<'a, str> {
        Cow::Borrowed(&layout.projection)
    }

    fn for_rows(
        &mut self,
        sql: &str,
        args: Vec<Value>,
        columns: &[ColumnLayout],
        prefix: RowPrefix,
        limit: usize,
        visit: impl FnMut(Vec<Value>) -> Result<bool, String>,
    ) -> Result<(), String>;
}

impl ReadTransport for Transaction<'_> {
    fn check_filter_tables(
        &mut self,
        tables: &[DbObject],
        writing: Option<&CatalogTable>,
    ) -> Result<(), String> {
        for table in tables {
            verify_filter_relation(table, read_catalog(self, table)?, writing)?;
            self.query_drop(format!("SELECT 1 FROM {} LIMIT 0", table.qualified_name()))
                .map_err(mysql_error)?;
            // Recheck after taking the same metadata pin as Layout::read.
            verify_filter_relation(table, read_catalog(self, table)?, writing)?;
        }
        Ok(())
    }

    fn for_rows(
        &mut self,
        sql: &str,
        args: Vec<Value>,
        _columns: &[ColumnLayout],
        _prefix: RowPrefix,
        _limit: usize,
        mut visit: impl FnMut(Vec<Value>) -> Result<bool, String>,
    ) -> Result<(), String> {
        {
            let result = self
                .exec_iter(sql, Params::Positional(args))
                .map_err(mysql_error)?;
            for row in result {
                if !visit(row.map_err(mysql_error)?.unwrap())? {
                    break;
                }
            }
        }
        check_write_warnings(self)
    }
}

pub(super) struct HttpReader<'a>(pub(super) &'a Session);

impl ReadTransport for HttpReader<'_> {
    fn check_filter_tables(
        &mut self,
        tables: &[DbObject],
        writing: Option<&CatalogTable>,
    ) -> Result<(), String> {
        for table in tables {
            verify_filter_relation(table, http_catalog(self.0, table)?, writing)?;
        }
        Ok(())
    }

    fn projection<'a>(&self, layout: &'a Layout) -> Cow<'a, str> {
        Cow::Owned(
            layout
                .columns
                .iter()
                .map(|column| {
                    if column.kind == "float" {
                        format!("({} + 0e0)", column.quoted)
                    } else {
                        column.read.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(", "),
        )
    }

    fn for_rows(
        &mut self,
        sql: &str,
        args: Vec<Value>,
        columns: &[ColumnLayout],
        prefix: RowPrefix,
        limit: usize,
        mut visit: impl FnMut(Vec<Value>) -> Result<bool, String>,
    ) -> Result<(), String> {
        let sql = render_parameters(sql, &args)?;
        let raw = http_query(self.0, &sql, limit)?;
        let width = columns.len() + usize::from(!matches!(prefix, RowPrefix::None));
        if raw.columns.len() != width {
            return Err("隧道返回的数据列数与表结构不一致。".into());
        }
        for row in raw.rows {
            let mut values = Vec::with_capacity(width);
            let mut row = row.into_iter();
            match prefix {
                RowPrefix::None => {}
                RowPrefix::Index => {
                    let number = row
                        .next()
                        .flatten()
                        .as_deref()
                        .and_then(parse_unsigned)
                        .ok_or("隧道未返回正确的匹配序号。")?;
                    values.push(Value::UInt(number));
                }
                RowPrefix::Digest => {
                    values.push(row.next().flatten().map_or(Value::NULL, Value::Bytes));
                }
            }
            for (column, value) in columns.iter().zip(row) {
                values.push(decode_value(column, value)?);
            }
            if !visit(values)? {
                break;
            }
        }
        Ok(())
    }
}

fn verify_filter_relation(
    table: &DbObject,
    canonical: CatalogTable,
    writing: Option<&CatalogTable>,
) -> Result<(), String> {
    if writing == Some(&canonical) {
        return Err(format!(
            "当前写入表 {} 不能作为同步筛选的子查询来源。",
            table.qualified_name(),
        ));
    }
    Ok(())
}

pub(super) fn too_large(error: &str) -> bool {
    error == RESPONSE_TOO_LARGE
}

fn parse_unsigned(raw: &[u8]) -> Option<u64> {
    std::str::from_utf8(raw).ok()?.parse().ok()
}

fn decode_value(column: &ColumnLayout, raw: Option<Vec<u8>>) -> Result<Value, String> {
    let Some(raw) = raw else {
        return Ok(Value::NULL);
    };
    if matches!(
        column.kind.as_str(),
        "decimal"
            | "numeric"
            | "char"
            | "varchar"
            | "tinytext"
            | "text"
            | "mediumtext"
            | "longtext"
            | "json"
            | "binary"
            | "varbinary"
            | "tinyblob"
            | "blob"
            | "mediumblob"
            | "longblob"
    ) {
        return Ok(Value::Bytes(raw));
    }
    let invalid = || format!("隧道返回了无法精确读取的「{}」字段值。", column.name);
    let text = std::str::from_utf8(&raw).map_err(|_| invalid())?;
    let value = match column.kind.as_str() {
        "bit" | "enum" | "set" | "year" => text.parse::<u64>().ok().map(Value::UInt),
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" => {
            if column.declaration.contains("unsigned") || column.declaration.contains("zerofill") {
                text.parse::<u64>().ok().map(Value::UInt)
            } else {
                text.parse::<i64>().ok().map(Value::Int)
            }
        }
        "float" => text
            .parse::<f32>()
            .ok()
            .filter(|value| value.is_finite())
            .map(Value::Float),
        "double" | "real" => text
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .map(Value::Double),
        "date" | "datetime" | "timestamp" | "time" => {
            super::super::row_editor::parse_temporal_value(&column.kind, text)
        }
        _ => None,
    };
    value.ok_or_else(invalid)
}

pub(super) fn render_parameters(sql: &str, args: &[Value]) -> Result<String, String> {
    let mut values = args.iter();
    let mut quote = None;
    let mut output = String::new();
    for character in sql.chars() {
        if let Some(current) = quote {
            if character == current {
                quote = None;
            }
        } else if matches!(character, '\'' | '"' | '\x60') {
            quote = Some(character);
        } else if character == '?' {
            let value = values.next().ok_or("同步参数数量不符。")?;
            match value {
                Value::Bytes(bytes) => {
                    output.push_str("UNHEX('");
                    for byte in bytes {
                        write!(output, "{byte:02X}").unwrap();
                    }
                    output.push_str("')");
                }
                Value::Float(value) if value.is_finite() => {
                    write!(output, "{:e}", f64::from(*value)).unwrap();
                }
                Value::Double(value) if value.is_finite() => {
                    write!(output, "{value:e}").unwrap();
                }
                Value::Float(_) | Value::Double(_) => return Err("同步需要有限浮点数。".into()),
                _ => output.push_str(&value.as_sql(true)),
            }
            continue;
        }
        output.push(character);
    }
    if values.next().is_some() {
        return Err("同步参数数量不符。".into());
    }
    Ok(output)
}

fn http_query(
    session: &Session,
    sql: &str,
    limit: usize,
) -> Result<http_tunnel::ResultSet, String> {
    let commands = [
        "SET NAMES utf8mb4",
        "SET SESSION time_zone = '+00:00', SESSION sql_quote_show_create = 1, SESSION sql_select_limit = 18446744073709551615",
        sql,
    ];
    let batch = batch_statements(&commands)?;
    let payload = session.prepare_payload(&batch, true, "")?;
    let reply = session.execute_payload(payload, limit)?;
    single_set(reply)
}

fn single_set(mut reply: http_tunnel::Reply) -> Result<http_tunnel::ResultSet, String> {
    if let Some(error) = reply.error {
        return Err(format!("ERROR {}: {}", error.code, error.message));
    }
    let set = reply.sets.pop().ok_or("HTTP 隧道没有返回同步结果。")?;
    if set.truncated || !reply.sets.is_empty() || reply.omitted_sets > 0 {
        return Err("HTTP 隧道返回的同步结果数量不符。".into());
    }
    Ok(set)
}
