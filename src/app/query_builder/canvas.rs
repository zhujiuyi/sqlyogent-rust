use std::collections::{HashMap, HashSet};

use eframe::egui::{
    self, Align2, FontId, PointerButton, Pos2, Rect, RichText, Stroke, StrokeKind, Vec2, pos2, vec2,
};
use serde::{Deserialize, Serialize};

use crate::{
    app::{design_tabs::appearance, state::Workspace, style},
    database::{DbObject, ObjectKind},
};

use super::{
    Aggregate, Builder, Column, FieldInput, JoinKind, Query, Section, Source,
    metadata::{Metadata, column_details},
};

const HEADER: f32 = 31.0;
const ROW: f32 = 23.0;
const MIN_CARD: Vec2 = vec2(122.0, 70.0);
const MAX_CARD: Vec2 = vec2(600.0, 800.0);
const MIN_ZOOM: f32 = 0.25;
const MAX_ZOOM: f32 = 2.0;
const WORLD_LIMIT: f32 = 100_000.0;
pub(super) const DEFAULT_CARD: [f32; 2] = [204.0, 178.0];

#[derive(Clone, Copy, Deserialize, PartialEq, Serialize)]
pub(super) struct Camera {
    pan: [f32; 2],
    zoom: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            pan: [14.0, 14.0],
            zoom: 1.0,
        }
    }
}

impl Camera {
    pub(super) fn valid(self) -> bool {
        (MIN_ZOOM..=MAX_ZOOM).contains(&self.zoom) && valid_point(self.pan)
    }

    fn origin(self, viewport: Rect) -> Pos2 {
        viewport.min + Vec2::from(self.pan)
    }

    fn zoom_at(&mut self, delta: f32, focus: Vec2) {
        let zoom = (self.zoom * delta).clamp(MIN_ZOOM, MAX_ZOOM);
        let pan = focus - (focus - Vec2::from(self.pan)) * (zoom / self.zoom);
        self.pan = bounded(pan);
        self.zoom = zoom;
    }
}

pub(super) fn valid_card(position: [f32; 2], size: [f32; 2]) -> bool {
    valid_point(position)
        && (MIN_CARD.x..=MAX_CARD.x).contains(&size[0])
        && (MIN_CARD.y..=MAX_CARD.y).contains(&size[1])
}

fn valid_point(point: [f32; 2]) -> bool {
    point
        .into_iter()
        .all(|axis| (-WORLD_LIMIT..=WORLD_LIMIT).contains(&axis))
}

fn bounded(point: Vec2) -> [f32; 2] {
    point
        .clamp(Vec2::splat(-WORLD_LIMIT), Vec2::splat(WORLD_LIMIT))
        .into()
}

pub(super) fn free_position(sources: &[Source]) -> [f32; 2] {
    let sizes = sources
        .iter()
        .map(|source| Rect::from_min_size(Pos2::from(source.position), Vec2::from(source.size)))
        .collect::<Vec<_>>();
    for slot in 0..=sources.len() * 16 {
        let pos = pos2((slot % 3) as f32 * 250.0, (slot / 3) as f32 * 220.0);
        let candidate = Rect::from_min_size(pos, Vec2::from(DEFAULT_CARD)).expand(12.0);
        if sizes.iter().all(|rect| !candidate.intersects(*rect)) {
            return pos.into();
        }
    }
    [
        0.0,
        (sizes.iter().map(Rect::bottom).fold(0.0, f32::max) + 32.0).min(WORLD_LIMIT),
    ]
}

#[derive(Default)]
pub(super) struct Canvas {
    selected: Option<usize>,
    rows: HashMap<usize, usize>,
    drag: Option<Drag>,
    popup: Option<Hit>,
    alias: Option<usize>,
    search: String,
    fit: bool,
}

impl Canvas {
    pub(super) fn column_drag_active(&self) -> bool {
        matches!(self.drag, Some(Drag::Link(_)))
    }

    pub(super) fn remove_source(&mut self, id: usize) {
        self.rows.remove(&id);
        if self.alias == Some(id) {
            self.alias = None;
        }
        if self.selected == Some(id) {
            self.selected = None;
        }
        self.popup = None;
    }
}

enum Drag {
    Pan,
    Card(usize),
    Resize(usize),
    Link(Column),
}

enum Hit {
    Card(usize),
    Link(usize, usize),
}

enum Action {
    AddColumn(Column, Option<Pos2>),
    AddLink(Column, Column),
    ChangeKind(usize, JoinKind),
    RemoveCondition(usize, usize),
    StartAt(usize),
    RemoveSource(usize),
}

