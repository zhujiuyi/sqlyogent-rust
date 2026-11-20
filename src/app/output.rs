mod clipboard;
mod form;
mod grid;
mod history;
pub(super) mod selection;
mod text;
pub(super) mod values;

pub(super) use grid::{
    GRID_HEADER_HEIGHT, GRID_ROW_GAP, GRID_ROW_HEIGHT, InlineJoinedCell, InlineKey,
    grid_cell_styled, grid_label, paint_cell_outline, paint_grid_header, render_inline_cell_editor,
};
pub(super) use history::{History, HistoryTiming};
pub(super) use text::TextRows;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use eframe::egui::{self, Align, Layout, RichText, TextEdit};

use crate::database::{Cell, Command, ObjectKind, ResultSet, transfer};

use super::io_jobs::export::{Options as ExportOptions, RESULT_FORMATS};
use super::preferences::{EditorSettings, GridWidths, TextFont};
use super::state::{Output, Workspace};
use super::style;
use super::workspace_windows::PanelHost;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum ResultMode {
    #[default]
    Grid,
    Text,
    Form,
}

impl ResultMode {
    pub(super) const ALL: [Self; 3] = [Self::Grid, Self::Text, Self::Form];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Grid => "表格",
            Self::Text => "文本",
            Self::Form => "表单",
        }
    }
}

pub(super) fn result_view_controls(ui: &mut egui::Ui, mode: &mut ResultMode, form: bool) {
    style::subtle_card(egui::Margin::symmetric(2, 1)).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for choice in ResultMode::ALL {
                let enabled = form || choice != ResultMode::Form;
                let selected = *mode == choice;
                let icon = match choice {
                    ResultMode::Grid => style::ToolbarIcon::GridView,
                    ResultMode::Text => style::ToolbarIcon::TextView,
                    ResultMode::Form => style::ToolbarIcon::FormView,
                };
                let tooltip = match choice {
                    ResultMode::Grid => "表格视图",
                    ResultMode::Text => "文本视图（Ctrl+L / ⌘L）",
                    ResultMode::Form => "表单视图",
                };
                let sense = if enabled {
                    egui::Sense::click()
                } else {
                    egui::Sense::hover()
                };
                let (rect, response) = ui.allocate_exact_size(egui::vec2(24.0, 20.0), sense);
                if ui.is_rect_visible(rect) {
                    let hovered = enabled && response.hovered();
                    let fill = if selected {
                        style::surface()
                    } else if hovered {
                        style::accent_tint()
                    } else {
                        egui::Color32::TRANSPARENT
                    };
                    let stroke = if selected {
                        egui::Stroke::new(1.0_f32, style::border())
                    } else {
                        egui::Stroke::NONE
                    };
                    let fg = if !enabled {
                        style::subtle()
                    } else if selected {
                        style::accent()
                    } else if hovered {
                        style::ink()
                    } else {
                        style::muted()
                    };
                    let painter = ui.painter();
                    painter.rect(
                        rect,
                        egui::CornerRadius::same(4),
                        fill,
                        stroke,
                        egui::StrokeKind::Inside,
                    );
                    style::paint_toolbar_icon(painter, rect, icon, fg);
                }
                if response.on_hover_text(tooltip).clicked() && enabled {
                    *mode = choice;
                }
            }
        });
    });
}

#[derive(Default)]
pub(super) struct ResultViews {
    sets: BTreeMap<usize, ResultView>,
    export: Option<(usize, ExportOptions)>,
}

#[derive(Default)]
struct ResultView {
    grid: grid::View,
    record: form::Record,
    grid_row: Option<usize>,
    text: TextRows,
}

#[derive(Default)]
pub(super) struct ResultGrid(grid::View);

#[derive(Debug, PartialEq, Eq)]
pub(super) enum JoinedGridAction {
    Edit(selection::Point, Option<String>),
    Move(selection::Point),
    InlineCommit,
    InlineCancel,
    InlineStepColumn { forward: bool },
}

impl ResultGrid {
    pub(super) fn has_modal(&self) -> bool {
        self.0.has_modal()
    }

    pub(super) fn take_export_request(&mut self) -> bool {
        self.0.take_export_request()
    }

    #[cfg(test)]
    pub(super) fn show(
        &mut self,
        ui: &mut egui::Ui,
        set: &ResultSet,
        headers: bool,
        keyboard_allowed: bool,
        settings: EditorSettings,
        grid_widths: &mut GridWidths,
    ) {
        let _ = self.render(
            ui,
            set,
            headers,
            keyboard_allowed,
            settings,
            grid_widths,
            None,
        );
    }

