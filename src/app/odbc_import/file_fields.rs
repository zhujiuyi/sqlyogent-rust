use std::path::PathBuf;

use eframe::egui::{self, ComboBox, TextEdit};
use harbor_sql::file_source::{FileSource, FileSourceEncoding, FileSourceFormat};
use harbor_sql::odbc::{DiscoveredTable, SourceTable};
use rfd::FileDialog;

pub(in crate::app) fn show(ui: &mut egui::Ui, source: &mut FileSource) -> bool {
    let original_format = source.format;
    ui.horizontal_wrapped(|ui| {
        ui.label("文件格式");
        ui.radio_value(&mut source.format, FileSourceFormat::Csv, "CSV");
        ui.radio_value(&mut source.format, FileSourceFormat::Excel, "Excel");
        ui.radio_value(&mut source.format, FileSourceFormat::Access, "Access");
    });
    let mut changed = source.format != original_format;
    if changed {
        source.path.clear();
        source.access_password_env = None;
        source.access_link_folder = None;
        source.excel_named_ranges = false;
    }
    let csv = source.format == FileSourceFormat::Csv;
    let mut path = source.path.to_string_lossy().into_owned();
    let (label, hint, title, extensions) = match source.format {
        FileSourceFormat::Csv => (
            "CSV 文件或目录",
            "输入或选择 CSV 文件或目录",
            "选择 CSV 文件",
            &["csv", "tsv", "txt"][..],
        ),
        FileSourceFormat::Excel => (
            "Excel 工作簿",
            "输入或选择 Excel 工作簿",
            "选择 Excel 工作簿",
            &["xlsx", "xls", "xlsm", "xlsb"][..],
        ),
        FileSourceFormat::Access => (
            "Access 数据库",
            "输入或选择 .mdb 或 .accdb 文件",
            "选择 Access 数据库",
            &["mdb", "accdb"][..],
        ),
    };
    ui.strong(label);
    if ui
        .add(
            TextEdit::singleline(&mut path)
                .desired_width(f32::INFINITY)
                .hint_text(hint),
        )
        .on_hover_text(&path)
        .changed()
    {
        source.path = PathBuf::from(path);
        changed = true;
    }
    ui.horizontal_wrapped(|ui| {
        if ui.button("选择文件…").clicked()
            && let Some(path) = FileDialog::new()
                .set_title(title)
                .add_filter(if csv { "分隔文本文件" } else { label }, extensions)
                .pick_file()
        {
            changed |= path != source.path;
            source.path = path;
        }
        if csv
            && ui.button("选择 CSV 目录…").clicked()
            && let Some(path) = FileDialog::new().set_title("选择 CSV 目录").pick_folder()
        {
            changed |= path != source.path;
            source.path = path;
        }
    });
    if source.format == FileSourceFormat::Access {
        let folder_changed = access_link_folder(ui, &mut source.access_link_folder);
        return changed || folder_changed;
    }
    if source.format == FileSourceFormat::Excel {
        let original_scope = source.excel_named_ranges;
        ui.horizontal_wrapped(|ui| {
            ui.label("导入范围");
            ui.radio_value(&mut source.excel_named_ranges, false, "工作表");
            ui.radio_value(&mut source.excel_named_ranges, true, "命名区域")
                .on_hover_text("读取工作簿中定义的固定单元格范围。");
        });
        changed |= source.excel_named_ranges != original_scope;
    }
    ui.horizontal_wrapped(|ui| {
        if csv {
            ui.label("文件编码");
            let encoding = source.encoding;
            ComboBox::from_id_salt("file-import-encoding")
                .width(132.0)
                .selected_text(encoding.label())
                .show_ui(ui, |ui| {
                    for encoding in FileSourceEncoding::ALL {
                        ui.selectable_value(&mut source.encoding, encoding, encoding.label());
                    }
                });
            changed |= source.encoding != encoding;
            ui.label("分隔符");
            changed |= ui
                .add(TextEdit::singleline(&mut source.delimiter).desired_width(48.0))
                .on_hover_text("输入一个分隔符。制表符可写成 \\t。")
                .changed();
        }
        changed |= ui.checkbox(&mut source.headers, "首行作为列名").changed();
    });
    changed
}

fn access_link_folder(ui: &mut egui::Ui, folder: &mut Option<PathBuf>) -> bool {
    let mut path = folder
        .as_ref()
        .map_or_else(String::new, |folder| folder.display().to_string());
    ui.strong("链接表文件夹");
    let mut changed = ui
        .add(
            TextEdit::singleline(&mut path)
                .id_salt("access-link-folder")
                .desired_width(f32::INFINITY)
                .hint_text("留空时只读取当前 Access 文件中的表"),
        )
        .on_hover_text(&path)
        .changed();
    if changed {
        *folder = (!path.is_empty()).then(|| PathBuf::from(path));
    }
    if ui.button("选择链接表文件夹…").clicked()
        && let Some(path) = FileDialog::new()
            .set_title("选择链接表文件夹")
            .pick_folder()
    {
        changed |= folder.as_ref() != Some(&path);
        *folder = Some(path);
    }
    changed
}

pub(in crate::app) fn show_job(ui: &mut egui::Ui, source: &mut FileSource) -> (bool, bool) {
    let changed = show(ui, source);
    if source.format != FileSourceFormat::Access {
        return (changed, false);
    }
    let mut variable = source.access_password_env.clone().unwrap_or_default();
    ui.label("Access 密码环境变量");
    let edited = ui
        .add(
            TextEdit::singleline(&mut variable)
                .desired_width(f32::INFINITY)
                .hint_text("选填变量名，例如 ACCESS_FILE_PASSWORD"),
        )
        .changed();
    if edited {
        let reference = (!variable.trim().is_empty()).then(|| variable.trim().into());
        let reference_changed = source.access_password_env != reference;
        source.access_password_env = reference;
        return (changed, reference_changed);
    }
    (changed, false)
}

pub(super) fn discovered_table(name: String) -> DiscoveredTable {
    DiscoveredTable {
        source: SourceTable {
            table: name,
            ..Default::default()
        },
        select_sql: String::new(),
    }
}
