use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, RichText, Stroke,
    TextStyle,
    text::{Galley, LayoutJob, LayoutSection, TextFormat},
};
use std::{
    borrow::Cow,
    ops::Range,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU8, Ordering},
    },
};

use crate::sql_tools::{EditorTokenKind, editor_tokens};

use super::{
    completion::vocabulary,
    preferences::{EditorSettings, FontRole, LetterCase, Preferences, SqlColors},
};

/// 界面主题模式：晴海浅色（默认）与深海暗夜。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeMode {
    Light,
    Dark,
}

static ACTIVE_THEME: AtomicU8 = AtomicU8::new(0);

#[derive(Clone, Copy)]
pub struct Palette {
    pub ink: Color32,
    pub muted: Color32,
    pub subtle: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    pub accent_tint: Color32,
    pub cyan: Color32,
    pub on_accent: Color32,
    pub error: Color32,
    pub error_soft: Color32,
    pub success: Color32,
    pub success_soft: Color32,
    pub warning: Color32,
    pub surface: Color32,
    pub panel: Color32,
    pub chrome: Color32,
    pub stripe: Color32,
    pub border: Color32,
    pub border_subtle: Color32,
    pub border_strong: Color32,
    pub backdrop: [u8; 4],
    pub shadow_alpha: u8,
    pub number: Color32,
}

const LIGHT: Palette = Palette {
    ink: Color32::from_rgb(11, 18, 32),
    muted: Color32::from_rgb(90, 100, 120),
    subtle: Color32::from_rgb(137, 146, 166),
    accent: Color32::from_rgb(37, 99, 235),
    accent_soft: Color32::from_rgb(234, 240, 255),
    accent_tint: Color32::from_rgb(244, 247, 255),
    cyan: Color32::from_rgb(14, 165, 233),
    on_accent: Color32::from_rgb(255, 255, 255),
    error: Color32::from_rgb(225, 29, 72),
    error_soft: Color32::from_rgb(253, 239, 243),
    success: Color32::from_rgb(22, 163, 74),
    success_soft: Color32::from_rgb(234, 248, 240),
    warning: Color32::from_rgb(217, 119, 6),
    surface: Color32::from_rgb(255, 255, 255),
    panel: Color32::from_rgb(245, 247, 251),
    chrome: Color32::from_rgb(236, 239, 245),
    stripe: Color32::from_rgb(249, 250, 253),
    border: Color32::from_rgb(223, 228, 238),
    border_subtle: Color32::from_rgb(235, 239, 246),
    border_strong: Color32::from_rgb(196, 204, 219),
    backdrop: [10, 18, 40, 110],
    shadow_alpha: 26,
    number: Color32::from_rgb(124, 92, 230),
};

const DARK: Palette = Palette {
    ink: Color32::from_rgb(231, 236, 245),
    muted: Color32::from_rgb(152, 163, 184),
    subtle: Color32::from_rgb(112, 123, 144),
    accent: Color32::from_rgb(124, 156, 255),
    accent_soft: Color32::from_rgb(27, 39, 69),
    accent_tint: Color32::from_rgb(19, 26, 43),
    cyan: Color32::from_rgb(56, 189, 248),
    on_accent: Color32::from_rgb(7, 17, 37),
    error: Color32::from_rgb(248, 113, 113),
    error_soft: Color32::from_rgb(58, 27, 34),
    success: Color32::from_rgb(74, 222, 128),
    success_soft: Color32::from_rgb(20, 44, 33),
    warning: Color32::from_rgb(251, 191, 36),
    surface: Color32::from_rgb(22, 27, 36),
    panel: Color32::from_rgb(14, 18, 25),
    chrome: Color32::from_rgb(9, 12, 17),
    stripe: Color32::from_rgb(27, 33, 44),
    border: Color32::from_rgb(42, 50, 66),
    border_subtle: Color32::from_rgb(32, 38, 51),
    border_strong: Color32::from_rgb(60, 71, 92),
    backdrop: [2, 5, 14, 170],
    shadow_alpha: 78,
    number: Color32::from_rgb(180, 156, 255),
};

pub fn theme() -> ThemeMode {
    if ACTIVE_THEME.load(Ordering::Relaxed) == 1 {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    }
}

pub fn is_dark() -> bool {
    theme() == ThemeMode::Dark
}

pub fn palette() -> Palette {
    if is_dark() { DARK } else { LIGHT }
}

pub fn toggle_theme(ctx: &egui::Context) {
    let next = if is_dark() { 0 } else { 1 };
    ACTIVE_THEME.store(next, Ordering::Relaxed);
    apply(ctx);
    ctx.request_repaint();
}

pub fn ink() -> Color32 {
    palette().ink
}
pub fn muted() -> Color32 {
    palette().muted
}
pub fn subtle() -> Color32 {
    palette().subtle
}
pub fn accent() -> Color32 {
    palette().accent
}
pub fn accent_soft() -> Color32 {
    palette().accent_soft
}
pub fn accent_tint() -> Color32 {
    palette().accent_tint
}
pub fn cyan() -> Color32 {
    palette().cyan
}
pub fn error() -> Color32 {
    palette().error
}
pub fn error_soft() -> Color32 {
    palette().error_soft
}
pub fn success() -> Color32 {
    palette().success
}
pub fn success_soft() -> Color32 {
    palette().success_soft
}
pub fn warning() -> Color32 {
    palette().warning
}
pub fn surface() -> Color32 {
    palette().surface
}
pub fn panel() -> Color32 {
    palette().panel
}
pub fn chrome() -> Color32 {
    palette().chrome
}
pub fn border() -> Color32 {
    palette().border
}
pub fn border_subtle() -> Color32 {
    palette().border_subtle
}
pub fn border_strong() -> Color32 {
    palette().border_strong
}

pub fn bar(margin: egui::Margin) -> egui::Frame {
    egui::Frame::new().fill(surface()).inner_margin(margin)
}

pub fn pane(margin: egui::Margin) -> egui::Frame {
    egui::Frame::new().fill(panel()).inner_margin(margin)
}

pub fn chrome_bar(margin: egui::Margin) -> egui::Frame {
    egui::Frame::new().fill(chrome()).inner_margin(margin)
}

/// 在两个颜色之间做平滑插值，用于悬停与选中态的轻量过渡。
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let channel = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(
        channel(a.r(), b.r()),
        channel(a.g(), b.g()),
        channel(a.b(), b.b()),
        channel(a.a(), b.a()),
    )
}

/// 带内嵌搜索图标的筛选输入框，保持与主题一致的细边框和柔和底色。
pub fn search_field(ui: &mut egui::Ui, text: &mut String, hint: &str) -> egui::Response {
    let p = palette();
    egui::Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0_f32, p.border))
        .corner_radius(CornerRadius::same(9))
        .inner_margin(egui::Margin::symmetric(10, 3))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let (icon_rect, _) =
                    ui.allocate_exact_size(egui::vec2(13.0, 13.0), egui::Sense::hover());
                paint_search_icon(ui.painter(), icon_rect, p.subtle);
                let response = ui.add(
                    egui::TextEdit::singleline(text)
                        .hint_text(hint)
                        .frame(false)
                        .margin(egui::Margin::symmetric(0, 4))
                        .desired_width((ui.available_width() - 18.0).max(40.0)),
                );
                if !text.is_empty()
                    && ui
                        .add(
                            egui::Label::new(RichText::new("×").size(15.0).color(p.subtle))
                                .sense(egui::Sense::click()),
                        )
                        .on_hover_text("清空筛选")
                        .clicked()
                {
                    text.clear();
                }
                response
            })
            .inner
        })
        .inner
}

fn paint_search_icon(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let center = rect.center() - egui::vec2(0.8, 0.8);
    let stroke = Stroke::new(1.35_f32, color);
    painter.circle_stroke(center, 4.1, stroke);
    painter.line_segment(
        [
            center + egui::vec2(2.9, 2.9),
            center + egui::vec2(5.4, 5.4),
        ],
        Stroke::new(1.6_f32, color),
    );
}

/// 连接工作区页签：低噪音底色、细描边与底部主题条，悬停时平滑过渡。
pub fn workspace_tab(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash,
    label: &str,
    selected: bool,
    dot_color: Color32,
    connected: bool,
) -> egui::Response {
    let p = palette();
    let font = FontId::proportional(13.5);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font.clone(), p.ink);
    let width = (13.0 + 13.0 + galley.size().x + 13.0).max(82.0);
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, 30.0), egui::Sense::click());
    let hover_t = ui.ctx().animate_bool_with_time(
        ui.make_persistent_id(("workspace-tab-hover", &id_salt)),
        response.hovered() && !selected,
        0.16,
    );
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let fill = if selected {
            p.surface
        } else {
            mix(Color32::TRANSPARENT, p.accent_tint, hover_t)
        };
        painter.rect(
            rect,
            CornerRadius::same(8),
            fill,
            Stroke::NONE,
            egui::StrokeKind::Inside,
        );
        if selected {
            let bar = egui::Rect::from_min_max(
                egui::pos2(rect.left() + 8.0, rect.bottom() - 2.0),
                egui::pos2(rect.right() - 8.0, rect.bottom()),
            );
            painter.rect_filled(bar, CornerRadius::same(1), p.accent);
        }
        let dot_center = egui::pos2(rect.left() + 13.0, rect.center().y);
        if connected {
            painter.circle_filled(dot_center, 4.0, dot_color.gamma_multiply(0.16));
            painter.circle_filled(dot_center, 2.3, dot_color);
        } else {
            painter.circle_stroke(dot_center, 2.4, Stroke::new(1.2_f32, dot_color));
        }
        let text_color = if selected {
            p.ink
        } else {
            mix(p.muted, p.ink, hover_t)
        };
        let text = painter.layout_no_wrap(label.to_owned(), font.clone(), text_color);
        let text_pos = egui::pos2(
            rect.left() + 26.0,
            rect.center().y - text.size().y * 0.5,
        );
        painter.galley(text_pos, text.clone(), text_color);
        if selected {
            let bold = painter.layout_no_wrap(label.to_owned(), font, text_color);
            painter.galley(text_pos + egui::vec2(0.35, 0.0), bold, text_color);
        }
    }
    response
}

