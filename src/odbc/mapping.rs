use crate::database::{OdbcForeignKey, OdbcIndex};

use super::{SourceData, SourceForeignKey, SourceIndex, SourceTable};

pub fn map_single_table_metadata(
    source_data: &SourceData,
    target_table: &str,
    mapping: &[(String, String)],
) -> Result<(Vec<OdbcIndex>, Vec<OdbcForeignKey>), String> {
    let indexes = map_indexes(&source_data.indexes, mapping)?;
    let foreign_keys = source_data
        .foreign_keys
        .iter()
        .map(|key| {
            if !source_data
                .source_table
                .as_ref()
                .is_some_and(|source| same_source_table(source, &key.referenced))
            {
                return Err(format!(
                    "外键 {} 引用源表 {}。请新建外部数据导入作业，把两张表一起导入。",
                    key_name(key),
                    source_table_name(&key.referenced)
                ));
            }
            map_foreign_key(key, mapping, target_table, mapping)
        })
        .collect::<Result<_, _>>()?;
    Ok((indexes, foreign_keys))
}

pub(crate) fn map_indexes(
    indexes: &[SourceIndex],
    mapping: &[(String, String)],
) -> Result<Vec<OdbcIndex>, String> {
    indexes
        .iter()
        .map(|index| {
            let columns = index
                .columns
                .iter()
                .map(|(name, descending)| {
                    mapped_column(mapping, name)
                        .map(|target| (target, *descending))
                        .ok_or_else(|| format!("源索引 {} 的列 {name} 缺少唯一映射。", index.name))
                })
                .collect::<Result<_, _>>()?;
            Ok(OdbcIndex {
                name: index.name.clone(),
                unique: index.unique,
                columns,
            })
        })
        .collect()
}

pub(crate) fn map_foreign_key(
    key: &SourceForeignKey,
    mapping: &[(String, String)],
    referenced_table: &str,
    referenced_mapping: &[(String, String)],
) -> Result<OdbcForeignKey, String> {
    let map_columns = |names: &[String], fields: &[(String, String)]| {
        names
            .iter()
            .map(|name| {
                mapped_column(fields, name)
                    .ok_or_else(|| format!("源外键 {} 的列 {name} 缺少唯一映射。", key_name(key)))
            })
            .collect::<Result<Vec<_>, _>>()
    };
    Ok(OdbcForeignKey {
        name: key.name.clone(),
        columns: map_columns(&key.columns, mapping)?,
        referenced_table: referenced_table.to_owned(),
        referenced_columns: map_columns(&key.referenced_columns, referenced_mapping)?,
        on_update: key.on_update,
        on_delete: key.on_delete,
    })
}

fn mapped_column(mapping: &[(String, String)], name: &str) -> Option<String> {
    let name = name.to_lowercase();
    let mut matches = mapping
        .iter()
        .filter(|(source, _)| source.to_lowercase() == name);
    let (_, target) = matches.next()?;
    matches.next().is_none().then(|| target.clone())
}

pub(crate) fn same_source_table(left: &SourceTable, right: &SourceTable) -> bool {
    let component = |left: Option<&str>, right: Option<&str>| {
        left.filter(|value| !value.is_empty()) == right.filter(|value| !value.is_empty())
    };
    left.table == right.table
        && component(left.catalog.as_deref(), right.catalog.as_deref())
        && component(left.schema.as_deref(), right.schema.as_deref())
}

pub(crate) fn key_name(key: &SourceForeignKey) -> &str {
    key.name.as_deref().unwrap_or("（未命名）")
}

pub(crate) fn source_table_name(table: &SourceTable) -> String {
    [
        table.catalog.as_deref(),
        table.schema.as_deref(),
        Some(&table.table),
    ]
    .into_iter()
    .flatten()
    .filter(|name| !name.is_empty())
    .collect::<Vec<_>>()
    .join(".")
}
