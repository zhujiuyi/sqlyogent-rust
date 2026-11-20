use eframe::egui::{self, RichText, TextEdit};

use crate::app::{design_tabs::appearance, objects, state::Workspace, style};

use super::{
    Aggregate, Builder, Choice, Column, Field, FieldInput, FilterPhase, Join, JoinCondition,
    JoinKind, OPERATORS, Predicate, Query, Section, Sort, Target, canvas, comparison_sql, metadata,
    panes::{DIVIDER_HEIGHT, Divider},
    takes_value,
};

pub(in crate::app) enum Action {
    OpenLayout,
    StageSql {
        sql: String,
        destination: Destination,
    },
}

pub(in crate::app) enum Destination {
    NewQuery,
    LinkedQuery,
    ViewDraft,
}

impl Builder {
    pub(in crate::app) fn design_view(
        &mut self,
        ui: &mut egui::Ui,
        workspace: &Workspace,
        interactive: bool,
        quote_identifiers: bool,
    ) -> Option<Action> {
        self.metadata.receive(&self.query, workspace);
        let linked_query = self.linked_query(workspace).map(|(_, edited)| edited);
        if linked_query.is_none() {
            self.linked_tab = None;
        }
        let before = (!self.dirty && interactive && ui.input(|input| !input.events.is_empty()))
            .then(|| self.query.clone());
        let height = ui.available_height();
        let splitter_id = ui.id().with("builder-splitters");
        let mut action = None;
        let mut save_as = None;
        ui.scope(|ui| {
            if !interactive {
                ui.disable();
            }
            egui::ScrollArea::vertical()
                .id_salt("builder-page")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let page = style::pane(egui::Margin::same(10));
                    page.show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        let top = ui.next_widget_position().y;
                        appearance::frame(egui::Margin::symmetric(8, 5)).show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.horizontal_wrapped(|ui| {
                                if ui.button("打开布局…").clicked() {
                                    action = Some(Action::OpenLayout);
                                }
                                if ui.button("保存布局").clicked() {
                                    save_as = Some(false);
                                }
                                if self.path.is_some() && ui.button("另存为…").clicked() {
                                    save_as = Some(true);
                                }
                                ui.separator();
                                canvas::tools(ui, self);
                            });
                        });
                        if let Some(status) = &self.status {
                            match status {
                                Ok(message) => ui.colored_label(style::SUCCESS, message),
                                Err(message) => ui.colored_label(style::ERROR, message),
                            };
                        }
                        if self.metadata.loading() {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("正在读取表、列或外键信息…");
                            });
                        } else if self.metadata.failed(&self.query, workspace) {
                            ui.horizontal_wrapped(|ui| {
                                ui.colored_label(style::WARNING, "部分表、列或外键信息无法读取。");
                                if ui.button("重试").clicked() {
                                    self.metadata.retry();
                                }
                            });
                        }
                        canvas::alias_editor(ui, self);
                        let spacing = ui.spacing();
                        let row = spacing.interact_size.y + spacing.item_spacing.y;
                        let inspector = appearance::frame(egui::Margin::symmetric(8, 6));
                        // Reserve space for the settings tabs, SQL title, output actions and splitters.
                        let chrome = page.total_margin().sum().y
                            + 2.0 * inspector.total_margin().sum().y
                            + 3.0 * row
                            + 2.0 * (DIVIDER_HEIGHT + spacing.item_spacing.y);
                        let available = height - chrome - (ui.next_widget_position().y - top);
                        let [canvas_height, fields_height, preview_height] =
                            self.pane_sizes.heights(available);
                        let pending_column = canvas::workspace(ui, self, workspace, canvas_height);
                        self.pane_sizes
                            .splitter(ui, splitter_id, Divider::Canvas, available);
                        let choices = self.choices();
                        let grid = inspector
                            .show(ui, |ui| {
                                ui.horizontal_wrapped(|ui| {
                                    for (section, label) in [
                                        (Section::Fields, "结果字段"),
                                        (Section::Connections, "连接条件"),
                                        (Section::Filter, "筛选"),
                                        (Section::GroupAndSort, "分组与排序"),
                                    ] {
                                        ui.selectable_value(&mut self.section, section, label);
                                    }
                                });
                                egui::ScrollArea::vertical()
                                    .id_salt(self.section as u8)
                                    .auto_shrink([false, false])
                                    .min_scrolled_height(fields_height)
                                    .max_height(fields_height)
                                    .show(ui, |ui| match self.section {
                                        Section::Connections => {
                                            show_connections(ui, self, workspace, &choices);
                                            None
                                        }
                                        Section::Fields => {
                                            Some(show_fields(ui, &mut self.query, &choices))
                                        }
                                        Section::Filter => {
                                            let targets = target_choices(&self.query, &choices);
                                            show_filters(ui, &mut self.query.filters, &targets);
                                            None
                                        }
                                        Section::GroupAndSort => {
                                            let targets = target_choices(&self.query, &choices);
                                            show_group_and_sort(ui, &mut self.query, &targets);
                                            None
                                        }
                                    })
                            })
                            .inner;
                        if let Some(targets) = grid.inner {
                            if let Some((column, point)) = pending_column {
                                targets.apply(&mut self.query, grid.inner_rect, column, point);
                            } else if self.canvas.column_drag_active() {
                                targets.show_marker(ui, grid.inner_rect);
                            }
                        }

                        self.pane_sizes
                            .splitter(ui, splitter_id, Divider::Fields, available);
                        let preview = self
                            .query
                            .sql_with_quoting(workspace.no_backslash_escapes, quote_identifiers);
                        inspector.show(ui, |ui| {
                            ui.label(RichText::new("实时 SQL").strong());
                            egui::Frame::new()
                                .fill(style::PANEL)
                                .corner_radius(6)
                                .inner_margin(egui::Margin::same(4))
                                .show(ui, |ui| {
                                    egui::ScrollArea::vertical()
                                        .id_salt("select-preview")
                                        .auto_shrink([false, false])
                                        .min_scrolled_height(preview_height)
                                        .max_height(preview_height)
                                        .show(ui, |ui| match &preview {
                                            Ok(sql) => appearance::sql_preview(ui, sql, 3),
                                            Err(error) => {
                                                let color = if self.query.sources.is_empty() {
                                                    style::MUTED
                                                } else {
                                                    style::ERROR
                                                };
                                                ui.colored_label(color, *error);
                                            }
                                        });
                                });
                            if let Some(output) =
                                self.show_output(ui, preview.as_deref().ok(), linked_query)
                            {
                                action = Some(output);
                            }
                        });
                    });
                });
        });
        if before.as_ref().is_some_and(|query| *query != self.query) {
            self.dirty = true;
            ui.ctx().request_repaint();
        }
        if let Some(save_as) = save_as {
            self.save_layout(save_as);
            ui.ctx().request_repaint();
        }
        action
    }

    fn show_output(
        &mut self,
        ui: &mut egui::Ui,
        preview: Option<&str>,
        linked_query_edited: Option<bool>,
    ) -> Option<Action> {
        let mut action = None;
        if let Some(name) = &mut self.view {
            let schema = self
                .query
                .sources
                .first()
                .map(|source| source.object.schema.as_str())
                .unwrap_or_default();
            let mut close = false;
            ui.horizontal_wrapped(|ui| {
                ui.label("视图名称");
                ui.label(RichText::new(schema).monospace());
                ui.add(
                    TextEdit::singleline(name)
                        .desired_width(155.0)
                        .hint_text("填写名称"),
                );
                let name = name.trim();
                let valid_name =
                    !name.is_empty() && name.chars().count() <= 64 && !name.contains('\0');
                if ui
                    .add_enabled(
                        !schema.is_empty() && valid_name && preview.is_some(),
                        egui::Button::new("打开视图 SQL"),
                    )
                    .clicked()
                    && let Some(sql) = preview
                {
                    action = Some(Action::StageSql {
                        sql: objects::create_view_draft(schema, name, sql),
                        destination: Destination::ViewDraft,
                    });
                }
                close = ui.button("返回").clicked();
            });
            if close {
                self.view = None;
            }
        } else {
            ui.horizontal_wrapped(|ui| {
                let enabled = preview.is_some();
                if ui
                    .add_enabled(enabled, style::primary("在新查询页打开"))
                    .clicked()
                    && let Some(sql) = preview
                {
                    action = Some(Action::StageSql {
                        sql: sql.to_owned(),
                        destination: Destination::NewQuery,
                    });
                }
                ui.add_enabled_ui(enabled, |ui| {
                    ui.menu_button("其他输出", |ui| {
                        let Some(sql) = preview else { return };
                        if ui.button("复制 SQL").clicked() {
                            ui.ctx().copy_text(sql.to_owned());
                            ui.close();
                        }
                        let update_hint = if linked_query_edited == Some(true) {
                            "关联查询页已有手动修改。本次会另开查询页。"
                        } else {
                            "更新本设计上次生成的查询页。"
                        };
                        if ui
                            .add_enabled(
                                linked_query_edited.is_some(),
                                egui::Button::new("更新关联查询页"),
                            )
                            .on_hover_text(update_hint)
                            .on_disabled_hover_text("先将本设计的 SQL 发送到新查询页。")
                            .clicked()
                        {
                            action = Some(Action::StageSql {
                                sql: sql.to_owned(),
                                destination: Destination::LinkedQuery,
                            });
                            ui.close();
                        }
                        if ui.button("生成视图 SQL…").clicked() {
                            self.view = Some(String::new());
                            ui.close();
                        }
                    });
                });
            });
        }
        action
    }
}

