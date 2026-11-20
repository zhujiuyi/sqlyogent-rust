use std::collections::BTreeSet;

use eframe::egui::{self, Align, Layout, RichText, Sense};
use egui_extras::{Column, TableBuilder};

use crate::database::{Cell, ColumnOrigin, ResultSet, transfer::CsvOptions};

use super::super::cell_value::{Inspector, Outcome};
use super::super::preferences::{EditorSettings, GridColumns, GridWidths};
use super::super::row_editor::text_entry;
use super::super::style;
use super::clipboard;
use super::selection::{GridSelection, Hit, Point, PointerInteraction, Selection, Shortcut};
use super::values::{self, ClipboardFormat, Operator, Sort};
use super::{JoinedGridAction, display_cell, hex};

pub(in crate::app) const GRID_ROW_HEIGHT: f32 = 23.0;
pub(in crate::app) const GRID_HEADER_HEIGHT: f32 = 27.0;
pub(in crate::app) const GRID_ROW_GAP: f32 = 1.0;
const ROW_CHECK_WIDTH: f32 = 36.0;
const NULL: Cell = Cell::Null;

pub(in crate::app) fn paint_grid_header(ui: &egui::Ui, height: f32) {
    let top = ui.cursor().min;
    let rect = egui::Rect::from_min_max(top, egui::pos2(ui.clip_rect().right(), top.y + height));
    ui.painter().rect_filled(rect, 0.0, style::PANEL);
    ui.painter().hline(
        rect.x_range(),
        rect.bottom(),
        egui::Stroke::new(1.0_f32, style::BORDER),
    );
}

pub(in crate::app) fn grid_label(ui: &mut egui::Ui, text: RichText, right_aligned: bool) {
    let layout = if right_aligned {
        Layout::right_to_left(Align::Center)
    } else {
        Layout::left_to_right(Align::Center)
    };
    let rect = ui.max_rect().shrink2(egui::vec2(5.0, 0.0));
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect).layout(layout), |ui| {
        // Let the cell layout align the widget while keeping the galley's origin at its left edge.
        ui.add(
            egui::Label::new(text)
                .halign(Align::Min)
                .truncate()
                .selectable(false),
        );
    });
}

pub(in crate::app) fn paint_cell_outline(ui: &egui::Ui) {
    ui.painter().rect_stroke(
        ui.max_rect().shrink(0.5),
        2.0,
        egui::Stroke::new(1.0_f32, style::ACCENT),
        egui::StrokeKind::Inside,
    );
}

