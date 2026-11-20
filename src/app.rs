mod admin;
mod alter_table;
mod cell_value;
mod compare;
mod completion;
mod copy;
mod create_table;
mod data_sync;
mod design_tabs;
mod editor_tools;
mod error_details;
mod favorites;
mod help;
mod io_jobs;
mod job_manager;
mod navigation;
mod objects;
mod odbc_file_dsn;
mod odbc_import;
mod output;
mod preferences;
mod processes;
mod profile_import;
mod profiler;
mod query_builder;
mod recent_files;
mod row_editor;
mod schedule;
mod schema_designer;
mod server_values;
mod sql_templates;
mod state;
mod style;
mod system_fonts;
mod table_keys;
mod table_maintenance;
mod text_search;
mod tools;
mod workspace_windows;

use std::{
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use eframe::egui::{self, Align, Button, Key, Layout, Modifiers, RichText, TextEdit};

use crate::database::{
    Command, ConnectionProbe, DbObject, HTTP_CONTENT_TYPES, ObjectKind, Response, Worker,
};
use crate::maintenance::ServerView;
use crate::sql_tools::{is_single_profile_select, split_script, statement_at_for_execution};
use navigation::Document;
use recent_files::{Kind as RecentKind, RecentFiles};
use state::{BrowseAction, Profile, Workspace};
use tools::Import;

fn read_sql_file(path: &Path) -> std::io::Result<String> {
    use std::io::{Error, ErrorKind};

    const LIMIT: u64 = 8 * 1024 * 1024;
    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(Error::new(ErrorKind::InvalidInput, "请选择普通 SQL 文件。"));
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIMIT {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "SQL 编辑器可打开的文件上限为 8 MiB。",
        ));
    }
    let mut sql =
        String::from_utf8(bytes).map_err(|error| Error::new(ErrorKind::InvalidData, error))?;
    if sql.starts_with('\u{feff}') {
        sql.drain(..'\u{feff}'.len_utf8());
    }
    Ok(sql)
}

#[cfg(test)]
#[test]
fn opens_utf8_sql_in_the_same_query_slot() {
    let executable = std::env::current_exe().unwrap();
    let fixture = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
    let path = fixture.path().join("查询.sql");
    std::fs::write(&path, "\u{feff}SELECT '港湾';").unwrap();

    let mut workspace = Workspace::new("测试工作区".into(), None);
    workspace.new_tab(String::from("SELECT 0"), None);
    let original_tab = workspace.tab().id;
    let tabs = workspace.tabs.len();
    assert!(workspace.replace_sql_file(original_tab, read_sql_file(&path).unwrap(), path.clone(),));
    assert_eq!(workspace.tabs.len(), tabs);
    assert_eq!(workspace.tab().path.as_deref(), Some(path.as_path()));
    assert_eq!(workspace.tab().sql, "SELECT '港湾';");
    assert_eq!(workspace.tab().sql, workspace.tab().saved);
}

struct ConnectionForm {
    profile: Profile,
    password: String,
    web_password: String,
    proxy_password: String,
    ssh_password: String,
    ssh_key_passphrase: String,
    init_sql: String,
    test_worker: Option<Worker>,
    test_result: Option<ConnectionProbe>,
    saved: Option<usize>,
    edit_existing: Option<usize>,
    error: String,
    notice: String,
}

impl ConnectionForm {
    fn reset_private(&mut self) {
        self.password.clear();
        self.web_password.clear();
        self.proxy_password.clear();
        self.ssh_password.clear();
        self.ssh_key_passphrase.clear();
        self.init_sql.clear();
        self.test_result = None;
        self.error.clear();
        self.notice.clear();
    }

    fn load_profile(&mut self, profile: Profile, saved: Option<usize>) {
        self.password = profile.password.clone();
        self.ssh_password = profile.ssh_password.clone();
        self.ssh_key_passphrase = profile.ssh_key_passphrase.clone();
        self.web_password = profile
            .http
            .as_ref()
            .map(|http| http.web_password.clone())
            .unwrap_or_default();
        self.proxy_password = profile
            .http
            .as_ref()
            .map(|http| http.proxy_password.clone())
            .unwrap_or_default();
        self.profile = profile;
        self.saved = saved;
        self.init_sql.clear();
        self.test_result = None;
        self.error.clear();
        self.notice.clear();
    }

    fn sync_passwords_to_profile(&mut self) {
        self.profile.password = self.password.clone();
        self.profile.ssh_password = self.ssh_password.clone();
        self.profile.ssh_key_passphrase = self.ssh_key_passphrase.clone();
        if let Some(http) = &mut self.profile.http {
            http.web_password = self.web_password.clone();
            http.proxy_password = self.proxy_password.clone();
        }
    }

    fn initialization(&self) -> Result<String, String> {
        use std::io::Read;
        const LIMIT: u64 = 1024 * 1024;
        let path = self.profile.advanced.init_file.trim();
        if self.profile.http.is_some() {
            return Ok(String::new());
        }
        if path.is_empty() {
            return Ok(self.init_sql.clone());
        }
        if !self.init_sql.trim().is_empty() {
            return Err("请选择初始化 SQL 文件或填写本次 SQL。".into());
        }
        let file =
            std::fs::File::open(path).map_err(|error| format!("无法打开初始化 SQL：{error}"))?;
        let mut bytes = Vec::new();
        file.take(LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("无法读取初始化 SQL：{error}"))?;
        if bytes.len() as u64 > LIMIT {
            return Err("初始化 SQL 文件的上限为 1 MiB。".into());
        }
        let value = String::from_utf8(bytes).map_err(|_| "初始化 SQL 文件须使用 UTF-8 编码。")?;
        Ok(value.trim_start_matches('\u{feff}').to_owned())
    }
}

enum Discard {
    Tab {
        workspace: usize,
        tab: usize,
    },
    QueryDesign(u64),
    Workspace(usize),
    OpenSql {
        workspace: usize,
        tab: u64,
        sql: String,
        path: PathBuf,
    },
    Quit,
}

struct PendingEditableQuery {
    workspace: u64,
    tab: u64,
    sql: String,
    limit: usize,
}

struct SqlTemplateRequest {
    workspace: u64,
    tab: u64,
    dialog: sql_templates::Dialog,
}

#[derive(Clone, Copy)]
struct EditMenuFocus {
    id: egui::Id,
    copy_from_output: bool,
    find_in_result: bool,
}

struct ClipboardRequest {
    target: egui::Id,
    command: egui::ViewportCommand,
}

#[derive(Clone, Copy)]
struct WorkspaceRegion {
    key: u64,
    rect: egui::Rect,
    layer: Option<egui::LayerId>,
}

impl ClipboardRequest {
    fn send(self, ctx: &egui::Context) {
        ctx.memory_mut(|memory| memory.request_focus(self.target));
        ctx.send_viewport_cmd(self.command);
        ctx.request_repaint();
    }
}

pub struct Workbench {
    brand_icon: egui::TextureHandle,
    profiles: Vec<Profile>,
    profiles_readable: bool,
    workspaces: Vec<Workspace>,
    active: usize,
    visible_workspace: Option<u64>,
    workspace_windows: workspace_windows::WorkspaceWindows,
    workspace_regions: Vec<WorkspaceRegion>,
    icon_revision: u64,
    visible_document: Option<(u64, navigation::Document)>,
    query_rename: Option<navigation::RenameQuery>,
    connection: Option<ConnectionForm>,
    discard: Option<Discard>,
    import: Option<Import>,
    admin: Option<admin::Admin>,
    processes: Option<processes::Dialog>,
    server_values: Option<server_values::Dialog>,
    sql_template: Option<SqlTemplateRequest>,
    compare: Option<compare::Compare>,
    object_plan: Option<objects::Plan>,
    create_table: Option<create_table::Designer>,
    alter_table: Option<alter_table::Editor>,
    row_editor: Option<row_editor::RowEditor>,
    visible_data_tab: Option<(u64, u64)>,
    transfer: Option<io_jobs::Form>,
    odbc_import: Option<odbc_import::Import>,
    copy: Option<copy::Copy>,
    copy_destination: Option<copy::Destination>,
    data_sync: Option<data_sync::DataSync>,
    schedule: schedule::Schedule,
    job_manager: job_manager::Manager,
    design_tabs: design_tabs::Tabs,
    canvas_table_origin: Option<u64>,
    table_keys: Option<table_keys::Manager>,
    table_maintenance: Option<table_maintenance::Dialog>,
    editor_tools: editor_tools::EditorTools,
    edit_menu_focus: Option<EditMenuFocus>,
    completion: completion::Completion,
    favorites: favorites::Favorites,
    recent_files: RecentFiles,
    preferences: preferences::Preferences,
    grid_widths: preferences::GridWidths,
    preferences_dialog: preferences::Dialog,
    help: Option<help::Page>,
    show_explorer: bool,
    show_editor: bool,
    show_output: bool,
    quitting: bool,
}

#[derive(Clone, Copy)]
enum Edit {
    Current,
    Batch,
    Selection,
    Editable,
    RefreshExplorer,
    OpenTableData,
    ExportDisplayedRows,
    Explain,
    Profile,
    Text(editor_tools::Action),
    SqlTemplate,
    FindResult { next: bool },
    ArrangeWorkspaces(workspace_windows::Arrangement),
    ArrangeWorkspaceIcons,
    MinimizeWorkspace,
    SelectWorkspace(usize),
    Complete(completion::Mode),
    AddFavorite,
    Open,
    OpenSame,
    OpenSchemaDesign,
    Save,
    SaveAs,
    New,
    NewQueryDesign,
    NewSchemaDesign,
    OpenQueryDesign,
    ShowHistory,
    TableDiagnostics,
    FlushServer,
    NewConnection,
    CloneConnection,
    DisconnectConnection,
    DisconnectAll,
    ObjectFile(ObjectFileAction),
    Navigate(navigation::Action),
}

#[derive(Clone, Copy)]
enum ObjectFileAction {
    Dump,
    Restore,
    Schedule,
    ImportText,
    SchemaHtml,
    Export,
}

impl ObjectFileAction {
    fn resolve(self, workspace: &Workspace) -> Result<BrowseAction, &'static str> {
        let server = workspace.server.as_ref();
        if server.is_none() && !matches!(self, Self::Schedule) {
            return Err("请先连接数据库服务器。");
        }
        let active_schema = server.and_then(|server| server.database.clone());
        let selected = workspace.selected.as_ref().filter(|_| server.is_some());
        let schema = || {
            selected
                .map(|object| object.schema.clone())
                .or(active_schema.clone())
        };
        let object = || {
            selected
                .filter(|object| matches!(object.kind, ObjectKind::Table | ObjectKind::View))
                .cloned()
        };
        Ok(match self {
            Self::Dump => BrowseAction::Backup {
                schema: schema().unwrap_or_default(),
                object: object(),
            },
            Self::Restore => BrowseAction::Restore(active_schema.unwrap_or_default()),
            Self::Schedule => BrowseAction::Schedule {
                schema: schema().unwrap_or_default(),
                object: object(),
            },
            Self::ImportText => BrowseAction::Import(
                selected
                    .filter(|object| object.kind == ObjectKind::Table)
                    .cloned()
                    .ok_or("请先在对象树中选择要导入的表。")?,
            ),
            Self::SchemaHtml => {
                BrowseAction::SchemaHtml(schema().ok_or("请先选择数据库或库中的对象。")?)
            }
            Self::Export => {
                BrowseAction::ExportPreview(object().ok_or("请先在对象浏览器中选择表或视图。")?)
            }
        })
    }
}

const CONNECTION_SHORTCUTS: [(Key, Modifiers, Edit); 5] = [
    (Key::M, Modifiers::CTRL, Edit::NewConnection),
    (Key::N, Modifiers::CTRL, Edit::CloneConnection),
    (Key::F4, Modifiers::CTRL, Edit::DisconnectConnection),
    (Key::W, Modifiers::CTRL, Edit::DisconnectConnection),
    (
        Key::F,
        Modifiers::CTRL.plus(Modifiers::ALT),
        Edit::FlushServer,
    ),
];

const FORMAT_SHORTCUTS: [(Key, Modifiers, Edit); 3] = [
    (
        Key::F12,
        Modifiers::NONE,
        Edit::Text(editor_tools::Action::FormatCurrent),
    ),
    (
        Key::F12,
        Modifiers::CTRL,
        Edit::Text(editor_tools::Action::FormatSelection),
    ),
    (
        Key::F12,
        Modifiers::SHIFT,
        Edit::Text(editor_tools::Action::FormatAll),
    ),
];

const QUERY_DESIGN_SHORTCUTS: [(Key, Modifiers, Edit); 1] =
    [(Key::K, Modifiers::CTRL, Edit::NewQueryDesign)];

const OBJECT_FILE_SHORTCUTS: [(Key, Modifiers, Edit); 7] = [
    (
        Key::E,
        Modifiers::CTRL.plus(Modifiers::SHIFT),
        Edit::ExportDisplayedRows,
    ),
    (
        Key::C,
        Modifiers::CTRL.plus(Modifiers::ALT),
        Edit::ObjectFile(ObjectFileAction::Export),
    ),
    (
        Key::E,
        Modifiers::CTRL.plus(Modifiers::ALT),
        Edit::ObjectFile(ObjectFileAction::Dump),
    ),
    (
        Key::Q,
        Modifiers::CTRL.plus(Modifiers::SHIFT),
        Edit::ObjectFile(ObjectFileAction::Restore),
    ),
    (
        Key::S,
        Modifiers::CTRL.plus(Modifiers::ALT),
        Edit::ObjectFile(ObjectFileAction::Schedule),
    ),
    (
        Key::M,
        Modifiers::CTRL.plus(Modifiers::SHIFT),
        Edit::ObjectFile(ObjectFileAction::ImportText),
    ),
    (
        Key::S,
        Modifiers::CTRL.plus(Modifiers::SHIFT).plus(Modifiers::ALT),
        Edit::ObjectFile(ObjectFileAction::SchemaHtml),
    ),
];

fn can_export_displayed_rows(workspace: &Workspace) -> bool {
    if workspace.busy() {
        return false;
    }
    let tab = workspace.tab();
    match workspace.output {
        state::Output::Data => tab
            .data_editor
            .as_ref()
            .is_some_and(row_editor::RowEditor::can_export_loaded),
        state::Output::Results => tab.editable_editor.as_ref().map_or_else(
            || {
                tab.report
                    .as_ref()
                    .and_then(|report| report.sets.get(tab.set))
                    .is_some_and(|set| !set.columns.is_empty())
            },
            row_editor::RowEditor::can_export_loaded,
        ),
        _ => false,
    }
}

fn result_text_has_focus(workspace: &Workspace, ctx: &egui::Context) -> bool {
    match workspace.output {
        state::Output::Results => workspace.tab().editable_editor.as_ref().map_or_else(
            || {
                workspace.result_mode() == output::ResultMode::Text
                    && workspace
                        .tab()
                        .result_views
                        .text_has_focus(ctx, workspace.tab().set)
            },
            |editor| editor.text_has_focus(ctx),
        ),
        state::Output::Data => workspace
            .tab()
            .data_editor
            .as_ref()
            .is_some_and(|editor| editor.text_has_focus(ctx)),
        _ => false,
    }
}

fn request_displayed_rows_export(workspace: &mut Workspace) -> bool {
    let request = |_: &mut Workspace, editor: &mut Option<row_editor::RowEditor>| {
        editor
            .as_mut()
            .is_some_and(|editor| editor.request_export(crate::database::transfer::ExportKind::Csv))
    };
    match workspace.output {
        state::Output::Data => workspace.with_data_editor(request),
        state::Output::Results if workspace.tab().editable_editor.is_some() => {
            workspace.with_editable_editor(request)
        }
        state::Output::Results => output::request_current_result_export(workspace),
        _ => false,
    }
}

#[cfg(test)]
#[test]
fn object_file_actions_use_the_selected_table_and_current_restore_database() {
    let mut workspace = Workspace::new("验收".into(), None);
    workspace.server = Some(crate::database::ServerInfo {
        version: "11.4.13-MariaDB".into(),
        connection_id: 27,
        database: Some("默认库".into()),
        http_tunnel: false,
    });
    let table = crate::database::DbObject {
        schema: "选择的库".into(),
        name: "资料".into(),
        kind: ObjectKind::Table,
    };
    workspace.selected = Some(table.clone());
    assert!(
        matches!(ObjectFileAction::Export.resolve(&workspace), Ok(BrowseAction::ExportPreview(object)) if object == table)
    );
    for action in [ObjectFileAction::Dump, ObjectFileAction::Schedule] {
        assert!(matches!(
            action.resolve(&workspace),
            Ok(BrowseAction::Backup { schema, object: Some(object) }
                | BrowseAction::Schedule { schema, object: Some(object) })
                if schema == table.schema && object == table
        ));
    }
    assert!(matches!(
        ObjectFileAction::Restore.resolve(&workspace),
        Ok(BrowseAction::Restore(schema)) if schema == "默认库"
    ));
    assert!(matches!(
        ObjectFileAction::ImportText.resolve(&workspace),
        Ok(BrowseAction::Import(object)) if object == table
    ));
    assert!(matches!(
        ObjectFileAction::SchemaHtml.resolve(&workspace),
        Ok(BrowseAction::SchemaHtml(schema)) if schema == table.schema
    ));
}

fn consume_exact_shortcut(
    input: &mut egui::InputState,
    bindings: &[(Key, Modifiers, Edit)],
) -> Option<Edit> {
    let (index, action) = input.events.iter().enumerate().find_map(|(index, event)| {
        let egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } = event
        else {
            return None;
        };
        bindings
            .iter()
            .find(|(candidate, pattern, _)| key == candidate && modifiers.matches_exact(*pattern))
            .map(|(_, _, action)| (index, *action))
    })?;
    input.events.remove(index);
    Some(action)
}

fn consume_result_mode_shortcut(
    input: &mut egui::InputState,
    current: output::ResultMode,
    last_structured: output::ResultMode,
    allow_form: bool,
) -> Option<output::ResultMode> {
    use output::ResultMode::{Form, Grid, Text};

    let (index, mode) = input.events.iter().enumerate().find_map(|(index, event)| {
        let egui::Event::Key {
            key: Key::L,
            pressed: true,
            repeat: false,
            modifiers,
            ..
        } = event
        else {
            return None;
        };

        let mode = if modifiers.matches_exact(Modifiers::COMMAND) {
            match current {
                Grid | Form => Text,
                Text if allow_form && last_structured == Form => Form,
                Text => Grid,
            }
        } else if allow_form && modifiers.matches_exact(Modifiers::COMMAND | Modifiers::ALT) {
            match current {
                Form => Grid,
                Text if last_structured == Form => Grid,
                Grid | Text => Form,
            }
        } else {
            return None;
        };
        Some((index, mode))
    })?;
    input.events.remove(index);
    Some(mode)
}

fn consume_query_shortcut(
    input: &mut egui::InputState,
    (execute, refresh): (Key, Key),
    in_query_tab: bool,
) -> Option<Edit> {
    let (index, edit) = input.events.iter().enumerate().find_map(|(index, event)| {
        let egui::Event::Key {
            key,
            pressed: true,
            repeat: false,
            modifiers,
            ..
        } = event
        else {
            return None;
        };
        let plain = modifiers.matches_exact(Modifiers::NONE);
        let action = match *key {
            key if key == refresh && plain => Edit::RefreshExplorer,
            key if in_query_tab && key == execute && plain => Edit::Current,
            key if in_query_tab && key == execute && modifiers.matches_exact(Modifiers::SHIFT) => {
                Edit::Batch
            }
            key if in_query_tab && key == execute && modifiers.matches_exact(Modifiers::CTRL) => {
                Edit::Selection
            }
            _ => return None,
        };
        Some((index, action))
    })?;
    input.events.remove(index);
    Some(edit)
}

fn consume_f8(input: &mut egui::InputState) -> bool {
    let Some(index) = input.events.iter().position(|event| {
        matches!(event, egui::Event::Key { key: Key::F8, pressed: true, repeat: false, modifiers, .. }
            if modifiers.matches_exact(Modifiers::NONE))
    }) else {
        return false;
    };
    input.events.remove(index);
    true
}

#[cfg(test)]
#[test]
fn result_mode_shortcuts_return_to_the_previous_structured_view() {
    use output::ResultMode::{Form, Grid, Text};

    let mut workspace = Workspace::new("结果视图测试".into(), None);
    let command = Modifiers::COMMAND;
    let with_alt = command | Modifiers::ALT;
    for (modifiers, allow_form, expected) in [
        (command, true, Text),
        (command, true, Grid),
        (with_alt, true, Form),
        (command, true, Text),
        (command, true, Form),
        (command, true, Text),
        (with_alt, true, Grid),
        (with_alt, true, Form),
        (command, true, Text),
        (command, false, Grid),
    ] {
        let ctx = egui::Context::default();
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: Key::L,
                    physical_key: Some(Key::L),
                    pressed: true,
                    repeat: false,
                    modifiers,
                }],
                ..Default::default()
            },
            |ctx| {
                let mode = ctx.input_mut(|input| {
                    consume_result_mode_shortcut(
                        input,
                        workspace.result_mode(),
                        workspace.last_structured_result_mode(),
                        allow_form,
                    )
                });
                assert_eq!(mode, Some(expected));
                workspace.set_result_mode(expected);
            },
        );
    }

    workspace.set_result_mode(Form);
    workspace.set_result_mode(Text);
    workspace.new_tab("SELECT 2".into(), None);
    assert_eq!(workspace.result_mode(), Grid);
    assert_eq!(workspace.last_structured_result_mode(), Grid);
    workspace.active_tab = 0;
    assert_eq!(workspace.result_mode(), Text);
    assert_eq!(workspace.last_structured_result_mode(), Form);
}

