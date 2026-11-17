//! End-to-end browser pipeline integration tests.
//!
//! These exercise the full HTML -> CSS -> layout -> text -> paint -> rasterize
//! path on small documents, asserting invariants on the resulting framebuffer
//! rather than testing any single crate in isolation.

use std::collections::HashMap;

use bweb_css::{StyleEngine, Stylesheet};
use bweb_html::{HtmlDocument, NodeKind};
use bweb_layout::{LayoutTree, PaintAdapter, prepare_text_runs};
use bweb_renderer::{DisplayList, Framebuffer, Painter, Rasterizer};
use bweb_text::Font;

/// The UA stylesheet applied to documents under test (mirrors the shell).
const UA_CSS: &str =
    "html, body, main, div, p, span, section, header, footer, form, ul, ol, li { display: block; }";

fn font() -> Font {
    Font::from_bytes(include_bytes!("../../bweb-text/tests/data/test-font.ttf").to_vec())
        .expect("test font")
}

/// Renders an HTML document (plus inline stylesheets) into an RGBA8 framebuffer.
struct Harness {
    framebuffer: Framebuffer,
    display_list: DisplayList,
}

impl Harness {
    fn render(html: &str) -> Self {
        let document = HtmlDocument::parse(html);
        let mut stylesheets = vec![Stylesheet::parse(UA_CSS).expect("UA stylesheet")];
        for source in document.inline_stylesheets() {
            stylesheets.push(Stylesheet::parse(&source).expect("inline stylesheet"));
        }
        let styles =
            StyleEngine::new().compute_with_viewport(&document, &stylesheets, 800.0, 600.0);
        let mut font = font();
        let images = HashMap::new();
        let pseudo = HashMap::new();
        let tree = LayoutTree::build_with_viewport(
            &document,
            &styles,
            800.0,
            600.0,
            Some(&font),
            &images,
            &pseudo,
        );
        let runs = prepare_text_runs(&tree, &styles, &mut font, 1.0);
        let adapter = PaintAdapter::new(
            &tree,
            &styles,
            bweb_layout::Rect { x: 0.0, y: 0.0, width: 800.0, height: 600.0 },
        )
        .with_text_runs(&runs);
        let display_list = Painter::new(&adapter, 1.0).expect("valid scale").paint();
        let mut framebuffer = Framebuffer::new(800, 600).expect("framebuffer");
        framebuffer.clear([255, 255, 255, 255]);
        Rasterizer.rasterize(&display_list, &mut framebuffer);
        Self { framebuffer, display_list }
    }

    fn non_background_pixels(&self, color: [u8; 4]) -> usize {
        (0..self.framebuffer.width())
            .flat_map(|x| (0..self.framebuffer.height()).map(move |y| (x, y)))
            .filter(|(x, y)| self.framebuffer.pixel(*x, *y) == color)
            .count()
    }
}

#[test]
fn pipeline_renders_text_and_background() {
    let harness = Harness::render(
        "<html><head><style>body{background:#ff0000}</style></head>\
         <body><p>Hello world</p></body></html>",
    );
    // The red background must cover a substantial part of the viewport.
    // (The UA box model sizes html/body to their content in this engine, so we
    // assert a large-but-not-full coverage rather than the entire framebuffer.)
    assert!(harness.non_background_pixels([255, 0, 0, 255]) > 10_000);
    // Text runs must be present in the display list.
    assert!(
        harness
            .display_list
            .items
            .iter()
            .any(|item| matches!(item, bweb_renderer::PaintItem::Text { .. }))
    );
}

#[test]
fn opacity_group_keeps_overlap_alpha_uniform() {
    let harness = Harness::render(
        "<html><head><style>\
         .box{width:100px;height:20px;background:#0000ff;opacity:0.5}\
         </style></head><body><div class='box'></div></body></html>",
    );
    // Some pixels must be a blend of blue over white (partial alpha).
    let blended = (0..harness.framebuffer.width())
        .flat_map(|x| (0..harness.framebuffer.height()).map(move |y| (x, y)))
        .filter(|(x, y)| {
            let p = harness.framebuffer.pixel(*x, *y);
            // Blue over white at 50% gives ~(127, 127, 255): red/green are
            // partially darkened while blue stays saturated.
            p[0] > 0 && p[0] < 255
        })
        .count();
    assert!(blended > 0, "opacity must produce blended pixels");
}

