use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::ops::Range;
use std::path::{Path, PathBuf};

use eframe::egui::{self, ScrollArea, TextEdit};
use serde::{Deserialize, Serialize};

use super::state::{QueryTab, profile_path, write_atomic};
use super::style;

type Id = u64;

const VERSION: u32 = 1;
const MAX_SCRIPT: u64 = 8 * 1024 * 1024;

pub(super) enum EditorAction {
    Insert(String),
    Open { sql: String, path: Option<PathBuf> },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Library {
    version: u32,
    next_id: Id,
    entries: BTreeMap<Id, Entry>,
}

impl Default for Library {
    fn default() -> Self {
        Self {
            version: VERSION,
            next_id: 1,
            entries: BTreeMap::new(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    name: String,
    parent: Option<Id>,
    #[serde(flatten)]
    kind: EntryKind,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum EntryKind {
    Folder,
    Sql { sql: String },
    Link { path: PathBuf },
}

impl Entry {
    fn folder(&self) -> bool {
        matches!(self.kind, EntryKind::Folder)
    }

    fn read(&self) -> Result<(String, Option<PathBuf>), String> {
        match &self.kind {
            EntryKind::Sql { sql } => Ok((sql.clone(), None)),
            EntryKind::Link { path } => read_link(path).map(|sql| (sql, Some(path.clone()))),
            EntryKind::Folder => Err("请选择 SQL 收藏。".into()),
        }
    }
}

impl Library {
    fn load(path: &Path) -> Result<Self, String> {
        match fs::read(path) {
            Ok(bytes) => {
                let library: Self = serde_json::from_slice(&bytes)
                    .map_err(|error| format!("收藏夹文件格式错误：{error}"))?;
                library.validate()?;
                Ok(library)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("无法读取收藏夹：{error}")),
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != VERSION {
            return Err("收藏夹文件版本不受支持。".into());
        }
        if self.next_id == 0 {
            return Err("收藏夹条目标识无效。".into());
        }
        let mut siblings = BTreeSet::new();
        let mut validated = BTreeSet::new();
        for (&id, entry) in &self.entries {
            if id == 0 || id >= self.next_id {
                return Err("收藏夹条目标识无效。".into());
            }
            if name_text(&entry.name)? != entry.name
                || !siblings.insert((entry.parent, entry.name.to_lowercase()))
            {
                return Err("收藏夹条目名称无效或重复。".into());
            }
            let mut chain = BTreeSet::new();
            let mut parent = entry.parent;
            while let Some(folder) = parent {
                let destination = self
                    .entries
                    .get(&folder)
                    .filter(|destination| destination.folder())
                    .ok_or("收藏夹中的父文件夹不存在。")?;
                if validated.contains(&folder) {
                    break;
                }
                if folder == id || !chain.insert(folder) {
                    return Err("收藏夹的文件夹关系形成循环。".into());
                }
                parent = destination.parent;
            }
            validated.extend(chain);
            validated.insert(id);
        }
        Ok(())
    }

    fn add(&mut self, parent: Option<Id>, name: &str, kind: EntryKind) -> Result<Id, String> {
        let name = self.check_name(parent, name, None)?;
        let id = self.next_id;
        let next_id = id.checked_add(1).ok_or("收藏夹的条目标识已用尽。")?;
        self.entries.insert(id, Entry { name, parent, kind });
        self.next_id = next_id;
        Ok(id)
    }

    fn rename(&mut self, id: Id, name: &str) -> Result<(), String> {
        let original = self.entries.get(&id).ok_or("收藏已不存在。")?;
        let name = self.check_name(original.parent, name, Some(id))?;
        self.entries.get_mut(&id).expect("existing entry").name = name;
        Ok(())
    }

    fn move_to(&mut self, id: Id, parent: Option<Id>) -> Result<(), String> {
        let original = self.entries.get(&id).ok_or("收藏已不存在。")?;
        self.check_name(parent, &original.name, Some(id))?;
        if parent.is_some_and(|folder| self.within(folder, id)) {
            return Err("请选择原文件夹以外的目标位置。".into());
        }
        self.entries.get_mut(&id).expect("existing entry").parent = parent;
        Ok(())
    }

    fn replace_link(&mut self, id: Id, path: PathBuf) -> Result<(), String> {
        let entry = self.entries.get_mut(&id).ok_or("收藏已不存在。")?;
        let EntryKind::Link { path: current } = &mut entry.kind else {
            return Err("请选择文件链接。".into());
        };
        *current = path;
        Ok(())
    }

    fn delete(&mut self, id: Id) -> Result<(), String> {
        if !self.entries.contains_key(&id) {
            return Err("收藏已不存在。".into());
        }
        let removed = self.descendants(id);
        self.entries.retain(|id, _| !removed.contains(id));
        Ok(())
    }

    fn check_name(
        &self,
        parent: Option<Id>,
        name: &str,
        ignore: Option<Id>,
    ) -> Result<String, String> {
        if parent.is_some_and(|id| !self.entries.get(&id).is_some_and(Entry::folder)) {
            return Err("请选择有效的文件夹。".into());
        }
        let name = name_text(name)?;
        let folded = name.to_lowercase();
        if self.entries.iter().any(|(&id, entry)| {
            entry.parent == parent && Some(id) != ignore && entry.name.to_lowercase() == folded
        }) {
            return Err("该文件夹中已有同名条目。".into());
        }
        Ok(name.into())
    }

    fn children(&self, parent: Option<Id>) -> Vec<(Id, &Entry)> {
        let mut items = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.parent == parent)
            .map(|(&id, entry)| (id, entry))
            .collect::<Vec<_>>();
        items.sort_by_key(|(_, entry)| (!entry.folder(), entry.name.to_lowercase()));
        items
    }

    fn within(&self, mut id: Id, ancestor: Id) -> bool {
        loop {
            if id == ancestor {
                return true;
            }
            let Some(parent) = self.entries.get(&id).and_then(|entry| entry.parent) else {
                return false;
            };
            id = parent;
        }
    }

    fn descendants(&self, id: Id) -> BTreeSet<Id> {
        self.entries
            .keys()
            .copied()
            .filter(|&item| self.within(item, id))
            .collect()
    }

    fn folder_label(&self, id: Option<Id>) -> String {
        let mut names = Vec::new();
        let mut parent = id;
        while let Some(id) = parent {
            let Some(entry) = self.entries.get(&id) else {
                break;
            };
            names.push(entry.name.as_str());
            parent = entry.parent;
        }
        if names.is_empty() {
            "收藏夹".into()
        } else {
            names.reverse();
            names.join(" › ")
        }
    }
}

fn name_text(name: &str) -> Result<&str, String> {
    let name = name.trim();
    if name.is_empty() || name.chars().any(char::is_control) {
        Err("请输入名称。".into())
    } else {
        Ok(name)
    }
}

fn library_path() -> Result<PathBuf, String> {
    profile_path()
        .map(|path| path.with_file_name("favorites.json"))
        .ok_or_else(|| "无法找到本机的配置目录。".into())
}

fn update_library<T>(
    path: &Path,
    previous: &Library,
    update: impl FnOnce(&mut Library) -> Result<T, String>,
) -> Result<(Library, T), String> {
    let directory = path.parent().ok_or("无法找到收藏夹的配置目录。")?;
    fs::create_dir_all(directory).map_err(|error| format!("无法创建收藏夹目录：{error}"))?;
    // Lock the adjacent file so that replacing the manifest does not release the lock.
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))
        .map_err(|error| format!("无法打开收藏夹锁：{error}"))?;
    lock.try_lock()
        .map_err(|error| format!("收藏夹无法锁定，请稍后重试：{error}"))?;
    let mut current = Library::load(path)?;
    if current != *previous {
        return Err("收藏夹已在其他窗口中更新，请刷新后再试。".into());
    }
    let result = update(&mut current)?;
    if current != *previous {
        current.validate()?;
        let bytes = serde_json::to_vec_pretty(&current)
            .map_err(|error| format!("无法保存收藏夹内容：{error}"))?;
        write_atomic(path, &bytes).map_err(|error| format!("无法写入收藏夹：{error}"))?;
    }
    Ok((current, result))
}

fn read_link(path: &Path) -> Result<String, String> {
    let read = || -> io::Result<String> {
        if !path.is_absolute() {
            return Err(io::Error::other("请重新选择本机上的 SQL 文件。"));
        }
        let metadata = fs::metadata(path)?;
        if !metadata.is_file() {
            return Err(io::Error::other("请选择普通 SQL 文件。"));
        }
        if metadata.len() > MAX_SCRIPT {
            return Err(io::Error::other("SQL 编辑器可打开的文件上限为 8 MiB。"));
        }
        let file = File::open(path)?;
        let mut bytes = Vec::new();
        file.take(MAX_SCRIPT + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_SCRIPT {
            return Err(io::Error::other("SQL 编辑器可打开的文件上限为 8 MiB。"));
        }
        let mut sql = String::from_utf8(bytes)
            .map_err(|_| io::Error::other("SQL 文件需使用 UTF-8 编码。"))?;
        if sql.starts_with('\u{feff}') {
            sql.drain(..'\u{feff}'.len_utf8());
        }
        Ok(sql)
    };
    read().map_err(|error| format!("无法打开 {}：{error}", path.display()))
}

#[derive(Default)]
pub(super) struct Favorites {
    library: Option<Library>,
    read_error: Option<String>,
    window: Option<Window>,
}

enum Window {
    Add(Add),
    Manage(Manage),
}

impl Favorites {
    fn load(&mut self) {
        match library_path().and_then(|path| Library::load(&path)) {
            Ok(library) => {
                self.library = Some(library);
                self.read_error = None;
            }
            Err(error) => self.read_error = Some(error),
        }
    }

    fn load_once(&mut self) {
        if self.library.is_none() && self.read_error.is_none() {
            self.load();
        }
    }

    fn change<T>(
        &mut self,
        update: impl FnOnce(&mut Library) -> Result<T, String>,
    ) -> Result<T, String> {
        if let Some(error) = &self.read_error {
            return Err(error.clone());
        }
        let library = self.library.as_ref().ok_or("请先加载收藏夹。")?;
        let (next, result) = update_library(&library_path()?, library, update)?;
        self.library = Some(next);
        Ok(result)
    }

    pub(super) fn active(&self) -> bool {
        self.window.is_some()
    }

    pub(super) fn add(&mut self, tab: &QueryTab) {
        self.load_once();
        self.window = Some(Window::Add(Add::new(tab)));
    }

    pub(super) fn menu(&mut self, ui: &mut egui::Ui, tab: &QueryTab) -> Option<EditorAction> {
        self.load_once();
        if ui.button("添加到收藏夹").clicked() {
            self.add(tab);
            ui.close();
        }
        if ui.button("管理收藏夹").clicked() {
            self.window = Some(Window::Manage(Manage::default()));
            ui.close();
        }
        if ui.button("刷新收藏夹").clicked() {
            self.load();
            if self.read_error.is_some() {
                self.window = Some(Window::Manage(Manage::default()));
            }
            ui.close();
        }
        ui.separator();
        if let Some(error) = &self.read_error {
            ui.colored_label(style::error(), error);
        }
        let library = self.library.as_ref()?;
        let id = menu_entries(ui, library, None)?;
        ui.close();
        match library.entries[&id].read() {
            Ok((sql, _)) => Some(EditorAction::Insert(sql)),
            Err(error) => {
                self.window = Some(Window::Manage(Manage {
                    selected: Some(id),
                    error: Some(error),
                    ..Manage::default()
                }));
                None
            }
        }
    }

    pub(super) fn dialog(&mut self, ctx: &egui::Context) -> Option<EditorAction> {
        let mut window = self.window.take()?;
        let height = ctx.content_rect().height();
        match &mut window {
            Window::Add(form) => {
                let response = crate::app::style::modal(ctx, egui::Id::new("favorite-add"))
                    .show(ctx, |ui| self.add_form(ui, form, height));
                if !response.inner && !response.should_close() {
                    self.window = Some(window);
                }
                None
            }
            Window::Manage(manager) => {
                let response = crate::app::style::modal(ctx, egui::Id::new("favorite-manage"))
                    .show(ctx, |ui| self.manage_form(ui, manager, height));
                let dismissed = response.should_close();
                let (close, action) = response.inner;
                if !close && action.is_none() && !dismissed {
                    self.window = Some(window);
                }
                action
            }
        }
    }
}

fn menu_entries(ui: &mut egui::Ui, library: &Library, parent: Option<Id>) -> Option<Id> {
    let children = library.children(parent);
    if children.is_empty() {
        ui.label("无收藏");
        return None;
    }
    for (id, entry) in children {
        let picked = if entry.folder() {
            ui.menu_button(&entry.name, |ui| menu_entries(ui, library, Some(id)))
                .inner
                .flatten()
        } else {
            ui.button(&entry.name).clicked().then_some(id)
        };
        if picked.is_some() {
            ui.close();
            return picked;
        }
    }
    None
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AddSource {
    Selection,
    Editor,
    Link,
}

struct Add {
    title: String,
    folder: Option<Id>,
    folder_name: Option<String>,
    source: AddSource,
    sql: String,
    selection: Option<Range<usize>>,
    link: Option<(PathBuf, String)>,
    error: Option<String>,
}

impl Add {
    fn new(tab: &QueryTab) -> Self {
        let selection = tab.selection.clone().filter(|range| {
            tab.sql
                .get(range.clone())
                .is_some_and(|sql| !sql.trim().is_empty())
        });
        let source = if selection.is_some() {
            AddSource::Selection
        } else if tab.sql.trim().is_empty() {
            AddSource::Link
        } else {
            AddSource::Editor
        };
        Self {
            title: String::new(),
            folder: None,
            folder_name: None,
            source,
            sql: tab.sql.clone(),
            selection,
            link: None,
            error: None,
        }
    }

    fn sql(&self) -> Option<&str> {
        match self.source {
            AddSource::Selection => self.selection.clone().and_then(|range| self.sql.get(range)),
            AddSource::Editor => Some(&self.sql),
            AddSource::Link => self.link.as_ref().map(|(_, sql)| sql.as_str()),
        }
    }

    fn entry(&self) -> Result<EntryKind, String> {
        if self.source == AddSource::Link {
            return self
                .link
                .as_ref()
                .map(|(path, _)| EntryKind::Link { path: path.clone() })
                .ok_or_else(|| "请选择要链接的 SQL 文件。".into());
        }
        let sql = self
            .sql()
            .filter(|sql| !sql.trim().is_empty())
            .ok_or("当前没有可收藏的 SQL。")?;
        if sql.len() as u64 > MAX_SCRIPT {
            return Err("收藏的 SQL 超过 8 MiB。".into());
        }
        Ok(EntryKind::Sql { sql: sql.into() })
    }
}

#[derive(Default)]
struct Manage {
    selected: Option<Id>,
    expanded: BTreeSet<Id>,
    operation: Option<Operation>,
    preview: Option<(Id, Result<String, String>)>,
    error: Option<String>,
}

enum Operation {
    Create { parent: Option<Id>, name: String },
    Rename { item: Id, name: String },
    Move { item: Id, destination: Option<Id> },
    Delete { item: Id },
}

enum OperationResult {
    Stay,
    Cancel,
    Saved(Option<Id>),
}

impl Favorites {
    fn add_form(&mut self, ui: &mut egui::Ui, form: &mut Add, height: f32) -> bool {
        ui.set_width((ui.ctx().content_rect().width() - 48.0).clamp(400.0, 580.0));
        ui.horizontal(|ui| {
            ui.heading("添加到收藏夹");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("刷新收藏夹").clicked() {
                    self.load();
                    form.error = None;
                }
            });
        });
        ui.add_space(6.0);
        if let Some(error) = &self.read_error {
            style::banner(ui, error, style::error(), style::error_soft());
            ui.add_space(6.0);
        }
        style::section_card(ui, "收藏配置", |ui| {
            ui.horizontal_wrapped(|ui| {
                if form.selection.is_some() {
                    ui.radio_value(&mut form.source, AddSource::Selection, "选中的 SQL");
                }
                ui.add_enabled_ui(!form.sql.trim().is_empty(), |ui| {
                    ui.radio_value(&mut form.source, AddSource::Editor, "当前编辑器");
                });
                ui.radio_value(&mut form.source, AddSource::Link, "链接 SQL 文件");
            });

            if form.source == AddSource::Link {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("选择文件…").clicked()
                        && let Some(result) = pick_link()
                    {
                        match result {
                            Ok((path, sql)) => {
                                if form.title.trim().is_empty()
                                    && let Some(stem) = path.file_stem()
                                {
                                    form.title = stem.to_string_lossy().into_owned();
                                }
                                form.link = Some((path, sql));
                                form.error = None;
                            }
                            Err(error) => form.error = Some(error),
                        }
                    }
                    if let Some((path, _)) = &form.link {
                        ui.add(egui::Label::new(path.display().to_string()).truncate())
                            .on_hover_text(path.display().to_string());
                    }
                });
            }

            ui.add_space(6.0);
            egui::Grid::new("favorite-add-grid")
                .num_columns(2)
                .spacing([14.0, 8.0])
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("名称").color(style::muted()));
                    ui.add(
                        TextEdit::singleline(&mut form.title)
                            .desired_width((ui.available_width() - 8.0).max(200.0)),
                    );
                    ui.end_row();
                    if let Some(library) = &self.library {
                        ui.label(egui::RichText::new("位置").color(style::muted()));
                        ui.horizontal(|ui| {
                            folder_picker(
                                ui,
                                "favorite-add-location",
                                library,
                                &mut form.folder,
                                None,
                            );
                            if ui
                                .add_enabled(
                                    self.read_error.is_none(),
                                    egui::Button::new("新建文件夹…"),
                                )
                                .clicked()
                            {
                                form.folder_name = Some(String::new());
                                form.error = None;
                            }
                        });
                        ui.end_row();
                    }
                });
            if let Some(name) = &mut form.folder_name {
                ui.add_space(4.0);
                let mut create = false;
                let mut cancel = false;
                ui.horizontal(|ui| {
                    ui.add(TextEdit::singleline(name).hint_text("文件夹名"));
                    create = ui.button("创建").clicked();
                    cancel = ui.button("取消新建").clicked();
                });
                if create {
                    match self.change(|library| library.add(form.folder, name, EntryKind::Folder))
                    {
                        Ok(id) => {
                            form.folder = Some(id);
                            form.folder_name = None;
                            form.error = None;
                        }
                        Err(error) => form.error = Some(error),
                    }
                } else if cancel {
                    form.folder_name = None;
                    form.error = None;
                }
            }
        });

        if let Some(sql) = form.sql() {
            ui.add_space(8.0);
            sql_preview(ui, "favorite-add-preview", sql, height);
        }
        if let Some(error) = &form.error {
            ui.add_space(6.0);
            style::banner(ui, error, style::error(), style::error_soft());
        }

        let mut save = false;
        let mut close = false;
        let valid =
            self.read_error.is_none() && self.library.is_some() && form.folder_name.is_none();
        style::dialog_footer(ui, |ui| {
            save = ui
                .add_enabled(valid, style::primary_enabled(valid, "保存收藏"))
                .clicked();
            close = ui.button("取消").clicked();
        });
        if save {
            let result = form.entry().and_then(|kind| {
                self.change(|library| library.add(form.folder, &form.title, kind))
            });
            return match result {
                Ok(_) => true,
                Err(error) => {
                    form.error = Some(error);
                    false
                }
            };
        }
        close
    }

    fn manage_form(
        &mut self,
        ui: &mut egui::Ui,
        manager: &mut Manage,
        height: f32,
    ) -> (bool, Option<EditorAction>) {
        ui.set_width(720.0);
        ui.heading("管理收藏夹");
        let mut close = false;
        ui.horizontal(|ui| {
            if ui.button("刷新收藏夹").clicked() {
                self.load();
                manager.preview = None;
                manager.error = None;
            }
            close = ui.button("关闭").clicked();
        });
        if let Some(error) = &self.read_error {
            ui.colored_label(style::error(), error);
        }
        if let Some(error) = &manager.error {
            ui.colored_label(style::error(), error);
        }
        let Some(library) = &self.library else {
            return (close, None);
        };
        if manager
            .selected
            .is_some_and(|id| !library.entries.contains_key(&id))
        {
            manager.selected = None;
            manager.preview = None;
            manager.operation = None;
        }
        if let Some(mut operation) = manager.operation.take() {
            match self.operation_form(ui, &mut operation, manager) {
                OperationResult::Stay => manager.operation = Some(operation),
                OperationResult::Cancel => manager.error = None,
                OperationResult::Saved(id) => {
                    manager.selected = id;
                    manager.preview = None;
                    manager.error = None;
                    if let Some(library) = &self.library {
                        let mut parent = id.and_then(|id| library.entries[&id].parent);
                        while let Some(folder) = parent {
                            manager.expanded.insert(folder);
                            parent = library.entries[&folder].parent;
                        }
                    }
                }
            }
            return (close, None);
        }

        let mut open = None;
        let mut relink = None;
        ui.columns(2, |columns| {
            let chosen = ScrollArea::vertical()
                .id_salt("favorite-tree")
                .max_height((height * 0.40).clamp(125.0, 360.0))
                .show(&mut columns[0], |ui| {
                    if ui
                        .selectable_label(manager.selected.is_none(), "收藏夹")
                        .clicked()
                    {
                        return Some(None);
                    }
                    tree(ui, library, None, manager, self.read_error.is_none()).map(Some)
                })
                .inner;
            if let Some(selected) = chosen {
                manager.selected = selected;
                manager.preview = None;
                manager.error = None;
            }

            let Some(id) = manager.selected else {
                return;
            };
            let entry = &library.entries[&id];
            let ui = &mut columns[1];
            if entry.folder() {
                ui.label(library.folder_label(Some(id)));
                return;
            }
            match &entry.kind {
                EntryKind::Sql { sql } => {
                    sql_preview(ui, "favorite-sql-preview", sql, height);
                }
                EntryKind::Link { path } => {
                    ui.add(egui::Label::new(path.display().to_string()).truncate())
                        .on_hover_text(path.display().to_string());
                    if manager
                        .preview
                        .as_ref()
                        .is_none_or(|(showing, _)| *showing != id)
                    {
                        manager.preview = Some((id, read_link(path)));
                    }
                    if let Some((_, preview)) = &manager.preview {
                        match preview {
                            Ok(sql) => sql_preview(ui, "favorite-link-preview", sql, height),
                            Err(error) => {
                                ui.colored_label(style::error(), error);
                            }
                        }
                    }
                    if ui
                        .add_enabled(
                            self.read_error.is_none(),
                            egui::Button::new("重新选择文件…"),
                        )
                        .clicked()
                    {
                        relink = Some(id);
                    }
                }
                EntryKind::Folder => {}
            }
            ui.horizontal_wrapped(|ui| {
                if ui.button("插入到编辑器").clicked() {
                    open = Some((id, true));
                }
                if ui.button("在新页签打开").clicked() {
                    open = Some((id, false));
                }
            });
        });
        let selected = manager
            .selected
            .and_then(|id| library.entries.get(&id).map(|entry| (id, entry)));
        ui.separator();
        ui.add_enabled_ui(self.read_error.is_none(), |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button("新建文件夹…").clicked() {
                    let parent = selected.and_then(|(id, entry)| {
                        if entry.folder() {
                            Some(id)
                        } else {
                            entry.parent
                        }
                    });
                    manager.operation = Some(Operation::Create {
                        parent,
                        name: String::new(),
                    });
                    manager.error = None;
                }
                if let Some((id, entry)) = selected
                    && let Some(operation) = entry_actions(ui, id, entry)
                {
                    manager.operation = Some(operation);
                    manager.error = None;
                }
            });
        });

        if let Some((id, insert)) = open {
            match library.entries[&id].read() {
                Ok((sql, path)) => {
                    return (
                        false,
                        Some(if insert {
                            EditorAction::Insert(sql)
                        } else {
                            EditorAction::Open { sql, path }
                        }),
                    );
                }
                Err(error) => manager.error = Some(error),
            }
        }
        if let Some(id) = relink
            && let Some(result) = pick_link()
        {
            match result.and_then(|(path, sql)| {
                self.change(|library| library.replace_link(id, path))?;
                Ok(sql)
            }) {
                Ok(sql) => {
                    manager.preview = Some((id, Ok(sql)));
                    manager.error = None;
                }
                Err(error) => manager.error = Some(error),
            }
        }
        (close, None)
    }

    fn operation_form(
        &mut self,
        ui: &mut egui::Ui,
        operation: &mut Operation,
        manager: &mut Manage,
    ) -> OperationResult {
        let library = self.library.as_ref().expect("loaded collection");
        let action = match operation {
            Operation::Create { parent, name } => {
                ui.label("新建文件夹");
                folder_picker(ui, "favorite-new-folder-location", library, parent, None);
                ui.add(TextEdit::singleline(name).hint_text("文件夹名"));
                "创建"
            }
            Operation::Rename { name, .. } => {
                ui.label("重命名");
                ui.add(TextEdit::singleline(name).hint_text("新名称"));
                "保存名称"
            }
            Operation::Move { item, destination } => {
                ui.label(format!("移动「{}」到", library.entries[item].name));
                folder_picker(
                    ui,
                    "favorite-move-location",
                    library,
                    destination,
                    Some(*item),
                );
                "移动"
            }
            Operation::Delete { item } => {
                let entry = &library.entries[item];
                if entry.folder() {
                    match library.descendants(*item).len().saturating_sub(1) {
                        0 => ui.label(format!("删除文件夹「{}」？", entry.name)),
                        descendants => ui.label(format!(
                            "删除文件夹「{}」以及其中的 {descendants} 项收藏？",
                            entry.name
                        )),
                    };
                } else if matches!(entry.kind, EntryKind::Link { .. }) {
                    ui.label(format!("移除文件链接「{}」？", entry.name));
                } else {
                    ui.label(format!("删除「{}」的收藏副本？", entry.name));
                }
                "确认删除"
            }
        };
        let mut confirm = false;
        let mut cancel = false;
        ui.horizontal(|ui| {
            cancel = ui.button("取消").clicked();
            confirm = ui
                .add_enabled(self.read_error.is_none(), style::primary(action))
                .clicked();
        });
        if cancel {
            return OperationResult::Cancel;
        }
        if !confirm {
            return OperationResult::Stay;
        }
        let result = self.change(|library| match operation {
            Operation::Create { parent, name } => {
                library.add(*parent, name, EntryKind::Folder).map(Some)
            }
            Operation::Rename { item, name } => {
                library.rename(*item, name)?;
                Ok(Some(*item))
            }
            Operation::Move { item, destination } => {
                library.move_to(*item, *destination)?;
                Ok(Some(*item))
            }
            Operation::Delete { item } => {
                let parent = library.entries.get(item).and_then(|entry| entry.parent);
                library.delete(*item)?;
                Ok(parent)
            }
        });
        match result {
            Ok(id) => OperationResult::Saved(id),
            Err(error) => {
                manager.error = Some(error);
                OperationResult::Stay
            }
        }
    }
}

