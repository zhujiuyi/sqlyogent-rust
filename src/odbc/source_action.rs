use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::SourceTable;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceAction {
    pub table: SourceTable,
    pub change: SourceChange,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceChange {
    Update { assignments: Vec<SourceAssignment> },
    Delete,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceAssignment {
    pub column: String,
    pub expression: String,
}

impl SourceAction {
    pub(crate) fn validate(&self) -> Result<(), String> {
        self.table.validate()?;
        if let SourceChange::Update { assignments } = &self.change {
            if !(1..=crate::database::MAX_IMPORT_COLUMNS).contains(&assignments.len()) {
                return Err("请为源表更新填写至少一项有效的列赋值。".into());
            }
            let mut columns = HashSet::new();
            for assignment in assignments {
                let column = assignment.column.as_str();
                if column.trim().is_empty()
                    || column.chars().count() > 64
                    || column.chars().any(char::is_control)
                    || !columns.insert(column.to_lowercase())
                {
                    return Err("源表更新需要有效且不重复的列名。".into());
                }
                let expression = &assignment.expression;
                if expression.trim().is_empty()
                    || expression.len() > 4096
                    || expression.contains('\0')
                {
                    return Err("请为源表更新填写有效的赋值表达式（最多 4096 字节）。".into());
                }
            }
        }
        Ok(())
    }
}
