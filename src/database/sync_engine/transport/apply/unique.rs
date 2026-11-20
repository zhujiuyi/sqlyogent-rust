use super::*;

pub(super) const REJECTED: &str =
    "镜像唯一约束预检未通过。请核对范围内的旧行、目标端保留行和待新增记录。";

pub(super) fn early_deletions(
    tables: &[Staged<'_>],
    dependencies: &BTreeSet<ForeignKeyEdge>,
) -> BTreeSet<CatalogTable> {
    let mut early: BTreeSet<_> = tables
        .iter()
        .filter(|table| {
            table.review.requires_unique_preflight()
                && table.review.approval.target_writes.deleted > 0
        })
        .map(|table| table.target.table.clone())
        .collect();
    // A parent scheduled for early removal still requires its selected child deletions
    // to run first. Keep unrelated deletions in their original late phase.
    loop {
        let before = early.len();
        for (child, parent) in dependencies {
            if early.contains(parent) {
                early.insert(child.clone());
            }
        }
        if early.len() == before {
            return early;
        }
    }
}

pub(super) fn mutation_order(early: &[bool]) -> impl Iterator<Item = (usize, bool)> + '_ {
    let reversed = (0..early.len()).rev();
    reversed
        .clone()
        .filter(move |&index| early[index])
        .map(|index| (index, true))
        .chain((0..early.len()).map(|index| (index, false)))
        .chain(
            reversed
                .filter(move |&index| !early[index])
                .map(|index| (index, true)),
        )
}

pub(super) fn preflight(
    index: usize,
    layout: &Layout,
    writer: &WriteStatements,
    mutations: &[Mutation],
    mut emit: impl FnMut(String) -> Result<(), String>,
) -> Result<(), String> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let scratch = qualified_table(&(
        layout.table.0.clone(),
        format!("__harbor_mirror_{index}_{nonce:x}"),
    ));
    let table = &layout.qualified;
    let (columns, slots): (Vec<_>, Vec<_>) = writer
        .writable
        .iter()
        .map(|&index| &layout.columns[index])
        .map(|column| (column.quoted.as_str(), column.bind.as_str()))
        .unzip();
    let columns = columns.join(", ");
    let insert = format!(
        "INSERT IGNORE INTO {scratch} ({columns}) SELECT {} WHERE @harbor_sync_unique_ok = 1",
        slots.join(", ")
    );

    // The server enforces the actual nullable/prefix index semantics and collations.
    // Check the whole receiver, including the rows outside the approved filter.
    for sql in [
        "SET @harbor_sync_unique_before = COALESCE(@harbor_sync_ok = 1, 0)".into(),
        "SET @harbor_sync_ok = 0, @harbor_sync_unique_ok = 0".into(),
        format!("DROP TEMPORARY TABLE IF EXISTS {scratch}"),
        format!("CREATE TEMPORARY TABLE {scratch} LIKE {table}"),
        format!("SELECT COUNT(*) INTO @harbor_sync_unique_rows FROM {table} FOR UPDATE"),
        format!("INSERT IGNORE INTO {scratch} ({columns}) SELECT {columns} FROM {table} WHERE @harbor_sync_unique_before = 1"),
        WRITE_DIAGNOSTICS.into(),
        "SET @harbor_sync_unique_ok = (@harbor_sync_unique_before = 1 AND @harbor_sync_affected = @harbor_sync_unique_rows AND @harbor_sync_warnings = 0)".into(),
    ] {
        emit(sql)?;
    }

    for mutation in mutations {
        if let Mutation::Delete(row) = mutation {
            let selected = existing_row(layout, row, true)?;
            let exact = existing_row(layout, row, false)?;
            emit(format!(
                "SET @harbor_sync_unique_ok = (@harbor_sync_unique_ok = 1 AND EXISTS (SELECT 1 FROM {table} WHERE {selected} LIMIT 1 FOR UPDATE))"
            ))?;
            emit(format!(
                "DELETE FROM {scratch} WHERE @harbor_sync_unique_ok = 1 AND {exact}"
            ))?;
            emit(WRITE_DIAGNOSTICS.into())?;
            emit("SET @harbor_sync_unique_ok = (@harbor_sync_unique_ok = 1 AND @harbor_sync_affected > 0 AND @harbor_sync_warnings = 0)".into())?;
        }
    }
    for mutation in mutations {
        if let Mutation::Insert(row) = mutation {
            let exact = existing_row(layout, row, false)?;
            emit(format!(
                "SET @harbor_sync_unique_ok = (@harbor_sync_unique_ok = 1 AND NOT EXISTS (SELECT 1 FROM {scratch} WHERE {exact} LIMIT 1))"
            ))?;
            emit(render_parameters(&insert, &writer.values(row))?)?;
            emit(WRITE_DIAGNOSTICS.into())?;
            emit("SET @harbor_sync_unique_ok = (@harbor_sync_unique_ok = 1 AND @harbor_sync_affected = 1 AND @harbor_sync_warnings = 0)".into())?;
        }
    }
    emit(format!("DROP TEMPORARY TABLE {scratch}"))?;
    emit("SET @harbor_sync_unique_failed = (@harbor_sync_unique_failed = 1 OR (@harbor_sync_unique_before = 1 AND COALESCE(@harbor_sync_unique_ok, 0) = 0)), @harbor_sync_ok = COALESCE(@harbor_sync_unique_ok, 0)".into())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::mutation_order;

    #[test]
    fn early_mirror_deletes_keep_child_first_order() {
        let early = [true, true, false];
        assert_eq!(
            mutation_order(&early).collect::<Vec<_>>(),
            [
                (1, true),
                (0, true),
                (0, false),
                (1, false),
                (2, false),
                (2, true)
            ]
        );
    }
}
