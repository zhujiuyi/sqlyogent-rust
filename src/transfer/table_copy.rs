use std::hash::{BuildHasher, RandomState};

use mysql::TxOpts;
use sha2::{Digest, Sha256};

use crate::sql_tools::{EditorTokenKind, editor_tokens};

use super::super::sync_engine::stable_definition;
use super::{
    Conn, DbObject, DumpSource, Failure, HttpSession, ObjectKind, Queryable, TransferSummary,
    comparable_name, http_session, mysql_error, mysql_text_literal, prepare_source,
    quote_identifier, setting_literal,
};

#[derive(Clone, Debug)]
pub struct TableCopySettings {
    pub schema: String,
    pub name: String,
    pub columns: Option<Vec<String>>,
    pub include_data: bool,
    pub include_indexes: bool,
    pub include_triggers: bool,
}

pub(in crate::database) fn copy_table_as(
    conn: &mut Conn,
    object: &DbObject,
    settings: &TableCopySettings,
) -> Result<TransferSummary, String> {
    prepare_source(conn, &object.schema)?;
    copy(&mut DumpSource::Native(conn), object, settings)
}

pub(in crate::database) fn copy_table_as_over_http(
    session: &HttpSession,
    object: &DbObject,
    settings: &TableCopySettings,
) -> Result<TransferSummary, String> {
    copy(&mut DumpSource::Http(session), object, settings)
}

