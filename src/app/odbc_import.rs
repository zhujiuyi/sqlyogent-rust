use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::AtomicBool,
    mpsc::{self, Receiver, TryRecvError},
};
use std::time::Duration;

use eframe::egui::{self, ComboBox, TextEdit};
use harbor_sql::file_source::{self, FileSource, FileSourceFormat};
use rfd::{FileDialog, MessageButtons, MessageDialog, MessageDialogResult};
use zeroize::{Zeroize, Zeroizing};

use crate::database::{
    Cell, Command, DbObject, MAX_IMPORT_COLUMNS, ObjectKind, OdbcColumnDefinition, OdbcConflict,
    OdbcForeignKey, OdbcIndex, OdbcReset, OdbcTarget, odbc_create_table_sql, quote_identifier,
    validate_odbc_create_table_sql, validate_odbc_engine,
};
use crate::odbc::{
    self, DiscoveredTable, PreparedSourceAction, SourceAction, SourceAssignment, SourceChange,
    SourceColumn, SourceData, SourceTable,
};

use super::{
    io_jobs::{form_scroll, form_section, form_width, right_aligned_row},
    odbc_file_dsn, read_sql_file,
    state::Workspace,
    style,
};

pub(super) mod file_fields;

const REPLACE_WARNING: &str = "按写入时的主键或唯一索引匹配。替换会删除冲突的旧行，未映射列采用默认值。删除可能触发外键级联和触发器。";

pub(super) fn source_query_file_button(
    ui: &mut egui::Ui,
    sql: &mut String,
) -> Result<bool, String> {
    if !ui.button("打开 SQL 文件…").clicked() {
        return Ok(false);
    }
    let Some(path) = FileDialog::new()
        .set_title("打开 SQL 文件")
        .add_filter("SQL 文件", &["sql", "txt"])
        .pick_file()
    else {
        return Ok(false);
    };
    *sql = read_sql_file(&path).map_err(|error| format!("无法打开 SQL 文件：{error}"))?;
    Ok(true)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TableMode {
    Create,
    Append,
    Truncate,
    Recreate,
}

impl TableMode {
    pub(super) fn from_settings(create: bool, reset: OdbcReset) -> Self {
        match reset {
            OdbcReset::Truncate => Self::Truncate,
            OdbcReset::Recreate => Self::Recreate,
            OdbcReset::None if create => Self::Create,
            OdbcReset::None => Self::Append,
        }
    }

    pub(super) fn defines_table(self) -> bool {
        matches!(self, Self::Create | Self::Recreate)
    }

    fn uses_existing(self) -> bool {
        self != Self::Create
    }

    pub(super) fn reset(self) -> OdbcReset {
        match self {
            Self::Truncate => OdbcReset::Truncate,
            Self::Recreate => OdbcReset::Recreate,
            _ => OdbcReset::None,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Create => "新建目标表",
            Self::Append => "追加到现有表",
            Self::Truncate => "清空后导入（保留表结构）",
            Self::Recreate => "重建目标表",
        }
    }

    pub(super) fn impact(self) -> Option<&'static str> {
        match self {
            Self::Truncate => Some("将清空以下目标表的全部记录，保留原表结构。"),
            Self::Recreate => Some("将删除以下目标表的现有结构和记录，并按列映射重建。"),
            _ => None,
        }
    }

    pub(super) fn picker(ui: &mut egui::Ui, id: &'static str, mode: &mut Self, allow_create: bool) {
        ui.horizontal_wrapped(|ui| {
            ui.label("导入方式");
            ComboBox::from_id_salt(id)
                .width(ui.available_width().min(250.0))
                .truncate()
                .selected_text(mode.label())
                .show_ui(ui, |ui| {
                    for item in [Self::Create, Self::Append, Self::Truncate, Self::Recreate] {
                        if item != Self::Create || allow_create {
                            ui.selectable_value(mode, item, item.label());
                        }
                    }
                });
        });
    }

    pub(super) fn confirm(self, tables: &[String]) -> bool {
        let Some(impact) = self.impact() else {
            return true;
        };
        MessageDialog::new()
            .set_title(if self == Self::Truncate {
                "确认清空目标表"
            } else {
                "确认重建目标表"
            })
            .set_description(format!("{impact}\n\n{}\n\n是否继续？", tables.join("\n")))
            .set_buttons(MessageButtons::YesNo)
            .show()
            == MessageDialogResult::Yes
    }
}

pub(super) fn target_name(schema: &str, table: &str) -> String {
    format!(
        "{}.{}",
        quote_identifier(schema.trim()),
        quote_identifier(table.trim())
    )
}

pub(super) fn metadata_fields(
    ui: &mut egui::Ui,
    id: &'static str,
    locator: &mut Option<SourceTable>,
    copy_foreign_keys: &mut bool,
    discovered: Option<&SourceTable>,
) {
    let mut copy_indexes = locator.is_some() || *copy_foreign_keys;
    metadata_checkboxes(ui, &mut copy_indexes, copy_foreign_keys);
    set_metadata_selection(locator, copy_indexes, *copy_foreign_keys);
    if let Some(locator) = locator {
        if let Some(discovered) = discovered {
            *locator = discovered.clone();
            ui.label(format!("结构来源  {}", source_table_name(locator)));
            return;
        }
        egui::Grid::new(id).num_columns(2).show(ui, |ui| {
            ui.label("源表名");
            ui.add(TextEdit::singleline(&mut locator.table).desired_width(240.0));
            ui.end_row();
            metadata_optional_field(ui, "源目录", &mut locator.catalog);
            metadata_optional_field(ui, "源架构", &mut locator.schema);
        });
        if !valid_metadata_locator(locator) {
            ui.colored_label(
                style::MUTED,
                "请填写准确的源表名。目录和架构请按驱动定义填写。",
            );
        }
    }
}

pub(super) fn metadata_checkboxes(
    ui: &mut egui::Ui,
    copy_indexes: &mut bool,
    copy_foreign_keys: &mut bool,
) {
    ui.horizontal_wrapped(|ui| {
        ui.checkbox(copy_foreign_keys, "复制源表外键");
        *copy_indexes |= *copy_foreign_keys;
        ui.add_enabled(
            !*copy_foreign_keys,
            egui::Checkbox::new(copy_indexes, "复制源表索引"),
        )
        .on_disabled_hover_text("复制外键时同时复制索引。");
    });
}

fn set_metadata_selection(locator: &mut Option<SourceTable>, indexes: bool, foreign_keys: bool) {
    if indexes || foreign_keys {
        locator.get_or_insert_with(SourceTable::default);
    } else {
        *locator = None;
    }
}

fn metadata_optional_field(ui: &mut egui::Ui, label: &str, value: &mut Option<String>) {
    ui.label(label);
    let mut text = value.clone().unwrap_or_default();
    if ui
        .add(
            TextEdit::singleline(&mut text)
                .desired_width(240.0)
                .hint_text("可选"),
        )
        .changed()
    {
        *value = (!text.trim().is_empty()).then_some(text);
    }
    ui.end_row();
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum AfterMode {
    None,
    Update,
    Delete,
}

impl AfterMode {
    fn from_action(action: Option<&SourceAction>) -> Self {
        match action.map(|action| &action.change) {
            Some(SourceChange::Update { .. }) => Self::Update,
            Some(SourceChange::Delete) => Self::Delete,
            None => Self::None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::None => "无",
            Self::Update => "更新已导入的源记录",
            Self::Delete => "删除已导入的源记录",
        }
    }

    pub(super) fn apply(self, action: &mut Option<SourceAction>) {
        if self == Self::from_action(action.as_ref()) {
            return;
        }
        let change = match self {
            Self::Update => SourceChange::Update {
                assignments: Vec::new(),
            },
            Self::Delete => SourceChange::Delete,
            Self::None => {
                *action = None;
                return;
            }
        };
        let table = action.take().map(|action| action.table).unwrap_or_default();
        *action = Some(SourceAction { table, change });
    }
}

pub(super) fn source_table_name(table: &SourceTable) -> String {
    if table.table.trim().is_empty() {
        return "待填写源表".into();
    }
    [
        table.catalog.as_deref(),
        table.schema.as_deref(),
        Some(&table.table),
    ]
    .into_iter()
    .flatten()
    .filter(|part| !part.trim().is_empty())
    .map(quote_identifier)
    .collect::<Vec<_>>()
    .join(".")
}

pub(super) fn source_action_fields(
    ui: &mut egui::Ui,
    action: &mut Option<SourceAction>,
    discovered: Option<&SourceTable>,
) {
    let mut mode = AfterMode::from_action(action.as_ref());
    ui.horizontal_wrapped(|ui| {
        ui.label("导入后处理源记录");
        ComboBox::from_id_salt("odbc-job-after-import")
            .selected_text(mode.label())
            .show_ui(ui, |ui| {
                for option in [AfterMode::None, AfterMode::Update, AfterMode::Delete] {
                    ui.selectable_value(&mut mode, option, option.label());
                }
            });
    });
    mode.apply(action);
    let Some(action) = action else { return };
    ui.label("适用于 MySQL 或 MariaDB 的 InnoDB 源表。源表操作仅处理目标库已确认写入的行，跳过的重复行继续留在源表。");
    let table = &mut action.table;
    let previous = (
        table.table.clone(),
        table.catalog.clone(),
        table.schema.clone(),
    );
    if let Some(discovered) = discovered {
        *table = discovered.clone();
        ui.label(format!("操作源表  {}", source_table_name(table)));
    } else {
        egui::Grid::new("odbc-job-action-source")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label("操作的源表名");
                ui.add(TextEdit::singleline(&mut table.table).desired_width(240.0));
                ui.end_row();
                metadata_optional_field(ui, "源表目录", &mut table.catalog);
                metadata_optional_field(ui, "源表架构", &mut table.schema);
            });
    }
    if let SourceChange::Update { assignments } = &mut action.change {
        if table.table != previous.0 || table.catalog != previous.1 || table.schema != previous.2 {
            assignments.clear();
        }
        source_assignments(ui, assignments);
    }
}

