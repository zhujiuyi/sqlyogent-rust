use std::collections::HashSet;

use eframe::egui::{self, RichText, TextEdit};

use crate::database::{Command, DbObject, ObjectKind};
use crate::maintenance::{CheckOption, DiagnosticOptions, TableAction};

use super::{state::Workspace, style};

enum Catalog {
    Waiting,
    Loading,
    Ready,
    Failed(String),
}

pub(super) struct Dialog {
    workspace_key: u64,
    schema: String,
    selected: HashSet<String>,
    catalog: Catalog,
    action: TableAction,
    options: DiagnosticOptions,
    confirmation: Option<String>,
}

impl Dialog {
    pub(super) fn open(
        ws: &Workspace,
        schema: Option<String>,
        table: Option<String>,
    ) -> Option<Self> {
        let server = ws.server.as_ref()?;
        let schema = schema
            .or_else(|| ws.selected.as_ref().map(|object| object.schema.clone()))
            .or_else(|| server.database.clone())
            .or_else(|| ws.schemas.first().cloned())
            .unwrap_or_default();
        Some(Self {
            workspace_key: ws.key,
            schema,
            selected: table.into_iter().collect(),
            catalog: Catalog::Waiting,
            action: TableAction::Check,
            options: DiagnosticOptions::default(),
            confirmation: None,
        })
    }

    fn update_catalog(&mut self, ws: &mut Workspace) {
        if self.schema.is_empty() || ws.server.is_none() || ws.busy() {
            return;
        }
        match self.catalog {
            Catalog::Waiting => {
                let command = Command::ListObjects {
                    schema: self.schema.clone(),
                };
                if ws.send(command, "正在读取数据表…") {
                    ws.objects.remove(&self.schema);
                    self.catalog = Catalog::Loading;
                } else {
                    self.catalog = Catalog::Failed(ws.status.clone());
                }
            }
            Catalog::Loading => {
                if let Some(objects) = ws.objects.get(&self.schema) {
                    self.selected.retain(|name| {
                        objects
                            .iter()
                            .any(|object| object.kind == ObjectKind::Table && object.name == *name)
                    });
                    self.catalog = Catalog::Ready;
                } else {
                    self.catalog = Catalog::Failed(ws.status.clone());
                }
            }
            Catalog::Ready | Catalog::Failed(_) => {}
        }
    }

    fn choose_schema(&mut self, ui: &mut egui::Ui, schemas: &[String]) {
        let before = self.schema.clone();
        ui.horizontal(|ui| {
            ui.label("数据库");
            egui::ComboBox::from_id_salt("diagnostic-database")
                .selected_text(if self.schema.is_empty() {
                    "请选择数据库"
                } else {
                    &self.schema
                })
                .show_ui(ui, |ui| {
                    for schema in schemas {
                        ui.selectable_value(&mut self.schema, schema.clone(), schema);
                    }
                });
        });
        if self.schema != before {
            self.catalog = Catalog::Waiting;
            self.selected.clear();
            ui.ctx().request_repaint();
        }
    }

    fn select_tables(&mut self, ui: &mut egui::Ui, tables: &[&DbObject], max_height: f32) {
        let count_text = format!("已选 {} / {}", self.selected.len(), tables.len());
        style::section_card_with_header(
            ui,
            "要诊断的数据表",
            |ui| {
                style::badge(ui, count_text, style::accent(), style::accent_soft());
            },
            |ui| {
                ui.horizontal(|ui| {
                    if ui.button("全选").clicked() {
                        self.selected
                            .extend(tables.iter().map(|table| table.name.clone()));
                    }
                    if ui.button("清除选择").clicked() {
                        self.selected.clear();
                    }
                });
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .id_salt(("diagnostic-tables", &self.schema))
                    .auto_shrink([false, false])
                    .max_height(max_height)
                    .show_rows(
                        ui,
                        ui.spacing().interact_size.y,
                        tables.len(),
                        |ui, rows| {
                            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                            for table in &tables[rows] {
                                let mut checked = self.selected.contains(&table.name);
                                if ui
                                    .checkbox(&mut checked, &table.name)
                                    .on_hover_text(&table.name)
                                    .changed()
                                {
                                    if checked {
                                        self.selected.insert(table.name.clone());
                                    } else {
                                        self.selected.remove(&table.name);
                                    }
                                }
                            }
                        },
                    );
            },
        );
    }

