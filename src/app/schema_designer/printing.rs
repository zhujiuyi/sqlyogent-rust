use eframe::egui::{Rect, pos2, vec2};

use super::{PAGE_HEIGHT, PAGE_WIDTH};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
use macos::print as system_print;
#[cfg(windows)]
use windows::print as system_print;

const MAX_PAGES: usize = 64;

pub(super) struct PrintPages {
    pub(super) viewports: Vec<Rect>,
    pub(super) shrunk: bool,
}

impl PrintPages {
    pub(super) fn new(bounds: Rect, page_breaks: bool) -> Self {
        if page_breaks {
            let left = (bounds.left() / PAGE_WIDTH).floor();
            let top = (bounds.top() / PAGE_HEIGHT).floor();
            let columns = ((bounds.right() / PAGE_WIDTH).ceil() - left) as usize;
            let rows = ((bounds.bottom() / PAGE_HEIGHT).ceil() - top) as usize;
            let count = columns.saturating_mul(rows);
            if count <= MAX_PAGES {
                let mut viewports = Vec::with_capacity(count);
                for row in 0..rows {
                    for column in 0..columns {
                        viewports.push(Rect::from_min_size(
                            pos2(
                                (left + column as f32) * PAGE_WIDTH,
                                (top + row as f32) * PAGE_HEIGHT,
                            ),
                            vec2(PAGE_WIDTH, PAGE_HEIGHT),
                        ));
                    }
                }
                return Self {
                    viewports,
                    shrunk: false,
                };
            }
        }
        Self {
            viewports: vec![bounds.expand(16.0)],
            shrunk: page_breaks,
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
impl super::Designer {
    pub(super) fn print_diagram(
        &self,
        ctx: &eframe::egui::Context,
        graph: &super::Graph<'_>,
        tables: &super::Tables<'_>,
        pages: &PrintPages,
    ) -> Result<bool, String> {
        let mut fonts = super::bitmap::diagram_fonts(ctx);
        let title = format!("{} · 架构布局", self.layout.schema);
        system_print(&title, pages.viewports.len(), |page| {
            self.render_print_page(graph, pages.viewports[page], tables, &mut fonts)
        })
    }
}
