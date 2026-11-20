use std::path::PathBuf;

use eframe::egui::{self, RichText};
use rfd::FileDialog;

use crate::database::{
    ColumnInfo, Command, DbObject, ObjectKind,
    transfer::{
        CsvOptions, EXCEL_CELL_TEXT_LIMIT, ExcelOptions, ExportKind, ExportSettings,
        ResultSqlTarget, SqlExportMode, sql_export_writable,
    },
};

use super::{file_stem, form_scroll, form_section, form_width, right_aligned_row};
use crate::app::{state::Workspace, style};

const TABLE_FORMATS: [ExportKind; 5] = [
    ExportKind::Csv,
    ExportKind::ExcelXml,
    ExportKind::Html,
    ExportKind::Xml,
    ExportKind::Sql,
];

pub(in crate::app) const RESULT_FORMATS: [ExportKind; 6] = [
    ExportKind::Csv,
    ExportKind::Json,
    ExportKind::Html,
    ExportKind::Xml,
    ExportKind::ExcelXml,
    ExportKind::Sql,
];

pub(in crate::app) struct Options {
    pub kind: ExportKind,
    pub selected: Vec<bool>,
    pub path: Option<PathBuf>,
    csv: CsvOptions,
    excel: ExcelOptions,
    sql: SqlExportMode,
    result_target: Option<ResultSqlTarget>,
}

impl Options {
    pub fn new(kind: ExportKind, fields: usize) -> Self {
        Self {
            kind,
            selected: vec![true; fields],
            path: None,
            csv: CsvOptions::default(),
            excel: ExcelOptions::default(),
            sql: SqlExportMode::default(),
            result_target: None,
        }
    }

    pub fn format(&mut self, ui: &mut egui::Ui, kinds: &[ExportKind]) {
        let previous = self.kind;
        ui.horizontal_wrapped(|ui| {
            ui.label("文件格式");
            egui::ComboBox::from_id_salt("export-format")
                .selected_text(self.kind.label())
                .show_ui(ui, |ui| {
                    for kind in kinds {
                        ui.selectable_value(&mut self.kind, *kind, kind.label());
                    }
                });
        });
        if self.kind != previous {
            self.path = None;
        }
        if self.kind == ExportKind::ExcelXml {
            ui.colored_label(style::muted(), "最多 65,535 行数据，256 列。");
        }
    }

    pub fn field_picker<'a>(
        &mut self,
        ui: &mut egui::Ui,
        mut fields: impl Iterator<Item = (usize, &'a str)> + Clone,
    ) -> bool {
        let total = fields.clone().count();
        let checked = fields
            .clone()
            .filter(|(index, _)| self.selected[*index])
            .count();
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(style::muted(), format!("已选 {checked} / {total} 个字段"));
            if ui.button("全选").clicked() {
                for (index, _) in fields.clone() {
                    self.selected[index] = true;
                }
            }
            if ui.button("清空").clicked() {
                for (index, _) in fields.clone() {
                    self.selected[index] = false;
                }
            }
        });
        egui::ScrollArea::vertical()
            .id_salt("export-fields")
            .auto_shrink([false, true])
            .max_height(156.0)
            .show(ui, |ui| {
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                for (index, name) in fields.clone() {
                    ui.push_id(index, |ui| {
                        ui.checkbox(&mut self.selected[index], name)
                            .on_hover_text(name);
                    });
                }
            });
        fields.any(|(index, _)| self.selected[index])
    }

    pub fn file_settings(
        &mut self,
        ui: &mut egui::Ui,
        stem: &str,
        allow_declared_widths: bool,
    ) -> bool {
        let valid = match self.kind {
            ExportKind::Csv => csv_options(ui, &mut self.csv, allow_declared_widths),
            ExportKind::ExcelXml => {
                excel_options(ui, &mut self.excel);
                true
            }
            ExportKind::Sql => {
                sql_options(ui, &mut self.sql);
                true
            }
            _ => true,
        };
        right_aligned_row(ui, |ui| {
            if ui.button("选择文件…").clicked() {
                let ext = self.kind.extension();
                if let Some(path) = FileDialog::new()
                    .set_title("保存导出的数据")
                    .add_filter(self.kind.label(), &[ext])
                    .set_file_name(format!("{stem}.{ext}"))
                    .save_file()
                {
                    self.path = Some(path);
                }
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                if let Some(file) = &self.path {
                    let label = file.display().to_string();
                    ui.add(egui::Label::new(&label).truncate().selectable(true))
                        .on_hover_text(label);
                } else {
                    ui.colored_label(style::muted(), "未选择输出文件");
                }
            });
        });
        valid && self.path.is_some()
    }

    pub fn settings(&self, fields: Option<Vec<String>>) -> ExportSettings {
        ExportSettings {
            kind: self.kind,
            fields,
            csv: self.csv.clone(),
            excel: self.excel,
            sql: self.sql,
            result_target: self.result_target.clone(),
        }
    }

    pub fn result_target(&mut self, ui: &mut egui::Ui) -> bool {
        if self.kind != ExportKind::Sql {
            return true;
        }
        let target = self
            .result_target
            .get_or_insert_with(ResultSqlTarget::default);
        egui::Grid::new("result-sql-target").show(ui, |ui| {
            ui.label("目标库名");
            ui.add(
                egui::TextEdit::singleline(&mut target.schema)
                    .hint_text("留空时使用执行脚本的数据库"),
            );
            ui.end_row();
            ui.label("目标表名");
            ui.add(egui::TextEdit::singleline(&mut target.table));
            ui.end_row();
        });
        if target.table.is_empty() {
            return false;
        }
        if let Err(error) = target.validate() {
            ui.colored_label(style::error(), error);
            return false;
        }
        true
    }

    pub fn structure_only(&self) -> bool {
        self.kind == ExportKind::Sql && self.sql == SqlExportMode::Structure
    }
}