fn source_assignments(ui: &mut egui::Ui, assignments: &mut Vec<SourceAssignment>) {
    let mut remove = None;
    for (index, assignment) in assignments.iter_mut().enumerate() {
        ui.push_id(("odbc-source-assignment", index), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("源列");
                ui.add(TextEdit::singleline(&mut assignment.column).desired_width(120.0));
                ui.label("值或表达式");
                ui.add(
                    TextEdit::singleline(&mut assignment.expression)
                        .desired_width(185.0)
                        .hint_text("例如 '已导入' 或 NOW()"),
                );
                if ui.button("移除").clicked() {
                    remove = Some(index);
                }
            });
        });
    }
    if let Some(index) = remove {
        assignments.remove(index);
    }
    if ui
        .add_enabled(
            assignments.len() < MAX_IMPORT_COLUMNS,
            egui::Button::new("添加源列赋值"),
        )
        .clicked()
    {
        assignments.push(SourceAssignment::default());
    }
    if assignments.iter().any(complete_assignment) {
        ui.collapsing("赋值预览", |ui| {
            for assignment in assignments
                .iter()
                .filter(|assignment| complete_assignment(assignment))
            {
                ui.monospace(format!(
                    "{} = {}",
                    quote_identifier(assignment.column.trim()),
                    assignment.expression.trim(),
                ));
            }
        });
    }
}

fn complete_assignment(assignment: &SourceAssignment) -> bool {
    !assignment.column.trim().is_empty() && !assignment.expression.trim().is_empty()
}

pub(super) fn source_action_effect(action: &SourceAction) -> String {
    let change = match &action.change {
        SourceChange::Delete => "删除已导入的行".into(),
        SourceChange::Update { assignments } => {
            let columns = assignments
                .iter()
                .filter(|assignment| !assignment.column.trim().is_empty())
                .map(|assignment| quote_identifier(assignment.column.trim()))
                .collect::<Vec<_>>();
            if columns.is_empty() {
                "更新已导入的行".into()
            } else {
                format!("更新列 {}", columns.join("、"))
            }
        }
    };
    format!("{} · {change}", source_table_name(&action.table))
}

pub(super) fn confirm_import(
    title: &str,
    mode: TableMode,
    targets: &[String],
    source_effects: &[String],
) -> bool {
    if source_effects.is_empty() {
        return mode.confirm(targets);
    }
    let mut description = String::new();
    if let Some(impact) = mode.impact() {
        description.push_str(impact);
        description.push('\n');
        description.push_str(&targets.join("\n"));
        description.push_str("\n\n");
    } else {
        description.push_str("导入目标\n");
        description.push_str(&targets.join("\n"));
        description.push_str("\n\n");
    }
    description.push_str("源表操作仅处理目标库已确认写入的记录。跳过的重复行继续留在源表。\n");
    description.push_str(&source_effects.join("\n"));
    description.push_str("\n\n是否继续？");
    MessageDialog::new()
        .set_title(title)
        .set_description(description)
        .set_buttons(MessageButtons::YesNo)
        .show()
        == MessageDialogResult::Yes
}

fn valid_metadata_locator(locator: &SourceTable) -> bool {
    [
        Some(locator.table.as_str()),
        locator.catalog.as_deref(),
        locator.schema.as_deref(),
    ]
    .into_iter()
    .flatten()
    .all(|name| {
        !name.trim().is_empty() && !name.contains('\0') && name.encode_utf16().count() <= 255
    })
}

pub(super) struct Import {
    index: usize,
    schema: String,
    object: Option<DbObject>,
    automatic_target: bool,
    choose_table: bool,
    mode: TableMode,
    structure_only: bool,
    new_table: String,
    create_sql: Option<String>,
    engine: Option<String>,
    tables_requested: bool,
    tables: Option<Vec<DbObject>>,
    target: Option<Vec<String>>,
    target_ddl: Option<String>,
    target_requested: bool,
    conflict: OdbcConflict,
    connection: Connection,
    source_mode: SourceMode,
    file_source: FileSource,
    access_password: Zeroizing<String>,
    source_tables: SourceTables,
    sql: String,
    indexes_from: Option<SourceTable>,
    copy_foreign_keys: bool,
    after_import: Option<SourceAction>,
    source: Source,
    error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceMode {
    Query,
    Table,
    File,
}

#[derive(Clone)]
enum Connection {
    Text(Zeroizing<String>),
    FileDsn {
        path: PathBuf,
        user: Zeroizing<String>,
        password: Zeroizing<String>,
    },
}

impl Connection {
    fn ready(&self) -> bool {
        match self {
            Self::Text(connection) => !connection.trim().is_empty(),
            Self::FileDsn { path, .. } => !path.as_os_str().is_empty(),
        }
    }

    fn connection_string(self) -> Result<String, String> {
        match self {
            Self::Text(connection) => Ok(connection.to_string()),
            Self::FileDsn {
                path,
                user,
                password,
            } => odbc::file_dsn_connection(
                &path,
                (!user.is_empty()).then(|| user.as_str()),
                (!password.is_empty()).then(|| password.as_str()),
            ),
        }
    }

    fn fields(&mut self, ui: &mut egui::Ui) -> Result<bool, String> {
        let original = matches!(self, Self::FileDsn { .. });
        let mut use_file = original;
        ui.horizontal_wrapped(|ui| {
            ui.label("ODBC 数据源");
            ui.radio_value(&mut use_file, false, "DSN 或连接字符串");
            ui.radio_value(&mut use_file, true, "文件 DSN");
        });
        let changed = use_file != original;
        if changed {
            *self = if use_file {
                Self::FileDsn {
                    path: PathBuf::new(),
                    user: Zeroizing::default(),
                    password: Zeroizing::default(),
                }
            } else {
                Self::Text(Zeroizing::default())
            };
        }
        match self {
            Self::Text(connection) => {
                let mut output = TextEdit::singleline(&mut **connection)
                    .password(true)
                    .desired_width(f32::INFINITY)
                    .hint_text("DSN 名称或完整连接字符串")
                    .show(ui);
                output.state.clear_undoer();
                Ok(changed || output.response.changed())
            }
            Self::FileDsn {
                path,
                user,
                password,
            } => {
                let path_changed = odbc_file_dsn::fields(ui, path)?;
                if path_changed {
                    user.zeroize();
                    password.zeroize();
                }
                let edited = egui::Grid::new("odbc-file-dsn-credentials")
                    .num_columns(2)
                    .show(ui, |ui| {
                        ui.label("源用户名");
                        let user_changed = ui
                            .add(TextEdit::singleline(&mut **user).hint_text("可选"))
                            .changed();
                        ui.end_row();
                        ui.label("源密码");
                        let mut output = TextEdit::singleline(&mut **password)
                            .password(true)
                            .hint_text("可选")
                            .show(ui);
                        output.state.clear_undoer();
                        ui.end_row();
                        user_changed || output.response.changed()
                    })
                    .inner;
                Ok(changed || path_changed || edited)
            }
        }
    }
}

enum SourceTables {
    Idle,
    Loading(Receiver<Result<Vec<DiscoveredTable>, String>>),
    Listed {
        tables: Vec<DiscoveredTable>,
        selected: Option<usize>,
    },
}

struct SourceRead {
    data: SourceData,
    action: Option<PreparedSourceAction>,
}

enum Source {
    Form,
    Reading(Receiver<Result<SourceRead, String>>),
    Ready {
        columns: Vec<SourceColumn>,
        rows: Vec<Vec<Cell>>,
        mapping: Mapping,
        metadata: Option<SourceData>,
        action: Option<PreparedSourceAction>,
    },
}

enum Mapping {
    Existing(Vec<Option<usize>>),
    New(Vec<NewColumn>),
}

struct NewColumn {
    name: String,
    selected: bool,
    definition: OdbcColumnDefinition,
}

fn selected_new_columns(columns: &[NewColumn]) -> Vec<(usize, String)> {
    columns
        .iter()
        .enumerate()
        .filter(|(_, column)| column.selected)
        .map(|(source, column)| (source, column.name.trim().to_owned()))
        .collect()
}

fn metadata_plan(
    source: &SourceData,
    columns: &[SourceColumn],
    selected: &[(usize, String)],
    target_table: &str,
) -> Result<(Vec<OdbcIndex>, Vec<OdbcForeignKey>), String> {
    let mapping = selected
        .iter()
        .map(|(source, target)| {
            columns
                .get(*source)
                .map(|column| (column.name.clone(), target.clone()))
                .ok_or("源列已变化，请重新读取源数据。")
        })
        .collect::<Result<Vec<_>, _>>()?;
    odbc::map_single_table_metadata(source, target_table, &mapping)
}

impl Import {
    pub(super) fn new(index: usize, object: DbObject) -> Self {
        let mut import = Self::for_schema(index, object.schema.clone());
        import.object = Some(object);
        import.choose_table = false;
        import
    }