pub(in crate::app) fn grid_cell(ui: &mut egui::Ui, cell: &Cell, numeric: bool, current: bool) {
    let text = RichText::new(display_cell(cell, true)).monospace();
    let text = if matches!(cell, Cell::Null) {
        text.italics().color(style::MUTED)
    } else {
        text.color(style::INK)
    };
    if current {
        paint_cell_outline(ui);
    }
    grid_label(ui, text, numeric);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Predicate {
    row: usize,
    column: usize,
    operator: Operator,
}

impl Predicate {
    fn value(self, set: &ResultSet) -> &Cell {
        get_cell(set, self.row, self.column)
    }

    fn matches(self, set: &ResultSet, row: usize) -> bool {
        self.operator
            .matches(get_cell(set, row, self.column), self.value(set))
    }

    fn label(self, set: &ResultSet) -> String {
        let value = self.value(set);
        if matches!(value, Cell::Null) {
            format!(
                "{} {}",
                set.columns[self.column],
                self.operator.label(value)
            )
        } else {
            format!(
                "{} {} {}",
                set.columns[self.column],
                self.operator.label(value),
                display_cell(value, true)
            )
        }
    }
}

#[derive(Default)]
pub(super) struct View {
    pub(super) search: String,
    inspection: Option<Inspector>,
    predicates: Vec<Predicate>,
    sorts: Vec<Sort>,
    rows: Option<RowOrder>,
    selection: GridSelection,
    column_widths: GridColumns,
    clipboard: Option<clipboard::Dialog>,
    duplicate_row: Option<usize>,
    export_requested: bool,
    checked: BTreeSet<usize>,
    check_anchor: Option<usize>,
    focus_scroll: Option<Point>,
}

struct RowOrder {
    all: Vec<usize>,
    visible: Vec<usize>,
}

impl View {
    pub(super) fn take_export_request(&mut self) -> bool {
        std::mem::take(&mut self.export_requested)
    }

    pub(super) fn has_modal(&self) -> bool {
        self.inspection.is_some() || self.clipboard.is_some()
    }

    pub(super) fn selected_source_row(&self) -> Option<usize> {
        Some(self.selected_source_point()?.row)
    }

    pub(super) fn selected_source_point(&self) -> Option<Point> {
        let head = self.selection.current()?.head;
        let row = *self.rows.as_ref()?.visible.get(head.row)?;
        Some(Point {
            row,
            column: head.column,
        })
    }

    pub(super) fn selected_source_rows(&self) -> &[usize] {
        self.selection
            .current()
            .and_then(|selection| self.rows.as_ref()?.visible.get(selection.rows()))
            .unwrap_or_default()
    }

    pub(super) fn adjacent_source_point(&self, forward: bool) -> Option<Point> {
        let head = self.selection.current()?.head;
        let adjacent = if forward {
            head.row.checked_add(1)?
        } else {
            head.row.checked_sub(1)?
        };
        let row = *self.rows.as_ref()?.visible.get(adjacent)?;
        Some(Point {
            row,
            column: head.column,
        })
    }

    pub(super) fn checked_source_rows(&self) -> Vec<usize> {
        let Some(order) = &self.rows else {
            return Vec::new();
        };
        order
            .visible
            .iter()
            .copied()
            .filter(|row| self.checked.contains(row))
            .collect()
    }

    pub(super) fn clear_checks(&mut self) {
        self.checked.clear();
        self.check_anchor = None;
    }

    fn check_row(
        &mut self,
        position: usize,
        checked: bool,
        extend: bool,
        can_check: &dyn Fn(usize) -> bool,
    ) {
        let Some(order) = &self.rows else { return };
        let Some(&source) = order.visible.get(position).filter(|&&row| can_check(row)) else {
            return;
        };
        let anchor = self
            .check_anchor
            .filter(|_| extend)
            .and_then(|anchor| order.visible.iter().position(|&row| row == anchor))
            .unwrap_or_else(|| {
                self.check_anchor = Some(source);
                position
            });
        for &row in &order.visible[position.min(anchor)..=position.max(anchor)] {
            if can_check(row) {
                if checked {
                    self.checked.insert(row);
                } else {
                    self.checked.remove(&row);
                }
            }
        }
    }

    pub(super) fn focus_source_point(&mut self, set: &ResultSet, point: Point) -> bool {
        self.prepare(set);
        if point.column >= set.columns.len() {
            return false;
        }
        let Some(row) = self.visible().iter().position(|&row| row == point.row) else {
            return false;
        };
        let point = Point { row, ..point };
        self.selection.select(
            Hit {
                point,
                row_header: false,
            },
            false,
        );
        self.selection.request_focus();
        self.focus_scroll = Some(point);
        true
    }

    pub(super) fn take_duplicate_row(&mut self) -> Option<usize> {
        self.duplicate_row.take()
    }

    pub(super) fn inspect_cell(&mut self, set: &ResultSet, source_row: usize, column: usize) {
        if let Some(name) = set.columns.get(column) {
            self.inspection = Some(Inspector::read(
                name.clone(),
                get_cell(set, source_row, column),
            ));
        }
    }

    pub(super) fn invalidate(&mut self) {
        self.rows = None;
        self.selection.clear();
        self.focus_scroll = None;
        self.column_widths.invalidate_automatic();
    }

    pub(super) fn request_focus(&mut self) {
        self.selection.request_focus();
    }

    pub(super) fn prepare(&mut self, set: &ResultSet) -> bool {
        if self.rows.is_some() {
            return false;
        }
        let search = self.search.to_lowercase();
        let mut all = (0..set.rows.len()).collect::<Vec<_>>();
        if !self.sorts.is_empty() {
            values::sort_rows(set, &mut all, &self.sorts);
        }
        let visible = all
            .iter()
            .copied()
            .filter(|&row| {
                (search.is_empty()
                    || set.rows[row]
                        .iter()
                        .any(|cell| matches_search(cell, &search)))
                    && self
                        .predicates
                        .iter()
                        .all(|predicate| predicate.matches(set, row))
            })
            .collect();
        self.rows = Some(RowOrder { all, visible });
        true
    }

    pub(super) fn visible(&self) -> &[usize] {
        &self.order().visible
    }

    fn order(&self) -> &RowOrder {
        self.rows.as_ref().expect("prepared result rows")
    }

    fn sort(&mut self, column: usize, additional: bool) {
        if !additional && (self.sorts.len() != 1 || self.sorts[0].column != column) {
            self.sorts.clear();
        }
        if let Some(index) = self.sorts.iter().position(|sort| sort.column == column) {
            if self.sorts[index].descending {
                self.sorts.remove(index);
            } else {
                self.sorts[index].descending = true;
            }
        } else {
            self.sorts.push(Sort {
                column,
                descending: false,
            });
        }
        self.invalidate();
    }

    fn filter(&mut self, predicate: Predicate) {
        self.predicates.retain(|current| {
            current.column != predicate.column || current.operator != predicate.operator
        });
        self.predicates.push(predicate);
        self.invalidate();
    }
}

fn matches_search(cell: &Cell, search: &str) -> bool {
    match cell {
        Cell::Null => "null".contains(search),
        Cell::Text(text) => text.to_lowercase().contains(search),
        Cell::Binary(bytes) => hex(bytes).contains(search),
    }
}

fn get_cell(set: &ResultSet, row: usize, column: usize) -> &Cell {
    set.rows
        .get(row)
        .and_then(|row| row.get(column))
        .unwrap_or(&NULL)
}

#[derive(Clone, Copy)]
enum CopyScope {
    Selection,
    Visible,
    All,
}

#[derive(Clone, Copy)]
enum Copy {
    Cell(usize, usize),
    Area(CopyScope, ClipboardFormat),
}

impl Copy {
    fn text(self, set: &ResultSet, view: &View, headers: bool) -> Option<String> {
        let order = view.order();
        let columns = 0..set.columns.len();
        match self {
            Self::Cell(row, column) => Some(values::cell_text(get_cell(set, row, column))),
            Self::Area(CopyScope::Selection, format) => selected_text(
                set,
                &order.visible,
                view.selection.current()?,
                headers,
                format,
            ),
            Self::Area(CopyScope::Visible, format) => Some(values::clipboard_rows(
                set,
                &order.visible,
                columns,
                headers,
                format,
            )),
            Self::Area(CopyScope::All, format) => Some(values::clipboard_rows(
                set, &order.all, columns, headers, format,
            )),
        }
    }

    fn apply(self, ctx: &egui::Context, set: &ResultSet, view: &View, headers: bool) {
        if let Some(text) = self.text(set, view, headers) {
            ctx.copy_text(text);
        }
    }
}

fn selected_text(
    set: &ResultSet,
    rows: &[usize],
    selection: Selection,
    headers: bool,
    format: ClipboardFormat,
) -> Option<String> {
    Some(values::clipboard_rows(
        set,
        rows.get(selection.rows())?,
        selection.columns(set.columns.len()),
        headers,
        format,
    ))
}

#[derive(Clone, Copy)]
enum Edit {
    Copy(Copy),
    ConfigureCopy(FormattedCopy),
    Inspect(usize, usize),
    DuplicateRow(usize),
    Export,
    SelectAll,
    ClearSelection,
    Filter(Predicate),
    ClearFilters,
}

#[derive(Clone, Copy)]
enum FormattedCopy {
    SelectedRows,
    VisibleRows,
    AllRows,
    CurrentRow(usize),
}

impl FormattedCopy {
    fn open(self, ctx: &egui::Context, set: &ResultSet, view: &mut View, headers: bool) {
        let (title, rows) = match self {
            Self::SelectedRows => {
                let Some(rows) = view
                    .selection
                    .current()
                    .and_then(|selection| view.visible().get(selection.rows()))
                else {
                    return;
                };
                ("复制选中行", rows.to_vec())
            }
            Self::VisibleRows => ("复制可见行", view.visible().to_vec()),
            Self::AllRows => ("复制全部已加载行", view.order().all.clone()),
            Self::CurrentRow(row) => ("复制当前记录", vec![row]),
        };
        if !rows.is_empty() && !set.columns.is_empty() {
            let previous = ctx
                .data(|data| data.get_temp::<CsvOptions>(egui::Id::new("result-clipboard-format")));
            view.clipboard = Some(clipboard::Dialog::new(
                title,
                rows,
                0..set.columns.len(),
                headers,
                previous.as_ref(),
            ));
        }
    }
}

pub(super) fn controls(
    ui: &mut egui::Ui,
    set: &ResultSet,
    view: &mut View,
    headers: bool,
    text_output: bool,
    allow_duplicate: bool,
) {
    let mut remove_filter = None;
    let mut clear_filters = false;
    let mut clear_sort = false;
    let mut action = None;
    if !text_output {
        ui.menu_button("复制", |ui| {
            action = copy_menu(ui, set, view);
            if action.is_some() {
                ui.close();
            }
        });
        if allow_duplicate
            && let Some(row) = view.selected_source_row()
            && duplicate_button(ui, set)
        {
            action = Some(Edit::DuplicateRow(row));
        }
    }
    if !view.predicates.is_empty() {
        ui.menu_button(
            format!("单元格筛选（{}）", view.predicates.len()),
            |ui| {
                for (index, predicate) in view.predicates.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(predicate.label(set));
                        if ui.small_button("移除").clicked() {
                            remove_filter = Some(index);
                            ui.close();
                        }
                    });
                }
                ui.separator();
                if ui.button("清除单元格筛选").clicked() {
                    clear_filters = true;
                    ui.close();
                }
            },
        );
    }
    if !view.sorts.is_empty() && ui.button("清除排序").clicked() {
        clear_sort = true;
    }
    if !text_output && let Some(selection) = view.selection.current() {
        ui.label(
            RichText::new(format!(
                "选中 {} 行 × {} 列",
                selection.rows().len(),
                selection.columns(set.columns.len()).len()
            ))
            .small()
            .color(style::MUTED),
        );
    }
    if clear_filters {
        view.predicates.clear();
        view.invalidate();
    } else if let Some(index) = remove_filter {
        view.predicates.remove(index);
        view.invalidate();
    }
    if clear_sort {
        view.sorts.clear();
        view.invalidate();
    }
    if let Some(action) = action {
        view.prepare(set);
        apply_edit(ui.ctx(), set, view, headers, action);
    }
}

