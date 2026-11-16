//! Adapter from the current layout tree and computed CSS values to the renderer port.

use std::{collections::HashMap, sync::Arc};

use crate::{LayoutBox, LayoutTree, Rect};
use bweb_css::ComputedStyleMap;
use bweb_html::NodeId;
use bweb_renderer::{Color, PaintLayout, PaintNode, PaintStyle, Rect as PaintRect, TextRun};

/// Exposes a layout tree as an immutable paint-order visitor.
pub struct PaintAdapter<'a> {
    pub tree: &'a LayoutTree,
    pub styles: &'a ComputedStyleMap,
    pub viewport: Rect,
    text_runs: Option<&'a HashMap<NodeId, Arc<TextRun>>>,
}

impl<'a> PaintAdapter<'a> {
    /// Creates an adapter without prepared text runs.
    #[must_use]
    pub fn new(tree: &'a LayoutTree, styles: &'a ComputedStyleMap, viewport: Rect) -> Self {
        Self { tree, styles, viewport, text_runs: None }
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
            };

            visitor(PaintNode {
                depth,
                rect: to_paint_rect(layout.rect),
                style,
                text: text_runs.and_then(|runs| runs.get(&layout.node).cloned()),
            });

            for child in &layout.children {
                walk(child, styles, text_runs, depth.saturating_add(1), visitor);
            }
        }

        walk(&self.tree.root, self.styles, self.text_runs, 0, visitor);
    }
}

fn to_paint_rect(rect: Rect) -> PaintRect {
    PaintRect { x: rect.x, y: rect.y, width: rect.width, height: rect.height }
}

/// Parses the small, explicit CSS color subset currently supported by painting.
fn parse_color(value: &str) -> Option<Color> {
    let value = value.trim().to_ascii_lowercase();
    match value.as_str() {
        "transparent" => return None,
        "black" => return Some(Color::rgba(0, 0, 0, 255)),
        "white" => return Some(Color::rgba(255, 255, 255, 255)),
        "red" => return Some(Color::rgba(255, 0, 0, 255)),
        "green" => return Some(Color::rgba(0, 128, 0, 255)),
        "blue" => return Some(Color::rgba(0, 0, 255, 255)),
        "yellow" => return Some(Color::rgba(255, 255, 0, 255)),
        "gray" | "grey" => return Some(Color::rgba(128, 128, 128, 255)),
        _ => {}
    }

    let hex = value.strip_prefix('#')?;
    let expand = |digit: u8| digit * 17;
    let channels = match hex.len() {
        3 => {
            let values: Vec<u8> = hex
                .bytes()
                .map(|byte| (byte as char).to_digit(16).map(|v| v as u8))
                .collect::<Option<_>>()?;
            [expand(values[0]), expand(values[1]), expand(values[2]), 255]
        }
        4 => {
            let values: Vec<u8> = hex
                .bytes()
                .map(|byte| (byte as char).to_digit(16).map(|v| v as u8))
                .collect::<Option<_>>()?;
            [expand(values[0]), expand(values[1]), expand(values[2]), expand(values[3])]
        }
        6 | 8 => {
            let mut values = [0_u8; 4];
            let mut bytes = hex.bytes();
            for channel in values.iter_mut().take(if hex.len() == 8 { 4 } else { 3 }) {
                let high = (bytes.next()? as char).to_digit(16)? as u8;
                let low = (bytes.next()? as char).to_digit(16)? as u8;
                *channel = high * 16 + low;
            }
            if hex.len() == 6 {
                values[3] = 255;
            }
            values
        }
        _ => return None,
    };
    Some(Color::rgba(channels[0], channels[1], channels[2], channels[3]))
}

#[cfg(test)]
mod tests {
    use super::{PaintAdapter, parse_color};
    use crate::{LayoutTree, Rect};
    use bweb_css::{StyleEngine, Stylesheet};
    use bweb_html::{HtmlDocument, NodeId, NodeKind};
    use bweb_renderer::{Color, PaintLayout, TextRun};
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
