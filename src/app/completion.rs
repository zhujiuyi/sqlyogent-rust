mod cache;
pub(super) mod vocabulary;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::path::Path;

use eframe::egui::{self, Key, Modifiers, RichText, text::CCursor};
use serde::{Deserialize, Serialize};

use crate::database::{Command, DbObject, ObjectDetail, ObjectKind, Response, quote_identifier};
use crate::sql_tools::{EditorToken, EditorTokenKind, editor_tokens};

use super::{editor_tools, is_tabular, kind_label, state::Workspace, style};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum Read {
    Schemas,
    Objects(String),
    Detail(DbObject),
}

#[derive(Default)]
pub(super) struct Metadata {
    details: HashMap<DbObject, Details>,
    failed: HashSet<Read>,
    pending: Option<Read>,
    rebuild: Option<Rebuild>,
    cached: cache::Tags,
    store: Option<cache::Store>,
    schemas_ready: bool,
    cache_generation: u64,
    cache_dirty: bool,
    announce_rebuild_save: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Details {
    Columns {
        names: Vec<String>,
        #[serde(skip)]
        primary_keys: Vec<String>,
    },
    Parameters(Option<Vec<String>>),
}

struct Rebuild {
    remaining: VecDeque<String>,
    completed: usize,
    failed: usize,
}

impl Metadata {
    pub(super) fn active_rebuild(&self) -> bool {
        self.rebuild.is_some()
    }

    pub(super) fn columns(&self, object: &DbObject) -> Option<&[String]> {
        match self.details.get(object) {
            Some(Details::Columns { names, .. }) => Some(names),
            _ => None,
        }
    }

    pub(super) fn primary_keys(&self, object: &DbObject) -> Option<&[String]> {
        match self.details.get(object) {
            Some(Details::Columns { primary_keys, .. }) => Some(primary_keys),
            _ => None,
        }
    }

    pub(super) fn invalidate(&mut self, schema: Option<&str>) {
        self.cache_generation = self.cache_generation.wrapping_add(1);
        self.cache_dirty |= self.schemas_ready || schema.is_some();
        let Some(schema) = schema else {
            self.details.clear();
            self.failed.clear();
            self.cached = cache::Tags::default();
            return;
        };
        self.details.retain(|object, _| object.schema != schema);
        self.cached.objects.remove(schema);
        self.cached
            .details
            .retain(|object, _| object.schema != schema);
        self.clear_schema_failures(schema);
    }

    pub(super) fn objects_loaded(&mut self, schema: &str, objects: &[DbObject]) {
        // A catalog read confirms which previously loaded details still match an object.
        let current: HashSet<_> = objects.iter().collect();
        self.details
            .retain(|object, _| object.schema != schema || current.contains(object));
        self.cached.reconcile_schema(schema, &current);
        self.clear_schema_failures(schema);
        self.cache_dirty = true;
    }

    fn clear_schema_failures(&mut self, schema: &str) {
        self.failed.retain(|read| match read {
            Read::Schemas => true,
            Read::Objects(name) => name != schema,
            Read::Detail(object) => object.schema != schema,
        });
    }

    pub(super) fn remember(&mut self, detail: &ObjectDetail) {
        let object = &detail.object;
        let metadata = match object.kind {
            ObjectKind::Table | ObjectKind::View => Details::Columns {
                names: detail
                    .columns
                    .iter()
                    .map(|column| column.name.clone())
                    .collect(),
                primary_keys: detail
                    .columns
                    .iter()
                    .filter(|column| column.key == "PRI")
                    .map(|column| column.name.clone())
                    .collect(),
            },
            ObjectKind::Procedure | ObjectKind::Function => {
                Details::Parameters(routine_parameters(&detail.ddl, object.kind))
            }
            _ => return,
        };
        self.failed.remove(&Read::Detail(object.clone()));
        self.details.insert(object.clone(), metadata);
        self.cache_dirty = true;
    }

    fn completion_detail(&self, object: &DbObject) -> Option<&Details> {
        self.details
            .get(object)
            .or_else(|| self.cached.details.get(object))
    }

