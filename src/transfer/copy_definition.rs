use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};

use crate::{
    database::{DbObject, ObjectKind, quote_identifier},
    sql_tools::{EditorToken, EditorTokenKind, definition_tokens},
};

const INVALID_HEADER: &str = "无法识别服务器返回的对象定义头。";
const DYNAMIC_SQL: &str = "定义执行了动态 SQL，无法确认运行时的数据库引用。请检查后手动迁移。";
const PREPARE_SEQUENCE: &str = "无法确认 EXECUTE 一定读取本例程刚准备的语句。请将同名的 PREPARE、EXECUTE 和释放操作放在连续的语句中，或手动迁移。";

pub(super) fn relocate(
    ddl: &str,
    object: &DbObject,
    destination: &str,
    sql_mode: &str,
    case_mode: u8,
) -> Result<String, String> {
    let destination = quote_identifier(destination);
    let mut definition = Definition::new(ddl, &object.schema, &destination, sql_mode, case_mode)?;
    let body = definition.relocate_header(object)?;
    definition.read_body(body, true)?;
    let mut output = String::with_capacity(ddl.len());
    let mut written = 0;
    for (start, (end, text)) in definition.edits {
        output.push_str(&ddl[written..start]);
        output.push_str(&text);
        written = end;
    }
    output.push_str(&ddl[written..]);
    Ok(output)
}

struct Token<'a> {
    lexeme: EditorToken<'a>,
    conditional: bool,
}

fn lex<'a>(
    text: &'a str,
    sql_mode: &str,
    offset: usize,
    conditional: bool,
    tokens: &mut Vec<Token<'a>>,
) -> Result<(), &'static str> {
    if text.contains('\0') {
        return Err("对象定义中含有 NUL 字符。");
    }
    for mut lexeme in definition_tokens(text, sql_mode) {
        match lexeme.kind {
            EditorTokenKind::Protected { closed } if lexeme.text.starts_with("/*") => {
                if !closed {
                    return Err("对象定义中的注释未闭合。");
                }
                let prefix = if lexeme.text.starts_with("/*!") {
                    3
                } else if lexeme.text.starts_with("/*M!") || lexeme.text.starts_with("/*m!") {
                    4
                } else {
                    continue;
                };
                if conditional {
                    return Err("对象定义中的嵌套执行注释需要手动检查。");
                }
                let code = &lexeme.text[prefix..lexeme.text.len() - 2];
                let guard = code.bytes().take_while(u8::is_ascii_digit).count();
                let guard = if matches!(guard, 5 | 6) { guard } else { 0 };
                lex(
                    &code[guard..],
                    sql_mode,
                    offset + lexeme.range.start + prefix + guard,
                    true,
                    tokens,
                )?;
                continue;
            }
            EditorTokenKind::Protected { .. } if lexeme.text.starts_with(['#', '-']) => continue,
            EditorTokenKind::Identifier { closed: false, .. }
            | EditorTokenKind::Protected { closed: false } => {
                return Err("对象定义中的标识符或字面量未闭合。");
            }
            _ => {}
        }
        lexeme.range.start += offset;
        lexeme.range.end += offset;
        tokens.push(Token {
            lexeme,
            conditional,
        });
    }
    Ok(())
}

impl<'a> Token<'a> {
    fn name(&self) -> Option<Cow<'a, str>> {
        let text = self.lexeme.text;
        match self.lexeme.kind {
            EditorTokenKind::Word if !text.bytes().all(|byte| byte.is_ascii_digit()) => {
                Some(Cow::Borrowed(text))
            }
            EditorTokenKind::Identifier { quote, .. } => {
                let end = char::from(if quote == b'[' { b']' } else { quote });
                let inner = &text[1..text.len() - 1];
                if inner.contains(end) {
                    let quote = end.to_string();
                    Some(Cow::Owned(inner.replace(&quote.repeat(2), &quote)))
                } else {
                    Some(Cow::Borrowed(inner))
                }
            }
            _ => None,
        }
    }
}

struct Definition<'a> {
    tokens: Vec<Token<'a>>,
    source: &'a str,
    destination: &'a str,
    sql_mode: &'a str,
    case_mode: u8,
    edits: BTreeMap<usize, (usize, Cow<'a, str>)>,
    verified_prepared: BTreeSet<usize>,
}

