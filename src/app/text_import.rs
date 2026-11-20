use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use eframe::egui::{self, Align, Layout, TextEdit};
use egui_extras::{Column, TableBuilder};

use crate::database::{
    ColumnInfo, Command, DbObject, LoadLocalConflict, LoadLocalFormat, LoadLocalImport,
    LoadLocalPriority, LoadLocalReport, OdbcConflict, TextImport,
};
use crate::sql_tools::delimited::{self, Data, FILE_SIZE_LIMIT, Options, SkipMode};

use crate::app::{
    io_jobs::{form_scroll, form_section, form_width, right_aligned_row},
    state::Workspace,
    style,
};

pub(crate) struct Import {
    index: usize,
    object: DbObject,
    path: PathBuf,
    method: Method,
    options: Options,
    local: LocalOptions,
    conflict: OdbcConflict,
    source: Source,
    target: Target,
    local_pending: bool,
    warnings_open: bool,
    error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Method {
    Parsed,
    Server,
}

struct LocalOptions {
    fixed_width: bool,
    charset: String,
    field_end: String,
    row_end: String,
    enclosure: String,
    escape: String,
    skip_lines: u64,
    priority: LoadLocalPriority,
    conflict: LoadLocalConflict,
    columns: Vec<LocalColumn>,
}

struct LocalColumn {
    index: usize,
    included: bool,
}

enum Source {
    File,
    Reading(Receiver<Result<Data, String>>),
    Ready { data: Data, mapping: Vec<Mapping> },
}

enum Target {
    Pending,
    Reading,
    Ready {
        fields: Vec<ColumnInfo>,
        ddl: Option<String>,
    },
    Error(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mapping {
    Unset,
    Skip,
    Column(usize),
}

impl Import {
    pub fn choose(index: usize, object: DbObject) -> Result<Option<Self>, String> {
        let Some(path) = choose_file()? else {
            return Ok(None);
        };
        let field_end = default_field_end(&path);
        Ok(Some(Self {
            index,
            object,
            path,
            method: Method::Parsed,
            options: Options {
                field_end: field_end.into(),
                ..Options::default()
            },
            local: LocalOptions::new(field_end),
            conflict: OdbcConflict::Abort,
            source: Source::File,
            target: Target::Pending,
            local_pending: false,
            warnings_open: false,
            error: None,
        }))
    }

    fn change_file(&mut self) {
        match choose_file() {
            Ok(Some(path)) => {
                let former = default_field_end(&self.path);
                let current = default_field_end(&path);
                if self.options.field_end == former {
                    self.options.field_end = current.into();
                }
                if self.local.field_end == former {
                    self.local.field_end = current.into();
                }
                self.path = path;
                self.source = Source::File;
                self.error = None;
            }
            Ok(None) => {}
            Err(error) => self.error = Some(error),
        }
    }

    fn poll_target(&mut self, workspace: &mut Workspace) {
        if workspace.busy() || workspace.server.is_none() {
            return;
        }
        match self.target {
            Target::Pending => {
                self.target = if workspace.send(
                    Command::Inspect {
                        object: self.object.clone(),
                    },
                    "正在读取导入目标字段…",
                ) {
                    workspace.detail = None;
                    Target::Reading
                } else {
                    Target::Error(workspace.status.clone())
                };
            }
            Target::Reading => {
                self.target = match workspace
                    .detail
                    .as_ref()
                    .filter(|detail| detail.object == self.object)
                {
                    Some(detail) => {
                        let fields: Vec<_> = detail
                            .columns
                            .iter()
                            .filter(|column| {
                                !column
                                    .extra
                                    .split_ascii_whitespace()
                                    .any(|word| word.eq_ignore_ascii_case("GENERATED"))
                            })
                            .cloned()
                            .collect();
                        if fields.is_empty() {
                            Target::Error("目标表没有可导入的列。".into())
                        } else {
                            self.local.columns = (0..fields.len())
                                .map(|index| LocalColumn {
                                    index,
                                    included: true,
                                })
                                .collect();
                            Target::Ready {
                                fields,
                                ddl: detail.ddl_error.is_none().then(|| detail.ddl.clone()),
                            }
                        }
                    }
                    None => Target::Error(workspace.status.clone()),
                };
            }
            _ => {}
        }
    }

    fn read_file(&mut self, ctx: &egui::Context) {
        self.error = None;
        let path = self.path.clone();
        let options = self.options.clone();
        let ctx = ctx.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        if let Err(error) = std::thread::Builder::new().spawn(move || {
            let result = File::open(path)
                .map_err(|error| format!("无法打开导入文件。{error}"))
                .and_then(|file| delimited::parse(file, &options));
            let _ = sender.send(result);
            ctx.request_repaint();
        }) {
            self.error = Some(format!("无法启动文件读取任务。{error}"));
        } else {
            self.source = Source::Reading(receiver);
        }
    }

    fn poll_file(&mut self) {
        let (Source::Reading(receiver), Target::Ready { fields, .. }) =
            (&self.source, &self.target)
        else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("文件读取任务已结束。".into()),
        };
        match result {
            Ok(data) => {
                let mapping = initial_mapping(&data, fields);
                self.source = Source::Ready { data, mapping };
            }
            Err(error) => {
                self.error = Some(error);
                self.source = Source::File;
            }
        }
    }

    fn write_parsed(&mut self, workspace: &mut Workspace) -> bool {
        let Target::Ready { fields, ddl } = &self.target else {
            return false;
        };
        let Source::Ready { data, mapping } = std::mem::replace(&mut self.source, Source::File)
        else {
            return false;
        };
        let columns = mapping
            .iter()
            .filter_map(|field| match field {
                Mapping::Column(index) => Some(fields[*index].name.clone()),
                _ => None,
            })
            .collect();
        let rows = data
            .rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .zip(&mapping)
                    .filter_map(|(value, field)| {
                        matches!(field, Mapping::Column(_)).then_some(value)
                    })
                    .collect()
            })
            .collect();
        if workspace.send(
            Command::ImportDelimitedText(TextImport {
                object: self.object.clone(),
                columns,
                rows,
                conflict: self.conflict,
                expected_ddl: ddl.clone(),
            }),
            "正在导入文本数据…",
        ) {
            return true;
        }
        self.error = Some(workspace.status.clone());
        false
    }