    fn choose_action(&mut self, ui: &mut egui::Ui) {
        style::section_card(ui, "诊断操作与选项", |ui| {
            ui.label(RichText::new("操作类型").small().color(style::muted()));
            ui.add_space(2.0);
            ui.horizontal_wrapped(|ui| {
                for action in TableAction::DIAGNOSTICS {
                    ui.radio_value(&mut self.action, action, action.label());
                }
            });
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(RichText::new("执行选项").small().color(style::muted()));
            ui.add_space(2.0);
            match self.action {
                TableAction::Check => {
                    for option in CheckOption::ALL {
                        let mut checked = self.options.checks.contains(&option);
                        if ui.checkbox(&mut checked, option.label()).changed() {
                            if checked {
                                self.options.checks.push(option);
                            } else {
                                self.options.checks.retain(|value| *value != option);
                            }
                        }
                    }
                }
                TableAction::Analyze => local_option(ui, &mut self.options.analyze_local),
                TableAction::Optimize => local_option(ui, &mut self.options.optimize_local),
                TableAction::Repair => {
                    ui.checkbox(&mut self.options.quick_repair, "仅修复索引（QUICK）");
                    ui.checkbox(
                        &mut self.options.extended_repair,
                        "逐行重建索引（EXTENDED）",
                    );
                    ui.checkbox(&mut self.options.use_frm, "按表定义重建索引（USE_FRM）");
                    local_option(ui, &mut self.options.repair_local);
                }
                TableAction::Truncate | TableAction::Drop => unreachable!(),
            }
            ui.add_space(6.0);
            ui.label(
                RichText::new("执行期间可能暂时锁定所选表。")
                    .small()
                    .color(style::muted()),
            );
        });
    }
}

fn local_option(ui: &mut egui::Ui, checked: &mut bool) {
    ui.checkbox(checked, "不写入二进制日志（LOCAL）");
}

enum Action {
    Close,
    OpenSql(String),
    Run(String),
}

pub(super) enum Outcome {
    Editor,
    Results,
}

