use std::time::{SystemTime, UNIX_EPOCH};

use mysql::{Conn, OptsBuilder, prelude::Queryable};

use super::{
    DbObject, ObjectKind, describe_schema_design, quote_identifier, snapshot_objects,
    snapshot_schema,
};

#[test]
#[ignore = "在一次性本机数据库中运行，需要 HARBOR_DESIGN_TEST_PORT"]
fn reads_schema_cards_and_foreign_keys_in_column_order() {
    let port = std::env::var("HARBOR_DESIGN_TEST_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let mut conn = Conn::new(
        OptsBuilder::default()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(port)
            .user(Some("root"))
            .prefer_socket(false),
    )
    .unwrap();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let schema = format!("harbor_design_{stamp:x}");
    let db = quote_identifier(&schema);
    let external = format!("{schema}_external");
    let external_db = quote_identifier(&external);
    conn.query_drop(format!("CREATE DATABASE {db} CHARACTER SET utf8mb4"))
        .unwrap();
    let result = (|| -> Result<_, String> {
        conn.query_drop(format!(
            "CREATE TABLE {db}.catalog (region INT NOT NULL, code INT NOT NULL, PRIMARY KEY(region, code)) ENGINE=InnoDB"
        )).map_err(|error| error.to_string())?;
        conn.query_drop(format!(
            "CREATE TABLE {db}.orders (id INT PRIMARY KEY, area INT NOT NULL, item INT NOT NULL, CONSTRAINT fk_item FOREIGN KEY(area, item) REFERENCES {db}.catalog(region, code) ON DELETE CASCADE ON UPDATE RESTRICT) ENGINE=InnoDB"
        )).map_err(|error| error.to_string())?;
        let model =
            describe_schema_design(&mut conn, schema.clone()).map_err(|error| error.message())?;
        let selected = snapshot_objects(
            &mut conn,
            schema.clone(),
            vec![DbObject {
                schema: schema.clone(),
                name: "orders".into(),
                kind: ObjectKind::Table,
            }],
        )
        .map_err(|error| error.message())?;
        for statement in [
            format!("CREATE DATABASE {external_db} CHARACTER SET utf8mb4"),
            format!(
                "CREATE TABLE {external_db}.reference(region INT, code INT, CONSTRAINT fk_external FOREIGN KEY(region, code) REFERENCES {db}.catalog(region, code)) ENGINE=InnoDB"
            ),
            "SET SESSION time_zone = '+03:00'".to_owned(),
            format!(
                "CREATE EVENT {db}.sample_event ON SCHEDULE AT CURRENT_TIMESTAMP + INTERVAL 1 HOUR DISABLE DO SELECT 1"
            ),
            format!(
                "CREATE TRIGGER {db}.sample_trigger BEFORE INSERT ON {db}.orders FOR EACH ROW SET @sample = NEW.id"
            ),
        ] {
            conn.query_drop(statement)
                .map_err(|error| error.to_string())?;
        }
        let full = snapshot_schema(&mut conn, schema.clone()).map_err(|error| error.message())?;
        Ok((model, selected, full))
    })();
    conn.query_drop(format!("DROP DATABASE IF EXISTS {external_db}"))
        .unwrap();
    conn.query_drop(format!("DROP DATABASE {db}")).unwrap();
    let (model, snapshot, full) = result.unwrap();
    assert_eq!(model.schema, schema);
    assert_eq!(
        model
            .tables
            .iter()
            .map(|table| table.object.name.as_str())
            .collect::<Vec<_>>(),
        ["catalog", "orders"]
    );
    assert_eq!(
        model.tables[1]
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        ["id", "area", "item"]
    );
    assert_eq!(model.foreign_keys.len(), 1);
    let key = &model.foreign_keys[0];
    assert_eq!(key.name, "fk_item");
    assert_eq!(key.table.name, "orders");
    assert_eq!(key.referenced_table.name, "catalog");
    assert_eq!(
        key.columns,
        [
            ("area".into(), "region".into()),
            ("item".into(), "code".into())
        ]
    );
    assert_eq!(
        (key.on_delete.as_str(), key.on_update.as_str()),
        ("CASCADE", "RESTRICT")
    );
    assert_eq!(snapshot.objects.len(), 1);
    assert!(
        snapshot.objects[0]
            .definition
            .as_ref()
            .unwrap()
            .contains("fk_item")
    );
    let metadata = full.context.as_ref().unwrap();
    assert!(!metadata.character_set_client.is_empty());
    let keys = full.foreign_keys.as_ref().unwrap();
    assert!(keys.iter().any(|key| key.name == "fk_external"
        && key.table.schema == external
        && key.referenced_table.schema == schema));
    let properties = |kind| {
        full.objects
            .iter()
            .find(|object| object.object.kind == kind)
            .and_then(|object| object.context.as_ref())
            .unwrap()
    };
    assert_eq!(
        properties(ObjectKind::Trigger).trigger_action_order,
        Some(1)
    );
    assert_eq!(
        properties(ObjectKind::Event).time_zone.as_deref(),
        Some("+03:00")
    );
}