    pub(super) fn for_schema(index: usize, schema: String) -> Self {
        Self {
            index,
            schema,
            object: None,
            automatic_target: false,
            choose_table: true,
            mode: TableMode::Append,
            structure_only: false,
            new_table: String::new(),
            create_sql: None,
            engine: None,
            tables_requested: false,
            tables: None,
            target: None,
            target_ddl: None,
            target_requested: false,
            conflict: OdbcConflict::Abort,
            connection: Connection::Text(Zeroizing::default()),
            source_mode: SourceMode::Query,
            file_source: FileSource::default(),
            access_password: Zeroizing::new(String::new()),
            source_tables: SourceTables::Idle,
            sql: String::new(),
            indexes_from: None,
            copy_foreign_keys: false,
            after_import: None,
            source: Source::Form,
            error: None,
        }
    }

    fn poll(&mut self, workspace: &mut Workspace) {
        self.poll_source_tables();
        if workspace.server.is_none() {
            self.error = Some("目标数据库连接已断开。".into());
            return;
        }
        if self.choose_table
            && self.mode.uses_existing()
            && self.tables.is_none()
            && !workspace.busy()
        {
            if self.tables_requested {
                if let Some(objects) = workspace.objects.get(&self.schema) {
                    let tables: Vec<_> = objects
                        .iter()
                        .filter(|object| object.kind == ObjectKind::Table)
                        .cloned()
                        .collect();
                    self.tables = Some(tables);
                } else {
                    self.error = Some(format!("无法读取目标数据库的表。 {}", workspace.status));
                }
            } else {
                self.tables_requested = workspace.send(
                    Command::ListObjects {
                        schema: self.schema.clone(),
                    },
                    "正在读取目标数据库的表…",
                );
                if self.tables_requested {
                    workspace.objects.remove(&self.schema);
                }
            }
        }
        if self.automatic_target
            && !self
                .selected_source_table()
                .zip(self.object.as_ref())
                .is_some_and(|(source, object)| {
                    self.source_mode == SourceMode::File && object.name == source.source.table
                })
        {
            self.select_target(self.mode, None);
        }
        if self.source_mode == SourceMode::File
            && self.choose_table
            && self.mode.uses_existing()
            && self.object.is_none()
            && let Some(source) = self.selected_source_table()
            && let Some(object) = self
                .tables
                .iter()
                .flatten()
                .find(|table| table.name == source.source.table)
        {
            self.select_target(self.mode, Some(object.clone()));
            self.automatic_target = true;
        }
        if self.mode.uses_existing()
            && let Some(object) = &self.object
            && self.target.is_none()
            && !workspace.busy()
        {
            if !self.target_requested {
                self.target_requested = workspace.send(
                    Command::Inspect {
                        object: object.clone(),
                    },
                    "正在读取目标表结构…",
                );
                if self.target_requested {
                    workspace.detail = None;
                }
            } else if let Some(detail) = workspace
                .detail
                .as_ref()
                .filter(|detail| &detail.object == object)
            {
                let columns: Vec<_> = detail
                    .columns
                    .iter()
                    .filter(|column| {
                        !column
                            .extra
                            .split_ascii_whitespace()
                            .any(|word| word.eq_ignore_ascii_case("GENERATED"))
                    })
                    .map(|column| column.name.clone())
                    .collect();
                if columns.is_empty() && !self.mode.defines_table() {
                    self.error = Some("目标表没有可导入的列。".into());
                }
                self.target = Some(columns);
                self.target_ddl = (detail.ddl_error.is_none() && !detail.ddl.trim().is_empty())
                    .then(|| detail.ddl.clone());
                if self.mode.impact().is_some() && self.target_ddl.is_none() {
                    self.error =
                        Some("无法核对目标表结构。请重新读取目标表后再决定清空或重建。".into());
                }
            } else {
                self.error = Some(format!("无法读取目标表结构。 {}", workspace.status));
            }
        }
        let Source::Reading(receiver) = &self.source else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(read)) if read.data.rows.is_empty() && !self.mode.defines_table() => {
                self.error = Some("没有读到可导入的源数据。".into());
                self.source = Source::Form;
            }
            Ok(Ok(SourceRead { mut data, action })) => {
                let mapping = if self.mode.defines_table() {
                    Mapping::New(new_columns(&data.columns))
                } else {
                    Mapping::Existing(initial_mapping(
                        data.columns.iter().map(|column| column.name.as_str()),
                        self.target.as_deref().unwrap_or_default(),
                    ))
                };
                self.source = Source::Ready {
                    columns: std::mem::take(&mut data.columns),
                    rows: std::mem::take(&mut data.rows),
                    mapping,
                    metadata: self.indexes_from.is_some().then_some(data),
                    action,
                };
            }
            Ok(Err(error)) => {
                self.error = Some(error);
                self.source = Source::Form;
            }
            Err(TryRecvError::Disconnected) => {
                self.error = Some("读取源数据的任务已结束。".into());
                self.source = Source::Form;
            }
            Err(TryRecvError::Empty) => {}
        }
    }

    fn set_source_mode(&mut self, mode: SourceMode) {
        if self.source_mode != mode {
            self.access_password.zeroize();
            self.clear_source_tables();
            self.source_mode = mode;
        }
    }

    fn clear_source_tables(&mut self) {
        if self.source_mode == SourceMode::File
            && self
                .selected_source_table()
                .is_some_and(|table| table.source.table == self.new_table)
        {
            self.new_table.clear();
        }
        self.source_tables = SourceTables::Idle;
        self.clear_source_binding();
    }

    fn clear_source_binding(&mut self) {
        self.clear_metadata();
        self.after_import = None;
        self.create_sql = None;
        self.source = Source::Form;
        self.error = None;
    }

    fn selected_source_table(&self) -> Option<&DiscoveredTable> {
        if self.source_mode == SourceMode::Query {
            return None;
        }
        match &self.source_tables {
            SourceTables::Listed {
                tables,
                selected: Some(index),
            } => tables.get(*index),
            _ => None,
        }
    }

    fn source_query(&self) -> Option<&str> {
        match self.source_mode {
            SourceMode::Query => (!self.sql.trim().is_empty()).then_some(self.sql.as_str()),
            SourceMode::Table => self
                .selected_source_table()
                .map(|table| table.select_sql.as_str()),
            SourceMode::File => None,
        }
    }

    fn source_ready(&self) -> bool {
        if self.source_mode == SourceMode::File {
            self.selected_source_table().is_some()
        } else {
            self.connection.ready() && self.source_query().is_some()
        }
    }

    fn select_source_table(&mut self, index: usize) {
        let SourceTables::Listed { tables, selected } = &mut self.source_tables else {
            return;
        };
        if let Some(table) = tables.get(index)
            && *selected != Some(index)
        {
            if self.source_mode == SourceMode::File
                && (self.new_table.is_empty()
                    || selected
                        .and_then(|previous| tables.get(previous))
                        .is_some_and(|previous| self.new_table == previous.source.table))
            {
                self.new_table.clone_from(&table.source.table);
            }
            *selected = Some(index);
            self.clear_source_binding();
        }
    }

    fn load_source_tables(&mut self) {
        self.clear_source_tables();
        let file = (self.source_mode == SourceMode::File)
            .then(|| (self.file_source.clone(), self.access_password.clone()));
        let connection = file.is_none().then(|| self.connection.clone());
        let (sender, receiver) = mpsc::sync_channel(1);
        match std::thread::Builder::new()
            .name("harbor-import-source-tables".into())
            .spawn(move || {
                let tables = if let Some((file, password)) = file {
                    let names = if password.is_empty() {
                        file_source::discover(&file)
                    } else {
                        file_source::discover_with_access_password(&file, password.as_str())
                    };
                    names.map(|names| {
                        names
                            .into_iter()
                            .map(file_fields::discovered_table)
                            .collect()
                    })
                } else {
                    connection
                        .ok_or_else(|| "请选择 ODBC 数据源。".to_owned())
                        .and_then(Connection::connection_string)
                        .and_then(odbc::discover_tables)
                };
                let _ = sender.send(tables);
            }) {
            Ok(_) => self.source_tables = SourceTables::Loading(receiver),
            Err(error) => self.error = Some(format!("无法开始读取数据表列表。{error}")),
        }
    }

    fn poll_source_tables(&mut self) {
        let SourceTables::Loading(receiver) = &self.source_tables else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(tables)) => {
                let select_only_table = self.source_mode == SourceMode::File && tables.len() == 1;
                self.source_tables = SourceTables::Listed {
                    tables,
                    selected: None,
                };
                if select_only_table {
                    self.select_source_table(0);
                }
            }
            Ok(Err(error)) => {
                self.source_tables = SourceTables::Idle;
                self.error = Some(error);
            }
            Err(TryRecvError::Disconnected) => {
                self.source_tables = SourceTables::Idle;
                self.error = Some("读取数据表列表的任务已结束。".into());
            }
            Err(TryRecvError::Empty) => {}
        }
    }

    fn source_form(&mut self, ui: &mut egui::Ui, target_mode: TableMode) {
        let mut source_mode = self.source_mode;
        ui.horizontal_wrapped(|ui| {
            ui.radio_value(&mut source_mode, SourceMode::Query, "使用 ODBC 查询");
            ui.radio_value(&mut source_mode, SourceMode::Table, "从 ODBC 源表选择");
            ui.radio_value(&mut source_mode, SourceMode::File, "从文件导入");
        });
        self.set_source_mode(source_mode);
        if source_mode == SourceMode::File {
            let format = self.file_source.format;
            let path = self.file_source.path.clone();
            let file_changed = file_fields::show(ui, &mut self.file_source);
            if format != self.file_source.format || path != self.file_source.path {
                self.access_password.zeroize();
            }
            let password_changed = self.file_source.format == FileSourceFormat::Access
                && self.access_password_field(ui);
            if file_changed || password_changed {
                self.clear_source_tables();
            }
            self.source_table_fields(ui);
            if target_mode.defines_table()
                && self.file_source.supports_metadata()
                && let Some(discovered) = self
                    .selected_source_table()
                    .map(|table| table.source.clone())
            {
                self.source_metadata_fields(ui, Some(&discovered));
            }
            return;
        }
        match self.connection.fields(ui) {
            Ok(true) => self.clear_source_tables(),
            Err(error) => self.error = Some(error),
            Ok(false) => {}
        }
        if source_mode == SourceMode::Query {
            let loaded = ui
                .horizontal_wrapped(|ui| {
                    ui.label("源查询");
                    source_query_file_button(ui, &mut self.sql)
                })
                .inner;
            let edited = ui
                .add(
                    TextEdit::multiline(&mut self.sql)
                        .desired_rows(5)
                        .desired_width(f32::INFINITY)
                        .font(egui::TextStyle::Monospace)
                        .hint_text("SELECT ... FROM ..."),
                )
                .changed();
            if edited || matches!(loaded, Ok(true)) {
                self.clear_source_binding();
            }
            if let Err(error) = loaded {
                self.error = Some(error);
            }
        } else {
            self.source_table_fields(ui);
        }
        let discovered = self
            .selected_source_table()
            .map(|table| table.source.clone());
        if self.source_mode == SourceMode::Table && discovered.is_none() {
            return;
        }
        if target_mode.defines_table() {
            self.source_metadata_fields(ui, discovered.as_ref());
        }
        if !self.structure_only {
            source_action_fields(ui, &mut self.after_import, discovered.as_ref());
        }
    }

    fn access_password_field(&mut self, ui: &mut egui::Ui) -> bool {
        ui.label("Access 文件密码");
        let mut output = TextEdit::singleline(&mut *self.access_password)
            .id_salt("odbc-import-access-password")
            .password(true)
            .hint_text("无密码时留空")
            .desired_width(f32::INFINITY)
            .show(ui);
        // The masked widget still keeps undo history.
        output.state.clear_undoer();
        output.response.changed()
    }

    fn source_metadata_fields(&mut self, ui: &mut egui::Ui, discovered: Option<&SourceTable>) {
        metadata_fields(
            ui,
            "odbc-import-source-metadata",
            &mut self.indexes_from,
            &mut self.copy_foreign_keys,
            discovered,
        );
        if self.copy_foreign_keys {
            ui.label("单表导入可复制自引用外键。跨表外键请将相关表加入同一个导入作业。");
        }
    }

    fn source_table_fields(&mut self, ui: &mut egui::Ui) {
        let loading = matches!(self.source_tables, SourceTables::Loading(_));
        let listed = matches!(self.source_tables, SourceTables::Listed { .. });
        let file = self.source_mode == SourceMode::File;
        let source_configured = if file {
            !self.file_source.path.as_os_str().is_empty()
        } else {
            self.connection.ready()
        };
        if ui
            .add_enabled(
                !loading && source_configured,
                egui::Button::new(match (file, listed) {
                    (true, true) => "重新读取文件中的数据表",
                    (true, false) => "读取文件中的数据表",
                    (false, true) => "重新读取源表列表",
                    (false, false) => "读取源表列表",
                }),
            )
            .clicked()
        {
            self.load_source_tables();
        }
        if loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("正在读取数据表列表…");
            });
            ui.ctx().request_repaint_after(Duration::from_millis(80));
        }
        if let SourceTables::Listed { tables, selected } = &self.source_tables {
            if tables.is_empty() {
                ui.label("没有找到可选择的数据表。");
                return;
            }
            let mut choice = *selected;
            ui.horizontal(|ui| {
                ui.label("源表");
                let label = choice.and_then(|index| tables.get(index)).map_or_else(
                    || "请选择源表".into(),
                    |table| source_table_name(&table.source),
                );
                ComboBox::from_id_salt("odbc-import-source-table")
                    .width(ui.available_width().min(420.0))
                    .truncate()
                    .selected_text(&label)
                    .show_ui(ui, |ui| {
                        for (index, table) in tables.iter().enumerate() {
                            ui.selectable_value(
                                &mut choice,
                                Some(index),
                                source_table_name(&table.source),
                            );
                        }
                    })
                    .response
                    .on_hover_text(label);
            });
            if let Some(index) = choice {
                self.select_source_table(index);
            }
        }
        if !file && let Some(table) = self.selected_source_table() {
            ui.collapsing("生成的 SELECT", |ui| {
                ui.add(
                    egui::Label::new(egui::RichText::new(&table.select_sql).monospace())
                        .wrap()
                        .selectable(true),
                );
            });
        }
    }

    fn select_target(&mut self, mode: TableMode, object: Option<DbObject>) {
        self.mode = mode;
        if self.object != object {
            self.automatic_target = false;
        }
        self.object = object;
        if !self.mode.defines_table() {
            self.structure_only = false;
            self.engine = None;
            self.clear_metadata();
        }
        if self.structure_only {
            self.after_import = None;
        }
        self.target = None;
        self.target_ddl = None;
        self.target_requested = false;
        self.conflict = OdbcConflict::Abort;
        self.create_sql = None;
        self.source = Source::Form;
        self.error = None;
    }

    fn clear_metadata(&mut self) {
        self.indexes_from = None;
        self.copy_foreign_keys = false;
    }

    fn metadata_ready(&self) -> bool {
        self.indexes_from
            .as_ref()
            .is_none_or(valid_metadata_locator)
            && (!self.copy_foreign_keys || self.indexes_from.is_some())
    }

    fn object(&self) -> Option<DbObject> {
        if self.mode == TableMode::Create {
            let name = self.new_table.trim();
            (!name.is_empty()).then(|| DbObject {
                schema: self.schema.clone(),
                name: name.to_owned(),
                kind: ObjectKind::Table,
            })
        } else {
            self.object.clone()
        }
    }

    fn target_ready(&self) -> bool {
        match self.mode {
            TableMode::Create => !self.new_table.trim().is_empty(),
            TableMode::Recreate => self.object.is_some() && self.target_ddl.is_some(),
            TableMode::Append | TableMode::Truncate => {
                self.object.is_some()
                    && self
                        .target
                        .as_ref()
                        .is_some_and(|columns| !columns.is_empty())
                    && (self.mode == TableMode::Append || self.target_ddl.is_some())
            }
        }
    }

    fn read_source(&mut self) {
        self.error = None;
        let table = self.new_table.trim();
        if self.mode == TableMode::Create
            && (table.chars().count() > 64 || table.contains(['\0', '.', '/', '\\']))
        {
            self.error = Some("请输入合法的新表名，最多 64 个字符。".into());
            return;
        }
        let structure_only = self.structure_only;
        if self.source_mode == SourceMode::File {
            let Some(name) = self
                .selected_source_table()
                .map(|table| table.source.table.clone())
            else {
                self.error = Some("请选择要导入的数据表。".into());
                return;
            };
            let file = self.file_source.clone();
            let password = self.access_password.clone();
            let copy_indexes = self.indexes_from.is_some();
            let copy_foreign_keys = self.copy_foreign_keys;
            self.start_read(move || {
                let data = if !password.is_empty() {
                    file_source::read_with_access_password(
                        &file,
                        &name,
                        structure_only,
                        copy_indexes,
                        copy_foreign_keys,
                        password.as_str(),
                    )
                } else if copy_indexes {
                    file_source::read_with_metadata(
                        &file,
                        &name,
                        structure_only,
                        copy_indexes,
                        copy_foreign_keys,
                    )
                } else {
                    file_source::read(&file, &name, structure_only)
                };
                data.map(|data| SourceRead { data, action: None })
            });
            return;
        }
        if !self.metadata_ready() {
            self.error = Some("请填写准确的 ODBC 源表名。目录和架构请按驱动定义填写。".into());
            return;
        }
        let Some(sql) = self.source_query().map(str::to_owned) else {
            self.error = Some("请选择源表或填写源查询。".into());
            return;
        };
        let connection = self.connection.clone();
        let indexes_from = self.indexes_from.clone();
        let copy_foreign_keys = self.copy_foreign_keys;
        if structure_only {
            self.after_import = None;
        }
        let after_import = self.after_import.clone();
        self.start_read(move || {
            let connection = connection.connection_string()?;
            if let Some(action) = &after_import {
                odbc::read_with_source_action(
                    connection,
                    &sql,
                    indexes_from.as_ref(),
                    copy_foreign_keys,
                    action,
                )
                .map(|(data, action)| SourceRead {
                    data,
                    action: Some(action),
                })
            } else {
                let data = if let Some(table) = &indexes_from {
                    odbc::read_with_metadata(
                        connection,
                        &sql,
                        table,
                        copy_foreign_keys,
                        structure_only,
                    )
                } else if structure_only {
                    odbc::read_columns(connection, &sql)
                } else {
                    odbc::read(connection, &sql)
                };
                data.map(|data| SourceRead { data, action: None })
            }
        });
    }

    fn start_read(&mut self, read: impl FnOnce() -> Result<SourceRead, String> + Send + 'static) {
        let (sender, receiver) = mpsc::sync_channel(1);
        match std::thread::Builder::new()
            .name("harbor-import-source".into())
            .spawn(move || {
                let _ = sender.send(read());
            }) {
            Ok(_) => self.source = Source::Reading(receiver),
            Err(error) => self.error = Some(format!("无法启动源数据读取任务。 {error}")),
        }
    }

    fn take_target(
        &mut self,
        confirm: impl FnOnce(TableMode, &DbObject) -> bool,
    ) -> Option<(OdbcTarget, Option<PreparedSourceAction>)> {
        if !self.target_ready() {
            self.error = Some("目标表信息已变化，请重新读取目标和源数据。".into());
            return None;
        }
        let Source::Ready {
            mapping,
            columns: source_columns,
            metadata,
            action,
            ..
        } = &self.source
        else {
            return None;
        };
        if self.indexes_from.is_some() && metadata.is_none() {
            self.error = Some("源表的索引或外键信息已失效，请重新读取源数据。".into());
            return None;
        }
        if self.after_import.is_some() != action.is_some()
            || self.structure_only && action.is_some()
        {
            self.error = Some("源表操作已变化，请重新读取本次要导入的源数据。".into());
            return None;
        }
        let object = self.object()?;
        if let Some(engine) = &self.engine
            && let Err(error) = validate_odbc_engine(engine)
        {
            self.error = Some(error);
            return None;
        }
        if let Some(sql) = &self.create_sql
            && let Err(error) = validate_odbc_create_table_sql(&object, sql)
        {
            self.error = Some(error);
            return None;
        }
        let selected = match mapping {
            Mapping::Existing(mapping) => {
                let target = self.target.as_ref()?;
                if mapping
                    .iter()
                    .flatten()
                    .any(|index| target.get(*index).is_none())
                {
                    self.error = Some("目标表的列已变化，请重新读取目标和源数据。".into());
                    return None;
                }
                mapping
                    .iter()
                    .enumerate()
                    .filter_map(|(source, column)| {
                        column
                            .and_then(|index| target.get(index))
                            .map(|name| (source, name.clone()))
                    })
                    .collect::<Vec<_>>()
            }
            Mapping::New(columns) => selected_new_columns(columns),
        };
        let mut used = HashSet::new();
        if selected.is_empty()
            || selected.iter().any(|(_, name)| {
                name.is_empty()
                    || self.mode.defines_table()
                        && (name.chars().count() > 64 || name.contains('\0'))
                    || !used.insert(name.to_lowercase())
            })
        {
            self.error = Some("请指定至少一个有效的目标列，每个目标列只能使用一次。".into());
            return None;
        }
        let create = if let Mapping::New(columns) = mapping {
            let mut definitions = Vec::with_capacity(selected.len());
            for (index, name) in &selected {
                let definition = &columns[*index].definition;
                if let Err(error) = definition.validate() {
                    self.error = Some(format!("列 {name} 的类型无效。{error}"));
                    return None;
                }
                definitions.push(definition.clone());
            }
            Some(definitions)
        } else {
            None
        };
        let (indexes, foreign_keys) = match metadata
            .as_ref()
            .map(|source| metadata_plan(source, source_columns, &selected, &object.name))
            .transpose()
        {
            Ok(metadata) => metadata.unwrap_or_default(),
            Err(error) => {
                self.error = Some(error);
                return None;
            }
        };
        if (self.mode.impact().is_some() || action.is_some()) && !confirm(self.mode, &object) {
            return None;
        }
        let Source::Ready {
            rows: source_rows,
            action,
            ..
        } = std::mem::replace(&mut self.source, Source::Form)
        else {
            return None;
        };
        let mut rows = Vec::new();
        if !self.structure_only {
            rows.extend(source_rows.into_iter().map(|mut row| {
                selected
                    .iter()
                    .map(|(index, _)| std::mem::replace(&mut row[*index], Cell::Null))
                    .collect()
            }));
        }
        let columns = selected.into_iter().map(|(_, name)| name).collect();
        Some((
            OdbcTarget {
                object,
                columns,
                rows,
                create,
                create_sql: self.create_sql.take(),
                engine: self.engine.clone(),
                indexes,
                foreign_keys,
                track_applied_rows: false,
                conflict: if self.structure_only {
                    OdbcConflict::Abort
                } else {
                    self.conflict
                },
                reset: self.mode.reset(),
                expected_ddl: self.target_ddl.take(),
            },
            action,
        ))
    }

    fn import(&mut self, workspace: &mut Workspace) -> bool {
        let source = self
            .after_import
            .as_ref()
            .map(source_action_effect)
            .into_iter()
            .collect::<Vec<_>>();
        let Some((target, action)) = self.take_target(|mode, object| {
            confirm_import(
                "确认导入数据",
                mode,
                &[target_name(&object.schema, &object.name)],
                &source,
            )
        }) else {
            return false;
        };
        let label = match (self.mode, self.structure_only) {
            (TableMode::Recreate, true) => "正在重建目标表结构…",
            (TableMode::Create, true) => "正在创建目标表结构…",
            (TableMode::Recreate, false) => "正在重建目标表并写入数据…",
            (TableMode::Create, false) => "正在创建目标表并写入数据…",
            (TableMode::Truncate, _) => "正在清空目标表并写入数据…",
            (TableMode::Append, _) => "正在写入目标表…",
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let (command, label) = if let Some(source) = action {
            (
                Command::ImportOdbcTableWithSourceAction {
                    table: target,
                    source,
                    cancel,
                },
                "正在导入数据，并准备处理目标库已确认写入的源记录…",
            )
        } else {
            (
                Command::ImportOdbcTables {
                    tables: vec![target],
                    cancel,
                },
                label,
            )
        };
        if workspace.send(command, label) {
            return true;
        }
        self.error = Some(format!("无法将数据写入目标表。 {}", workspace.status));
        false
    }
}