#[cfg(test)]
#[test]
fn f8_toggles_on_a_new_press_and_ignores_key_repeat() {
    let ctx = egui::Context::default();
    for (pressed, consumed) in [(true, true), (true, false), (false, false), (true, true)] {
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: Key::F8,
                    physical_key: None,
                    pressed,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ctx| assert_eq!(ctx.input_mut(consume_f8), consumed),
        );
    }
}

#[cfg(test)]
#[test]
fn query_and_explorer_shortcuts_follow_the_f5_f9_preference() {
    for (execute_with_f9, execute, refresh) in [(true, Key::F9, Key::F5), (false, Key::F5, Key::F9)]
    {
        let preferences = preferences::Preferences {
            execute_with_f9,
            ..Default::default()
        };
        for (key, modifiers, in_query_tab, expected) in [
            (execute, Modifiers::NONE, true, Edit::Current),
            (execute, Modifiers::SHIFT, true, Edit::Batch),
            (execute, Modifiers::CTRL, true, Edit::Selection),
            (refresh, Modifiers::NONE, true, Edit::RefreshExplorer),
            (refresh, Modifiers::NONE, false, Edit::RefreshExplorer),
        ] {
            let ctx = egui::Context::default();
            let _ = ctx.run(
                egui::RawInput {
                    events: vec![egui::Event::Key {
                        key,
                        physical_key: Some(key),
                        pressed: true,
                        repeat: false,
                        modifiers,
                    }],
                    ..Default::default()
                },
                |ctx| {
                    let action = ctx.input_mut(|input| {
                        consume_query_shortcut(input, preferences.query_keys(), in_query_tab)
                    });
                    assert_eq!(
                        action.map(|edit| std::mem::discriminant(&edit)),
                        Some(std::mem::discriminant(&expected))
                    );
                },
            );
        }
    }
}

#[cfg(test)]
#[test]
fn edit_menu_preserves_query_focus_for_clipboard_and_document_commands() {
    fn frame(
        ctx: &egui::Context,
        app: &mut Workbench,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 740.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                let clipboard = app.top_bar(ctx);
                if app.show_output {
                    output::panel(
                        ctx,
                        &mut app.workspaces[app.active],
                        false,
                        true,
                        app.preferences.editor,
                        false,
                        &mut app.grid_widths,
                    );
                }
                app.editor(ctx, true);
                if let Some(clipboard) = clipboard {
                    clipboard.send(ctx);
                }
            },
        )
    }

    fn label_position(output: &egui::FullOutput, label: &str) -> egui::Pos2 {
        fn find(shape: &egui::Shape, label: &str) -> Option<egui::Pos2> {
            match shape {
                egui::Shape::Text(text) if text.galley.text() == label => {
                    Some(text.pos + text.galley.rect.size() / 2.0)
                }
                egui::Shape::Vec(shapes) => shapes.iter().find_map(|shape| find(shape, label)),
                _ => None,
            }
        }
        output
            .shapes
            .iter()
            .find_map(|shape| find(&shape.shape, label))
            .unwrap_or_else(|| panic!("未显示菜单项 {label}"))
    }

    fn click(ctx: &egui::Context, app: &mut Workbench, pos: egui::Pos2) -> egui::FullOutput {
        let event = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        frame(ctx, app, vec![egui::Event::PointerMoved(pos), event(true)]);
        frame(ctx, app, vec![event(false)])
    }

    fn choose(ctx: &egui::Context, app: &mut Workbench, label: &str) -> egui::FullOutput {
        let main = frame(ctx, app, Vec::new());
        click(ctx, app, label_position(&main, "编辑"));
        let menu = frame(ctx, app, Vec::new());
        click(ctx, app, label_position(&menu, label))
    }

    let ctx = egui::Context::default();
    let mut app = Workbench::new(&ctx);
    let sql = "SELECT '中文';\nSELECT 2;";
    app.ws().tab_mut().sql = sql.into();
    app.ws().focus_request = true;
    let editor = editor_tools::editor_id(app.ws());

    choose(&ctx, &mut app, "全选");
    assert_eq!(app.ws().tab().selection, Some(0..sql.len()));
    for (label, request) in [
        ("复制", egui::ViewportCommand::RequestCopy),
        ("剪切", egui::ViewportCommand::RequestCut),
    ] {
        let output = choose(&ctx, &mut app, label);
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .iter()
                .any(|cmd| std::mem::discriminant(cmd) == std::mem::discriminant(&request))
        );
        assert!(ctx.memory(|memory| memory.has_focus(editor)));
    }
    frame(&ctx, &mut app, vec![egui::Event::Cut]);
    assert!(app.ws().tab().sql.is_empty());

    let pasted = choose(&ctx, &mut app, "粘贴");
    assert!(
        pasted.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .iter()
            .any(|cmd| matches!(cmd, egui::ViewportCommand::RequestPaste))
    );
    assert!(ctx.memory(|memory| memory.has_focus(editor)));
    frame(&ctx, &mut app, vec![egui::Event::Paste(sql.into())]);
    assert_eq!(app.ws().tab().sql, sql);

    choose(&ctx, &mut app, "清除查询内容");
    assert!(app.ws().tab().sql.is_empty());
    choose(&ctx, &mut app, "撤销");
    assert_eq!(app.ws().tab().sql, sql);
    choose(&ctx, &mut app, "重做");
    assert!(app.ws().tab().sql.is_empty());

    let workspace = app.ws();
    workspace.tab_mut().report = Some(crate::database::QueryReport {
        sets: vec![crate::database::ResultSet {
            source_sql: None,
            columns: vec!["值".into()],
            column_hints: Vec::new(),
            column_origins: Vec::new(),
            excel_types: Vec::new(),
            export_columns: Vec::new(),
            duplicate_source: None,
            notice: None,
            rows: vec![vec![crate::database::Cell::Text("结果文本样例".into())]],
            affected_rows: 0,
            last_insert_id: None,
            truncated: false,
        }],
        ..Default::default()
    });
    workspace.output = state::Output::Results;
    workspace.set_result_mode(output::ResultMode::Text);
    let result = frame(&ctx, &mut app, Vec::new());
    click(&ctx, &mut app, label_position(&result, "结果文本样例"));
    assert!(result_text_has_focus(app.ws(), &ctx));
    let via_f3 = frame(
        &ctx,
        &mut app,
        vec![egui::Event::Key {
            key: Key::F3,
            physical_key: Some(Key::F3),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }],
    );
    label_position(&via_f3, "上一处");
    click(&ctx, &mut app, label_position(&via_f3, "收起"));
    assert!(result_text_has_focus(app.ws(), &ctx));
    let opened = choose(&ctx, &mut app, "查找…");
    label_position(&opened, "上一处");
    assert!(result_text_has_focus(app.ws(), &ctx));
}

#[cfg(test)]
#[test]
fn arranged_workspaces_keep_each_editor_and_result_through_minimize_and_restore() {
    use workspace_windows::Arrangement;

    fn render(
        ctx: &egui::Context,
        app: &mut Workbench,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 820.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                app.select_workspace_at_pointer(ctx);
                let _ = app.top_bar(ctx);
                if app.workspace_windows.mode != Arrangement::Maximized {
                    app.arranged_workspaces(ctx, None, false);
                }
            },
        )
    }

    fn label(shape: &egui::Shape, value: &str) -> Option<egui::Pos2> {
        match shape {
            egui::Shape::Text(text) if text.galley.text() == value => {
                Some(text.pos + text.galley.rect.size() / 2.0)
            }
            egui::Shape::Vec(shapes) => shapes.iter().find_map(|shape| label(shape, value)),
            _ => None,
        }
    }

    fn click(ctx: &egui::Context, app: &mut Workbench, pos: egui::Pos2) {
        let event = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        render(ctx, app, vec![egui::Event::PointerMoved(pos), event(true)]);
        render(ctx, app, vec![event(false)]);
    }

    fn click_label(ctx: &egui::Context, app: &mut Workbench, value: &str) {
        let output = render(ctx, app, Vec::new());
        let pos = output
            .shapes
            .iter()
            .find_map(|shape| label(&shape.shape, value))
            .unwrap_or_else(|| panic!("未显示窗口命令 {value}"));
        click(ctx, app, pos);
    }

    let ctx = egui::Context::default();
    let mut app = Workbench::new(&ctx);
    app.ws().label = "甲连接".into();
    app.workspaces.push(Workspace::new("乙连接".into(), None));
    for (workspace, (sql, message)) in app.workspaces.iter_mut().zip([
        ("SELECT '甲'", "甲的查询消息"),
        ("SELECT '乙'", "乙的查询消息"),
    ]) {
        workspace.tab_mut().sql = sql.into();
        workspace.output = state::Output::Messages;
        workspace.messages.push(message.into());
    }
    let first = app.workspaces[0].key;
    let second = app.workspaces[1].key;
    app.workspace_windows.arrange(Arrangement::Maximized);
    click_label(&ctx, &mut app, "窗口");
    click_label(&ctx, &mut app, "平铺");
    let tiled = render(&ctx, &mut app, Vec::new());
    assert_eq!(app.workspace_regions.len(), 2);
    let left = app
        .workspace_regions
        .iter()
        .find(|region| region.key == first)
        .unwrap()
        .rect;
    let right = app
        .workspace_regions
        .iter()
        .find(|region| region.key == second)
        .unwrap()
        .rect;
    assert!(left.right() < right.left());
    for message in ["甲的查询消息", "乙的查询消息"] {
        assert!(
            tiled
                .shapes
                .iter()
                .any(|shape| label(&shape.shape, message).is_some())
        );
    }
    click(
        &ctx,
        &mut app,
        egui::pos2(right.center().x, right.top() + 12.0),
    );
    assert_eq!(app.active, 1);

    click_label(&ctx, &mut app, "最小化");
    assert_eq!(app.active, 0);
    assert!(app.workspace_windows.minimized.contains(&second));
    click_label(&ctx, &mut app, "窗口");
    click_label(&ctx, &mut app, "排列图标");
    render(&ctx, &mut app, Vec::new());
    let minimized = render(&ctx, &mut app, Vec::new());
    assert_eq!(app.workspace_regions.len(), 1);
    let restore = minimized
        .shapes
        .iter()
        .find_map(|shape| label(&shape.shape, "还原"))
        .expect("最小化连接应提供还原按钮");
    click(&ctx, &mut app, restore);
    assert_eq!(app.active, 1);
    assert!(!app.workspace_windows.minimized.contains(&second));
    assert_eq!(app.ws().tab().sql, "SELECT '乙'");
    assert_eq!(app.ws().messages, ["乙的查询消息"]);

    click_label(&ctx, &mut app, "窗口");
    click_label(&ctx, &mut app, "层叠");
    render(&ctx, &mut app, Vec::new());
    render(&ctx, &mut app, Vec::new());
    assert_eq!(app.workspace_regions.len(), 2);
    assert!(
        app.workspace_regions
            .iter()
            .all(|region| region.layer.is_some())
    );
    assert!(
        app.workspace_regions[0]
            .rect
            .intersects(app.workspace_regions[1].rect)
    );
    assert_eq!(app.workspaces[0].tab().sql, "SELECT '甲'");
}

#[cfg(test)]
#[test]
fn exact_shortcuts_use_the_modifiers_at_key_press() {
    use editor_tools::Action;

    let ctx = egui::Context::default();
    for (modifiers, expected) in [
        (Modifiers::NONE, Action::FormatCurrent),
        (Modifiers::CTRL, Action::FormatSelection),
        (Modifiers::SHIFT, Action::FormatAll),
    ] {
        let _ = ctx.run(
            egui::RawInput {
                // The modifier can be released before egui draws the frame.
                modifiers: Modifiers::NONE,
                events: vec![egui::Event::Key {
                    key: Key::F12,
                    physical_key: Some(Key::F12),
                    pressed: true,
                    repeat: false,
                    modifiers,
                }],
                ..Default::default()
            },
            |ctx| {
                ctx.input_mut(|input| {
                    let Some(Edit::Text(actual)) = consume_exact_shortcut(input, &FORMAT_SHORTCUTS)
                    else {
                        panic!("缺少格式化快捷键动作。");
                    };
                    assert_eq!(
                        std::mem::discriminant(&actual),
                        std::mem::discriminant(&expected)
                    );
                    assert!(consume_exact_shortcut(input, &FORMAT_SHORTCUTS).is_none());
                });
            },
        );
    }
    let _ = ctx.run(
        egui::RawInput {
            events: vec![egui::Event::Key {
                key: Key::M,
                physical_key: Some(Key::M),
                pressed: true,
                repeat: false,
                modifiers: Modifiers::CTRL,
            }],
            ..Default::default()
        },
        |ctx| {
            assert!(matches!(
                ctx.input_mut(|input| consume_exact_shortcut(input, &CONNECTION_SHORTCUTS)),
                Some(Edit::NewConnection)
            ));
        },
    );
}

impl Workbench {
    pub fn new(ctx: &egui::Context) -> Self {
        style::apply(ctx);
        let icon = crate::application_icon();
        let brand_icon = ctx.load_texture(
            "application-icon",
            egui::ColorImage::from_rgba_unmultiplied(
                [icon.width as usize, icon.height as usize],
                &icon.rgba,
            ),
            egui::TextureOptions::LINEAR,
        );
        let mut workspace = Workspace::new("临时查询".into(), None);
        let mut preferences = match preferences::Preferences::load() {
            Ok(preferences) => preferences,
            Err(error) => {
                workspace.message(format!("无法读取偏好设置：{error}"), true);
                preferences::Preferences::default()
            }
        };
        for error in style::apply_font_preferences(ctx, &mut preferences) {
            workspace.message(error, true);
        }
        let recent_files = RecentFiles::load().unwrap_or_else(|error| {
            workspace.message(format!("无法读取最近文件：{error}"), true);
            RecentFiles::default()
        });
        let mut grid_widths = preferences::GridWidths::load(preferences.remember_grid_widths)
            .unwrap_or_else(|error| {
                workspace.message(error, true);
                preferences::GridWidths::new(preferences.remember_grid_widths)
            });
        grid_widths.set_data_sizing(preferences.truncate_headers_to_data_width);
        workspace.result_limit = preferences.default_result_limit;
        workspace.copy_column_headers = preferences.copy_column_headers;
        workspace.show_data_info = preferences.show_data_info;
        ctx.set_zoom_factor(preferences.zoom_factor());
        let (profiles, profiles_readable) = match state::load_profiles() {
            Ok(profiles) => (profiles, true),
            Err(error) => {
                workspace.message(format!("无法读取已保存的连接：{error}"), true);
                (Vec::new(), false)
            }
        };
        Self {
            brand_icon,
            profiles,
            profiles_readable,
            workspaces: vec![workspace],
            active: 0,
            visible_workspace: None,
            workspace_windows: workspace_windows::WorkspaceWindows {
                mode: if preferences.workspace_maximized {
                    workspace_windows::Arrangement::Maximized
                } else {
                    workspace_windows::Arrangement::Cascade
                },
                ..Default::default()
            },
            workspace_regions: Vec::new(),
            icon_revision: 0,
            visible_document: None,
            query_rename: None,
            connection: None,
            discard: None,
            import: None,
            admin: None,
            processes: None,
            server_values: None,
            sql_template: None,
            compare: None,
            object_plan: None,
            create_table: None,
            alter_table: None,
            row_editor: None,
            visible_data_tab: None,
            transfer: None,
            odbc_import: None,
            copy: None,
            copy_destination: None,
            data_sync: None,
            schedule: schedule::Schedule::default(),
            job_manager: job_manager::Manager::default(),
            design_tabs: design_tabs::Tabs::default(),
            canvas_table_origin: None,
            table_keys: None,
            table_maintenance: None,
            editor_tools: editor_tools::EditorTools::default(),
            edit_menu_focus: None,
            completion: completion::Completion::default(),
            favorites: favorites::Favorites::default(),
            recent_files,
            preferences,
            grid_widths,
            preferences_dialog: preferences::Dialog::default(),
            help: None,
            show_explorer: true,
            show_editor: true,
            show_output: true,
            quitting: false,
        }
    }

    fn ws(&mut self) -> &mut Workspace {
        &mut self.workspaces[self.active]
    }

    /// 桌面端启动时直接打开连接弹窗，省去手动点击“新建连接”。
    pub fn open_startup_connection(&mut self) {
        if self.connection.is_none() {
            self.new_connection(None);
        }
    }

    /// 判断是否为启动时创建的未连接默认占位工作区（尚无连接配置、未连接且非忙碌）。
    fn is_placeholder_workspace(&self, index: usize) -> bool {
        let workspace = &self.workspaces[index];
        workspace.profile.is_none()
            && workspace.server.is_none()
            && !workspace.busy()
            && workspace.label == "临时查询"
    }

    fn active_layout(&self) -> Option<Document> {
        self.design_tabs.active(self.workspaces[self.active].key)
    }

    fn open_duplicate_result(&mut self) {
        let index = self.active;
        if !self.workspaces[index].busy()
            && let Some(page) = self.workspaces[index].duplicate_result_page.take()
        {
            match row_editor::RowEditor::from_result_duplicate(index, page) {
                Ok(editor) => self.row_editor = Some(editor),
                Err(error) => self.workspaces[index].message(error, true),
            }
        }
    }

    fn new_connection(&mut self, edit: Option<usize>) {
        if let Some(index) = edit
            && self.reveal_protected_row_editor(
                index,
                "请先结束数据编辑操作，或保存、取消草稿再连接。",
            )
        {
            return;
        }
        let (profile, saved) = if let Some(index) = edit {
            let mut profile = self.workspaces[index].profile.clone().unwrap_or_default();
            let saved = self
                .profiles
                .iter()
                .position(|item| item.label == profile.label);
            if let Some(saved_index) = saved {
                let stored = &self.profiles[saved_index];
                if profile.password.is_empty() {
                    profile.password = stored.password.clone();
                }
                if profile.ssh_password.is_empty() {
                    profile.ssh_password = stored.ssh_password.clone();
                }
                if profile.ssh_key_passphrase.is_empty() {
                    profile.ssh_key_passphrase = stored.ssh_key_passphrase.clone();
                }
                if let (Some(http), Some(stored_http)) = (&mut profile.http, &stored.http) {
                    if http.web_password.is_empty() {
                        http.web_password = stored_http.web_password.clone();
                    }
                    if http.proxy_password.is_empty() {
                        http.proxy_password = stored_http.proxy_password.clone();
                    }
                }
            }
            (profile, saved)
        } else if let Some(first) = self.profiles.first() {
            (first.clone(), Some(0))
        } else {
            (Profile::default(), None)
        };
        let mut form = ConnectionForm {
            profile: Profile::default(),
            saved: None,
            password: String::new(),
            web_password: String::new(),
            proxy_password: String::new(),
            ssh_password: String::new(),
            ssh_key_passphrase: String::new(),
            init_sql: String::new(),
            test_worker: None,
            test_result: None,
            edit_existing: edit,
            error: String::new(),
            notice: String::new(),
        };
        form.load_profile(profile, saved);
        self.connection = Some(form);
    }

    fn receive_cloned_connections(&mut self) {
        let cloned: Vec<_> = self
            .workspaces
            .iter_mut()
            .enumerate()
            .filter_map(|(index, source)| {
                let (worker, request) = source.take_cloned_connection()?;
                let mut profile = source.profile.clone();
                if let Some(profile) = &mut profile
                    && profile.ssh
                    && profile.http.is_none()
                {
                    profile.ssh_local_port = None;
                }
                let mut workspace = Workspace::new(source.label.clone(), profile);
                workspace.result_limit = self.preferences.default_result_limit;
                workspace.copy_column_headers = self.preferences.copy_column_headers;
                workspace.show_data_info = self.preferences.show_data_info;
                workspace.receive_cloned_worker(worker, request);
                Some((index, workspace))
            })
            .collect();
        for (source, workspace) in cloned {
            self.workspaces.push(workspace);
            if self.active == source {
                self.active = self.workspaces.len() - 1;
            }
        }
    }

    fn disconnect_all(&mut self) {
        if let Some(index) = self
            .workspaces
            .iter()
            .position(|workspace| workspace.protected_row_editor_tab().is_some())
            && self.reveal_protected_row_editor(
                index,
                "请先结束数据编辑操作，或保存、取消草稿，再断开全部连接。",
            )
        {
            return;
        }
        if !Workspace::disconnect_all(&mut self.workspaces) {
            self.ws()
                .message("有连接正在执行任务，全部结束后可断开。", false);
        }
    }

