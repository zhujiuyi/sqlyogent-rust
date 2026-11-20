use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    fmt::Write as _,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use eframe::egui::{
    self, Align2, Color32, FontId, Pos2, Rect, RichText, Stroke, StrokeKind, Vec2, pos2, vec2,
};
use rfd::FileDialog;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    database::{
        ColumnInfo, Command, DbObject, ObjectKind, SchemaDesign, SchemaDesignTable,
        SchemaForeignKey, SchemaSnapshot, quote_identifier,
    },
    maintenance::TableAction,
    sql_tools::split_script,
};

use super::{
    design_tabs::appearance,
    objects::Operation,
    state::{BrowseAction, Workspace, write_atomic},
    style,
};

mod bitmap;
mod mutation;
mod printing;
mod reconcile;
use mutation::{Effect, KeyChange, Mutation};
use printing::PrintPages;
use reconcile::Reconcile;

const FILE_LIMIT: usize = 16 * 1024 * 1024;
const TABLE_LIMIT: usize = 2_000;
const TITLE: f32 = 32.0;
const ROW: f32 = 22.0;
const CARD_WIDTH: f32 = 230.0;
const MIN_CARD: Vec2 = vec2(120.0, 64.0);
const MAX_CARD: Vec2 = vec2(1_600.0, 5_000.0);
const MIN_ZOOM: f32 = 0.10;
const MAX_ZOOM: f32 = 6.0;
const PAGE_WIDTH: f32 = 790.0;
const PAGE_HEIGHT: f32 = 1_100.0;

pub(super) enum Action {
    StageSql(String),
    Browse(BrowseAction),
    CreateTable(String),
    OpenLayout(PathBuf),
    ReturnToQuery,
}

#[derive(Clone, Serialize, Deserialize)]
struct Layout {
    version: u32,
    schema: String,
    zoom: f32,
    pan: [f32; 2],
    #[serde(default)]
    page_breaks: bool,
    tables: Vec<Card>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Card {
    name: String,
    position: [f32; 2],
    size: [f32; 2],
    names_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    definition: Option<String>,
}

impl Layout {
    fn new(schema: String) -> Self {
        Self {
            version: 1,
            schema,
            zoom: 1.0,
            pan: [16.0, 16.0],
            page_breaks: false,
            tables: Vec::new(),
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("此港湾架构布局版本不受支持。".into());
        }
        if (!self.schema.is_empty() && !identifier_ok(&self.schema))
            || !(MIN_ZOOM..=MAX_ZOOM).contains(&self.zoom)
            || !safe_coordinates(self.pan)
            || self.tables.len() > TABLE_LIMIT
        {
            return Err("布局的数据库、缩放位置或表数量无效。".into());
        }
        let mut names = HashSet::new();
        for card in &self.tables {
            if !identifier_ok(&card.name)
                || !names.insert(card.name.as_str())
                || !safe_coordinates(card.position)
                || !(MIN_CARD.x..=MAX_CARD.x).contains(&card.size[0])
                || !(MIN_CARD.y..=MAX_CARD.y).contains(&card.size[1])
            {
                return Err(format!("布局中的表名或卡片尺寸无效：{}", card.name));
            }
        }
        Ok(())
    }

    fn bounds(&self) -> Option<Rect> {
        self.tables
            .iter()
            .map(Card::world_rect)
            .reduce(|a, b| a.union(b))
    }
}

impl Card {
    fn world_rect(&self) -> Rect {
        Rect::from_min_size(
            pos2(self.position[0], self.position[1]),
            vec2(self.size[0], self.size[1]),
        )
    }
}

fn identifier_ok(value: &str) -> bool {
    !value.trim().is_empty() && !value.contains('\0') && value.chars().count() <= 64
}

fn safe_coordinates(value: [f32; 2]) -> bool {
    value.into_iter().all(|value| value.abs() <= 1_000_000.0)
}

type Tables<'a> = BTreeMap<&'a str, &'a SchemaDesignTable>;

fn tables_by_name(design: Option<&SchemaDesign>) -> Tables<'_> {
    design
        .into_iter()
        .flat_map(|design| design.tables.iter())
        .map(|table| (table.object.name.as_str(), table))
        .collect()
}

enum SnapshotUse {
    Save(SaveTarget),
    Check,
}

type FileRevision = [u8; 32];

struct SaveTarget {
    path: PathBuf,
    expected: Option<FileRevision>,
}

struct PendingSnapshot {
    schema: String,
    purpose: SnapshotUse,
}

#[derive(Clone)]
enum Drag {
    Pan,
    Move(usize),
    Resize(usize),
    Link { table: usize, field: String },
}

pub(super) struct Designer {
    workspace: u64,
    pub(super) closed: bool,
    layout: Layout,
    pub(super) path: Option<PathBuf>,
    saved_revision: Option<FileRevision>,
    pub(super) recent_files: VecDeque<PathBuf>,
    pub(super) dirty: bool,
    catalog_loading: Option<String>,
    catalog_failed: Option<String>,
    refresh: bool,
    snapshot_pending: Option<PendingSnapshot>,
    reconciliation: Option<Reconcile>,
    check_layout: bool,
    live_definitions: BTreeMap<String, String>,
    status: Option<Result<String, String>>,
    pending_close: bool,
    catalog_filter: String,
    selected_card: Option<String>,
    selected_key: Option<(String, String)>,
    row_offsets: BTreeMap<String, usize>,
    drag: Option<Drag>,
    draft_key: Option<ForeignKeyDraft>,
    delete_key: Option<SchemaForeignKey>,
    mutation: Option<Mutation>,
    fit: bool,
}

impl Designer {
    pub(super) fn new(workspace: u64, schema: String, table: Option<DbObject>) -> Self {
        let mut designer = Self {
            workspace,
            closed: false,
            layout: Layout::new(schema),
            path: None,
            saved_revision: None,
            recent_files: VecDeque::new(),
            dirty: false,
            catalog_loading: None,
            catalog_failed: None,
            refresh: false,
            snapshot_pending: None,
            reconciliation: None,
            check_layout: false,
            live_definitions: BTreeMap::new(),
            status: None,
            pending_close: false,
            catalog_filter: String::new(),
            selected_card: None,
            selected_key: None,
            row_offsets: BTreeMap::new(),
            drag: None,
            draft_key: None,
            delete_key: None,
            mutation: None,
            fit: false,
        };
        if let Some(table) = table.filter(|table| table.kind == ObjectKind::Table) {
            designer.layout.schema = table.schema;
            designer.add_card(&table.name, 9);
        }
        designer
    }

    fn schema(&self) -> &str {
        &self.layout.schema
    }

    pub(super) fn from_file(workspace: u64, path: PathBuf) -> Result<Self, String> {
        let (layout, saved_revision) = read_layout(&path)?;
        let mut designer = Self::new(workspace, String::new(), None);
        designer.reconciliation = Some(Reconcile::new(layout.schema.clone()));
        designer.layout = layout;
        designer.recent_files.push_back(path.clone());
        designer.path = Some(path);
        designer.saved_revision = Some(saved_revision);
        Ok(designer)
    }

    pub(super) fn focus_table(&mut self, table: &DbObject) {
        if table.schema == self.layout.schema && table.kind == ObjectKind::Table {
            self.add_card(&table.name, 9);
            self.selected_card = Some(table.name.clone());
            self.fit = true;
        }
    }

    pub(super) fn accepts_table(&self, schema: &str) -> bool {
        !self.closed && self.schema() == schema && self.reconciliation.is_none()
    }

    fn change_schema(&mut self, schema: String) {
        self.layout.schema = schema;
        self.dirty |= !self.layout.tables.is_empty() || self.path.is_some();
        self.catalog_failed = None;
        self.live_definitions.clear();
        self.row_offsets.clear();
        self.drag = None;
        self.check_layout = self
            .layout
            .tables
            .iter()
            .any(|card| card.definition.is_some());
        self.selected_key = None;
        self.draft_key = None;
        self.delete_key = None;
        self.status = None;
    }

    fn add_card(&mut self, name: &str, columns: usize) {
        if self.layout.tables.len() == TABLE_LIMIT
            || self.layout.tables.iter().any(|card| card.name == name)
        {
            return;
        }
        let width = CARD_WIDTH;
        let height = (TITLE + (columns.max(2) as f32) * ROW + 12.0).min(276.0);
        let first_slot = self.layout.tables.len();
        let position = (first_slot..=first_slot * 2)
            .map(|slot| pos2((slot % 3) as f32 * 278.0, (slot / 3) as f32 * 320.0))
            .find(|position| {
                let bounds = Rect::from_min_size(*position, vec2(width, height)).shrink(2.0);
                self.layout
                    .tables
                    .iter()
                    .all(|card| !card.world_rect().intersects(bounds))
            })
            .unwrap_or_else(|| {
                pos2(
                    0.0,
                    self.layout
                        .bounds()
                        .map_or(0.0, |bounds| bounds.bottom() + 30.0),
                )
            });
        self.layout.tables.push(Card {
            name: name.into(),
            position: [position.x, position.y],
            size: [width, height],
            names_only: false,
            definition: None,
        });
        self.dirty = true;
    }

    fn remove_card(&mut self, index: usize) {
        let removed = self.layout.tables.remove(index);
        self.row_offsets.remove(&removed.name);
        if self.selected_card.as_deref() == Some(removed.name.as_str()) {
            self.selected_card = None;
        }
        self.selected_key = None;
        self.dirty = true;
    }

    fn update(&mut self, ws: &mut Workspace) {
        self.advance(ws, true);
    }

    pub(super) fn has_pending_work(&self) -> bool {
        self.mutation.is_some()
            || self.snapshot_pending.is_some()
            || self.catalog_loading.is_some()
            || self.reconciliation.as_ref().is_some_and(Reconcile::waiting)
    }

    pub(super) fn advance(&mut self, ws: &mut Workspace, show: bool) {
        if ws.busy() {
            return;
        }
        if let Some(reconciliation) = &mut self.reconciliation {
            reconciliation.poll(ws, &self.layout);
            return;
        }
        if self.mutation.is_some() {
            self.poll_mutation(ws);
            return;
        }
        if let Some(pending) = self.snapshot_pending.take()
            && pending.schema == self.layout.schema
        {
            let Some(snapshot) = ws.snapshots.get(&pending.schema) else {
                self.status = Some(Err(ws.status.clone()));
                return;
            };
            self.live_definitions = snapshot_definitions(snapshot);
            if let SnapshotUse::Save(target) = pending.purpose {
                self.write_layout(target, true);
            }
        }
        if let Some(schema) = self.catalog_loading.take()
            && schema == self.layout.schema
        {
            if ws.schema_designs.contains_key(&schema) {
                self.status = None;
            } else {
                self.catalog_failed = Some(schema);
                self.status = Some(Err(ws.status.clone()));
            }
        }
        if !show || self.closed || ws.server.is_none() {
            return;
        }
        if self.layout.schema.is_empty() {
            self.layout.schema = ws
                .server
                .as_ref()
                .and_then(|server| server.database.clone())
                .or_else(|| ws.schemas.first().cloned())
                .unwrap_or_default();
        }
        if !ws.schemas.contains(&self.layout.schema) {
            return;
        }
        let refreshing = std::mem::take(&mut self.refresh);
        if refreshing
            || (!ws.schema_designs.contains_key(&self.layout.schema)
                && self.catalog_failed.as_ref() != Some(&self.layout.schema))
        {
            let schema = self.layout.schema.clone();
            if ws.send(
                Command::DescribeSchemaDesign {
                    schema: schema.clone(),
                },
                "正在读取表字段和外键…",
            ) {
                ws.schema_designs.remove(&schema);
                self.catalog_loading = Some(schema);
            } else {
                self.catalog_failed = Some(schema);
                self.status = Some(Err(ws.status.clone()));
            }
        } else if self.check_layout {
            self.check_layout = false;
            let tables = tables_by_name(ws.schema_designs.get(&self.layout.schema));
            let names = self
                .layout
                .tables
                .iter()
                .filter(|table| {
                    table.definition.is_some() && tables.contains_key(table.name.as_str())
                })
                .map(|table| table.name.clone())
                .collect::<Vec<_>>();
            if !names.is_empty() {
                self.snapshot(ws, names, SnapshotUse::Check);
            }
        }
    }

    fn snapshot(&mut self, ws: &mut Workspace, names: Vec<String>, purpose: SnapshotUse) -> bool {
        let schema = self.layout.schema.clone();
        if ws.send(
            Command::SnapshotDesignTables {
                schema: schema.clone(),
                names,
            },
            "正在读取布局表的建表语句…",
        ) {
            ws.snapshots.remove(&schema);
            self.snapshot_pending = Some(PendingSnapshot { schema, purpose });
            true
        } else {
            self.status = Some(Err(ws.status.clone()));
            false
        }
    }

    pub(super) fn save(&mut self, ws: &mut Workspace, save_as: bool) {
        if self.mutation.is_some()
            || self.snapshot_pending.is_some()
            || self.reconciliation.is_some()
        {
            return;
        }
        let existing_document = !save_as && self.path.is_some();
        let path = if existing_document {
            self.path.clone()
        } else {
            FileDialog::new()
                .set_title("保存架构布局")
                .add_filter("港湾架构布局", &["hschema"])
                .add_filter("SQLyog 架构 XML", &["schemaxml"])
                .set_file_name("架构布局.hschema")
                .save_file()
        };
        let Some(path) = path else { return };
        let expected = if existing_document {
            self.saved_revision
        } else {
            match layout_revision(&path) {
                Ok(revision) => revision,
                Err(error) => {
                    self.status = Some(Err(format!("无法核对布局保存路径。{error}")));
                    return;
                }
            }
        };
        let target = SaveTarget { path, expected };
        let tables = tables_by_name(ws.schema_designs.get(&self.layout.schema));
        let names = self
            .layout
            .tables
            .iter()
            .filter(|card| tables.contains_key(card.name.as_str()))
            .map(|card| card.name.clone())
            .collect::<Vec<_>>();
        if names.is_empty() {
            self.write_layout(target, false);
        } else {
            self.snapshot(ws, names, SnapshotUse::Save(target));
        }
    }