pub(super) fn dialog(
    ctx: &egui::Context,
    state: &mut Option<Import>,
    workspaces: &mut [Workspace],
) {
    let Some(import) = state else { return };
    let Some(workspace) = workspaces.get_mut(import.index) else {
        *state = None;
        return;
    };
    import.poll(workspace);
    let can_read = workspace.server.is_some();
    let can_write = workspace.server.is_some() && !workspace.busy();
    let mut read = false;
    let mut write = false;
    let mut close = false;
    let mut back = false;
    let mut reload = false;
    let mut load_engines = false;
    let mut metadata_valid = true;
    let mut object = import.object.clone();
    let mut mode = import.mode;
    let selected_target = import.object();
    style::modal(ctx, egui::Id::new("odbc-import")).show(ctx, |ui| {
        ui.set_width(form_width(ctx, 690.0));
        ui.heading("导入外部数据");
        ui.add_space(4.0);
        let configuring = matches!(import.source, Source::Form);
        form_scroll(ctx, 140.0).show(ui, |ui| {
            form_section(ui, "目标表", |ui| {
                if configuring {
                    TableMode::picker(ui, "odbc-import-mode", &mut mode, import.choose_table);
                }
                if import.choose_table && configuring {
                    let schema = format!("{}.", import.schema);
                    ui.horizontal_wrapped(|ui| {
                        ui.add_sized(
                            [
                                ui.available_width().min(150.0),
                                ui.spacing().interact_size.y,
                            ],
                            egui::Label::new(&schema).truncate(),
                        )
                        .on_hover_text(&schema);
                        if mode == TableMode::Create {
                            ui.add(TextEdit::singleline(&mut import.new_table).hint_text("新表名"));
                            return;
                        }
                        ui.add_enabled_ui(
                            import
                                .tables
                                .as_ref()
                                .is_some_and(|tables| !tables.is_empty()),
                            |ui| {
                                let label = object
                                    .as_ref()
                                    .map_or("请选择数据表", |table| table.name.as_str())
                                    .to_owned();
                                ComboBox::from_id_salt("odbc-target-table")
                                    .width(ui.available_width().min(300.0))
                                    .truncate()
                                    .selected_text(&label)
                                    .show_ui(ui, |ui| {
                                        for table in import.tables.iter().flatten() {
                                            ui.selectable_value(
                                                &mut object,
                                                Some(table.clone()),
                                                &table.name,
                                            );
                                        }
                                    })
                                    .response
                                    .on_hover_text(label);
                            },
                        );
                    });
                    if mode.uses_existing() && import.tables.as_ref().is_some_and(Vec::is_empty) {
                        ui.label("此数据库没有现有表。可选择创建新表。");
                    }
                } else if let Some(object) = import.object() {
                    let label = target_name(&object.schema, &object.name);
                    ui.add(egui::Label::new(&label).truncate().selectable(true))
                        .on_hover_text(label);
                }
                if configuring && mode.uses_existing() {
                    reload = ui
                        .add_enabled(can_write, egui::Button::new("重新读取目标信息"))
                        .clicked();
                }
                if configuring && mode.defines_table() {
                    ui.checkbox(&mut import.structure_only, "仅导入表结构");
                    engine_fields(ui, &mut import.engine, &workspace.engines);
                    load_engines = ui
                        .add_enabled(can_write, egui::Button::new("读取服务器引擎"))
                        .clicked();
                    if import.structure_only {
                        import.after_import = None;
                    }
                }
                if let Some(impact) = mode.impact()
                    && let Some(object) = &object
                {
                    ui.colored_label(
                        style::WARNING,
                        format!("{impact}\n{}", target_name(&object.schema, &object.name)),
                    );
                }
                if configuring
                    && mode.uses_existing()
                    && workspace.busy()
                    && import.target.is_none()
                {
                    ui.colored_label(
                        style::MUTED,
                        if import.choose_table && import.tables.is_none() {
                            "正在读取目标数据库的表…"
                        } else {
                            "正在读取目标表结构…"
                        },
                    );
                }
            });
            match &mut import.source {
                Source::Form => {
                    form_section(ui, "导入来源", |ui| import.source_form(ui, mode));
                }
                Source::Reading(_) => {
                    form_section(ui, "导入来源", |ui| {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(if import.structure_only {
                                "正在读取源列…"
                            } else {
                                "正在读取源数据…"
                            });
                        });
                    });
                    ctx.request_repaint_after(Duration::from_millis(80));
                }
                Source::Ready {
                    columns,
                    rows,
                    mapping,
                    metadata,
                    ..
                } => {
                    form_section(ui, "源数据", |ui| {
                        if let Some(action) = &import.after_import {
                            ui.colored_label(
                                style::WARNING,
                                format!("导入后的源表操作 · {}", source_action_effect(action)),
                            );
                        }
                        if import.structure_only {
                            ui.label(format!("已读取 {} 个源列。仅导入表结构。", columns.len()));
                        } else {
                            ui.label(format!("已读取 {} 行。", rows.len()));
                            let can_choose_conflict =
                                import.mode.defines_table() || import.target_ddl.is_some();
                            ui.add_enabled_ui(can_choose_conflict, |ui| {
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("主键或唯一索引冲突");
                                    ComboBox::from_id_salt("odbc-import-conflict")
                                        .selected_text(conflict_label(import.conflict))
                                        .show_ui(ui, |ui| {
                                            for conflict in [
                                                OdbcConflict::Abort,
                                                OdbcConflict::KeepTarget,
                                                OdbcConflict::Replace,
                                            ] {
                                                ui.selectable_value(
                                                    &mut import.conflict,
                                                    conflict,
                                                    conflict_label(conflict),
                                                );
                                            }
                                        });
                                });
                            });
                            if !can_choose_conflict {
                                ui.colored_label(
                                    style::MUTED,
                                    "无法核对目标表定义，遇到冲突时结束导入。",
                                );
                            }
                            if import.conflict == OdbcConflict::Replace {
                                ui.colored_label(style::WARNING, REPLACE_WARNING);
                            }
                        }
                    });
                    form_section(ui, "字段映射", |ui| {
                        match mapping {
                            Mapping::Existing(mapping) => mapping_fields(
                                ui,
                                columns,
                                import.target.as_deref().unwrap_or_default(),
                                mapping,
                            ),
                            Mapping::New(mapping) => new_column_fields(ui, columns, mapping),
                        }
                        if let Mapping::New(mapping) = mapping
                            && let Some(target) = &selected_target
                        {
                            let selected = selected_new_columns(mapping);
                            let plan = metadata
                                .as_ref()
                                .map(|source| {
                                    metadata_plan(source, columns, &selected, &target.name)
                                })
                                .transpose()
                                .map(Option::unwrap_or_default);
                            match &plan {
                                Ok((indexes, foreign_keys)) => {
                                    if metadata.is_some() && import.copy_foreign_keys {
                                        ui.label(format!(
                                            "将复制 {} 个索引、{} 个外键。",
                                            indexes.len(),
                                            foreign_keys.len()
                                        ));
                                        if foreign_keys.is_empty() {
                                            ui.label("源表未返回可复制的外键。");
                                        }
                                    } else if metadata.is_some() {
                                        ui.label(format!("将复制 {} 个索引。", indexes.len()));
                                    }
                                }
                                Err(error) => {
                                    metadata_valid = false;
                                    ui.colored_label(style::ERROR, error);
                                }
                            }
                            let sql_valid =
                                create_sql_editor(ui, target, &mut import.create_sql, || {
                                    if selected.is_empty() {
                                        return Err("请先选择要导入的列。".into());
                                    }
                                    let (indexes, _) = plan?;
                                    let definitions = selected
                                        .iter()
                                        .map(|(index, _)| mapping[*index].definition.clone())
                                        .collect::<Vec<_>>();
                                    let names = selected
                                        .into_iter()
                                        .map(|(_, name)| name)
                                        .collect::<Vec<_>>();
                                    odbc_create_table_sql(
                                        target,
                                        &names,
                                        &definitions,
                                        &indexes,
                                        import.engine.as_deref(),
                                    )
                                });
                            metadata_valid &= sql_valid;
                        }
                    });
                }
            }
        });
        ui.separator();
        if let Some(error) = &import.error {
            ui.add(egui::Label::new(egui::RichText::new(error).color(style::ERROR)).truncate())
                .on_hover_text(error);
        }
        right_aligned_row(ui, |ui| {
            match &import.source {
                Source::Form => {
                    read = ui
                        .add_enabled(
                            can_read
                                && import.source_ready()
                                && object == import.object
                                && mode == import.mode
                                && !reload
                                && import.metadata_ready()
                                && import.target_ready(),
                            style::primary(if import.structure_only {
                                "读取源列"
                            } else {
                                "读取源数据"
                            }),
                        )
                        .clicked();
                }
                Source::Reading(_) => {}
                Source::Ready { mapping, .. } => {
                    let selection = match mapping {
                        Mapping::Existing(mapping) => mapping.iter().any(Option::is_some),
                        Mapping::New(mapping) => mapping.iter().any(|column| column.selected),
                    };
                    write = ui
                        .add_enabled(
                            can_write && selection && metadata_valid && import.target_ready(),
                            style::primary(match (import.mode, import.structure_only) {
                                (TableMode::Create, true) => "创建表结构",
                                (TableMode::Recreate, true) => "重建表结构",
                                (TableMode::Create, false) => "创建并导入",
                                (TableMode::Recreate, false) => "重建并导入",
                                (TableMode::Truncate, _) => "清空并导入",
                                (TableMode::Append, _)
                                    if import.conflict == OdbcConflict::Replace =>
                                {
                                    "导入并替换冲突行"
                                }
                                (TableMode::Append, _) => "导入数据",
                            }),
                        )
                        .clicked();
                    back = ui.button("返回设置").clicked();
                }
            }
            close = ui.button("关闭").clicked();
        });
    });
    if load_engines {
        workspace.send(Command::ListEngines, "正在读取可用存储引擎…");
    }
    if close {
        *state = None;
    } else if back || reload || mode != import.mode || object != import.object {
        if reload && import.choose_table {
            import.tables = None;
            import.tables_requested = false;
        }
        import.select_target(mode, object);
    } else if read {
        import.read_source();
    } else if write && import.import(workspace) {
        *state = None;
    }
}

