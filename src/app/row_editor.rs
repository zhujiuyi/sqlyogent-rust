use std::collections::BTreeSet;

use eframe::egui::{self, Align, Button, Layout, RichText, Sense, TextEdit};
use egui_extras::{Column, TableBuilder};

use std::num::NonZeroUsize;

use crate::database::{
    Cell, Command, DbObject, DuplicateChoice, DuplicateScope, EDITABLE_READ_CANCELLED, FieldValue,
    JoinedEditTarget, JoinedEditablePage, TABLE_READ_CANCELLED, TableColumn, TableEdit, TablePage,
    TablePredicate, TablePredicateOp, TableReadRange, TableRow, TableSelection, TableSort,
    preview_table_update,
    transfer::{ExportKind, ResultExport},
};

use super::{
    PendingEditableQuery,
    cell_value::{Inspector, Outcome},
    editor_tools::cursors,
    io_jobs::export::Options as ExportOptions,
    output::{
        GRID_HEADER_HEIGHT, GRID_ROW_GAP, GRID_ROW_HEIGHT, InlineJoinedCell, InlineKey,
        JoinedGridAction, ResultGrid, ResultMode as DataView, TextRows, data_info_line,
        data_info_line_height, display_cell, grid_cell_styled, grid_label, paint_cell_outline,
        paint_grid_header, query_time_label, render_inline_cell_editor, result_view_controls,
        selection::{GridSelection, Hit, Point, PointerInteraction, Selection, Shortcut},
        values::{ClipboardFormat, cell_text, clipboard_table},
    },
    preferences::{DEFAULT_TABLE_DATA_LIMIT, EditorSettings, GridColumns, GridWidths},
    state::{EditableResult, Workspace},
    style,
};

const ROW_CHECK_WIDTH: f32 = 36.0;

mod form;
mod joined;
#[cfg(test)]
mod tests;
pub(super) mod text_entry;

enum ReadCursor {
    At(Point),
    Last(usize),
    Record(TableRow, usize),
    JoinedRecord(TableRow, Point),
}

enum Pending {
    Open,
    Read(Option<ReadCursor>),
    TableRead(PendingTableRead),
    Write(TableEdit),
    DeleteRows,
}

struct PendingTableRead {
    range: TableReadRange,
    previous_offsets: Vec<usize>,
    cursor: Option<ReadCursor>,
    retry_from_start: bool,
    focus_refresh: bool,
}

struct TableReadFields {
    offset: usize,
    rows: usize,
    all: bool,
}

impl TableReadFields {
    fn range(&self) -> TableReadRange {
        if self.all {
            TableReadRange::All
        } else {
            TableReadRange::Page {
                offset: self.offset,
                rows: self.row_limit(),
            }
        }
    }

    fn row_limit(&self) -> NonZeroUsize {
        NonZeroUsize::new(self.rows).unwrap_or(NonZeroUsize::MIN)
    }
}

enum Deletion {
    Row(usize),
    Checked(Vec<usize>),
}

impl Deletion {
    fn rows(&self) -> &[usize] {
        match self {
            Self::Row(index) => std::slice::from_ref(index),
            Self::Checked(indices) => indices,
        }
    }
}

struct DuplicatePrompt {
    edit: TableEdit,
    matches: u64,
}

enum Navigate {
    Previous,
    Next(usize),
    Refresh,
    First,
}

enum RowMove {
    Row(Point),
    JoinedRow { source: Point, result: Point },
    Page(Navigate),
}

enum QueuedMove {
    Cursor(ReadCursor),
    Page(Navigate),
}

enum GridAction {
    Open(Point),
    OpenJoined(Point, Point),
    BeginDraft {
        row: Option<usize>,
        column: usize,
        text: Option<String>,
    },
    BeginJoinedDraft {
        row: Option<usize>,
        column: usize,
        text: Option<String>,
        joined_point: Point,
    },
    InlineCommit,
    InlineCancel,
    InlineSwitchCell {
        column: usize,
        joined_point: Option<Point>,
    },
    InlineStepColumn {
        forward: bool,
    },
    Focus(Point),
    Duplicate(usize),
    Delete(usize),
    Export,
    SetValue(Point, ValuePreset),
    Navigate(Navigate),
    Filter(TablePredicate),
    Sort(TableSort, bool),
    NewFilter(usize),
}

struct FilterDraft {
    column: usize,
    operator: TablePredicateOp,
    value: String,
    is_null: bool,
}

pub(super) struct RowEditor {
    index: usize,
    object: Option<DbObject>,
    query: Option<(String, usize)>,
    query_read_only: bool,
    joined: Option<joined::Joined>,
    duplicate_only: bool,
    offset: usize,
    previous_offsets: Vec<usize>,
    table_fields: TableReadFields,
    table_range: TableReadRange,
    table_read_stopping: bool,
    page: Option<TablePage>,
    grid: DataGrid,
    joined_grid: ResultGrid,
    text_rows: TextRows,
    focus_text: bool,
    view: DataView,
    last_structured_view: DataView,
    server_selection: TableSelection,
    applied_selection: TableSelection,
    filter_draft: Option<FilterDraft>,
    draft: Option<Draft>,
    inspection: Option<(Option<usize>, usize, Inspector)>,
    export: Option<ExportOptions>,
    deleting: Option<Deletion>,
    duplicate_prompt: Option<DuplicatePrompt>,
    pending: Option<Pending>,
    queued_move: Option<QueuedMove>,
    error: Option<String>,
    notice: Option<String>,
}

impl RowEditor {
    pub(super) fn new(index: usize, object: DbObject) -> Self {
        let grid_id = egui::Id::new(("table-data-grid", &object.schema, &object.name));
        Self::with_target(index, Some(object), grid_id)
    }

    fn with_target(index: usize, object: Option<DbObject>, grid_id: egui::Id) -> Self {
        let table_fields = TableReadFields {
            offset: 0,
            rows: DEFAULT_TABLE_DATA_LIMIT,
            all: false,
        };
        Self {
            index,
            object,
            query: None,
            query_read_only: false,
            joined: None,
            duplicate_only: false,
            offset: 0,
            previous_offsets: Vec::new(),
            table_range: table_fields.range(),
            table_fields,
            table_read_stopping: false,
            page: None,
            grid: DataGrid::new(grid_id),
            joined_grid: ResultGrid::default(),
            text_rows: TextRows::default(),
            focus_text: false,
            view: DataView::Grid,
            last_structured_view: DataView::Grid,
            server_selection: TableSelection::default(),
            applied_selection: TableSelection::default(),
            filter_draft: None,
            draft: None,
            inspection: None,
            export: None,
            deleting: None,
            duplicate_prompt: None,
            pending: Some(Pending::Open),
            queued_move: None,
            error: None,
            notice: None,
        }
    }

    pub(super) fn set_table_defaults(&mut self, offset: usize, rows: usize, all: bool) {
        self.table_fields = TableReadFields { offset, rows, all };
        self.table_range = self.table_fields.range();
    }

    pub(super) fn use_workspace_view(&mut self, workspace: &Workspace) -> bool {
        let previous = self.view;
        let view = workspace.result_mode();
        self.set_view(view);
        if view == DataView::Text {
            self.last_structured_view = if self
                .joined
                .as_ref()
                .is_some_and(|joined| joined.selected.is_none())
            {
                DataView::Grid
            } else {
                workspace.last_structured_result_mode()
            };
        }
        previous != self.view
    }

    pub(super) fn object(&self) -> Option<&DbObject> {
        self.object.as_ref()
    }

    pub(super) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub(super) fn has_modal(&self) -> bool {
        self.inspection.is_some() || self.export.is_some() || self.joined_grid.has_modal()
    }

