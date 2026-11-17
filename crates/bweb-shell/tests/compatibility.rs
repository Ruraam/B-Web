//! Browser compatibility tests on representative real-web HTML/CSS patterns.
//!
//! Each test feeds a small but idiomatic page through the actual engine
//! pipeline — HTML parse, cascade, layout, paint, rasterize — rather than
//! probing a single helper. The goal is to pin down the behaviours the engine
//! already handles so regressions in common layouts are caught early.

use std::collections::HashMap;

use bweb_css::{ComputedStyle, StyleEngine, Stylesheet};
use bweb_html::{HtmlDocument, NodeId};
use bweb_layout::{LayoutBox, LayoutTree, PaintAdapter, prepare_text_runs};
use bweb_renderer::{DisplayList, Framebuffer, Painter, Rasterizer};
use bweb_text::Font;

/// The same built-in UA sheet the shell applies to every real page.
const UA_CSS: &str = "html, body, main, header, footer, nav, section, article, aside, div, p, h1, h2, h3, ul, ol, li, form, table, blockquote { display: block; } \
     input, button, select { display: inline-block; border: 2px solid #aaa; padding: 2px 4px; background: white; color: black; } \
     textarea { display: block; border: 2px solid #aaa; padding: 4px; background: white; color: black; height: 60px; }";

const VIEWPORT_W: f32 = 800.0;
const VIEWPORT_H: f32 = 600.0;

fn font() -> Font {
    Font::from_bytes(include_bytes!("../../bweb-text/tests/data/test-font.ttf").to_vec())
        .expect("test font")
}

/// A parsed and laid-out document, ready for style or rendering assertions.
struct Page {
    document: HtmlDocument,
    styles: bweb_css::ComputedStyleMap,
    tree: LayoutTree,
    width: f32,
    height: f32,
}

impl Page {
    fn new(html: &str, css: &str, width: f32, height: f32) -> Self {
        let document = HtmlDocument::parse(html);
        let ua = Stylesheet::parse(UA_CSS).expect("UA stylesheet");
        let author = Stylesheet::parse(css).expect("author stylesheet");
        let styles =
            StyleEngine::new().compute_with_viewport(&document, &[ua, author], width, height);
        let font = font();
        let images = HashMap::new();
        let pseudo = HashMap::new();
        let tree = LayoutTree::build_with_viewport(
            &document,
            &styles,
            width,
            height,
            Some(&font),
            &images,
            &pseudo,
        );
        Self { document, styles, tree, width, height }
    }

    fn node(&self, selector: &str) -> NodeId {
        self.document.query_selector(selector).unwrap_or_else(|| panic!("no node for {selector}"))
    }

    fn style(&self, selector: &str) -> &ComputedStyle {
        self.styles.get(self.node(selector)).unwrap_or_else(|| panic!("no style for {selector}"))
    }

    fn box_for(&self, selector: &str) -> &LayoutBox {
        let target = self.node(selector);
        fn find(box_: &LayoutBox, target: NodeId) -> Option<&LayoutBox> {
            if box_.node == target {
                return Some(box_);
            }
            box_.children.iter().find_map(|child| find(child, target))
        }
        find(&self.tree.root, target).unwrap_or_else(|| panic!("no layout box for {selector}"))
    }

    fn paint(&self) -> (DisplayList, Framebuffer) {
        let mut font = font();
        let runs = prepare_text_runs(&self.tree, &self.styles, &mut font, 1.0);
        let adapter = PaintAdapter::new(
            &self.tree,
            &self.styles,
            bweb_layout::Rect { x: 0.0, y: 0.0, width: self.width, height: self.height },
        )
        .with_text_runs(&runs);
        let display_list = Painter::new(&adapter, 1.0).expect("valid scale").paint();
        let mut framebuffer =
            Framebuffer::new(self.width as u32, self.height as u32).expect("framebuffer");
        framebuffer.clear([255, 255, 255, 255]);
        Rasterizer.rasterize(&display_list, &mut framebuffer);
        (display_list, framebuffer)
    }
}

