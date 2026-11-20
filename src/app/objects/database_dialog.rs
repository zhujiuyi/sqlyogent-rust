use std::collections::HashSet;

use eframe::egui::{self, RichText, TextEdit};

use crate::database::{
    Command, DatabaseContentsAction, DatabaseContentsPlan, DatabaseDefaults, DbObject,
};

use super::{Plan, kind_label, style, text};
use crate::app::state::Workspace;

pub(super) struct ContentsDialog {
    pub(super) schema: String,
    pub(super) action: DatabaseContentsAction,
    selection: HashSet<DbObject>,
    filter: String,
    initialized: bool,
    reconcile_selection: bool,
    confirmed_plan: Option<DatabaseContentsPlan>,
    applying: bool,
}

impl ContentsDialog {
    pub(super) fn open(ws: &mut Workspace, schema: String, action: DatabaseContentsAction) -> Self {
        let dialog = Self {
            schema,
            action,
            selection: HashSet::new(),
            filter: String::new(),
            initialized: false,
            reconcile_selection: false,
            confirmed_plan: None,
            applying: false,
        };
        dialog.load(ws);
        dialog
    }

    fn load(&self, ws: &mut Workspace) {
        ws.database_contents = None;
        ws.database_contents_result = None;
        ws.send(
            Command::ReadDatabaseContents {
                schema: self.schema.clone(),
                action: self.action,
            },
            "正在读取数据库对象和相关外键…",
        );
    }