/// 统一页面/弹窗标题：左侧短主题色标记 + 加粗标题，让所有二级与三级页面拥有同一识别点。
pub fn page_title(ui: &mut egui::Ui, text: impl Into<RichText>) -> egui::Response {
    let p = palette();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 7.0;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(3.0, 18.0), egui::Sense::hover());
        if ui.is_rect_visible(rect) {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(2), p.accent);
        }
        ui.heading(text)
    })
    .inner
}

pub fn card(margin: egui::Margin) -> egui::Frame {
    egui::Frame::new()
        .fill(surface())
        .stroke(Stroke::new(1.0_f32, border()))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(margin)
}

pub fn subtle_card(margin: egui::Margin) -> egui::Frame {
    egui::Frame::new()
        .fill(panel())
        .stroke(Stroke::new(1.0_f32, border()))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(margin)
}

/// 带浅色标题条的结构化内容卡片，始终撑满可用宽度，避免对话框出现空洞留白。
pub fn section_card<R>(
    ui: &mut egui::Ui,
    title: &str,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    section_card_with_header(ui, title, |_| {}, body)
}

/// 带自定义右侧头部内容（如数量徽章、快捷操作）的结构化内容卡片。
pub fn section_card_with_header<R>(
    ui: &mut egui::Ui,
    title: &str,
    header_right: impl FnOnce(&mut egui::Ui),
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let p = palette();
    egui::Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0_f32, p.border))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::same(0))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            egui::Frame::new()
                .fill(p.panel)
                .corner_radius(CornerRadius {
                    nw: 10,
                    ne: 10,
                    sw: 0,
                    se: 0,
                })
                .inner_margin(egui::Margin::symmetric(12, 7))
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(title).size(13.0).strong().color(p.ink));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            header_right(ui);
                        });
                    });
                });
            let sep_rect = ui.available_rect_before_wrap();
            ui.painter().hline(
                sep_rect.x_range(),
                sep_rect.top(),
                Stroke::new(1.0_f32, p.border_subtle),
            );
            egui::Frame::new()
                .inner_margin(egui::Margin::same(12))
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    body(ui)
                })
                .inner
        })
        .inner
}

/// 均衡的左右双栏容器（支持安全共享同一个可变状态）。
pub fn split_two_columns_with<T, R1, R2>(
    ui: &mut egui::Ui,
    left_ratio: f32,
    gap: f32,
    state: &mut T,
    left: impl FnOnce(&mut egui::Ui, &mut T) -> R1,
    right: impl FnOnce(&mut egui::Ui, &mut T) -> R2,
) -> (R1, R2) {
    let total = ui.available_width();
    let left_w = ((total - gap) * left_ratio).max(120.0);
    let right_w = (total - gap - left_w).max(120.0);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        let r1 = ui
            .vertical(|ui| {
                ui.set_width(left_w);
                left(ui, state)
            })
            .inner;
        let r2 = ui
            .vertical(|ui| {
                ui.set_width(right_w);
                right(ui, state)
            })
            .inner;
        (r1, r2)
    })
    .inner
}

/// 均衡的左右双栏容器（自动按比例分配并留出舒适栏间距）。
pub fn split_two_columns<R1, R2>(
    ui: &mut egui::Ui,
    left_ratio: f32,
    gap: f32,
    left: impl FnOnce(&mut egui::Ui) -> R1,
    right: impl FnOnce(&mut egui::Ui) -> R2,
) -> (R1, R2) {
    let mut unit = ();
    split_two_columns_with(ui, left_ratio, gap, &mut unit, |ui, _| left(ui), |ui, _| right(ui))
}

/// 标准弹窗底部操作栏（顶部细分割线 + 右对齐操作按钮）。
pub fn dialog_footer<R>(ui: &mut egui::Ui, right: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.add_space(10.0);
    ui.separator();
    ui.add_space(6.0);
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), ui.spacing().interact_size.y),
        egui::Layout::right_to_left(egui::Align::Center),
        right,
    )
    .inner
}

/// 带左侧辅助操作的标准弹窗底栏（如恢复默认、刷新），右侧仍放主次操作。
pub fn dialog_footer_split<R1, R2>(
    ui: &mut egui::Ui,
    leading: impl FnOnce(&mut egui::Ui) -> R1,
    trailing: impl FnOnce(&mut egui::Ui) -> R2,
) -> (R1, R2) {
    ui.add_space(10.0);
    ui.separator();
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let left = leading(ui);
        let right = ui
            .with_layout(egui::Layout::right_to_left(egui::Align::Center), trailing)
            .inner;
        (left, right)
    })
    .inner
}

/// 在禁用状态下自动呈现清爽中性灰的主按钮，避免发白低对比度。
pub fn primary_enabled(enabled: bool, label: &str) -> egui::Button<'_> {
    let p = palette();
    if enabled {
        primary(label)
    } else {
        egui::Button::new(RichText::new(label).color(p.subtle))
            .fill(p.chrome)
            .stroke(Stroke::new(1.0_f32, p.border))
            .corner_radius(CornerRadius::same(7))
    }
}

/// 在容器边缘绘制 1px 细腻分割线，避免面板之间生硬粘连。
pub fn paint_bottom_hairline(ui: &egui::Ui) {
    let rect = ui.max_rect();
    ui.painter()
        .hline(rect.x_range(), rect.bottom(), Stroke::new(1.0_f32, border()));
}

pub fn paint_top_hairline(ui: &egui::Ui) {
    let rect = ui.max_rect();
    ui.painter()
        .hline(rect.x_range(), rect.top(), Stroke::new(1.0_f32, border()));
}

pub fn paint_right_hairline(ui: &egui::Ui) {
    let rect = ui.max_rect();
    ui.painter()
        .vline(rect.right(), rect.y_range(), Stroke::new(1.0_f32, border()));
}

pub(super) fn modal(ctx: &egui::Context, id: egui::Id) -> egui::Modal {
    let p = palette();
    let frame = egui::Frame::window(&ctx.style())
        .fill(p.surface)
        .stroke(Stroke::new(1.0_f32, p.border))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(egui::Margin::same(18));
    let [r, g, b, a] = p.backdrop;
    egui::Modal::new(id)
        .frame(frame)
        .backdrop_color(Color32::from_rgba_unmultiplied(r, g, b, a))
}

/// 设置页与作业编辑器的统一分段页签栏：柔和底色、细描边、选中态主题色。
pub fn page_tabs<T: Copy + PartialEq>(ui: &mut egui::Ui, current: &mut T, tabs: &[(T, &str)]) {
    let p = palette();
    egui::Frame::new()
        .fill(p.panel)
        .stroke(Stroke::new(1.0_f32, p.border_subtle))
        .corner_radius(CornerRadius::same(9))
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                for (page, label) in tabs {
                    let selected = *current == *page;
                    let text = if selected {
                        RichText::new(*label).strong().color(p.accent)
                    } else {
                        RichText::new(*label).color(p.muted)
                    };
                    if tab_button(ui, selected, text, true).clicked() && !selected {
                        *current = *page;
                    }
                }
            });
        });
}

/// 右键菜单与下拉菜单统一的内边距和行距，让密集命令列表更易扫读。
pub fn context_menu_ui(ui: &mut egui::Ui) {
    ui.spacing_mut().button_padding = egui::vec2(6.0, 4.0);
    ui.spacing_mut().item_spacing.y = 2.0;
}

pub(super) fn menu(ui: &mut egui::Ui, label: &str, entries: impl FnOnce(&mut egui::Ui)) {
    ui.scope(|ui| {
        let p = palette();
        let widgets = &mut ui.visuals_mut().widgets;
        // 顶部菜单栏采用无框轻量悬停胶囊风格，减少视觉噪音
        widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
        widgets.inactive.bg_fill = Color32::TRANSPARENT;
        widgets.inactive.bg_stroke = Stroke::NONE;
        widgets.hovered.weak_bg_fill = p.accent_tint;
        widgets.hovered.bg_fill = p.accent_tint;
        widgets.hovered.bg_stroke = Stroke::NONE;
        widgets.open.weak_bg_fill = p.accent_soft;
        widgets.open.bg_fill = p.accent_soft;
        widgets.open.bg_stroke = Stroke::NONE;
        widgets.open.fg_stroke.color = p.accent;
        ui.menu_button(RichText::new(label).size(14.0).color(p.ink), |ui| {
            context_menu_ui(ui);
            let height = (ui.ctx().content_rect().height() - 108.0).max(76.0);
            egui::ScrollArea::vertical()
                .id_salt(("command-menu", label))
                .max_height(height)
                .show(ui, entries);
        });
    });
}

/// 菜单中的破坏性操作：正文使用错误色，悬停时给出柔和红色底，避免误触。
pub fn menu_item_danger(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let p = palette();
    ui.scope(|ui| {
        let widgets = &mut ui.visuals_mut().widgets;
        widgets.hovered.weak_bg_fill = p.error_soft;
        widgets.hovered.bg_fill = p.error_soft;
        widgets.active.weak_bg_fill = p.error_soft;
        widgets.active.bg_fill = p.error_soft;
        ui.button(RichText::new(label).color(p.error))
    })
    .inner
}