fn copy(
    connection: &mut DumpSource<'_>,
    object: &DbObject,
    settings: &TableCopySettings,
) -> Result<TransferSummary, String> {
    if object.kind != ObjectKind::Table {
        return Err("只有基表可以复制为新表。".into());
    }
    for value in [&settings.schema, &settings.name] {
        if value.trim().is_empty() || value.chars().count() > 64 || value.contains('\0') {
            return Err("目标库和表名不能为空，且各自最多包含 64 个字符。".into());
        }
    }

    let (mode, case_mode) = connection.sql_mode()?;
    let same = |left: &str, right: &str| {
        comparable_name(left, case_mode) == comparable_name(right, case_mode)
    };
    if same(&settings.schema, &object.schema) && same(&settings.name, &object.name) {
        return Err("目标表名与源表相同。".into());
    }
    if settings.include_triggers && !same(&settings.schema, &object.schema) {
        return Err("复制触发器时，目标表需与源表位于同一个数据库。".into());
    }
    let original = object.qualified_name();
    let target_table = DbObject {
        schema: settings.schema.clone(),
        name: settings.name.clone(),
        kind: ObjectKind::Table,
    };
    let target = target_table.qualified_name();
    let target_scope = table_scope(&target_table, "TABLE_SCHEMA", "TABLE_NAME");
    if !connection
        .copy_metadata::<1>(&format!(
            "SELECT 1 FROM information_schema.TABLES WHERE {target_scope} LIMIT 1"
        ))?
        .is_empty()
    {
        return Err(format!("目标 {target} 已存在，请指定另一个表名。"));
    }
    let staging = DbObject {
        schema: settings.schema.clone(),
        name: format!("_harbor_copy_{:016x}", RandomState::new().hash_one(&target)),
        kind: ObjectKind::Table,
    };
    let stage = staging.qualified_name();
    let source_scope = table_scope(object, "TABLE_SCHEMA", "TABLE_NAME");
    let [engine] = connection
        .copy_metadata::<1>(&format!(
            "SELECT ENGINE FROM information_schema.TABLES WHERE {source_scope} AND TABLE_TYPE = 'BASE TABLE'"
        ))?
        .pop()
        .ok_or("找不到可复制的源表。")?;
    if engine.as_deref().is_some_and(|value| {
        matches!(
            value.to_ascii_uppercase().as_str(),
            "CONNECT" | "FEDERATED" | "FEDERATEDX" | "SPIDER" | "S3" | "MRG_MYISAM"
        )
    }) {
        return Err("此表的存储引擎使用外部数据，无法创建独立副本。".into());
    }
    let allows_grouped_auto_increment = engine.as_deref().is_some_and(|value| {
        value.eq_ignore_ascii_case("MyISAM") || value.eq_ignore_ascii_case("Aria")
    });
    let original_ddl = stable_definition(connection.definition(object)?.ddl);
    connection
        .copy_commands(
            &settings.schema,
            &[&format!("CREATE TABLE {stage} LIKE {original}")],
        )
        .map_err(|error| format!("无法确认中间表 {stage} 是否已创建。{error}"))?;

    let result = (|| {
        let fields = connection.copy_metadata::<2>(&format!(
            "SELECT COLUMN_NAME, EXTRA FROM information_schema.COLUMNS WHERE {}
             ORDER BY ORDINAL_POSITION",
            table_scope(&staging, "TABLE_SCHEMA", "TABLE_NAME")
        ))?;
        let fields = required_rows(fields)?;
        let kept = |name: &str| {
            settings.columns.as_ref().is_none_or(|columns| {
                columns
                    .iter()
                    .any(|column| column.eq_ignore_ascii_case(name) || column == name)
            })
        };
        if fields.is_empty()
            || settings.columns.as_ref().is_some_and(|columns| {
                columns.is_empty()
                    || columns.iter().any(|name| {
                        !fields
                            .iter()
                            .any(|[column, _]| column.eq_ignore_ascii_case(name) || column == name)
                    })
            })
        {
            return Err("选择的字段不完整，请重新读取源表字段。".into());
        }

        let indexes = connection.copy_metadata::<3>(&format!(
            "SELECT INDEX_NAME, CAST(SEQ_IN_INDEX AS CHAR), COLUMN_NAME
               FROM information_schema.STATISTICS WHERE {}
               ORDER BY BINARY INDEX_NAME, SEQ_IN_INDEX",
            table_scope(&staging, "TABLE_SCHEMA", "TABLE_NAME")
        ))?;
        let all_fields = fields.iter().all(|[name, _]| kept(name));
        let mut removed_indexes = Vec::new();
        for [index, _, column] in &indexes {
            let index = index.as_ref().ok_or("服务器未返回索引名。")?;
            let remove = !settings.include_indexes || !column.as_deref().map_or(all_fields, kept);
            if remove && !removed_indexes.contains(index) {
                removed_indexes.push(index.clone());
            }
        }
        let mut changes = removed_indexes
            .iter()
            .map(|index| {
                if index == "PRIMARY" {
                    "DROP PRIMARY KEY".into()
                } else {
                    format!("DROP INDEX {}", quote_identifier(index))
                }
            })
            .collect::<Vec<_>>();
        for [name, extra] in &fields {
            let remains_indexed = indexes.iter().any(|[index, position, column]| {
                (position.as_deref() == Some("1") || allows_grouped_auto_increment)
                    && column.as_deref() == Some(name.as_str())
                    && index
                        .as_ref()
                        .is_some_and(|index| !removed_indexes.contains(index))
            });
            if !kept(name) {
                changes.push(format!("DROP COLUMN {}", quote_identifier(name)));
            } else if extra.to_ascii_lowercase().contains("auto_increment") && !remains_indexed {
                let ddl = connection.definition(&staging)?.ddl;
                let plain_column = without_auto_increment(&ddl, name, &mode)?;
                changes.push(format!("MODIFY COLUMN {plain_column}"));
            }
        }
        if !changes.is_empty() {
            connection.copy_commands(
                &settings.schema,
                &[&format!("ALTER TABLE {stage} {}", changes.join(", "))],
            )?;
        }

        let columns = fields
            .iter()
            .filter_map(|[column, extra]| {
                let generated = extra
                    .to_ascii_uppercase()
                    .split_ascii_whitespace()
                    .any(|word| matches!(word, "VIRTUAL" | "STORED" | "PERSISTENT" | "GENERATED"));
                (kept(column) && !generated).then(|| quote_identifier(column))
            })
            .collect::<Vec<_>>();
        let rows = if settings.include_data {
            if columns.is_empty() {
                return Err("复制数据需要至少保留一个可写字段。".into());
            }
            let columns = columns.join(", ");
            let sql = format!("INSERT INTO {stage} ({columns}) SELECT {columns} FROM {original}");
            copy_data(
                connection,
                object,
                &settings.schema,
                &original_ddl,
                &mode,
                &sql,
            )?
        } else {
            0
        };

        if settings.include_triggers {
            let trigger_scope = table_scope(object, "EVENT_OBJECT_SCHEMA", "EVENT_OBJECT_TABLE");
            let triggers = connection.copy_metadata::<6>(&format!(
                "SELECT TRIGGER_NAME, ACTION_TIMING, EVENT_MANIPULATION, ACTION_STATEMENT, SQL_MODE, COLLATION_CONNECTION
                 FROM information_schema.TRIGGERS WHERE {trigger_scope}
                 ORDER BY ACTION_TIMING, EVENT_MANIPULATION, ACTION_ORDER"
            ))?;
            for [name, timing, event, body, mode, collation] in required_rows(triggers)? {
                if !matches!(timing.as_str(), "BEFORE" | "AFTER")
                    || !matches!(event.as_str(), "INSERT" | "UPDATE" | "DELETE")
                {
                    return Err(format!("触发器 {name} 的触发条件不受支持。"));
                }
                let sql_mode = format!("SET SESSION sql_mode = {}", setting_literal(&mode)?);
                let collation = format!(
                    "SET SESSION collation_connection = {}",
                    setting_literal(&collation)?
                );
                let name = qualified(&settings.schema, &derived_name(&name, &settings.name));
                let create = format!(
                    "CREATE TRIGGER {name} {timing} {event} ON {stage} FOR EACH ROW {body}"
                );
                connection.copy_commands(&settings.schema, &[&sql_mode, &collation, &create])?;
            }
        }

        connection.copy_commands(
            &settings.schema,
            &[&format!("RENAME TABLE {stage} TO {target}")],
        )?;
        Ok(TransferSummary {
            objects: 1,
            rows,
            ..TransferSummary::default()
        })
    })();
    result.map_err(|error| {
        match connection.copy_commands(
            &settings.schema,
            &[&format!("DROP TABLE IF EXISTS {stage}")],
        ) {
            Ok(_) => format!("无法确认 {target} 已复制。{error}"),
            Err(cleanup) => {
                format!("无法确认 {target} 已复制。{error}\n中间表 {stage} 清理失败：{cleanup}")
            }
        }
    })
}

