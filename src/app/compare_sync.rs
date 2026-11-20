use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::ops::Range;

use crate::database::{
    DbObject, ObjectKind, SchemaChange, SchemaDefaults, SchemaForeignKey, SchemaSnapshot,
    SnapshotObject, quote_identifier,
};
use crate::sql_tools::split_script;

type Key = (ObjectKind, String);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Difference {
    Same,
    Changed,
    SourceOnly,
    TargetOnly,
    Unavailable,
}

impl Difference {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Same => "一致",
            Self::Changed => "有差异",
            Self::SourceOnly => "仅源端存在",
            Self::TargetOnly => "仅目标端存在",
            Self::Unavailable => "定义不可用",
        }
    }
}

pub(super) struct Item {
    pub kind: ObjectKind,
    pub name: String,
    pub source: Option<usize>,
    pub target: Option<usize>,
    pub difference: Difference,
    collision: bool,
}

impl Item {
    pub(super) fn selectable(&self) -> bool {
        !self.collision
            && self.difference != Difference::Unavailable
            && (self.difference != Difference::Same || self.kind == ObjectKind::Table)
    }

    pub(super) fn problem(&self) -> Option<&'static str> {
        self.collision
            .then_some("两端对象在目标端共享同一名称，需手动处理。")
    }
}

pub(super) struct Plan {
    pub sql: String,
    pub review: Vec<String>,
    pub statements: usize,
    pub changes: Vec<SchemaChange>,
    pub target: SchemaSnapshot,
}

#[derive(Default)]
pub(super) struct PlanOptions {
    pub ignore_definer: bool,
    pub sync_database_defaults: bool,
}

pub(in crate::app) struct LayoutTableMatch {
    pub name: String,
    pub state: LayoutTableState,
    pub problem: Option<&'static str>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum LayoutTableState {
    Same,
    Different,
    Unverified,
    Missing,
    Blocked,
}

fn qualified(schema: &str, object: &str) -> String {
    format!("{}.{}", quote_identifier(schema), quote_identifier(object))
}

fn sql_kind(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Table => "TABLE",
        ObjectKind::View => "VIEW",
        ObjectKind::Procedure => "PROCEDURE",
        ObjectKind::Function => "FUNCTION",
        ObjectKind::Trigger => "TRIGGER",
        ObjectKind::Event => "EVENT",
    }
}

fn key(snapshot: &SchemaSnapshot, kind: ObjectKind, name: &str) -> Key {
    let insensitive = matches!(
        kind,
        ObjectKind::Procedure | ObjectKind::Function | ObjectKind::Event
    ) || matches!(kind, ObjectKind::Table | ObjectKind::View)
        && snapshot
            .context
            .as_ref()
            .is_some_and(|context| context.lower_case_table_names != 0);
    (
        kind,
        if insensitive {
            name.to_lowercase()
        } else {
            name.into()
        },
    )
}

pub(super) fn differences(
    source: &SchemaSnapshot,
    target: &SchemaSnapshot,
    ignore_definer: bool,
) -> Vec<Item> {
    let mut pairs = BTreeMap::<_, (Vec<usize>, Vec<usize>)>::new();
    let mut tabular = BTreeMap::<String, BTreeSet<Key>>::new();
    let mut triggers = BTreeMap::<String, BTreeSet<Key>>::new();
    for (is_source, snapshot) in [(true, source), (false, target)] {
        for (index, item) in snapshot.objects.iter().enumerate() {
            let object_key = key(target, item.object.kind, &item.object.name);
            let collision_name = if target.context.is_none() {
                item.object.name.to_lowercase()
            } else {
                object_key.1.clone()
            };
            match item.object.kind {
                ObjectKind::Table | ObjectKind::View => {
                    tabular
                        .entry(collision_name)
                        .or_default()
                        .insert(object_key.clone());
                }
                ObjectKind::Trigger => {
                    triggers
                        .entry(item.object.name.to_lowercase())
                        .or_default()
                        .insert(object_key.clone());
                }
                _ => {}
            }
            let pair = pairs.entry(object_key).or_default();
            if is_source {
                pair.0.push(index);
            } else {
                pair.1.push(index);
            }
        }
    }

    let collisions = tabular
        .into_values()
        .chain(triggers.into_values())
        .filter(|keys| keys.len() > 1)
        .flatten()
        .collect::<BTreeSet<_>>();
    let mut items = Vec::with_capacity(pairs.len());
    for (key, (left, right)) in pairs {
        let left_item = left.first().map(|&index| &source.objects[index]);
        let right_item = right.first().map(|&index| &target.objects[index]);
        let name = left_item
            .or(right_item)
            .expect("listed object")
            .object
            .name
            .clone();
        let collision = collisions.contains(&key) || left.len() > 1 || right.len() > 1;
        let difference = match (left_item, right_item) {
            (Some(left), Some(right)) => match (&left.definition, &right.definition) {
                (Ok(_), Ok(_)) if equivalent(source, left, target, right, ignore_definer) => {
                    Difference::Same
                }
                (Ok(_), Ok(_)) => Difference::Changed,
                _ => Difference::Unavailable,
            },
            (Some(left), None) if target.omitted_objects == 0 && left.definition.is_ok() => {
                Difference::SourceOnly
            }
            (None, Some(right)) if source.omitted_objects == 0 && right.definition.is_ok() => {
                Difference::TargetOnly
            }
            _ => Difference::Unavailable,
        };
        items.push(Item {
            kind: key.0,
            name,
            source: left.first().copied(),
            target: right.first().copied(),
            difference,
            collision,
        });
    }
    items
}

pub(super) fn database_defaults_diff<'a>(
    source: &'a SchemaSnapshot,
    target: &'a SchemaSnapshot,
) -> Option<(&'a SchemaDefaults, &'a SchemaDefaults)> {
    source
        .defaults
        .as_ref()
        .zip(target.defaults.as_ref())
        .filter(|(source, target)| source != target)
}

pub(super) fn compare_layout_tables(
    source: &SchemaSnapshot,
    target: &SchemaSnapshot,
) -> Vec<LayoutTableMatch> {
    let items = differences(source, target, false);
    let paired = items
        .iter()
        .filter(|item| item.source.is_some())
        .map(|item| (key(target, item.kind, &item.name), item))
        .collect::<BTreeMap<_, _>>();
    source
        .objects
        .iter()
        .map(|object| {
            let name = &object.object.name;
            let (state, problem) = match paired.get(&key(target, ObjectKind::Table, name)) {
                Some(item) if item.collision => (
                    LayoutTableState::Blocked,
                    Some("布局或目标库存在同名对象，无法确定表身份。"),
                ),
                Some(item) if item.target.is_some() => (
                    match item.difference {
                        Difference::Same => LayoutTableState::Same,
                        Difference::Changed => LayoutTableState::Different,
                        _ => LayoutTableState::Unverified,
                    },
                    None,
                ),
                Some(item) if item.difference == Difference::SourceOnly => {
                    match Table::parse(source, object) {
                        Ok(_) => (LayoutTableState::Missing, None),
                        Err(error) => (LayoutTableState::Blocked, Some(error)),
                    }
                }
                _ => (
                    LayoutTableState::Blocked,
                    Some(if target.omitted_objects != 0 {
                        "目标库对象清单不完整，无法确认此表是否存在。"
                    } else {
                        "布局未提供可执行的建表定义。"
                    }),
                ),
            };
            LayoutTableMatch {
                name: name.clone(),
                state,
                problem,
            }
        })
        .collect()
}

pub(super) fn plan_layout_creations(
    source: &SchemaSnapshot,
    target: &SchemaSnapshot,
    selected: &BTreeSet<String>,
) -> Result<Plan, Vec<String>> {
    if target.origin.is_none() || target.context.is_none() {
        return Err(vec![
            "无法确认目标连接或 SQL 模式，请重新读取目标库。".into(),
        ]);
    }
    let items = differences(source, target, false);
    let mut pending = selected.clone();
    let selected = items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            item.kind == ObjectKind::Table
                && item.difference == Difference::SourceOnly
                && item.selectable()
                && pending.remove(&item.name)
        })
        .map(|(index, _)| index)
        .collect::<BTreeSet<_>>();
    if !pending.is_empty() {
        return Err(vec![format!(
            "无法确认这些表仅在布局中存在，请重新读取目标库：{}。",
            pending.into_iter().collect::<Vec<_>>().join("、")
        )]);
    }
    plan(source, target, &items, &selected, PlanOptions::default())
}

fn equivalent(
    left: &SchemaSnapshot,
    source: &SnapshotObject,
    right: &SchemaSnapshot,
    target: &SnapshotObject,
    ignore_definer: bool,
) -> bool {
    let (Ok(source_ddl), Ok(target_ddl)) = (&source.definition, &target.definition) else {
        return false;
    };
    if source.object.kind == ObjectKind::Table {
        let pair = Table::parse(left, source)
            .ok()
            .zip(Table::parse(right, target).ok());
        return pair.map_or_else(
            || source_ddl == target_ddl && quoting(left, source) == quoting(right, target),
            |(source, target)| source.equivalent(&target),
        );
    }
    let contexts = [source, target].map(|object| {
        object.context.as_ref().map(|context| {
            (
                context.sql_mode.as_deref().map(mode_signature),
                context.character_set_client.as_deref(),
                context.collation_connection.as_deref(),
                context.trigger_action_order,
                context.time_zone.as_deref(),
            )
        })
    });
    if contexts[0] != contexts[1] {
        return false;
    }
    let normalized = |snapshot: &SchemaSnapshot, object: &SnapshotObject| {
        Program::parse(
            snapshot,
            object,
            "__harbor_comparison_schema__",
            ignore_definer,
        )
        .map(|program| program.signature)
        .ok()
    };
    match (normalized(left, source), normalized(right, target)) {
        (Some(left), Some(right)) => left == right,
        _ => source_ddl == target_ddl && quoting(left, source) == quoting(right, target),
    }
}

fn mode_signature(mode: &str) -> BTreeSet<&str> {
    mode.split(',').filter(|flag| !flag.is_empty()).collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Quoting {
    ansi: bool,
    backslash: Option<bool>,
}

fn quoting(snapshot: &SchemaSnapshot, object: &SnapshotObject) -> Quoting {
    let mode = object
        .context
        .as_ref()
        .and_then(|context| context.sql_mode.as_deref())
        .or_else(|| {
            snapshot
                .context
                .as_ref()
                .map(|context| context.sql_mode.as_str())
        });
    let has =
        |flag| mode.is_some_and(|mode| mode.split(',').any(|item| item.eq_ignore_ascii_case(flag)));
    Quoting {
        ansi: has("ANSI_QUOTES"),
        backslash: mode.map(|_| !has("NO_BACKSLASH_ESCAPES")),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Word,
    Identifier,
    Literal,
    Number,
    Symbol(u8),
}

#[derive(Clone, Copy)]
struct Token<'a> {
    kind: Kind,
    start: usize,
    text: &'a str,
}

impl Token<'_> {
    fn keyword(&self, word: &str) -> bool {
        self.kind == Kind::Word && self.text.eq_ignore_ascii_case(word)
    }

    fn identifier(&self) -> Option<String> {
        match self.kind {
            Kind::Word => Some(self.text.into()),
            Kind::Identifier if self.text.starts_with('\x60') => {
                Some(self.text[1..self.text.len() - 1].replace("\x60\x60", "\x60"))
            }
            Kind::Identifier => Some(self.text[1..self.text.len() - 1].replace("\"\"", "\"")),
            _ => None,
        }
    }

    fn end(&self) -> usize {
        self.start + self.text.len()
    }

    fn symbol(&self, byte: u8) -> bool {
        self.kind == Kind::Symbol(byte)
    }
}