    fn connection_dialog(&mut self, ctx: &egui::Context) {
        let Some(form) = &mut self.connection else {
            return;
        };
        let mut close = false;
        let mut connect = false;
        let mut save = false;
        let mut forget = false;
        let mut test = false;
        let mut import = false;
        if let Some(event) = form.test_worker.as_ref().and_then(Worker::try_recv) {
            form.test_worker = None;
            match event.result {
                Ok(Response::ConnectionTest(probe)) => form.test_result = Some(probe),
                Err(error) => form.error = error,
                _ => form.error = "连接测试未返回服务器信息。".to_owned(),
            }
        }
        let testing = form.test_worker.is_some();
        if testing {
            ctx.request_repaint_after(Duration::from_millis(25));
        }
        let has_feedback =
            !form.error.is_empty() || form.test_result.is_some() || !form.notice.is_empty();
        let exterior_height =
            if has_feedback { 218.0 } else { 128.0 } + if testing { 32.0 } else { 0.0 };
        style::modal(ctx, egui::Id::new("connection-dialog")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 48.0).clamp(1.0, 580.0));
            ui.horizontal(|ui| {
                ui.image((self.brand_icon.id(), egui::vec2(26.0, 26.0)));
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.heading("数据库连接");
                    ui.label(
                        RichText::new("配置 MySQL / MariaDB 服务器、SSH/HTTP 隧道与安全证书")
                            .small()
                            .color(style::muted()),
                    );
                });
            });
            ui.add_space(10.0);
            egui::ScrollArea::vertical()
                .id_salt("connection-form-body")
                .max_height((ctx.content_rect().height() - exterior_height).max(64.0))
                .show(ui, |ui| {
                    ui.add_enabled_ui(!testing, |ui| {
                        style::subtle_card(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.horizontal_wrapped(|ui| {
                                if !self.profiles.is_empty() {
                                    egui::ComboBox::from_id_salt("profiles")
                                        .selected_text(
                                            form.saved
                                                .map(|i| self.profiles[i].label.as_str())
                                                .unwrap_or("已保存的连接"),
                                        )
                                        .show_ui(ui, |ui| {
                                            if ui
                                                .selectable_label(
                                                    form.saved.is_none(),
                                                    "新建连接配置",
                                                )
                                                .clicked()
                                            {
                                                form.saved = None;
                                                form.profile = Profile::default();
                                                form.reset_private();
                                            }
                                            for (index, profile) in self.profiles.iter().enumerate()
                                            {
                                                if ui
                                                    .selectable_label(
                                                        form.saved == Some(index),
                                                        &profile.label,
                                                    )
                                                    .clicked()
                                                {
                                                    form.load_profile(profile.clone(), Some(index));
                                                }
                                            }
                                        });
                                }
                                import = ui
                                    .add_enabled(
                                        self.profiles_readable,
                                        Button::new("导入 SQLyog 连接配置…"),
                                    )
                                    .clicked();
                            });
                        });
                        ui.add_space(10.0);
                        style::card(egui::Margin::same(14)).show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.label(RichText::new("服务器").strong().color(style::ink()));
                            ui.add_space(6.0);
                            egui::Grid::new("connection-fields")
                                .num_columns(2)
                                .spacing([16.0, 9.0])
                                .show(ui, |ui| {
                                    field(ui, "连接名称", &mut form.profile.label, "开发环境");
                                    field(ui, "主机地址", &mut form.profile.host, "127.0.0.1");
                                    ui.label(RichText::new("端口").color(style::muted()));
                                    ui.add(
                                        egui::DragValue::new(&mut form.profile.port)
                                            .range(1..=65535),
                                    );
                                    ui.end_row();
                                    field(ui, "用户名", &mut form.profile.user, "root");
                                    ui.label(RichText::new("密码").color(style::muted()));
                                    ui.add(
                                        TextEdit::singleline(&mut form.password)
                                            .password(true)
                                            .desired_width(310.0),
                                    );
                                    ui.end_row();
                                    field(ui, "默认数据库", &mut form.profile.schema, "选填");
                                });
                        });
                        ui.add_space(10.0);
                        style::card(egui::Margin::same(14)).show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.label(RichText::new("传输与安全").strong().color(style::ink()));
                            ui.add_space(4.0);
                            let mut http = form.profile.http.is_some();
                            if ui
                                .checkbox(&mut http, "通过 HTTP / HTTPS 隧道连接")
                                .changed()
                            {
                                form.profile.http = http.then(state::HttpProfile::default);
                                form.web_password.clear();
                                form.proxy_password.clear();
                            }
                            if http {
                                http_connection_fields(ui, form);
                            } else {
                                ui.checkbox(&mut form.profile.tls, "启用 TLS 并验证服务器证书");
                                if form.profile.tls {
                                    ui.horizontal(|ui| {
                                        ui.add(
                                            TextEdit::singleline(&mut form.profile.ca)
                                                .desired_width(350.0)
                                                .hint_text("CA 证书文件（选填）"),
                                        );
                                        if ui.button("浏览…").clicked()
                                            && let Some(path) = rfd::FileDialog::new()
                                                .set_title("选择 CA 证书文件")
                                                .pick_file()
                                        {
                                            form.profile.ca = path.to_string_lossy().into_owned();
                                        }
                                    });
                                    ui.horizontal(|ui| {
                                        ui.add(
                                            TextEdit::singleline(&mut form.profile.client_identity)
                                                .desired_width(350.0)
                                                .hint_text("客户端证书链（选填）"),
                                        );
                                        if ui.button("浏览…").clicked()
                                            && let Some(path) = rfd::FileDialog::new()
                                                .set_title("选择客户端证书链")
                                                .add_filter(
                                                    "证书文件",
                                                    &["pem", "crt", "cer", "der"],
                                                )
                                                .pick_file()
                                        {
                                            form.profile.client_identity =
                                                path.to_string_lossy().into_owned();
                                        }
                                    });
                                    ui.horizontal(|ui| {
                                        ui.add(
                                            TextEdit::singleline(
                                                &mut form.profile.client_private_key,
                                            )
                                            .desired_width(350.0)
                                            .hint_text("客户端私钥文件（选填）"),
                                        );
                                        if ui.button("浏览…").clicked()
                                            && let Some(path) = rfd::FileDialog::new()
                                                .set_title("选择客户端私钥")
                                                .add_filter("私钥文件", &["pem", "key", "der"])
                                                .pick_file()
                                        {
                                            form.profile.client_private_key =
                                                path.to_string_lossy().into_owned();
                                        }
                                    });
                                }
                                ui.checkbox(&mut form.profile.ssh, "通过 SSH 隧道连接");
                                if form.profile.ssh {
                                    ui.label(
                                        RichText::new("SSH 隧道使用已登记的主机密钥验证服务器。")
                                            .small()
                                            .color(style::muted()),
                                    );
                                    ui.horizontal(|ui| {
                                        ui.selectable_value(
                                            &mut form.profile.ssh_password_auth,
                                            false,
                                            "系统密钥或代理",
                                        );
                                        ui.selectable_value(
                                            &mut form.profile.ssh_password_auth,
                                            true,
                                            "密码",
                                        );
                                    });
                                    egui::Grid::new("ssh-fields")
                                        .num_columns(2)
                                        .spacing([14.0, 7.0])
                                        .show(ui, |ui| {
                                            field(
                                                ui,
                                                "SSH 主机",
                                                &mut form.profile.ssh_host,
                                                "bastion.example.net",
                                            );
                                            ui.label(
                                                RichText::new("SSH 端口").color(style::muted()),
                                            );
                                            ui.add(
                                                egui::DragValue::new(&mut form.profile.ssh_port)
                                                    .range(1..=65535),
                                            );
                                            ui.end_row();
                                            field(
                                                ui,
                                                "SSH 用户名",
                                                &mut form.profile.ssh_user,
                                                if form.profile.ssh_password_auth {
                                                    "必填"
                                                } else {
                                                    "选填"
                                                },
                                            );
                                            if form.profile.ssh_password_auth {
                                                ui.label(
                                                    RichText::new("SSH 密码").color(style::muted()),
                                                );
                                                ui.add(
                                                    TextEdit::singleline(&mut form.ssh_password)
                                                        .password(true)
                                                        .desired_width(310.0),
                                                );
                                                ui.end_row();
                                            } else {
                                                ui.label(
                                                    RichText::new("私钥文件").color(style::muted()),
                                                );
                                                ui.horizontal(|ui| {
                                                    if ui
                                                        .add(
                                                            TextEdit::singleline(
                                                                &mut form.profile.ssh_key,
                                                            )
                                                            .desired_width(235.0)
                                                            .hint_text("OpenSSH 或 PuTTY .ppk"),
                                                        )
                                                        .changed()
                                                    {
                                                        form.ssh_key_passphrase.clear();
                                                    }
                                                    if ui.button("浏览…").clicked()
                                                        && let Some(path) = rfd::FileDialog::new()
                                                            .set_title("选择 SSH 私钥")
                                                            .pick_file()
                                                    {
                                                        form.profile.ssh_key =
                                                            path.to_string_lossy().into_owned();
                                                        form.ssh_key_passphrase.clear();
                                                    }
                                                });
                                                ui.end_row();
                                                if !form.profile.ssh_key.trim().is_empty() {
                                                    ui.label(
                                                        RichText::new("私钥口令")
                                                            .color(style::muted()),
                                                    );
                                                    ui.add(
                                                        TextEdit::singleline(
                                                            &mut form.ssh_key_passphrase,
                                                        )
                                                        .password(true)
                                                        .desired_width(310.0)
                                                        .hint_text("私钥已加密时填写"),
                                                    );
                                                    ui.end_row();
                                                }
                                            }
                                            ui.label(
                                                RichText::new("主机密钥文件").color(style::muted()),
                                            );
                                            ui.horizontal(|ui| {
                                                ui.add(
                                                    TextEdit::singleline(
                                                        &mut form.profile.ssh_known_hosts,
                                                    )
                                                    .desired_width(235.0)
                                                    .hint_text("默认 known_hosts"),
                                                );
                                                if ui.button("浏览…").clicked()
                                                    && let Some(path) = rfd::FileDialog::new()
                                                        .set_title("选择 SSH 主机密钥文件")
                                                        .pick_file()
                                                {
                                                    form.profile.ssh_known_hosts =
                                                        path.to_string_lossy().into_owned();
                                                }
                                            });
                                            ui.end_row();
                                            let mut fixed = form.profile.ssh_local_port.is_some();
                                            if ui.checkbox(&mut fixed, "指定本地转发端口").changed()
                                            {
                                                form.profile.ssh_local_port = fixed.then_some(33060);
                                            }
                                            if let Some(port) = &mut form.profile.ssh_local_port {
                                                ui.add(
                                                    egui::DragValue::new(port).range(1..=65535),
                                                );
                                            } else {
                                                ui.label(
                                                    RichText::new("自动分配").color(style::muted()),
                                                );
                                            }
                                            ui.end_row();
                                        });
                                }
                            }
                            ui.add_space(4.0);
                            connection_advanced_fields(ui, form);
                        });
                    })
                });
            if testing {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("正在测试连接…").color(style::accent()));
                });
            }
            if has_feedback {
                ui.add_space(6.0);
                egui::ScrollArea::vertical()
                    .id_salt("connection-feedback")
                    .max_height(88.0)
                    .show(ui, |ui| {
                        if !form.error.is_empty() {
                            style::banner(ui, &form.error, style::error(), style::error_soft());
                        }
                        if let Some(probe) = &form.test_result {
                            let compression = probe
                                .compression
                                .map_or("", |enabled| if enabled { "，已启用压缩" } else { "" });
                            let schema = probe
                                .database
                                .as_ref()
                                .map_or_else(String::new, |schema| format!("，数据库 {schema}"));
                            let summary = format!(
                                "上次测试成功：{}，{}{}{}，用时 {} 毫秒",
                                probe.version,
                                probe.transport,
                                compression,
                                schema,
                                probe.elapsed.as_millis()
                            );
                            style::banner(ui, &summary, style::success(), style::success_soft());
                        }
                        if !form.notice.is_empty() {
                            style::banner(ui, &form.notice, style::accent(), style::accent_soft());
                        }
                    });
            }
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                test = ui
                    .add_enabled(!testing, Button::new("测试连接"))
                    .on_hover_text("检查登录和连接参数，跳过初始化 SQL。")
                    .clicked();
                save = ui
                    .add_enabled(!testing && self.profiles_readable, Button::new("保存连接"))
                    .clicked();
                forget = ui
                    .add_enabled(
                        !testing && form.saved.is_some() && self.profiles_readable,
                        Button::new(RichText::new("删除连接配置").color(style::error())),
                    )
                    .clicked();
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    connect = ui.add_enabled(!testing, style::primary("连接")).clicked();
                    close = ui.button("取消").clicked();
                });
            });
        });
        if close {
            self.connection = None;
            return;
        }
        if import {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("导入 SQLyog 连接配置")
                .add_filter("SQLyog 连接配置", &["sycs", "ini"])
                .pick_file()
            {
                match state::import_legacy_profiles(&path, &self.profiles) {
                    Ok(report) => {
                        let summary = report.summary();
                        self.profiles = report.profiles;
                        if let Some(index) = report.first_imported {
                            form.load_profile(self.profiles[index].clone(), Some(index));
                        }
                        form.error.clear();
                        form.notice = summary;
                    }
                    Err(error) => {
                        form.notice.clear();
                        form.error = error;
                    }
                }
            }
            return;
        }
        if !(connect || save || forget || test) {
            return;
        }
        if forget {
            let mut profiles = self.profiles.clone();
            profiles.remove(form.saved.expect("saved profile"));
            match state::save_profiles(&self.profiles, &profiles) {
                Ok(()) => {
                    self.profiles = profiles;
                    form.saved = None;
                    form.error.clear();
                }
                Err(error) => form.error = error,
            }
            return;
        }
        if form.profile.host.trim().is_empty() || form.profile.user.trim().is_empty() {
            form.error = "请填写数据库主机和用户名。".into();
            return;
        }
        if form.profile.http.is_none()
            && form.profile.ssh
            && form.profile.ssh_host.trim().is_empty()
        {
            form.error = "请填写 SSH 主机地址。".into();
            return;
        }
        if form.profile.http.is_none()
            && form.profile.ssh
            && form.profile.ssh_password_auth
            && form.profile.ssh_user.trim().is_empty()
        {
            form.error = "SSH 密码认证需要用户名。".into();
            return;
        }
        if form.profile.http.is_none()
            && form.profile.tls
            && (form.profile.client_identity.trim().is_empty()
                != form.profile.client_private_key.trim().is_empty())
        {
            form.error = "客户端 TLS 需要同时选择证书和私钥。".into();
            return;
        }
        if let Some(profile) = &form.profile.http {
            if profile.url.trim().is_empty() {
                form.error = "请填写 SQLyog PHP 隧道地址。".into();
                return;
            }
            if save
                && [&profile.url, &profile.proxy_url]
                    .into_iter()
                    .filter(|value| !value.trim().is_empty())
                    .any(|value| {
                        reqwest::Url::parse(value.trim()).map_or(true, |url| {
                            url.query().is_some()
                                || !url.username().is_empty()
                                || url.password().is_some()
                        })
                    })
            {
                form.error =
                    "请使用不含认证信息或查询参数的地址保存连接。带参数的地址可以直接连接。".into();
                return;
            }
        }
        if form.profile.label.trim().is_empty() {
            form.profile.label = format!("{}:{}", form.profile.host, form.profile.port);
        }
        if test {
            let config = form.profile.config(
                form.password.clone(),
                form.ssh_password.clone(),
                form.ssh_key_passphrase.clone(),
                String::new(),
            );
            let http = form.profile.http.as_ref().map(|profile| {
                profile.config(form.web_password.clone(), form.proxy_password.clone())
            });
            let worker = Worker::new();
            form.test_result = None;
            form.error.clear();
            match worker.send(1, Command::TestConnection { config, http }) {
                Ok(()) => form.test_worker = Some(worker),
                Err(error) => form.error = error,
            }
            return;
        }
        if save {
            form.sync_passwords_to_profile();
            let mut profiles = self.profiles.clone();
            let index = form.saved.unwrap_or(profiles.len());
            if index == profiles.len() {
                profiles.push(form.profile.clone());
            } else {
                profiles[index] = form.profile.clone();
            }
            match state::save_profiles(&self.profiles, &profiles) {
                Ok(()) => {
                    self.profiles = profiles;
                    form.saved = Some(index);
                    form.error.clear();
                    form.notice = "已保存连接配置与密码。".into();
                }
                Err(error) => {
                    form.notice.clear();
                    form.error = error;
                }
            }
        }
        if connect {
            let init_sql = match form.initialization() {
                Ok(sql) => sql,
                Err(error) => {
                    form.error = error;
                    return;
                }
            };
            form.sync_passwords_to_profile();
            let form = self.connection.take().expect("connection form");
            let config = form.profile.config(
                form.password,
                form.ssh_password,
                form.ssh_key_passphrase,
                init_sql,
            );
            let tunnel = form
                .profile
                .http
                .as_ref()
                .map(|profile| profile.config(form.web_password, form.proxy_password));
            let label = form.profile.label.clone();
            if let Some(index) = form.edit_existing {
                self.active = index;
                let workspace = self.ws();
                workspace.label = label;
                workspace.profile = Some(form.profile);
                workspace.connect(config, tunnel);
            } else if let Some(index) =
                (0..self.workspaces.len()).find(|&index| self.is_placeholder_workspace(index))
            {
                // 首次连接直接接管启动默认的占位工作区，避免残留“临时查询”
                self.active = index;
                let workspace = self.ws();
                workspace.label = label;
                workspace.profile = Some(form.profile);
                workspace.connect(config, tunnel);
            } else {
                let mut workspace = Workspace::new(label, Some(form.profile));
                workspace.result_limit = self.preferences.default_result_limit;
                workspace.copy_column_headers = self.preferences.copy_column_headers;
                workspace.show_data_info = self.preferences.show_data_info;
                workspace.connect(config, tunnel);
                self.workspaces.push(workspace);
                self.active = self.workspaces.len() - 1;
            }
        }
    }

    fn top_bar(&mut self, ctx: &egui::Context) -> Option<ClipboardRequest> {
        // Opening the menu can take focus from the text or result widget.
        if !egui::Popup::is_any_open(ctx) {
            let workspace = &self.workspaces[self.active];
            let find_in_result = self.show_output && result_text_has_focus(workspace, ctx);
            let copy_from_output = find_in_result
                || (self.show_output
                    && workspace.output == state::Output::History
                    && workspace.history.has_focus(ctx));
            self.edit_menu_focus = ctx
                .memory(|memory| memory.focused())
                .map(|id| EditMenuFocus {
                    id,
                    copy_from_output,
                    find_in_result,
                });
        }
        let mut edit = None;
        let mut clipboard = None;
        let mut recent_file = None;
        let mut connect = false;
        let mut script = None;
        let mut open_admin = false;
        let mut open_accounts = false;
        let mut open_processes = false;
        let mut values_view = None;
        let mut open_compare = false;
        let mut open_sync_job = false;
        let mut open_notification_job = false;
        let mut open_odbc_job = false;
        let mut open_interval_backup = false;
        let mut open_designer = false;
        let mut favorite = None;
        let name = self.preferences.query_keys().0.name();
        egui::TopBottomPanel::top("application-bar")
            .frame(style::bar(egui::Margin::symmetric(10, 5)))
            .show(ctx, |ui| {
                style::paint_bottom_hairline(ui);
                let compact = ui.available_width() < 760.0;
                ui.horizontal_wrapped(|ui| {
                    style::menu(ui, "文件", |ui| {
                        let current = &self.workspaces[self.active];
                        let active_connection = current.server.is_some();
                        let ready = active_connection && !current.busy();
                        let busy = self.workspaces.iter().any(Workspace::busy);
                        let connected = self.workspaces.iter().any(|ws| ws.server.is_some());
                        let current_reason = if current.busy() {
                            "当前连接正在执行任务。"
                        } else {
                            "当前工作区未连接。"
                        };
                        for (label, shortcut, action, enabled, reason) in [
                            ("新建连接…", "Ctrl+M", Edit::NewConnection, true, ""),
                            (
                                "使用当前设置新建连接",
                                "Ctrl+N",
                                Edit::CloneConnection,
                                ready,
                                current_reason,
                            ),
                            (
                                "断开当前连接",
                                "Ctrl+F4",
                                Edit::DisconnectConnection,
                                ready,
                                current_reason,
                            ),
                            (
                                "断开全部连接",
                                "",
                                Edit::DisconnectAll,
                                connected && !busy,
                                if busy {
                                    "有连接正在执行任务。"
                                } else {
                                    "没有活动连接。"
                                },
                            ),
                        ] {
                            if ui
                                .add_enabled(enabled, Button::new(label).shortcut_text(shortcut))
                                .on_disabled_hover_text(reason)
                                .clicked()
                            {
                                edit = Some(action);
                                ui.close();
                            }
                        }
                        ui.separator();
                        for (label, shortcut, action) in [
                            ("新建查询", "", Edit::New),
                            ("新建查询设计", "Ctrl+K", Edit::NewQueryDesign),
                            ("新建架构设计", "", Edit::NewSchemaDesign),
                            ("打开 SQL 到当前查询页…", "", Edit::OpenSame),
                            ("打开 SQL 到新查询页…", "", Edit::Open),
                            ("打开查询布局…", "", Edit::OpenQueryDesign),
                            ("打开架构布局…", "", Edit::OpenSchemaDesign),
                        ] {
                            if ui.add(Button::new(label).shortcut_text(shortcut)).clicked() {
                                edit = Some(action);
                                ui.close();
                            }
                        }
                        for (kind, label) in [
                            (RecentKind::Sql, "最近的 SQL 文件"),
                            (RecentKind::SchemaDesign, "最近的架构布局"),
                        ] {
                            style::menu(ui, label, |ui| {
                                let paths = self.recent_files.files(kind);
                                if paths.is_empty() {
                                    ui.label("暂无最近打开的文件");
                                }
                                for path in paths {
                                    let full_path = recent_files::display_path(path);
                                    let name = path.file_name().map_or_else(
                                        || full_path.clone(),
                                        |name| name.to_string_lossy().into_owned(),
                                    );
                                    if ui.button(name).on_hover_text(full_path).clicked() {
                                        recent_file = Some((kind, path.clone()));
                                        ui.close();
                                    }
                                }
                            });
                        }
                        ui.separator();
                        let save_label = match self.active_layout() {
                            Some(Document::Design(_)) => "保存架构布局",
                            Some(Document::Builder(_)) => "保存查询布局",
                            _ => "保存 SQL 文件",
                        };
                        for (label, action) in [(save_label, Edit::Save), ("另存为…", Edit::SaveAs)]
                        {
                            if ui.button(label).clicked() {
                                edit = Some(action);
                                ui.close();
                            }
                        }
                        if ui
                            .add_enabled(
                                self.show_editor,
                                Button::new("关闭当前页签").shortcut_text("Alt+L"),
                            )
                            .clicked()
                        {
                            edit = Some(Edit::Navigate(navigation::Action::CloseDocument));
                            ui.close();
                        }
                    });
                    style::menu(ui, "编辑", |ui| {
                        use editor_tools::Action;
                        let workspace = &self.workspaces[self.active];
                        let query_open = self.active_layout().is_none();
                        let editor_id = editor_tools::editor_id(workspace);
                        let editor = self
                            .edit_menu_focus
                            .filter(|focus| focus.id == editor_id && query_open && self.show_editor)
                            .map(|focus| focus.id);
                        let (can_undo, can_redo) = editor.map_or((false, false), |_| {
                            editor_tools::history_availability(ctx, workspace)
                        });
                        let has_text = editor.is_some() && !workspace.tab().sql.is_empty();
                        let has_selection = editor_tools::has_selection(workspace);
                        let selected = editor.filter(|_| has_selection);
                        let modifier = if cfg!(target_os = "macos") {
                            "⌘"
                        } else {
                            "Ctrl"
                        };
                        for (label, key, action, enabled) in [
                            ("撤销", "Z", Action::Undo, can_undo),
                            ("重做", "Y", Action::Redo, can_redo),
                        ] {
                            if ui
                                .add_enabled(
                                    enabled,
                                    Button::new(label).shortcut_text(format!("{modifier}+{key}")),
                                )
                                .clicked()
                            {
                                edit = Some(Edit::Text(action));
                                ui.close();
                            }
                        }
                        ui.separator();
                        let copy = selected.or_else(|| {
                            self.edit_menu_focus
                                .filter(|focus| {
                                    query_open
                                        && self.show_output
                                        && has_selection
                                        && focus.copy_from_output
                                })
                                .map(|focus| focus.id)
                        });
                        for (label, key, command, target) in [
                            ("剪切", "X", egui::ViewportCommand::RequestCut, selected),
                            ("复制", "C", egui::ViewportCommand::RequestCopy, copy),
                            ("粘贴", "V", egui::ViewportCommand::RequestPaste, editor),
                        ] {
                            if ui
                                .add_enabled(
                                    target.is_some(),
                                    Button::new(label).shortcut_text(format!("{modifier}+{key}")),
                                )
                                .clicked()
                                && let Some(target) = target
                            {
                                clipboard = Some(ClipboardRequest { target, command });
                                ui.close();
                            }
                        }
                        if ui
                            .add_enabled(
                                has_text,
                                Button::new("全选").shortcut_text(format!("{modifier}+A")),
                            )
                            .clicked()
                        {
                            edit = Some(Edit::Text(Action::SelectAll));
                            ui.close();
                        }
                        if ui
                            .add_enabled(has_text, Button::new("清除查询内容"))
                            .clicked()
                        {
                            edit = Some(Edit::Text(Action::Clear));
                            ui.close();
                        }
                        ui.separator();
                        let find_in_result = query_open
                            && self
                                .edit_menu_focus
                                .is_some_and(|focus| focus.find_in_result);
                        for (label, action) in [
                            ("查找…", Action::Find),
                            ("继续查找", Action::Next),
                            ("替换…", Action::Replace),
                            ("转到指定行…", Action::GoTo),
                            ("转为大写", Action::Upper),
                            ("转为小写", Action::Lower),
                            ("注释所选行", Action::Comment),
                            ("取消行注释", Action::Uncomment),
                        ] {
                            let button = match action {
                                Action::Find => {
                                    Button::new(label).shortcut_text(format!("{modifier}+F / F3"))
                                }
                                Action::Replace => Button::new(label).shortcut_text("Ctrl+H"),
                                _ => Button::new(label),
                            };
                            let enabled = match action {
                                Action::Find | Action::Next => editor.is_some() || find_in_result,
                                Action::Replace => editor.is_some(),
                                _ => true,
                            };
                            if ui.add_enabled(enabled, button).clicked() {
                                edit = Some(
                                    if find_in_result
                                        && matches!(action, Action::Find | Action::Next)
                                    {
                                        Edit::FindResult {
                                            next: matches!(action, Action::Next),
                                        }
                                    } else {
                                        Edit::Text(action)
                                    },
                                );
                                ui.close();
                            }
                        }
                        ui.separator();
                        if ui
                            .add_enabled(
                                query_open && self.show_editor,
                                Button::new("插入 SQL 模板…"),
                            )
                            .clicked()
                        {
                            edit = Some(Edit::SqlTemplate);
                            ui.close();
                        }
                        ui.separator();
                        for (label, mode) in [
                            ("匹配的补全项  Ctrl+Space", completion::Mode::Matching),
                            ("全部补全项", completion::Mode::All),
                            ("函数参数  Ctrl+Shift+Space", completion::Mode::Signature),
                        ] {
                            if ui.button(label).clicked() {
                                edit = Some(Edit::Complete(mode));
                                ui.close();
                            }
                        }
                        ui.separator();
                        let workspace = &self.workspaces[self.active];
                        let designs = &self.design_tabs;
                        let has_other_tab = workspace.tabs.len() > 1
                            || designs.documents(workspace.key).next().is_some();
                        for (label, shortcut, action, enabled) in [
                            (
                                "上一个页签",
                                "Ctrl+PgUp",
                                navigation::Action::Document(navigation::Step::Previous),
                                has_other_tab,
                            ),
                            (
                                "下一个页签",
                                "Ctrl+PgDn",
                                navigation::Action::Document(navigation::Step::Next),
                                has_other_tab,
                            ),
                            (
                                "重命名查询页…",
                                "Alt+F2",
                                navigation::Action::RenameQuery,
                                designs.active(workspace.key).is_none(),
                            ),
                        ] {
                            if ui
                                .add_enabled(enabled, Button::new(label).shortcut_text(shortcut))
                                .clicked()
                            {
                                edit = Some(Edit::Navigate(action));
                                ui.close();
                            }
                        }
                    });
                    style::menu(ui, "查询", |ui| {
                        let workspace = &self.workspaces[self.active];
                        let has_selection = workspace
                            .tab()
                            .selection
                            .as_ref()
                            .is_some_and(|selection| !selection.is_empty());
                        let editable_label = if workspace.cancellable() {
                            "停止查询  F8"
                        } else if has_selection {
                            "执行选区并查看结果  F8"
                        } else {
                            "执行并编辑结果  F8"
                        };
                        for (label, action) in [
                            (format!("执行当前语句或选区  {name}"), Edit::Current),
                            (format!("执行选区或整页  Shift+{name}"), Edit::Batch),
                            (format!("执行选区  Ctrl+{name}"), Edit::Selection),
                            (editable_label.into(), Edit::Editable),
                            ("查看当前语句的执行计划".into(), Edit::Explain),
                            ("执行并分析当前 SELECT".into(), Edit::Profile),
                            (
                                "格式化当前语句  F12".into(),
                                Edit::Text(editor_tools::Action::FormatCurrent),
                            ),
                            (
                                "格式化选区  Ctrl+F12".into(),
                                Edit::Text(editor_tools::Action::FormatSelection),
                            ),
                            (
                                "格式化整页 SQL  Shift+F12".into(),
                                Edit::Text(editor_tools::Action::FormatAll),
                            ),
                        ] {
                            if ui
                                .add_enabled(
                                    !matches!(action, Edit::Selection) || has_selection,
                                    Button::new(label),
                                )
                                .clicked()
                            {
                                edit = Some(action);
                                ui.close();
                            }
                        }
                    });
                    style::menu(ui, "视图", |ui| {
                        ui.checkbox(&mut self.show_explorer, "显示对象浏览器")
                            .on_hover_text("Ctrl+1");
                        ui.checkbox(&mut self.show_editor, "显示 SQL 编辑器")
                            .on_hover_text("Ctrl+3");
                        ui.checkbox(&mut self.show_output, "显示输出面板")
                            .on_hover_text("Ctrl+2");
                        if ui.button("查询历史").clicked() {
                            edit = Some(Edit::ShowHistory);
                            ui.close();
                        }
                        ui.menu_button("结果显示方式", |ui| {
                            let workspace = &mut self.workspaces[self.active];
                            for option in output::ResultMode::ALL {
                                if ui
                                    .selectable_label(
                                        workspace.result_mode() == option,
                                        option.label(),
                                    )
                                    .clicked()
                                {
                                    workspace.set_result_mode(option);
                                    ui.close();
                                }
                            }
                        });
                    });
                    style::menu(ui, "收藏夹", |ui| {
                        favorite = self.favorites.menu(ui, self.workspaces[self.active].tab());
                    });
                    style::menu(ui, "服务器", |ui| {
                        for view in ServerView::ALL {
                            if view == ServerView::Processes {
                                if ui
                                    .add_enabled(
                                        self.workspaces[self.active].server.is_some(),
                                        Button::new("进程列表…"),
                                    )
                                    .clicked()
                                {
                                    open_processes = true;
                                    ui.close();
                                }
                                continue;
                            }
                            if view == ServerView::Accounts {
                                if ui.button("用户与权限…").clicked() {
                                    open_accounts = true;
                                    ui.close();
                                }
                                continue;
                            }
                            if view.is_variable_or_status() {
                                if ui
                                    .add_enabled(
                                        self.workspaces[self.active].server.is_some(),
                                        Button::new(format!("{}…", view.label())),
                                    )
                                    .clicked()
                                {
                                    values_view = Some(view);
                                    ui.close();
                                }
                                continue;
                            }
                            if ui.button(view.label()).clicked() {
                                script = Some(view.sql().to_string());
                                ui.close();
                            }
                        }
                        ui.separator();
                        let workspace = &self.workspaces[self.active];
                        if ui
                            .add_enabled(
                                workspace.server.is_some() && !workspace.busy(),
                                Button::new("刷新服务器资源…  Ctrl+Alt+F"),
                            )
                            .clicked()
                        {
                            edit = Some(Edit::FlushServer);
                            ui.close();
                        }
                        if ui.button("管理数据库…").clicked() {
                            open_admin = true;
                            ui.close();
                        }
                    });
                    style::menu(ui, "窗口", |ui| {
                        use workspace_windows::Arrangement;
                        for (label, action) in [
                            ("层叠", Edit::ArrangeWorkspaces(Arrangement::Cascade)),
                            ("平铺", Edit::ArrangeWorkspaces(Arrangement::Tile)),
                            (
                                "最大化当前连接",
                                Edit::ArrangeWorkspaces(Arrangement::Maximized),
                            ),
                            ("最小化当前连接", Edit::MinimizeWorkspace),
                            ("排列图标", Edit::ArrangeWorkspaceIcons),
                        ] {
                            let enabled = match action {
                                Edit::ArrangeWorkspaceIcons => {
                                    !self.workspace_windows.minimized.is_empty()
                                }
                                Edit::MinimizeWorkspace => !self
                                    .workspace_windows
                                    .minimized
                                    .contains(&self.workspaces[self.active].key),
                                _ => true,
                            };
                            if ui.add_enabled(enabled, Button::new(label)).clicked() {
                                edit = Some(action);
                                ui.close();
                            }
                        }
                        ui.separator();
                        for (index, workspace) in self.workspaces.iter().enumerate() {
                            let label = navigation::workspace_label(workspace);
                            if ui.selectable_label(self.active == index, label).clicked() {
                                edit = Some(Edit::SelectWorkspace(index));
                                ui.close();
                            }
                        }
                    });
                    style::menu(ui, "工具", |ui| {
                        if ui.button("偏好设置…").clicked() {
                            self.preferences_dialog.open(&self.preferences);
                            ui.close();
                        }
                        let connected = self.workspaces.iter().any(|ws| ws.server.is_some());
                        let query_page = self.active_layout().is_none();
                        let workspace = &mut self.workspaces[self.active];
                        if ui
                            .add_enabled(
                                query_page && can_export_displayed_rows(workspace),
                                Button::new("导出当前结果或表数据…").shortcut_text("Ctrl+Shift+E"),
                            )
                            .clicked()
                        {
                            edit = Some(Edit::ExportDisplayedRows);
                            ui.close();
                        }
                        for (label, shortcut, action) in [
                            ("备份为 SQL 转储文件…", "Ctrl+Alt+E", ObjectFileAction::Dump),
                            (
                                "从 SQL 转储文件还原…",
                                "Ctrl+Shift+Q",
                                ObjectFileAction::Restore,
                            ),
                            (
                                "导入文本文件…",
                                "Ctrl+Shift+M",
                                ObjectFileAction::ImportText,
                            ),
                            (
                                "导出数据库结构说明…",
                                "Ctrl+Alt+Shift+S",
                                ObjectFileAction::SchemaHtml,
                            ),
                        ] {
                            let available = action.resolve(workspace);
                            let button = ui.add_enabled(
                                available.is_ok(),
                                Button::new(label).shortcut_text(shortcut),
                            );
                            let button = if let Err(reason) = available {
                                button.on_disabled_hover_text(reason)
                            } else {
                                button
                            };
                            if button.clicked() {
                                edit = Some(Edit::ObjectFile(action));
                                ui.close();
                            }
                        }
                        if ui
                            .add_enabled(
                                workspace.server.is_some(),
                                Button::new("表诊断…  Ctrl+Alt+T"),
                            )
                            .clicked()
                        {
                            edit = Some(Edit::TableDiagnostics);
                            ui.close();
                        }
                        if ui
                            .add_enabled(
                                workspace.server.is_some()
                                    && !workspace.busy()
                                    && !workspace.tags.active_rebuild(),
                                Button::new("重建补全标签"),
                            )
                            .on_hover_text(
                                "刷新当前连接的数据库和对象名称，列与参数会在用到时重新读取。",
                            )
                            .clicked()
                        {
                            completion::rebuild(workspace);
                            ui.close();
                        }
                        if ui
                            .add(Button::new("新建查询设计").shortcut_text("Ctrl+K"))
                            .clicked()
                        {
                            edit = Some(Edit::NewQueryDesign);
                            ui.close();
                        }
                        if ui
                            .add_enabled(connected, Button::new("比较数据库结构…"))
                            .clicked()
                        {
                            open_compare = true;
                            ui.close();
                        }
                        if ui
                            .add(Button::new("计划备份…").shortcut_text("Ctrl+Alt+S"))
                            .clicked()
                        {
                            edit = Some(Edit::ObjectFile(ObjectFileAction::Schedule));
                            ui.close();
                        }
                        if ui.button("数据同步作业…").clicked() {
                            open_sync_job = true;
                            ui.close();
                        }
                        if ui.button("SQL 调度与邮件通知…").clicked() {
                            open_notification_job = true;
                            ui.close();
                        }
                        if ui.button("外部数据导入作业…").clicked() {
                            open_odbc_job = true;
                            ui.close();
                        }
                        if ui.button("连接内间隔备份…").clicked() {
                            open_interval_backup = true;
                            ui.close();
                        }
                        let label = if self.job_manager.busy() {
                            "作业管理（运行中）…"
                        } else {
                            "作业管理…"
                        };
                        if ui.button(label).clicked() {
                            self.job_manager.open();
                            ui.close();
                        }
                        if ui
                            .add_enabled(connected, Button::new("架构设计器…"))
                            .clicked()
                        {
                            open_designer = true;
                            ui.close();
                        }
                    });
                    help::menu(ui, &mut self.help);
                    if compact {
                        if style::theme_toggle_button(ui).clicked() {
                            style::toggle_theme(ctx);
                        }
                        connect = ui.add(style::primary("新建连接")).clicked();
                    } else {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            connect = ui.add(style::primary("新建连接")).clicked();
                            if style::theme_toggle_button(ui).clicked() {
                                style::toggle_theme(ctx);
                            }
                        });
                    }
                });
            });
        if connect {
            self.new_connection(None);
        }
        if open_admin {
            self.admin = Some(admin::Admin::new(self.workspaces[self.active].key));
        }
        if open_accounts {
            self.admin = Some(admin::Admin::accounts(self.workspaces[self.active].key));
        }
        if open_processes {
            self.processes = Some(processes::Dialog::new(self.workspaces[self.active].key));
        }
        if let Some(view) = values_view {
            self.server_values = Some(server_values::Dialog::new(
                self.workspaces[self.active].key,
                view,
            ));
        }
        if open_sync_job {
            self.job_manager
                .open_data_sync(self.workspaces.get(self.active));
        }
        if open_notification_job {
            self.job_manager
                .open_notification(self.workspaces.get(self.active));
        }
        if open_odbc_job {
            self.job_manager.open_odbc(self.workspaces.get(self.active));
        }
        if open_interval_backup {
            let workspace = self
                .workspaces
                .get(self.active)
                .filter(|ws| ws.server.is_some());
            let object = workspace.and_then(|ws| ws.selected.clone());
            let schema = object
                .as_ref()
                .map(|object| object.schema.clone())
                .or_else(|| {
                    workspace.and_then(|ws| {
                        ws.server
                            .as_ref()
                            .and_then(|server| server.database.clone())
                            .or_else(|| ws.schemas.first().cloned())
                    })
                });
            if let Some(schema) = schema {
                self.schedule.open(workspace, schema, object);
            } else {
                self.schedule.open(None, String::new(), None);
            }
        }
        if open_designer {
            self.new_schema_design();
        }
        if open_compare {
            let index = if self.workspaces[self.active].server.is_some() {
                self.active
            } else {
                self.workspaces
                    .iter()
                    .position(|ws| ws.server.is_some())
                    .unwrap_or(self.active)
            };
            self.compare = Some(compare::Compare::new(index, &self.workspaces));
        }
        if let Some(edit) = edit {
            self.edit(ctx, edit);
        }
        if let Some((kind, path)) = recent_file {
            self.completion.close();
            match kind {
                RecentKind::Sql => self.open_sql_file(self.active, path, false),
                RecentKind::SchemaDesign => self.open_schema_file(path),
            }
        }
        if let Some(favorite) = favorite {
            self.apply_favorite(ctx, favorite);
        }
        if let Some(script) = script {
            self.design_tabs
                .select_query(self.workspaces[self.active].key);
            let workspace = self.ws();
            workspace.new_tab(script, None);
            if !workspace.busy() && workspace.server.is_some() {
                let sql = workspace.tab().sql.clone();
                workspace.start_batch(vec![sql]);
            }
        }
        clipboard
    }

    fn workspace_bar(&mut self, ctx: &egui::Context) {
        // 未建立任何连接前隐藏默认占位工作区条，避免顶部多出一个无用的“临时查询”
        if self.workspaces.len() == 1 && self.is_placeholder_workspace(0) {
            return;
        }
        let mut close = None;
        let mut active_action = None;
        let mut select = None;
        let mut switch_schema: Option<String> = None;
        let key = self.workspaces[self.active].key;
        let reveal = self.visible_workspace.replace(key) != Some(key);
        let active_connected = self.workspaces[self.active].server.is_some();
        let active_db = self.workspaces[self.active]
            .active_database()
            .map(str::to_owned);
        let active_schemas = self.workspaces[self.active].schemas.clone();
        egui::TopBottomPanel::top("workspace-bar")
            .frame(style::chrome_bar(egui::Margin::symmetric(14, 5)))
            .show(ctx, |ui| {
                style::paint_bottom_hairline(ui);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if self.workspaces.len() > 1 {
                        ui.menu_button("连接列表", |ui| {
                            egui::ScrollArea::vertical()
                                .max_height(320.0)
                                .show(ui, |ui| {
                                    for (index, workspace) in self.workspaces.iter().enumerate() {
                                        if ui
                                            .selectable_label(
                                                self.active == index,
                                                navigation::workspace_label(workspace),
                                            )
                                            .clicked()
                                        {
                                            select = Some(index);
                                            ui.close();
                                        }
                                    }
                                });
                        })
                        .response
                        .on_hover_text("切换连接（Ctrl+Tab / Ctrl+Shift+Tab）");
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        let tabs_max_width = if active_connected {
                            (ui.available_width() - 256.0).max(160.0)
                        } else {
                            ui.available_width().max(160.0)
                        };
                        egui::ScrollArea::horizontal()
                            .max_width(tabs_max_width)
                            .auto_shrink([true, true])
                            .show(ui, |ui| {
                                for (index, workspace) in self.workspaces.iter().enumerate() {
                                    ui.push_id(workspace.key, |ui| {
                                        let selected = self.active == index;
                                        let connected = workspace.server.is_some();
                                        let dot_color = if workspace.busy() {
                                            style::accent()
                                        } else if connected {
                                            style::success()
                                        } else {
                                            style::subtle()
                                        };
                                        let response = egui::Frame::new()
                                            .fill(if selected {
                                                style::surface()
                                            } else {
                                                style::panel()
                                            })
                                            .stroke(egui::Stroke::new(
                                                1.0_f32,
                                                if selected {
                                                    style::border()
                                                } else {
                                                    style::border_subtle()
                                                },
                                            ))
                                            .corner_radius(egui::CornerRadius::same(6))
                                            .inner_margin(egui::Margin::symmetric(9, 3))
                                            .show(ui, |ui| {
                                                ui.horizontal(|ui| {
                                                    ui.spacing_mut().item_spacing.x = 5.0;
                                                    style::draw_status_dot(ui, dot_color, connected);
                                                    let label = if selected {
                                                        RichText::new(&workspace.label)
                                                            .strong()
                                                            .color(style::ink())
                                                    } else {
                                                        RichText::new(&workspace.label)
                                                            .color(style::muted())
                                                    };
                                                    ui.add(
                                                        egui::Label::new(label).selectable(false),
                                                    );
                                                });
                                            })
                                            .response
                                            .interact(egui::Sense::click());
                                        if selected {
                                            let rect = response.rect;
                                            ui.painter().rect_filled(
                                                egui::Rect::from_min_max(
                                                    egui::pos2(rect.left() + 6.0, rect.bottom() - 2.0),
                                                    egui::pos2(rect.right() - 6.0, rect.bottom()),
                                                ),
                                                egui::CornerRadius::same(1),
                                                style::accent(),
                                            );
                                        }
                                        if reveal && selected {
                                            response.scroll_to_me(None);
                                        }
                                        if response.clicked() {
                                            select = Some(index);
                                        }
                                        response.context_menu(|ui| {
                                            if let Some(server) = &workspace.server {
                                                let mut action = None;
                                                objects::connection_menu(
                                                    ui,
                                                    server.database.as_deref(),
                                                    &mut action,
                                                );
                                                if let Some(action) = action {
                                                    active_action = Some((index, action));
                                                }
                                                ui.separator();
                                            }
                                            if self.workspaces.len() > 1
                                                && ui.button("关闭工作区").clicked()
                                            {
                                                close = Some(index);
                                                ui.close();
                                            }
                                        });
                                    });
                                }
                            });
                        if active_connected {
                            ui.separator();
                            draw_database_icon(ui, style::accent());
                            let selected_label = active_db.as_deref().unwrap_or("选择默认数据库");
                            let text = if active_db.is_some() {
                                RichText::new(selected_label).strong().color(style::ink())
                            } else {
                                RichText::new(selected_label).color(style::muted())
                            };
                            egui::ComboBox::from_id_salt(("active-database-selector", key))
                                .width(196.0)
                                .height(340.0)
                                .selected_text(text)
                                .show_ui(ui, |ui| {
                                    if active_schemas.is_empty() {
                                        ui.label(
                                            RichText::new("暂无可选数据库")
                                                .small()
                                                .color(style::muted()),
                                        );
                                    } else {
                                        for schema in &active_schemas {
                                            let is_current =
                                                active_db.as_deref() == Some(schema.as_str());
                                            if ui.selectable_label(is_current, schema).clicked() {
                                                switch_schema = Some(schema.clone());
                                            }
                                        }
                                    }
                                })
                                .response
                                .on_hover_text(
                                    "当前默认数据库：执行 SQL 时默认在此数据库中运行",
                                );
                        }
                    });
                });
            });
        if let Some(index) = select {
            self.activate_workspace(ctx, index);
        }
        if let Some(schema) = switch_schema {
            self.workspaces[self.active].request_schema_switch(schema);
        }
        if let Some(index) = close {
            let unsaved_design = self.design_tabs.dirty(Some(self.workspaces[index].key));
            if unsaved_design
                || self.workspaces[index]
                    .tabs
                    .iter()
                    .any(|tab| tab.saved != tab.sql)
            {
                self.discard = Some(Discard::Workspace(index));
            } else {
                self.close_workspace(index);
            }
        }
        if let Some((index, action)) = active_action {
            self.activate_workspace(ctx, index);
            self.browse(ctx, action);
        }
    }

    fn close_workspace(&mut self, index: usize) {
        if self.reveal_protected_row_editor(
            index,
            "请先结束数据编辑操作，或保存、取消草稿再关闭连接。",
        ) {
            return;
        }
        let key = self.workspaces[index].key;
        self.design_tabs.remove_workspace(key);
        self.workspace_windows.minimized.remove(&key);
        self.workspaces.remove(index);
        if self.active > index || self.active == self.workspaces.len() {
            self.active -= 1;
        }
    }

    fn reveal_protected_row_editor(&mut self, index: usize, message: &str) -> bool {
        let Some(tab) = self.workspaces[index].protected_row_editor_tab() else {
            return false;
        };
        self.workspace_windows.restore(self.workspaces[index].key);
        self.active = index;
        self.show_output = true;
        let workspace = &mut self.workspaces[index];
        self.design_tabs.select_query(workspace.key);
        workspace.select_tab(tab);
        workspace.output = if workspace.editable_result_protected(tab) {
            state::Output::Results
        } else {
            state::Output::Data
        };
        workspace.focus_request = false;
        workspace.message(message, false);
        true
    }

    fn footer(&mut self, ctx: &egui::Context) {
        self.footer_in(&mut workspace_windows::PanelHost::Root(ctx), self.active);
    }

    fn select_workspace_at_pointer(&mut self, ctx: &egui::Context) {
        if self.workspace_windows.mode == workspace_windows::Arrangement::Maximized
            || egui::Popup::is_any_open(ctx)
            || ctx.memory(|memory| memory.top_modal_layer().is_some())
        {
            return;
        }
        let Some(pointer) = ctx.input(|input| {
            input
                .pointer
                .interact_pos()
                .filter(|_| input.pointer.any_pressed())
        }) else {
            return;
        };
        let layer = ctx.layer_id_at(pointer);
        let Some(region) = self.workspace_regions.iter().rev().find(|region| {
            region.rect.contains(pointer) && region.layer.is_none_or(|id| layer == Some(id))
        }) else {
            return;
        };
        if let Some(index) = self.workspaces.iter().position(|ws| ws.key == region.key)
            && self.active != index
        {
            self.active = index;
            self.completion.close();
            ctx.request_repaint();
        }
    }

    fn arranged_workspaces(
        &mut self,
        ctx: &egui::Context,
        focus: Option<navigation::Pane>,
        modal: bool,
    ) {
        use workspace_windows::{Arrangement, cascade_rectangles, tile_rectangles};

        let mut regions = Vec::new();
        let mut action = None;
        let minimized: Vec<_> = self
            .workspaces
            .iter()
            .enumerate()
            .filter_map(|(index, ws)| {
                self.workspace_windows
                    .minimized
                    .contains(&ws.key)
                    .then_some(index)
            })
            .collect();
        let visible: Vec<_> = self
            .workspaces
            .iter()
            .enumerate()
            .filter_map(|(index, ws)| {
                (!self.workspace_windows.minimized.contains(&ws.key)).then_some(index)
            })
            .collect();
        let mode = self.workspace_windows.mode;
        let revision = self.workspace_windows.revision;
        egui::CentralPanel::default().show(ctx, |ui| {
            let available = ui.max_rect();
            let columns = (available.width() / 176.0).floor().max(1.0) as usize;
            let icons_height =
                (minimized.len().div_ceil(columns) as f32 * 36.0).min(available.height() * 0.35);
            let body = egui::Rect::from_min_max(
                available.min,
                available.max - egui::vec2(0.0, icons_height),
            );
            let rectangles = if mode == Arrangement::Tile {
                tile_rectangles(body, visible.len())
            } else {
                cascade_rectangles(body, visible.len())
            };
            let draw = |this: &mut Self, ui: &mut egui::Ui, index, label: Option<&str>| {
                if let Some(request) = this.workspace_window(ui, index, focus, modal, label) {
                    action = Some(request);
                }
            };
            let mut draw = draw;
            for (index, rect) in visible.into_iter().zip(rectangles) {
                let workspace = &self.workspaces[index];
                let key = workspace.key;
                let title = navigation::workspace_label(workspace);
                if mode == Arrangement::Tile {
                    let mut tile = ui.new_child(
                        egui::UiBuilder::new()
                            .id_salt(("workspace-tile", key))
                            .max_rect(rect)
                            .layout(Layout::top_down(Align::Min)),
                    );
                    tile.set_clip_rect(tile.clip_rect().intersect(rect));
                    tile.set_min_size(rect.size());
                    draw(self, &mut tile, index, Some(&title));
                    regions.push(WorkspaceRegion {
                        key,
                        rect,
                        layer: None,
                    });
                } else if let Some(response) = egui::Window::new(title)
                    .id(egui::Id::new(("workspace-window", key, revision)))
                    .default_rect(rect)
                    .min_size(egui::vec2(260.0, 170.0))
                    .constrain_to(body)
                    .collapsible(false)
                    .show(ctx, |ui| draw(self, ui, index, None))
                {
                    regions.push(WorkspaceRegion {
                        key,
                        rect: response.response.rect,
                        layer: Some(response.response.layer_id),
                    });
                }
            }
            for (slot, index) in minimized.into_iter().enumerate() {
                let workspace = &self.workspaces[index];
                let position = egui::pos2(
                    available.left() + (slot % columns) as f32 * 176.0,
                    body.bottom() + (slot / columns) as f32 * 36.0,
                );
                let restored = egui::Area::new(egui::Id::new((
                    "workspace-icon",
                    workspace.key,
                    self.icon_revision,
                )))
                .default_pos(position)
                .constrain_to(available)
                .movable(true)
                .show(ctx, |ui| {
                    style::bar(egui::Margin::symmetric(6, 3))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.add_sized(
                                    [106.0, 24.0],
                                    egui::Label::new(navigation::workspace_label(workspace))
                                        .selectable(false)
                                        .truncate(),
                                )
                                .on_hover_text("拖动图标");
                                ui.small_button("还原").clicked()
                            })
                            .inner
                        })
                        .inner
                })
                .inner;
                if restored {
                    action = Some(Edit::SelectWorkspace(index));
                }
            }
        });
        self.workspace_regions = regions;
        if mode == Arrangement::Cascade
            && let Some(layer) = self
                .workspace_regions
                .iter()
                .find(|region| region.key == self.workspaces[self.active].key)
                .and_then(|region| region.layer)
        {
            ctx.move_to_top(layer);
        }
        if let Some(action) = action {
            self.edit(ctx, action);
        }
    }

    fn workspace_window(
        &mut self,
        ui: &mut egui::Ui,
        index: usize,
        focus: Option<navigation::Pane>,
        modal: bool,
        title: Option<&str>,
    ) -> Option<Edit> {
        let ctx = ui.ctx().clone();
        let active = index == self.active;
        let action = ui
            .horizontal(|ui| {
                if let Some(title) = title {
                    ui.label(RichText::new(title).strong());
                }
                if !active {
                    return ui
                        .button("切换到此连接")
                        .clicked()
                        .then_some(Edit::SelectWorkspace(index));
                }
                if ui.button("最小化").clicked() {
                    Some(Edit::MinimizeWorkspace)
                } else if ui.button("最大化").clicked() {
                    Some(Edit::ArrangeWorkspaces(
                        workspace_windows::Arrangement::Maximized,
                    ))
                } else {
                    None
                }
            })
            .inner;
        ui.separator();
        ui.add_enabled_ui(active, |ui| {
            let mut host = workspace_windows::PanelHost::Local(ui);
            self.footer_in(&mut host, index);
            if self.show_explorer {
                self.sidebar_in(
                    &ctx,
                    &mut host,
                    index,
                    active && focus == Some(navigation::Pane::Explorer),
                );
            }
            let layout_visible = self.show_editor
                && self
                    .design_tabs
                    .active(self.workspaces[index].key)
                    .is_some();
            if self.show_output && !layout_visible {
                let workspace = &mut self.workspaces[index];
                let output_input = active && (!modal || output::has_modal(workspace));
                output::panel_in(
                    &mut host,
                    workspace,
                    !self.show_editor,
                    output_input,
                    self.preferences.editor,
                    active && focus == Some(navigation::Pane::Output),
                    &mut self.grid_widths,
                );
            }
            if self.show_editor {
                self.editor_in(&ctx, &mut host, index, active && !modal);
            }
        });
        action
    }

    fn footer_in(&mut self, host: &mut workspace_windows::PanelHost<'_>, index: usize) {
        let workspace = &mut self.workspaces[index];
        let id = if host.is_local() {
            egui::Id::new(("status-bar", workspace.key))
        } else {
            egui::Id::new("status-bar")
        };
        let panel =
            egui::TopBottomPanel::bottom(id).frame(style::pane(egui::Margin::symmetric(14, 6)));
        host.bottom(panel, |ui| {
            style::paint_top_hairline(ui);
            // 先从右侧排布数据库名和版本徽章，再让左侧状态文本在剩余宽度内截断，避免长消息覆盖右侧信息
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(server) = &workspace.server {
                    let version_tip = if server.http_tunnel {
                        "HTTP 隧道：每次请求独立连接。".to_owned()
                    } else {
                        format!("连接 ID：{}", server.connection_id)
                    };
                    egui::Frame::new()
                        .fill(style::chrome())
                        .corner_radius(egui::CornerRadius::same(5))
                        .inner_margin(egui::Margin::symmetric(7, 1))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(&server.version)
                                    .small()
                                    .monospace()
                                    .color(style::muted()),
                            )
                            .on_hover_text(version_tip);
                        });
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(workspace.active_database().unwrap_or("未选择默认数据库"))
                            .small()
                            .strong()
                            .color(style::ink()),
                    );
                    if !workspace.busy() {
                        style::draw_status_dot(ui, style::success(), true);
                    }
                } else {
                    ui.label(RichText::new("未连接").small().color(style::muted()));
                    style::draw_status_dot(ui, style::subtle(), false);
                }
                ui.add_space(12.0);
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    if workspace.busy() {
                        ui.spinner();
                    }
                    if workspace.restoring() {
                        if let Some(progress) = &workspace.restore_progress
                            && progress.total_bytes > 0
                        {
                            let fraction = progress.bytes as f64 / progress.total_bytes as f64;
                            ui.add(
                                egui::ProgressBar::new(fraction.clamp(0.0, 1.0) as f32)
                                    .desired_width(108.0)
                                    .show_percentage(),
                            );
                        }
                        if ui
                            .add_enabled(workspace.can_stop_restore(), style::danger("停止还原"))
                            .on_hover_text("停止提交后续 SQL。已经提交的更改可能保留。")
                            .clicked()
                        {
                            workspace.stop_restore();
                        }
                    }
                    let restore_label = workspace.restore_status();
                    let label = if let Some(label) = &restore_label {
                        label.as_str()
                    } else if workspace.status.is_empty() {
                        "就绪"
                    } else {
                        &workspace.status
                    };
                    ui.add(
                        egui::Label::new(RichText::new(label).small().color(style::muted()))
                            .truncate(),
                    )
                    .on_hover_text(label);
                });
            });
        });
    }

    fn sidebar(&mut self, ctx: &egui::Context, request_focus: bool) {
        self.sidebar_in(
            ctx,
            &mut workspace_windows::PanelHost::Root(ctx),
            self.active,
            request_focus,
        );
    }

    fn sidebar_in(
        &mut self,
        ctx: &egui::Context,
        host: &mut workspace_windows::PanelHost<'_>,
        index: usize,
        request_focus: bool,
    ) {
        let mut action = None;
        let mut schema_switch: Option<String> = None;
        let mut reconnect = false;
        let key = self.workspaces[index].key;
        let layout = self.design_tabs.active(key).filter(|_| self.show_editor);
        let design_open = matches!(layout, Some(Document::Design(_)));
        let builder_open = matches!(layout, Some(Document::Builder(_)));
        let insert_object_name = self.show_editor
            && layout.is_none()
            && self.preferences.insert_object_name_on_double_click;
        let may_fetch_tree = self.transfer.is_none()
            && self.create_table.is_none()
            && self.alter_table.is_none()
            && self.table_keys.is_none()
            && self.data_sync.is_none();
        let refresh_key = self.preferences.query_keys().1.name();
        let width = if host.is_local() {
            (host.width() * 0.34).clamp(96.0, 262.0)
        } else {
            262.0
        };
        let minimum = if host.is_local() {
            width.min(150.0)
        } else {
            210.0
        };
        let workspace = &mut self.workspaces[index];
        let id = if host.is_local() {
            egui::Id::new(("explorer", key))
        } else {
            egui::Id::new("explorer")
        };
        let panel = egui::SidePanel::left(id)
            .default_width(width)
            .width_range(minimum..=430.0)
            .frame(style::pane(egui::Margin::symmetric(12, 12)));
        host.side(panel, |ui| {
            style::paint_right_hairline(ui);
            ui.horizontal(|ui| {
                ui.label(RichText::new("数据库").size(15.0).strong().color(style::ink()));
                if workspace.server.is_some() && !workspace.schemas.is_empty() {
                    style::badge(
                        ui,
                        workspace.schemas.len().to_string(),
                        style::muted(),
                        style::chrome(),
                    );
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if workspace.server.is_some() {
                        if ui
                            .add_enabled(!workspace.busy(), Button::new("刷新"))
                            .on_hover_text(format!("刷新对象浏览器（{refresh_key}）"))
                            .clicked()
                        {
                            workspace.send(Command::ListSchemas, "正在刷新数据库列表…");
                        }
                    } else if workspace.profile.is_some() {
                        reconnect = ui
                            .add_enabled(!workspace.busy(), Button::new("重新连接"))
                            .clicked();
                    }
                });
            });
            ui.add_space(8.0);
            if workspace.server.is_none() {
                style::card(egui::Margin::same(14)).show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.vertical_centered(|ui| {
                        ui.add_space(6.0);
                        style::draw_status_dot(ui, style::subtle(), false);
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new("连接后即可浏览数据库对象。")
                                .small()
                                .color(style::muted()),
                        );
                        ui.add_space(6.0);
                    });
                });
                return;
            }
            ui.horizontal(|ui| {
                ui.add(
                    TextEdit::singleline(&mut workspace.schema_filter)
                        .hint_text("筛选数据库和对象")
                        .margin(egui::Margin::symmetric(10, 6))
                        .desired_width(f32::INFINITY),
                );
            });
            ui.add_space(8.0);
            let filter = workspace.schema_filter.to_lowercase();
            let focus_schema = workspace
                .selected
                .as_ref()
                .map(|object| object.schema.as_str())
                .or_else(|| workspace.server.as_ref()?.database.as_deref());
            egui::ScrollArea::vertical().show(ui, |ui| {
                let mut object_focus = None;
                let mut schema_focus = None;
                for schema in &workspace.schemas {
                    let items = workspace.objects.get(schema);
                    let matches_schema = schema.to_lowercase().contains(&filter);
                    if !matches_schema
                        && !items.is_some_and(|items| {
                            items
                                .iter()
                                .any(|item| item.name.to_lowercase().contains(&filter))
                        })
                    {
                        continue;
                    }
                    let current = workspace.active_database() == Some(schema.as_str());
                    let label = if current {
                        RichText::new(schema).strong().color(style::accent())
                    } else {
                        RichText::new(schema).strong()
                    };
                    let header = egui::CollapsingHeader::new(label)
                        .id_salt(schema)
                        .default_open(current)
                        .show(ui, |ui| {
                            let Some(items) = items else {
                                if may_fetch_tree && !workspace.busy() && action.is_none() {
                                    action = Some(BrowseAction::Objects(schema.clone()));
                                }
                                ui.label(RichText::new("正在加载数据库对象…").weak());
                                return;
                            };
                            for kind in [
                                ObjectKind::Table,
                                ObjectKind::View,
                                ObjectKind::Procedure,
                                ObjectKind::Function,
                                ObjectKind::Trigger,
                                ObjectKind::Event,
                            ] {
                                let matches: Vec<_> = items
                                    .iter()
                                    .filter(|item| {
                                        item.kind == kind
                                            && (matches_schema
                                                || item.name.to_lowercase().contains(&filter))
                                    })
                                    .collect();
                                if matches.is_empty() && !matches_schema {
                                    continue;
                                }
                                let folder_label = RichText::new(kind_label(kind))
                                    .color(style::object_kind_color(kind))
                                    .strong();
                                let group = egui::CollapsingHeader::new(folder_label)
                                    .id_salt((schema, kind_label(kind)))
                                    .default_open(kind == ObjectKind::Table)
                                    .show(ui, |ui| {
                                        for object in matches {
                                            let selected =
                                                workspace.selected.as_ref() == Some(object);
                                            let response = if (design_open
                                                && kind == ObjectKind::Table)
                                                || (builder_open && is_tabular(kind))
                                            {
                                                let response = ui.add(
                                                    Button::selectable(selected, &object.name)
                                                        .sense(egui::Sense::click_and_drag()),
                                                );
                                                if builder_open {
                                                    response.dnd_set_drag_payload(object.clone());
                                                } else {
                                                    schema_designer::drag_table(&response, object);
                                                }
                                                response
                                            } else {
                                                ui.selectable_label(selected, &object.name)
                                            };
                                            if request_focus && selected {
                                                object_focus = Some(response.clone());
                                            }
                                            if response.double_clicked() {
                                                schema_switch = Some(object.schema.clone());
                                                action = Some(object_double_click(
                                                    object,
                                                    design_open,
                                                    insert_object_name,
                                                ));
                                            } else if response.clicked() {
                                                schema_switch = Some(object.schema.clone());
                                                action = Some(BrowseAction::SelectObject(
                                                    object.clone(),
                                                ));
                                            }
                                            response.context_menu(|ui| {
                                                objects::object_menu(ui, object, &mut action);
                                            });
                                        }
                                    });
                                if group.header_response.clicked() {
                                    schema_switch = Some(schema.clone());
                                }
                                group.header_response.context_menu(|ui| {
                                    objects::folder_menu(ui, schema, kind, &mut action);
                                });
                            }
                        });
                    if header.header_response.clicked() {
                        schema_switch = Some(schema.clone());
                    }
                    if request_focus && (schema_focus.is_none() || focus_schema == Some(schema)) {
                        schema_focus = Some(header.header_response.clone());
                    }
                    header.header_response.context_menu(|ui| {
                        objects::schema_menu(ui, schema, &mut action);
                    });
                }
                if let Some(target) = object_focus.or(schema_focus) {
                    target.request_focus();
                    target.scroll_to_me(None);
                }
            });
        });
        if reconnect {
            self.new_connection(Some(index));
        }
        if let Some(action) = action {
            self.browse(ctx, action);
        }
        if let Some(schema) = schema_switch {
            self.workspaces[index].request_schema_switch(schema);
        }
    }

    fn browse(&mut self, ctx: &egui::Context, action: BrowseAction) {
        let index = self.active;
        let workspace = &mut self.workspaces[index];
        match action {
            BrowseAction::TableDiagnostics { schema, table } => {
                self.table_maintenance =
                    table_maintenance::Dialog::open(workspace, Some(schema), table);
            }
            BrowseAction::SchemaDesigner { schema, table } => {
                let key = workspace.key;
                self.design_tabs.open_table(key, schema, table);
                self.show_editor = true;
                self.completion.close();
            }
            BrowseAction::RefreshAll => {
                workspace.send(Command::ListSchemas, "正在刷新数据库列表…");
            }
            BrowseAction::Objects(schema) => {
                workspace.send(Command::ListObjects { schema }, "正在加载数据库对象…");
            }
            BrowseAction::RefreshObjects(schema) => {
                workspace.refresh_objects(schema, "正在刷新对象列表…");
            }
            BrowseAction::UseSchema(schema) => {
                workspace.request_schema_switch(schema);
            }
            BrowseAction::SelectObject(object)
                if workspace.output == state::Output::Data && object.kind == ObjectKind::Table =>
            {
                self.open_table_data(object, false);
            }
            BrowseAction::Inspect(object) | BrowseAction::SelectObject(object) => {
                workspace.selected = Some(object.clone());
                workspace.send(Command::Inspect { object }, "正在读取对象定义…");
            }
            BrowseAction::Builder(object) => {
                workspace.selected = Some(object.clone());
                self.design_tabs
                    .new_builder(workspace.key, query_builder::Builder::new(object));
                self.show_editor = true;
                self.completion.close();
            }
            BrowseAction::InsertObjectName(object) => {
                let name = crate::database::quote_identifier(&object.name);
                workspace.selected = Some(object);
                self.completion.close();
                editor_tools::insert_text(ctx, workspace, &name, false);
            }
            BrowseAction::Preview(object) => {
                workspace.selected = Some(object.clone());
                if workspace.send(
                    Command::Preview {
                        object,
                        limit: workspace.result_limit,
                    },
                    "正在读取数据…",
                ) && matches!(
                    self.design_tabs.active(workspace.key),
                    Some(Document::Builder(_))
                ) {
                    self.design_tabs.select_query(workspace.key);
                    self.show_output = true;
                }
            }
            BrowseAction::Import(object) => {
                self.import = Import::choose(index, object)
                    .map_err(|error| workspace.message(error, true))
                    .ok()
                    .flatten();
            }
            BrowseAction::ImportOdbc(object) => {
                self.odbc_import = Some(odbc_import::Import::new(index, object));
            }
            BrowseAction::ImportOdbcSchema(schema) => {
                self.odbc_import = Some(odbc_import::Import::for_schema(index, schema));
            }
            BrowseAction::Script(script) => {
                self.design_tabs.select_query(workspace.key);
                workspace.new_tab(script, None);
            }
            BrowseAction::RunSql(script) => {
                self.design_tabs.select_query(workspace.key);
                workspace.new_tab(script.clone(), None);
                workspace.start_batch(vec![script]);
            }
            BrowseAction::Manage(operation) => {
                if workspace.busy() {
                    workspace.message("当前连接正在执行其他任务，请稍候再试。", false);
                } else if let objects::Operation::CreateObject {
                    schema,
                    kind: ObjectKind::Table,
                    ..
                } = operation
                {
                    self.create_table =
                        Some(create_table::Designer::open(index, schema, workspace));
                } else if let objects::Operation::TableStructure(object) = operation {
                    self.alter_table = Some(alter_table::Editor::open(index, object, workspace));
                } else if let objects::Operation::Index(object) = operation {
                    self.table_keys = Some(table_keys::Manager::open(
                        index,
                        object,
                        table_keys::Kind::Index,
                        workspace,
                    ));
                } else if let objects::Operation::ForeignKey(object) = operation {
                    self.table_keys = Some(table_keys::Manager::open(
                        index,
                        object,
                        table_keys::Kind::Foreign,
                        workspace,
                    ));
                } else {
                    self.object_plan = Some(objects::Plan::new(index, operation, workspace));
                }
            }
            BrowseAction::Backup { schema, object } => {
                self.transfer = Some(io_jobs::Form::new(
                    index,
                    io_jobs::Job::Backup {
                        schema,
                        object,
                        defaults: self.preferences.dump,
                    },
                ));
            }
            BrowseAction::Schedule { schema, object } => {
                self.job_manager
                    .open_backup(Some(&self.workspaces[index]), schema, object);
            }
            BrowseAction::Restore(schema) => {
                self.transfer = Some(io_jobs::Form::new(index, io_jobs::Job::Restore(schema)));
            }
            BrowseAction::SchemaHtml(schema) => {
                if workspace.busy() {
                    workspace.message("当前连接正在执行其他任务，请稍候再试。", false);
                } else {
                    self.transfer =
                        Some(io_jobs::Form::new(index, io_jobs::Job::SchemaHtml(schema)));
                }
            }
            BrowseAction::ExportPreview(object) => {
                self.transfer = Some(io_jobs::Form::new(index, io_jobs::Job::Export(object)));
            }
            BrowseAction::Copy { schema, object } => {
                self.copy = Some(copy::Copy::new(
                    index,
                    schema,
                    object,
                    &self.workspaces,
                    self.copy_destination.as_ref(),
                ));
            }
            BrowseAction::SyncData(object) => {
                self.data_sync = Some(data_sync::DataSync::new(index, object, &self.workspaces));
            }
            BrowseAction::SyncDatabase(schema) => {
                self.data_sync = Some(data_sync::DataSync::for_schema(
                    index,
                    schema,
                    &self.workspaces,
                ));
            }
            BrowseAction::EditRows(object) => {
                self.open_table_data(object, true);
            }
        }
    }

    fn open_table_data(&mut self, object: DbObject, explicit: bool) {
        let index = self.active;
        let workspace = &mut self.workspaces[index];
        let context = (workspace.key, workspace.tab().id);
        let entering = self.visible_data_tab != Some(context);
        let refresh_on_focus = self.preferences.refresh_table_data_on_focus;
        if workspace.server.is_none() {
            workspace.message("请先连接数据库。", true);
            return;
        }
        let same_table = workspace
            .tab()
            .data_editor
            .as_ref()
            .and_then(row_editor::RowEditor::object)
            == Some(&object);
        if !same_table && workspace.data_tab_protected(workspace.active_tab) {
            workspace.message("请先结束当前表数据操作，或保存、取消草稿。", false);
            return;
        }
        workspace.selected = Some(object.clone());
        workspace.output = state::Output::Data;
        self.show_output = true;
        self.design_tabs.select_query(workspace.key);
        if explicit {
            workspace.set_result_mode(output::ResultMode::Grid);
        }
        if !same_table {
            let mut editor = row_editor::RowEditor::new(index, object);
            editor.set_table_defaults(
                self.preferences.table_data_default_offset,
                self.preferences.table_data_default_limit,
                self.preferences.table_data_default_all,
            );
            workspace.tab_mut().data_editor = Some(editor);
        }
        workspace.with_data_editor(|workspace, editor| {
            let Some(editor) = editor else { return };
            let view_changed = editor.use_workspace_view(workspace);
            if same_table && entering && refresh_on_focus && !view_changed {
                editor.refresh_on_focus(workspace);
            }
            if explicit {
                editor.request_focus();
            }
            editor.poll(workspace);
        });
        self.visible_data_tab = Some(context);
    }

    fn sync_visible_data_tab(&mut self, explicit_focus: bool, modal: bool) {
        let workspace = &self.workspaces[self.active];
        let visible = (self.show_output
            && workspace.output == state::Output::Data
            && (!self.show_editor || self.active_layout().is_none()))
        .then_some((workspace.key, workspace.tab().id));
        let entering = self.visible_data_tab != visible;
        self.visible_data_tab = visible;
        if visible.is_none() || modal {
            return;
        }
        let workspace = &mut self.workspaces[self.active];
        let use_selected = workspace.data_entry_selection.take() == Some(workspace.tab().id);
        let selected = workspace
            .selected
            .as_ref()
            .filter(|object| object.kind == ObjectKind::Table);
        let displayed = workspace
            .tab()
            .data_editor
            .as_ref()
            .and_then(row_editor::RowEditor::object);
        if (use_selected || entering && displayed.is_none())
            && selected != displayed
            && let Some(object) = selected.cloned()
        {
            self.open_table_data(object, false);
            return;
        }
        if entering
            && let Some(object) = workspace
                .tab()
                .data_editor
                .as_ref()
                .and_then(row_editor::RowEditor::object)
                .cloned()
        {
            workspace.selected = Some(object);
        }
        workspace.with_data_editor(|workspace, editor| {
            let Some(editor) = editor else { return };
            let view_changed = entering && editor.use_workspace_view(workspace);
            if (entering || explicit_focus)
                && self.preferences.refresh_table_data_on_focus
                && !view_changed
            {
                editor.refresh_on_focus(workspace);
            }
            editor.poll(workspace);
        });
    }

    fn editor(&mut self, ctx: &egui::Context, interactive: bool) {
        self.editor_in(
            ctx,
            &mut workspace_windows::PanelHost::Root(ctx),
            self.active,
            interactive,
        );
    }

    fn editor_in(
        &mut self,
        ctx: &egui::Context,
        host: &mut workspace_windows::PanelHost<'_>,
        index: usize,
        interactive: bool,
    ) {
        let active = index == self.active;
        let execution_key = self.preferences.query_keys().0.name();
        let mut edit = None;
        let mut close_tab = None;
        let mut close_builder = None;
        let mut rename_tab = None;
        let mut design_action = None;
        let mut builder_action = None;
        let confirm_tab_close = self.preferences.confirm_tab_close;
        let quote_identifiers = self.preferences.query_design_quote_identifiers;
        let workspace = &mut self.workspaces[index];
        let key = workspace.key;
        let designs = &mut self.design_tabs;
        let panel = egui::CentralPanel::default().frame(style::bar(egui::Margin::same(0)));
        host.central(panel, |ui| {
            // 整行撑满宽度的 IDE 页签栏，激活页签与下方工具栏无缝融合
            egui::Frame::new()
                .fill(style::chrome())
                .inner_margin(egui::Margin {
                    left: 10,
                    right: 10,
                    top: 6,
                    bottom: 0,
                })
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    style::paint_bottom_hairline(ui);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        let mut next_query = None;
                        let mut next_layout = None;
                        let document = designs
                            .active(key)
                            .unwrap_or(Document::Query(workspace.tab().id));
                        let reveal = active
                            && self.visible_document.replace((key, document))
                                != Some((key, document));
                        egui::ScrollArea::horizontal()
                            .id_salt(("editor-tab-bar", key))
                            .auto_shrink([true, true])
                            .max_width((ui.available_width() - 40.0).max(96.0))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 4.0;
                                    for (tab_index, tab) in workspace.tabs.iter().enumerate() {
                                        let selected = document == Document::Query(tab.id);
                                        let (response, closed) = style::editor_tab(
                                            ui,
                                            ("query-tab", key, tab.id),
                                            &tab.label(),
                                            selected,
                                            style::accent(),
                                            true,
                                            "关闭查询",
                                        );
                                        let response = response.on_hover_ui(|ui| {
                                            ui.label(tab.title());
                                            if let Some(path) = &tab.path {
                                                ui.label(
                                                    RichText::new(path.to_string_lossy())
                                                        .small()
                                                        .color(style::muted()),
                                                );
                                            }
                                        });
                                        if reveal && selected {
                                            response.scroll_to_me(None);
                                        }
                                        if response.clicked() {
                                            next_query = Some(tab_index);
                                        }
                                        response.context_menu(|ui| {
                                            if ui.button("重命名查询页…").clicked() {
                                                rename_tab = Some(tab.id);
                                                ui.close();
                                            }
                                            if ui.button("关闭查询").clicked() {
                                                close_tab = Some(tab_index);
                                                ui.close();
                                            }
                                        });
                                        if closed {
                                            close_tab = Some(tab_index);
                                        }
                                    }
                                    for (tab, label) in designs.list(key) {
                                        let selected = document == tab;
                                        let (accent_color, closable, close_tip) = match tab {
                                            Document::Builder(_) => {
                                                (style::cyan(), true, "关闭查询设计")
                                            }
                                            Document::Design(_) => {
                                                (style::success(), false, "")
                                            }
                                            Document::Query(_) => (style::accent(), false, ""),
                                        };
                                        let (response, closed) = style::editor_tab(
                                            ui,
                                            ("design-tab", key, tab),
                                            &label,
                                            selected,
                                            accent_color,
                                            closable,
                                            close_tip,
                                        );
                                        if reveal && selected {
                                            response.scroll_to_me(None);
                                        }
                                        if response.clicked() {
                                            next_layout = Some(tab);
                                        }
                                        if closed
                                            && let Document::Builder(id) = tab
                                        {
                                            close_builder = Some(id);
                                        }
                                    }
                                });
                            });
                        if let Some(tab) = next_query {
                            workspace.select_tab(tab);
                            designs.select_query(key);
                            self.completion.close();
                            ctx.memory_mut(|memory| {
                                memory.request_focus(editor_tools::editor_id(workspace));
                            });
                        } else if let Some(tab) = next_layout {
                            designs.select(key, tab);
                        }
                        ui.add_space(2.0);
                        style::icon_menu_button(
                            ui,
                            style::ToolbarIcon::AddRow,
                            true,
                            "新建页签（查询 / 查询设计 / 架构设计）",
                            |ui| {
                                for (label, action) in [
                                    ("新建查询", Edit::New),
                                    ("新建查询设计", Edit::NewQueryDesign),
                                    ("新建架构设计", Edit::NewSchemaDesign),
                                ] {
                                    if ui.button(label).clicked() {
                                        edit = Some(action);
                                        ui.close();
                                    }
                                }
                            },
                        );
                    });
                });
            if let Some(Document::Design(id)) = designs.active(key)
                && let Some(designer) = designs.schema_mut(id)
            {
                design_action = ui
                    .push_id(("design-tab", id), |ui| {
                        designer.design_view(ui, workspace, interactive, confirm_tab_close)
                    })
                    .inner
                    .map(|action| (id, action));
                if active {
                    self.completion.close();
                }
                return;
            }
            if let Some(Document::Builder(id)) = designs.active(key)
                && let Some(builder) = designs.builder_mut(id)
            {
                builder_action = ui
                    .push_id(("query-design-tab", id), |ui| {
                        builder.design_view(ui, workspace, interactive, quote_identifiers)
                    })
                    .inner
                    .map(|action| (id, action));
                if active {
                    self.completion.close();
                }
                return;
            }
            style::bar(egui::Margin::symmetric(12, 6)).show(ui, |ui| {
                style::paint_bottom_hairline(ui);
                ui.horizontal(|ui| {
                    let ready = workspace.server.is_some() && !workspace.busy();
                    let cancellable = workspace.cancellable();
                    let stopping = workspace.editable_read_stopping();
                    let stop_batch = cancellable && workspace.batch_stop_button();
                    let stop_current = cancellable && !stop_batch;
                    let stop_label = if stopping {
                        "正在停止…"
                    } else {
                        "停止执行"
                    };
                    let stop_hint = "取消当前查询并停止执行剩余语句";
                    let current = ui
                        .add_enabled(
                            ready || (stop_current && !stopping),
                            if stop_current {
                                style::danger(stop_label)
                            } else {
                                style::primary("执行")
                            },
                        )
                        .on_hover_text(if stop_current {
                            stop_hint.to_owned()
                        } else {
                            format!("执行当前语句或选中的语句（{execution_key}）")
                        })
                        .clicked();
                    let batch = ui
                        .add_enabled(
                            ready || (stop_batch && !stopping),
                            if stop_batch {
                                style::danger(stop_label)
                            } else {
                                Button::new("批量执行")
                            },
                        )
                        .on_hover_text(if stop_batch {
                            stop_hint.to_owned()
                        } else {
                            format!("有选区时执行选区，否则执行整页（Shift+{execution_key}）")
                        })
                        .clicked();
                    if (current && stop_current) || (batch && stop_batch) {
                        workspace.stop_batch();
                    } else if current {
                        edit = Some(Edit::Current);
                    } else if batch {
                        edit = Some(Edit::Batch);
                    }
                    if ui.button("格式化").clicked() {
                        edit = Some(Edit::Text(editor_tools::Action::FormatAll));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        style::subtle_card(egui::Margin::symmetric(9, 2)).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new("结果行数上限").small().color(style::muted()),
                                );
                                ui.add(
                                    egui::DragValue::new(&mut workspace.result_limit)
                                        .range(1..=10_000)
                                        .speed(20),
                                );
                            });
                        });
                    });
                });
            });
            if active {
                self.editor_tools.show(ui, workspace);
            }
            let mut focus = active && std::mem::take(&mut workspace.focus_request);
            let id = editor_tools::editor_id(workspace);
            if focus {
                ui.memory_mut(|memory| memory.request_focus(id));
            }
            let completion_input =
                active.then(|| self.completion.keyboard(ui, id, workspace, interactive));
            let settings = self.preferences.editor;
            if interactive {
                editor_tools::indent_from_keyboard(ui, workspace, id, settings);
                focus |= std::mem::take(&mut workspace.focus_request);
            }
            let quoting = editor_tools::editor_quoting(workspace);
            let mut layouter = |ui: &egui::Ui, text: &dyn egui::TextBuffer, width: f32| {
                style::sql_galley(ui, text.as_str(), width, settings, quoting)
            };
            egui::ScrollArea::new([!settings.word_wrap, true])
                .id_salt(id.with("scroll"))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let tab = workspace.tab_mut();
                    let clipboard_start = settings
                        .changes_case()
                        .then(|| ui.output(|output| output.commands.len()));
                    let output = TextEdit::multiline(&mut tab.sql)
                        .id(id)
                        .layouter(&mut layouter)
                        .hint_text("输入 SQL 语句，或打开 SQL 文件…")
                        .code_editor()
                        .font(settings.font.id())
                        .desired_width(f32::INFINITY)
                        .margin(egui::Margin::symmetric(18, 14))
                        .frame(false)
                        .show(ui);
                    if active && output.response.changed() {
                        self.editor_tools.invalidate_wrap();
                    }
                    if focus {
                        output.response.request_focus();
                        if let Some(range) = output.cursor_range {
                            let cursor = output
                                .galley
                                .pos_from_cursor(range.primary)
                                .translate(output.galley_pos.to_vec2());
                            ui.scroll_to_rect(cursor, None);
                        }
                    }
                    if let Some(range) = output.cursor_range {
                        let chars = range.as_sorted_char_range();
                        let start = byte_index(&tab.sql, chars.start);
                        let end = byte_index(&tab.sql, chars.end);
                        tab.selection = (start < end).then_some(start..end);
                        tab.focused_byte = byte_index(&tab.sql, range.primary.index);
                    }
                    if let (Some(start), Some(selection)) = (clipboard_start, tab.selection.clone())
                    {
                        style::apply_case_to_clipboard(
                            ui, start, &tab.sql, selection, settings, quoting,
                        );
                    }
                    if let Some(completion_input) = completion_input {
                        self.completion.show(
                            ui,
                            workspace,
                            &output,
                            completion_input,
                            self.preferences.automatic_completion,
                        );
                    }
                });
        });
        if let Some(edit) = edit {
            self.edit(ctx, edit);
        }
        if let Some((id, action)) = builder_action {
            self.query_design_action(ctx, id, action);
        }
        if let Some((id, action)) = design_action {
            use schema_designer::Action;
            match action {
                Action::StageSql(sql) => {
                    self.design_tabs.select_query(key);
                    self.workspaces[index].new_tab(sql, None);
                }
                Action::Browse(action) => self.browse(ctx, action),
                Action::CreateTable(schema) => {
                    self.canvas_table_origin = Some(id);
                    self.create_table = Some(create_table::Designer::open_on_canvas(
                        index,
                        schema,
                        &mut self.workspaces[index],
                    ));
                }
                Action::OpenLayout(path) => self.open_schema_file(path),
                Action::ReturnToQuery => self.design_tabs.select_query(key),
            }
        }
        if let Some(tab) = close_tab {
            self.close_query_tab(ctx, index, tab);
        }
        if let Some(id) = close_builder {
            self.close_query_design(ctx, id);
        }
        if let Some(tab) = rename_tab {
            self.begin_query_rename(index, tab);
        }
    }

    fn edit(&mut self, ctx: &egui::Context, edit: Edit) {
        let index = self.active;
        let key = self.workspaces[index].key;
        match edit {
            Edit::ArrangeWorkspaces(mode) => {
                if mode == workspace_windows::Arrangement::Maximized {
                    self.workspace_windows.restore(key);
                }
                self.workspace_windows.arrange(mode);
                self.workspace_regions.clear();
                ctx.request_repaint();
                return;
            }
            Edit::ArrangeWorkspaceIcons => {
                self.icon_revision = self.icon_revision.wrapping_add(1);
                ctx.request_repaint();
                return;
            }
            Edit::MinimizeWorkspace => {
                self.workspace_windows.minimize(key);
                if let Some(next) = self
                    .workspaces
                    .iter()
                    .position(|ws| !self.workspace_windows.minimized.contains(&ws.key))
                {
                    self.activate_workspace(ctx, next);
                }
                self.workspace_regions.retain(|region| region.key != key);
                ctx.request_repaint();
                return;
            }
            Edit::SelectWorkspace(index) => {
                self.activate_workspace(ctx, index);
                return;
            }
            _ => {}
        }
        if matches!(edit, Edit::Editable) && self.workspaces[index].cancellable() {
            self.workspaces[index].stop_batch();
            return;
        }
        if let Some(layout) = self.design_tabs.active(key) {
            if matches!(edit, Edit::Save | Edit::SaveAs) {
                let save_as = matches!(edit, Edit::SaveAs);
                match layout {
                    Document::Design(id) => {
                        let workspace = &mut self.workspaces[index];
                        if !workspace.busy()
                            && let Some(designer) = self.design_tabs.schema_mut(id)
                        {
                            designer.save(workspace, save_as);
                        }
                    }
                    Document::Builder(id) => {
                        if let Some(builder) = self.design_tabs.builder_mut(id) {
                            builder.save_layout(save_as);
                        }
                    }
                    Document::Query(_) => unreachable!(),
                }
                return;
            }
            if matches!(
                edit,
                Edit::Current
                    | Edit::Batch
                    | Edit::Selection
                    | Edit::Editable
                    | Edit::ExportDisplayedRows
                    | Edit::FindResult { .. }
                    | Edit::SqlTemplate
                    | Edit::Explain
                    | Edit::Profile
                    | Edit::Text(_)
                    | Edit::Complete(_)
                    | Edit::AddFavorite
            ) {
                self.workspaces[index].message("请先选择查询页。", true);
                return;
            }
        }
        let ws = &mut self.workspaces[index];
        if !matches!(edit, Edit::Complete(_)) {
            self.completion.close();
        }
        match edit {
            Edit::ArrangeWorkspaces(_)
            | Edit::ArrangeWorkspaceIcons
            | Edit::MinimizeWorkspace
            | Edit::SelectWorkspace(_) => unreachable!(),
            Edit::RefreshExplorer => {
                if ws.server.is_some() {
                    self.browse(ctx, BrowseAction::RefreshAll);
                }
            }
            Edit::OpenTableData => {
                if let Some(object) = ws
                    .selected
                    .as_ref()
                    .filter(|object| object.kind == ObjectKind::Table)
                    .cloned()
                {
                    self.open_table_data(object, true);
                } else {
                    ws.message("请先在对象浏览器中选择一张表。", false);
                }
            }
            Edit::ExportDisplayedRows => {
                if can_export_displayed_rows(ws) && request_displayed_rows_export(ws) {
                    self.show_output = true;
                } else {
                    ws.message("请先打开已加载的查询结果或表数据。", false);
                }
            }
            Edit::NewConnection => self.new_connection(None),
            Edit::CloneConnection => {
                if ws.busy() {
                    ws.message("当前连接正在执行任务，完成后可新建连接。", false);
                } else if ws.server.is_none() {
                    ws.message("请先建立连接。", false);
                } else {
                    ws.send(Command::CloneConnection, "正在读取当前连接设置…");
                }
            }
            Edit::DisconnectConnection => ws.disconnect(),
            Edit::DisconnectAll => self.disconnect_all(),
            Edit::ObjectFile(action) => match action.resolve(ws) {
                Ok(action) => self.browse(ctx, action),
                Err(error) => ws.message(error, true),
            },
            Edit::Navigate(action) => self.navigate(ctx, action),
            Edit::FlushServer => {
                if ws.server.is_none() {
                    ws.message("请先连接数据库服务器。", true);
                } else if ws.busy() {
                    ws.message("当前连接正在执行其他任务，请稍候再试。", false);
                } else {
                    self.admin = Some(admin::Admin::flush(ws.key));
                }
            }
            Edit::Complete(mode) => {
                self.show_editor = true;
                ws.focus_request = true;
                self.completion.open(mode, ws);
            }
            Edit::Current
            | Edit::Batch
            | Edit::Selection
            | Edit::Editable
            | Edit::Explain
            | Edit::Profile => {
                if ws.busy() {
                    return;
                }
                let tab = ws.tab();
                let script = match tab.selection.clone() {
                    Some(selection) if !selection.is_empty() => {
                        let Some(selected) = tab.sql.get(selection) else {
                            ws.message("选区已改变，请重新选择要执行的语句。", true);
                            return;
                        };
                        Some(selected)
                    }
                    _ if matches!(edit, Edit::Batch) => Some(tab.sql.as_str()),
                    _ if matches!(edit, Edit::Selection) => return,
                    _ => None,
                };
                let batch_scope = script.is_some();
                let statements: Vec<_> = script
                    .map(split_script)
                    .unwrap_or_else(|| {
                        statement_at_for_execution(&tab.sql, tab.focused_byte)
                            .into_iter()
                            .collect()
                    })
                    .into_iter()
                    .map(|part| part.sql.to_owned())
                    .collect();
                if matches!(edit, Edit::Editable) && !batch_scope {
                    if let [sql] = statements.as_slice() {
                        if ws.start_f8(index, sql.clone(), ws.result_limit) {
                            ws.output = state::Output::Results;
                            self.show_output = true;
                        }
                    } else {
                        ws.message("光标处没有可执行的 SQL 语句。", true);
                    }
                } else if matches!(edit, Edit::Explain) {
                    if let [statement] = statements.as_slice() {
                        ws.start_editor_batch(vec![format!("EXPLAIN {statement}")]);
                    } else {
                        ws.message("请选择一条语句查看执行计划。", true);
                    }
                } else if matches!(edit, Edit::Profile) {
                    if let [sql] = statements.as_slice()
                        && is_single_profile_select(sql)
                    {
                        ws.start_profiled_query(
                            sql.clone(),
                            self.preferences.profile_options,
                            true,
                        );
                    } else {
                        ws.message("请选择一条 SELECT 语句进行分析。", true);
                    }
                } else if self.preferences.profile_selects
                    && ws.server.as_ref().is_some_and(|server| !server.http_tunnel)
                    && let [sql] = statements.as_slice()
                    && is_single_profile_select(sql)
                {
                    ws.start_profiled_query(sql.clone(), self.preferences.profile_options, false);
                } else if matches!(edit, Edit::Current | Edit::Selection)
                    && let [sql] = statements.as_slice()
                    && is_single_profile_select(sql)
                {
                    // 单条 SELECT 默认以可切换目标表的只读结果返回（成功后切到结果页，出错自动切到消息页）
                    if ws.start_f8_with_mode(index, sql.clone(), ws.result_limit, true) {
                        self.show_output = true;
                    }
                } else {
                    ws.start_editor_batch(statements);
                }
                if batch_scope {
                    ws.use_batch_stop_button();
                }
            }
            Edit::Text(action) => {
                self.editor_tools
                    .action(action, ctx, ws, self.preferences.formatter)
            }
            Edit::SqlTemplate => {
                self.sql_template = Some(SqlTemplateRequest {
                    workspace: ws.key,
                    tab: ws.tab().id,
                    dialog: sql_templates::Dialog::new(),
                });
            }
            Edit::FindResult { next } => match ws.output {
                state::Output::Results if ws.tab().editable_editor.is_some() => {
                    ws.with_editable_editor(|_, editor| {
                        if let Some(editor) = editor {
                            editor.text_search(next);
                        }
                    });
                }
                state::Output::Results => {
                    let tab = ws.tab_mut();
                    tab.result_views.text_search(tab.set, next);
                }
                state::Output::Data => {
                    ws.with_data_editor(|_, editor| {
                        if let Some(editor) = editor {
                            editor.text_search(next);
                        }
                    });
                }
                _ => {}
            },
            Edit::AddFavorite => self.favorites.add(ws.tab()),
            Edit::New => {
                self.design_tabs.select_query(key);
                ws.new_tab(String::new(), None);
            }
            Edit::NewSchemaDesign => self.new_schema_design(),
            Edit::ShowHistory => {
                self.design_tabs.select_query(key);
                self.show_output = true;
                ws.output = state::Output::History;
            }
            Edit::TableDiagnostics => {
                self.table_maintenance = table_maintenance::Dialog::open(ws, None, None);
                if self.table_maintenance.is_none() {
                    ws.message("请先连接数据库。", true);
                }
            }
            Edit::NewQueryDesign => {
                let schema = ws
                    .server
                    .as_ref()
                    .and_then(|server| server.database.clone());
                self.design_tabs
                    .new_builder(key, query_builder::Builder::new_blank(schema));
                self.show_editor = true;
            }
            Edit::OpenQueryDesign => {
                if let Some(path) = query_builder::pick_layout() {
                    self.open_query_design(path);
                }
            }
            Edit::Open | Edit::OpenSame => {
                if let Some(path) = rfd::FileDialog::new()
                    .set_title("打开 SQL 文件")
                    .add_filter("SQL 文件", &["sql", "txt"])
                    .pick_file()
                {
                    self.open_sql_file(index, path, matches!(edit, Edit::OpenSame));
                }
            }
            Edit::OpenSchemaDesign => {
                if let Some(path) = schema_designer::pick_layout() {
                    self.open_schema_file(path);
                }
            }
            Edit::Save | Edit::SaveAs => {
                let tab = ws.active_tab;
                self.save_query_tab(index, tab, matches!(edit, Edit::SaveAs));
            }
        }
    }

    fn open_query_design(&mut self, path: PathBuf) {
        let workspace = &mut self.workspaces[self.active];
        if let Err(error) = self.design_tabs.open_builder_file(workspace.key, path) {
            workspace.message(format!("无法打开查询布局。{error}"), true);
            return;
        }
        self.show_editor = true;
        self.completion.close();
    }

    fn query_design_action(&mut self, ctx: &egui::Context, id: u64, action: query_builder::Action) {
        use query_builder::{Action, Destination};
        if self.active_layout() != Some(Document::Builder(id)) {
            return;
        }
        match action {
            Action::OpenLayout => self.edit(ctx, Edit::OpenQueryDesign),
            Action::StageSql { sql, destination } => {
                let workspace = &mut self.workspaces[self.active];
                let Some(builder) = self.design_tabs.builder_mut(id) else {
                    return;
                };
                match destination {
                    Destination::ViewDraft => workspace.new_tab(sql, None),
                    Destination::NewQuery => {
                        workspace.new_tab(sql, None);
                        builder.link_query(workspace.tab().id);
                    }
                    Destination::LinkedQuery => {
                        let Some((index, edited)) = builder.linked_query(workspace) else {
                            return;
                        };
                        if edited {
                            workspace.new_tab(sql, None);
                            builder.link_query(workspace.tab().id);
                            workspace.message("关联查询页已有手动修改，已另开查询页。", false);
                        } else {
                            workspace.select_tab(index);
                            let end = workspace.tab().sql.len();
                            editor_tools::change(ctx, workspace, 0..end, &sql, false);
                            workspace.tab_mut().saved = sql;
                            workspace.output = state::Output::Results;
                        }
                    }
                }
                self.design_tabs.select_query(workspace.key);
                self.completion.close();
                self.focus_active_query(ctx);
            }
        }
    }

    fn save_query_tab(&mut self, index: usize, tab_index: usize, save_as: bool) -> bool {
        let ws = &mut self.workspaces[index];
        let quoting = editor_tools::editor_quoting(ws);
        let Some(tab) = ws.tabs.get_mut(tab_index) else {
            return false;
        };
        let path = if save_as { None } else { tab.path.clone() };
        let Some(path) = path.or_else(|| {
            rfd::FileDialog::new()
                .set_title("保存 SQL 文件")
                .add_filter("SQL 脚本", &["sql"])
                .set_file_name("查询.sql")
                .save_file()
        }) else {
            return false;
        };
        let text = style::sql_file_text(&tab.sql, self.preferences.editor, quoting);
        if let Err(error) = state::write_atomic(&path, text.as_bytes()) {
            ws.message(format!("无法保存 SQL 文件：{error}"), true);
            return false;
        }
        tab.path = Some(path.clone());
        tab.saved.clone_from(&tab.sql);
        ws.message("已保存 SQL 文件", false);
        self.record_recent(index, RecentKind::Sql, &path);
        true
    }

    fn open_sql_file(&mut self, index: usize, path: PathBuf, current_tab: bool) {
        if current_tab
            && self.workspaces[index].row_editor_tab_protected(self.workspaces[index].active_tab)
        {
            let workspace = &mut self.workspaces[index];
            workspace.output = if workspace.editable_result_protected(workspace.active_tab) {
                state::Output::Results
            } else {
                state::Output::Data
            };
            self.show_output = true;
            self.design_tabs.select_query(workspace.key);
            workspace.message(
                "请先结束数据编辑操作，或保存、取消草稿再替换查询页。",
                false,
            );
            return;
        }
        let sql = match read_sql_file(&path) {
            Ok(sql) => sql,
            Err(error) => {
                self.workspaces[index].message(format!("无法打开 SQL 文件：{error}"), true);
                return;
            }
        };
        let ws = &mut self.workspaces[index];
        if current_tab {
            let tab = ws.tab();
            if tab.sql != tab.saved {
                self.discard = Some(Discard::OpenSql {
                    workspace: index,
                    tab: tab.id,
                    sql,
                    path,
                });
                return;
            }
            if !ws.replace_sql_file(tab.id, sql, path.clone()) {
                return;
            }
        } else {
            ws.new_tab(sql, Some(path.clone()));
        }
        self.design_tabs.select_query(ws.key);
        self.show_editor = true;
        self.record_recent(index, RecentKind::Sql, &path);
    }

    fn connected_design_workspace(&self) -> Option<usize> {
        if self.workspaces[self.active].server.is_some() {
            Some(self.active)
        } else {
            self.workspaces.iter().position(|ws| ws.server.is_some())
        }
    }

    fn new_schema_design(&mut self) {
        let Some(index) = self.connected_design_workspace() else {
            self.ws().message("请先连接数据库服务器。", true);
            return;
        };
        let workspace = &self.workspaces[index];
        let schema = workspace
            .server
            .as_ref()
            .and_then(|server| server.database.clone())
            .or_else(|| workspace.schemas.first().cloned())
            .unwrap_or_default();
        self.design_tabs.new_design(workspace.key, schema, None);
        self.active = index;
        self.show_editor = true;
        self.completion.close();
    }

    fn open_schema_file(&mut self, path: PathBuf) {
        let Some(index) = self.connected_design_workspace() else {
            self.ws()
                .message("请先连接数据库服务器，再打开架构布局。", true);
            return;
        };
        let workspace = &mut self.workspaces[index];
        if let Err(error) = self.design_tabs.open_schema_file(workspace.key, path) {
            workspace.message(format!("无法打开架构布局：{error}"), true);
            return;
        }
        self.active = index;
        self.show_editor = true;
        self.completion.close();
    }

    fn record_recent(&mut self, workspace: usize, kind: RecentKind, path: &Path) {
        if let Err(error) = self.recent_files.record(kind, path) {
            self.workspaces[workspace].message(format!("无法保存最近文件列表：{error}"), true);
        }
    }

    fn apply_favorite(&mut self, ctx: &egui::Context, action: favorites::EditorAction) {
        let index = self.active;
        let workspace = &mut self.workspaces[index];
        self.design_tabs.select_query(workspace.key);
        match action {
            favorites::EditorAction::Insert(sql) => {
                self.show_editor = true;
                editor_tools::insert_text(ctx, workspace, &sql, true);
            }
            favorites::EditorAction::Open { sql, path } => {
                self.show_editor = true;
                workspace.new_tab(sql, path);
            }
        }
    }

    fn insert_sql_template(
        &mut self,
        ctx: &egui::Context,
        workspace: u64,
        tab: u64,
        snippet: &str,
    ) {
        let target = self
            .workspaces
            .iter()
            .position(|current| current.key == workspace)
            .and_then(|index| {
                self.workspaces[index]
                    .tabs
                    .iter()
                    .position(|query| query.id == tab)
                    .map(|tab_index| (index, tab_index))
            });
        let Some((index, tab_index)) = target else {
            self.ws()
                .message("原查询页已关闭，请重新选择查询页和模板。", false);
            return;
        };
        self.activate_workspace(ctx, index);
        self.design_tabs.select_query(workspace);
        self.show_editor = true;
        let ws = &mut self.workspaces[index];
        ws.select_tab(tab_index);
        editor_tools::insert_text(ctx, ws, snippet, true);
    }

    fn confirm_discard(&mut self, ctx: &egui::Context) {
        enum Choice {
            Edit,
            Discard,
            Save,
        }

        if self.discard.is_none() {
            return;
        }
        let closing_document = matches!(
            self.discard,
            Some(Discard::Tab { .. } | Discard::QueryDesign(_))
        );
        let mut choice = None;
        let dialog = crate::app::style::modal(ctx, egui::Id::new("discard-sql")).show(ctx, |ui| {
            ui.heading(match self.discard {
                Some(Discard::Tab { .. }) => "保存查询修改？",
                Some(Discard::QueryDesign(_)) => "保存查询布局？",
                _ => "放弃未保存的更改吗？",
            });
            ui.horizontal(|ui| {
                if ui.button("继续编辑").clicked() {
                    choice = Some(Choice::Edit);
                }
                let discard = if closing_document {
                    "不保存"
                } else {
                    "放弃修改"
                };
                if ui
                    .button(RichText::new(discard).color(style::error()))
                    .clicked()
                {
                    choice = Some(Choice::Discard);
                }
                if closing_document && ui.add(style::primary("保存并关闭")).clicked() {
                    choice = Some(Choice::Save);
                }
            });
        });
        if closing_document && choice.is_none() && dialog.should_close() {
            choice = Some(Choice::Edit);
        }
        let Some(choice) = choice else { return };
        let target = self.discard.take().expect("discard target");
        match target {
            Discard::Tab { workspace, tab } => {
                if matches!(choice, Choice::Discard)
                    || matches!(choice, Choice::Save) && self.save_query_tab(workspace, tab, false)
                {
                    self.workspaces[workspace].close_tab(tab);
                }
                self.focus_active_query(ctx);
            }
            Discard::QueryDesign(id) => {
                if matches!(choice, Choice::Discard)
                    || matches!(choice, Choice::Save)
                        && self
                            .design_tabs
                            .builder_mut(id)
                            .is_some_and(|builder| builder.save_layout(false))
                {
                    self.design_tabs.remove_builder(id);
                    self.focus_active_query(ctx);
                }
            }
            Discard::Workspace(index) if matches!(choice, Choice::Discard) => {
                self.close_workspace(index)
            }
            Discard::OpenSql {
                workspace,
                tab,
                sql,
                path,
            } if matches!(choice, Choice::Discard) => {
                if self.workspaces[workspace].replace_sql_file(tab, sql, path.clone()) {
                    self.design_tabs
                        .select_query(self.workspaces[workspace].key);
                    self.show_editor = true;
                    self.record_recent(workspace, RecentKind::Sql, &path);
                }
            }
            Discard::Quit if matches!(choice, Choice::Discard) => {
                self.quitting = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            _ => {}
        }
    }
}

