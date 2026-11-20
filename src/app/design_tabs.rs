use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
};

use crate::database::DbObject;

use super::{
    navigation::Document, preferences::Preferences, query_builder::Builder,
    schema_designer::Designer, state::Workspace,
};

#[derive(Default)]
pub(super) struct Tabs {
    tabs: Vec<Tab>,
    selected: BTreeMap<u64, Document>,
    connected: HashSet<u64>,
    next_schema_id: u64,
    next_builder_id: u64,
}

struct Tab {
    id: u64,
    workspace: u64,
    layout: Layout,
}

enum Layout {
    Schema(Box<Designer>),
    Query(Box<Builder>),
}

impl Tab {
    fn document(&self) -> Document {
        match self.layout {
            Layout::Schema(_) => Document::Design(self.id),
            Layout::Query(_) => Document::Builder(self.id),
        }
    }

    fn closed(&self) -> bool {
        matches!(&self.layout, Layout::Schema(designer) if designer.closed)
    }

    fn path(&self) -> Option<&Path> {
        match &self.layout {
            Layout::Schema(designer) => designer.path.as_deref(),
            Layout::Query(builder) => builder.path(),
        }
    }

    fn dirty(&self) -> bool {
        match &self.layout {
            Layout::Schema(designer) => designer.dirty,
            Layout::Query(builder) => builder.dirty(),
        }
    }

    fn label(&self) -> String {
        let mut label = self.path().and_then(Path::file_name).map_or_else(
            || {
                let kind = match self.layout {
                    Layout::Schema(_) => "架构设计",
                    Layout::Query(_) => "查询设计",
                };
                format!("{kind} {}", self.id + 1)
            },
            |name| name.to_string_lossy().into_owned(),
        );
        if self.dirty() {
            label.push_str(" •");
        }
        label
    }
}

impl Tabs {
    pub(super) fn initialize_connection(
        &mut self,
        workspace: &Workspace,
        preferences: &Preferences,
    ) {
        let key = workspace.key;
        if !self.connected.insert(key) {
            return;
        }
        let selected = self.active(key);
        let schema = workspace
            .server
            .as_ref()
            .and_then(|server| server.database.clone());
        if preferences.query_design_tab_on_connect
            && !self
                .documents(key)
                .any(|document| matches!(document, Document::Builder(_)))
        {
            self.new_builder(key, Builder::new_blank(schema.clone()));
        }
        if preferences.schema_design_tab_on_connect
            && !self
                .documents(key)
                .any(|document| matches!(document, Document::Design(_)))
        {
            self.new_design(key, schema.unwrap_or_default(), None);
        }
        if let Some(document) = selected {
            self.select(key, document);
        } else {
            self.select_query(key);
        }
    }

    pub(super) fn new_design(
        &mut self,
        workspace: u64,
        schema: String,
        table: Option<DbObject>,
    ) -> u64 {
        let designer = Designer::new(workspace, schema, table);
        self.push(workspace, Layout::Schema(Box::new(designer)))
    }

    pub(super) fn new_builder(&mut self, workspace: u64, builder: Builder) {
        self.push(workspace, Layout::Query(Box::new(builder)));
    }

    fn push(&mut self, workspace: u64, layout: Layout) -> u64 {
        let next_id = match layout {
            Layout::Schema(_) => &mut self.next_schema_id,
            Layout::Query(_) => &mut self.next_builder_id,
        };
        let id = *next_id;
        *next_id += 1;
        let tab = Tab {
            id,
            workspace,
            layout,
        };
        self.selected.insert(workspace, tab.document());
        self.tabs.push(tab);
        id
    }

    pub(super) fn open_schema_file(&mut self, workspace: u64, path: PathBuf) -> Result<(), String> {
        let path = path.canonicalize().unwrap_or(path);
        if let Some(tab) = self.tabs.iter_mut().find(|tab| {
            tab.workspace == workspace
                && !tab.closed()
                && matches!(tab.layout, Layout::Schema(_))
                && same_path(tab.path(), &path)
        }) {
            self.selected.insert(workspace, tab.document());
            if let Layout::Schema(designer) = &mut tab.layout {
                designer.recent_files.push_back(path);
            }
        } else {
            let designer = Designer::from_file(workspace, path)?;
            self.push(workspace, Layout::Schema(Box::new(designer)));
        }
        Ok(())
    }

