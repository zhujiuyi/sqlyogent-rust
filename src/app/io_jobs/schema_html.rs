use std::path::PathBuf;

use eframe::egui;
use rfd::FileDialog;

use crate::database::{Command, DbObject, ObjectKind};

use super::{
    Workspace, file_stem, form_scroll, form_section, form_width, right_aligned_row, style,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Tables,
    Database,
}

enum Catalog {
    Waiting,
    Loading,
    Ready(Vec<Table>),
    Failed(String),
}

struct Table {
    name: String,
    checked: bool,
}

impl Table {
    fn choices(objects: &[DbObject]) -> Vec<Self> {
        objects
            .iter()
            .filter(|object| object.kind == ObjectKind::Table)
            .map(|object| Self {
                name: object.name.clone(),
                checked: true,
            })
            .collect()
    }
}

pub(super) struct Form {
    schema: String,
    scope: Scope,
    catalog: Catalog,
    error: Option<String>,
}

impl Form {
    pub(super) fn new(schema: String) -> Self {
        Self {
            schema,
            scope: Scope::Tables,
            catalog: Catalog::Waiting,
            error: None,
        }
    }

    fn poll_tables(&mut self, workspace: &mut Workspace) {
        if workspace.busy() || workspace.server.is_none() {
            return;
        }
        match self.catalog {
            Catalog::Waiting | Catalog::Loading => {
                if let Some(objects) = workspace.objects.get(&self.schema) {
                    self.catalog = Catalog::Ready(Table::choices(objects));
                } else if matches!(self.catalog, Catalog::Loading) {
                    self.catalog = Catalog::Failed(workspace.status.clone());
                } else if workspace.send(
                    Command::ListObjects {
                        schema: self.schema.clone(),
                    },
                    "正在读取数据表…",
                ) {
                    self.catalog = Catalog::Loading;
                } else {
                    self.catalog = Catalog::Failed(workspace.status.clone());
                }
            }
            Catalog::Ready(_) | Catalog::Failed(_) => {}
        }
    }

    fn export_command(&self, path: PathBuf) -> Option<Command> {
        let schema = self.schema.clone();
        if self.scope == Scope::Database {
            return Some(Command::SchemaHtml { schema, path });
        }
        let Catalog::Ready(choices) = &self.catalog else {
            return None;
        };
        let tables: Vec<_> = choices
            .iter()
            .filter(|table| table.checked)
            .map(|table| table.name.clone())
            .collect();
        (!tables.is_empty()).then_some(Command::SchemaHtmlTables {
            schema,
            tables,
            path,
        })
    }

    pub(super) fn dialog(&mut self, ctx: &egui::Context, workspace: &mut Workspace) -> bool {
        let mut save = false;
        let mut cancel = false;
        let response = style::modal(ctx, egui::Id::new("schema-html")).show(ctx, |ui| {
            ui.set_width(form_width(ctx, 500.0));
            ui.heading("导出数据库结构说明（HTML）");
            ui.add(egui::Label::new(egui::RichText::new(&self.schema).color(style::MUTED)).truncate())
                .on_hover_text(&self.schema);
            let connected = workspace.server.is_some();
            form_scroll(ctx, 145.0).show(ui, |ui| {
                form_section(ui, "导出范围", |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.radio_value(&mut self.scope, Scope::Tables, "选择数据表");
                        ui.radio_value(&mut self.scope, Scope::Database, "全部数据库对象")
                            .on_hover_text("包含数据表、视图和其他数据库对象。");
                    });
                    if !connected {
                        ui.colored_label(style::ERROR, "连接已断开。");
                    } else if self.scope == Scope::Tables {
                        self.poll_tables(workspace);
                        let mut retry = false;
                        match &mut self.catalog {
                            Catalog::Waiting | Catalog::Loading => {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label("正在读取数据表…");
                                });
                            }
                            Catalog::Ready(tables) if tables.is_empty() => {
                                ui.colored_label(style::MUTED, "此数据库没有可选的数据表。");
                            }
                            Catalog::Ready(tables) => {
                                let height = (ctx.content_rect().height() * 0.38).clamp(110.0, 300.0);
                                table_list(ui, tables, height);
                            }
                            Catalog::Failed(error) => {
                                ui.colored_label(style::ERROR, error.as_str());
                                retry = ui.button("重新读取数据表").clicked();
                            }
                        }
                        if retry {
                            self.catalog = Catalog::Waiting;
                            self.poll_tables(workspace);
                        }
                    }
                });
            });
            if let Some(error) = &self.error {
                ui.add(egui::Label::new(egui::RichText::new(error).color(style::ERROR)).truncate())
                    .on_hover_text(error);
            }
            ui.separator();
            let has_scope = self.scope == Scope::Database
                || matches!(&self.catalog, Catalog::Ready(tables) if tables.iter().any(|table| table.checked));
            right_aligned_row(ui, |ui| {
                save = ui.add_enabled(connected && !workspace.busy() && has_scope, style::primary("选择保存位置…")).clicked();
                cancel = ui.button("取消").clicked();
            });
        });
        if save {
            let path = FileDialog::new()
                .set_title("导出数据库结构说明")
                .add_filter("HTML 网页", &["html"])
                .set_file_name(format!("结构_{}.html", file_stem(&self.schema)))
                .save_file();
            if let Some(command) = path.and_then(|path| self.export_command(path)) {
                if workspace.send(command, "正在导出数据库结构说明…") {
                    return true;
                }
                self.error = Some(workspace.status.clone());
            }
            return false;
        }
        cancel || response.should_close()
    }
}

fn table_list(ui: &mut egui::Ui, tables: &mut [Table], max_height: f32) {
    ui.horizontal_wrapped(|ui| {
        if ui.button("全选").clicked() {
            for table in tables.iter_mut() {
                table.checked = true;
            }
        }
        if ui.button("清除选择").clicked() {
            for table in tables.iter_mut() {
                table.checked = false;
            }
        }
        let selected = tables.iter().filter(|table| table.checked).count();
        ui.colored_label(
            style::MUTED,
            format!("已选 {selected} / {} 张表", tables.len()),
        );
    });
    egui::ScrollArea::vertical()
        .id_salt("schema-html-tables")
        .max_height(max_height)
        .show_rows(
            ui,
            ui.spacing().interact_size.y,
            tables.len(),
            |ui, range| {
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                for table in &mut tables[range] {
                    ui.checkbox(&mut table.checked, &table.name)
                        .on_hover_text(&table.name);
                }
            },
        );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_html_uses_checked_tables_and_keeps_the_full_report_available() {
        let mut form = Form::new("stock".into());
        let object = |name: &str, kind| DbObject {
            schema: form.schema.clone(),
            name: name.into(),
            kind,
        };
        let mut tables = Table::choices(&[
            object("inventory", ObjectKind::Table),
            object("warehouse", ObjectKind::Table),
            object("inventory_view", ObjectKind::View),
        ]);
        assert_eq!(tables.len(), 2);
        assert!(tables.iter().all(|table| table.checked));
        tables[1].checked = false;
        form.catalog = Catalog::Ready(tables);
        let file = PathBuf::from("structure.html");
        assert!(matches!(
            form.export_command(file.clone()),
            Some(Command::SchemaHtmlTables { schema, tables, path })
                if schema == "stock" && tables == ["inventory"] && path == file
        ));
        form.scope = Scope::Database;
        assert!(matches!(
            form.export_command(file.clone()),
            Some(Command::SchemaHtml { schema, path }) if schema == "stock" && path == file
        ));
    }
}
