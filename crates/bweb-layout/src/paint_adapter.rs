//! Adapter from the current layout tree and computed CSS values to the renderer port.

use std::{collections::HashMap, sync::Arc};

use crate::{LayoutBox, LayoutTree, Rect};
use bweb_css::ComputedStyleMap;
use bweb_html::NodeId;
use bweb_renderer::{
    BackgroundImage, Image, PaintLayout, PaintNode, PaintStyle, Rect as PaintRect, TextRun,
    parse_background_image, parse_color,
};

/// Exposes a layout tree as an immutable paint-order visitor.
pub struct PaintAdapter<'a> {
    pub tree: &'a LayoutTree,
    pub styles: &'a ComputedStyleMap,
    pub viewport: Rect,
    text_runs: Option<&'a HashMap<NodeId, Arc<TextRun>>>,
    images: Option<&'a HashMap<NodeId, Arc<Image>>>,
    background_images: Option<&'a HashMap<String, Arc<Image>>>,
}

impl<'a> PaintAdapter<'a> {
    /// Creates an adapter without prepared text runs.
    #[must_use]
    pub fn new(tree: &'a LayoutTree, styles: &'a ComputedStyleMap, viewport: Rect) -> Self {
        Self { tree, styles, viewport, text_runs: None, images: None, background_images: None }
    }

    /// Adds prepared runs keyed by the DOM text node that produced them.
    ///
    /// Each run must contain final physical-pixel glyph positions. Font
    /// selection, shaping/layout, rasterization, and device scaling are
    /// deliberately performed before constructing this adapter.
    #[must_use]
    pub fn with_text_runs(mut self, text_runs: &'a HashMap<NodeId, Arc<TextRun>>) -> Self {
        self.text_runs = Some(text_runs);
        self
    }

    /// Adds decoded images keyed by the DOM element that references them.
    #[must_use]
    pub fn with_images(mut self, images: &'a HashMap<NodeId, Arc<Image>>) -> Self {
        self.images = Some(images);
        self
    }

    /// Adds decoded CSS background images keyed by their resolved URL.
    #[must_use]
    pub fn with_background_images(mut self, images: &'a HashMap<String, Arc<Image>>) -> Self {
        self.background_images = Some(images);
        self
    }
}

/// Parses a uniform CSS `border-radius` into logical pixels. Percentages
/// resolve against the smaller side, so `50%` yields a circle/ellipse.
fn parse_border_radius(value: &str, width: f32, height: f32) -> Option<f32> {
    let first = value.split('/').next()?.trim().split_ascii_whitespace().next()?;
    if let Some(percent) = first.strip_suffix('%') {
        let fraction = percent.trim().parse::<f32>().ok()?.max(0.0) / 100.0;
        return Some(width.min(height) * fraction);
    }
    first.strip_suffix("px").unwrap_or(first).trim().parse::<f32>().ok().map(|v| v.max(0.0))
}