    pub(super) fn open_builder_file(
        &mut self,
        workspace: u64,
        path: PathBuf,
    ) -> Result<(), String> {
        let path = path.canonicalize().unwrap_or(path);
        if let Some(tab) = self.tabs.iter().find(|tab| {
            tab.workspace == workspace
                && matches!(tab.layout, Layout::Query(_))
                && same_path(tab.path(), &path)
        }) {
            self.selected.insert(workspace, tab.document());
        } else {
            self.new_builder(workspace, Builder::load_layout(path)?);
        }
        Ok(())
    }

    pub(super) fn open_table(&mut self, workspace: u64, schema: String, table: Option<DbObject>) {
        let accepts_table = |tab: &Tab| {
            tab.workspace == workspace
                && matches!(&tab.layout, Layout::Schema(designer) if designer.accepts_table(&schema))
        };
        let current = self.active(workspace);
        let id = self
            .tabs
            .iter()
            .find(|tab| Some(tab.document()) == current && accepts_table(tab))
            .or_else(|| self.tabs.iter().rev().find(|tab| accepts_table(tab)))
            .map(|tab| tab.id);
        if let Some(id) = id {
            self.select(workspace, Document::Design(id));
            if let Some(table) = &table
                && let Some(designer) = self.schema_mut(id)
            {
                designer.focus_table(table);
            }
        } else {
            self.new_design(workspace, schema, table);
        }
    }

    fn workspace_tabs(&self, workspace: u64) -> impl Iterator<Item = &Tab> {
        self.tabs
            .iter()
            .filter(move |tab| tab.workspace == workspace && !tab.closed())
    }

    pub(super) fn documents(&self, workspace: u64) -> impl Iterator<Item = Document> + '_ {
        self.workspace_tabs(workspace).map(Tab::document)
    }

    pub(super) fn list(&self, workspace: u64) -> impl Iterator<Item = (Document, String)> + '_ {
        self.workspace_tabs(workspace)
            .map(|tab| (tab.document(), tab.label()))
    }

    pub(super) fn active(&self, workspace: u64) -> Option<Document> {
        let document = *self.selected.get(&workspace)?;
        self.documents(workspace)
            .find(|candidate| *candidate == document)
    }

    pub(super) fn schema_mut(&mut self, id: u64) -> Option<&mut Designer> {
        self.tabs.iter_mut().find_map(|tab| match &mut tab.layout {
            Layout::Schema(designer) if tab.id == id && !designer.closed => Some(designer.as_mut()),
            _ => None,
        })
    }

    pub(super) fn builder_mut(&mut self, id: u64) -> Option<&mut Builder> {
        self.tabs.iter_mut().find_map(|tab| match &mut tab.layout {
            Layout::Query(builder) if tab.id == id => Some(builder.as_mut()),
            _ => None,
        })
    }

    pub(super) fn select(&mut self, workspace: u64, document: Document) {
        if self
            .documents(workspace)
            .any(|candidate| candidate == document)
        {
            self.selected.insert(workspace, document);
        }
    }

    pub(super) fn select_query(&mut self, workspace: u64) {
        self.selected.remove(&workspace);
    }

    pub(super) fn dirty(&self, workspace: Option<u64>) -> bool {
        self.tabs.iter().any(|tab| {
            !tab.closed()
                && workspace.is_none_or(|workspace| tab.workspace == workspace)
                && tab.dirty()
        })
    }

    pub(super) fn remove_builder(&mut self, id: u64) {
        let document = Document::Builder(id);
        self.tabs.retain(|tab| tab.document() != document);
        self.selected.retain(|_, selected| *selected != document);
    }

    pub(super) fn remove_workspace(&mut self, workspace: u64) {
        self.tabs.retain(|tab| tab.workspace != workspace);
        self.selected.remove(&workspace);
        self.connected.remove(&workspace);
    }

    pub(super) fn poll(&mut self, workspaces: &mut [Workspace]) -> Vec<(u64, PathBuf)> {
        let mut recent = Vec::new();
        for tab in &mut self.tabs {
            let workspace = workspaces
                .iter_mut()
                .find(|workspace| workspace.key == tab.workspace);
            match &mut tab.layout {
                Layout::Schema(designer) => {
                    if !designer.closed
                        && designer.has_pending_work()
                        && let Some(workspace) = workspace
                    {
                        designer.advance(workspace, false);
                    }
                    recent.extend(
                        designer
                            .recent_files
                            .drain(..)
                            .map(|path| (tab.workspace, path)),
                    );
                }
                Layout::Query(builder) => {
                    if let Some(workspace) = workspace {
                        builder.receive_metadata(workspace);
                    }
                }
            }
        }
        self.tabs.retain(|tab| {
            if tab.closed() {
                if self.selected.get(&tab.workspace) == Some(&tab.document()) {
                    self.selected.remove(&tab.workspace);
                }
                false
            } else {
                true
            }
        });
        recent
    }
}