fn show_connections(
    ui: &mut egui::Ui,
    builder: &mut Builder,
    workspace: &Workspace,
    choices: &[Choice],
) {
    let mut suggestion = None;
    ui.horizontal_wrapped(|ui| {
        let [left, right] = &mut builder.manual_join;
        column_combo(ui, "add-join-left", choices, left, |_| true, "选择左列");
        ui.label("=");
        let left_id = left.as_ref().map(|column| column.table);
        column_combo(
            ui,
            "add-join-right",
            choices,
            right,
            |column| Some(column.table) != left_id,
            "选择右列",
        );
        let pair = left
            .as_ref()
            .zip(right.as_ref())
            .filter(|(left, right)| left.table != right.table);
        if ui
            .add_enabled(pair.is_some(), egui::Button::new("添加连接"))
            .clicked()
            && let Some((left, right)) = pair
        {
            let (left, right) = (left.clone(), right.clone());
            builder.connect(left, right);
            builder.manual_join = [None, None];
        }
        ui.menu_button("从外键添加", |ui| {
            let suggestions = metadata::suggestions(&builder.query, workspace);
            if suggestions.is_empty() {
                ui.label("没有可添加的外键。");
            }
            egui::ScrollArea::vertical()
                .max_height(230.0)
                .show(ui, |ui| {
                    for candidate in suggestions {
                        if ui.button(candidate.label).clicked() {
                            suggestion = Some(candidate.join);
                            ui.close();
                        }
                    }
                });
        });
    });
    if let Some(join) = suggestion {
        builder.metadata.auto_join.remove(&join.left);
        builder.metadata.auto_join.remove(&join.right);
        builder.query.joins.push(join);
    }
    let mut remove = None;
    let query = &mut builder.query;
    for (index, join) in query.joins.iter_mut().enumerate() {
        let titles = [join.left, join.right].map(|id| {
            query
                .sources
                .iter()
                .find(|source| source.id == id)
                .map_or("?", |source| source.label())
        });
        if show_join(ui, join, titles, choices) {
            remove = Some(index);
        }
    }
    if let Some(index) = remove {
        query.joins.remove(index);
    }
}