pub(super) fn dialog(
    ctx: &egui::Context,
    state: &mut Option<Dialog>,
    workspaces: &mut [Workspace],
) -> Option<Outcome> {
    let form = state.as_mut()?;
    let Some(ws) = workspaces
        .iter_mut()
        .find(|ws| ws.key == form.workspace_key)
    else {
        *state = None;
        return None;
    };
    form.update_catalog(ws);
    let size = ctx.content_rect().size();
    let response =
        crate::app::style::modal(ctx, egui::Id::new("table-diagnostics")).show(ctx, |ui| {
            let width = (size.x - 64.0).clamp(400.0, 700.0);
            ui.set_width(width);
            ui.horizontal(|ui| {
                ui.heading("表诊断");
                ui.add_space(6.0);
                style::badge(ui, &ws.label, style::muted(), style::chrome());
            });
            ui.add_space(6.0);
            if ws.server.is_none() {
                style::banner(ui, "连接已断开。", style::error(), style::error_soft());
                return style::dialog_footer(ui, |ui| {
                    ui.button("关闭").clicked().then_some(Action::Close)
                });
            }
            if let Some(sql) = &form.confirmation {
                let warn = match form.action {
                    TableAction::Optimize => "优化可能重建表并暂时阻塞写入。",
                    TableAction::Repair => "修复可能造成数据丢失。请先备份所选表。",
                    _ => unreachable!(),
                };
                style::banner(ui, warn, style::error(), style::error_soft());
                if form.action == TableAction::Repair && form.options.use_frm {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("仅在索引文件缺失或文件头损坏时使用 USE_FRM。")
                            .small()
                            .color(style::muted()),
                    );
                }
                ui.add_space(8.0);
                show_sql(ui, sql);
                return style::dialog_footer(ui, |ui| {
                    let can_run = !ws.busy();
                    if ui
                        .add_enabled(
                            can_run,
                            style::primary_enabled(can_run, &format!("确认{}", form.action.label())),
                        )
                        .clicked()
                    {
                        form.confirmation.take().map(Action::Run)
                    } else if ui.button("返回选择").clicked() {
                        form.confirmation = None;
                        None
                    } else {
                        None
                    }
                });
            }
            style::subtle_card(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                form.choose_schema(ui, &ws.schemas);
            });
            ui.add_space(10.0);
            let mut sql = None;
            match &form.catalog {
                Catalog::Ready => {
                    let tables: Vec<_> = ws
                        .objects
                        .get(&form.schema)
                        .into_iter()
                        .flatten()
                        .filter(|object| object.kind == ObjectKind::Table)
                        .collect();
                    if tables.is_empty() {
                        style::subtle_card(egui::Margin::same(14)).show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.label(
                                RichText::new("此数据库没有可选的数据表。").color(style::muted()),
                            );
                        });
                    } else {
                        let list_h = (size.y * 0.30).clamp(130.0, 240.0);
                        if width >= 540.0 {
                            style::split_two_columns_with(
                                ui,
                                0.52,
                                12.0,
                                form,
                                |ui, form| form.select_tables(ui, &tables, list_h),
                                |ui, form| form.choose_action(ui),
                            );
                        } else {
                            form.select_tables(ui, &tables, list_h);
                            ui.add_space(8.0);
                            form.choose_action(ui);
                        }
                        sql = form.options.sql(
                            form.action,
                            &form.schema,
                            tables
                                .into_iter()
                                .filter(|table| form.selected.contains(&table.name))
                                .map(|table| table.name.as_str()),
                        );
                        if let Some(sql) = &sql {
                            ui.add_space(8.0);
                            show_sql(ui, sql);
                        }
                    }
                }
                Catalog::Failed(error) => {
                    style::banner(ui, error, style::error(), style::error_soft());
                    ui.add_space(6.0);
                    if ui.button("重新读取").clicked() {
                        form.catalog = Catalog::Waiting;
                        ctx.request_repaint();
                    }
                }
                Catalog::Waiting if form.schema.is_empty() => {
                    ui.label(RichText::new("请选择数据库。").color(style::muted()));
                }
                Catalog::Waiting | Catalog::Loading => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(if matches!(form.catalog, Catalog::Waiting) {
                            "等待当前连接任务完成…"
                        } else {
                            "正在读取数据表…"
                        });
                    });
                }
            }
            let can_run = sql.is_some() && !ws.busy();
            style::dialog_footer(ui, |ui| {
                if ui
                    .add_enabled(can_run, style::primary_enabled(can_run, form.action.label()))
                    .clicked()
                {
                    if matches!(form.action, TableAction::Optimize | TableAction::Repair) {
                        form.confirmation = sql;
                        return None;
                    }
                    return sql.map(Action::Run);
                }
                if ui
                    .add_enabled(sql.is_some(), egui::Button::new("在编辑器中打开"))
                    .clicked()
                {
                    return sql.map(Action::OpenSql);
                }
                if ui.button("取消").clicked() {
                    return Some(Action::Close);
                }
                None
            })
        });
    match response.inner {
        Some(Action::Run(sql)) => {
            let started = ws.start_table_diagnostics(sql);
            *state = None;
            started.then_some(Outcome::Results)
        }
        Some(Action::OpenSql(sql)) => {
            ws.new_tab(sql, None);
            *state = None;
            Some(Outcome::Editor)
        }
        Some(Action::Close) => {
            *state = None;
            None
        }
        None => {
            if response.should_close() {
                *state = None;
            }
            None
        }
    }
}

