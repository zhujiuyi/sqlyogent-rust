use std::collections::{BTreeMap, HashMap, HashSet};

use super::super::OdbcReferentialAction;
use super::*;

pub(super) struct PlannedForeignKey {
    child: usize,
    parent: usize,
    destination: String,
    name: Option<String>,
    columns: String,
    referenced_columns: String,
    on_update: &'static str,
    on_delete: &'static str,
}

pub(super) fn plan_foreign_keys(
    tables: &[OdbcTarget],
    cancel: &AtomicBool,
) -> Result<Vec<PlannedForeignKey>, String> {
    let endpoints = tables
        .iter()
        .enumerate()
        .map(|(index, table)| {
            (
                (table.object.schema.as_str(), table.object.name.as_str()),
                index,
            )
        })
        .collect::<HashMap<_, _>>();
    let mut plans = Vec::new();
    for (child, table) in tables.iter().enumerate() {
        check_import_cancelled(Some(cancel))?;
        for fk in &table.foreign_keys {
            check_import_cancelled(Some(cancel))?;
            if !valid_identifier(&table.object.schema)
                || !valid_identifier(&table.object.name)
                || fk
                    .name
                    .as_deref()
                    .is_some_and(|name| !valid_identifier(name))
                || !valid_identifier(&fk.referenced_table)
                || fk.columns.is_empty()
                || fk.columns.len() != fk.referenced_columns.len()
            {
                return Err("ODBC 外键的名称或字段映射无效。".into());
            }
            let parent = *endpoints
                .get(&(table.object.schema.as_str(), fk.referenced_table.as_str()))
                .ok_or_else(|| {
                    format!(
                        "外键引用的表 {} 不在同一目标库的导入范围内。",
                        quote_identifier(&fk.referenced_table)
                    )
                })?;
            let nullable = fk.on_update == OdbcReferentialAction::SetNull
                || fk.on_delete == OdbcReferentialAction::SetNull;
            plans.push(PlannedForeignKey {
                child,
                parent,
                destination: table.object.qualified_name(),
                name: fk.name.clone(),
                columns: mapped_columns(table, &fk.columns, nullable)?,
                referenced_columns: mapped_columns(&tables[parent], &fk.referenced_columns, false)?,
                on_update: action_sql(fk.on_update)?,
                on_delete: action_sql(fk.on_delete)?,
            });
        }
    }
    Ok(plans)
}

fn mapped_columns(
    table: &OdbcTarget,
    columns: &[String],
    nullable: bool,
) -> Result<String, String> {
    let definitions = table
        .create
        .as_ref()
        .ok_or("复制外键时，关联的两张目标表都须选择新建或重建。")?;
    let mut seen = HashSet::new();
    let mut mapped = Vec::with_capacity(columns.len());
    for name in columns {
        let folded = name.to_lowercase();
        let index = table
            .columns
            .iter()
            .position(|column| column.to_lowercase() == folded);
        let Some(index) = index.filter(|&index| valid_identifier(name) && seen.insert(index))
        else {
            return Err(format!(
                "目标表 {} 的外键列 {} 缺少有效映射。",
                table.object.qualified_name(),
                quote_identifier(name),
            ));
        };
        let definition = definitions.get(index).ok_or("外键列缺少目标列定义。")?;
        if nullable && (definition.not_null || definition.primary) {
            return Err(format!(
                "外键列 {} 必须允许 NULL 才能使用 SET NULL。",
                quote_identifier(name)
            ));
        }
        mapped.push(quote_identifier(&table.columns[index]));
    }
    Ok(mapped.join(", "))
}

fn valid_identifier(name: &str) -> bool {
    !name.is_empty() && name.chars().count() <= 64 && !name.contains('\0')
}

fn action_sql(action: OdbcReferentialAction) -> Result<&'static str, String> {
    match action {
        OdbcReferentialAction::Cascade => Ok("CASCADE"),
        OdbcReferentialAction::Restrict => Ok("RESTRICT"),
        OdbcReferentialAction::SetNull => Ok("SET NULL"),
        OdbcReferentialAction::NoAction => Ok("NO ACTION"),
        OdbcReferentialAction::SetDefault => {
            Err("InnoDB 外键不支持 SET DEFAULT。请选择受支持的动作。".into())
        }
    }
}

pub(super) struct ForeignKeyError {
    message: String,
    duplicate_name: bool,
}

impl ForeignKeyError {
    pub(super) fn other(message: String) -> Self {
        Self {
            message,
            duplicate_name: false,
        }
    }

    pub(super) fn server(code: u32, message: String) -> Self {
        // MySQL reports ER_FK_DUP_NAME; MariaDB also uses InnoDB errno 121.
        let duplicate_name = code == 1826 || (code == 1005 && message.contains("errno: 121"));
        Self {
            message: format!("ERROR {code}: {message}"),
            duplicate_name,
        }
    }
}

pub(super) fn install_foreign_keys(
    plans: &[PlannedForeignKey],
    stages: &[&DbObject],
    token: u64,
    cancel: &AtomicBool,
    warnings: &mut Vec<String>,
    mut execute: impl FnMut(&str) -> Result<(), ForeignKeyError>,
) -> Result<(), String> {
    for (index, plan) in plans.iter().enumerate() {
        check_import_cancelled(Some(cancel))?;
        let child = stages[plan.child];
        let statement = |name| {
            format!(
                "ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}) ON UPDATE {} ON DELETE {}",
                child.qualified_name(),
                quote_identifier(name),
                plan.columns,
                stages[plan.parent].qualified_name(),
                plan.referenced_columns,
                plan.on_update,
                plan.on_delete,
            )
        };
        let generated = format!("_harbor_odbc_fk_{token:016x}_{index}");
        // InnoDB interprets the table_ibfk_ prefix as an internally generated
        // symbol and may change it during RENAME. Stage names are private.
        let rename_prefix = format!("{}_ibfk_", child.name.to_lowercase());
        let mut name = plan
            .name
            .as_deref()
            .filter(|name| !name.to_lowercase().starts_with(&rename_prefix))
            .unwrap_or(&generated);
        let result = match execute(&statement(name)) {
            Err(error) if error.duplicate_name && name != generated => {
                check_import_cancelled(Some(cancel))?;
                name = &generated;
                execute(&statement(name))
            }
            result => result,
        };
        result.map_err(|error| {
            format!(
                "无法添加目标表 {} 的外键 {}。{}",
                plan.destination,
                quote_identifier(name),
                error.message
            )
        })?;
        if let Some(requested) = plan.name.as_deref().filter(|requested| *requested != name) {
            warnings.push(format!(
                "目标表 {} 的外键 {} 使用了新名称 {}。",
                plan.destination,
                quote_identifier(requested),
                quote_identifier(name)
            ));
        }
    }
    Ok(())
}

pub(super) fn stage_cleanup_groups<'a>(
    stages: impl Iterator<Item = &'a DbObject>,
    linked: bool,
) -> Vec<Vec<String>> {
    if !linked {
        return stages.map(|stage| vec![stage.qualified_name()]).collect();
    }
    let mut groups = BTreeMap::<_, Vec<_>>::new();
    for stage in stages {
        groups
            .entry(&stage.schema)
            .or_default()
            .push(stage.qualified_name());
    }
    groups.into_values().collect()
}
