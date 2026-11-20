use crate::database::{odbc_identifier_key, quote_identifier};
use crate::sql_tools::mysql_text_literal;

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExistingTarget {
    Error,
    Append,
    Skip,
    Truncate,
    Recreate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingTarget {
    Error,
    Create,
    Skip,
}

#[derive(Clone, Copy, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TableStrategy {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub if_exists: Option<ExistingTarget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub if_missing: Option<MissingTarget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structure_only: Option<bool>,
}

impl TableStrategy {
    pub fn is_empty(&self) -> bool {
        self.if_exists.is_none() && self.if_missing.is_none() && self.structure_only.is_none()
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct EffectiveTableStrategy {
    pub if_exists: ExistingTarget,
    pub if_missing: MissingTarget,
    pub structure_only: bool,
}

impl EffectiveTableStrategy {
    pub fn with_override(self, table: Option<TableStrategy>) -> Self {
        let table = table.unwrap_or_default();
        Self {
            if_exists: table.if_exists.unwrap_or(self.if_exists),
            if_missing: table.if_missing.unwrap_or(self.if_missing),
            structure_only: table.structure_only.unwrap_or(self.structure_only),
        }
    }

    pub fn defines_table(self) -> bool {
        self.if_missing == MissingTarget::Create || self.if_exists == ExistingTarget::Recreate
    }

    pub fn imports_rows(self) -> bool {
        !self.structure_only
            && (self.if_missing == MissingTarget::Create
                || matches!(
                    self.if_exists,
                    ExistingTarget::Append | ExistingTarget::Truncate | ExistingTarget::Recreate
                ))
    }

    pub(super) fn validate(self) -> Result<(), String> {
        if self.structure_only && !self.defines_table() {
            return Err("仅导入表结构需要允许创建或重建目标表。".into());
        }
        if self.structure_only && self.if_exists == ExistingTarget::Truncate {
            return Err("仅导入表结构时不能清空已有表。".into());
        }
        Ok(())
    }
}

impl OdbcJob {
    pub fn has_table_strategies(&self) -> bool {
        self.tables
            .iter()
            .flatten()
            .any(|table| table.strategy.is_some_and(|strategy| !strategy.is_empty()))
    }

    pub fn default_table_strategy(&self) -> EffectiveTableStrategy {
        let if_exists = match self.reset {
            OdbcReset::Truncate => ExistingTarget::Truncate,
            OdbcReset::Recreate => ExistingTarget::Recreate,
            OdbcReset::None if self.create_table => ExistingTarget::Error,
            OdbcReset::None => ExistingTarget::Append,
        };
        EffectiveTableStrategy {
            if_exists,
            if_missing: if self.create_table {
                MissingTarget::Create
            } else {
                MissingTarget::Error
            },
            structure_only: self.structure_only,
        }
    }

    pub(super) fn default_import_plan(&self) -> ImportPlan {
        ImportPlan {
            create: self.create_table,
            reset: self.reset,
            structure_only: self.structure_only,
            expected_ddl: None,
        }
    }
}

pub(super) struct ImportPlan {
    pub(super) create: bool,
    pub(super) reset: OdbcReset,
    pub(super) structure_only: bool,
    pub(super) expected_ddl: Option<String>,
}

pub(super) enum TargetDecision {
    Import(ImportPlan),
    Skip(&'static str),
}

pub(super) fn read_presence(
    worker: &Worker,
    schema: &str,
    specs: &[TableSpec<'_>],
    cancel: &AtomicBool,
) -> Result<Vec<Option<String>>, String> {
    check_import_cancelled(Some(cancel))?;
    let names = specs
        .iter()
        .enumerate()
        .map(|(index, spec)| {
            format!(
                "SELECT {index} AS harbor_index, {} AS harbor_table",
                mysql_text_literal(spec.name)
            )
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let key = odbc_identifier_key("wanted.harbor_table");
    let schema_key = odbc_identifier_key(&mysql_text_literal(schema));
    let table_schema = odbc_identifier_key("t.TABLE_SCHEMA");
    let table_name = odbc_identifier_key("t.TABLE_NAME");
    let sql = format!(
        "SELECT wanted.harbor_index, {key}, t.TABLE_TYPE
         FROM ({names}) AS wanted
         LEFT JOIN information_schema.TABLES t
           ON CAST({table_schema} AS BINARY) = CAST({schema_key} AS BINARY)
          AND CAST({table_name} AS BINARY) = CAST({key} AS BINARY)
         ORDER BY wanted.harbor_index"
    );
    let Response::Queried(report) = request(
        worker,
        4,
        Command::Query {
            sql,
            limit: specs.len() + 1,
        },
    )?
    else {
        return Err("目标连接未返回表状态。".into());
    };
    check_import_cancelled(Some(cancel))?;
    if let Some(error) = report.error {
        return Err(format!("无法读取 ODBC 目标表状态。{error}"));
    }
    let [set] = report.sets.as_slice() else {
        return Err("目标连接未返回表状态列表。".into());
    };
    if set.truncated || set.rows.len() != specs.len() {
        return Err("目标数据库返回的表状态数量不符。".into());
    }
    let mut seen = HashSet::new();
    set.rows
        .iter()
        .enumerate()
        .map(|(position, row)| {
            let [Cell::Text(index), Cell::Text(key), kind] = row.as_slice() else {
                return Err("目标数据库返回的表状态格式无效。".into());
            };
            if index.parse::<usize>().ok() != Some(position) {
                return Err("目标数据库返回的表状态顺序无效。".into());
            }
            if !seen.insert(key) {
                return Err(format!(
                    "目标服务器将多份映射识别为同一张表「{}」。",
                    specs[position].name
                ));
            }
            match kind {
                Cell::Text(kind) => Ok(Some(kind.clone())),
                Cell::Null => Ok(None),
                Cell::Binary(_) => Err("目标数据库返回的表类型无效。".into()),
            }
        })
        .collect()
}

impl TableSpec<'_> {
    pub(super) fn effective_strategy(&self, job: &OdbcJob) -> EffectiveTableStrategy {
        job.default_table_strategy().with_override(self.strategy)
    }

    pub(super) fn resolve_target(
        &self,
        job: &OdbcJob,
        worker: &Worker,
        presence: Option<&Option<String>>,
        cancel: &AtomicBool,
    ) -> Result<TargetDecision, String> {
        check_import_cancelled(Some(cancel))?;
        let Some(presence) = presence else {
            return Ok(TargetDecision::Import(job.default_import_plan()));
        };
        let policy = self.effective_strategy(job);
        let object = DbObject {
            schema: job.schema.clone(),
            name: self.name.to_owned(),
            kind: ObjectKind::Table,
        };
        let qualified = format!(
            "{}.{}",
            quote_identifier(&job.schema),
            quote_identifier(self.name)
        );
        let mut plan = ImportPlan {
            create: false,
            reset: OdbcReset::None,
            structure_only: policy.structure_only,
            expected_ddl: None,
        };
        let Some(kind) = presence else {
            match policy.if_missing {
                MissingTarget::Create => plan.create = true,
                MissingTarget::Error => return Err(format!("目标表 {qualified} 不存在。")),
                MissingTarget::Skip => {
                    return Ok(TargetDecision::Skip("目标表不存在，已按策略跳过。"));
                }
            }
            return Ok(TargetDecision::Import(plan));
        };
        match policy.if_exists {
            ExistingTarget::Error => return Err(format!("目标对象 {qualified} 已存在。")),
            ExistingTarget::Skip => {
                return Ok(TargetDecision::Skip("已有目标对象，已按策略跳过。"));
            }
            ExistingTarget::Append if plan.structure_only => {
                return Ok(TargetDecision::Skip("已有目标对象，已保留结构和数据。"));
            }
            ExistingTarget::Append => (),
            ExistingTarget::Truncate => plan.reset = OdbcReset::Truncate,
            ExistingTarget::Recreate => {
                plan.create = true;
                plan.reset = OdbcReset::Recreate;
            }
        }
        if !matches!(kind.as_str(), "BASE TABLE" | "SYSTEM VERSIONED") {
            return Err(format!("已有目标对象 {qualified} 不是可导入的数据表。"));
        }
        let Response::Inspected(detail) = request(worker, 4, Command::Inspect { object })? else {
            return Err("目标连接未返回已有表的结构。".into());
        };
        check_import_cancelled(Some(cancel))?;
        if let Some(error) = detail.ddl_error {
            return Err(format!("无法确认已有表结构。{error}"));
        }
        if detail.ddl.trim().is_empty() {
            return Err("目标连接返回了空的建表语句。".into());
        }
        plan.expected_ddl = Some(detail.ddl);
        Ok(TargetDecision::Import(plan))
    }
}