    pub(super) fn show(&mut self, ctx: &egui::Context, ws: &mut Workspace) -> bool {
        let screen = ctx.content_rect();
        let mut submit = false;
        let mut reload = false;
        let mut close = false;
        let modal =
            crate::app::style::modal(ctx, egui::Id::new("database-contents")).show(ctx, |ui| {
                ui.set_width((screen.width() - 64.0).clamp(400.0, 620.0));
                let (title, effect, confirm) = match self.action {
                    DatabaseContentsAction::Drop => (
                        "清空数据库",
                        "删除所选对象及其数据，保留数据库。",
                        "确认删除所选对象",
                    ),
                    DatabaseContentsAction::Truncate => (
                        "截断数据库中的表",
                        "删除所选表中的全部数据并重置自增计数，保留表结构。",
                        "确认截断所选表",
                    ),
                };
                ui.horizontal(|ui| {
                    ui.heading(title);
                    ui.add_space(6.0);
                    style::badge(ui, &self.schema, style::error(), style::error_soft());
                });
                ui.label(egui::RichText::new(effect).small().color(style::muted()));
                ui.add_space(8.0);
                if self.applying {
                    if ws.busy() {
                        style::subtle_card(egui::Margin::same(14)).show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("正在依次处理所选对象…");
                            });
                        });
                        return;
                    }
                    if let Some(result) = &ws.database_contents_result {
                        style::banner(
                            ui,
                            &format!("已处理 {} / {} 个对象。", result.completed, result.total),
                            if result.error.is_some() {
                                style::warning()
                            } else {
                                style::success()
                            },
                            if result.error.is_some() {
                                style::panel()
                            } else {
                                style::success_soft()
                            },
                        );
                        if let Some(error) = &result.error {
                            ui.add_space(6.0);
                            style::banner(ui, error, style::error(), style::error_soft());
                        }
                    } else {
                        style::banner(ui, &ws.status, style::error(), style::error_soft());
                    }
                    style::dialog_footer(ui, |ui| {
                        close = ui.add(style::primary("关闭")).clicked();
                        if ws
                            .database_contents_result
                            .as_ref()
                            .is_none_or(|r| r.error.is_some())
                        {
                            reload = ui.button("重新读取对象").clicked();
                        }
                    });
                    return;
                }
                if let Some(plan) = &self.confirmed_plan {
                    style::banner(
                        ui,
                        &format!("待处理 {} 个对象。此操作无法撤销。", plan.object_count()),
                        style::error(),
                        style::error_soft(),
                    );
                    if plan.suspend_foreign_key_checks {
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(
                                "请暂停所选表的写入。执行时会在单独的连接内临时关闭外键检查。",
                            )
                            .small()
                            .color(style::muted()),
                        );
                    }
                    if self.action == DatabaseContentsAction::Drop {
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(
                                "删除表会一并删除它的触发器，也可能影响引用所选对象的视图或存储程序。",
                            )
                            .small()
                            .color(style::muted()),
                        );
                    }
                    ui.add_space(8.0);
                    style::section_card(ui, "将要执行的 SQL", |ui| {
                        egui::ScrollArea::both()
                            .id_salt("contents-confirmation")
                            .auto_shrink([false, false])
                            .max_height((screen.height() * 0.44).min(300.0))
                            .show(ui, |ui| {
                                for statement in plan.statements() {
                                    ui.monospace(statement);
                                }
                            });
                    });
                    style::dialog_footer(ui, |ui| {
                        let ready = !ws.busy() && ws.server.is_some();
                        submit = ui.add_enabled(ready, style::danger(confirm)).clicked();
                        if ui.button("返回选择").clicked() {
                            self.confirmed_plan = None;
                        }
                    });
                    return;
                }
                let mut candidate_plan = None;
                if ws.busy() {
                    style::subtle_card(egui::Margin::same(14)).show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("正在读取可处理的对象…");
                        });
                    });
                } else if let Some(contents) = ws
                    .database_contents
                    .as_ref()
                    .filter(|contents| contents.schema == self.schema)
                {
                    let objects = contents
                        .objects
                        .iter()
                        .filter(|object| self.action.includes(object.kind))
                        .collect::<Vec<_>>();
                    if !self.initialized {
                        if self.action == DatabaseContentsAction::Truncate {
                            self.selection
                                .extend(objects.iter().map(|&object| object.clone()));
                        }
                        self.initialized = true;
                    }
                    if std::mem::take(&mut self.reconcile_selection) {
                        let available = objects.iter().copied().collect::<HashSet<_>>();
                        self.selection.retain(|object| available.contains(object));
                    }
                    if objects.is_empty() {
                        style::subtle_card(egui::Margin::same(14)).show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.label(
                                egui::RichText::new("没有可处理的对象。").color(style::muted()),
                            );
                        });
                    } else {
                        self.pick_objects(ui, &objects, screen.height());
                        let candidate =
                            contents
                                .plan(self.action, &self.selection)
                                .and_then(|plan| {
                                    if plan.suspend_foreign_key_checks
                                        && ws
                                            .server
                                            .as_ref()
                                            .is_some_and(|server| server.http_tunnel)
                                    {
                                        Err("所选表互相引用。请使用直连或 SSH 隧道截断。".into())
                                    } else {
                                        Ok(plan)
                                    }
                                });
                        if !self.selection.is_empty()
                            && let Err(error) = &candidate
                        {
                            ui.add_space(6.0);
                            style::banner(ui, error, style::error(), style::error_soft());
                        }
                        candidate_plan = candidate.ok();
                    }
                } else {
                    style::banner(ui, &ws.status, style::error(), style::error_soft());
                    ui.add_space(6.0);
                    reload = ui
                        .add_enabled(ws.server.is_some(), egui::Button::new("重新读取对象"))
                        .clicked();
                }
                style::dialog_footer(ui, |ui| {
                    let ready = candidate_plan.is_some();
                    if ui
                        .add_enabled(ready, style::primary_enabled(ready, "检查操作范围"))
                        .clicked()
                    {
                        self.confirmed_plan = candidate_plan;
                    }
                    close = ui.button("关闭").clicked();
                });
            });

        if reload {
            self.confirmed_plan = None;
            self.applying = false;
            self.reconcile_selection = true;
            self.load(ws);
        } else if submit && let Some(plan) = self.confirmed_plan.take() {
            ws.database_contents_result = None;
            self.applying = true;
            ws.send(
                Command::ApplyDatabaseContents { plan },
                "正在处理所选的数据库对象…",
            );
        }
        let dismissed = modal.should_close();
        close || (dismissed && (!self.applying || !ws.busy()))
    }

    fn pick_objects(&mut self, ui: &mut egui::Ui, objects: &[&DbObject], height: f32) {
        let count_badge = format!("已选 {} / {}", self.selection.len(), objects.len());
        style::section_card_with_header(
            ui,
            "要处理的对象（当前账号可见）",
            |ui| {
                style::badge(ui, count_badge, style::accent(), style::accent_soft());
            },
            |ui| {
                ui.horizontal(|ui| {
                    ui.add(
                        TextEdit::singleline(&mut self.filter)
                            .desired_width((ui.available_width() - 190.0).max(150.0))
                            .hint_text("按对象名称筛选"),
                    );
                    if ui
                        .button(format!("选中全部 {} 项", objects.len()))
                        .clicked()
                    {
                        self.selection
                            .extend(objects.iter().map(|&object| object.clone()));
                    }
                    if ui.button("清除选择").clicked() {
                        self.selection.clear();
                    }
                });
                ui.add_space(6.0);
                let filter = self.filter.trim().to_lowercase();
                let filtered = objects
                    .iter()
                    .copied()
                    .filter(|object| {
                        filter.is_empty() || object.name.to_lowercase().contains(&filter)
                    })
                    .collect::<Vec<_>>();
                if filtered.is_empty() {
                    ui.label(egui::RichText::new("没有匹配的对象。").color(style::muted()));
                    return;
                }
                egui::ScrollArea::vertical()
                    .id_salt("contents-objects")
                    .auto_shrink([false, false])
                    .max_height((height * 0.40).min(280.0))
                    .show_rows(
                        ui,
                        ui.spacing().interact_size.y,
                        filtered.len(),
                        |ui, rows| {
                            for index in rows {
                                let object = filtered[index];
                                let label =
                                    format!("{}  ·  {}", kind_label(object.kind), object.name);
                                let mut selected = self.selection.contains(object);
                                let changed = ui
                                    .push_id(object, |ui| ui.checkbox(&mut selected, label))
                                    .inner
                                    .changed();
                                if changed {
                                    if selected {
                                        self.selection.insert(object.clone());
                                    } else {
                                        self.selection.remove(object);
                                    }
                                }
                            }
                        },
                    );
            },
        );
    }
}