    fn export_input(&self) -> Option<ResultExport<'_>> {
        if let Some(joined) = &self.joined {
            joined
                .response
                .as_ref()
                .map(|reply| ResultExport::from(&reply.result))
        } else {
            self.page.as_ref().map(ResultExport::from)
        }
    }

    pub(super) fn can_export_loaded(&self) -> bool {
        !self.duplicate_only
            && !self.has_draft_or_modal()
            && !self.has_pending()
            && self
                .export_input()
                .is_some_and(|input| input.column_count() > 0)
    }

    pub(super) fn request_export(&mut self, kind: ExportKind) -> bool {
        if !self.can_export_loaded() {
            return false;
        }
        if let Some(input) = self.export_input() {
            self.export = Some(ExportOptions::new(kind, input.column_count()));
            return true;
        }
        false
    }

    fn show_export(&mut self, ctx: &egui::Context, workspace: &mut Workspace) {
        let Some(mut options) = self.export.take() else {
            return;
        };
        let Some(input) = self.export_input() else {
            return;
        };
        let table_page = self.query.is_none();
        let id = egui::Id::new((
            "row-editor-export",
            workspace.key,
            workspace.row_editor_tab_id(),
            table_page,
        ));
        let (close, outcome) =
            super::output::result_export_dialog(ctx, id, &mut options, input, table_page);
        let structure_only = options.structure_only();
        if !close {
            self.export = Some(options);
        }
        if outcome.is_some() {
            let previous = workspace.output;
            super::output::report_result_export(workspace, structure_only, outcome);
            workspace.output = previous;
        }
    }

    pub(super) fn has_draft_or_modal(&self) -> bool {
        self.draft.is_some()
            || self.has_modal()
            || self.deleting.is_some()
            || self.duplicate_prompt.is_some()
            || self.filter_draft.is_some()
    }

    pub(super) fn text_has_focus(&self, ctx: &egui::Context) -> bool {
        self.view == DataView::Text && super::output::text_has_focus(&self.text_rows, ctx)
    }

    pub(super) fn text_search(&mut self, next: bool) {
        if self.view == DataView::Text {
            self.text_rows.request_find(next);
        }
    }

    pub(super) fn request_focus(&mut self) {
        match self.view {
            DataView::Text => self.focus_text = true,
            DataView::Grid if self.joined.is_some() => self.joined_grid.request_focus(),
            DataView::Grid | DataView::Form => self.grid.selection.request_focus(),
        }
    }

    pub(super) fn keep_current_pane_focus(&mut self) {
        self.grid.selection.clear_focus_request();
        self.focus_text = false;
    }

    pub(super) fn refresh_on_focus(&mut self, workspace: &mut Workspace) -> bool {
        if workspace.busy()
            || workspace.server.is_none()
            || self.query.is_some()
            || self.duplicate_only
            || self.has_pending()
            || self.has_draft_or_modal()
            || self.table_fields.range() != self.table_range
            || self.page.is_none()
        {
            return false;
        }
        self.start_table_read(
            workspace,
            PendingTableRead {
                range: self.table_range,
                previous_offsets: self.previous_offsets.clone(),
                cursor: None,
                retry_from_start: true,
                focus_refresh: true,
            },
        );
        self.has_pending()
    }

    fn has_previous_page(&self) -> bool {
        self.query.is_none() && self.offset > 0
    }

    fn origin_matches(&self, workspace: &Workspace) -> bool {
        self.joined.as_ref().is_none_or(|joined| {
            joined.workspace == workspace.key && joined.tab == workspace.row_editor_tab_id()
        })
    }

    #[cfg(test)]
    pub(super) fn from_query(index: usize, page: TablePage, sql: String, limit: usize) -> Self {
        Self::from_query_with_mode(index, page, sql, limit, false)
    }

    pub(super) fn from_query_with_mode(
        index: usize,
        page: TablePage,
        sql: String,
        limit: usize,
        read_only: bool,
    ) -> Self {
        let mut editor = Self::new(index, page.object.clone());
        editor.grid.id = egui::Id::new(("editable-query-grid", &sql));
        editor.query = Some((sql, limit));
        editor.query_read_only = read_only;
        editor.page = Some(page);
        editor.pending = None;
        editor
    }

    #[cfg(test)]
    pub(super) fn from_joined(
        index: usize,
        page: JoinedEditablePage,
        query: PendingEditableQuery,
    ) -> Self {
        Self::from_joined_with_mode(index, page, query, false)
    }

    pub(super) fn from_joined_with_mode(
        index: usize,
        page: JoinedEditablePage,
        query: PendingEditableQuery,
        read_only: bool,
    ) -> Self {
        let joined = joined::Joined::new_with_mode(query.workspace, query.tab, page, read_only);
        let object = joined.selected.as_ref().map(|target| target.table.clone());
        let mut editor = Self::with_target(index, object, joined.grid_id());
        editor.joined = Some(joined);
        editor.query = Some((query.sql, query.limit));
        editor.pending = None;
        editor
    }

    pub(super) fn from_result_duplicate(index: usize, page: TablePage) -> Result<Self, String> {
        if page.rows.len() != 1 {
            return Err("未能定位所选记录，请重新执行原查询。".into());
        }
        if let Some(error) = &page.edit_error {
            return Err(error.clone());
        }
        let draft = Draft::duplicate(&page, 0)?;
        let mut editor = Self::new(index, page.object.clone());
        editor.page = Some(page);
        editor.draft = Some(draft);
        editor.view = DataView::Form;
        editor.last_structured_view = DataView::Form;
        editor.duplicate_only = true;
        editor.pending = None;
        Ok(editor)
    }

    fn set_view(&mut self, view: DataView) {
        if self.view == view
            || view == DataView::Form
                && self
                    .joined
                    .as_ref()
                    .is_some_and(|joined| joined.selected.is_none())
        {
            return;
        }
        let original = self.last_structured_view;
        self.view = view;
        if view == DataView::Text {
            self.focus_text = true;
            return;
        }
        self.last_structured_view = view;
        if view == original {
            if self.joined.is_some() && view == DataView::Grid {
                self.joined_grid.request_focus();
            } else {
                self.grid.selection.request_focus();
            }
            return;
        }
        self.joined_grid.clear_checks();
        self.grid.checked.clear();
        self.grid.blank_column = None;
        if let Some(page) = self
            .joined
            .as_ref()
            .map_or(self.page.as_ref(), joined::Joined::page)
        {
            let mut point = self.grid.form_point();
            if let Some(joined) = &self.joined
                && let Some(binding) = joined.binding()
                && let Some(result) = self.joined_grid.selected_source_point()
                && view == DataView::Form
                && let Some(row) = joined::source_row(binding, result.row)
            {
                point = Point {
                    row,
                    column: joined::source_point(binding, result)
                        .map_or(point.column, |point| point.column),
                };
            }
            if let Some(row) = self.draft.as_ref().and_then(|draft| draft.row) {
                point.row = row;
            }
            self.grid.focus_form_point(page, point);
            if view == DataView::Grid
                && let Some(joined) = &self.joined
                && let Some(binding) = joined.binding()
                && let Some(reply) = &joined.response
                && let Some(result) = joined::result_point(
                    binding,
                    point,
                    self.joined_grid.selected_source_point().unwrap_or_default(),
                )
            {
                self.joined_grid.focus_source_point(&reply.result, result);
            }
        }
    }

    fn can_switch_target(&self) -> bool {
        self.pending.is_none()
            && self.draft.is_none()
            && self.deleting.is_none()
            && self.inspection.is_none()
            && self.filter_draft.is_none()
            && self.duplicate_prompt.is_none()
            && self.joined.is_some()
            && !self.joined_grid.has_modal()
    }

    fn select_joined_target(&mut self, target: Option<joined::Target>) {
        if !self.can_switch_target() {
            return;
        }
        if let Some(joined) = &mut self.joined
            && joined.select(target)
        {
            self.object = joined.selected.as_ref().map(|target| target.table.clone());
            self.grid = DataGrid::new(joined.grid_id());
            self.joined_grid.clear_checks();
            if self.view != DataView::Text {
                self.view = DataView::Grid;
            }
            self.last_structured_view = DataView::Grid;
            self.error = None;
            self.notice = None;
        }
    }

    fn joined_cursor(&self) -> Option<ReadCursor> {
        let binding = self.joined.as_ref()?.binding()?;
        let result = self.joined_grid.selected_source_point()?;
        let row = joined::source_row(binding, result.row)?;
        Some(ReadCursor::JoinedRecord(
            binding.page.rows[row].clone(),
            result,
        ))
    }

    fn read_page(&mut self, workspace: &mut Workspace, cursor: Option<ReadCursor>) {
        if self.query.is_some()
            && let Some(error) = workspace.take_editable_read_error()
        {
            if error == EDITABLE_READ_CANCELLED {
                self.append_notice(EDITABLE_READ_CANCELLED.trim_end_matches('。'));
            } else {
                self.error = Some(format!("读取可编辑查询结果失败：{error}"));
            }
            return;
        }
        self.grid.selection.clear();
        self.grid.checked.clear();
        self.grid.blank_column = None;
        self.grid.form_scroll = None;
        self.deleting = None;
        if let Some(joined) = &mut self.joined {
            match workspace.editable_page.take() {
                Some(EditableResult::Joined(response)) => {
                    let lost_target = joined.replace(response);
                    self.object = joined.selected.as_ref().map(|target| target.table.clone());
                    self.grid = DataGrid::new(joined.grid_id());
                    self.joined_grid = ResultGrid::default();
                    self.text_rows.reset_match();
                    if lost_target {
                        if self.view != DataView::Text {
                            self.view = DataView::Grid;
                        }
                        self.last_structured_view = DataView::Grid;
                        self.append_notice("原写入目标已不可用，已显示完整结果。");
                    } else if let Some(cursor) = cursor
                        && !restore_joined_cursor(
                            joined,
                            &mut self.joined_grid,
                            &mut self.grid,
                            cursor,
                        )
                    {
                        self.append_notice("目标行未出现在重新读取的结果中。");
                    }
                }
                Some(EditableResult::Table(_)) => {
                    self.error = Some("查询结果类型已变化。请回到查询页重新执行 F8。".into());
                }
                None => self.error = Some(format!("读取联表查询结果失败：{}", workspace.status)),
            }
            return;
        }
        let page = if self.query.is_some() {
            match workspace.editable_page.take() {
                Some(EditableResult::Table(page)) => Some(page),
                Some(EditableResult::Joined(_)) => {
                    self.error = Some("查询结果类型已变化。请回到查询页重新执行 F8。".into());
                    return;
                }
                None => None,
            }
        } else {
            workspace.row_page.take()
        };
        match page {
            Some(page) if Some(&page.object) != self.object.as_ref() => {
                self.error = Some("查询的源表已变化。请返回查询编辑器重新打开结果。".into());
            }
            Some(page) if page.rows.is_empty() && page.offset > 0 => {
                self.previous_offsets.clear();
                self.load(
                    workspace,
                    0,
                    cursor.map(|cursor| match cursor {
                        ReadCursor::Record(..) => cursor,
                        _ => ReadCursor::At(Point::default()),
                    }),
                );
            }
            Some(page) => self.install_page(page, cursor),
            None => self.error = Some(format!("读取表数据失败：{}", workspace.status)),
        }
    }

    fn install_page(&mut self, page: TablePage, cursor: Option<ReadCursor>) {
        if let Some(cursor) = cursor
            && !self.grid.restore_cursor(&page, cursor)
        {
            self.append_notice("目标行未出现在重新读取的结果中。");
        }
        self.grid.column_widths.invalidate_automatic();
        self.text_rows.reset_match();
        self.page = Some(page);
    }

    fn finish_table_read(&mut self, workspace: &mut Workspace, request: PendingTableRead) {
        self.table_read_stopping = false;
        let Some(page) = workspace.row_page.take() else {
            self.server_selection = self.applied_selection.clone();
            self.table_fields.all = self.table_range == TableReadRange::All;
            let error = workspace
                .take_table_read_error()
                .unwrap_or_else(|| workspace.status.clone());
            if error == TABLE_READ_CANCELLED {
                self.notice = Some(TABLE_READ_CANCELLED.into());
            } else {
                self.error = Some(format!("读取表数据失败：{error}"));
            }
            return;
        };
        if Some(&page.object) != self.object.as_ref() {
            self.server_selection = self.applied_selection.clone();
            self.error = Some("查询的源表已变化，请重新打开表数据。".into());
            return;
        }
        if request.retry_from_start
            && page.rows.is_empty()
            && page.offset > 0
            && let TableReadRange::Page { rows, .. } = request.range
        {
            self.start_table_read(
                workspace,
                PendingTableRead {
                    range: TableReadRange::Page { offset: 0, rows },
                    previous_offsets: Vec::new(),
                    cursor: request.cursor,
                    retry_from_start: false,
                    focus_refresh: request.focus_refresh,
                },
            );
            return;
        }
        self.table_range = request.range;
        self.applied_selection = self.server_selection.clone();
        self.previous_offsets = request.previous_offsets;
        self.grid.scroll_to_top |= self.offset != page.offset;
        self.offset = page.offset;
        self.table_fields.all = request.range == TableReadRange::All;
        if let TableReadRange::Page { offset, rows } = request.range {
            self.table_fields.offset = offset;
            self.table_fields.rows = rows.get();
        }
        if request.focus_refresh
            && let Some(old) = &self.page
        {
            self.grid.keep_selection_after_refresh(old, &page);
        } else {
            self.grid.selection.clear();
            self.grid.checked.clear();
            self.grid.blank_column = None;
            self.grid.form_scroll = None;
        }
        self.install_page(page, request.cursor);
    }

    fn append_notice(&mut self, message: &str) {
        self.notice = Some(self.notice.take().map_or_else(
            || message.into(),
            |previous| format!("{previous}。{message}"),
        ));
    }

    pub(super) fn poll(&mut self, workspace: &mut Workspace) {
        if workspace.busy() {
            return;
        }
        match self.pending.take() {
            Some(Pending::Open) => self.start_table_read(
                workspace,
                PendingTableRead {
                    range: self.table_range,
                    previous_offsets: Vec::new(),
                    cursor: None,
                    retry_from_start: false,
                    focus_refresh: false,
                },
            ),
            Some(Pending::Read(cursor)) => self.read_page(workspace, cursor),
            Some(Pending::TableRead(request)) => self.finish_table_read(workspace, request),
            Some(Pending::Write(edit)) => {
                if let Some(matches) = workspace.row_edit_match_count.take() {
                    self.duplicate_prompt = Some(DuplicatePrompt { edit, matches });
                } else {
                    self.finish_write(workspace, false);
                }
            }
            Some(Pending::DeleteRows) => self.finish_write(workspace, true),
            None => {}
        }
    }

    fn finish_write(&mut self, workspace: &mut Workspace, batch: bool) {
        let queued_move = self.queued_move.take();
        if let Some(count) = workspace.row_edit_affected_rows.take() {
            let cursor = if self.last_structured_view == DataView::Form {
                Some(ReadCursor::At(self.grid.form_point()))
            } else {
                self.joined_cursor()
            };
            let notice = if batch || self.deleting.is_some() {
                format!("已删除 {count} 行")
            } else if self.draft.as_ref().is_some_and(|draft| draft.row.is_none()) {
                format!("已新增 {count} 行")
            } else {
                format!("已保存更改，影响 {count} 行")
            };
            self.draft = None;
            self.deleting = None;
            self.grid.checked.clear();
            self.joined_grid.clear_checks();
            if self.query.is_some() {
                self.page = None;
                if let Some(joined) = &mut self.joined {
                    joined.response = None;
                }
            }
            match queued_move {
                Some(QueuedMove::Cursor(cursor)) => self.load(workspace, self.offset, Some(cursor)),
                Some(QueuedMove::Page(action)) => self.navigate(workspace, action),
                None if !self.duplicate_only => self.load(workspace, self.offset, cursor),
                None => {}
            }
            self.notice = Some(notice);
        } else if batch {
            self.deleting = None;
            self.grid.checked.clear();
            self.error = Some(format!("删除失败。{}", workspace.status));
        } else {
            self.error = Some(format!("保存失败：{}", workspace.status));
        }
    }

    fn move_to(&mut self, workspace: &mut Workspace, target: RowMove) {
        if workspace.busy()
            || self.pending.is_some()
            || self.duplicate_prompt.is_some()
            || self.deleting.is_some()
            || self.inspection.is_some()
            || self.filter_draft.is_some()
        {
            return;
        }
        let Some(page) = self
            .joined
            .as_ref()
            .map_or(self.page.as_ref(), joined::Joined::page)
        else {
            return;
        };
        let source_point = match &target {
            RowMove::Row(point) | RowMove::JoinedRow { source: point, .. } => Some(*point),
            RowMove::Page(_) => None,
        };
        let focus_result = |grid: &mut ResultGrid| {
            if let RowMove::JoinedRow { result, .. } = &target
                && let Some(set) = self
                    .joined
                    .as_ref()
                    .and_then(|joined| joined.response.as_ref())
            {
                grid.focus_source_point(&set.result, *result);
            }
        };
        if let Some(point) = source_point {
            if page.rows.get(point.row).is_none() {
                return;
            }
            if self.draft.as_ref().and_then(|draft| draft.row) == Some(point.row) {
                self.grid.focus_form_point(page, point);
                focus_result(&mut self.joined_grid);
                return;
            }
        }
        if let Some(draft) = &self.draft {
            if draft.row.is_none() || workspace.server.is_none() || page.edit_error.is_some() {
                return;
            }
            let edit = draft.edit(page);
            if matches!(&edit, TableEdit::Update { values, .. } if values.is_empty()) {
                self.draft = None;
                self.error = None;
            } else {
                let queued = match target {
                    RowMove::Row(point) => QueuedMove::Cursor(ReadCursor::Record(
                        page.rows[point.row].clone(),
                        point.column,
                    )),
                    RowMove::JoinedRow { source, result } => QueuedMove::Cursor(
                        ReadCursor::JoinedRecord(page.rows[source.row].clone(), result),
                    ),
                    RowMove::Page(action) => QueuedMove::Page(action),
                };
                self.write(workspace, edit, None);
                if matches!(self.pending, Some(Pending::Write(_))) {
                    self.queued_move = Some(queued);
                }
                return;
            }
        }
        match target {
            RowMove::Row(point) | RowMove::JoinedRow { source: point, .. } => {
                self.grid.focus_form_point(page, point);
                focus_result(&mut self.joined_grid);
            }
            RowMove::Page(action) => self.navigate(workspace, action),
        }
    }

    #[cfg(test)]
    fn begin_grid_draft(&mut self, row: Option<usize>, column: usize, text: Option<String>) {
        self.begin_grid_draft_at(row, column, text, None);
    }

    fn begin_grid_draft_at(
        &mut self,
        row: Option<usize>,
        column: usize,
        text: Option<String>,
        joined_point: Option<Point>,
    ) {
        if self.draft.is_some()
            || self.pending.is_some()
            || self.inspection.is_some()
            || self.deleting.is_some()
            || self.duplicate_prompt.is_some()
            || self.filter_draft.is_some()
        {
            return;
        }
        let Some(page) = self
            .joined
            .as_ref()
            .map_or(self.page.as_ref(), joined::Joined::page)
        else {
            return;
        };
        if page.edit_error.is_some()
            || !page.columns.get(column).is_some_and(|field| field.writable)
            || row.map_or(!page.can_insert, |row| row >= page.rows.len())
        {
            return;
        }
        if let Some(row) = row {
            self.grid.focus_form_point(page, Point { row, column });
        } else {
            self.grid.selection.clear();
            self.grid.blank_column = Some(column);
        }
        let field = &page.columns[column];
        match text {
            Some(text) if field.is_binary() => {
                self.inspection = Some((
                    row,
                    column,
                    Inspector::edit_typed(
                        page.column_label(column).to_owned(),
                        text,
                        field.is_binary(),
                        field.nullable,
                    ),
                ));
            }
            text => {
                if let Some(row_index) = row
                    && self.view != DataView::Form
                    && !field.is_binary()
                    && !cell_has_newline(&page.rows[row_index].cells[column])
                {
                    self.draft = Some(Draft::new_inline(
                        page,
                        row_index,
                        column,
                        text,
                        joined_point,
                    ));
                } else {
                    let mut draft = Draft::new(page, row, Some(column));
                    if let Some(text) = text {
                        draft.fields[column].set_cell(Cell::Text(text));
                        draft.focus_at_end = true;
                    }
                    self.draft = Some(draft);
                }
            }
        }
        self.error = None;
        self.notice = None;
    }

    fn show_inspection(&mut self, ctx: &egui::Context, settings: EditorSettings) {
        let Some((row, column, inspector)) = self.inspection.as_mut() else {
            return;
        };
        match inspector.show(ctx, settings) {
            Outcome::Continue => {}
            Outcome::Dismiss => {
                self.inspection = None;
                self.grid.selection.request_focus();
            }
            Outcome::Apply(value) => {
                if let Some(page) = self
                    .joined
                    .as_ref()
                    .map_or(self.page.as_ref(), joined::Joined::page)
                {
                    let draft = self
                        .draft
                        .get_or_insert_with(|| Draft::new(page, *row, None));
                    draft.fields[*column].set_cell(value);
                    self.error = None;
                }
                self.inspection = None;
            }
        }
    }

    fn navigate(&mut self, workspace: &mut Workspace, action: Navigate) {
        if !matches!(action, Navigate::Refresh) {
            self.grid.scroll_to_top = true;
        }
        let cursor = if self.last_structured_view == DataView::Form {
            Some({
                let point = self.grid.form_point();
                match &action {
                    Navigate::Previous => ReadCursor::Last(point.column),
                    Navigate::Refresh => ReadCursor::At(point),
                    Navigate::Next(_) | Navigate::First => ReadCursor::At(Point {
                        row: 0,
                        column: point.column,
                    }),
                }
            })
        } else if matches!(action, Navigate::Refresh) {
            self.joined_cursor()
        } else {
            None
        };
        self.notice = None;
        if self.query.is_some() {
            self.load(workspace, 0, cursor);
            return;
        }
        let rows = match self.table_range {
            TableReadRange::Page { rows, .. } => rows,
            TableReadRange::All => self.table_fields.row_limit(),
        };
        let mut previous_offsets = self.previous_offsets.clone();
        let refresh = matches!(action, Navigate::Refresh);
        let range = match action {
            Navigate::Previous => TableReadRange::Page {
                offset: previous_offsets
                    .pop()
                    .unwrap_or(self.offset.saturating_sub(rows.get())),
                rows,
            },
            Navigate::Next(offset) => {
                previous_offsets.push(self.offset);
                TableReadRange::Page { offset, rows }
            }
            Navigate::Refresh => {
                let range = self.table_fields.range();
                if range != self.table_range {
                    previous_offsets.clear();
                    self.grid.scroll_to_top = true;
                }
                range
            }
            Navigate::First => {
                previous_offsets.clear();
                self.table_range_at(0)
            }
        };
        self.start_table_read(
            workspace,
            PendingTableRead {
                cursor,
                range,
                previous_offsets,
                retry_from_start: !refresh,
                focus_refresh: false,
            },
        );
    }

    fn table_range_at(&self, offset: usize) -> TableReadRange {
        match self.table_range {
            TableReadRange::Page { rows, .. } => TableReadRange::Page { offset, rows },
            TableReadRange::All => TableReadRange::All,
        }
    }

    fn start_table_read(&mut self, workspace: &mut Workspace, request: PendingTableRead) {
        let Some(object) = self.object.as_ref() else {
            return;
        };
        self.error = None;
        self.table_read_stopping = false;
        if workspace.server.is_none() {
            self.server_selection = self.applied_selection.clone();
            self.error = Some("连接已断开。".into());
        } else if workspace.send(
            Command::ReadTableData {
                object: object.clone(),
                selection: self.server_selection.clone(),
                range: request.range,
            },
            "正在读取表数据…",
        ) {
            self.pending = Some(Pending::TableRead(request));
        } else {
            self.server_selection = self.applied_selection.clone();
            self.error = Some(format!("读取表数据失败：{}", workspace.status));
        }
    }

    fn load(&mut self, workspace: &mut Workspace, offset: usize, cursor: Option<ReadCursor>) {
        if self.query.is_none() {
            self.start_table_read(
                workspace,
                PendingTableRead {
                    range: self.table_range_at(offset),
                    previous_offsets: self.previous_offsets.clone(),
                    cursor,
                    retry_from_start: true,
                    focus_refresh: false,
                },
            );
            return;
        }
        if !self.origin_matches(workspace) {
            self.error = Some("原联表查询页已变化，请重新执行 F8。".into());
            return;
        }
        self.grid.scroll_to_top |= self.offset != offset;
        self.filter_draft = None;
        self.offset = offset;
        self.error = None;
        let command = if let Some((sql, limit)) = &self.query {
            Command::QueryEditable {
                sql: sql.clone(),
                limit: *limit,
            }
        } else {
            return;
        };
        if workspace.server.is_none() {
            self.error = Some("连接已断开。".into());
        } else if workspace.send(command, "正在读取可编辑查询结果…") {
            self.pending = Some(Pending::Read(cursor));
        } else {
            self.error = Some(format!("读取可编辑查询结果失败：{}", workspace.status));
        }
    }

    fn write(
        &mut self,
        workspace: &mut Workspace,
        edit: TableEdit,
        choice: Option<DuplicateChoice>,
    ) {
        let Some(object) = self.write_target(workspace) else {
            self.queued_move = None;
            return;
        };
        self.error = None;
        self.notice = None;
        let label = match &edit {
            TableEdit::Insert { .. } => "正在新增一行…",
            TableEdit::Update { .. } => "正在保存更改…",
            TableEdit::Delete { .. } => "正在删除所选行…",
        };
        let pending = edit.clone();
        let command = if let Some(choice) = choice {
            Command::ConfirmTableEdit {
                object,
                edit,
                choice,
            }
        } else {
            Command::EditTable { object, edit }
        };
        if workspace.send(command, label) {
            self.pending = Some(Pending::Write(pending));
        } else {
            self.queued_move = None;
            self.error = Some(format!("保存失败：{}", workspace.status));
        }
    }

    fn delete_rows(&mut self, workspace: &mut Workspace, originals: Vec<TableRow>) {
        let Some(object) = self.write_target(workspace) else {
            return;
        };
        let label = format!("正在删除已勾选的 {} 行…", originals.len());
        self.error = None;
        self.notice = None;
        if workspace.send(Command::DeleteTableRows { object, originals }, &label) {
            self.pending = Some(Pending::DeleteRows);
        } else {
            self.error = Some(format!("删除失败。{}", workspace.status));
        }
    }

    fn write_target(&mut self, workspace: &Workspace) -> Option<DbObject> {
        let object = self.object.clone()?;
        if !self.origin_matches(workspace)
            || self
                .joined
                .as_ref()
                .is_some_and(|joined| joined.page().is_none())
        {
            self.error = Some("请重新读取原联表查询后再保存。".into());
            return None;
        }
        Some(object)
    }
}

