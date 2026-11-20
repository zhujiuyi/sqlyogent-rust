use crate::database::quote_identifier;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerView {
    Processes,
    GlobalStatus,
    SessionStatus,
    GlobalVariables,
    SessionVariables,
    Accounts,
    CurrentGrants,
    CharacterSets,
    Collations,
}

impl ServerView {
    pub const ALL: [Self; 9] = [
        Self::Processes,
        Self::GlobalStatus,
        Self::SessionStatus,
        Self::GlobalVariables,
        Self::SessionVariables,
        Self::Accounts,
        Self::CurrentGrants,
        Self::CharacterSets,
        Self::Collations,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Processes => "进程列表",
            Self::GlobalStatus => "全局状态",
            Self::SessionStatus => "会话状态",
            Self::GlobalVariables => "全局变量",
            Self::SessionVariables => "会话变量",
            Self::Accounts => "用户账户",
            Self::CurrentGrants => "当前账户权限",
            Self::CharacterSets => "字符集",
            Self::Collations => "排序规则",
        }
    }

    pub fn sql(self) -> &'static str {
        match self {
            Self::Processes => "SHOW FULL PROCESSLIST",
            Self::GlobalStatus => "SHOW GLOBAL STATUS",
            Self::SessionStatus => "SHOW SESSION STATUS",
            Self::GlobalVariables => "SHOW GLOBAL VARIABLES",
            Self::SessionVariables => "SHOW SESSION VARIABLES",
            Self::Accounts => "SELECT `User`, `Host` FROM `mysql`.`user` ORDER BY `User`, `Host`",
            Self::CurrentGrants => "SHOW GRANTS",
            Self::CharacterSets => "SHOW CHARACTER SET",
            Self::Collations => "SHOW COLLATION",
        }
    }

    pub fn is_variable_or_status(self) -> bool {
        matches!(
            self,
            Self::GlobalStatus
                | Self::SessionStatus
                | Self::GlobalVariables
                | Self::SessionVariables
        )
    }

    pub fn is_session(self) -> bool {
        matches!(self, Self::SessionStatus | Self::SessionVariables)
    }
}

pub fn kill_process(id: u64, query_only: bool) -> String {
    let target = if query_only { "QUERY" } else { "CONNECTION" };
    format!("KILL {target} {id}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlushTarget {
    Logs,
    Hosts,
    Privileges,
    Tables,
    Status,
    DesKeyFile,
    QueryCache,
    UserResources,
    ReadLock,
}

impl FlushTarget {
    pub const ALL: [Self; 9] = [
        Self::Logs,
        Self::Hosts,
        Self::Privileges,
        Self::Tables,
        Self::Status,
        Self::DesKeyFile,
        Self::QueryCache,
        Self::UserResources,
        Self::ReadLock,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Logs => "重新打开日志文件",
            Self::Hosts => "清除主机缓存",
            Self::Privileges => "重新加载权限",
            Self::Tables => "刷新数据表缓存",
            Self::Status => "重置服务器状态计数",
            Self::DesKeyFile => "重新加载 DES 密钥文件",
            Self::QueryCache => "整理查询缓存",
            Self::UserResources => "重置账户每小时用量",
            Self::ReadLock => "刷新表并加读锁",
        }
    }

    fn clause(self, server: FlushCapabilities) -> &'static str {
        match self {
            Self::Logs => "LOGS",
            Self::Hosts => "HOSTS",
            Self::Privileges => "PRIVILEGES",
            Self::Tables => "TABLES",
            Self::Status if server.mariadb && server.version >= Some((11, 5)) => "GLOBAL STATUS",
            Self::Status => "STATUS",
            Self::DesKeyFile => "DES_KEY_FILE",
            Self::QueryCache => "QUERY CACHE",
            Self::UserResources => "USER_RESOURCES",
            Self::ReadLock => "TABLES WITH READ LOCK",
        }
    }
}

#[derive(Clone, Copy)]
pub struct FlushCapabilities {
    mariadb: bool,
    version: Option<(u32, u32)>,
}

