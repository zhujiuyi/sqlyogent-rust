use std::fs::{self, OpenOptions};
use std::io;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use eframe::egui::{self, DragValue};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::database::ProfileOptions;
use crate::database::transfer::{
    DEFAULT_DUMP_CHUNK_ROWS, DumpOptions, DumpSettings, MAX_BULK_INSERT_KIB,
    MAX_HTTP_DUMP_CHUNK_ROWS,
};
use crate::sql_tools::{MAX_FORMAT_INDENT, SqlFormatSettings, format_sql_with_settings};

use super::state::{profile_path, write_atomic};
use super::style;
use super::system_fonts::{self, FontFace};

mod grid_widths;
pub(super) use grid_widths::{GridColumns, GridWidths};

const VERSION: u64 = 1;
pub(super) const DEFAULT_TABLE_DATA_LIMIT: usize = 50;
const MIN_ROWS: usize = 1;
const MAX_ROWS: usize = 10_000;
const MIN_ZOOM: u16 = 75;
const MAX_ZOOM: u16 = 150;
const MIN_FONT_SIZE: u8 = 8;
const MAX_FONT_SIZE: u8 = 36;
const MAX_TAB_WIDTH: u8 = 16;
const DUMP_CHECKBOXES: &str = "dump_checkboxes";
const COPY_CHOICES: &str = "copy_choices";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum FontFamily {
    #[default]
    Monospace,
    Proportional,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FontRole {
    Editor,
    History,
    Blob,
    Grid,
}

impl FontRole {
    pub(super) const ALL: [Self; 4] = [Self::Editor, Self::History, Self::Blob, Self::Grid];

    pub(super) fn font_name(self) -> &'static str {
        match self {
            Self::Editor => "Harbor SQL editor font",
            Self::History => "Harbor SQL history font",
            Self::Blob => "Harbor SQL cell viewer font",
            Self::Grid => "Harbor SQL grid font",
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Editor => "SQL 编辑器",
            Self::History => "查询历史和对象定义",
            Self::Blob => "单元格内容查看器",
            Self::Grid => "数据表格和字段编辑表格",
        }
    }
}

#[derive(Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct FontChoices {
    pub editor: Option<FontFace>,
    pub history: Option<FontFace>,
    pub blob: Option<FontFace>,
    pub grid: Option<FontFace>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct TextFont {
    family: FontFamily,
    size: u8,
    #[serde(skip)]
    pub(super) system_role: Option<FontRole>,
}

impl Default for TextFont {
    fn default() -> Self {
        Self {
            family: FontFamily::Monospace,
            size: 14,
            system_role: None,
        }
    }
}

impl TextFont {
    pub(in crate::app) fn id(self) -> egui::FontId {
        let family = self.system_role.map_or_else(
            || self.builtin_family(),
            |role| egui::FontFamily::Name(role.font_name().into()),
        );
        egui::FontId::new(f32::from(self.size), family)
    }

    pub(super) fn builtin_family(self) -> egui::FontFamily {
        match self.family {
            FontFamily::Monospace => egui::FontFamily::Monospace,
            FontFamily::Proportional => egui::FontFamily::Proportional,
        }
    }