fn folder_picker(
    ui: &mut egui::Ui,
    id: &str,
    library: &Library,
    folder: &mut Option<Id>,
    excluded: Option<Id>,
) {
    let label = if folder.is_some_and(|id| !library.entries.contains_key(&id)) {
        "请选择有效的文件夹".into()
    } else {
        library.folder_label(*folder)
    };
    egui::ComboBox::from_id_salt(id)
        .selected_text(label)
        .show_ui(ui, |ui| {
            ui.selectable_value(folder, None, "收藏夹");
            let mut folders = library
                .entries
                .iter()
                .filter(|&(&id, entry)| {
                    entry.folder() && !excluded.is_some_and(|ancestor| library.within(id, ancestor))
                })
                .map(|(&id, _)| (id, library.folder_label(Some(id))))
                .collect::<Vec<_>>();
            folders.sort_by_key(|(_, name)| name.to_lowercase());
            for (id, name) in folders {
                ui.selectable_value(folder, Some(id), name);
            }
        });
}

fn tree(
    ui: &mut egui::Ui,
    library: &Library,
    parent: Option<Id>,
    manager: &mut Manage,
    editable: bool,
) -> Option<Id> {
    let mut picked = None;
    for (id, entry) in library.children(parent) {
        ui.push_id(id, |ui| {
            if entry.folder() {
                ui.horizontal(|ui| {
                    let expanded = manager.expanded.contains(&id);
                    let button = if expanded { "▾" } else { "▸" };
                    if ui
                        .small_button(button)
                        .on_hover_text(if expanded {
                            "收起文件夹"
                        } else {
                            "展开文件夹"
                        })
                        .clicked()
                    {
                        if expanded {
                            manager.expanded.remove(&id);
                        } else {
                            manager.expanded.insert(id);
                        }
                    }
                    let row = ui.selectable_label(manager.selected == Some(id), &entry.name);
                    if row_selection(&row, id, entry, manager, editable) {
                        picked = Some(id);
                    }
                    if row.double_clicked() && !manager.expanded.remove(&id) {
                        manager.expanded.insert(id);
                    }
                });
                if manager.expanded.contains(&id) {
                    let child = ui
                        .indent("children", |ui| {
                            tree(ui, library, Some(id), manager, editable)
                        })
                        .inner;
                    picked = child.or(picked);
                }
            } else {
                let row = ui.selectable_label(manager.selected == Some(id), &entry.name);
                if row_selection(&row, id, entry, manager, editable) {
                    picked = Some(id);
                }
                if matches!(entry.kind, EntryKind::Link { .. }) {
                    row.on_hover_text("SQL 文件链接");
                }
            }
        });
    }
    picked
}