impl PaintLayout for PaintAdapter<'_> {
    fn viewport(&self) -> PaintRect {
        to_paint_rect(self.viewport)
    }

    fn visit(&self, visitor: &mut dyn FnMut(PaintNode)) {
        fn walk(
            layout: &LayoutBox,
            styles: &ComputedStyleMap,
            text_runs: Option<&HashMap<NodeId, Arc<TextRun>>>,
            images: Option<&HashMap<NodeId, Arc<Image>>>,
            background_images: Option<&HashMap<String, Arc<Image>>>,
            depth: u32,
            visitor: &mut dyn FnMut(PaintNode),
        ) {
            let computed = styles.get(layout.node);
            let style = PaintStyle {
                background: computed
                    .and_then(|style| style.get("background-color"))
                    .and_then(parse_color),
                border_color: computed
                    .and_then(|style| style.get("border-color"))
                    .and_then(parse_color),
                border_widths: bweb_renderer::Edges {
                    top: layout.border.top,
                    right: layout.border.right,
                    bottom: layout.border.bottom,
                    left: layout.border.left,
                },
                clips_children: computed
                    .and_then(|style| style.get("overflow"))
                    .is_some_and(|value| value == "hidden" || value == "clip"),
                border_radius: computed
                    .and_then(|style| style.get("border-radius"))
                    .and_then(|value| {
                        parse_border_radius(value, layout.rect.width, layout.rect.height)
                    })
                    .unwrap_or(0.0),
            };

            let background_image: Option<Arc<BackgroundImage>> = computed
                .and_then(|style| style.get("background-image"))
                .and_then(parse_background_image)
                .map(Arc::new);
            let background_image_bitmap = background_image.as_ref().and_then(|image| {
                if let BackgroundImage::Url(url) = image.as_ref() {
                    background_images?.get(url).cloned()
                } else {
                    None
                }
            });

            visitor(PaintNode {
                depth,
                rect: to_paint_rect(layout.rect),
                style,
                text: text_runs.and_then(|runs| runs.get(&layout.node).cloned()),
                image: images.and_then(|items| items.get(&layout.node).cloned()),
                background_image,
                background_image_bitmap,
                background_size: computed
                    .and_then(|style| style.get("background-size"))
                    .map(str::to_owned),
                background_position: computed
                    .and_then(|style| style.get("background-position"))
                    .map(str::to_owned),
                background_repeat: computed
                    .and_then(|style| style.get("background-repeat"))
                    .map(str::to_owned),
                transform: computed.and_then(|style| style.get("transform")).map(str::to_owned),
                transform_origin: computed
                    .and_then(|style| style.get("transform-origin"))
                    .map(str::to_owned),
                opacity: computed.and_then(|style| style.get("opacity")).map(str::to_owned),
                filter: computed.and_then(|style| style.get("filter")).map(str::to_owned),
                box_shadow: computed.and_then(|style| style.get("box-shadow")).map(str::to_owned),
                sticky: layout.sticky.active.then_some((
                    layout.sticky.active,
                    layout.sticky.threshold,
                    layout.sticky.dx,
                    layout.sticky.dy,
                )),
            });

            for child in &layout.children {
                walk(
                    child,
                    styles,
                    text_runs,
                    images,
                    background_images,
                    depth.saturating_add(1),
                    visitor,
                );
            }
        }

        walk(
            &self.tree.root,
            self.styles,
            self.text_runs,
            self.images,
            self.background_images,
            0,
            visitor,
        );
    }
}

fn to_paint_rect(rect: Rect) -> PaintRect {
    PaintRect { x: rect.x, y: rect.y, width: rect.width, height: rect.height }
}

#[cfg(test)]
mod tests {
    use super::{PaintAdapter, parse_color};
    use crate::{LayoutTree, Rect};
    use bweb_css::{StyleEngine, Stylesheet};
    use bweb_html::{HtmlDocument, NodeId, NodeKind};
    use bweb_renderer::{
        BackgroundImage, Color, Framebuffer, Image, PaintItem, PaintLayout, Painter, Rasterizer,
        TextRun,
    };
    use bweb_text::Coverage;
    use std::{collections::HashMap, sync::Arc};

    #[test]
    fn parses_named_and_hex_colors() {
        assert_eq!(parse_color("red"), Some(Color::rgba(255, 0, 0, 255)));
        assert_eq!(parse_color("#0f8"), Some(Color::rgba(0, 255, 136, 255)));
        assert_eq!(parse_color("#11223380"), Some(Color::rgba(17, 34, 51, 128)));
        assert_eq!(parse_color("transparent"), None);
        assert_eq!(parse_color("not-a-color"), None);
    }

    fn gradient_document() -> (HtmlDocument, Vec<Stylesheet>) {
        let document = HtmlDocument::parse(
            "<html><head><style>\
             div { display:block; width:100px; height:40px; background: linear-gradient(to right, red, blue); }\
             </style></head><body><div>x</div></body></html>",
        );
        let sheets: Vec<_> = document
            .inline_stylesheets()
            .iter()
            .map(|source| Stylesheet::parse(source).expect("inline stylesheet"))
            .collect();
        (document, sheets)
    }

