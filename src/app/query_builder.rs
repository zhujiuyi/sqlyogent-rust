use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use serde::{Deserialize, Serialize};

use crate::database::{DbObject, ObjectKind, quote_identifier};
use crate::sql_tools::{Comparison, mysql_text_literal, wrap_sql_expression};

#[path = "query_builder/canvas.rs"]
mod canvas;
#[path = "query_builder/layout.rs"]
mod layout;
#[path = "query_builder/metadata.rs"]
mod metadata;
#[path = "query_builder/panes.rs"]
mod panes;
#[path = "query_builder/view.rs"]
mod view;

pub(super) use layout::pick_layout;
pub(super) use view::{Action, Destination};

use super::state::Workspace;

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
struct Column {
    table: usize,
    name: String,
}

struct Choice {
    column: Column,
    label: String,
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
struct Source {
    id: usize,
    #[serde(with = "layout::StoredObject")]
    object: DbObject,
    alias: String,
    position: [f32; 2],
    size: [f32; 2],
}

impl Source {
    fn name(&self) -> String {
        format!("{}.{}", self.object.schema, self.object.name)
    }

    fn label(&self) -> &str {
        let alias = self.alias.trim();
        if alias.is_empty() {
            &self.object.name
        } else {
            alias
        }
    }

    fn sql(&self, options: SqlOptions) -> String {
        let qualified = format!(
            "{}.{}",
            options.identifier(&self.object.schema),
            options.identifier(&self.object.name),
        );
        let alias = self.alias.trim();
        if alias.is_empty() {
            qualified
        } else {
            format!("{qualified} AS {}", options.identifier(alias))
        }
    }
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq, Serialize)]
enum JoinKind {
    Inner,
    Left,
    Right,
}

impl JoinKind {
    const ALL: [Self; 3] = [Self::Inner, Self::Left, Self::Right];