    fn needs(&self, read: &Read, workspace: &Workspace) -> bool {
        !self.failed.contains(read)
            && match read {
                Read::Schemas => false,
                Read::Objects(schema) => {
                    !workspace.objects.contains_key(schema)
                        && !self.cached.objects.contains_key(schema)
                }
                Read::Detail(object) => self.completion_detail(object).is_none(),
            }
    }
}

pub(super) fn read_detail(workspace: &mut Workspace, object: DbObject, label: &str) -> bool {
    let read = Read::Detail(object.clone());
    if workspace.send(Command::Inspect { object }, label) {
        workspace.tags.pending = Some(read);
        true
    } else {
        false
    }
}

pub(super) fn rebuild(workspace: &mut Workspace) {
    if workspace.server.is_some()
        && !workspace.busy()
        && !workspace.tags.active_rebuild()
        && workspace.send(Command::ListSchemas, "正在重建补全标签，读取数据库列表…")
    {
        workspace.tags.pending = Some(Read::Schemas);
        workspace.tags.rebuild = Some(Rebuild {
            remaining: VecDeque::new(),
            completed: 0,
            failed: 0,
        });
    }
}

pub(super) fn schedule_rebuild(workspace: &mut Workspace) {
    if workspace.busy() || workspace.server.is_none() {
        return;
    }
    let Some(rebuild) = &mut workspace.tags.rebuild else {
        return;
    };
    let Some(schema) = rebuild.remaining.pop_front() else {
        let Rebuild {
            completed, failed, ..
        } = workspace.tags.rebuild.take().unwrap();
        let status = if failed == 0 {
            workspace.tags.cache_dirty = true;
            workspace.tags.announce_rebuild_save = true;
            format!("已重建 {completed} 个数据库的补全标签。")
        } else {
            workspace.tags.cache_dirty = false;
            workspace.tags.announce_rebuild_save = false;
            format!("已重建 {completed} 个数据库的补全标签，{failed} 个读取失败。")
        };
        workspace.message(status, failed != 0);
        return;
    };
    if workspace.send(
        Command::ListObjects {
            schema: schema.clone(),
        },
        &format!("正在重建 {schema} 的补全标签…"),
    ) {
        workspace.tags.pending = Some(Read::Objects(schema));
    } else if let Some(rebuild) = &mut workspace.tags.rebuild {
        rebuild.failed += 1;
    }
}

/// Keep editor metadata reads from replacing the query results or object definition.
pub(super) fn receive(
    workspace: &mut Workspace,
    event: Result<Response, String>,
) -> Result<Option<Response>, String> {
    let Some(read) = workspace.tags.pending.take() else {
        return event.map(Some);
    };
    match event {
        Ok(Response::Inspected(detail)) => {
            workspace.tags.remember(&detail);
            workspace.status = format!("已读取 {} 的补全标签。", detail.object.name);
            Ok(None)
        }
        Ok(response) => {
            if let Some(rebuild) = &mut workspace.tags.rebuild
                && matches!(read, Read::Objects(_))
            {
                rebuild.completed += 1;
            }
            Ok(Some(response))
        }
        Err(error) => {
            workspace.tags.failed.insert(read.clone());
            if read == Read::Schemas {
                workspace.tags.rebuild = None;
                return Err(format!("无法重建补全标签：{error}"));
            }
            if let Some(rebuild) = &mut workspace.tags.rebuild {
                rebuild.failed += 1;
                workspace.message(format!("无法读取补全标签：{error}"), false);
            } else {
                workspace.status = format!("无法读取补全标签：{error}");
            }
            Ok(None)
        }
    }
}

pub(super) fn schemas_loaded(workspace: &mut Workspace) {
    workspace.tags.schemas_ready = true;
    if let Some(rebuild) = &mut workspace.tags.rebuild {
        rebuild.remaining = workspace.schemas.iter().cloned().collect();
    }
}

pub(super) fn poll_cache(workspace: &mut Workspace, directory: Option<&Path>, ctx: &egui::Context) {
    let Some(directory) = directory else {
        workspace.tags.store = None;
        return;
    };
    if workspace.server.is_none() || !workspace.tags.schemas_ready {
        return;
    }
    if workspace
        .tags
        .store
        .as_ref()
        .is_none_or(|store| store.directory.as_path() != directory)
    {
        let Some(profile) = &workspace.profile else {
            return;
        };
        if workspace.tags.active_rebuild() {
            return;
        }
        let mut store = cache::Store::new(directory, profile);
        // A completed rebuild should write its own snapshot even when the store is new.
        let started = if workspace.tags.announce_rebuild_save {
            Ok(())
        } else {
            store.load(ctx, workspace.tags.cache_generation)
        };
        workspace.tags.store = Some(store);
        workspace.tags.cache_dirty |= !workspace.objects.is_empty();
        if let Err(error) = started {
            cache_error(workspace, error);
        }
    }

    if let Some(result) = workspace.tags.store.as_mut().and_then(cache::Store::poll) {
        match result {
            Ok(cache::Outcome::Read {
                generation,
                snapshot: Some(snapshot),
            }) if workspace.tags.cache_generation == generation
                && !workspace.tags.active_rebuild() =>
            {
                let mut tags = snapshot.into_tags(&workspace.schemas);
                for (schema, objects) in &workspace.objects {
                    tags.reconcile_schema(schema, &objects.iter().collect());
                }
                workspace.tags.cached = tags;
            }
            Ok(cache::Outcome::Saved { announce }) => {
                if let Some(store) = &mut workspace.tags.store {
                    store.last_error = None;
                }
                if announce {
                    workspace.message("已保存当前连接的补全标签。", false);
                }
            }
            Err(error) => cache_error(workspace, error),
            _ => {}
        }
    }

    let tags = &workspace.tags;
    if !tags.cache_dirty || tags.active_rebuild() {
        return;
    }
    let Some(store) = tags.store.as_ref().filter(|store| store.idle()) else {
        return;
    };
    let snapshot = cache::Snapshot::capture(workspace, store.connection.clone());
    let tags = &mut workspace.tags;
    tags.cache_dirty = false;
    let announce = std::mem::take(&mut tags.announce_rebuild_save);
    if let Some(store) = &mut tags.store
        && let Err(error) = store.save(ctx, snapshot, announce)
    {
        cache_error(workspace, error);
    }
}

fn cache_error(workspace: &mut Workspace, error: String) {
    if let Some(store) = &mut workspace.tags.store {
        if store.last_error.as_ref() == Some(&error) {
            return;
        }
        store.last_error = Some(error.clone());
    }
    workspace.message(format!("{error} 当前会话仍可使用已读取的补全标签。"), false);
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    All,
    Matching,
    Qualified,
    Signature,
}

#[derive(Clone, Copy)]
pub(super) enum Input {
    Open(Mode),
    Move(isize),
    Accept,
    Close,
    Disabled,
    None,
}

#[derive(Default)]
pub(super) struct Completion {
    session: Option<Session>,
    demand: Option<(u64, Read)>,
}

struct Session {
    workspace: u64,
    tab: u64,
    mode: Mode,
    selected: usize,
    first: usize,
    caret: usize,
    span_start: Option<usize>,
}

impl Session {
    fn matches(&self, workspace: &Workspace) -> bool {
        self.workspace == workspace.key && self.tab == workspace.tab().id
    }
}

impl Completion {
    pub(super) fn close(&mut self) {
        self.session = None;
        self.demand = None;
    }