impl eframe::App for Workbench {
    fn on_exit(&mut self, _: Option<&eframe::glow::Context>) {
        let maximized = self.workspace_windows.mode == workspace_windows::Arrangement::Maximized;
        if maximized != self.preferences.workspace_maximized
            && let Err(error) = preferences::Preferences::save_window_state(maximized)
        {
            eprintln!("无法保存连接窗口状态。{error}");
        }
    }

    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.select_workspace_at_pointer(ctx);
        let completed_query = self.poll_workspaces();
        self.receive_cloned_connections();
        for (workspace, path) in self.design_tabs.poll(&mut self.workspaces) {
            if let Some(index) = self.workspaces.iter().position(|ws| ws.key == workspace) {
                self.record_recent(index, RecentKind::SchemaDesign, &path);
            }
        }
        let visible_layout = self.active_layout().filter(|_| self.show_editor);
        self.schedule.tick(
            ctx,
            &mut self.workspaces,
            self.copy.is_none()
                && self.data_sync.is_none()
                && self.transfer.is_none()
                && self.create_table.is_none()
                && self.alter_table.is_none()
                && self.table_keys.is_none()
                && !matches!(visible_layout, Some(Document::Design(_))),
            self.preferences.dump,
        );
        if !self.quitting && ctx.input(|input| input.viewport().close_requested()) {
            if let Some(index) = self
                .workspaces
                .iter()
                .position(|workspace| workspace.protected_row_editor_tab().is_some())
            {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.reveal_protected_row_editor(
                    index,
                    "请先结束数据编辑操作，或保存、取消草稿再退出。",
                );
            } else if self
                .workspaces
                .iter()
                .any(|ws| ws.tabs.iter().any(|tab| tab.sql != tab.saved))
                || self.design_tabs.dirty(None)
            {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.discard = Some(Discard::Quit);
            }
        }
        let modal = self.connection.is_some()
            || self.query_rename.is_some()
            || self.favorites.active()
            || self.preferences_dialog.active()
            || self.help.is_some()
            || self.job_manager.active()
            || self.discard.is_some()
            || self.import.is_some()
            || self.admin.is_some()
            || self.processes.is_some()
            || self.server_values.is_some()
            || self.sql_template.is_some()
            || self.compare.is_some()
            || self.object_plan.is_some()
            || self.create_table.is_some()
            || self.alter_table.is_some()
            || self.table_keys.is_some()
            || self.table_maintenance.is_some()
            || self.row_editor.is_some()
            || self.transfer.is_some()
            || self.copy.is_some()
            || self.data_sync.is_some()
            || self.odbc_import.is_some()
            || self.schedule.open_now();
        let workspace = &self.workspaces[self.active];
        let output_modal = output::has_modal(workspace);
        let modal = modal || (self.show_output && visible_layout.is_none() && output_modal);
        if !modal && visible_layout.is_none() {
            self.open_duplicate_result();
        }
        let modal = modal || self.row_editor.is_some();
        if !modal && ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::F1)) {
            self.help = Some(help::Page::Guide);
        }
        let modal = modal || self.help.is_some();
        let completion_open =
            self.show_editor && self.completion.list_open(&self.workspaces[self.active]);
        let workspace = &self.workspaces[self.active];
        let history_focused = self.show_output
            && workspace.output == state::Output::History
            && workspace.history.has_focus(ctx);
        let result_text_focused =
            self.show_output && visible_layout.is_none() && result_text_has_focus(workspace, ctx);
        let sql_editor_focused = self.show_editor
            && ctx.memory(|memory| memory.has_focus(editor_tools::editor_id(workspace)));
        let (focus, navigate) = match (!modal).then(|| navigation::shortcut(ctx)).flatten() {
            Some(navigation::Shortcut::Focus(pane)) => (Some(pane), None),
            Some(navigation::Shortcut::Navigate(action)) => (None, Some(action)),
            None => (None, None),
        };
        let shortcut = if modal || focus.is_some() {
            None
        } else if let Some(action) = navigate {
            Some(Edit::Navigate(action))
        } else if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::F11)) {
            Some(Edit::OpenTableData)
        } else if let Some(edit) = ctx.input_mut(|input| {
            consume_exact_shortcut(input, &CONNECTION_SHORTCUTS)
                .or_else(|| consume_exact_shortcut(input, &OBJECT_FILE_SHORTCUTS))
                .or_else(|| consume_exact_shortcut(input, &QUERY_DESIGN_SHORTCUTS))
                .or_else(|| {
                    (visible_layout.is_none() && consume_f8(input)).then_some(Edit::Editable)
                })
                .or_else(|| {
                    consume_query_shortcut(
                        input,
                        self.preferences.query_keys(),
                        visible_layout.is_none(),
                    )
                })
        }) {
            Some(edit)
        } else if ctx
            .input_mut(|input| input.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::H))
        {
            Some(Edit::ShowHistory)
        } else if let Some(layout) = visible_layout {
            ctx.input_mut(|input| {
                [
                    (
                        Modifiers::COMMAND | Modifiers::ALT,
                        Key::D,
                        Edit::NewSchemaDesign,
                    ),
                    (Modifiers::COMMAND, Key::T, Edit::New),
                ]
                .into_iter()
                .find_map(|(modifiers, key, edit)| {
                    input.consume_key(modifiers, key).then_some(edit)
                })
                .or_else(|| {
                    if !matches!(layout, Document::Builder(_)) {
                        return None;
                    }
                    [
                        (Modifiers::COMMAND, Key::O, Edit::OpenQueryDesign),
                        (Modifiers::COMMAND | Modifiers::SHIFT, Key::S, Edit::SaveAs),
                        (Modifiers::COMMAND, Key::S, Edit::Save),
                    ]
                    .into_iter()
                    .find_map(|(modifiers, key, edit)| {
                        input.consume_key(modifiers, key).then_some(edit)
                    })
                })
            })
        } else {
            ctx.input_mut(|input| {
                if let Some(edit) = consume_exact_shortcut(input, &FORMAT_SHORTCUTS) {
                    return Some(edit);
                }
                if !result_text_focused
                    && let Some(edit) = consume_exact_shortcut(
                        input,
                        &[(
                            Key::F3,
                            Modifiers::NONE,
                            Edit::Text(editor_tools::Action::Find),
                        )],
                    )
                {
                    return Some(edit);
                }
                [
                    (
                        Modifiers::COMMAND | Modifiers::ALT,
                        Key::D,
                        Edit::NewSchemaDesign,
                    ),
                    (
                        Modifiers::CTRL | Modifiers::ALT,
                        Key::T,
                        Edit::TableDiagnostics,
                    ),
                    (
                        Modifiers::COMMAND | Modifiers::SHIFT,
                        Key::F,
                        Edit::AddFavorite,
                    ),
                    (
                        Modifiers::COMMAND,
                        Key::F,
                        Edit::Text(editor_tools::Action::Find),
                    ),
                    (
                        Modifiers::CTRL,
                        Key::H,
                        Edit::Text(editor_tools::Action::Replace),
                    ),
                    (
                        Modifiers::COMMAND,
                        Key::G,
                        Edit::Text(editor_tools::Action::GoTo),
                    ),
                    (Modifiers::COMMAND, Key::T, Edit::New),
                    (Modifiers::COMMAND | Modifiers::ALT, Key::O, Edit::OpenSame),
                    (Modifiers::COMMAND, Key::O, Edit::Open),
                    (Modifiers::COMMAND | Modifiers::SHIFT, Key::S, Edit::SaveAs),
                    (Modifiers::COMMAND, Key::S, Edit::Save),
                ]
                .into_iter()
                .find_map(|(modifiers, key, edit)| {
                    if history_focused && matches!(edit, Edit::Save) {
                        return None;
                    }
                    if !sql_editor_focused
                        && matches!(edit, Edit::Text(editor_tools::Action::Replace))
                    {
                        return None;
                    }
                    if result_text_focused
                        && matches!(
                            edit,
                            Edit::Text(editor_tools::Action::Find | editor_tools::Action::Next)
                        )
                    {
                        return None;
                    }
                    input.consume_key(modifiers, key).then_some(edit)
                })
                .or_else(|| {
                    (!completion_open && input.consume_key(Modifiers::COMMAND, Key::Enter))
                        .then_some(Edit::Current)
                })
            })
        };
        let clipboard = self.top_bar(ctx);
        self.workspace_bar(ctx);
        let maximized = self.workspace_windows.mode == workspace_windows::Arrangement::Maximized;
        if maximized {
            self.footer(ctx);
            if self.show_explorer {
                self.sidebar(ctx, focus == Some(navigation::Pane::Explorer));
            }
        }
        let focus = focus.or_else(|| {
            if modal || shortcut.is_some() {
                return None;
            }
            self.focus_after_query(ctx, completed_query?)
        });
        if focus == Some(navigation::Pane::Editor) {
            self.focus_active_query(ctx);
        }
        if maximized {
            let layout_visible = self.show_editor && self.active_layout().is_some();
            if self.show_output && !layout_visible {
                let full = !self.show_editor;
                let settings = self.preferences.editor;
                output::panel(
                    ctx,
                    &mut self.workspaces[self.active],
                    full,
                    !modal,
                    settings,
                    focus == Some(navigation::Pane::Output),
                    &mut self.grid_widths,
                );
            }
            self.sync_visible_data_tab(focus == Some(navigation::Pane::Output), modal);
            if self.show_editor {
                self.editor(ctx, !modal);
            }
        } else {
            if !self
                .workspace_windows
                .minimized
                .contains(&self.workspaces[self.active].key)
            {
                self.sync_visible_data_tab(focus == Some(navigation::Pane::Output), modal);
            }
            self.arranged_workspaces(ctx, focus, modal);
        }
        if !self.show_editor {
            self.completion.close();
        }
        if let Some(action) = shortcut {
            self.edit(ctx, action);
        }
        if !modal && let Some(clipboard) = clipboard {
            clipboard.send(ctx);
        }
        let current_workspace = self.workspaces[self.active].key;
        let current_query = self.workspaces[self.active].tab().id;
        let mut keep_canvas = false;
        self.connection_dialog(ctx);
        self.confirm_discard(ctx);
        self.query_rename_dialog(ctx);
        tools::import_dialog(ctx, &mut self.import, &mut self.workspaces);
        admin::dialog(ctx, &mut self.admin, &mut self.workspaces);
        processes::dialog(ctx, &mut self.processes, &mut self.workspaces);
        server_values::dialog(ctx, &mut self.server_values, &mut self.workspaces);
        error_details::dialog(ctx, &mut self.workspaces[self.active].error_dialog);
        if let Some(choice) = self
            .sql_template
            .as_mut()
            .and_then(|request| request.dialog.show(ctx))
        {
            let request = self.sql_template.take().expect("opened SQL templates");
            if let sql_templates::Choice::Insert(snippet) = choice {
                self.insert_sql_template(ctx, request.workspace, request.tab, snippet);
            }
        }
        if let Some(index) = compare::dialog(ctx, &mut self.compare, &mut self.workspaces) {
            self.active = index;
            self.design_tabs.select_query(self.workspaces[index].key);
        }
        objects::dialog(ctx, &mut self.object_plan, &mut self.workspaces);
        if let Some(outcome) =
            create_table::dialog(ctx, &mut self.create_table, &mut self.workspaces)
            && let Some(id) = self.canvas_table_origin.take()
            && let Some(designer) = self.design_tabs.schema_mut(id)
        {
            keep_canvas = outcome.creation.is_some();
            designer.return_from_create(outcome.workspace, outcome.creation);
            if outcome.show_sql {
                self.design_tabs.select_query(outcome.workspace);
            }
        }
        if self.create_table.is_none() {
            self.canvas_table_origin = None;
        }
        alter_table::dialog(ctx, &mut self.alter_table, &mut self.workspaces);
        table_keys::dialog(ctx, &mut self.table_keys, &mut self.workspaces);
        match table_maintenance::dialog(ctx, &mut self.table_maintenance, &mut self.workspaces) {
            Some(table_maintenance::Outcome::Editor) => {
                self.design_tabs
                    .select_query(self.workspaces[self.active].key);
                self.show_editor = true;
            }
            Some(table_maintenance::Outcome::Results) => {
                self.design_tabs
                    .select_query(self.workspaces[self.active].key);
                self.show_output = true;
            }
            None => {}
        }
        row_editor::dialog(
            ctx,
            &mut self.row_editor,
            &mut self.workspaces,
            self.preferences.editor,
            &mut self.grid_widths,
        );
        io_jobs::dialog(ctx, &mut self.transfer, &mut self.workspaces);
        copy::dialog(
            ctx,
            &mut self.copy,
            &mut self.workspaces,
            &mut self.copy_destination,
        );
        if let Some(index) = data_sync::dialog(ctx, &mut self.data_sync, &mut self.workspaces) {
            self.active = index;
            self.design_tabs.select_query(self.workspaces[index].key);
        }
        self.schedule.dialog(ctx, &self.workspaces);
        self.job_manager
            .dialog(ctx, &self.profiles, self.workspaces.get(self.active));
        odbc_import::dialog(ctx, &mut self.odbc_import, &mut self.workspaces);
        if let Some(action) = self.favorites.dialog(ctx) {
            self.apply_favorite(ctx, action);
        }
        if let Some(mut preferences) = self.preferences_dialog.dialog(ctx) {
            for workspace in &mut self.workspaces {
                if workspace.result_limit == self.preferences.default_result_limit {
                    workspace.result_limit = preferences.default_result_limit;
                }
                workspace.copy_column_headers = preferences.copy_column_headers;
                workspace.show_data_info = preferences.show_data_info;
            }
            ctx.set_zoom_factor(preferences.zoom_factor());
            self.grid_widths
                .set_enabled(preferences.remember_grid_widths);
            self.grid_widths
                .set_data_sizing(preferences.truncate_headers_to_data_width);
            for error in style::apply_font_preferences(ctx, &mut preferences) {
                self.ws().message(error, true);
            }
            self.preferences = preferences;
        }
        help::dialog(ctx, &mut self.help, &self.preferences);
        if let Err(error) = self.grid_widths.save_pending(ctx) {
            self.ws().message(error, true);
        }
        if !keep_canvas
            && self.workspaces[self.active].key == current_workspace
            && self.workspaces[self.active].tab().id != current_query
        {
            self.design_tabs.select_query(current_workspace);
        }
        if !modal
            && self.show_editor
            && let Some(Document::Builder(id)) = self.active_layout()
            && let Some(builder) = self.design_tabs.builder_mut(id)
            && builder.load_metadata(&mut self.workspaces[self.active])
        {
            ctx.request_repaint();
        }
        for workspace in &mut self.workspaces {
            completion::schedule_rebuild(workspace);
            completion::poll_cache(
                workspace,
                self.preferences.completion_tags_directory.as_deref(),
                ctx,
            );
        }
        // Give user commands and persisted tags priority over live completion reads.
        self.completion
            .schedule_read(&mut self.workspaces[self.active]);
        if self
            .workspaces
            .iter()
            .any(|workspace| workspace.busy() || workspace.tags.active_rebuild())
        {
            ctx.request_repaint_after(Duration::from_millis(25));
        }
    }
}