/// 无框下拉按钮：用于工具栏与状态栏中的次级菜单，默认安静、悬停才出现底色。
pub fn ghost_menu_button(
    ui: &mut egui::Ui,
    label: RichText,
    entries: impl FnOnce(&mut egui::Ui),
) -> egui::Response {
    let p = palette();
    ui.scope(|ui| {
        let widgets = &mut ui.visuals_mut().widgets;
        widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
        widgets.inactive.bg_fill = Color32::TRANSPARENT;
        widgets.inactive.bg_stroke = Stroke::NONE;
        widgets.hovered.weak_bg_fill = p.accent_tint;
        widgets.hovered.bg_fill = p.accent_tint;
        widgets.hovered.bg_stroke = Stroke::NONE;
        widgets.open.weak_bg_fill = p.accent_soft;
        widgets.open.bg_fill = p.accent_soft;
        widgets.open.bg_stroke = Stroke::NONE;
        widgets.open.fg_stroke.color = p.accent;
        ui.menu_button(label, entries).response
    })
    .inner
}

/// 工具栏次级操作：默认无描边，悬停时出现柔和主题底，减少成排方框按钮的陈旧感。
pub fn ghost_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let p = palette();
    let text_color = if ui.is_enabled() { p.ink } else { p.subtle };
    ui.scope(|ui| {
        let widgets = &mut ui.visuals_mut().widgets;
        widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
        widgets.inactive.bg_fill = Color32::TRANSPARENT;
        widgets.inactive.bg_stroke = Stroke::NONE;
        widgets.hovered.weak_bg_fill = p.accent_tint;
        widgets.hovered.bg_fill = p.accent_tint;
        widgets.hovered.bg_stroke = Stroke::NONE;
        ui.add(
            egui::Button::new(RichText::new(label).color(text_color))
                .corner_radius(CornerRadius::same(7))
                .min_size(egui::vec2(0.0, 28.0)),
        )
    })
    .inner
}

pub fn ghost_button_enabled(ui: &mut egui::Ui, label: &str, enabled: bool) -> egui::Response {
    ui.add_enabled_ui(enabled, |ui| ghost_button(ui, label)).inner
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionTone {
    Primary,
    Secondary,
    Danger,
}

/// 图标 + 文字的明确操作按钮：用于编辑区「执行 / 批量执行 / 格式化」，
/// 通过实体按钮轮廓与图标和上方页签栏拉开差异，避免被误读为页签。
pub fn action_button(
    ui: &mut egui::Ui,
    icon: ToolbarIcon,
    label: &str,
    tone: ActionTone,
    enabled: bool,
) -> egui::Response {
    let p = palette();
    let font = FontId::proportional(13.5);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, p.ink);
    let icon_w = 14.0;
    let gap = 6.0;
    let pad = 10.0;
    let width = (pad * 2.0 + icon_w + gap + galley.size().x).max(72.0);
    let height = 28.0;
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), sense);
    let hover_t = ui.ctx().animate_bool_with_time(
        response.id.with("action-button-hover"),
        enabled && response.hovered(),
        0.14,
    );
    if ui.is_rect_visible(rect) {
        let active = enabled && response.is_pointer_button_down_on();
        let (fill, stroke, text_col, icon_col) = if !enabled {
            (p.panel, p.border_subtle, p.subtle, p.subtle)
        } else {
            match tone {
                ActionTone::Primary => {
                    let fill = if active {
                        mix(p.accent, p.ink, 0.16)
                    } else {
                        mix(p.accent, p.on_accent, 0.10 * hover_t)
                    };
                    (fill, Color32::TRANSPARENT, p.on_accent, p.on_accent)
                }
                ActionTone::Danger => {
                    let fill = if active {
                        mix(p.error, p.ink, 0.14)
                    } else {
                        mix(p.error, p.on_accent, 0.10 * hover_t)
                    };
                    (fill, Color32::TRANSPARENT, p.on_accent, p.on_accent)
                }
                ActionTone::Secondary => {
                    let fill = if active {
                        p.accent_soft
                    } else {
                        mix(p.accent_tint, p.accent_soft, hover_t)
                    };
                    let stroke = if active {
                        p.accent
                    } else {
                        mix(p.border, p.accent, 0.45 * hover_t)
                    };
                    (fill, stroke, p.ink, mix(p.accent, p.ink, 0.15 * hover_t))
                }
            }
        };
        let painter = ui.painter();
        painter.rect(
            rect,
            CornerRadius::same(8),
            fill,
            if stroke == Color32::TRANSPARENT {
                Stroke::NONE
            } else {
                Stroke::new(1.0_f32, stroke)
            },
            egui::StrokeKind::Inside,
        );
        let icon_rect = egui::Rect::from_center_size(
            egui::pos2(rect.left() + pad + icon_w * 0.5, rect.center().y),
            egui::vec2(icon_w, icon_w),
        );
        paint_toolbar_icon(painter, icon_rect, icon, icon_col);
        painter.galley(
            egui::pos2(
                icon_rect.right() + gap,
                rect.center().y - galley.size().y * 0.5,
            ),
            galley,
            text_col,
        );
    }
    response
}

pub fn primary(label: &str) -> egui::Button<'_> {
    let p = palette();
    egui::Button::new(RichText::new(label).strong().color(p.on_accent))
        .fill(p.accent)
        .stroke(Stroke::NONE)
        .corner_radius(CornerRadius::same(8))
        .min_size(egui::vec2(0.0, 28.0))
}

pub fn danger(label: &str) -> egui::Button<'_> {
    let p = palette();
    egui::Button::new(RichText::new(label).strong().color(p.on_accent))
        .fill(p.error)
        .stroke(Stroke::NONE)
        .corner_radius(CornerRadius::same(8))
        .min_size(egui::vec2(0.0, 28.0))
}

/// 精致的胶囊标签（用于错误码、状态、计数、快捷键键帽等）。
pub fn badge(ui: &mut egui::Ui, text: impl Into<String>, fg: Color32, bg: Color32) {
    egui::Frame::new()
        .fill(bg)
        .corner_radius(CornerRadius::same(5))
        .inner_margin(egui::Margin::symmetric(7, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text.into()).small().strong().color(fg));
        });
}

/// 快捷键键帽胶囊，呈现键盘按键质感。
pub fn keycap(ui: &mut egui::Ui, text: &str) {
    let p = palette();
    egui::Frame::new()
        .fill(p.panel)
        .stroke(Stroke::new(1.0_f32, p.border))
        .corner_radius(CornerRadius::same(5))
        .inner_margin(egui::Margin::symmetric(7, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).small().monospace().color(p.ink));
        });
}

/// 带左侧主题色条的现代提示横幅。
pub fn banner(ui: &mut egui::Ui, text: &str, fg: Color32, bg: Color32) {
    let frame = egui::Frame::new()
        .fill(bg)
        .stroke(Stroke::new(1.0_f32, fg.gamma_multiply(0.28)))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin {
            left: 12,
            right: 10,
            top: 7,
            bottom: 7,
        });
    let response = frame.show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        ui.label(RichText::new(text).color(fg));
    });
    let rect = response.response.rect;
    let accent_strip = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 1.0, rect.top() + 5.0),
        egui::pos2(rect.left() + 4.0, rect.bottom() - 5.0),
    );
    ui.painter()
        .rect_filled(accent_strip, CornerRadius::same(2), fg);
}

/// 绘制连接状态圆点（可选柔和外圈光晕）。
pub fn draw_status_dot(ui: &mut egui::Ui, color: Color32, connected: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 12.0), egui::Sense::hover());
    let center = rect.center();
    let painter = ui.painter();
    if connected {
        painter.circle_filled(center, 5.0, color.gamma_multiply(0.22));
        painter.circle_filled(center, 3.0, color);
    } else {
        painter.circle_stroke(center, 3.0, Stroke::new(1.4_f32, color));
    }
}

/// 工具栏通用矢量图标枚举，确保高 DPI 下锐利清晰且不依赖系统字形。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolbarIcon {
    GridView,
    TextView,
    FormView,
    PrevPage,
    NextPage,
    Refresh,
    Stop,
    AddRow,
    EditRow,
    DuplicateRow,
    DeleteRow,
    Copy,
    Export,
    Save,
    Cancel,
    Run,
    RunBatch,
    Format,
}