    pub(super) fn open(&mut self, mode: Mode, workspace: &Workspace) {
        self.session = Some(Session {
            workspace: workspace.key,
            tab: workspace.tab().id,
            mode,
            selected: 0,
            first: 0,
            caret: workspace.tab().focused_byte,
            span_start: None,
        });
    }

    pub(super) fn list_open(&self, workspace: &Workspace) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| session.matches(workspace) && session.mode != Mode::Signature)
    }

    pub(super) fn keyboard(
        &mut self,
        ui: &egui::Ui,
        editor: egui::Id,
        workspace: &Workspace,
        enabled: bool,
    ) -> Input {
        self.demand = None;
        if !enabled {
            self.close();
            return Input::Disabled;
        }
        if self
            .session
            .as_ref()
            .is_some_and(|session| !session.matches(workspace))
        {
            self.close();
        }
        if !ui.input(|input| input.focused) || !ui.memory(|memory| memory.has_focus(editor)) {
            return Input::None;
        }
        let mut action = Input::None;
        ui.input_mut(|input| {
            input.events.retain(|event| {
                let egui::Event::Key {
                    key,
                    modifiers,
                    pressed: true,
                    ..
                } = event
                else {
                    return true;
                };
                if !matches!(action, Input::None) {
                    return true;
                }
                action = if *key == Key::Space && modifiers.matches_exact(Modifiers::CTRL) {
                    Input::Open(Mode::Matching)
                } else if *key == Key::Space
                    && modifiers.matches_exact(Modifiers::CTRL | Modifiers::SHIFT)
                {
                    Input::Open(Mode::Signature)
                } else if *key == Key::Escape && self.session.is_some() {
                    Input::Close
                } else if self.list_open(workspace) {
                    match key {
                        Key::ArrowUp if *modifiers == Modifiers::NONE => Input::Move(-1),
                        Key::ArrowDown if *modifiers == Modifiers::NONE => Input::Move(1),
                        Key::Enter
                            if *modifiers == Modifiers::NONE
                                || modifiers.matches_exact(Modifiers::COMMAND) =>
                        {
                            Input::Accept
                        }
                        Key::Tab if *modifiers == Modifiers::NONE => Input::Accept,
                        _ => Input::None,
                    }
                } else {
                    Input::None
                };
                matches!(action, Input::None)
            });
        });
        action
    }

    pub(super) fn schedule_read(&mut self, workspace: &mut Workspace) {
        let Some((key, read)) = self.demand.take() else {
            return;
        };
        if workspace.key != key
            || workspace.busy()
            || workspace.server.is_none()
            || workspace.tags.active_rebuild()
            || workspace
                .tags
                .store
                .as_ref()
                .is_some_and(cache::Store::loading)
            || !workspace.tags.needs(&read, workspace)
        {
            return;
        }
        let command = match &read {
            Read::Schemas => return,
            Read::Objects(schema) => Command::ListObjects {
                schema: schema.clone(),
            },
            Read::Detail(object) => {
                read_detail(workspace, object.clone(), "正在读取 SQL 补全标签…");
                return;
            }
        };
        if workspace.send(command, "正在读取 SQL 补全标签…") {
            workspace.tags.pending = Some(read);
        }
    }

    pub(super) fn show(
        &mut self,
        ui: &egui::Ui,
        workspace: &mut Workspace,
        output: &egui::text_edit::TextEditOutput,
        input: Input,
        automatic: bool,
    ) {
        if !output.response.has_focus() || matches!(input, Input::Close | Input::Disabled) {
            self.close();
            return;
        }
        let tab = workspace.tab();
        let caret = tab.focused_byte;
        let trigger = tab
            .sql
            .get(..caret)
            .and_then(|text| text.chars().next_back());
        let changed = output.response.changed();
        if let Input::Open(mode) = input {
            self.open(mode, workspace);
        } else if automatic && changed && tab.selection.is_none() {
            match trigger {
                Some('.') => self.open(Mode::Qualified, workspace),
                Some('(') => self.open(Mode::Signature, workspace),
                _ => {}
            }
        }

        let Some(session) = self.session.as_mut() else {
            return;
        };
        if !session.matches(workspace)
            || (!changed && session.caret != caret && session.span_start.is_some())
        {
            self.close();
            return;
        }
        let scoped = Statement::new(workspace);
        if scoped.protected && session.mode != Mode::Signature {
            self.close();
            return;
        }
        let anchor = output
            .galley
            .pos_from_cursor(output.cursor_range.map_or_else(
                || CCursor::new(tab.sql[..caret].chars().count()),
                |range| range.primary,
            ))
            .translate(output.galley_pos.to_vec2());
        if !output.text_clip_rect.intersects(anchor) {
            self.close();
            return;
        }
        let popup_id = output.response.id.with("sql-completion");
        let mut open = true;
        let popup = egui::Popup::new(popup_id, ui.ctx().clone(), anchor, ui.layer_id())
            .gap(3.0)
            .open_bool(&mut open)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside);
        session.caret = caret;
        let mut suggestions = Suggestions::new(workspace);
        let mut chosen = None;
        if session.mode == Mode::Signature {
            let Some(call) = scoped.call() else {
                self.close();
                return;
            };
            let signature = suggestions.signature(&call);
            if let Some(parameters) = signature {
                popup.show(|ui| {
                    ui.set_max_width(420.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(format!("{}(", call.name)).monospace());
                        for (index, parameter) in parameters.iter().enumerate() {
                            let label = format!(
                                "{parameter}{}",
                                if index + 1 == parameters.len() {
                                    ""
                                } else {
                                    ","
                                }
                            );
                            let text = RichText::new(label).monospace();
                            ui.label(if index == call.argument {
                                text.strong().color(style::accent())
                            } else {
                                text
                            });
                        }
                        ui.label(RichText::new(")").monospace());
                    });
                });
            } else if suggestions.demand.is_some() {
                popup.show(|ui| {
                    ui.label("正在读取参数…");
                });
            } else {
                self.close();
                return;
            }
        } else {
            let span = scoped.span(tab.selection.clone());
            if session.mode == Mode::Qualified && span.qualifiers.is_empty() {
                self.close();
                return;
            }
            if session
                .span_start
                .is_some_and(|start| start != span.range.start)
            {
                self.close();
                return;
            }
            let reset_selection = session.span_start.is_none() || changed;
            if reset_selection {
                session.selected = 0;
            }
            if changed && session.span_start.is_some() && session.mode == Mode::All {
                session.mode = Mode::Matching;
            }
            session.span_start = Some(span.range.start);
            let candidates = suggestions.candidates(&scoped, &span, session.mode);
            let loading = suggestions.demand.is_some();
            if let Input::Move(delta) = input {
                session.selected = session.selected.saturating_add_signed(delta);
            }
            session.selected = session.selected.min(candidates.len().saturating_sub(1));
            if matches!(input, Input::Accept) {
                chosen = candidates
                    .get(session.selected)
                    .map(|candidate| (span.range, candidate.insertion(span.quote)));
                open = false;
            } else {
                let clicked = popup.show(|ui| {
                    show_candidates(ui, session, &candidates, loading, input, reset_selection)
                });
                if let Some(index) = clicked.and_then(|response| response.inner) {
                    chosen = Some((span.range, candidates[index].insertion(span.quote)));
                    open = false;
                }
            }
        }
        let demand = suggestions.demand;
        if let Some((range, text)) = chosen {
            editor_tools::change(ui.ctx(), workspace, range, &text, false);
            ui.ctx().request_repaint();
        }
        if open {
            self.demand = demand.map(|read| (workspace.key, read));
            ui.memory_mut(|memory| {
                memory.set_focus_lock_filter(
                    output.response.id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                )
            });
        } else {
            self.close();
        }
    }
}