fn show_sql(ui: &mut egui::Ui, mut sql: &str) {
    egui::ScrollArea::vertical()
        .id_salt("diagnostic-sql")
        .max_height(84.0)
        .show(ui, |ui| {
            ui.add(
                TextEdit::multiline(&mut sql)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY)
                    .desired_rows(2),
            );
        });
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use crate::app::state::Output;
    use crate::database::{Cell, ConnectionConfig};

    use super::*;

    #[test]
    #[ignore = "在一次性本机 MariaDB 中运行，需要 HARBOR_DIAGNOSTIC_TEST_PORT"]
    fn diagnostics_report_both_tables_and_preserve_the_editor_transaction() {
        let port = std::env::var("HARBOR_DIAGNOSTIC_TEST_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let schema = format!("harbor_diagnostics_{stamp:x}");
        let mut ws = Workspace::new("本机测试".into(), None);
        ws.connect(
            ConnectionConfig {
                port,
                ..Default::default()
            },
            None,
        );
        wait_until_idle(&mut ws);
        assert!(ws.server.is_some(), "{}", ws.status);
        let tables = ["active", "old`rows"];
        ws.start_batch(vec![
            format!("CREATE DATABASE `{schema}`"),
            format!("CREATE TABLE `{schema}`.`active` (id INT PRIMARY KEY) ENGINE=MyISAM"),
            format!("CREATE TABLE `{schema}`.`old``rows` (id INT PRIMARY KEY) ENGINE=MyISAM"),
            format!("CREATE TABLE `{schema}`.`editor_changes` (id INT PRIMARY KEY) ENGINE=InnoDB"),
            format!("INSERT INTO `{schema}`.`active` VALUES (1)"),
            format!("INSERT INTO `{schema}`.`old``rows` VALUES (2)"),
            "START TRANSACTION".into(),
            format!("INSERT INTO `{schema}`.`editor_changes` VALUES (3)"),
        ]);
        wait_until_idle(&mut ws);
        let setup = ws.tab().report.as_ref().unwrap();
        assert_eq!(setup.error, None);
        let mut dialog = Dialog::open(&ws, Some(schema.clone()), Some(tables[0].into())).unwrap();
        dialog.update_catalog(&mut ws);
        wait_until_idle(&mut ws);
        dialog.update_catalog(&mut ws);
        assert!(matches!(dialog.catalog, Catalog::Ready), "{}", ws.status);
        assert_eq!(dialog.selected, HashSet::from([tables[0].into()]));
        let options = DiagnosticOptions {
            checks: vec![CheckOption::Quick],
            analyze_local: true,
            optimize_local: true,
            repair_local: true,
            quick_repair: true,
            ..Default::default()
        };
        for action in TableAction::DIAGNOSTICS {
            let sql = options.sql(action, &schema, tables).unwrap();
            assert!(ws.start_table_diagnostics(sql));
            wait_until_idle(&mut ws);
            assert!(matches!(ws.output, Output::Results), "{}", ws.status);
            let report = ws.tab().report.as_ref().unwrap();
            assert_eq!(report.error, None);
            assert_eq!(
                report.sets[0].columns,
                ["Table", "Op", "Msg_type", "Msg_text"]
            );
            for name in tables {
                assert!(
                    report.sets[0].rows.iter().any(|row| {
                        row[0] == Cell::Text(format!("{schema}.{name}"))
                            && row[2] == Cell::Text("status".into())
                    }),
                    "{:?}",
                    report.sets[0].rows
                );
            }
        }
        ws.start_batch(vec![
            "ROLLBACK".into(),
            format!("SELECT COUNT(*) FROM `{schema}`.`editor_changes`"),
            format!("DROP DATABASE `{schema}`"),
        ]);
        wait_until_idle(&mut ws);
        let report = ws.tab().report.as_ref().unwrap();
        assert_eq!(report.error, None);
        assert_eq!(report.sets[1].rows, [vec![Cell::Text("0".into())]]);
    }

    fn wait_until_idle(ws: &mut Workspace) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while ws.busy() && Instant::now() < deadline {
            ws.poll();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!ws.busy(), "{}", ws.status);
    }
}