fn copy_menu(ui: &mut egui::Ui, set: &ResultSet, view: &View) -> Option<Edit> {
    let mut action = None;
    if ui
        .add_enabled(
            view.selection.current().is_some(),
            egui::Button::new("复制选中行…"),
        )
        .clicked()
    {
        action = Some(Edit::ConfigureCopy(FormattedCopy::SelectedRows));
    }
    if !view.visible().is_empty()
        && view.visible().len() != set.rows.len()
        && ui.button("复制可见行…").clicked()
    {
        action = Some(Edit::ConfigureCopy(FormattedCopy::VisibleRows));
    }
    if ui
        .add_enabled(!set.rows.is_empty(), egui::Button::new("复制全部已加载行…"))
        .clicked()
    {
        action = Some(Edit::ConfigureCopy(FormattedCopy::AllRows));
    }
    ui.separator();
    ui.label(
        RichText::new("快速复制中，NULL 用 \\N，二进制以 \\x 开头")
            .small()
            .color(style::MUTED),
    );
    for format in [ClipboardFormat::Tsv, ClipboardFormat::Csv] {
        for (label, scope, enabled) in [
            (
                "复制选区",
                CopyScope::Selection,
                view.selection.current().is_some(),
            ),
            (
                "复制可见行",
                CopyScope::Visible,
                view.visible().len() != set.rows.len(),
            ),
            ("复制全部已加载行", CopyScope::All, true),
        ] {
            if matches!(scope, CopyScope::Visible) && !enabled {
                continue;
            }
            if ui
                .add_enabled(
                    enabled,
                    egui::Button::new(format!("{label}（{}）", format.label())),
                )
                .clicked()
            {
                action = Some(Edit::Copy(Copy::Area(scope, format)));
            }
        }
        ui.separator();
    }
    if ui
        .add_enabled(!view.visible().is_empty(), egui::Button::new("全选可见行"))
        .clicked()
    {
        action = Some(Edit::SelectAll);
    }
    if ui
        .add_enabled(
            view.selection.current().is_some(),
            egui::Button::new("取消选区"),
        )
        .clicked()
    {
        action = Some(Edit::ClearSelection);
    }
    action
}