fn same_path(current: Option<&Path>, path: &Path) -> bool {
    current.is_some_and(|current| {
        current == path || current.canonicalize().is_ok_and(|current| current == path)
    })
}

pub(super) mod appearance {
    use std::ops::Range;

    use eframe::egui::{
        self, Align2, Color32, CornerRadius, FontId, Painter, Pos2, Rect, Stroke, StrokeKind, Vec2,
        pos2, vec2,
    };

    use crate::app::style;

    pub(in crate::app) fn frame(margin: egui::Margin) -> egui::Frame {
        egui::Frame::new()
            .fill(style::SURFACE)
            .stroke(Stroke::new(1.0_f32, style::BORDER))
            .corner_radius(8)
            .inner_margin(margin)
    }

    pub(in crate::app) fn tray_width(width: f32) -> f32 {
        (width * 0.28).clamp(112.0, 192.0)
    }

    pub(in crate::app) fn tray<R>(
        ui: &mut egui::Ui,
        size: Vec2,
        body: impl FnOnce(&mut egui::Ui, f32) -> R,
    ) -> R {
        let frame = frame(egui::Margin::same(8));
        let content = size - frame.total_margin().sum();
        frame
            .show(ui, |ui| {
                ui.vertical(|ui| {
                    ui.set_width(content.x);
                    ui.set_min_height(content.y);
                    body(ui, content.y)
                })
                .inner
            })
            .inner
    }

    pub(in crate::app) fn canvas(painter: &Painter, viewport: Rect, origin: Pos2, zoom: f32) {
        painter.rect(
            viewport,
            8,
            style::PANEL,
            Stroke::new(1.0_f32, style::BORDER),
            StrokeKind::Inside,
        );
        let dots = viewport.shrink(4.0).intersect(painter.clip_rect());
        let step = (24.0 * zoom).max(12.0);
        let mut x = dots.left() + (origin.x - dots.left()).rem_euclid(step);
        while x < dots.right() {
            let mut y = dots.top() + (origin.y - dots.top()).rem_euclid(step);
            while y < dots.bottom() {
                painter.circle_filled(pos2(x, y), 0.75, style::BORDER);
                y += step;
            }
            x += step;
        }
    }

    pub(in crate::app) fn canvas_scroll(ui: &egui::Ui) -> (f32, Vec2) {
        ui.input_mut(|input| {
            // The canvas uses the raw wheel movement. Its containing page must not scroll too.
            input.smooth_scroll_delta = Vec2::ZERO;
            (input.zoom_delta(), input.raw_scroll_delta)
        })
    }