    pub(super) fn show_read_only(
        &mut self,
        ui: &mut egui::Ui,
        set: &ResultSet,
        headers: bool,
        keyboard_allowed: bool,
        settings: EditorSettings,
        grid_widths: &mut GridWidths,
    ) {
        let _ = self.render(
            ui,
            set,
            headers,
            keyboard_allowed,
            settings,
            grid_widths,
            Some(grid::JoinedEditing {
                can_edit: None,
                can_check: None,
                move_draft: false,
                read_only_tint: true,
                inline: None,
                modified_cell: None,
            }),
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn show_joined(
        &mut self,
        ui: &mut egui::Ui,
        set: &ResultSet,
        headers: bool,
        keyboard_allowed: bool,
        settings: EditorSettings,
        grid_widths: &mut GridWidths,
        can_edit: Option<&dyn Fn(selection::Point) -> bool>,
        can_check: Option<&dyn Fn(usize) -> bool>,
        move_draft: bool,
        inline: Option<grid::InlineJoinedCell<'_>>,
        modified_cell: Option<&dyn Fn(selection::Point) -> Option<String>>,
    ) -> Option<JoinedGridAction> {
        self.render(
            ui,
            set,
            headers,
            keyboard_allowed,
            settings,
            grid_widths,
            Some(grid::JoinedEditing {
                can_edit,
                can_check,
                move_draft,
                read_only_tint: false,
                inline,
                modified_cell,
            }),
        )
    }

    pub(super) fn show_text(
        &mut self,
        ui: &mut egui::Ui,
        set: &ResultSet,
        text: &mut TextRows,
        copy_headers: bool,
        keyboard_allowed: bool,
        request_focus: bool,
    ) {
        if self.0.prepare(set) {
            text.reset_match();
        }
        show_text_result(
            ui,
            set,
            self.0.visible(),
            text,
            copy_headers,
            keyboard_allowed,
            request_focus,
        );
    }

    pub(super) fn request_focus(&mut self) {
        self.0.request_focus();
    }

    pub(super) fn selected_source_point(&self) -> Option<selection::Point> {
        self.0.selected_source_point()
    }

    pub(super) fn selected_source_rows(&self) -> Vec<usize> {
        self.0.selected_source_rows().to_vec()
    }

    pub(super) fn adjacent_source_point(&self, forward: bool) -> Option<selection::Point> {
        self.0.adjacent_source_point(forward)
    }

    pub(super) fn checked_source_rows(&self) -> Vec<usize> {
        self.0.checked_source_rows()
    }

    pub(super) fn clear_checks(&mut self) {
        self.0.clear_checks();
    }

    pub(super) fn focus_source_point(&mut self, set: &ResultSet, point: selection::Point) -> bool {
        self.0.focus_source_point(set, point)
    }

    pub(super) fn show_controls(
        &mut self,
        ui: &mut egui::Ui,
        set: &ResultSet,
        headers: bool,
        enabled: bool,
    ) {
        self.0.prepare(set);
        ui.horizontal(|ui| {
            if !enabled {
                ui.disable();
            }
            grid::controls(ui, set, &mut self.0, headers, false, false);
        });
        self.0.prepare(set);
    }

    fn render(
        &mut self,
        ui: &mut egui::Ui,
        set: &ResultSet,
        headers: bool,
        keyboard_allowed: bool,
        settings: EditorSettings,
        grid_widths: &mut GridWidths,
        joined: Option<grid::JoinedEditing<'_>>,
    ) -> Option<JoinedGridAction> {
        self.0.prepare(set);
        if joined.is_none() {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.horizontal_wrapped(|ui| {
                grid::controls(ui, set, &mut self.0, headers, false, true);
            });
            self.0.prepare(set);
        }
        let action = grid::show_grid(
            ui,
            set,
            &mut self.0,
            headers,
            keyboard_allowed,
            false,
            grid_widths,
            joined,
        );
        grid::dialogs(ui.ctx(), set, &mut self.0, settings);
        action
    }
}

impl ResultViews {
    pub(super) fn has_modal(&self, result: usize) -> bool {
        self.export
            .as_ref()
            .is_some_and(|(index, _)| *index == result)
            || self
                .sets
                .get(&result)
                .is_some_and(|view| view.grid.has_modal())
    }

    pub(super) fn text_has_focus(&self, ctx: &egui::Context, result: usize) -> bool {
        self.sets
            .get(&result)
            .is_some_and(|view| view.text.has_focus(ctx))
    }

    pub(super) fn text_search(&mut self, result: usize, next: bool) {
        if let Some(view) = self.sets.get_mut(&result) {
            view.text.request_find(next);
        }
    }
}

pub(super) fn text_has_focus(text: &TextRows, ctx: &egui::Context) -> bool {
    text.has_focus(ctx)
}

pub(super) fn has_modal(workspace: &Workspace) -> bool {
    if workspace.error_dialog.is_some() {
        return true;
    }
    let tab = workspace.tab();
    match workspace.output {
        Output::Results => tab.editable_editor.as_ref().map_or_else(
            || tab.result_views.has_modal(tab.set),
            super::row_editor::RowEditor::has_modal,
        ),
        Output::Data => tab
            .data_editor
            .as_ref()
            .is_some_and(super::row_editor::RowEditor::has_modal),
        _ => false,
    }
}

pub(super) fn panel(
    ctx: &egui::Context,
    workspace: &mut Workspace,
    full_height: bool,
    keyboard_allowed: bool,
    settings: EditorSettings,
    request_focus: bool,
    grid_widths: &mut GridWidths,
) {
    panel_in(
        &mut PanelHost::Root(ctx),
        workspace,
        full_height,
        keyboard_allowed,
        settings,
        request_focus,
        grid_widths,
    );
}

pub(super) fn panel_in(
    host: &mut PanelHost<'_>,
    workspace: &mut Workspace,
    full_height: bool,
    keyboard_allowed: bool,
    settings: EditorSettings,
    request_focus: bool,
    grid_widths: &mut GridWidths,
) {
    let request_focus = request_focus && keyboard_allowed;
    let local = host.is_local();
    let suppress_modal = local && !keyboard_allowed;
    let panel_id = if local {
        egui::Id::new(("output", workspace.key))
    } else {
        egui::Id::new("output")
    };
    let mut content = |ui: &mut egui::Ui| {
        ui.set_min_height(ui.available_height());
        let has_profile = workspace.tab().profile.is_some();
        let has_editable_result = workspace.tab().editable_editor.is_some();
        let starting_f8 = workspace.starting_f8_on_active_tab();
        let mut focus_editable_result = request_focus;
        if workspace.output == Output::Profile && !has_profile {
            workspace.output = Output::Results;
        }
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.horizontal_wrapped(|ui| {
            for (output, label) in [
                (Output::Results, "结果"),
                (Output::Profile, "查询分析"),
                (Output::Messages, "消息"),
                (Output::Data, "表数据"),
                (Output::History, "历史记录"),
                (Output::Definition, "对象定义"),
            ] {
                if output == Output::Profile && !has_profile {
                    continue;
                }
                let active = workspace.output == output;
                let text = if active {
                    RichText::new(label).strong().color(style::accent())
                } else {
                    RichText::new(label).color(style::muted())
                };
                let tab = style::tab_button(ui, active, text, true);
                let editable_result_tab = output == Output::Results && has_editable_result;
                let tab = if editable_result_tab {
                    tab.on_hover_text("点击聚焦可编辑结果，也可按 Ctrl+R。")
                } else {
                    tab
                };
                if request_focus && active {
                    tab.request_focus();
                }
                if tab.clicked() {
                    if output == Output::Data && !active {
                        workspace.data_entry_selection = Some(workspace.tab().id);
                    }
                    if editable_result_tab && keyboard_allowed {
                        focus_editable_result = true;
                    }
                    workspace.output = output;
                }
            }
            if workspace.output == Output::Results && starting_f8 {
                let stopping = workspace.editable_read_stopping();
                let label = if stopping {
                    "正在停止…"
                } else {
                    "停止读取"
                };
                if ui
                    .add_enabled(keyboard_allowed && !stopping, style::danger(label))
                    .on_hover_text("停止当前查询页的 F8 读取")
                    .clicked()
                {
                    workspace.cancel_editable_read();
                }
            }
        });
        ui.separator();
        if suppress_modal && has_modal(workspace) {
            ui.label(RichText::new("切换到此连接以继续处理对话框。").color(style::muted()));
            return;
        }
        match workspace.output {
            Output::Results if has_editable_result => {
                workspace.with_editable_editor(|workspace, editor| {
                    if let Some(editor) = editor {
                        editor.use_workspace_view(workspace);
                    }
                    super::row_editor::panel(
                        ui,
                        editor,
                        workspace,
                        keyboard_allowed,
                        focus_editable_result,
                        settings,
                        grid_widths,
                    );
                });
            }
            Output::Results if starting_f8 && workspace.tab().report.is_none() => {
                ui.label(RichText::new(&workspace.status).color(style::muted()));
            }
            Output::Results => results(
                ui,
                workspace,
                keyboard_allowed,
                settings,
                request_focus,
                grid_widths,
            ),
            Output::Profile => super::profiler::panel(ui, workspace),
            Output::Data => workspace.with_data_editor(|workspace, editor| {
                if editor.is_some() {
                    super::row_editor::panel(
                        ui,
                        editor,
                        workspace,
                        keyboard_allowed,
                        request_focus,
                        settings,
                        grid_widths,
                    );
                } else {
                    ui.label(
                        RichText::new("在对象浏览器中选择一张表以查看数据。").color(style::muted()),
                    );
                }
            }),
            Output::Messages => {
                let batch = workspace.tab().batch_messages.as_ref();
                let mut detail = None;
                if let Some(error) = &workspace.tab().command_error
                    && ui
                        .button(RichText::new("查看错误详情…").color(style::error()))
                        .clicked()
                {
                    detail = Some(error.clone());
                }
                if batch.is_none() && workspace.messages.is_empty() {
                    ui.label(RichText::new("暂无消息").color(style::muted()));
                }
                egui::ScrollArea::vertical()
                    .id_salt(("query-messages", workspace.tab().id, batch.is_some()))
                    .stick_to_bottom(batch.is_none())
                    .show(ui, |ui| {
                        if let Some(batch) = batch {
                            let summary = ui.add(
                                egui::Label::new(RichText::new(batch.summary()).strong())
                                    .selectable(true)
                                    .sense(egui::Sense::focusable_noninteractive()),
                            );
                            if request_focus {
                                summary.request_focus();
                                summary.scroll_to_me(Some(Align::Min));
                            }
                            let sets = workspace
                                .tab()
                                .report
                                .as_ref()
                                .map_or(&[][..], |report| report.sets.as_slice());
                            for (index, statement) in batch.statements.iter().enumerate() {
                                ui.add_space(6.0);
                                let failed = statement.error.is_some();
                                let card_frame = style::subtle_card(egui::Margin {
                                    left: 14,
                                    right: 12,
                                    top: 8,
                                    bottom: 8,
                                });
                                let card_response = card_frame.show(ui, |ui| {
                                    ui.set_min_width(ui.available_width());
                                    let title = RichText::new(format!("第 {} 条语句", index + 1));
                                    ui.label(if failed {
                                        title.strong().color(style::error())
                                    } else {
                                        title.strong().color(style::ink())
                                    });
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(&statement.sql)
                                                .monospace()
                                                .color(style::ink()),
                                        )
                                        .selectable(true),
                                    );
                                    if let Some(editable) = &statement.editable_result {
                                        if editable.truncated {
                                            ui.label(format!(
                                                "已保留前 {} 行可编辑结果",
                                                editable.rows
                                            ));
                                        } else {
                                            ui.label(format!("返回 {} 行可编辑结果", editable.rows));
                                        }
                                    }
                                    for (index, set) in
                                        sets[statement.result_sets.clone()].iter().enumerate()
                                    {
                                        let detail = if set.columns.is_empty() {
                                            format!("受影响 {} 行", set.affected_rows)
                                        } else if set.truncated {
                                            format!("显示前 {} 行，结果已截断", set.rows.len())
                                        } else {
                                            format!("返回 {} 行", set.rows.len())
                                        };
                                        ui.label(detail);
                                        if let Some(id) = set.last_insert_id {
                                            ui.label(format!("最后插入 ID {id}"));
                                        }
                                        if let Some(notice) = &set.notice {
                                            ui.label(notice);
                                        }
                                        if let Some(diagnostics) =
                                            statement.diagnostics.results.get(index)
                                        {
                                            if let Some(count) =
                                                diagnostics.warning_count.filter(|&count| count > 0)
                                            {
                                                ui.colored_label(
                                                    style::warning(),
                                                    format!("警告 {count} 条"),
                                                );
                                            }
                                            ui.label(
                                                RichText::new(query_time_label(
                                                    diagnostics.elapsed,
                                                    statement.diagnostics.execution,
                                                ))
                                                .small()
                                                .color(style::muted()),
                                            );
                                        }
                                    }
                                    if statement.omitted_sets > 0 {
                                        ui.label(format!(
                                            "另有 {} 个结果未保留",
                                            statement.omitted_sets
                                        ));
                                    }
                                    if let Some(error) = &statement.error {
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(error).color(style::error()),
                                            )
                                            .selectable(true),
                                        );
                                        if ui
                                            .button(
                                                RichText::new("查看错误详情…").color(style::error()),
                                            )
                                            .clicked()
                                        {
                                            detail = Some(super::error_details::ErrorDetails::new(
                                                &statement.sql,
                                                error,
                                            ));
                                        }
                                    }
                                    if (statement.diagnostics.results.is_empty()
                                        || statement.omitted_sets > 0
                                        || statement.error.is_some())
                                        && let Some(elapsed) = statement.elapsed
                                    {
                                        ui.label(
                                            RichText::new(query_time_label(
                                                elapsed,
                                                statement.diagnostics.execution,
                                            ))
                                            .small()
                                            .color(style::muted()),
                                        );
                                    }
                                });
                                let rect = card_response.response.rect;
                                let bar_color = if failed {
                                    style::error()
                                } else {
                                    style::success()
                                };
                                ui.painter().rect_filled(
                                    egui::Rect::from_min_max(
                                        egui::pos2(rect.left() + 1.0, rect.top() + 6.0),
                                        egui::pos2(rect.left() + 4.0, rect.bottom() - 6.0),
                                    ),
                                    egui::CornerRadius::same(2),
                                    bar_color,
                                );
                            }
                            if !workspace.messages.is_empty() {
                                ui.separator();
                            }
                        }
                        if let Some((last, previous)) = workspace.messages.split_last() {
                            for message in previous {
                                ui.label(message);
                            }
                            let message = ui.add(
                                egui::Label::new(last)
                                    .selectable(true)
                                    .sense(egui::Sense::focusable_noninteractive()),
                            );
                            if request_focus && batch.is_none() {
                                message.request_focus();
                                message.scroll_to_me(None);
                            }
                        }
                    });
                if let Some(detail) = detail {
                    workspace.error_dialog = Some(Box::new(detail));
                }
            }
            Output::History => history::panel(
                ui,
                workspace,
                keyboard_allowed,
                settings.history_font,
                request_focus,
            ),
            Output::Definition => definition(ui, workspace, settings.history_font, request_focus),
        }
    };
    let frame = style::bar(egui::Margin::symmetric(10, 5));
    if full_height {
        host.central(egui::CentralPanel::default().frame(frame), content);
    } else {
        host.bottom(
            egui::TopBottomPanel::bottom(panel_id)
                .resizable(true)
                .default_height(330.0)
                .height_range(125.0..=640.0)
                .frame(frame),
            |ui| {
                style::paint_top_hairline(ui);
                // Keep the splitter's stored height independent of the content's measured size.
                let rect = ui.available_rect_before_wrap();
                ui.allocate_rect(rect, egui::Sense::hover());
                let mut pane = ui.new_child(egui::UiBuilder::new().max_rect(rect));
                pane.shrink_clip_rect(rect);
                content(&mut pane);
            },
        );
    }
}