fn row_selection(
    row: &egui::Response,
    id: Id,
    entry: &Entry,
    manager: &mut Manage,
    editable: bool,
) -> bool {
    let mut action = None;
    if editable {
        row.context_menu(|ui| {
            if let Some(operation) = entry_actions(ui, id, entry) {
                action = Some(operation);
                ui.close();
            }
        });
    }
    if let Some(operation) = action {
        manager.operation = Some(operation);
        return true;
    }
    row.clicked()
}

fn entry_actions(ui: &mut egui::Ui, id: Id, entry: &Entry) -> Option<Operation> {
    if ui.button("重命名…").clicked() {
        Some(Operation::Rename {
            item: id,
            name: entry.name.clone(),
        })
    } else if ui.button("移动到…").clicked() {
        Some(Operation::Move {
            item: id,
            destination: entry.parent,
        })
    } else if ui.button("删除…").clicked() {
        Some(Operation::Delete { item: id })
    } else {
        None
    }
}

fn pick_link() -> Option<Result<(PathBuf, String), String>> {
    let path = rfd::FileDialog::new()
        .set_title("链接 SQL 文件")
        .add_filter("SQL 文件", &["sql", "txt"])
        .pick_file()?;
    Some(
        std::path::absolute(&path)
            .map_err(|error| error.to_string())
            .and_then(|path| read_link(&path).map(|sql| (path, sql))),
    )
}