    pub(in crate::app) fn card(
        painter: &Painter,
        rect: Rect,
        title_height: f32,
        scale: f32,
        selected: bool,
    ) -> Rect {
        let radius = (8.0 * scale).clamp(2.0, 16.0) as u8;
        if scale >= 0.5 {
            painter.add(
                egui::epaint::Shadow {
                    offset: [0, 2],
                    blur: 8,
                    spread: 0,
                    color: Color32::from_black_alpha(if selected { 18 } else { 12 }),
                }
                .as_shape(rect, radius),
            );
        }
        painter.rect_filled(rect, radius, style::SURFACE);
        let title = Rect::from_min_size(rect.min, vec2(rect.width(), title_height));
        painter.rect_filled(
            title,
            CornerRadius {
                nw: radius,
                ne: radius,
                sw: 0,
                se: 0,
            },
            if selected {
                style::ACCENT_SOFT
            } else {
                style::PANEL
            },
        );
        painter.hline(
            title.x_range(),
            title.bottom(),
            Stroke::new(1.0_f32, style::BORDER),
        );
        let border = if selected {
            Stroke::new(2.0_f32, style::ACCENT)
        } else {
            Stroke::new(1.0_f32, style::BORDER)
        };
        painter.rect_stroke(rect, radius, border, StrokeKind::Inside);
        title
    }

    pub(in crate::app) fn text(
        painter: &Painter,
        rect: Rect,
        text: &str,
        font: FontId,
        color: Color32,
    ) {
        if !rect.is_positive() || !painter.clip_rect().intersects(rect) {
            return;
        }
        let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
        job.wrap = egui::text::TextWrapping::truncate_at_width(rect.width());
        let galley = painter.layout_job(job);
        let bounds = Align2::LEFT_CENTER.anchor_size(rect.left_center(), galley.size());
        painter
            .with_clip_rect(rect)
            .galley(bounds.min, galley, color);
    }

    pub(in crate::app) fn table_icon(
        painter: &Painter,
        center: Pos2,
        scale: f32,
        view: bool,
        color: Color32,
    ) {
        let icon = Rect::from_center_size(center, vec2(12.0, 10.0) * scale);
        let stroke = Stroke::new((1.1 * scale).max(0.7), color);
        painter.rect_stroke(icon, (2.0 * scale) as u8, stroke, StrokeKind::Inside);
        if view {
            painter.circle_filled(center, 1.65 * scale, color);
        } else {
            painter.hline(icon.x_range(), icon.top() + 3.5 * scale, stroke);
            painter.vline(center.x, center.y - 1.5 * scale..=icon.bottom(), stroke);
        }
    }

    #[derive(Clone, Copy)]
    pub(in crate::app) enum KeyMarker {
        Primary,
        Unique,
    }

    impl KeyMarker {
        pub(in crate::app) fn from_column(key: &str) -> Option<Self> {
            match key {
                "PRI" => Some(Self::Primary),
                "UNI" => Some(Self::Unique),
                _ => None,
            }
        }

        pub(in crate::app) fn paint(self, painter: &Painter, center: Pos2, scale: f32) {
            let stroke = Stroke::new((1.2 * scale).max(0.65), style::ACCENT);
            match self {
                Self::Primary => {
                    painter.circle_stroke(center + vec2(-2.0, 0.0) * scale, 2.5 * scale, stroke);
                    painter.line_segment(
                        [
                            center + vec2(0.5, 0.0) * scale,
                            center + vec2(5.5, 0.0) * scale,
                        ],
                        stroke,
                    );
                    painter.line_segment(
                        [
                            center + vec2(3.5, 0.0) * scale,
                            center + vec2(3.5, 2.2) * scale,
                        ],
                        stroke,
                    );
                }
                Self::Unique => {
                    painter.add(egui::Shape::closed_line(
                        [
                            vec2(0.0, -3.5),
                            vec2(3.5, 0.0),
                            vec2(0.0, 3.5),
                            vec2(-3.5, 0.0),
                        ]
                        .map(|point| center + point * scale)
                        .to_vec(),
                        stroke,
                    ));
                }
            }
        }
    }

    pub(in crate::app) fn resize_grip(painter: &Painter, bounds: Rect, zoom: f32) {
        let scale = zoom.clamp(0.55, 1.4);
        let bottom = bounds.max - Vec2::splat(3.0 * scale);
        for length in [4.0, 8.0] {
            painter.line_segment(
                [
                    bottom - vec2(length, 0.0) * scale,
                    bottom - vec2(0.0, length) * scale,
                ],
                Stroke::new(1.0_f32, style::MUTED),
            );
        }
    }