    fn label(self) -> &'static str {
        match self.family {
            FontFamily::Monospace => "内置等宽",
            FontFamily::Proportional => "内置比例",
        }
    }

    fn bound(&mut self) {
        self.size = self.size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct SqlColors {
    pub normal: [u8; 3],
    pub keyword: [u8; 3],
    pub function: [u8; 3],
    pub string: [u8; 3],
    pub comment: [u8; 3],
}

impl Default for SqlColors {
    fn default() -> Self {
        Self {
            normal: [29, 42, 53],
            keyword: [21, 103, 105],
            function: [33, 91, 152],
            string: [153, 68, 30],
            comment: [83, 103, 116],
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum LetterCase {
    #[default]
    AsTyped,
    Upper,
    Lower,
}

impl LetterCase {
    fn label(self) -> &'static str {
        match self {
            Self::AsTyped => "保留原样",
            Self::Upper => "大写",
            Self::Lower => "小写",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct EditorSettings {
    pub font: TextFont,
    pub history_font: TextFont,
    pub blob_font: TextFont,
    pub grid_font: TextFont,
    pub colors: SqlColors,
    pub keyword_case: LetterCase,
    pub function_case: LetterCase,
    pub tab_width: u8,
    pub indent_with_spaces: bool,
    pub word_wrap: bool,
}

impl Default for EditorSettings {
    fn default() -> Self {
        Self {
            font: TextFont::default(),
            history_font: TextFont::default(),
            blob_font: TextFont::default(),
            grid_font: TextFont::default(),
            colors: SqlColors::default(),
            keyword_case: LetterCase::default(),
            function_case: LetterCase::default(),
            tab_width: 4,
            indent_with_spaces: false,
            word_wrap: true,
        }
    }
}

impl EditorSettings {
    pub(super) fn changes_case(self) -> bool {
        self.keyword_case != LetterCase::AsTyped || self.function_case != LetterCase::AsTyped
    }

    fn bound(&mut self) {
        self.font.bound();
        self.history_font.bound();
        self.blob_font.bound();
        self.grid_font.bound();
        self.tab_width = self.tab_width.clamp(1, MAX_TAB_WIDTH);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct DumpPreferences {
    pub use_source_packet_limit: bool,
    pub bulk_insert_kib: u32,
    pub chunked: bool,
    pub chunk_rows: u32,
}

impl Default for DumpPreferences {
    fn default() -> Self {
        Self {
            use_source_packet_limit: true,
            bulk_insert_kib: 1,
            chunked: false,
            chunk_rows: DEFAULT_DUMP_CHUNK_ROWS,
        }
    }
}

impl DumpPreferences {
    pub(super) fn bulk_insert_limit_kib(self) -> Option<NonZeroU32> {
        if self.use_source_packet_limit {
            None
        } else {
            NonZeroU32::new(self.bulk_insert_kib)
        }
    }

    pub(super) fn chunk_rows(self) -> Option<NonZeroU32> {
        if self.chunked {
            NonZeroU32::new(self.chunk_rows)
        } else {
            None
        }
    }

    fn bound(&mut self) {
        self.bulk_insert_kib = self.bulk_insert_kib.clamp(1, MAX_BULK_INSERT_KIB);
        self.chunk_rows = self.chunk_rows.max(1);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct DumpCheckboxes {
    lock_source_tables: bool,
    flush_logs: bool,
    single_transaction: bool,
    lock_insert_tables: bool,
    include_use: bool,
    include_create_database: bool,
    disable_foreign_key_checks: bool,
    bulk_insert: bool,
    drop_existing: bool,
}

impl Default for DumpCheckboxes {
    fn default() -> Self {
        Self {
            lock_source_tables: false,
            flush_logs: false,
            single_transaction: false,
            lock_insert_tables: false,
            include_use: true,
            include_create_database: true,
            disable_foreign_key_checks: true,
            bulk_insert: true,
            drop_existing: true,
        }
    }
}

impl DumpCheckboxes {
    pub(super) fn load() -> Result<Self, String> {
        Self::load_at(&settings_path()?)
    }

    fn load_at(path: &Path) -> Result<Self, String> {
        let mut fields = read_settings(path)?.unwrap_or_default();
        let Some(saved) = fields.remove(DUMP_CHECKBOXES) else {
            return Ok(Self::default());
        };
        serde_json::from_value(saved).map_err(|error| format!("转储选项格式错误。{error}"))
    }

    pub(super) fn save(self) -> Result<(), String> {
        self.save_at(&settings_path()?)
    }

    fn save_at(self, path: &Path) -> Result<(), String> {
        update_settings(path, |fields| {
            let value = serde_json::to_value(self).map_err(|error| error.to_string())?;
            fields.insert(DUMP_CHECKBOXES.into(), value);
            Ok(())
        })
    }
}

impl From<DumpCheckboxes> for DumpSettings {
    fn from(choices: DumpCheckboxes) -> Self {
        Self {
            options: DumpOptions {
                drop_existing: choices.drop_existing,
                ..DumpOptions::default()
            },
            lock_source_tables: choices.lock_source_tables && !choices.single_transaction,
            flush_logs: choices.flush_logs,
            single_transaction: choices.single_transaction,
            lock_insert_tables: choices.lock_insert_tables,
            include_use: choices.include_use,
            include_create_database: choices.include_create_database,
            disable_foreign_key_checks: choices.disable_foreign_key_checks,
            bulk_insert: choices.bulk_insert,
            ..Self::default()
        }
    }
}

impl From<&DumpSettings> for DumpCheckboxes {
    fn from(settings: &DumpSettings) -> Self {
        Self {
            lock_source_tables: settings.lock_source_tables,
            flush_logs: settings.flush_logs,
            single_transaction: settings.single_transaction,
            lock_insert_tables: settings.lock_insert_tables,
            include_use: settings.include_use,
            include_create_database: settings.include_create_database,
            disable_foreign_key_checks: settings.disable_foreign_key_checks,
            bulk_insert: settings.bulk_insert,
            drop_existing: settings.options.drop_existing,
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct SavedCopyDestination {
    pub profile_key: String,
    pub schema: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct CopyChoices {
    pub include_structure: bool,
    pub include_data: bool,
    pub bulk_insert: bool,
    pub drop_existing: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination: Option<SavedCopyDestination>,
}

impl Default for CopyChoices {
    fn default() -> Self {
        Self {
            include_structure: true,
            include_data: true,
            bulk_insert: true,
            drop_existing: false,
            destination: None,
        }
    }
}

impl CopyChoices {
    pub(super) fn load() -> Result<Self, String> {
        let mut fields = read_settings(&settings_path()?)?.unwrap_or_default();
        let Some(value) = fields.remove(COPY_CHOICES) else {
            return Ok(Self::default());
        };
        serde_json::from_value(value).map_err(|error| format!("复制选项格式错误。{error}"))
    }

    pub(super) fn save(&self) -> Result<(), String> {
        update_settings(&settings_path()?, |fields| {
            let value = serde_json::to_value(self).map_err(|error| error.to_string())?;
            fields.insert(COPY_CHOICES.into(), value);
            Ok(())
        })
    }
}

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct Preferences {
    pub default_result_limit: usize,
    pub table_data_default_offset: usize,
    pub table_data_default_limit: usize,
    pub table_data_default_all: bool,
    pub refresh_table_data_on_focus: bool,
    pub copy_column_headers: bool,
    pub show_data_info: bool,
    pub remember_grid_widths: bool,
    pub truncate_headers_to_data_width: bool,
    pub execute_with_f9: bool,
    pub keep_focus_after_query: bool,
    pub confirm_tab_close: bool,
    pub workspace_maximized: bool,
    // Existing preference files keep their previous double-click behavior.
    #[serde(default)]
    pub insert_object_name_on_double_click: bool,
    pub zoom_percent: u16,
    pub profile_selects: bool,
    pub profile_options: ProfileOptions,
    pub automatic_completion: bool,
    pub completion_tags_directory: Option<PathBuf>,
    pub query_design_tab_on_connect: bool,
    pub schema_design_tab_on_connect: bool,
    pub query_design_quote_identifiers: bool,
    pub editor: EditorSettings,
    pub fonts: FontChoices,
    pub formatter: SqlFormatSettings,
    pub dump: DumpPreferences,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            default_result_limit: 2_000,
            table_data_default_offset: 0,
            table_data_default_limit: DEFAULT_TABLE_DATA_LIMIT,
            table_data_default_all: false,
            refresh_table_data_on_focus: false,
            copy_column_headers: true,
            show_data_info: true,
            remember_grid_widths: true,
            truncate_headers_to_data_width: false,
            execute_with_f9: true,
            keep_focus_after_query: true,
            confirm_tab_close: true,
            workspace_maximized: true,
            insert_object_name_on_double_click: true,
            zoom_percent: 100,
            profile_selects: false,
            profile_options: ProfileOptions::default(),
            automatic_completion: true,
            completion_tags_directory: None,
            query_design_tab_on_connect: true,
            schema_design_tab_on_connect: true,
            query_design_quote_identifiers: false,
            editor: EditorSettings::default(),
            fonts: FontChoices::default(),
            formatter: SqlFormatSettings::default(),
            dump: DumpPreferences::default(),
        }
    }
}

impl Preferences {
    pub(super) fn save_window_state(maximized: bool) -> Result<(), String> {
        Self::save_window_state_at(&settings_path()?, maximized)
    }

    fn save_window_state_at(path: &Path, maximized: bool) -> Result<(), String> {
        update_settings(path, |fields| {
            fields.insert("workspace_maximized".into(), Value::Bool(maximized));
            Ok(())
        })
    }

    pub(super) fn query_keys(&self) -> (egui::Key, egui::Key) {
        if self.execute_with_f9 {
            (egui::Key::F9, egui::Key::F5)
        } else {
            (egui::Key::F5, egui::Key::F9)
        }
    }

    pub(super) fn load() -> Result<Self, String> {
        Self::load_at(&settings_path()?)
    }

    fn load_at(path: &Path) -> Result<Self, String> {
        match read_settings(path)? {
            Some(fields) => Self::decode(&fields),
            None => Ok(Self::default()),
        }
    }

    pub(super) fn zoom_factor(&self) -> f32 {
        f32::from(self.zoom_percent) / 100.0
    }

    fn decode(fields: &Map<String, Value>) -> Result<Self, String> {
        let preferences = serde_json::from_value::<Self>(Value::Object(fields.clone()))
            .map_err(|error| format!("偏好设置内容无效：{error}"))?;
        Ok(preferences.bounded())
    }

    fn bounded(mut self) -> Self {
        self.default_result_limit = self.default_result_limit.clamp(MIN_ROWS, MAX_ROWS);
        self.table_data_default_limit = self.table_data_default_limit.max(1);
        self.zoom_percent = self.zoom_percent.clamp(MIN_ZOOM, MAX_ZOOM);
        self.editor.bound();
        self.formatter.indent_spaces = self.formatter.indent_spaces.min(MAX_FORMAT_INDENT);
        self.dump.bound();
        self
    }

    fn save_at(&self, path: &Path) -> Result<Self, String> {
        let preferences = self.clone().bounded();
        update_settings(path, |fields| {
            Self::decode(fields)?;
            let changes = serde_json::to_value(&preferences).map_err(|error| error.to_string())?;
            fields.extend(changes.as_object().expect("preferences fields").clone());
            Ok(())
        })?;
        Ok(preferences)
    }
}

fn update_settings(
    path: &Path,
    update: impl FnOnce(&mut Map<String, Value>) -> Result<(), String>,
) -> Result<(), String> {
    let directory = path.parent().ok_or("无法找到设置目录。")?;
    fs::create_dir_all(directory).map_err(|error| format!("无法创建设置目录。{error}"))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("lock"))
        .map_err(|error| format!("无法打开设置锁。{error}"))?;
    lock.try_lock()
        .map_err(|error| format!("偏好设置正在使用，请稍后重试。{error}"))?;

    let mut fields = read_settings(path)?.unwrap_or_else(|| {
        // A partial save must persist the fresh preference's default too.
        let mut fields = Map::new();
        fields.insert(
            "insert_object_name_on_double_click".into(),
            Value::Bool(Preferences::default().insert_object_name_on_double_click),
        );
        fields
    });
    update(&mut fields)?;
    fields.insert("version".into(), Value::from(VERSION));
    let bytes = serde_json::to_vec_pretty(&fields).map_err(|error| error.to_string())?;
    write_atomic(path, &bytes).map_err(|error| format!("无法保存偏好设置。{error}"))
}

fn settings_path() -> Result<PathBuf, String> {
    profile_path()
        .map(|path| path.with_file_name("preferences.json"))
        .ok_or_else(|| "无法找到本机的配置目录。".into())
}

fn read_settings(path: &Path) -> Result<Option<Map<String, Value>>, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("无法读取偏好设置：{error}")),
    };
    let fields: Map<String, Value> =
        serde_json::from_slice(&bytes).map_err(|error| format!("偏好设置文件格式错误：{error}"))?;
    if fields
        .get("version")
        .is_some_and(|version| version.as_u64() != Some(VERSION))
    {
        return Err("偏好设置文件版本不受支持。".into());
    }
    Ok(Some(fields))
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Page {
    #[default]
    General,
    Editor,
    Formatter,
    Profiler,
    Design,
}

#[derive(Default)]
pub(super) struct Dialog {
    draft: Option<Preferences>,
    error: Option<String>,
    page: Page,
    font_catalog: Option<Vec<FontFace>>,
    font_catalog_rx: Option<mpsc::Receiver<Vec<FontFace>>>,
    font_search: String,
}

impl Dialog {
    pub(super) fn open(&mut self, current: &Preferences) {
        self.draft = Some(current.clone());
        self.error = None;
        self.font_search.clear();
        if self.font_catalog_rx.is_none() {
            self.font_catalog = None;
        }
    }

    pub(super) fn active(&self) -> bool {
        self.draft.is_some()
    }

    pub(super) fn dialog(&mut self, ctx: &egui::Context) -> Option<Preferences> {
        if self.draft.is_some() && self.page == Page::Editor {
            self.poll_font_catalog(ctx);
        }
        let draft = self.draft.as_mut()?;
        let mut save = false;
        let mut cancel = false;
        let response = style::modal(ctx, egui::Id::new("preferences")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 48.0).clamp(1.0, 560.0));
            ui.heading("偏好设置");
            style::pane(egui::Margin::symmetric(8, 6))
                .corner_radius(8.0)
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal_wrapped(|ui| {
                        ui.selectable_value(&mut self.page, Page::General, "常规");
                        ui.selectable_value(&mut self.page, Page::Editor, "字体与编辑器");
                        ui.selectable_value(&mut self.page, Page::Formatter, "SQL 格式化");
                        ui.selectable_value(&mut self.page, Page::Profiler, "查询分析");
                        ui.selectable_value(&mut self.page, Page::Design, "设计工具");
                    });
                });
            ui.add_space(4.0);
            let max_height = (ctx.content_rect().height() - 190.0).max(155.0);
            egui::ScrollArea::vertical()
                .max_height(max_height)
                .show(ui, |ui| match self.page {
                    Page::General => general_fields(ui, draft),
                    Page::Editor => editor_fields(
                        ui,
                        &mut draft.editor,
                        &mut draft.fonts,
                        self.font_catalog.as_deref(),
                        &mut self.font_search,
                    ),
                    Page::Formatter => formatter_fields(ui, draft),
                    Page::Profiler => profiler_fields(ui, draft),
                    Page::Design => design_fields(ui, draft),
                });

            if let Some(error) = &self.error {
                ui.colored_label(style::ERROR, error);
            }

            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("恢复本页默认值").clicked() {
                    self.page.reset(draft);
                    self.error = None;
                }
                if ui.button("恢复全部默认值").clicked() {
                    *draft = Preferences::default();
                    self.error = None;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    save = ui.add(style::primary("保存")).clicked();
                    cancel = ui.button("取消").clicked();
                });
            });
        });
        if save {
            return match settings_path().and_then(|path| draft.save_at(&path)) {
                Ok(preferences) => {
                    self.draft = None;
                    Some(preferences)
                }
                Err(error) => {
                    self.error = Some(error);
                    None
                }
            };
        }
        if cancel || response.should_close() {
            self.draft = None;
        }
        None
    }

    fn poll_font_catalog(&mut self, ctx: &egui::Context) {
        if let Some(receiver) = &self.font_catalog_rx {
            match receiver.try_recv() {
                Ok(fonts) => self.font_catalog = Some(fonts),
                Err(mpsc::TryRecvError::Disconnected) => self.font_catalog = Some(Vec::new()),
                Err(mpsc::TryRecvError::Empty) => return,
            }
            self.font_catalog_rx = None;
        }
        if self.font_catalog.is_none() {
            let (sender, receiver) = mpsc::channel();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let _ = sender.send(system_fonts::catalog());
                ctx.request_repaint();
            });
            self.font_catalog_rx = Some(receiver);
        }
    }
}

