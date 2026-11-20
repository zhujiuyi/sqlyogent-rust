use super::*;

const MAX_ERROR_DETAILS: usize = 40;

pub(super) struct RowSync<'a, 'b, 'p> {
    tables: &'a [Staged<'b>],
    pending: Vec<usize>,
    status: SyncProgress,
    result: SyncResult,
    progress: &'p mut dyn FnMut(SyncProgress) -> Result<(), String>,
}

impl<'a, 'b, 'p> RowSync<'a, 'b, 'p> {
    pub(super) fn new(
        tables: &'a [Staged<'b>],
        status: SyncProgress,
        progress: &'p mut dyn FnMut(SyncProgress) -> Result<(), String>,
    ) -> Self {
        Self {
            tables,
            pending: tables
                .iter()
                .map(|item| item.mutations.source.len() + item.mutations.target.len())
                .collect(),
            status,
            result: SyncResult::default(),
            progress,
        }
    }

    pub(super) fn receiver(
        &mut self,
        transport: &Transport,
        reader: &mut Reader<'_, '_>,
        identity: &EndpointIdentity,
        side: SyncSide,
        order: &[usize],
        skip_fk_checks: bool,
    ) -> Result<bool, String> {
        self.status.side = Some(side);
        match self.write_receiver(transport, reader, identity, side, order, skip_fk_checks) {
            Ok(()) => Ok(true),
            Err(CommitFailure::BeforeCommit(error)) => {
                if self.status.written == 0 && self.result.skipped_rows == 0 {
                    return Err(error);
                }
                self.result.stopped = Some(format!("{}已停止写入。{error}", side.label()));
                Ok(false)
            }
            Err(CommitFailure::Uncertain(error)) => {
                self.result.stopped =
                    Some(format!("无法确认{}当前行是否已提交。{error}", side.label()));
                self.result.uncertain = Some(side);
                Ok(false)
            }
        }
    }

    pub(super) fn finish(self) -> (SyncResult, usize) {
        let completed = self.pending.iter().filter(|&&count| count == 0).count();
        (self.result, completed)
    }

    fn write_receiver(
        &mut self,
        transport: &Transport,
        reader: &mut Reader<'_, '_>,
        identity: &EndpointIdentity,
        side: SyncSide,
        order: &[usize],
        skip_fk_checks: bool,
    ) -> Result<(), CommitFailure> {
        let pending = self.tables;
        let mut tables = Vec::new();
        let has_deletes = !skip_fk_checks
            && order.iter().any(|&index| {
                pending[index]
                    .mutations
                    .for_side(side)
                    .iter()
                    .any(Mutation::is_delete)
            });
        if has_deletes && matches!(transport, Transport::Http(_)) {
            mirror::require_grant_visibility(reader)?;
        }
        for &index in order {
            let item = &pending[index];
            let changes = item.mutations.for_side(side);
            if changes.is_empty() {
                continue;
            }
            (self.progress)(self.status)?;
            let (layout, review) = item.receiver(side);
            let condition = receiver_layout_condition(reader, identity, side, layout, review)?;
            let lock = if layout.is_keyless() {
                keyless_receiver_lock(layout)
            } else {
                format!("SELECT 1 FROM {} LIMIT 0", layout.qualified)
            };
            let mut checks = filter_metadata_pins(layout).collect::<Vec<_>>();
            checks.extend([
                lock,
                format!("SET @harbor_sync_ok = (@harbor_sync_ok = 1 AND {condition})"),
            ]);
            let delete = if has_deletes && changes.iter().any(Mutation::is_delete) {
                let policy = DeletePolicy::read(reader, &review.object, layout)?;
                checks.push(format!(
                    "SET @harbor_sync_ok = (@harbor_sync_ok = 1 AND {})",
                    policy.metadata_condition
                ));
                policy.guards
            } else {
                Vec::new()
            };
            tables.push(PendingTable {
                index,
                layout,
                object: &review.object,
                changes,
                checks,
                delete,
                statements: WriteStatements::new(layout),
            });
        }
        if tables.is_empty() {
            return Ok(());
        }
        let start = receiver_start(identity, skip_fk_checks, true)?;
        let mut writer = match transport {
            Transport::Native(opts) => {
                let (connection, current) = open_writer(opts, side, skip_fk_checks)?;
                if &current != identity {
                    return Err("接收端连接已变化，请重新比较。".to_owned().into());
                }
                RowWriter::Native(connection)
            }
            Transport::Http(session) => RowWriter::Http(session),
        };
        // Parents are written before their children. Remove rows in the reverse order.
        for table in &tables {
            self.write_rows(&mut writer, &start, table, side, false)?;
        }
        for table in tables.iter().rev() {
            self.write_rows(&mut writer, &start, table, side, true)?;
        }
        Ok(())
    }

    fn write_rows(
        &mut self,
        writer: &mut RowWriter<'_>,
        start: &[String],
        table: &PendingTable<'_>,
        side: SyncSide,
        deleting: bool,
    ) -> Result<(), CommitFailure> {
        for change in table
            .changes
            .iter()
            .filter(|change| change.is_delete() == deleting)
        {
            (self.progress)(self.status)?;
            let mut row = Vec::new();
            let mut size = start
                .iter()
                .chain(&table.checks)
                .map(|sql| sql.len() + 32)
                .sum();
            if let Mutation::Insert(incoming) = change
                && !table.layout.is_keyless()
            {
                // A newly occupied matching key is a stale plan. Checking it before
                // INSERT distinguishes that case from a secondary constraint error.
                let key =
                    render_parameters(&table.layout.predicate, &table.layout.key_values(incoming))?;
                add_commands(
                    &mut row,
                    &mut size,
                    [format!(
                        "SET @harbor_sync_ok = (@harbor_sync_ok = 1 AND NOT EXISTS (SELECT 1 FROM {} WHERE {key} LIMIT 1 FOR UPDATE))",
                        table.layout.qualified,
                    )],
                )?;
            }
            add_commands(
                &mut row,
                &mut size,
                mutation_sql(table.layout, &table.statements, change, &table.delete)?,
            )?;
            let skipped = writer.write(start, &table.checks, &row, change)?;
            if let Some(code) = skipped {
                self.result.skipped_rows += 1;
                if self.result.row_errors.len() < MAX_ERROR_DETAILS {
                    self.result.row_errors.push(SyncRowError {
                        side,
                        table: table.object.clone(),
                        operation: change.operation(),
                        code,
                    });
                }
            } else {
                let writes = match side {
                    SyncSide::Source => &mut self.result.source,
                    SyncSide::Target => &mut self.result.target,
                };
                match change {
                    Mutation::Insert(_) => writes.inserted += 1,
                    Mutation::Update { .. } => writes.updated += 1,
                    Mutation::Delete(_) => writes.deleted += 1,
                }
                self.status.written += 1;
            }
            self.pending[table.index] -= 1;
        }
        Ok(())
    }
}