fn show_candidates(
    ui: &mut egui::Ui,
    session: &mut Session,
    candidates: &[Candidate],
    loading: bool,
    input: Input,
    reset_selection: bool,
) -> Option<usize> {
    const VISIBLE_ROWS: usize = 8;
    ui.set_width(290.0);
    if candidates.is_empty() {
        ui.label(if loading {
            "正在读取补全标签…"
        } else {
            "没有匹配的补全项"
        });
        return None;
    }
    let height = ui.spacing().interact_size.y;
    let step = height + ui.spacing().item_spacing.y;
    let mut area = egui::ScrollArea::vertical().max_height(step * VISIBLE_ROWS as f32);
    if reset_selection {
        area = area.vertical_scroll_offset(0.0);
    } else if matches!(input, Input::Move(_)) {
        if session.selected < session.first {
            area = area.vertical_scroll_offset(session.selected as f32 * step);
        } else if session.selected >= session.first + VISIBLE_ROWS {
            area = area.vertical_scroll_offset((session.selected + 1 - VISIBLE_ROWS) as f32 * step);
        }
    }
    let reply = area.show_rows(ui, height, candidates.len(), |ui, rows| {
        let mut clicked = None;
        for number in rows {
            let candidate = &candidates[number];
            let selected = number == session.selected;
            let label = format!("{}  {}", candidate.name, candidate.kind.label());
            let mut job = egui::text::LayoutJob::default();
            job.append(
                &candidate.name,
                0.0,
                egui::TextFormat::simple(
                    egui::FontId::monospace(13.0),
                    if selected {
                        style::accent()
                    } else {
                        style::ink()
                    },
                ),
            );
            job.append(
                &format!("  {}", candidate.kind.label()),
                0.0,
                egui::TextFormat::simple(egui::FontId::proportional(11.5), style::muted()),
            );
            let response = ui
                .add_sized(
                    [ui.available_width(), height],
                    egui::Button::selectable(selected, job).truncate(),
                )
                .on_hover_text(label);
            if response.clicked() {
                clicked = Some(number);
            }
        }
        clicked
    });
    session.first = (reply.state.offset.y / step) as usize;
    reply.inner
}

struct Statement<'a> {
    tokens: Vec<EditorToken<'a>>,
    caret: usize,
    protected: bool,
}

struct Span {
    range: Range<usize>,
    prefix: String,
    quote: Option<u8>,
    qualifiers: Vec<String>,
}

struct Relation {
    schema: Option<String>,
    name: String,
    alias: Option<String>,
}

struct Call {
    schema: Option<String>,
    name: String,
    argument: usize,
}