#[test]
fn flex_card_row_distributes_equal_widths_with_gaps() {
    let page = Page::new(
        "<html><body><div class='cards'>\
           <div id='card1' class='card'></div><div id='card2' class='card'></div>\
           <div id='card3' class='card'></div>\
         </div></body></html>",
        ".cards { display: flex; width: 600px; gap: 20px; } \
         .card { flex: 1; height: 100px; background-color: #3498db; }",
        VIEWPORT_W,
        VIEWPORT_H,
    );
    let cards: Vec<_> = [1, 2, 3].iter().map(|n| page.box_for(&format!("#card{n}"))).collect();
    // 600px minus two 20px gaps, split across three equal flexible cards.
    for card in &cards {
        assert!((card.rect.width - 186.67).abs() < 1.0, "card width {}", card.rect.width);
        assert_eq!(card.rect.height, 100.0);
    }
    assert!(cards[0].rect.x < cards[1].rect.x && cards[1].rect.x < cards[2].rect.x);
    let gap_1 = cards[1].rect.x - (cards[0].rect.x + cards[0].rect.width);
    let gap_2 = cards[2].rect.x - (cards[1].rect.x + cards[1].rect.width);
    assert!((gap_1 - 20.0).abs() < 1.0 && (gap_2 - 20.0).abs() < 1.0, "gaps {gap_1} {gap_2}");
}

#[test]
fn flex_navbar_space_between_spreads_links_to_the_edges() {
    let page = Page::new(
        "<html><body><nav>\
           <a id='home'>Home</a><a id='about'>About</a><a id='contact'>Contact</a>\
         </nav></body></html>",
        "nav { display: flex; width: 400px; justify-content: space-between; }",
        VIEWPORT_W,
        VIEWPORT_H,
    );
    let nav = page.box_for("nav");
    let (home, about, contact) =
        (page.box_for("#home"), page.box_for("#about"), page.box_for("#contact"));
    assert!(home.rect.x < about.rect.x && about.rect.x < contact.rect.x);
    assert!(home.rect.x < 1.0, "first link pinned left: {}", home.rect.x);
    assert!(
        (contact.rect.x + contact.rect.width - (nav.rect.x + nav.rect.width)).abs() < 1.0,
        "last link pinned right"
    );
    let left_gap = about.rect.x - (home.rect.x + home.rect.width);
    let right_gap = contact.rect.x - (about.rect.x + about.rect.width);
    assert!((left_gap - right_gap).abs() < 1.0, "links evenly spread: {left_gap} vs {right_gap}");
}

#[test]
fn grid_gallery_builds_three_equal_fr_columns() {
    let page = Page::new(
        "<html><body><div class='gallery'>\
           <div id='one'></div><div id='two'></div><div id='three'></div>\
         </div></body></html>",
        ".gallery { display: grid; grid-template-columns: repeat(3, 1fr); gap: 10px; width: 320px; }",
        VIEWPORT_W,
        VIEWPORT_H,
    );
    let (one, two, three) = (page.box_for("#one"), page.box_for("#two"), page.box_for("#three"));
    // (320 - 2*10) / 3 = 100px per column.
    for cell in [one, two, three] {
        assert!((cell.rect.width - 100.0).abs() < 1.0, "column width {}", cell.rect.width);
    }
    assert_eq!(one.rect.y, two.rect.y);
    assert_eq!(two.rect.y, three.rect.y);
    assert!(one.rect.x < two.rect.x && two.rect.x < three.rect.x);
}

#[test]
fn common_selectors_and_specificity_resolve_expected_colors() {
    let page = Page::new(
        "<html><body><ul class='list'>\
           <li id='a' class='item'>a</li>\
           <li id='b' class='item' data-role='admin'>b</li>\
           <li id='c' class='item'>c</li>\
         </ul></body></html>",
        ".item { color: blue; } \
         ul .item { color: green; } \
         li:first-child { color: red; } \
         li:nth-child(2) { color: orange; } \
         li[data-role=admin] { color: purple; } \
         #c { color: teal; }",
        VIEWPORT_W,
        VIEWPORT_H,
    );
    // A class wins over a bare tag pseudo-class; a descendant selector has
    // higher specificity than a lone class.
    assert_eq!(page.style("#a").get("color"), Some("red"));
    // `li:nth-child(2)` and `li[data-role=admin]` have equal specificity, so
    // the later declaration wins.
    assert_eq!(page.style("#b").get("color"), Some("purple"));
    // An id selector beats everything else for #c.
    assert_eq!(page.style("#c").get("color"), Some("teal"));
}