    fn write_layout(&mut self, target: SaveTarget, use_live: bool) {
        let SaveTarget { path, expected } = target;
        let mut candidate = self.layout.clone();
        if use_live {
            for card in &mut candidate.tables {
                if let Some(sql) = self.live_definitions.get(&card.name) {
                    card.definition = Some(sql.clone());
                }
            }
        }
        for card in &mut candidate.tables {
            if card
                .definition
                .as_ref()
                .is_some_and(|sql| layout_definition(sql).is_none())
            {
                card.definition = None;
            }
        }
        let save = (|| {
            let bytes = encode_layout(&candidate, &path)?;
            let lock = File::options()
                .write(true)
                .create(true)
                .truncate(false)
                .open(path.with_file_name(".harbor-layout.lock"))
                .map_err(|error| format!("无法创建布局保存锁。{error}"))?;
            lock.try_lock()
                .map_err(|error| format!("无法取得布局保存锁。{error}"))?;
            if layout_revision(&path)? != expected {
                return Err("布局文件已在其他页签或程序中修改，请另存为其他文件。".into());
            }
            write_atomic(&path, &bytes).map_err(|error| error.to_string())?;
            Ok::<_, String>(Sha256::digest(bytes).into())
        })();
        match save {
            Ok(revision) => {
                self.layout = candidate;
                self.recent_files.push_back(path.clone());
                self.path = Some(path);
                self.saved_revision = Some(revision);
                self.dirty = false;
                let missing = self
                    .layout
                    .tables
                    .iter()
                    .filter(|card| card.definition.is_none())
                    .count();
                self.status = Some(Ok(if missing == 0 {
                    "架构布局已保存。".into()
                } else {
                    format!("架构布局已保存，{missing} 个表仅保存了卡片位置。")
                }));
                if std::mem::take(&mut self.pending_close) {
                    self.closed = true;
                }
            }
            Err(error) => self.status = Some(Err(format!("保存布局失败。{error}"))),
        }
    }

    pub(super) fn can_close(&self) -> bool {
        self.mutation.is_none()
            && self.snapshot_pending.is_none()
            && self
                .reconciliation
                .as_ref()
                .is_none_or(|flow| !flow.waiting())
    }

    pub(super) fn request_close(&mut self, confirm_tab_close: bool) {
        if self.can_close() {
            self.pending_close = self.dirty && (confirm_tab_close || self.path.is_some());
            self.closed = !self.pending_close;
        }
    }

    fn card_columns<'a>(card: &Card, tables: &Tables<'a>) -> &'a [ColumnInfo] {
        tables
            .get(card.name.as_str())
            .map_or(&[], |table| &table.columns)
    }

    fn stage_saved(&mut self, card: usize) -> Option<Action> {
        let definition = self.layout.tables[card].definition.as_ref()?;
        let Some(definition) = layout_definition(definition) else {
            self.status = Some(Err("此卡片没有可用于生成草稿的建表语句。".into()));
            return None;
        };
        let mut script = String::new();
        if !self.layout.schema.is_empty() {
            script.push_str(&format!(
                "USE {};\n\n",
                quote_identifier(&self.layout.schema)
            ));
        }
        script.push_str(definition);
        script.push_str(";\n");
        Some(Action::StageSql(script))
    }

    fn new_key(&mut self, source: usize, target: usize, from: &str, to: &str) {
        self.draft_key = Some(ForeignKeyDraft {
            table: self.layout.tables[source].name.clone(),
            referenced_table: self.layout.tables[target].name.clone(),
            name: String::new(),
            columns: vec![(from.to_owned(), to.to_owned())],
            on_delete: Rule::Restrict,
            on_update: Rule::Restrict,
            previous: None,
            temporary_name: String::new(),
        });
    }

    fn edit_key(&mut self, key: &SchemaForeignKey) {
        let Some(on_delete) = Rule::parse(&key.on_delete) else {
            self.status = Some(Err("此外键的删除动作需要在 SQL 中修改。".into()));
            return;
        };
        let Some(on_update) = Rule::parse(&key.on_update) else {
            self.status = Some(Err("此外键的更新动作需要在 SQL 中修改。".into()));
            return;
        };
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        self.draft_key = Some(ForeignKeyDraft {
            table: key.table.name.clone(),
            referenced_table: key.referenced_table.name.clone(),
            name: key.name.clone(),
            columns: key.columns.clone(),
            on_delete,
            on_update,
            previous: Some(key.clone()),
            temporary_name: format!("harbor_fk_{unique:x}_{}", std::process::id()),
        });
    }
}

fn snapshot_definitions(snapshot: &SchemaSnapshot) -> BTreeMap<String, String> {
    snapshot
        .objects
        .iter()
        .filter(|item| item.object.kind == ObjectKind::Table)
        .filter_map(|item| {
            item.definition
                .as_ref()
                .ok()
                .filter(|sql| layout_definition(sql).is_some())
                .map(|sql| (item.object.name.clone(), sql.clone()))
        })
        .collect()
}

fn layout_definition(sql: &str) -> Option<&str> {
    let statements = split_script(sql);
    let [statement] = statements.as_slice() else {
        return None;
    };
    let mut words = statement.sql.split_ascii_whitespace();
    if !words.next()?.eq_ignore_ascii_case("CREATE") || !words.next()?.eq_ignore_ascii_case("TABLE")
    {
        return None;
    }
    // Table connection options can carry credentials in SHOW CREATE TABLE output.
    let lower = statement.sql.to_ascii_lowercase();
    let has_credential_option = ["connection", "password"].iter().any(|keyword| {
        lower.match_indices(keyword).any(|(start, _)| {
            let after = &lower[start + keyword.len()..];
            let before = lower.as_bytes().get(start.wrapping_sub(1));
            before.is_none_or(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'_' | b'`'))
                && after.trim_start().starts_with(['=', '\'', '"'])
        })
    });
    (!has_credential_option).then_some(statement.sql.trim())
}

#[derive(Deserialize, Serialize)]
#[serde(rename = "schemadesigner")]
struct LegacyLayout {
    #[serde(rename = "@version")]
    version: String,
    source: LegacySource,
    canvas: LegacyCanvas,
}

#[derive(Deserialize, Serialize)]
struct LegacySource {
    database: LegacyDatabase,
}

#[derive(Deserialize, Serialize)]
struct LegacyDatabase {
    #[serde(rename = "$text")]
    name: String,
}

#[derive(Deserialize, Serialize)]
struct LegacyCanvas {
    #[serde(rename = "@zoom")]
    zoom: u16,
    tables: LegacyTables,
}

#[derive(Deserialize, Serialize)]
struct LegacyTables {
    #[serde(rename = "table", default)]
    items: Vec<LegacyTable>,
}

#[derive(Deserialize, Serialize)]
struct LegacyTable {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "@view", default)]
    view: String,
    left: i32,
    top: i32,
    width: i32,
    height: i32,
    #[serde(default)]
    sql_create_table: String,
}

fn layout_bytes(file: File) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    file.take(FILE_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > FILE_LIMIT {
        return Err("布局文件超出 16 MiB 上限。".into());
    }
    Ok(bytes)
}

fn layout_revision(path: &Path) -> Result<Option<FileRevision>, String> {
    match File::open(path) {
        Ok(file) => layout_bytes(file).map(|bytes| Some(Sha256::digest(bytes).into())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn read_layout(path: &Path) -> Result<(Layout, FileRevision), String> {
    let bytes = layout_bytes(File::open(path).map_err(|error| error.to_string())?)?;
    let source = std::str::from_utf8(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes))
        .map_err(|_| "布局文件需要采用 UTF-8 编码。")?;
    let layout = if source.trim_start().starts_with('<') {
        read_legacy(source)?
    } else {
        serde_json::from_str(source).map_err(|error| error.to_string())?
    };
    layout.validate()?;
    Ok((layout, Sha256::digest(bytes).into()))
}

fn read_legacy(source: &str) -> Result<Layout, String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(source);
    loop {
        match reader.read_event().map_err(|error| error.to_string())? {
            Event::Start(tag) if tag.name().as_ref() == b"schemadesigner" => break,
            Event::Decl(_) | Event::Comment(_) => {}
            Event::Text(text) if text.iter().all(u8::is_ascii_whitespace) => {}
            _ => return Err("文件缺少 SQLyog 架构布局根元素。".into()),
        }
    }
    if source.contains("<!DOCTYPE") {
        return Err("布局 XML 含有文档类型声明。".into());
    }
    let file: LegacyLayout = quick_xml::de::from_str(source).map_err(|error| error.to_string())?;
    if file.version != "6.5" {
        return Err(format!("架构 XML 版本 {} 不受支持。", file.version));
    }
    let mut layout = Layout::new(file.source.database.name);
    layout.zoom = f32::from(file.canvas.zoom) / 100.0;
    if !(MIN_ZOOM..=MAX_ZOOM).contains(&layout.zoom) {
        return Err("架构 XML 的缩放比例无效。".into());
    }
    layout.tables = file
        .canvas
        .tables
        .items
        .into_iter()
        .map(|table| Card {
            name: table.name,
            position: [
                table.left as f32 / layout.zoom,
                table.top as f32 / layout.zoom,
            ],
            size: [
                (table.width as f32 / layout.zoom).clamp(MIN_CARD.x, MAX_CARD.x),
                (table.height as f32 / layout.zoom).clamp(MIN_CARD.y, MAX_CARD.y),
            ],
            names_only: table.view == "colnames",
            definition: (!table.sql_create_table.trim().is_empty())
                .then_some(table.sql_create_table),
        })
        .collect();
    Ok(layout)
}

fn encode_layout(layout: &Layout, path: &Path) -> Result<Vec<u8>, String> {
    layout.validate()?;
    let bytes = if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("schemaxml"))
    {
        if layout.schema.is_empty() {
            return Err("请先为 XML 布局选择数据库。".into());
        }
        let ratio = (layout.zoom * 100.0).round() as u16;
        let pixel = |value: f32| (value * f32::from(ratio) / 100.0).round() as i32;
        let file = LegacyLayout {
            version: "6.5".into(),
            source: LegacySource {
                database: LegacyDatabase {
                    name: layout.schema.clone(),
                },
            },
            canvas: LegacyCanvas {
                zoom: ratio,
                tables: LegacyTables {
                    items: layout
                        .tables
                        .iter()
                        .map(|card| LegacyTable {
                            name: card.name.clone(),
                            view: if card.names_only {
                                "colnames"
                            } else {
                                "standard"
                            }
                            .into(),
                            left: pixel(card.position[0]),
                            top: pixel(card.position[1]),
                            width: pixel(card.size[0]),
                            height: pixel(card.size[1]),
                            sql_create_table: card.definition.clone().unwrap_or_default(),
                        })
                        .collect(),
                },
            },
        };
        let xml = quick_xml::se::to_string(&file).map_err(|error| error.to_string())?;
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n{xml}\n").into_bytes()
    } else {
        serde_json::to_vec_pretty(layout).map_err(|error| error.to_string())?
    };
    if bytes.len() > FILE_LIMIT {
        return Err("布局文件超出 16 MiB 上限。".into());
    }
    Ok(bytes)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Rule {
    Restrict,
    Cascade,
    SetNull,
    NoAction,
}

impl Rule {
    fn parse(sql: &str) -> Option<Self> {
        match sql {
            "RESTRICT" => Some(Self::Restrict),
            "CASCADE" => Some(Self::Cascade),
            "SET NULL" => Some(Self::SetNull),
            "NO ACTION" => Some(Self::NoAction),
            _ => None,
        }
    }

    fn sql(self) -> &'static str {
        match self {
            Self::Restrict => "RESTRICT",
            Self::Cascade => "CASCADE",
            Self::SetNull => "SET NULL",
            Self::NoAction => "NO ACTION",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Restrict => "限制（RESTRICT）",
            Self::Cascade => "级联（CASCADE）",
            Self::SetNull => "设为 NULL",
            Self::NoAction => "无操作（NO ACTION）",
        }
    }
}

struct ForeignKeyDraft {
    table: String,
    referenced_table: String,
    name: String,
    columns: Vec<(String, String)>,
    on_delete: Rule,
    on_update: Rule,
    previous: Option<SchemaForeignKey>,
    temporary_name: String,
}

enum KeyDecision {
    Stage(String),
    Apply(KeyChange),
}

fn qualified(schema: &str, table: &str) -> String {
    format!("{}.{}", quote_identifier(schema), quote_identifier(table))
}