fn tokenize(sql: &str, quoting: Quoting, program: bool) -> Result<Vec<Token<'_>>, &'static str> {
    let mut tokens = Vec::new();
    let bytes = sql.as_bytes();
    let mut start = 0;
    while let Some(&byte) = bytes.get(start) {
        let tail = &bytes[start..];
        if byte.is_ascii_whitespace() {
            start += 1;
            continue;
        }
        if tail.starts_with(b"/*") {
            if !program || tail.starts_with(b"/*!") || tail.starts_with(b"/*M!") {
                return Err("定义含有需要按服务器版本解析的注释。");
            }
            // Optimizer hints affect execution and must participate in the comparison.
            let end = sql[start + 2..]
                .find("*/")
                .ok_or("定义含有未闭合的注释。")?
                + 4;
            if tail.starts_with(b"/*+") {
                tokens.push(Token {
                    kind: Kind::Literal,
                    start,
                    text: &sql[start..start + end],
                });
            }
            start += end;
            continue;
        }
        if byte == b'#'
            || (tail.starts_with(b"--") && tail.get(2).is_none_or(u8::is_ascii_whitespace))
        {
            if !program {
                return Err("表定义含有无法验证的注释。");
            }
            start += tail
                .iter()
                .position(|&byte| matches!(byte, b'\r' | b'\n'))
                .unwrap_or(tail.len());
            continue;
        }
        let (kind, width) = match byte {
            b'\x60' => (Kind::Identifier, quoted_width(tail, Some(false))?),
            b'"' if quoting.ansi => (Kind::Identifier, quoted_width(tail, Some(false))?),
            b'"' if quoting.backslash.is_none() => {
                return Err("双引号的含义取决于服务器 SQL 模式。");
            }
            b'"' | b'\'' => (Kind::Literal, quoted_width(tail, quoting.backslash)?),
            byte if byte.is_ascii_digit() => {
                let mut width = tail.iter().take_while(|&&byte| word_byte(byte)).count();
                if tail.get(width) == Some(&b'.')
                    && tail.get(width + 1).is_some_and(u8::is_ascii_digit)
                {
                    width += 1 + tail[width + 1..]
                        .iter()
                        .take_while(|byte| byte.is_ascii_digit())
                        .count();
                }
                (Kind::Number, width)
            }
            byte if word_byte(byte) => (
                Kind::Word,
                tail.iter().take_while(|&&byte| word_byte(byte)).count(),
            ),
            0 => return Err("定义中含有 NUL 字符。"),
            other => (Kind::Symbol(other), 1),
        };
        tokens.push(Token {
            kind,
            start,
            text: &sql[start..start + width],
        });
        start += width;
    }
    Ok(tokens)
}

fn word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$' | 0x80..=0xff)
}

fn quoted_width(bytes: &[u8], backslash: Option<bool>) -> Result<usize, &'static str> {
    let quote = bytes[0];
    let mut index = 1;
    while let Some(&byte) = bytes.get(index) {
        if byte == 0 {
            return Err("定义中含有 NUL 字符。");
        }
        index += 1;
        if byte == b'\\' {
            match backslash {
                Some(true) => index += 1,
                Some(false) => {}
                None => return Err("定义中的转义取决于服务器 SQL 模式。"),
            }
        } else if byte == quote {
            if bytes.get(index) != Some(&quote) {
                return Ok(index);
            }
            index += 1;
        }
    }
    Err("定义中的标识符或字面量未闭合。")
}

fn signature(tokens: &[Token<'_>]) -> Vec<(Kind, String)> {
    tokens
        .iter()
        .map(|token| {
            let text = match token.kind {
                Kind::Word => token.text.to_ascii_lowercase(),
                Kind::Identifier => token.identifier().expect("quoted identifier"),
                _ => token.text.into(),
            };
            (token.kind, text)
        })
        .collect()
}

fn tokens_sql<'a>(sql: &'a str, tokens: &[Token<'_>]) -> Option<&'a str> {
    Some(&sql[tokens.first()?.start..tokens.last()?.end()])
}

struct Name {
    schema: Option<String>,
    object: String,
    span: Range<usize>,
}

fn read_name(tokens: &[Token<'_>], start: usize) -> Option<(Name, usize)> {
    let first = tokens.get(start)?;
    let name = first.identifier()?;
    if tokens
        .get(start + 1)
        .is_some_and(|token| token.symbol(b'.'))
    {
        let second = tokens.get(start + 2)?;
        Some((
            Name {
                schema: Some(name),
                object: second.identifier()?,
                span: first.start..second.end(),
            },
            start + 3,
        ))
    } else {
        Some((
            Name {
                schema: None,
                object: name,
                span: first.start..first.end(),
            },
            start + 1,
        ))
    }
}

fn same_schema(snapshot: &SchemaSnapshot, name: &str) -> bool {
    name == snapshot.schema
        || snapshot
            .context
            .as_ref()
            .is_some_and(|context| context.lower_case_table_names != 0)
            && name.to_lowercase() == snapshot.schema.to_lowercase()
}

fn local_name(snapshot: &SchemaSnapshot, name: &Name) -> bool {
    name.schema
        .as_deref()
        .is_none_or(|schema| same_schema(snapshot, schema))
}

fn matches_object(snapshot: &SchemaSnapshot, object: &DbObject, name: &Name) -> bool {
    local_name(snapshot, name)
        && key(snapshot, object.kind, &object.name) == key(snapshot, object.kind, &name.object)
}

fn is_keyword(token: &Token<'_>, words: &[&str]) -> bool {
    words.iter().any(|word| token.keyword(word))
}

#[derive(Clone, PartialEq, Eq)]
struct IndexColumn {
    name: String,
    prefix: bool,
}

struct Column<'a> {
    name: String,
    sql: &'a str,
    signature: Vec<(Kind, String)>,
    storage_type: String,
    encoding: Option<(String, String)>,
    additive: bool,
}

struct Index<'a> {
    name: String,
    sql: &'a str,
    signature: Vec<(Kind, String)>,
    columns: Vec<IndexColumn>,
    unique: bool,
    ordinary: bool,
    primary: bool,
}

struct ForeignKey {
    name: String,
    referenced_schema: Option<String>,
    referenced_table: String,
    columns: Vec<String>,
    referenced_columns: Vec<String>,
    delete: &'static str,
    update: &'static str,
}

impl ForeignKey {
    fn signature(
        &self,
    ) -> (
        &str,
        &Option<String>,
        &str,
        &[String],
        &[String],
        &str,
        &str,
    ) {
        (
            &self.name,
            &self.referenced_schema,
            &self.referenced_table,
            &self.columns,
            &self.referenced_columns,
            self.delete,
            self.update,
        )
    }

    fn sql(&self, schema: &str, parent_name: &str) -> String {
        let names = |columns: &[String]| {
            columns
                .iter()
                .map(|name| quote_identifier(name))
                .collect::<Vec<_>>()
                .join(", ")
        };
        format!(
            "ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}) ON DELETE {} ON UPDATE {}",
            quote_identifier(&self.name),
            names(&self.columns),
            qualified(schema, parent_name),
            names(&self.referenced_columns),
            self.delete,
            self.update,
        )
    }
}

struct Table<'a> {
    columns: Vec<Column<'a>>,
    indexes: Vec<Index<'a>>,
    foreign_keys: Vec<ForeignKey>,
    options: BTreeMap<&'static str, &'a str>,
}

const TABLE_UNSUPPORTED: &str = "此表含有需手动处理的列、索引、约束或表选项。";

impl<'a> Table<'a> {
    fn parse(snapshot: &SchemaSnapshot, item: &'a SnapshotObject) -> Result<Self, &'static str> {
        if snapshot.context.as_ref().is_some_and(|context| {
            context.sql_mode.split(',').any(|flag| {
                ["NO_TABLE_OPTIONS", "NO_KEY_OPTIONS", "NO_FIELD_OPTIONS"].contains(&flag)
            })
        }) {
            return Err("服务器 SQL 模式省略了部分表定义，请以完整定义重新比较。");
        }
        let sql = item
            .definition
            .as_deref()
            .map_err(|_| "未读取到完整定义。")?
            .trim_ascii()
            .trim_end_matches(';')
            .trim_ascii_end();
        let tokens = tokenize(sql, quoting(snapshot, item), false)?;
        let [create, kind, ..] = tokens.as_slice() else {
            return Err(TABLE_UNSUPPORTED);
        };
        if !create.keyword("CREATE") || !kind.keyword("TABLE") {
            return Err(TABLE_UNSUPPORTED);
        }
        let (name, start) = read_name(&tokens, 2).ok_or(TABLE_UNSUPPORTED)?;
        if !matches_object(snapshot, &item.object, &name) {
            return Err("表定义与对象名称不一致。");
        }
        let end = matching_parenthesis(&tokens, start).ok_or(TABLE_UNSUPPORTED)?;
        let options = table_options(&tokens[end + 1..]).ok_or(TABLE_UNSUPPORTED)?;
        let mut table = Self {
            columns: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            options,
        };
        let encoding = (table.option("CHARSET"), table.option("COLLATE"));
        for part in comma_parts(&tokens[start + 1..end]).ok_or(TABLE_UNSUPPORTED)? {
            if part[0].keyword("CONSTRAINT") {
                table
                    .foreign_keys
                    .push(parse_foreign_key(snapshot, part).ok_or(TABLE_UNSUPPORTED)?);
            } else if is_keyword(
                &part[0],
                &["PRIMARY", "KEY", "INDEX", "UNIQUE", "FULLTEXT", "SPATIAL"],
            ) {
                table
                    .indexes
                    .push(parse_index(sql, part).ok_or(TABLE_UNSUPPORTED)?);
            } else {
                table
                    .columns
                    .push(parse_column(sql, part, encoding).ok_or(TABLE_UNSUPPORTED)?);
            }
        }
        if table.columns.is_empty() {
            return Err(TABLE_UNSUPPORTED);
        }
        Ok(table)
    }

    fn option(&self, name: &str) -> &'a str {
        self.options.get(name).copied().unwrap_or("")
    }

    fn column(&self, name: &str) -> Option<&Column<'a>> {
        self.columns
            .iter()
            .find(|column| column.name.to_lowercase() == name.to_lowercase())
    }

    fn index(&self, name: &str) -> Option<&Index<'a>> {
        self.indexes
            .iter()
            .find(|index| index.name.to_lowercase() == name.to_lowercase())
    }

    fn foreign_key(&self, name: &str) -> Option<&ForeignKey> {
        self.foreign_keys
            .iter()
            .find(|foreign_key| foreign_key.name.to_lowercase() == name.to_lowercase())
    }

    fn equivalent(&self, target: &Self) -> bool {
        OPTIONS
            .iter()
            .all(|&name| same_option(self.option(name), target.option(name)))
            && self
                .columns
                .iter()
                .map(|column| &column.signature)
                .eq(target.columns.iter().map(|column| &column.signature))
            && self.indexes.len() == target.indexes.len()
            && self.indexes.iter().all(|index| {
                target
                    .index(&index.name)
                    .is_some_and(|other| index.signature == other.signature)
            })
            && self.foreign_keys.len() == target.foreign_keys.len()
            && self.foreign_keys.iter().all(|foreign_key| {
                target
                    .foreign_key(&foreign_key.name)
                    .is_some_and(|other| foreign_key.signature() == other.signature())
            })
    }

    fn create(&self, schema: &str, name: &str) -> String {
        let parts = self
            .columns
            .iter()
            .map(|column| column.sql)
            .chain(self.indexes.iter().map(|index| index.sql))
            .collect::<Vec<_>>()
            .join(",\n  ");
        let options = self
            .options
            .iter()
            .map(|(&name, value)| format!("{}={value}", option_keyword(name)))
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "CREATE TABLE {} (\n  {parts}\n) {options}",
            qualified(schema, name)
        )
        .trim_end()
        .into()
    }
}

fn matching_parenthesis(tokens: &[Token<'_>], start: usize) -> Option<usize> {
    if !tokens.get(start)?.symbol(b'(') {
        return None;
    }
    let mut depth = 0;
    for (index, token) in tokens.iter().enumerate().skip(start) {
        if token.symbol(b'(') {
            depth += 1;
        } else if token.symbol(b')') {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

fn comma_parts<'a, 'b>(tokens: &'a [Token<'b>]) -> Option<Vec<&'a [Token<'b>]>> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while let Some(token) = tokens.get(index) {
        if token.symbol(b'(') {
            index = matching_parenthesis(tokens, index)?;
        } else if token.symbol(b',') {
            if start == index {
                return None;
            }
            parts.push(&tokens[start..index]);
            start = index + 1;
        }
        index += 1;
    }
    if start == tokens.len() {
        return None;
    }
    parts.push(&tokens[start..]);
    Some(parts)
}

const OPTIONS: &[&str] = &[
    "ENGINE",
    "CHARSET",
    "COLLATE",
    "AVG_ROW_LENGTH",
    "CHECKSUM",
    "COMMENT",
    "COMPRESSION",
    "DELAY_KEY_WRITE",
    "KEY_BLOCK_SIZE",
    "MAX_ROWS",
    "MIN_ROWS",
    "PACK_KEYS",
    "ROW_FORMAT",
    "STATS_AUTO_RECALC",
    "STATS_PERSISTENT",
    "STATS_SAMPLE_PAGES",
];

fn table_options<'a>(mut tokens: &[Token<'a>]) -> Option<BTreeMap<&'static str, &'a str>> {
    let mut options = BTreeMap::new();
    while let Some((mut name, rest)) = tokens.split_first() {
        tokens = rest;
        if name.keyword("DEFAULT") {
            (name, tokens) = tokens.split_first()?;
        }
        let name = if name.keyword("CHARACTER") {
            if !tokens.first()?.keyword("SET") {
                return None;
            }
            tokens = &tokens[1..];
            "CHARSET"
        } else if name.keyword("AUTO_INCREMENT") {
            "AUTO_INCREMENT"
        } else {
            OPTIONS.iter().copied().find(|word| name.keyword(word))?
        };
        if tokens.first()?.symbol(b'=') {
            tokens = &tokens[1..];
        }
        let (value, rest) = tokens.split_first()?;
        tokens = rest;
        match name {
            "AUTO_INCREMENT" if value.kind == Kind::Number => continue,
            "ENGINE" if is_keyword(value, &["INNODB", "MYISAM", "ARIA", "MEMORY"]) => {}
            "ENGINE" => return None,
            "COMMENT" if value.kind == Kind::Literal => {}
            "CHARSET" | "COLLATE" if value.kind == Kind::Word => {}
            "COMMENT" | "CHARSET" | "COLLATE" => return None,
            _ if matches!(value.kind, Kind::Word | Kind::Number | Kind::Literal) => {}
            _ => return None,
        }
        if options.insert(name, value.text).is_some() {
            return None;
        }
    }
    Some(options)
}