#[test]
fn common_shorthands_expand_to_their_longhands() {
    let page = Page::new(
        "<html><body><div id='box'></div></body></html>",
        "#box { margin: 10px 20px; padding: 5px; border: 2px solid #333; \
               font: italic 20px serif; flex: 2 1 auto; gap: 8px; \
               background: red url(pic.png) no-repeat; }",
        VIEWPORT_W,
        VIEWPORT_H,
    );
    let style = page.style("#box");
    assert_eq!(style.get("margin-top"), Some("10px"));
    assert_eq!(style.get("margin-left"), Some("20px"));
    assert_eq!(style.get("padding-bottom"), Some("5px"));
    assert_eq!(style.get("border-width"), Some("2px"));
    assert_eq!(style.get("font-size"), Some("20px"));
    assert_eq!(style.get("flex-grow"), Some("2"));
    assert_eq!(style.get("row-gap"), Some("8px"));
    assert_eq!(style.get("background-color"), Some("red"));
    assert_eq!(style.get("background-repeat"), Some("no-repeat"));
}

#[test]
fn custom_properties_layers_and_media_queries_cascade() {
    // var() substitution and @layer ordering are both common in modern CSS.
    let page = Page::new(
        "<html><body><p id='p'>text</p></body></html>",
        ":root { --brand: #ff6600; } \
         @layer base { p { color: red; } } \
         @layer theme { p { color: blue; } } \
         #p { color: var(--brand); border-color: var(--missing, teal); }",
        VIEWPORT_W,
        VIEWPORT_H,
    );
    assert_eq!(page.style("#p").get("color"), Some("#ff6600"));
    assert_eq!(page.style("#p").get("border-color"), Some("teal"));

    let responsive = |width: f32| {
        let page = Page::new(
            "<html><body><p id='p'>text</p></body></html>",
            "@layer base { p { color: red; } } \
             @layer theme { p { color: blue; } } \
             @media (min-width: 700px) { p { color: green; } }",
            width,
            VIEWPORT_H,
        );
        page.style("#p").get("color").unwrap().to_owned()
    };
    assert_eq!(responsive(800.0), "green", "unlayered @media rule wins when it matches");
    assert_eq!(responsive(600.0), "blue", "later layer wins when @media does not match");
}

#[test]
fn realistic_article_page_paints_backgrounds_shadows_and_text() {
    let page = Page::new(
        "<html><body>\
           <header id='top'><h1>B-Web</h1></header>\
           <main id='content'><p id='lead'>A small but complete page.</p></main>\
         </body></html>",
        "body { background-color: #fafafa; } \
         header { background-color: #2c3e50; padding: 16px; } \
         h1 { color: white; text-shadow: 1px 1px 2px #000; } \
         main { background-color: white; box-shadow: 0 2px 6px rgba(0,0,0,0.25); padding: 12px; } \
         #lead { color: #34495e; }",
        VIEWPORT_W,
        VIEWPORT_H,
    );
    let (display_list, framebuffer) = page.paint();
    // The header band and the white card are actually painted.
    assert!(page.non_background_pixels(&framebuffer, [44, 62, 80, 255]) > 1000);
    assert!(page.non_background_pixels(&framebuffer, [255, 255, 255, 255]) > 1000);
    // Text glyphs reached the display list (heading + lead paragraph).
    let text_items = display_list
        .items
        .iter()
        .filter(|item| matches!(item, bweb_renderer::PaintItem::Text { .. }))
        .count();
    assert!(text_items >= 2, "expected heading and paragraph text, got {text_items}");
}

impl Page {
    fn non_background_pixels(&self, framebuffer: &Framebuffer, color: [u8; 4]) -> usize {
        (0..framebuffer.width())
            .flat_map(|x| (0..framebuffer.height()).map(move |y| (x, y)))
            .filter(|(x, y)| framebuffer.pixel(*x, *y) == color)
            .count()
    }
}