fn field(ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str) {
    ui.label(RichText::new(label).color(style::muted()));
    ui.add(
        TextEdit::singleline(value)
            .desired_width(310.0)
            .hint_text(hint),
    );
    ui.end_row();
}

fn http_connection_fields(ui: &mut egui::Ui, form: &mut ConnectionForm) {
    let Some(profile) = &mut form.profile.http else {
        return;
    };
    egui::ScrollArea::vertical()
        .id_salt("http-settings")
        .max_height(215.0)
        .show(ui, |ui| {
            egui::Grid::new("http-fields")
                .num_columns(2)
                .spacing([14.0, 5.0])
                .show(ui, |ui| {
                    field(
                        ui,
                        "隧道地址",
                        &mut profile.url,
                        "https://主机/SQLyogTunnel.php",
                    );
                    ui.label("内容类型");
                    egui::ComboBox::from_id_salt("tunnel-content-type")
                        .selected_text(
                            HTTP_CONTENT_TYPES
                                .get(profile.content_type)
                                .copied()
                                .unwrap_or(HTTP_CONTENT_TYPES[0]),
                        )
                        .show_ui(ui, |ui| {
                            for (index, content_type) in HTTP_CONTENT_TYPES.iter().enumerate() {
                                ui.selectable_value(
                                    &mut profile.content_type,
                                    index,
                                    *content_type,
                                );
                            }
                        });
                    ui.end_row();
                });
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut profile.force_base64, "始终使用 Base64 编码");
                ui.checkbox(&mut profile.utf8mb4, "使用 utf8mb4");
            });
            ui.horizontal(|ui| {
                ui.label("请求超时（秒）");
                ui.add(egui::DragValue::new(profile.timeout.get_or_insert(120)).range(1..=3_600));
            });
            if profile
                .url
                .trim()
                .to_ascii_lowercase()
                .starts_with("http://")
            {
                ui.colored_label(style::error(), "HTTP 会明文传输连接信息和 SQL。");
            }
            ui.collapsing("网页认证和代理", |ui| {
                egui::Grid::new("http-auth-proxy")
                    .num_columns(2)
                    .spacing([14.0, 5.0])
                    .show(ui, |ui| {
                        field(ui, "网页认证用户名", &mut profile.web_user, "选填");
                        ui.label("网页认证密码");
                        ui.add(
                            TextEdit::singleline(&mut form.web_password)
                                .password(true)
                                .desired_width(310.0),
                        );
                        ui.end_row();
                        field(ui, "代理地址", &mut profile.proxy_url, "选填");
                        field(ui, "代理用户名", &mut profile.proxy_user, "选填");
                        ui.label("代理密码");
                        ui.add(
                            TextEdit::singleline(&mut form.proxy_password)
                                .password(true)
                                .desired_width(310.0),
                        );
                        ui.end_row();
                    });
            });
            ui.label("每次请求独立连接。切换默认数据库请使用对象树。");
        });
}