struct Card<'a> {
    source: &'a Source,
    rect: Rect,
    columns: &'a [String],
    first: usize,
    shown: usize,
}

impl Card<'_> {
    fn column_at(&self, mouse: Pos2, zoom: f32) -> Option<Column> {
        let y = (mouse.y - self.rect.top()) / zoom - HEADER;
        if y < 0.0 || self.resize_at(mouse) {
            return None;
        }
        let slot = (y / ROW) as usize;
        (slot < self.shown).then(|| Column {
            table: self.source.id,
            name: self.columns[self.first + slot].clone(),
        })
    }

    fn anchor_y(&self, column: &str, zoom: f32) -> f32 {
        let Some(index) = self.columns.iter().position(|name| name == column) else {
            return self.rect.top() + HEADER * zoom / 2.0;
        };
        let slot = index
            .saturating_sub(self.first)
            .min(self.shown.saturating_sub(1));
        self.rect.top() + (HEADER + (slot as f32 + 0.5) * ROW) * zoom
    }

    fn resize_at(&self, point: Pos2) -> bool {
        Rect::from_min_max(
            self.rect.right_bottom() - vec2(13.0, 13.0),
            self.rect.right_bottom(),
        )
        .contains(point)
    }
}

struct Link {
    join: usize,
    condition: usize,
    points: [Pos2; 4],
}

struct Diagram<'a> {
    cards: Vec<Card<'a>>,
    links: Vec<Link>,
}

impl<'a> Diagram<'a> {
    fn new(
        query: &'a Query,
        metadata: &'a Metadata,
        rows: &HashMap<usize, usize>,
        selected: Option<usize>,
        viewport: Rect,
    ) -> Self {
        let origin = query.camera.origin(viewport);
        let zoom = query.camera.zoom;
        let mut cards = query
            .sources
            .iter()
            .map(|source| {
                let columns = metadata.columns(&source.object);
                let shown = (((source.size[1] - HEADER - 6.0) / ROW) as usize).min(columns.len());
                Card {
                    source,
                    rect: Rect::from_min_size(
                        origin + Vec2::from(source.position) * zoom,
                        Vec2::from(source.size) * zoom,
                    ),
                    columns,
                    first: rows
                        .get(&source.id)
                        .copied()
                        .unwrap_or(0)
                        .min(columns.len() - shown),
                    shown,
                }
            })
            .collect::<Vec<_>>();
        cards.sort_by_key(|card| selected == Some(card.source.id));
        let ids = cards
            .iter()
            .enumerate()
            .map(|(index, card)| (card.source.id, index))
            .collect::<HashMap<_, _>>();
        let mut links = Vec::new();
        for (join_index, join) in query.joins.iter().enumerate() {
            let (Some(&left_card), Some(&right_card)) = (ids.get(&join.left), ids.get(&join.right))
            else {
                continue;
            };
            let (left_card, right_card) = (&cards[left_card], &cards[right_card]);
            let forward = left_card.rect.center().x <= right_card.rect.center().x;
            for (condition_index, condition) in join.conditions.iter().enumerate() {
                let (Some(left), Some(right)) = (&condition.left, &condition.right) else {
                    continue;
                };
                let x_from = if forward {
                    left_card.rect.right()
                } else {
                    left_card.rect.left()
                };
                let x_to = if forward {
                    right_card.rect.left()
                } else {
                    right_card.rect.right()
                };
                let y_from = left_card.anchor_y(&left.name, zoom);
                let y_to = right_card.anchor_y(&right.name, zoom);
                let bend = (x_from + x_to) / 2.0;
                links.push(Link {
                    join: join_index,
                    condition: condition_index,
                    points: [
                        pos2(x_from, y_from),
                        pos2(bend, y_from),
                        pos2(bend, y_to),
                        pos2(x_to, y_to),
                    ],
                });
            }
        }
        Self { cards, links }
    }

    fn card_at(&self, point: Pos2) -> Option<&Card<'_>> {
        self.cards
            .iter()
            .rev()
            .find(|card| card.rect.contains(point))
    }

    fn link_at(&self, point: Pos2) -> Option<&Link> {
        if self.card_at(point).is_some() {
            return None;
        }
        self.links
            .iter()
            .map(|link| {
                let distance = link
                    .points
                    .windows(2)
                    .map(|edge| {
                        if edge[0].y == edge[1].y {
                            let x = point
                                .x
                                .clamp(edge[0].x.min(edge[1].x), edge[0].x.max(edge[1].x));
                            point.distance(pos2(x, edge[0].y))
                        } else {
                            let y = point
                                .y
                                .clamp(edge[0].y.min(edge[1].y), edge[0].y.max(edge[1].y));
                            point.distance(pos2(edge[0].x, y))
                        }
                    })
                    .fold(f32::INFINITY, f32::min);
                (link, distance)
            })
            .filter(|(_, distance)| *distance <= 6.0)
            .min_by(|(_, first), (_, second)| first.total_cmp(second))
            .map(|(link, _)| link)
    }
}