    fn write_local(&mut self, workspace: &mut Workspace) {
        let Target::Ready { fields, .. } = &self.target else {
            return;
        };
        let command = match self.local.request(&self.object, &self.path, fields) {
            Ok(request) => Command::ImportLocalFile(request),
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        if workspace.send(command, "正在将文本文件上传到数据库…") {
            self.local_pending = true;
            self.error = None;
        } else {
            self.error = Some(workspace.status.clone());
        }
    }
}

fn choose_file() -> Result<Option<PathBuf>, String> {
    let path = rfd::FileDialog::new()
        .set_title("选择要导入的文本文件")
        .add_filter("文本文件（CSV、TSV、TXT）", &["csv", "tsv", "txt"])
        .add_filter("所有文件", &["*"])
        .pick_file();
    if let Some(path) = &path {
        let file = fs::metadata(path).map_err(|error| format!("无法读取所选文件。{error}"))?;
        if !file.is_file() {
            return Err("请选择文件。".into());
        }
    }
    Ok(path)
}

fn default_field_end(path: &Path) -> &'static str {
    if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("tsv"))
    {
        r"\t"
    } else {
        ","
    }
}

fn initial_mapping(data: &Data, fields: &[ColumnInfo]) -> Vec<Mapping> {
    if let Some(headers) = &data.headers {
        let destination: HashMap<_, _> = fields
            .iter()
            .enumerate()
            .map(|(index, column)| (column.name.to_lowercase(), index))
            .collect();
        headers
            .iter()
            .map(|name| {
                destination
                    .get(&name.to_lowercase())
                    .copied()
                    .map_or(Mapping::Unset, Mapping::Column)
            })
            .collect()
    } else {
        (0..data.rows[0].len())
            .map(|index| {
                if index < fields.len() {
                    Mapping::Column(index)
                } else {
                    Mapping::Unset
                }
            })
            .collect()
    }
}

fn field_delimiter(ui: &mut egui::Ui, value: &mut String) {
    ui.horizontal_wrapped(|ui| {
        ui.label("字段分隔符");
        ui.add(TextEdit::singleline(value).desired_width(80.0))
            .on_hover_text(r"支持多个字符。\t 表示制表符。");
        for (delimiter, label) in [(",", "逗号"), (r"\t", "制表符"), (";", "分号")] {
            ui.selectable_value(value, delimiter.into(), label);
        }
    });
}