fn cell_menu(
    ui: &mut egui::Ui,
    set: &ResultSet,
    view: &View,
    row: usize,
    column: usize,
    form: bool,
    allow_duplicate: bool,
) -> Option<Edit> {
    let mut action = None;
    if ui.button("复制单元格").clicked() {
        action = Some(Edit::Copy(Copy::Cell(row, column)));
    }
    if form && ui.button("复制当前记录…").clicked() {
        action = Some(Edit::ConfigureCopy(FormattedCopy::CurrentRow(row)));
    }
    if allow_duplicate && duplicate_button(ui, set) {
        action = Some(Edit::DuplicateRow(row));
    }
    if ui.button("查看内容…").clicked() {
        action = Some(Edit::Inspect(row, column));
    }
    if let Some(inner) = ui
        .menu_button("复制", |ui| copy_menu(ui, set, view))
        .inner
        .flatten()
    {
        action = Some(inner);
    }
    if ui.button("导出全部已读取行…").clicked() {
        action = Some(Edit::Export);
    }
    if let Some(inner) = ui
        .menu_button("筛选已加载行", |ui| {
            let value = get_cell(set, row, column);
            let mut action = None;
            for operator in Operator::ALL {
                if operator.supports(value) && ui.button(operator.label(value)).clicked() {
                    action = Some(Edit::Filter(Predicate {
                        row,
                        column,
                        operator,
                    }));
                }
            }
            if !view.predicates.is_empty() {
                ui.separator();
                if ui.button("清除单元格筛选").clicked() {
                    action = Some(Edit::ClearFilters);
                }
            }
            if action.is_some() {
                ui.close();
            }
            action
        })
        .inner
        .flatten()
    {
        action = Some(inner);
    }
    if action.is_some() {
        ui.close();
    }
    action
}

fn duplicate_button(ui: &mut egui::Ui, set: &ResultSet) -> bool {
    ui.add_enabled(
        set.duplicate_source
            .as_ref()
            .is_some_and(|origin| origin.is_bound()),
        egui::Button::new("复制为新行…"),
    )
    .on_disabled_hover_text("此结果缺少当前连接的单表查询来源。")
    .clicked()
}

fn apply_edit(ctx: &egui::Context, set: &ResultSet, view: &mut View, headers: bool, edit: Edit) {
    match edit {
        Edit::Copy(copy) => copy.apply(ctx, set, view, headers),
        Edit::ConfigureCopy(copy) => copy.open(ctx, set, view, headers),
        Edit::Inspect(row, column) => view.inspect_cell(set, row, column),
        Edit::DuplicateRow(row) => view.duplicate_row = Some(row),
        Edit::Export => view.export_requested = true,
        Edit::SelectAll => {
            let count = view.visible().len();
            view.selection.select_all(count, set.columns.len());
            view.selection.request_focus();
        }
        Edit::ClearSelection => view.selection.clear(),
        Edit::Filter(predicate) => view.filter(predicate),
        Edit::ClearFilters => {
            view.predicates.clear();
            view.invalidate();
        }
    }
}

pub(super) fn form_cell_menu(
    ui: &mut egui::Ui,
    set: &ResultSet,
    view: &mut View,
    source_row: usize,
    column: usize,
    headers: bool,
) {
    view.prepare(set);
    if let Some(action) = cell_menu(ui, set, view, source_row, column, true, true) {
        apply_edit(ui.ctx(), set, view, headers, action);
    }
}

pub(super) fn show(
    ui: &mut egui::Ui,
    set: &ResultSet,
    view: &mut View,
    headers: bool,
    keyboard_allowed: bool,
    request_focus: bool,
    grid_widths: &mut GridWidths,
) {
    let _ = show_grid(
        ui,
        set,
        view,
        headers,
        keyboard_allowed,
        request_focus,
        grid_widths,
        None,
    );
}

pub(super) struct JoinedEditing<'a> {
    pub(super) can_edit: Option<&'a dyn Fn(Point) -> bool>,
    pub(super) can_check: Option<&'a dyn Fn(usize) -> bool>,
    pub(super) move_draft: bool,
}