pub(super) fn tools(ui: &mut egui::Ui, builder: &mut Builder) {
    if ui
        .button("适合画布")
        .on_hover_text("可拖动画布空白处平移，按住 Ctrl 滚动缩放。")
        .clicked()
    {
        builder.canvas.fit = true;
    }
    if ui.button("整理表位置").clicked() {
        for (index, source) in builder.query.sources.iter_mut().enumerate() {
            source.size = DEFAULT_CARD;
            source.position = [(index % 3) as f32 * 250.0, (index / 3) as f32 * 220.0];
        }
        builder.canvas.fit = true;
    }
    ui.label(
        RichText::new(format!("{:.0}%", builder.query.camera.zoom * 100.0))
            .monospace()
            .small()
            .color(style::muted()),
    )
    .on_hover_text("画布缩放比例");
    ui.menu_button("画布操作", |ui| {
        ui.set_max_width(292.0);
        ui.label("拖动画布空白处可平移，按住 Ctrl 滚动可缩放。");
        ui.label("拖动两列建立连接。右击连接线可更改类型或移除，双击表头可改别名。");
        ui.label("双击列可追加结果字段。把列拖到结果字段可插入到该字段前。");
        ui.label("钥匙标示主键，菱形标示唯一键，勾号标示结果字段。");
    });
}

pub(super) fn alias_editor(ui: &mut egui::Ui, builder: &mut Builder) {
    if let Some(source) = builder.canvas.alias.and_then(|id| {
        builder
            .query
            .sources
            .iter_mut()
            .find(|source| source.id == id)
    }) {
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("{} 的别名", source.name()));
            ui.add(egui::TextEdit::singleline(&mut source.alias).desired_width(115.0));
            if ui.button("完成").clicked() {
                builder.canvas.alias = None;
            }
        });
    }
}

pub(super) fn workspace(
    ui: &mut egui::Ui,
    builder: &mut Builder,
    connection: &Workspace,
    height: f32,
) -> Option<(Column, Pos2)> {
    ui.horizontal_top(|ui| {
        let tray_width = appearance::tray_width(ui.available_width());
        let added = appearance::tray(ui, vec2(tray_width, height), |ui, height| {
            catalog(ui, builder, connection, height)
        });
        if let Some(object) = added {
            builder.add_source(object, None);
            builder.canvas.fit = true;
        }
        let width = ui.available_width().max(1.0);
        draw(ui, builder, connection, vec2(width, height))
    })
    .inner
}

fn catalog(
    ui: &mut egui::Ui,
    builder: &mut Builder,
    connection: &Workspace,
    height: f32,
) -> Option<DbObject> {
    let top = ui.next_widget_position().y;
    ui.label(RichText::new("表与视图").strong());
    let width = ui.available_width();
    let schema = &mut builder.query.schema;
    egui::ComboBox::from_id_salt("builder-schema")
        .width(width)
        .truncate()
        .selected_text(schema.as_deref().unwrap_or("选择数据库"))
        .show_ui(ui, |ui| {
            for name in &connection.schemas {
                if ui
                    .selectable_value(schema, Some(name.clone()), name)
                    .clicked()
                {
                    builder.canvas.search.clear();
                }
            }
        });
    ui.add(
        egui::TextEdit::singleline(&mut builder.canvas.search)
            .desired_width(width)
            .hint_text("筛选表与视图"),
    );
    let Some(objects) = schema
        .as_ref()
        .and_then(|schema| connection.objects.get(schema))
    else {
        if connection.server.is_none() {
            ui.label(
                RichText::new("连接数据库后可添加表。")
                    .small()
                    .color(style::muted()),
            );
        }
        return None;
    };
    let filter = builder.canvas.search.trim().to_lowercase();
    let visible = objects
        .iter()
        .filter(|object| {
            matches!(object.kind, ObjectKind::Table | ObjectKind::View)
                && (filter.is_empty() || object.name.to_lowercase().contains(&filter))
        })
        .collect::<Vec<_>>();
    let mut add = None;
    if visible.is_empty() {
        ui.label(
            RichText::new("没有匹配的表或视图。")
                .small()
                .color(style::muted()),
        );
    }
    let row_height = ui.spacing().interact_size.y.max(22.0);
    let list_height = (height - (ui.next_widget_position().y - top)).max(row_height);
    egui::ScrollArea::vertical()
        .id_salt("builder-table-catalog")
        .max_height(list_height)
        .auto_shrink([false, true])
        .show_rows(ui, row_height, visible.len(), |ui, rows| {
            for object in &visible[rows] {
                let id = ui.id().with(("builder-table", *object));
                let row = ui.dnd_drag_source(id, (*object).clone(), |ui| {
                    ui.add(
                        egui::Button::new(&object.name)
                            .frame_when_inactive(false)
                            .min_size(vec2(ui.available_width(), row_height))
                            .truncate(),
                    )
                });
                let mut picked = row.inner.double_clicked();
                row.inner
                    .on_hover_text(format!("{} · 双击添加", object.name))
                    .context_menu(|ui| {
                        if ui.button("添加到画布").clicked() {
                            picked = true;
                            ui.close();
                        }
                    });
                if picked {
                    add = Some((*object).clone());
                }
            }
        });
    add
}