fn copy_data(
    connection: &mut DumpSource<'_>,
    object: &DbObject,
    schema: &str,
    expected: &str,
    mode: &str,
    sql: &str,
) -> Result<u64, String> {
    let mode = format!(
        "SET SESSION sql_mode = {}",
        setting_literal(
            &if mode.split(',').any(|item| item == "NO_AUTO_VALUE_ON_ZERO") {
                mode.to_owned()
            } else {
                [mode, "NO_AUTO_VALUE_ON_ZERO"]
                    .into_iter()
                    .filter(|value| !value.is_empty())
                    .collect::<Vec<_>>()
                    .join(",")
            }
        )?
    );
    let original = object.qualified_name();
    let warning = "源表数据无法完整写入，请检查服务器返回的写入警告。";
    match connection {
        DumpSource::Native(conn) => {
            conn.query_drop(mode).map_err(mysql_error)?;
            let mut txn = conn
                .start_transaction(TxOpts::default())
                .map_err(mysql_error)?;
            txn.query_drop(format!("SELECT * FROM {original} LIMIT 0"))
                .map_err(mysql_error)?;
            let (_, ddl): (String, String) = txn
                .query_first_opt(format!("SHOW CREATE TABLE {original}"))
                .map_err(mysql_error)?
                .transpose()
                .map_err(|error| mysql_error(error.into()))?
                .ok_or("无法重新读取源表结构。")?;
            if stable_definition(ddl) != expected {
                return Err("复制期间源表结构发生变化，请重新读取字段后复制。".into());
            }
            txn.query_drop(sql).map_err(mysql_error)?;
            let rows = txn.affected_rows();
            let warning_count = txn
                .query_first::<u64, _>("SELECT @@warning_count")
                .map_err(mysql_error)?;
            if warning_count != Some(0) {
                return Err(warning.into());
            }
            txn.commit().map_err(mysql_error)?;
            Ok(rows)
        }
        DumpSource::Http(session) => {
            if stable_definition(DumpSource::Http(session).definition(object)?.ddl) != expected {
                return Err("复制期间源表结构发生变化，请重新读取字段后复制。".into());
            }
            let reply = http_commands(
                session,
                schema,
                &[&mode, sql, "SELECT ROW_COUNT(), @@warning_count"],
            )?;
            let values = reply
                .sets
                .first()
                .and_then(|set| set.rows.first())
                .ok_or(warning)?;
            let rows = values
                .first()
                .and_then(Option::as_deref)
                .and_then(|value| std::str::from_utf8(value).ok())
                .and_then(|value| value.parse().ok())
                .ok_or(warning)?;
            if values.get(1).and_then(Option::as_deref) != Some(b"0".as_slice()) {
                return Err(warning.into());
            }
            Ok(rows)
        }
    }
}