fn show_join(ui: &mut egui::Ui, join: &mut Join, titles: [&str; 2], choices: &[Choice]) -> bool {
    ui.push_id(("join", join.left, join.right), |ui| {
        ui.group(|ui| {
            let remove = ui
                .horizontal_wrapped(|ui| {
                    ui.label(RichText::new(format!("{} → {}", titles[0], titles[1])).monospace());
                    egui::ComboBox::from_id_salt("join-kind")
                        .selected_text(join.kind.label())
                        .show_ui(ui, |ui| {
                            for kind in JoinKind::ALL {
                                ui.selectable_value(&mut join.kind, kind, kind.label());
                            }
                        });
                    ui.button("移除连接").clicked()
                })
                .inner;
            let mut remove_condition = None;
            let multiple = join.conditions.len() > 1;
            for (index, condition) in join.conditions.iter_mut().enumerate() {
                ui.horizontal_wrapped(|ui| {
                    ui.label(if index == 0 { "连接条件" } else { "AND" });
                    column_combo(
                        ui,
                        ("join-left", index),
                        choices,
                        &mut condition.left,
                        |column| column.table == join.left,
                        "选择左列",
                    );
                    ui.label("=");
                    column_combo(
                        ui,
                        ("join-right", index),
                        choices,
                        &mut condition.right,
                        |column| column.table == join.right,
                        "选择右列",
                    );
                    if multiple && ui.button("移除条件").clicked() {
                        remove_condition = Some(index);
                    }
                });
            }
            if let Some(condition) = remove_condition {
                join.conditions.remove(condition);
            }
            if ui.button("添加连接条件").clicked() {
                join.conditions.push(JoinCondition::default());
            }
            remove
        })
        .inner
    })
    .inner
}