impl Page {
    fn reset(self, draft: &mut Preferences) {
        let defaults = Preferences::default();
        match self {
            Self::General => {
                draft.default_result_limit = defaults.default_result_limit;
                draft.table_data_default_offset = defaults.table_data_default_offset;
                draft.table_data_default_limit = defaults.table_data_default_limit;
                draft.table_data_default_all = defaults.table_data_default_all;
                draft.refresh_table_data_on_focus = defaults.refresh_table_data_on_focus;
                draft.copy_column_headers = defaults.copy_column_headers;
                draft.show_data_info = defaults.show_data_info;
                draft.remember_grid_widths = defaults.remember_grid_widths;
                draft.truncate_headers_to_data_width = defaults.truncate_headers_to_data_width;
                draft.execute_with_f9 = defaults.execute_with_f9;
                draft.keep_focus_after_query = defaults.keep_focus_after_query;
                draft.confirm_tab_close = defaults.confirm_tab_close;
                draft.insert_object_name_on_double_click =
                    defaults.insert_object_name_on_double_click;
                draft.zoom_percent = defaults.zoom_percent;
                draft.automatic_completion = defaults.automatic_completion;
                draft.completion_tags_directory = defaults.completion_tags_directory;
                draft.dump = defaults.dump;
            }
            Self::Editor => {
                draft.editor = defaults.editor;
                draft.fonts = defaults.fonts;
            }
            Self::Formatter => draft.formatter = defaults.formatter,
            Self::Profiler => {
                draft.profile_selects = defaults.profile_selects;
                draft.profile_options = defaults.profile_options;
            }
            Self::Design => {
                draft.query_design_tab_on_connect = defaults.query_design_tab_on_connect;
                draft.schema_design_tab_on_connect = defaults.schema_design_tab_on_connect;
                draft.query_design_quote_identifiers = defaults.query_design_quote_identifiers;
            }
        }
    }
}

