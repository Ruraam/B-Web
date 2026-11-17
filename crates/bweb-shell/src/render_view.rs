//! Retained software-rendering backend used by the native window resize path.

use std::{collections::HashMap, sync::Arc};

use bweb_css::{ComputedStyle, ComputedStyleMap, PseudoElement, StyleEngine, Stylesheet};
use bweb_html::{HtmlDocument, NodeId};
use bweb_layout::{ImageSize, LayoutTree, PaintAdapter, Rect, ViewBackend, Viewport};
use bweb_renderer::{DisplayList, Framebuffer, Image, PaintItem, Painter, Rasterizer, TextRun};
use bweb_text::Font;

use crate::{generated::pseudo_content, images::image_sizes};

/// Retains layout, prepared text, and paint commands across viewport changes.
pub struct BrowserViewBackend {
    document: HtmlDocument,
    stylesheets: Vec<Stylesheet>,
    styles: ComputedStyleMap,
    font: Font,
    images: HashMap<NodeId, Arc<Image>>,
    background_images: HashMap<String, Arc<Image>>,
    pseudo_styles: HashMap<(NodeId, PseudoElement), ComputedStyle>,
    text_runs: HashMap<NodeId, Arc<TextRun>>,
    display_list: Option<DisplayList>,
    framebuffer: Option<Framebuffer>,
    content_height_css: f32,
    scroll_y: f32,
}

impl BrowserViewBackend {
    #[must_use]
    pub fn new(
        document: HtmlDocument,
        stylesheets: Vec<Stylesheet>,
        font: Font,
        images: HashMap<NodeId, Arc<Image>>,
        background_images: HashMap<String, Arc<Image>>,
        pseudo_styles: HashMap<(NodeId, PseudoElement), ComputedStyle>,
        document_url: &str,
    ) -> Self {
        let mut styles =
            StyleEngine::new().compute_with_viewport(&document, &stylesheets, 1280.0, 720.0);
        styles.resolve_urls(document_url);
        Self {
            document,
            stylesheets,
            styles,
            font,
            images,
            background_images,
            pseudo_styles,
            text_runs: HashMap::new(),
            display_list: None,
            framebuffer: None,
            content_height_css: 0.0,
            scroll_y: 0.0,
        }
    }

    #[must_use]
    pub const fn document(&self) -> &HtmlDocument {
        &self.document
    }

    #[must_use]
    pub fn framebuffer(&self) -> Option<&Framebuffer> {
        self.framebuffer.as_ref()
    }

    /// Current vertical scroll offset in physical pixels.
    #[must_use]
    pub const fn scroll_y(&self) -> f32 {
        self.scroll_y
    }

    /// Sets the vertical scroll offset, clamping it to the laid-out content.
    pub fn set_scroll_y(&mut self, scroll_y: f32, viewport_height: u32, scale_factor: f64) {
        self.scroll_y = scroll_y.clamp(0.0, self.max_scroll(viewport_height, scale_factor));
    }

    /// Largest meaningful scroll offset for a viewport height in physical pixels.
    #[must_use]
    pub fn max_scroll(&self, viewport_height: u32, scale_factor: f64) -> f32 {
        if !scale_factor.is_finite() || scale_factor <= 0.0 {
            return 0.0;
        }
        let content = self.content_height_css * scale_factor as f32;
        (content - viewport_height as f32).max(0.0)
    }

    /// Returns the DOM node whose rasterized glyph covers a point expressed in
    /// page physical pixels. Coordinates are relative to the document, not to
    /// the address bar or to the current scroll position.
    #[must_use]
    pub fn hit_test(&self, x: f32, y: f32) -> Option<NodeId> {
        for (node, run) in &self.text_runs {
            for glyph in &run.glyphs {
                let right = glyph.x + glyph.coverage.width as f32;
                let bottom = glyph.y + glyph.coverage.height as f32;
                if x >= glyph.x && x <= right && y >= glyph.y && y <= bottom {
                    return Some(*node);
                }
            }
        }
        None
    }
}