fn format_settings(ui: &mut egui::Ui, options: &mut Options) {
    field_delimiter(ui, &mut options.field_end);
    egui::Grid::new("text-import-format")
        .num_columns(4)
        .spacing([8.0, 5.0])
        .show(ui, |ui| {
            ui.label("行分隔符");
            ui.add(
                TextEdit::singleline(&mut options.row_end)
                    .desired_width(110.0)
                    .hint_text("自动识别"),
            )
            .on_hover_text(r"支持多个字符，例如 \r\n。留空可同时识别 CRLF、LF 和 CR。");
            ui.label("封闭符");
            ui.add(
                TextEdit::singleline(&mut options.enclosure)
                    .desired_width(80.0)
                    .hint_text("无"),
            )
            .on_hover_text("输入一个字符或留空。封闭范围内的分隔符保留在字段里。");
            ui.end_row();
            ui.label("NULL 标记");
            ui.add(
                TextEdit::singleline(&mut options.null)
                    .desired_width(110.0)
                    .hint_text("无"),
            )
            .on_hover_text("字段值与标记完全相同时导入为 SQL NULL。");
            ui.label("转义符");
            ui.add(
                TextEdit::singleline(&mut options.escape)
                    .desired_width(80.0)
                    .hint_text("无"),
            )
            .on_hover_text("输入一个字符或留空。未封闭的转义符加 N 也表示 NULL。");
            ui.end_row();
        });
    ui.horizontal_wrapped(|ui| {
        ui.label("跳过开头");
        ui.add(egui::DragValue::new(&mut options.skip).range(0..=FILE_SIZE_LIMIT));
        ui.selectable_value(&mut options.skip_mode, SkipMode::Lines, "文本行")
            .on_hover_text("按行分隔符计数，包含空行。");
        ui.selectable_value(&mut options.skip_mode, SkipMode::Records, "数据记录")
            .on_hover_text("按解析出的记录计数，封闭范围内的换行保留在字段里。");
    });
    ui.horizontal_wrapped(|ui| {
        ui.checkbox(&mut options.headers, "首条有效记录是列名");
        ui.add_enabled(
            !options.null.is_empty(),
            egui::Checkbox::new(&mut options.quoted_null, "封闭的标记也转为 NULL"),
        );
    });
}

impl LocalOptions {
    fn new(field_end: &str) -> Self {
        Self {
            fixed_width: false,
            charset: String::new(),
            field_end: field_end.into(),
            row_end: r"\n".into(),
            enclosure: "\"".into(),
            escape: String::new(),
            skip_lines: 0,
            priority: LoadLocalPriority::Normal,
            conflict: LoadLocalConflict::ServerDefault,
            columns: Vec::new(),
        }
    }

    fn request(
        &self,
        object: &DbObject,
        file: &Path,
        fields: &[ColumnInfo],
    ) -> Result<LoadLocalImport, String> {
        let format = if self.fixed_width {
            LoadLocalFormat::FixedWidth
        } else {
            let field_terminator = delimited::unescape(&self.field_end)?;
            let enclosure = delimited::marker(&self.enclosure, "字段引号")?;
            LoadLocalFormat::Delimited {
                field_terminator,
                enclosure,
                optionally_enclosed: enclosure.is_some(),
            }
        };
        let charset = self.charset.trim();
        Ok(LoadLocalImport {
            object: object.clone(),
            file: file.to_owned(),
            columns: self
                .columns
                .iter()
                .filter(|column| column.included)
                .map(|column| fields[column.index].name.clone())
                .collect(),
            charset: (!charset.is_empty()).then(|| charset.to_owned()),
            format,
            line_terminator: delimited::unescape(&self.row_end)?,
            escape: delimited::marker(&self.escape, "转义符")?,
            skip_lines: self.skip_lines,
            priority: self.priority,
            conflict: self.conflict,
        })
    }