impl ForeignKeyDraft {
    fn statement(&self, schema: &str, tables: &Tables<'_>) -> Result<String, &'static str> {
        let child = tables.get(self.table.as_str()).ok_or("请选择子表。")?;
        let parent = tables
            .get(self.referenced_table.as_str())
            .ok_or("请选择被引用表。")?;
        let name = self.name.trim();
        if (!name.is_empty() || self.previous.is_some()) && !identifier_ok(name) {
            return Err("外键名称无效。");
        }
        let mut from = Vec::new();
        let mut to = Vec::new();
        let mut local_names = HashSet::new();
        let mut referenced_names = HashSet::new();
        for (local, reference) in &self.columns {
            let local_column = child
                .columns
                .iter()
                .find(|column| column.name == *local)
                .ok_or("请选择完整的子表字段。")?;
            if !parent
                .columns
                .iter()
                .any(|column| column.name == *reference)
            {
                return Err("请选择完整的被引用字段。");
            }
            if !local_names.insert(local.as_str()) || !referenced_names.insert(reference.as_str()) {
                return Err("同一侧的字段不能重复。");
            }
            if !local_column.nullable
                && (self.on_delete == Rule::SetNull || self.on_update == Rule::SetNull)
            {
                return Err("SET NULL 要求所有子表字段允许 NULL。");
            }
            from.push(quote_identifier(local));
            to.push(quote_identifier(reference));
        }
        if from.is_empty() {
            return Err("请添加至少一组字段。");
        }
        let table = qualified(schema, &self.table);
        let add = |name: &str| {
            let constraint = if name.is_empty() {
                String::new()
            } else {
                format!("CONSTRAINT {} ", quote_identifier(name))
            };
            format!(
                "ADD {constraint}FOREIGN KEY ({})\n  REFERENCES {} ({})\n  ON DELETE {} ON UPDATE {}",
                from.join(", "),
                qualified(schema, &self.referenced_table),
                to.join(", "),
                self.on_delete.sql(),
                self.on_update.sql(),
            )
        };
        let Some(previous) = &self.previous else {
            return Ok(format!("ALTER TABLE {table}\n  {};\n", add(name)));
        };
        if previous.name == name
            && previous.referenced_table.name == self.referenced_table
            && previous.columns == self.columns
            && previous.on_delete == self.on_delete.sql()
            && previous.on_update == self.on_update.sql()
        {
            return Err("请修改名称、字段映射或引用动作。");
        }
        let drop = format!("DROP FOREIGN KEY {}", quote_identifier(&previous.name));
        let replacement = add(name);
        if !previous.name.eq_ignore_ascii_case(name) {
            return Ok(format!(
                "ALTER TABLE {table}\n  {drop},\n  {replacement};\n"
            ));
        }
        // Validate the new constraint under another name before dropping the original.
        let temporary = add(&self.temporary_name);
        let temporary_name = quote_identifier(&self.temporary_name);
        Ok(format!(
            "ALTER TABLE {table}\n  {temporary};\nALTER TABLE {table} {drop};\n\
             ALTER TABLE {table}\n  {replacement};\nALTER TABLE {table} DROP FOREIGN KEY {temporary_name};\n"
        ))
    }

    fn change(&self, schema: &str, sql: String) -> KeyChange {
        let table = |name: &str| DbObject {
            schema: schema.into(),
            name: name.into(),
            kind: ObjectKind::Table,
        };
        KeyChange {
            sql,
            previous: self.previous.clone(),
            effect: Effect::SaveKey(SchemaForeignKey {
                table: table(&self.table),
                name: self.name.trim().into(),
                referenced_table: table(&self.referenced_table),
                columns: self.columns.clone(),
                on_delete: self.on_delete.sql().into(),
                on_update: self.on_update.sql().into(),
            }),
        }
    }
}

fn column_dropdown(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash,
    column: &mut String,
    fields: &[ColumnInfo],
) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(if column.is_empty() {
            "选择字段"
        } else {
            column.as_str()
        })
        .width(160.0)
        .truncate()
        .show_ui(ui, |ui| {
            for field in fields {
                ui.selectable_value(column, field.name.clone(), &field.name);
            }
        });
}

fn rule_dropdown(ui: &mut egui::Ui, label: &str, rule: &mut Rule) {
    ui.label(label);
    egui::ComboBox::from_id_salt(label)
        .selected_text(rule.label())
        .show_ui(ui, |ui| {
            for candidate in [Rule::Restrict, Rule::Cascade, Rule::SetNull, Rule::NoAction] {
                ui.selectable_value(rule, candidate, candidate.label());
            }
        });
}

fn sql_preview(ui: &mut egui::Ui, sql: &str) {
    ui.collapsing("预览 SQL", |ui| {
        egui::Frame::new()
            .fill(style::PANEL)
            .corner_radius(6)
            .inner_margin(egui::Margin::same(4))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(120.0)
                    .show(ui, |ui| appearance::sql_preview(ui, sql, 2));
            });
    });
}

impl Designer {
    fn key_dialog(
        &mut self,
        ctx: &egui::Context,
        tables: &Tables<'_>,
        allow_apply: bool,
    ) -> Option<KeyDecision> {
        let mut action = None;
        let mut close = false;
        let draft = self.draft_key.as_mut()?;
        let editing = draft.previous.is_some();
        let modal = style::modal(ctx, egui::Id::new("schema-design-key")).show(ctx, |ui| {
            ui.set_width(450.0);
            ui.heading(if editing {
                "编辑外键"
            } else {
                "新建外键"
            });
            egui::Grid::new("fk-source-and-target").show(ui, |ui| {
                for (name, label, side) in [
                    (&mut draft.table, "子表", 0),
                    (&mut draft.referenced_table, "被引用表", 1),
                ] {
                    ui.label(label);
                    let previous = name.clone();
                    if editing && side == 0 {
                        ui.label(name.as_str());
                    } else {
                        egui::ComboBox::from_id_salt(label)
                            .width(260.0)
                            .truncate()
                            .selected_text(name.as_str())
                            .show_ui(ui, |ui| {
                                for table in tables.values() {
                                    let candidate = &table.object.name;
                                    ui.selectable_value(name, candidate.clone(), candidate);
                                }
                            });
                    }
                    if previous != *name {
                        for (local, reference) in &mut draft.columns {
                            if side == 0 {
                                local.clear();
                            } else {
                                reference.clear();
                            }
                        }
                    }
                    ui.end_row();
                }
                ui.label("外键名称");
                ui.add(
                    egui::TextEdit::singleline(&mut draft.name).hint_text(if editing {
                        "输入外键名称"
                    } else {
                        "留空由数据库命名"
                    }),
                );
                ui.end_row();
            });
            let source = tables
                .get(draft.table.as_str())
                .map_or(&[][..], |table| &table.columns);
            let target = tables
                .get(draft.referenced_table.as_str())
                .map_or(&[][..], |table| &table.columns);
            let mut remove = None;
            egui::ScrollArea::vertical()
                .max_height(170.0)
                .show(ui, |ui| {
                    egui::Grid::new("fk-field-pairs").show(ui, |ui| {
                        ui.label("子表字段");
                        ui.label("被引用字段");
                        ui.label("");
                        ui.end_row();
                        let multiple = draft.columns.len() > 1;
                        for (index, (left, right)) in draft.columns.iter_mut().enumerate() {
                            column_dropdown(ui, (index, "child"), left, source);
                            column_dropdown(ui, (index, "parent"), right, target);
                            if ui
                                .add_enabled(multiple, egui::Button::new("移除"))
                                .clicked()
                            {
                                remove = Some(index);
                            }
                            ui.end_row();
                        }
                    });
                });
            if let Some(index) = remove {
                draft.columns.remove(index);
            }
            if ui
                .add_enabled(
                    draft.columns.len() < source.len().min(target.len()),
                    egui::Button::new("添加字段对"),
                )
                .clicked()
            {
                draft.columns.push((String::new(), String::new()));
            }
            ui.horizontal(|ui| {
                rule_dropdown(ui, "删除时", &mut draft.on_delete);
                rule_dropdown(ui, "更新时", &mut draft.on_update);
            });
            let sql = draft.statement(&self.layout.schema, tables);
            match &sql {
                Ok(sql) => {
                    sql_preview(ui, sql);
                    if draft
                        .previous
                        .as_ref()
                        .is_some_and(|key| key.name.eq_ignore_ascii_case(draft.name.trim()))
                    {
                        ui.label("同名修改分步执行，若有语句失败，已执行的修改可能保留。");
                    }
                }
                Err(error) => {
                    ui.label(RichText::new(*error).color(style::MUTED));
                }
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(allow_apply && sql.is_ok(), style::primary("保存外键"))
                    .clicked()
                    && let Ok(sql) = &sql
                {
                    action = Some(KeyDecision::Apply(
                        draft.change(&self.layout.schema, sql.clone()),
                    ));
                }
                if ui
                    .add_enabled(sql.is_ok(), egui::Button::new("打开 SQL 草稿"))
                    .clicked()
                    && let Ok(sql) = sql
                {
                    action = Some(KeyDecision::Stage(sql));
                }
                close = ui.button("取消").clicked();
            });
        });
        if close || modal.should_close() || action.is_some() {
            self.draft_key = None;
        }
        action
    }

    fn delete_key_dialog(&mut self, ctx: &egui::Context, allow_apply: bool) -> Option<KeyDecision> {
        let key = self.delete_key.as_ref()?;
        let mut action = None;
        let mut close = false;
        let modal = style::modal(ctx, egui::Id::new("schema-design-delete-key")).show(ctx, |ui| {
            ui.set_max_width(480.0);
            ui.heading("删除外键");
            ui.label(format!(
                "{}  ·  {} → {}",
                key.name, key.table.name, key.referenced_table.name
            ));
            sql_preview(ui, &drop_key(key));
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(allow_apply, style::primary("确认删除外键"))
                    .clicked()
                {
                    action = Some(KeyDecision::Apply(KeyChange::delete(key)));
                }
                if ui.button("打开 SQL 草稿").clicked() {
                    action = Some(KeyDecision::Stage(drop_key(key)));
                }
                close = ui.button("取消").clicked();
            });
        });
        if close || modal.should_close() || action.is_some() {
            self.delete_key = None;
        }
        action
    }
}

struct Graph<'a> {
    cards: Vec<Rect>,
    links: Vec<Cable<'a>>,
}

struct Cable<'a> {
    key: &'a SchemaForeignKey,
    points: [Pos2; 4],
    marks: [Option<&'static str>; 2],
}

impl Cable<'_> {
    fn labels(&self, scale: f32) -> impl Iterator<Item = (Pos2, &'static str)> {
        [
            (self.points[0], self.points[1], self.marks[0]),
            (self.points[3], self.points[2], self.marks[1]),
        ]
        .into_iter()
        .filter_map(move |(end, elbow, mark)| {
            let side = if elbow.x >= end.x { 1.0 } else { -1.0 };
            let offset = (15.0 * scale).clamp(7.0, 23.0);
            Some((end + vec2(side * offset, -(8.0 * scale).max(5.0)), mark?))
        })
    }
}

fn cardinality<'a>(
    table: Option<&SchemaDesignTable>,
    columns: impl Iterator<Item = &'a str> + Clone,
) -> Option<&'static str> {
    let indexes = table?.unique_indexes.as_ref()?;
    let at_most_one = indexes.iter().any(|index| {
        index.columns.iter().all(|key_column| {
            columns
                .clone()
                .any(|column| column.eq_ignore_ascii_case(key_column))
        })
    });
    Some(if at_most_one { "1" } else { "∞" })
}

struct CatalogTable {
    schema: String,
    name: String,
}

pub(super) fn drag_table(response: &egui::Response, table: &DbObject) {
    response.dnd_set_drag_payload(CatalogTable {
        schema: table.schema.clone(),
        name: table.name.clone(),
    });
}

impl Graph<'_> {
    fn card_at(&self, pointer: Pos2) -> Option<usize> {
        self.cards.iter().rposition(|rect| rect.contains(pointer))
    }

    fn link_at(&self, pointer: Pos2) -> Option<&SchemaForeignKey> {
        self.links
            .iter()
            .filter_map(|link| {
                let distance = link
                    .points
                    .windows(2)
                    .map(|points| segment_distance(pointer, points[0], points[1]))
                    .fold(f32::INFINITY, f32::min);
                (distance <= 7.0).then_some((distance, link.key))
            })
            .min_by(|first, second| first.0.total_cmp(&second.0))
            .map(|(_, key)| key)
    }
}

fn segment_distance(pointer: Pos2, start: Pos2, end: Pos2) -> f32 {
    let direction = end - start;
    let length = direction.length_sq();
    if length == 0.0 {
        return pointer.distance(start);
    }
    let t = ((pointer - start).dot(direction) / length).clamp(0.0, 1.0);
    pointer.distance(start + direction * t)
}

fn cable_points(
    from: Rect,
    to: Rect,
    y_from: f32,
    y_to: f32,
    scale: f32,
    self_link: bool,
) -> [Pos2; 4] {
    let (x_from, x_to, elbow) = if self_link || from.intersects(to) {
        (
            from.right(),
            to.right(),
            from.right().max(to.right()) + 23.0 * scale,
        )
    } else if from.center().x <= to.center().x {
        (from.right(), to.left(), (from.right() + to.left()) / 2.0)
    } else {
        (from.left(), to.right(), (from.left() + to.right()) / 2.0)
    };
    let y_to = if self_link && (y_from - y_to).abs() < 5.0 * scale {
        (y_from + 8.0 * scale).min(to.bottom())
    } else {
        y_to
    };
    [
        pos2(x_from, y_from),
        pos2(elbow, y_from),
        pos2(elbow, y_to),
        pos2(x_to, y_to),
    ]
}

fn select_rule_label(value: &str) -> &str {
    match Rule::parse(value) {
        Some(rule) => rule.label(),
        None if value.is_empty() => "默认",
        None => value,
    }
}