fn option_keyword(name: &str) -> &str {
    match name {
        "CHARSET" => "DEFAULT CHARACTER SET",
        "COLLATE" => "COLLATE",
        _ => name,
    }
}

fn same_option(left: &str, right: &str) -> bool {
    left == right || !left.starts_with(['\'', '"']) && left.eq_ignore_ascii_case(right)
}

fn parse_column<'a>(
    sql: &'a str,
    tokens: &[Token<'a>],
    table_encoding: (&str, &str),
) -> Option<Column<'a>> {
    let name = tokens.first()?.identifier()?;
    let data_type = tokens.get(1)?;
    const TEXT: &[&str] = &[
        "CHAR",
        "VARCHAR",
        "TINYTEXT",
        "TEXT",
        "MEDIUMTEXT",
        "LONGTEXT",
        "ENUM",
        "SET",
    ];
    const INTEGER: &[&str] = &[
        "TINYINT",
        "SMALLINT",
        "MEDIUMINT",
        "INT",
        "INTEGER",
        "BIGINT",
    ];
    const OTHER: &[&str] = &[
        "FLOAT",
        "DOUBLE",
        "REAL",
        "DECIMAL",
        "NUMERIC",
        "BIT",
        "DATE",
        "DATETIME",
        "TIMESTAMP",
        "TIME",
        "YEAR",
        "BINARY",
        "VARBINARY",
        "TINYBLOB",
        "BLOB",
        "MEDIUMBLOB",
        "LONGBLOB",
        "JSON",
        "GEOMETRY",
        "POINT",
        "LINESTRING",
        "POLYGON",
        "MULTIPOINT",
        "MULTILINESTRING",
        "MULTIPOLYGON",
        "GEOMETRYCOLLECTION",
    ];
    let textual = is_keyword(data_type, TEXT);
    let integer = is_keyword(data_type, INTEGER);
    if !textual && !integer && !is_keyword(data_type, OTHER) {
        return None;
    }
    let attrs_start = if tokens.get(2).is_some_and(|token| token.symbol(b'(')) {
        matching_parenthesis(tokens, 2)? + 1
    } else {
        2
    };
    let attrs = &tokens[attrs_start..];
    if attrs.iter().any(|token| {
        is_keyword(
            token,
            &[
                "AS",
                "CHECK",
                "CONSTRAINT",
                "GENERATED",
                "NEXT",
                "PERIOD",
                "REFERENCES",
                "SELECT",
                "SYSTEM",
                "VERSIONING",
            ],
        ) || token.symbol(b';')
    }) {
        return None;
    }
    let mut storage_type = data_type.text.to_ascii_lowercase();
    if storage_type == "integer" {
        storage_type = "int".into();
    }
    if !integer && !matches!(storage_type.as_str(), "varchar" | "char") {
        for token in &tokens[2..attrs_start] {
            storage_type.push_str(token.text);
        }
    }
    if attrs
        .iter()
        .any(|token| token.keyword("UNSIGNED") || token.keyword("ZEROFILL"))
    {
        storage_type.push_str(" unsigned");
    }
    let column_charset = attrs
        .windows(3)
        .find(|group| group[0].keyword("CHARACTER") && group[1].keyword("SET"))
        .map(|group| group[2].text);
    let column_collation = attrs
        .windows(2)
        .find(|group| group[0].keyword("COLLATE"))
        .map(|group| group[1].text);
    let charset = column_charset.unwrap_or(table_encoding.0);
    let collation = column_collation.unwrap_or(
        if column_charset.is_none_or(|value| value == table_encoding.0) {
            table_encoding.1
        } else {
            ""
        },
    );
    let encoding = textual.then(|| (charset.to_ascii_lowercase(), collation.to_ascii_lowercase()));
    let required = attrs
        .windows(2)
        .any(|group| group[0].keyword("NOT") && group[1].keyword("NULL"));
    let default = attrs
        .iter()
        .position(|token| token.keyword("DEFAULT"))
        .map(|index| &attrs[index + 1..]);
    if default.is_some_and(|tokens| !simple_default(tokens)) {
        return None;
    }
    let default_available = default.is_some_and(|tokens| !tokens[0].keyword("NULL"));
    let restricted = attrs
        .iter()
        .any(|token| is_keyword(token, &["AUTO_INCREMENT", "PRIMARY", "UNIQUE"]));
    Some(Column {
        name,
        sql: tokens_sql(sql, tokens)?,
        signature: signature(tokens),
        storage_type,
        encoding,
        additive: !restricted && (!required || default_available),
    })
}

