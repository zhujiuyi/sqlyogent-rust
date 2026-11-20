use std::path::{Path, PathBuf};

use eframe::egui;
use harbor_sql::sql_encoding::SqlDumpCharset;
use rfd::FileDialog;

use super::{form_scroll, form_section, form_width, right_aligned_row, transfer_progress};
use crate::app::{
    state::{RestoreCompletion, RestoreOutcome, Workspace, restore_error_summary, write_atomic},
    style,
};

const PARTIAL_WARNING: &str = "已提交的更改可能保留。重新执行前请核对目标数据库。";
const AUTO_CHARSET: &str = "按文件标记读取（默认 UTF-8）";

enum Run {
    Ready,
    Running,
    Finished(RestoreCompletion),
}

pub(super) struct RestoreForm {
    schema: String,
    file: Option<PathBuf>,
    file_charset: Option<SqlDumpCharset>,
    run: Run,
    report_open: bool,
    report_feedback: Option<Result<PathBuf, String>>,
}

impl RestoreForm {
    pub(super) fn new(schema: String) -> Self {
        Self {
            schema,
            file: None,
            file_charset: None,
            run: Run::Ready,
            report_open: false,
            report_feedback: None,
        }
    }

    fn select_file(&mut self, path: PathBuf) {
        if self.file.as_ref() != Some(&path) {
            self.file_charset = None;
        }
        self.file = Some(path);
        self.run = Run::Ready;
        self.report_feedback = None;
    }

    fn start(&mut self, ws: &mut Workspace) {
        let Some(path) = self.file.clone() else {
            return;
        };
        self.report_feedback = None;
        let schema = self.schema.clone();
        let started = match self.file_charset {
            None => ws.start_desktop_restore(schema, path),
            Some(charset) => ws.start_desktop_restore_encoded(schema, path, charset),
        };
        self.run = if started {
            Run::Running
        } else {
            Run::Finished(RestoreCompletion {
                outcome: RestoreOutcome::Failed(restore_error_summary(&ws.status)),
                progress: None,
            })
        };
    }

    fn refresh(&mut self, ws: &mut Workspace) {
        if matches!(self.run, Run::Running)
            && let Some(result) = ws.restore_result.take()
        {
            self.run = Run::Finished(result);
        }
    }

    fn report(&self) -> Option<String> {
        let Run::Finished(RestoreCompletion {
            outcome: RestoreOutcome::Failed(detail),
            ..
        }) = &self.run
        else {
            return None;
        };
        Some(format!(
            "SQL 还原诊断报告\n结果：失败\n{detail}\n\n{PARTIAL_WARNING}\n"
        ))
    }

    fn save_report(&mut self) {
        let (Some(report), Some(source)) = (self.report(), self.file.as_deref()) else {
            return;
        };
        if let Some(path) = FileDialog::new()
            .set_title("保存 SQL 还原诊断报告")
            .set_file_name("SQL 还原诊断报告.txt")
            .add_filter("UTF-8 文本", &["txt"])
            .save_file()
        {
            self.report_feedback = Some(write_report(source, &path, &report).map(|()| path));
        }
    }