impl Designer {
    fn geometry<'a>(
        &self,
        origin: Pos2,
        scale: f32,
        tables: &Tables<'_>,
        keys: &'a [SchemaForeignKey],
    ) -> Graph<'a> {
        let cards = self
            .layout
            .tables
            .iter()
            .map(|card| {
                let rect = card.world_rect();
                Rect::from_min_size(origin + rect.min.to_vec2() * scale, rect.size() * scale)
            })
            .collect::<Vec<_>>();
        let indices: BTreeMap<_, _> = self
            .layout
            .tables
            .iter()
            .enumerate()
            .map(|(index, card)| (card.name.as_str(), index))
            .collect();
        let links = keys
            .iter()
            .filter_map(|key| {
                if key.table.schema != self.layout.schema
                    || key.referenced_table.schema != self.layout.schema
                {
                    return None;
                }
                let source = *indices.get(key.table.name.as_str())?;
                let target = *indices.get(key.referenced_table.name.as_str())?;
                let (left, right) = key.columns.first()?;
                let y_from = self.field_y(source, left, cards[source], scale, tables);
                let y_to = self.field_y(target, right, cards[target], scale, tables);
                Some(Cable {
                    key,
                    marks: [
                        cardinality(
                            tables.get(key.table.name.as_str()).copied(),
                            key.columns.iter().map(|(column, _)| column.as_str()),
                        ),
                        cardinality(
                            tables.get(key.referenced_table.name.as_str()).copied(),
                            key.columns.iter().map(|(_, column)| column.as_str()),
                        ),
                    ],
                    points: cable_points(
                        cards[source],
                        cards[target],
                        y_from,
                        y_to,
                        scale,
                        source == target,
                    ),
                })
            })
            .collect();
        Graph { cards, links }
    }

    fn field_y(
        &self,
        card: usize,
        field: &str,
        bounds: Rect,
        scale: f32,
        tables: &Tables<'_>,
    ) -> f32 {
        let card = &self.layout.tables[card];
        let columns = Self::card_columns(card, tables);
        let y = columns
            .iter()
            .position(|column| column.name == field)
            .map_or(TITLE / 2.0, |index| {
                let offset = self.row_offsets.get(&card.name).copied().unwrap_or(0);
                TITLE + (index as f32 - offset as f32 + 0.5) * ROW
            });
        bounds.top() + (y * scale).clamp(TITLE * 0.35 * scale, bounds.height() - 3.0 * scale)
    }

    fn column_at<'a>(
        &self,
        index: usize,
        bounds: Rect,
        pointer: Pos2,
        tables: &Tables<'a>,
    ) -> Option<&'a str> {
        let y = (pointer.y - bounds.top()) / self.layout.zoom - TITLE;
        if y < 0.0 || pointer.y >= bounds.bottom() - 3.0 * self.layout.zoom {
            return None;
        }
        let card = &self.layout.tables[index];
        let row =
            (y / ROW).floor() as usize + self.row_offsets.get(&card.name).copied().unwrap_or(0);
        Self::card_columns(card, tables)
            .get(row)
            .map(|column| column.name.as_str())
    }

    fn hit_handle(&self, bounds: Rect, pointer: Pos2) -> bool {
        Rect::from_min_size(
            bounds.max - Vec2::splat((10.0 * self.layout.zoom).max(6.0)),
            Vec2::splat((10.0 * self.layout.zoom).max(6.0)),
        )
        .contains(pointer)
    }

    fn zoom_at(&mut self, factor: f32, anchor: Vec2) {
        let zoom = factor.clamp(MIN_ZOOM, MAX_ZOOM);
        if self.layout.zoom == zoom {
            return;
        }
        let pan = vec2(self.layout.pan[0], self.layout.pan[1]);
        let shifted = anchor - (anchor - pan) * (zoom / self.layout.zoom);
        self.layout.pan = clamp_vec(shifted).into();
        self.layout.zoom = zoom;
        self.dirty |= !self.layout.tables.is_empty();
    }

    fn fit_graph(&mut self, viewport: Rect) {
        let (pan, zoom) = if let Some(bounds) = self.layout.bounds() {
            let usable = (viewport.size() - Vec2::splat(32.0)).max(Vec2::splat(16.0));
            let zoom = (usable.x / bounds.width())
                .min(usable.y / bounds.height())
                .clamp(MIN_ZOOM, 1.0);
            (vec2(16.0, 16.0) - bounds.min.to_vec2() * zoom, zoom)
        } else {
            (Vec2::splat(16.0), 1.0)
        };
        self.dirty |= !self.layout.tables.is_empty()
            && (self.layout.pan != <[f32; 2]>::from(pan) || self.layout.zoom != zoom);
        self.layout.pan = clamp_vec(pan).into();
        self.layout.zoom = zoom;
    }

    fn select_at(&mut self, pointer: Pos2, graph: &Graph<'_>) {
        if self.status.as_ref().is_some_and(|status| status.is_ok()) {
            self.status = None;
        }
        self.selected_card = graph
            .card_at(pointer)
            .map(|index| self.layout.tables[index].name.clone());
        self.selected_key = if self.selected_card.is_none() {
            graph
                .link_at(pointer)
                .map(|key| (key.table.name.clone(), key.name.clone()))
        } else {
            None
        };
    }

    fn process_canvas_input(
        &mut self,
        response: &egui::Response,
        ui: &egui::Ui,
        graph: &Graph<'_>,
        tables: &Tables<'_>,
    ) {
        let viewport = response.rect;
        let scale = self.layout.zoom;
        if let Some(hover) = response.hover_pos() {
            let (zoom_delta, scroll) = appearance::canvas_scroll(ui);
            if zoom_delta != 1.0 {
                self.zoom_at(scale * zoom_delta, hover - viewport.min);
            } else if scroll != Vec2::ZERO {
                let mut handled = false;
                if let Some(index) = graph.card_at(hover) {
                    let card = &self.layout.tables[index];
                    let rows = card_rows(graph.cards[index].height(), scale);
                    let total = Self::card_columns(card, tables).len();
                    if rows < total && scroll.y != 0.0 {
                        let lines = (scroll.y.abs() / 36.0).round().max(1.0) as usize;
                        let offset = self.row_offsets.entry(card.name.clone()).or_default();
                        *offset = if scroll.y > 0.0 {
                            offset.saturating_sub(lines)
                        } else {
                            (*offset + lines).min(total - rows)
                        };
                        handled = true;
                    }
                }
                if !handled {
                    self.layout.pan =
                        clamp_vec(vec2(self.layout.pan[0], self.layout.pan[1]) + scroll).into();
                    self.dirty |= !self.layout.tables.is_empty();
                }
            }
        }

        if response.drag_started_by(egui::PointerButton::Middle) {
            self.drag = Some(Drag::Pan);
        } else if response.drag_started_by(egui::PointerButton::Primary)
            && let Some(pointer) = ui.input(|input| input.pointer.press_origin())
        {
            self.select_at(pointer, graph);
            self.drag = Some(match graph.card_at(pointer) {
                Some(index) if self.hit_handle(graph.cards[index], pointer) => Drag::Resize(index),
                Some(index) if pointer.y < graph.cards[index].top() + TITLE * scale => {
                    Drag::Move(index)
                }
                Some(index) => self
                    .column_at(index, graph.cards[index], pointer, tables)
                    .map_or(Drag::Move(index), |field| Drag::Link {
                        table: index,
                        field: field.to_owned(),
                    }),
                None => Drag::Pan,
            });
        }
        if response.dragged() {
            let delta = response.drag_delta();
            match self.drag {
                Some(Drag::Pan) => {
                    self.layout.pan =
                        clamp_vec(vec2(self.layout.pan[0], self.layout.pan[1]) + delta).into();
                    self.dirty |= delta != Vec2::ZERO && !self.layout.tables.is_empty();
                }
                Some(Drag::Move(index)) => {
                    let pos = self.layout.tables[index].world_rect().min.to_vec2() + delta / scale;
                    self.layout.tables[index].position = clamp_vec(pos).into();
                    self.dirty |= delta != Vec2::ZERO;
                }
                Some(Drag::Resize(index)) => {
                    let card = &mut self.layout.tables[index];
                    card.size = (card.world_rect().size() + delta / scale)
                        .max(MIN_CARD)
                        .min(MAX_CARD)
                        .into();
                    if let Some(offset) = self.row_offsets.get_mut(&card.name) {
                        let shown = card_rows(card.size[1] * scale, scale);
                        let total = Self::card_columns(card, tables).len();
                        *offset = (*offset).min(total.saturating_sub(shown));
                    }
                    self.dirty |= delta != Vec2::ZERO;
                }
                _ => {}
            }
        }
        if response.drag_stopped()
            && let Some(Drag::Link { table, field }) = self.drag.take()
            && let Some(pointer) = response
                .interact_pointer_pos()
                .filter(|pointer| viewport.contains(*pointer))
            && let Some(target) = graph.card_at(pointer)
        {
            let column = self
                .column_at(target, graph.cards[target], pointer, tables)
                .or_else(|| {
                    preferred_key(
                        tables
                            .get(self.layout.tables[target].name.as_str())
                            .copied(),
                    )
                })
                .unwrap_or_default()
                .to_owned();
            if table != target || field != column {
                self.new_key(table, target, &field, &column);
            }
        }
        if (response.clicked() || response.secondary_clicked())
            && let Some(pointer) = response.interact_pointer_pos()
        {
            self.select_at(pointer, graph);
        }
    }

    fn draw_canvas(
        &mut self,
        ui: &mut egui::Ui,
        size: Vec2,
        tables: &Tables<'_>,
        keys: &[SchemaForeignKey],
        connected: bool,
        can_edit: bool,
        action: &mut Option<Action>,
    ) -> Option<DiagramExport> {
        let mut export = None;
        let (response, painter) = ui.allocate_painter(size, egui::Sense::click_and_drag());
        let viewport = response.rect;
        if std::mem::take(&mut self.fit) {
            self.fit_graph(viewport);
        }
        let get_origin = |pan: [f32; 2]| viewport.min + vec2(pan[0], pan[1]);
        if let Some(dropped) = response.dnd_release_payload::<CatalogTable>()
            && dropped.schema == self.layout.schema
            && let Some(table) = tables.get(dropped.name.as_str())
            && let Some(pointer) = ui.input(|input| input.pointer.interact_pos())
        {
            let count = self.layout.tables.len();
            self.add_card(&table.object.name, table.columns.len());
            if self.layout.tables.len() > count {
                let world = (pointer - get_origin(self.layout.pan)) / self.layout.zoom;
                self.layout.tables[count].position = clamp_vec(world).into();
            }
            self.selected_card = Some(table.object.name.clone());
            self.selected_key = None;
        }
        let before = self.geometry(get_origin(self.layout.pan), self.layout.zoom, tables, keys);
        self.process_canvas_input(&response, ui, &before, tables);
        if can_edit
            && response.double_clicked()
            && let Some(key) = self.selected_relation(keys)
        {
            self.edit_key(key);
        }
        let origin = get_origin(self.layout.pan);
        let scale = self.layout.zoom;
        let graph = self.geometry(origin, scale, tables, keys);
        let painter = painter.with_clip_rect(viewport);
        appearance::canvas(&painter, viewport, origin, scale);
        if self.layout.page_breaks {
            draw_page_lines(&painter, viewport, origin, scale);
        }
        let hover = response.hover_pos();
        if response
            .dnd_hover_payload::<CatalogTable>()
            .is_some_and(|table| table.schema == self.layout.schema)
        {
            painter.rect_stroke(
                viewport.shrink(1.0),
                8,
                Stroke::new(2.0_f32, style::ACCENT),
                StrokeKind::Inside,
            );
        }
        if graph.cards.is_empty() {
            painter.text(
                viewport.center() - vec2(0.0, 9.0),
                Align2::CENTER_CENTER,
                "添加表到画布",
                FontId::proportional(17.0),
                style::INK,
            );
            painter.text(
                viewport.center() + vec2(0.0, 14.0),
                Align2::CENTER_CENTER,
                "点选或拖入左侧条目",
                FontId::proportional(12.0),
                style::MUTED,
            );
        }
        let hover_card = hover.and_then(|point| graph.card_at(point));
        let hover_link = hover
            .filter(|_| hover_card.is_none())
            .and_then(|pointer| graph.link_at(pointer));
        for cable in &graph.links {
            let selected = self.selected_key.as_ref().is_some_and(|(table, name)| {
                table == &cable.key.table.name && name == &cable.key.name
            });
            let hovered = hover_link.is_some_and(|key| std::ptr::eq(key, cable.key));
            let adjacent = self.selected_card.as_ref().is_some_and(|name| {
                name == &cable.key.table.name || name == &cable.key.referenced_table.name
            });
            let stroke = if selected || hovered || adjacent {
                Stroke::new(
                    if selected || hovered {
                        2.6_f32
                    } else {
                        2.0_f32
                    },
                    style::ACCENT,
                )
            } else {
                Stroke::new(1.6_f32, style::MUTED)
            };
            appearance::link(&painter, cable.points, stroke);
            let tip = cable.points[3];
            let direction = if cable.points[2].x < tip.x { -1.0 } else { 1.0 };
            for dy in [-4.0, 4.0] {
                painter.line_segment([tip, tip + vec2(direction * 7.0, dy)], stroke);
            }
            for (position, label) in cable.labels(scale) {
                let font_size = (13.0 * scale).clamp(8.0, 16.0);
                painter.rect_filled(
                    Rect::from_center_size(position, Vec2::splat(font_size + 3.0)),
                    3,
                    style::SURFACE,
                );
                painter.text(
                    position,
                    Align2::CENTER_CENTER,
                    label,
                    FontId::proportional(font_size),
                    stroke.color,
                );
            }
        }
        for (index, bounds) in graph.cards.iter().copied().enumerate() {
            if bounds.intersects(viewport) {
                self.paint_card(
                    &painter,
                    index,
                    bounds,
                    tables,
                    connected,
                    hover.filter(|_| hover_card == Some(index)),
                );
            }
        }
        if let Some(Drag::Link { table, field }) = &self.drag
            && let Some(pointer) = ui.input(|input| input.pointer.interact_pos())
        {
            let bounds = graph.cards[*table];
            let y = self.field_y(*table, field, bounds, scale, tables);
            let x = if pointer.x < bounds.center().x {
                bounds.left()
            } else {
                bounds.right()
            };
            painter.line_segment([pos2(x, y), pointer], Stroke::new(2.0_f32, style::ACCENT));
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        } else if let Some(pointer) = hover
            && let Some(index) = hover_card
        {
            let bounds = graph.cards[index];
            if self.hit_handle(bounds, pointer) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeNwSe);
            } else if pointer.y < bounds.top() + TITLE * scale {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            }
        }
        if let Some(key) = hover_link {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            response.clone().on_hover_text(format!(
                "{}  {} → {}",
                key.name, key.table.name, key.referenced_table.name
            ));
        }
        response.context_menu(|ui| {
            if let Some(index) = self.selected_card.as_ref().and_then(|name| {
                self.layout
                    .tables
                    .iter()
                    .position(|card| &card.name == name)
            }) {
                self.card_menu(ui, index, tables, action);
            } else if let Some(key) = self.selected_relation(keys) {
                self.relation_menu(ui, key, can_edit, action);
            } else {
                ui.add_enabled_ui(can_edit, |ui| {
                    menu_action(ui, "新建表…", action, || {
                        Action::CreateTable(self.layout.schema.clone())
                    });
                });
                self.add_table_menu(ui, tables);
                if ui.button("适应画布").clicked() {
                    self.fit = true;
                    ui.close();
                }
                if ui
                    .add_enabled(connected, egui::Button::new("刷新结构"))
                    .clicked()
                {
                    self.request_refresh();
                    ui.close();
                }
                export = image_export_menu(ui, !self.layout.tables.is_empty());
            }
        });
        export
    }

    fn paint_card(
        &self,
        painter: &egui::Painter,
        index: usize,
        bounds: Rect,
        tables: &Tables<'_>,
        connected: bool,
        hover: Option<Pos2>,
    ) {
        let scale = self.layout.zoom;
        let card = &self.layout.tables[index];
        let exists = tables.contains_key(card.name.as_str());
        let selected = self.selected_card.as_deref() == Some(card.name.as_str());
        let title = appearance::card(painter, bounds, TITLE * scale, scale, selected);
        let inset = if selected { 2.0 } else { 1.0 };
        let painter = painter.with_clip_rect(bounds.shrink(inset));
        let differs = self.definition_differs(card);
        let title_right = if differs { 22.0 } else { 9.0 };
        appearance::table_icon(
            &painter,
            pos2(title.left() + 14.0 * scale, title.center().y),
            scale,
            false,
            if selected {
                style::ACCENT
            } else {
                style::MUTED
            },
        );
        appearance::text(
            &painter,
            Rect::from_min_max(
                title.min + vec2(26.0 * scale, 0.0),
                title.max - vec2(title_right * scale, 0.0),
            ),
            &card.name,
            FontId::proportional((14.0 * scale).max(1.0)),
            style::INK,
        );
        if differs {
            painter.circle_filled(
                pos2(title.right() - 10.0 * scale, title.center().y),
                3.2 * scale,
                style::WARNING,
            );
        }
        if exists {
            let columns = Self::card_columns(card, tables);
            let first = self.row_offsets.get(&card.name).copied().unwrap_or(0);
            let count = card_rows(bounds.height(), scale).min(columns.len().saturating_sub(first));
            let name_font = FontId::monospace((12.0 * scale).max(1.0));
            let type_font = FontId::monospace((11.0 * scale).max(1.0));
            let column_x = bounds.left() + 23.0 * scale;
            let type_x = bounds.left() + bounds.width() * 0.56;
            let right = bounds.right() - 9.0 * scale;
            let name_right = if card.names_only {
                right
            } else {
                type_x - 5.0 * scale
            };
            for (line, column) in columns.iter().skip(first).take(count).enumerate() {
                let y = title.bottom() + (line as f32 + 0.5) * ROW * scale;
                let row = Rect::from_center_size(
                    pos2(bounds.center().x, y),
                    vec2(bounds.width(), ROW * scale),
                );
                if hover.is_some_and(|point| row.contains(point) && !self.hit_handle(bounds, point))
                {
                    let target = matches!(&self.drag, Some(Drag::Link { table, field }) if *table != index || field != &column.name);
                    let tint = row.shrink2(vec2(3.0, 1.5) * scale);
                    if target {
                        painter.rect(
                            tint,
                            4,
                            style::ACCENT_SOFT,
                            Stroke::new(1.0_f32, style::ACCENT),
                            StrokeKind::Inside,
                        );
                    } else {
                        painter.rect_filled(tint, 4, style::PANEL);
                    }
                }
                if let Some(key) = appearance::KeyMarker::from_column(&column.key) {
                    key.paint(&painter, pos2(bounds.left() + 13.0 * scale, y), scale);
                }
                appearance::text(
                    &painter,
                    Rect::from_min_max(pos2(column_x, row.top()), pos2(name_right, row.bottom())),
                    &column.name,
                    name_font.clone(),
                    style::INK,
                );
                if !card.names_only {
                    appearance::text(
                        &painter,
                        Rect::from_min_max(pos2(type_x, row.top()), pos2(right, row.bottom())),
                        &column.data_type,
                        type_font.clone(),
                        style::MUTED,
                    );
                }
            }
            appearance::row_scroll(
                &painter,
                Rect::from_min_max(
                    pos2(bounds.left(), title.bottom() + 4.0 * scale),
                    bounds.max - vec2(0.0, 14.0 * scale),
                ),
                first..first + count,
                columns.len(),
                scale,
            );
        } else {
            appearance::text(
                &painter,
                Rect::from_min_size(
                    title.left_bottom() + vec2(9.0 * scale, 0.0),
                    vec2(bounds.width() - 18.0 * scale, ROW * scale),
                ),
                if connected {
                    "当前数据库缺少此表"
                } else {
                    "等待连接和表元数据"
                },
                FontId::proportional((12.0 * scale).max(1.0)),
                style::MUTED,
            );
        }
        appearance::resize_grip(&painter, bounds, scale);
    }

    fn selected_relation<'a>(&self, keys: &'a [SchemaForeignKey]) -> Option<&'a SchemaForeignKey> {
        let (table, name) = self.selected_key.as_ref()?;
        keys.iter()
            .find(|key| &key.table.name == table && &key.name == name)
    }

    fn definition_differs(&self, card: &Card) -> bool {
        card.definition
            .as_ref()
            .zip(self.live_definitions.get(&card.name))
            .is_some_and(|(saved, live)| !same_definition(saved, live))
    }

    fn request_refresh(&mut self) {
        self.refresh = true;
        self.catalog_failed = None;
        self.check_layout = self
            .layout
            .tables
            .iter()
            .any(|card| card.definition.is_some());
    }
}