/// 在指定矩形区域居中绘制 14×14px 高辨识度矢量图标。
pub fn paint_toolbar_icon(painter: &egui::Painter, rect: egui::Rect, icon: ToolbarIcon, color: Color32) {
    let c = rect.center();
    let s = Stroke::new(1.45_f32, color);
    let thin = Stroke::new(1.2_f32, color);
    match icon {
        ToolbarIcon::GridView => {
            let box_rect = egui::Rect::from_center_size(c, egui::vec2(12.0, 11.0));
            painter.rect_stroke(box_rect, CornerRadius::same(2), s, egui::StrokeKind::Inside);
            painter.line_segment(
                [egui::pos2(box_rect.left() + 1.0, box_rect.top() + 3.6), egui::pos2(box_rect.right() - 1.0, box_rect.top() + 3.6)],
                thin,
            );
            painter.line_segment(
                [egui::pos2(box_rect.left() + 1.0, box_rect.top() + 7.2), egui::pos2(box_rect.right() - 1.0, box_rect.top() + 7.2)],
                thin,
            );
            painter.line_segment(
                [egui::pos2(c.x, box_rect.top() + 3.6), egui::pos2(c.x, box_rect.bottom() - 1.0)],
                thin,
            );
        }
        ToolbarIcon::TextView => {
            for (dy, half_w) in [(-4.0, 5.5), (0.0, 5.5), (4.0, 3.2)] {
                painter.line_segment(
                    [egui::pos2(c.x - 5.5, c.y + dy), egui::pos2(c.x - 5.5 + half_w * 2.0, c.y + dy)],
                    s,
                );
            }
        }
        ToolbarIcon::FormView => {
            let box_rect = egui::Rect::from_center_size(c, egui::vec2(12.5, 11.0));
            painter.rect_stroke(box_rect, CornerRadius::same(2), s, egui::StrokeKind::Inside);
            painter.line_segment(
                [egui::pos2(box_rect.left() + 2.5, c.y - 2.0), egui::pos2(box_rect.left() + 4.8, c.y - 2.0)],
                s,
            );
            painter.line_segment(
                [egui::pos2(box_rect.left() + 6.4, c.y - 2.0), egui::pos2(box_rect.right() - 2.2, c.y - 2.0)],
                thin,
            );
            painter.line_segment(
                [egui::pos2(box_rect.left() + 2.5, c.y + 2.2), egui::pos2(box_rect.left() + 4.8, c.y + 2.2)],
                s,
            );
            painter.line_segment(
                [egui::pos2(box_rect.left() + 6.4, c.y + 2.2), egui::pos2(box_rect.right() - 2.2, c.y + 2.2)],
                thin,
            );
        }
        ToolbarIcon::PrevPage => {
            let pts = vec![
                egui::pos2(c.x + 2.5, c.y - 4.5),
                egui::pos2(c.x - 2.5, c.y),
                egui::pos2(c.x + 2.5, c.y + 4.5),
            ];
            painter.add(egui::Shape::line(pts, Stroke::new(1.7_f32, color)));
        }
        ToolbarIcon::NextPage => {
            let pts = vec![
                egui::pos2(c.x - 2.5, c.y - 4.5),
                egui::pos2(c.x + 2.5, c.y),
                egui::pos2(c.x - 2.5, c.y + 4.5),
            ];
            painter.add(egui::Shape::line(pts, Stroke::new(1.7_f32, color)));
        }
        ToolbarIcon::Refresh => {
            let r = 5.0_f32;
            let mut pts = Vec::with_capacity(14);
            let start = -0.35_f32;
            let end = std::f32::consts::TAU - 1.15_f32;
            for i in 0..=12 {
                let t = i as f32 / 12.0;
                let a = start + (end - start) * t;
                pts.push(egui::pos2(c.x + a.cos() * r, c.y + a.sin() * r));
            }
            painter.add(egui::Shape::line(pts, s));
            // 顶部右侧小箭头
            let tip = egui::pos2(c.x + start.cos() * r, c.y + start.sin() * r);
            painter.add(egui::Shape::line(
                vec![
                    egui::pos2(tip.x - 3.2, tip.y - 0.6),
                    tip,
                    egui::pos2(tip.x + 0.5, tip.y - 3.4),
                ],
                s,
            ));
        }
        ToolbarIcon::Stop => {
            let sq = egui::Rect::from_center_size(c, egui::vec2(9.0, 9.0));
            painter.rect_filled(sq, CornerRadius::same(2), color);
        }
        ToolbarIcon::AddRow => {
            let bold = Stroke::new(1.8_f32, color);
            painter.line_segment([egui::pos2(c.x - 5.0, c.y), egui::pos2(c.x + 5.0, c.y)], bold);
            painter.line_segment([egui::pos2(c.x, c.y - 5.0), egui::pos2(c.x, c.y + 5.0)], bold);
        }
        ToolbarIcon::EditRow => {
            // 斜向铅笔 + 底部基线
            let p1 = egui::pos2(c.x - 4.5, c.y + 4.5);
            let p2 = egui::pos2(c.x - 3.6, c.y + 1.2);
            let p3 = egui::pos2(c.x + 2.4, c.y - 4.8);
            let p4 = egui::pos2(c.x + 4.8, c.y - 2.4);
            let p5 = egui::pos2(c.x - 1.2, c.y + 3.6);
            painter.add(egui::Shape::closed_line(vec![p1, p2, p3, p4, p5], thin));
            painter.line_segment(
                [egui::pos2(c.x + 0.6, c.y - 3.0), egui::pos2(c.x + 3.0, c.y - 0.6)],
                thin,
            );
            painter.line_segment(
                [egui::pos2(c.x - 5.2, c.y + 5.5), egui::pos2(c.x + 5.2, c.y + 5.5)],
                thin,
            );
        }
        ToolbarIcon::DuplicateRow => {
            // 后方重叠页 + 前方带「+」卡片
            let back = egui::Rect::from_center_size(c - egui::vec2(1.8, 1.8), egui::vec2(8.0, 8.5));
            let front = egui::Rect::from_center_size(c + egui::vec2(1.8, 1.8), egui::vec2(8.0, 8.5));
            painter.add(egui::Shape::line(
                vec![
                    egui::pos2(back.left(), back.bottom() - 1.5),
                    egui::pos2(back.left(), back.top()),
                    egui::pos2(back.right() - 1.5, back.top()),
                ],
                thin,
            ));
            painter.rect_stroke(front, CornerRadius::same(1), thin, egui::StrokeKind::Inside);
            let fc = front.center();
            painter.line_segment([egui::pos2(fc.x - 2.0, fc.y), egui::pos2(fc.x + 2.0, fc.y)], thin);
            painter.line_segment([egui::pos2(fc.x, fc.y - 2.0), egui::pos2(fc.x, fc.y + 2.0)], thin);
        }
        ToolbarIcon::DeleteRow => {
            // 垃圾桶：盖子 + 提手 + 桶身 + 两条竖纹
            painter.line_segment([egui::pos2(c.x - 5.0, c.y - 3.5), egui::pos2(c.x + 5.0, c.y - 3.5)], s);
            painter.line_segment([egui::pos2(c.x - 2.0, c.y - 5.4), egui::pos2(c.x + 2.0, c.y - 5.4)], s);
            painter.line_segment([egui::pos2(c.x - 2.0, c.y - 5.4), egui::pos2(c.x - 2.0, c.y - 3.5)], thin);
            painter.line_segment([egui::pos2(c.x + 2.0, c.y - 5.4), egui::pos2(c.x + 2.0, c.y - 3.5)], thin);
            let body = egui::Rect::from_min_max(
                egui::pos2(c.x - 3.8, c.y - 3.2),
                egui::pos2(c.x + 3.8, c.y + 5.2),
            );
            painter.rect_stroke(body, CornerRadius::same(1), thin, egui::StrokeKind::Inside);
            painter.line_segment([egui::pos2(c.x - 1.4, c.y - 1.2), egui::pos2(c.x - 1.4, c.y + 3.2)], thin);
            painter.line_segment([egui::pos2(c.x + 1.4, c.y - 1.2), egui::pos2(c.x + 1.4, c.y + 3.2)], thin);
        }
        ToolbarIcon::Copy => {
            let back = egui::Rect::from_center_size(c - egui::vec2(1.8, 1.8), egui::vec2(8.0, 9.0));
            let front = egui::Rect::from_center_size(c + egui::vec2(1.8, 1.6), egui::vec2(8.0, 9.0));
            painter.add(egui::Shape::line(
                vec![
                    egui::pos2(back.left(), back.bottom() - 2.0),
                    egui::pos2(back.left(), back.top()),
                    egui::pos2(back.right() - 1.5, back.top()),
                ],
                s,
            ));
            painter.rect_stroke(front, CornerRadius::same(1), s, egui::StrokeKind::Inside);
        }
        ToolbarIcon::Export => {
            // 底部托盘 + 向上导出箭头
            painter.add(egui::Shape::line(
                vec![
                    egui::pos2(c.x - 5.2, c.y + 1.5),
                    egui::pos2(c.x - 5.2, c.y + 5.0),
                    egui::pos2(c.x + 5.2, c.y + 5.0),
                    egui::pos2(c.x + 5.2, c.y + 1.5),
                ],
                s,
            ));
            painter.line_segment([egui::pos2(c.x, c.y + 2.2), egui::pos2(c.x, c.y - 5.2)], s);
            painter.add(egui::Shape::line(
                vec![
                    egui::pos2(c.x - 3.2, c.y - 2.0),
                    egui::pos2(c.x, c.y - 5.2),
                    egui::pos2(c.x + 3.2, c.y - 2.0),
                ],
                s,
            ));
        }
        ToolbarIcon::Save => {
            // 经典软盘轮廓
            let pts = vec![
                egui::pos2(c.x - 5.0, c.y - 5.2),
                egui::pos2(c.x + 2.6, c.y - 5.2),
                egui::pos2(c.x + 5.0, c.y - 2.8),
                egui::pos2(c.x + 5.0, c.y + 5.2),
                egui::pos2(c.x - 5.0, c.y + 5.2),
            ];
            painter.add(egui::Shape::closed_line(pts, thin));
            let top_bay = egui::Rect::from_min_max(
                egui::pos2(c.x - 2.6, c.y - 5.0),
                egui::pos2(c.x + 1.6, c.y - 1.4),
            );
            painter.rect_stroke(top_bay, CornerRadius::same(0), thin, egui::StrokeKind::Inside);
            let bottom_bay = egui::Rect::from_min_max(
                egui::pos2(c.x - 3.0, c.y + 0.8),
                egui::pos2(c.x + 3.0, c.y + 5.0),
            );
            painter.rect_stroke(bottom_bay, CornerRadius::same(0), thin, egui::StrokeKind::Inside);
        }
        ToolbarIcon::Cancel => {
            let bold = Stroke::new(1.6_f32, color);
            painter.line_segment([egui::pos2(c.x - 4.0, c.y - 4.0), egui::pos2(c.x + 4.0, c.y + 4.0)], bold);
            painter.line_segment([egui::pos2(c.x + 4.0, c.y - 4.0), egui::pos2(c.x - 4.0, c.y + 4.0)], bold);
        }
        ToolbarIcon::Run => {
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(c.x - 3.4, c.y - 5.4),
                    egui::pos2(c.x + 5.4, c.y),
                    egui::pos2(c.x - 3.4, c.y + 5.4),
                ],
                color,
                Stroke::NONE,
            ));
        }
        ToolbarIcon::RunBatch => {
            let chevron = Stroke::new(1.8_f32, color);
            for offset in [-2.2_f32, 3.2] {
                painter.add(egui::Shape::line(
                    vec![
                        egui::pos2(c.x - 1.6 + offset, c.y - 4.4),
                        egui::pos2(c.x + 2.2 + offset, c.y),
                        egui::pos2(c.x - 1.6 + offset, c.y + 4.4),
                    ],
                    chevron,
                ));
            }
        }
        ToolbarIcon::Format => {
            for (dy, half_w) in [(-4.4, 4.6), (0.0, 3.4), (4.4, 5.2)] {
                painter.line_segment(
                    [
                        egui::pos2(c.x - 5.6, c.y + dy),
                        egui::pos2(c.x - 5.6 + half_w * 2.0, c.y + dy),
                    ],
                    s,
                );
            }
            let star = Stroke::new(1.1_f32, color);
            painter.add(egui::Shape::line(
                vec![
                    egui::pos2(c.x + 5.0, c.y - 5.6),
                    egui::pos2(c.x + 5.0, c.y - 2.4),
                ],
                star,
            ));
            painter.add(egui::Shape::line(
                vec![
                    egui::pos2(c.x + 3.7, c.y - 4.0),
                    egui::pos2(c.x + 6.3, c.y - 4.0),
                ],
                star,
            ));
        }
    }
}