struct FieldDropTargets {
    clip: egui::Rect,
    top: f32,
    rows: Vec<(usize, f32)>,
}

impl FieldDropTargets {
    fn target_at(&self, grid: egui::Rect, point: egui::Pos2) -> Option<(Option<usize>, f32)> {
        if !self.clip.intersect(grid).contains(point) || point.y < self.top {
            return None;
        }
        let mut top = self.top;
        for &(id, bottom) in &self.rows {
            if point.y < bottom {
                return Some((Some(id), top));
            }
            top = bottom;
        }
        Some((None, top))
    }

    fn apply(&self, query: &mut Query, grid: egui::Rect, column: Column, point: egui::Pos2) {
        if let Some((before, _)) = self.target_at(grid, point)
            && query.source(column.table).is_some()
        {
            query.insert_field(before, Some(FieldInput::Column(column)), Aggregate::None);
        }
    }

    fn show_marker(&self, ui: &egui::Ui, grid: egui::Rect) {
        if ui.is_enabled()
            && ui.input(|input| input.pointer.primary_down())
            && let Some(point) = ui.ctx().pointer_interact_pos()
            && let Some((_, y)) = self.target_at(grid, point)
        {
            let clip = grid.intersect(self.clip);
            ui.painter().with_clip_rect(clip).hline(
                clip.x_range(),
                y.max(clip.top() + 1.0),
                egui::Stroke::new(2.0_f32, style::ACCENT),
            );
            ui.ctx().set_cursor_icon(egui::CursorIcon::Copy);
        }
    }
}

#[derive(Clone, Copy)]
enum FieldAction {
    Row(RowAction),
    InsertBefore(usize),
    Reset,
}