fn design_fields(ui: &mut egui::Ui, draft: &mut Preferences) {
    ui.checkbox(
        &mut draft.query_design_tab_on_connect,
        "新连接自动创建查询设计页签",
    );
    ui.checkbox(
        &mut draft.schema_design_tab_on_connect,
        "新连接自动创建架构设计页签",
    );
    ui.checkbox(
        &mut draft.query_design_quote_identifiers,
        "查询设计为标识符统一加反引号",
    );
}

fn general_fields(ui: &mut egui::Ui, draft: &mut Preferences) {
    egui::Grid::new("general-preferences")
        .num_columns(2)
        .show(ui, |ui| {
            ui.label("默认结果行数上限");
            ui.add(
                DragValue::new(&mut draft.default_result_limit)
                    .range(MIN_ROWS..=MAX_ROWS)
                    .speed(20),
            );
            ui.end_row();

            ui.label("表数据默认起始偏移");
            ui.add(DragValue::new(&mut draft.table_data_default_offset).range(0..=usize::MAX));
            ui.end_row();

            ui.label("表数据默认每页行数");
            ui.add(DragValue::new(&mut draft.table_data_default_limit).range(1..=usize::MAX));
            ui.end_row();

            ui.label("执行查询快捷键");
            ui.horizontal(|ui| {
                ui.radio_value(&mut draft.execute_with_f9, false, "F5");
                ui.radio_value(&mut draft.execute_with_f9, true, "F9");
                ui.label(format!("{} 刷新对象浏览器", draft.query_keys().1.name()));
            });
            ui.end_row();

            ui.label("界面缩放");
            ui.add(
                DragValue::new(&mut draft.zoom_percent)
                    .range(MIN_ZOOM..=MAX_ZOOM)
                    .suffix("%"),
            );
            ui.end_row();
        });
    ui.checkbox(&mut draft.copy_column_headers, "复制可见结果时包含列名");
    ui.checkbox(
        &mut draft.show_data_info,
        "显示表数据、查询结果和查询分析的信息条",
    );
    ui.checkbox(&mut draft.remember_grid_widths, "记住数据表格列宽")
        .on_hover_text(
            "拖动列标题右侧的分隔线后，其他连接和查询会按原库名、表名和列名使用相同列宽。",
        );
    ui.checkbox(
        &mut draft.truncate_headers_to_data_width,
        "按数据宽度截断列标题",
    )
    .on_hover_text("按当前显示的数据计算列宽。已记住手动宽度的列沿用手动宽度。");
    ui.checkbox(
        &mut draft.table_data_default_all,
        "打开表数据时默认显示全部",
    );
    ui.checkbox(
        &mut draft.refresh_table_data_on_focus,
        "进入表数据页时自动刷新",
    );
    ui.checkbox(
        &mut draft.keep_focus_after_query,
        "执行查询后将焦点留在 SQL 编辑器",
    )
    .on_hover_text("取消勾选后，查询结束时将焦点移到结果或消息。");
    ui.checkbox(&mut draft.confirm_tab_close, "关闭页签时提示保存修改")
        .on_hover_text(
            "适用于尚未保存为文件的查询和架构设计页。与文件关联的页签有修改时始终提示保存。",
        );
    ui.checkbox(
        &mut draft.insert_object_name_on_double_click,
        "双击对象时将名称插入 SQL 编辑器",
    )
    .on_hover_text("查询页可见时，将表、视图等对象名插入光标处。也可通过右键菜单打开表数据。");
    ui.checkbox(
        &mut draft.automatic_completion,
        "自动显示 SQL 补全与参数提示",
    )
    .on_hover_text("输入句点或左括号时显示。可随时用 Ctrl+Space 查看匹配的补全项。");
    ui.horizontal_wrapped(|ui| {
        ui.label("补全标签目录");
        if ui.button("选择目录…").clicked()
            && let Some(directory) = rfd::FileDialog::new()
                .set_title("选择补全标签保存目录")
                .pick_folder()
        {
            draft.completion_tags_directory = Some(directory);
        }
        if ui
            .add_enabled(
                draft.completion_tags_directory.is_some(),
                egui::Button::new("清除目录"),
            )
            .clicked()
        {
            draft.completion_tags_directory = None;
        }
    });
    if let Some(directory) = &draft.completion_tags_directory {
        ui.label(directory.display().to_string());
    }

    ui.separator();
    ui.strong("SQL 转储");
    let dump = &mut draft.dump;
    ui.horizontal_wrapped(|ui| {
        ui.label("批量 INSERT 上限");
        ui.checkbox(&mut dump.use_source_packet_limit, "按源库通信包大小")
            .on_hover_text("采用源库的 max_allowed_packet。");
        ui.add_enabled(
            !dump.use_source_packet_limit,
            DragValue::new(&mut dump.bulk_insert_kib)
                .range(1..=MAX_BULK_INSERT_KIB)
                .suffix(" KiB"),
        )
        .on_hover_text("也受源库通信包大小限制。");
    });
    ui.horizontal_wrapped(|ui| {
        ui.label("读取表数据");
        ui.radio_value(&mut dump.chunked, false, "连续读取")
            .on_hover_text("直连或 SSH 时，每张表只执行一次 SELECT。");
        ui.radio_value(&mut dump.chunked, true, "分批读取");
        ui.add_enabled(
            dump.chunked,
            DragValue::new(&mut dump.chunk_rows)
                .range(1..=u32::MAX)
                .suffix(" 行/批"),
        );
    });
    ui.small(format!(
        "HTTP 隧道每批最多读取 {MAX_HTTP_DUMP_CHUNK_ROWS} 行。"
    ));
}