fn sql_preview(ui: &mut egui::Ui, id: &str, sql: &str, height: f32) {
    let end = sql.floor_char_boundary(64 * 1024);
    let mut visible = &sql[..end];
    ScrollArea::both()
        .id_salt(id)
        .max_height((height * 0.30).clamp(110.0, 260.0))
        .show(ui, |ui| {
            ui.add(
                TextEdit::multiline(&mut visible)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY),
            );
        });
    if end < sql.len() {
        ui.label("显示前 64 KiB");
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::Workspace;
    use super::*;

    fn storage_fixture() -> tempfile::TempDir {
        let binary = std::env::current_exe().unwrap();
        tempfile::Builder::new()
            .prefix("harbor-favorites-")
            .tempdir_in(binary.parent().unwrap())
            .unwrap()
    }

    #[test]
    fn saves_selection_by_default_and_preserves_sql_bytes() {
        let mut workspace = Workspace::new("收藏测试".into(), None);
        let tab = workspace.tab_mut();
        tab.sql = "SELECT '甲';\r\nSELECT '乙';\r\n".into();
        let split = tab.sql.find('\n').unwrap() + 1;
        tab.selection = Some(split..tab.sql.len());

        let mut draft = Add::new(tab);
        assert!(matches!(draft.entry(), Ok(EntryKind::Sql { sql }) if sql == "SELECT '乙';\r\n"));
        draft.source = AddSource::Editor;
        assert!(matches!(draft.entry(), Ok(EntryKind::Sql { sql }) if sql == tab.sql));
    }

    #[test]
    fn persists_and_organizes_nested_sql_and_links() {
        let directory = storage_fixture();
        let library_path = directory.path().join("favorites.json");
        let sql_path = directory.path().join("库存 查询.sql");
        fs::write(&sql_path, "\u{feff}select '库存';\r\n").unwrap();

        let (library, (reports, daily, saved, linked)) =
            update_library(&library_path, &Library::default(), |library| {
                let reports = library.add(None, "报表", EntryKind::Folder)?;
                let daily = library.add(Some(reports), "日常", EntryKind::Folder)?;
                let saved = library.add(
                    Some(daily),
                    "查询",
                    EntryKind::Sql {
                        sql: "SELECT '甲';".into(),
                    },
                )?;
                let linked = library.add(
                    Some(daily),
                    "库存",
                    EntryKind::Link {
                        path: sql_path.clone(),
                    },
                )?;
                Ok((reports, daily, saved, linked))
            })
            .unwrap();
        let reopened = Library::load(&library_path).unwrap();
        assert!(reopened == library);
        assert_eq!(reopened.folder_label(Some(daily)), "报表 › 日常");
        assert_eq!(
            reopened.entries[&linked].read().unwrap().0,
            "select '库存';\r\n"
        );
        fs::write(&sql_path, "select '新库存';").unwrap();
        assert_eq!(
            reopened.entries[&linked].read().unwrap(),
            ("select '新库存';".into(), Some(sql_path.clone()))
        );

        let (mut moved, ()) = update_library(&library_path, &reopened, |library| {
            library.move_to(saved, Some(reports))?;
            library.rename(saved, "日报")?;
            library.delete(daily)
        })
        .unwrap();
        assert!(moved.move_to(reports, Some(reports)).is_err());
        let reopened = Library::load(&library_path).unwrap();
        assert_eq!(reopened.entries[&saved].name, "日报");
        assert!(!reopened.entries.contains_key(&linked));
        assert_eq!(fs::read_to_string(sql_path).unwrap(), "select '新库存';");
    }

    #[test]
    fn rejects_stale_edits_and_preserves_unreadable_manifests() {
        let directory = storage_fixture();
        let path = directory.path().join("favorites.json");
        let original = Library::load(&path).unwrap();
        let (updated, _) = update_library(&path, &original, |library| {
            library.add(None, "已保存", EntryKind::Folder)
        })
        .unwrap();
        assert!(
            update_library(&path, &original, |library| library.add(
                None,
                "过期",
                EntryKind::Folder
            ))
            .is_err()
        );
        assert!(Library::load(&path).unwrap() == updated);

        let corrupt = b"{";
        fs::write(&path, corrupt).unwrap();
        assert!(update_library(&path, &updated, |_| Ok(())).is_err());
        assert_eq!(fs::read(path).unwrap(), corrupt);
    }
}
