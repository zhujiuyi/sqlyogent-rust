use std::fmt::Write as _;

use eframe::egui::{self, RichText};

use crate::database::QueryProfile;

use super::output::{markup, with_statement_info};
use super::state::{Workspace, write_atomic};
use super::style;

pub(super) fn panel(ui: &mut egui::Ui, workspace: &mut Workspace) {
    let tab = workspace.tab();
    let Some(profile) = &tab.profile else { return };
    let mut save = false;
    ui.horizontal(|ui| {
        ui.strong(elapsed(profile));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            save = ui.button("保存 HTML…").clicked();
        });
    });
    let statement = workspace.show_data_info.then_some(profile.sql.as_str());
    with_statement_info(ui, statement, |ui| {
        egui::ScrollArea::both()
            .id_salt(("profile", workspace.key, tab.id))
            .show(ui, |ui| {
                if let Some(error) = &profile.error {
                    ui.colored_label(style::ERROR, error);
                }
                for notice in &profile.notices {
                    ui.colored_label(style::MUTED, notice);
                }
                if statement.is_none() {
                    egui::CollapsingHeader::new("已执行的 SQL").show(ui, |ui| {
                        ui.monospace(&profile.sql);
                    });
                }
                for section in &profile.sections {
                    egui::CollapsingHeader::new(section.title)
                        .default_open(true)
                        .show(ui, |ui| {
                            ui.label(RichText::new(section.help).color(style::MUTED));
                            match &section.data {
                                Ok(table) => {
                                    if !table.summary.is_empty() {
                                        ui.label(&table.summary);
                                    }
                                    if table.rows.is_empty() {
                                        return;
                                    }
                                    egui::Grid::new(section.title).striped(true).show(ui, |ui| {
                                        for column in &table.columns {
                                            ui.strong(column);
                                        }
                                        ui.end_row();
                                        for row in &table.rows {
                                            for value in row {
                                                ui.monospace(value);
                                            }
                                            ui.end_row();
                                        }
                                    });
                                }
                                Err(error) => {
                                    ui.label(
                                        RichText::new(format!("无法采集。{error}"))
                                            .color(style::MUTED),
                                    );
                                }
                            }
                        });
                }
            });
    });
    if save
        && let Some(path) = rfd::FileDialog::new()
            .set_title("保存查询分析")
            .add_filter("HTML", &["html"])
            .set_file_name("query-profile.html")
            .save_file()
    {
        match write_atomic(&path, html(profile).as_bytes()) {
            Ok(()) => workspace.message(format!("已保存查询分析到 {}", path.display()), false),
            Err(error) => workspace.message(format!("无法保存查询分析。{error}"), true),
        }
    }
}

fn elapsed(profile: &QueryProfile) -> String {
    format!(
        "客户端查询及读取结果耗时 {:.3} 毫秒",
        profile.elapsed.as_secs_f64() * 1_000.0
    )
}

#[cfg(test)]
#[test]
fn profile_info_line_follows_the_workspace_preference() {
    fn contains(shape: &egui::Shape, label: &str) -> bool {
        match shape {
            egui::Shape::Text(text) => text.galley.text() == label,
            egui::Shape::Vec(shapes) => shapes.iter().any(|shape| contains(shape, label)),
            _ => false,
        }
    }

    let mut workspace = Workspace::new("查询分析测试".into(), None);
    workspace.tab_mut().profile = Some(Box::new(QueryProfile {
        sql: "SELECT 1".into(),
        elapsed: std::time::Duration::from_millis(7),
        error: None,
        sections: Vec::new(),
        notices: Vec::new(),
    }));
    let expected = elapsed(workspace.tab().profile.as_ref().unwrap());
    let ctx = egui::Context::default();
    for visible in [true, false] {
        workspace.show_data_info = visible;
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| panel(ui, &mut workspace));
        });
        assert!(
            output
                .shapes
                .iter()
                .any(|shape| contains(&shape.shape, &expected))
        );
        assert_eq!(
            output
                .shapes
                .iter()
                .any(|shape| contains(&shape.shape, "SELECT 1")),
            visible
        );
    }
}

fn html(profile: &QueryProfile) -> String {
    let mut report = String::from(
        "<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\"><title>查询分析</title>\n\
         <style>table{border-collapse:collapse}th,td{border:1px solid #aaa;padding:5px 9px;text-align:left;white-space:pre-wrap}pre{white-space:pre-wrap}</style>\n\
         <h1>查询分析</h1>\n",
    );
    let _ = writeln!(report, "<p>{}</p>", elapsed(profile));
    if let Some(error) = &profile.error {
        let _ = writeln!(report, "<p>{}</p>", markup(error));
    }
    for notice in &profile.notices {
        let _ = writeln!(report, "<p>{}</p>", markup(notice));
    }
    let _ = writeln!(
        report,
        "<h2>已执行的 SQL</h2><pre>{}</pre>",
        markup(&profile.sql)
    );
    for section in &profile.sections {
        let _ = writeln!(
            report,
            "<h2>{}</h2><p>{}</p>",
            markup(section.title),
            markup(section.help)
        );
        match &section.data {
            Ok(table) => {
                if !table.summary.is_empty() {
                    let _ = writeln!(report, "<p>{}</p>", markup(&table.summary));
                }
                if table.rows.is_empty() {
                    continue;
                }
                report.push_str("<table><thead><tr>");
                for column in &table.columns {
                    let _ = write!(report, "<th scope=\"col\">{}</th>", markup(column));
                }
                report.push_str("</tr></thead><tbody>\n");
                for row in &table.rows {
                    report.push_str("<tr>");
                    for value in row {
                        let _ = write!(report, "<td>{}</td>", markup(value));
                    }
                    report.push_str("</tr>\n");
                }
                report.push_str("</tbody></table>\n");
            }
            Err(error) => {
                let _ = writeln!(report, "<p>无法采集。{}</p>", markup(error));
            }
        }
    }
    report.push_str("</html>\n");
    report
}