fn preferred_key(table: Option<&SchemaDesignTable>) -> Option<&str> {
    let table = table?;
    if let Some(indexes) = &table.unique_indexes {
        return indexes
            .iter()
            .find(|index| index.name == "PRIMARY")
            .or_else(|| indexes.first())
            .and_then(|index| index.columns.first())
            .map(String::as_str)
            .or_else(|| table.columns.first().map(|field| field.name.as_str()));
    }
    table
        .columns
        .iter()
        .find(|field| field.key == "PRI")
        .or_else(|| table.columns.iter().find(|field| field.key == "UNI"))
        .or_else(|| table.columns.first())
        .map(|field| field.name.as_str())
}

fn clamp_vec(value: Vec2) -> Vec2 {
    value
        .max(Vec2::splat(-1_000_000.0))
        .min(Vec2::splat(1_000_000.0))
}

fn card_rows(height: f32, scale: f32) -> usize {
    ((height / scale - TITLE - 3.0) / ROW).max(0.0) as usize
}

fn field_label(column: &ColumnInfo) -> String {
    match column.key.as_str() {
        "PRI" => format!("主键  {}", column.name),
        "UNI" => format!("唯一  {}", column.name),
        _ => column.name.clone(),
    }
}

fn draw_page_lines(painter: &egui::Painter, viewport: Rect, origin: Pos2, scale: f32) {
    let stride = vec2(PAGE_WIDTH, PAGE_HEIGHT) * scale;
    let guide = Stroke::new(1.0_f32, Color32::from_rgb(167, 181, 188));
    let mut x = origin.x + ((viewport.left() - origin.x) / stride.x).ceil() * stride.x;
    while x < viewport.right() {
        painter.line_segment([pos2(x, viewport.top()), pos2(x, viewport.bottom())], guide);
        x += stride.x;
    }
    let mut y = origin.y + ((viewport.top() - origin.y) / stride.y).ceil() * stride.y;
    while y < viewport.bottom() {
        painter.line_segment([pos2(viewport.left(), y), pos2(viewport.right(), y)], guide);
        y += stride.y;
    }
}

fn same_definition(first: &str, second: &str) -> bool {
    fn without_trailing_delimiter(value: &str) -> &str {
        value.trim().trim_end_matches(';').trim_end()
    }
    without_trailing_delimiter(first)
        .lines()
        .eq(without_trailing_delimiter(second).lines())
}

fn menu_action(
    ui: &mut egui::Ui,
    title: &str,
    action: &mut Option<Action>,
    create: impl FnOnce() -> Action,
) {
    if ui.button(title).clicked() {
        *action = Some(create());
        ui.close();
    }
}

fn browse(op: Operation) -> Action {
    Action::Browse(BrowseAction::Manage(op))
}

impl Designer {
    fn card_menu(
        &mut self,
        ui: &mut egui::Ui,
        index: usize,
        tables: &Tables<'_>,
        action: &mut Option<Action>,
    ) {
        let card = &mut self.layout.tables[index];
        let previous = card.names_only;
        ui.selectable_value(&mut card.names_only, false, "显示字段和类型");
        ui.selectable_value(&mut card.names_only, true, "仅显示字段名");
        if previous != card.names_only {
            self.dirty = true;
            ui.close();
        }
        let has_saved = card.definition.is_some();
        if let Some(table) = tables.get(card.name.as_str()) {
            let object = &table.object;
            ui.separator();
            menu_action(ui, "查看实时建表语句", action, || {
                Action::Browse(BrowseAction::Inspect(object.clone()))
            });
            menu_action(ui, "修改表字段…", action, || {
                browse(Operation::TableStructure(object.clone()))
            });
            menu_action(ui, "管理索引…", action, || {
                browse(Operation::Index(object.clone()))
            });
            menu_action(ui, "管理外键…", action, || {
                browse(Operation::ForeignKey(object.clone()))
            });
            menu_action(ui, "重排字段…", action, || {
                browse(Operation::ColumnOrder(object.clone()))
            });
            menu_action(ui, "更改存储引擎…", action, || {
                browse(Operation::TableEngine(object.clone()))
            });
            menu_action(ui, "生成清空表 SQL", action, || {
                Action::StageSql(TableAction::Truncate.sql(&object.schema, &object.name))
            });
            menu_action(ui, "生成删除表 SQL", action, || {
                Action::StageSql(TableAction::Drop.sql(&object.schema, &object.name))
            });
        }
        if has_saved {
            ui.separator();
            if ui.button("查看布局中的建表 SQL").clicked() {
                *action = self.stage_saved(index);
                ui.close();
            }
        }
        ui.separator();
        if ui.button("移出画布").clicked() {
            self.remove_card(index);
            ui.close();
        }
    }

    fn relation_menu(
        &mut self,
        ui: &mut egui::Ui,
        key: &SchemaForeignKey,
        can_edit: bool,
        action: &mut Option<Action>,
    ) {
        ui.label(&key.name);
        for (child, parent) in &key.columns {
            ui.label(format!("{child}  →  {parent}"));
        }
        ui.label(format!(
            "被引用行删除时 {}   被引用字段更新时 {}",
            select_rule_label(&key.on_delete),
            select_rule_label(&key.on_update)
        ));
        if ui
            .add_enabled(can_edit, egui::Button::new("编辑此外键…"))
            .clicked()
        {
            self.edit_key(key);
            ui.close();
        }
        if ui
            .add_enabled(can_edit, egui::Button::new("删除此外键…"))
            .clicked()
        {
            self.delete_key = Some(key.clone());
            ui.close();
        }
        menu_action(ui, "管理外键…", action, || {
            browse(Operation::ForeignKey(key.table.clone()))
        });
        menu_action(ui, "生成删除外键 SQL", action, || {
            Action::StageSql(drop_key(key))
        });
    }

    fn add_table_menu(&mut self, ui: &mut egui::Ui, tables: &Tables<'_>) {
        ui.menu_button("添加已有表", |ui| {
            let placed: HashSet<_> = self
                .layout
                .tables
                .iter()
                .map(|card| card.name.as_str())
                .collect();
            let available = tables
                .values()
                .filter(|table| !placed.contains(table.object.name.as_str()))
                .copied()
                .collect::<Vec<_>>();
            if available.is_empty() {
                ui.label("没有可添加的表。");
                return;
            }
            let mut selected = None;
            egui::ScrollArea::vertical().max_height(230.0).show_rows(
                ui,
                ui.spacing().interact_size.y,
                available.len(),
                |ui, rows| {
                    for table in &available[rows] {
                        if ui.button(&table.object.name).clicked() {
                            selected = Some(*table);
                        }
                    }
                },
            );
            if let Some(table) = selected {
                self.add_card(&table.object.name, table.columns.len());
                self.selected_card = Some(table.object.name.clone());
                self.selected_key = None;
                self.fit = true;
                ui.close();
            }
        });
    }