fn restore_joined_cursor(
    joined: &joined::Joined,
    result_grid: &mut ResultGrid,
    form_grid: &mut DataGrid,
    cursor: ReadCursor,
) -> bool {
    let Some(target) = joined.binding() else {
        return false;
    };
    let ReadCursor::JoinedRecord(record, preferred) = cursor else {
        return form_grid.restore_cursor(&target.page, cursor);
    };
    let Some(source_row) = unique_record_position(&target.page, &record) else {
        return false;
    };
    let Some(row) = joined::result_row(target, source_row, preferred.row) else {
        return false;
    };
    let Some(reply) = &joined.response else {
        return false;
    };
    let point = Point {
        row,
        column: preferred.column,
    };
    if let Some(source) = joined::source_point(target, point) {
        form_grid.focus_form_point(&target.page, source);
    }
    result_grid.focus_source_point(&reply.result, point)
}

fn unique_record_position(page: &TablePage, record: &TableRow) -> Option<usize> {
    let mut matching = page
        .rows
        .iter()
        .enumerate()
        .filter(|(_, row)| record.same_source_record(row));
    let (row, _) = matching.next()?;
    matching.next().is_none().then_some(row)
}

fn joined_source_rows(target: &JoinedEditTarget, rows: Vec<usize>) -> Vec<usize> {
    rows.into_iter()
        .filter_map(|row| joined::source_row(target, row))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn joined_move(target: &JoinedEditTarget, result: Point, current_column: usize) -> Option<RowMove> {
    let row = joined::source_row(target, result.row)?;
    let column = joined::source_point(target, result).map_or(current_column, |point| point.column);
    Some(RowMove::JoinedRow {
        source: Point { row, column },
        result,
    })
}

fn joined_edit_action(
    target: &JoinedEditTarget,
    result: Point,
    text: Option<String>,
) -> Option<GridAction> {
    let point = joined::source_point(target, result)?;
    Some(match text {
        Some(text) => GridAction::BeginJoinedDraft {
            row: Some(point.row),
            column: point.column,
            text: Some(text),
            joined_point: result,
        },
        None => GridAction::OpenJoined(point, result),
    })
}

pub(super) fn dialog(
    ctx: &egui::Context,
    state: &mut Option<RowEditor>,
    workspaces: &mut [Workspace],
    settings: EditorSettings,
    grid_widths: &mut GridWidths,
) {
    let Some(editor) = state else { return };
    let index = editor.joined.as_ref().map_or(Some(editor.index), |joined| {
        workspaces
            .iter()
            .position(|workspace| workspace.key == joined.workspace)
    });
    let Some(workspace) = index.and_then(|index| workspaces.get_mut(index)) else {
        *state = None;
        return;
    };
    editor.poll(workspace);
    if editor.duplicate_only && editor.draft.is_none() && editor.pending.is_none() {
        *state = None;
        return;
    }
    if present(
        editor,
        workspace,
        Surface::Dialog(ctx),
        settings,
        grid_widths,
    ) {
        *state = None;
    }
}

pub(super) fn panel(
    ui: &mut egui::Ui,
    state: &mut Option<RowEditor>,
    workspace: &mut Workspace,
    keyboard_allowed: bool,
    request_focus: bool,
    settings: EditorSettings,
    grid_widths: &mut GridWidths,
) {
    let Some(editor) = state else {
        ui.label(RichText::new("在对象浏览器中选择一张表以查看数据。").color(style::muted()));
        return;
    };
    if request_focus && keyboard_allowed {
        editor.request_focus();
    }
    present(
        editor,
        workspace,
        Surface::Docked(ui, keyboard_allowed),
        settings,
        grid_widths,
    );
}

enum Surface<'a> {
    Dialog(&'a egui::Context),
    Docked(&'a mut egui::Ui, bool),
}

fn present(
    editor: &mut RowEditor,
    workspace: &mut Workspace,
    surface: Surface<'_>,
    settings: EditorSettings,
    grid_widths: &mut GridWidths,
) -> bool {
    let (ctx, dock_height, keyboard_allowed, draft_keys) = match &surface {
        Surface::Dialog(ctx) => ((**ctx).clone(), None, true, true),
        Surface::Docked(ui, keyboard_allowed) => {
            let area = ui.max_rect().intersect(ui.clip_rect());
            let focus = ui.memory(|memory| memory.focused());
            let focus_inside = focus
                .and_then(|id| ui.ctx().read_response(id))
                .is_some_and(|response| area.contains(response.rect.center()));
            (
                ui.ctx().clone(),
                Some(ui.available_height().max(0.0)),
                *keyboard_allowed,
                focus_inside,
            )
        }
    };
    let docked = dock_height.is_some();
    let modal_id = egui::Id::new(("table-row-editor", editor.index));
    let dock_id = (
        "row-editor-pane",
        workspace.key,
        workspace.row_editor_tab_id(),
        editor.grid.id,
    );

    let origin_matches = editor.origin_matches(workspace);
    let connected = workspace.server.is_some() && origin_matches;
    let waiting = workspace.busy() || editor.pending.is_some();
    let reviewing_matches = editor.duplicate_prompt.is_some();
    let read_only_modal = editor.joined_grid.has_modal() || editor.export.is_some();
    let ready = connected && !waiting && !reviewing_matches && !read_only_modal;
    let table_mode = editor.query.is_none() && !editor.duplicate_only;
    let show_data_info = workspace.show_data_info && (table_mode || editor.query.is_some());
    let browsing = editor.draft.is_none()
        && editor.deleting.is_none()
        && editor.inspection.is_none()
        && editor.filter_draft.is_none();
    let single_query_read_only =
        editor.query.is_some() && editor.joined.is_none() && editor.query_read_only;
    let read_only = editor
        .joined
        .as_ref()
        .is_some_and(|joined| joined.selected.is_none())
        || single_query_read_only;
    let can_switch_view = !waiting
        && !read_only_modal
        && !reviewing_matches
        && editor.deleting.is_none()
        && editor.inspection.is_none()
        && editor.filter_draft.is_none();
    let copy_headers = workspace.copy_column_headers;
    let can_export = keyboard_allowed && !waiting && editor.can_export_loaded();
    let closing_allowed = editor.draft.is_none()
        && editor.deleting.is_none()
        && editor.inspection.is_none()
        && !reviewing_matches
        && !read_only_modal
        && !waiting;
    let size = ctx.content_rect().size();
    let mut close = false;
    let mut export_kind = None;
    let mut navigate = None;
    let mut row_move = None;
    let mut draft_start = None;
    let mut write = None;
    let mut batch_delete = None;
    let mut duplicate_choice = None;
    let mut cancel_duplicates = false;
    let contents = |ui: &mut egui::Ui| {
        let top = ui.cursor().min.y;
        if !docked {
            ui.set_width((size.x - 80.0).clamp(480.0, 1110.0));
        }
        let info_height = if show_data_info {
            data_info_line_height(ui)
        } else {
            0.0
        };
        let dock_available = |ui: &egui::Ui| {
            dock_height.map(|height| (height - (ui.cursor().min.y - top) - info_height).max(0.0))
        };
        if !docked {
            ui.horizontal(|ui| {
                style::page_title(ui, if editor.query.is_some() {
                    "编辑查询结果"
                } else {
                    "编辑表数据"
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    close = ui
                        .add_enabled(closing_allowed, Button::new("关闭"))
                        .clicked();
                });
            });
            ui.add_space(4.0);
        }
        let joined_unselected = editor
            .joined
            .as_ref()
            .is_some_and(|joined| joined.selected.is_none());
        let has_page = !joined_unselected
            && editor
                .joined
                .as_ref()
                .map_or(editor.page.as_ref(), joined::Joined::page)
                .is_some();

        let render_view_switcher =
            |ui: &mut egui::Ui, editor: &mut RowEditor, workspace: &mut Workspace| {
                let mut view = editor.view;
                ui.horizontal(|ui| {
                    if !can_switch_view {
                        ui.disable();
                    }
                    result_view_controls(ui, &mut view, !read_only);
                    if keyboard_allowed
                        && can_switch_view
                        && !egui::Popup::is_any_open(ui.ctx())
                        && let Some(next) = ui.input_mut(|input| {
                            super::consume_result_mode_shortcut(
                                input,
                                view,
                                editor.last_structured_view,
                                !read_only,
                            )
                        })
                    {
                        view = next;
                    }
                });
                let previous = editor.view;
                editor.set_view(view);
                if editor.view != previous && !editor.duplicate_only && origin_matches {
                    workspace.set_result_mode(editor.view);
                }
            };

        let render_table_paging_controls =
            |ui: &mut egui::Ui,
             editor: &mut RowEditor,
             workspace: &mut Workspace,
             navigate: &mut Option<Navigate>| {
                let paging_label = if editor.table_fields.all {
                    "显示全部".to_owned()
                } else if editor.table_fields.offset > 0 {
                    format!(
                        "偏移 {} · {} 行",
                        editor.table_fields.offset, editor.table_fields.rows
                    )
                } else {
                    format!("每页 {} 行", editor.table_fields.rows)
                };
                ui.horizontal(|ui| {
                    if !(ready && browsing) {
                        ui.disable();
                    }
                    ui.menu_button(paging_label, |ui| {
                        ui.set_min_width(210.0);
                        let all = &mut editor.table_fields.all;
                        ui.horizontal(|ui| {
                            let change = ui.radio_value(all, false, "限制行数").changed();
                            if ui.radio_value(all, true, "显示全部").changed() || change {
                                *navigate = Some(Navigate::Refresh);
                                ui.close();
                            }
                        });
                        ui.add_space(4.0);
                        ui.add_enabled_ui(!*all, |ui| {
                            egui::Grid::new("table-paging-popover")
                                .num_columns(2)
                                .spacing([10.0, 6.0])
                                .show(ui, |ui| {
                                    ui.label("起始偏移").on_hover_text("从 0 开始。");
                                    ui.add(
                                        egui::DragValue::new(&mut editor.table_fields.offset)
                                            .range(0..=usize::MAX),
                                    );
                                    ui.end_row();
                                    ui.label("每页行数");
                                    ui.add(
                                        egui::DragValue::new(&mut editor.table_fields.rows)
                                            .range(1..=usize::MAX),
                                    );
                                    ui.end_row();
                                });
                        });
                        ui.add_space(6.0);
                        if ui.add(style::primary("应用并读取")).clicked() {
                            *navigate = Some(Navigate::Refresh);
                            ui.close();
                        }
                    })
                    .response
                    .on_hover_text("设置分页模式、起始偏移和每页行数");
                });
                if matches!(editor.pending, Some(Pending::TableRead(_))) {
                    let tip = if editor.table_read_stopping {
                        "正在停止读取…"
                    } else {
                        "停止读取"
                    };
                    if style::icon_button_tinted(
                        ui,
                        style::ToolbarIcon::Stop,
                        !editor.table_read_stopping,
                        Some(style::error()),
                        false,
                        None,
                        tip,
                    )
                    .clicked()
                        && workspace.cancel_table_read()
                    {
                        editor.table_read_stopping = true;
                    }
                } else if style::icon_button(
                    ui,
                    style::ToolbarIcon::Refresh,
                    ready && browsing,
                    "重新读取",
                )
                .clicked()
                {
                    *navigate = Some(Navigate::Refresh);
                }
            };

        if !has_page {
            let mut switched_target = None;
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(5.0, 4.0);
                ui.spacing_mut().button_padding = egui::vec2(7.0, 2.5);
                ui.spacing_mut().interact_size.y = 22.0;
                ui.horizontal_wrapped(|ui| {
                    render_view_switcher(ui, editor, workspace);
                    if let Some(joined) = &editor.joined {
                        ui.separator();
                        switched_target =
                            joined.target_picker(ui, ready && editor.can_switch_target());
                    }
                    ui.separator();
                    if table_mode {
                        render_table_paging_controls(ui, editor, workspace, &mut navigate);
                    } else if joined_unselected {
                        if let Some(reply) = editor
                            .joined
                            .as_ref()
                            .and_then(|joined| joined.response.as_ref())
                        {
                            ui.label(
                                RichText::new(format!("显示 {} 行联表", reply.result.rows.len()))
                                    .small()
                                    .color(style::muted()),
                            );
                            if reply.result.truncated {
                                ui.label(
                                    RichText::new("已达行数上限")
                                        .small()
                                        .color(style::warning()),
                                );
                            }
                            if reply.targets.is_empty() {
                                ui.label(
                                    RichText::new("无可写入源表")
                                        .small()
                                        .color(style::muted()),
                                );
                            }
                        }
                        if style::icon_button(
                            ui,
                            style::ToolbarIcon::Refresh,
                            ready && !read_only_modal,
                            "重新读取",
                        )
                        .clicked()
                        {
                            navigate = Some(Navigate::Refresh);
                        }
                    } else if ready {
                        if !editor.previous_offsets.is_empty()
                            && style::icon_button(ui, style::ToolbarIcon::PrevPage, true, "上一页")
                                .clicked()
                        {
                            navigate = Some(Navigate::Previous);
                        }
                        if style::icon_button(ui, style::ToolbarIcon::Refresh, true, "重新读取")
                            .clicked()
                        {
                            navigate = Some(Navigate::Refresh);
                        }
                    }
                    if joined_unselected
                        && editor.view != DataView::Text
                        && let Some(reply) = editor
                            .joined
                            .as_ref()
                            .and_then(|joined| joined.response.as_ref())
                    {
                        ui.separator();
                        editor.joined_grid.show_controls(
                            ui,
                            &reply.result,
                            copy_headers,
                            keyboard_allowed && !waiting && !read_only_modal,
                        );
                    }
                    if !editor.duplicate_only {
                        if !(joined_unselected && editor.view != DataView::Text) {
                            ui.separator();
                        }
                        export_kind = super::output::result_export_menu(ui, can_export);
                    }
                    if let Some(notice) = &editor.notice {
                        ui.separator();
                        style::badge(ui, notice, style::accent(), style::accent_soft());
                    }
                });
            });
            if let Some(target) = switched_target {
                editor.select_joined_target(target);
            }
            if editor.joined.is_some() {
                if !origin_matches {
                    ui.colored_label(style::error(), "原查询页已变化，请回到原查询重新执行 F8。");
                } else if editor.draft.is_some() {
                    ui.colored_label(style::muted(), "保存或取消草稿后可切换写入目标。");
                }
            } else if let Some(object) = &editor.object
                && (!table_mode || !workspace.show_data_info || editor.page.is_none())
            {
                let name = format!("{}.{}", object.schema, object.name);
                ui.add(egui::Label::new(RichText::new(&name).monospace()).truncate())
                    .on_hover_text(name);
            }
            if let Some(error) = &editor.error {
                ui.colored_label(style::error(), error);
            }
            if waiting {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(&workspace.status);
                    if matches!(editor.pending, Some(Pending::Read(_)))
                        && workspace.reading_editable_result()
                    {
                        let stopping = workspace.editable_read_stopping();
                        let label = if stopping {
                            "正在停止…"
                        } else {
                            "停止读取"
                        };
                        let shortcut = keyboard_allowed && ui.input_mut(super::consume_f8);
                        if ui
                            .add_enabled(!stopping, Button::new(label))
                            .on_hover_text("F8")
                            .clicked()
                            || shortcut
                        {
                            workspace.cancel_editable_read();
                        }
                    }
                });
            }
            if let Some(joined) = &editor.joined
                && joined.selected.is_none()
            {
                let grid_id = joined.result_grid_id();
                if let Some(reply) = &joined.response {
                    let height = dock_available(ui).map_or_else(
                        || (size.y - 240.0 - info_height).clamp(150.0, 510.0),
                        |available| available.max(56.0),
                    );
                    ui.push_id(grid_id, |ui| {
                        ui.set_height(height);
                        if editor.view == DataView::Text {
                            editor.joined_grid.show_text(
                                ui,
                                &reply.result,
                                &mut editor.text_rows,
                                copy_headers,
                                keyboard_allowed && !waiting && !read_only_modal,
                                std::mem::take(&mut editor.focus_text),
                            );
                        } else {
                            editor.joined_grid.show_read_only(
                                ui,
                                &reply.result,
                                copy_headers,
                                keyboard_allowed && !waiting && !read_only_modal,
                                settings,
                                grid_widths,
                            );
                        }
                    });
                    if show_data_info {
                        let summary = reply
                            .query_timing
                            .map(|timing| query_time_label(timing.elapsed, timing.execution))
                            .unwrap_or_default();
                        data_info_line(
                            ui,
                            &summary,
                            editor.query.as_ref().map(|(sql, _)| sql.as_str()),
                        );
                    }
                }
            }
            return;
        }

        let (
            page_edit_error_none,
            page_has_more,
            page_offset,
            page_row_count,
            page_can_insert,
            draft_has_changes,
        ) = {
            let page = editor
                .joined
                .as_ref()
                .map_or(editor.page.as_ref(), joined::Joined::page)
                .expect("loaded page");
            (
                page.edit_error.is_none(),
                page.has_more,
                page.offset,
                page.rows.len(),
                page.can_insert,
                editor
                    .draft
                    .as_ref()
                    .is_some_and(|draft| draft.has_changes(page)),
            )
        };
        let draft_is_inline = editor.draft.as_ref().is_some_and(Draft::is_inline);
        let editable = ready && !read_only && page_edit_error_none;
        let in_form = editor.view == DataView::Form;
        let in_text = editor.view == DataView::Text;
        let can_edit_rows = editable && !in_text;
        let full_join_info = if in_form {
            None
        } else {
            editor.joined.as_ref().and_then(|joined| {
                let result = &joined.response.as_ref()?.result;
                let target = joined.binding()?;
                Some((
                    result.rows.len(),
                    result.truncated,
                    joined_source_rows(target, editor.joined_grid.selected_source_rows()),
                    joined_source_rows(target, editor.joined_grid.checked_source_rows()),
                ))
            })
        };
        let has_full_join = full_join_info.is_some();
        let selected_join_rows = full_join_info
            .as_ref()
            .map_or_else(Vec::new, |(_, _, selected, _)| selected.clone());
        let checked_join_rows = full_join_info
            .as_ref()
            .map_or_else(Vec::new, |(_, _, _, checked)| checked.clone());
        let mut point = editor.grid.form_point();
        let draft_row = editor.draft.as_ref().and_then(|draft| draft.row);
        if let Some(row) = draft_row {
            point.row = row;
        }
        let moving_draft = keyboard_allowed
            && editable
            && !in_text
            && draft_row.is_some()
            && editor.deleting.is_none()
            && editor.inspection.is_none()
            && editor.filter_draft.is_none();
        let can_page = ready && (browsing || moving_draft);
        let has_previous_page = editor.has_previous_page();
        let mut toolbar_action = None;
        let mut open_draft: Option<Option<usize>> = None;
        let mut switched_target = None;
        let mut inline_commit = false;
        let mut inline_cancel = false;

        // 将视图切换、目标表选择、分页、数据读写、未保存提示和导出统一收敛到单行紧凑工具栏
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(5.0, 4.0);
            ui.spacing_mut().button_padding = egui::vec2(7.0, 2.5);
            ui.spacing_mut().interact_size.y = 22.0;
            ui.horizontal_wrapped(|ui| {
                render_view_switcher(ui, editor, workspace);
                if let Some(joined) = &editor.joined {
                    ui.separator();
                    switched_target =
                        joined.target_picker(ui, ready && editor.can_switch_target());
                } else if editor.query.is_some()
                    && let Some(object) = &editor.object
                {
                    ui.separator();
                    let can_toggle = ready && browsing && page_edit_error_none;
                    let table_name = object.name.clone();
                    let label = if editor.query_read_only {
                        RichText::new("(只读)").color(style::muted())
                    } else {
                        RichText::new(&table_name).strong().color(style::accent())
                    };
                    ui.horizontal(|ui| {
                        if !can_toggle {
                            ui.disable();
                        }
                        egui::ComboBox::from_id_salt(("single-query-target", editor.grid.id))
                            .width(150.0)
                            .selected_text(label)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut editor.query_read_only, true, "(只读)");
                                ui.selectable_value(
                                    &mut editor.query_read_only,
                                    false,
                                    &table_name,
                                );
                            })
                            .response
                            .on_hover_text(
                                "选择表名后归属该表的单元格变为可编辑状态；选「(只读)」锁定不可改",
                            );
                    });
                }
                ui.separator();
                if editor.query.is_none()
                    && style::icon_button(
                        ui,
                        style::ToolbarIcon::PrevPage,
                        can_page && has_previous_page,
                        "上一页",
                    )
                    .clicked()
                {
                    row_move = Some(RowMove::Page(Navigate::Previous));
                }
                if let Some((join_rows, _, _, _)) = &full_join_info {
                    ui.label(
                        RichText::new(format!("{join_rows} 行联表"))
                            .small()
                            .color(style::muted()),
                    );
                } else if page_row_count == 0 {
                    ui.label(RichText::new("0 行").small().color(style::muted()));
                } else {
                    ui.label(
                        RichText::new(format!(
                            "{}–{}",
                            page_offset + 1,
                            page_offset + page_row_count
                        ))
                        .small()
                        .monospace()
                        .color(style::muted()),
                    );
                }
                if editor.query.is_none()
                    && style::icon_button(
                        ui,
                        style::ToolbarIcon::NextPage,
                        can_page && page_has_more,
                        if editor.table_range == TableReadRange::All {
                            "继续分页"
                        } else {
                            "下一页"
                        },
                    )
                    .clicked()
                {
                    row_move = Some(RowMove::Page(Navigate::Next(page_offset + page_row_count)));
                }
                if table_mode {
                    render_table_paging_controls(ui, editor, workspace, &mut navigate);
                } else if style::icon_button(
                    ui,
                    style::ToolbarIcon::Refresh,
                    ready && browsing,
                    "重新读取",
                )
                .clicked()
                {
                    navigate = Some(Navigate::Refresh);
                }
                if editor.query.is_none() && !editor.server_selection.predicates.is_empty() {
                    let label = format!("清除筛选（{}）", editor.server_selection.predicates.len());
                    if ui
                        .add_enabled(ready && browsing, Button::new(label))
                        .clicked()
                    {
                        editor.server_selection.predicates.clear();
                        navigate = Some(Navigate::First);
                    }
                }
                if editor.query.is_none() && !editor.server_selection.order.is_empty() {
                    let label = format!("清除排序（{}）", editor.server_selection.order.len());
                    if ui
                        .add_enabled(ready && browsing, Button::new(label))
                        .clicked()
                    {
                        editor.server_selection.order.clear();
                        navigate = Some(Navigate::First);
                    }
                }
                if editor.query.is_some()
                    && full_join_info
                        .as_ref()
                        .map_or(page_has_more, |(_, truncated, _, _)| *truncated)
                {
                    ui.label(
                        RichText::new("已达行数上限")
                            .small()
                            .color(style::warning()),
                    );
                }
                if table_mode && editor.table_range == TableReadRange::All && page_has_more {
                    ui.label(
                        RichText::new("已达显示上限")
                            .small()
                            .color(style::warning()),
                    );
                }
                ui.separator();
                let add_enabled = can_edit_rows && browsing && page_can_insert;
                let add_tip = if add_enabled {
                    "新增一行"
                } else if editor.joined.is_some() {
                    "新增一行（需打开完整数据表）"
                } else if read_only {
                    "新增一行（当前处于只读模式，请在下拉框中选择目标表）"
                } else {
                    "新增一行（需打开完整数据表或查询全部字段）"
                };
                if style::icon_button_tinted(
                    ui,
                    style::ToolbarIcon::AddRow,
                    add_enabled,
                    Some(style::success()),
                    false,
                    None,
                    add_tip,
                )
                .clicked()
                {
                    open_draft = Some(None);
                }
                let row = if has_full_join {
                    match selected_join_rows.as_slice() {
                        [row] => Some(*row),
                        _ => None,
                    }
                } else if in_form {
                    (point.row < page_row_count).then_some(point.row)
                } else {
                    editor.grid.single_row()
                };
                let selected = can_edit_rows && browsing && row.is_some();
                if style::icon_button(
                    ui,
                    style::ToolbarIcon::EditRow,
                    selected,
                    if in_form {
                        "编辑当前行"
                    } else {
                        "编辑所选行（也可直接双击单元格就地修改）"
                    },
                )
                .clicked()
                    && let Some(row) = row
                {
                    open_draft = Some(Some(row));
                }
                if style::icon_button(
                    ui,
                    style::ToolbarIcon::DuplicateRow,
                    selected,
                    "复制为新行",
                )
                .clicked()
                    && let Some(row) = row
                {
                    toolbar_action = Some(GridAction::Duplicate(row));
                }
                let checked = if has_full_join {
                    checked_join_rows.len()
                } else if in_form {
                    0
                } else {
                    editor.grid.checked.rows.len()
                };
                let delete_label = if checked > 0 {
                    format!("删除已勾选的 {checked} 行")
                } else if selected_join_rows.len() > 1 {
                    format!("删除所选的 {} 条源记录", selected_join_rows.len())
                } else if in_form {
                    "删除当前行".into()
                } else {
                    "删除所选行".into()
                };
                let delete_badge = if checked > 0 {
                    Some(checked.to_string())
                } else if selected_join_rows.len() > 1 {
                    Some(selected_join_rows.len().to_string())
                } else {
                    None
                };
                if style::icon_button_tinted(
                    ui,
                    style::ToolbarIcon::DeleteRow,
                    can_edit_rows
                        && browsing
                        && !read_only_modal
                        && (checked > 0 || row.is_some() || !selected_join_rows.is_empty()),
                    Some(style::error()),
                    false,
                    delete_badge.as_deref(),
                    &delete_label,
                )
                .clicked()
                {
                    editor.deleting = if has_full_join {
                        match if checked > 0 {
                            &checked_join_rows
                        } else {
                            &selected_join_rows
                        }
                        .as_slice()
                        {
                            [] => None,
                            [row] => Some(Deletion::Row(*row)),
                            rows => Some(Deletion::Checked(rows.to_vec())),
                        }
                    } else if in_form {
                        row.map(Deletion::Row)
                    } else {
                        editor.grid.deletion(row)
                    };
                }
                ui.separator();
                if in_form {
                    if let Some(page) = editor
                        .joined
                        .as_ref()
                        .map_or(editor.page.as_ref(), joined::Joined::page)
                        && page.rows.get(point.row).is_some()
                    {
                        style::icon_menu_button(
                            ui,
                            style::ToolbarIcon::Copy,
                            !waiting,
                            "复制（选择范围与格式）",
                            |ui| {
                                copy_menu(ui, page, None, Some(point), copy_headers);
                            },
                        );
                    }
                } else if has_full_join && !in_text {
                    if let Some(reply) = editor
                        .joined
                        .as_ref()
                        .and_then(|joined| joined.response.as_ref())
                    {
                        editor.joined_grid.show_controls(
                            ui,
                            &reply.result,
                            copy_headers,
                            keyboard_allowed
                                && !waiting
                                && !moving_draft
                                && !draft_is_inline
                                && !read_only_modal,
                        );
                    }
                } else if !in_text
                    && let Some(page) = editor.page.as_ref()
                {
                    ui.horizontal(|ui| {
                        if waiting {
                            ui.disable();
                        }
                        editor.grid.copy_controls(ui, page, copy_headers);
                    });
                }
                if !editor.duplicate_only {
                    export_kind = super::output::result_export_menu(ui, can_export);
                }
                if draft_has_changes {
                    ui.separator();
                    egui::Frame::new()
                        .fill(style::error_soft())
                        .stroke(egui::Stroke::new(
                            1.0_f32,
                            style::error().gamma_multiply(0.45),
                        ))
                        .corner_radius(egui::CornerRadius::same(5))
                        .inner_margin(egui::Margin::symmetric(8, 2))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new("数据已修改但尚未保存")
                                    .small()
                                    .strong()
                                    .color(style::error()),
                            );
                        });
                    if draft_is_inline {
                        if style::icon_button_tinted(
                            ui,
                            style::ToolbarIcon::Save,
                            editable && !waiting,
                            None,
                            true,
                            None,
                            "保存当前行修改（向上/向下移动焦点或按 Enter 也会自动保存）",
                        )
                        .clicked()
                        {
                            inline_commit = true;
                        }
                        if style::icon_button(
                            ui,
                            style::ToolbarIcon::Cancel,
                            !waiting,
                            "取消修改（Esc）",
                        )
                        .clicked()
                        {
                            inline_cancel = true;
                        }
                    }
                } else if draft_is_inline {
                    ui.separator();
                    ui.label(
                        RichText::new("上下移动焦点自动保存 · Esc 取消")
                            .small()
                            .color(style::muted()),
                    );
                } else if let Some(notice) = &editor.notice {
                    ui.separator();
                    style::badge(ui, notice, style::accent(), style::accent_soft());
                }
            });
        });

        if let Some(target) = switched_target {
            editor.select_joined_target(target);
        }
        if editor.joined.is_some() {
            if !origin_matches {
                ui.colored_label(style::error(), "原查询页已变化，请回到原查询重新执行 F8。");
            } else if editor.draft.is_some() && !draft_is_inline {
                ui.colored_label(style::muted(), "保存或取消草稿后可切换写入目标。");
            }
        } else if let Some(object) = &editor.object
            && editor.query.is_none()
            && (!table_mode || !workspace.show_data_info || editor.page.is_none())
        {
            let name = format!("{}.{}", object.schema, object.name);
            ui.add(egui::Label::new(RichText::new(&name).monospace()).truncate())
                .on_hover_text(name);
        }
        if let Some(error) = &editor.error {
            ui.colored_label(style::error(), error);
        }
        if waiting {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(&workspace.status);
                if matches!(editor.pending, Some(Pending::Read(_)))
                    && workspace.reading_editable_result()
                {
                    let stopping = workspace.editable_read_stopping();
                    let label = if stopping {
                        "正在停止…"
                    } else {
                        "停止读取"
                    };
                    let shortcut = keyboard_allowed && ui.input_mut(super::consume_f8);
                    if ui
                        .add_enabled(!stopping, Button::new(label))
                        .on_hover_text("F8")
                        .clicked()
                        || shortcut
                    {
                        workspace.cancel_editable_read();
                    }
                }
            });
        }
        let Some(page) = editor
            .joined
            .as_ref()
            .map_or(editor.page.as_ref(), joined::Joined::page)
        else {
            return;
        };
        if let Some(target_row) = open_draft {
            editor.draft = Some(Draft::new(page, target_row, None));
        }
        if let Some(reason) = &page.edit_error {
            ui.colored_label(style::muted(), reason);
        }
        let in_form = editor.view == DataView::Form;
        let in_text = editor.view == DataView::Text;
        let full_join = if in_form {
            None
        } else {
            editor.joined.as_ref().and_then(|joined| {
                Some((
                    joined.result_grid_id(),
                    &joined.response.as_ref()?.result,
                    joined.binding()?,
                ))
            })
        };
        if let Some(draft) = &mut editor.filter_draft {
            let (change, dismiss) = show_filter_draft(ui, draft, page);
            if let Some(change) = change {
                match change {
                    Ok(predicate) => {
                        editor.server_selection.predicates.push(predicate);
                        editor.filter_draft = None;
                        navigate = Some(Navigate::First);
                    }
                    Err(reason) => editor.error = Some(reason),
                }
            } else if dismiss {
                editor.filter_draft = None;
                editor.error = None;
                editor.grid.selection.request_focus();
            }
        }
        let inline_editing = editor.draft.as_ref().is_some_and(Draft::is_inline);
        let editing = editor.draft.is_some()
            || editor.deleting.is_some()
            || reviewing_matches
            || editor.inspection.is_some()
            || editor.filter_draft.is_some();
        let panel_editing = editor.draft.as_ref().is_some_and(|draft| !draft.is_inline())
            || editor.deleting.is_some()
            || reviewing_matches
            || editor.inspection.is_some()
            || editor.filter_draft.is_some();
        let grid_height = if let Some(available) = dock_available(ui) {
            if panel_editing {
                (available * 0.34).clamp(54.0, 162.0)
            } else {
                available.max(56.0)
            }
        } else if panel_editing {
            (size.y * 0.27 - info_height).clamp(112.0, 240.0)
        } else {
            (size.y - 230.0 - info_height).clamp(150.0, 510.0)
        };
        let server_tools =
            (ready && !editing && editor.query.is_none()).then_some(&editor.server_selection);
        let controls = form::Controls {
            interactive: keyboard_allowed && ((!editing && !waiting) || moving_draft),
            editable,
            paged: editor.query.is_none(),
            previous_page: ready && has_previous_page,
            next_page: ready && page.has_more,
            server_filter: server_tools.is_some(),
            headers: copy_headers,
            height: grid_height,
        };
        let keyboard_action = if !inline_editing
            && draft_keys
            && moving_draft
            && !egui::Popup::is_any_open(ui.ctx())
            && editor
                .draft
                .as_ref()
                .is_some_and(|draft| !draft.preview_sql)
        {
            if let Some((_, _, target)) = full_join {
                for (forward, key) in [(false, egui::Key::ArrowUp), (true, egui::Key::ArrowDown)] {
                    if let Some(next) = editor
                        .joined_grid
                        .adjacent_source_point(forward)
                        .and_then(|result| joined_move(target, result, point.column))
                        && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, key))
                    {
                        row_move = Some(next);
                        break;
                    }
                }
                None
            } else {
                let previous = form::adjacent(page, point, &controls, false);
                let next = form::adjacent(page, point, &controls, true);
                if previous.is_some()
                    && ui.input_mut(|input| {
                        input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)
                    })
                {
                    previous
                } else if next.is_some()
                    && ui.input_mut(|input| {
                        input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)
                    })
                {
                    next
                } else {
                    None
                }
            }
        } else {
            None
        };
        let action = if in_text {
            let focus = std::mem::take(&mut editor.focus_text);
            if let Some((grid_id, result, _)) = full_join {
                ui.push_id(grid_id, |ui| {
                    ui.set_height(grid_height);
                    editor.joined_grid.show_text(
                        ui,
                        result,
                        &mut editor.text_rows,
                        copy_headers,
                        keyboard_allowed && !waiting,
                        focus,
                    );
                });
            } else {
                ui.push_id(editor.grid.id.with("text"), |ui| {
                    ui.set_height(grid_height);
                    editor.text_rows.show(
                        ui,
                        page.rows.len(),
                        page.columns.len(),
                        |column| page.column_label(column),
                        |row| page.rows[row].cells.as_slice(),
                        copy_headers,
                        keyboard_allowed && !waiting,
                        focus,
                    );
                });
            }
            None
        } else if in_form {
            if docked && keyboard_allowed {
                let _ = editor.grid.selection.focus(ui);
            }
            if editor.draft.is_none() || reviewing_matches {
                form::show(ui, page, point, controls)
            } else if draft_row.is_some() {
                form::navigation(ui, page, point, &controls)
            } else {
                None
            }
        } else if let Some((grid_id, result, target)) = full_join {
            let can_edit = |result| joined::source_point(target, result).is_some();
            let can_check = |row| editable && !editing && joined::source_row(target, row).is_some();
            let modified_row = editor
                .draft
                .as_ref()
                .filter(|draft| draft.is_inline())
                .and_then(|draft| draft.row);
            let modified_previews: Vec<(usize, String)> = editor
                .draft
                .as_ref()
                .filter(|draft| draft.is_inline())
                .map(|draft| {
                    (0..page.columns.len())
                        .filter_map(|col| {
                            draft
                                .modified_preview(page, col)
                                .map(|preview| (col, preview))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let modified_cell = |result_point: Point| -> Option<String> {
                let row_idx = modified_row?;
                let sp = joined::source_point(target, result_point)?;
                if sp.row != row_idx {
                    return None;
                }
                modified_previews
                    .iter()
                    .find(|(col, _)| *col == sp.column)
                    .map(|(_, text)| text.clone())
            };
            let mut inline_request_focus = false;
            let mut inline_changed = false;
            let mut inline_col_for_change = None;
            let mut inline_select_all = false;
            let mut inline_focus_at_end = false;
            let mut inline_was_null = false;
            let mut inline_point = None;
            if let Some(draft) = editor.draft.as_mut()
                && let Some(col) = draft.inline_column
                && let Some(row_idx) = draft.row
                && let Some(jp) = draft.inline_joined_point
            {
                inline_request_focus = draft.focus.take() == Some(col);
                inline_select_all = draft.inline_select_all;
                inline_focus_at_end = std::mem::take(&mut draft.focus_at_end);
                inline_was_null = draft.fields[col].mode == Mode::Keep
                    && matches!(page.rows[row_idx].cells[col], Cell::Null);
                inline_col_for_change = Some(col);
                inline_point = Some(jp);
            }
            let action = ui
                .push_id(grid_id, |ui| {
                    ui.set_height(grid_height);
                    ui.add_enabled_ui(!waiting && (!panel_editing || moving_draft), |ui| {
                        let inline = inline_point.and_then(|jp| {
                            let col = inline_col_for_change?;
                            let draft = editor.draft.as_mut()?;
                            Some(InlineJoinedCell {
                                point: jp,
                                text: &mut draft.fields[col].text,
                                was_null: inline_was_null,
                                request_focus: &mut inline_request_focus,
                                select_all: inline_select_all,
                                focus_at_end: inline_focus_at_end,
                                changed: &mut inline_changed,
                            })
                        });
                        editor.joined_grid.show_joined(
                            ui,
                            result,
                            copy_headers,
                            keyboard_allowed && !editing && !waiting && !read_only_modal,
                            settings,
                            grid_widths,
                            (editable && !panel_editing).then_some(&can_edit as _),
                            Some(&can_check),
                            moving_draft,
                            inline,
                            (!modified_previews.is_empty()).then_some(&modified_cell as _),
                        )
                    })
                    .inner
                })
                .inner;
            if inline_changed
                && let Some(col) = inline_col_for_change
                && let Some(draft) = editor.draft.as_mut()
            {
                draft.fields[col].mode = Mode::Text;
            }
            match action {
                Some(JoinedGridAction::Edit(result, text)) => {
                    joined_edit_action(target, result, text)
                }
                Some(JoinedGridAction::Move(result)) => {
                    if let Some(draft) = editor.draft.as_ref()
                        && draft.is_inline()
                        && let Some(sp) = joined::source_point(target, result)
                        && draft.row == Some(sp.row)
                        && !page.columns[sp.column].is_binary()
                        && !cell_has_newline(&page.rows[sp.row].cells[sp.column])
                    {
                        Some(GridAction::InlineSwitchCell {
                            column: sp.column,
                            joined_point: Some(result),
                        })
                    } else {
                        if row_move.is_none() {
                            row_move = joined_move(target, result, point.column);
                        }
                        if row_move.is_none() {
                            Some(GridAction::InlineCommit)
                        } else {
                            None
                        }
                    }
                }
                Some(JoinedGridAction::InlineCommit) => Some(GridAction::InlineCommit),
                Some(JoinedGridAction::InlineCancel) => Some(GridAction::InlineCancel),
                Some(JoinedGridAction::InlineStepColumn { forward }) => {
                    Some(GridAction::InlineStepColumn { forward })
                }
                None => None,
            }
        } else {
            let body_height = if docked {
                (grid_height - style::grid_row_height(ui, GRID_HEADER_HEIGHT) - GRID_ROW_GAP)
                    .max(24.0)
            } else {
                grid_height
            };
            editor.grid.show(
                ui,
                page,
                keyboard_allowed && !editing && !waiting,
                moving_draft,
                editable,
                single_query_read_only,
                has_previous_page,
                &mut editor.draft,
                body_height,
                server_tools,
                copy_headers,
                grid_widths,
            )
        };
        if inline_commit {
            if let Some(draft) = &editor.draft {
                if draft.has_changes(page) {
                    write = Some(draft.edit(page));
                } else {
                    editor.draft = None;
                    editor.error = None;
                    editor.grid.selection.request_focus();
                }
            }
        } else if inline_cancel {
            editor.draft = None;
            editor.queued_move = None;
            row_move = None;
            editor.error = None;
            editor.grid.selection.request_focus();
        }
        if let Some(action) = toolbar_action.or(keyboard_action).or(action) {
            match action {
                GridAction::BeginDraft { row, column, text } => {
                    draft_start = Some((row, column, text, None));
                }
                GridAction::BeginJoinedDraft {
                    row,
                    column,
                    text,
                    joined_point,
                } => {
                    draft_start = Some((row, column, text, Some(joined_point)));
                }
                GridAction::InlineCommit => {
                    if let Some(draft) = &editor.draft {
                        if draft.has_changes(page) {
                            write = Some(draft.edit(page));
                        } else {
                            editor.draft = None;
                            editor.error = None;
                            editor.grid.selection.request_focus();
                        }
                    }
                }
                GridAction::InlineCancel => {
                    editor.draft = None;
                    editor.queued_move = None;
                    row_move = None;
                    editor.error = None;
                    editor.grid.selection.request_focus();
                }
                GridAction::InlineSwitchCell {
                    column,
                    joined_point,
                } => {
                    if let Some(draft) = &mut editor.draft {
                        draft.switch_inline_column(page, column, joined_point);
                        if let Some(row) = draft.row {
                            editor.grid.focus_form_point(page, Point { row, column });
                        }
                    }
                }
                GridAction::InlineStepColumn { forward } => {
                    if let Some(draft) = &mut editor.draft {
                        if let Some((_, _, target)) = full_join {
                            if let Some(current_jp) = draft.inline_joined_point
                                && let Some(row_idx) = draft.row
                            {
                                let col_count = target.result_columns.len();
                                let mut candidate = current_jp.column;
                                for _ in 0..col_count {
                                    candidate = if forward {
                                        (candidate + 1) % col_count
                                    } else {
                                        (candidate + col_count - 1) % col_count
                                    };
                                    let next_jp = Point {
                                        row: current_jp.row,
                                        column: candidate,
                                    };
                                    if let Some(sp) = joined::source_point(target, next_jp)
                                        && sp.row == row_idx
                                        && !page.columns[sp.column].is_binary()
                                        && !cell_has_newline(&page.rows[row_idx].cells[sp.column])
                                    {
                                        draft.switch_inline_column(page, sp.column, Some(next_jp));
                                        break;
                                    }
                                }
                            }
                        } else if draft.step_inline_column(page, forward)
                            && let (Some(row), Some(column)) = (draft.row, draft.inline_column)
                        {
                            editor.grid.focus_form_point(page, Point { row, column });
                        }
                    }
                }
                GridAction::OpenJoined(Point { row, column }, joined_point) => {
                    editor.grid.focus_form_point(page, Point { row, column });
                    let field = &page.columns[column];
                    let cell = &page.rows[row].cells[column];
                    if editable && field.writable && !field.is_binary() && !cell_has_newline(cell) {
                        editor.draft =
                            Some(Draft::new_inline(page, row, column, None, Some(joined_point)));
                        editor.error = None;
                        editor.notice = None;
                    } else if inspectable(field) {
                        let dialog = if editable && field.writable {
                            Inspector::edit(
                                page.column_label(column).to_owned(),
                                cell,
                                field.is_binary(),
                                field.nullable,
                            )
                        } else {
                            Inspector::read(page.column_label(column).to_owned(), cell)
                        };
                        editor.inspection = Some((Some(row), column, dialog));
                    } else if editable {
                        editor.draft = Some(Draft::new(page, Some(row), Some(column)));
                    }
                }
                GridAction::Open(Point { row, column }) => {
                    if in_form || full_join.is_some() {
                        editor.grid.focus_form_point(page, Point { row, column });
                    }
                    let field = &page.columns[column];
                    let cell = &page.rows[row].cells[column];
                    if !in_form
                        && editable
                        && field.writable
                        && !field.is_binary()
                        && !cell_has_newline(cell)
                    {
                        editor.draft = Some(Draft::new_inline(page, row, column, None, None));
                        editor.error = None;
                        editor.notice = None;
                    } else if inspectable(field) {
                        let dialog = if editable && field.writable {
                            Inspector::edit(
                                page.column_label(column).to_owned(),
                                cell,
                                field.is_binary(),
                                field.nullable,
                            )
                        } else {
                            Inspector::read(page.column_label(column).to_owned(), cell)
                        };
                        editor.inspection = Some((Some(row), column, dialog));
                    } else if editable && field.writable {
                        editor.draft = Some(Draft::new(page, Some(row), Some(column)));
                    }
                }
                GridAction::Focus(point) => row_move = Some(RowMove::Row(point)),
                GridAction::Duplicate(row) => match Draft::duplicate(page, row) {
                    Ok(draft) => {
                        editor.draft = Some(draft);
                        editor.error = None;
                        editor.notice = None;
                    }
                    Err(reason) => editor.error = Some(reason),
                },
                GridAction::Delete(row) => editor.deleting = Some(Deletion::Row(row)),
                GridAction::Export => export_kind = Some(ExportKind::Csv),
                GridAction::SetValue(point, preset) => {
                    editor.grid.focus_form_point(page, point);
                    let mut draft = Draft::new(page, Some(point.row), Some(point.column));
                    draft.fields[point.column].apply_preset(preset, &page.columns[point.column]);
                    editor.draft = Some(draft);
                }
                GridAction::Navigate(action) => row_move = Some(RowMove::Page(action)),
                GridAction::Filter(predicate) => {
                    editor.server_selection.predicates.push(predicate);
                    navigate = Some(Navigate::First);
                }
                GridAction::Sort(sort, append) => {
                    if append {
                        editor
                            .server_selection
                            .order
                            .retain(|previous| previous.column != sort.column);
                    } else {
                        editor.server_selection.order.clear();
                    }
                    editor.server_selection.order.push(sort);
                    navigate = Some(Navigate::First);
                }
                GridAction::NewFilter(column) => {
                    editor.filter_draft = Some(FilterDraft {
                        column,
                        operator: TablePredicateOp::Equal,
                        value: String::new(),
                        is_null: false,
                    });
                }
            }
        }
        if let Some(prompt) = &editor.duplicate_prompt {
            (duplicate_choice, cancel_duplicates) =
                confirm_duplicate_rows(ui, prompt, connected && !waiting);
        } else if let Some(draft) = &mut editor.draft
            && !draft.is_inline()
        {
            let height = if let Some(available) = dock_available(ui) {
                (available - ui.spacing().interact_size.y - 16.0).max(34.0)
            } else if in_form {
                (size.y - 230.0).clamp(150.0, 510.0)
            } else {
                (size.y * 0.25).max(110.0)
            };
            let (edit, cancel, inspect) = draft.show(ui, page, editable, waiting, height);
            write = edit;
            if cancel {
                editor.draft = None;
                editor.queued_move = None;
                row_move = None;
                editor.error = None;
                editor.grid.selection.request_focus();
            } else if let Some(column) = inspect {
                let field = &draft.columns(page)[column];
                let original = draft.row.map(|index| &page.rows[index].cells[column]);
                match draft.fields[column].current_value(field, original) {
                    Ok(cell) => {
                        let dialog = Inspector::edit(
                            if draft.source_columns.is_some() {
                                field.name.clone()
                            } else {
                                page.column_label(column).to_owned()
                            },
                            &cell,
                            field.is_binary(),
                            field.nullable,
                        );
                        editor.inspection = Some((draft.row, column, dialog));
                    }
                    Err(reason) => editor.error = Some(reason),
                }
            }
        } else if let Some(deletion) = &editor.deleting {
            let (confirm, cancel) = confirm_delete(ui, page, deletion, editable, waiting);
            if confirm {
                let originals = deletion
                    .rows()
                    .iter()
                    .map(|&row| page.rows.get(row).cloned())
                    .collect::<Option<Vec<_>>>();
                if let Some(mut originals) = originals {
                    if originals.len() > 1 {
                        batch_delete = Some(originals);
                    } else if let Some(original) = originals.pop() {
                        write = Some(TableEdit::Delete { original });
                    }
                }
            }
            if cancel {
                editor.deleting = None;
                editor.error = None;
                editor.grid.selection.request_focus();
            }
        }
        if show_data_info {
            if table_mode {
                let summary = format!("数据库 {} · 表 {}", page.object.schema, page.object.name);
                data_info_line(ui, &summary, None);
            } else {
                let timing = editor
                    .joined
                    .as_ref()
                    .and_then(|joined| joined.response.as_ref())
                    .and_then(|reply| reply.query_timing)
                    .or(page.query_timing);
                let summary = timing
                    .map(|timing| query_time_label(timing.elapsed, timing.execution))
                    .unwrap_or_default();
                data_info_line(
                    ui,
                    &summary,
                    editor.query.as_ref().map(|(sql, _)| sql.as_str()),
                );
            }
        }
    };
    let modal = match surface {
        Surface::Dialog(_) => Some(style::modal(&ctx, modal_id).show(&ctx, contents)),
        Surface::Docked(ui, _) => {
            ui.push_id(dock_id, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("editor-scroll")
                    .max_height(dock_height.unwrap_or(0.0))
                    .auto_shrink([false, false])
                    .scroll_source(egui::scroll_area::ScrollSource {
                        drag: false,
                        ..Default::default()
                    })
                    .show(ui, |ui| ui.add_enabled_ui(keyboard_allowed, contents));
            });
            None
        }
    };
    if editor.joined_grid.take_export_request() {
        export_kind = Some(ExportKind::Csv);
    }
    if can_export && let Some(kind) = export_kind {
        editor.request_export(kind);
    }
    editor.show_inspection(&ctx, settings);
    editor.show_export(&ctx, workspace);
    if cancel_duplicates {
        editor.duplicate_prompt = None;
        editor.deleting = None;
        editor.queued_move = None;
        editor.grid.selection.request_focus();
    } else if let Some(scope) = duplicate_choice
        && let Some(prompt) = editor.duplicate_prompt.take()
    {
        editor.write(
            workspace,
            prompt.edit,
            Some(DuplicateChoice {
                matches: prompt.matches,
                scope,
            }),
        );
    } else if close || (closing_allowed && modal.is_some_and(|modal| modal.should_close())) {
        return true;
    } else if let Some(action) = navigate {
        editor.navigate(workspace, action);
    } else if let Some(originals) = batch_delete {
        editor.delete_rows(workspace, originals);
    } else if let Some(edit) = write {
        editor.write(workspace, edit, None);
    } else if let Some((row, column, text, joined_point)) = draft_start {
        editor.begin_grid_draft_at(row, column, text, joined_point);
    } else if let Some(target) = row_move {
        editor.move_to(workspace, target);
    }
    false
}

struct DataGrid {
    id: egui::Id,
    selection: GridSelection,
    checked: CheckedRows,
    column_widths: GridColumns,
    scroll_to_top: bool,
    form_scroll: Option<Point>,
    blank_column: Option<usize>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct EntryCell {
    row: Option<usize>,
    column: usize,
}

impl EntryCell {
    fn draft(self, text: Option<String>) -> GridAction {
        GridAction::BeginDraft {
            row: self.row,
            column: self.column,
            text,
        }
    }

    fn accepts_text(self, action: Option<&GridAction>) -> bool {
        match action {
            None => true,
            Some(GridAction::Open(point)) => {
                self.row == Some(point.row) && self.column == point.column
            }
            Some(GridAction::BeginDraft {
                row,
                column,
                text: None,
            }) => self.row == *row && self.column == *column,
            _ => false,
        }
    }
}

#[derive(Default)]
struct CheckedRows {
    rows: BTreeSet<usize>,
    anchor: Option<usize>,
}

impl CheckedRows {
    fn set(&mut self, row: usize, checked: bool, extend: bool) {
        let anchor = if extend {
            *self.anchor.get_or_insert(row)
        } else {
            self.anchor = Some(row);
            row
        };
        let range = row.min(anchor)..=row.max(anchor);
        if checked {
            self.rows.extend(range);
        } else {
            self.rows.retain(|index| !range.contains(index));
        }
    }

    fn clear(&mut self) {
        self.rows.clear();
        self.anchor = None;
    }
}

impl DataGrid {
    fn new(id: egui::Id) -> Self {
        let mut selection = GridSelection::default();
        selection.request_focus();
        Self {
            id,
            selection,
            checked: CheckedRows::default(),
            column_widths: GridColumns::default(),
            scroll_to_top: true,
            form_scroll: None,
            blank_column: None,
        }
    }

    fn single_row(&self) -> Option<usize> {
        let rows = self.selection.current()?.rows();
        (rows.len() == 1).then_some(rows.start)
    }

    fn deletion(&self, single_row: Option<usize>) -> Option<Deletion> {
        if self.checked.rows.is_empty() {
            single_row.map(Deletion::Row)
        } else {
            Some(Deletion::Checked(
                self.checked.rows.iter().copied().collect(),
            ))
        }
    }

    fn form_point(&self) -> Point {
        self.selection
            .current()
            .map_or(Point::default(), |selection| selection.head)
    }

    fn focus_form_point(&mut self, page: &TablePage, point: Point) {
        if page.rows.is_empty() || page.columns.is_empty() {
            return;
        }
        self.blank_column = None;
        let point = Point {
            row: point.row.min(page.rows.len() - 1),
            column: point.column.min(page.columns.len() - 1),
        };
        self.selection.select(
            Hit {
                point,
                row_header: false,
            },
            false,
        );
        self.selection.request_focus();
        self.form_scroll = Some(point);
    }

    fn restore_cursor(&mut self, page: &TablePage, cursor: ReadCursor) -> bool {
        let point = match cursor {
            ReadCursor::At(point) => point,
            ReadCursor::Last(column) => Point {
                row: page.rows.len().saturating_sub(1),
                column,
            },
            ReadCursor::Record(original, column) => {
                let Some(row) = unique_record_position(page, &original) else {
                    return false;
                };
                Point { row, column }
            }
            ReadCursor::JoinedRecord(..) => return false,
        };
        self.focus_form_point(page, point);
        true
    }

    fn keep_selection_after_refresh(&mut self, old: &TablePage, page: &TablePage) {
        self.form_scroll = None;
        if !old
            .columns
            .iter()
            .map(|column| &column.name)
            .eq(page.columns.iter().map(|column| &column.name))
        {
            self.selection.clear();
            self.checked.clear();
            self.blank_column = None;
            return;
        }

        let same_row_at = |index: usize| {
            old.rows
                .get(index)
                .zip(page.rows.get(index))
                .is_some_and(|(old, row)| old.same_source_record(row))
        };
        self.checked.rows.retain(|&row| same_row_at(row));
        self.checked.anchor = self.checked.anchor.filter(|&row| same_row_at(row));
        if let Some(selection) = self.selection.current()
            && !selection.rows().all(same_row_at)
        {
            self.selection.clear();
            let source = old.rows.get(selection.head.row);
            if let Some(row) = source.and_then(|row| unique_record_position(page, row)) {
                let point = Point {
                    row,
                    ..selection.head
                };
                self.selection.select(
                    Hit {
                        point,
                        row_header: selection.whole_rows,
                    },
                    false,
                );
                self.form_scroll = Some(point);
            }
        }
    }

    fn copy_controls(&self, ui: &mut egui::Ui, page: &TablePage, headers: bool) {
        let selection = self.selection.current();
        style::icon_menu_button(
            ui,
            style::ToolbarIcon::Copy,
            !page.rows.is_empty(),
            "复制（选择范围与格式）",
            |ui| {
                copy_menu(ui, page, selection, selection.map(|s| s.head), headers);
            },
        );
        if let Some(selection) = selection {
            ui.label(
                RichText::new(format!(
                    "{}×{}",
                    selection.rows().len(),
                    selection.columns(page.columns.len()).len()
                ))
                .small()
                .monospace()
                .color(style::muted()),
            )
            .on_hover_text(format!(
                "已选中 {} 行 × {} 列",
                selection.rows().len(),
                selection.columns(page.columns.len()).len()
            ));
        }
    }

    fn entry_cell(&self, page: &TablePage) -> Option<EntryCell> {
        let entry = if let Some(column) = self.blank_column {
            EntryCell { row: None, column }
        } else {
            let point = self.selection.current()?.single_cell()?;
            page.rows.get(point.row)?;
            EntryCell {
                row: Some(point.row),
                column: point.column,
            }
        };
        page.columns
            .get(entry.column)
            .is_some_and(|field| field.writable)
            .then_some(entry)
    }

    fn blank_keys(&mut self, ui: &mut egui::Ui, page: &TablePage) -> (Option<GridAction>, bool) {
        let mut scroll = false;
        if self.blank_column.is_none()
            && let Some(point) = self.selection.current().and_then(Selection::single_cell)
            && point.row + 1 == page.rows.len()
            && let Some(column) = page
                .columns
                .get(point.column)
                .filter(|field| field.writable)
                .map(|_| point.column)
                .or_else(|| first_insert_column(page))
            && consume_grid_arrow(ui, egui::Key::ArrowDown)
        {
            self.selection.clear();
            self.blank_column = Some(column);
            scroll = true;
        }
        let Some(mut column) = self.blank_column else {
            return (None, scroll);
        };
        if let Some(last) = page.rows.len().checked_sub(1)
            && consume_grid_arrow(ui, egui::Key::ArrowUp)
        {
            self.focus_form_point(page, Point { row: last, column });
            return (None, false);
        }
        let adjacent = if consume_grid_arrow(ui, egui::Key::ArrowLeft) {
            (0..column)
                .rev()
                .find(|&index| page.columns[index].writable)
        } else if consume_grid_arrow(ui, egui::Key::ArrowRight) {
            (column + 1..page.columns.len()).find(|&index| page.columns[index].writable)
        } else {
            None
        };
        if let Some(index) = adjacent {
            self.blank_column = Some(index);
            column = index;
            scroll = true;
        }
        let open = ui.input_mut(|input| {
            input.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                || input.consume_key(egui::Modifiers::NONE, egui::Key::F2)
        });
        (
            open.then(|| EntryCell { row: None, column }.draft(None)),
            scroll,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn show(
        &mut self,
        ui: &mut egui::Ui,
        page: &TablePage,
        interactive: bool,
        moving_draft: bool,
        editable: bool,
        read_only_tint: bool,
        has_previous_page: bool,
        draft: &mut Option<Draft>,
        height: f32,
        server_tools: Option<&TableSelection>,
        headers: bool,
        grid_widths: &mut GridWidths,
    ) -> Option<GridAction> {
        let row_height = style::grid_row_height(ui, GRID_ROW_HEIGHT);
        let header_height = style::grid_row_height(ui, GRID_HEADER_HEIGHT);
        let mut action = None;
        let show_blank = editable && page.can_insert && first_insert_column(page).is_some();
        if !show_blank {
            self.blank_column = None;
        } else if interactive && page.rows.is_empty() && self.blank_column.is_none() {
            self.blank_column = first_insert_column(page);
            self.selection.request_focus();
        }
        let focus = interactive.then(|| self.selection.focus(ui));
        let focused = focus.as_ref().is_some_and(egui::Response::has_focus)
            && !egui::Popup::is_any_open(ui.ctx());
        let mut scroll_blank = false;
        let mut space_entry = None;
        if focused && show_blank {
            (action, scroll_blank) = self.blank_keys(ui, page);
            if scroll_blank && let Some(focus) = &focus {
                focus.request_focus();
                ui.ctx().request_repaint();
            }
            if let Some(column) = self.blank_column
                && long_value_field(&page.columns[column])
                && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Space))
            {
                space_entry = Some(EntryCell { row: None, column });
            }
        }
        if focused && self.blank_column.is_none() {
            let page_rows = (height / (row_height + GRID_ROW_GAP)).max(1.0) as usize;
            for shortcut in
                self.selection
                    .shortcuts(ui, page.rows.len(), page.columns.len(), page_rows, true)
            {
                match shortcut {
                    Shortcut::Copy(selection) => {
                        if let Some(point) = selection.single_cell() {
                            ui.ctx()
                                .copy_text(cell_text(&page.rows[point.row].cells[point.column]));
                        } else {
                            CopyTarget::Area(selection).copy(
                                ui.ctx(),
                                page,
                                headers,
                                ClipboardFormat::Tsv,
                            );
                        }
                    }
                    Shortcut::Open(point, key) => {
                        if self.selection.current().is_none() {
                            self.selection.select(
                                Hit {
                                    point,
                                    row_header: false,
                                },
                                false,
                            );
                        }
                        if key == egui::Key::Space
                            && editable
                            && long_value_field(&page.columns[point.column])
                        {
                            space_entry = Some(EntryCell {
                                row: Some(point.row),
                                column: point.column,
                            });
                        }
                        action = Some(GridAction::Open(point));
                    }
                }
            }
        }

        let selection = self.selection.current();
        let mut pointer = PointerInteraction::default();
        let mut select_all = false;
        let scroll_to = self.selection.scroll_target().or(self.form_scroll.take());
        let mut did_scroll = false;
        let entry = if editable && interactive {
            self.entry_cell(page)
        } else {
            None
        };
        let mut visible_entry = None;
        let mut clicked_entry = None;
        let show_checks = page.edit_error.is_none();
        let draft_click = |response: &egui::Response, point| {
            (moving_draft
                && (response.clicked_by(egui::PointerButton::Primary)
                    || response.secondary_clicked())
                && response.ctx.input(|input| !input.modifiers.shift))
            .then_some(GridAction::Focus(point))
        };
        self.column_widths.measure_data(
            ui,
            grid_widths,
            page.columns.iter().map(|field| !long_value_field(field)),
            page.rows
                .iter()
                .flat_map(|row| row.cells.iter().enumerate()),
        );
        let column_widths = self.column_widths.prepare(
            &page.column_labels,
            page.columns.iter().map(|column| {
                Some((
                    page.object.schema.as_str(),
                    page.object.name.as_str(),
                    column.name.as_str(),
                ))
            }),
            (60.0, 46.0),
            grid_widths,
        );
        let numeric_columns: Vec<_> = page.columns.iter().map(numeric_column).collect();
        egui::ScrollArea::horizontal()
            .id_salt(self.id.with("horizontal-scroll"))
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
                let resize_id = ui.id().with(self.id).with("column-resize");
                let mut table = TableBuilder::new(ui)
                    .id_salt(self.id)
                    .striped(true)
                    .drag_to_scroll(false)
                    .min_scrolled_height(height.min(200.0))
                    .max_scroll_height(height)
                    .sense(if interactive {
                        Sense::click_and_drag() - Sense::focusable_noninteractive()
                    } else {
                        Sense::click() - Sense::focusable_noninteractive()
                    })
                    .cell_layout(Layout::left_to_right(Align::Center));
                if show_checks {
                    table = table.column(Column::exact(ROW_CHECK_WIDTH));
                }
                for column in &column_widths {
                    table = table.column(column.column());
                }
                if std::mem::take(&mut self.scroll_to_top) {
                    table = table.vertical_scroll_offset(0.0);
                }
                if let Some(target) = scroll_to {
                    table = table.scroll_to_row(target.row, None);
                } else if scroll_blank {
                    table = table.scroll_to_row(page.rows.len(), None);
                }
                table
                    .header(header_height, |mut header| {
                        if show_checks {
                            header.col(|ui| {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new("勾选").small().color(style::muted()),
                                    )
                                    .selectable(false),
                                )
                                .on_hover_text("勾选要删除的行");
                            });
                        }
                        let (_, gutter) = header.col(|ui| {
                            grid_label(ui, RichText::new("行号").color(style::muted()), true);
                        });
                        if interactive && !page.rows.is_empty() {
                            select_all = gutter.on_hover_text("全选当前页").clicked();
                        }
                        for (index, column) in page.columns.iter().enumerate() {
                            let suffix = if page.primary_key.contains(&index) {
                                "（主键）"
                            } else if page.row_key.contains(&index) {
                                "（定位键）"
                            } else {
                                ""
                            };
                            let ordered = server_tools.and_then(|selection| {
                                selection
                                    .order
                                    .iter()
                                    .enumerate()
                                    .find(|(_, sort)| sort.column == column.name)
                            });
                            let label = page.column_label(index);
                            let (_, response) = header.col(|ui| {
                                let name = if let Some((rank, sort)) = ordered {
                                    let arrow = if sort.descending { "↓" } else { "↑" };
                                    RichText::new(format!("{label} {arrow}{}", rank + 1))
                                        .color(style::accent())
                                } else {
                                    RichText::new(label).color(style::ink())
                                };
                                grid_label(ui, name.strong(), numeric_columns[index]);
                            });
                            let detail = format!("{} · {}{suffix}", column.name, column.data_type);
                            let mut tooltip = if label == column.name {
                                detail
                            } else {
                                format!("{label}\n{detail}")
                            };
                            if server_tools.is_some() {
                                tooltip.push_str("\n右键可排序或筛选全表");
                            }
                            let response = response.on_hover_text(tooltip);
                            if server_tools.is_some() {
                                response.context_menu(|ui| {
                                    style::context_menu_ui(ui);
                                    for (label, descending, append) in [
                                        ("全表升序", false, false),
                                        ("全表降序", true, false),
                                        ("追加升序字段", false, true),
                                        ("追加降序字段", true, true),
                                    ] {
                                        if ui.button(label).clicked() {
                                            action = Some(GridAction::Sort(
                                                TableSort {
                                                    column: column.name.clone(),
                                                    descending,
                                                },
                                                append,
                                            ));
                                            ui.close();
                                        }
                                    }
                                    ui.separator();
                                    if ui.button("按输入值筛选全表…").clicked() {
                                        action = Some(GridAction::NewFilter(index));
                                        ui.close();
                                    }
                                });
                            }
                        }
                    })
                    .body(|body| {
                        body.rows(
                            row_height,
                            page.rows.len() + usize::from(show_blank),
                            |mut row| {
                                let index = row.index();
                                if show_blank && index == page.rows.len() {
                                    if show_checks {
                                        row.col(|_| {});
                                    }
                                    row.col(|ui| {
                                        grid_label(
                                            ui,
                                            RichText::new("＋ 新增").color(style::accent()),
                                            true,
                                        );
                                    });
                                    for (column, field) in page.columns.iter().enumerate() {
                                        let active = self.blank_column == Some(column);
                                        row.set_selected(active);
                                        let (_, response) = row.col(|ui| {
                                            if active {
                                                paint_cell_outline(ui);
                                                if scroll_blank {
                                                    ui.scroll_to_cursor(None);
                                                }
                                            }
                                        });
                                        let cell = EntryCell { row: None, column };
                                        if Some(cell) == entry {
                                            visible_entry = Some((cell, response.interact_rect));
                                        }
                                        if interactive
                                            && field.writable
                                            && response.clicked_by(egui::PointerButton::Primary)
                                        {
                                            clicked_entry = Some((cell, response.interact_rect));
                                            if let Some(focus) = &focus {
                                                focus.request_focus();
                                            }
                                            action = Some(cell.draft(None));
                                        }
                                    }
                                    return;
                                }
                                if show_checks {
                                    row.col(|ui| {
                                        let mut checked = self.checked.rows.contains(&index);
                                        if ui
                                            .add_enabled(
                                                interactive && editable,
                                                egui::Checkbox::new(&mut checked, ""),
                                            )
                                            .on_hover_text("勾选本行。按住 Shift 可勾选一段。")
                                            .changed()
                                        {
                                            let extend = ui.input(|input| input.modifiers.shift);
                                            self.checked.set(index, checked, extend);
                                            ui.ctx().request_repaint();
                                        }
                                    });
                                }
                                let row_selected = selection.is_some_and(|area| {
                                    area.whole_rows && area.rows().contains(&index)
                                });
                                row.set_selected(row_selected);
                                let (_, gutter) = row.col(|ui| {
                                    grid_label(
                                        ui,
                                        RichText::new((page.offset + index + 1).to_string())
                                            .monospace()
                                            .color(if row_selected {
                                                style::accent()
                                            } else {
                                                style::muted()
                                            }),
                                        true,
                                    );
                                });
                                if let Some(movement) = draft_click(
                                    &gutter,
                                    Point {
                                        row: index,
                                        column: self.form_point().column,
                                    },
                                ) {
                                    action = Some(movement);
                                }
                                pointer.visit(
                                    &gutter,
                                    Hit {
                                        point: Point {
                                            row: index,
                                            column: 0,
                                        },
                                        row_header: true,
                                    },
                                );
                                if interactive {
                                    gutter.context_menu(|ui| {
                                        style::context_menu_ui(ui);
                                        if ui.button("导出全部已读取行…").clicked() {
                                            action = Some(GridAction::Export);
                                            ui.close();
                                        }
                                        copy_menu(ui, page, selection, None, headers);
                                        if editable && ui.button("复制为新行").clicked() {
                                            action = Some(GridAction::Duplicate(index));
                                            ui.close();
                                        }
                                    });
                                }
                                for (column, cell) in page.rows[index].cells.iter().enumerate() {
                                    let point = Point { row: index, column };
                                    let is_inline_cell = draft.as_ref().is_some_and(|d| {
                                        d.row == Some(index) && d.inline_column == Some(column)
                                    });
                                    row.set_selected(
                                        !is_inline_cell
                                            && selection
                                                .is_some_and(|area| area.contains(point)),
                                    );
                                    let (_, response) = row.col(|ui| {
                                        if is_inline_cell
                                            && let Some(active_draft) = draft.as_mut()
                                        {
                                            let edit_id = ui.make_persistent_id((
                                                "data-inline-edit",
                                                self.id,
                                                index,
                                                column,
                                            ));
                                            let mut request_focus =
                                                active_draft.focus.take() == Some(column);
                                            let select_all = active_draft.inline_select_all;
                                            let focus_at_end =
                                                std::mem::take(&mut active_draft.focus_at_end);
                                            let was_null = active_draft.fields[column].mode
                                                == Mode::Keep
                                                && matches!(cell, Cell::Null);
                                            let (changed, key) = render_inline_cell_editor(
                                                ui,
                                                edit_id,
                                                &mut active_draft.fields[column].text,
                                                was_null,
                                                &mut request_focus,
                                                select_all,
                                                focus_at_end,
                                            );
                                            if changed {
                                                active_draft.fields[column].mode = Mode::Text;
                                            }
                                            if let Some(key) = key {
                                                action = Some(match key {
                                                    InlineKey::Up => {
                                                        if index > 0 {
                                                            GridAction::Focus(Point {
                                                                row: index - 1,
                                                                column,
                                                            })
                                                        } else if has_previous_page {
                                                            GridAction::Navigate(Navigate::Previous)
                                                        } else {
                                                            GridAction::InlineCommit
                                                        }
                                                    }
                                                    InlineKey::Down => {
                                                        if index + 1 < page.rows.len() {
                                                            GridAction::Focus(Point {
                                                                row: index + 1,
                                                                column,
                                                            })
                                                        } else if page.has_more {
                                                            GridAction::Navigate(Navigate::Next(
                                                                page.offset + page.rows.len(),
                                                            ))
                                                        } else {
                                                            GridAction::InlineCommit
                                                        }
                                                    }
                                                    InlineKey::Commit => GridAction::InlineCommit,
                                                    InlineKey::Cancel => GridAction::InlineCancel,
                                                    InlineKey::TabNext => {
                                                        GridAction::InlineStepColumn {
                                                            forward: true,
                                                        }
                                                    }
                                                    InlineKey::TabPrev => {
                                                        GridAction::InlineStepColumn {
                                                            forward: false,
                                                        }
                                                    }
                                                });
                                            }
                                        } else {
                                            let current = selection.is_some_and(|area| {
                                                !area.whole_rows && area.head == point
                                            });
                                            let modified_preview = draft
                                                .as_ref()
                                                .filter(|d| d.is_inline() && d.row == Some(index))
                                                .and_then(|d| d.modified_preview(page, column));
                                            if let Some(preview) = &modified_preview {
                                                let preview_cell = Cell::Text(preview.clone());
                                                grid_cell_styled(
                                                    ui,
                                                    &preview_cell,
                                                    numeric_columns[column],
                                                    current,
                                                    false,
                                                    true,
                                                );
                                            } else {
                                                let cell_read_only = read_only_tint
                                                    || !page.columns[column].writable;
                                                grid_cell_styled(
                                                    ui,
                                                    cell,
                                                    numeric_columns[column],
                                                    current,
                                                    cell_read_only,
                                                    false,
                                                );
                                            }
                                        }
                                        if scroll_to == Some(point) {
                                            ui.scroll_to_cursor(None);
                                            did_scroll = true;
                                        }
                                    });
                                    let data_entry = EntryCell {
                                        row: Some(index),
                                        column,
                                    };
                                    if Some(data_entry) == entry {
                                        visible_entry = Some((data_entry, response.interact_rect));
                                    }
                                    if interactive
                                        && editable
                                        && page.columns[column].writable
                                        && response.clicked_by(egui::PointerButton::Primary)
                                    {
                                        clicked_entry = Some((data_entry, response.interact_rect));
                                    }
                                    pointer.visit(
                                        &response,
                                        Hit {
                                            point,
                                            row_header: false,
                                        },
                                    );
                                    if !is_inline_cell {
                                        if let Some(active_draft) = draft.as_ref()
                                            && active_draft.is_inline()
                                            && active_draft.row == Some(index)
                                            && page.columns[column].writable
                                            && !page.columns[column].is_binary()
                                            && !cell_has_newline(cell)
                                            && response.clicked_by(egui::PointerButton::Primary)
                                        {
                                            action = Some(GridAction::InlineSwitchCell {
                                                column,
                                                joined_point: None,
                                            });
                                        } else if let Some(movement) = draft_click(&response, point)
                                        {
                                            action = Some(movement);
                                        }
                                    }
                                    if interactive && response.double_clicked() {
                                        action = Some(GridAction::Open(point));
                                    }
                                    if interactive {
                                        response.context_menu(|ui| {
                                            style::context_menu_ui(ui);
                                            if ui.button("导出全部已读取行…").clicked() {
                                                action = Some(GridAction::Export);
                                                ui.close();
                                            }
                                            if inspectable(&page.columns[column])
                                                && ui.button("打开单元格内容…").clicked()
                                            {
                                                action = Some(GridAction::Open(point));
                                                ui.close();
                                            }
                                            if ui.button("复制单元格").clicked() {
                                                ui.ctx().copy_text(cell_text(cell));
                                                ui.close();
                                            }
                                            ui.menu_button("复制", |ui| {
                                                copy_menu(
                                                    ui,
                                                    page,
                                                    selection,
                                                    Some(point),
                                                    headers,
                                                );
                                            });
                                            if editable {
                                                if ui.button("复制为新行").clicked() {
                                                    action = Some(GridAction::Duplicate(index));
                                                    ui.close();
                                                }
                                                if let Some(preset) =
                                                    preset_menu(ui, &page.columns[column])
                                                {
                                                    action =
                                                        Some(GridAction::SetValue(point, preset));
                                                }
                                            }
                                            if server_tools.is_some()
                                                && let Some(value) =
                                                    page.rows[index].original().get(column)
                                                && let Some(predicate) = cell_filter_menu(
                                                    ui,
                                                    &page.columns[column],
                                                    cell,
                                                    value,
                                                )
                                            {
                                                action = Some(GridAction::Filter(predicate));
                                            }
                                        });
                                    }
                                }
                            },
                        );
                    });
                let data_top = if show_checks {
                    top + egui::vec2(ROW_CHECK_WIDTH + ui.spacing().item_spacing.x, 0.0)
                } else {
                    top
                };
                self.column_widths.resize_handles(
                    ui,
                    resize_id,
                    data_top,
                    &column_widths,
                    grid_widths,
                );
            });

        self.selection.finish_scroll(ui, did_scroll);
        if let Some(focus) = focus {
            self.selection.apply_pointer(ui, &focus, pointer);
            if select_all {
                self.selection
                    .select_all(page.rows.len(), page.columns.len());
                focus.request_focus();
                ui.ctx().request_repaint();
            }
            if self.selection.current().is_some() {
                self.blank_column = None;
            }
            let selected = self.entry_cell(page);
            let clicked = clicked_entry.filter(|(cell, _)| {
                Some(*cell) == selected
                    || (cell.row.is_none() && matches!(&action,
                        Some(GridAction::BeginDraft { row: None, column, text: None, .. }) if *column == cell.column
                    ))
            });
            let visible =
                clicked.or_else(|| visible_entry.filter(|(cell, _)| Some(*cell) == selected));
            if focus.has_focus()
                && !egui::Popup::is_any_open(ui.ctx())
                && let Some((cell, rect)) = visible
                && rect.is_positive()
                && rect.is_finite()
                && cell.accepts_text(action.as_ref())
            {
                text_entry::request_grid_ime(ui, rect);
                let changed_by_click = clicked.is_some() && Some(cell) != entry;
                if !changed_by_click || text_entry::text_follows_click(ui, rect) {
                    let text = text_entry::take_grid_text(ui)
                        .or_else(|| (space_entry == Some(cell)).then(|| " ".to_owned()));
                    if let Some(text) = text {
                        action = Some(cell.draft(Some(text)));
                    }
                }
            }
        }
        action
    }
}