pub(super) fn query_time_label(elapsed: Duration, execution: Option<Duration>) -> String {
    let total = elapsed.as_millis();
    if let Some(execution) = execution {
        let execution = execution.as_millis();
        let transfer = total.saturating_sub(execution);
        format!("执行 {execution} 毫秒 · 传输 {transfer} 毫秒 · 总耗时 {total} 毫秒")
    } else {
        format!("总耗时 {total} 毫秒")
    }
}

fn definition(ui: &mut egui::Ui, workspace: &mut Workspace, font: TextFont, request_focus: bool) {
    let Some(detail) = &workspace.detail else {
        ui.label(RichText::new("选择数据库对象以查看定义。").color(style::muted()));
        return;
    };
    let mut open = false;
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("{}.{}", detail.object.schema, detail.object.name))
                .strong()
                .color(style::ink()),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            open = ui
                .add_enabled(
                    !detail.ddl.is_empty(),
                    egui::Button::new("在编辑器中打开 DDL"),
                )
                .clicked();
        });
    });
    ui.add_space(4.0);
    egui::ScrollArea::both().show(ui, |ui| {
        if !detail.columns.is_empty() {
            style::card(egui::Margin::same(10)).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                egui::Grid::new("column-metadata")
                    .striped(true)
                    .spacing([16.0, 6.0])
                    .show(ui, |ui| {
                        for label in ["列名", "类型", "允许 NULL", "键类型", "默认值", "其他属性"]
                        {
                            ui.label(RichText::new(label).strong().color(style::muted()));
                        }
                        ui.end_row();
                        for column in &detail.columns {
                            ui.label(RichText::new(&column.name).strong().color(style::ink()));
                            ui.label(
                                RichText::new(&column.data_type)
                                    .monospace()
                                    .color(style::cyan()),
                            );
                            ui.label(
                                RichText::new(if column.nullable { "是" } else { "否" }).color(
                                    if column.nullable {
                                        style::muted()
                                    } else {
                                        style::ink()
                                    },
                                ),
                            );
                            match column.key.as_str() {
                                "PRI" => style::badge(
                                    ui,
                                    "PRI",
                                    style::accent(),
                                    style::accent_soft(),
                                ),
                                "UNI" => style::badge(
                                    ui,
                                    "UNI",
                                    style::cyan(),
                                    style::accent_tint(),
                                ),
                                "MUL" => style::badge(
                                    ui,
                                    "MUL",
                                    style::muted(),
                                    style::chrome(),
                                ),
                                other => {
                                    ui.label(RichText::new(other).color(style::muted()));
                                }
                            }
                            ui.label(
                                RichText::new(column.default_value.as_deref().unwrap_or("—"))
                                    .monospace()
                                    .color(style::muted()),
                            );
                            ui.label(RichText::new(&column.extra).small().color(style::muted()));
                            ui.end_row();
                        }
                    });
            });
            ui.add_space(8.0);
        }
        if let Some(error) = &detail.ddl_error {
            ui.colored_label(style::error(), error);
        }
        let mut ddl = detail.ddl.as_str();
        let text = ui.add(
            TextEdit::multiline(&mut ddl)
                .font(font.id())
                .desired_width(f32::INFINITY),
        );
        if request_focus {
            text.request_focus();
            text.scroll_to_me(None);
        }
    });
    if open {
        let mut ddl = detail.ddl.clone();
        if matches!(
            detail.object.kind,
            ObjectKind::Procedure | ObjectKind::Function | ObjectKind::Trigger | ObjectKind::Event
        ) {
            let mut delimiter = "$$".to_owned();
            while ddl.contains(&delimiter) {
                delimiter.push('$');
            }
            ddl = format!("DELIMITER {delimiter}\n{ddl}{delimiter}\nDELIMITER ;\n");
        }
        workspace.new_tab(ddl, None);
    }
}