fn formatter_fields(ui: &mut egui::Ui, draft: &mut Preferences) {
    let settings = &mut draft.formatter;
    ui.horizontal(|ui| {
        ui.label("表和字段列表");
        ui.radio_value(&mut settings.stacked, false, "同一行");
        ui.radio_value(&mut settings.stacked, true, "逐项换行");
    });
    ui.add_enabled_ui(settings.stacked, |ui| {
        ui.checkbox(&mut settings.align_aliases, "按列对齐 AS 和字段别名");
        ui.horizontal(|ui| {
            ui.label("换行位置");
            ui.radio_value(&mut settings.comma_first, true, "逗号前");
            ui.radio_value(&mut settings.comma_first, false, "逗号后");
        });
    });
    ui.horizontal(|ui| {
        ui.label("缩进");
        ui.add(DragValue::new(&mut settings.indent_spaces).range(0..=MAX_FORMAT_INDENT));
        ui.label("个空格");
    });
    ui.separator();
    ui.strong("预览");
    let sample = "SELECT o.id AS 订单号, o.total AS 金额 FROM orders o, customers c WHERE o.customer_id = c.id GROUP BY o.id, o.total ORDER BY o.id;";
    let preview = format_sql_with_settings(sample, 0..sample.len(), *settings, (false, false));
    let layout = style::sql_galley(
        ui,
        &preview,
        ui.available_width(),
        draft.editor,
        (false, false),
    );
    ui.label(layout);
}

