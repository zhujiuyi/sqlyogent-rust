use mysql::{Params, Row, Transaction, Value, prelude::Queryable};

use super::http::fingerprint_expression;
use super::{TableLayout, TableRow, check_write_warnings, mysql_error};

pub(super) fn lock_matches(
    transaction: &mut Transaction<'_>,
    layout: &TableLayout,
    qualified: &str,
    original: &TableRow,
) -> Result<(String, Vec<Value>, u64), String> {
    layout.recheck_keyless(original)?;
    if original.original().len() != layout.columns.len() {
        return Err("未读取该行的全部原值，请刷新后重试。".into());
    }
    let fingerprint = original
        .fingerprint
        .as_ref()
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or("未读取该行的完整原值校验值，请刷新后重试。")?;

    // Older InnoDB versions can turn off the gap locks required to hold the match set.
    let unsafe_locks = transaction
        .query_first::<(String, String), _>(
            "SHOW GLOBAL VARIABLES WHERE Variable_name = 'innodb_locks_unsafe_for_binlog'",
        )
        .map_err(mysql_error)?;
    if unsafe_locks.is_some_and(|(_, value)| !value.eq_ignore_ascii_case("OFF") && value != "0") {
        return Err("当前服务器禁用了无键编辑需要的间隙锁，仅可浏览此表。".into());
    }

    let predicate = format!(
        "BINARY {} <=> CAST(? AS BINARY)",
        fingerprint_expression(layout, None)
    );
    let parameters = vec![Value::Bytes(fingerprint.as_bytes().to_vec())];
    let sql = format!(
        "SELECT {} FROM {qualified} WHERE {predicate} FOR UPDATE",
        layout.projection()
    );
    let mut matches = 0_u64;
    {
        // The non-indexable fingerprint scan under REPEATABLE READ locks the
        // matching records and guards their range against concurrent inserts.
        let mut current = transaction
            .exec_iter(sql, Params::Positional(parameters.clone()))
            .map_err(mysql_error)?;
        for row in &mut current {
            let row: Row = row.map_err(mysql_error)?;
            if !same_values(&row.unwrap(), original.original()) {
                return Err("服务器返回的记录与全部原值不一致，请刷新后重试。".into());
            }
            matches = matches.checked_add(1).ok_or("匹配行数超过上限。")?;
        }
    }
    check_write_warnings(transaction)
        .map_err(|error| format!("核对整行原值时服务器返回警告。{error}"))?;
    if matches == 0 {
        return Err("此行已被其他操作修改或删除，请刷新后重试。".into());
    }
    Ok((predicate, parameters, matches))
}

pub(super) fn same_values(current: &[Value], original: &[Value]) -> bool {
    current.len() == original.len()
        && current
            .iter()
            .zip(original)
            .all(|(current, original)| same_value(current, original))
}

pub(super) fn same_value(current: &Value, original: &Value) -> bool {
    match (current, original) {
        (Value::Float(current), Value::Float(original)) => current.to_bits() == original.to_bits(),
        (Value::Double(current), Value::Double(original)) => {
            current.to_bits() == original.to_bits()
        }
        _ => current == original,
    }
}

#[cfg(test)]
mod tests {
    use mysql::{Conn, Opts, OptsBuilder};

    use super::super::{
        DuplicateChoice, DuplicateScope, FieldValue, TableEdit, TableEditOutcome, TablePage,
        edit_table_with_confirmation, table_page,
    };
    use crate::database::{Cell, DbObject, ObjectKind};
    use mysql::prelude::Queryable;

    struct Fixture {
        client: Conn,
        schema: String,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self
                .client
                .query_drop(format!("DROP DATABASE IF EXISTS `{}`", self.schema));
        }
    }

    #[test]
    #[ignore = "requires a disposable MariaDB via HARBOR_EDITABLE_FIXTURE_PORT"]
    fn reviews_duplicate_full_rows_before_updating_one_and_deleting_all() {
        let port = std::env::var("HARBOR_EDITABLE_FIXTURE_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let opts: Opts = OptsBuilder::default()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(port)
            .user(Some("root"))
            .prefer_socket(false)
            .into();
        let mut fixture = Fixture {
            client: Conn::new(opts.clone()).unwrap(),
            schema: format!("harbor_keyless_native_{}", std::process::id()),
        };
        fixture
            .client
            .query_drop(format!("CREATE DATABASE `{}`", fixture.schema))
            .unwrap();
        let object = DbObject {
            schema: fixture.schema.clone(),
            name: "stock".into(),
            kind: ObjectKind::Table,
        };
        let table = object.qualified_name();
        fixture
            .client
            .query_drop(format!(
                "CREATE TABLE {table} (
                    label VARCHAR(12) CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci,
                    payload VARBINARY(10), note TEXT, quantity INT NOT NULL
                ) ENGINE=InnoDB"
            ))
            .unwrap();
        fixture
            .client
            .query_drop(format!(
                "INSERT INTO {table} VALUES
                 ('样品A',0x00ff,NULL,5), ('样品A',0x00ff,NULL,5), ('样品A',0x00ff,NULL,5),
                 ('样品a',0x00ff,NULL,5), ('样品A',0x00fe,NULL,5)"
            ))
            .unwrap();
        let duplicate = |page: TablePage| {
            assert!(page.edit_error.is_none());
            assert!(page.row_key.is_empty());
            page.rows
                .into_iter()
                .find(|row| {
                    matches!(&row.cells[..],
                        [Cell::Text(label), Cell::Binary(payload), Cell::Null, Cell::Text(quantity)]
                        if label == "样品A" && payload == &[0, 255] && quantity == "5")
                })
                .unwrap()
        };
        let original = duplicate(table_page(&opts, object.clone(), 20, 0).unwrap());
        let edit = TableEdit::Update {
            original,
            values: vec![("quantity".into(), FieldValue::Text("8".into()))],
        };
        let apply = |edit, choice| {
            edit_table_with_confirmation(&opts, object.clone(), edit, choice).unwrap()
        };
        assert_eq!(
            apply(edit.clone(), None),
            TableEditOutcome::NeedsConfirmation { matches: 3 }
        );
        assert_eq!(
            fixture
                .client
                .query_first::<u64, _>(format!("SELECT COUNT(*) FROM {table} WHERE quantity=8"))
                .unwrap(),
            Some(0)
        );
        assert_eq!(
            apply(
                edit,
                Some(DuplicateChoice {
                    matches: 3,
                    scope: DuplicateScope::One
                })
            ),
            TableEditOutcome::Applied(1)
        );

        let original = duplicate(table_page(&opts, object.clone(), 20, 0).unwrap());
        let delete = TableEdit::Delete { original };
        assert_eq!(
            apply(delete.clone(), None),
            TableEditOutcome::NeedsConfirmation { matches: 2 }
        );
        assert_eq!(
            apply(
                delete,
                Some(DuplicateChoice {
                    matches: 2,
                    scope: DuplicateScope::All
                })
            ),
            TableEditOutcome::Applied(2)
        );
        let remaining: Vec<(String, Vec<u8>, i64)> = fixture
            .client
            .query(format!(
                "SELECT label, payload, quantity FROM {table} ORDER BY BINARY label, payload"
            ))
            .unwrap();
        assert_eq!(
            remaining,
            [
                ("样品A".into(), vec![0, 254], 5),
                ("样品A".into(), vec![0, 255], 8),
                ("样品a".into(), vec![0, 255], 5),
            ]
        );
    }
}
