use crate::database::object_operations::grants_allow_global_select;

use super::*;

pub(super) struct MirrorCheck {
    records: Vec<String>,
    source_only: bool,
}

impl MirrorCheck {
    pub(super) fn new(source_only: bool) -> Self {
        Self {
            records: Vec::new(),
            source_only,
        }
    }

    pub(super) fn record(
        &mut self,
        layout: &Layout,
        observed: ObservedRow<'_>,
        remaining: &mut usize,
    ) -> Result<(), String> {
        let Some(exists) = mirror_record_condition(layout, observed)? else {
            return Ok(());
        };
        *remaining = remaining
            .checked_sub(exists.len() + 32)
            .ok_or("本次隧道镜像的复核数据超过 32 MiB，请缩小比较范围。")?;
        self.records.push(exists);
        Ok(())
    }

    pub(super) fn lock(&self, layout: &Layout) -> Option<String> {
        if self.source_only {
            return None;
        }
        let key = &layout.columns[layout.key[0].column].quoted;
        let filter = filter_clause(layout);
        let index_hint = if layout.key_index == "PRIMARY" {
            " FORCE INDEX (PRIMARY)"
        } else {
            ""
        };
        // A locking read under repeatable read also locks gaps within the range scanned.
        // Invisible or ignored unique indexes still enforce uniqueness without a usable hint.
        Some(format!(
            "SELECT {key} FROM {}{index_hint}{filter} FOR UPDATE",
            layout.qualified
        ))
    }

    pub(super) fn checks(&self, layout: &Layout) -> Vec<String> {
        let mut checks = self
            .records
            .chunks(24)
            .map(|row_group| {
                format!(
                    "SET @harbor_sync_ok = (@harbor_sync_ok = 1 AND {})",
                    row_group.join(" AND ")
                )
            })
            .collect::<Vec<_>>();
        if !self.source_only {
            checks.push(format!(
                "SET @harbor_sync_ok = (@harbor_sync_ok = 1 AND {})",
                mirror_count_condition(layout, self.records.len() as u64)
            ));
        }
        checks
    }
}

pub(in crate::database::sync_engine) fn mirror_record_condition(
    layout: &Layout,
    observed: ObservedRow<'_>,
) -> Result<Option<String>, String> {
    let row = match observed {
        ObservedRow::Equal(row)
        | ObservedRow::SourceOnly(row)
        | ObservedRow::TargetOutside(row)
        | ObservedRow::Different { source: row, .. } => row,
        ObservedRow::TargetOnly(_) => return Ok(None),
    };
    let predicate = row_condition(layout, row, true, true)?;
    Ok(Some(format!(
        "EXISTS (SELECT 1 FROM {} WHERE {predicate} LIMIT 1 FOR UPDATE)",
        layout.qualified
    )))
}

pub(in crate::database::sync_engine) fn mirror_count_condition(
    layout: &Layout,
    count: u64,
) -> String {
    let table = &layout.qualified;
    let filter = filter_clause(layout);
    if let Some(digest) = layout.keyless_digest() {
        // Matching full rows are checked individually. Count distinct records to detect extras.
        format!(
            "(SELECT COUNT(DISTINCT {digest}) = {count} AND COUNT({digest}) = COUNT(*)
              FROM {table}{filter} FOR UPDATE)"
        )
    } else {
        format!("(SELECT COUNT(*) FROM {table}{filter}) = {count}")
    }
}

fn filter_clause(layout: &Layout) -> String {
    layout
        .selection
        .as_ref()
        .map_or_else(String::new, |filter| format!(" WHERE ({filter})"))
}

pub(super) fn require_grant_visibility(reader: &mut Reader<'_, '_>) -> Result<(), String> {
    let grants = match reader {
        Reader::Http(reader) => read_metadata::<1>(reader.0, "SHOW GRANTS", 4_096)?
            .into_iter()
            .filter_map(|[value]| value)
            .collect::<Vec<_>>(),
        Reader::Native(tx) => tx.query::<String, _>("SHOW GRANTS").map_err(mysql_error)?,
    };
    if !grants_allow_global_select(&grants) {
        return Err("无法确认接收端账户能看到服务器上的全部外键。请使用拥有全局 SELECT 的账户，或改用不删除的同步模式。".into());
    }
    Ok(())
}

pub(super) struct DeletePolicy {
    pub guards: Vec<DeleteGuard>,
    pub metadata_condition: String,
}

impl DeletePolicy {
    pub(super) fn read(
        reader: &mut Reader<'_, '_>,
        object: &DbObject,
        layout: &Layout,
    ) -> Result<Self, String> {
        let query = bind_object(DELETE_RELATIONS_SQL.to_owned(), object);
        let rows = match reader {
            Reader::Http(reader) => read_metadata::<5>(reader.0, &query, 8_192)?
                .into_iter()
                .map(|row| match row {
                    [
                        Some(schema),
                        Some(table),
                        Some(name),
                        Some(child),
                        Some(parent),
                    ] => Ok((schema, table, name, child, parent)),
                    _ => Err("隧道返回了不完整的级联外键元数据。".to_owned()),
                })
                .collect::<Result<Vec<_>, _>>()?,
            Reader::Native(tx) => tx
                .query_opt::<DeleteRelation, _>(&query)
                .map_err(mysql_error)?
                .into_iter()
                .map(|row| row.map_err(|error| mysql_error(error.into())))
                .collect::<Result<Vec<_>, _>>()?,
        };
        let guards = build_delete_guards(layout, &rows);
        let count = rows.len();
        let match_expected = if rows.is_empty() {
            "0".to_owned()
        } else {
            rows.iter()
                .map(|(schema, table, name, child, parent)| {
                    let fields = [
                        ("TABLE_SCHEMA", schema),
                        ("TABLE_NAME", table),
                        ("CONSTRAINT_NAME", name),
                        ("COLUMN_NAME", child),
                        ("REFERENCED_COLUMN_NAME", parent),
                    ];
                    let terms = fields
                        .into_iter()
                        .map(|(field, value)| {
                            format!("BINARY f.{field} <=> {}", mysql_text_literal(value))
                        })
                        .collect::<Vec<_>>()
                        .join(" AND ");
                    format!("({terms})")
                })
                .collect::<Vec<_>>()
                .join(" OR ")
        };
        // Recheck the complete inbound cascade inventory after acquiring the parent metadata lock.
        let metadata_condition = format!(
            "(SELECT COUNT(*) = {count} AND COALESCE(SUM(({match_expected})), 0) = {count} FROM ({query}) AS f)"
        );
        Ok(Self {
            guards,
            metadata_condition,
        })
    }
}