fn consume_grid_arrow(ui: &mut egui::Ui, key: egui::Key) -> bool {
    let moved = ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, key));
    if moved {
        // Cancel egui's queued widget traversal just as GridSelection does for real rows.
        ui.memory_mut(|memory| memory.move_focus(egui::FocusDirection::None));
    }
    moved
}

fn first_insert_column(page: &TablePage) -> Option<usize> {
    page.columns
        .iter()
        .position(|field| field.writable && !field.auto_increment)
        .or_else(|| page.columns.iter().position(|field| field.writable))
}

#[derive(Clone, Copy)]
enum CopyTarget {
    Cell(Point),
    Row(usize),
    Area(Selection),
    Page,
}

impl CopyTarget {
    fn copy(self, ctx: &egui::Context, page: &TablePage, headers: bool, format: ClipboardFormat) {
        let (rows, columns, headers) = match self {
            Self::Cell(point) => (
                point.row..point.row + 1,
                point.column..point.column + 1,
                false,
            ),
            Self::Row(row) => (row..row + 1, 0..page.columns.len(), headers),
            Self::Area(selection) => (
                selection.rows(),
                selection.columns(page.columns.len()),
                headers,
            ),
            Self::Page => (0..page.rows.len(), 0..page.columns.len(), headers),
        };
        ctx.copy_text(clipboard_table(
            rows,
            columns,
            |column| page.column_label(column),
            |row, column| page.rows.get(row).and_then(|row| row.cells.get(column)),
            headers,
            format,
        ));
    }
}