fn simple_default(tokens: &[Token<'_>]) -> bool {
    let Some(value) = tokens.first() else {
        return false;
    };
    let next = tokens.get(1);
    let width =
        if value.keyword("CURRENT_TIMESTAMP") && next.is_some_and(|token| token.symbol(b'(')) {
            match matching_parenthesis(tokens, 1) {
                Some(2) => 3,
                Some(3) if tokens[2].kind == Kind::Number => 4,
                _ => return false,
            }
        } else if matches!(value.kind, Kind::Literal | Kind::Number)
            || is_keyword(value, &["NULL", "TRUE", "FALSE", "CURRENT_TIMESTAMP"])
        {
            1
        } else if ((value.symbol(b'+') || value.symbol(b'-'))
            && next.is_some_and(|token| token.kind == Kind::Number))
            || ((is_keyword(value, &["B", "X"]) || value.text.starts_with('_'))
                && next.is_some_and(|token| token.kind == Kind::Literal))
        {
            2
        } else {
            return false;
        };
    tokens.get(width).is_none_or(|token| {
        is_keyword(
            token,
            &[
                "ON",
                "COMMENT",
                "INVISIBLE",
                "VISIBLE",
                "COLLATE",
                "COLUMN_FORMAT",
                "STORAGE",
            ],
        )
    })
}

fn parse_index<'a>(sql: &'a str, tokens: &[Token<'a>]) -> Option<Index<'a>> {
    let primary = tokens[0].keyword("PRIMARY");
    let unique = primary || tokens[0].keyword("UNIQUE");
    let ordinary = !is_keyword(&tokens[0], &["FULLTEXT", "SPATIAL"]);
    let name_index = if primary || !ordinary || unique { 2 } else { 1 };
    if !is_keyword(tokens.get(name_index - 1)?, &["KEY", "INDEX"]) {
        return None;
    }
    let name = if primary {
        "PRIMARY".into()
    } else {
        tokens.get(name_index)?.identifier()?
    };
    let mut start = name_index + usize::from(!primary);
    if tokens
        .get(start)
        .is_some_and(|token| token.keyword("USING"))
    {
        if !is_keyword(tokens.get(start + 1)?, &["BTREE", "HASH"]) {
            return None;
        }
        start += 2;
    }
    let end = matching_parenthesis(tokens, start)?;
    let columns = comma_parts(&tokens[start + 1..end])?
        .into_iter()
        .map(|part| {
            let name = part[0].identifier()?;
            let mut remaining = &part[1..];
            let prefix = remaining.first().is_some_and(|token| token.symbol(b'('));
            if prefix {
                if remaining.get(1)?.kind != Kind::Number || !remaining.get(2)?.symbol(b')') {
                    return None;
                }
                remaining = &remaining[3..];
            }
            if remaining
                .first()
                .is_some_and(|token| is_keyword(token, &["ASC", "DESC"]))
            {
                remaining = &remaining[1..];
            }
            remaining.is_empty().then_some(IndexColumn { name, prefix })
        })
        .collect::<Option<Vec<_>>>()?;
    let mut attrs = &tokens[end + 1..];
    while let Some((name, remaining)) = attrs.split_first() {
        attrs = remaining;
        if name.keyword("USING") && is_keyword(attrs.first()?, &["BTREE", "HASH"])
            || name.keyword("COMMENT") && attrs.first()?.kind == Kind::Literal
        {
            attrs = &attrs[1..];
        } else if !is_keyword(name, &["VISIBLE", "INVISIBLE", "IGNORED"]) {
            return None;
        }
    }
    Some(Index {
        name,
        sql: tokens_sql(sql, tokens)?,
        signature: signature(tokens),
        columns,
        unique,
        ordinary,
        primary,
    })
}

fn column_list(tokens: &[Token<'_>], start: usize) -> Option<(Vec<String>, usize)> {
    let end = matching_parenthesis(tokens, start)?;
    let names = comma_parts(&tokens[start + 1..end])?
        .into_iter()
        .map(|tokens| match tokens {
            [name] => name.identifier(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    Some((names, end + 1))
}

fn parse_foreign_key(snapshot: &SchemaSnapshot, tokens: &[Token<'_>]) -> Option<ForeignKey> {
    let name = tokens.get(1)?.identifier()?;
    if !tokens.get(2)?.keyword("FOREIGN") || !tokens.get(3)?.keyword("KEY") {
        return None;
    }
    let (columns, end) = column_list(tokens, 4)?;
    if !tokens.get(end)?.keyword("REFERENCES") {
        return None;
    }
    let (reference, end) = read_name(tokens, end + 1)?;
    let (referenced_columns, end) = column_list(tokens, end)?;
    if referenced_columns.len() != columns.len() {
        return None;
    }
    let mut options = &tokens[end..];
    let mut delete = None;
    let mut update = None;
    while let [on, event, action @ ..] = options {
        if !on.keyword("ON") {
            return None;
        }
        let slot = if event.keyword("DELETE") {
            &mut delete
        } else if event.keyword("UPDATE") {
            &mut update
        } else {
            return None;
        };
        let rest;
        let value;
        (value, rest) = match action {
            [word, rest @ ..] if word.keyword("CASCADE") => ("CASCADE", rest),
            [word, rest @ ..] if word.keyword("RESTRICT") => ("RESTRICT", rest),
            [set, null, rest @ ..] if set.keyword("SET") && null.keyword("NULL") => {
                ("SET NULL", rest)
            }
            [no, action, rest @ ..] if no.keyword("NO") && action.keyword("ACTION") => {
                ("RESTRICT", rest)
            }
            _ => return None,
        };
        if slot.replace(value).is_some() {
            return None;
        }
        options = rest;
    }
    if !options.is_empty() {
        return None;
    }
    let local = local_name(snapshot, &reference);
    Some(ForeignKey {
        name,
        columns,
        referenced_columns,
        referenced_schema: if local { None } else { reference.schema },
        referenced_table: reference.object,
        delete: delete.unwrap_or("RESTRICT"),
        update: update.unwrap_or("RESTRICT"),
    })
}

struct Program {
    sql: String,
    signature: Vec<(Kind, String)>,
    dependencies: BTreeSet<Key>,
    external: BTreeSet<String>,
    trigger: Option<TriggerGroup>,
}

struct TriggerGroup {
    table: String,
    timing: String,
    event: String,
    order: Option<u64>,
    insert_after: usize,
}

type Replacement = (Range<usize>, String);

const PROGRAM_UNSUPPORTED: &str = "无法识别此对象定义中的 SQL 结构。";

impl Program {
    fn parse(
        snapshot: &SchemaSnapshot,
        object: &SnapshotObject,
        destination: &str,
        ignore_definer: bool,
    ) -> Result<Self, &'static str> {
        let ddl = object
            .definition
            .as_deref()
            .map_err(|_| "未读取到完整定义。")?
            .trim_ascii()
            .trim_end_matches(';')
            .trim_ascii_end();
        let quoting = quoting(snapshot, object);
        let tokens = tokenize(ddl, quoting, true)?;
        let kind = object.object.kind;
        let (start, definer) = program_header(&tokens, kind).ok_or(PROGRAM_UNSUPPORTED)?;
        let (name, body) = read_name(&tokens, start).ok_or(PROGRAM_UNSUPPORTED)?;
        if !matches_object(snapshot, &object.object, &name) {
            return Err("对象定义与清单中的名称不一致。");
        }
        let mut reader = ProgramReader {
            snapshot,
            destination,
            dependencies: BTreeSet::new(),
            external: BTreeSet::new(),
            replacements: vec![(name.span, qualified(destination, &object.object.name))],
        };
        if let Some(definer) = definer.filter(|_| ignore_definer) {
            reader.replacements.push((definer, String::new()));
        }
        let mut trigger = None;
        let body = if kind == ObjectKind::Trigger {
            let timing = tokens
                .get(body)
                .filter(|token| is_keyword(token, &["BEFORE", "AFTER"]))
                .ok_or(PROGRAM_UNSUPPORTED)?;
            let event = tokens
                .get(body + 1)
                .filter(|token| is_keyword(token, &["INSERT", "UPDATE", "DELETE"]))
                .ok_or(PROGRAM_UNSUPPORTED)?;
            if !tokens
                .get(body + 2)
                .is_some_and(|token| token.keyword("ON"))
            {
                return Err(PROGRAM_UNSUPPORTED);
            }
            let (table, end) = read_name(&tokens, body + 3).ok_or(PROGRAM_UNSUPPORTED)?;
            let owner = reader.record(table, &[ObjectKind::Table]);
            let [for_, each, row, ..] = &tokens[end..] else {
                return Err(PROGRAM_UNSUPPORTED);
            };
            if !for_.keyword("FOR") || !each.keyword("EACH") || !row.keyword("ROW") {
                return Err(PROGRAM_UNSUPPORTED);
            }
            let order = object
                .context
                .as_ref()
                .and_then(|context| context.trigger_action_order);
            let mut next = end + 3;
            if tokens
                .get(next)
                .is_some_and(|token| is_keyword(token, &["FOLLOWS", "PRECEDES"]))
            {
                let anchor = tokens
                    .get(next + 1)
                    .filter(|token| token.identifier().is_some())
                    .ok_or(PROGRAM_UNSUPPORTED)?;
                if order.is_none() {
                    return Err("缺少触发器的执行顺序。");
                }
                reader
                    .replacements
                    .push((tokens[next].start..anchor.end(), String::new()));
                next += 2;
            }
            trigger = Some(TriggerGroup {
                table: owner,
                timing: timing.text.to_ascii_uppercase(),
                event: event.text.to_ascii_uppercase(),
                order,
                insert_after: row.end(),
            });
            next
        } else if kind == ObjectKind::View {
            tokens[body..]
                .iter()
                .position(|token| token.keyword("AS"))
                .map(|start| body + start + 1)
                .ok_or(PROGRAM_UNSUPPORTED)?
        } else {
            body
        };
        reader.read_relations(&tokens, body, kind == ObjectKind::View)?;
        reader.read_qualified_references(&tokens, body)?;
        let (sql, marker) = replace_spans(
            ddl,
            &mut reader.replacements,
            trigger.as_ref().map(|group| group.insert_after),
        )?;
        if let Some(trigger) = &mut trigger {
            trigger.insert_after = marker;
        }
        let tokens = tokenize(&sql, quoting, true)?;
        let signature = program_signature(&tokens, destination);
        Ok(Self {
            sql,
            signature,
            dependencies: reader.dependencies,
            external: reader.external,
            trigger,
        })
    }

    fn ordered_after(&mut self, earlier: bool, anchor: &str) {
        if let Some(group) = &self.trigger {
            let clause = if earlier { "FOLLOWS" } else { "PRECEDES" };
            self.sql.insert_str(
                group.insert_after,
                &format!(" {clause} {}", quote_identifier(anchor)),
            );
        }
    }
}

fn program_signature(tokens: &[Token<'_>], destination: &str) -> Vec<(Kind, String)> {
    let mut normalized = Vec::with_capacity(tokens.len());
    let mut index = 0;
    while let Some(token) = tokens.get(index) {
        // SHOW CREATE VIEW can print table.column or database.table.column depending
        // on the default database of the session reading the definition.
        if token.kind == Kind::Identifier
            && token.identifier().as_deref() == Some(destination)
            && tokens
                .get(index + 1)
                .is_some_and(|token| token.symbol(b'.'))
            && tokens
                .get(index + 2)
                .is_some_and(|token| token.identifier().is_some())
            && tokens
                .get(index + 3)
                .is_some_and(|token| token.symbol(b'.'))
        {
            index += 2;
        } else {
            normalized.push(*token);
            index += 1;
        }
    }
    signature(&normalized)
}

fn program_header(tokens: &[Token<'_>], kind: ObjectKind) -> Option<(usize, Option<Range<usize>>)> {
    if !tokens.first()?.keyword("CREATE") {
        return None;
    }
    let mut index = 1;
    let mut definer = None;
    while let Some(token) = tokens.get(index) {
        if token.keyword(sql_kind(kind)) {
            return Some((index + 1, definer));
        }
        if token.keyword("ALGORITHM") && kind == ObjectKind::View {
            if !tokens.get(index + 1)?.symbol(b'=')
                || !is_keyword(tokens.get(index + 2)?, &["UNDEFINED", "MERGE", "TEMPTABLE"])
            {
                return None;
            }
            index += 3;
        } else if token.keyword("DEFINER") {
            if definer.is_some() || !tokens.get(index + 1)?.symbol(b'=') {
                return None;
            }
            let account = tokens.get(index + 2)?;
            if !matches!(account.kind, Kind::Word | Kind::Identifier | Kind::Literal) {
                return None;
            }
            let start = index;
            index += 3;
            if tokens.get(index).is_some_and(|token| token.symbol(b'@')) {
                if !matches!(
                    tokens.get(index + 1)?.kind,
                    Kind::Word | Kind::Identifier | Kind::Literal
                ) {
                    return None;
                }
                index += 2;
            } else if account.keyword("CURRENT_USER")
                && tokens.get(index).is_some_and(|token| token.symbol(b'('))
            {
                if !tokens.get(index + 1)?.symbol(b')') {
                    return None;
                }
                index += 2;
            }
            definer = Some(tokens[start].start..tokens[index - 1].end());
        } else if token.keyword("SQL") && kind == ObjectKind::View {
            if !tokens.get(index + 1)?.keyword("SECURITY")
                || !is_keyword(tokens.get(index + 2)?, &["DEFINER", "INVOKER"])
            {
                return None;
            }
            index += 3;
        } else {
            return None;
        }
    }
    None
}

#[derive(Default)]
struct Scope {
    select: bool,
    from: bool,
    expect_relation: bool,
}

struct ProgramReader<'a> {
    snapshot: &'a SchemaSnapshot,
    destination: &'a str,
    dependencies: BTreeSet<Key>,
    external: BTreeSet<String>,
    replacements: Vec<Replacement>,
}

impl ProgramReader<'_> {
    fn record(&mut self, name: Name, kinds: &[ObjectKind]) -> String {
        if let Some(schema) = name
            .schema
            .as_deref()
            .filter(|schema| !same_schema(self.snapshot, schema))
        {
            self.external.insert(qualified(schema, &name.object));
            return name.object;
        }
        let candidate = self.snapshot.objects.iter().find(|item| {
            kinds.contains(&item.object.kind)
                && key(self.snapshot, item.object.kind, &item.object.name)
                    == key(self.snapshot, item.object.kind, &name.object)
        });
        let (kind, name_text) = candidate
            .map(|item| (item.object.kind, item.object.name.as_str()))
            .unwrap_or((kinds[0], &name.object));
        self.dependencies.insert((kind, name_text.into()));
        self.replacements
            .push((name.span, qualified(self.destination, name_text)));
        name_text.into()
    }

    fn read_relations(
        &mut self,
        tokens: &[Token<'_>],
        start: usize,
        view: bool,
    ) -> Result<(), &'static str> {
        let mut scopes = vec![Scope::default()];
        let mut position = start;
        while let Some(token) = tokens.get(position) {
            let index = position;
            position += 1;
            let scope = scopes.last_mut().expect("SQL scope");
            if token.symbol(b'(') {
                let expect_relation = std::mem::take(&mut scope.expect_relation);
                scopes.push(Scope {
                    from: expect_relation,
                    expect_relation,
                    ..Scope::default()
                });
                continue;
            }
            if token.symbol(b')') {
                if scopes.len() > 1 {
                    scopes.pop();
                }
                continue;
            }
            if token.keyword("SELECT") {
                *scope = Scope {
                    select: true,
                    ..Scope::default()
                };
                continue;
            }
            if token.keyword("WITH")
                && !tokens.get(position).is_some_and(|token| {
                    is_keyword(token, &["CHECK", "LOCAL", "CASCADED", "ROLLUP", "CUBE"])
                })
            {
                return Err("此对象含有公用表表达式，需审阅其依赖后手动同步。");
            }
            if !view
                && is_keyword(
                    token,
                    &[
                        "PREPARE", "EXECUTE", "ALTER", "CREATE", "DROP", "TRUNCATE", "RENAME",
                    ],
                )
            {
                return Err("此对象含有动态 SQL 或结构变更语句，需手动同步。");
            }
            if scope.expect_relation {
                if token.keyword("LATERAL") {
                    continue;
                }
                let (name, next) = read_name(tokens, index).ok_or(PROGRAM_UNSUPPORTED)?;
                if name.schema.is_some() || !name.object.eq_ignore_ascii_case("DUAL") {
                    if tokens.get(next).is_some_and(|token| token.symbol(b'(')) {
                        return Err("定义中使用了未知的表函数。");
                    }
                    self.record(name, &[ObjectKind::Table, ObjectKind::View]);
                }
                scope.expect_relation = false;
                position = next;
            } else if token.keyword("FROM") && scope.select
                || scope.from
                    && (is_keyword(token, &["JOIN", "STRAIGHT_JOIN"]) || token.symbol(b','))
            {
                scope.from = true;
                scope.expect_relation = true;
            } else if is_keyword(
                token,
                &[
                    "WHERE",
                    "GROUP",
                    "ORDER",
                    "HAVING",
                    "UNION",
                    "INTERSECT",
                    "EXCEPT",
                    "LIMIT",
                    "WINDOW",
                    "SET",
                ],
            ) || token.symbol(b';')
            {
                *scope = Scope::default();
            } else if !view {
                let previous = index.checked_sub(1).and_then(|i| tokens.get(i));
                if token.keyword("DELETE") {
                    *scope = Scope {
                        select: true,
                        ..Scope::default()
                    };
                } else if token.keyword("UPDATE")
                    && !previous.is_some_and(|token| is_keyword(token, &["FOR", "KEY", "ON"]))
                {
                    position = skip_keywords(tokens, position, &["LOW_PRIORITY", "IGNORE"]);
                    scope.from = true;
                    scope.expect_relation = true;
                } else if is_keyword(token, &["INSERT", "REPLACE"])
                    && !tokens.get(position).is_some_and(|token| token.symbol(b'('))
                {
                    let name = skip_keywords(
                        tokens,
                        position,
                        &["INTO", "LOW_PRIORITY", "DELAYED", "HIGH_PRIORITY", "IGNORE"],
                    );
                    let (name, next) = read_name(tokens, name).ok_or(PROGRAM_UNSUPPORTED)?;
                    self.record(name, &[ObjectKind::Table, ObjectKind::View]);
                    position = next;
                } else if token.keyword("CALL") {
                    let (name, next) = read_name(tokens, position).ok_or(PROGRAM_UNSUPPORTED)?;
                    self.record(name, &[ObjectKind::Procedure]);
                    position = next;
                }
            }
        }
        if scopes.iter().any(|scope| scope.expect_relation) {
            return Err(PROGRAM_UNSUPPORTED);
        }
        Ok(())
    }

    fn read_qualified_references(
        &mut self,
        tokens: &[Token<'_>],
        start: usize,
    ) -> Result<(), &'static str> {
        let mut position = start;
        while let Some(token) = tokens.get(position) {
            let index = position;
            position += 1;
            let Some((name, next)) = read_name(tokens, index) else {
                continue;
            };
            if self
                .replacements
                .iter()
                .any(|(span, _)| span.contains(&token.start))
            {
                continue;
            }
            if tokens.get(next).is_some_and(|token| token.symbol(b'.')) && name.schema.is_some() {
                self.record(name, &[ObjectKind::Table, ObjectKind::View]);
                position = next + 2;
            } else if tokens.get(next).is_some_and(|token| token.symbol(b'('))
                && (name.schema.is_some()
                    || self.snapshot.objects.iter().any(|object| {
                        object.object.kind == ObjectKind::Function
                            && object.object.name.eq_ignore_ascii_case(&name.object)
                    }))
            {
                // Only qualify explicit schema references. Unqualified function names may
                // resolve to a built-in even if a stored function has the same name.
                if name.schema.is_some() {
                    self.record(name, &[ObjectKind::Function]);
                } else {
                    self.dependencies
                        .insert((ObjectKind::Function, name.object));
                }
                position = next;
            } else if name
                .schema
                .as_deref()
                .is_some_and(|schema| same_schema(self.snapshot, schema))
            {
                return Err("定义中有无法区分数据库名称和别名的引用。");
            } else if name.schema.is_some() {
                // Skip the second part of an alias.column reference.
                position = next;
            }
        }
        Ok(())
    }
}

fn skip_keywords(tokens: &[Token<'_>], start: usize, choices: &[&str]) -> usize {
    start
        + tokens[start..]
            .iter()
            .take_while(|token| is_keyword(token, choices))
            .count()
}

fn replace_spans(
    sql: &str,
    replacements: &mut [Replacement],
    marker: Option<usize>,
) -> Result<(String, usize), &'static str> {
    replacements.sort_by_key(|(span, _)| span.start);
    let mut output = String::new();
    let mut offset = 0;
    let mut movement = 0isize;
    for (span, text) in replacements {
        if span.start < offset {
            return Err(PROGRAM_UNSUPPORTED);
        }
        output.push_str(&sql[offset..span.start]);
        output.push_str(text);
        offset = span.end;
        if marker.is_some_and(|marker| span.end <= marker) {
            movement += text.len() as isize - span.len() as isize;
        }
    }
    output.push_str(&sql[offset..]);
    Ok((
        output,
        marker.map_or(0, |position| position.saturating_add_signed(movement)),
    ))
}

#[derive(Default)]
struct TableEdit {
    statement: Option<Step>,
    drop_keys: BTreeSet<String>,
    add_keys: BTreeSet<String>,
    changed_columns: BTreeSet<String>,
    removed_indexes: Vec<Vec<IndexColumn>>,
    engine_changed: bool,
    review: bool,
}

impl TableEdit {
    fn affects(&self, columns: &[String]) -> bool {
        self.engine_changed
            || columns
                .iter()
                .any(|name| self.changed_columns.contains(&name.to_lowercase()))
            || self
                .removed_indexes
                .iter()
                .any(|index| index_prefix(index, columns))
    }
}

fn index_prefix(index: &[IndexColumn], columns: &[String]) -> bool {
    index.len() >= columns.len()
        && index
            .iter()
            .zip(columns)
            .all(|(index, name)| !index.prefix && index.name.to_lowercase() == name.to_lowercase())
}

fn alter_table(
    source: &Table<'_>,
    target: &Table<'_>,
    name: &str,
    settings: Settings,
) -> Result<TableEdit, &'static str> {
    let mut edit = TableEdit::default();
    let mut clauses = Vec::new();
    let mut column_order = Vec::new();
    for column in &target.columns {
        if source.column(&column.name).is_none() {
            clauses.push(format!("DROP COLUMN {}", quote_identifier(&column.name)));
            edit.changed_columns.insert(column.name.to_lowercase());
        } else {
            column_order.push(column.name.to_lowercase());
        }
    }
    edit.review = !clauses.is_empty();
    for index in &target.indexes {
        if source
            .index(&index.name)
            .is_none_or(|new| new.signature != index.signature)
        {
            clauses.push(if index.primary {
                "DROP PRIMARY KEY".into()
            } else {
                format!("DROP INDEX {}", quote_identifier(&index.name))
            });
            if index.ordinary {
                edit.removed_indexes.push(index.columns.clone());
            }
            edit.review = true;
        }
    }
    for (index, column) in source.columns.iter().enumerate() {
        let current = column.name.to_lowercase();
        let old_index = column_order.iter().position(|name| name == &current);
        let needs_position = old_index.unwrap_or(column_order.len()) != index;
        let order = if !needs_position {
            String::new()
        } else if index == 0 {
            " FIRST".into()
        } else {
            format!(
                " AFTER {}",
                quote_identifier(&source.columns[index - 1].name)
            )
        };
        if let Some(old_index) = old_index {
            let old = target.column(&column.name).expect("listed column");
            if old.signature != column.signature
                || old.encoding != column.encoding
                || needs_position
            {
                let operation = if old.name == column.name {
                    "MODIFY COLUMN".into()
                } else {
                    format!("CHANGE COLUMN {}", quote_identifier(&old.name))
                };
                clauses.push(format!("{operation} {}{order}", column.sql));
                if old.signature != column.signature || old.encoding != column.encoding {
                    edit.changed_columns.insert(current.clone());
                }
                column_order.remove(old_index);
                column_order.insert(index, current);
                edit.review = true;
            }
        } else {
            clauses.push(format!("ADD COLUMN {}{order}", column.sql));
            column_order.insert(index, current);
            edit.review |= !column.additive;
        }
    }
    for index in &source.indexes {
        if target
            .index(&index.name)
            .is_none_or(|old| old.signature != index.signature)
        {
            clauses.push(format!("ADD {}", index.sql));
            edit.review |= index.unique || !index.ordinary;
        }
    }
    for &name in OPTIONS {
        let original = target.option(name);
        let desired = source.option(name);
        if same_option(original, desired) {
            continue;
        }
        let desired = if desired.is_empty() {
            match name {
                "COMMENT" => "''",
                "ROW_FORMAT" | "PACK_KEYS" | "STATS_AUTO_RECALC" | "STATS_PERSISTENT"
                | "STATS_SAMPLE_PAGES" => "DEFAULT",
                "AVG_ROW_LENGTH" | "CHECKSUM" | "DELAY_KEY_WRITE" | "KEY_BLOCK_SIZE"
                | "MAX_ROWS" | "MIN_ROWS" => "0",
                _ => return Err("源端缺少对应的表选项，无法确定其目标默认值。"),
            }
        } else {
            desired
        };
        clauses.push(format!("{}={desired}", option_keyword(name)));
        edit.review |= name != "COMMENT";
        edit.engine_changed |= name == "ENGINE";
    }
    for foreign_key in &target.foreign_keys {
        if source
            .foreign_key(&foreign_key.name)
            .is_none_or(|new| new.signature() != foreign_key.signature())
        {
            edit.drop_keys.insert(foreign_key.name.clone());
            edit.review = true;
        }
    }
    for foreign_key in &source.foreign_keys {
        if target
            .foreign_key(&foreign_key.name)
            .is_none_or(|old| old.signature() != foreign_key.signature())
        {
            edit.add_keys.insert(foreign_key.name.clone());
        }
    }
    if !clauses.is_empty() {
        edit.statement = Some(Step {
            sql: alter(name, &clauses),
            settings,
        });
    }
    Ok(edit)
}

fn alter(name: &str, clauses: &[String]) -> String {
    format!("ALTER TABLE {name}\n  {}", clauses.join(",\n  "))
}

#[derive(Default)]
struct Settings {
    mode: Option<String>,
    charset: Option<String>,
    collation: Option<String>,
    time_zone: Option<String>,
}

impl Settings {
    fn from_definition(
        source: &SchemaSnapshot,
        object: &SnapshotObject,
        target: &SchemaSnapshot,
    ) -> Result<Self, &'static str> {
        let default_mode = source
            .context
            .as_ref()
            .map(|context| context.sql_mode.as_str());
        let definition_mode = object
            .context
            .as_ref()
            .and_then(|context| context.sql_mode.as_deref());
        let target_mode = target
            .context
            .as_ref()
            .map(|context| context.sql_mode.as_str());
        let mode = definition_mode.or(default_mode).filter(|mode| {
            target_mode.is_none_or(|target| mode_signature(target) != mode_signature(mode))
        });
        let collation = object
            .context
            .as_ref()
            .and_then(|context| context.collation_connection.as_deref())
            .filter(|collation| {
                target
                    .context
                    .as_ref()
                    .is_none_or(|context| &context.collation_connection != collation)
            });
        let valid = |text: &str| {
            text.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b','))
        };
        if !mode.into_iter().chain(collation).all(valid) {
            return Err("定义的 SQL 模式或排序规则无法写入同步脚本。");
        }
        let charset = object
            .context
            .as_ref()
            .and_then(|context| context.character_set_client.as_deref());
        if charset.is_some_and(|charset| !matches!(charset, "utf8" | "utf8mb3" | "utf8mb4")) {
            return Err("定义使用非 UTF-8 客户端字符集，需确认编码后手动同步。");
        }
        let charset = charset.filter(|charset| {
            target
                .context
                .as_ref()
                .is_none_or(|context| &context.character_set_client != charset)
        });
        let time_zone = object
            .context
            .as_ref()
            .and_then(|context| context.time_zone.clone());
        if time_zone.as_ref().is_some_and(|zone| {
            !zone.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'/' | b'+' | b'-' | b':')
            })
        }) {
            return Err("事件的时区无法写入同步脚本。");
        }
        Ok(Self {
            mode: mode.map(str::to_owned),
            charset: charset.map(str::to_owned),
            collation: collation.map(str::to_owned),
            time_zone,
        })
    }
}