impl DumpSource<'_> {
    fn copy_commands(&mut self, schema: &str, commands: &[&str]) -> Result<(), String> {
        match self {
            Self::Native(conn) => commands
                .iter()
                .try_for_each(|sql| conn.query_drop(*sql).map_err(mysql_error)),
            Self::Http(session) => http_commands(session, schema, commands).map(|_| ()),
        }
    }

    fn copy_metadata<const N: usize>(
        &mut self,
        sql: &str,
    ) -> Result<Vec<[Option<String>; N]>, String> {
        match self {
            Self::Native(conn) => conn
                .query_iter(sql)
                .map_err(mysql_error)?
                .enumerate()
                .map(|(index, row)| {
                    if index == 8192 {
                        return Err("表定义的元数据条目超过读取上限。".into());
                    }
                    row.map_err(mysql_error)?
                        .unwrap()
                        .into_iter()
                        .map(|value| {
                            mysql::from_value_opt::<Option<String>>(value)
                                .map_err(|error| mysql_error(error.into()))
                        })
                        .collect::<Result<Vec<_>, _>>()?
                        .try_into()
                        .map_err(|_| "表定义元数据列数不符。".into())
                })
                .collect(),
            Self::Http(session) => session
                .metadata_columns(sql, 8192)
                .map_err(Failure::message),
        }
    }
}

fn http_commands(
    session: &HttpSession,
    schema: &str,
    commands: &[&str],
) -> Result<crate::http_tunnel::Reply, String> {
    let batch = commands.len() > 1;
    let body = if batch {
        http_session::batch_statements(commands)?
    } else {
        commands[0].to_owned()
    };
    let reply = session.execute_payload(session.prepare_payload(&body, batch, schema)?, 1)?;
    if let Some(error) = &reply.error {
        return Err(format!("ERROR {}: {}", error.code, error.message));
    }
    Ok(reply)
}

fn required_rows<const N: usize>(
    rows: Vec<[Option<String>; N]>,
) -> Result<Vec<[String; N]>, String> {
    rows.into_iter()
        .map(|row| {
            let values = row
                .into_iter()
                .collect::<Option<Vec<_>>>()
                .ok_or("服务器未返回完整的表定义元数据。")?;
            Ok(values.try_into().expect("column count retained"))
        })
        .collect()
}

fn table_scope(object: &DbObject, schema: &str, table: &str) -> String {
    let database = mysql_text_literal(&object.schema);
    let name = mysql_text_literal(&object.name);
    format!(
        "{schema} = {database} AND {table} = {name}
         AND (@@lower_case_table_names <> 0 OR (BINARY {schema} = BINARY {database} AND BINARY {table} = BINARY {name}))"
    )
}