pub(super) fn default_fields(ui: &mut egui::Ui, form: &mut Plan, ws: &Workspace, alter: bool) {
    if !alter {
        text(ui, "库名", &mut form.name, "");
    }
    let Some(defaults) = &ws.database_defaults else {
        return;
    };
    let scope = if alter {
        "当前默认设置"
    } else {
        "服务器默认设置"
    };
    ui.label(
        RichText::new(format!(
            "{scope}  {} / {}",
            defaults.charset, defaults.collation
        ))
        .color(style::muted()),
    );

    let previous = form.second.clone();
    ui.horizontal(|ui| {
        ui.label("默认字符集");
        let inherited = if alter {
            "保留当前字符集"
        } else {
            "使用服务器默认字符集"
        };
        egui::ComboBox::from_id_salt("database-charset")
            .selected_text(if form.second.is_empty() {
                inherited
            } else {
                &form.second
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut form.second, String::new(), inherited);
                for charset in defaults.charsets.keys() {
                    ui.selectable_value(&mut form.second, charset.clone(), charset);
                }
            });
    });
    if previous != form.second {
        form.third.clear();
    }
    let Some(options) = defaults.charsets.get(effective_charset(form, defaults)) else {
        return;
    };
    let default_collation = if form.second.is_empty() {
        &defaults.collation
    } else {
        &options.default_collation
    };
    let inherited = format!("默认（{default_collation}）");
    ui.horizontal(|ui| {
        ui.label("默认排序规则");
        egui::ComboBox::from_id_salt("database-collation")
            .selected_text(if form.third.is_empty() {
                &inherited
            } else {
                &form.third
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut form.third, String::new(), &inherited);
                for collation in &options.collations {
                    ui.selectable_value(&mut form.third, collation.clone(), collation);
                }
            });
    });
}

pub(super) fn selected_defaults<'a>(
    form: &'a Plan,
    ws: &'a Workspace,
) -> Result<(Option<&'a str>, Option<&'a str>), String> {
    let defaults = ws.database_defaults.as_ref().ok_or_else(|| {
        if ws.busy() {
            "正在读取字符集和排序规则…".into()
        } else {
            format!("无法读取字符集和排序规则。{}", ws.status)
        }
    })?;
    let charset = effective_charset(form, defaults);
    let options = defaults
        .charsets
        .get(charset)
        .ok_or("请选择有效的字符集。")?;
    let collation = (!form.third.is_empty()).then_some(form.third.as_str());
    if collation.is_some_and(|value| !options.collations.contains(value)) {
        return Err("排序规则须与所选字符集匹配。".into());
    }
    let charset = (!form.second.is_empty() || collation.is_some()).then_some(charset);
    Ok((charset, collation))
}

fn effective_charset<'a>(form: &'a Plan, defaults: &'a DatabaseDefaults) -> &'a str {
    if form.second.is_empty() {
        &defaults.charset
    } else {
        &form.second
    }
}