/// 紧凑型工具栏矢量图标按钮（统一 26×24px，自带清晰中文悬停提示）。
pub fn icon_button(
    ui: &mut egui::Ui,
    icon: ToolbarIcon,
    enabled: bool,
    tooltip: &str,
) -> egui::Response {
    icon_button_tinted(ui, icon, enabled, None, false, None, tooltip)
}

/// 支持语义色、主按钮样式及小型角标的紧凑型图标按钮。
pub fn icon_button_tinted(
    ui: &mut egui::Ui,
    icon: ToolbarIcon,
    enabled: bool,
    tint: Option<Color32>,
    primary: bool,
    badge: Option<&str>,
    tooltip: &str,
) -> egui::Response {
    let p = palette();
    let width = if let Some(b) = badge {
        26.0 + (b.chars().count() as f32 * 7.0) + 4.0
    } else {
        26.0
    };
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 28.0), sense);
    let hover_t = ui.ctx().animate_bool_with_time(
        response.id.with("icon-button-hover"),
        enabled && response.hovered(),
        0.14,
    );
    if ui.is_rect_visible(rect) {
        let active = enabled && response.is_pointer_button_down_on();
        let (fill, stroke_col, icon_col) = if !enabled {
            (p.panel, p.border_subtle, p.subtle)
        } else if primary {
            let fill = if active {
                p.accent.gamma_multiply(0.88)
            } else {
                mix(p.accent, p.accent.gamma_multiply(0.86), hover_t)
            };
            (fill, fill, p.on_accent)
        } else {
            let base = tint.unwrap_or(p.muted);
            let hover_target = tint.unwrap_or(p.ink);
            let fg = mix(base, hover_target, hover_t);
            let fill = if active {
                p.accent_soft
            } else {
                mix(Color32::TRANSPARENT, p.accent_tint, hover_t)
            };
            let border_col = if active {
                p.accent_soft
            } else {
                Color32::TRANSPARENT
            };
            (fill, border_col, fg)
        };
        let painter = ui.painter();
        painter.rect(
            rect,
            CornerRadius::same(7),
            fill,
            Stroke::new(1.0_f32, stroke_col),
            egui::StrokeKind::Inside,
        );
        let icon_rect = egui::Rect::from_min_size(rect.min, egui::vec2(26.0, rect.height()));
        paint_toolbar_icon(painter, icon_rect, icon, icon_col);
        if let Some(b) = badge {
            painter.text(
                egui::pos2(rect.left() + 24.0, rect.center().y),
                egui::Align2::LEFT_CENTER,
                b,
                FontId::proportional(11.5),
                icon_col,
            );
        }
    }
    response.on_hover_text(tooltip)
}

/// 紧凑型图标下拉菜单按钮（用于「复制」「导出」等弹出菜单，自带右下微三角标识）。
pub fn icon_menu_button(
    ui: &mut egui::Ui,
    icon: ToolbarIcon,
    enabled: bool,
    tooltip: &str,
    menu: impl FnOnce(&mut egui::Ui),
) {
    let p = palette();
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(32.0, 28.0), sense);
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&response));
    if ui.is_rect_visible(rect) {
        let hovered = enabled && response.hovered();
        let (fill, border_col, fg) = if !enabled {
            (p.panel, p.border_subtle, p.subtle)
        } else if open {
            (p.accent_soft, p.accent, p.accent)
        } else if hovered {
            (p.accent_tint, Color32::TRANSPARENT, p.ink)
        } else {
            (Color32::TRANSPARENT, Color32::TRANSPARENT, p.muted)
        };
        let painter = ui.painter();
        painter.rect(
            rect,
            CornerRadius::same(7),
            fill,
            Stroke::new(1.0_f32, border_col),
            egui::StrokeKind::Inside,
        );
        let icon_rect = egui::Rect::from_min_size(
            egui::pos2(rect.left() + 2.0, rect.top()),
            egui::vec2(20.0, rect.height()),
        );
        paint_toolbar_icon(painter, icon_rect, icon, fg);
        // 右侧小下拉箭头
        let cx = rect.right() - 6.0;
        let cy = rect.center().y + 0.5;
        painter.add(egui::Shape::line(
            vec![
                egui::pos2(cx - 2.5, cy - 1.2),
                egui::pos2(cx, cy + 1.5),
                egui::pos2(cx + 2.5, cy - 1.2),
            ],
            Stroke::new(1.2_f32, if enabled { p.muted } else { p.subtle }),
        ));
    }
    let response = response.on_hover_text(tooltip);
    if enabled {
        egui::Popup::menu(&response).show(menu);
    }
}

/// 绘制顶部栏一键切换浅色与深色主题的按钮。
pub fn theme_toggle_button(ui: &mut egui::Ui) -> egui::Response {
    let dark = is_dark();
    let p = palette();
    let desired = egui::vec2(30.0, 30.0);
    let (rect, response) = ui.allocate_exact_size(desired, egui::Sense::click());
    let hover_t = ui.ctx().animate_bool_with_time(
        response.id.with("theme-toggle-hover"),
        response.hovered(),
        0.16,
    );
    if ui.is_rect_visible(rect) {
        let fill = mix(Color32::TRANSPARENT, p.accent_tint, hover_t);
        let stroke = Stroke::new(1.0_f32, mix(Color32::TRANSPARENT, p.border, hover_t));
        let painter = ui.painter();
        painter.rect(
            rect,
            CornerRadius::same(8),
            fill,
            stroke,
            egui::StrokeKind::Inside,
        );
        let icon_center = rect.center();
        if dark {
            let sun = Color32::from_rgb(251, 191, 36);
            painter.circle_filled(icon_center, 3.6, sun);
            for step in 0..8 {
                let angle = step as f32 * std::f32::consts::FRAC_PI_4;
                let dir = egui::vec2(angle.cos(), angle.sin());
                painter.line_segment(
                    [icon_center + dir * 5.4, icon_center + dir * 7.0],
                    Stroke::new(1.3_f32, sun),
                );
            }
        } else {
            let moon = mix(p.muted, p.ink, hover_t);
            painter.circle_filled(icon_center, 5.0, moon);
            painter.circle_filled(
                icon_center + egui::vec2(2.5, -1.8),
                4.0,
                mix(p.surface, p.accent_tint, hover_t),
            );
        }
    }
    response.on_hover_text(if dark {
        "切换为浅色主题"
    } else {
        "切换为深色主题"
    })
}

/// 现代 IDE 风格编辑器标签页（与下方工具栏无缝衔接，内置类型色点与关闭按钮）。
/// 返回 `(标签主体 Response, 是否点击关闭按钮)`。
pub fn editor_tab(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash,
    label: &str,
    selected: bool,
    _accent_color: Color32,
    closable: bool,
    close_tooltip: &str,
) -> (egui::Response, bool) {
    let p = palette();
    let font = FontId::proportional(13.5);
    let text_color = if selected { p.ink } else { p.muted };
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font, text_color);
    let text_w = galley.size().x;
    let left_pad = 12.0;
    let close_space = if closable { 24.0 } else { 12.0 };
    let width = (left_pad + text_w + close_space).max(74.0);
    let height = 32.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let visual_rect = if selected {
        rect
    } else {
        egui::Rect::from_min_max(egui::pos2(rect.left(), rect.top() + 2.0), rect.max)
    };
    let close_rect = egui::Rect::from_center_size(
        egui::pos2(visual_rect.right() - 12.0, visual_rect.center().y),
        egui::vec2(16.0, 16.0),
    );
    let tab_id = ui.make_persistent_id(("editor-tab", &id_salt));
    let close_id = ui.make_persistent_id(("editor-tab-close", &id_salt));

    let close_resp = closable.then(|| ui.interact(close_rect, close_id, egui::Sense::click()));
    let body_rect = if closable {
        egui::Rect::from_min_max(
            visual_rect.min,
            egui::pos2(close_rect.left() - 2.0, visual_rect.bottom()),
        )
    } else {
        visual_rect
    };
    let response = ui.interact(body_rect, tab_id, egui::Sense::click());
    let hovered = response.hovered() || close_resp.as_ref().is_some_and(egui::Response::hovered);
    let close_hovered = close_resp.as_ref().is_some_and(egui::Response::hovered);

    let hover_t = ui.ctx().animate_bool_with_time(
        ui.make_persistent_id(("editor-tab-hover", &id_salt)),
        hovered && !selected,
        0.16,
    );
    if ui.is_rect_visible(visual_rect) {
        let painter = ui.painter();
        let radius = CornerRadius {
            nw: 8,
            ne: 8,
            sw: 0,
            se: 0,
        };
        let fill = if selected {
            p.surface
        } else {
            mix(Color32::TRANSPARENT, p.surface, hover_t)
        };
        painter.rect(
            visual_rect,
            radius,
            fill,
            Stroke::NONE,
            egui::StrokeKind::Inside,
        );
        if selected {
            let bottom_bar = egui::Rect::from_min_max(
                egui::pos2(visual_rect.left() + 8.0, visual_rect.bottom() - 2.0),
                egui::pos2(visual_rect.right() - 8.0, visual_rect.bottom()),
            );
            painter.rect_filled(bottom_bar, CornerRadius::same(1), p.accent);
        }
        let text_pos = egui::pos2(
            visual_rect.left() + left_pad,
            visual_rect.center().y - galley.size().y * 0.5,
        );
        let final_color = if selected || hovered { p.ink } else { p.muted };
        let final_galley = ui.painter().layout_no_wrap(
            label.to_owned(),
            FontId::proportional(13.5),
            final_color,
        );
        painter.galley(text_pos, final_galley, final_color);
        // 内置关闭图标（选中态常显，未选中态悬停显现）
        if closable && (selected || hovered) {
            if close_hovered {
                painter.rect_filled(close_rect, CornerRadius::same(4), p.error_soft);
            }
            let x_col = if close_hovered { p.error } else { p.muted };
            let c = close_rect.center();
            let x_stroke = Stroke::new(1.3_f32, x_col);
            painter.line_segment(
                [egui::pos2(c.x - 3.0, c.y - 3.0), egui::pos2(c.x + 3.0, c.y + 3.0)],
                x_stroke,
            );
            painter.line_segment(
                [egui::pos2(c.x + 3.0, c.y - 3.0), egui::pos2(c.x - 3.0, c.y + 3.0)],
                x_stroke,
            );
        }
    }
    let close_clicked = close_resp
        .map(|r| r.on_hover_text(close_tooltip).clicked())
        .unwrap_or(false);
    (response, close_clicked)
}