fn conflict_label(conflict: OdbcConflict) -> &'static str {
    match conflict {
        OdbcConflict::Abort => "终止本次导入",
        OdbcConflict::KeepTarget => "保留目标行",
        OdbcConflict::Replace => "用源数据替换整行",
    }
}

pub(super) fn engine_fields(
    ui: &mut egui::Ui,
    selected: &mut Option<String>,
    available: &[String],
) {
    ui.horizontal_wrapped(|ui| {
        ui.label("存储引擎");
        let mut text = selected.clone().unwrap_or_default();
        if ui
            .add(
                TextEdit::singleline(&mut text)
                    .desired_width(130.0)
                    .hint_text("InnoDB"),
            )
            .changed()
        {
            let name = text.trim();
            *selected = (!name.is_empty()).then(|| name.to_owned());
        }
        ComboBox::from_id_salt("odbc-target-engine")
            .selected_text("选择引擎")
            .show_ui(ui, |ui| {
                ui.selectable_value(selected, None, "默认（InnoDB）");
                let common = ["MyISAM", "Aria"];
                let engines = if available.is_empty() {
                    common.into_iter().collect::<Vec<_>>()
                } else {
                    available.iter().map(String::as_str).collect()
                };
                for name in engines {
                    if ui
                        .selectable_label(selected.as_deref() == Some(name), name)
                        .clicked()
                    {
                        *selected = Some(name.to_owned());
                    }
                }
            });
    });
}