fn csv_options(ui: &mut egui::Ui, options: &mut CsvOptions, allow_declared_widths: bool) -> bool {
    ui.horizontal_wrapped(|ui| {
        ui.label("字段宽度");
        ui.radio_value(&mut options.fixed_width, false, "分隔");
        ui.radio_value(&mut options.fixed_width, true, "定宽");
    });
    let fixed = options.fixed_width;
    if fixed {
        if !allow_declared_widths {
            options.declared_widths = false;
        }
        ui.horizontal_wrapped(|ui| {
            ui.label("定宽依据");
            ui.radio_value(&mut options.declared_widths, false, "按内容");
            ui.add_enabled_ui(allow_declared_widths, |ui| {
                ui.radio_value(&mut options.declared_widths, true, "按声明宽度");
            })
            .response
            .on_disabled_hover_text("源连接或结果未提供列的声明宽度");
        });
    }
    egui::Grid::new("export-csv-options")
        .num_columns(4)
        .show(ui, |ui| {
            if !fixed {
                csv_entry(ui, "字段分隔符", &mut options.field_end);
                csv_entry(ui, "封闭符", &mut options.enclosure);
                ui.end_row();
            }
            csv_entry(ui, "行分隔符", &mut options.row_end);
            csv_entry(ui, "转义符", &mut options.escape);
            ui.end_row();
            if !fixed {
                csv_entry(ui, "NULL 表示", &mut options.null);
                ui.end_row();
            }
        });
    ui.horizontal_wrapped(|ui| {
        ui.checkbox(&mut options.headers, "首行写入字段名");
        if !fixed {
            ui.add_enabled_ui(!options.enclosure.is_empty(), |ui| {
                ui.checkbox(&mut options.enclose_all, "封闭所有非 NULL 值");
            });
        }
    });
    let hint = if fixed && options.declared_widths {
        "按服务器声明的字节宽度补齐，NULL 写为空格。转义会增加字节。长文本和二进制请选按内容。"
    } else if fixed {
        "按各列最长内容的 UTF-8 字节数补齐空格，NULL 写为空格。"
    } else {
        r"分隔符可用 \t、\r、\n、\\。封闭符、转义符及 NULL 表示均可留空。"
    };
    ui.label(RichText::new(hint).small().color(style::muted()));
    if let Err(error) = options.validate() {
        ui.colored_label(style::error(), error);
        false
    } else {
        true
    }
}

fn csv_entry(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.label(label);
    ui.add(egui::TextEdit::singleline(value).desired_width(85.0));
}

fn excel_options(ui: &mut egui::Ui, options: &mut ExcelOptions) {
    ui.horizontal_wrapped(|ui| {
        let mut limited = options.text_limit.is_some();
        if ui.checkbox(&mut limited, "限制文本长度").changed() {
            options.text_limit = limited.then_some(50);
        }
        if let Some(limit) = &mut options.text_limit {
            ui.add(egui::DragValue::new(limit).range(1..=EXCEL_CELL_TEXT_LIMIT))
                .on_hover_text("按 UTF-16 单位计数，补充字符占两个单位");
        }
    });
    ui.horizontal_wrapped(|ui| {
        let mut limited = options.decimal_places.is_some();
        if ui.checkbox(&mut limited, "限制小数位数").changed() {
            options.decimal_places = limited.then_some(2);
        }
        if let Some(places) = &mut options.decimal_places {
            ui.add(egui::DragValue::new(places));
        }
    });
    if options.text_limit.is_some() || options.decimal_places.is_some() {
        ui.colored_label(style::muted(), "导出时截去超限文本和小数位。");
    }
}

fn sql_options(ui: &mut egui::Ui, mode: &mut SqlExportMode) {
    ui.horizontal_wrapped(|ui| {
        ui.label("导出内容");
        ui.radio_value(mode, SqlExportMode::Data, "仅数据");
        ui.radio_value(mode, SqlExportMode::Structure, "仅简化结构");
        ui.radio_value(mode, SqlExportMode::StructureAndData, "简化结构和数据");
    });
    if *mode != SqlExportMode::Data {
        ui.colored_label(style::muted(), "简化结构只保留所选字段名和数据类型。");
    }
}