fn results(
    ui: &mut egui::Ui,
    workspace: &mut Workspace,
    keyboard_allowed: bool,
    settings: EditorSettings,
    request_focus: bool,
    grid_widths: &mut GridWidths,
) {
    let mut export = None;
    let mut duplicate = None;
    let mut mode = workspace.result_mode();
    let last_structured_mode = workspace.last_structured_result_mode();
    let copy_headers = workspace.copy_column_headers;
    let workspace_key = workspace.key;
    let show_data_info = workspace.show_data_info;
    let tab = workspace.tab_mut();
    let Some(report) = &tab.report else {
        ui.label(RichText::new("执行查询后查看结果。").color(style::muted()));
        return;
    };
    let displayed: Vec<_> = report
        .sets
        .iter()
        .enumerate()
        .filter_map(|(index, set)| {
            (tab.batch_messages.is_none() || !set.columns.is_empty()).then_some(index)
        })
        .collect();
    if displayed.is_empty() {
        ui.label("没有结果集");
        return;
    }
    let current = displayed
        .iter()
        .position(|&index| index == tab.set)
        .unwrap_or(0);
    tab.set = displayed[current];
    if keyboard_allowed
        && !report.sets[tab.set].columns.is_empty()
        && !egui::Popup::is_any_open(ui.ctx())
        && let Some(next) = ui.input_mut(|input| {
            super::consume_result_mode_shortcut(input, mode, last_structured_mode, true)
        })
    {
        mode = next;
    }
    let mut filter_changed = false;
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(5.0, 4.0);
        ui.spacing_mut().button_padding = egui::vec2(7.0, 2.5);
        ui.spacing_mut().interact_size.y = 22.0;
        ui.horizontal_wrapped(|ui| {
            egui::ComboBox::from_id_salt(("result-set", workspace_key, tab.id))
                .width(100.0)
                .selected_text(format!("结果集 {} / {}", current + 1, displayed.len()))
                .show_ui(ui, |ui| {
                    for (offset, &index) in displayed.iter().enumerate() {
                        let set = &report.sets[index];
                        let summary = if set.columns.is_empty() {
                            format!("影响 {} 行", set.affected_rows)
                        } else {
                            format!("{} 行", set.rows.len())
                        };
                        ui.selectable_value(
                            &mut tab.set,
                            index,
                            format!("{}   {summary}", offset + 1),
                        );
                    }
                });
            let set = &report.sets[tab.set];
            if !set.columns.is_empty() {
                ui.separator();
                result_view_controls(ui, &mut mode, true);
                ui.separator();
                let view = &mut tab.result_views.sets.entry(tab.set).or_default().grid;
                let search_id =
                    ui.make_persistent_id(("result-search", workspace_key, tab.id, tab.set));
                if ui
                    .add(
                        TextEdit::singleline(&mut view.search)
                            .id(search_id)
                            .desired_width(140.0)
                            .hint_text("筛选已加载行"),
                    )
                    .changed()
                {
                    view.invalidate();
                }
                export = result_export_menu(ui, true);
                filter_changed = view.prepare(set);
                grid::controls(ui, set, view, copy_headers, mode != ResultMode::Grid, true);
                filter_changed |= view.prepare(set);
            }
            if set.truncated {
                ui.label(RichText::new("结果已截断").small().color(style::warning()));
            }
            if report.omitted_sets > 0 {
                ui.label(
                    RichText::new(format!("已省略 {} 个结果集", report.omitted_sets))
                        .small()
                        .color(style::warning()),
                );
            }
        });
    });
    let set = &report.sets[tab.set];
    let statement = show_data_info
        .then_some(set.source_sql.as_deref())
        .flatten();
    let elapsed = tab
        .batch_messages
        .as_ref()
        .and_then(|batch| {
            batch
                .statements
                .iter()
                .find(|statement| statement.result_sets.contains(&tab.set))
        })
        .and_then(|statement| {
            statement
                .diagnostics
                .results
                .get(tab.set - statement.result_sets.start)
                .map(|result| result.elapsed)
                .or(statement.elapsed)
        })
        .unwrap_or(report.elapsed)
        .as_millis();
    if let Some(notice) = &set.notice {
        ui.add(egui::Label::new(RichText::new(notice).small().color(style::muted())).truncate())
            .on_hover_text(notice);
    }
    if set.columns.is_empty() {
        with_statement_info(ui, statement, |ui| {
            ui.label(format!("影响 {} 行", set.affected_rows));
            if let Some(id) = set.last_insert_id {
                ui.label(format!("最近一次插入 ID {id}"));
            }
        });
    } else {
        let ResultView {
            grid: view,
            record,
            grid_row,
            text,
        } = tab.result_views.sets.entry(tab.set).or_default();
        ui.push_id((workspace_key, tab.id, tab.set), |ui| {
            if filter_changed {
                text.reset_match();
            }
            let summary = format!(
                "显示 {} / {} 行 · 耗时 {elapsed} 毫秒",
                view.visible().len(),
                set.rows.len(),
            );
            with_data_info(ui, &summary, statement, |ui| match mode {
                ResultMode::Grid => grid::show(
                    ui,
                    set,
                    view,
                    copy_headers,
                    keyboard_allowed,
                    request_focus,
                    grid_widths,
                ),
                ResultMode::Text => show_text_result(
                    ui,
                    set,
                    view.visible(),
                    text,
                    copy_headers,
                    keyboard_allowed,
                    request_focus,
                ),
                ResultMode::Form => {
                    let selected = view.selected_source_row();
                    if selected != *grid_row {
                        *grid_row = selected;
                        if let Some(source) = selected {
                            record.select(source);
                        }
                    }
                    form::show(ui, set, view, record, copy_headers, request_focus);
                }
            });
            grid::dialogs(ui.ctx(), set, view, settings);
            if view.take_export_request() && export.is_none() {
                export = Some(transfer::ExportKind::Csv);
            }
            if let Some(row) = view.take_duplicate_row()
                && let (Some(source), Some(cells)) = (&set.duplicate_source, set.rows.get(row))
            {
                duplicate = Some(Command::DuplicateResultRow {
                    source: source.clone(),
                    cells: cells.clone(),
                });
            }
        });
    }
    if let Some(kind) = export {
        tab.result_views.export = Some((tab.set, ExportOptions::new(kind, set.columns.len())));
    }
    if let Some((index, options)) = &mut tab.result_views.export
        && *index == tab.set
    {
        let id = egui::Id::new(("result-export", workspace_key, tab.id, *index));
        let (close, result) = result_export_dialog(ui.ctx(), id, options, set.into(), false);
        let structure_only = options.structure_only();
        if close {
            tab.result_views.export = None;
        }
        report_result_export(workspace, structure_only, result);
    }
    workspace.set_result_mode(mode);
    if let Some(command) = duplicate {
        if workspace.busy() {
            workspace.message("连接正忙，请稍后复制记录。", true);
        } else {
            workspace.send(command, "正在核对所选记录的原值…");
        }
    }
}