impl<'a> Definition<'a> {
    fn new(
        sql: &'a str,
        source: &'a str,
        destination: &'a str,
        sql_mode: &'a str,
        case_mode: u8,
    ) -> Result<Self, &'static str> {
        let mut tokens = Vec::new();
        lex(sql, sql_mode, 0, false, &mut tokens)?;
        Ok(Self {
            tokens,
            source,
            destination,
            sql_mode,
            case_mode,
            edits: BTreeMap::new(),
            verified_prepared: BTreeSet::new(),
        })
    }

    fn word(&self, position: usize, keyword: &str) -> bool {
        self.tokens.get(position).is_some_and(|token| {
            token.lexeme.kind == EditorTokenKind::Word
                && token.lexeme.text.eq_ignore_ascii_case(keyword)
        })
    }

    fn one_of(&self, position: usize, choices: &[&str]) -> bool {
        choices.iter().any(|keyword| self.word(position, keyword))
    }

    fn symbol(&self, position: usize, symbol: char) -> bool {
        self.tokens
            .get(position)
            .is_some_and(|token| token.lexeme.kind == EditorTokenKind::Symbol(symbol))
    }

    fn name(&self, position: usize) -> Option<Cow<'_, str>> {
        self.tokens.get(position).and_then(Token::name)
    }

    fn same(&self, name: &str, expected: &str, ignore_case: bool) -> bool {
        name == expected
            || (self.case_mode != 0 || ignore_case)
                && name.to_lowercase() == expected.to_lowercase()
    }

    fn source_name(&self, position: usize) -> bool {
        self.name(position)
            .is_some_and(|name| self.same(&name, self.source, false))
    }

    fn skip(&self, mut position: usize, choices: &[&str]) -> usize {
        while self.one_of(position, choices) {
            position += 1;
        }
        position
    }

    fn after_if_clause(&self, position: usize) -> usize {
        if self.word(position, "IF") && self.word(position + 1, "EXISTS") {
            position + 2
        } else if self.word(position, "IF")
            && self.word(position + 1, "NOT")
            && self.word(position + 2, "EXISTS")
        {
            position + 3
        } else {
            position
        }
    }

    fn object_kind(&self, position: usize) -> Option<ObjectKind> {
        [
            ("TABLE", ObjectKind::Table),
            ("VIEW", ObjectKind::View),
            ("PROCEDURE", ObjectKind::Procedure),
            ("FUNCTION", ObjectKind::Function),
            ("TRIGGER", ObjectKind::Trigger),
            ("EVENT", ObjectKind::Event),
        ]
        .into_iter()
        .find_map(|(word, kind)| self.word(position, word).then_some(kind))
    }

    fn create_name(&self, mut position: usize) -> Option<(ObjectKind, usize)> {
        if !self.word(position, "CREATE") {
            return None;
        }
        position += 1;
        if self.word(position, "OR") && self.word(position + 1, "REPLACE") {
            position += 2;
        }
        loop {
            if let Some(kind) = self.object_kind(position) {
                return Some((kind, self.after_if_clause(position + 1)));
            }
            if self.word(position, "TEMPORARY") {
                position += 1;
            } else if self.word(position, "ALGORITHM") && self.symbol(position + 1, '=') {
                position += 3;
            } else if self.word(position, "DEFINER") && self.symbol(position + 1, '=') {
                position += 3;
                if self.symbol(position, '@') {
                    position += 2;
                } else if self.symbol(position, '(') && self.symbol(position + 1, ')') {
                    position += 2;
                }
            } else if self.word(position, "SQL") && self.word(position + 1, "SECURITY") {
                position += 3;
            } else {
                return None;
            }
        }
    }

    fn has_qualifier(&self, position: usize) -> bool {
        self.symbol(position + 1, '.') && self.name(position + 2).is_some()
    }

    fn mark_reference(&mut self, position: usize) {
        if (self.has_qualifier(position)
            || self.symbol(position + 1, '.') && self.symbol(position + 2, '*'))
            && self.source_name(position)
        {
            let span = &self.tokens[position].lexeme.range;
            self.edits
                .insert(span.start, (span.end, Cow::Borrowed(self.destination)));
        }
    }

    fn relocate_header(&mut self, object: &DbObject) -> Result<usize, &'static str> {
        let (kind, start) = self.create_name(0).ok_or(INVALID_HEADER)?;
        if kind != object.kind {
            return Err(INVALID_HEADER);
        }
        let qualified = self.has_qualifier(start);
        let name = if qualified { start + 2 } else { start };
        let ignore_case = matches!(
            kind,
            ObjectKind::Procedure | ObjectKind::Function | ObjectKind::Event
        );
        if qualified && !self.source_name(start)
            || !self
                .name(name)
                .is_some_and(|name| self.same(&name, &object.name, ignore_case))
        {
            return Err("服务器返回的对象定义与所选源对象不一致。");
        }
        let span = &self.tokens[start].lexeme.range;
        let edit = if qualified {
            (span.end, Cow::Borrowed(self.destination))
        } else {
            (span.start, Cow::Owned(format!("{}.", self.destination)))
        };
        self.edits.insert(span.start, edit);
        let mut body = name + 1;
        if kind == ObjectKind::Trigger {
            let on = (body..self.tokens.len())
                .find(|&position| self.word(position, "ON"))
                .ok_or("无法识别触发器所属的表。")?;
            self.mark_reference(on + 1);
            body = (on + 2..self.tokens.len())
                .find(|&position| {
                    self.word(position, "FOR")
                        && self.word(position + 1, "EACH")
                        && self.word(position + 2, "ROW")
                })
                .map(|position| position + 3)
                .ok_or("无法识别触发器主体。")?;
        }
        Ok(body)
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Statement {
    #[default]
    Other,
    Select,
    Delete,
    Update,
    AlterObject,
    RenameTable,
    Index,
    Load,
    Privilege,
}