fn qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_identifier(schema), quote_identifier(name))
}

fn derived_name(original: &str, target: &str) -> String {
    let name = format!("{original}_{target}");
    if name.chars().count() <= 64 {
        return name;
    }
    let hash = format!("{:x}", Sha256::digest(&name));
    format!(
        "{}_{}",
        name.chars().take(47).collect::<String>(),
        &hash[..16]
    )
}

fn without_auto_increment(ddl: &str, name: &str, mode: &str) -> Result<String, String> {
    let mode_has = |needle| {
        mode.split(',')
            .any(|part| part.eq_ignore_ascii_case(needle))
    };
    let mut tokens = editor_tokens(
        ddl,
        mode_has("NO_BACKSLASH_ESCAPES"),
        mode_has("ANSI_QUOTES"),
    )
    .skip_while(|token| token.kind != EditorTokenKind::Symbol('('));
    tokens.next();
    let mut start = None;
    let mut matching = false;
    let mut depth = 0;
    let mut attribute = None;
    let quoted = quote_identifier(name);
    let ansi = format!("\"{}\"", name.replace('"', "\"\""));
    for token in tokens {
        if start.is_none() {
            matching = token.text == name || token.text == quoted || token.text == ansi;
            start = Some(token.range.start);
        }
        if depth == 0 && matches!(token.kind, EditorTokenKind::Symbol(',' | ')')) {
            if matching && let Some(range) = attribute {
                let start = start.unwrap_or_default();
                let mut result = ddl[start..token.range.start].to_owned();
                result.replace_range(range, "");
                return Ok(result.trim_end().to_owned());
            }
            if token.kind == EditorTokenKind::Symbol(')') {
                break;
            }
            start = None;
            attribute = None;
            continue;
        }
        match token.kind {
            EditorTokenKind::Symbol('(') => depth += 1,
            EditorTokenKind::Symbol(')') => depth -= 1,
            EditorTokenKind::Word
                if depth == 0 && matching && token.text.eq_ignore_ascii_case("AUTO_INCREMENT") =>
            {
                let start = start.unwrap_or_default();
                attribute = Some(token.range.start - start..token.range.end - start);
            }
            _ => {}
        }
    }
    Err(format!("无法从字段 {name} 中保留定义并移除自增属性。"))
}

#[cfg(test)]
mod tests {
    use crate::database::{
        Command, ConnectionConfig, Response, Worker,
        http_session::workflow_test::{TestGateway, archived_tunnel, fixture_database},
    };

    use super::*;