impl ViewBackend for BrowserViewBackend {
    type Error = String;

    fn reflow(&mut self, width: u32, scale_factor: f64) -> Result<(), Self::Error> {
        let scale = scale_factor as f32;
        if !scale.is_finite() || scale <= 0.0 {
            return Err("device scale factor is outside the supported f32 range".into());
        }
        let width_css = width as f32 / scale;
        if !width_css.is_finite() || width_css <= 0.0 {
            return Err("logical viewport width is invalid".into());
        }

        // Media queries depend on the viewport width, so styles are recomputed
        // on every reflow from the original stylesheets.
        let (styles, pseudo_styles) = StyleEngine::new().compute_generated(
            &self.document,
            &self.stylesheets,
            width_css,
            720.0,
        );
        self.styles = styles;
        self.pseudo_styles = pseudo_styles;
        let sizes: HashMap<NodeId, ImageSize> = image_sizes(&self.images);
        let pseudo = pseudo_content(&self.pseudo_styles);
        let layout = LayoutTree::build_with_all(
            &self.document,
            &self.styles,
            width_css,
            Some(&self.font),
            &sizes,
            &pseudo,
        );
        self.text_runs =
            bweb_layout::prepare_text_runs(&layout, &self.styles, &mut self.font, scale);

        // The viewport clip height is patched during repaint. Keeping it out of
        // layout inputs lets height-only changes reuse this complete display list.
        let adapter = PaintAdapter::new(
            &layout,
            &self.styles,
            Rect { x: 0.0, y: 0.0, width: width_css, height: 0.0 },
        )
        .with_text_runs(&self.text_runs)
        .with_images(&self.images)
        .with_background_images(&self.background_images);
        let display_list =
            Painter::new(&adapter, scale).map_err(|error| error.to_string())?.paint();

        self.content_height_css = layout.root.rect.height.max(0.0);
        self.display_list = Some(display_list);
        Ok(())
    }

    fn repaint(&mut self, viewport: Viewport) -> Result<(), Self::Error> {
        let Some(stored) = self.display_list.as_ref() else {
            return Err("cannot repaint before the initial layout".to_owned());
        };
        if !matches!(stored.items.first(), Some(PaintItem::PushClip { .. })) {
            return Err("display list is missing its viewport clip".into());
        }

        self.scroll_y =
            self.scroll_y.clamp(0.0, self.max_scroll(viewport.height(), viewport.scale_factor()));
        let mut frame = stored.clone();
        if let PaintItem::PushClip { rect } = &mut frame.items[0] {
            rect.width = viewport.width() as f32;
            rect.height = viewport.height() as f32;
        }
        // The first clip is the viewport itself; every later command belongs to
        // the document and must move together when the page is scrolled.
        if self.scroll_y > 0.0 {
            translate_scroll(&mut frame.items, self.scroll_y);
        }

        let mut framebuffer = Framebuffer::new(viewport.width(), viewport.height())
            .map_err(|error| format!("could not allocate render frame: {error:?}"))?;
        framebuffer.clear([255, 255, 255, 255]);
        Rasterizer.rasterize(&frame, &mut framebuffer);
        self.framebuffer = Some(framebuffer);
        Ok(())
    }
}

/// Moves one paint command vertically without changing the viewport clip.
fn translate_y(item: &mut PaintItem, offset: f32) {
    match item {
        PaintItem::FillRect { rect, .. }
        | PaintItem::BorderRect { rect, .. }
        | PaintItem::LinearGradient { rect, .. }
        | PaintItem::PushClip { rect }
        | PaintItem::PushRoundClip { rect, .. }
        | PaintItem::Image { rect, .. } => rect.y += offset,
        PaintItem::Text { glyphs, .. } => {
            for glyph in glyphs {
                glyph.y += offset;
            }
        }
        PaintItem::PushSticky { .. }
        | PaintItem::PopSticky
        | PaintItem::PushOpacity { .. }
        | PaintItem::PopOpacity
        | PaintItem::PushTransform { .. }
        | PaintItem::PopTransform
        | PaintItem::PopClip => {}
    }
}