fn draw(
    ui: &mut egui::Ui,
    builder: &mut Builder,
    connection: &Workspace,
    size: Vec2,
) -> Option<(Column, Pos2)> {
    let (response, painter) = ui.allocate_painter(size, egui::Sense::click_and_drag());
    let viewport = response.rect;
    if std::mem::take(&mut builder.canvas.fit) {
        fit(&mut builder.query, viewport);
    }
    let zoom = builder.query.camera.zoom;
    let plot = Diagram::new(
        &builder.query,
        &builder.metadata,
        &builder.canvas.rows,
        builder.canvas.selected,
        viewport,
    );
    let mouse = response
        .interact_pointer_pos()
        .or_else(|| ui.ctx().pointer_interact_pos());
    let hover = response.hover_pos();
    let mut action = None;
    if response.drag_started_by(PointerButton::Middle) {
        builder.canvas.drag = Some(Drag::Pan);
    } else if response.drag_started_by(PointerButton::Primary)
        && let Some(point) = ui.input(|input| input.pointer.press_origin())
    {
        let card = plot.card_at(point);
        builder.canvas.selected = card.map(|card| card.source.id);
        builder.canvas.drag = Some(match card {
            Some(card) if card.resize_at(point) => Drag::Resize(card.source.id),
            Some(card) => card
                .column_at(point, zoom)
                .map_or(Drag::Card(card.source.id), Drag::Link),
            None => Drag::Pan,
        });
    }
    if response.drag_stopped()
        && let Some(Drag::Link(left)) = builder.canvas.drag.take()
        && let Some(point) = mouse
    {
        if viewport.contains(point) {
            if let Some(right) = plot
                .card_at(point)
                .and_then(|card| card.column_at(point, zoom))
            {
                action = Some(Action::AddLink(left, right));
            }
        } else {
            action = Some(Action::AddColumn(left, Some(point)));
        }
    }
    if let Some(pointer) = mouse
        && (response.clicked() || response.secondary_clicked() || response.double_clicked())
    {
        let card = plot.card_at(pointer);
        builder.canvas.selected = card.map(|card| card.source.id);
        if response.secondary_clicked() {
            builder.canvas.popup = card.map(|card| Hit::Card(card.source.id)).or_else(|| {
                plot.link_at(pointer)
                    .map(|link| Hit::Link(link.join, link.condition))
            });
        }
        if response.double_clicked()
            && let Some(card) = card
        {
            if let Some(column) = card.column_at(pointer, zoom) {
                action = Some(Action::AddColumn(column, None));
            } else if pointer.y < card.rect.top() + HEADER * zoom {
                builder.canvas.alias = Some(card.source.id);
            }
        }
    }
    let mut scroll = Vec2::ZERO;
    let mut zoom_by = 1.0;
    if let Some(pointer) = hover {
        let (scale, delta) = appearance::canvas_scroll(ui);
        zoom_by = scale;
        if scale == 1.0 && delta != Vec2::ZERO {
            let card = plot
                .card_at(pointer)
                .filter(|card| card.shown < card.columns.len());
            if let Some(card) = card.filter(|_| delta.y != 0.0) {
                let step = (delta.y.abs() / 28.0).round().max(1.0) as usize;
                let first = if delta.y > 0.0 {
                    card.first.saturating_sub(step)
                } else {
                    card.first
                        .saturating_add(step)
                        .min(card.columns.len() - card.shown)
                };
                builder.canvas.rows.insert(card.source.id, first);
            } else {
                scroll = delta;
            }
        }
        if let Some(card) = plot.card_at(pointer) {
            let cursor = if card.resize_at(pointer) {
                egui::CursorIcon::ResizeNwSe
            } else {
                egui::CursorIcon::Grab
            };
            ui.ctx().set_cursor_icon(cursor);
        } else if plot.link_at(pointer).is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
    }
    drop(plot);
    if let Some(pointer) = hover.filter(|_| zoom_by != 1.0) {
        builder
            .query
            .camera
            .zoom_at(zoom_by, pointer - viewport.min);
    }
    if response.dragged() {
        let delta = response.drag_delta();
        match builder.canvas.drag.as_ref() {
            Some(Drag::Pan) => scroll += delta,
            Some(Drag::Card(id)) | Some(Drag::Resize(id)) => {
                if let Some(source) = builder
                    .query
                    .sources
                    .iter_mut()
                    .find(|source| source.id == *id)
                {
                    if matches!(builder.canvas.drag, Some(Drag::Resize(_))) {
                        source.size = (Vec2::from(source.size) + delta / zoom)
                            .clamp(MIN_CARD, MAX_CARD)
                            .into();
                    } else {
                        source.position = bounded(Vec2::from(source.position) + delta / zoom);
                    }
                }
            }
            _ => {}
        }
    }
    if scroll != Vec2::ZERO {
        builder.query.camera.pan = bounded(Vec2::from(builder.query.camera.pan) + scroll);
    }
    response.context_menu(|ui| context_menu(ui, builder, &mut action));
    if let Some(object) = response.dnd_release_payload::<DbObject>()
        && let Some(pointer) = mouse
    {
        let corner = (pointer - builder.query.camera.origin(viewport)) / builder.query.camera.zoom;
        builder.add_source((*object).clone(), Some(bounded(corner - vec2(16.0, 12.0))));
    }
    let mut pending = None;
    match action {
        Some(Action::AddColumn(column, Some(point))) => pending = Some((column, point)),
        Some(Action::AddColumn(column, None)) => {
            builder
                .query
                .add_field(Some(FieldInput::Column(column)), Aggregate::None);
            builder.section = Section::Fields;
        }
        Some(Action::AddLink(left, right)) => builder.connect(left, right),
        Some(Action::ChangeKind(index, kind)) => builder.query.joins[index].kind = kind,
        Some(Action::RemoveCondition(index, condition)) => {
            if builder.query.joins[index].conditions.len() == 1 {
                builder.query.joins.remove(index);
            } else {
                builder.query.joins[index].conditions.remove(condition);
            }
            builder.canvas.popup = None;
        }
        Some(Action::StartAt(id)) => {
            if let Some(index) = builder
                .query
                .sources
                .iter()
                .position(|source| source.id == id)
            {
                let source = builder.query.sources.remove(index);
                builder.query.sources.insert(0, source);
            }
        }
        Some(Action::RemoveSource(id)) => builder.remove_source(id),
        None => {}
    }
    paint(
        &response,
        painter.with_clip_rect(viewport),
        viewport,
        builder,
        connection,
        hover,
        mouse,
    );
    pending
}