fn copy_menu(
    ui: &mut egui::Ui,
    page: &TablePage,
    selection: Option<Selection>,
    point: Option<Point>,
    headers: bool,
) {
    ui.label(
        RichText::new("NULL 用 \\N，二进制用 \\x 加十六进制")
            .small()
            .color(style::muted()),
    );
    let targets = [
        ("复制单元格", point.map(CopyTarget::Cell)),
        ("复制当前行", point.map(|point| CopyTarget::Row(point.row))),
        ("复制选区", selection.map(CopyTarget::Area)),
        (
            "复制当前页",
            (!page.rows.is_empty()).then_some(CopyTarget::Page),
        ),
    ];
    for format in [ClipboardFormat::Tsv, ClipboardFormat::Csv] {
        ui.separator();
        for (label, target) in targets {
            if ui
                .add_enabled(
                    target.is_some(),
                    Button::new(format!("{label}（{}）", format.label())),
                )
                .clicked()
                && let Some(target) = target
            {
                target.copy(ui.ctx(), page, headers, format);
                ui.close();
            }
        }
    }
}

fn show_filter_draft(
    ui: &mut egui::Ui,
    draft: &mut FilterDraft,
    page: &TablePage,
) -> (Option<Result<TablePredicate, String>>, bool) {
    const OPERATORS: [(&str, TablePredicateOp); 7] = [
        ("等于", TablePredicateOp::Equal),
        ("不等于", TablePredicateOp::NotEqual),
        ("大于", TablePredicateOp::Greater),
        ("小于", TablePredicateOp::Less),
        ("开头是", TablePredicateOp::StartsWith),
        ("结尾是", TablePredicateOp::EndsWith),
        ("包含", TablePredicateOp::Contains),
    ];
    let mut change = None;
    let mut dismiss = false;
    let column = &page.columns[draft.column];
    ui.horizontal_wrapped(|ui| {
        ui.strong(&column.name);
        let label = OPERATORS
            .iter()
            .find(|(_, operator)| *operator == draft.operator)
            .map_or("等于", |(label, _)| *label);
        egui::ComboBox::from_id_salt("table-where-operator")
            .selected_text(label)
            .show_ui(ui, |ui| {
                for (label, value) in OPERATORS {
                    ui.selectable_value(&mut draft.operator, value, label);
                }
            });
        let null_allowed = matches!(
            draft.operator,
            TablePredicateOp::Equal | TablePredicateOp::NotEqual
        );
        if !null_allowed {
            draft.is_null = false;
        }
        ui.add_enabled(
            null_allowed,
            egui::Checkbox::new(&mut draft.is_null, "NULL"),
        );
        ui.add_enabled(
            !draft.is_null,
            TextEdit::singleline(&mut draft.value)
                .desired_width(165.0)
                .hint_text("比较值"),
        );
        if ui.button("添加筛选").clicked() {
            let value = if draft.is_null {
                Ok(mysql::Value::NULL)
            } else if matches!(
                draft.operator,
                TablePredicateOp::StartsWith
                    | TablePredicateOp::EndsWith
                    | TablePredicateOp::Contains
            ) {
                Ok(mysql::Value::Bytes(draft.value.as_bytes().to_vec()))
            } else {
                column.parse_filter_value(draft.value.clone())
            };
            change = Some(value.map(|value| TablePredicate {
                column: column.name.clone(),
                operator: draft.operator,
                value,
            }));
        }
        dismiss = ui.small_button("取消").clicked();
    });
    (change, dismiss)
}