/// Translates document commands by `-scroll_y`, capping the shift inside sticky
/// subtrees so they stay pinned to their threshold.
fn translate_scroll(items: &mut [PaintItem], scroll_y: f32) {
    let mut thresholds: Vec<f32> = Vec::new();
    for item in items.iter_mut().skip(1) {
        match item {
            PaintItem::PushSticky { threshold, .. } => thresholds.push(*threshold),
            PaintItem::PopSticky => {
                thresholds.pop();
            }
            other => {
                let shift =
                    thresholds.last().map_or(-scroll_y, |threshold| -scroll_y.min(*threshold));
                translate_y(other, shift);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BrowserViewBackend, translate_scroll, translate_y};
    use bweb_css::Stylesheet;
    use bweb_html::{HtmlDocument, NodeId, NodeKind};
    use bweb_layout::{ViewBackend, Viewport};
    use bweb_renderer::{
        Color, ColorStop, DisplayList, LinearGradient, PaintItem, PositionedGlyph, Rect, TextRun,
    };
    use bweb_text::{Coverage, Font};
    use std::{collections::HashMap, sync::Arc};

    fn backend(document: HtmlDocument) -> BrowserViewBackend {
        let font =
            Font::from_bytes(include_bytes!("../../bweb-text/tests/data/test-font.ttf").to_vec())
                .expect("fixture font should load");
        BrowserViewBackend::new(
            document,
            Vec::new(),
            font,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            "https://example.test/",
        )
    }

    fn text_node(document: &HtmlDocument, value: &str) -> NodeId {
        document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Text(text) if text == value).then_some(id)
            })
            .expect("text node")
    }

    #[test]
    fn hit_test_maps_a_rasterized_glyph_to_its_dom_text_node() {
        let document = HtmlDocument::parse("<main><a href='/next'>hello</a></main>");
        let node = text_node(&document, "hello");
        let mut backend = backend(document);
        backend.text_runs.insert(
            node,
            Arc::new(TextRun {
                glyphs: vec![PositionedGlyph {
                    coverage: Coverage {
                        width: 8,
                        height: 12,
                        left: 0,
                        top: 12,
                        data: vec![255; 96],
                    },
                    x: 10.0,
                    y: 4.0,
                }],
                color: Color::rgba(0, 0, 0, 255),
                ..Default::default()
            }),
        );

        assert_eq!(backend.hit_test(12.0, 8.0), Some(node));
        assert_eq!(backend.hit_test(100.0, 8.0), None);
        assert_eq!(
            backend.document().hyperlink_href(node),
            Some("/next"),
            "the hit node must retain its link ancestry",
        );
    }

    #[test]
    fn scroll_offset_is_clamped_to_content_height() {
        let mut backend = backend(HtmlDocument::parse("<main>text</main>"));
        backend.content_height_css = 1000.0;
        assert_eq!(backend.max_scroll(200, 2.0), 1800.0);
        backend.set_scroll_y(5000.0, 200, 2.0);
        assert_eq!(backend.scroll_y(), 1800.0);
        backend.set_scroll_y(-10.0, 200, 2.0);
        assert_eq!(backend.scroll_y(), 0.0);
    }

    #[test]
    fn scrolling_changes_the_rasterized_frame_without_reflow() {
        let markup = format!("<main>{}</main>", "<p>MMMMMMMMMMMMMMMMMM</p>".repeat(40));
        let document = HtmlDocument::parse(markup);
        let sheet = Stylesheet::parse("main, p { display: block }").expect("valid stylesheet");
        let font =
            Font::from_bytes(include_bytes!("../../bweb-text/tests/data/test-font.ttf").to_vec())
                .expect("fixture font should load");
        let mut backend = BrowserViewBackend::new(
            document,
            vec![sheet],
            font,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            "https://example.test/",
        );
        let viewport = Viewport::new(200, 100, 1.0).expect("viewport");
        backend.reflow(200, 1.0).expect("initial reflow");
        backend.repaint(viewport).expect("initial paint");
        let initial = backend.framebuffer().expect("initial frame").clone();

        let max = backend.max_scroll(viewport.height(), viewport.scale_factor());
        assert!(max > 0.0, "a long page must be scrollable");
        backend.set_scroll_y(max, viewport.height(), viewport.scale_factor());
        backend.repaint(viewport).expect("scrolled paint");
        let scrolled = backend.framebuffer().expect("scrolled frame").clone();

        assert_ne!(initial.pixels(), scrolled.pixels(), "scrolling must move painted content");
    }

    #[test]
    fn sticky_subtree_caps_scroll_translation() {
        let mut items = vec![
            PaintItem::PushClip { rect: Rect { x: 0.0, y: 0.0, width: 100.0, height: 100.0 } },
            PaintItem::PushSticky { threshold: 20.0, dx: 0.0, dy: 5.0 },
            PaintItem::FillRect {
                rect: Rect { x: 0.0, y: 25.0, width: 10.0, height: 10.0 },
                color: Color::rgba(255, 0, 0, 255),
            },
            PaintItem::PopSticky,
            PaintItem::FillRect {
                rect: Rect { x: 0.0, y: 50.0, width: 10.0, height: 10.0 },
                color: Color::rgba(0, 0, 255, 255),
            },
        ];
        translate_scroll(&mut items, 60.0);
        let list = DisplayList { items };

        // The sticky box is capped at threshold 20: 25 - 20 = 5 (its offset).
        assert_eq!(
            list.items[2],
            PaintItem::FillRect {
                rect: Rect { x: 0.0, y: 5.0, width: 10.0, height: 10.0 },
                color: Color::rgba(255, 0, 0, 255),
            }
        );
        // The normal box moves the full scroll amount.
        assert_eq!(
            list.items[4],
            PaintItem::FillRect {
                rect: Rect { x: 0.0, y: -10.0, width: 10.0, height: 10.0 },
                color: Color::rgba(0, 0, 255, 255),
            }
        );
    }

    #[test]
    fn translating_a_paint_command_moves_geometry_and_glyphs_only() {
        let glyph = PositionedGlyph {
            coverage: Coverage { width: 1, height: 1, left: 0, top: 0, data: vec![255] },
            x: 4.0,
            y: 8.0,
        };
        let gradient = Arc::new(LinearGradient {
            angle: 90.0,
            stops: vec![
                ColorStop { color: Color::rgba(255, 0, 0, 255), position: None },
                ColorStop { color: Color::rgba(0, 0, 255, 255), position: None },
            ],
        });
        let mut items = [
            PaintItem::PushClip { rect: Rect { x: 0.0, y: 0.0, width: 10.0, height: 10.0 } },
            PaintItem::LinearGradient {
                rect: Rect { x: 0.0, y: 2.0, width: 10.0, height: 10.0 },
                gradient,
            },
            PaintItem::Text {
                glyphs: vec![glyph],
                color: Color::rgba(0, 0, 0, 255),
                decoration: bweb_renderer::TextDecoration::None,
            },
            PaintItem::PopClip,
        ];
        for item in items.iter_mut().skip(1) {
            translate_y(item, -3.0);
        }
        assert_eq!(
            items[0],
            PaintItem::PushClip { rect: Rect { x: 0.0, y: 0.0, width: 10.0, height: 10.0 } }
        );
        assert!(matches!(&items[1], PaintItem::LinearGradient { rect, .. } if rect.y == -1.0));
        assert!(
            matches!(&items[2], PaintItem::Text { glyphs, .. } if glyphs[0].y == 5.0 && glyphs[0].x == 4.0)
        );
        assert_eq!(items[3], PaintItem::PopClip);
    }
}