#[derive(Default)]
struct Scope {
    statement: Statement,
    relation_list: bool,
    next_relation: bool,
}

enum PreparedCommand {
    Prepare,
    Execute,
    Immediate,
    Release,
}

impl Scope {
    fn relation(&mut self) {
        self.relation_list = true;
        self.next_relation = true;
    }
}

impl Definition<'_> {
    fn read_body(&mut self, start: usize, allow_prepared: bool) -> Result<(), &'static str> {
        let mut scopes = vec![Scope::default()];
        for index in start..self.tokens.len() {
            let scope = scopes.last_mut().expect("root statement scope");
            if self.symbol(index, '(') {
                let grouping = std::mem::take(&mut scope.next_relation)
                    || index >= 2 && self.symbol(index - 1, '=') && self.word(index - 2, "UNION");
                let statement = scope.statement;
                scopes.push(Scope {
                    statement: if grouping {
                        statement
                    } else {
                        Statement::Other
                    },
                    relation_list: grouping,
                    next_relation: grouping,
                });
                continue;
            }
            if self.symbol(index, ')') {
                if scopes.len() > 1 {
                    scopes.pop();
                }
                continue;
            }
            if self.symbol(index, ';') {
                *scope = Scope::default();
                continue;
            }
            let previous = index.saturating_sub(1);
            if self.symbol(previous, '.') || self.symbol(previous, '@') {
                continue;
            }
            if scope.statement == Statement::Privilege {
                if self.word(index, "ON") {
                    self.mark_reference(self.skip(index + 1, &["TABLE", "FUNCTION", "PROCEDURE"]));
                    *scope = Scope::default();
                }
                continue;
            }
            if self.word(index, "SELECT") {
                *scope = Scope {
                    statement: Statement::Select,
                    ..Scope::default()
                };
            } else if scope.next_relation {
                if self.word(index, "LATERAL")
                    || scope.statement == Statement::Delete
                        && self.one_of(index, &["FROM", "LOW_PRIORITY", "QUICK", "IGNORE"])
                    || scope.statement == Statement::Update
                        && self.one_of(index, &["LOW_PRIORITY", "IGNORE"])
                {
                    continue;
                }
                scope.next_relation = false;
                if self.word(index, "WITH") {
                    *scope = Scope::default();
                } else {
                    self.mark_reference(index);
                }
            } else if self.word(index, "FROM")
                && matches!(scope.statement, Statement::Select | Statement::Delete)
                || self.word(index, "USING") && scope.statement == Statement::Delete
                || scope.relation_list
                    && (self.one_of(index, &["JOIN", "STRAIGHT_JOIN"]) || self.symbol(index, ','))
            {
                scope.relation();
            } else if self.ends_relation_list(index, previous) {
                scope.relation_list = false;
            } else {
                self.read_statement(index, previous, scope, allow_prepared)?;
            }

            // Dotted user/system variable names were excluded above. Outside
            // relation positions, only calls and three-part column refs qualify.
            if self.symbol(index + 3, '(')
                || self.symbol(index + 3, '.')
                    && (self.name(index + 4).is_some() || self.symbol(index + 4, '*'))
                || self.symbol(index + 3, '%') && self.word(index + 4, "ROWTYPE")
            {
                self.mark_reference(index);
            }
        }
        for (index, token) in self.tokens.iter().enumerate() {
            let previous = index.saturating_sub(1);
            if token.conditional
                && !self.symbol(previous, '@')
                && !self.symbol(previous, '.')
                && self.has_qualifier(index)
                && self.source_name(index)
                && !self.edits.contains_key(&token.lexeme.range.start)
            {
                return Err("执行注释中存在无法确认用途的源库引用。请检查后手动迁移。");
            }
        }
        Ok(())
    }

    fn ends_relation_list(&self, index: usize, previous: usize) -> bool {
        self.one_of(
            index,
            &[
                "WHERE",
                "HAVING",
                "UNION",
                "INTERSECT",
                "EXCEPT",
                "LIMIT",
                "RETURNING",
                "QUALIFY",
                "SET",
            ],
        ) || self.one_of(index, &["GROUP", "ORDER"])
            && self.word(index + 1, "BY")
            && !self.word(previous, "FOR")
            || self.word(index, "WINDOW") && self.word(index + 2, "AS")
    }

    fn read_statement(
        &mut self,
        index: usize,
        previous: usize,
        scope: &mut Scope,
        allow_prepared: bool,
    ) -> Result<(), &'static str> {
        let next = index + 1;
        if scope.statement != Statement::Select
            && let Some(command) = self.prepared_command(index, previous)
        {
            if !allow_prepared || self.tokens[index].conditional {
                return Err(DYNAMIC_SQL);
            }
            return self.read_prepared(index, command);
        }
        if self.one_of(index, &["GRANT", "REVOKE"]) {
            scope.statement = Statement::Privilege;
        } else if self.word(index, "DELETE") && !self.word(previous, "ON") {
            *scope = Scope {
                statement: Statement::Delete,
                relation_list: true,
                next_relation: true,
            };
        } else if self.word(index, "UPDATE") && !self.one_of(previous, &["FOR", "KEY", "ON"]) {
            scope.statement = Statement::Update;
            scope.relation();
        } else if self.one_of(index, &["INSERT", "REPLACE"]) && !self.symbol(next, '(') {
            let table = self.skip(
                next,
                &["INTO", "LOW_PRIORITY", "HIGH_PRIORITY", "DELAYED", "IGNORE"],
            );
            self.mark_reference(table);
        } else if self.one_of(
            index,
            &["CALL", "REFERENCES", "HANDLER", "DESCRIBE", "DESC"],
        ) {
            self.mark_reference(next);
        } else if (scope.statement == Statement::RenameTable && self.word(index, "TO"))
            || (scope.statement == Statement::AlterObject && self.word(index, "RENAME"))
        {
            self.mark_reference(self.skip(next, &["TO", "AS"]));
        } else if self.word(index, "LOAD") && self.one_of(next, &["DATA", "XML"]) {
            scope.statement = Statement::Load;
        } else if self.word(index, "INTO") && scope.statement == Statement::Load {
            self.mark_reference(self.skip(next, &["TABLE"]));
        } else if self.word(index, "ON") && scope.statement == Statement::Index {
            self.mark_reference(next);
            scope.statement = Statement::Other;
        } else if index >= 2
            && self.word(index, "OF")
            && self.word(previous, "TYPE")
            && self.word(index - 2, "ROW")
        {
            self.mark_reference(next);
        } else if self.word(index, "CREATE") {
            if let Some((kind, name)) = self.create_name(index) {
                self.mark_reference(name);
                let after = name + if self.has_qualifier(name) { 3 } else { 1 };
                if kind == ObjectKind::Table && self.word(after, "LIKE") {
                    self.mark_reference(after + 1);
                }
            } else if self.word(
                self.skip(next, &["OR", "REPLACE", "UNIQUE", "FULLTEXT", "SPATIAL"]),
                "INDEX",
            ) {
                scope.statement = Statement::Index;
            } else {
                self.check_schema_change(self.skip(next, &["OR", "REPLACE"]))?;
            }
        } else if self.one_of(
            index,
            &[
                "ALTER", "DROP", "TRUNCATE", "RENAME", "LOCK", "CHECK", "CHECKSUM", "OPTIMIZE",
                "ANALYZE", "REPAIR",
            ],
        ) {
            let kind = self.skip(next, &["TEMPORARY", "ONLINE", "OFFLINE", "IGNORE"]);
            self.check_schema_change(kind)?;
            if self.object_kind(kind).is_some() || self.word(kind, "TABLES") {
                let object = self.after_if_clause(kind + 1);
                self.mark_reference(object);
                if self.one_of(kind, &["TABLE", "TABLES", "VIEW"]) {
                    scope.relation_list = self.one_of(
                        index,
                        &[
                            "DROP", "RENAME", "LOCK", "CHECK", "CHECKSUM", "OPTIMIZE", "ANALYZE",
                            "REPAIR",
                        ],
                    );
                    if self.word(index, "RENAME") {
                        scope.statement = Statement::RenameTable;
                    }
                }
                if self.word(index, "ALTER") && self.one_of(kind, &["TABLE", "EVENT"]) {
                    scope.statement = Statement::AlterObject;
                }
            } else if self.word(index, "TRUNCATE") {
                self.mark_reference(kind);
            } else if self.word(index, "DROP") && self.word(kind, "INDEX") {
                scope.statement = Statement::Index;
            }
        } else if self.word(index, "USE")
            && !self.one_of(next, &["INDEX", "KEY"])
            && self.source_name(next)
        {
            return Err("对象定义中直接切换到了源数据库。请检查后手动迁移。");
        } else if self.word(index, "TABLE") {
            self.mark_reference(next);
        }
        Ok(())
    }

    fn prepared_command(&self, index: usize, previous: usize) -> Option<PreparedCommand> {
        if self.one_of(previous, &["DECLARE", "AS", "IN", "OUT", "INOUT"])
            || self.symbol(previous, ',')
        {
            return None;
        }
        let name = index + 1;
        let after = index + 2;
        if self.word(index, "PREPARE") && self.name(name).is_some() && self.word(after, "FROM") {
            Some(PreparedCommand::Prepare)
        } else if self.word(index, "EXECUTE") && self.word(name, "IMMEDIATE") {
            Some(PreparedCommand::Immediate)
        } else if self.word(index, "EXECUTE")
            && self.name(name).is_some()
            && (after == self.tokens.len()
                || self.symbol(after, ';')
                || self.one_of(after, &["USING", "END"]))
        {
            Some(PreparedCommand::Execute)
        } else if self.one_of(index, &["DEALLOCATE", "DROP"]) && self.word(name, "PREPARE") {
            Some(PreparedCommand::Release)
        } else {
            None
        }
    }

    fn read_prepared(
        &mut self,
        index: usize,
        command: PreparedCommand,
    ) -> Result<(), &'static str> {
        match command {
            PreparedCommand::Prepare => self.read_prepare_sequence(index),
            PreparedCommand::Execute | PreparedCommand::Release => self
                .verified_prepared
                .remove(&index)
                .then_some(())
                .ok_or(PREPARE_SEQUENCE),
            PreparedCommand::Immediate => {
                let literal = index + 2;
                let end = self.statement_end(literal + 1);
                if !self.has_using_or_ends(literal + 1, end) || !self.unconditional(index, end) {
                    return Err(DYNAMIC_SQL);
                }
                self.rewrite_statement_literal(literal)
            }
        }
    }

    fn read_prepare_sequence(&mut self, prepare: usize) -> Result<(), &'static str> {
        let literal = prepare + 3;
        let end = literal + 1;
        if !self.symbol(end, ';') {
            return Err(DYNAMIC_SQL);
        }
        // A handler can declare PREPARE as its one statement without executing
        // it on the normal path, or resume after a failed PREPARE.
        if (0..prepare).any(|index| {
            self.word(index, "DECLARE")
                && self.one_of(index + 1, &["CONTINUE", "EXIT", "UNDO"])
                && self.word(index + 2, "HANDLER")
        }) {
            return Err(PREPARE_SEQUENCE);
        }
        let name = self
            .name(prepare + 1)
            .ok_or(PREPARE_SEQUENCE)?
            .to_lowercase();
        let same_name = |index| {
            self.name(index)
                .is_some_and(|candidate| candidate.to_lowercase() == name)
        };
        let execute = end + 1;
        let execute_end = self.statement_end(execute);
        if !self.word(execute, "EXECUTE")
            || self.word(execute + 1, "IMMEDIATE")
            || !same_name(execute + 1)
            || !self.has_using_or_ends(execute + 2, execute_end)
            || !self.unconditional(prepare, execute_end)
        {
            return Err(PREPARE_SEQUENCE);
        }
        let release = execute_end + 1;
        let release =
            if self.one_of(release, &["DEALLOCATE", "DROP"]) && self.word(release + 1, "PREPARE") {
                let release_end = self.statement_end(release);
                if !same_name(release + 2)
                    || release_end != release + 3
                    || !self.unconditional(release, release_end)
                {
                    return Err(PREPARE_SEQUENCE);
                }
                Some(release)
            } else {
                None
            };
        self.rewrite_statement_literal(literal)?;
        self.verified_prepared.insert(execute);
        self.verified_prepared.extend(release);
        Ok(())
    }

    fn statement_end(&self, start: usize) -> usize {
        (start..self.tokens.len())
            .find(|&position| self.symbol(position, ';'))
            .unwrap_or(self.tokens.len())
    }

    fn has_using_or_ends(&self, position: usize, end: usize) -> bool {
        position == end || self.word(position, "USING") && end > position + 1
    }

    fn unconditional(&self, start: usize, end: usize) -> bool {
        self.tokens[start..end]
            .iter()
            .all(|token| !token.conditional)
            && self.tokens.get(end).is_none_or(|token| !token.conditional)
    }

    fn sets_sql_mode(&self) -> bool {
        (0..self.tokens.len()).any(|index| {
            let previous = index.saturating_sub(1);
            self.word(index, "SET")
                && (index == 0
                    || self.symbol(previous, ';')
                    || self.one_of(previous, &["BEGIN", "THEN", "ELSE", "DO", "LOOP", "REPEAT"]))
                && (index + 1..self.statement_end(index + 1)).any(|position| {
                    self.name(position)
                        .is_some_and(|name| name.eq_ignore_ascii_case("sql_mode"))
                        && (self.symbol(position + 1, '=') || self.symbol(position + 1, ':'))
                        && (!self.symbol(position - 1, '@')
                            || position
                                .checked_sub(2)
                                .is_some_and(|index| self.symbol(index, '@')))
                })
        })
    }

    fn rewrite_statement_literal(&mut self, position: usize) -> Result<(), &'static str> {
        const MODE_CHANGE: &str = "运行时修改 SQL 模式的准备语句需要手动迁移。";
        const ONE_STATEMENT: &str = "准备语句的字面量需要包含一条可静态迁移的 SQL。";
        let token = &self.tokens.get(position).ok_or(DYNAMIC_SQL)?.lexeme;
        let text = token.text;
        if !matches!(token.kind, EditorTokenKind::Protected { closed: true })
            || !text.starts_with(['\'', '"'])
        {
            return Err(DYNAMIC_SQL);
        }
        if self.sets_sql_mode() {
            return Err(MODE_CHANGE);
        }
        let quote = char::from(text.as_bytes()[0]);
        let backslash = !self
            .sql_mode
            .split(',')
            .any(|mode| mode.trim().eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES"));
        let (sql, offsets) = literal_contents(text, quote, backslash).ok_or(ONE_STATEMENT)?;
        let mut inner = Definition::new(
            &sql,
            self.source,
            self.destination,
            self.sql_mode,
            self.case_mode,
        )?;
        let end = inner.statement_end(0);
        if end == 0 || end + 1 < inner.tokens.len() {
            return Err(ONE_STATEMENT);
        }
        if inner.sets_sql_mode() {
            return Err(MODE_CHANGE);
        }
        inner.read_body(0, false)?;
        if inner.edits.is_empty() {
            return Ok(());
        }

        let mut revised = String::new();
        let mut cursor = 0;
        for (start, (end, replacement)) in inner.edits {
            revised.push_str(&text[cursor..offsets[start]]);
            for character in replacement.chars() {
                if character == quote || character == '\\' && backslash {
                    revised.push(character);
                }
                revised.push(character);
            }
            cursor = offsets[end];
        }
        revised.push_str(&text[cursor..]);
        self.edits
            .insert(token.range.start, (token.range.end, Cow::Owned(revised)));
        Ok(())
    }

    fn check_schema_change(&self, mut kind: usize) -> Result<(), &'static str> {
        if self.word(kind, "OR") && self.word(kind + 1, "REPLACE") {
            kind += 2;
        }
        if self.one_of(kind, &["DATABASE", "SCHEMA"])
            && self.source_name(self.after_if_clause(kind + 1))
        {
            return Err("对象定义中直接变更了源数据库。请检查后手动迁移。");
        }
        Ok(())
    }
}