/// 结果/设置面板的分段页签：激活态使用柔和主题底色，避免整块白底与重复描边。
pub fn tab_button(
    ui: &mut egui::Ui,
    selected: bool,
    text: impl Into<egui::WidgetText>,
    _bottom_indicator: bool,
) -> egui::Response {
    let p = palette();
    ui.scope(|ui| {
        let widgets = &mut ui.visuals_mut().widgets;
        widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
        widgets.inactive.bg_fill = Color32::TRANSPARENT;
        widgets.inactive.bg_stroke = Stroke::NONE;
        widgets.hovered.weak_bg_fill = p.accent_tint;
        widgets.hovered.bg_fill = p.accent_tint;
        widgets.hovered.bg_stroke = Stroke::NONE;
        let mut button = egui::Button::new(text)
            .corner_radius(CornerRadius::same(7))
            .min_size(egui::vec2(0.0, 28.0));
        if selected {
            button = button
                .fill(p.accent_soft)
                .stroke(Stroke::new(1.0_f32, mix(p.border, p.accent, 0.22)));
        }
        ui.add(button)
    })
    .inner
}

pub fn apply(ctx: &egui::Context) {
    let dark = is_dark();
    let p = palette();
    ctx.set_theme(if dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    });
    let mut visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    visuals.panel_fill = p.panel;
    visuals.window_fill = p.surface;
    visuals.window_stroke = Stroke::new(1.0_f32, p.border);
    visuals.window_corner_radius = CornerRadius::same(14);
    visuals.window_shadow = egui::epaint::Shadow {
        offset: [0, 12],
        blur: 32,
        spread: 0,
        color: Color32::from_black_alpha(p.shadow_alpha),
    };
    visuals.menu_corner_radius = CornerRadius::same(10);
    visuals.popup_shadow = egui::epaint::Shadow {
        offset: [0, 6],
        blur: 20,
        spread: 0,
        color: Color32::from_black_alpha(p.shadow_alpha),
    };
    visuals.extreme_bg_color = p.surface;
    visuals.faint_bg_color = p.stripe;
    visuals.code_bg_color = p.panel;
    visuals.hyperlink_color = p.accent;
    visuals.override_text_color = None;
    visuals.weak_text_color = Some(p.muted);
    visuals.warn_fg_color = p.warning;
    visuals.error_fg_color = p.error;
    visuals.selection.bg_fill = p.accent_soft;
    visuals.selection.stroke = Stroke::new(1.0_f32, p.accent);
    visuals.striped = true;
    visuals.text_cursor.stroke = Stroke::new(1.8_f32, p.accent);

    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_fill = p.panel;
    widgets.noninteractive.weak_bg_fill = p.panel;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.border_subtle);
    widgets.noninteractive.fg_stroke.color = p.ink;

    widgets.inactive.bg_fill = p.surface;
    widgets.inactive.weak_bg_fill = p.surface;
    widgets.inactive.bg_stroke = Stroke::new(1.0_f32, p.border);
    widgets.inactive.fg_stroke.color = p.ink;

    widgets.hovered.bg_fill = p.accent_tint;
    widgets.hovered.weak_bg_fill = p.accent_tint;
    widgets.hovered.bg_stroke = Stroke::new(1.0_f32, p.border_strong);
    widgets.hovered.fg_stroke.color = p.ink;
    widgets.hovered.expansion = 0.0;

    widgets.active.weak_bg_fill = p.accent_soft;
    widgets.active.bg_fill = p.accent_soft;
    widgets.active.bg_stroke = Stroke::new(1.2_f32, p.accent);
    widgets.active.fg_stroke.color = p.accent;
    widgets.active.expansion = 0.0;

    widgets.open.bg_fill = p.accent_soft;
    widgets.open.weak_bg_fill = p.accent_soft;
    widgets.open.bg_stroke = Stroke::new(1.0_f32, p.accent);
    widgets.open.fg_stroke.color = p.ink;

    // 4px 圆角确保复选框呈现利落的圆角方形（避免 7px 把 14px 复选框变成圆圈）
    for widget in [
        &mut widgets.noninteractive,
        &mut widgets.inactive,
        &mut widgets.hovered,
        &mut widgets.active,
        &mut widgets.open,
    ] {
        widget.corner_radius = CornerRadius::same(7);
    }
    ctx.set_visuals(visuals);
    ctx.style_mut(|style| {
        style.animation_time = 0.16;
        style.spacing.item_spacing = egui::vec2(8.0, 7.0);
        style.spacing.button_padding = egui::vec2(11.0, 5.0);
        style.spacing.interact_size.y = 28.0;
        style.spacing.window_margin = egui::Margin::same(18);
        style.spacing.menu_margin = egui::Margin::same(8);
        style.spacing.indent = 16.0;
        style.spacing.scroll = egui::style::ScrollStyle::thin();
        for (role, size) in [
            (TextStyle::Small, 12.0),
            (TextStyle::Body, 13.5),
            (TextStyle::Button, 13.5),
            (TextStyle::Heading, 19.0),
        ] {
            style.text_styles.insert(role, FontId::proportional(size));
        }
        style
            .text_styles
            .insert(TextStyle::Monospace, FontId::monospace(13.5));
    });
}

fn base_fonts() -> &'static FontDefinitions {
    static FONTS: OnceLock<FontDefinitions> = OnceLock::new();
    FONTS.get_or_init(|| {
        #[cfg(windows)]
        let mut font_candidates = std::env::var_os("WINDIR")
            .or_else(|| std::env::var_os("SystemRoot"))
            .map(|dir| std::path::PathBuf::from(dir).join("Fonts"))
            .into_iter()
            .flat_map(|dir| {
                ["msyh.ttc", "deng.ttf", "simhei.ttf", "simsun.ttc"].map(|file| dir.join(file))
            });
        #[cfg(target_os = "macos")]
        let mut font_candidates = [
            "/System/Library/Fonts/PingFang.ttc",
            // Recent macOS versions store system PingFang in a private outline format.
            "/System/Library/Fonts/Hiragino Sans GB.ttc",
            "/System/Library/Fonts/STHeiti Medium.ttc",
            "/System/Library/Fonts/STHeiti Light.ttc",
        ]
        .into_iter()
        .map(std::path::PathBuf::from);
        #[cfg(not(any(windows, target_os = "macos")))]
        let mut font_candidates = std::iter::empty::<std::path::PathBuf>();

        let mut fonts = FontDefinitions::default();
        #[cfg(windows)]
        let ui_fonts = std::env::var_os("WINDIR")
            .or_else(|| std::env::var_os("SystemRoot"))
            .map(|dir| std::path::PathBuf::from(dir).join("Fonts"))
            .into_iter()
            .flat_map(|dir| ["SegUIVar.ttf", "segoeui.ttf"].map(|file| dir.join(file)));
        #[cfg(target_os = "macos")]
        let ui_fonts = [std::path::PathBuf::from("/System/Library/Fonts/SFNS.ttf")].into_iter();
        #[cfg(not(any(windows, target_os = "macos")))]
        let ui_fonts = std::iter::empty::<std::path::PathBuf>();
        if let Some(bytes) = ui_fonts.into_iter().find_map(|path| {
            let bytes = std::fs::read(path).ok()?;
            ttf_parser::Face::parse(&bytes, 0).ok()?;
            Some(bytes)
        }) {
            let name = "System UI".to_owned();
            fonts
                .font_data
                .insert(name.clone(), FontData::from_owned(bytes).into());
            fonts
                .families
                .entry(FontFamily::Proportional)
                .or_default()
                .insert(0, name);
        }
        // 优先加载现代等宽编程字体，显著提升 SQL 编辑器与数据表格清晰度
        #[cfg(windows)]
        let mono_fonts = std::env::var_os("WINDIR")
            .or_else(|| std::env::var_os("SystemRoot"))
            .map(|dir| std::path::PathBuf::from(dir).join("Fonts"))
            .into_iter()
            .flat_map(|dir| {
                ["CascadiaMono.ttf", "CascadiaCode.ttf", "consola.ttf"].map(|file| dir.join(file))
            });
        #[cfg(target_os = "macos")]
        let mono_fonts = [
            "/System/Library/Fonts/SFNSMono.ttf",
            "/System/Library/Fonts/Menlo.ttc",
        ]
        .into_iter()
        .map(std::path::PathBuf::from);
        #[cfg(not(any(windows, target_os = "macos")))]
        let mono_fonts = std::iter::empty::<std::path::PathBuf>();
        if let Some(bytes) = mono_fonts.into_iter().find_map(|path| {
            let bytes = std::fs::read(path).ok()?;
            ttf_parser::Face::parse(&bytes, 0).ok()?;
            Some(bytes)
        }) {
            let name = "System Mono".to_owned();
            fonts
                .font_data
                .insert(name.clone(), FontData::from_owned(bytes).into());
            fonts
                .families
                .entry(FontFamily::Monospace)
                .or_default()
                .insert(0, name);
        }
        if let Some(bytes) = font_candidates.find_map(|path| {
            let bytes = std::fs::read(path).ok()?;
            ttf_parser::Face::parse(&bytes, 0).ok()?;
            Some(bytes)
        }) {
            let name = "System CJK".to_owned();
            fonts
                .font_data
                .insert(name.clone(), FontData::from_owned(bytes).into());
            for family in [FontFamily::Proportional, FontFamily::Monospace] {
                fonts.families.entry(family).or_default().push(name.clone());
            }
        }
        // The preference dialog may queue a font change at the end of a frame.
        // Reserve these families in the live atlas before any selection uses them.
        let fallback = fonts
            .families
            .get(&FontFamily::Monospace)
            .cloned()
            .unwrap_or_default();
        for role in FontRole::ALL {
            fonts
                .families
                .insert(FontFamily::Name(role.font_name().into()), fallback.clone());
        }
        fonts
    })
}