    fn label(self) -> &'static str {
        match self {
            Self::Inner => "内连接（INNER JOIN）",
            Self::Left => "左连接（LEFT JOIN）",
            Self::Right => "右连接（RIGHT JOIN）",
        }
    }

    fn sql(self) -> &'static str {
        match self {
            Self::Inner => "INNER JOIN",
            Self::Left => "LEFT JOIN",
            Self::Right => "RIGHT JOIN",
        }
    }

    fn reversed(self) -> Self {
        match self {
            Self::Inner => Self::Inner,
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

#[derive(Clone, Default, Deserialize, Eq, PartialEq, Serialize)]
struct JoinCondition {
    left: Option<Column>,
    right: Option<Column>,
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
struct Join {
    left: usize,
    right: usize,
    kind: JoinKind,
    conditions: Vec<JoinCondition>,
}

impl Join {
    fn contains(&self, id: usize) -> bool {
        self.left == id || self.right == id
    }

    fn previous(&self, id: usize, included: &HashSet<usize>) -> Option<JoinKind> {
        if self.right == id && included.contains(&self.left) {
            Some(self.kind)
        } else if self.left == id && included.contains(&self.right) {
            Some(self.kind.reversed())
        } else {
            None
        }
    }
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
enum FieldInput {
    Column(Column),
    Sql { text: String, aggregate: bool },
}

impl FieldInput {
    fn is_aggregate(&self) -> bool {
        matches!(
            self,
            Self::Sql {
                aggregate: true,
                ..
            }
        )
    }
}

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq, Serialize)]
enum Aggregate {
    #[default]
    None,
    CountRows,
    Count,
    CountDistinct,
    Sum,
    Avg,
    Min,
    Max,
    GroupConcat,
}

impl Aggregate {
    const ALL: [Self; 9] = [
        Self::None,
        Self::CountRows,
        Self::Count,
        Self::CountDistinct,
        Self::Sum,
        Self::Avg,
        Self::Min,
        Self::Max,
        Self::GroupConcat,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::None => "无聚合",
            Self::CountRows => "COUNT(*)",
            Self::Count => "COUNT",
            Self::CountDistinct => "COUNT DISTINCT",
            Self::Sum => "SUM",
            Self::Avg => "AVG",
            Self::Min => "MIN",
            Self::Max => "MAX",
            Self::GroupConcat => "GROUP_CONCAT",
        }
    }

    fn apply(self, argument: String) -> String {
        match self {
            Self::None => argument,
            Self::CountRows => "COUNT(*)".into(),
            Self::CountDistinct => format!("COUNT(DISTINCT {argument})"),
            function => format!("{}({argument})", function.label()),
        }
    }
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
struct Field {
    id: usize,
    input: Option<FieldInput>,
    aggregate: Aggregate,
    alias: String,
    show: bool,
}

impl Field {
    fn blank(id: usize) -> Self {
        Self {
            id,
            input: None,
            aggregate: Aggregate::None,
            alias: String::new(),
            show: false,
        }
    }

    fn is_placeholder(&self) -> bool {
        self.input.is_none() && self.aggregate == Aggregate::None && self.alias.trim().is_empty()
    }

    fn has_projection(&self) -> bool {
        self.input.is_some() || self.aggregate == Aggregate::CountRows
    }

    fn is_aggregate(&self) -> bool {
        self.aggregate != Aggregate::None
            || self.input.as_ref().is_some_and(FieldInput::is_aggregate)
    }

    fn has_group_key(&self) -> bool {
        self.aggregate != Aggregate::CountRows
            && self
                .input
                .as_ref()
                .is_some_and(|input| !input.is_aggregate())
    }

    fn sql(&self, query: &Query, group_key: bool, options: SqlOptions) -> SqlResult<String> {
        if group_key && !self.has_group_key() {
            return Err("分组需要使用列或非聚合表达式。");
        }
        if self.aggregate == Aggregate::CountRows {
            return Ok("COUNT(*)".into());
        }
        let argument = match &self.input {
            Some(FieldInput::Column(column)) => query.column_sql(column, options)?,
            Some(FieldInput::Sql { text, .. }) => sql_expression(text, options.no_backslash)?,
            None => return Err("请为结果字段选择列或填写 SQL 表达式。"),
        };
        if group_key {
            Ok(argument)
        } else {
            Ok(self.aggregate.apply(argument))
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum Target {
    Column(Column),
    Field(usize),
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
struct Sort {
    target: Target,
    descending: bool,
}

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq, Serialize)]
enum FilterPhase {
    #[default]
    Where,
    Having,
}

impl FilterPhase {
    fn label(self) -> &'static str {
        match self {
            Self::Where => "WHERE",
            Self::Having => "HAVING",
        }
    }

    fn inferred(aggregate: bool) -> Self {
        if aggregate { Self::Having } else { Self::Where }
    }
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
enum Predicate {
    Compare {
        target: Option<Target>,
        #[serde(with = "layout::StoredComparison")]
        operator: Comparison,
        value: String,
        numeric: bool,
    },
    Sql {
        text: String,
        phase: FilterPhase,
    },
}

impl Default for Predicate {
    fn default() -> Self {
        Self::Compare {
            target: None,
            operator: Comparison::Equal,
            value: String::new(),
            numeric: false,
        }
    }
}

impl Predicate {
    fn sql(&self, query: &Query, options: SqlOptions) -> SqlResult<(FilterPhase, String)> {
        match self {
            Self::Compare {
                target,
                operator,
                value,
                numeric,
            } => {
                let target = target.as_ref().ok_or("请为筛选条件选择字段。")?;
                let (expression, aggregate) = query.target_sql(target, false, options)?;
                let mut sql = format!("{expression} {}", comparison_sql(*operator));
                if takes_value(*operator) {
                    sql.push(' ');
                    if *numeric {
                        let value = value.trim();
                        if value.parse::<f64>().is_err()
                            || !value.bytes().any(|byte| byte.is_ascii_digit())
                        {
                            return Err("请填写有效的数字筛选值。");
                        }
                        sql.push_str(value);
                    } else {
                        sql.push_str(&mysql_text_literal(value));
                    }
                }
                Ok((FilterPhase::inferred(aggregate), sql))
            }
            Self::Sql { text, phase } => Ok((*phase, sql_expression(text, options.no_backslash)?)),
        }
    }
}

const OPERATORS: [(Comparison, &str); 9] = [
    (Comparison::Equal, "="),
    (Comparison::NotEqual, "<>"),
    (Comparison::LessThan, "<"),
    (Comparison::LessOrEqual, "<="),
    (Comparison::GreaterThan, ">"),
    (Comparison::GreaterOrEqual, ">="),
    (Comparison::Like, "LIKE"),
    (Comparison::IsNull, "IS NULL"),
    (Comparison::IsNotNull, "IS NOT NULL"),
];

fn comparison_sql(comparison: Comparison) -> &'static str {
    OPERATORS
        .iter()
        .find(|(value, _)| *value == comparison)
        .map_or("=", |(_, label)| *label)
}

fn takes_value(comparison: Comparison) -> bool {
    !matches!(comparison, Comparison::IsNull | Comparison::IsNotNull)
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Section {
    Connections,
    #[default]
    Fields,
    Filter,
    GroupAndSort,
}

pub(super) struct Builder {
    query: Query,
    canvas: canvas::Canvas,
    pane_sizes: panes::PaneSizes,
    metadata: metadata::Metadata,
    section: Section,
    view: Option<String>,
    linked_tab: Option<u64>,
    path: Option<PathBuf>,
    dirty: bool,
    status: Option<Result<String, String>>,
    manual_join: [Option<Column>; 2],
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
struct Query {
    schema: Option<String>,
    sources: Vec<Source>,
    joins: Vec<Join>,
    next_table: usize,
    fields: Vec<Field>,
    next_field: usize,
    filters: Vec<Vec<Predicate>>,
    groups: Vec<Target>,
    sorts: Vec<Sort>,
    distinct: bool,
    limit_rows: bool,
    limit: u32,
    camera: canvas::Camera,
}

type SqlResult<T> = Result<T, &'static str>;

#[derive(Clone, Copy)]
struct SqlOptions {
    no_backslash: bool,
    quote_identifiers: bool,
}

impl SqlOptions {
    fn identifier(self, name: &str) -> Cow<'_, str> {
        let mut bytes = name.bytes();
        if !self.quote_identifiers
            && bytes
                .next()
                .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
            && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            && !RESERVED_NAMES.contains(name.to_ascii_uppercase().as_str())
        {
            Cow::Borrowed(name)
        } else {
            Cow::Owned(quote_identifier(name))
        }
    }
}

// Keep MySQL and MariaDB keywords, names affected by IGNORE_SPACE, and character-set
// introducers quoted across input layouts and sessions.
static RESERVED_NAMES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    "
    ACCESSIBLE ADD ADDDATE ADMIN ALL ALTER ANALYZE AND ARRAY AS ASC ASENSITIVE BEFORE
    BETWEEN BIGINT BINARY BIT_AND BIT_OR BIT_XOR BLOB BODY BOTH BY CALL CASCADE CASE CAST
    CHANGE CHAR CHARACTER CHECK COLLATE COLUMN CONDITION CONSTRAINT CONTINUE CONVERSION
    CONVERT COUNT CREATE CROSS CUBE CUME_DIST CURDATE CURRENT_DATE CURRENT_PATH CURRENT_ROLE
    CURRENT_TIME CURRENT_TIMESTAMP CURRENT_USER CURSOR CURTIME DATABASE DATABASES DATE_ADD
    DATE_SUB DAY_HOUR DAY_MICROSECOND DAY_MINUTE DAY_SECOND DEC DECIMAL DECLARE DEFAULT
    DELAYED DELETE DELETE_DOMAIN_ID DENSE_RANK DESC DESCRIBE DETERMINISTIC DISTINCT
    DISTINCTROW DIV DOUBLE DO_DOMAIN_IDS DROP DUAL EACH ELSE ELSEIF ELSIF EMPTY ENCLOSED
    ESCAPED EXCEPT EXISTS EXIT EXPLAIN EXTERNAL EXTRACT FALSE FETCH FIRST_VALUE FLOAT FLOAT4
    FLOAT8 FOR FORCE FOREIGN FROM FULLTEXT FUNCTION GENERATED GET GET_MASTER_PUBLIC_KEY GOTO
    GRANT GROUP GROUPING GROUPS GROUP_CONCAT HAVING HIGH_PRIORITY HOUR_MICROSECOND
    HOUR_MINUTE HOUR_SECOND IF IGNORE IGNORE_DOMAIN_IDS IN INDEX INFILE INNER INOUT
    INSENSITIVE INSERT INT INT1 INT2 INT3 INT4 INT8 INTEGER INTERSECT INTERVAL INTO
    IO_AFTER_GTIDS IO_BEFORE_GTIDS IS ITERATE JOIN JSON_ARRAYAGG JSON_OBJECTAGG JSON_TABLE
    KEY KEYS KILL LAG LAST_VALUE LATERAL LEAD LEADING LEAVE LEFT LIBRARY LIKE LIMIT LINEAR
    LINES LOAD LOCALTIME LOCALTIMESTAMP LOCK LONG LONGBLOB LONGTEXT LOOP LOW_PRIORITY MANUAL
    MASTER_BIND MASTER_DEMOTE_TO_REPLICA MASTER_DEMOTE_TO_SLAVE
    MASTER_SSL_VERIFY_SERVER_CERT MATCH MAX MAXVALUE MEDIAN MEDIUMBLOB MEDIUMINT MEDIUMTEXT
    MEMBER MID MIDDLEINT MIN MINUS MINUTE_MICROSECOND MINUTE_SECOND MOD MODIFIES NATURAL NOT
    NOW NO_WRITE_TO_BINLOG NTH_VALUE NTILE NULL NUMERIC OF OFFSET ON OPTIMIZE
    OPTIMIZER_COSTS OPTION OPTIONALLY OR ORDER OTHERS OUT OUTER OUTFILE OVER PACKAGE
    PAGE_CHECKSUM PARALLEL PARSE_GCOL_EXPR PARSE_VCOL_EXPR PARTITION PERCENTILE_CONT
    PERCENTILE_DISC PERCENT_RANK PERSIST_ONLY PORTION POSITION PRECISION PRIMARY PRIOR
    PROCEDURE PURGE QUALIFY RAISE RANGE RANK READ READS READ_WRITE REAL RECURSIVE REFERENCES
    REF_SYSTEM_ID REGEXP RELEASE RENAME REPEAT REPLACE REQUIRE RESIGNAL RESTRICT RETURN
    RETURNING REVOKE RIGHT RLIKE ROW ROWNUM ROWS ROWTYPE ROW_NUMBER SCHEMA SCHEMAS
    SECOND_MICROSECOND SELECT SENSITIVE SEPARATOR SESSION_USER SET SHOW SIGNAL SMALLINT
    SPATIAL SPECIFIC SQL SQLEXCEPTION SQLSTATE SQLWARNING SQL_AFTER_GTIDS SQL_BEFORE_GTIDS
    SQL_BIG_RESULT SQL_CACHE SQL_CALC_FOUND_ROWS SQL_NO_CACHE SQL_SMALL_RESULT SSL STARTING
    STATS_AUTO_RECALC STATS_PERSISTENT STATS_SAMPLE_PAGES STD STDDEV STDDEV_POP STDDEV_SAMP
    STORED STRAIGHT_JOIN ST_COLLECT SUBDATE SUBSTR SUBSTRING SUM SYSDATE SYSTEM SYSTEM_USER
    TABLE TABLESAMPLE TERMINATED THEN TINYBLOB TINYINT TINYTEXT TO TO_DATE TRAILING TRIGGER
    TRIM TRIM_ORACLE TRUE UNDO UNION UNIQUE UNLOCK UNSIGNED UPDATE USAGE USE USING UTC_DATE
    UTC_TIME UTC_TIMESTAMP VALUES VARBINARY VARCHAR VARCHARACTER VARIANCE VARYING VAR_POP
    VAR_SAMP VECTOR VIRTUAL WHEN WHERE WHILE WINDOW WITH WRITE XOR YEAR_MONTH ZEROFILL
    _ARMSCII8 _ASCII _BIG5 _BINARY _CP1250 _CP1251 _CP1256 _CP1257 _CP850 _CP852 _CP866
    _CP932 _DEC8 _EUCJPMS _EUCKR _FILENAME _GB18030 _GB2312 _GBK _GEOSTD8 _GREEK _HEBREW
    _HP8 _KEYBCS2 _KOI8R _KOI8U _LATIN1 _LATIN2 _LATIN5 _LATIN7 _MACCE _MACROMAN _SJIS _SWE7
    _TIS620 _UCS2 _UJIS _UTF16 _UTF16LE _UTF32 _UTF8 _UTF8MB3 _UTF8MB4"
        .split_ascii_whitespace()
        .collect()
});

impl Builder {
    pub(super) fn new(object: DbObject) -> Self {
        let mut builder = Self::new_blank(Some(object.schema.clone()));
        builder.add_source(object, None);
        builder.dirty = false;
        builder
    }

    pub(super) fn new_blank(schema: Option<String>) -> Self {
        Self {
            query: Query {
                schema,
                sources: Vec::new(),
                joins: Vec::new(),
                next_table: 1,
                fields: Vec::new(),
                next_field: 1,
                filters: Vec::new(),
                groups: Vec::new(),
                sorts: Vec::new(),
                distinct: false,
                limit_rows: true,
                limit: 100,
                camera: canvas::Camera::default(),
            },
            canvas: canvas::Canvas::default(),
            pane_sizes: panes::PaneSizes::default(),
            metadata: metadata::Metadata::default(),
            section: Section::default(),
            view: None,
            linked_tab: None,
            path: None,
            dirty: false,
            status: None,
            manual_join: [None, None],
        }
    }

    pub(super) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub(super) fn dirty(&self) -> bool {
        self.dirty
    }

    pub(super) fn linked_query(&self, workspace: &Workspace) -> Option<(usize, bool)> {
        let id = self.linked_tab?;
        workspace.tabs.iter().enumerate().find_map(|(index, tab)| {
            (tab.id == id && tab.path.is_none()).then(|| (index, tab.sql != tab.saved))
        })
    }

    pub(super) fn link_query(&mut self, tab: u64) {
        self.linked_tab = Some(tab);
    }

    pub(super) fn receive_metadata(&mut self, workspace: &Workspace) {
        if self.metadata.loading() || workspace.server.is_none() {
            self.metadata.receive(&self.query, workspace);
        }
    }

    pub(super) fn load_metadata(&mut self, workspace: &mut Workspace) -> bool {
        self.metadata.receive(&self.query, workspace);
        let had_schema = self.query.schema.is_some();
        let joined = self.metadata.request(&mut self.query, workspace);
        self.dirty |= joined;
        joined || !had_schema && self.query.schema.is_some()
    }

    fn add_source(&mut self, object: DbObject, position: Option<[f32; 2]>) {
        if self.query.sources.len() >= layout::TABLE_LIMIT {
            self.status = Some(Err(format!(
                "单个查询布局最多支持 {} 个数据源。",
                layout::TABLE_LIMIT
            )));
            return;
        }
        if let Some(id) = self.query.add_source(object, position) {
            self.metadata.auto_join.insert(id);
            self.dirty = true;
        }
    }

    fn connect(&mut self, left: Column, right: Column) {
        if left.table != right.table {
            self.metadata.auto_join.remove(&left.table);
            self.metadata.auto_join.remove(&right.table);
            self.query.connect(left, right);
        }
    }

    fn remove_source(&mut self, id: usize) {
        self.query.remove_source(id);
        self.metadata.auto_join.remove(&id);
        self.canvas.remove_source(id);
        self.clear_manual_selection(id);
    }

    fn clear_manual_selection(&mut self, id: usize) {
        for selected in &mut self.manual_join {
            if selected.as_ref().is_some_and(|column| column.table == id) {
                *selected = None;
            }
        }
    }

    fn choices(&self) -> Vec<Choice> {
        self.query
            .sources
            .iter()
            .flat_map(|source| {
                self.metadata
                    .columns(&source.object)
                    .iter()
                    .map(move |name| {
                        let label =
                            if !source.alias.trim().is_empty() || self.query.sources.len() > 1 {
                                format!("{}.{name}", source.label())
                            } else {
                                name.clone()
                            };
                        Choice {
                            column: Column {
                                table: source.id,
                                name: name.clone(),
                            },
                            label,
                        }
                    })
            })
            .collect()
    }
}

impl Query {
    fn source(&self, id: usize) -> Option<&Source> {
        self.sources.iter().find(|source| source.id == id)
    }

    fn add_source(&mut self, object: DbObject, position: Option<[f32; 2]>) -> Option<usize> {
        if self.sources.len() >= layout::TABLE_LIMIT
            || self.next_table >= layout::ID_LIMIT
            || !matches!(object.kind, ObjectKind::Table | ObjectKind::View)
        {
            return None;
        }
        let id = self.next_table;
        self.next_table += 1;
        let alias = if self.sources.is_empty() {
            String::new()
        } else {
            for index in 0..self.sources.len() {
                if self.sources[index].alias.trim().is_empty() {
                    self.sources[index].alias = self.free_alias(self.sources[index].id);
                }
            }
            self.free_alias(id)
        };
        self.sources.push(Source {
            id,
            object,
            alias,
            position: position.unwrap_or_else(|| canvas::free_position(&self.sources)),
            size: canvas::DEFAULT_CARD,
        });
        Some(id)
    }

    fn free_alias(&self, start: usize) -> String {
        (start..)
            .map(|id| format!("t{id}"))
            .find(|alias| {
                self.sources
                    .iter()
                    .all(|source| !source.alias.trim().eq_ignore_ascii_case(alias))
            })
            .expect("finite source aliases")
    }

    fn connect(&mut self, left: Column, right: Column) {
        if left.table != right.table {
            let (from, to) = (left.table, right.table);
            let relation = self
                .joins
                .iter_mut()
                .find(|join| join.contains(from) && join.contains(to));
            let condition = JoinCondition {
                left: Some(left),
                right: Some(right),
            };
            if let Some(join) = relation {
                let condition = if join.left == from {
                    condition
                } else {
                    JoinCondition {
                        left: condition.right,
                        right: condition.left,
                    }
                };
                if !join.conditions.contains(&condition) {
                    join.conditions.push(condition);
                }
            } else {
                self.joins.push(Join {
                    left: from,
                    right: to,
                    kind: JoinKind::Inner,
                    conditions: vec![condition],
                });
            }
        }
    }

    fn remove_source(&mut self, id: usize) {
        self.sources.retain(|source| source.id != id);
        self.joins.retain(|join| !join.contains(id));
        let mut removed_fields = HashSet::new();
        self.fields.retain_mut(|field| {
            if !matches!(&field.input, Some(FieldInput::Column(column)) if column.table == id) {
                return true;
            }
            if field.aggregate == Aggregate::CountRows {
                field.input = None;
                true
            } else {
                removed_fields.insert(field.id);
                false
            }
        });
        self.retain_targets(|target| match target {
            Target::Column(column) => column.table != id,
            Target::Field(field) => !removed_fields.contains(field),
        });
    }

    fn add_field(&mut self, input: Option<FieldInput>, aggregate: Aggregate) {
        self.insert_field(None, input, aggregate);
    }

    fn insert_field(
        &mut self,
        before: Option<usize>,
        input: Option<FieldInput>,
        aggregate: Aggregate,
    ) {
        if self.next_field >= layout::ID_LIMIT {
            return;
        }
        let index = match before {
            Some(id) => {
                let Some(index) = self.fields.iter().position(|field| field.id == id) else {
                    return;
                };
                index
            }
            None => self.fields.len(),
        };
        let mut field = Field {
            input,
            aggregate,
            ..Field::blank(self.next_field)
        };
        field.show = field.has_projection();
        self.fields.insert(index, field);
        self.next_field += 1;
    }

    fn remove_field(&mut self, index: usize) {
        let field = self.fields.remove(index);
        self.retain_targets(|target| *target != Target::Field(field.id));
    }

    fn clear_fields(&mut self) {
        self.fields.clear();
        self.retain_targets(|target| matches!(target, Target::Column(_)));
    }

    fn reset_field_slots(&mut self) {
        for field in &mut self.fields {
            *field = Field::blank(field.id);
        }
        self.retain_targets(|target| matches!(target, Target::Column(_)));
    }

    fn retain_targets(&mut self, keep: impl Fn(&Target) -> bool) {
        self.groups.retain(&keep);
        self.sorts.retain(|sort| keep(&sort.target));
        self.filters.retain_mut(|group| {
            group.retain(|filter| match filter {
                Predicate::Compare { target, .. } => target.as_ref().is_none_or(&keep),
                Predicate::Sql { .. } => true,
            });
            !group.is_empty()
        });
    }

    fn column_sql(&self, column: &Column, options: SqlOptions) -> SqlResult<String> {
        let source = self
            .source(column.table)
            .ok_or("所选列的数据源已移除，请重新选择列。")?;
        let name = options.identifier(&column.name);
        let alias = source.alias.trim();
        if alias.is_empty() {
            Ok(name.into_owned())
        } else {
            Ok(format!("{}.{name}", options.identifier(alias)))
        }
    }

    fn target_sql(
        &self,
        target: &Target,
        group_key: bool,
        options: SqlOptions,
    ) -> SqlResult<(String, bool)> {
        match target {
            Target::Column(column) => Ok((self.column_sql(column, options)?, false)),
            Target::Field(id) => {
                let field = self
                    .fields
                    .iter()
                    .find(|field| field.id == *id)
                    .ok_or("引用的结果字段已移除，请重新选择字段。")?;
                Ok((field.sql(self, group_key, options)?, field.is_aggregate()))
            }
        }
    }

    #[cfg(test)]
    fn sql(&self, no_backslash: bool) -> SqlResult<String> {
        self.sql_with_quoting(no_backslash, true)
    }

    fn sql_with_quoting(&self, no_backslash: bool, quote_identifiers: bool) -> SqlResult<String> {
        let options = SqlOptions {
            no_backslash,
            quote_identifiers,
        };
        let base = self.sources.first().ok_or("请先添加一张表或视图。")?;
        let mut aliases = HashSet::new();
        for source in &self.sources {
            let alias = source.alias.trim();
            if self.sources.len() > 1 && alias.is_empty() {
                return Err("多表查询要求为每张表填写别名。");
            }
            if alias.contains('\0') || !aliases.insert(alias.to_lowercase()) {
                return Err("表别名须唯一，且不能包含 NUL 字符。");
            }
        }

        let groups = self
            .groups
            .iter()
            .map(|group| self.target_sql(group, true, options).map(|(sql, _)| sql))
            .collect::<SqlResult<Vec<_>>>()?;
        let (where_groups, having_groups) = self.compile_filters(options)?;
        let mut projection = Vec::new();
        for field in self
            .fields
            .iter()
            .filter(|field| field.show && !field.is_placeholder())
        {
            let mut expression = field.sql(self, false, options)?;
            let alias = field.alias.trim();
            if !alias.is_empty() {
                if alias.contains('\0') {
                    return Err("结果字段别名不能包含 NUL 字符。");
                }
                let _ = write!(expression, " AS {}", options.identifier(alias));
            }
            projection.push(expression);
        }
        if projection.is_empty() {
            if groups.is_empty() && having_groups.is_empty() {
                projection.push("*".into());
            } else {
                projection.extend(groups.iter().cloned());
                projection.push(format!("COUNT(*) AS {}", options.identifier("记录数")));
            }
        }

        let distinct = if self.distinct { "DISTINCT " } else { "" };
        let mut sql = format!(
            "SELECT {distinct}{}\nFROM {}",
            projection.join(", "),
            base.sql(options)
        );
        self.append_joins(&mut sql, base.id, options)?;
        append_filters(&mut sql, FilterPhase::Where, &where_groups);
        if !groups.is_empty() {
            let _ = write!(sql, "\nGROUP BY {}", groups.join(", "));
        }
        append_filters(&mut sql, FilterPhase::Having, &having_groups);

        let orders = self
            .sorts
            .iter()
            .map(|sort| {
                let (expression, _) = self.target_sql(&sort.target, false, options)?;
                let direction = if sort.descending { "DESC" } else { "ASC" };
                Ok(format!("{expression} {direction}"))
            })
            .collect::<SqlResult<Vec<_>>>()?;
        if !orders.is_empty() {
            let _ = write!(sql, "\nORDER BY {}", orders.join(", "));
        }
        if self.limit_rows {
            let _ = write!(sql, "\nLIMIT {}", self.limit);
        }
        sql.push(';');
        Ok(sql)
    }

    fn append_joins(&self, sql: &mut String, base: usize, options: SqlOptions) -> SqlResult<()> {
        let mut included = HashSet::from([base]);
        let mut neighbors = HashMap::<usize, Vec<&Join>>::new();
        for join in &self.joins {
            neighbors.entry(join.left).or_default().push(join);
            neighbors.entry(join.right).or_default().push(join);
        }
        while included.len() < self.sources.len() {
            let Some(source) = self.sources.iter().find(|source| {
                !included.contains(&source.id)
                    && neighbors.get(&source.id).is_some_and(|joins| {
                        joins
                            .iter()
                            .any(|join| join.previous(source.id, &included).is_some())
                    })
            }) else {
                return Err("请连接每张表或视图，再生成多表查询。");
            };
            let mut links = neighbors[&source.id].iter().filter_map(|join| {
                join.previous(source.id, &included)
                    .map(|kind| (*join, kind))
            });
            let (first, kind) = links.next().expect("a connected source");
            let _ = write!(sql, "\n{} {}", kind.sql(), source.sql(options));
            let mut keyword = "ON";
            for (join, relation_kind) in std::iter::once((first, kind)).chain(links) {
                if kind != relation_kind {
                    return Err("同一张表连接到多个已加入的表时，连接类型须一致。");
                }
                for condition in &join.conditions {
                    let (Some(left), Some(right)) = (&condition.left, &condition.right) else {
                        return Err("请为每个 JOIN 条件选择左列和右列。");
                    };
                    let (left, right) = if left.table == source.id {
                        (right, left)
                    } else {
                        (left, right)
                    };
                    let _ = write!(
                        sql,
                        "\n  {keyword} {} = {}",
                        self.column_sql(left, options)?,
                        self.column_sql(right, options)?
                    );
                    keyword = "AND";
                }
            }
            included.insert(source.id);
        }
        Ok(())
    }

    fn compile_filters(&self, options: SqlOptions) -> SqlResult<(Vec<String>, Vec<String>)> {
        let mut where_groups = Vec::new();
        let mut having_groups = Vec::new();
        for group in &self.filters {
            let mut row_conditions = Vec::new();
            let mut group_conditions = Vec::new();
            for filter in group {
                let (phase, condition) = filter.sql(self, options)?;
                match phase {
                    FilterPhase::Where => row_conditions.push(condition),
                    FilterPhase::Having => group_conditions.push(condition),
                }
            }
            if !row_conditions.is_empty() {
                where_groups.push(row_conditions.join(" AND "));
            }
            if !group_conditions.is_empty() {
                having_groups.push(group_conditions.join(" AND "));
            }
        }
        Ok((where_groups, having_groups))
    }
}

fn sql_expression(text: &str, no_backslash: bool) -> SqlResult<String> {
    wrap_sql_expression(text, no_backslash).ok_or("请填写单个 SQL 表达式，或检查表达式中的引号。")
}

fn append_filters(sql: &mut String, phase: FilterPhase, groups: &[String]) {
    let keyword = phase.label();
    match groups {
        [] => {}
        [only] => {
            let _ = write!(sql, "\n{keyword} {only}");
        }
        [first, rest @ ..] => {
            let _ = write!(sql, "\n{keyword} ({first})");
            for group in rest {
                let _ = write!(sql, "\n   OR ({group})");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::{
            BrowseAction, Document, Edit, QUERY_DESIGN_SHORTCUTS, Workbench,
            consume_exact_shortcut,
            navigation::{self, Step},
            preferences::Preferences,
            state::Workspace,
        },
        database::{ObjectKind, ServerInfo},
        sql_tools::split_script,
    };
    use eframe::egui::{self, Key, Modifiers};

    fn table(name: &str) -> DbObject {
        DbObject {
            schema: "school.regional".into(),
            name: name.into(),
            kind: ObjectKind::Table,
        }
    }

    #[test]
    fn query_builder_tabs_preserve_layouts_across_connections_and_sql_output() {
        let ctx = egui::Context::default();
        let mut app = Workbench::new(&ctx);
        let server = ServerInfo {
            version: "fixture".into(),
            connection_id: 1,
            database: Some("school.regional".into()),
            http_tunnel: false,
        };
        app.ws().server = Some(server.clone());
        app.design_tabs
            .initialize_connection(&app.workspaces[0], &Preferences::default());
        let key = app.ws().key;
        let initial_query = app.ws().tab().id;
        assert!(app.active_layout().is_none());
        let initial = app.design_tabs.documents(key).collect::<Vec<_>>();
        assert!(matches!(
            initial.as_slice(),
            [Document::Builder(_), Document::Design(_)]
        ));

        app.browse(&ctx, BrowseAction::Builder(table("class")));
        let Some(Document::Builder(first)) = app.active_layout() else {
            panic!("missing table query design");
        };
        let builder = app.design_tabs.builder_mut(first).unwrap();
        builder.section = Section::Filter;
        builder.view = Some("class_view".into());
        builder.query.sources[0].position = [198.0, 64.0];
        let sql = builder.query.sql(false).unwrap();

        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: Key::K,
                    physical_key: Some(Key::K),
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::CTRL,
                }],
                ..Default::default()
            },
            |ctx| {
                let action =
                    ctx.input_mut(|input| consume_exact_shortcut(input, &QUERY_DESIGN_SHORTCUTS));
                assert!(matches!(action, Some(Edit::NewQueryDesign)));
                app.edit(ctx, action.unwrap());
            },
        );
        let Some(Document::Builder(second)) = app.active_layout() else {
            panic!("missing new query design");
        };
        assert_ne!(first, second);
        assert!(
            app.design_tabs
                .builder_mut(second)
                .unwrap()
                .query
                .sources
                .is_empty()
        );

        let mut other = Workspace::new("其他连接".into(), None);
        other.server = Some(server);
        let other_key = other.key;
        app.workspaces.push(other);
        app.design_tabs
            .initialize_connection(&app.workspaces[1], &Preferences::default());
        app.activate_workspace(&ctx, 1);
        let mut view = table("student_view");
        view.kind = ObjectKind::View;
        app.browse(&ctx, BrowseAction::Builder(view.clone()));
        let Some(Document::Builder(other)) = app.active_layout() else {
            panic!("missing view query design");
        };
        app.activate_workspace(&ctx, 0);
        assert_eq!(app.active_layout(), Some(Document::Builder(second)));
        app.navigate(&ctx, navigation::Action::Document(Step::Previous));
        assert_eq!(app.active_layout(), Some(Document::Builder(first)));

        app.query_design_action(
            &ctx,
            first,
            Action::StageSql {
                sql: sql.clone(),
                destination: Destination::NewQuery,
            },
        );
        assert!(app.active_layout().is_none());
        assert_ne!(app.ws().tab().id, initial_query);
        assert_eq!(app.ws().tab().sql, sql);
        let builder = app.design_tabs.builder_mut(first).unwrap();
        assert_eq!(builder.query.sources[0].position, [198.0, 64.0]);
        assert!(builder.section == Section::Filter);
        assert_eq!(builder.view.as_deref(), Some("class_view"));

        app.close_workspace(0);
        assert_eq!(app.design_tabs.documents(key).count(), 0);
        assert_eq!(app.active_layout(), Some(Document::Builder(other)));
        assert_eq!(app.ws().key, other_key);
        assert_eq!(
            app.design_tabs.builder_mut(other).unwrap().query.sources[0].object,
            view
        );
    }

    #[test]
    fn updates_each_query_designs_linked_sql_without_changing_other_queries() {
        let ctx = egui::Context::default();
        let mut app = Workbench::new(&ctx);
        let key = app.ws().key;
        let unrelated = "SELECT '保留';";
        app.ws().tab_mut().sql = unrelated.into();

        let open_design = |app: &mut Workbench, name| {
            app.browse(&ctx, BrowseAction::Builder(table(name)));
            let Some(Document::Builder(id)) = app.active_layout() else {
                panic!("missing query design");
            };
            id
        };
        let stage = |app: &mut Workbench, id, sql: &str, destination| {
            app.design_tabs.select(key, Document::Builder(id));
            app.query_design_action(
                &ctx,
                id,
                Action::StageSql {
                    sql: sql.into(),
                    destination,
                },
            );
            app.ws().tab().id
        };

        let first = open_design(&mut app, "class");
        let initial = "SELECT id FROM class;";
        let first_query = stage(&mut app, first, initial, Destination::NewQuery);
        let second = open_design(&mut app, "student");
        let second_sql = "SELECT id FROM student;";
        let second_query = stage(&mut app, second, second_sql, Destination::NewQuery);
        stage(
            &mut app,
            first,
            "CREATE VIEW sample AS SELECT id FROM class;",
            Destination::ViewDraft,
        );

        let updated = "SELECT id, name FROM class;";
        assert_eq!(
            stage(&mut app, first, updated, Destination::LinkedQuery),
            first_query
        );
        let workspace = app.ws();
        assert_eq!(workspace.tab().sql, updated);
        assert_eq!(workspace.tabs[0].sql, unrelated);
        assert_eq!(workspace.tabs[2].id, second_query);
        assert_eq!(workspace.tabs[2].sql, second_sql);
        assert_eq!(workspace.tabs.len(), 4);
        let editor = crate::app::editor_tools::editor_id(workspace);
        let state = egui::TextEdit::load_state(&ctx, editor).unwrap();
        let range = state.cursor.char_range().unwrap();
        let mut undo = state.undoer();
        let (_, previous) = undo.undo(&(range, updated.into())).unwrap();
        assert_eq!(previous, initial);

        let edited = "SELECT name FROM class WHERE id = 42;";
        workspace.tab_mut().sql = edited.into();
        let latest = stage(&mut app, first, updated, Destination::LinkedQuery);
        assert_ne!(latest, first_query);
        assert_eq!(app.ws().tabs[1].sql, edited);
        assert_eq!(
            stage(
                &mut app,
                second,
                "SELECT name FROM student;",
                Destination::LinkedQuery
            ),
            second_query
        );
        assert_eq!(app.ws().tabs[4].id, latest);
        assert_eq!(app.ws().tabs[4].sql, updated);

        app.ws().tab_mut().path = Some(PathBuf::from("student.sql"));
        assert!(
            app.design_tabs
                .builder_mut(second)
                .unwrap()
                .linked_query(&app.workspaces[0])
                .is_none()
        );
    }

    #[test]
    fn dragging_columns_and_using_the_row_menu_updates_projection() {
        let ctx = egui::Context::default();
        crate::app::style::apply(&ctx);
        let workspace = Workspace::new("查询设计测试".into(), None);
        let object = table("inventory");
        let mut builder = Builder::new(object.clone());
        builder
            .metadata
            .columns
            .insert(object, vec!["stock".into(), "label".into()]);
        builder.query.add_field(
            Some(FieldInput::Column(Column {
                table: 1,
                name: "label".into(),
            })),
            Aggregate::None,
        );
        builder.query.fields[0].alias = "商品".into();
        builder.query.add_field(
            Some(FieldInput::Sql {
                text: "CHAR_LENGTH(label)".into(),
                aggregate: false,
            }),
            Aggregate::None,
        );
        builder.query.fields[1].alias = "名称长度".into();
        let sort_field = builder.query.fields[1].id;
        let base_column = Target::Column(Column {
            table: 1,
            name: "label".into(),
        });
        builder.query.sorts = [
            Target::Field(sort_field),
            Target::Field(builder.query.fields[0].id),
            base_column.clone(),
        ]
        .into_iter()
        .map(|target| Sort {
            target,
            descending: false,
        })
        .collect();
        builder.query.filters = vec![vec![Predicate::Compare {
            target: Some(base_column.clone()),
            operator: Comparison::Equal,
            value: "商品 A".into(),
            numeric: false,
        }]];

        let render = |builder: &mut Builder, events: Vec<egui::Event>| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200.0, 850.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        builder.design_view(ui, &workspace, true, false);
                    });
                },
            )
        };
        let text_at = |output: &egui::FullOutput, label: &str| {
            output
                .shapes
                .iter()
                .rev()
                .find_map(|item| {
                    let egui::Shape::Text(text) = &item.shape else {
                        return None;
                    };
                    let point = text.galley.rect.translate(text.pos.to_vec2()).center();
                    (text.galley.text() == label && item.clip_rect.contains(point)).then_some(point)
                })
                .unwrap_or_else(|| panic!("查询设计中未找到 {label}"))
        };
        let button_event = |pos, button, pressed| egui::Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: Modifiers::NONE,
        };
        let click = |builder: &mut Builder, point, button| {
            render(
                builder,
                vec![
                    egui::Event::PointerMoved(point),
                    button_event(point, button, true),
                ],
            );
            render(builder, vec![button_event(point, button, false)]);
            render(builder, Vec::new())
        };
        let drag_column = |builder: &mut Builder, from, to| {
            let primary = egui::PointerButton::Primary;
            render(
                builder,
                vec![
                    egui::Event::PointerMoved(from),
                    button_event(from, primary, true),
                ],
            );
            render(builder, vec![egui::Event::PointerMoved(to)]);
            render(builder, vec![button_event(to, primary, false)]);
            render(builder, Vec::new())
        };
        let row_menu = |builder: &mut Builder, field, action| {
            // 删除字段会改变自适应分区高度，须按新一帧布局定位下一次右键。
            let output = render(builder, Vec::new());
            let menu = click(
                builder,
                text_at(&output, field),
                egui::PointerButton::Secondary,
            );
            assert!(
                egui::Popup::is_any_open(&ctx),
                "字段 {field} 的右键菜单应打开"
            );
            click(
                builder,
                text_at(&menu, action),
                egui::PointerButton::Primary,
            )
        };

        let initial = render(&mut builder, Vec::new());
        drag_column(
            &mut builder,
            text_at(&initial, "stock"),
            text_at(&initial, "名称长度"),
        );
        assert_eq!(builder.query.fields.len(), 3);
        assert!(
            matches!(&builder.query.fields[1].input, Some(FieldInput::Column(column)) if column.name == "stock")
        );
        assert_eq!(builder.query.fields[2].id, sort_field);
        assert_eq!(builder.query.sorts[0].target, Target::Field(sort_field));

        let sql = builder.query.sql_with_quoting(false, false).unwrap();
        let inserted = row_menu(&mut builder, "名称长度", "在此字段前插入");
        assert_eq!(builder.query.fields.len(), 4);
        assert!(builder.query.fields[2].is_placeholder());
        assert!(!builder.query.fields[2].show);
        assert_eq!(builder.query.fields[3].id, sort_field);
        assert_eq!(builder.query.sql_with_quoting(false, false).unwrap(), sql);

        let columns = click(
            &mut builder,
            text_at(&inserted, "选择列或表达式"),
            egui::PointerButton::Primary,
        );
        click(
            &mut builder,
            text_at(&columns, "label"),
            egui::PointerButton::Primary,
        );
        assert!(builder.query.fields[2].show);

        row_menu(&mut builder, "名称长度", "删除字段");
        assert_eq!(builder.query.fields.len(), 3);
        assert_eq!(builder.query.sorts.len(), 2);
        let slots = builder
            .query
            .fields
            .iter()
            .map(|field| field.id)
            .collect::<Vec<_>>();
        let cleared = row_menu(&mut builder, "商品", "清除全部字段");
        assert_eq!(
            builder
                .query
                .fields
                .iter()
                .map(|field| field.id)
                .collect::<Vec<_>>(),
            slots
        );
        assert!(
            builder
                .query
                .fields
                .iter()
                .all(|field| field.is_placeholder() && !field.show)
        );
        assert_eq!(builder.query.sorts.len(), 1);
        assert_eq!(builder.query.sorts[0].target, base_column);
        let fallback = builder.query.sql_with_quoting(false, false).unwrap();
        assert!(fallback.starts_with("SELECT *\n"));
        assert!(fallback.contains("WHERE label = '商品 A'\nORDER BY label ASC"));

        drag_column(
            &mut builder,
            text_at(&cleared, "stock"),
            text_at(&cleared, "选择列或表达式") + egui::vec2(0.0, 40.0),
        );
        assert_eq!(builder.query.fields.len(), slots.len() + 1);
        assert!(
            matches!(&builder.query.fields[slots.len()].input, Some(FieldInput::Column(column)) if column.name == "stock")
        );
    }

    #[test]
    fn quotes_only_generated_identifiers_when_requested() {
        let object = |name: &str| DbObject {
            schema: "harbor_query_design".into(),
            name: name.into(),
            kind: ObjectKind::Table,
        };
        let column = |table, name: &str| Column {
            table,
            name: name.into(),
        };
        let mut builder = Builder::new(object("items"));
        builder.add_source(object("order"), None);
        let query = &mut builder.query;
        query.connect(column(1, "id"), column(2, "id"));
        query.add_field(Some(FieldInput::Column(column(1, "qty"))), Aggregate::None);
        query.fields[0].alias = "quantity".into();
        query.add_field(
            Some(FieldInput::Column(column(2, "group"))),
            Aggregate::None,
        );
        query.fields[1].alias = "order".into();
        query.add_field(
            Some(FieldInput::Sql {
                text: "UPPER(`t1`.`label`)".into(),
                aggregate: false,
            }),
            Aggregate::None,
        );
        query.fields[2].alias = "display_label".into();
        query.filters = vec![vec![
            Predicate::Compare {
                target: Some(Target::Column(column(1, "label"))),
                operator: Comparison::Equal,
                value: "O'Brien;".into(),
                numeric: false,
            },
            Predicate::Sql {
                text: "`t1`.`qty` >= 1".into(),
                phase: FilterPhase::Where,
            },
        ]];
        query.sorts = vec![Sort {
            target: Target::Column(column(1, "id")),
            descending: false,
        }];

        let optional = query.sql_with_quoting(false, false).unwrap();
        let quoted = query.sql_with_quoting(false, true).unwrap();
        assert_eq!(
            optional,
            "SELECT t1.qty AS quantity, t2.`group` AS `order`, (UPPER(`t1`.`label`)) AS display_label\n\
             FROM harbor_query_design.items AS t1\n\
             INNER JOIN harbor_query_design.`order` AS t2\n  ON t1.id = t2.id\n\
             WHERE t1.label = 'O''Brien;' AND (`t1`.`qty` >= 1)\n\
             ORDER BY t1.id ASC\nLIMIT 100;"
        );
        assert_eq!(
            quoted,
            "SELECT `t1`.`qty` AS `quantity`, `t2`.`group` AS `order`, (UPPER(`t1`.`label`)) AS `display_label`\n\
             FROM `harbor_query_design`.`items` AS `t1`\n\
             INNER JOIN `harbor_query_design`.`order` AS `t2`\n  ON `t1`.`id` = `t2`.`id`\n\
             WHERE `t1`.`label` = 'O''Brien;' AND (`t1`.`qty` >= 1)\n\
             ORDER BY `t1`.`id` ASC\nLIMIT 100;"
        );
        let view = crate::app::objects::create_view_draft(
            "harbor_query_design",
            "design_report",
            &optional,
        );
        println!("BEGIN optional\n{optional}\nEND optional");
        println!("BEGIN quoted\n{quoted}\nEND quoted");
        println!("BEGIN view\n{view}\nEND view");
    }

    #[test]
    fn builds_a_joined_aggregate_query_with_separate_filter_groups() {
        let mut builder = Builder::new(table("class"));
        builder.add_source(table("student"), None);
        let query = &mut builder.query;
        query.sources[0].alias = "group`Name".into();

        let class = |name: &str| Column {
            table: 1,
            name: name.into(),
        };
        let student = |name: &str| Column {
            table: 2,
            name: name.into(),
        };
        query.connect(class("id"), student("class_id"));
        query.joins[0].kind = JoinKind::Left;
        query.groups = vec![
            Target::Column(class("district")),
            Target::Column(class("name")),
        ];

        let fallback = query.sql(false).unwrap();
        assert!(fallback.contains("COUNT(*) AS `记录数`"));
        for name in ["district", "name"] {
            query.add_field(Some(FieldInput::Column(class(name))), Aggregate::None);
        }
        query.add_field(None, Aggregate::CountRows);
        query.fields[2].alias = "人数".into();
        let count = Target::Field(query.fields[2].id);
        query.add_field(Some(FieldInput::Column(student("score"))), Aggregate::Sum);
        query.fields[3].alias = "总分".into();
        let sum = Target::Field(query.fields[3].id);
        query.add_field(
            Some(FieldInput::Sql {
                text: "YEAR(t2.enrolled_on)".into(),
                aggregate: false,
            }),
            Aggregate::None,
        );
        query.fields[4].show = false;
        let year = Target::Field(query.fields[4].id);

        let compare = |target, operator, value: &str, numeric| Predicate::Compare {
            target: Some(target),
            operator,
            value: value.into(),
            numeric,
        };
        let surname = Target::Column(student("sur`name"));
        query.filters = vec![
            vec![
                compare(surname.clone(), Comparison::Equal, "O'Brien;", false),
                compare(year, Comparison::Equal, "2026", true),
                compare(count, Comparison::GreaterThan, "1", true),
            ],
            vec![
                compare(surname, Comparison::Equal, "\\'", false),
                compare(sum.clone(), Comparison::GreaterOrEqual, "100", true),
            ],
        ];
        query.sorts = vec![
            Sort {
                target: sum,
                descending: true,
            },
            Sort {
                target: Target::Column(class("name")),
                descending: false,
            },
        ];

        let sql = query.sql(true).unwrap();
        assert_eq!(split_script(&sql).len(), 1);
        assert_eq!(
            sql,
            "SELECT `group``Name`.`district`, `group``Name`.`name`, COUNT(*) AS `人数`, SUM(`t2`.`score`) AS `总分`\n\
             FROM `school.regional`.`class` AS `group``Name`\n\
             LEFT JOIN `school.regional`.`student` AS `t2`\n  ON `group``Name`.`id` = `t2`.`class_id`\n\
             WHERE (`t2`.`sur``name` = 'O''Brien;' AND (YEAR(t2.enrolled_on)) = 2026)\n   OR (`t2`.`sur``name` = CONVERT(X'5C27' USING utf8mb4))\n\
             GROUP BY `group``Name`.`district`, `group``Name`.`name`\n\
             HAVING (COUNT(*) > 1)\n   OR (SUM(`t2`.`score`) >= 100)\n\
             ORDER BY SUM(`t2`.`score`) DESC, `group``Name`.`name` ASC\nLIMIT 100;"
        );
    }
}