fn connection_advanced_fields(ui: &mut egui::Ui, form: &mut ConnectionForm) {
    let native = form.profile.http.is_none();
    let advanced = &mut form.profile.advanced;
    ui.collapsing("高级连接设置", |ui| {
        ui.horizontal(|ui| {
            ui.label("连接超时（秒）");
            ui.add(egui::DragValue::new(&mut advanced.connect_timeout).range(1..=3_600));
        });
        ui.label("显示哪些数据库");
        ui.add(
            TextEdit::multiline(&mut advanced.displayed_databases)
                .desired_rows(2)
                .desired_width(360.0)
                .hint_text("用分号或换行分隔，留空显示全部"),
        );
        if !native {
            return;
        }
        ui.checkbox(&mut advanced.compression, "压缩 MySQL 协议");
        ui.horizontal(|ui| {
            ui.label("读取超时（秒）");
            ui.add(egui::DragValue::new(&mut advanced.read_timeout).range(1..=86_400));
        });
        optional_seconds(
            ui,
            "指定会话空闲超时（秒）",
            &mut advanced.wait_timeout,
            28_800,
            31_536_000,
        );
        optional_seconds(
            ui,
            "空闲时定期保活（秒）",
            &mut advanced.keep_alive,
            60,
            86_400,
        );
        let mut custom_mode = advanced.sql_mode.is_some();
        if ui.checkbox(&mut custom_mode, "指定 SQL 模式").changed() {
            advanced.sql_mode = custom_mode.then(String::new);
        }
        if let Some(mode) = &mut advanced.sql_mode {
            ui.add(
                TextEdit::singleline(mode)
                    .desired_width(360.0)
                    .hint_text("例如 STRICT_TRANS_TABLES，留空表示空模式"),
            );
        }
        ui.horizontal(|ui| {
            ui.add(
                TextEdit::singleline(&mut advanced.init_file)
                    .desired_width(345.0)
                    .hint_text("连接后执行的 SQL 文件（选填）"),
            );
            if ui.button("浏览…").clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .set_title("选择初始化 SQL")
                    .add_filter("SQL 文件", &["sql", "txt"])
                    .pick_file()
            {
                advanced.init_file = path.to_string_lossy().into_owned();
                form.init_sql.clear();
            }
        });
        if advanced.init_file.trim().is_empty() {
            ui.add(
                TextEdit::multiline(&mut form.init_sql)
                    .code_editor()
                    .desired_rows(2)
                    .desired_width(360.0)
                    .hint_text("本次连接初始化 SQL（不随配置保存）"),
            );
        }
    });
}

