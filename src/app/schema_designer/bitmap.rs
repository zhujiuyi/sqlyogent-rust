use std::path::Path;

use eframe::egui::{
    self, Align2, Color32, FontId, Pos2, Rect,
    epaint::{AlphaFromCoverage, text::Fonts},
    pos2, vec2,
};
use image::{ExtendedColorType, RgbImage, codecs::bmp::BmpEncoder};

use super::{
    Designer, Graph, PAGE_HEIGHT, PAGE_WIDTH, ROW, TITLE, Tables, card_rows, field_label, style,
    write_atomic,
};

const MAX_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_SIDE: u32 = 16 * 1024;
const ATLAS_SIDE: usize = 4096;
const BORDER: Color32 = Color32::from_rgb(216, 226, 231);
const HEADER: Color32 = Color32::from_rgb(213, 235, 232);
const PAGE_LINE: Color32 = Color32::from_rgb(167, 181, 188);

impl Designer {
    pub(super) fn write_bitmap(
        &self,
        ctx: &egui::Context,
        path: &Path,
        graph: &Graph<'_>,
        viewport: Rect,
        tables: &Tables<'_>,
    ) -> Result<(), String> {
        let image = self.paint_bitmap(
            graph,
            tables,
            &mut diagram_fonts(ctx),
            Canvas::new(viewport, 1.0)?,
            self.layout.page_breaks,
        );
        let stride = (image.width() as usize * 3).next_multiple_of(4);
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(54 + stride * image.height() as usize)
            .map_err(|_| "没有足够内存编码 BMP。")?;
        BmpEncoder::new(&mut bytes)
            .encode(
                image.as_raw(),
                image.width(),
                image.height(),
                ExtendedColorType::Rgb8,
            )
            .map_err(|error| error.to_string())?;
        write_atomic(path, &bytes).map_err(|error| error.to_string())
    }

    #[cfg(any(windows, target_os = "macos", test))]
    pub(super) fn render_print_page(
        &self,
        graph: &Graph<'_>,
        viewport: Rect,
        tables: &Tables<'_>,
        fonts: &mut Fonts,
    ) -> Result<RgbImage, String> {
        let scale = (PAGE_WIDTH / viewport.width())
            .min(PAGE_HEIGHT / viewport.height())
            .min(1.0)
            * 2.0;
        Ok(self.paint_bitmap(graph, tables, fonts, Canvas::new(viewport, scale)?, false))
    }

    fn paint_bitmap(
        &self,
        graph: &Graph<'_>,
        tables: &Tables<'_>,
        fonts: &mut Fonts,
        mut canvas: Canvas,
        page_guides: bool,
    ) -> RgbImage {
        let viewport = canvas.viewport;
        fonts.begin_pass(ATLAS_SIDE, AlphaFromCoverage::LIGHT_MODE_DEFAULT);

        if page_guides {
            let mut x = (viewport.left() / PAGE_WIDTH).ceil() * PAGE_WIDTH;
            while x < viewport.right() {
                canvas.line(
                    pos2(x, viewport.top()),
                    pos2(x, viewport.bottom()),
                    PAGE_LINE,
                );
                x += PAGE_WIDTH;
            }
            let mut y = (viewport.top() / PAGE_HEIGHT).ceil() * PAGE_HEIGHT;
            while y < viewport.bottom() {
                canvas.line(
                    pos2(viewport.left(), y),
                    pos2(viewport.right(), y),
                    PAGE_LINE,
                );
                y += PAGE_HEIGHT;
            }
        }
        for cable in &graph.links {
            for points in cable.points.windows(2) {
                canvas.line(points[0], points[1], style::MUTED);
            }
            let tip = cable.points[3];
            let direction = if cable.points[2].x < tip.x { -1.0 } else { 1.0 };
            for dy in [-4.0, 4.0] {
                canvas.line(tip, tip + vec2(direction * 7.0, dy), style::MUTED);
            }
            for (position, label) in cable.labels(1.0) {
                canvas.text(
                    fonts,
                    position - vec2(5.0, 0.0),
                    14.0,
                    label.into(),
                    style::MUTED,
                    viewport,
                );
            }
        }

        for (card, bounds) in self.layout.tables.iter().zip(&graph.cards) {
            let bounds = *bounds;
            if !bounds.intersects(viewport) {
                continue;
            }
            fonts.begin_pass(ATLAS_SIDE, AlphaFromCoverage::LIGHT_MODE_DEFAULT);
            let inner = bounds.shrink(1.0);
            let header = Rect::from_min_max(inner.min, pos2(inner.right(), bounds.top() + TITLE));
            canvas.fill(bounds, BORDER);
            canvas.fill(inner, Color32::WHITE);
            canvas.fill(header, HEADER);
            canvas.text(
                fonts,
                pos2(bounds.left() + 9.0, header.center().y),
                14.0,
                card.name.clone(),
                style::INK,
                header,
            );

            let columns = Self::card_columns(card, tables);
            let count = card_rows(bounds.height(), 1.0);
            let first = self.row_offsets.get(&card.name).copied().unwrap_or(0);
            let type_x = bounds.left() + bounds.width() * 0.54;
            let name_clip = if card.names_only {
                inner
            } else {
                Rect::from_min_max(inner.min, pos2(type_x - 4.0, inner.bottom()))
            };
            for (row, field) in columns.iter().skip(first).take(count).enumerate() {
                let y = bounds.top() + TITLE + (row as f32 + 0.5) * ROW;
                canvas.text(
                    fonts,
                    pos2(bounds.left() + 8.0, y),
                    14.0,
                    field_label(field),
                    style::INK,
                    name_clip,
                );
                if !card.names_only {
                    canvas.text(
                        fonts,
                        pos2(type_x, y),
                        14.0,
                        field.data_type.clone(),
                        style::MUTED,
                        inner,
                    );
                }
            }
            if first > 0 || columns.len() > first + count {
                canvas.text(
                    fonts,
                    pos2(bounds.left() + 8.0, bounds.bottom() - 7.0),
                    10.0,
                    "…".into(),
                    style::MUTED,
                    inner,
                );
            }
        }
        canvas.image
    }
}