fn show_fields(ui: &mut egui::Ui, query: &mut Query, choices: &[Choice]) -> FieldDropTargets {
    ui.horizontal_wrapped(|ui| {
        let mut column = None;
        column_combo(ui, "add-column", choices, &mut column, |_| true, "添加列");
        if let Some(column) = column {
            query.add_field(Some(FieldInput::Column(column)), Aggregate::None);
        }
        if ui.button("添加 SQL 表达式").clicked() {
            query.add_field(
                Some(FieldInput::Sql {
                    text: String::new(),
                    aggregate: false,
                }),
                Aggregate::None,
            );
        }
        if ui.button("添加 COUNT(*)").clicked() {
            query.add_field(None, Aggregate::CountRows);
        }
        if !query.fields.is_empty() && ui.button("清空字段").clicked() {
            query.clear_fields();
        }
    });
    let mut targets = FieldDropTargets {
        clip: ui.clip_rect(),
        top: ui.next_widget_position().y,
        rows: Vec::new(),
    };
    if query.fields.is_empty() {
        let hint = if query.groups.is_empty() {
            "没有指定输出字段时默认查询所有列。"
        } else {
            "没有指定输出字段时返回分组字段和记录数。"
        };
        ui.colored_label(style::MUTED, hint);
    }

    let count = query.fields.len();
    let mut action = None;
    for (index, field) in query.fields.iter_mut().enumerate() {
        let had_projection = field.has_projection();
        let row = ui.push_id(("result-field", field.id), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut field.show, "输出");
                let raw_aggregate = field.input.as_ref().is_some_and(FieldInput::is_aggregate);
                ui.add_enabled_ui(!raw_aggregate, |ui| {
                    egui::ComboBox::from_id_salt("aggregate")
                        .width(122.0)
                        .selected_text(field.aggregate.label())
                        .show_ui(ui, |ui| {
                            for aggregate in Aggregate::ALL {
                                ui.selectable_value(
                                    &mut field.aggregate,
                                    aggregate,
                                    aggregate.label(),
                                );
                            }
                        });
                });
                if field.aggregate != Aggregate::CountRows {
                    field_input_combo(ui, field, choices);
                }
                ui.add(
                    TextEdit::singleline(&mut field.alias)
                        .desired_width(100.0)
                        .hint_text("列别名"),
                );
                action = row_action(ui, index, count)
                    .map(FieldAction::Row)
                    .or(action);
            });
            if field.aggregate != Aggregate::CountRows
                && let Some(FieldInput::Sql { text, aggregate }) = &mut field.input
            {
                ui.horizontal_wrapped(|ui| {
                    ui.add(
                        TextEdit::singleline(text)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(290.0)
                            .hint_text("例如 COALESCE(score, 0)"),
                    );
                    ui.checkbox(aggregate, "聚合表达式")
                        .on_hover_text("表达式已包含聚合函数时勾选。对应筛选将进入 HAVING。");
                    if *aggregate {
                        field.aggregate = Aggregate::None;
                    }
                });
            }
        });
        if !had_projection && field.has_projection() {
            field.show = true;
        }
        action = field_context_menu(ui, &row.response, field.id, index, count > 1).or(action);
        targets.rows.push((field.id, ui.next_widget_position().y));
    }
    match action {
        Some(FieldAction::Row(RowAction::Remove(index))) => query.remove_field(index),
        Some(FieldAction::Row(action)) => action.apply(&mut query.fields),
        Some(FieldAction::InsertBefore(id)) => query.insert_field(Some(id), None, Aggregate::None),
        Some(FieldAction::Reset) => query.reset_field_slots(),
        None => {}
    }
    ui.add_space(ui.spacing().interact_size.y);
    targets
}

fn field_context_menu(
    ui: &egui::Ui,
    row: &egui::Response,
    id: usize,
    index: usize,
    can_delete: bool,
) -> Option<FieldAction> {
    if !ui.is_enabled() {
        return None;
    }
    // The menu can be opened over a field's controls without intercepting their primary clicks.
    let row_hit = egui::Rect::from_min_max(
        row.rect.min,
        egui::pos2(ui.max_rect().right(), row.rect.bottom()),
    );
    let open =
        ui.rect_contains_pointer(row_hit) && ui.input(|input| input.pointer.secondary_clicked());
    egui::Popup::context_menu(row)
        .id(ui.make_persistent_id(("result-field-menu", id)))
        .open_memory(open.then_some(egui::SetOpenCommand::Bool(true)))
        .show(|ui| {
            for (label, action, enabled) in [
                ("在此字段前插入", FieldAction::InsertBefore(id), true),
                (
                    "删除字段",
                    FieldAction::Row(RowAction::Remove(index)),
                    can_delete,
                ),
                ("清除全部字段", FieldAction::Reset, true),
            ] {
                if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                    ui.close();
                    return Some(action);
                }
            }
            None
        })
        .and_then(|menu| menu.inner)
}