    fn settings(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("文件格式");
            ui.radio_value(&mut self.fixed_width, false, "按字符分隔");
            ui.radio_value(&mut self.fixed_width, true, "固定宽度");
        });
        ui.horizontal(|ui| {
            ui.label("文件字符集");
            ui.add(
                TextEdit::singleline(&mut self.charset)
                    .hint_text("目标库默认值")
                    .desired_width(116.0),
            )
            .on_hover_text("也可以直接输入数据库支持的字符集名称。");
            egui::ComboBox::from_id_salt("text-import-charset")
                .selected_text("选择字符集")
                .show_ui(ui, |ui| {
                    for (value, label) in [
                        ("", "目标库默认值"),
                        ("utf8mb4", "UTF-8（utf8mb4）"),
                        ("gbk", "简体中文（gbk）"),
                        ("big5", "繁体中文（big5）"),
                        ("latin1", "西欧（latin1）"),
                        ("ascii", "ASCII"),
                        ("binary", "按原始字节（binary）"),
                    ] {
                        ui.selectable_value(&mut self.charset, value.into(), label);
                    }
                });
        });
        if self.fixed_width {
            ui.small("数据库按所选目标列的类型和长度切分字段。文件须使用单字节字符集，目标列须有固定宽度。");
        } else {
            field_delimiter(ui, &mut self.field_end);
        }
        ui.horizontal(|ui| {
            ui.label("行分隔符");
            ui.add(
                TextEdit::singleline(&mut self.row_end)
                    .hint_text("空")
                    .desired_width(80.0),
            )
            .on_hover_text(
                r"可输入多个 ASCII 字符。\r\n 表示 Windows 换行。定宽文件没有换行符时可留空。",
            );
            ui.selectable_value(&mut self.row_end, r"\n".into(), "LF");
            ui.selectable_value(&mut self.row_end, r"\r\n".into(), "CRLF");
        });
        ui.horizontal(|ui| {
            if !self.fixed_width {
                ui.label("字段引号");
                ui.add(
                    TextEdit::singleline(&mut self.enclosure)
                        .desired_width(80.0)
                        .hint_text("无"),
                )
                .on_hover_text("可填一个 ASCII 字符，例如双引号，也可留空。");
            }
            ui.label("转义符");
            ui.add(
                TextEdit::singleline(&mut self.escape)
                    .desired_width(80.0)
                    .hint_text("无"),
            )
            .on_hover_text(r"可填一个 ASCII 字符，例如 \，也可留空。");
        });
        ui.horizontal(|ui| {
            ui.label("跳过开头");
            ui.add(egui::DragValue::new(&mut self.skip_lines).range(0..=u64::MAX));
            ui.label("行").on_hover_text("文件有列名行时可填 1。");
        });
        ui.horizontal(|ui| {
            ui.label("导入优先级");
            ui.radio_value(&mut self.priority, LoadLocalPriority::Normal, "默认");
            ui.radio_value(&mut self.priority, LoadLocalPriority::Low, "低优先级")
                .on_hover_text("对 MyISAM 等使用表级锁的引擎生效，等待其他读取结束后导入。");
            ui.radio_value(
                &mut self.priority,
                LoadLocalPriority::Concurrent,
                "并发读取",
            )
            .on_hover_text("满足并发插入条件的 MyISAM 表可在导入期间继续读取。");
        });
        ui.horizontal(|ui| {
            ui.label("遇到重复键");
            ui.radio_value(
                &mut self.conflict,
                LoadLocalConflict::ServerDefault,
                "由数据库决定",
            )
            .on_hover_text("使用数据库对 LOAD DATA LOCAL 的默认处理规则。");
            ui.radio_value(&mut self.conflict, LoadLocalConflict::Ignore, "跳过重复行");
            ui.radio_value(&mut self.conflict, LoadLocalConflict::Replace, "替换已有行")
                .on_hover_text("先删除冲突行，再插入文件中的数据。未导入的列使用默认值。");
        });
    }

    fn column_settings(&mut self, ui: &mut egui::Ui, fields: &[ColumnInfo]) -> bool {
        ui.horizontal_wrapped(|ui| {
            if ui.button("全选").clicked() {
                for column in &mut self.columns {
                    column.included = true;
                }
            }
            if ui.button("清空选择").clicked() {
                for column in &mut self.columns {
                    column.included = false;
                }
            }
            if ui.button("恢复表中顺序").clicked() {
                self.columns.sort_by_key(|column| column.index);
            }
        });
        ui.small("文件字段依次写入下方勾选的列。可调整列顺序以匹配文件。");
        let mut reorder = None;
        let count = self.columns.len();
        egui::ScrollArea::vertical()
            .id_salt("text-import-local-columns")
            .max_height(158.0)
            .show_rows(ui, ui.spacing().interact_size.y, count, |ui, range| {
                for position in range {
                    let column = &mut self.columns[position];
                    let field = &fields[column.index];
                    ui.push_id(column.index, |ui| {
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut column.included, "")
                                .on_hover_text(&field.name);
                            ui.add_sized(
                                [138.0, ui.spacing().interact_size.y],
                                egui::Label::new(&field.name).truncate(),
                            )
                            .on_hover_text(&field.name);
                            ui.add_sized(
                                [128.0, ui.spacing().interact_size.y],
                                egui::Label::new(&field.data_type).truncate(),
                            )
                            .on_hover_text(&field.data_type);
                            if ui
                                .add_enabled(position > 0, egui::Button::new("上移"))
                                .clicked()
                            {
                                reorder = Some((position, position - 1));
                            }
                            if ui
                                .add_enabled(position + 1 < count, egui::Button::new("下移"))
                                .clicked()
                            {
                                reorder = Some((position, position + 1));
                            }
                        });
                    });
                }
            });
        if let Some((from, to)) = reorder {
            self.columns.swap(from, to);
        }
        let selected = self.columns.iter().any(|column| column.included);
        if !selected {
            ui.colored_label(style::ERROR, "请至少勾选一个目标列。");
        }
        selected
    }
}