pub(super) fn diagram_fonts(ctx: &egui::Context) -> Fonts {
    Fonts::new(
        ATLAS_SIDE,
        AlphaFromCoverage::LIGHT_MODE_DEFAULT,
        ctx.fonts(|fonts| fonts.definitions().clone()),
    )
}

struct Canvas {
    image: RgbImage,
    viewport: Rect,
    scale: f32,
}

impl Canvas {
    fn new(viewport: Rect, scale: f32) -> Result<Self, String> {
        let width = (viewport.width() * scale).ceil() as u32;
        let height = (viewport.height() * scale).ceil() as u32;
        let pixels = u64::from(width) * u64::from(height);
        if pixels == 0 || pixels > MAX_PIXELS || width > MAX_SIDE || height > MAX_SIDE {
            return Err("画布超过 BMP 的导出尺寸上限，请导出为 SVG。".into());
        }
        let len = pixels as usize * 3;
        let mut data = Vec::new();
        data.try_reserve_exact(len)
            .map_err(|_| "没有足够内存绘制架构图。")?;
        data.resize(len, 255);
        let image = RgbImage::from_raw(width, height, data).ok_or("无法创建架构图画布。")?;
        Ok(Self {
            image,
            viewport,
            scale,
        })
    }

    fn position(&self, point: Pos2) -> (i32, i32) {
        let pixel = (point - self.viewport.min) * self.scale;
        (pixel.x.round() as i32, pixel.y.round() as i32)
    }

    fn fill(&mut self, rect: Rect, color: Color32) {
        let (left, top) = self.position(rect.min);
        let (right, bottom) = self.position(rect.max);
        self.fill_pixels(left, top, right, bottom, color);
    }

    fn fill_pixels(&mut self, left: i32, top: i32, right: i32, bottom: i32, color: Color32) {
        let width = self.image.width() as usize;
        let height = self.image.height() as i32;
        let left = left.clamp(0, width as i32) as usize;
        let right = right.clamp(0, width as i32) as usize;
        if left >= right {
            return;
        }
        let color = [color.r(), color.g(), color.b()];
        let bytes: &mut [u8] = self.image.as_mut();
        for y in top.max(0)..bottom.min(height) {
            let start = (y as usize * width + left) * 3;
            let end = (y as usize * width + right) * 3;
            for pixel in bytes[start..end].chunks_exact_mut(3) {
                pixel.copy_from_slice(&color);
            }
        }
    }