pub(super) fn show_grid(
    ui: &mut egui::Ui,
    set: &ResultSet,
    view: &mut View,
    headers: bool,
    keyboard_allowed: bool,
    request_focus: bool,
    grid_widths: &mut GridWidths,
    joined: Option<JoinedEditing<'_>>,
) -> Option<JoinedGridAction> {
    let row_height = style::grid_row_height(ui, GRID_ROW_HEIGHT);
    let header_height = style::grid_row_height(ui, GRID_HEADER_HEIGHT);
    let move_draft = joined.as_ref().is_some_and(|mode| mode.move_draft);
    let interactive = joined.is_none() || keyboard_allowed && !move_draft;
    let can_edit = joined
        .as_ref()
        .and_then(|mode| mode.can_edit)
        .filter(|_| interactive);
    let can_check = joined.as_ref().and_then(|mode| mode.can_check);
    let mut joined_action = None;
    let mut space_entry = None;
    if request_focus {
        view.selection.request_focus();
    }
    let focus = view.selection.focus(ui);
    let columns = set.columns.len();
    if keyboard_allowed && !move_draft && focus.has_focus() {
        let rows = view.visible().len();
        let page_rows = (ui.available_height() / (row_height + GRID_ROW_GAP)).max(1.0) as usize;
        for shortcut in view
            .selection
            .shortcuts(ui, rows, columns, page_rows, joined.is_some())
        {
            match shortcut {
                Shortcut::Copy(selection) => {
                    if let Some(point) = selection.single_cell() {
                        Copy::Cell(view.visible()[point.row], point.column).apply(
                            ui.ctx(),
                            set,
                            view,
                            headers,
                        );
                    } else if let Some(text) = selected_text(
                        set,
                        view.visible(),
                        selection,
                        headers,
                        ClipboardFormat::Tsv,
                    ) {
                        ui.ctx().copy_text(text);
                    }
                }
                Shortcut::Open(point, key) => {
                    let row = view.visible()[point.row];
                    let source = Point { row, ..point };
                    if can_edit.is_some_and(|can_edit| can_edit(source)) {
                        if view.selection.current().is_none() {
                            view.selection.select(
                                Hit {
                                    point,
                                    row_header: false,
                                },
                                false,
                            );
                        }
                        joined_action = Some(JoinedGridAction::Edit(source, None));
                        if key == egui::Key::Space && set.long_value_column(point.column) {
                            space_entry = Some(source);
                        }
                    } else {
                        apply_edit(
                            ui.ctx(),
                            set,
                            view,
                            headers,
                            Edit::Inspect(row, point.column),
                        );
                    }
                }
            }
        }
    }

    let mut pointer = PointerInteraction::default();
    let mut sort = None;
    let mut action = None;
    let mut check = None;
    let mut did_scroll = false;
    let scroll_to = view.selection.scroll_target().or(view.focus_scroll.take());
    let selected = view.selection.current();
    let selected_cell = selected.and_then(Selection::single_cell);
    let mut visible_cell = None;
    let mut clicked_cell = None;
    let click_moves_draft = |response: &egui::Response| {
        move_draft
            && (response.clicked_by(egui::PointerButton::Primary) || response.secondary_clicked())
            && response.ctx.input(|input| !input.modifiers.shift)
    };
    let visible_rows = &view.rows.as_ref().expect("prepared result rows").visible;
    view.column_widths.measure_data(
        ui,
        grid_widths,
        (0..columns).map(|column| !set.long_value_column(column)),
        visible_rows
            .iter()
            .flat_map(|&row| (0..columns).map(move |column| (column, get_cell(set, row, column)))),
    );
    let column_widths = view.column_widths.prepare(
        &set.columns,
        (0..columns).map(|index| {
            set.column_origins
                .get(index)
                .and_then(Option::as_ref)
                .map(ColumnOrigin::identifiers)
        }),
        (55.0, 44.0),
        grid_widths,
    );
    let numeric_columns: Vec<_> = (0..columns)
        .map(|column| {
            set.export_columns
                .get(column)
                .is_some_and(|column| column.is_numeric())
        })
        .collect();
    egui::ScrollArea::horizontal()
        .id_salt("result-scroll-x")
        .scroll_source(egui::scroll_area::ScrollSource {
            drag: false,
            ..Default::default()
        })
        .auto_shrink([false, true])
        .show(ui, |ui| {
            style::apply_grid_font(ui);
            ui.spacing_mut().item_spacing.y = GRID_ROW_GAP;
            let top = ui.cursor().min;
            paint_grid_header(ui, header_height);
            let resize_id = ui.id().with("result-column-resize");
            let mut table = TableBuilder::new(ui)
                .id_salt("result-table")
                .striped(true)
                .min_scrolled_height(0.0)
                .drag_to_scroll(false)
                .sense(Sense::click_and_drag() - Sense::focusable_noninteractive())
                .cell_layout(Layout::left_to_right(Align::Center));
            if can_check.is_some() {
                table = table.column(Column::exact(ROW_CHECK_WIDTH));
            }
            for column in &column_widths {
                table = table.column(column.column());
            }
            if let Some(target) = scroll_to {
                table = table.scroll_to_row(target.row, None);
            }
            table
                .header(header_height, |mut header| {
                    if can_check.is_some() {
                        header.col(|ui| {
                            ui.add(
                                egui::Label::new(RichText::new("勾选").small().color(style::MUTED))
                                    .selectable(false),
                            )
                            .on_hover_text("勾选要删除的源记录");
                        });
                    }
                    let (_, gutter) = header.col(|ui| {
                        grid_label(ui, RichText::new("#").monospace().color(style::MUTED), true);
                    });
                    if interactive && gutter.on_hover_text("全选可见行").clicked() {
                        action = Some(Edit::SelectAll);
                        focus.request_focus();
                    }
                    for (column, name) in set.columns.iter().enumerate() {
                        let (_, response) = header.col(|ui| {
                            let active_sort = view
                                .sorts
                                .iter()
                                .enumerate()
                                .find(|(_, sort)| sort.column == column);
                            let text = if let Some((priority, sort)) = active_sort {
                                let arrow = if sort.descending { "↓" } else { "↑" };
                                let mut label = format!("{name} {arrow}");
                                if view.sorts.len() > 1 {
                                    label.push_str(&(priority + 1).to_string());
                                }
                                RichText::new(label).color(style::ACCENT)
                            } else {
                                RichText::new(name).color(style::INK)
                            };
                            grid_label(ui, text.strong(), numeric_columns[column]);
                        });
                        let tooltip = set
                            .column_hints
                            .get(column)
                            .filter(|info| !info.is_empty())
                            .map_or_else(
                                || format!("{name}\n点击排序，按住 Ctrl 或 Cmd 添加排序列"),
                                |info| {
                                    format!("{name}\n{info}\n点击排序，按住 Ctrl 或 Cmd 添加排序列")
                                },
                            );
                        let response = response.on_hover_text(tooltip);
                        if interactive && response.clicked() {
                            sort = Some(column);
                        }
                    }
                })
                .body(|body| {
                    body.rows(row_height, view.visible().len(), |mut row| {
                        let visible = row.index();
                        let source = view.visible()[visible];
                        let row_selected = selected.is_some_and(|selection| {
                            selection.whole_rows && selection.rows().contains(&visible)
                        });
                        row.set_selected(row_selected);
                        if let Some(can_check) = can_check {
                            let eligible = can_check(source);
                            row.col(|ui| {
                                let mut checked = eligible && view.checked.contains(&source);
                                if ui
                                    .add_enabled(
                                        interactive && eligible,
                                        egui::Checkbox::new(&mut checked, ""),
                                    )
                                    .on_hover_text("勾选本行。按住 Shift 可勾选一段。")
                                    .changed()
                                {
                                    check = Some((
                                        visible,
                                        checked,
                                        ui.input(|input| input.modifiers.shift),
                                    ));
                                }
                            });
                        }
                        let (_, gutter) = row.col(|ui| {
                            grid_label(
                                ui,
                                RichText::new((source + 1).to_string()).monospace().color(
                                    if row_selected {
                                        style::ACCENT
                                    } else {
                                        style::MUTED
                                    },
                                ),
                                true,
                            );
                        });
                        let point = Point {
                            row: visible,
                            column: 0,
                        };
                        if click_moves_draft(&gutter) {
                            joined_action = Some(JoinedGridAction::Move(Point {
                                row: source,
                                column: selected.map_or(0, |selection| selection.head.column),
                            }));
                        }
                        if interactive {
                            pointer.visit(
                                &gutter,
                                Hit {
                                    point,
                                    row_header: true,
                                },
                            );
                            gutter.context_menu(|ui| {
                                if ui.button("导出全部已读取行…").clicked() {
                                    action = Some(Edit::Export);
                                    ui.close();
                                }
                                if joined.is_none() && duplicate_button(ui, set) {
                                    action = Some(Edit::DuplicateRow(source));
                                    ui.close();
                                }
                                if let Some(edit) = copy_menu(ui, set, view) {
                                    action = Some(edit);
                                    ui.close();
                                }
                            });
                        }
                        for (column, &numeric) in numeric_columns.iter().enumerate() {
                            let point = Point {
                                row: visible,
                                column,
                            };
                            let source_point = Point {
                                row: source,
                                column,
                            };
                            let editable = can_edit.is_some_and(|can_edit| can_edit(source_point));
                            row.set_selected(
                                selected.is_some_and(|selection| selection.contains(point)),
                            );
                            let value = get_cell(set, source, column);
                            let (_, response) = row.col(|ui| {
                                let current = selected.is_some_and(|selection| {
                                    !selection.whole_rows && selection.head == point
                                });
                                grid_cell(ui, value, numeric, current);
                                if scroll_to == Some(point) {
                                    ui.scroll_to_cursor(None);
                                    did_scroll = true;
                                }
                            });
                            let clicked = response.clicked_by(egui::PointerButton::Primary);
                            let rect = response.interact_rect;
                            if keyboard_allowed
                                && interactive
                                && (selected_cell == Some(point) || clicked)
                                && (editable || set.long_value_column(column))
                                && rect.is_positive()
                                && rect.is_finite()
                            {
                                let cell = (point, source, rect);
                                if selected_cell == Some(point) {
                                    visible_cell = Some(cell);
                                }
                                if clicked {
                                    clicked_cell = Some(cell);
                                }
                            }
                            if click_moves_draft(&response) {
                                joined_action = Some(JoinedGridAction::Move(source_point));
                            }
                            if interactive {
                                pointer.visit(
                                    &response,
                                    Hit {
                                        point,
                                        row_header: false,
                                    },
                                );
                                if response.double_clicked() {
                                    if editable {
                                        joined_action =
                                            Some(JoinedGridAction::Edit(source_point, None));
                                    } else if matches!(value, Cell::Binary(_) | Cell::Text(_))
                                        || joined.is_some() && set.long_value_column(column)
                                    {
                                        action = Some(Edit::Inspect(source, column));
                                    }
                                }
                                response.context_menu(|ui| {
                                    if editable && ui.button("编辑单元格").clicked() {
                                        joined_action =
                                            Some(JoinedGridAction::Edit(source_point, None));
                                        ui.close();
                                    } else if let Some(edit) = cell_menu(
                                        ui,
                                        set,
                                        view,
                                        source,
                                        column,
                                        false,
                                        joined.is_none(),
                                    ) {
                                        action = Some(edit);
                                    }
                                });
                            }
                        }
                    });
                });
            let data_top = if can_check.is_some() {
                top + egui::vec2(ROW_CHECK_WIDTH + ui.spacing().item_spacing.x, 0.0)
            } else {
                top
            };
            view.column_widths
                .resize_handles(ui, resize_id, data_top, &column_widths, grid_widths);
        });

    view.selection.finish_scroll(ui, did_scroll);
    if let Some(column) = sort {
        let command = ui.input(|input| input.modifiers.command || input.modifiers.ctrl);
        view.sort(column, command);
    } else if let Some((position, checked, extend)) = check
        && let Some(can_check) = can_check
    {
        view.check_row(position, checked, extend, can_check);
        ui.ctx().request_repaint();
    }
    if interactive {
        view.selection.apply_pointer(ui, &focus, pointer);
    }
    let accepts_text = match &joined_action {
        None => true,
        Some(JoinedGridAction::Edit(point, None)) => Some(*point) == view.selected_source_point(),
        _ => false,
    };
    if keyboard_allowed
        && interactive
        && sort.is_none()
        && action.is_none()
        && accepts_text
        && !view.has_modal()
        && focus.has_focus()
        && !egui::Popup::is_any_open(ui.ctx())
    {
        let selected = view.selection.current().and_then(Selection::single_cell);
        let selected_entry = |(point, _, _): &(Point, usize, egui::Rect)| Some(*point) == selected;
        if let Some((point, row, rect)) = clicked_cell
            .filter(selected_entry)
            .or_else(|| visible_cell.filter(selected_entry))
        {
            text_entry::request_grid_ime(ui, rect);
            if selected_cell == Some(point) || text_entry::text_follows_click(ui, rect) {
                let source = Point { row, ..point };
                let typed = text_entry::take_grid_text(ui)
                    .or_else(|| (space_entry == Some(source)).then(|| " ".to_owned()));
                if let Some(text) = typed {
                    if can_edit.is_some_and(|can_edit| can_edit(source)) {
                        joined_action = Some(JoinedGridAction::Edit(source, Some(text)));
                    } else {
                        action = Some(Edit::Inspect(row, point.column));
                    }
                }
            }
        }
    }
    if let Some(action) = action {
        view.prepare(set);
        apply_edit(ui.ctx(), set, view, headers, action);
    }
    joined_action
}