pub(super) fn create_sql_editor(
    ui: &mut egui::Ui,
    object: &DbObject,
    custom: &mut Option<String>,
    generate: impl FnOnce() -> Result<String, String>,
) -> bool {
    let validity = custom
        .as_deref()
        .map(|sql| validate_odbc_create_table_sql(object, sql));
    ui.collapsing("编辑建表 SQL", |ui| {
        if custom.is_some() && ui.button("按列映射重新生成").clicked() {
            *custom = None;
            return;
        }
        let mut text = match custom.as_ref() {
            Some(sql) => sql.clone(),
            None => match generate() {
                Ok(sql) => sql,
                Err(error) => {
                    ui.colored_label(style::ERROR, error);
                    return;
                }
            },
        };
        egui::ScrollArea::vertical()
            .max_height(180.0)
            .show(ui, |ui| {
                if ui
                    .add(
                        TextEdit::multiline(&mut text)
                            .code_editor()
                            .desired_rows(7)
                            .desired_width(f32::INFINITY),
                    )
                    .changed()
                {
                    *custom = Some(text);
                }
            });
        if custom.is_some() {
            ui.label("导入时使用编辑后的建表 SQL。");
        }
        if let Some(Err(error)) = &validity {
            ui.colored_label(style::ERROR, error);
        }
    });
    custom
        .as_deref()
        .is_none_or(|sql| validate_odbc_create_table_sql(object, sql).is_ok())
}