    #[test]
    fn background_gradient_reaches_the_paint_adapter() {
        let (document, sheets) = gradient_document();
        let styles = StyleEngine::new().compute(&document, &sheets);
        let tree = LayoutTree::build(&document, &styles, 320.0);
        let adapter =
            PaintAdapter::new(&tree, &styles, Rect { x: 0.0, y: 0.0, width: 320.0, height: 200.0 });

        let mut gradients = Vec::new();
        adapter.visit(&mut |node| {
            if let Some(image) = node.background_image {
                gradients.push(image);
            }
        });

        assert_eq!(gradients.len(), 1);
        assert!(matches!(
            gradients[0].as_ref(),
            BackgroundImage::Linear(gradient) if gradient.angle == 90.0 && gradient.stops.len() == 2
        ));
    }

    #[test]
    fn border_radius_emits_a_round_clip() {
        let document = HtmlDocument::parse(
            "<html><head><style>div { display:block; width:20px; height:20px; \
             border-radius:50%; background:red }</style></head><body><div>x</div></body></html>",
        );
        let sheets: Vec<_> = document
            .inline_stylesheets()
            .iter()
            .map(|source| Stylesheet::parse(source).expect("inline stylesheet"))
            .collect();
        let styles = StyleEngine::new().compute(&document, &sheets);
        let tree = LayoutTree::build(&document, &styles, 320.0);
        let adapter =
            PaintAdapter::new(&tree, &styles, Rect { x: 0.0, y: 0.0, width: 320.0, height: 200.0 });
        let list = Painter::new(&adapter, 1.0).expect("valid scale").paint();
        assert!(
            list.items.iter().any(
                |item| matches!(item, PaintItem::PushRoundClip { radius, .. } if *radius > 0.0)
            ),
            "a 50% radius on a 20px box should emit a round clip"
        );
    }

    #[test]
    fn url_background_reaches_the_display_list_and_framebuffer() {
        let document = HtmlDocument::parse(
            "<html><head><style>div { display:block; width:40px; height:20px; \
             background-image:url('https://example.test/bg.png') }</style></head><body><div>x</div></body></html>",
        );
        let sheets: Vec<_> = document
            .inline_stylesheets()
            .iter()
            .map(|source| Stylesheet::parse(source).expect("inline stylesheet"))
            .collect();
        let mut styles = StyleEngine::new().compute(&document, &sheets);
        styles.resolve_urls("https://example.test/page/index.html");
        let tree = LayoutTree::build(&document, &styles, 320.0);
        let background = Arc::new(Image::new(2, 2, [255, 0, 0, 255].repeat(4)).expect("image"));
        let backgrounds =
            HashMap::from([("https://example.test/bg.png".to_owned(), Arc::clone(&background))]);
        let adapter =
            PaintAdapter::new(&tree, &styles, Rect { x: 0.0, y: 0.0, width: 320.0, height: 200.0 })
                .with_background_images(&backgrounds);
        let list = Painter::new(&adapter, 1.0).expect("valid scale").paint();
        assert!(list.items.iter().any(|item| matches!(item, PaintItem::Image { .. })));

        let mut framebuffer = Framebuffer::new(320, 200).expect("framebuffer");
        Rasterizer.rasterize(&list, &mut framebuffer);
        assert_eq!(framebuffer.pixels()[0..4], [255, 0, 0, 255]);
    }