pub(super) fn request_current_result_export(workspace: &mut Workspace) -> bool {
    let tab = workspace.tab_mut();
    if tab.editable_editor.is_some() || tab.result_views.export.is_some() {
        return false;
    }
    let Some(set) = tab
        .report
        .as_ref()
        .and_then(|report| report.sets.get(tab.set))
        .filter(|set| !set.columns.is_empty())
    else {
        return false;
    };
    tab.result_views.export = Some((
        tab.set,
        ExportOptions::new(transfer::ExportKind::Csv, set.columns.len()),
    ));
    true
}

pub(super) fn result_export_menu(ui: &mut egui::Ui, enabled: bool) -> Option<transfer::ExportKind> {
    let mut selected = None;
    style::icon_menu_button(ui, style::ToolbarIcon::Export, enabled, "导出结果…", |ui| {
        for format in RESULT_FORMATS {
            if ui.button(format!("{}…", format.label())).clicked() {
                selected = Some(format);
                ui.close();
            }
        }
    });
    selected
}

pub(super) fn report_result_export(
    workspace: &mut Workspace,
    structure_only: bool,
    result: Option<Result<transfer::TransferSummary, String>>,
) {
    match result {
        Some(Ok(_)) if structure_only => workspace.message("已导出建表语句", false),
        Some(Ok(summary)) => workspace.message(format!("已导出 {} 行", summary.rows), false),
        Some(Err(error)) => workspace.message(format!("导出失败：{error}"), true),
        None => {}
    }
}