fn field_input_combo(ui: &mut egui::Ui, field: &mut Field, choices: &[Choice]) {
    let label = match &field.input {
        Some(FieldInput::Column(column)) => column_label(choices, column).unwrap_or("列已移除"),
        Some(FieldInput::Sql { .. }) => "自定义 SQL",
        None => "选择列或表达式",
    };
    egui::ComboBox::from_id_salt("input")
        .selected_text(label)
        .truncate()
        .width(150.0)
        .height(245.0)
        .show_ui(ui, |ui| {
            let custom = matches!(field.input, Some(FieldInput::Sql { .. }));
            if ui.selectable_label(custom, "自定义 SQL").clicked() && !custom {
                field.input = Some(FieldInput::Sql { text: String::new(), aggregate: false });
            }
            for choice in choices {
                let selected = matches!(&field.input, Some(FieldInput::Column(column)) if *column == choice.column);
                if ui.selectable_label(selected, &choice.label).clicked() {
                    field.input = Some(FieldInput::Column(choice.column.clone()));
                }
            }
        });
}

struct TargetChoice {
    target: Target,
    label: String,
    group_label: Option<String>,
    aggregate: bool,
}

impl TargetChoice {
    fn label(&self, grouping: bool) -> Option<&str> {
        if grouping {
            self.group_label.as_deref()
        } else {
            Some(&self.label)
        }
    }
}

fn target_choices(builder: &Query, columns: &[Choice]) -> Vec<TargetChoice> {
    let mut choices = Vec::new();
    for field in &builder.fields {
        let argument = match &field.input {
            Some(FieldInput::Column(column)) => column_label(columns, column),
            Some(FieldInput::Sql { text, .. }) => Some(text.trim()).filter(|text| !text.is_empty()),
            None => None,
        };
        if argument.is_none() && field.aggregate != Aggregate::CountRows {
            continue;
        }
        let expression = field.aggregate.apply(argument.unwrap_or_default().into());
        let alias = field.alias.trim();
        let label = if alias.is_empty() {
            format!("结果字段 · {expression}")
        } else {
            format!("{alias}（{expression}）")
        };
        let group_label = argument.filter(|_| field.has_group_key()).map(|argument| {
            if alias.is_empty() || field.is_aggregate() {
                format!("结果字段的分组键 · {argument}")
            } else {
                format!("{alias}（{argument}）")
            }
        });
        choices.push(TargetChoice {
            target: Target::Field(field.id),
            label,
            group_label,
            aggregate: field.is_aggregate(),
        });
    }
    for choice in columns {
        choices.push(TargetChoice {
            target: Target::Column(choice.column.clone()),
            label: choice.label.clone(),
            group_label: Some(choice.label.clone()),
            aggregate: false,
        });
    }
    choices
}

fn show_filters(ui: &mut egui::Ui, groups: &mut Vec<Vec<Predicate>>, choices: &[TargetChoice]) {
    if !groups.is_empty() {
        ui.colored_label(
            style::MUTED,
            "同一组内的条件用 AND 连接，不同组之间用 OR 连接。WHERE 和 HAVING 分别组合。",
        );
    }
    let mut remove_group = None;
    for (group_index, group) in groups.iter_mut().enumerate() {
        ui.push_id(("filter-group", group_index), |ui| {
            ui.group(|ui| {
                if group_index > 0 {
                    ui.label("OR");
                }
                let mut remove = None;
                for (index, filter) in group.iter_mut().enumerate() {
                    ui.push_id(index, |ui| {
                        if show_predicate(ui, filter, choices, index > 0) {
                            remove = Some(index);
                        }
                    });
                }
                if let Some(index) = remove {
                    group.remove(index);
                }
                if group.is_empty() {
                    remove_group = Some(group_index);
                }
                if ui.button("添加 AND 条件").clicked() {
                    group.push(Predicate::default());
                }
            });
        });
    }
    if let Some(group) = remove_group {
        groups.remove(group);
    }
    let label = if groups.is_empty() {
        "添加筛选条件"
    } else {
        "添加 OR 条件组"
    };
    if ui.button(label).clicked() {
        groups.push(vec![Predicate::default()]);
    }
}