    pub(in crate::app) fn row_scroll(
        painter: &Painter,
        bounds: Rect,
        rows: Range<usize>,
        total: usize,
        zoom: f32,
    ) {
        if rows.len() >= total || rows.is_empty() || zoom < 0.55 || !bounds.is_positive() {
            return;
        }
        let x = bounds.right() - 4.0 * zoom;
        let y = |row: usize| bounds.top() + row as f32 / total as f32 * bounds.height();
        painter.vline(x, bounds.y_range(), Stroke::new(2.0 * zoom, style::BORDER));
        painter.vline(
            x,
            y(rows.start)..=y(rows.end),
            Stroke::new(2.0 * zoom, style::MUTED),
        );
    }

    pub(in crate::app) fn link(painter: &Painter, points: [Pos2; 4], stroke: Stroke) {
        painter.add(egui::Shape::line(
            points.to_vec(),
            Stroke::new(stroke.width + 2.0, style::SURFACE),
        ));
        painter.add(egui::Shape::line(points.to_vec(), stroke));
        painter.circle(points[0], 2.8, style::SURFACE, stroke);
    }

    pub(in crate::app) fn sql_preview(ui: &mut egui::Ui, mut sql: &str, rows: usize) {
        let mut highlight = |ui: &egui::Ui, text: &dyn egui::TextBuffer, width: f32| {
            ui.fonts_mut(|fonts| fonts.layout_job(style::highlight(text.as_str(), width)))
        };
        ui.add(
            egui::TextEdit::multiline(&mut sql)
                .font(egui::TextStyle::Monospace)
                .desired_rows(rows)
                .desired_width(f32::INFINITY)
                .frame(false)
                .margin(egui::Margin::same(4))
                .layouter(&mut highlight),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::ServerInfo;

    #[test]
    fn design_startup_preferences_apply_independently_and_preserve_open_drafts() {
        let defaults: Preferences =
            serde_json::from_str(r#"{"default_result_limit":800}"#).unwrap();
        assert!(defaults.query_design_tab_on_connect);
        assert!(defaults.schema_design_tab_on_connect);
        assert!(!defaults.query_design_quote_identifiers);

        let connected_workspace = |label: &str| {
            let mut workspace = Workspace::new(label.into(), None);
            workspace.server = Some(ServerInfo {
                version: "MariaDB".into(),
                connection_id: 1,
                database: Some("inventory".into()),
                http_tunnel: false,
            });
            workspace
        };
        let mut tabs = Tabs::default();
        let query = connected_workspace("查询设计");
        let query_options = Preferences {
            schema_design_tab_on_connect: false,
            query_design_quote_identifiers: true,
            ..defaults.clone()
        };
        let saved = serde_json::to_string(&query_options).unwrap();
        let reloaded = serde_json::from_str(&saved).unwrap();
        assert!(query_options == reloaded);
        tabs.initialize_connection(&query, &reloaded);
        assert!(matches!(
            tabs.documents(query.key).collect::<Vec<_>>().as_slice(),
            [Document::Builder(_)]
        ));
        assert!(tabs.active(query.key).is_none());

        let schema = connected_workspace("架构设计");
        tabs.initialize_connection(
            &schema,
            &Preferences {
                query_design_tab_on_connect: false,
                ..defaults.clone()
            },
        );
        assert!(matches!(
            tabs.documents(schema.key).collect::<Vec<_>>().as_slice(),
            [Document::Design(_)]
        ));
        assert!(tabs.active(schema.key).is_none());

        let manual = connected_workspace("保留草稿");
        let design = tabs.new_design(manual.key, "inventory".into(), None);
        tabs.schema_mut(design).unwrap().dirty = true;
        tabs.initialize_connection(
            &manual,
            &Preferences {
                query_design_tab_on_connect: false,
                schema_design_tab_on_connect: false,
                ..defaults.clone()
            },
        );
        assert!(matches!(tabs.active(manual.key), Some(Document::Design(id)) if id == design));
        assert!(tabs.dirty(Some(manual.key)));

        tabs.initialize_connection(&query, &defaults);
        assert!(matches!(
            tabs.documents(query.key).collect::<Vec<_>>().as_slice(),
            [Document::Builder(_)]
        ));
    }
}