impl<'a> Statement<'a> {
    fn new(workspace: &'a Workspace) -> Self {
        let tab = workspace.tab();
        let (no_backslash_escapes, ansi_quotes) = editor_tools::editor_quoting(workspace);
        Self::at(
            &tab.sql,
            tab.focused_byte,
            no_backslash_escapes,
            ansi_quotes,
        )
    }

    fn at(sql: &'a str, caret: usize, no_backslash_escapes: bool, ansi_quotes: bool) -> Self {
        let mut tokens = Vec::new();
        let mut protected = false;
        for token in editor_tokens(sql, no_backslash_escapes, ansi_quotes) {
            if is_symbol(&token, ';') {
                if token.range.start >= caret {
                    break;
                }
                tokens.clear();
                continue;
            }
            if let EditorTokenKind::Protected { closed } = token.kind {
                protected |= token.range.start < caret
                    && (caret < token.range.end || (!closed && caret == token.range.end));
                if token.text.starts_with(['#', '/', '-']) {
                    continue;
                }
            }
            tokens.push(token);
        }
        Self {
            tokens,
            caret,
            protected,
        }
    }

    fn span(&self, selection: Option<Range<usize>>) -> Span {
        let caret = self.caret;
        let mut index = self.tokens.partition_point(|token| token.range.end < caret);
        if self
            .tokens
            .get(index)
            .is_some_and(|token| is_symbol(token, '.') && token.range.end == caret)
        {
            index += 1;
        }
        let name = self.tokens.get(index).filter(|token| {
            token.range.start <= caret
                && matches!(
                    token.kind,
                    EditorTokenKind::Word | EditorTokenKind::Identifier { .. }
                )
        });
        let (range, prefix, quote) = match name {
            Some(token) => {
                let range = token.range.clone();
                let text = &token.text[..caret - token.range.start];
                match token.kind {
                    EditorTokenKind::Identifier { quote, closed } => {
                        let text = text.get(1..).unwrap_or("");
                        let text = if closed && caret == token.range.end {
                            &text[..text.len() - 1]
                        } else {
                            text
                        };
                        (range, unescape_name(text, quote).into_owned(), Some(quote))
                    }
                    _ => (range, text.to_owned(), None),
                }
            }
            None => {
                if self
                    .tokens
                    .get(index)
                    .is_some_and(|token| token.range.end == caret)
                {
                    index += 1;
                }
                (caret..caret, String::new(), None)
            }
        };
        let qualifiers = self.qualifiers(index);
        let range = selection.unwrap_or(range);
        Span {
            range,
            prefix,
            quote,
            qualifiers,
        }
    }

    fn qualifiers(&self, mut index: usize) -> Vec<String> {
        let mut names = Vec::new();
        while index >= 2 && names.len() < 2 && is_symbol(&self.tokens[index - 1], '.') {
            let Some(name) = identifier(&self.tokens[index - 2]) else {
                break;
            };
            names.push(name.into_owned());
            index -= 2;
        }
        names.reverse();
        names
    }

    fn relations(&self) -> impl Iterator<Item = Relation> + '_ {
        self.tokens.iter().enumerate().filter_map(|(index, token)| {
            if !["FROM", "JOIN", "UPDATE", "INTO"]
                .iter()
                .any(|word| is_word(token, word))
            {
                return None;
            }
            let (schema, name, after) = qualified_name(&self.tokens, index + 1)?;
            let next = self.tokens.get(after);
            let alias = if next.is_some_and(|token| is_word(token, "AS")) {
                self.tokens.get(after + 1).and_then(identifier)
            } else {
                next.filter(|token| {
                    token.kind != EditorTokenKind::Word || !vocabulary::is_keyword(token.text)
                })
                .and_then(identifier)
            };
            Some(Relation {
                schema,
                name,
                alias: alias.map(Cow::into_owned),
            })
        })
    }

    fn column_aliases(&self) -> Vec<String> {
        let mut depth = 0_usize;
        let mut projection = false;
        let mut aliases = Vec::new();
        for (index, token) in self.tokens.iter().enumerate() {
            match token.kind {
                EditorTokenKind::Symbol('(') => depth += 1,
                EditorTokenKind::Symbol(')') => depth = depth.saturating_sub(1),
                EditorTokenKind::Word if depth == 0 => {
                    if is_word(token, "SELECT") {
                        projection = true;
                    } else if is_word(token, "FROM") {
                        projection = false;
                    } else if projection
                        && is_word(token, "AS")
                        && let Some(name) = self.tokens.get(index + 1).and_then(identifier)
                    {
                        aliases.push(name.into_owned());
                    }
                }
                _ => {}
            }
        }
        aliases
    }

    fn call(&self) -> Option<Call> {
        let mut frames: Vec<Option<Call>> = Vec::new();
        for (index, token) in self
            .tokens
            .iter()
            .enumerate()
            .take_while(|(_, token)| token.range.start < self.caret)
        {
            match token.kind {
                EditorTokenKind::Symbol('(') => {
                    let call = index.checked_sub(1).and_then(|name_index| {
                        let name = identifier(&self.tokens[name_index])?;
                        let schema = self.qualifiers(name_index).pop();
                        Some(Call {
                            schema,
                            name: name.into_owned(),
                            argument: 0,
                        })
                    });
                    frames.push(call);
                }
                EditorTokenKind::Symbol(')') => {
                    frames.pop();
                }
                EditorTokenKind::Symbol(',') => {
                    if let Some(Some(call)) = frames.last_mut() {
                        call.argument += 1;
                    }
                }
                _ => {}
            }
        }
        frames.into_iter().rev().flatten().next()
    }
}