pub(super) fn with_statement_info(
    ui: &mut egui::Ui,
    statement: Option<&str>,
    body: impl FnOnce(&mut egui::Ui),
) {
    with_data_info(ui, "", statement, body);
}

fn with_data_info(
    ui: &mut egui::Ui,
    summary: &str,
    statement: Option<&str>,
    body: impl FnOnce(&mut egui::Ui),
) {
    if !summary.is_empty() || statement.is_some_and(|sql| !sql.trim().is_empty()) {
        let gap = ui.spacing().item_spacing.y;
        let mut rect = ui.available_rect_before_wrap();
        rect.max.y = (rect.max.y - data_info_line_height(ui) - gap).max(rect.min.y);
        ui.allocate_rect(rect, egui::Sense::hover());
        let mut content = ui.new_child(egui::UiBuilder::new().max_rect(rect));
        content.shrink_clip_rect(rect);
        body(&mut content);
        ui.painter().hline(
            rect.x_range(),
            rect.bottom() + gap * 0.5,
            egui::Stroke::new(1.0_f32, style::border()),
        );
        data_info_line(ui, summary, statement);
    } else {
        body(ui);
    }
}

pub(super) fn data_info_line_height(ui: &egui::Ui) -> f32 {
    ui.spacing()
        .interact_size
        .y
        .max(ui.text_style_height(&egui::TextStyle::Button) + 2.0 * ui.spacing().button_padding.y)
        + ui.spacing().item_spacing.y
}

pub(super) fn data_info_line(ui: &mut egui::Ui, summary: &str, statement: Option<&str>) {
    let height = data_info_line_height(ui) - ui.spacing().item_spacing.y;
    ui.horizontal(|ui| {
        ui.set_min_height(height);
        if !summary.is_empty() {
            ui.add(egui::Label::new(RichText::new(summary).small().color(style::muted())).truncate())
                .on_hover_text(summary);
        }
        let Some(statement) = statement.filter(|sql| !sql.trim().is_empty()) else {
            return;
        };
        if !summary.is_empty() {
            ui.separator();
        }
        ui.menu_button("查看 SQL", |ui| {
            if ui.button("复制 SQL").clicked() {
                ui.ctx().copy_text(statement.to_owned());
                ui.close();
            }
            let width = (ui.ctx().content_rect().width() - 48.0).clamp(180.0, 560.0);
            egui::ScrollArea::both()
                .id_salt("full-result-sql")
                .max_width(width)
                .max_height(240.0)
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(statement).monospace())
                            .wrap_mode(egui::TextWrapMode::Extend)
                            .selectable(true),
                    );
                });
        });
        let mut chars = statement.trim().chars();
        let mut preview = chars
            .by_ref()
            .take(180)
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .collect::<String>();
        if chars.next().is_some() {
            preview.push('…');
        }
        ui.add(egui::Label::new(RichText::new(preview).small().color(style::muted())).truncate());
    });
}