fn confirm_delete(
    ui: &mut egui::Ui,
    page: &TablePage,
    deletion: &Deletion,
    enabled: bool,
    waiting: bool,
) -> (bool, bool) {
    let label = match deletion {
        Deletion::Row(row) => format!("删除第 {} 行？", page.offset + row + 1),
        Deletion::Checked(rows) => format!("删除 {} 条源记录？", rows.len()),
    };
    let valid =
        !deletion.rows().is_empty() && deletion.rows().iter().all(|&row| row < page.rows.len());
    style::pane(egui::Margin::symmetric(9, 6))
        .corner_radius(7.0)
        .stroke(egui::Stroke::new(1.0_f32, style::border()))
        .show(ui, |ui| {
            let (confirm, cancel) = ui
                .horizontal_wrapped(|ui| {
                    ui.label(RichText::new(label).strong().color(style::error()));
                    ui.separator();
                    let cancel = ui.add_enabled(!waiting, Button::new("取消")).clicked();
                    let confirm = ui
                        .add_enabled(enabled && valid, delete_button("确认删除"))
                        .clicked();
                    (confirm, cancel)
                })
                .inner;
            if let [row] = deletion.rows()
                && let Some(record) = page.rows.get(*row)
            {
                ui.horizontal_wrapped(|ui| {
                    for &index in &page.row_key {
                        let key = format!(
                            "{} = {}",
                            page.columns[index].name,
                            display_cell(&record.cells[index], true)
                        );
                        ui.add(
                            egui::Label::new(RichText::new(key).monospace().color(style::muted()))
                                .truncate(),
                        );
                    }
                });
            }
            (confirm, cancel)
        })
        .inner
}