fn mapping_settings(
    ui: &mut egui::Ui,
    data: &Data,
    mapping: &mut [Mapping],
    fields: &[ColumnInfo],
) -> bool {
    ui.colored_label(
        style::MUTED,
        format!("{} 行数据 · 文件字段 → 目标列", data.rows.len()),
    );
    egui::ScrollArea::vertical()
        .id_salt("text-import-mapping")
        .max_height(170.0)
        .show_rows(
            ui,
            ui.spacing().interact_size.y,
            mapping.len(),
            |ui, range| {
                for index in range {
                    ui.horizontal(|ui| {
                        let label = data
                            .headers
                            .as_ref()
                            .map(|names| names[index].as_str())
                            .filter(|name| !name.is_empty())
                            .map_or_else(
                                || Cow::Owned(format!("字段 {}", index + 1)),
                                Cow::Borrowed,
                            );
                        let width = ui.available_width();
                        ui.add_sized(
                            [width * 0.4, ui.spacing().interact_size.y],
                            egui::Label::new(label.as_ref()).truncate(),
                        )
                        .on_hover_text(label.as_ref());
                        let field = &mut mapping[index];
                        let label = match *field {
                            Mapping::Unset => "请选择目标列",
                            Mapping::Skip => "跳过此字段",
                            Mapping::Column(index) => &fields[index].name,
                        };
                        egui::ComboBox::from_id_salt(("text-import-field", index))
                            .width(ui.available_width())
                            .truncate()
                            .selected_text(label)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(field, Mapping::Skip, "跳过此字段");
                                for (index, column) in fields.iter().enumerate() {
                                    ui.selectable_value(
                                        field,
                                        Mapping::Column(index),
                                        &column.name,
                                    );
                                }
                            });
                    });
                }
            },
        );
    let mut used = HashSet::new();
    let mut missing = false;
    let mut duplicate = false;
    for field in mapping {
        match *field {
            Mapping::Unset => missing = true,
            Mapping::Column(index) => duplicate |= !used.insert(index),
            Mapping::Skip => {}
        }
    }
    if missing || duplicate || used.is_empty() {
        ui.colored_label(
            style::ERROR,
            "请指定目标列或跳过对应字段。目标列须至少选择一个，且每个只能选一次。",
        );
        return false;
    }
    true
}

fn local_report(ui: &mut egui::Ui, report: &LoadLocalReport, warnings_open: &mut bool) -> bool {
    form_section(ui, "导入完成", |ui| {
        let color = if report.warning_count == 0 {
            style::SUCCESS
        } else {
            style::WARNING
        };
        ui.colored_label(
            color,
            format!(
                "受影响 {} 行，收到 {} 条警告。",
                report.affected_rows, report.warning_count
            ),
        );
        if let (Some(records), Some(deleted), Some(skipped)) =
            (report.records, report.deleted, report.skipped)
        {
            ui.label(format!(
                "读取 {records} 条记录，删除 {deleted} 行，跳过 {skipped} 条记录。"
            ));
        } else if !report.info.is_empty() {
            ui.add(egui::Label::new(&report.info).wrap().selectable(true));
        }
    });
    right_aligned_row(ui, |ui| {
        let closed = ui.add(style::primary("关闭")).clicked();
        if ui.button("查看警告…").clicked() {
            *warnings_open = true;
        }
        closed
    })
}