    fn line(&mut self, start: Pos2, end: Pos2, color: Color32) {
        let (mut x, mut y) = self.position(start);
        let (end_x, end_y) = self.position(end);
        let thickness = (self.scale.round() as i32).max(1);
        // The long cable segments and guides are axis-aligned. Clip these in one fill.
        if y == end_y {
            self.fill_pixels(
                x.min(end_x),
                y,
                x.max(end_x) + thickness,
                y + thickness,
                color,
            );
            return;
        }
        if x == end_x {
            self.fill_pixels(
                x,
                y.min(end_y),
                x + thickness,
                y.max(end_y) + thickness,
                color,
            );
            return;
        }
        let dx = (end_x - x).abs();
        let dy = -(end_y - y).abs();
        let mut error = dx + dy;
        let x_step = (end_x - x).signum();
        let y_step = (end_y - y).signum();
        loop {
            self.fill_pixels(x, y, x + thickness, y + thickness, color);
            if x == end_x && y == end_y {
                break;
            }
            let next = error * 2;
            if next >= dy {
                error += dy;
                x += x_step;
            }
            if next <= dx {
                error += dx;
                y += y_step;
            }
        }
    }

    fn text(
        &mut self,
        fonts: &mut Fonts,
        anchor: Pos2,
        size: f32,
        text: String,
        color: Color32,
        clip: Rect,
    ) {
        let clip = clip.intersect(self.viewport);
        if !clip.is_positive() {
            return;
        }
        let galley = fonts.with_pixels_per_point(self.scale).layout_no_wrap(
            text,
            FontId::proportional(size),
            color,
        );
        let position = Align2::LEFT_CENTER.anchor_size(anchor, galley.size()).min;
        let (left, top) = self.position(clip.min);
        let (right, bottom) = self.position(clip.max);
        let atlas = fonts.texture_atlas().image();
        for row in &galley.rows {
            for glyph in &row.glyphs {
                let uv = glyph.uv_rect;
                let (x, y) =
                    self.position(position + row.pos.to_vec2() + glyph.pos.to_vec2() + uv.offset);
                let width = i32::from(uv.max[0] - uv.min[0]);
                let height = i32::from(uv.max[1] - uv.min[1]);
                for dy in (top - y).max(0)..(bottom - y).min(height) {
                    for dx in (left - x).max(0)..(right - x).min(width) {
                        let column = usize::from(uv.min[0]) + dx as usize;
                        let line = usize::from(uv.min[1]) + dy as usize;
                        let alpha = u16::from(atlas[(column, line)].a());
                        if alpha != 0
                            && let Some(pixel) = self
                                .image
                                .get_pixel_mut_checked((x + dx) as u32, (y + dy) as u32)
                        {
                            for (value, foreground) in
                                pixel.0.iter_mut().zip([color.r(), color.g(), color.b()])
                            {
                                *value = ((u16::from(*value) * (255 - alpha)
                                    + u16::from(foreground) * alpha
                                    + 127)
                                    / 255) as u8;
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::schema_designer::printing::PrintPages;
    use crate::database::{ColumnInfo, DbObject, ObjectKind, SchemaDesignTable, SchemaForeignKey};
    use image::Rgb;

    #[test]
    fn renders_diagram_bitmap_and_print_pages_with_fields_and_links() {
        let fixture =
            tempfile::tempdir_in(std::env::current_exe().unwrap().parent().unwrap()).unwrap();
        let path = fixture.path().join("架构.bmp");
        let schema = "会计";
        let child = "收款";
        let parent = "账簿";
        let object = |name: &str| DbObject {
            schema: schema.into(),
            name: name.into(),
            kind: ObjectKind::Table,
        };
        let table = |name: &str, key: &str| SchemaDesignTable {
            object: object(name),
            columns: vec![ColumnInfo {
                name: "账簿编号".into(),
                data_type: "bigint".into(),
                nullable: false,
                key: key.into(),
                default_value: None,
                extra: String::new(),
            }],
            unique_indexes: Some(if key == "PRI" {
                vec![crate::database::SchemaUniqueIndex {
                    name: "PRIMARY".into(),
                    columns: vec!["账簿编号".into()],
                }]
            } else {
                Vec::new()
            }),
        };
        let child_table = table(child, "");
        let parent_table = table(parent, "PRI");
        let tables = Tables::from([(child, &child_table), (parent, &parent_table)]);
        let keys = [SchemaForeignKey {
            table: object(child),
            name: "fk_ledger".into(),
            referenced_table: object(parent),
            columns: vec![("账簿编号".into(), "账簿编号".into())],
            on_delete: "RESTRICT".into(),
            on_update: "RESTRICT".into(),
        }];

        let mut designer = Designer::new(1, schema.into(), None);
        designer.add_card(child, 1);
        designer.add_card(parent, 1);
        designer.layout.tables[0].position = [-250.0, 85.0];
        designer.layout.tables[1].position = [95.0, 120.0];
        let graph = designer.geometry(Pos2::ZERO, 1.0, &tables, &keys);
        let viewport = designer.layout.bounds().unwrap().expand(16.0);
        let ctx = egui::Context::default();
        style::apply(&ctx);
        let _ = ctx.run(Default::default(), |ctx| {
            ctx.fonts_mut(|fonts| {
                if fonts.definitions().font_data.contains_key("System CJK") {
                    assert!(fonts.has_glyphs(&FontId::proportional(14.0), "收款账簿编号"));
                }
            });
            designer
                .write_bitmap(ctx, &path, &graph, viewport, &tables)
                .unwrap();
            let plain = image::open(&path).unwrap().into_rgb8();
            let sample = |image: &RgbImage, x: f32, y: f32| {
                *image.get_pixel(
                    (x - viewport.left()).round() as u32,
                    (y - viewport.top()).round() as u32,
                )
            };
            let color = |c: Color32| Rgb([c.r(), c.g(), c.b()]);
            for (position, _) in graph.links[0].labels(1.0) {
                assert!((-6..=6).any(|x| (-5..=2).any(|y| {
                    sample(&plain, position.x + x as f32, position.y + y as f32)
                        != color(Color32::WHITE)
                })));
            }
            let guide_y = viewport.top() + 5.0;
            assert_eq!(sample(&plain, 0.0, guide_y), color(Color32::WHITE));
            designer.layout.page_breaks = true;
            designer
                .write_bitmap(ctx, &path, &graph, viewport, &tables)
                .unwrap();

            let bytes = std::fs::read(&path).unwrap();
            assert_eq!(&bytes[..2], b"BM");
            let bitmap = image::open(&path).unwrap().into_rgb8();
            assert_eq!(
                bitmap.dimensions(),
                (
                    viewport.width().ceil() as u32,
                    viewport.height().ceil() as u32
                )
            );
            for rect in &graph.cards {
                assert_eq!(
                    sample(&bitmap, rect.right() - 4.0, rect.top() + 8.0),
                    color(HEADER)
                );
                assert!((0..58).any(|x| (0..20).any(|y| {
                    sample(
                        &bitmap,
                        rect.left() + 8.0 + x as f32,
                        rect.top() + TITLE + y as f32,
                    ) != color(Color32::WHITE)
                })));
            }
            let gap = graph.links[0].points;
            assert_eq!(
                sample(&bitmap, (gap[0].x + gap[1].x) * 0.5, gap[0].y),
                color(style::MUTED)
            );
            assert_eq!(sample(&bitmap, 0.0, guide_y), color(PAGE_LINE));

            let pages = PrintPages::new(designer.layout.bounds().unwrap(), true);
            assert_eq!(pages.viewports.len(), 2);
            assert_eq!(pages.viewports[0].left(), -PAGE_WIDTH);
            assert_eq!(pages.viewports[1].left(), 0.0);
            let mut fonts = diagram_fonts(ctx);
            for (index, page) in pages.viewports.into_iter().enumerate() {
                let raster = designer
                    .render_print_page(&graph, page, &tables, &mut fonts)
                    .unwrap();
                assert_eq!(raster.dimensions(), (1_580, 2_200));
                let sample_page = |x: f32, y: f32| {
                    *raster.get_pixel(
                        ((x - page.left()) * 2.0).round() as u32,
                        ((y - page.top()) * 2.0).round() as u32,
                    )
                };
                let card = graph.cards[index];
                assert_eq!(
                    sample_page(card.right() - 4.0, card.top() + 8.0),
                    color(HEADER)
                );
                assert!((0..58).any(|x| (0..20).any(|y| {
                    sample_page(card.left() + 8.0 + x as f32, card.top() + TITLE + y as f32)
                        != color(Color32::WHITE)
                })));
                assert_eq!(
                    sample_page(if index == 0 { -8.0 } else { 8.0 }, gap[0].y),
                    color(style::MUTED)
                );
                assert_eq!(
                    sample_page(page.left(), page.top() + 5.0),
                    color(Color32::WHITE)
                );
            }
        });
    }
}