// Map SQL byte positions back onto the original literal so only changed
// identifiers are re-escaped. Existing text and escape spelling stay intact.
fn literal_contents(text: &str, quote: char, backslash: bool) -> Option<(String, Vec<usize>)> {
    let mut sql = String::new();
    let mut offsets = vec![1];
    let mut chars = text[1..text.len() - 1].char_indices();
    let mut append = |character: char, end: usize| {
        sql.push(character);
        offsets.extend(std::iter::repeat_n(end, character.len_utf8()));
    };
    while let Some((mut index, mut character)) = chars.next() {
        if character == quote {
            let (_, second) = chars.next()?;
            if second != quote {
                return None;
            }
            index += 1;
        } else if character == '\\' && backslash {
            let (escape, escaped) = chars.next()?;
            if matches!(escaped, '%' | '_') {
                append('\\', index + 2);
            }
            index = escape;
            character = match escaped {
                '0' => '\0',
                'b' => '\u{0008}',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                'Z' => '\u{001a}',
                other => other,
            };
            append(character, index + escaped.len_utf8() + 1);
            continue;
        }
        append(character, index + character.len_utf8() + 1);
    }
    Some((sql, offsets))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relocates_literal_prepared_sql_with_original_escape_bytes() {
        let object = DbObject {
            schema: "旧库".into(),
            name: "copy_record".into(),
            kind: ObjectKind::Procedure,
        };
        let examples = [
            (
                "",
                "`旧库`",
                r#"CREATE PROCEDURE {header}copy_record(IN item_id INT)
BEGIN
    SET @copy_id = item_id;
    PREPARE Copy_Line FROM 'INSERT INTO {local} /* retain gap */ .order_line (ref_id, detail)
      VALUES (?, ''C:\\\\warehouse\\\\旧库.order_line'')';
    EXECUTE copy_line USING @copy_id;
    DEALLOCATE PREPARE COPY_LINE;
END"#,
            ),
            (
                "ANSI_QUOTES,NO_BACKSLASH_ESCAPES",
                "\"旧库\"",
                r#"CREATE PROCEDURE {header}"copy_record"(IN item_id INT)
BEGIN
    EXECUTE IMMEDIATE 'INSERT INTO {local}."audit_log" (ref_id, detail)
      VALUES (?, ''C:\warehouse\旧库.audit_log'')' USING item_id;
END"#,
            ),
        ];
        for (mode, source, template) in examples {
            let ddl = template.replace("{header}", "").replace("{local}", source);
            let expected = template
                .replace("{header}", "`新'库`.")
                .replace("{local}", "`新''库`");
            assert_eq!(relocate(&ddl, &object, "新'库", mode, 0).unwrap(), expected);
        }
    }

    #[test]
    fn relocates_show_create_definitions_with_preserved_aliases_and_sql_modes() {
        let standard = "`harbor_migrate_src`";
        let ansi = "\"harbor_migrate_src\"";
        let bare = "Harbor_Migrate_Src";
        let mode = "ANSI_QUOTES,NO_BACKSLASH_ESCAPES";
        let examples = [
            (
                ObjectKind::Table,
                "entries",
                "",
                standard,
                r#"CREATE TABLE {header}`entries` (
    `id` int NOT NULL,
    `catalog_id` int NOT NULL,
    `description` varchar(100) DEFAULT 'harbor_migrate_src.entries',
    CONSTRAINT `ref_catalog` FOREIGN KEY (`catalog_id`) REFERENCES {local} /* retain gap */ .`catalog` (`id`)
) ENGINE=InnoDB"#,
            ),
            (
                ObjectKind::View,
                "entry_report",
                "",
                standard,
                r#"CREATE ALGORITHM=UNDEFINED DEFINER=`harbor_migrate_src`@`127.0.0.1` SQL SECURITY INVOKER
VIEW {local}.`entry_report` AS
WITH details AS (
    SELECT {local}.`entries`.`id`, other.`labels`.`label`
    FROM ({local}.`entries`, other.`labels`)
)
SELECT harbor_migrate_src.`id`, harbor_migrate_src.`label`,
       {local}.`entry_total`() AS total_qty, "harbor_migrate_src.entries" AS literal_text
FROM details AS harbor_migrate_src
JOIN {local}.`entry_details` AS detail ON harbor_migrate_src.id = detail.id
-- do not replace harbor_migrate_src.entries here
WHERE detail.id > 0"#,
            ),
            (
                ObjectKind::Function,
                "entry_total",
                mode,
                ansi,
                r#"CREATE DEFINER="root"@"127.0.0.1" FUNCTION {header}"entry_total"() RETURNS int(11)
    READS SQL DATA
    DETERMINISTIC
RETURN (SELECT COALESCE(SUM(qty),0) FROM {local}."entries")"#,
            ),
            (
                ObjectKind::Procedure,
                "record_entry",
                mode,
                ansi,
                r#"CREATE DEFINER="root"@"127.0.0.1" PROCEDURE {header}"record_entry"(IN item_id INT)
    MODIFIES SQL DATA
BEGIN
    DECLARE execute INT DEFAULT 0;
    INSERT INTO {local}."entries" (id,catalog_id) VALUES(item_id,1);
    IF item_id > 0 THEN
        UPDATE LOW_PRIORITY IGNORE {local}."entries" AS harbor_migrate_src
        SET harbor_migrate_src.qty = {local}."entry_total"() WHERE harbor_migrate_src.id = item_id;
    ELSE
        DELETE LOW_PRIORITY FROM {local}."entries" WHERE id = item_id;
    END IF;
    CALL {local}."check_entry"(item_id);
    GRANT SELECT, DELETE ON {local}.* TO 'copy_reader'@'localhost';
    REVOKE EXECUTE ON PROCEDURE {local}."check_entry" FROM 'copy_reader'@'localhost';
    SET @harbor_migrate_src.note = 'c:\copy\harbor_migrate_src.entries';
END"#,
            ),
            (
                ObjectKind::Trigger,
                "entry_audit",
                "",
                bare,
                r#"CREATE DEFINER=`root`@`127.0.0.1` TRIGGER {local}.entry_audit
AFTER INSERT ON {local}.entries
FOR EACH ROW INSERT INTO {local}.audit_log(kind,detail)
VALUES ('trigger', CONCAT('harbor_migrate_src.entries#',NEW.id))"#,
            ),
            (
                ObjectKind::Event,
                "entry_snapshot",
                "",
                standard,
                r#"CREATE DEFINER=`root`@`127.0.0.1` EVENT {header}`entry_snapshot`
ON SCHEDULE EVERY 1 DAY STARTS '2037-01-01 00:00:00' ON COMPLETION NOT PRESERVE DISABLE DO
BEGIN
    INSERT INTO {local}.audit_log(kind,detail)
    VALUES ('event', CONCAT('harbor_migrate_src.audit_log=',{local}.entry_total()));
    SELECT COUNT(*) /*!50000 FROM {local}.entries */;
END"#,
            ),
        ];
        let target = "Target`Group";
        let quoted_target = "`Target``Group`";
        for (kind, name, mode, source_prefix, template) in examples {
            let object = DbObject {
                schema: "harbor_migrate_src".into(),
                name: name.into(),
                kind,
            };
            let source = template
                .replace("{local}", source_prefix)
                .replace("{header}", "");
            let expected = template
                .replace("{local}", quoted_target)
                .replace("{header}", &format!("{quoted_target}."));
            assert_eq!(
                relocate(&source, &object, target, mode, 1).unwrap(),
                expected
            );
        }
    }
}