impl FlushCapabilities {
    pub fn from_version(version: &str) -> Self {
        let mariadb = version.to_ascii_lowercase().contains("mariadb");
        let version = if mariadb {
            version.strip_prefix("5.5.5-").unwrap_or(version)
        } else {
            version
        };
        let mut parts = version.split('.');
        let version = parts
            .next()
            .and_then(|major| major.parse().ok())
            .zip(parts.next().and_then(|minor| minor.parse().ok()));
        Self { mariadb, version }
    }

    pub fn unavailable(self, target: FlushTarget) -> Option<&'static str> {
        match target {
            FlushTarget::Hosts if !self.mariadb && self.version >= Some((8, 4)) => {
                Some("MySQL 8.4 起移除了 FLUSH HOSTS。")
            }
            FlushTarget::DesKeyFile if self.mariadb && self.version >= Some((13, 0)) => {
                Some("MariaDB 13.0 起移除了 DES 密钥文件。")
            }
            FlushTarget::DesKeyFile if !self.mariadb && self.version >= Some((8, 0)) => {
                Some("MySQL 8.0 起移除了 DES 密钥文件。")
            }
            FlushTarget::QueryCache if !self.mariadb && self.version >= Some((8, 0)) => {
                Some("MySQL 8.0 起移除了查询缓存。")
            }
            _ => None,
        }
    }

    pub fn supports_no_write_to_binlog(self) -> bool {
        self.version.is_none_or(|version| version >= (4, 1))
    }
}

pub struct FlushPlan {
    targets: usize,
    statements: Vec<String>,
}

impl FlushPlan {
    pub fn new(
        server: FlushCapabilities,
        selected: &[bool; FlushTarget::ALL.len()],
        all: bool,
        no_write_to_binlog: bool,
    ) -> Option<Self> {
        let mut plan = Self {
            targets: 0,
            statements: Vec::new(),
        };
        let prefix = if no_write_to_binlog && server.supports_no_write_to_binlog() {
            "FLUSH NO_WRITE_TO_BINLOG"
        } else {
            "FLUSH"
        };
        for target in FlushTarget::ALL {
            if (all || selected[target as usize]) && server.unavailable(target).is_none() {
                plan.targets += 1;
                plan.statements
                    .push(format!("{prefix} {}", target.clause(server)));
                if target == FlushTarget::ReadLock {
                    plan.statements.push("UNLOCK TABLES".into());
                }
            }
        }
        (plan.targets > 0).then_some(plan)
    }

    pub fn targets(&self) -> usize {
        self.targets
    }

    pub fn statements(&self) -> &[String] {
        &self.statements
    }