fn local_warnings_dialog(ctx: &egui::Context, report: &LoadLocalReport) -> bool {
    let modal = style::modal(ctx, egui::Id::new("text-import-warnings")).show(ctx, |ui| {
        ui.set_width(form_width(ctx, 560.0));
        ui.heading("导入警告");
        if !report.warnings.is_empty() && report.warning_count > report.warnings.len() as u64 {
            ui.small(format!("显示其中 {} 条警告。", report.warnings.len()));
        }
        let row_height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        TableBuilder::new(ui)
            .id_salt("warning-rows")
            .striped(true)
            .cell_layout(Layout::left_to_right(Align::Center))
            .columns(Column::exact(64.0).clip(true), 2)
            .column(Column::remainder().clip(true))
            .min_scrolled_height(0.0)
            .max_scroll_height((ctx.content_rect().height() - 160.0).clamp(75.0, 240.0))
            .header(row_height, |mut header| {
                for label in ["级别", "代码", "消息"] {
                    header.col(|ui| {
                        ui.strong(label);
                    });
                }
            })
            .body(|body| {
                body.rows(row_height, report.warnings.len(), |mut row| {
                    let warning = &report.warnings[row.index()];
                    let level = match warning.level.as_str() {
                        "Error" => "错误",
                        "Warning" => "警告",
                        "Note" => "提示",
                        other => other,
                    };
                    let code = warning.code.to_string();
                    for text in [level, code.as_str(), &warning.message] {
                        row.col(|ui| {
                            ui.add(egui::Label::new(text).truncate().selectable(true))
                                .on_hover_text(text);
                        });
                    }
                });
            });
        if report.warnings.is_empty() {
            if report.warning_count == 0 {
                ui.label("本次导入没有警告。");
            } else {
                ui.label(format!(
                    "服务器报告 {} 条警告，未返回明细。",
                    report.warning_count
                ));
            }
        }
        right_aligned_row(ui, |ui| ui.add(style::primary("关闭")).clicked())
    });
    modal.inner || modal.should_close()
}

