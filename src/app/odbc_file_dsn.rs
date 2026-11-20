use std::path::PathBuf;

use eframe::egui::{self, ComboBox, TextEdit};
use harbor_sql::odbc;
use rfd::FileDialog;

use super::{
    io_jobs::{form_section, right_aligned_row},
    style,
};

#[derive(Clone)]
struct Creation {
    drivers: Vec<String>,
    driver: usize,
    attributes: String,
}

fn dialog() -> FileDialog {
    FileDialog::new().add_filter("ODBC 文件 DSN", &["dsn"])
}

pub(super) fn fields(ui: &mut egui::Ui, path: &mut PathBuf) -> Result<bool, String> {
    let id = ui.id().with("create-odbc-file-dsn");
    let (choose, create) = ui
        .horizontal_wrapped(|ui| {
            let choose = ui.button("选择文件 DSN…").clicked();
            let create = ui
                .add_enabled(cfg!(windows), egui::Button::new("新建文件 DSN…"))
                .on_disabled_hover_text("新建文件 DSN 需要 Windows。")
                .clicked();
            (choose, create)
        })
        .inner;
    if choose && let Some(selected) = dialog().pick_file() {
        odbc::validate_file_dsn_path(&selected)?;
        let changed = *path != selected;
        *path = selected;
        ui.data_mut(|data| data.remove::<Creation>(id));
        return Ok(changed);
    }
    if !path.as_os_str().is_empty() {
        let label = path.display().to_string();
        ui.add(egui::Label::new(&label).truncate().selectable(true))
            .on_hover_text(label);
    } else {
        ui.colored_label(style::muted(), "未选择文件 DSN");
    }
    let mut draft: Option<Creation> = ui.data(|data| data.get_temp(id));
    if create {
        let drivers = odbc::installed_odbc_drivers()?;
        if drivers.is_empty() {
            return Err("未找到适用于当前程序架构的 ODBC 驱动。".into());
        }
        draft = Some(Creation {
            drivers,
            driver: 0,
            attributes: String::new(),
        });
    }
    let Some(mut draft) = draft else {
        return Ok(false);
    };
    let (save, cancel) = form_section(ui, "新建文件 DSN", |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label("ODBC 驱动");
            ComboBox::from_id_salt("file-dsn-driver")
                .width(ui.available_width().min(290.0))
                .truncate()
                .selected_text(&draft.drivers[draft.driver])
                .show_ui(ui, |ui| {
                    for (index, driver) in draft.drivers.iter().enumerate() {
                        ui.selectable_value(&mut draft.driver, index, driver);
                    }
                });
        });
        ui.colored_label(
            style::muted(),
            "填写驱动需要的连接属性，每行名称=值。用户名和密码由连接时提供。",
        );
        ui.add(
            TextEdit::multiline(&mut draft.attributes)
                .code_editor()
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .hint_text("例如 SERVER=主机名、DATABASE=库名、DBQ=文件路径"),
        );
        right_aligned_row(ui, |ui| {
            (
                ui.add(style::primary("保存文件 DSN…")).clicked(),
                ui.button("取消新建").clicked(),
            )
        })
    });
    if cancel {
        ui.data_mut(|data| data.remove::<Creation>(id));
        return Ok(false);
    }
    let result = if save && let Some(mut destination) = dialog().save_file() {
        if destination.extension().is_none() {
            destination.set_extension("dsn");
        }
        odbc::create_file_dsn(
            &destination,
            &draft.drivers[draft.driver],
            &draft.attributes,
        )
        .map(|()| {
            *path = destination;
            true
        })
    } else {
        Ok(false)
    };
    if matches!(result, Ok(true)) {
        ui.data_mut(|data| data.remove::<Creation>(id));
    } else {
        ui.data_mut(|data| data.insert_temp(id, draft));
    }
    result
}