fn optional_seconds(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut Option<u32>,
    default: u32,
    max: u32,
) {
    ui.horizontal(|ui| {
        let mut enabled = value.is_some();
        if ui.checkbox(&mut enabled, label).changed() {
            *value = enabled.then_some(default);
        }
        if let Some(seconds) = value {
            ui.add(egui::DragValue::new(seconds).range(1..=max));
        }
    });
}

fn draw_database_icon(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 15.0), egui::Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let painter = ui.painter();
    let stroke = egui::Stroke::new(1.35_f32, color);
    let left = rect.left() + 1.5;
    let right = rect.right() - 1.5;
    let top = rect.top() + 3.0;
    let bottom = rect.bottom() - 2.5;
    let mid_y = (top + bottom) * 0.5;
    // Cylinder body with rounded top/bottom caps
    let cap = egui::Rect::from_min_max(egui::pos2(left, top - 1.6), egui::pos2(right, top + 1.8));
    painter.rect_filled(cap, egui::CornerRadius::same(3), color.gamma_multiply(0.22));
    painter.rect_stroke(cap, egui::CornerRadius::same(3), stroke, egui::StrokeKind::Inside);
    painter.line_segment([egui::pos2(left + 0.5, top + 0.5), egui::pos2(left + 0.5, bottom - 0.5)], stroke);
    painter.line_segment([egui::pos2(right - 0.5, top + 0.5), egui::pos2(right - 0.5, bottom - 0.5)], stroke);
    painter.line_segment([egui::pos2(left + 0.5, mid_y), egui::pos2(right - 0.5, mid_y)], egui::Stroke::new(1.1_f32, color.gamma_multiply(0.7)));
    painter.line_segment([egui::pos2(left + 0.5, bottom), egui::pos2(right - 0.5, bottom)], stroke);
}

