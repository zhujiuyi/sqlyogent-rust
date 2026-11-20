use eframe::egui::{self, TextEdit};
use harbor_sql::file_source::FileSource;
use harbor_sql::jobs::odbc_editor::Source;
use harbor_sql::odbc::DiscoveredTable;

pub(super) struct Catalog {
    pub(super) login: Source,
    entries: Vec<Entry>,
    filter: String,
    copy_indexes: bool,
    copy_foreign_keys: bool,
}

struct Entry {
    table: DiscoveredTable,
    label: String,
    selected: bool,
    target: String,
}

pub(super) enum Action {
    Back,
    Add,
}

impl Catalog {
    pub(super) fn new(login: Source, tables: Vec<DiscoveredTable>) -> Self {
        Self {
            login,
            entries: tables
                .into_iter()
                .map(|table| {
                    let name = &table.source;
                    let label = [
                        name.catalog.as_deref(),
                        name.schema.as_deref(),
                        Some(name.table.as_str()),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(".");
                    Entry {
                        selected: false,
                        target: name.table.clone(),
                        label,
                        table,
                    }
                })
                .collect(),
            filter: String::new(),
            copy_indexes: false,
            copy_foreign_keys: false,
        }
    }

    pub(super) fn show(&mut self, ui: &mut egui::Ui, creates_tables: bool) -> Option<Action> {
        ui.strong("选择要导入的数据表");
        ui.add(TextEdit::singleline(&mut self.filter).hint_text(
            if self.login.file_source.is_some() {
                "按数据表名称筛选"
            } else {
                "按表名、目录或架构筛选"
            },
        ));
        let filter = self.filter.trim().to_lowercase();
        let visible: Vec<_> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.label.to_lowercase().contains(&filter))
            .map(|(index, _)| index)
            .collect();
        ui.horizontal_wrapped(|ui| {
            if ui.button("全选当前列表").clicked() {
                for &index in &visible {
                    self.entries[index].selected = true;
                }
            }
            if ui.button("取消当前列表的勾选").clicked() {
                for &index in &visible {
                    self.entries[index].selected = false;
                }
            }
        });
        if visible.is_empty() {
            ui.label("暂无符合条件的数据表。");
        }
        egui::ScrollArea::vertical()
            .id_salt("odbc-discovered-tables")
            .max_height(240.0)
            .show_rows(ui, 23.0, visible.len(), |ui, rows| {
                for row in rows {
                    let index = visible[row];
                    let entry = &mut self.entries[index];
                    ui.push_id(index, |ui| {
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut entry.selected, "");
                            ui.add_sized([250.0, 19.0], egui::Label::new(&entry.label).truncate())
                                .on_hover_text(&entry.label);
                            ui.add_enabled(
                                entry.selected,
                                TextEdit::singleline(&mut entry.target)
                                    .hint_text("目标表名")
                                    .desired_width(170.0),
                            );
                        })
                    });
                }
            });
        if creates_tables && self.supports_metadata() {
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut self.copy_foreign_keys, "复制所选表外键");
                ui.add_enabled(
                    !self.copy_foreign_keys,
                    egui::Checkbox::new(&mut self.copy_indexes, "复制所选表索引"),
                );
                if self.copy_foreign_keys {
                    self.copy_indexes = true;
                }
            });
            if self.copy_foreign_keys && self.login.file_source.is_some() {
                ui.label("请将跨表外键涉及的所有 Access 表加入同一个作业。");
            }
        }
        let count = self.entries.iter().filter(|entry| entry.selected).count();
        ui.horizontal_wrapped(|ui| {
            if ui.button("返回").clicked() {
                return Some(Action::Back);
            }
            if ui
                .add_enabled(count > 0, egui::Button::new(format!("加入 {count} 张表")))
                .clicked()
            {
                return Some(Action::Add);
            }
            None
        })
        .inner
    }

    pub(super) fn copy_metadata(&self, creates_tables: bool) -> (bool, bool) {
        let enabled = creates_tables && self.supports_metadata();
        (
            enabled && self.copy_indexes,
            enabled && self.copy_foreign_keys,
        )
    }

    fn supports_metadata(&self) -> bool {
        self.login
            .file_source
            .as_ref()
            .is_none_or(FileSource::supports_metadata)
    }

    pub(super) fn selection(&self) -> impl Iterator<Item = (&DiscoveredTable, &str)> {
        self.entries
            .iter()
            .filter(|entry| entry.selected)
            .map(|entry| (&entry.table, entry.target.trim()))
    }
}