fn editor_fields(
    ui: &mut egui::Ui,
    draft: &mut EditorSettings,
    fonts: &mut FontChoices,
    catalog: Option<&[FontFace]>,
    search: &mut String,
) {
    egui::Grid::new("editor-fonts")
        .num_columns(3)
        .show(ui, |ui| {
            for (role, font, selected) in [
                (FontRole::Editor, &mut draft.font, &mut fonts.editor),
                (
                    FontRole::History,
                    &mut draft.history_font,
                    &mut fonts.history,
                ),
                (FontRole::Blob, &mut draft.blob_font, &mut fonts.blob),
                (FontRole::Grid, &mut draft.grid_font, &mut fonts.grid),
            ] {
                font_row(ui, role, font, selected, catalog, search);
            }
        });

    ui.separator();
    ui.horizontal(|ui| {
        ui.label("制表符宽度");
        ui.add(DragValue::new(&mut draft.tab_width).range(1..=MAX_TAB_WIDTH));
        ui.radio_value(&mut draft.indent_with_spaces, false, "保留制表符");
        ui.radio_value(&mut draft.indent_with_spaces, true, "插入空格");
    });
    ui.checkbox(&mut draft.word_wrap, "SQL 编辑器和单元格文本查看器自动换行");

    ui.separator();
    egui::Grid::new("sql-colors").num_columns(5).show(ui, |ui| {
        for label in ["普通文本", "关键字", "函数", "字符串", "注释"] {
            ui.label(label);
        }
        ui.end_row();
        for color in [
            &mut draft.colors.normal,
            &mut draft.colors.keyword,
            &mut draft.colors.function,
            &mut draft.colors.string,
            &mut draft.colors.comment,
        ] {
            ui.color_edit_button_srgb(color);
        }
        ui.end_row();
    });
    ui.horizontal_wrapped(|ui| {
        ui.label("关键字大小写");
        case_picker(ui, "keyword-case", &mut draft.keyword_case);
        ui.label("函数大小写");
        case_picker(ui, "function-case", &mut draft.function_case);
    })
    .response
    .on_hover_text("显示、复制和保存时应用此设置。执行时使用编辑器中的原始 SQL。");

    let example = "-- 查询示例\nselect\tconcat('订单 ', 编号) from 订单;";
    let layout = style::sql_galley(ui, example, ui.available_width(), *draft, (false, false));
    ui.label(layout);
}

fn font_row(
    ui: &mut egui::Ui,
    role: FontRole,
    font: &mut TextFont,
    selected: &mut Option<FontFace>,
    catalog: Option<&[FontFace]>,
    search: &mut String,
) {
    ui.label(role.label());
    let label = selected
        .as_ref()
        .map_or_else(|| font.label().to_owned(), FontFace::label);
    egui::ComboBox::from_id_salt(role.font_name())
        .width(185.0)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .selected_text(label)
        .show_ui(ui, |ui| {
            for (family, label) in [
                (FontFamily::Monospace, "内置等宽"),
                (FontFamily::Proportional, "内置比例"),
            ] {
                if ui
                    .selectable_label(selected.is_none() && font.family == family, label)
                    .clicked()
                {
                    *selected = None;
                    font.family = family;
                    font.system_role = None;
                    ui.close();
                }
            }
            ui.separator();
            match catalog {
                None => {
                    ui.label("正在读取系统字体…");
                }
                Some([]) => {
                    ui.label("未找到可用系统字体。");
                }
                Some(fonts) => {
                    ui.add(egui::TextEdit::singleline(search).hint_text("搜索字体或字形"));
                    let needle = search.trim().to_lowercase();
                    let matching = fonts
                        .iter()
                        .filter(|face| {
                            needle.is_empty() || face.label().to_lowercase().contains(&needle)
                        })
                        .collect::<Vec<_>>();
                    if matching.is_empty() {
                        ui.label("未找到匹配的字体。");
                    }
                    let row_height = ui.spacing().interact_size.y;
                    egui::ScrollArea::vertical()
                        .id_salt(role.font_name())
                        .max_height(245.0)
                        .show_rows(ui, row_height, matching.len(), |ui, rows| {
                            for face in &matching[rows] {
                                if ui
                                    .selectable_label(
                                        selected.as_ref() == Some(*face),
                                        face.label(),
                                    )
                                    .clicked()
                                {
                                    *selected = Some((**face).clone());
                                    ui.close();
                                }
                            }
                        });
                }
            }
        });
    ui.add(DragValue::new(&mut font.size).range(MIN_FONT_SIZE..=MAX_FONT_SIZE));
    ui.end_row();
}

fn case_picker(ui: &mut egui::Ui, id: &str, value: &mut LetterCase) {
    egui::ComboBox::from_id_salt(id)
        .width(76.0)
        .selected_text(value.label())
        .show_ui(ui, |ui| {
            for case in [LetterCase::AsTyped, LetterCase::Upper, LetterCase::Lower] {
                ui.selectable_value(value, case, case.label());
            }
        });
}