pub(super) fn result_export_dialog(
    ctx: &egui::Context,
    id: egui::Id,
    options: &mut ExportOptions,
    input: transfer::ResultExport<'_>,
    table_page: bool,
) -> (bool, Option<Result<transfer::TransferSummary, String>>) {
    let (heading, file_stem, row_label, truncated) = if table_page {
        (
            "导出当前页数据",
            "表数据",
            "当前页已加载",
            "此表还有尚未加载的行。",
        )
    } else {
        (
            "导出查询结果",
            "查询结果",
            "当前结果集已加载",
            "查询结果已截断。",
        )
    };
    let mut close = false;
    let mut start = false;
    let modal = style::modal(ctx, id).show(ctx, |ui| {
        ui.set_min_width(475.0);
        ui.heading(heading);
        ui.label(format!("{row_label} {} 行。", input.row_count()));
        if input.truncated() {
            ui.colored_label(style::error(), truncated);
        }
        options.format(ui, &RESULT_FORMATS);
        ui.separator();
        let fields = options.field_picker(
            ui,
            (0..input.column_count()).map(|index| (index, input.column_label(index))),
        );
        let file = options.file_settings(ui, file_stem, input.has_declared_widths());
        let target = options.result_target(ui);
        ui.horizontal(|ui| {
            start = ui
                .add_enabled(fields && file && target, style::primary("开始导出"))
                .clicked();
            close = ui.button("取消").clicked();
        });
    });
    if close || modal.should_close() {
        return (true, None);
    }
    if start && let Some(path) = &options.path {
        let fields: Vec<_> = options
            .selected
            .iter()
            .enumerate()
            .filter_map(|(index, &selected)| selected.then_some(index))
            .collect();
        let result = input.export(path, &fields, &options.settings(None));
        return (true, Some(result));
    }
    (false, None)
}