pub(super) struct TableForm {
    object: DbObject,
    columns: Vec<ColumnInfo>,
    options: Options,
    loading: bool,
    error: Option<String>,
}

impl TableForm {
    pub(super) fn new(object: DbObject) -> Self {
        Self {
            object,
            columns: Vec::new(),
            options: Options::new(ExportKind::Csv, 0),
            loading: false,
            error: None,
        }
    }

    fn load_fields(&mut self, workspace: &mut Workspace) {
        if workspace.busy() {
            return;
        }
        if self.loading {
            self.loading = false;
            if let Some(reply) = workspace
                .export_columns
                .take()
                .filter(|reply| reply.object == self.object)
            {
                self.columns = reply.columns;
                self.options.selected = vec![true; self.columns.len()];
                if self.columns.is_empty() {
                    self.error = Some("服务器没有返回可导出的字段。".into());
                }
            } else {
                self.error = Some(workspace.status.clone());
            }
        }
        if workspace.server.is_some() && self.columns.is_empty() && self.error.is_none() {
            workspace.export_columns = None;
            self.loading = workspace.send(
                Command::ExportColumns {
                    object: self.object.clone(),
                },
                "正在读取导出字段…",
            );
            if !self.loading {
                self.error = Some(workspace.status.clone());
            }
        }
    }

    pub(super) fn dialog(&mut self, ctx: &egui::Context, workspace: &mut Workspace) -> bool {
        self.load_fields(workspace);
        let mut close = false;
        let mut start = false;
        let modal = style::modal(ctx, egui::Id::new("table-export")).show(ctx, |ui| {
            ui.set_width(form_width(ctx, 570.0));
            style::page_title(ui, "导出数据");
            ui.add_space(4.0);
            let ready = form_scroll(ctx, 120.0)
                .show(ui, |ui| {
                    form_section(ui, "数据来源", |ui| {
                        let label = format!("{}.{}", self.object.schema, self.object.name);
                        ui.add(egui::Label::new(&label).truncate().selectable(true))
                            .on_hover_text(label);
                        let formats = if self.object.kind == ObjectKind::Table {
                            &TABLE_FORMATS[..]
                        } else {
                            &TABLE_FORMATS[..TABLE_FORMATS.len() - 1]
                        };
                        self.options.format(ui, formats);
                    });
                    let any_selected = form_section(ui, "导出字段", |ui| {
                        let fields = self
                            .columns
                            .iter()
                            .enumerate()
                            .filter(|(_, column)| eligible(column, self.options.kind))
                            .map(|(index, column)| (index, column.name.as_str()));
                        let fields = fields.collect::<Vec<_>>();
                        if self.loading {
                            ui.colored_label(style::muted(), "正在读取导出字段…");
                            false
                        } else if let Some(error) = &self.error {
                            ui.colored_label(style::error(), error);
                            if ui
                                .add_enabled(
                                    !workspace.busy() && workspace.server.is_some(),
                                    egui::Button::new("重新读取"),
                                )
                                .clicked()
                            {
                                self.error = None;
                            }
                            false
                        } else if fields.is_empty() {
                            ui.colored_label(style::muted(), "当前格式没有可导出的字段。");
                            false
                        } else {
                            if self.options.kind == ExportKind::Sql {
                                ui.colored_label(style::muted(), "SQL 导出可写字段。");
                            }
                            self.options.field_picker(ui, fields.iter().copied())
                        }
                    });
                    let file_ready = form_section(ui, "导出文件", |ui| {
                        self.options.file_settings(
                            ui,
                            &format!("数据_{}", file_stem(&self.object.name)),
                            workspace
                                .server
                                .as_ref()
                                .is_some_and(|server| !server.http_tunnel),
                        )
                    });
                    if workspace
                        .server
                        .as_ref()
                        .is_some_and(|server| server.http_tunnel)
                    {
                        ui.colored_label(
                            style::muted(),
                            "HTTP 分批读取，请在导出期间保持源数据和结构稳定。",
                        );
                    }
                    any_selected && file_ready && !workspace.busy() && workspace.server.is_some()
                })
                .inner;
            ui.separator();
            right_aligned_row(ui, |ui| {
                start = ui.add_enabled(ready, style::primary("开始导出")).clicked();
                close = ui.button("取消").clicked();
            });
        });
        if close || modal.should_close() {
            return true;
        }
        if start && let Some(path) = self.options.path.clone() {
            let fields = self
                .columns
                .iter()
                .zip(&self.options.selected)
                .filter(|(column, selected)| **selected && eligible(column, self.options.kind))
                .map(|(column, _)| column.name.clone())
                .collect();
            return workspace.send(
                Command::Export {
                    object: self.object.clone(),
                    path,
                    settings: self.options.settings(Some(fields)),
                },
                "正在导出数据…",
            );
        }
        false
    }
}

fn eligible(column: &ColumnInfo, kind: ExportKind) -> bool {
    if kind == ExportKind::Sql {
        sql_export_writable(&column.extra)
    } else {
        !column
            .extra
            .split_ascii_whitespace()
            .any(|flag| flag.eq_ignore_ascii_case("INVISIBLE"))
    }
}