pub(super) fn apply_font_preferences(
    ctx: &egui::Context,
    preferences: &mut Preferences,
) -> Vec<String> {
    let mut definitions = base_fonts().clone();
    let mut errors = Vec::new();
    for (role, font, face) in [
        (
            FontRole::Editor,
            &mut preferences.editor.font,
            &preferences.fonts.editor,
        ),
        (
            FontRole::History,
            &mut preferences.editor.history_font,
            &preferences.fonts.history,
        ),
        (
            FontRole::Blob,
            &mut preferences.editor.blob_font,
            &preferences.fonts.blob,
        ),
        (
            FontRole::Grid,
            &mut preferences.editor.grid_font,
            &preferences.fonts.grid,
        ),
    ] {
        font.system_role = None;
        let Some(face) = face else {
            continue;
        };
        let data = match face.data() {
            Ok(data) => data,
            Err(error) => {
                errors.push(format!(
                    "{}字体「{}」加载失败，已使用内置字体。{error}",
                    role.label(),
                    face.label()
                ));
                continue;
            }
        };
        let name = role.font_name().to_owned();
        let mut family = definitions
            .families
            .get(&font.builtin_family())
            .cloned()
            .unwrap_or_default();
        family.insert(0, name.clone());
        definitions.font_data.insert(name.clone(), data.into());
        definitions
            .families
            .insert(FontFamily::Name(name.into()), family);
        font.system_role = Some(role);
    }
    ctx.set_fonts(definitions);
    ctx.style_mut(|style| {
        style
            .text_styles
            .insert(grid_font_style(), preferences.editor.grid_font.id());
    });
    errors
}

fn grid_font_style() -> TextStyle {
    TextStyle::Name(FontRole::Grid.font_name().into())
}

pub(super) fn grid_font(ui: &egui::Ui) -> Option<FontId> {
    ui.style().text_styles.get(&grid_font_style()).cloned()
}

pub(super) fn apply_grid_font(ui: &mut egui::Ui) {
    if let Some(font) = grid_font(ui) {
        ui.style_mut().override_font_id = Some(font);
    }
}

pub(super) fn grid_row_height(ui: &mut egui::Ui, default: f32) -> f32 {
    grid_font(ui).map_or(default, |font| {
        default.max(ui.fonts_mut(|fonts| fonts.row_height(&font)) + 6.0)
    })
}

pub(super) const KEYWORDS: &[&str] = &[
    "ADD",
    "ALTER",
    "ANALYZE",
    "AND",
    "AS",
    "ASC",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CALL",
    "CASE",
    "CHECK",
    "COMMIT",
    "CREATE",
    "DATABASE",
    "DECLARE",
    "DEFAULT",
    "DELETE",
    "DELIMITER",
    "DESC",
    "DESCRIBE",
    "DISTINCT",
    "DROP",
    "ELSE",
    "END",
    "EVENT",
    "EXISTS",
    "EXPLAIN",
    "FALSE",
    "FOREIGN",
    "FROM",
    "FUNCTION",
    "GRANT",
    "GROUP",
    "HAVING",
    "IF",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NOT",
    "NULL",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "PRIMARY",
    "PROCEDURE",
    "REFERENCES",
    "REPLACE",
    "REVOKE",
    "RIGHT",
    "ROLLBACK",
    "SELECT",
    "SET",
    "SHOW",
    "TABLE",
    "THEN",
    "TRIGGER",
    "TRUE",
    "TRUNCATE",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USE",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WITH",
];

pub fn highlight(sql: &str, width: f32) -> LayoutJob {
    sql_layout_job(sql, width, EditorSettings::default(), (false, false))
}

pub(super) fn sql_galley(
    ui: &egui::Ui,
    sql: &str,
    width: f32,
    settings: EditorSettings,
    quoting: (bool, bool),
) -> Arc<Galley> {
    let mut job = sql_layout_job(sql, width, settings, quoting);
    ui.fonts_mut(|fonts| {
        if job.text.contains('\t') {
            layout_tabs(fonts, &mut job, &settings.font.id(), settings.tab_width);
        }
        fonts.layout_job(job)
    })
}

pub(super) fn sql_file_text(
    sql: &str,
    settings: EditorSettings,
    quoting: (bool, bool),
) -> Cow<'_, str> {
    if settings.changes_case() {
        Cow::Owned(sql_layout_job(sql, f32::INFINITY, settings, quoting).text)
    } else {
        Cow::Borrowed(sql)
    }
}

pub(super) fn apply_case_to_clipboard(
    ui: &egui::Ui,
    first_command: usize,
    sql: &str,
    selection: Range<usize>,
    settings: EditorSettings,
    quoting: (bool, bool),
) {
    let selected = &sql[selection.clone()];
    ui.output_mut(|output| {
        let mut converted = None;
        for command in &mut output.commands[first_command..] {
            if let egui::OutputCommand::CopyText(text) = command
                && text == selected
            {
                let changed = converted.get_or_insert_with(|| {
                    sql_file_text(sql, settings, quoting)[selection.clone()].to_owned()
                });
                text.clone_from(changed);
            }
        }
    });
}

fn resolved_sql_colors(colors: SqlColors) -> SqlColors {
    if is_dark() && colors == SqlColors::default() {
        SqlColors {
            normal: [226, 234, 246],
            keyword: [96, 165, 250],
            function: [45, 212, 191],
            string: [251, 168, 110],
            comment: [108, 126, 152],
        }
    } else {
        colors
    }
}

fn sql_layout_job(
    sql: &str,
    width: f32,
    settings: EditorSettings,
    (no_backslash_escapes, ansi_quotes): (bool, bool),
) -> LayoutJob {
    let colors = resolved_sql_colors(settings.colors);
    let normal = rgb(colors.normal);
    let number_color = palette().number;
    let font = settings.font.id();
    let mut job = LayoutJob {
        text: sql.to_owned(),
        ..LayoutJob::default()
    };
    if settings.word_wrap {
        job.wrap.max_width = width;
    }
    let mut cursor = 0;
    let mut previous_is_prefix = false;
    let mut expressions = vec![false];
    let mut tokens = editor_tokens(sql, no_backslash_escapes, ansi_quotes).peekable();
    while let Some(token) = tokens.next() {
        section(&mut job, cursor..token.range.start, &font, normal);
        let next = tokens.peek().map(|token| token.kind);
        let unqualified = !previous_is_prefix && next != Some(EditorTokenKind::Symbol('.'));
        let (color, case) = match token.kind {
            EditorTokenKind::Protected { .. } => {
                let color = if token.text.starts_with(['\'', '"']) {
                    colors.string
                } else {
                    colors.comment
                };
                (rgb(color), LetterCase::AsTyped)
            }
            EditorTokenKind::Word if token.text.bytes().all(|byte| byte.is_ascii_digit()) => {
                (number_color, LetterCase::AsTyped)
            }
            EditorTokenKind::Word
                if unqualified
                    && next == Some(EditorTokenKind::Symbol('('))
                    && expressions.last() == Some(&true)
                    && vocabulary::parameters(token.text).is_some() =>
            {
                (rgb(colors.function), settings.function_case)
            }
            EditorTokenKind::Word if unqualified && vocabulary::is_keyword(token.text) => {
                let case = if is_reserved_keyword(token.text) {
                    settings.keyword_case
                } else {
                    LetterCase::AsTyped
                };
                (rgb(colors.keyword), case)
            }
            _ => (normal, LetterCase::AsTyped),
        };
        // Draw case changes on the same byte and character boundaries as the source text.
        match case {
            LetterCase::AsTyped => {}
            LetterCase::Upper => job.text[token.range.clone()].make_ascii_uppercase(),
            LetterCase::Lower => job.text[token.range.clone()].make_ascii_lowercase(),
        }
        cursor = token.range.end;
        section(&mut job, token.range, &font, color);
        if matches!(token.kind, EditorTokenKind::Protected { .. })
            && !token.text.starts_with(['\'', '"'])
        {
            continue;
        }
        previous_is_prefix = matches!(token.kind, EditorTokenKind::Symbol('.' | '@' | ':'));
        match token.kind {
            EditorTokenKind::Symbol('(') => {
                expressions.push(expressions.last() == Some(&true));
            }
            EditorTokenKind::Symbol(')') if expressions.len() > 1 => {
                expressions.pop();
            }
            EditorTokenKind::Symbol(';') => {
                expressions.truncate(1);
                expressions[0] = false;
            }
            EditorTokenKind::Word if unqualified => {
                let expression = expressions
                    .last_mut()
                    .expect("at least the outer statement");
                if sql_word_in(
                    token.text,
                    &[
                        "SELECT", "WHERE", "HAVING", "ON", "GROUP", "ORDER", "VALUES", "SET",
                        "RETURN", "THEN",
                    ],
                ) {
                    *expression = true;
                } else if sql_word_in(token.text, &["FROM", "JOIN", "INTO", "USE"]) {
                    *expression = false;
                }
            }
            _ => {}
        }
    }
    section(&mut job, cursor..sql.len(), &font, normal);
    job
}