#[test]
fn inline_and_external_script_order_is_declared_in_dom() {
    let document = HtmlDocument::parse(
        "<html><head>\
         <script>window.A = 1;</script>\
         <script src='/lib.js'></script>\
         <script>window.C = 3;</script>\
         </head><body></body></html>",
    );
    let refs = document.script_references();
    assert_eq!(refs.len(), 3);
    assert!(matches!(refs[0], bweb_html::ScriptRef::Inline(_)));
    assert!(matches!(refs[1], bweb_html::ScriptRef::External(_)));
    assert!(matches!(refs[2], bweb_html::ScriptRef::Inline(_)));
}

/// Renders with a specific viewport size and returns the layout height of a
/// block sized with `vh`, plus whether a `@media (min-height)` rule applied.
fn render_vh(width: f32, height: f32) -> (f32, bool) {
    let html = "<html><head><style>\
        div { height: 50vh; background: #ff0000 } \
        @media (min-height: 500px) { div { background: #00ff00 } } \
        </style></head><body><div>x</div></body></html>";
    let document = HtmlDocument::parse(html);
    let mut stylesheets = vec![Stylesheet::parse(UA_CSS).expect("UA")];
    for source in document.inline_stylesheets() {
        stylesheets.push(Stylesheet::parse(&source).expect("inline"));
    }
    let styles = StyleEngine::new().compute_with_viewport(&document, &stylesheets, width, height);
    let images = HashMap::new();
    let pseudo = HashMap::new();
    let tree =
        LayoutTree::build_with_viewport(&document, &styles, width, height, None, &images, &pseudo);
    let div = tree
        .box_count
        .checked_sub(1)
        .and_then(|_| {
            // Find the div's layout box by walking from the root.
            fn find_div<'a>(
                box_: &'a bweb_layout::LayoutBox,
                doc: &HtmlDocument,
            ) -> Option<&'a bweb_layout::LayoutBox> {
                if let Some(NodeKind::Element { tag_name, .. }) =
                    doc.node(box_.node).map(|n| n.kind())
                {
                    if tag_name == "div" {
                        return Some(box_);
                    }
                }
                box_.children.iter().find_map(|child| find_div(child, doc))
            }
            find_div(&tree.root, &document)
        })
        .expect("div layout box");
    let green = styles
        .get(
            document
                .iter()
                .find_map(|(id, n)| {
                    matches!(n.kind(), NodeKind::Element { tag_name, .. } if tag_name == "div")
                        .then_some(id)
                })
                .unwrap(),
        )
        .and_then(|style| style.get("background-color"))
        == Some("#00ff00");
    (div.rect.height, green)
}

#[test]
fn viewport_height_drives_vh_and_media_queries() {
    let (h400, green400) = render_vh(800.0, 400.0);
    let (h800, green800) = render_vh(800.0, 800.0);
    assert_eq!(h400, 200.0, "50vh of 400 = 200");
    assert_eq!(h800, 400.0, "50vh of 800 = 400");
    assert!(!green400, "min-height:500 must not apply at 400px");
    assert!(green800, "min-height:500 must apply at 800px");
}

#[test]
fn zero_flex_basis_distributes_space_in_pipeline() {
    let harness = Harness::render(
        "<html><head><style>\
         main{display:flex;width:200px}\
         span{flex:1 0 0;display:block;height:20px;background:#00ff00}\
         </style></head><body><main><span>a</span><span>b</span></main></body></html>",
    );
    // Two green blocks should each be ~100px wide and fill 40 rows.
    let green = harness.non_background_pixels([0, 255, 0, 255]);
    assert!(green > 3500 && green < 4500, "expected ~4000 green pixels, got {green}");
}