    struct Cleanup(Conn, String);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self
                .0
                .query_drop(format!("DROP DATABASE {}", quote_identifier(&self.1)));
        }
    }

    #[test]
    #[ignore = "requires disposable MariaDB via HARBOR_HTTP_FIXTURE_PORT"]
    fn copies_selected_columns_with_indexes_and_trigger() {
        let (port, opts) = fixture_database();
        let schema = format!("harbor_http_selective_copy_{}", std::process::id());
        let mut connection = Conn::new(opts.clone()).unwrap();
        connection
            .query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
            .unwrap();
        let mut db = Cleanup(connection, schema.clone());
        db.0.select_db(&schema).unwrap();
        for sql in [
            "CREATE TABLE parent (id INT PRIMARY KEY) ENGINE=InnoDB",
            "INSERT INTO parent VALUES (3)",
            "CREATE TABLE source_table (
                id INT NOT NULL AUTO_INCREMENT PRIMARY KEY,
                parent_id INT NOT NULL,
                label VARCHAR(40) NOT NULL,
                payload VARBINARY(16),
                omitted VARCHAR(24),
                payload_length INT AS (OCTET_LENGTH(payload)) STORED,
                KEY label_prefix (label(8)),
                KEY depends_on_omitted (omitted,parent_id),
                CONSTRAINT source_parent FOREIGN KEY (parent_id)
                  REFERENCES parent (id) ON DELETE CASCADE
            ) ENGINE=InnoDB",
            "INSERT INTO source_table (parent_id,label,payload,omitted)
                   VALUES (3, '源字段', X'0041FF', '不复制')",
            "CREATE TRIGGER add_prefix BEFORE INSERT ON source_table FOR EACH ROW
                BEGIN SET NEW.label = CONCAT('复制:', NEW.label); END",
        ] {
            db.0.query_drop(sql).unwrap();
        }

        let archived = archived_tunnel();
        let gateway = archived.is_none().then(|| TestGateway::new(opts));
        let tunnel = archived.unwrap_or_else(|| gateway.as_ref().unwrap().tunnel.clone());
        let config = ConnectionConfig {
            port,
            database: Some(schema.clone()),
            ..Default::default()
        };
        for (suffix, connect) in [
            ("native", Command::Connect(config.clone())),
            ("http", Command::ConnectHttp(config, tunnel)),
        ] {
            let worker = Worker::new();
            let request = |id, command| {
                worker.send(id, command).unwrap();
                let event = worker
                    .recv_timeout(std::time::Duration::from_secs(20))
                    .unwrap()
                    .unwrap();
                assert_eq!(event.id, id);
                event
                    .result
                    .unwrap_or_else(|error| panic!("{suffix}: {error}"))
            };
            assert!(matches!(request(1, connect), Response::Connected(_)));
            let object = DbObject {
                schema: schema.clone(),
                name: "source_table".into(),
                kind: ObjectKind::Table,
            };
            let name = format!("copy_{suffix}");
            let Response::Transferred(summary) = request(
                2,
                Command::CopyTableAs {
                    object: object.clone(),
                    settings: TableCopySettings {
                        schema: schema.clone(),
                        name: name.clone(),
                        columns: Some(
                            ["label", "id", "parent_id", "payload", "payload_length"]
                                .map(str::to_owned)
                                .to_vec(),
                        ),
                        include_data: true,
                        include_indexes: true,
                        include_triggers: true,
                    },
                },
            ) else {
                panic!("expected copy completion")
            };
            assert_eq!((summary.objects, summary.rows), (1, 1));
            let names: Vec<String> = db.0.query(format!(
                "SELECT COLUMN_NAME FROM information_schema.COLUMNS
                     WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = '{name}' ORDER BY ORDINAL_POSITION"
            )).unwrap();
            assert_eq!(
                names,
                ["id", "parent_id", "label", "payload", "payload_length"]
            );
            let row: (u32, String, String, u32) =
                db.0.query_first(format!(
                    "SELECT id,label,HEX(payload),payload_length FROM {name}"
                ))
                .unwrap()
                .unwrap();
            assert_eq!(row, (1, "源字段".into(), "0041FF".into(), 3));
            let indexes: Vec<(String, Option<u64>)> =
                db.0.query(format!(
                    "SELECT INDEX_NAME, SUB_PART FROM information_schema.STATISTICS
                    WHERE TABLE_SCHEMA=DATABASE() AND TABLE_NAME='{name}'"
                ))
                .unwrap();
            assert!(indexes.contains(&("PRIMARY".into(), None)));
            assert!(indexes.contains(&("label_prefix".into(), Some(8))));
            assert!(indexes.contains(&("source_parent".into(), None)));
            assert!(
                !indexes
                    .iter()
                    .any(|(index, _)| index == "depends_on_omitted")
            );
            let key: Option<(String, String)> = db.0.query_first(format!(
                "SELECT REFERENCED_TABLE_NAME, DELETE_RULE FROM information_schema.REFERENTIAL_CONSTRAINTS
                    WHERE CONSTRAINT_SCHEMA=DATABASE() AND TABLE_NAME='{name}'"
            )).unwrap();
            assert!(key.is_none());
            let trigger: Option<String> =
                db.0.query_first(format!(
                    "SELECT TRIGGER_NAME FROM information_schema.TRIGGERS
                    WHERE EVENT_OBJECT_SCHEMA=DATABASE() AND EVENT_OBJECT_TABLE='{name}'"
                ))
                .unwrap();
            assert_eq!(
                trigger.as_deref(),
                Some(format!("add_prefix_{name}").as_str())
            );
            db.0.query_drop(format!(
                "INSERT INTO {name} (parent_id,label) VALUES (3, '新记录')"
            ))
            .unwrap();
            assert_eq!(
                db.0.query_first::<String, _>(format!("SELECT label FROM {name} WHERE id=2"))
                    .unwrap()
                    .as_deref(),
                Some("复制:新记录")
            );

            let empty = format!("empty_{suffix}");
            let Response::Transferred(summary) = request(
                3,
                Command::CopyTableAs {
                    object,
                    settings: TableCopySettings {
                        schema: schema.clone(),
                        name: empty.clone(),
                        columns: Some(vec!["id".into(), "label".into()]),
                        include_data: false,
                        include_indexes: false,
                        include_triggers: false,
                    },
                },
            ) else {
                panic!("expected structure copy completion")
            };
            assert_eq!(summary.rows, 0);
            assert_eq!(
                db.0.query_first::<u64, _>(format!("SELECT COUNT(*) FROM {empty}"))
                    .unwrap(),
                Some(0)
            );
            let id: Option<(String, String)> =
                db.0.query_first(format!(
                    "SELECT EXTRA, COLUMN_KEY FROM information_schema.COLUMNS
                    WHERE TABLE_SCHEMA=DATABASE() AND TABLE_NAME='{empty}' AND COLUMN_NAME='id'"
                ))
                .unwrap();
            assert_eq!(id, Some((String::new(), String::new())));
        }
        if let Some(gateway) = gateway {
            gateway.stop();
        }
    }

    #[test]
    #[ignore = "requires disposable MariaDB via HARBOR_HTTP_FIXTURE_PORT"]
    fn keeps_grouped_auto_increment_with_its_index() {
        let (_, opts) = fixture_database();
        let schema = format!("harbor_grouped_copy_{}", std::process::id());
        let mut connection = Conn::new(opts).unwrap();
        connection
            .query_drop(format!("CREATE DATABASE {schema}"))
            .unwrap();
        let mut db = Cleanup(connection, schema.clone());
        db.0.select_db(&schema).unwrap();
        for engine in ["MyISAM", "Aria"] {
            let source = format!("source_{engine}");
            let target = format!("copy_{engine}");
            db.0.query_drop(format!(
                "CREATE TABLE {source} (group_id INT NOT NULL, id INT NOT NULL AUTO_INCREMENT,
                    omitted INT, PRIMARY KEY (group_id, id), KEY ignored_index (omitted)) ENGINE={engine}"
            )).unwrap();
            db.0.query_drop(format!(
                "INSERT INTO {source} (group_id) VALUES (1), (1), (2)"
            ))
            .unwrap();
            let object = DbObject {
                schema: schema.clone(),
                name: source,
                kind: ObjectKind::Table,
            };
            let settings = TableCopySettings {
                schema: schema.clone(),
                name: target.clone(),
                columns: Some(vec!["group_id".into(), "id".into()]),
                include_data: true,
                include_indexes: true,
                include_triggers: false,
            };
            assert_eq!(
                copy_table_as(&mut db.0, &object, &settings).unwrap().rows,
                3
            );
            db.0.query_drop(format!("INSERT INTO {target} (group_id) VALUES (1), (2)"))
                .unwrap();
            let rows: Vec<(u32, u32)> =
                db.0.query(format!(
                    "SELECT group_id, id FROM {target} ORDER BY group_id, id"
                ))
                .unwrap();
            assert_eq!(rows, [(1, 1), (1, 2), (1, 3), (2, 1), (2, 2)], "{engine}");
        }
    }
}