    fn catalog_ui(
        &mut self,
        ui: &mut egui::Ui,
        tables: &Tables<'_>,
        height: f32,
        schema: &mut String,
        schemas: &[String],
    ) {
        let top = ui.next_widget_position().y;
        ui.label(RichText::new("表清单").strong())
            .on_hover_text("钥匙标示主键，菱形标示唯一键。");
        egui::ComboBox::from_id_salt("designer-schema")
            .selected_text(if schema.is_empty() {
                "选择数据库"
            } else {
                schema.as_str()
            })
            .width(ui.available_width())
            .truncate()
            .show_ui(ui, |ui| {
                for name in schemas {
                    if ui.selectable_value(schema, name.clone(), name).changed() {
                        ui.ctx().request_repaint();
                    }
                }
            });
        ui.add(
            egui::TextEdit::singleline(&mut self.catalog_filter)
                .desired_width(ui.available_width())
                .hint_text("搜索表"),
        );
        let filter = self.catalog_filter.to_lowercase();
        let visible = tables
            .values()
            .copied()
            .filter(|table| table.object.name.to_lowercase().contains(&filter))
            .collect::<Vec<_>>();
        let placed: HashSet<_> = self
            .layout
            .tables
            .iter()
            .map(|card| card.name.as_str())
            .collect();
        let added = visible
            .iter()
            .filter(|table| !placed.contains(table.object.name.as_str()))
            .count();
        let add_all = ui
            .add_enabled(
                added > 0,
                egui::Button::new(if filter.is_empty() {
                    "添加全部"
                } else {
                    "添加筛选结果"
                }),
            )
            .clicked();
        let mut choose = None;
        let row_height = ui.spacing().interact_size.y;
        let list_height = (height - (ui.next_widget_position().y - top)).max(row_height);
        egui::ScrollArea::vertical()
            .id_salt("schema-table-catalog")
            .max_height(list_height)
            .auto_shrink([false, true])
            .show_rows(ui, row_height, visible.len(), |ui, rows| {
                for table in &visible[rows] {
                    let name = table.object.name.as_str();
                    let row = egui::Button::selectable(placed.contains(name), name)
                        .min_size(vec2(ui.available_width(), row_height))
                        .truncate()
                        .sense(egui::Sense::click_and_drag());
                    let response = ui.add(row).on_hover_text(name);
                    drag_table(&response, &table.object);
                    if response.clicked() {
                        choose = Some(*table);
                    }
                }
            });
        drop(placed);
        if add_all {
            for table in visible {
                self.add_card(&table.object.name, table.columns.len());
            }
            self.fit = true;
        } else if let Some(table) = choose {
            self.add_card(&table.object.name, table.columns.len());
            self.selected_card = Some(table.object.name.clone());
            self.selected_key = None;
            self.fit = true;
        }
    }

    fn draft_from_selection(&mut self, tables: &Tables<'_>) {
        let mut available = self
            .layout
            .tables
            .iter()
            .enumerate()
            .filter(|(_, card)| tables.contains_key(card.name.as_str()));
        let Some((first, _)) = available.next() else {
            return;
        };
        let second = available.next().map_or(first, |(index, _)| index);
        let source = self
            .selected_card
            .as_ref()
            .and_then(|name| {
                self.layout
                    .tables
                    .iter()
                    .position(|card| &card.name == name && tables.contains_key(name.as_str()))
            })
            .unwrap_or(first);
        let target = if source == first { second } else { first };
        let parent = preferred_key(
            tables
                .get(self.layout.tables[target].name.as_str())
                .copied(),
        )
        .unwrap_or_default();
        self.new_key(source, target, "", parent);
    }

    pub(super) fn design_view(
        &mut self,
        ui: &mut egui::Ui,
        ws: &mut Workspace,
        interactive: bool,
        confirm_tab_close: bool,
    ) -> Option<Action> {
        if self.closed {
            return None;
        }
        self.update(ws);
        if self.reconciliation.is_some() {
            self.reconcile_view(ui, ws, interactive);
            return None;
        }
        let ctx = ui.ctx().clone();
        let page_height = ui.available_height();
        let design = ws.schema_designs.get(&self.layout.schema);
        let tables = tables_by_name(design);
        let keys = design.map_or(&[][..], |design| design.foreign_keys.as_slice());
        let saving = self
            .snapshot_pending
            .as_ref()
            .is_some_and(|snapshot| matches!(snapshot.purpose, SnapshotUse::Save(_)));
        let mut action = None;
        let mut open_layout = false;
        let mut close = false;
        let mut save = None;
        let mut export = None;
        let mut zoom_input = self.layout.zoom * 100.0;
        let mut next_schema = self.layout.schema.clone();
        let applying = self.mutation.is_some();
        let can_save = interactive && !ws.busy() && !saving && !applying;
        let connected = ws.server.is_some();
        let can_edit = interactive && !ws.busy() && connected && design.is_some();
        let pending_close = self.pending_close;
        if interactive
            && !saving
            && !applying
            && !pending_close
            && self.draft_key.is_none()
            && self.delete_key.is_none()
        {
            ctx.input_mut(|input| {
                if input.consume_key(egui::Modifiers::COMMAND, egui::Key::O) {
                    open_layout = true;
                }
                if can_save {
                    if input.consume_key(
                        egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                        egui::Key::S,
                    ) {
                        save = Some(true);
                    } else if input.consume_key(egui::Modifiers::COMMAND, egui::Key::S) {
                        save = Some(false);
                    }
                }
            });
        }
        let status = |ui: &mut egui::Ui, status: &Result<String, String>| {
            let (color, message) = match status {
                Ok(message) => (style::SUCCESS, message),
                Err(message) => (style::ERROR, message),
            };
            ui.add(egui::Label::new(RichText::new(message).color(color)).truncate())
                .on_hover_text(message);
        };
        egui::ScrollArea::vertical()
            .id_salt("schema-designer")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let page = style::pane(egui::Margin::same(10));
                page.show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    let top = ui.next_widget_position().y;
                    let footer = appearance::frame(egui::Margin::symmetric(8, 6));
                    ui.add_enabled_ui(
                        interactive && !saving && !applying && !pending_close,
                        |ui| {
                            appearance::frame(egui::Margin::symmetric(8, 5)).show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                ui.horizontal_wrapped(|ui| {
                                    if ui.button("打开布局…").clicked() {
                                        open_layout = true;
                                    }
                                    if ui
                                        .add_enabled(can_save, egui::Button::new("保存布局"))
                                        .clicked()
                                    {
                                        save = Some(false);
                                    }
                                    if ui
                                        .add_enabled(can_save, egui::Button::new("另存为…"))
                                        .clicked()
                                    {
                                        save = Some(true);
                                    }
                                    if ui.button("返回查询").clicked() {
                                        action = Some(Action::ReturnToQuery);
                                    }
                                    if ui.button("关闭").clicked() {
                                        close = true;
                                    }
                                    if let Some(path) = &self.path {
                                        ui.separator();
                                        let name = path
                                            .file_name()
                                            .unwrap_or(path.as_os_str())
                                            .to_string_lossy();
                                        let suffix = if self.dirty { " •" } else { "" };
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(format!("{name}{suffix}"))
                                                    .color(style::MUTED),
                                            )
                                            .truncate(),
                                        )
                                        .on_hover_text(path.display().to_string());
                                    }
                                });
                                ui.horizontal_wrapped(|ui| {
                                    if ui
                                        .add_enabled(
                                            connected
                                                && !ws.busy()
                                                && !self.layout.schema.is_empty(),
                                            egui::Button::new("刷新结构"),
                                        )
                                        .clicked()
                                    {
                                        self.request_refresh();
                                    }
                                    if ui
                                        .add_enabled(can_edit, egui::Button::new("新建表…"))
                                        .clicked()
                                    {
                                        action =
                                            Some(Action::CreateTable(self.layout.schema.clone()));
                                    }
                                    let usable_cards = self
                                        .layout
                                        .tables
                                        .iter()
                                        .any(|card| tables.contains_key(card.name.as_str()));
                                    if ui
                                        .add_enabled(
                                            usable_cards && can_edit,
                                            egui::Button::new("添加外键…"),
                                        )
                                        .clicked()
                                    {
                                        self.draft_from_selection(&tables);
                                    }
                                    ui.separator();
                                    if ui.button("适应画布").clicked() {
                                        self.fit = true;
                                    }
                                    egui::ComboBox::from_id_salt("designer-zoom")
                                        .width(80.0)
                                        .selected_text(format!("{}%", zoom_input.round()))
                                        .show_ui(ui, |ui| {
                                            for zoom in [10.0, 25.0, 50.0, 75.0, 100.0, 200.0] {
                                                ui.selectable_value(
                                                    &mut zoom_input,
                                                    zoom,
                                                    format!("{zoom}%"),
                                                );
                                            }
                                        });
                                    ui.add(
                                        egui::DragValue::new(&mut zoom_input)
                                            .range(10.0..=600.0)
                                            .speed(1.0)
                                            .fixed_decimals(0)
                                            .suffix("%"),
                                    )
                                    .on_hover_text("自定义缩放。也可在画布上按 Ctrl 滚动。");
                                    ui.separator();
                                    if ui
                                        .checkbox(&mut self.layout.page_breaks, "打印分页线")
                                        .changed()
                                    {
                                        self.dirty = true;
                                    }
                                    if let Some(format) =
                                        image_export_menu(ui, !self.layout.tables.is_empty())
                                    {
                                        export = Some(format);
                                    }
                                    if ui
                                        .add_enabled(
                                            !self.layout.tables.is_empty(),
                                            egui::Button::new("打印…"),
                                        )
                                        .clicked()
                                    {
                                        export = Some(DiagramExport::Print);
                                    }
                                });
                            });
                            let width = ui.available_width();
                            let tray_width = appearance::tray_width(width);
                            let spacing = ui.spacing();
                            let remaining = page_height
                                - (ui.next_widget_position().y - top)
                                - page.total_margin().sum().y
                                - footer.total_margin().sum().y
                                - spacing.interact_size.y
                                - 2.0 * spacing.item_spacing.y;
                            let height = remaining.max(165.0);
                            let canvas_width =
                                (width - tray_width - spacing.item_spacing.x).max(1.0);
                            if (zoom_input - self.layout.zoom * 100.0).abs() >= 0.1 {
                                self.zoom_at(
                                    zoom_input / 100.0,
                                    vec2(canvas_width / 2.0, height / 2.0),
                                );
                            }
                            ui.horizontal_top(|ui| {
                                appearance::tray(ui, vec2(tray_width, height), |ui, height| {
                                    self.catalog_ui(
                                        ui,
                                        &tables,
                                        height,
                                        &mut next_schema,
                                        &ws.schemas,
                                    );
                                });
                                let canvas_width = ui.available_width().max(1.0);
                                if let Some(format) = self.draw_canvas(
                                    ui,
                                    vec2(canvas_width, height),
                                    &tables,
                                    keys,
                                    design.is_some(),
                                    can_edit,
                                    &mut action,
                                ) {
                                    export = Some(format);
                                }
                            });
                        },
                    );
                    footer.show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        if pending_close {
                            ui.horizontal_wrapped(|ui| {
                                ui.colored_label(style::WARNING, "架构布局有未保存的修改。");
                                if ui
                                    .add_enabled(can_save, style::primary("保存并关闭"))
                                    .clicked()
                                {
                                    save = Some(false);
                                }
                                if ui
                                    .add_enabled(
                                        interactive && !saving,
                                        egui::Button::new("不保存"),
                                    )
                                    .clicked()
                                {
                                    self.pending_close = false;
                                    self.closed = true;
                                }
                                if ui
                                    .add_enabled(
                                        interactive && !saving,
                                        egui::Button::new("继续编辑"),
                                    )
                                    .clicked()
                                {
                                    self.pending_close = false;
                                }
                            });
                            if let Some(current) = &self.status {
                                status(ui, current);
                            }
                        } else if saving || applying || ws.busy() {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(&ws.status);
                            });
                        } else if let Some(current) = &self.status {
                            status(ui, current);
                        } else if let Some(key) = self.selected_relation(keys) {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(format!(
                                    "{}  {} → {}",
                                    key.name, key.table.name, key.referenced_table.name
                                ));
                                ui.menu_button("外键属性", |ui| {
                                    self.relation_menu(ui, key, can_edit, &mut action)
                                });
                            });
                        } else if let Some(index) = self.selected_card.as_ref().and_then(|name| {
                            self.layout
                                .tables
                                .iter()
                                .position(|card| &card.name == name)
                        }) {
                            let differing = self.definition_differs(&self.layout.tables[index]);
                            ui.horizontal_wrapped(|ui| {
                                ui.menu_button(self.layout.tables[index].name.clone(), |ui| {
                                    self.card_menu(ui, index, &tables, &mut action)
                                });
                                if let Some(table) =
                                    tables.get(self.layout.tables[index].name.as_str())
                                {
                                    ui.add_enabled_ui(can_edit, |ui| {
                                        if ui.small_button("修改表字段…").clicked() {
                                            action = Some(browse(Operation::TableStructure(
                                                table.object.clone(),
                                            )));
                                        }
                                        if ui.small_button("索引…").clicked() {
                                            action = Some(browse(Operation::Index(
                                                table.object.clone(),
                                            )));
                                        }
                                        if ui.small_button("外键…").clicked() {
                                            action = Some(browse(Operation::ForeignKey(
                                                table.object.clone(),
                                            )));
                                        }
                                    });
                                }
                                if differing {
                                    ui.label(
                                        RichText::new("布局中的建表语句与当前数据库不同。")
                                            .color(style::WARNING),
                                    );
                                }
                            });
                        } else if !connected {
                            ui.colored_label(style::MUTED, "连接数据库后可读取表和关系。");
                        } else if !ws.schemas.contains(&self.layout.schema) {
                            ui.colored_label(style::MUTED, "请选择当前连接中的数据库。");
                        } else {
                            ui.label(
                                RichText::new(
                                    "从表清单拖入表，拖动字段建立外键，双击连线编辑外键。",
                                )
                                .color(style::MUTED),
                            );
                        }
                    });
                });
            });
        if next_schema != self.layout.schema {
            self.change_schema(next_schema);
        }
        if let Some(format) = export {
            self.export_diagram(&ctx, &tables, keys, format);
        }
        let decision = interactive
            .then(|| {
                self.key_dialog(&ctx, &tables, can_edit)
                    .or_else(|| self.delete_key_dialog(&ctx, can_edit))
            })
            .flatten();
        match decision {
            Some(KeyDecision::Stage(sql)) => action = Some(Action::StageSql(sql)),
            Some(KeyDecision::Apply(change)) => self.apply_key_change(ws, change),
            None => {}
        }
        if let Some(save_as) = save {
            self.save(ws, save_as);
        }
        if close {
            self.request_close(confirm_tab_close);
        }
        if open_layout && let Some(path) = pick_layout() {
            action = Some(Action::OpenLayout(path));
        }
        action
    }
}