fn show_text_result(
    ui: &mut egui::Ui,
    set: &ResultSet,
    rows: &[usize],
    text: &mut TextRows,
    copy_headers: bool,
    keyboard_allowed: bool,
    request_focus: bool,
) {
    text.show(
        ui,
        rows.len(),
        set.columns.len(),
        |column| set.columns[column].as_str(),
        |row| set.rows[rows[row]].as_slice(),
        copy_headers,
        keyboard_allowed,
        request_focus,
    );
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

pub(super) fn display_cell(cell: &Cell, preview: bool) -> String {
    match cell {
        Cell::Null => "NULL".into(),
        Cell::Text(text) if preview => {
            let mut chars = text.chars();
            let mut view: String = chars.by_ref().take(300).collect();
            if chars.next().is_some() {
                view.push('…');
            }
            view.replace(['\r', '\n'], " ↵ ")
        }
        Cell::Text(text) => text.clone(),
        Cell::Binary(bytes) if preview && bytes.len() > 72 => {
            format!("0x{}…", hex(&bytes[..72]))
        }
        Cell::Binary(bytes) => format!("0x{}", hex(bytes)),
    }
}

pub(super) fn markup(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for char in text.chars() {
        match char {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            '\r' => escaped.push_str("&#13;"),
            char if xml_char(char) => escaped.push(char),
            char => {
                let _ = write!(escaped, "\\u{{{:04X}}}", char as u32);
            }
        }
    }
    escaped
}

fn xml_char(char: char) -> bool {
    matches!(char, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::database::{DbObject, QueryReport, TablePage};

    use super::*;

    fn visible_text(workspace: &mut Workspace) -> BTreeMap<String, f32> {
        fn collect(shape: &egui::Shape, output: &mut BTreeMap<String, f32>) {
            match shape {
                egui::Shape::Text(text) => {
                    output.insert(text.galley.text().to_owned(), text.pos.y);
                }
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        collect(shape, output);
                    }
                }
                _ => {}
            }
        }

        let ctx = egui::Context::default();
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 420.0),
                )),
                ..Default::default()
            },
            |ctx| {
                panel(
                    ctx,
                    workspace,
                    true,
                    true,
                    EditorSettings::default(),
                    false,
                    &mut GridWidths::new(false),
                );
            },
        );
        let mut text = BTreeMap::new();
        for shape in output.shapes {
            collect(&shape.shape, &mut text);
        }
        text
    }

    #[test]
    fn batch_messages_show_the_original_sql_results_and_errors_in_order() {
        use crate::app::state::{BatchMessages, StatementMessage};
        use crate::database::{QueryDiagnostics, ResultDiagnostics};

        let mut workspace = Workspace::new("批量执行消息测试".into(), None);
        workspace.output = Output::Messages;
        workspace.tab_mut().report = Some(QueryReport {
            sets: vec![ResultSet {
                source_sql: None,
                columns: Vec::new(),
                column_hints: Vec::new(),
                column_origins: Vec::new(),
                excel_types: Vec::new(),
                export_columns: Vec::new(),
                duplicate_source: None,
                notice: None,
                rows: Vec::new(),
                affected_rows: 2,
                last_insert_id: None,
                truncated: false,
            }],
            ..Default::default()
        });
        let successful_sql = "UPDATE items SET value = 2";
        let failed_sql = "SELECT missing_column FROM items";
        let error = "数据库错误 1054";
        workspace.tab_mut().batch_messages = Some(BatchMessages {
            total: 2,
            statements: vec![
                StatementMessage {
                    sql: successful_sql.into(),
                    result_sets: 0..1,
                    omitted_sets: 0,
                    elapsed: Some(Duration::from_millis(8)),
                    diagnostics: QueryDiagnostics {
                        execution: Some(Duration::from_millis(3)),
                        results: vec![ResultDiagnostics {
                            elapsed: Duration::from_millis(8),
                            warning_count: Some(1),
                        }],
                    },
                    editable_result: None,
                    error: None,
                },
                StatementMessage {
                    sql: failed_sql.into(),
                    result_sets: 1..1,
                    omitted_sets: 0,
                    elapsed: Some(Duration::from_millis(12)),
                    diagnostics: QueryDiagnostics::default(),
                    editable_result: None,
                    error: Some(error.into()),
                },
            ],
        });
        let visible = visible_text(&mut workspace);
        let positions = [
            "已提交 2/2 条语句，1 条出错。",
            "第 1 条语句",
            successful_sql,
            "受影响 2 行",
            "警告 1 条",
            "执行 3 毫秒 · 传输 5 毫秒 · 总耗时 8 毫秒",
            "第 2 条语句",
            failed_sql,
            error,
            "总耗时 12 毫秒",
        ]
        .map(|text| visible[text]);
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn result_info_follows_the_selected_statement_and_hides_independently() {
        let first = "SELECT 1 AS first_result";
        let second = "SELECT 2 AS second_result";
        let mut workspace = Workspace::new("结果信息测试".into(), None);
        workspace.tab_mut().report = Some(QueryReport {
            sets: [
                (first, Some("first_result")),
                (second, Some("second_result")),
                ("UPDATE items SET val = 3", None),
            ]
            .into_iter()
            .map(|(source, column)| ResultSet {
                source_sql: Some(source.into()),
                columns: column.into_iter().map(str::to_owned).collect(),
                column_hints: Vec::new(),
                column_origins: Vec::new(),
                excel_types: Vec::new(),
                export_columns: Vec::new(),
                duplicate_source: None,
                notice: column.is_none().then(|| "更新提示".into()),
                rows: column
                    .into_iter()
                    .map(|_| vec![Cell::Text("sample_value".into())])
                    .collect(),
                affected_rows: if column.is_none() { 3 } else { 0 },
                last_insert_id: None,
                truncated: false,
            })
            .collect(),
            elapsed: Duration::from_millis(42),
            error: None,
            continue_after_error: false,
            diagnostics: Default::default(),
            omitted_sets: 0,
            active_database: None,
        });
        let summary = "显示 1 / 1 行 · 耗时 42 毫秒";

        workspace.show_data_info = true;
        let visible = visible_text(&mut workspace);
        assert!(visible[first] > visible["first_result"]);
        assert!(visible.contains_key(summary));
        assert!(!visible.contains_key(second));

        workspace.tab_mut().set = 1;
        let visible = visible_text(&mut workspace);
        assert!(visible[second] > visible["second_result"]);
        assert!(!visible.contains_key(first));

        workspace.show_data_info = false;
        let hidden = visible_text(&mut workspace);
        assert!(hidden.contains_key("second_result"));
        assert!(!hidden.contains_key(second));
        assert!(hidden.contains_key(summary));

        workspace.tab_mut().set = 2;
        let update = visible_text(&mut workspace);
        assert!(update.contains_key("影响 3 行"));
        assert!(update.contains_key("更新提示"));
        assert!(!update.contains_key("UPDATE items SET val = 3"));
    }

    #[test]
    fn short_results_preserve_the_default_and_dragged_output_split() {
        let ctx = egui::Context::default();
        style::apply(&ctx);
        let mut workspace = Workspace::new("输出分隔条测试".into(), None);
        let mut grid_widths = GridWidths::new(false);
        let report = QueryReport {
            sets: vec![ResultSet {
                source_sql: Some("SELECT name FROM items ORDER BY id LIMIT 4".into()),
                columns: vec!["name".into()],
                column_hints: Vec::new(),
                column_origins: Vec::new(),
                excel_types: Vec::new(),
                export_columns: Vec::new(),
                duplicate_source: None,
                notice: None,
                rows: ["alpha", "beta", "gamma", "delta"]
                    .into_iter()
                    .map(|name| vec![Cell::Text(name.into())])
                    .collect(),
                affected_rows: 0,
                last_insert_id: None,
                truncated: false,
            }],
            ..Default::default()
        };
        let mut frame = |workspace: &mut Workspace, events| {
            let mut split = 0.0;
            let rendered = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1223.0, 821.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    panel(
                        ctx,
                        workspace,
                        false,
                        true,
                        EditorSettings::default(),
                        false,
                        &mut grid_widths,
                    );
                    split = ctx.available_rect().bottom();
                },
            );
            (split, rendered)
        };

        let (initial, _) = frame(&mut workspace, vec![]);
        workspace.tab_mut().report = Some(report.clone());
        let (after_query, rendered) = frame(&mut workspace, vec![]);
        assert_eq!(after_query, initial);
        assert!(rendered.shapes.iter().any(|shape| {
            matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text() == "delta" && shape.clip_rect.contains(text.pos))
        }));
        assert_eq!(frame(&mut workspace, vec![]).0, initial);

        let start = egui::pos2(600.0, initial);
        let end = start + egui::vec2(0.0, -55.0);
        let press = |pos, pressed| egui::Event::PointerButton {
            pos,
            pressed,
            button: egui::PointerButton::Primary,
            modifiers: egui::Modifiers::NONE,
        };
        frame(
            &mut workspace,
            vec![egui::Event::PointerMoved(start), press(start, true)],
        );
        frame(&mut workspace, vec![egui::Event::PointerMoved(end)]);
        frame(&mut workspace, vec![press(end, false)]);
        let (dragged, _) = frame(&mut workspace, vec![]);
        assert_eq!(dragged, end.y);

        workspace.tab_mut().report = None;
        assert_eq!(frame(&mut workspace, vec![]).0, dragged);
        workspace.tab_mut().report = Some(report);
        assert_eq!(frame(&mut workspace, vec![]).0, dragged);
        assert_eq!(frame(&mut workspace, vec![]).0, dragged);
    }

    #[test]
    fn editable_result_panel_follows_the_active_query_tab() {
        let mut workspace = Workspace::new("可编辑结果测试".into(), None);
        workspace.tab_mut().editable_editor =
            Some(super::super::row_editor::RowEditor::from_query(
                0,
                TablePage {
                    object: DbObject {
                        schema: "sample".into(),
                        name: "editable_items".into(),
                        kind: ObjectKind::Table,
                    },
                    columns: vec![],
                    column_labels: vec![],
                    primary_key: vec![],
                    row_key: vec![],
                    rows: vec![],
                    offset: 0,
                    has_more: false,
                    can_insert: false,
                    edit_error: None,
                    query_timing: Some(crate::database::QueryTiming {
                        execution: Some(Duration::from_millis(4)),
                        elapsed: Duration::from_millis(11),
                    }),
                },
                "SELECT * FROM sample.editable_items".into(),
                10,
            ));

        let first = visible_text(&mut workspace);
        assert!(first.contains_key("重新读取"));
        assert!(first.contains_key("sample.editable_items"));
        assert!(first.contains_key("执行 4 毫秒 · 传输 7 毫秒 · 总耗时 11 毫秒"));

        workspace.new_tab("SELECT 2".into(), None);
        let second = visible_text(&mut workspace);
        assert!(second.contains_key("执行查询后查看结果。"));
        assert!(!second.contains_key("sample.editable_items"));
        assert!(!second.contains_key("执行 4 毫秒 · 传输 7 毫秒 · 总耗时 11 毫秒"));

        workspace.select_tab(0);
        assert!(visible_text(&mut workspace).contains_key("sample.editable_items"));
    }
}