fn show_predicate(
    ui: &mut egui::Ui,
    predicate: &mut Predicate,
    choices: &[TargetChoice],
    conjunction: bool,
) -> bool {
    let mut change_mode = false;
    let remove = ui
        .horizontal_wrapped(|ui| {
            if conjunction {
                ui.label("AND");
            }
            match predicate {
                Predicate::Compare {
                    target,
                    operator,
                    value,
                    numeric,
                } => {
                    if let Some(chosen) = target_combo(
                        ui,
                        "filter-target",
                        choices,
                        target.as_ref(),
                        false,
                        "选择字段",
                    ) {
                        *target = Some(chosen);
                    }
                    let aggregate = target.as_ref().is_some_and(|target| {
                        choices
                            .iter()
                            .any(|choice| choice.target == *target && choice.aggregate)
                    });
                    ui.colored_label(style::MUTED, FilterPhase::inferred(aggregate).label());
                    egui::ComboBox::from_id_salt("comparison")
                        .selected_text(comparison_sql(*operator))
                        .width(80.0)
                        .show_ui(ui, |ui| {
                            for (condition, label) in OPERATORS {
                                ui.selectable_value(operator, condition, label);
                            }
                        });
                    if takes_value(*operator) {
                        ui.add(
                            TextEdit::singleline(value)
                                .desired_width(125.0)
                                .hint_text("比较值"),
                        );
                        ui.checkbox(numeric, "数字")
                            .on_hover_text("勾选后按数字生成，默认按文本生成。");
                    }
                    change_mode = ui.button("改用 SQL").clicked();
                }
                Predicate::Sql { text, phase } => {
                    egui::ComboBox::from_id_salt("filter-phase")
                        .selected_text(phase.label())
                        .show_ui(ui, |ui| {
                            for value in [FilterPhase::Where, FilterPhase::Having] {
                                ui.selectable_value(phase, value, value.label());
                            }
                        });
                    ui.add(
                        TextEdit::singleline(text)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(290.0)
                            .hint_text("例如 score BETWEEN 60 AND 100"),
                    );
                    change_mode = ui.button("改用字段").clicked();
                }
            }
            ui.button("移除条件").clicked()
        })
        .inner;
    if change_mode {
        *predicate = match predicate {
            Predicate::Compare { .. } => Predicate::Sql {
                text: String::new(),
                phase: FilterPhase::Where,
            },
            Predicate::Sql { .. } => Predicate::default(),
        };
    }
    remove
}