fn context_menu(ui: &mut egui::Ui, builder: &mut Builder, action: &mut Option<Action>) {
    match builder.canvas.popup {
        Some(Hit::Card(id)) => {
            if let Some(source) = builder.query.source(id) {
                ui.label(source.name());
                if ui.button("编辑表别名").clicked() {
                    builder.canvas.alias = Some(id);
                    ui.close();
                }
                if builder
                    .query
                    .sources
                    .first()
                    .is_some_and(|base| base.id != id)
                    && ui.button("设为起始表").clicked()
                {
                    *action = Some(Action::StartAt(id));
                    ui.close();
                }
                if ui.button("从画布移除").clicked() {
                    *action = Some(Action::RemoveSource(id));
                    ui.close();
                }
            }
        }
        Some(Hit::Link(index, condition)) => {
            if let Some(join) = builder.query.joins.get(index) {
                let left = builder.query.source(join.left).map_or("?", Source::label);
                let right = builder.query.source(join.right).map_or("?", Source::label);
                ui.label(format!("{left} → {right}"));
                for kind in JoinKind::ALL {
                    if ui
                        .selectable_label(join.kind == kind, kind.label())
                        .clicked()
                    {
                        *action = Some(Action::ChangeKind(index, kind));
                        ui.close();
                    }
                }
                if ui.button("移除连接条件").clicked() {
                    *action = Some(Action::RemoveCondition(index, condition));
                    ui.close();
                }
            }
        }
        None => {
            if ui.button("适合画布").clicked() {
                builder.canvas.fit = true;
                ui.close();
            }
        }
    }
}