    fn show_report(&mut self, ctx: &egui::Context) {
        let Some(report) = self.report() else { return };
        let modal = style::modal(ctx, egui::Id::new("restore-diagnostic-report")).show(ctx, |ui| {
            ui.set_width(form_width(ctx, 560.0));
            style::page_title(ui, "SQL 还原诊断报告");
            let mut text = report.as_str();
            form_scroll(ctx, 116.0).show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut text)
                        .desired_width(f32::INFINITY)
                        .desired_rows(6),
                );
            });
            right_aligned_row(ui, |ui| ui.add(style::primary("关闭报告")).clicked())
        });
        if modal.inner || modal.should_close() {
            self.report_open = false;
        }
    }

    fn show_progress(&self, ui: &mut egui::Ui, ws: &Workspace) {
        let (progress, status, color, complete) = match &self.run {
            Run::Ready => {
                let status = if ws.busy() {
                    "当前连接正在执行其他任务，请稍候。"
                } else {
                    "等待执行"
                };
                (None, status.into(), style::muted(), false)
            }
            Run::Running => (
                ws.restore_progress,
                ws.restore_status()
                    .unwrap_or_else(|| "正在等待还原结果…".into()),
                style::ink(),
                false,
            ),
            Run::Finished(result) => {
                let (status, color, complete) = match &result.outcome {
                    RestoreOutcome::Completed(count) => (
                        format!("还原完成，共执行 {count} 条语句。"),
                        style::success(),
                        true,
                    ),
                    RestoreOutcome::Cancelled => ("已停止还原。".into(), style::muted(), false),
                    RestoreOutcome::Failed(detail) => {
                        (format!("还原失败。{detail}"), style::error(), false)
                    }
                };
                (result.progress, status, color, complete)
            }
        };
        let fraction = if complete {
            1.0
        } else {
            progress
                .filter(|item| item.total_bytes > 0)
                .map_or(0.0, |item| {
                    (item.bytes as f64 / item.total_bytes as f64).clamp(0.0, 1.0) as f32
                })
        };
        transfer_progress(ui, &status, fraction, color);
    }

    pub(super) fn dialog(&mut self, ctx: &egui::Context, ws: &mut Workspace) -> bool {
        self.refresh(ws);
        let running = matches!(self.run, Run::Running);
        let finished = matches!(self.run, Run::Finished(_));
        let completed = matches!(
            self.run,
            Run::Finished(RestoreCompletion {
                outcome: RestoreOutcome::Completed(_),
                ..
            })
        );
        let failed = matches!(
            self.run,
            Run::Finished(RestoreCompletion {
                outcome: RestoreOutcome::Failed(_),
                ..
            })
        );
        let mut start = false;
        let mut close = false;
        let mut save_report = false;
        let modal = style::modal(ctx, egui::Id::new("restore-form")).show(ctx, |ui| {
            ui.set_width(form_width(ctx, 600.0));
            style::page_title(ui, "从 SQL 转储文件还原");
            ui.add_space(4.0);
            form_scroll(ctx, 150.0).show(ui, |ui| {
                form_section(ui, "还原目标", |ui| {
                    if self.schema.is_empty() {
                        ui.label("未选择起始数据库。脚本需指定数据库。");
                    } else {
                        ui.label(format!("起始数据库 · {}", self.schema));
                    }
                    ui.colored_label(style::muted(), "脚本中的 USE 或带库名的对象可以指定其他数据库。");
                    ui.colored_label(style::warning(), "还原会在当前服务器执行脚本，已提交的操作无法整体撤销。");
                    if ws.server.as_ref().is_some_and(|server| server.http_tunnel) {
                        ui.colored_label(style::muted(), "HTTP 隧道按文件提交批次，编码后的单次请求上限为 64 MiB。");
                    }
                });
                form_section(ui, "还原来源", |ui| {
                    ui.add_enabled_ui(!running && !completed, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            if ui.button("选择文件…").clicked()
                                && let Some(path) = FileDialog::new()
                                    .set_title("选择要还原的 SQL 转储文件")
                                    .add_filter("SQL 转储文件", &["sql"])
                                    .pick_file()
                            {
                                self.select_file(path);
                            }
                            if ui.button("选择备份目录…").clicked()
                                && let Some(path) = FileDialog::new().set_title("选择备份目录").pick_folder()
                            {
                                self.select_file(path);
                            }
                        });
                    });
                    if let Some(file) = &self.file {
                        let path = file.display().to_string();
                        ui.add(egui::Label::new(&path).truncate().selectable(true)).on_hover_text(path);
                    } else {
                        ui.colored_label(style::muted(), "请选择 SQL 文件或分对象备份目录。");
                    }
                    ui.add_enabled_ui(!running && !completed && self.file.is_some(), |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label("SQL 文件字符集");
                            egui::ComboBox::from_id_salt("restore-file-charset")
                                .selected_text(self.file_charset.map_or(AUTO_CHARSET, SqlDumpCharset::label))
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut self.file_charset, None, AUTO_CHARSET);
                                    for charset in SqlDumpCharset::ALL {
                                        ui.selectable_value(&mut self.file_charset, Some(charset), charset.label());
                                    }
                                })
                                .response
                                .on_hover_text("手动选择时，备份文件里的编码标记须与之相同。选目录时，所有文件共用此选择。");
                        });
                    });
                });
                if finished && !completed {
                    ui.colored_label(style::warning(), PARTIAL_WARNING);
                }
                if failed {
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("打开诊断报告").clicked() {
                            self.report_open = true;
                        }
                        save_report = ui.button("保存诊断报告…").clicked();
                    });
                    if let Some(feedback) = &self.report_feedback {
                        match feedback {
                            Ok(path) => ui.colored_label(style::success(), format!("报告已保存到 {}", path.display())),
                            Err(error) => ui.colored_label(style::error(), error),
                        };
                    }
                }
            });
            style::pane(egui::Margin::symmetric(12, 8))
                .corner_radius(egui::CornerRadius::same(10))
                .show(ui, |ui| {
                    self.show_progress(ui, ws);
                    right_aligned_row(ui, |ui| {
                        if running {
                            if ui
                                .add_enabled(ws.can_stop_restore(), egui::Button::new("停止还原"))
                                .on_hover_text("停止提交后续 SQL。已提交的更改可能保留。")
                                .clicked()
                            {
                                ws.stop_restore();
                            }
                        } else if completed {
                            close = ui.add(style::primary("完成")).clicked();
                        } else {
                            let valid = ws.server.is_some() && !ws.busy() && self.file.is_some();
                            let label = if finished { "重新执行" } else { "执行还原" };
                            start = ui.add_enabled(valid, style::primary(label)).clicked();
                            close = ui.button("关闭").clicked();
                        }
                    });
                });
        });
        if save_report {
            self.save_report();
        }
        if self.report_open {
            self.show_report(ctx);
        } else if close || (!running && modal.should_close()) {
            return true;
        } else if start {
            self.start(ws);
        }
        false
    }
}

