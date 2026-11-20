use std::collections::BTreeMap;

use mysql::{Conn, params, prelude::Queryable};

use crate::sql_tools::mysql_text_literal;

use super::{DbResult, Failure, SchemaDesignTable, SchemaUniqueIndex, http_session::Session};

const INDEX_ROWS: usize = 16_384;
const SQL: &str = "
    SELECT TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX, COLUMN_NAME, SUB_PART
    FROM information_schema.STATISTICS
    WHERE TABLE_SCHEMA = :schema AND NON_UNIQUE = 0
        AND (@@lower_case_table_names <> 0 OR BINARY TABLE_SCHEMA = BINARY :schema)
    ORDER BY BINARY TABLE_NAME, BINARY INDEX_NAME, SEQ_IN_INDEX
    LIMIT 16385";

type IndexRow = (String, String, usize, Option<String>, Option<u64>);

pub(super) fn read_native(conn: &mut Conn, schema: &str, tables: &mut [SchemaDesignTable]) {
    if let Ok(rows) = conn.exec_iter(SQL, params! { "schema" => schema }) {
        attach_if_complete(
            tables,
            rows.map(|row| {
                mysql::from_row_opt::<IndexRow>(row?)
                    .map_err(mysql::Error::from)
                    .map_err(Failure::from)
            }),
        );
    }
}

pub(super) fn read_http(session: &Session, schema: &str, tables: &mut [SchemaDesignTable]) {
    let sql = SQL.replace(":schema", &mysql_text_literal(schema));
    if let Ok(rows) = session.metadata_columns::<5>(&sql, INDEX_ROWS + 1) {
        attach_if_complete(
            tables,
            rows.into_iter().map(|row| {
                let malformed = || Failure::Message("索引元数据不完整。".into());
                let [Some(table), Some(index), Some(position), column, prefix] = row else {
                    return Err(malformed());
                };
                let position = position.parse::<usize>().map_err(|_| malformed())?;
                let prefix = prefix
                    .map(|prefix| prefix.parse::<u64>())
                    .transpose()
                    .map_err(|_| malformed())?;
                Ok((table, index, position, column, prefix))
            }),
        );
    }
}

fn attach_if_complete(
    tables: &mut [SchemaDesignTable],
    rows: impl Iterator<Item = DbResult<IndexRow>>,
) {
    let mut indexes = BTreeMap::<String, BTreeMap<String, Option<Vec<String>>>>::new();
    for (offset, row) in rows.enumerate() {
        if offset == INDEX_ROWS {
            return;
        }
        let Ok((table, name, position, column, prefix)) = row else {
            return;
        };
        let index = indexes
            .entry(table)
            .or_default()
            .entry(name)
            .or_insert_with(|| Some(Vec::new()));
        // Only ordinary complete column lists provide directly usable FK keys.
        // An expression or a prefix part must disqualify the entire index.
        if let (Some(fields), Some(column), None) = (index.as_mut(), column, prefix)
            && position == fields.len() + 1
        {
            fields.push(column);
        } else {
            *index = None;
        }
    }
    for table in tables {
        table.unique_indexes = Some(
            indexes
                .remove(&table.object.name)
                .into_iter()
                .flatten()
                .filter_map(|(name, columns)| {
                    columns.map(|columns| SchemaUniqueIndex { name, columns })
                })
                .collect(),
        );
    }
}