fn is_symbol(token: &EditorToken<'_>, symbol: char) -> bool {
    token.kind == EditorTokenKind::Symbol(symbol)
}

fn is_word(token: &EditorToken<'_>, word: &str) -> bool {
    token.kind == EditorTokenKind::Word && token.text.eq_ignore_ascii_case(word)
}

fn identifier<'a>(token: &EditorToken<'a>) -> Option<Cow<'a, str>> {
    match token.kind {
        EditorTokenKind::Word if !token.text.bytes().all(|byte| byte.is_ascii_digit()) => {
            Some(Cow::Borrowed(token.text))
        }
        EditorTokenKind::Identifier {
            quote,
            closed: true,
        } => Some(unescape_name(&token.text[1..token.text.len() - 1], quote)),
        _ => None,
    }
}

fn unescape_name(value: &str, quote: u8) -> Cow<'_, str> {
    let doubled = if quote == b'`' { "``" } else { "\"\"" };
    if value.contains(doubled) {
        Cow::Owned(value.replace(doubled, &char::from(quote).to_string()))
    } else {
        Cow::Borrowed(value)
    }
}

fn qualified_name(
    tokens: &[EditorToken<'_>],
    index: usize,
) -> Option<(Option<String>, String, usize)> {
    let first = identifier(tokens.get(index)?)?.into_owned();
    if tokens
        .get(index + 1)
        .is_some_and(|token| is_symbol(token, '.'))
    {
        let name = identifier(tokens.get(index + 2)?)?.into_owned();
        Some((Some(first), name, index + 3))
    } else {
        Some((None, first, index + 1))
    }
}

fn routine_parameters(ddl: &str, kind: ObjectKind) -> Option<Vec<String>> {
    let declaration = if kind == ObjectKind::Procedure {
        "PROCEDURE"
    } else {
        "FUNCTION"
    };
    let tokens: Vec<_> = editor_tokens(ddl, false, false).collect();
    let keyword = tokens
        .iter()
        .position(|token| is_word(token, declaration))?;
    let (_, _, open) = qualified_name(&tokens, keyword + 1)?;
    let opening = tokens.get(open)?;
    if !is_symbol(opening, '(') {
        return None;
    }
    let mut depth = 0_usize;
    let mut start = opening.range.end;
    let mut parameters = Vec::new();
    for token in &tokens[open + 1..] {
        match token.kind {
            EditorTokenKind::Symbol('(') => depth += 1,
            EditorTokenKind::Symbol(')') if depth > 0 => depth -= 1,
            EditorTokenKind::Symbol(')' | ',') if depth == 0 => {
                let parameter = ddl[start..token.range.start].trim();
                if !parameter.is_empty() {
                    parameters.push(parameter.to_owned());
                }
                if is_symbol(token, ')') {
                    return Some(parameters);
                }
                start = token.range.end;
            }
            _ => {}
        }
    }
    None
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Column,
    Alias,
    Object(ObjectKind),
    Schema,
    Keyword,
    Function,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::Column => "列",
            Self::Alias => "别名",
            Self::Object(kind) => kind_label(kind),
            Self::Schema => "数据库",
            Self::Keyword => "关键字",
            Self::Function => "内置函数",
        }
    }
}

struct Candidate {
    name: String,
    kind: Kind,
}

impl Candidate {
    fn insertion(&self, quote: Option<u8>) -> String {
        match self.kind {
            Kind::Keyword | Kind::Function => self.name.clone(),
            _ if quote == Some(b'"') => format!("\"{}\"", self.name.replace('"', "\"\"")),
            _ => quote_identifier(&self.name),
        }
    }
}

struct Suggestions<'a> {
    workspace: &'a Workspace,
    demand: Option<Read>,
}

impl<'a> Suggestions<'a> {
    fn new(workspace: &'a Workspace) -> Self {
        Self {
            workspace,
            demand: None,
        }
    }

    fn request(&mut self, read: Read) {
        if self.demand.is_none() && self.workspace.tags.needs(&read, self.workspace) {
            self.demand = Some(read);
        }
    }