fn delete_button(label: &str) -> Button<'_> {
    Button::new(RichText::new(label).strong().color(style::surface()))
        .fill(style::error())
        .stroke(egui::Stroke::NONE)
}

fn confirm_duplicate_rows(
    ui: &mut egui::Ui,
    prompt: &DuplicatePrompt,
    enabled: bool,
) -> (Option<DuplicateScope>, bool) {
    let deleting = matches!(prompt.edit, TableEdit::Delete { .. });
    let verb = if deleting { "删除" } else { "修改" };
    style::pane(egui::Margin::symmetric(9, 6))
        .corner_radius(7.0)
        .stroke(egui::Stroke::new(1.0_f32, style::border()))
        .show(ui, |ui| {
            ui.label(
                RichText::new(format!(
                    "有 {} 行与所选行的全部原值相同。请选择{verb}范围。",
                    prompt.matches
                ))
                .color(style::warning()),
            );
            ui.horizontal_wrapped(|ui| {
                let cancel = ui.button("取消").clicked()
                    || ui.input_mut(|input| {
                        input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
                    });
                let one = ui
                    .add_enabled(enabled, Button::new(format!("只{verb}一行")))
                    .clicked();
                let all_label = format!("{verb}全部 {} 行", prompt.matches);
                let all_button = if deleting {
                    delete_button(&all_label)
                } else {
                    Button::new(&all_label)
                };
                let all = ui.add_enabled(enabled, all_button).clicked();
                let choice = if all {
                    Some(DuplicateScope::All)
                } else {
                    one.then_some(DuplicateScope::One)
                };
                (choice, cancel)
            })
            .inner
        })
        .inner
}

fn cell_filter_menu(
    ui: &mut egui::Ui,
    column: &TableColumn,
    cell: &Cell,
    value: &mysql::Value,
) -> Option<TablePredicate> {
    let mut predicate = None;
    ui.menu_button("按此值筛选全表", |ui| {
        let comparable = !matches!(cell, Cell::Null);
        let pattern = matches!(value, mysql::Value::Bytes(_));
        for (label, operator, enabled) in [
            ("等于", TablePredicateOp::Equal, true),
            ("不等于", TablePredicateOp::NotEqual, true),
            ("大于", TablePredicateOp::Greater, comparable),
            ("小于", TablePredicateOp::Less, comparable),
            ("开头是", TablePredicateOp::StartsWith, pattern),
            ("结尾是", TablePredicateOp::EndsWith, pattern),
            ("包含", TablePredicateOp::Contains, pattern),
        ] {
            if ui.add_enabled(enabled, Button::new(label)).clicked() {
                predicate = Some(TablePredicate {
                    column: column.name.clone(),
                    operator,
                    value: value.clone(),
                });
                ui.close();
            }
        }
    });
    predicate
}