fn write_report(source: &Path, destination: &Path, report: &str) -> Result<(), String> {
    let resolved = source
        .canonicalize()
        .ok()
        .zip(destination.canonicalize().ok());
    if source == destination
        || resolved.is_some_and(|(source, destination)| {
            source == destination || (source.is_dir() && destination.starts_with(source))
        })
    {
        return Err("请选择单独的报告文件，保留原 SQL 转储或备份目录中的文件。".into());
    }
    write_atomic(destination, report.as_bytes())
        .map_err(|error| format!("无法保存诊断报告。{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::ConnectionConfig;
    use std::time::{Duration, Instant};

    fn fixture() -> tempfile::TempDir {
        let executable = std::env::current_exe().unwrap();
        tempfile::tempdir_in(executable.parent().unwrap()).unwrap()
    }

    #[test]
    fn restore_report_copies_only_safe_failure_details_to_utf8() {
        let folder = fixture();
        let path = folder.path().join("待还原.sql");
        let sql = "SELECT 'synthetic_sql_secret';";
        std::fs::write(&path, sql).unwrap();
        let error = "还原在第 8 条语句处停止（「raw_filename.sql」字节位置 146）。之前已执行的更改可能已经提交。MySqlError { ERROR 1064 (42000): near 'synthetic_sql_secret' }";
        let mut form = RestoreForm::new("测试".into());
        form.select_file(path.clone());
        form.run = Run::Running;
        let mut ws = Workspace::new("测试".into(), None);
        ws.restore_result = Some(RestoreCompletion {
            outcome: RestoreOutcome::Failed(restore_error_summary(error)),
            progress: None,
        });
        form.refresh(&mut ws);
        assert_eq!(form.file.as_deref(), Some(path.as_path()));
        let report = form.report().unwrap();
        let saved_report = folder.path().join("诊断报告.txt");
        write_report(&path, &saved_report, &report).unwrap();
        let saved = std::fs::read_to_string(saved_report).unwrap();
        assert!(saved.contains("第 8 条语句，字节位置 146"));
        assert!(saved.contains("MySQL 错误代码 1064"));
        assert!(!saved.contains("synthetic_sql_secret"));
        assert!(!saved.contains("raw_filename.sql"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), sql);

        let preflight = restore_error_summary(
            "HTTP 还原在「00000003-raw_filename.sql」处停止。已完成 4 条语句。预检在文件内第 1 条语句处停止（字节位置 77）。synthetic_sql_secret",
        );
        assert!(preflight.contains("备份文件编号 3"));
        assert!(preflight.contains("已确认完成 4 条语句"));
        assert!(preflight.contains("第 1 条语句停止，字节位置 77"));
        assert!(
            !preflight.contains("synthetic_sql_secret") && !preflight.contains("raw_filename.sql")
        );

        let normalized = restore_error_summary(
            "还原在第 8 条语句处停止（「raw_filename.sql」转码后的 UTF-8 字节位置 146）。之前已执行的更改可能已经提交。ERROR 1064 (42000): synthetic_sql_secret",
        );
        assert!(normalized.contains("第 8 条语句，转码后的 UTF-8 字节位置 146"));
        assert!(normalized.contains("MySQL 错误代码 1064"));
        assert!(!normalized.contains("synthetic_sql_secret"));
    }

    fn wait_until_idle(ws: &mut Workspace) {
        let started = Instant::now();
        loop {
            ws.poll();
            if !ws.busy() {
                return;
            }
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    #[ignore = "需要 HARBOR_RESTORE_DIALOG_FIXTURE_PORT 指定一次性 MariaDB"]
    fn restore_dialog_uses_selected_charset_for_an_unmarked_sql_file() {
        let folder = fixture();
        let path = folder.path().join("旧备份.sql");
        let next_path = folder.path().join("新备份.sql");
        let charset = SqlDumpCharset::Gbk;
        let mut encoded = Vec::new();
        harbor_sql::sql_encoding::encode_sql(charset, "SELECT '港湾';".as_bytes(), &mut encoded)
            .unwrap();
        std::fs::write(&path, encoded).unwrap();
        std::fs::write(&next_path, "SELECT '港湾';").unwrap();

        let mut form = RestoreForm::new(String::new());
        form.select_file(path.clone());
        assert_eq!(form.file_charset, None);
        form.file_charset = Some(charset);
        form.select_file(path);
        assert_eq!(form.file_charset, Some(charset));

        let port = std::env::var("HARBOR_RESTORE_DIALOG_FIXTURE_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let mut ws = Workspace::new("测试".into(), None);
        ws.connect(
            ConnectionConfig {
                port,
                ..Default::default()
            },
            None,
        );
        wait_until_idle(&mut ws);
        assert!(ws.server.is_some());
        form.start(&mut ws);
        wait_until_idle(&mut ws);
        form.refresh(&mut ws);
        assert!(matches!(
            form.run,
            Run::Finished(RestoreCompletion {
                outcome: RestoreOutcome::Completed(1),
                ..
            })
        ));
        assert_eq!(form.file_charset, Some(charset));
        form.select_file(next_path);
        assert_eq!(form.file_charset, None);
    }

    #[test]
    #[ignore = "需要 HARBOR_RESTORE_DIALOG_FIXTURE_PORT 指定一次性 MariaDB"]
    fn restore_dialog_keeps_source_across_stop_failure_and_retry() {
        let folder = fixture();
        let path = folder.path().join("测试还原.sql");
        let port = std::env::var("HARBOR_RESTORE_DIALOG_FIXTURE_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let mut ws = Workspace::new("测试".into(), None);
        ws.connect(
            ConnectionConfig {
                port,
                ..Default::default()
            },
            None,
        );
        wait_until_idle(&mut ws);
        assert!(ws.server.is_some());
        ws.transfer_result = Some(Ok(()));
        let mut form = RestoreForm::new(String::new());
        form.select_file(path.clone());

        std::fs::write(&path, "SELECT SLEEP(0.2); SELECT 1;").unwrap();
        form.start(&mut ws);
        assert!(matches!(form.run, Run::Running));
        assert!(ws.can_stop_restore());
        ws.stop_restore();
        wait_until_idle(&mut ws);
        form.refresh(&mut ws);
        assert!(matches!(
            form.run,
            Run::Finished(RestoreCompletion {
                outcome: RestoreOutcome::Cancelled,
                ..
            })
        ));
        assert_eq!(form.file.as_deref(), Some(path.as_path()));

        std::fs::write(
            &path,
            "SELECT 1;\nSIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'synthetic_sql_secret';",
        )
        .unwrap();
        form.start(&mut ws);
        wait_until_idle(&mut ws);
        form.refresh(&mut ws);
        let report = form.report().unwrap();
        assert!(report.contains("第 2 条语句，字节位置"));
        assert!(report.contains("MySQL 错误代码 1644"));
        assert!(!report.contains("synthetic_sql_secret"));
        assert!(
            ws.messages
                .last()
                .is_some_and(|message| !message.contains("synthetic_sql_secret"))
        );

        std::fs::write(&path, "SELECT 2;\nSELECT 3;").unwrap();
        form.start(&mut ws);
        wait_until_idle(&mut ws);
        form.refresh(&mut ws);
        assert!(matches!(
            form.run,
            Run::Finished(RestoreCompletion {
                outcome: RestoreOutcome::Completed(2),
                ..
            })
        ));
        assert_eq!(form.file, Some(path));
        assert!(matches!(ws.transfer_result, Some(Ok(()))));
    }
}