fn mapping_fields(
    ui: &mut egui::Ui,
    sources: &[SourceColumn],
    target: &[String],
    mapping: &mut [Option<usize>],
) {
    let width = ui.available_width();
    egui::ScrollArea::vertical()
        .id_salt("odbc-existing-mapping")
        .max_height(260.0)
        .show(ui, |ui| {
            egui::Grid::new("odbc-import-mapping")
                .num_columns(2)
                .striped(true)
                .show(ui, |ui| {
                    ui.strong("源列");
                    ui.strong("目标列");
                    ui.end_row();
                    for (index, (source, selected)) in sources.iter().zip(mapping).enumerate() {
                        source_name(ui, source, index, width * 0.4);
                        let label = selected.map_or("跳过", |index| target[index].as_str());
                        ComboBox::from_id_salt(("odbc-import-column", index))
                            .width(width * 0.48)
                            .truncate()
                            .selected_text(label)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(selected, None, "跳过");
                                for (index, name) in target.iter().enumerate() {
                                    ui.selectable_value(selected, Some(index), name);
                                }
                            })
                            .response
                            .on_hover_text(label);
                        ui.end_row();
                    }
                });
        });
}

fn new_column_fields(ui: &mut egui::Ui, sources: &[SourceColumn], columns: &mut [NewColumn]) {
    let width = ui.available_width();
    egui::ScrollArea::vertical()
        .id_salt("odbc-new-mapping")
        .max_height(260.0)
        .show(ui, |ui| {
            egui::Grid::new("odbc-create-columns")
                .num_columns(5)
                .striped(true)
                .show(ui, |ui| {
                    ui.strong("导入");
                    ui.strong("源列");
                    ui.strong("新表列名");
                    ui.strong("MySQL 类型");
                    ui.strong("主键");
                    ui.end_row();
                    for (index, (source, column)) in sources.iter().zip(columns).enumerate() {
                        ui.checkbox(&mut column.selected, "");
                        source_name(ui, source, index, width * 0.28);
                        ui.add_enabled(
                            column.selected,
                            TextEdit::singleline(&mut column.name).desired_width(width * 0.22),
                        );
                        ui.add_enabled(
                            column.selected,
                            TextEdit::singleline(&mut column.definition.sql_type)
                                .desired_width(width * 0.21),
                        );
                        ui.add_enabled(
                            column.selected,
                            egui::Checkbox::without_text(&mut column.definition.primary),
                        );
                        ui.end_row();
                    }
                });
        });
}

fn source_name(ui: &mut egui::Ui, column: &SourceColumn, index: usize, width: f32) {
    if column.name.is_empty() {
        ui.colored_label(style::MUTED, format!("未命名列 {}", index + 1));
    } else {
        ui.add_sized(
            [width, ui.spacing().interact_size.y],
            egui::Label::new(&column.name).truncate(),
        )
        .on_hover_text(&column.name);
    }
}

fn new_columns(sources: &[SourceColumn]) -> Vec<NewColumn> {
    let mut used = HashSet::new();
    let mut serial = 0;
    sources
        .iter()
        .map(|source| {
            let mut name = source.name.trim().to_owned();
            while name.is_empty()
                || name.chars().count() > 64
                || name.contains('\0')
                || !used.insert(name.to_lowercase())
            {
                serial += 1;
                name = format!("column_{serial}");
            }
            NewColumn {
                name,
                selected: true,
                definition: source.definition.clone(),
            }
        })
        .collect()
}