struct Step {
    sql: String,
    settings: Settings,
}

impl Step {
    fn simple(sql: String) -> Self {
        Self {
            sql,
            settings: Settings::default(),
        }
    }
}

#[derive(Clone, Copy)]
struct Choice<'a> {
    item: &'a Item,
    source: Option<&'a SnapshotObject>,
    target: Option<&'a SnapshotObject>,
}

impl Choice<'_> {
    fn target_name(&self) -> &str {
        self.target.map_or(self.item.name.as_str(), |target| {
            target.object.name.as_str()
        })
    }
}

type Tables<'a> = BTreeMap<Key, Result<Table<'a>, &'static str>>;

fn parse_tables<'a>(snapshot: &'a SchemaSnapshot, target: &SchemaSnapshot) -> Tables<'a> {
    snapshot
        .objects
        .iter()
        .filter(|object| object.object.kind == ObjectKind::Table)
        .map(|object| {
            (
                key(target, ObjectKind::Table, &object.object.name),
                Table::parse(snapshot, object),
            )
        })
        .collect()
}
pub(super) fn plan(
    source: &SchemaSnapshot,
    target: &SchemaSnapshot,
    items: &[Item],
    selected: &BTreeSet<usize>,
    options: PlanOptions,
) -> Result<Plan, Vec<String>> {
    let defaults = if options.sync_database_defaults {
        Some(
            database_defaults_diff(source, target)
                .ok_or_else(|| vec!["无法确认两端的数据库默认字符集和排序规则有差异。".into()])?
                .0,
        )
    } else {
        None
    };
    let choices = selected
        .iter()
        .filter_map(|&index| items.get(index))
        .map(|item| {
            (
                key(target, item.kind, &item.name),
                Choice {
                    item,
                    source: item.source.map(|index| &source.objects[index]),
                    target: item.target.map(|index| &target.objects[index]),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    if choices.is_empty() && defaults.is_none() {
        return Err(vec!["请选择要同步的变更。".into()]);
    }
    let mut issues = Vec::new();
    let mut review = BTreeSet::new();
    let mut table_edits = BTreeMap::<Key, TableEdit>::new();
    let mut drops = BTreeSet::new();
    let mut programs = BTreeMap::<Key, (Program, Settings)>::new();

    let left_tables = parse_tables(source, target);
    let right_tables = parse_tables(target, target);

    for (object_key, choice) in &choices {
        let item = choice.item;
        let name = qualified(&target.schema, choice.target_name());
        if !item.selectable() {
            issues.push(format!(
                "{name} · {}",
                item.problem().unwrap_or("定义不可用。")
            ));
            continue;
        }
        if item.difference == Difference::TargetOnly {
            drops.insert(object_key.clone());
            review.insert(format!("删除 {} {name}", sql_kind(item.kind)));
            continue;
        }
        let Some(object) = choice.source else {
            continue;
        };
        let prepared = (|| -> Result<(), &'static str> {
            let settings = Settings::from_definition(source, object, target)?;
            if item.kind == ObjectKind::Table {
                let definition = left_tables
                    .get(object_key)
                    .ok_or(TABLE_UNSUPPORTED)?
                    .as_ref()
                    .map_err(|&reason| reason)?;
                let edit = if choice.target.is_some() {
                    let current = right_tables
                        .get(object_key)
                        .ok_or(TABLE_UNSUPPORTED)?
                        .as_ref()
                        .map_err(|&reason| reason)?;
                    alter_table(definition, current, &name, settings)?
                } else {
                    TableEdit {
                        statement: Some(Step {
                            sql: definition.create(&target.schema, choice.target_name()),
                            settings,
                        }),
                        add_keys: definition
                            .foreign_keys
                            .iter()
                            .map(|foreign_key| foreign_key.name.clone())
                            .collect(),
                        ..TableEdit::default()
                    }
                };
                if edit.review {
                    review.insert(format!("修改表 {name} 的列、索引、外键或存储属性"));
                }
                table_edits.insert(object_key.clone(), edit);
            } else {
                let mut program =
                    Program::parse(source, object, &target.schema, options.ignore_definer)?;
                if !program.external.is_empty() {
                    return Err("定义引用了其他数据库，需手动确认依赖和目标位置。");
                }
                if choice.target.is_some() {
                    if item.kind == ObjectKind::View {
                        program.sql.insert_str("CREATE".len(), " OR REPLACE");
                    } else {
                        drops.insert(object_key.clone());
                    }
                    review.insert(format!("替换 {} {name}", sql_kind(item.kind)));
                } else if matches!(item.kind, ObjectKind::Trigger | ObjectKind::Event) {
                    review.insert(format!("创建可触发执行的 {} {name}", sql_kind(item.kind)));
                }
                programs.insert(object_key.clone(), (program, settings));
            }
            Ok(())
        })();
        if let Err(reason) = prepared {
            issues.push(format!("{name} · {reason}"));
        }
    }

    if !issues.is_empty() {
        return Err(issues);
    }
    let final_keys = target
        .objects
        .iter()
        .filter_map(|object| {
            let key = key(target, object.object.kind, &object.object.name);
            (!drops.contains(&key) || programs.contains_key(&key)).then_some(key)
        })
        .chain(
            choices
                .iter()
                .filter(|(_, choice)| choice.source.is_some())
                .map(|(key, _)| key.clone()),
        )
        .collect::<BTreeSet<_>>();
    let removed = drops
        .iter()
        .filter(|key| !programs.contains_key(*key))
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut existing_dependencies = BTreeMap::<Key, BTreeSet<Key>>::new();
    if !drops.is_empty() {
        for object in target
            .objects
            .iter()
            .filter(|object| object.object.kind != ObjectKind::Table)
        {
            let object_key = key(target, object.object.kind, &object.object.name);
            match Program::parse(target, object, &target.schema, false) {
                Ok(program) => {
                    let dependencies = program
                        .dependencies
                        .into_iter()
                        .map(|(kind, name)| key(target, kind, &name))
                        .collect::<BTreeSet<_>>();
                    if !drops.contains(&object_key) && !programs.contains_key(&object_key) {
                        for (_, missing) in dependencies.intersection(&removed) {
                            issues.push(format!(
                                "目标端对象 {} 仍依赖 {missing}。",
                                qualified(&target.schema, &object.object.name)
                            ));
                        }
                    }
                    existing_dependencies.insert(object_key, dependencies);
                }
                Err(_)
                    if !drops.contains(&object_key)
                        && !programs.contains_key(&object_key)
                        && !removed.is_empty()
                        && matches!(object.object.kind, ObjectKind::View | ObjectKind::Trigger) =>
                {
                    issues.push(format!(
                        "无法验证目标端依赖对象 {} 的定义。",
                        qualified(&target.schema, &object.object.name)
                    ));
                }
                _ => {}
            }
        }
    }

    let mut program_dependencies = BTreeMap::<Key, BTreeSet<Key>>::new();
    for (object_key, (program, _)) in &programs {
        let dependencies = program
            .dependencies
            .iter()
            .map(|(kind, name)| key(target, *kind, name))
            .collect::<BTreeSet<_>>();
        for (_, missing) in dependencies.difference(&final_keys) {
            issues.push(format!(
                "{} 缺少目标端依赖 {missing}。",
                qualified(&target.schema, &choices[object_key].item.name)
            ));
        }
        program_dependencies.insert(object_key.clone(), dependencies);
    }
    if target.omitted_objects != 0 && !removed.is_empty() {
        issues.push("目标端对象清单不完整，无法验证删除操作的依赖。".into());
    }
    if !programs.is_empty() {
        arrange_triggers(
            source,
            target,
            &choices,
            &final_keys,
            &mut programs,
            &mut program_dependencies,
            &mut issues,
        );
    }
    let mut skip_keys = BTreeSet::new();
    let mut key_drops = prepare_key_drops(
        target,
        &choices,
        &right_tables,
        &drops,
        &left_tables,
        &mut table_edits,
        &mut skip_keys,
        &mut review,
        &mut issues,
    );
    let key_additions = prepare_key_additions(
        target,
        &choices,
        &left_tables,
        &right_tables,
        &final_keys,
        &table_edits,
        &skip_keys,
        &mut issues,
    );
    let mut drop_order = topological_order(drops.iter().cloned(), &existing_dependencies)
        .map_err(|_| vec!["所选目标端对象存在无法按顺序删除的循环依赖。".into()])?;
    let create_order = topological_order(programs.keys().cloned(), &program_dependencies)
        .map_err(|_| vec!["所选源端对象存在无法按顺序创建的循环依赖。".into()])?;
    if !issues.is_empty() {
        issues.sort();
        issues.dedup();
        return Err(issues);
    }

    let mut steps = Vec::new();
    if let Some(defaults) = defaults {
        let name = quote_identifier(&target.schema);
        review.insert(format!(
            "修改数据库 {name} 新建对象使用的默认字符集和排序规则"
        ));
        steps.push(Step::simple(format!(
            "ALTER DATABASE {name} DEFAULT CHARACTER SET {} DEFAULT COLLATE {}",
            quote_identifier(&defaults.charset),
            quote_identifier(&defaults.collation),
        )));
    }
    let check_keys = !key_drops.is_empty()
        || !key_additions.is_empty()
        || drops.iter().any(|(kind, _)| *kind == ObjectKind::Table)
        || table_edits.values().any(|edit| {
            edit.engine_changed
                || !edit.changed_columns.is_empty()
                || !edit.removed_indexes.is_empty()
        });
    steps.append(&mut key_drops);
    drop_order.reverse();
    for (kind, name) in drop_order {
        let name = choices[&(kind, name)].target_name();
        steps.push(Step::simple(format!(
            "DROP {} {}",
            sql_kind(kind),
            qualified(&target.schema, name)
        )));
    }
    steps.extend(table_edits.into_values().filter_map(|edit| edit.statement));
    steps.extend(key_additions);
    for object_key in create_order {
        if let Some((program, settings)) = programs.remove(&object_key) {
            steps.push(Step {
                sql: program.sql,
                settings,
            });
        }
    }
    if steps.is_empty() {
        return Err(vec!["所选对象没有需要执行的结构变更。".into()]);
    }
    let statements = steps.len();
    let sql = render(&steps, check_keys).map_err(|reason| vec![reason.into()])?;
    Ok(Plan {
        sql,
        statements,
        review: review.into_iter().collect(),
        target: target.clone(),
        changes: steps
            .into_iter()
            .map(|Step { sql, settings }| SchemaChange {
                sql,
                mode: settings.mode,
                charset: settings.charset,
                collation: settings.collation,
                time_zone: settings.time_zone,
            })
            .collect(),
    })
}

fn current_keys(target: &SchemaSnapshot, tables: &Tables<'_>) -> Vec<SchemaForeignKey> {
    if let Some(foreign_keys) = &target.foreign_keys {
        return foreign_keys.clone();
    }
    let mut keys = Vec::new();
    for object in &target.objects {
        if let Some(Ok(table)) = tables.get(&key(target, object.object.kind, &object.object.name)) {
            for foreign_key in &table.foreign_keys {
                keys.push(SchemaForeignKey {
                    table: object.object.clone(),
                    name: foreign_key.name.clone(),
                    referenced_table: DbObject {
                        schema: foreign_key
                            .referenced_schema
                            .clone()
                            .unwrap_or_else(|| target.schema.clone()),
                        name: foreign_key.referenced_table.clone(),
                        kind: ObjectKind::Table,
                    },
                    columns: foreign_key
                        .columns
                        .iter()
                        .cloned()
                        .zip(foreign_key.referenced_columns.iter().cloned())
                        .collect(),
                    on_delete: foreign_key.delete.into(),
                    on_update: foreign_key.update.into(),
                });
            }
        }
    }
    keys
}

type KeyName = (Key, String);

#[allow(clippy::too_many_arguments)]
fn prepare_key_drops(
    target: &SchemaSnapshot,
    choices: &BTreeMap<Key, Choice<'_>>,
    target_tables: &Tables<'_>,
    drops: &BTreeSet<Key>,
    source_tables: &Tables<'_>,
    edits: &mut BTreeMap<Key, TableEdit>,
    skip: &mut BTreeSet<KeyName>,
    review: &mut BTreeSet<String>,
    issues: &mut Vec<String>,
) -> Vec<Step> {
    let mut clauses = BTreeMap::<String, BTreeSet<String>>::new();
    let mut remove = |table: &str, name: &str| {
        clauses
            .entry(table.into())
            .or_default()
            .insert(format!("DROP FOREIGN KEY {}", quote_identifier(name)));
        skip.insert((key(target, ObjectKind::Table, table), name.to_lowercase()));
    };
    for (object_key, edit) in &*edits {
        for name in &edit.drop_keys {
            remove(choices[object_key].target_name(), name);
        }
    }
    if target.foreign_keys.is_none()
        && (target_tables.values().any(Result::is_err) || target.omitted_objects != 0)
        && (drops.iter().any(|(kind, _)| *kind == ObjectKind::Table)
            || edits.values().any(|edit| {
                edit.engine_changed
                    || !edit.changed_columns.is_empty()
                    || !edit.removed_indexes.is_empty()
            }))
    {
        issues.push("目标端外键清单不完整，无法验证表、列和索引变更的依赖。".into());
    }
    for relation in current_keys(target, target_tables) {
        let child = key(target, ObjectKind::Table, &relation.table.name);
        let parent = key(target, ObjectKind::Table, &relation.referenced_table.name);
        let child_local = same_schema(target, &relation.table.schema);
        let parent_local = same_schema(target, &relation.referenced_table.schema);
        let child_removed = child_local && drops.contains(&child);
        let parent_removed = parent_local && drops.contains(&parent);
        let child_columns = relation
            .columns
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        let parent_columns = relation
            .columns
            .iter()
            .map(|(_, name)| name.clone())
            .collect::<Vec<_>>();
        let changed =
            |key, columns: &[String]| edits.get(key).is_some_and(|edit| edit.affects(columns));
        if child_removed {
            // Breaking links between tables that are both being dropped also handles FK cycles.
            if parent_removed && parent != child {
                remove(&relation.table.name, &relation.name);
            }
            continue;
        }
        if !parent_removed
            && !(child_local && changed(&child, &child_columns))
            && !(parent_local && changed(&parent, &parent_columns))
        {
            continue;
        }
        if child_local
            && edits.get(&child).is_some_and(|edit| {
                edit.drop_keys
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(&relation.name))
            })
        {
            continue;
        }
        if child_local
            && parent_local
            && !parent_removed
            && let Some(edit) = edits.get_mut(&child)
            && let Some(Ok(table)) = source_tables.get(&child)
            && let Some(foreign_key) = table.foreign_key(&relation.name)
        {
            remove(&relation.table.name, &relation.name);
            edit.add_keys.insert(foreign_key.name.clone());
            review.insert(format!(
                "重建 {} 的外键 {}",
                qualified(&target.schema, &relation.table.name),
                quote_identifier(&relation.name)
            ));
        } else {
            let owner = qualified(&relation.table.schema, &relation.table.name);
            let parent = qualified(
                &relation.referenced_table.schema,
                &relation.referenced_table.name,
            );
            issues.push(format!("{owner} 的外键 {} 仍引用 {parent}。请处理该外键后重新比较，或将其所属表一并纳入同步。", quote_identifier(&relation.name)));
        }
    }
    clauses
        .into_iter()
        .map(|(table, clauses)| {
            Step::simple(alter(
                &qualified(&target.schema, &table),
                &clauses.into_iter().collect::<Vec<_>>(),
            ))
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn prepare_key_additions(
    target: &SchemaSnapshot,
    choices: &BTreeMap<Key, Choice<'_>>,
    left: &Tables<'_>,
    right: &Tables<'_>,
    final_keys: &BTreeSet<Key>,
    edits: &BTreeMap<Key, TableEdit>,
    skip: &BTreeSet<KeyName>,
    issues: &mut Vec<String>,
) -> Vec<Step> {
    let existing = current_keys(target, right)
        .into_iter()
        .filter_map(|relation| {
            let child = key(target, ObjectKind::Table, &relation.table.name);
            let name = relation.name.to_lowercase();
            (same_schema(target, &relation.table.schema)
                && final_keys.contains(&child)
                && !skip.contains(&(child.clone(), name.clone())))
            .then_some((name, child))
        })
        .collect::<BTreeMap<_, _>>();
    let mut names = existing;
    let mut steps = Vec::new();
    for (table_key, edit) in edits {
        let Some(Ok(table)) = left.get(table_key) else {
            continue;
        };
        let owner = qualified(&target.schema, choices[table_key].target_name());
        let mut clauses = Vec::new();
        for name in &edit.add_keys {
            let foreign_key = table.foreign_key(name).expect("selected key");
            let parent_key = key(target, ObjectKind::Table, &foreign_key.referenced_table);
            let parent = if edits.contains_key(&parent_key) {
                left.get(&parent_key)
            } else {
                right.get(&parent_key)
            };
            let error = if foreign_key.referenced_schema.is_some() {
                Some("外键引用其他数据库，需手动验证。")
            } else if !final_keys.contains(&parent_key) {
                Some("缺少被引用表，请同时选择该表。")
            } else if names
                .get(&name.to_lowercase())
                .is_some_and(|key| key != table_key)
            {
                Some("目标数据库存在另一个同名外键。")
            } else if let Some(Ok(parent)) = parent {
                if valid_foreign_key(table, parent, foreign_key) {
                    let parent_name = choices.get(&parent_key).map_or_else(
                        || {
                            target
                                .objects
                                .iter()
                                .find(|object| {
                                    key(target, ObjectKind::Table, &object.object.name)
                                        == parent_key
                                        && object.object.kind == ObjectKind::Table
                                })
                                .map_or(foreign_key.referenced_table.as_str(), |object| {
                                    object.object.name.as_str()
                                })
                        },
                        Choice::target_name,
                    );
                    names.insert(name.to_lowercase(), table_key.clone());
                    clauses.push(foreign_key.sql(&target.schema, parent_name));
                    None
                } else {
                    Some("目标端外键两侧的列类型、字符集或可用索引不兼容。")
                }
            } else {
                Some("无法验证被引用表的定义。")
            };
            if let Some(error) = error {
                issues.push(format!("{owner} · {} · {error}", quote_identifier(name)));
            }
        }
        if !clauses.is_empty() {
            steps.push(Step::simple(alter(&owner, &clauses)));
        }
    }
    steps
}

fn valid_foreign_key(child: &Table<'_>, parent: &Table<'_>, key: &ForeignKey) -> bool {
    child.option("ENGINE").eq_ignore_ascii_case("InnoDB")
        && parent.option("ENGINE").eq_ignore_ascii_case("InnoDB")
        && parent
            .indexes
            .iter()
            .any(|index| index.ordinary && index_prefix(&index.columns, &key.referenced_columns))
        && child
            .indexes
            .iter()
            .any(|index| index.ordinary && index_prefix(&index.columns, &key.columns))
        && key
            .columns
            .iter()
            .zip(&key.referenced_columns)
            .all(|(child_name, parent_name)| {
                child
                    .column(child_name)
                    .zip(parent.column(parent_name))
                    .is_some_and(|(child, parent)| {
                        child.storage_type == parent.storage_type
                            && child.encoding == parent.encoding
                    })
            })
}

fn arrange_triggers(
    source: &SchemaSnapshot,
    target: &SchemaSnapshot,
    choices: &BTreeMap<Key, Choice<'_>>,
    final_keys: &BTreeSet<Key>,
    programs: &mut BTreeMap<Key, (Program, Settings)>,
    dependencies: &mut BTreeMap<Key, BTreeSet<Key>>,
    issues: &mut Vec<String>,
) {
    if programs
        .keys()
        .all(|(kind, _)| *kind != ObjectKind::Trigger)
    {
        return;
    }
    let mut groups = BTreeMap::<_, Vec<_>>::new();
    for object in source
        .objects
        .iter()
        .filter(|object| object.object.kind == ObjectKind::Trigger)
    {
        let object_key = key(target, ObjectKind::Trigger, &object.object.name);
        if !final_keys.contains(&object_key) {
            continue;
        }
        if let Ok(program) = Program::parse(source, object, &target.schema, false)
            && let Some(trigger) = program.trigger
        {
            groups
                .entry((trigger.table, trigger.timing, trigger.event))
                .or_default()
                .push((trigger.order, object_key, object.object.name.clone()));
        } else {
            issues.push(format!(
                "无法验证触发器 {} 的执行顺序。",
                quote_identifier(&object.object.name)
            ));
        }
    }
    let created = programs.keys().cloned().collect::<BTreeSet<_>>();
    for mut group in groups.into_values().filter(|group| group.len() > 1) {
        if group.iter().all(|(_, key, _)| !created.contains(key)) {
            continue;
        }
        group.sort();
        if group.iter().any(|(order, _, _)| order.is_none())
            || group.windows(2).any(|pair| pair[0].0 == pair[1].0)
        {
            issues.push("同一个表有多个相同事件和时机的触发器，缺少可验证的执行顺序。".into());
            continue;
        }
        for (index, (_, trigger, _)) in group.iter().enumerate() {
            let Some((program, _)) = programs.get_mut(trigger) else {
                continue;
            };
            if index > 0 {
                let (_, previous, name) = &group[index - 1];
                let target_name = choices
                    .get(previous)
                    .map_or(name.as_str(), Choice::target_name);
                program.ordered_after(true, target_name);
                if created.contains(previous) {
                    dependencies
                        .entry(trigger.clone())
                        .or_default()
                        .insert(previous.clone());
                }
            } else if let Some((_, following, name)) =
                group[1..].iter().find(|(_, key, _)| !created.contains(key))
            {
                let target_name = choices
                    .get(following)
                    .map_or(name.as_str(), Choice::target_name);
                program.ordered_after(false, target_name);
            }
        }
    }
}

fn topological_order(
    objects: impl IntoIterator<Item = Key>,
    dependencies: &BTreeMap<Key, BTreeSet<Key>>,
) -> Result<Vec<Key>, ()> {
    let mut remaining = objects.into_iter().collect::<BTreeSet<_>>();
    let mut ordered = Vec::with_capacity(remaining.len());
    while !remaining.is_empty() {
        let ready = remaining
            .iter()
            .filter(|key| {
                dependencies.get(*key).is_none_or(|required| {
                    required
                        .iter()
                        .all(|dependency| dependency == *key || !remaining.contains(dependency))
                })
            })
            .min_by_key(|(kind, _)| match kind {
                ObjectKind::Table => 0,
                ObjectKind::Function => 1,
                ObjectKind::Procedure => 2,
                ObjectKind::View => 3,
                ObjectKind::Trigger => 4,
                ObjectKind::Event => 5,
            })
            .cloned()
            .ok_or(())?;
        remaining.remove(&ready);
        ordered.push(ready);
    }
    Ok(ordered)
}

fn render(steps: &[Step], check_keys: bool) -> Result<String, &'static str> {
    let mut script = String::from("-- 结构同步。DDL 可能隐式提交，遇到错误需重新比较后再继续。\n");
    if check_keys {
        script.push_str("SET @harbor_sync_fk_checks = @@SESSION.foreign_key_checks;\nSET SESSION foreign_key_checks = 1;\n");
    }
    let save_mode = steps.iter().any(|step| step.settings.mode.is_some());
    let save_charset = steps.iter().any(|step| step.settings.charset.is_some());
    let save_collation = steps.iter().any(|step| step.settings.collation.is_some());
    let save_time = steps.iter().any(|step| step.settings.time_zone.is_some());
    if save_mode {
        script.push_str("SET @harbor_sync_sql_mode = @@SESSION.sql_mode;\n");
    }
    if save_collation {
        script.push_str("SET @harbor_sync_collation = @@SESSION.collation_connection;\n");
    }
    if save_charset {
        script.push_str("SET @harbor_sync_charset = @@SESSION.character_set_client;\n");
    }
    if save_time {
        script.push_str("SET @harbor_sync_time_zone = @@SESSION.time_zone;\n");
    }
    if save_mode || save_charset || save_collation || save_time {
        script.push_str("-- 如果会话设置未能恢复，请重新连接。\n");
    }
    for Step { sql, settings } in steps {
        if let Some(mode) = &settings.mode {
            writeln!(script, "\nSET SESSION sql_mode = '{mode}';").expect("write to string");
        }
        if let Some(collation) = &settings.collation {
            writeln!(script, "SET SESSION collation_connection = '{collation}';")
                .expect("write to string");
        }
        if let Some(charset) = &settings.charset {
            writeln!(script, "SET SESSION character_set_client = '{charset}';")
                .expect("write to string");
        }
        if let Some(time_zone) = &settings.time_zone {
            writeln!(script, "SET SESSION time_zone = '{time_zone}';").expect("write to string");
        }
        let mut wrapped = String::new();
        if sql.contains(';') || sql.contains('#') || sql.contains("--") {
            let mut delimiter = "$$".to_owned();
            while sql.contains(&delimiter) {
                delimiter.push('$');
            }
            writeln!(
                wrapped,
                "DELIMITER {delimiter}\n{sql}\n{delimiter}\nDELIMITER ;"
            )
            .expect("write to string");
        } else {
            writeln!(wrapped, "{sql};").expect("write to string");
        }
        if !matches!(split_script(&wrapped).as_slice(), [statement] if statement.sql == sql.as_str())
        {
            return Err("定义的引号或转义与 SQL 编辑器的语句分隔规则不兼容。");
        }
        script.push_str(&wrapped);
        if settings.time_zone.is_some() {
            script.push_str("SET SESSION time_zone = @harbor_sync_time_zone;\n");
        }
        if settings.charset.is_some() {
            script.push_str("SET SESSION character_set_client = @harbor_sync_charset;\n");
        }
        if settings.collation.is_some() {
            script.push_str("SET SESSION collation_connection = @harbor_sync_collation;\n");
        }
        if settings.mode.is_some() {
            script.push_str("SET SESSION sql_mode = @harbor_sync_sql_mode;\n");
        }
    }
    if check_keys {
        script.push_str("SET SESSION foreign_key_checks = @harbor_sync_fk_checks;\n");
    }
    Ok(script)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::{
        Command, ConnectionConfig, Response, SchemaApply, SchemaApplyStop, Worker,
    };
    use std::time::Duration;

    fn snapshot(schema: &str, definitions: &[(ObjectKind, &str, &str)]) -> SchemaSnapshot {
        SchemaSnapshot {
            schema: schema.into(),
            defaults: None,
            objects: definitions
                .iter()
                .map(|(kind, name, sql)| SnapshotObject {
                    object: DbObject {
                        schema: schema.into(),
                        name: (*name).into(),
                        kind: *kind,
                    },
                    definition: Ok((*sql).into()),
                    context: None,
                })
                .collect(),
            omitted_objects: 0,
            context: None,
            foreign_keys: None,
            origin: None,
        }
    }

    #[test]
    fn applies_selected_database_defaults_before_object_changes() {
        let mut source = snapshot(
            "source_db",
            &[(
                ObjectKind::Table,
                "items",
                "CREATE TABLE `items` (`id` int NOT NULL) ENGINE=InnoDB",
            )],
        );
        source.defaults = Some(SchemaDefaults {
            charset: "utf8mb4".into(),
            collation: "utf8mb4_unicode_ci".into(),
        });
        let mut target = snapshot("target_db", &[]);
        target.defaults = Some(SchemaDefaults {
            charset: "latin1".into(),
            collation: "latin1_swedish_ci".into(),
        });
        let items = differences(&source, &target, false);
        let selected = BTreeSet::from([0]);
        let objects_only = plan(&source, &target, &items, &selected, PlanOptions::default())
            .expect("preview selected table");
        let with_defaults = plan(
            &source,
            &target,
            &items,
            &selected,
            PlanOptions {
                sync_database_defaults: true,
                ..PlanOptions::default()
            },
        )
        .expect("preview selected database defaults and table");

        let alter = "ALTER DATABASE `target_db` DEFAULT CHARACTER SET `utf8mb4` DEFAULT COLLATE `utf8mb4_unicode_ci`";
        assert_eq!(objects_only.changes.len(), 1);
        assert_eq!(with_defaults.changes.len(), 2);
        assert_eq!(with_defaults.changes[0].sql, alter);
        assert_eq!(with_defaults.changes[1].sql, objects_only.changes[0].sql);
        let statements = split_script(&with_defaults.sql);
        assert_eq!(statements.len(), 2);
        assert_eq!(statements[0].sql.lines().last(), Some(alter));
        assert_eq!(statements[1].sql, objects_only.changes[0].sql);
    }

    #[test]
    fn stages_multiple_objects_and_keeps_the_source_qualified_only_in_literals() {
        let source = snapshot(
            "inventory",
            &[
                (
                    ObjectKind::View,
                    "a_nested",
                    "CREATE VIEW a_nested AS SELECT inventory.z_base.id FROM inventory.z_base",
                ),
                (
                    ObjectKind::View,
                    "z_base",
                    "CREATE VIEW z_base AS SELECT id, 'inventory.sales' AS label FROM inventory.sales",
                ),
                (
                    ObjectKind::Table,
                    "sales",
                    "CREATE TABLE sales (id int NOT NULL, PRIMARY KEY (id)) ENGINE=InnoDB AUTO_INCREMENT=9",
                ),
            ],
        );
        let target = snapshot("archive", &[]);
        let items = differences(&source, &target, false);
        let selected = (0..items.len()).collect();
        let plan =
            plan(&source, &target, &items, &selected, PlanOptions::default()).expect("build plan");
        for (before, after) in [
            (
                "CREATE TABLE \x60archive\x60.\x60sales\x60",
                "CREATE VIEW \x60archive\x60.\x60z_base\x60",
            ),
            (
                "CREATE VIEW \x60archive\x60.\x60z_base\x60",
                "CREATE VIEW \x60archive\x60.\x60a_nested\x60",
            ),
        ] {
            assert!(plan.sql.find(before).unwrap() < plan.sql.find(after).unwrap());
        }
        assert!(plan.sql.contains("'inventory.sales'"));
        assert!(!plan.sql.contains("FROM inventory."));
        assert!(!plan.sql.contains("AUTO_INCREMENT=9"));
        assert!(plan.review.is_empty());
    }

    #[test]
    fn detects_missing_dependencies_and_schema_local_foreign_keys() {
        let source = snapshot(
            "origin",
            &[
                (
                    ObjectKind::Table,
                    "child",
                    "CREATE TABLE child (id int, KEY child_id (id), CONSTRAINT child_fk FOREIGN KEY (id) REFERENCES origin.parent (id)) ENGINE=InnoDB",
                ),
                (
                    ObjectKind::Table,
                    "parent",
                    "CREATE TABLE parent (id int, PRIMARY KEY (id)) ENGINE=InnoDB",
                ),
            ],
        );
        let target = snapshot("destination", &[]);
        let items = differences(&source, &target, false);
        let partial = plan(
            &source,
            &target,
            &items,
            &BTreeSet::from([0]),
            PlanOptions::default(),
        );
        assert!(partial.err().unwrap()[0].contains("缺少被引用表"));
        let all = plan(
            &source,
            &target,
            &items,
            &BTreeSet::from([0, 1]),
            PlanOptions::default(),
        )
        .unwrap();
        assert!(
            all.sql
                .find("CREATE TABLE \x60destination\x60.\x60parent\x60")
                .unwrap()
                < all
                    .sql
                    .find("REFERENCES \x60destination\x60.\x60parent\x60")
                    .unwrap()
        );
    }

    #[test]
    fn protects_referenced_columns_and_rebuilds_selected_foreign_keys() {
        let make_schema = |schema, width| {
            let child = format!(
                "CREATE TABLE child (id {width}, KEY child_id (id), CONSTRAINT child_fk FOREIGN KEY (id) REFERENCES parent (id)) ENGINE=InnoDB"
            );
            let parent =
                format!("CREATE TABLE parent (id {width}, PRIMARY KEY (id)) ENGINE=InnoDB");
            snapshot(
                schema,
                &[
                    (ObjectKind::Table, "child", &child),
                    (ObjectKind::Table, "parent", &parent),
                ],
            )
        };
        let source = make_schema("source", "bigint");
        let target = make_schema("target", "int");
        let items = differences(&source, &target, false);
        let partial = plan(
            &source,
            &target,
            &items,
            &BTreeSet::from([1]),
            PlanOptions::default(),
        );
        assert!(partial.err().unwrap()[0].contains("child_fk"));
        let all = plan(
            &source,
            &target,
            &items,
            &BTreeSet::from([0, 1]),
            PlanOptions::default(),
        )
        .unwrap();
        assert!(!all.review.is_empty());
        let remove = all.sql.find("DROP FOREIGN KEY").unwrap();
        let modify = all.sql.find("MODIFY COLUMN").unwrap();
        let restore = all.sql.find("ADD CONSTRAINT").unwrap();
        assert!(remove < modify && modify < restore);
    }

    #[test]
    #[ignore = "requires the isolated loopback MariaDB fixture"]
    fn round_trip_on_disposable_mariadb() -> Result<(), String> {
        let port = std::env::var("HARBOR_STRUCTURE_FIXTURE_PORT")
            .map_err(|error| error.to_string())?
            .parse::<u16>()
            .map_err(|error| error.to_string())?;
        let worker = Worker::new();
        let mut id = 0;
        let mut request = |command| -> Result<Response, String> {
            id += 1;
            worker.send(id, command)?;
            let response = worker
                .recv_timeout(Duration::from_secs(8))?
                .ok_or("fixture timeout")?;
            if response.id != id {
                return Err("unexpected response".into());
            }
            response.result
        };
        request(Command::Connect(ConnectionConfig {
            host: "127.0.0.1".into(),
            port,
            user: "root".into(),
            ..Default::default()
        }))?;
        let source_schema = format!("harbor_structure_source_{}", std::process::id());
        let target_schema = format!("harbor_structure_target_{}", std::process::id());
        let request_query = |request: &mut dyn FnMut(Command) -> Result<Response, String>,
                             sql: &str| {
            match request(Command::Query {
                sql: sql.into(),
                limit: 50,
            })? {
                Response::Queried(report) => report.error.map_or(Ok(report.sets), Err),
                _ => Err("query response missing".into()),
            }
        };
        let request_snapshot = |request: &mut dyn FnMut(Command) -> Result<Response, String>,
                                schema: &str|
         -> Result<SchemaSnapshot, String> {
            match request(Command::SnapshotSchema {
                schema: schema.into(),
            })? {
                Response::Snapshot(snapshot) => Ok(snapshot),
                _ => Err("snapshot missing".into()),
            }
        };
        let result = (|| -> Result<(), String> {
            request_query(
                &mut request,
                &format!(
                    "CREATE DATABASE {source_schema} CHARACTER SET utf8mb4; CREATE DATABASE {target_schema} CHARACTER SET utf8mb4"
                ),
            )?;
            for (schema, setup) in [
                (&source_schema, "
                    CREATE TABLE parent (id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY, tag varchar(60) NOT NULL DEFAULT 'a,b', KEY tag_idx (tag)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci COMMENT='source table';
                    CREATE TABLE child (id BIGINT NOT NULL PRIMARY KEY, parent_id BIGINT, CONSTRAINT child_parent_fk FOREIGN KEY (parent_id) REFERENCES parent(id) ON DELETE CASCADE) ENGINE=InnoDB;
                    CREATE TABLE audit (note VARCHAR(20)) ENGINE=InnoDB;
                    CREATE TABLE linked (parent_id BIGINT, CONSTRAINT linked_parent_fk FOREIGN KEY (parent_id) REFERENCES parent(id)) ENGINE=InnoDB;
                    CREATE VIEW z_base AS SELECT id, tag FROM parent;
                    CREATE VIEW a_nested AS SELECT * FROM z_base;
                    CREATE FUNCTION label() RETURNS INT DETERMINISTIC RETURN 7;
                    CREATE PROCEDURE lookup_tag() SELECT label(), tag FROM parent;
                    CREATE TRIGGER zz_first BEFORE INSERT ON parent FOR EACH ROW INSERT INTO audit VALUES ('first');
                    CREATE TRIGGER aa_second BEFORE INSERT ON parent FOR EACH ROW FOLLOWS zz_first INSERT INTO audit VALUES ('second');
                    CREATE EVENT periodic ON SCHEDULE EVERY 1 DAY DISABLE DO INSERT INTO audit VALUES ('event');"),
                (&target_schema, "
                    CREATE TABLE parent (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, legacy INT, tag varchar(30) NOT NULL DEFAULT 'a,b', KEY tag_idx (tag(12))) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_general_ci;
                    CREATE TABLE child (id BIGINT NOT NULL PRIMARY KEY, parent_id INT, CONSTRAINT child_parent_fk FOREIGN KEY (parent_id) REFERENCES parent(id)) ENGINE=InnoDB;
                    INSERT INTO parent (id, legacy, tag) VALUES (4, 1, 'persist');
                    INSERT INTO child (id, parent_id) VALUES (1, 4);
                    CREATE VIEW old_view AS SELECT tag FROM parent;
                    CREATE TABLE legacy_table (id INT) ENGINE=InnoDB;"),
            ] {
                request_query(&mut request, &format!("USE {schema}; {setup}"))?;
                request_query(&mut request, "CREATE TABLE tx_guard (id INT PRIMARY KEY) ENGINE=InnoDB")?;
            }
            // The selected target is deliberately not the session's active database.
            request_query(&mut request, &format!("USE {source_schema}"))?;
            let source = request_snapshot(&mut request, &source_schema)?;
            let target = request_snapshot(&mut request, &target_schema)?;
            let items = differences(&source, &target, false);
            let selected = items
                .iter()
                .enumerate()
                .filter_map(|(index, item)| (item.difference != Difference::Same).then_some(index))
                .collect();
            let plan = plan(&source, &target, &items, &selected, PlanOptions::default())
                .map_err(|issues| issues.join("\n"))?;
            assert!(!plan.review.is_empty());
            request_query(
                &mut request,
                "START TRANSACTION; INSERT INTO tx_guard VALUES (1)",
            )?;
            let Response::SchemaApplied(applied) = request(Command::ApplySchema {
                apply: Box::new(SchemaApply {
                    expected: plan.target,
                    changes: plan.changes,
                }),
            })?
            else {
                return Err("schema apply response missing".into());
            };
            if let Some(stopped) = applied.stopped {
                return Err(match stopped {
                    SchemaApplyStop::Cancelled => "schema apply was cancelled".into(),
                    SchemaApplyStop::Failed { sql, error, .. } => format!("{error}\n{sql}"),
                });
            }
            assert_eq!(applied.completed, plan.statements);
            let rows = request_query(&mut request, "SELECT DATABASE(), @@in_transaction")?;
            assert_eq!(
                rows[0].rows[0],
                vec![
                    crate::database::Cell::Text(source_schema.clone()),
                    crate::database::Cell::Text("1".into()),
                ]
            );
            request_query(&mut request, "ROLLBACK")?;
            let rows = request_query(&mut request, "SELECT COUNT(*) FROM tx_guard")?;
            assert_eq!(rows[0].rows[0][0], crate::database::Cell::Text("0".into()));
            let after = request_snapshot(&mut request, &target_schema)?;
            let remaining = differences(&source, &after, false);
            let drift = remaining
                .iter()
                .filter(|item| item.difference != Difference::Same)
                .map(|item| {
                    (
                        item.name.as_str(),
                        item.difference.label(),
                        item.source.map(|index| &source.objects[index]),
                        item.target.map(|index| &after.objects[index]),
                    )
                })
                .collect::<Vec<_>>();
            if !drift.is_empty() {
                return Err(format!("structure drift {drift:?}\n{}", plan.sql));
            }
            let rows = request_query(
                &mut request,
                &format!("SELECT id, tag FROM {target_schema}.a_nested"),
            )?;
            assert!(
                matches!(rows[0].rows.as_slice(), [row] if row[0] == crate::database::Cell::Text("4".into()) && row[1] == crate::database::Cell::Text("persist".into()))
            );
            request_query(
                &mut request,
                &format!("INSERT INTO {target_schema}.parent (tag) VALUES ('new')"),
            )?;
            let rows = request_query(
                &mut request,
                &format!(
                    "SELECT note FROM {target_schema}.audit ORDER BY FIELD(note, 'first', 'second')"
                ),
            )?;
            assert_eq!(rows[0].rows.len(), 2);
            Ok(())
        })();
        let cleanup = request_query(
            &mut request,
            &format!(
                "DROP DATABASE IF EXISTS {target_schema}; DROP DATABASE IF EXISTS {source_schema}"
            ),
        );
        result.and(cleanup.map(|_| ()))
    }
}