    pub fn sql(&self) -> String {
        format!("{};", self.statements.join(";\n"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableAction {
    Check,
    Analyze,
    Optimize,
    Repair,
    Truncate,
    Drop,
}

impl TableAction {
    pub const DIAGNOSTICS: [Self; 4] = [Self::Check, Self::Analyze, Self::Optimize, Self::Repair];

    pub fn label(self) -> &'static str {
        match self {
            Self::Check => "检查表",
            Self::Analyze => "分析表",
            Self::Optimize => "优化表",
            Self::Repair => "修复表",
            Self::Truncate => "清空表数据",
            Self::Drop => "删除表",
        }
    }

    pub fn sql(self, database: &str, table: &str) -> String {
        format!(
            "{} TABLE {}",
            self.keyword(),
            qualified_table(database, table)
        )
    }

    fn keyword(self) -> &'static str {
        match self {
            Self::Check => "CHECK",
            Self::Analyze => "ANALYZE",
            Self::Optimize => "OPTIMIZE",
            Self::Repair => "REPAIR",
            Self::Truncate => "TRUNCATE",
            Self::Drop => "DROP",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckOption {
    Quick,
    Fast,
    Changed,
    Medium,
    Extended,
    ForUpgrade,
}

impl CheckOption {
    pub const ALL: [Self; 6] = [
        Self::Quick,
        Self::Fast,
        Self::Changed,
        Self::Medium,
        Self::Extended,
        Self::ForUpgrade,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Quick => "快速（QUICK）",
            Self::Fast => "异常关闭（FAST）",
            Self::Changed => "有改动或异常关闭（CHANGED）",
            Self::Medium => "常规扫描（MEDIUM）",
            Self::Extended => "完整索引检查（EXTENDED）",
            Self::ForUpgrade => "升级兼容性检查（FOR UPGRADE）",
        }
    }

    fn keyword(self) -> &'static str {
        match self {
            Self::Quick => "QUICK",
            Self::Fast => "FAST",
            Self::Changed => "CHANGED",
            Self::Medium => "MEDIUM",
            Self::Extended => "EXTENDED",
            Self::ForUpgrade => "FOR UPGRADE",
        }
    }
}

#[derive(Default)]
pub struct DiagnosticOptions {
    pub checks: Vec<CheckOption>,
    pub analyze_local: bool,
    pub optimize_local: bool,
    pub repair_local: bool,
    pub quick_repair: bool,
    pub extended_repair: bool,
    pub use_frm: bool,
}

impl DiagnosticOptions {
    pub fn sql<'a>(
        &self,
        action: TableAction,
        database: &str,
        tables: impl IntoIterator<Item = &'a str>,
    ) -> Option<String> {
        if database.is_empty() || !TableAction::DIAGNOSTICS.contains(&action) {
            return None;
        }
        let tables: Vec<_> = tables
            .into_iter()
            .map(|table| qualified_table(database, table))
            .collect();
        if tables.is_empty() {
            return None;
        }
        let local_only = match action {
            TableAction::Analyze => self.analyze_local,
            TableAction::Optimize => self.optimize_local,
            TableAction::Repair => self.repair_local,
            _ => false,
        };
        let local = if local_only { " LOCAL" } else { "" };
        let mut sql = format!("{}{local} TABLE {}", action.keyword(), tables.join(", "));
        if action == TableAction::Check {
            for option in CheckOption::ALL {
                if self.checks.contains(&option) {
                    sql.push(' ');
                    sql.push_str(option.keyword());
                }
            }
        }
        if action == TableAction::Repair {
            if self.quick_repair {
                sql.push_str(" QUICK");
            }
            if self.extended_repair {
                sql.push_str(" EXTENDED");
            }
            if self.use_frm {
                sql.push_str(" USE_FRM");
            }
        }
        Some(sql)
    }
}

pub fn create_database(name: &str, charset: Option<&str>, collation: Option<&str>) -> String {
    let mut sql = format!("CREATE DATABASE {}", quote_identifier(name));
    append_database_defaults(&mut sql, charset, collation);
    sql
}

pub fn alter_database(
    name: &str,
    charset: Option<&str>,
    collation: Option<&str>,
) -> Result<String, &'static str> {
    let mut sql = format!("ALTER DATABASE {}", quote_identifier(name));
    let prefix_length = sql.len();
    append_database_defaults(&mut sql, charset, collation);
    if sql.len() == prefix_length {
        return Err("请填写字符集或排序规则");
    }
    Ok(sql)
}

fn append_database_defaults(sql: &mut String, charset: Option<&str>, collation: Option<&str>) {
    if let Some(charset) = charset.filter(|value| !value.is_empty()) {
        sql.push_str(" DEFAULT CHARACTER SET ");
        sql.push_str(&quote_identifier(charset));
    }
    if let Some(collation) = collation.filter(|value| !value.is_empty()) {
        sql.push_str(" DEFAULT COLLATE ");
        sql.push_str(&quote_identifier(collation));
    }
}

pub fn drop_database(name: &str) -> String {
    format!("DROP DATABASE {}", quote_identifier(name))
}

pub fn rename_table(database: &str, table: &str, new_table: &str) -> String {
    format!(
        "RENAME TABLE {} TO {}",
        qualified_table(database, table),
        qualified_table(database, new_table)
    )
}

pub fn copy_table_structure(
    database: &str,
    table: &str,
    target_database: &str,
    target_table: &str,
) -> String {
    format!(
        "CREATE TABLE {} LIKE {}",
        qualified_table(target_database, target_table),
        qualified_table(database, table)
    )
}

fn qualified_table(database: &str, table: &str) -> String {
    format!("{}.{}", quote_identifier(database), quote_identifier(table))
}

pub fn show_grants(user: &str, host: &str) -> String {
    format!("SHOW GRANTS FOR {}", account_name(user, host))
}

fn account_name(user: &str, host: &str) -> String {
    fn account_part(value: &str) -> String {
        if value.is_empty() {
            "''".to_owned()
        } else {
            quote_identifier(value)
        }
    }

    format!("{}@{}", account_part(user), account_part(host))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Privilege {
    Select,
    Insert,
    Update,
    Delete,
    Create,
    Alter,
    Drop,
    Index,
    References,
    Execute,
    CreateView,
    ShowView,
    Trigger,
    Event,
    Process,
    Reload,
    All,
}

impl Privilege {
    pub const ALL: [Self; 17] = [
        Self::Select,
        Self::Insert,
        Self::Update,
        Self::Delete,
        Self::Create,
        Self::Alter,
        Self::Drop,
        Self::Index,
        Self::References,
        Self::Execute,
        Self::CreateView,
        Self::ShowView,
        Self::Trigger,
        Self::Event,
        Self::Process,
        Self::Reload,
        Self::All,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Select => "查询数据",
            Self::Insert => "插入数据",
            Self::Update => "更新数据",
            Self::Delete => "删除数据",
            Self::Create => "创建库表",
            Self::Alter => "修改表结构",
            Self::Drop => "删除库表",
            Self::Index => "管理索引",
            Self::References => "引用外键",
            Self::Execute => "执行存储例程",
            Self::CreateView => "创建视图",
            Self::ShowView => "查看视图定义",
            Self::Trigger => "管理触发器",
            Self::Event => "管理事件",
            Self::Process => "查看所有进程",
            Self::Reload => "重新加载",
            Self::All => "全部权限",
        }
    }

    pub fn sql(self) -> &'static str {
        match self {
            Self::Select => "SELECT",
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
            Self::Create => "CREATE",
            Self::Alter => "ALTER",
            Self::Drop => "DROP",
            Self::Index => "INDEX",
            Self::References => "REFERENCES",
            Self::Execute => "EXECUTE",
            Self::CreateView => "CREATE VIEW",
            Self::ShowView => "SHOW VIEW",
            Self::Trigger => "TRIGGER",
            Self::Event => "EVENT",
            Self::Process => "PROCESS",
            Self::Reload => "RELOAD",
            Self::All => "ALL PRIVILEGES",
        }
    }
}