    fn default_schema(&self) -> Option<&'a str> {
        self.workspace
            .server
            .as_ref()?
            .database
            .as_deref()
            .or_else(|| {
                self.workspace
                    .selected
                    .as_ref()
                    .map(|object| object.schema.as_str())
            })
    }

    fn schema(&self, name: &str) -> Option<&'a str> {
        self.workspace
            .schemas
            .iter()
            .find(|schema| *schema == name)
            .or_else(|| {
                self.workspace
                    .schemas
                    .iter()
                    .find(|schema| schema.eq_ignore_ascii_case(name))
            })
            .map(String::as_str)
            .or_else(|| {
                self.default_schema()
                    .filter(|schema| schema.eq_ignore_ascii_case(name))
            })
    }

    fn objects(&mut self, schema: &str) -> &'a [DbObject] {
        if let Some(objects) = self
            .workspace
            .objects
            .get(schema)
            .or_else(|| self.workspace.tags.cached.objects.get(schema))
        {
            return objects;
        }
        self.request(Read::Objects(schema.to_owned()));
        &[]
    }

    fn object(
        &mut self,
        schema: &str,
        name: &str,
        kinds: impl Fn(ObjectKind) -> bool,
    ) -> Option<&'a DbObject> {
        let objects = self.objects(schema);
        objects
            .iter()
            .find(|object| kinds(object.kind) && object.name == name)
            .or_else(|| {
                let mut folded = objects
                    .iter()
                    .filter(|object| kinds(object.kind) && object.name.eq_ignore_ascii_case(name));
                let first = folded.next()?;
                folded.next().is_none().then_some(first)
            })
    }

    fn columns(&mut self, schema: Option<&str>, table: &str) -> &'a [String] {
        let schema = schema
            .and_then(|name| self.schema(name))
            .or_else(|| schema.is_none().then(|| self.default_schema()).flatten());
        let Some(object) = schema.and_then(|schema| self.object(schema, table, is_tabular)) else {
            return &[];
        };
        if let Some(Details::Columns { names, .. }) = self.workspace.tags.completion_detail(object)
        {
            return names;
        }
        self.request(Read::Detail(object.clone()));
        &[]
    }

    fn signature(&mut self, call: &Call) -> Option<Vec<String>> {
        if call.schema.is_none()
            && let Some(parameters) = vocabulary::parameters(&call.name)
        {
            return Some(
                parameters
                    .iter()
                    .map(|parameter| (*parameter).to_owned())
                    .collect(),
            );
        }
        let schema = match call.schema.as_deref() {
            Some(schema) => self.schema(schema),
            None => self.default_schema(),
        }?;
        let object = self.object(schema, &call.name, |kind| {
            matches!(kind, ObjectKind::Function | ObjectKind::Procedure)
        })?;
        if let Some(Details::Parameters(parameters)) = self.workspace.tags.completion_detail(object)
        {
            return parameters.clone();
        }
        self.request(Read::Detail(object.clone()));
        None
    }

    fn candidates(&mut self, statement: &Statement<'_>, span: &Span, mode: Mode) -> Vec<Candidate> {
        let prefix = if mode == Mode::All {
            String::new()
        } else {
            span.prefix.to_lowercase()
        };
        let mut candidates = Vec::new();
        let mut push = |name: &str, kind: Kind| {
            if prefix.is_empty() || name.to_lowercase().starts_with(&prefix) {
                candidates.push(Candidate {
                    name: name.to_owned(),
                    kind,
                });
            }
        };
        match span.qualifiers.as_slice() {
            [schema, table] => {
                for column in self.columns(Some(schema), table) {
                    push(column, Kind::Column);
                }
            }
            [qualifier] => {
                let relation = statement.relations().find(|relation| {
                    relation
                        .alias
                        .as_deref()
                        .is_some_and(|alias| alias.eq_ignore_ascii_case(qualifier))
                });
                if let Some(relation) = relation {
                    for column in self.columns(relation.schema.as_deref(), &relation.name) {
                        push(column, Kind::Column);
                    }
                } else if let Some(schema) = self.schema(qualifier) {
                    for object in self.objects(schema) {
                        push(&object.name, Kind::Object(object.kind));
                    }
                } else {
                    for column in self.columns(None, qualifier) {
                        push(column, Kind::Column);
                    }
                }
            }
            [] => {
                if span.quote.is_none() {
                    for keyword in vocabulary::keywords() {
                        push(keyword, Kind::Keyword);
                    }
                    for name in vocabulary::functions() {
                        push(name, Kind::Function);
                    }
                }
                for name in &self.workspace.schemas {
                    push(name, Kind::Schema);
                }
                if let Some(schema) = self.default_schema() {
                    for object in self.objects(schema) {
                        push(&object.name, Kind::Object(object.kind));
                    }
                }
                for relation in statement.relations() {
                    if let Some(alias) = relation.alias.as_ref() {
                        push(alias, Kind::Alias);
                    }
                    for column in self.columns(relation.schema.as_deref(), &relation.name) {
                        push(column, Kind::Column);
                    }
                }
                for alias in statement.column_aliases() {
                    push(&alias, Kind::Alias);
                }
            }
            _ => unreachable!("at most two qualifiers"),
        }
        candidates.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.kind.cmp(&b.kind)));
        candidates.dedup_by(|a, b| a.name == b.name && a.kind == b.kind);
        candidates.sort_by_cached_key(|candidate| (candidate.kind, candidate.name.to_lowercase()));
        candidates
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::{Output, Profile};
    use crate::database::{ColumnInfo, ServerInfo};

    #[test]
    fn offers_database_objects_and_quoted_alias_columns() {
        let mut workspace = workspace_with_catalog();
        let tab = workspace.tab_mut();
        tab.sql = "SELECT * FROM 店铺.".into();
        tab.focused_byte = tab.sql.len();
        let statement = Statement::new(&workspace);
        let candidates = Suggestions::new(&workspace).candidates(
            &statement,
            &statement.span(None),
            Mode::Qualified,
        );
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].insertion(None), "`订单`");

        let tab = workspace.tab_mut();
        tab.sql = "SELECT o.`客` FROM `店铺`.`订单` AS o".into();
        tab.focused_byte = tab.sql.find("客").unwrap() + "客".len();
        let statement = Statement::new(&workspace);
        let span = statement.span(None);
        let candidates = Suggestions::new(&workspace).candidates(&statement, &span, Mode::Matching);
        assert_eq!(&workspace.tab().sql[span.range], "`客`");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].insertion(span.quote), "`客户名`");
    }

    #[test]
    fn finds_the_active_parameter_of_a_stored_procedure() {
        let mut workspace = workspace_with_catalog();
        let procedure = DbObject {
            schema: "店铺".into(),
            name: "查询订单".into(),
            kind: ObjectKind::Procedure,
        };
        workspace
            .objects
            .get_mut("店铺")
            .unwrap()
            .push(procedure.clone());
        workspace.tags.remember(&ObjectDetail {
            object: procedure,
            columns: Vec::new(),
            ddl: "CREATE DEFINER=`root`@`localhost` PROCEDURE `查询订单`(IN total DECIMAL(10,2), OUT result INT) BEGIN SELECT 1; END".into(),
            ddl_error: None,
        });
        let tab = workspace.tab_mut();
        tab.sql = "CALL `店铺`.`查询订单`(ROUND(15.5, 1), @result".into();
        tab.focused_byte = tab.sql.len();
        let call = Statement::new(&workspace).call().unwrap();
        assert_eq!(call.argument, 1);
        assert_eq!(
            Suggestions::new(&workspace).signature(&call).unwrap(),
            ["IN total DECIMAL(10,2)", "OUT result INT"]
        );
    }

    #[test]
    fn editor_shortcut_inserts_suggestion_and_undo_restores_original_text() {
        let mut workspace = Workspace::new("编辑器".into(), None);
        workspace.tab_mut().sql = "sel".into();
        let context = egui::Context::default();
        let mut completion = Completion::default();
        let id = editor_tools::editor_id(&workspace);
        context.memory_mut(|memory| memory.request_focus(id));
        let mut frame = |event: Option<egui::Event>| {
            let events = event.into_iter().collect();
            let _ = context.run(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let input = completion.keyboard(ui, id, &workspace, true);
                        let tab = workspace.tab_mut();
                        let output = egui::TextEdit::multiline(&mut tab.sql)
                            .id(id)
                            .code_editor()
                            .show(ui);
                        tab.focused_byte = super::super::byte_index(
                            &tab.sql,
                            output.cursor_range.unwrap().primary.index,
                        );
                        completion.show(ui, &mut workspace, &output, input, true);
                    });
                },
            );
            (
                workspace.tab().sql.clone(),
                completion.list_open(&workspace),
            )
        };
        frame(None);
        assert_eq!(
            frame(Some(key(Key::Space, Modifiers::CTRL))),
            ("sel".to_owned(), true)
        );
        assert_eq!(
            frame(Some(key(Key::Tab, Modifiers::NONE))),
            ("SELECT".to_owned(), false)
        );
        assert_eq!(
            frame(Some(key(Key::Z, Modifiers::COMMAND))),
            ("sel".to_owned(), false)
        );
    }

    #[test]
    fn rebuilds_live_catalog_and_reads_columns_without_replacing_output() {
        let Ok(port) = std::env::var("HARBOR_COMPLETION_TEST_PORT") else {
            return;
        };
        let mut profile = Profile {
            port: port.parse().unwrap(),
            schema: "information_schema".into(),
            ..Profile::default()
        };
        profile.advanced.displayed_databases = profile.schema.clone();
        let config = profile.config(String::new(), String::new(), String::new(), String::new());
        let mut workspace = Workspace::new("补全验证".into(), Some(profile));
        workspace.connect(config, None);
        wait_for_metadata(&mut workspace);
        assert_eq!(workspace.schemas, ["information_schema"]);

        rebuild(&mut workspace);
        wait_for_metadata(&mut workspace);
        assert!(
            workspace.status.starts_with("已重建 1 个数据库"),
            "{}",
            workspace.status
        );
        workspace.output = Output::History;
        let tab = workspace.tab_mut();
        tab.sql = "SELECT t.TABLE_SC FROM information_schema.TABLES AS t".into();
        tab.focused_byte = "SELECT t.TABLE_SC".len();

        let statement = Statement::new(&workspace);
        let mut suggestions = Suggestions::new(&workspace);
        assert!(
            suggestions
                .candidates(&statement, &statement.span(None), Mode::Matching)
                .is_empty()
        );
        let mut completion = Completion {
            demand: suggestions.demand.map(|read| (workspace.key, read)),
            ..Completion::default()
        };
        completion.schedule_read(&mut workspace);
        wait_for_metadata(&mut workspace);

        assert!(workspace.output == Output::History);
        let statement = Statement::new(&workspace);
        let names = Suggestions::new(&workspace).candidates(
            &statement,
            &statement.span(None),
            Mode::Matching,
        );
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].name, "TABLE_SCHEMA");
    }

    fn wait_for_metadata(workspace: &mut Workspace) {
        use std::time::{Duration, Instant};

        let deadline = Instant::now() + Duration::from_secs(10);
        while workspace.busy() || workspace.tags.active_rebuild() {
            assert!(Instant::now() < deadline, "{}", workspace.status);
            std::thread::sleep(Duration::from_millis(10));
            workspace.poll();
            schedule_rebuild(workspace);
        }
    }

    fn key(key: Key, modifiers: Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            modifiers,
            physical_key: None,
            pressed: true,
            repeat: false,
        }
    }

    fn workspace_with_catalog() -> Workspace {
        let mut workspace = Workspace::new("元数据".into(), None);
        workspace.server = Some(ServerInfo {
            version: "8.0".into(),
            connection_id: 1,
            database: Some("店铺".into()),
            http_tunnel: false,
        });
        workspace.schemas.push("店铺".into());
        let object = DbObject {
            schema: "店铺".into(),
            name: "订单".into(),
            kind: ObjectKind::Table,
        };
        workspace
            .objects
            .insert(object.schema.clone(), vec![object.clone()]);
        workspace.tags.remember(&ObjectDetail {
            object,
            columns: vec![ColumnInfo {
                name: "客户名".into(),
                data_type: "varchar(40)".into(),
                nullable: false,
                key: String::new(),
                default_value: None,
                extra: String::new(),
            }],
            ddl: String::new(),
            ddl_error: None,
        });
        workspace
    }
}