fn fit(query: &mut Query, viewport: Rect) {
    let bounds = query.sources.iter().fold(Rect::NOTHING, |all, source| {
        all.union(Rect::from_min_size(
            Pos2::from(source.position),
            Vec2::from(source.size),
        ))
    });
    if bounds.is_negative() {
        query.camera = Camera::default();
        return;
    }
    let space = (viewport.size() - Vec2::splat(28.0)).max(Vec2::splat(1.0));
    let zoom = (space.x / bounds.width())
        .min(space.y / bounds.height())
        .clamp(MIN_ZOOM, 1.0);
    query.camera = Camera {
        zoom,
        pan: bounded(Vec2::splat(14.0) - bounds.min.to_vec2() * zoom),
    };
}

fn paint(
    response: &egui::Response,
    painter: egui::Painter,
    viewport: Rect,
    builder: &Builder,
    connection: &Workspace,
    hover: Option<Pos2>,
    pointer: Option<Pos2>,
) {
    let zoom = builder.query.camera.zoom;
    let origin = builder.query.camera.origin(viewport);
    appearance::canvas(&painter, viewport, origin, zoom);
    if response.dnd_hover_payload::<DbObject>().is_some() {
        painter.rect_stroke(
            viewport.shrink(1.0),
            8,
            Stroke::new(2.0_f32, style::accent()),
            StrokeKind::Inside,
        );
    }
    let diagram = Diagram::new(
        &builder.query,
        &builder.metadata,
        &builder.canvas.rows,
        builder.canvas.selected,
        viewport,
    );
    if diagram.cards.is_empty() {
        painter.text(
            viewport.center(),
            Align2::CENTER_CENTER,
            "将表或视图拖到这里",
            FontId::proportional(17.0),
            style::muted(),
        );
        return;
    }
    let active = hover.and_then(|point| diagram.link_at(point));
    for link in &diagram.links {
        let hovered = active
            .is_some_and(|active| active.join == link.join && active.condition == link.condition);
        let join = &builder.query.joins[link.join];
        let adjacent = builder
            .canvas
            .selected
            .is_some_and(|id| id == join.left || id == join.right);
        let stroke = if hovered || adjacent {
            Stroke::new(if hovered { 2.6_f32 } else { 2.0_f32 }, style::accent())
        } else {
            Stroke::new(1.6_f32, style::muted())
        };
        appearance::link(&painter, link.points, stroke);
        let [_, _, bend, end] = link.points;
        let direction = (end.x - bend.x).signum();
        if (end.x - bend.x).abs() >= 10.0 {
            painter.add(egui::Shape::line(
                vec![
                    end + vec2(-7.0 * direction, -4.0),
                    end,
                    end + vec2(-7.0 * direction, 4.0),
                ],
                stroke,
            ));
        }
    }
    let projected = builder
        .query
        .fields
        .iter()
        .filter_map(|field| match &field.input {
            Some(FieldInput::Column(column)) if field.show => {
                Some((column.table, column.name.as_str()))
            }
            _ => None,
        })
        .collect::<HashSet<_>>();
    let hovered_card = hover
        .and_then(|point| diagram.card_at(point))
        .map(|card| card.source.id);
    let drop_card = pointer
        .filter(|point| viewport.contains(*point))
        .and_then(|point| diagram.card_at(point))
        .map(|card| card.source.id);
    for card in &diagram.cards {
        if !card.rect.intersects(viewport) {
            continue;
        }
        let id = card.source.id;
        let selected = builder.canvas.selected == Some(id);
        let header = appearance::card(&painter, card.rect, HEADER * zoom, zoom, selected);
        let from = card.source;
        let live_table = connection.server.is_some() && from.object.kind == ObjectKind::Table;
        let details = live_table
            .then(|| column_details(connection, &from.object))
            .flatten();
        let cached_keys = live_table
            .then(|| connection.tags.primary_keys(&from.object))
            .flatten()
            .unwrap_or_default();
        let font = FontId::monospace((12.0 * zoom).max(5.0));
        let content = painter.with_clip_rect(card.rect.shrink(if selected { 2.0 } else { 1.0 }));
        appearance::table_icon(
            &content,
            pos2(header.left() + 14.0 * zoom, header.center().y),
            zoom,
            from.object.kind == ObjectKind::View,
            if selected {
                style::accent()
            } else {
                style::muted()
            },
        );
        let title_bounds = Rect::from_min_max(
            header.left_top() + vec2(26.0 * zoom, 0.0),
            header.right_bottom() - vec2(9.0 * zoom, 0.0),
        );
        let alias = from.alias.trim();
        let label = if alias.is_empty() {
            std::borrow::Cow::Borrowed(from.object.name.as_str())
        } else {
            std::borrow::Cow::Owned(format!("{} · {alias}", from.object.name))
        };
        appearance::text(
            &content,
            title_bounds,
            &label,
            FontId::proportional((14.0 * zoom).max(5.0)),
            style::ink(),
        );
        for (slot, column) in card.columns[card.first..card.first + card.shown]
            .iter()
            .enumerate()
        {
            let y = header.bottom() + (slot as f32 + 0.5) * ROW * zoom;
            let row = Rect::from_center_size(
                pos2(card.rect.center().x, y),
                vec2(card.rect.width(), ROW * zoom),
            );
            let is_drop = matches!(&builder.canvas.drag, Some(Drag::Link(from)) if from.table != id)
                && drop_card == Some(id)
                && pointer.is_some_and(|point| row.contains(point) && !card.resize_at(point));
            let is_hovered =
                hovered_card == Some(id) && hover.is_some_and(|point| row.contains(point));
            let tint = row.shrink2(vec2(3.0 * zoom, 1.5 * zoom));
            if is_drop {
                content.rect(
                    tint,
                    4,
                    style::accent_soft(),
                    Stroke::new(1.0_f32, style::accent()),
                    StrokeKind::Inside,
                );
            } else if is_hovered {
                content.rect_filled(tint, 4, style::panel());
            }
            let key = details.map_or_else(
                || {
                    cached_keys
                        .contains(column)
                        .then_some(appearance::KeyMarker::Primary)
                },
                |columns| {
                    columns
                        .iter()
                        .find(|info| info.name == *column)
                        .and_then(|info| appearance::KeyMarker::from_column(&info.key))
                },
            );
            if let Some(key) = key {
                key.paint(&content, pos2(row.left() + 13.0 * zoom, y), zoom);
            }
            if projected.contains(&(id, column.as_str())) {
                let center = pos2(row.right() - 16.0 * zoom, y);
                let check = [vec2(-4.0, 0.0), vec2(-1.0, 3.0), vec2(4.3, -3.5)]
                    .map(|point| center + point * zoom);
                content.add(egui::Shape::line(
                    check.to_vec(),
                    Stroke::new((1.5 * zoom).max(0.65), style::accent()),
                ));
            }
            appearance::text(
                &content,
                Rect::from_min_max(
                    row.min + vec2(23.0 * zoom, 0.0),
                    row.max - vec2(24.0 * zoom, 0.0),
                ),
                column,
                font.clone(),
                style::ink(),
            );
        }
        if card.columns.is_empty() {
            content.text(
                pos2(
                    card.rect.left() + 9.0 * zoom,
                    header.bottom() + ROW * zoom / 2.0,
                ),
                Align2::LEFT_CENTER,
                "暂无列信息",
                font.clone(),
                style::muted(),
            );
        }
        appearance::row_scroll(
            &content,
            Rect::from_min_max(
                pos2(card.rect.left(), header.bottom() + 4.0 * zoom),
                card.rect.max - vec2(0.0, 14.0 * zoom),
            ),
            card.first..card.first + card.shown,
            card.columns.len(),
            zoom,
        );
        appearance::resize_grip(&content, card.rect, zoom);
    }
    if let Some(Drag::Link(column)) = &builder.canvas.drag
        && let Some(point) = pointer
        && let Some(card) = diagram
            .cards
            .iter()
            .find(|card| column.table == card.source.id)
    {
        let x = if point.x >= card.rect.center().x {
            card.rect.right()
        } else {
            card.rect.left()
        };
        painter.line_segment(
            [pos2(x, card.anchor_y(&column.name, zoom)), point],
            Stroke::new(2.3_f32, style::accent()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::{ColumnInfo, ObjectDetail, ServerInfo};

    fn render(
        ctx: &egui::Context,
        builder: &mut Builder,
        connection: &Workspace,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(850.0, 480.0))),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    workspace(ui, builder, connection, 250.0);
                });
            },
        )
    }

    fn text_positions(output: &egui::FullOutput, label: &str) -> Vec<Pos2> {
        output
            .shapes
            .iter()
            .filter_map(|item| {
                let egui::Shape::Text(text) = &item.shape else {
                    return None;
                };
                if text.galley.text() == label {
                    let point = text.galley.rect.translate(text.pos.to_vec2()).center();
                    return item.clip_rect.contains(point).then_some(point);
                }
                None
            })
            .collect()
    }

    fn text_at(output: &egui::FullOutput, label: &str) -> Pos2 {
        text_positions(output, label)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("测试画布上未找到 {label}"))
    }

    fn drag(
        ctx: &egui::Context,
        builder: &mut Builder,
        connection: &Workspace,
        from: Pos2,
        to: Pos2,
    ) -> egui::FullOutput {
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        render(
            ctx,
            builder,
            connection,
            vec![egui::Event::PointerMoved(from), button(from, true)],
        );
        render(
            ctx,
            builder,
            connection,
            vec![egui::Event::PointerMoved(to)],
        );
        render(ctx, builder, connection, vec![button(to, false)]);
        render(ctx, builder, connection, Vec::new())
    }

    #[test]
    fn query_builder_canvas_marks_every_composite_primary_key_column() {
        let ctx = egui::Context::default();
        style::apply(&ctx);
        let table = DbObject {
            schema: "warehouse".into(),
            name: "inventory".into(),
            kind: ObjectKind::Table,
        };
        let mut connection = Workspace::new("测试连接".into(), None);
        connection.server = Some(ServerInfo {
            version: "fixture".into(),
            connection_id: 1,
            database: Some(table.schema.clone()),
            http_tunnel: false,
        });
        connection.tags.remember(&ObjectDetail {
            object: table.clone(),
            columns: [("item_id", "PRI"), ("bin_id", "PRI"), ("quantity", "")]
                .into_iter()
                .map(|(name, key)| ColumnInfo {
                    name: name.into(),
                    data_type: "int".into(),
                    nullable: false,
                    key: key.into(),
                    default_value: None,
                    extra: String::new(),
                })
                .collect(),
            ddl: String::new(),
            ddl_error: None,
        });
        let mut builder = Builder::new(table);
        builder.metadata.receive(&builder.query, &connection);

        let output = render(&ctx, &mut builder, &connection, Vec::new());
        let stars = text_positions(&output, "*");
        assert_eq!(stars.len(), 2);
        for name in ["item_id", "bin_id"] {
            let column = text_at(&output, name);
            assert!(
                stars
                    .iter()
                    .any(|star| star.x < column.x && (star.y - column.y).abs() < 4.0),
                "主键标记应在列名 {name} 左侧"
            );
        }
    }

    #[test]
    fn dragging_catalog_tables_and_columns_builds_a_query() {
        let object = |name: &str| DbObject {
            schema: "warehouse".into(),
            name: name.into(),
            kind: ObjectKind::Table,
        };
        let ctx = egui::Context::default();
        style::apply(&ctx);
        let mut connection = Workspace::new("测试连接".into(), None);
        connection.schemas.push("warehouse".into());
        connection
            .objects
            .insert("warehouse".into(), vec![object("orders"), object("users")]);
        let mut builder = Builder::new_blank(Some("warehouse".into()));
        builder
            .metadata
            .columns
            .insert(object("orders"), vec!["user_id".into()]);
        builder
            .metadata
            .columns
            .insert(object("users"), vec!["id".into()]);

        let initial = render(&ctx, &mut builder, &connection, Vec::new());
        let center = text_at(&initial, "将表或视图拖到这里");
        let orders = text_at(&initial, "orders");
        let users = text_at(&initial, "users");
        drag(
            &ctx,
            &mut builder,
            &connection,
            orders,
            center - vec2(230.0, 88.0),
        );
        let canvas = drag(
            &ctx,
            &mut builder,
            &connection,
            users,
            center + vec2(20.0, -88.0),
        );
        assert_eq!(builder.query.sources.len(), 2);
        let header = text_at(&canvas, "orders · t1");
        let before = builder.query.sources[0].position[1];
        let moved = drag(
            &ctx,
            &mut builder,
            &connection,
            header,
            header + vec2(13.0, 36.0),
        );
        assert!(builder.query.sources[0].position[1] >= before + 30.0);

        let from = text_at(&moved, "id");
        let to = text_at(&moved, "user_id");
        drag(&ctx, &mut builder, &connection, from, to);
        assert_eq!(builder.query.joins.len(), 1);
        assert_eq!(builder.query.joins[0].left, 2);
        assert_eq!(builder.query.joins[0].right, 1);
        builder.query.joins[0].kind = JoinKind::Left;
        let sql = builder.query.sql(false).unwrap();
        assert!(
            sql.contains("RIGHT JOIN `warehouse`.`users` AS `t2`\n  ON `t1`.`user_id` = `t2`.`id`")
        );
    }
}