fn drop_key(key: &SchemaForeignKey) -> String {
    format!(
        "ALTER TABLE {} DROP FOREIGN KEY {};\n",
        qualified(&key.table.schema, &key.table.name),
        quote_identifier(&key.name),
    )
}

pub(super) fn pick_layout() -> Option<PathBuf> {
    FileDialog::new()
        .set_title("打开架构布局")
        .add_filter("架构布局", &["hschema", "schemaxml", "xml"])
        .pick_file()
}

#[derive(Clone, Copy)]
enum DiagramExport {
    Bitmap,
    Svg,
    PrintableHtml,
    Print,
}

fn image_export_menu(ui: &mut egui::Ui, enabled: bool) -> Option<DiagramExport> {
    ui.add_enabled_ui(enabled, |ui| {
        ui.menu_button("导出图像…", |ui| {
            for (label, format) in [
                ("BMP 位图…", DiagramExport::Bitmap),
                ("SVG 矢量图…", DiagramExport::Svg),
                ("可打印网页…", DiagramExport::PrintableHtml),
            ] {
                if ui.button(label).clicked() {
                    ui.close();
                    return Some(format);
                }
            }
            None
        })
        .inner
        .flatten()
    })
    .inner
}

impl Designer {
    fn export_diagram(
        &mut self,
        ctx: &egui::Context,
        tables: &Tables<'_>,
        keys: &[SchemaForeignKey],
        format: DiagramExport,
    ) {
        let graph = self.geometry(Pos2::ZERO, 1.0, tables, keys);
        let Some(mut bounds) = self.layout.bounds() else {
            return;
        };
        for cable in &graph.links {
            for point in cable.points {
                bounds.extend_with(point);
            }
        }
        #[cfg(any(windows, target_os = "macos"))]
        if matches!(format, DiagramExport::Print) {
            let pages = PrintPages::new(bounds, self.layout.page_breaks);
            match self.print_diagram(ctx, &graph, tables, &pages) {
                Ok(true) => {
                    self.status = Some(Ok(if pages.shrunk {
                        "分页数超过 64，已将整张架构图缩放为单页并提交打印。".into()
                    } else {
                        "架构图已提交打印。".into()
                    }));
                }
                Ok(false) => {}
                Err(error) => self.status = Some(Err(format!("架构图打印失败。{error}"))),
            }
            return;
        }
        let (title, filter, extension, name) = match format {
            DiagramExport::Bitmap => ("导出架构图像", "BMP 位图", "bmp", "架构布局.bmp"),
            DiagramExport::Svg => ("导出架构图像", "SVG 矢量图", "svg", "架构布局.svg"),
            DiagramExport::Print | DiagramExport::PrintableHtml => (
                "保存可打印的架构布局",
                "可打印网页",
                "html",
                "架构布局.html",
            ),
        };
        let Some(path) = FileDialog::new()
            .set_title(title)
            .add_filter(filter, &[extension])
            .set_file_name(name)
            .save_file()
        else {
            return;
        };
        let padded = bounds.expand(16.0);
        if matches!(format, DiagramExport::Bitmap) {
            self.status = Some(
                self.write_bitmap(ctx, &path, &graph, padded, tables)
                    .map(|()| "架构图像已导出。".into())
                    .map_err(|error| format!("导出架构图失败。{error}")),
            );
            return;
        }
        let print = matches!(format, DiagramExport::Print | DiagramExport::PrintableHtml);
        let drawing = self.svg_body(&graph, tables);
        let svg_ns = "http://www.w3.org/2000/svg";
        let mut shrunk = false;
        let contents = if print {
            let pages = PrintPages::new(bounds, self.layout.page_breaks);
            shrunk = pages.shrunk;
            let mut page_markup = String::new();
            for viewport in pages.viewports {
                let _ = write!(
                    page_markup,
                    r##"<svg class="page" viewBox="{}"><use href="#design"/></svg>"##,
                    view_box(viewport)
                );
            }
            format!(
                r##"<!doctype html><html lang="zh-CN"><meta charset="utf-8">
<title>架构布局打印</title>
<style>@page{{size:A4;margin:6mm}} body{{font-family:system-ui,sans-serif}}
.page{{display:block;width:100%;height:auto;max-height:285mm;overflow:hidden}} .page+.page{{break-before:page}}
@media print{{button{{display:none}}body{{margin:0}}}}</style>
<button type="button" onclick="window.print()">打印</button>
<svg xmlns="{svg_ns}" width="0" height="0" aria-hidden="true"><defs><g id="design">{drawing}</g></defs></svg>
{page_markup}</html>"##
            )
        } else {
            format!(
                r#"<svg xmlns="{svg_ns}" viewBox="{}" width="{}" height="{}">{drawing}</svg>"#,
                view_box(padded),
                padded.width().ceil(),
                padded.height().ceil()
            )
        };
        match write_atomic(&path, contents.as_bytes()) {
            Ok(()) => {
                self.status = Some(Ok(if print {
                    if matches!(format, DiagramExport::Print)
                        && let Some(url) = local_file_url(&path)
                    {
                        ctx.open_url(egui::OpenUrl::new_tab(url));
                    }
                    if shrunk {
                        "分页数超过 64，已保存适应单页的打印稿。".into()
                    } else {
                        "已保存打印稿，可在浏览器中打印。".into()
                    }
                } else {
                    "架构图像已导出。".into()
                }));
            }
            Err(error) => self.status = Some(Err(format!("导出架构图失败。{error}"))),
        }
    }

    fn svg_body(&self, graph: &Graph<'_>, tables: &Tables<'_>) -> String {
        use quick_xml::escape::escape;
        let mut body = String::from(
            r##"<defs><marker id="arrow" markerWidth="9" markerHeight="9" refX="8" refY="4.5" orient="auto"><path d="M1 1L8 4.5L1 8" fill="none" stroke="#536774"/></marker></defs>"##,
        );
        for cable in &graph.links {
            let _ = write!(
                body,
                r##"<polyline points="{},{} {},{} {},{} {},{}" fill="none" stroke="#536774" stroke-width="1.4" marker-end="url(#arrow)"/>"##,
                cable.points[0].x,
                cable.points[0].y,
                cable.points[1].x,
                cable.points[1].y,
                cable.points[2].x,
                cable.points[2].y,
                cable.points[3].x,
                cable.points[3].y,
            );
            for (position, label) in cable.labels(1.0) {
                let _ = write!(
                    body,
                    r##"<text x="{}" y="{}" dominant-baseline="central" text-anchor="middle" font-family="system-ui, sans-serif" font-size="14" fill="#536774">{label}</text>"##,
                    position.x, position.y,
                );
            }
        }
        let _ = write!(
            body,
            r##"<g font-family="system-ui, sans-serif" font-size="14" fill="#1d2a35">"##
        );
        for (index, rect) in graph.cards.iter().copied().enumerate() {
            let card = &self.layout.tables[index];
            let rect_attrs = svg_rect(rect);
            let names_width = if card.names_only {
                rect.width()
            } else {
                rect.width() * 0.54 - 4.0
            };
            let names_rect = Rect::from_min_size(rect.min, vec2(names_width, rect.height()));
            let _ = write!(
                body,
                r##"<clipPath id="card-{index}"><rect {rect_attrs}/></clipPath><clipPath id="name-{index}"><rect {}/></clipPath>"##,
                svg_rect(names_rect)
            );
            let _ = write!(
                body,
                r##"<rect {rect_attrs} rx="4" fill="#fff" stroke="#d8e2e7"/><g clip-path="url(#card-{index})">"##
            );
            let _ = write!(
                body,
                r##"<rect x="{}" y="{}" width="{}" height="{TITLE}" fill="#d5ebe8"/>"##,
                rect.left(),
                rect.top(),
                rect.width()
            );
            let _ = write!(
                body,
                r#"<text x="{}" y="{}" font-weight="650">{}</text>"#,
                rect.left() + 9.0,
                rect.top() + 21.0,
                escape(&card.name)
            );
            let columns = Self::card_columns(card, tables);
            let count = card_rows(rect.height(), 1.0);
            let first = self.row_offsets.get(&card.name).copied().unwrap_or(0);
            for (row, column) in columns.iter().skip(first).take(count).enumerate() {
                let y = rect.top() + TITLE + 16.0 + row as f32 * ROW;
                let _ = write!(
                    body,
                    r##"<text x="{}" y="{y}" clip-path="url(#name-{index})">{}</text>"##,
                    rect.left() + 8.0,
                    escape(field_label(column))
                );
                if !card.names_only {
                    let _ = write!(
                        body,
                        r##"<text x="{}" y="{y}" fill="#536774">{}</text>"##,
                        rect.left() + rect.width() * 0.54,
                        escape(&column.data_type)
                    );
                }
            }
            if first > 0 || columns.len() > first + count {
                let _ = write!(
                    body,
                    r##"<text x="{}" y="{}" fill="#536774" font-size="10">…</text>"##,
                    rect.left() + 8.0,
                    rect.bottom() - 4.0
                );
            }
            body.push_str("</g>");
        }
        body.push_str("</g>");
        body
    }
}

fn svg_rect(rect: Rect) -> String {
    format!(
        r#"x="{}" y="{}" width="{}" height="{}""#,
        rect.left(),
        rect.top(),
        rect.width(),
        rect.height()
    )
}

fn view_box(rect: Rect) -> String {
    format!(
        "{} {} {} {}",
        rect.left(),
        rect.top(),
        rect.width(),
        rect.height()
    )
}