fn is_tabular(kind: ObjectKind) -> bool {
    matches!(kind, ObjectKind::Table | ObjectKind::View)
}

fn object_double_click(
    object: &crate::database::DbObject,
    design_open: bool,
    insert_name: bool,
) -> BrowseAction {
    match (design_open, insert_name, object.kind) {
        (true, _, ObjectKind::Table) => BrowseAction::SchemaDesigner {
            schema: object.schema.clone(),
            table: Some(object.clone()),
        },
        (false, true, _) => BrowseAction::InsertObjectName(object.clone()),
        (_, _, ObjectKind::Table) => BrowseAction::EditRows(object.clone()),
        (_, _, ObjectKind::View) => BrowseAction::Preview(object.clone()),
        _ => BrowseAction::Inspect(object.clone()),
    }
}

#[cfg(test)]
#[test]
fn object_double_click_inserts_quoted_name_and_preserves_data_and_designer_routes() {
    let ctx = egui::Context::default();
    let mut app = Workbench::new(&ctx);
    let table = crate::database::DbObject {
        schema: "演示库".into(),
        kind: ObjectKind::Table,
        name: "订`单".into(),
    };
    let sql = "SELECT * FROM 旧表;";
    let start = sql.find("旧表").unwrap();
    let workspace = app.ws();
    let editor = editor_tools::editor_id(workspace);
    workspace.tab_mut().sql = sql.into();
    workspace.tab_mut().selection = Some(start..start + "旧表".len());
    app.browse(&ctx, object_double_click(&table, false, true));
    let workspace = app.ws();
    let tab = workspace.tab();
    assert_eq!(tab.sql, "SELECT * FROM `订``单`;");
    assert_eq!(tab.focused_byte, tab.sql.find(';').unwrap());
    assert!(tab.selection.is_none());
    assert!(workspace.focus_request);
    assert_eq!(workspace.selected.as_ref(), Some(&table));
    let state = TextEdit::load_state(&ctx, editor).unwrap();
    let range = state.cursor.char_range().unwrap();
    let mut undo = state.undoer();
    let (_, previous) = undo.undo(&(range, tab.sql.clone())).unwrap();
    assert_eq!(previous, sql);

    assert!(matches!(
        object_double_click(&table, false, false),
        BrowseAction::EditRows(object) if object == table
    ));
    assert!(matches!(
        object_double_click(&table, true, true),
        BrowseAction::SchemaDesigner { table: Some(object), .. } if object == table
    ));
    for kind in [
        ObjectKind::View,
        ObjectKind::Procedure,
        ObjectKind::Function,
        ObjectKind::Trigger,
        ObjectKind::Event,
    ] {
        let object = crate::database::DbObject {
            kind,
            ..table.clone()
        };
        assert!(matches!(
            object_double_click(&object, false, true),
            BrowseAction::InsertObjectName(selected) if selected == object
        ));
    }
}

fn kind_label(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Table => "表",
        ObjectKind::View => "视图",
        ObjectKind::Procedure => "存储过程",
        ObjectKind::Function => "函数",
        ObjectKind::Trigger => "触发器",
        ObjectKind::Event => "事件",
    }
}

fn byte_index(text: &str, index: usize) -> usize {
    text.char_indices()
        .nth(index)
        .map_or(text.len(), |(offset, _)| offset)
}