#[derive(Clone, Copy)]
enum ValuePreset {
    Empty,
    Null,
    Default,
}

fn preset_menu(ui: &mut egui::Ui, column: &TableColumn) -> Option<ValuePreset> {
    if !column.writable {
        return None;
    }
    let mut selection = None;
    for (label, preset, enabled) in [
        (
            if column.is_binary() {
                "设为空字节串"
            } else {
                "设为空字符串"
            },
            ValuePreset::Empty,
            column.parse_filter_value(String::new()).is_ok(),
        ),
        ("设为 NULL", ValuePreset::Null, column.nullable),
        ("使用默认值", ValuePreset::Default, allows_default(column)),
    ] {
        if ui.add_enabled(enabled, Button::new(label)).clicked() {
            selection = Some(preset);
            ui.close();
        }
    }
    selection
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Keep,
    Text,
    Null,
    Default,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Self::Keep => "保留原值",
            Self::Text => "输入值",
            Self::Null => "设为 NULL",
            Self::Default => "使用默认值",
        }
    }
}

struct Field {
    mode: Mode,
    text: String,
    binary: Option<Vec<u8>>,
}

impl Field {
    fn apply_preset(&mut self, preset: ValuePreset, column: &TableColumn) {
        match preset {
            ValuePreset::Empty if column.is_binary() => self.set_cell(Cell::Binary(Vec::new())),
            ValuePreset::Empty => self.set_cell(Cell::Text(String::new())),
            ValuePreset::Null => self.mode = Mode::Null,
            ValuePreset::Default => self.mode = Mode::Default,
        }
    }

    fn changed(&self, original: Option<&Cell>) -> bool {
        match self.mode {
            Mode::Keep => false,
            Mode::Text => match original {
                Some(Cell::Text(value)) => value != &self.text,
                Some(cell @ Cell::Binary(bytes)) => self.binary.as_ref().map_or_else(
                    || display_cell(cell, false) != self.text,
                    |binary| binary != bytes,
                ),
                _ => true,
            },
            Mode::Null => original != Some(&Cell::Null),
            Mode::Default => true,
        }
    }

    fn value(&self) -> Option<FieldValue> {
        match self.mode {
            Mode::Keep => None,
            Mode::Text => Some(match &self.binary {
                Some(binary) => FieldValue::Binary(binary.clone()),
                None => FieldValue::Text(self.text.clone()),
            }),
            Mode::Null => Some(FieldValue::Null),
            Mode::Default => Some(FieldValue::Default),
        }
    }

    fn current_value(&self, column: &TableColumn, original: Option<&Cell>) -> Result<Cell, String> {
        match self.mode {
            Mode::Keep => Ok(original.cloned().unwrap_or(Cell::Null)),
            Mode::Null | Mode::Default => Ok(Cell::Null),
            Mode::Text => match &self.binary {
                Some(binary) => Ok(Cell::Binary(binary.clone())),
                None if column.is_binary() => {
                    match column.parse_filter_value(self.text.clone())? {
                        mysql::Value::Bytes(bytes) => Ok(Cell::Binary(bytes)),
                        _ => Err("无法读取当前十六进制值。".into()),
                    }
                }
                None => Ok(Cell::Text(self.text.clone())),
            },
        }
    }

    fn set_cell(&mut self, value: Cell) {
        self.text.clear();
        self.binary = None;
        self.mode = match value {
            Cell::Null => Mode::Null,
            Cell::Text(text) => {
                self.text = text;
                Mode::Text
            }
            Cell::Binary(binary) => {
                self.binary = Some(binary);
                Mode::Text
            }
        };
    }
}

struct Draft {
    row: Option<usize>,
    source_columns: Option<Vec<TableColumn>>,
    fields: Vec<Field>,
    focus: Option<usize>,
    focus_at_end: bool,
    preview_sql: bool,
    inline_column: Option<usize>,
    inline_select_all: bool,
    inline_joined_point: Option<Point>,
}

impl Draft {
    fn new(page: &TablePage, row: Option<usize>, focus: Option<usize>) -> Self {
        let original = row.map(|index| &page.rows[index]);
        let fields = page
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                let cell = original.map(|row| &row.cells[index]);
                let text = match cell {
                    Some(Cell::Text(text)) => text.clone(),
                    Some(binary @ Cell::Binary(_)) => display_cell(binary, false),
                    _ => String::new(),
                };
                let mode = if !column.writable {
                    Mode::Keep
                } else if (row.is_some()
                    && focus == Some(index)
                    && !matches!(cell, Some(Cell::Null)))
                    || (row.is_none() && !allows_default(column))
                {
                    Mode::Text
                } else if row.is_some() {
                    Mode::Keep
                } else {
                    Mode::Default
                };
                Field {
                    mode,
                    text,
                    binary: None,
                }
            })
            .collect();
        Self {
            row,
            source_columns: None,
            fields,
            focus,
            focus_at_end: false,
            preview_sql: false,
            inline_column: None,
            inline_select_all: false,
            inline_joined_point: None,
        }
    }

    fn new_inline(
        page: &TablePage,
        row: usize,
        column: usize,
        typed: Option<String>,
        joined_point: Option<Point>,
    ) -> Self {
        let mut draft = Self::new(page, Some(row), Some(column));
        draft.inline_column = Some(column);
        draft.inline_joined_point = joined_point;
        if let Some(text) = typed {
            draft.fields[column].set_cell(Cell::Text(text));
            draft.focus_at_end = true;
            draft.inline_select_all = false;
        } else {
            draft.inline_select_all = true;
        }
        draft
    }

    fn is_inline(&self) -> bool {
        self.inline_column.is_some()
    }

    fn has_changes(&self, page: &TablePage) -> bool {
        let original = self.row.map(|index| &page.rows[index]);
        original.is_none_or(|row| {
            self.fields
                .iter()
                .zip(&row.cells)
                .any(|(field, cell)| field.changed(Some(cell)))
        })
    }

    fn modified_preview(&self, page: &TablePage, column: usize) -> Option<String> {
        let row_index = self.row?;
        let original = page.rows.get(row_index)?.cells.get(column)?;
        let field = self.fields.get(column)?;
        if !field.changed(Some(original)) {
            return None;
        }
        Some(match field.mode {
            Mode::Null => "NULL".to_owned(),
            Mode::Default => "DEFAULT".to_owned(),
            Mode::Text => field.text.clone(),
            Mode::Keep => return None,
        })
    }

    fn switch_inline_column(
        &mut self,
        page: &TablePage,
        column: usize,
        joined_point: Option<Point>,
    ) {
        if let Some(row_index) = self.row
            && let Some(field) = self.fields.get_mut(column)
            && field.mode == Mode::Keep
            && !matches!(page.rows[row_index].cells[column], Cell::Null)
        {
            field.mode = Mode::Text;
        }
        self.inline_column = Some(column);
        self.inline_joined_point = joined_point;
        self.focus = Some(column);
        self.inline_select_all = true;
        self.focus_at_end = false;
    }

    fn step_inline_column(&mut self, page: &TablePage, forward: bool) -> bool {
        let (Some(row_index), Some(current)) = (self.row, self.inline_column) else {
            return false;
        };
        let count = page.columns.len();
        if count <= 1 {
            return false;
        }
        let mut candidate = current;
        for _ in 0..count {
            candidate = if forward {
                (candidate + 1) % count
            } else {
                (candidate + count - 1) % count
            };
            let field = &page.columns[candidate];
            if field.writable
                && !field.is_binary()
                && !cell_has_newline(&page.rows[row_index].cells[candidate])
            {
                self.switch_inline_column(page, candidate, None);
                return true;
            }
        }
        false
    }

    fn duplicate(page: &TablePage, index: usize) -> Result<Self, String> {
        let original = &page.rows[index];
        let columns = original.source_columns();
        let fields = columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                let mut field = Field {
                    mode: Mode::Keep,
                    text: String::new(),
                    binary: None,
                };
                if column.writable {
                    if column.auto_increment {
                        field.mode = Mode::Default;
                    } else {
                        let value = original.source_cell(index).ok_or("缺少源表中的原值。")?;
                        if matches!(&value, Cell::Binary(_)) && !column.is_binary() {
                            return Err(format!(
                                "列「{}」的原值无法按当前字符集复制。",
                                column.name
                            ));
                        }
                        field.set_cell(value);
                    }
                }
                Ok(field)
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Self {
            row: None,
            source_columns: Some(columns.to_vec()),
            fields,
            focus: columns
                .iter()
                .position(|column| column.writable && !column.auto_increment),
            focus_at_end: false,
            preview_sql: false,
            inline_column: None,
            inline_select_all: false,
            inline_joined_point: None,
        })
    }

    fn columns<'a>(&'a self, page: &'a TablePage) -> &'a [TableColumn] {
        self.source_columns.as_deref().unwrap_or(&page.columns)
    }

    fn edit(&self, page: &TablePage) -> TableEdit {
        let original = self.row.map(|index| &page.rows[index]);
        let values = self
            .columns(page)
            .iter()
            .zip(&self.fields)
            .enumerate()
            .filter_map(|(index, (column, field))| {
                let cell = original.map(|row| &row.cells[index]);
                if column.writable && field.changed(cell) {
                    field.value().map(|value| (column.name.clone(), value))
                } else {
                    None
                }
            })
            .collect();
        match original {
            Some(original) => TableEdit::Update {
                original: original.clone(),
                values,
            },
            None => TableEdit::Insert { values },
        }
    }

    fn show(
        &mut self,
        ui: &mut egui::Ui,
        page: &TablePage,
        enabled: bool,
        waiting: bool,
        height: f32,
    ) -> (Option<TableEdit>, bool, Option<usize>) {
        let original = self.row.map(|index| &page.rows[index]);
        let copied = self.source_columns.is_some();
        let title = if let Some(row) = self.row {
            format!("草稿 · 编辑第 {} 行", page.offset + row + 1)
        } else if copied {
            "草稿 · 复制为新行".to_owned()
        } else {
            "草稿 · 新增一行".to_owned()
        };
        let has_changes = original.is_none_or(|row| {
            self.fields
                .iter()
                .zip(&row.cells)
                .any(|(field, cell)| field.changed(Some(cell)))
        });
        self.preview_sql &= has_changes;
        let (save, cancel) = style::pane(egui::Margin::symmetric(9, 5))
            .corner_radius(7.0)
            .stroke(egui::Stroke::new(1.0_f32, style::border()))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(title).strong().color(style::accent()));
                    ui.separator();
                    let cancel = ui.add_enabled(!waiting, Button::new("取消")).clicked();
                    let save = ui
                        .add_enabled(
                            enabled && has_changes,
                            style::primary(if original.is_some() {
                                "保存更改"
                            } else {
                                "新增记录"
                            }),
                        )
                        .clicked();
                    if original.is_some()
                        && has_changes
                        && ui
                            .selectable_label(self.preview_sql, "查看待保存 SQL")
                            .clicked()
                    {
                        self.preview_sql = !self.preview_sql;
                    }
                    (save, cancel)
                })
                .inner
            })
            .inner;

        let focus = self.focus.take();
        let focus_at_end = std::mem::take(&mut self.focus_at_end);
        let mut inspect = None;
        let columns = self.source_columns.as_deref().unwrap_or(&page.columns);
        let value_width = (ui.available_width() - 340.0).clamp(180.0, 640.0);
        let preview_height = if self.preview_sql {
            (height * 0.4).clamp(32.0, 120.0)
        } else {
            0.0
        };
        ui.add_enabled_ui(!waiting, |ui| {
            egui::ScrollArea::both()
                .id_salt("edit-fields")
                .max_height((height - preview_height).max(27.0))
                .show(ui, |ui| {
                    egui::Grid::new("row-values")
                        .striped(true)
                        .spacing([10.0, 6.0])
                        .show(ui, |ui| {
                            for (index, (field, column)) in
                                self.fields.iter_mut().zip(columns).enumerate()
                            {
                                let cell = original.map(|row| &row.cells[index]);
                                let title = if copied {
                                    &column.name
                                } else {
                                    page.column_label(index)
                                };
                                if field_row(
                                    ui,
                                    index,
                                    field,
                                    column,
                                    title,
                                    cell,
                                    focus == Some(index),
                                    focus_at_end && focus == Some(index),
                                    value_width,
                                ) {
                                    inspect = Some(index);
                                }
                            }
                        });
                });
        });

        if self.preview_sql {
            match preview_table_update(&self.edit(page)) {
                Ok(sql) => {
                    let mut sql = sql.as_str();
                    egui::ScrollArea::both()
                        .id_salt("pending-update-sql")
                        .max_height(preview_height)
                        .show(ui, |ui| {
                            ui.add(
                                egui::TextEdit::multiline(&mut sql)
                                    .background_color(style::panel())
                                    .font(egui::TextStyle::Monospace)
                                    .desired_rows(2)
                                    .desired_width(f32::INFINITY),
                            );
                        });
                }
                Err(error) => {
                    ui.colored_label(style::error(), error);
                }
            }
        }
        (save.then(|| self.edit(page)), cancel, inspect)
    }
}

fn cell_has_newline(cell: &Cell) -> bool {
    matches!(cell, Cell::Text(text) if text.contains(['\n', '\r']))
}

fn allows_default(column: &TableColumn) -> bool {
    column.nullable || column.default_value.is_some() || column.auto_increment
}

fn inspectable(column: &TableColumn) -> bool {
    column.is_binary()
        || matches!(
            column.data_type.as_str(),
            "tinytext" | "text" | "mediumtext" | "longtext" | "json"
        )
}

fn long_value_field(column: &TableColumn) -> bool {
    matches!(
        column.data_type.as_str(),
        "tinyblob"
            | "blob"
            | "mediumblob"
            | "longblob"
            | "tinytext"
            | "text"
            | "mediumtext"
            | "longtext"
    )
}

fn numeric_column(column: &TableColumn) -> bool {
    let name = column
        .data_type
        .split(['(', ' '])
        .next()
        .unwrap_or_default();
    matches!(
        name.to_ascii_lowercase().as_str(),
        "tinyint"
            | "smallint"
            | "mediumint"
            | "int"
            | "integer"
            | "bigint"
            | "decimal"
            | "numeric"
            | "float"
            | "double"
            | "real"
            | "year"
            | "bit"
    )
}

fn field_row(
    ui: &mut egui::Ui,
    index: usize,
    field: &mut Field,
    column: &TableColumn,
    title: &str,
    original: Option<&Cell>,
    focus: bool,
    focus_at_end: bool,
    value_width: f32,
) -> bool {
    let label = ui
        .add_sized(
            [132.0, 25.0],
            egui::Label::new(RichText::new(title).color(style::ink()))
                .truncate()
                .sense(Sense::click()),
        )
        .on_hover_text(format!("{} · {}", column.name, column.data_type));
    if focus {
        label.scroll_to_me(Some(Align::Center));
    }
    label.context_menu(|ui| {
        style::context_menu_ui(ui);
        if let Some(preset) = preset_menu(ui, column) {
            field.apply_preset(preset, column);
        }
    });
    if column.writable {
        egui::ComboBox::from_id_salt(("value-mode", index))
            .width(82.0)
            .selected_text(field.mode.label())
            .show_ui(ui, |ui| {
                if original.is_some() {
                    ui.selectable_value(&mut field.mode, Mode::Keep, Mode::Keep.label());
                }
                ui.selectable_value(&mut field.mode, Mode::Text, Mode::Text.label());
                if column.nullable {
                    ui.selectable_value(&mut field.mode, Mode::Null, Mode::Null.label());
                }
                if allows_default(column) {
                    ui.selectable_value(&mut field.mode, Mode::Default, Mode::Default.label());
                }
            });
    } else {
        ui.label(RichText::new("只读").color(style::muted()));
    }
    if let (Mode::Text, Some(binary)) = (field.mode, &field.binary) {
        ui.label(format!("二进制数据 · {} 字节", binary.len()));
    } else if field.mode == Mode::Text {
        let binary = column.is_binary();
        let hint = if binary {
            "十六进制，例如 0x1A2B"
        } else {
            &column.data_type
        };
        let mut output = ui
            .allocate_ui_with_layout(
                egui::vec2(value_width, 0.0),
                egui::Layout::top_down(Align::Min),
                |ui| {
                    ui.set_min_width(value_width);
                    TextEdit::multiline(&mut field.text)
                        .desired_rows(1)
                        .font(egui::TextStyle::Monospace)
                        .hint_text(hint)
                        .desired_width(value_width)
                        .show(ui)
                },
            )
            .inner;
        if focus {
            if focus_at_end {
                let end = field.text.len();
                output
                    .state
                    .cursor
                    .set_char_range(Some(cursors(&field.text, end..end)));
                output.state.store(ui.ctx(), output.response.id);
            }
            output.response.request_focus();
        }
    } else if field.mode == Mode::Default {
        let default = column
            .default_value
            .as_deref()
            .unwrap_or(if column.auto_increment {
                "自动生成"
            } else {
                "NULL"
            });
        ui.monospace(default);
    } else if field.mode == Mode::Null {
        ui.monospace("NULL");
    } else if let Some(cell) = original {
        ui.add(egui::Label::new(RichText::new(display_cell(cell, true)).monospace()).truncate());
    } else {
        ui.label("—");
    }
    let inspect = column.writable && inspectable(column) && ui.small_button("编辑内容…").clicked();
    ui.end_row();
    inspect
}