struct PendingTable<'a> {
    index: usize,
    layout: &'a Layout,
    object: &'a DbObject,
    changes: &'a [Mutation],
    checks: Vec<String>,
    delete: Vec<DeleteGuard>,
    statements: WriteStatements,
}

impl Mutation {
    fn is_delete(&self) -> bool {
        matches!(self, Self::Delete(_))
    }

    fn operation(&self) -> &'static str {
        match self {
            Self::Insert(_) => "insert",
            Self::Update { .. } => "update",
            Self::Delete(_) => "delete",
        }
    }

    fn matches_statement(&self, sql: &str) -> bool {
        let prefix = match self {
            Self::Insert(_) => "INSERT INTO ",
            Self::Update { .. } => "UPDATE ",
            Self::Delete(_) => "DELETE FROM ",
        };
        sql.starts_with(prefix)
    }
}

enum RowWriter<'a> {
    Native(Conn),
    Http(&'a Session),
}

impl RowWriter<'_> {
    fn write(
        &mut self,
        start: &[String],
        checks: &[String],
        row: &[String],
        change: &Mutation,
    ) -> Result<Option<u16>, CommitFailure> {
        let commands = start
            .iter()
            .chain(checks)
            .chain(row)
            .map(String::as_str)
            .chain(std::iter::once(script::RESTORE_FOREIGN_KEY_CHECKS));
        let committed = match self {
            Self::Native(connection) => {
                for sql in commands {
                    if let Err(error) = connection.query_drop(sql) {
                        let skipped = match &error {
                            mysql::Error::MySqlError(details)
                                if change.matches_statement(sql) && is_row_error(details.code) =>
                            {
                                Some(details.code)
                            }
                            _ => None,
                        };
                        let rollback = connection.query_drop("ROLLBACK AND NO CHAIN NO RELEASE");
                        if let Some(code) = skipped {
                            rollback.map_err(mysql_error)?;
                            return Ok(Some(code));
                        }
                        return Err(mysql_error(error).into());
                    }
                }
                let valid = connection
                    .query_first::<u8, _>("SELECT @harbor_sync_ok")
                    .map_err(mysql_error)?;
                match valid {
                    Some(1) => {
                        connection
                            .query_drop("COMMIT AND NO CHAIN NO RELEASE")
                            .map_err(|error| CommitFailure::Uncertain(mysql_error(error)))?;
                        true
                    }
                    Some(0) => {
                        connection
                            .query_drop("ROLLBACK AND NO CHAIN NO RELEASE")
                            .map_err(mysql_error)?;
                        false
                    }
                    _ => return Err("服务器未返回可验证的同步行校验结果。".to_owned().into()),
                }
            }
            Self::Http(session) => {
                let commands = commands.chain(FINISH_RECEIVER).collect::<Vec<_>>();
                let batch = batch_statements(&commands)?;
                let payload = session.prepare_payload(&batch, true, "")?;
                let mut reply = session
                    .execute_payload(payload, 1)
                    .map_err(CommitFailure::Uncertain)?;
                if let Some(error) = reply.error.take() {
                    // SQLyog 8.05 stops processing statements on a server error.
                    // The allowed errors are row statement rejections. The request
                    // closes its uncommitted connection without reaching COMMIT.
                    if let Ok(code) = u16::try_from(error.code)
                        && is_row_error(code)
                    {
                        return Ok(Some(code));
                    }
                    return Err(CommitFailure::Uncertain(format!(
                        "隧道返回数据库错误 {}。{}",
                        error.code, error.message
                    )));
                }
                let set = single_set(reply).map_err(CommitFailure::Uncertain)?;
                receiver_confirmed(&set).ok_or_else(|| {
                    CommitFailure::Uncertain("隧道未返回可验证的提交状态。".into())
                })?
            }
        };
        if committed {
            Ok(None)
        } else {
            Err("接收端的结构、原始行或写入结果已变化，请重新比较。"
                .to_owned()
                .into())
        }
    }
}

fn is_row_error(code: u16) -> bool {
    matches!(
        code,
        1022 | 1048
            | 1062
            | 1169
            | 1216
            | 1217
            | 1263
            | 1264
            | 1265
            | 1292
            | 1364
            | 1365
            | 1366
            | 1406
            | 1451
            | 1452
            | 1644
            | 1690
            | 3140
            | 3819
            | 4025
    )
}