fn show_group_and_sort(ui: &mut egui::Ui, builder: &mut Query, choices: &[TargetChoice]) {
    ui.label("分组字段（GROUP BY）");
    let mut action = None;
    let count = builder.groups.len();
    for (index, target) in builder.groups.iter_mut().enumerate() {
        ui.push_id(("group", index), |ui| {
            ui.horizontal_wrapped(|ui| {
                if let Some(chosen) =
                    target_combo(ui, "target", choices, Some(target), true, "选择分组字段")
                {
                    *target = chosen;
                }
                action = row_action(ui, index, count).or(action);
            });
        });
    }
    if let Some(action) = action {
        action.apply(&mut builder.groups);
    }
    if let Some(target) = target_combo(ui, "add-group", choices, None, true, "添加分组字段") {
        builder.groups.push(target);
    }

    ui.separator();
    ui.label("排序字段（ORDER BY）");
    let mut action = None;
    let count = builder.sorts.len();
    for (index, sort) in builder.sorts.iter_mut().enumerate() {
        ui.push_id(("sort", index), |ui| {
            ui.horizontal_wrapped(|ui| {
                if let Some(chosen) = target_combo(
                    ui,
                    "target",
                    choices,
                    Some(&sort.target),
                    false,
                    "选择排序字段",
                ) {
                    sort.target = chosen;
                }
                egui::ComboBox::from_id_salt("direction")
                    .selected_text(if sort.descending { "降序" } else { "升序" })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut sort.descending, false, "升序");
                        ui.selectable_value(&mut sort.descending, true, "降序");
                    });
                action = row_action(ui, index, count).or(action);
            });
        });
    }
    if let Some(action) = action {
        action.apply(&mut builder.sorts);
    }
    if let Some(target) = target_combo(ui, "add-sort", choices, None, false, "添加排序字段") {
        builder.sorts.push(Sort {
            target,
            descending: false,
        });
    }

    ui.separator();
    ui.horizontal_wrapped(|ui| {
        ui.checkbox(&mut builder.distinct, "去重（DISTINCT）");
        ui.checkbox(&mut builder.limit_rows, "限制返回行数");
        ui.add_enabled(
            builder.limit_rows,
            egui::DragValue::new(&mut builder.limit).range(1..=10_000),
        );
    });
}

#[derive(Clone, Copy)]
enum RowAction {
    Remove(usize),
    Move(usize, usize),
}

impl RowAction {
    fn apply<T>(self, rows: &mut Vec<T>) {
        match self {
            Self::Remove(index) => {
                rows.remove(index);
            }
            Self::Move(from, to) => rows.swap(from, to),
        }
    }
}

fn row_action(ui: &mut egui::Ui, index: usize, count: usize) -> Option<RowAction> {
    if count > 1 {
        if ui
            .add_enabled(index > 0, egui::Button::new("↑"))
            .on_hover_text("上移")
            .clicked()
        {
            return Some(RowAction::Move(index, index - 1));
        }
        if ui
            .add_enabled(index + 1 < count, egui::Button::new("↓"))
            .on_hover_text("下移")
            .clicked()
        {
            return Some(RowAction::Move(index, index + 1));
        }
    }
    ui.button("移除")
        .clicked()
        .then_some(RowAction::Remove(index))
}

fn target_combo(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash,
    choices: &[TargetChoice],
    selected: Option<&Target>,
    group: bool,
    empty: &str,
) -> Option<Target> {
    let display = selected.map_or(empty, |target| {
        choices
            .iter()
            .find(|choice| choice.target == *target)
            .and_then(|choice| choice.label(group))
            .unwrap_or("请重新选择字段")
    });
    let mut change = None;
    egui::ComboBox::from_id_salt(id)
        .selected_text(display)
        .truncate()
        .width(178.0)
        .height(245.0)
        .show_ui(ui, |ui| {
            for (choice, label) in choices
                .iter()
                .filter_map(|choice| choice.label(group).map(|label| (choice, label)))
            {
                let selection = selected == Some(&choice.target);
                if ui.selectable_label(selection, label).clicked() {
                    change = Some(choice.target.clone());
                }
            }
        });
    change
}

fn column_label<'a>(choices: &'a [Choice], column: &Column) -> Option<&'a str> {
    choices
        .iter()
        .find(|choice| &choice.column == column)
        .map(|choice| choice.label.as_str())
}

fn column_combo(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash,
    choices: &[Choice],
    selected: &mut Option<Column>,
    allowed: impl Fn(&Column) -> bool,
    empty: &str,
) {
    let display = selected
        .as_ref()
        .and_then(|column| column_label(choices, column))
        .unwrap_or(empty);
    egui::ComboBox::from_id_salt(id)
        .selected_text(display)
        .truncate()
        .width(145.0)
        .height(245.0)
        .show_ui(ui, |ui| {
            for choice in choices.iter().filter(|choice| allowed(&choice.column)) {
                ui.selectable_value(selected, Some(choice.column.clone()), &choice.label);
            }
        });
}