fn profiler_fields(ui: &mut egui::Ui, draft: &mut Preferences) {
    ui.checkbox(&mut draft.profile_selects, "执行单条 SELECT 时自动分析")
        .on_hover_text("查询分析会读取当前会话的统计信息。需要在下一条语句中读取 FOUND_ROWS() 或 SHOW WARNINGS 时，请关闭自动分析。");
    ui.checkbox(&mut draft.profile_options.explain, "执行计划（EXPLAIN）");
    ui.add_enabled(
        draft.profile_options.explain,
        egui::Checkbox::new(&mut draft.profile_options.extended, "显示优化器改写和提示"),
    );
    ui.checkbox(&mut draft.profile_options.status, "会话与全局状态");
    ui.checkbox(
        &mut draft.profile_options.stages,
        "服务器执行阶段（SHOW PROFILE）",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_click_preference_distinguishes_fresh_and_existing_settings() {
        let executable = std::env::current_exe().unwrap();
        let directory = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let path = directory.path().join("preferences.json");
        assert!(
            Preferences::load_at(&path)
                .unwrap()
                .insert_object_name_on_double_click
        );
        DumpCheckboxes::default().save_at(&path).unwrap();
        assert!(
            Preferences::load_at(&path)
                .unwrap()
                .insert_object_name_on_double_click
        );

        fs::write(&path, r#"{"version":1,"zoom_percent":110}"#).unwrap();
        let mut migrated = Preferences::load_at(&path).unwrap();
        assert!(!migrated.insert_object_name_on_double_click);
        assert_eq!(migrated.zoom_percent, 110);
        migrated.insert_object_name_on_double_click = true;
        migrated.save_at(&path).unwrap();
        assert!(
            Preferences::load_at(&path)
                .unwrap()
                .insert_object_name_on_double_click
        );
    }

    #[test]
    fn remembers_dump_checkboxes_alongside_general_preferences() {
        let executable = std::env::current_exe().unwrap();
        let directory = tempfile::Builder::new()
            .prefix("harbor-dump-checkboxes-")
            .tempdir_in(executable.parent().unwrap())
            .unwrap();
        let path = directory.path().join("preferences.json");
        let preferences = Preferences {
            dump: DumpPreferences {
                chunked: true,
                chunk_rows: 250,
                ..DumpPreferences::default()
            },
            ..Preferences::default()
        };
        preferences.save_at(&path).unwrap();

        let mut settings: DumpSettings = DumpCheckboxes::load_at(&path).unwrap().into();
        assert!(settings.include_use);
        assert!(settings.include_create_database);
        assert!(settings.disable_foreign_key_checks);
        assert!(settings.bulk_insert);
        assert!(settings.options.drop_existing);
        assert!(!settings.single_transaction);
        assert!(!settings.flush_logs);
        assert!(!settings.lock_source_tables);
        assert!(!settings.lock_insert_tables);

        settings.single_transaction = true;
        settings.lock_insert_tables = true;
        settings.include_use = false;
        settings.include_create_database = false;
        settings.bulk_insert = false;
        let chosen = DumpCheckboxes::from(&settings);
        chosen.save_at(&path).unwrap();
        assert!(Preferences::load_at(&path).unwrap() == preferences);

        Preferences {
            zoom_percent: 110,
            ..preferences
        }
        .save_at(&path)
        .unwrap();
        let restored = DumpCheckboxes::load_at(&path).unwrap();
        let restored_settings: DumpSettings = restored.into();
        assert_eq!(DumpCheckboxes::from(&restored_settings), chosen);
    }

    #[test]
    fn loads_older_partial_preferences_and_bounds_them() {
        let fields =
            serde_json::from_str(r#"{"default_result_limit":20000,"zoom_percent":10}"#).unwrap();
        let preferences = Preferences::decode(&fields).unwrap();
        assert_eq!(preferences.default_result_limit, MAX_ROWS);
        assert_eq!(preferences.table_data_default_offset, 0);
        assert_eq!(preferences.table_data_default_limit, 50);
        assert!(!preferences.table_data_default_all);
        assert!(!preferences.refresh_table_data_on_focus);
        assert_eq!(preferences.zoom_percent, MIN_ZOOM);
        assert!(preferences.copy_column_headers);
        assert!(preferences.show_data_info);
        assert!(preferences.remember_grid_widths);
        assert!(!preferences.truncate_headers_to_data_width);
        assert!(preferences.execute_with_f9);
        assert!(preferences.keep_focus_after_query);
        assert!(preferences.confirm_tab_close);
        assert!(!preferences.insert_object_name_on_double_click);
        assert_eq!(preferences.editor, EditorSettings::default());
        assert!(preferences.dump.use_source_packet_limit);
        assert_eq!(preferences.dump.bulk_insert_kib, 1);
        assert!(!preferences.dump.chunked);
        assert_eq!(preferences.dump.chunk_rows, 1_000);
    }

    #[test]
    fn saves_preferences_atomically_and_keeps_existing_fields() {
        let executable = std::env::current_exe().unwrap();
        let directory = tempfile::Builder::new()
            .prefix("harbor-preferences-")
            .tempdir_in(executable.parent().unwrap())
            .unwrap();
        let path = directory.path().join("preferences.json");
        let previous = r#"{"other_preference":"kept"}"#;
        fs::write(&path, previous).unwrap();

        let settings = Preferences {
            default_result_limit: 800,
            table_data_default_offset: 12,
            table_data_default_limit: 75,
            table_data_default_all: true,
            refresh_table_data_on_focus: true,
            execute_with_f9: true,
            keep_focus_after_query: false,
            confirm_tab_close: false,
            workspace_maximized: false,
            insert_object_name_on_double_click: false,
            zoom_percent: 110,
            copy_column_headers: false,
            show_data_info: false,
            remember_grid_widths: false,
            truncate_headers_to_data_width: true,
            profile_selects: true,
            automatic_completion: false,
            completion_tags_directory: Some(directory.path().join("补全标签")),
            query_design_tab_on_connect: false,
            schema_design_tab_on_connect: false,
            query_design_quote_identifiers: true,
            profile_options: ProfileOptions {
                extended: false,
                ..ProfileOptions::default()
            },
            editor: EditorSettings {
                font: TextFont {
                    family: FontFamily::Proportional,
                    size: 20,
                    system_role: None,
                },
                colors: SqlColors {
                    keyword: [22, 75, 116],
                    ..SqlColors::default()
                },
                keyword_case: LetterCase::Upper,
                tab_width: 6,
                indent_with_spaces: true,
                word_wrap: false,
                ..EditorSettings::default()
            },
            formatter: SqlFormatSettings {
                comma_first: true,
                indent_spaces: 4,
                ..SqlFormatSettings::default()
            },
            dump: DumpPreferences {
                use_source_packet_limit: false,
                bulk_insert_kib: 32,
                chunked: true,
                chunk_rows: 200,
            },
            fonts: FontChoices::default(),
        };
        assert!(settings.save_at(&path).unwrap() == settings);
        let stored = read_settings(&path).unwrap().unwrap();
        assert_eq!(stored["refresh_table_data_on_focus"], true);
        assert_eq!(stored["workspace_maximized"], false);
        let mut reloaded = Preferences::decode(&stored).unwrap();
        assert!(reloaded == settings);
        assert_eq!(
            reloaded.dump.bulk_insert_limit_kib().map(NonZeroU32::get),
            Some(32)
        );
        assert_eq!(reloaded.dump.chunk_rows().map(NonZeroU32::get), Some(200));
        assert_eq!(stored["other_preference"], "kept");
        Page::General.reset(&mut reloaded);
        assert!(reloaded.show_data_info);
        assert!(!reloaded.refresh_table_data_on_focus);

        Preferences::save_window_state_at(&path, true).unwrap();
        let restored = read_settings(&path).unwrap().unwrap();
        assert_eq!(restored["workspace_maximized"], true);
        assert_eq!(restored["other_preference"], "kept");
        assert_eq!(restored["refresh_table_data_on_focus"], true);

        let future = r#"{"version":2,"future_setting":"kept"}"#;
        fs::write(&path, future).unwrap();
        assert!(settings.save_at(&path).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), future);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires the installed Windows typefaces"]
    fn system_font_preferences_render_each_role_after_reload() {
        let catalog = system_fonts::catalog();
        let from_file = |name: &str, index: u32| {
            catalog
                .iter()
                .find(|face| {
                    face.index == index
                        && face
                            .path
                            .file_name()
                            .is_some_and(|value| value.eq_ignore_ascii_case(name))
                })
                .unwrap_or_else(|| panic!("missing installed font {name}, face {index}"))
                .clone()
        };
        let choices = FontChoices {
            editor: Some(from_file("consola.ttf", 0)),
            history: Some(from_file("arial.ttf", 0)),
            blob: Some(from_file("ariali.ttf", 0)),
            grid: Some(from_file("msyh.ttc", 1)),
        };
        let executable = std::env::current_exe().unwrap();
        let directory = tempfile::Builder::new()
            .prefix("harbor-system-fonts-")
            .tempdir_in(executable.parent().unwrap())
            .unwrap();
        let path = directory.path().join("preferences.json");
        let mut preferences = Preferences {
            fonts: choices.clone(),
            ..Preferences::default()
        };
        preferences.editor.grid_font.size = 30;
        preferences.save_at(&path).unwrap();
        let mut reloaded = Preferences::load_at(&path).unwrap();
        assert!(reloaded.fonts == choices);

        let ctx = egui::Context::default();
        style::apply(&ctx);
        assert!(style::apply_font_preferences(&ctx, &mut reloaded).is_empty());
        let set = crate::database::ResultSet {
            source_sql: None,
            columns: vec!["项目".into()],
            column_hints: Vec::new(),
            column_origins: Vec::new(),
            excel_types: Vec::new(),
            export_columns: Vec::new(),
            duplicate_source: None,
            notice: None,
            rows: vec![vec![crate::database::Cell::Text("表格字体实测".into())]],
            affected_rows: 0,
            last_insert_id: None,
            truncated: false,
        };
        let mut view = super::super::output::ResultGrid::default();
        let mut widths = GridWidths::new(false);
        widths.set_data_sizing(true);
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(930.0, 660.0),
                )),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    for (role, font, face) in [
                        (
                            FontRole::Editor,
                            reloaded.editor.font,
                            choices.editor.as_ref().unwrap(),
                        ),
                        (
                            FontRole::History,
                            reloaded.editor.history_font,
                            choices.history.as_ref().unwrap(),
                        ),
                        (
                            FontRole::Blob,
                            reloaded.editor.blob_font,
                            choices.blob.as_ref().unwrap(),
                        ),
                        (
                            FontRole::Grid,
                            reloaded.editor.grid_font,
                            choices.grid.as_ref().unwrap(),
                        ),
                    ] {
                        let id = font.id();
                        ui.fonts_mut(|fonts| {
                            let definitions = fonts.definitions();
                            assert_eq!(definitions.families[&id.family][0], role.font_name());
                            assert_eq!(definitions.font_data[role.font_name()].index, face.index);
                            assert!(fonts.glyph_width(&id, 'W') > 0.0);
                        });
                    }
                    assert!(style::grid_row_height(ui, 25.0) > 25.0);
                    let sql = style::sql_galley(
                        ui,
                        "SELECT width",
                        400.0,
                        reloaded.editor,
                        (false, false),
                    );
                    ui.label(sql);
                    view.show(ui, &set, false, false, reloaded.editor, &mut widths);
                });
            },
        );
        fn draws(shape: &egui::Shape, text: &str, font: &egui::FontId) -> bool {
            match shape {
                egui::Shape::Text(shape) => {
                    shape.galley.text() == text
                        && shape
                            .galley
                            .job
                            .sections
                            .iter()
                            .all(|section| &section.format.font_id == font)
                }
                egui::Shape::Vec(shapes) => shapes.iter().any(|shape| draws(shape, text, font)),
                _ => false,
            }
        }
        for (text, font) in [
            ("SELECT width", reloaded.editor.font.id()),
            ("表格字体实测", reloaded.editor.grid_font.id()),
        ] {
            assert!(
                output
                    .shapes
                    .iter()
                    .any(|shape| draws(&shape.shape, text, &font)),
                "{text}"
            );
        }

        Page::Editor.reset(&mut reloaded);
        reloaded.save_at(&path).unwrap();
        assert!(Preferences::load_at(&path).unwrap().fonts == FontChoices::default());
    }
}
