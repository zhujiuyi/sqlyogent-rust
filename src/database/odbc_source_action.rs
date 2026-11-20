use std::sync::atomic::AtomicBool;

use crate::odbc::{PreparedSourceAction, SOURCE_SERVER_ID_SQL};
use crate::sql_tools::{mysql_binary_literal, mysql_text_literal};

use super::{Connection, DbObject, OdbcTarget, Response, odbc_import};

pub(super) const SOURCE_TARGET_SAME_TABLE: &str =
    "源表与目标表是同一服务器上的同一张表。请另选目标表后再执行源表操作。";

pub(super) fn run(
    connection: &mut Connection,
    mut table: OdbcTarget,
    source: PreparedSourceAction,
    cancel: &AtomicBool,
) -> Result<Response, String> {
    let plan = source.plan();
    if table.rows.len() != plan.len() {
        return Err("待导入的行数与已核对的源记录不符，请重新读取源数据。".into());
    }
    let guard = source_guard_sql(plan.server_identity(), plan.table_identity(), &table.object);
    table.track_applied_rows = true;
    let created_in = table.create.is_some().then(|| table.object.schema.clone());
    let mut counts = match connection {
        Connection::Native(session) => {
            odbc_import::run_checked(&session.opts, vec![table], cancel, Some(&guard))?
        }
        Connection::Http(session) => {
            odbc_import::run_http_checked(session, vec![table], cancel, Some(&guard))?
        }
    };

    // The target import returns only after its data commit and, when needed,
    // its staging tables' publication have both been acknowledged.
    let receipt = std::mem::take(&mut counts.applied_rows);
    counts.source_action = Some(match receipt.as_slice() {
        [Some(applied)] if applied.len() == source.plan().len() => {
            let written = applied.iter().filter(|&&row| row).count();
            if written as u64 == counts.rows && (applied.len() - written) as u64 == counts.skipped {
                source.execute(applied, cancel)
            } else {
                Err("目标端逐行回执与导入统计不符。".into())
            }
        }
        _ => Err("目标端逐行回执不完整。".into()),
    });
    Ok(Response::OdbcImported { counts, created_in })
}

fn source_guard_sql(identity: &[String; 5], source: (&str, &str), target: &DbObject) -> String {
    let identifier_match = |left: &str, right: &str| {
        let left = mysql_text_literal(left);
        let right = mysql_text_literal(right);
        format!(
            "CAST(IF(@@lower_case_table_names = 0, {left}, LOWER({left})) AS BINARY)
             <=> CAST(IF(@@lower_case_table_names = 0, {right}, LOWER({right})) AS BINARY)"
        )
    };
    // Use the same fixed field order as the source ODBC adapter. Compare bytes
    // so the target connection's collation cannot change server identity.
    let server_match = SOURCE_SERVER_ID_SQL
        .strip_prefix("SELECT ")
        .expect("fixed server identity field list")
        .split(", ")
        .zip(identity)
        .map(|(field, value)| {
            format!(
                "CAST({field} AS BINARY) <=> {}",
                mysql_binary_literal(value.as_bytes())
            )
        })
        .collect::<Vec<_>>()
        .join(" AND ");
    let same_schema = identifier_match(source.0, &target.schema);
    let same_table = identifier_match(source.1, &target.name);
    format!("NOT (({same_schema}) AND ({same_table}) AND ({server_match}))")
}

#[cfg(test)]
mod tests {
    use mysql::{Conn, prelude::Queryable};

    use super::*;
    use crate::database::{ObjectKind, http_session::workflow_test::fixture_database};

    #[test]
    #[ignore = "requires a disposable MariaDB via HARBOR_HTTP_FIXTURE_PORT"]
    fn permits_different_source_and_target_tables_on_the_same_server() {
        let (_, options) = fixture_database();
        let mut conn = Conn::new(options).unwrap();
        let (host, port, server_id, version, datadir): (String, String, String, String, String) =
            conn.query_first(SOURCE_SERVER_ID_SQL).unwrap().unwrap();
        let identity = [host, port, server_id, version, datadir];
        let target = DbObject {
            schema: "warehouse".into(),
            name: "receiving".into(),
            kind: ObjectKind::Table,
        };
        let guard = source_guard_sql(&identity, ("warehouse", "outbound"), &target);
        assert_eq!(
            conn.query_first::<u8, _>(format!("SELECT ({guard})"))
                .unwrap(),
            Some(1)
        );
    }
}