fn local_file_url(path: &Path) -> Option<String> {
    let path = path.canonicalize().ok()?;
    let raw = path.to_str()?;
    let raw = if let Some(path) = raw.strip_prefix(r"\\?\UNC\") {
        format!("//{path}")
    } else {
        raw.strip_prefix(r"\\?\").unwrap_or(raw).to_owned()
    };
    let raw = raw.replace('\\', "/");
    let mut url = if raw.starts_with("//") {
        "file:".to_owned()
    } else if raw.starts_with('/') {
        "file://".to_owned()
    } else {
        "file:///".to_owned()
    };
    for char in raw.bytes() {
        if char.is_ascii_alphanumeric() || b"-._~/:".contains(&char) {
            url.push(char::from(char));
        } else {
            let _ = write!(url, "%{char:02X}");
        }
    }
    Some(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{design_tabs::Tabs, navigation::Document};

    #[test]
    fn design_tabs_keep_separate_layouts_and_workspace_selection() {
        let mut tabs = Tabs::default();
        let table = DbObject {
            schema: "warehouse".into(),
            kind: ObjectKind::Table,
            name: "stock".into(),
        };
        let first = tabs.new_design(1, "warehouse".into(), Some(table));
        let second = tabs.new_design(1, "warehouse".into(), None);
        let third = tabs.new_design(2, "analytics".into(), None);

        assert_eq!(tabs.active(1), Some(Document::Design(second)));
        assert_eq!(tabs.active(2), Some(Document::Design(third)));
        assert!(tabs.dirty(Some(1)));
        assert!(tabs.schema_mut(second).unwrap().layout.tables.is_empty());
        tabs.select(1, Document::Design(first));
        assert_eq!(tabs.active(1), Some(Document::Design(first)));
        assert_eq!(
            tabs.schema_mut(first).unwrap().layout.tables[0].name,
            "stock"
        );

        tabs.select_query(1);
        assert_eq!(tabs.list(1).count(), 2);
        tabs.remove_workspace(1);
        assert_eq!(tabs.list(1).count(), 0);
        assert_eq!(tabs.active(2), Some(Document::Design(third)));
    }

    #[test]
    fn file_backed_design_close_still_prompts_with_draft_prompt_disabled() {
        let mut designer = Designer::new(
            1,
            "warehouse".into(),
            Some(DbObject {
                schema: "warehouse".into(),
                kind: ObjectKind::Table,
                name: "stock".into(),
            }),
        );
        designer.path = Some(PathBuf::from("warehouse.hbr"));
        designer.request_close(false);
        assert!(designer.pending_close);
        assert!(!designer.closed);
    }

    #[test]
    fn separately_opened_layouts_keep_each_others_saves() {
        let executable = std::env::current_exe().unwrap();
        let fixture = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let path = fixture.path().join("库存.hschema");
        let mut initial = Designer::new(1, "inventory".into(), None);
        initial.add_card("items", 3);
        initial.write_layout(
            SaveTarget {
                path: path.clone(),
                expected: None,
            },
            false,
        );
        let mut first = Designer::from_file(1, path.clone()).unwrap();
        let mut second = Designer::from_file(2, path.clone()).unwrap();

        first.add_card("depots", 2);
        first.write_layout(
            SaveTarget {
                path: path.clone(),
                expected: first.saved_revision,
            },
            false,
        );
        second.add_card("invoices", 2);
        second.write_layout(
            SaveTarget {
                path: path.clone(),
                expected: second.saved_revision,
            },
            false,
        );

        assert!(second.dirty);
        assert!(second.status.as_ref().is_some_and(Result::is_err));
        let (written, _) = read_layout(&path).unwrap();
        assert_eq!(
            written
                .tables
                .iter()
                .map(|table| table.name.as_str())
                .collect::<Vec<_>>(),
            ["items", "depots"]
        );
    }
    use crate::database::ServerInfo;

    #[test]
    fn preserves_legacy_layout_geometry_and_definition() {
        let fixture = r#"<?xml version="1.0" encoding="UTF-8"?>
<schemadesigner version="6.5">
  <source><database charset="utf8mb4" collation="utf8mb4_bin">样例库</database></source>
  <canvas zoom="50">
    <tables>
      <table name="content" view="colnames">
        <left>140</left><top>40</top><width>120</width><height>90</height>
        <sql_create_table>CREATE TABLE `content` (`caption` varchar(10) DEFAULT 'a &amp; b')</sql_create_table>
      </table>
    </tables>
  </canvas>
</schemadesigner>"#;
        let layout = read_legacy(fixture).unwrap();
        layout.validate().unwrap();
        assert_eq!(layout.schema, "样例库");
        assert_eq!(layout.zoom, 0.5);
        let card = &layout.tables[0];
        assert_eq!(card.position, [280.0, 80.0]);
        assert_eq!(card.size, [240.0, 180.0]);
        assert!(card.names_only);
        assert!(card.definition.as_deref().unwrap().contains("'a & b'"));
        let legacy = encode_layout(&layout, Path::new("layout.schemaxml")).unwrap();
        let reopened = read_legacy(std::str::from_utf8(&legacy).unwrap()).unwrap();
        assert_eq!(reopened.tables[0].position, card.position);
        assert_eq!(reopened.tables[0].definition, card.definition);
        let native = encode_layout(&layout, Path::new("layout.hschema")).unwrap();
        let reopened: Layout = serde_json::from_slice(&native).unwrap();
        assert_eq!(reopened.tables[0].name, "content");
        assert!(
            layout_definition("CREATE TABLE `test` (id INT) CONNECTION='remote_server'").is_none()
        );
        assert!(layout_definition("CREATE TABLE `test` (id INT); DROP TABLE `test`").is_none());
    }

    #[test]
    #[ignore = "requires a disposable MariaDB via HARBOR_HTTP_FIXTURE_PORT"]
    fn displays_cardinality_from_complete_composite_indexes() {
        use crate::database::{ConnectionConfig, HttpTunnelConfig, Response, Worker};
        use mysql::{Conn, OptsBuilder, prelude::Queryable};
        use std::time::Duration;

        let port: u16 = std::env::var("HARBOR_HTTP_FIXTURE_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let mut fixture = Conn::new(
            OptsBuilder::new()
                .ip_or_hostname(Some("127.0.0.1"))
                .tcp_port(port)
                .user(Some("root"))
                .prefer_socket(false),
        )
        .unwrap();
        let schema = format!("harbor_http_design_indexes_{}", std::process::id());
        fixture
            .query_drop(format!("CREATE DATABASE {schema}"))
            .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fixture.select_db(&schema).unwrap();
            fixture.query_drop("CREATE TABLE parent (sku INT NOT NULL, tenant INT NOT NULL, PRIMARY KEY (tenant, sku)) ENGINE=InnoDB").unwrap();
            fixture.query_drop("CREATE TABLE child_one (id INT PRIMARY KEY, tenant INT, sku INT, UNIQUE KEY uq_target (sku, tenant), FOREIGN KEY (tenant, sku) REFERENCES parent (tenant,sku)) ENGINE=InnoDB").unwrap();
            fixture.query_drop("CREATE TABLE child_many (id INT PRIMARY KEY, tenant INT, sku INT, revision INT, UNIQUE KEY uq_history (tenant,sku,revision), FOREIGN KEY (tenant,sku) REFERENCES parent (tenant,sku)) ENGINE=InnoDB").unwrap();

            let config = ConnectionConfig {
                port,
                ..Default::default()
            };
            let mut connections = vec![("native", Command::Connect(config.clone()))];
            if let Ok(url) = std::env::var("HARBOR_LEGACY_TUNNEL_URL") {
                let endpoint = reqwest::Url::parse(&url).unwrap();
                assert_eq!(endpoint.scheme(), "http");
                assert_eq!(endpoint.host_str(), Some("127.0.0.1"));
                assert!(endpoint.username().is_empty() && endpoint.password().is_none());
                connections.push((
                    "archived PHP",
                    Command::ConnectHttp(
                        config,
                        HttpTunnelConfig {
                            url,
                            force_base64: true,
                            use_utf8mb4: true,
                            ..Default::default()
                        },
                    ),
                ));
            }

            for (transport, connect) in connections {
                let worker = Worker::new();
                let request = |id, command| {
                    worker.send(id, command).unwrap();
                    let outcome = worker
                        .recv_timeout(Duration::from_secs(15))
                        .unwrap()
                        .unwrap();
                    assert_eq!(outcome.id, id);
                    outcome
                        .result
                        .unwrap_or_else(|error| panic!("{transport}: {error}"))
                };
                assert!(matches!(request(1, connect), Response::Connected(_)));
                let Response::SchemaDesign(design) = request(
                    2,
                    Command::DescribeSchemaDesign {
                        schema: schema.clone(),
                    },
                ) else {
                    panic!("expected schema design")
                };
                let tables = tables_by_name(Some(&design));
                assert_eq!(preferred_key(tables.get("parent").copied()), Some("tenant"));
                let mut designer = Designer::new(1, schema.clone(), None);
                for table in ["parent", "child_one", "child_many"] {
                    designer.add_card(table, tables[table].columns.len());
                }
                let graph = designer.geometry(Pos2::ZERO, 1.0, &tables, &design.foreign_keys);
                assert_eq!(graph.links.len(), 2);
                for link in &graph.links {
                    let observed = link.labels(1.0).map(|(_, label)| label).collect::<Vec<_>>();
                    let expected = if link.key.table.name == "child_one" {
                        ["1", "1"]
                    } else {
                        ["∞", "1"]
                    };
                    assert_eq!(observed, expected, "{transport}: {}", link.key.table.name);
                }
                let svg = designer.svg_body(&graph, &tables);
                assert!(svg.contains(">∞</text>") && svg.contains(">1</text>"));
            }
        }));
        fixture
            .query_drop(format!("DROP DATABASE IF EXISTS {schema}"))
            .unwrap();
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }

    const TEST_FRAME: Rect = Rect::from_min_max(Pos2::ZERO, pos2(850.0, 560.0));

    fn render(
        ctx: &egui::Context,
        designer: &mut Designer,
        workspace: &mut Workspace,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(TEST_FRAME),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    assert!(designer.design_view(ui, workspace, true, true).is_none());
                });
            },
        )
    }

    fn label_position(output: &egui::FullOutput, label: &str, minimum_x: f32) -> Pos2 {
        fn search(shape: &egui::epaint::Shape, label: &str, clip: Rect) -> Option<Pos2> {
            use egui::epaint::Shape;
            match shape {
                Shape::Text(text) if text.galley.text() == label => {
                    let point = text.galley.rect.translate(text.pos.to_vec2()).center();
                    clip.contains(point).then_some(point)
                }
                Shape::Vec(items) => items.iter().find_map(|item| search(item, label, clip)),
                _ => None,
            }
        }
        let area = Rect::from_min_max(pos2(minimum_x, 0.0), TEST_FRAME.max);
        output
            .shapes
            .iter()
            .find_map(|shape| search(&shape.shape, label, shape.clip_rect.intersect(area)))
            .unwrap_or_else(|| panic!("测试画布中未找到控件 {label}"))
    }

    fn click_or_drag(
        ctx: &egui::Context,
        designer: &mut Designer,
        workspace: &mut Workspace,
        start: Pos2,
        end: Pos2,
    ) -> egui::FullOutput {
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        render(
            ctx,
            designer,
            workspace,
            vec![egui::Event::PointerMoved(start), button(start, true)],
        );
        render(
            ctx,
            designer,
            workspace,
            vec![egui::Event::PointerMoved(end)],
        );
        render(ctx, designer, workspace, vec![button(end, false)]);
        render(ctx, designer, workspace, Vec::new())
    }

    #[test]
    fn moves_cards_and_stages_a_dragged_foreign_key() {
        let ctx = egui::Context::default();
        style::apply(&ctx);
        let mut workspace = Workspace::new("测试连接".into(), None);
        let schema = "sample";
        workspace.server = Some(ServerInfo {
            version: "test".into(),
            connection_id: 1,
            database: Some(schema.into()),
            http_tunnel: false,
        });
        workspace.schemas.push(schema.into());
        let object = |name: &str| DbObject {
            schema: schema.into(),
            name: name.into(),
            kind: ObjectKind::Table,
        };
        let field = |name: &str, key: &str| ColumnInfo {
            name: name.into(),
            data_type: "int".into(),
            nullable: false,
            key: key.into(),
            default_value: None,
            extra: String::new(),
        };
        workspace.schema_designs.insert(
            schema.into(),
            SchemaDesign {
                schema: schema.into(),
                tables: vec![
                    SchemaDesignTable {
                        object: object("child_table"),
                        columns: vec![field("parent_id", "")],
                        unique_indexes: Some(Vec::new()),
                    },
                    SchemaDesignTable {
                        object: object("parent_table"),
                        columns: vec![field("id", "PRI")],
                        unique_indexes: Some(vec![crate::database::SchemaUniqueIndex {
                            name: "PRIMARY".into(),
                            columns: vec!["id".into()],
                        }]),
                    },
                ],
                foreign_keys: vec![SchemaForeignKey {
                    table: object("child_table"),
                    name: "fk_parent".into(),
                    referenced_table: object("parent_table"),
                    columns: vec![("parent_id".into(), "id".into())],
                    on_delete: "RESTRICT".into(),
                    on_update: "RESTRICT".into(),
                }],
            },
        );
        let mut designer = Designer::new(workspace.key, schema.into(), None);
        render(&ctx, &mut designer, &mut workspace, Vec::new());
        let initial = render(&ctx, &mut designer, &mut workspace, Vec::new());
        for control in [
            "新建表…",
            "另存为…",
            "适应画布",
            "添加外键…",
            "导出图像…",
            "打印…",
            "关闭",
        ] {
            label_position(&initial, control, 0.0);
        }
        let catalog_table = label_position(&initial, "child_table", 0.0);
        let destination = label_position(&initial, "从左侧添加表到画布", 200.0);
        let first = click_or_drag(
            &ctx,
            &mut designer,
            &mut workspace,
            catalog_table,
            destination,
        );
        assert_eq!(designer.layout.tables.len(), 1);
        assert_eq!(designer.layout.tables[0].name, "child_table");
        let add = label_position(&first, "添加全部", 0.0);
        let canvas = click_or_drag(&ctx, &mut designer, &mut workspace, add, add);
        assert_eq!(designer.layout.tables.len(), 2);
        label_position(&canvas, "1", 200.0);
        label_position(&canvas, "∞", 200.0);
        let header = label_position(&canvas, "child_table", 200.0);
        let before = designer.layout.tables[0].position;
        let moved = click_or_drag(
            &ctx,
            &mut designer,
            &mut workspace,
            header,
            header + vec2(24.0, 42.0),
        );
        assert!(designer.layout.tables[0].position[1] > before[1] + 20.0);
        let from = label_position(&moved, "parent_id", 200.0);
        let to = label_position(&moved, "主键  id", 200.0);
        click_or_drag(&ctx, &mut designer, &mut workspace, from, to);
        let graph = &workspace.schema_designs[schema];
        let tables = tables_by_name(Some(graph));
        let draft = designer
            .draft_key
            .as_ref()
            .expect("没有根据拖动创建外键草稿");
        let sql = draft.statement(schema, &tables).unwrap();
        assert!(sql.contains("ALTER TABLE `sample`.`child_table`"));
        assert!(
            sql.contains("FOREIGN KEY (`parent_id`)\n  REFERENCES `sample`.`parent_table` (`id`)")
        );
        let geometry = designer.geometry(Pos2::ZERO, 1.0, &tables, &graph.foreign_keys);
        assert_eq!(geometry.links.len(), 1);
        assert_eq!(
            drop_key(geometry.links[0].key),
            "ALTER TABLE `sample`.`child_table` DROP FOREIGN KEY `fk_parent`;\n",
        );
    }
}