fn sql_word_in(word: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| word.eq_ignore_ascii_case(candidate))
}

fn is_reserved_keyword(word: &str) -> bool {
    // Nonreserved vocabulary may spell a table or column name. Only fold common
    // reserved words whose use as identifiers requires quoting in both dialects.
    sql_word_in(
        word,
        &[
            "ADD",
            "ALL",
            "ALTER",
            "ANALYZE",
            "AND",
            "AS",
            "ASC",
            "BETWEEN",
            "CASE",
            "CHECK",
            "CREATE",
            "CROSS",
            "DECLARE",
            "DEFAULT",
            "DELETE",
            "DESC",
            "DESCRIBE",
            "DISTINCT",
            "DROP",
            "ELSE",
            "EXISTS",
            "EXPLAIN",
            "FALSE",
            "FOR",
            "FOREIGN",
            "FROM",
            "GRANT",
            "GROUP",
            "HAVING",
            "IF",
            "IN",
            "INDEX",
            "INNER",
            "INSERT",
            "INTO",
            "IS",
            "JOIN",
            "KEY",
            "LEFT",
            "LIKE",
            "LIMIT",
            "NOT",
            "NULL",
            "ON",
            "OR",
            "ORDER",
            "OUTER",
            "PRIMARY",
            "REFERENCES",
            "REPLACE",
            "REVOKE",
            "RIGHT",
            "SELECT",
            "SET",
            "SHOW",
            "TABLE",
            "THEN",
            "TRIGGER",
            "TRUE",
            "UNION",
            "UNIQUE",
            "UPDATE",
            "USE",
            "USING",
            "VALUES",
            "WHEN",
            "WHERE",
        ],
    )
}

fn rgb([r, g, b]: [u8; 3]) -> Color32 {
    Color32::from_rgb(r, g, b)
}

fn section(job: &mut LayoutJob, range: Range<usize>, font: &FontId, color: Color32) {
    if range.is_empty() {
        return;
    }
    if let Some(previous) = job.sections.last_mut()
        && previous.format.color == color
    {
        previous.byte_range.end = range.end;
    } else {
        job.sections.push(LayoutSection {
            leading_space: 0.0,
            byte_range: range,
            format: TextFormat::simple(font.clone(), color),
        });
    }
}

fn tab_advance(x: f32, stop: f32) -> f32 {
    ((x / stop + 0.0001).floor() + 1.0) * stop - x
}

pub(super) fn spaces_to_tab_stop(ui: &egui::Ui, prefix: &str, settings: EditorSettings) -> usize {
    let font = settings.font.id();
    ui.fonts_mut(|fonts| {
        let space = fonts.glyph_width(&font, ' ');
        let tab = space * f32::from(settings.tab_width);
        if tab <= 0.0 {
            return usize::from(settings.tab_width);
        }
        let mut x = 0.0;
        for ch in prefix.chars() {
            x += if ch == '\t' {
                tab_advance(x, tab)
            } else {
                fonts.glyph_width(&font, ch)
            };
        }
        (tab_advance(x, tab) / space)
            .round()
            .clamp(1.0, f32::from(settings.tab_width)) as usize
    })
}

fn layout_tabs(
    fonts: &mut egui::epaint::text::FontsView<'_>,
    job: &mut LayoutJob,
    font: &FontId,
    width: u8,
) {
    let tab = fonts.glyph_width(font, ' ') * f32::from(width);
    let native_tab = fonts.glyph_width(font, '\t');
    if tab <= 0.0 || native_tab <= 0.0 {
        return;
    }
    let height = fonts.row_height(font);
    let mut layouts = Vec::new();
    let mut line_start = 0;
    for line in job.text.split_inclusive('\n') {
        if let Some(last_tab) = line.rfind('\t') {
            let mut x = 0.0;
            for (offset, ch) in line[..=last_tab].char_indices() {
                if ch == '\t' {
                    // Size only the invisible glyph, preserving one cursor position per source
                    // character and preserving the row height of the regular editor font.
                    let mut tab_font = font.clone();
                    tab_font.size *= tab_advance(x, tab) / native_tab;
                    x += fonts.glyph_width(&tab_font, '\t');
                    layouts.push((line_start + offset, tab_font));
                } else {
                    x += fonts.glyph_width(font, ch);
                }
            }
        }
        line_start += line.len();
    }

    let mut tabs = layouts.into_iter().peekable();
    for mut section in std::mem::take(&mut job.sections) {
        let end = section.byte_range.end;
        while let Some(&(offset, _)) = tabs.peek().filter(|(offset, _)| *offset < end) {
            let (_, tab_font) = tabs.next().expect("peeked tab");
            if section.byte_range.start < offset {
                job.sections.push(LayoutSection {
                    byte_range: section.byte_range.start..offset,
                    ..section.clone()
                });
            }
            job.sections.push(LayoutSection {
                byte_range: offset..offset + 1,
                format: TextFormat {
                    font_id: tab_font,
                    line_height: Some(height),
                    ..section.format.clone()
                },
                ..section.clone()
            });
            section.byte_range.start = offset + 1;
        }
        if !section.byte_range.is_empty() {
            job.sections.push(section);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_and_saves_sql_with_the_same_safe_case_as_the_editor() {
        let original = "select CONCAT('原文 from', t.order) from t;";
        let shown = "SELECT concat('原文 from', t.order) FROM t;";
        let mut sql = original.to_owned();
        let id = egui::Id::new("editor-case");
        let settings = EditorSettings {
            keyword_case: LetterCase::Upper,
            function_case: LetterCase::Lower,
            ..EditorSettings::default()
        };
        let ctx = egui::Context::default();
        let mut initialize = true;
        let mut draw = |ctx: &egui::Context| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let mut layout = |ui: &egui::Ui, text: &dyn egui::TextBuffer, width| {
                    sql_galley(ui, text.as_str(), width, settings, (false, false))
                };
                let first = ui.output(|output| output.commands.len());
                let mut output = egui::TextEdit::multiline(&mut sql)
                    .id(id)
                    .layouter(&mut layout)
                    .show(ui);
                assert_eq!(output.galley.text(), shown);
                let color_at = |index| {
                    output
                        .galley
                        .job
                        .sections
                        .iter()
                        .find(|section| section.byte_range.contains(&index))
                        .unwrap()
                        .format
                        .color
                };
                assert_eq!(color_at(0), rgb(settings.colors.keyword));
                assert_eq!(color_at(7), rgb(settings.colors.function));
                assert_eq!(color_at(14), rgb(settings.colors.string));
                apply_case_to_clipboard(ui, first, &sql, 0..sql.len(), settings, (false, false));
                if initialize {
                    let range = super::super::editor_tools::cursors(&sql, 0..sql.len());
                    output.state.cursor.set_char_range(Some(range));
                    output.state.store(ctx, id);
                    output.response.request_focus();
                    initialize = false;
                }
            });
        };
        let _ = ctx.run(egui::RawInput::default(), &mut draw);
        let output = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Copy],
                ..egui::RawInput::default()
            },
            draw,
        );
        assert!(output.platform_output.commands.iter().any(|command| {
            matches!(command, egui::OutputCommand::CopyText(copied) if copied == shown)
        }));
        assert_eq!(sql, original);
        assert_eq!(
            sql_file_text(
                "create table CoNcAt (data int); select CONCAT(data) from CoNcAt;",
                settings,
                (false, false),
            ),
            "CREATE TABLE CoNcAt (data int); SELECT concat(data) FROM CoNcAt;"
        );
    }

    #[test]
    fn aligns_tabs_to_the_custom_stop_and_only_wraps_when_enabled() {
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let settings = EditorSettings {
                    tab_width: 6,
                    word_wrap: false,
                    ..EditorSettings::default()
                };
                let tabbed = sql_galley(ui, "a\tb", 100.0, settings, (false, false));
                let spaces = sql_galley(ui, "a     b", 100.0, settings, (false, false));
                assert_eq!(tabbed.text(), "a\tb");
                let x = |galley: &Galley, character| {
                    galley
                        .pos_from_cursor(egui::text::CCursor::new(character))
                        .left()
                };
                assert!((x(&tabbed, 2) - x(&spaces, 6)).abs() <= 1.0);
                assert_eq!(tabbed.rows[0].height(), spaces.rows[0].height());

                let long = "SELECT title FROM books WHERE visible = 1;";
                assert_eq!(
                    sql_galley(ui, long, 110.0, settings, (false, false))
                        .rows
                        .len(),
                    1
                );
                let wrapping = EditorSettings {
                    word_wrap: true,
                    ..settings
                };
                assert!(
                    sql_galley(ui, long, 110.0, wrapping, (false, false))
                        .rows
                        .len()
                        > 1
                );
            });
        });
    }
}