pub(super) fn dialogs(
    ctx: &egui::Context,
    set: &ResultSet,
    view: &mut View,
    settings: EditorSettings,
) {
    if let Some(dialog) = &mut view.inspection
        && !matches!(dialog.show(ctx, settings), Outcome::Continue)
    {
        view.inspection = None;
    }
    if let Some(dialog) = &mut view.clipboard {
        match dialog.show(ctx, set) {
            clipboard::Outcome::Continue => {}
            clipboard::Outcome::Cancel => view.clipboard = None,
            clipboard::Outcome::Copied(options) => {
                ctx.data_mut(|data| {
                    data.insert_temp(egui::Id::new("result-clipboard-format"), options)
                });
                view.clipboard = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::Key;

    #[test]
    fn joined_grid_edits_original_cells_and_checks_only_mapped_rows() {
        let set = ResultSet {
            source_sql: None,
            columns: vec!["值".into()],
            column_hints: Vec::new(),
            column_origins: Vec::new(),
            excel_types: Vec::new(),
            export_columns: Vec::new(),
            duplicate_source: None,
            notice: None,
            rows: ["10", "2", "3"]
                .into_iter()
                .map(|value| vec![Cell::Text(value.into())])
                .collect(),
            affected_rows: 0,
            last_insert_id: None,
            truncated: false,
        };
        let original = Point { row: 0, column: 0 };
        let can_edit = |point: Point| point == original;
        let can_check = |row| row != 2;
        let mut view = View::default();
        view.sort(0, false);
        assert!(view.focus_source_point(&set, original));
        assert_eq!(view.selected_source_rows(), [0]);
        assert_eq!(
            view.adjacent_source_point(false),
            Some(Point { row: 2, column: 0 })
        );
        view.check_row(0, true, false, &can_check);
        view.check_row(2, true, true, &can_check);
        assert_eq!(view.checked_source_rows(), [1, 0]);

        let ctx = egui::Context::default();
        let mut grid_widths = GridWidths::new(false);
        let mut frame = |events| {
            let mut action = None;
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(700.0, 300.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        action = show_grid(
                            ui,
                            &set,
                            &mut view,
                            true,
                            true,
                            false,
                            &mut grid_widths,
                            Some(JoinedEditing {
                                can_edit: Some(&can_edit),
                                can_check: Some(&can_check),
                                move_draft: false,
                            }),
                        );
                    });
                },
            );
            action
        };
        assert_eq!(frame(vec![]), None);
        assert_eq!(
            frame(vec![egui::Event::Ime(egui::ImeEvent::Commit(
                "新值".into()
            ))]),
            Some(JoinedGridAction::Edit(original, Some("新值".into())))
        );
    }

    #[test]
    fn data_sizing_remeasures_result_columns_after_invalidation() {
        const HEADER: &str = "Detailed order identifier";

        fn header_elided(shape: &egui::Shape) -> Option<bool> {
            match shape {
                egui::Shape::Text(text) if text.galley.text() == HEADER => Some(text.galley.elided),
                egui::Shape::Vec(shapes) => shapes.iter().find_map(header_elided),
                _ => None,
            }
        }

        let mut set = ResultSet {
            source_sql: None,
            columns: vec![HEADER.into()],
            column_hints: Vec::new(),
            column_origins: Vec::new(),
            excel_types: Vec::new(),
            export_columns: Vec::new(),
            duplicate_source: None,
            notice: None,
            rows: vec![vec![Cell::Text("x".into())]],
            affected_rows: 0,
            last_insert_id: None,
            truncated: false,
        };
        let ctx = egui::Context::default();
        let mut view = View::default();
        let mut grid_widths = GridWidths::new(false);
        grid_widths.set_data_sizing(true);
        let mut render = |set: &ResultSet, view: &mut View| {
            view.prepare(set);
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(700.0, 300.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        show(ui, set, view, true, true, false, &mut grid_widths);
                    });
                },
            )
            .shapes
            .iter()
            .find_map(|shape| header_elided(&shape.shape))
            .expect("the result header was painted")
        };

        assert!(render(&set, &mut view));
        set.rows[0][0] = Cell::Text("W".repeat(48));
        view.invalidate();
        assert!(!render(&set, &mut view));
    }

    #[test]
    fn copies_visual_rectangle_and_keeps_original_row_indices_after_sort() {
        let set = ResultSet {
            source_sql: None,
            columns: vec!["ID".into(), "说明".into()],
            column_hints: Vec::new(),
            column_origins: Vec::new(),
            excel_types: Vec::new(),
            export_columns: Vec::new(),
            duplicate_source: None,
            notice: None,
            rows: vec![
                vec![Cell::Text("10".into()), Cell::Text("后".into())],
                vec![Cell::Text("2".into()), Cell::Text("先".into())],
            ],
            affected_rows: 0,
            last_insert_id: None,
            truncated: false,
        };
        let mut view = View::default();
        view.sort(0, false);
        view.prepare(&set);
        view.selection.select(
            Hit {
                point: Point { row: 1, column: 1 },
                row_header: false,
            },
            false,
        );
        view.selection.select(
            Hit {
                point: Point { row: 0, column: 1 },
                row_header: false,
            },
            true,
        );
        let copy = Copy::Area(CopyScope::Selection, ClipboardFormat::Tsv);
        assert_eq!(copy.text(&set, &view, true).unwrap(), "说明\n先\n后");

        view.filter(Predicate {
            row: 0,
            column: 0,
            operator: Operator::Equal,
        });
        view.prepare(&set);
        assert!(view.selection.current().is_none());
        assert_eq!(view.visible(), [0]);
        assert_eq!(
            Copy::Area(CopyScope::All, ClipboardFormat::Tsv)
                .text(&set, &view, false)
                .unwrap(),
            "2\t先\n10\t后"
        );
    }

    #[test]
    fn mouse_focus_and_shift_arrow_copy_a_grid_range() {
        let set = ResultSet {
            source_sql: None,
            columns: vec!["entry".into(), "category".into()],
            column_hints: Vec::new(),
            column_origins: Vec::new(),
            excel_types: Vec::new(),
            export_columns: Vec::new(),
            duplicate_source: None,
            notice: None,
            rows: vec![
                vec![Cell::Text("first row".into()), Cell::Text("a".into())],
                vec![Cell::Text("second row".into()), Cell::Text("b".into())],
            ],
            affected_rows: 0,
            last_insert_id: None,
            truncated: false,
        };
        let ctx = egui::Context::default();
        let mut view = View::default();
        let mut grid_widths = GridWidths::new(true);
        view.prepare(&set);
        let mut frame = |events, modifiers| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(600.0, 300.0),
                    )),
                    events,
                    modifiers,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        show(ui, &set, &mut view, true, true, false, &mut grid_widths);
                    });
                },
            )
        };
        frame(vec![], egui::Modifiers::NONE);
        let rendered = frame(vec![], egui::Modifiers::NONE);
        let position = |label| {
            rendered
                .shapes
                .iter()
                .find_map(|shape| {
                    if let egui::Shape::Text(text) = &shape.shape
                        && text.galley.text() == label
                    {
                        Some(text.pos + text.galley.size() * 0.5)
                    } else {
                        None
                    }
                })
                .expect("grid label is visible")
        };
        let clicked = position("first row");
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            pressed,
            button: egui::PointerButton::Primary,
            modifiers: egui::Modifiers::NONE,
        };
        for pressed in [true, false] {
            frame(
                vec![egui::Event::PointerMoved(clicked), button(clicked, pressed)],
                egui::Modifiers::NONE,
            );
        }
        frame(vec![], egui::Modifiers::NONE);
        frame(
            vec![egui::Event::Key {
                key: Key::ArrowDown,
                physical_key: Some(Key::ArrowDown),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::SHIFT,
            }],
            egui::Modifiers::SHIFT,
        );
        let output = frame(vec![egui::Event::Copy], egui::Modifiers::COMMAND);
        assert!(output.platform_output.commands.iter().any(|command| {
            matches!(command, egui::OutputCommand::CopyText(text) if text == "entry\nfirst row\nsecond row")
        }));

        let first_gutter = position("1");
        let second_gutter = position("2");
        frame(
            vec![
                egui::Event::PointerMoved(first_gutter),
                button(first_gutter, true),
            ],
            egui::Modifiers::NONE,
        );
        frame(
            vec![egui::Event::PointerMoved(second_gutter)],
            egui::Modifiers::NONE,
        );
        frame(vec![button(second_gutter, false)], egui::Modifiers::NONE);
        let output = frame(vec![egui::Event::Copy], egui::Modifiers::COMMAND);
        assert!(output.platform_output.commands.iter().any(|command| {
            matches!(command, egui::OutputCommand::CopyText(text) if text == "entry\tcategory\nfirst row\ta\nsecond row\tb")
        }));
    }
}