fn initial_mapping<'a>(
    sources: impl IntoIterator<Item = &'a str>,
    target: &[String],
) -> Vec<Option<usize>> {
    let names: Vec<_> = target.iter().map(|name| name.to_lowercase()).collect();
    let mut used = HashSet::new();
    sources
        .into_iter()
        .map(|source| {
            let source = source.to_lowercase();
            names
                .iter()
                .position(|name| name == &source)
                .filter(|index| used.insert(*index))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovered_source_keeps_exact_locator_and_switches_back_to_manual_sql() {
        let mut import = Import::for_schema(0, "目标库".into());
        import.sql = "SELECT id FROM 手写表".into();
        let discovery = |schema: &str, query: &str| DiscoveredTable {
            source: SourceTable {
                catalog: Some("原始目录".into()),
                schema: Some(schema.into()),
                table: "同名表".into(),
            },
            select_sql: query.into(),
        };
        import.set_source_mode(SourceMode::Table);
        import.source_tables = SourceTables::Listed {
            tables: vec![
                discovery("源一", "SELECT * FROM [原始目录].[源一].[同名表]"),
                discovery("源二", "SELECT * FROM [原始目录].[源二].[同名表]"),
            ],
            selected: None,
        };
        import.select_source_table(0);
        assert_eq!(
            import.source_query(),
            Some("SELECT * FROM [原始目录].[源一].[同名表]")
        );
        let locator = import.selected_source_table().unwrap().source.clone();
        import.indexes_from = Some(locator.clone());
        import.copy_foreign_keys = true;
        import.after_import = Some(SourceAction {
            table: locator,
            change: SourceChange::Delete,
        });
        assert_eq!(
            source_action_effect(import.after_import.as_ref().unwrap()),
            "`原始目录`.`源一`.`同名表` · 删除已导入的行"
        );
        import.select_source_table(1);
        assert_eq!(
            import
                .selected_source_table()
                .unwrap()
                .source
                .schema
                .as_deref(),
            Some("源二")
        );
        assert_eq!(
            import.source_query(),
            Some("SELECT * FROM [原始目录].[源二].[同名表]")
        );
        assert!(import.indexes_from.is_none() && import.after_import.is_none());
        assert!(!import.copy_foreign_keys);
        import.set_source_mode(SourceMode::Query);
        assert_eq!(import.source_query(), Some("SELECT id FROM 手写表"));
    }

    #[test]
    fn odbc_file_source_defaults_to_the_selected_csv_name_and_reads_without_a_driver() {
        let executable = std::env::current_exe().unwrap();
        let directory = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let file = directory.path().join("商品.csv");
        std::fs::write(&file, "编号,名称\n7,茶\n").unwrap();
        let mut import = Import::for_schema(0, "目标库".into());
        import.set_source_mode(SourceMode::File);
        import.select_target(TableMode::Create, None);
        import.file_source.path = file;
        import.load_source_tables();
        let SourceTables::Loading(receiver) = &import.source_tables else {
            panic!("应从源文件读取数据表列表");
        };
        let tables = receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(tables[0].source.table, "商品");
        import.source_tables = SourceTables::Listed {
            tables,
            selected: None,
        };
        import.select_source_table(0);
        assert!(import.source_ready() && import.target_ready());
        assert_eq!(import.object().unwrap().name, "商品");
        assert!(import.source_query().is_none());

        import.read_source();
        let Source::Reading(receiver) = import.source else {
            panic!("应从 CSV 读取源数据");
        };
        let selected = receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert!(selected.action.is_none());
        assert_eq!(selected.data.columns[0].name, "编号");
        assert_eq!(selected.data.rows[0][1], Cell::Text("茶".into()));
    }

    #[test]
    fn odbc_access_password_edit_refreshes_discovery_and_stays_out_of_file_settings() {
        let mut import = Import::for_schema(0, "目标库".into());
        import.set_source_mode(SourceMode::File);
        import.file_source.format = FileSourceFormat::Access;
        import.file_source.path = "仓库.accdb".into();
        import.source_tables = SourceTables::Listed {
            tables: vec![file_fields::discovered_table("出库明细".into())],
            selected: Some(0),
        };
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                import.source_form(ui, TableMode::Append);
                let password_id =
                    ui.make_persistent_id(egui::Id::new("odbc-import-access-password"));
                ui.memory_mut(|memory| memory.request_focus(password_id));
            });
        });
        let input = egui::RawInput {
            events: vec![egui::Event::Text("示例临时文本".into())],
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| import.source_form(ui, TableMode::Append));
        });

        assert_eq!(import.access_password.as_str(), "示例临时文本");
        assert!(matches!(import.source_tables, SourceTables::Idle));
        assert!(import.file_source.access_password_env.is_none());

        import.set_source_mode(SourceMode::Query);
        assert!(import.access_password.is_empty());
    }

    #[test]
    fn odbc_access_link_folder_edit_refreshes_discovery_and_keeps_the_file_password() {
        let mut import = Import::for_schema(0, "目标库".into());
        import.set_source_mode(SourceMode::File);
        import.file_source.format = FileSourceFormat::Access;
        import.file_source.path = "仓库.accdb".into();
        import.access_password.push_str("示例临时文本");
        import.source_tables = SourceTables::Listed {
            tables: vec![file_fields::discovered_table("出库明细".into())],
            selected: Some(0),
        };
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                import.source_form(ui, TableMode::Append);
                let folder_id = ui.make_persistent_id(egui::Id::new("access-link-folder"));
                ui.memory_mut(|memory| memory.request_focus(folder_id));
            });
        });
        let input = egui::RawInput {
            events: vec![egui::Event::Text("库存来源".into())],
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| import.source_form(ui, TableMode::Append));
        });

        assert_eq!(
            import.file_source.access_link_folder.as_deref(),
            Some(std::path::Path::new("库存来源"))
        );
        assert!(matches!(import.source_tables, SourceTables::Idle));
        assert_eq!(import.access_password.as_str(), "示例临时文本");
    }

    #[test]
    fn odbc_foreign_keys_keep_a_source_locator_and_clear_when_appending() {
        let mut locator = None;
        set_metadata_selection(&mut locator, false, true);
        locator.as_mut().unwrap().table = "源商品".into();
        assert!(valid_metadata_locator(locator.as_ref().unwrap()));

        let mut import = Import::for_schema(0, "目标库".into());
        import.select_target(TableMode::Create, None);
        import.indexes_from = locator;
        import.copy_foreign_keys = true;
        assert!(import.metadata_ready());
        import.select_target(TableMode::Append, None);
        assert!(import.indexes_from.is_none() && !import.copy_foreign_keys);
    }

    #[test]
    fn odbc_target_modes_desktop_reset_mapping_and_confirm_current_target() {
        let earlier = DbObject {
            schema: "导入库".into(),
            name: "旧表".into(),
            kind: ObjectKind::Table,
        };
        let target = DbObject {
            name: "商品表".into(),
            ..earlier.clone()
        };
        let columns = || {
            vec![SourceColumn {
                name: "编号".into(),
                definition: OdbcColumnDefinition {
                    sql_type: "INT".into(),
                    ..Default::default()
                },
            }]
        };
        let mut import = Import::new(0, earlier);
        import.target_ddl = Some("旧表定义".into());
        import.target = Some(vec!["编号".into()]);
        import.source = Source::Ready {
            columns: columns(),
            rows: vec![vec![Cell::Text("7".into())]],
            mapping: Mapping::Existing(vec![Some(0)]),
            metadata: None,
            action: None,
        };
        import.select_target(TableMode::Truncate, Some(target.clone()));
        assert!(matches!(import.source, Source::Form));
        assert!(import.target_ddl.is_none() && !import.target_ready());
        import.target = Some(vec!["编号".into()]);
        import.target_ddl = Some("当前表定义".into());
        import.source = Source::Ready {
            columns: columns(),
            rows: vec![vec![Cell::Text("8".into())]],
            mapping: Mapping::Existing(vec![Some(0)]),
            metadata: None,
            action: None,
        };
        assert!(
            import
                .take_target(|mode, object| {
                    assert!(mode == TableMode::Truncate);
                    assert_eq!(object, &target);
                    false
                })
                .is_none()
        );
        assert!(matches!(import.source, Source::Ready { .. }));
        let (write, source) = import.take_target(|_, _| true).unwrap();
        assert!(write.reset == OdbcReset::Truncate && write.create.is_none());
        assert!(source.is_none());
        assert_eq!(write.expected_ddl.as_deref(), Some("当前表定义"));
        assert_eq!(write.rows, [vec![Cell::Text("8".into())]]);

        import.select_target(TableMode::Recreate, Some(target.clone()));
        import.target_ddl = Some("仍是当前表定义".into());
        import.structure_only = true;
        let columns = columns();
        import.source = Source::Ready {
            mapping: Mapping::New(new_columns(&columns)),
            columns,
            rows: Vec::new(),
            metadata: None,
            action: None,
        };
        let (structure, source) = import
            .take_target(|mode, object| mode == TableMode::Recreate && object == &target)
            .unwrap();
        assert!(structure.reset == OdbcReset::Recreate && structure.rows.is_empty());
        assert!(source.is_none());
        assert!(structure.foreign_keys.is_empty());
        assert_eq!(structure.create.as_ref().unwrap()[0].sql_type, "INT");
    }

    #[test]
    fn matches_named_source_columns_once() {
        let sources = ["编号", "NAME", "name", "来源"];
        let target = ["name", "编号", "备注"].map(str::to_owned);
        assert_eq!(
            initial_mapping(sources, &target),
            [Some(1), Some(0), None, None]
        );
    }
}