    #[test]
    fn background_size_and_repeat_shape_the_painted_tiles() {
        let document = HtmlDocument::parse(
            "<html><head><style>div { display:block; width:40px; height:20px; \
             background-image:url('https://example.test/bg.png'); \
             background-size:20px 10px; background-repeat:repeat; \
             background-position:0 0 }</style></head><body><div>x</div></body></html>",
        );
        let sheets: Vec<_> = document
            .inline_stylesheets()
            .iter()
            .map(|source| Stylesheet::parse(source).expect("inline stylesheet"))
            .collect();
        let mut styles = StyleEngine::new().compute(&document, &sheets);
        styles.resolve_urls("https://example.test/");
        let tree = LayoutTree::build(&document, &styles, 320.0);
        let background = Arc::new(Image::new(2, 2, [255, 0, 0, 255].repeat(4)).expect("image"));
        let backgrounds =
            HashMap::from([("https://example.test/bg.png".to_owned(), Arc::clone(&background))]);
        let adapter =
            PaintAdapter::new(&tree, &styles, Rect { x: 0.0, y: 0.0, width: 320.0, height: 200.0 })
                .with_background_images(&backgrounds);
        let list = Painter::new(&adapter, 1.0).expect("valid scale").paint();

        let tiles: Vec<_> = list
            .items
            .iter()
            .filter_map(|item| match item {
                PaintItem::Image { rect, .. } => Some(*rect),
                _ => None,
            })
            .collect();
        assert_eq!(tiles.len(), 4, "a 40x20 box tiled at 20x10 needs 4 tiles");
        assert!(tiles.iter().any(|tile| tile.x == 0.0 && tile.y == 0.0));
        assert!(tiles.iter().any(|tile| tile.x == 20.0 && tile.y == 10.0));

        let mut framebuffer = Framebuffer::new(320, 200).expect("framebuffer");
        Rasterizer.rasterize(&list, &mut framebuffer);
        assert_eq!(framebuffer.pixels()[0..4], [255, 0, 0, 255]);
    }

    #[test]
    fn rasterizes_a_css_gradient_background_into_pixels() {
        let (document, sheets) = gradient_document();
        let styles = StyleEngine::new().compute(&document, &sheets);
        let tree = LayoutTree::build(&document, &styles, 320.0);
        let adapter =
            PaintAdapter::new(&tree, &styles, Rect { x: 0.0, y: 0.0, width: 320.0, height: 200.0 });
        let list = Painter::new(&adapter, 1.0).expect("valid scale").paint();
        let rect = list
            .items
            .iter()
            .find_map(|item| match item {
                PaintItem::LinearGradient { rect, .. } => Some(*rect),
                _ => None,
            })
            .expect("gradient paint command");

        let mut framebuffer = Framebuffer::new(320, 200).expect("framebuffer");
        framebuffer.clear([255, 255, 255, 255]);
        Rasterizer.rasterize(&list, &mut framebuffer);
        let pixel = |x: u32, y: u32| {
            let offset = (y as usize * 320 + x as usize) * 4;
            framebuffer.pixels()[offset..offset + 4].to_vec()
        };

        let y = (rect.y + rect.height / 2.0) as u32;
        let left = pixel((rect.x + 1.0) as u32, y);
        let right = pixel((rect.x + rect.width - 2.0) as u32, y);
        assert!(left[0] > left[2], "left side should be red-dominant: {left:?}");
        assert!(right[2] > right[0], "right side should be blue-dominant: {right:?}");
        assert_eq!(pixel(310, 190), vec![255, 255, 255, 255]);
    }

    #[test]
    fn attaches_prepared_text_run_to_its_dom_text_node() {
        let document = HtmlDocument::parse("<main>hello</main>");
        let stylesheet = Stylesheet::parse("main { display:block }").expect("valid CSS");
        let styles = StyleEngine::new().compute(&document, &[stylesheet]);
        let tree = LayoutTree::build(&document, &styles, 320.0);
        let text_node: NodeId = document
            .iter()
            .find_map(|(id, node)| matches!(node.kind(), NodeKind::Text(_)).then_some(id))
            .expect("text node");
        let run = Arc::new(TextRun {
            glyphs: vec![bweb_renderer::PositionedGlyph {
                coverage: Coverage { width: 1, height: 1, left: 0, top: 1, data: vec![255] },
                x: 12.0,
                y: 8.0,
            }],
            color: Color::rgba(20, 30, 40, 255),
            ..Default::default()
        });
        let runs = HashMap::from([(text_node, Arc::clone(&run))]);
        let adapter =
            PaintAdapter::new(&tree, &styles, Rect { x: 0.0, y: 0.0, width: 320.0, height: 200.0 })
                .with_text_runs(&runs);

        let mut painted_text = Vec::new();
        adapter.visit(&mut |node| {
            if let Some(text) = node.text {
                painted_text.push(text);
            }
        });

        assert_eq!(painted_text, vec![run]);
    }
}