#[derive(Clone, Copy)]
pub enum PrivilegeScope<'a> {
    Global,
    Database(&'a str),
    Table {
        database: &'a str,
        table: &'a str,
    },
    Columns {
        database: &'a str,
        table: &'a str,
        columns: &'a [&'a str],
    },
}

pub fn grant_privileges(
    user: &str,
    host: &str,
    scope: PrivilegeScope<'_>,
    privileges: &[Privilege],
) -> Result<String, &'static str> {
    let on = privileges_on(scope, privileges)?;
    Ok(format!("GRANT {on} TO {}", account_name(user, host)))
}

pub fn revoke_privileges(
    user: &str,
    host: &str,
    scope: PrivilegeScope<'_>,
    privileges: &[Privilege],
) -> Result<String, &'static str> {
    let on = privileges_on(scope, privileges)?;
    Ok(format!("REVOKE {on} FROM {}", account_name(user, host)))
}

fn privileges_on(
    scope: PrivilegeScope<'_>,
    privileges: &[Privilege],
) -> Result<String, &'static str> {
    if privileges.is_empty() {
        return Err("请至少选择一项权限");
    }
    if privileges.contains(&Privilege::All) && privileges.len() != 1 {
        return Err("请选择「全部权限」这一项");
    }

    let (target, column_list) = match scope {
        PrivilegeScope::Global => ("*.*".to_owned(), String::new()),
        PrivilegeScope::Database(database) => {
            // Database-level privileges interpret these characters as wildcards.
            let literal = database.replace('_', "\\_").replace('%', "\\%");
            (format!("{}.*", quote_identifier(&literal)), String::new())
        }
        PrivilegeScope::Table { database, table } => {
            (qualified_table(database, table), String::new())
        }
        PrivilegeScope::Columns {
            database,
            table,
            columns,
        } => {
            if columns.is_empty() || columns.iter().any(|name| name.is_empty()) {
                return Err("请至少填写一列");
            }
            if !privileges.iter().all(|privilege| {
                matches!(
                    privilege,
                    Privilege::Select
                        | Privilege::Insert
                        | Privilege::Update
                        | Privilege::References
                )
            }) {
                return Err("列级权限仅支持查询、插入、更新和外键引用");
            }
            let names = columns
                .iter()
                .map(|name| quote_identifier(name))
                .collect::<Vec<_>>();
            (
                qualified_table(database, table),
                format!(" ({})", names.join(", ")),
            )
        }
    };

    let clauses = privileges
        .iter()
        .map(|privilege| format!("{}{column_list}", privilege.sql()))
        .collect::<Vec<_>>();
    Ok(format!("{} ON {target}", clauses.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flush_all_builds_supported_statements_and_releases_the_read_lock() {
        let selected = [false; FlushTarget::ALL.len()];
        let maria = FlushCapabilities::from_version("5.5.5-11.4.13-MariaDB");
        let plan = FlushPlan::new(maria, &selected, true, true).unwrap();
        assert_eq!(plan.targets(), 9);
        assert_eq!(
            plan.statements(),
            [
                "FLUSH NO_WRITE_TO_BINLOG LOGS",
                "FLUSH NO_WRITE_TO_BINLOG HOSTS",
                "FLUSH NO_WRITE_TO_BINLOG PRIVILEGES",
                "FLUSH NO_WRITE_TO_BINLOG TABLES",
                "FLUSH NO_WRITE_TO_BINLOG STATUS",
                "FLUSH NO_WRITE_TO_BINLOG DES_KEY_FILE",
                "FLUSH NO_WRITE_TO_BINLOG QUERY CACHE",
                "FLUSH NO_WRITE_TO_BINLOG USER_RESOURCES",
                "FLUSH NO_WRITE_TO_BINLOG TABLES WITH READ LOCK",
                "UNLOCK TABLES",
            ]
        );
        let mysql = FlushCapabilities::from_version("8.4.6");
        let plan = FlushPlan::new(mysql, &selected, true, false).unwrap();
        assert_eq!(
            plan.statements(),
            [
                "FLUSH LOGS",
                "FLUSH PRIVILEGES",
                "FLUSH TABLES",
                "FLUSH STATUS",
                "FLUSH USER_RESOURCES",
                "FLUSH TABLES WITH READ LOCK",
                "UNLOCK TABLES",
            ]
        );
    }

    #[test]
    fn diagnostic_sql_quotes_multiple_tables_and_applies_the_selected_action_options() {
        let tables = ["active", "old`rows"];
        let options = DiagnosticOptions {
            checks: vec![
                CheckOption::Fast,
                CheckOption::Quick,
                CheckOption::ForUpgrade,
            ],
            analyze_local: true,
            optimize_local: true,
            repair_local: true,
            quick_repair: true,
            extended_repair: true,
            use_frm: true,
        };
        let cases = [
            (
                TableAction::Check,
                "CHECK TABLE `shop`.`active`, `shop`.`old``rows` QUICK FAST FOR UPGRADE",
            ),
            (
                TableAction::Analyze,
                "ANALYZE LOCAL TABLE `shop`.`active`, `shop`.`old``rows`",
            ),
            (
                TableAction::Optimize,
                "OPTIMIZE LOCAL TABLE `shop`.`active`, `shop`.`old``rows`",
            ),
            (
                TableAction::Repair,
                "REPAIR LOCAL TABLE `shop`.`active`, `shop`.`old``rows` QUICK EXTENDED USE_FRM",
            ),
        ];
        for (action, expected) in cases {
            assert_eq!(
                options.sql(action, "shop", tables).as_deref(),
                Some(expected)
            );
        }
    }

    #[test]
    fn generated_admin_statements_quote_names_and_escape_schema_wildcards() {
        assert_eq!(
            TableAction::Truncate.sql("shop", "line`item"),
            "TRUNCATE TABLE `shop`.`line``item`"
        );
        assert_eq!(
            grant_privileges(
                "an`alyst",
                "%",
                PrivilegeScope::Database("sales_2026%"),
                &[Privilege::Select]
            ),
            Ok("GRANT SELECT ON `sales\\_2026\\%`.* TO `an``alyst`@`%`".to_owned()),
        );
    }
}