pub(crate) fn import_dialog(
    ctx: &egui::Context,
    state: &mut Option<Import>,
    workspaces: &mut [Workspace],
) {
    let Some(import) = state else { return };
    let Some(workspace) = workspaces.get_mut(import.index) else {
        *state = None;
        return;
    };
    if import.local_pending
        && let Some(Err(error)) = &workspace.local_file_import_result
    {
        import.local_pending = false;
        import.error = Some(error.clone());
        workspace.local_file_import_result = None;
    }
    import.poll_target(workspace);
    import.poll_file();
    let mut choose = false;
    let mut read = false;
    let mut run = false;
    let mut cancel = false;
    let modal = style::modal(ctx, egui::Id::new("import-dialog")).show(ctx, |ui| {
        ui.set_width(form_width(ctx, 620.0));
        ui.heading("导入文本文件");
        let destination = format!("目标表 · {}.{}", import.object.schema, import.object.name);
        ui.add(egui::Label::new(egui::RichText::new(&destination).color(style::MUTED)).truncate())
            .on_hover_text(destination);
        if import.local_pending {
            if let Some(Ok(report)) = &workspace.local_file_import_result {
                cancel = local_report(ui, report, &mut import.warnings_open);
            } else {
                form_section(ui, "导入进度", |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("正在将文本文件上传到数据库…");
                    });
                });
            }
            return;
        }

        let reading = matches!(import.source, Source::Reading(_));
        let connected = workspace.server.is_some();
        let can_local = workspace
            .server
            .as_ref()
            .is_some_and(|server| !server.http_tunnel);
        let can_start = connected && !workspace.busy();
        let ready = form_scroll(ctx, 160.0)
            .show(ui, |ui| {
                form_section(ui, "文件来源", |ui| {
                    right_aligned_row(ui, |ui| {
                        choose = ui
                            .add_enabled(!reading, egui::Button::new("更换文件…"))
                            .clicked();
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            let path = import.path.display().to_string();
                            ui.add(egui::Label::new(&path).truncate().selectable(true))
                                .on_hover_text(path);
                        });
                    });
                    let previous = import.method;
                    ui.add_enabled_ui(!reading, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label("导入方式");
                            ui.selectable_value(&mut import.method, Method::Parsed, "读取后导入");
                            ui.add_enabled_ui(can_local, |ui| {
                                ui.selectable_value(
                                    &mut import.method,
                                    Method::Server,
                                    "服务器导入",
                                );
                            })
                            .response
                            .on_disabled_hover_text("服务器导入需要直连或 SSH。");
                        });
                    });
                    if import.method != previous {
                        import.error = None;
                    }
                    if !can_local && connected {
                        ui.colored_label(style::MUTED, "通过 HTTP 隧道读取并导入 UTF-8 文本。");
                    }
                });
                form_section(ui, "文件解析", |ui| match import.method {
                    Method::Parsed => {
                        if can_local {
                            ui.colored_label(style::MUTED, "读取 UTF-8 文件并匹配字段。");
                        }
                        let previous = import.options.clone();
                        ui.add_enabled_ui(!reading, |ui| format_settings(ui, &mut import.options));
                        if import.options != previous {
                            import.source = Source::File;
                            import.error = None;
                        }
                        ui.horizontal_wrapped(|ui| {
                            ui.label("遇到重复键");
                            ui.radio_value(&mut import.conflict, OdbcConflict::Abort, "停止并回滚");
                            ui.radio_value(
                                &mut import.conflict,
                                OdbcConflict::KeepTarget,
                                "跳过重复行",
                            )
                            .on_hover_text("HTTP 隧道使用此策略时需要 MariaDB 10.1 或更新版本。");
                            ui.radio_value(
                                &mut import.conflict,
                                OdbcConflict::Replace,
                                "替换已有行",
                            )
                            .on_hover_text("删除冲突行后插入，未映射的列使用默认值。");
                        });
                    }
                    Method::Server => {
                        ui.colored_label(
                            style::MUTED,
                            "将本机文件上传到数据库解析。数据库须启用 local_infile。",
                        );
                        import.local.settings(ui);
                    }
                });
                match &import.target {
                    Target::Ready { fields, .. } => match (&import.method, &mut import.source) {
                        (Method::Server, _) => form_section(ui, "目标列顺序", |ui| {
                            import.local.column_settings(ui, fields)
                        }),
                        (Method::Parsed, Source::Ready { data, mapping }) => {
                            form_section(ui, "字段映射", |ui| {
                                mapping_settings(ui, data, mapping, fields)
                            })
                        }
                        _ => false,
                    },
                    Target::Error(error) => {
                        ui.colored_label(style::ERROR, error);
                        if ui
                            .add_enabled(can_start, egui::Button::new("重新读取目标字段"))
                            .clicked()
                        {
                            import.target = Target::Pending;
                        }
                        false
                    }
                    Target::Pending | Target::Reading => {
                        if connected {
                            ui.colored_label(style::MUTED, "正在读取目标字段…");
                        }
                        false
                    }
                }
            })
            .inner;
        ui.separator();
        if reading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("正在读取文件…");
            });
        }
        if !connected {
            ui.colored_label(style::ERROR, "数据库连接已断开。");
        }
        if let Some(error) = &import.error {
            ui.add(egui::Label::new(egui::RichText::new(error).color(style::ERROR)).truncate())
                .on_hover_text(error);
        }
        right_aligned_row(ui, |ui| {
            if import.method == Method::Server {
                run = ui
                    .add_enabled(can_start && can_local && ready, style::primary("导入数据"))
                    .clicked();
            } else if matches!(import.source, Source::Ready { .. }) {
                run = ui
                    .add_enabled(can_start && ready, style::primary("导入数据"))
                    .clicked();
                read = ui
                    .add_enabled(can_start, egui::Button::new("重新读取文件"))
                    .clicked();
            } else {
                read = ui
                    .add_enabled(
                        can_start && !reading && matches!(import.target, Target::Ready { .. }),
                        style::primary("读取文件"),
                    )
                    .clicked();
            }
            cancel = ui.button("取消").clicked();
        });
    });
    if import.warnings_open
        && let Some(Ok(report)) = &workspace.local_file_import_result
    {
        import.warnings_open = !local_warnings_dialog(ctx, report);
        return;
    }
    let uploading = import.local_pending && workspace.local_file_import_result.is_none();
    if cancel || modal.should_close() && !uploading {
        *state = None;
        workspace.local_file_import_result = None;
    } else if choose {
        import.change_file();
    } else if read {
        import.read_file(ctx);
    } else if run {
        if import.method == Method::Server {
            import.write_local(workspace);
        } else if import.write_parsed(workspace) {
            *state = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::{LoadLocalWarning, ObjectKind};

    #[test]
    fn server_import_summary_offers_a_separate_warning_view_for_zero_and_positive_counts() {
        fn has_label(output: &egui::FullOutput, label: &str) -> bool {
            fn matches(shape: &egui::Shape, label: &str) -> bool {
                match shape {
                    egui::Shape::Text(text) => text.galley.text() == label,
                    egui::Shape::Vec(shapes) => shapes.iter().any(|shape| matches(shape, label)),
                    _ => false,
                }
            }
            output
                .shapes
                .iter()
                .any(|shape| matches(&shape.shape, label))
        }

        for has_warning in [false, true] {
            let report = LoadLocalReport {
                affected_rows: 1,
                records: Some(1),
                deleted: Some(0),
                skipped: Some(0),
                info: String::new(),
                warning_count: u64::from(has_warning),
                warnings: has_warning
                    .then(|| LoadLocalWarning {
                        level: "Warning".into(),
                        code: 1062,
                        message: "重复主键".into(),
                    })
                    .into_iter()
                    .collect(),
            };
            let ctx = egui::Context::default();
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 500.0),
                )),
                ..Default::default()
            };
            let mut warnings_open = false;
            let summary = ctx.run(input.clone(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    local_report(ui, &report, &mut warnings_open);
                });
            });
            assert!(has_label(&summary, "查看警告…"));
            assert!(!has_label(&summary, "重复主键"));
            assert!(!warnings_open);

            let render = || {
                ctx.run(input.clone(), |ctx| {
                    local_warnings_dialog(ctx, &report);
                })
            };
            let _ = render();
            let warnings = render();
            for label in ["导入警告", "级别", "代码", "消息", "关闭"] {
                assert!(has_label(&warnings, label), "未显示 {label}");
            }
            assert_eq!(has_label(&warnings, "1062"), has_warning);
            assert_eq!(has_label(&warnings, "重复主键"), has_warning);
            assert_eq!(has_label(&warnings, "本次导入没有警告。"), !has_warning);
        }
    }

    #[test]
    fn server_import_preserves_selected_column_order_and_file_settings() {
        let table = DbObject {
            schema: "test_import".into(),
            name: "entries".into(),
            kind: ObjectKind::Table,
        };
        let fields = ["id", "comment", "item_code"].map(|name| ColumnInfo {
            name: name.into(),
            data_type: "char(4)".into(),
            nullable: true,
            key: String::new(),
            default_value: None,
            extra: String::new(),
        });
        let file = Path::new("entries.tsv");
        let mut settings = LocalOptions::new(default_field_end(file));
        settings.columns = vec![
            LocalColumn {
                index: 2,
                included: true,
            },
            LocalColumn {
                index: 1,
                included: false,
            },
            LocalColumn {
                index: 0,
                included: true,
            },
        ];
        settings.charset = "latin1".into();
        settings.row_end = r"\r\n".into();
        settings.escape = r"\\".into();
        settings.skip_lines = 1;
        settings.priority = LoadLocalPriority::Low;
        settings.conflict = LoadLocalConflict::Ignore;

        let request = settings.request(&table, file, &fields).unwrap();
        assert_eq!(request.object, table);
        assert_eq!(request.file, file);
        assert_eq!(request.columns, ["item_code", "id"]);
        assert_eq!(request.charset.as_deref(), Some("latin1"));
        assert_eq!(request.line_terminator, "\r\n");
        assert_eq!(request.escape, Some('\\'));
        assert_eq!(request.skip_lines, 1);
        assert!(matches!(request.priority, LoadLocalPriority::Low));
        assert!(matches!(request.conflict, LoadLocalConflict::Ignore));
        assert!(matches!(
            request.format,
            LoadLocalFormat::Delimited {
                field_terminator,
                enclosure: Some('"'),
                optionally_enclosed: true,
            } if field_terminator == "\t"
        ));

        settings.fixed_width = true;
        let fixed = settings.request(&table, file, &fields).unwrap();
        assert_eq!(fixed.columns, ["item_code", "id"]);
        assert!(matches!(fixed.format, LoadLocalFormat::FixedWidth));
    }
}
