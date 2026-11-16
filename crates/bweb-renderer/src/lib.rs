//! Backend-independent layout tree and CSS box-model foundations.
//!
//! Layout uses logical CSS pixels. Inline formatting is intentionally a
//! first-pass flow model; text shaping, line wrapping, floats and positioning
//! are later milestones.

use bweb_css::ComputedStyleMap;
use bweb_html::{HtmlDocument, NodeId, NodeKind};

/// A rectangle in CSS logical pixels.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Four-sided CSS box edges, stored as top/right/bottom/left.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Edges {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

/// Simplified display category used by the first layout pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Display {
    Block,
    Inline,
}

/// A geometric box associated with one DOM arena node.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutBox {
    pub node: NodeId,
    pub display: Display,
    /// Border-box geometry.
    pub rect: Rect,
    pub margin: Edges,
    pub padding: Edges,
    pub border: Edges,
    pub children: Vec<LayoutBox>,
    pub line_boxes: Vec<LineBox>,
}

/// A line box emitted by the initial inline formatting pass.
#[derive(Debug, Clone, PartialEq)]
pub struct LineBox {
    pub rect: Rect,
    pub fragments: Vec<TextFragment>,
}

/// A text run retaining the DOM node that supplied it.
#[derive(Debug, Clone, PartialEq)]
pub struct TextFragment {
    pub node: NodeId,
    pub text: String,
    pub rect: Rect,
}

/// Layout tree rooted at the DOM document node.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutTree {
    pub root: LayoutBox,
    pub box_count: usize,
}

impl LayoutTree {
    /// Formats the geometry tree and inline formatting output for inspection.
    #[must_use]
    pub fn dump(&self, document: &HtmlDocument) -> String {
        use std::fmt::Write as _;
        let mut output = String::new();
        let _ = writeln!(output, "Layout Tree (CSS px) — {} boxes", self.box_count);
        dump_box(&mut output, document, &self.root, 0);
        output
    }

    /// Builds a simple normal-flow layout for the supplied viewport width.
    #[must_use]
    pub fn build(document: &HtmlDocument, styles: &ComputedStyleMap, viewport_width: f32) -> Self {
        let width = finite_non_negative(viewport_width);
        let mut count = 0;
        let root = layout_node(document, styles, document.root(), 0.0, 0.0, width, &mut count)
            .unwrap_or_else(|| LayoutBox {
                node: document.root(),
                display: Display::Block,
                rect: Rect { x: 0.0, y: 0.0, width, height: 0.0 },
                margin: Edges::default(),
                padding: Edges::default(),
                border: Edges::default(),
                children: Vec::new(),
                line_boxes: Vec::new(),
            });
        Self { root, box_count: count }
    }
}

fn layout_node(
    document: &HtmlDocument,
    styles: &ComputedStyleMap,
    id: NodeId,
    x: f32,
    y: f32,
    available_width: f32,
    count: &mut usize,
) -> Option<LayoutBox> {
    let node = document.node(id)?;
    let style = styles.get(id);
    if style.and_then(|style| style.get("display")) == Some("none") {
        return None;
    }
    let is_text = matches!(node.kind(), NodeKind::Text(_));
    let display = if is_text || style.and_then(|style| style.get("display")) == Some("inline") {
        Display::Inline
    } else {
        Display::Block
    };
    let margin = parse_edges(style.and_then(|style| style.get("margin")));
    let padding = parse_edges(style.and_then(|style| style.get("padding")));
    let border = parse_border_widths(style);
    let content_x = x + margin.left + border.left + padding.left;
    let content_y = y + margin.top + border.top + padding.top;
    let available_outer_width = (available_width - margin.left - margin.right).max(0.0);
    let horizontal_insets = padding.left + padding.right + border.left + border.right;
    let box_sizing = style.and_then(|style| style.get("box-sizing")).unwrap_or("content-box");
    let specified_width = style.and_then(|style| style.get("width")).and_then(parse_length);
    let outer_width = match specified_width {
        Some(width) if box_sizing == "border-box" => width.min(available_outer_width),
        Some(width) => (width + horizontal_insets).min(available_outer_width),
        None => available_outer_width,
    };
    let content_width = (outer_width - horizontal_insets).max(0.0);

    *count += 1;
    let mut children = Vec::new();
    let mut cursor_y = content_y;
    let mut inline_x = content_x;
    let mut content_height: f32 = 0.0;
    for child_id in node.children() {
        let Some(child_node) = document.node(*child_id) else { continue };
        let child_is_text = matches!(child_node.kind(), NodeKind::Text(_));
        let child_display = styles.get(*child_id).and_then(|computed| computed.get("display"));
        if child_display == Some("none") {
            continue;
        }
        let child_is_inline = child_is_text || child_display == Some("inline");
        if child_is_inline {
            let text_width = match child_node.kind() {
                NodeKind::Text(text) => text.chars().count() as f32 * 8.0,
                _ => content_width.min(80.0),
            };
            let child_width = text_width.min(content_width);
            if inline_x + child_width > content_x + content_width && inline_x > content_x {
                cursor_y += 19.2;
                inline_x = content_x;
            }
            if let Some(child) =
                layout_node(document, styles, *child_id, inline_x, cursor_y, child_width, count)
            {
                inline_x += child.rect.width;
                content_height = content_height.max(cursor_y - content_y + child.rect.height);
                children.push(child);
            }
        } else {
            if inline_x > content_x {
                cursor_y += 19.2;
                inline_x = content_x;
            }
            if let Some(child) =
                layout_node(document, styles, *child_id, content_x, cursor_y, content_width, count)
            {
                cursor_y = child.rect.y + child.rect.height + child.margin.bottom;
                content_height = cursor_y - content_y;
                children.push(child);
            }
        }
    }

    let line_boxes =
        build_line_boxes(document, &children, content_x, content_y, content_width, style);
    let inline_height: f32 = line_boxes.iter().map(|line| line.rect.height).sum();
    content_height = content_height.max(inline_height);

    let explicit_height = style.and_then(|style| style.get("height")).and_then(parse_length);
    let vertical_insets = padding.top + padding.bottom + border.top + border.bottom;
    let used_content_height =
        explicit_height.unwrap_or(content_height.max(if is_text { 19.2 } else { 0.0 }));
    let height = match (explicit_height, box_sizing) {
        (Some(value), "border-box") => value,
        _ => used_content_height + vertical_insets,
    };
    let is_document = matches!(node.kind(), NodeKind::Document);
    let width = if is_document { available_width } else { outer_width };
    Some(LayoutBox {
        node: id,
        display,
        rect: Rect { x: x + margin.left, y: y + margin.top, width, height },
        margin,
        padding,
        border,
        children,
        line_boxes,
    })
}

fn build_line_boxes(
    document: &HtmlDocument,
    children: &[LayoutBox],
    origin_x: f32,
    origin_y: f32,
    available_width: f32,
    style: Option<&bweb_css::ComputedStyle>,
) -> Vec<LineBox> {
    let white_space = style.and_then(|s| s.get("white-space")).unwrap_or("normal");
    let font_size =
        style.and_then(|s| s.get("font-size")).and_then(parse_length).unwrap_or(16.0).max(1.0);
    let line_height = style
        .and_then(|s| s.get("line-height"))
        .and_then(parse_length)
        .unwrap_or(font_size * 1.2)
        .max(1.0);
    let glyph_width = font_size * 0.5;
    let preserve = matches!(white_space, "pre" | "pre-wrap");
    let preserve_newlines = matches!(white_space, "pre" | "pre-wrap" | "pre-line");
    let wrap = !matches!(white_space, "nowrap" | "pre");
    let mut lines = Vec::new();
    let mut fragments = Vec::new();
    let mut x = origin_x;
    let mut y = origin_y;
    let mut line_width: f32 = 0.0;
    let mut pending_space = false;

    for child in children.iter().filter(|child| child.display == Display::Inline) {
        let mut text = String::new();
        collect_text(document, child.node, &mut text);
        if text.is_empty() {
            continue;
        }
        let mut run = String::new();
        let mut run_width = 0.0;
        let chars: Vec<char> = text.chars().collect();
        let mut index = 0;
        while index < chars.len() {
            let ch = chars[index];
            if ch == '\n' && preserve_newlines {
                if !run.is_empty() {
                    fragments.push(TextFragment {
                        node: child.node,
                        text: std::mem::take(&mut run),
                        rect: Rect { x, y, width: run_width, height: line_height },
                    });
                    x += run_width;
                    line_width = x - origin_x;
                    run_width = 0.0;
                }
                if !fragments.is_empty() {
                    lines.push(LineBox {
                        rect: Rect { x: origin_x, y, width: line_width, height: line_height },
                        fragments: std::mem::take(&mut fragments),
                    });
                }
                y += line_height;
                x = origin_x;
                line_width = 0.0;
                pending_space = false;
                index += 1;
                continue;
            }
            if ch.is_whitespace() && !preserve {
                if !run.is_empty() {
                    fragments.push(TextFragment {
                        node: child.node,
                        text: std::mem::take(&mut run),
                        rect: Rect { x, y, width: run_width, height: line_height },
                    });
                    x += run_width;
                    line_width = x - origin_x;
                    run_width = 0.0;
                }
                let mut end = index + 1;
                while end < chars.len() && chars[end].is_whitespace() && chars[end] != '\n' {
                    end += 1;
                }
                if !pending_space && x > origin_x {
                    fragments.push(TextFragment {
                        node: child.node,
                        text: " ".to_owned(),
                        rect: Rect { x, y, width: glyph_width, height: line_height },
                    });
                    x += glyph_width;
                    line_width = x - origin_x;
                }
                pending_space = true;
                index = end;
                continue;
            }
            pending_space = false;
            run.push(ch);
            run_width += glyph_width;
            index += 1;
            let boundary = index == chars.len() || chars[index].is_whitespace();
            if boundary && wrap && x > origin_x && x + run_width > origin_x + available_width {
                run.pop();
                run_width -= glyph_width;
                if !run.is_empty() {
                    fragments.push(TextFragment {
                        node: child.node,
                        text: std::mem::take(&mut run),
                        rect: Rect { x, y, width: run_width, height: line_height },
                    });
                    x += run_width;
                    line_width = x - origin_x;
                }
                if !fragments.is_empty() {
                    lines.push(LineBox {
                        rect: Rect { x: origin_x, y, width: line_width, height: line_height },
                        fragments: std::mem::take(&mut fragments),
                    });
                }
                y += line_height;
                x = origin_x;
                line_width = 0.0;
                run.push(ch);
                run_width = glyph_width;
            }
        }
        if !run.is_empty() {
            fragments.push(TextFragment {
                node: child.node,
                text: run,
                rect: Rect { x, y, width: run_width, height: line_height },
            });
            x += run_width;
            line_width = x - origin_x;
        }
    }
    if !fragments.is_empty() {
        lines.push(LineBox {
            rect: Rect { x: origin_x, y, width: line_width, height: line_height },
            fragments,
        });
    }
    lines
}

fn collect_text(document: &HtmlDocument, id: NodeId, output: &mut String) {
    let Some(node) = document.node(id) else { return };
    if let NodeKind::Text(value) = node.kind() {
        output.push_str(value);
    }
    for child in node.children() {
        collect_text(document, *child, output);
    }
}

fn dump_box(output: &mut String, document: &HtmlDocument, layout: &LayoutBox, depth: usize) {
    use std::fmt::Write as _;
    let indent = "  ".repeat(depth);
    let label = match document.node(layout.node).map(|node| node.kind()) {
        Some(NodeKind::Document) => "#document".to_owned(),
        Some(NodeKind::Element { tag_name, .. }) => tag_name.clone(),
        Some(NodeKind::Text(text)) => format!("#text {:?}", text.trim()),
        Some(NodeKind::Comment(_)) => "#comment".to_owned(),
        None => format!("#node-{}", layout.node.index()),
    };
    let _ = writeln!(
        output,
        "{indent}{label} {:?} x={:.1} y={:.1} w={:.1} h={:.1}",
        layout.display, layout.rect.x, layout.rect.y, layout.rect.width, layout.rect.height
    );
    for line in &layout.line_boxes {
        let _ = writeln!(
            output,
            "{indent}  LineBox x={:.1} y={:.1} w={:.1} h={:.1}",
            line.rect.x, line.rect.y, line.rect.width, line.rect.height
        );
        for fragment in &line.fragments {
            let _ = writeln!(
                output,
                "{indent}    Fragment node={} {:?} x={:.1} y={:.1} w={:.1} h={:.1}",
                fragment.node.index(),
                fragment.text,
                fragment.rect.x,
                fragment.rect.y,
                fragment.rect.width,
                fragment.rect.height
            );
        }
    }
    for child in &layout.children {
        dump_box(output, document, child, depth + 1);
    }
}

fn parse_edges(value: Option<&str>) -> Edges {
    let Some(value) = value else { return Edges::default() };
    let values: Vec<f32> = value.split_ascii_whitespace().filter_map(parse_length).collect();
    match values.as_slice() {
        [all] => Edges { top: *all, right: *all, bottom: *all, left: *all },
        [vertical, horizontal] => {
            Edges { top: *vertical, right: *horizontal, bottom: *vertical, left: *horizontal }
        }
        [top, horizontal, bottom] => {
            Edges { top: *top, right: *horizontal, bottom: *bottom, left: *horizontal }
        }
        [top, right, bottom, left, ..] => {
            Edges { top: *top, right: *right, bottom: *bottom, left: *left }
        }
        _ => Edges::default(),
    }
}

fn parse_border_widths(style: Option<&bweb_css::ComputedStyle>) -> Edges {
    let Some(style) = style else { return Edges::default() };
    let mut edges = parse_edges(style.get("border-width"));
    if let Some(value) = style.get("border-top-width").and_then(parse_length) {
        edges.top = value;
    }
    if let Some(value) = style.get("border-right-width").and_then(parse_length) {
        edges.right = value;
    }
    if let Some(value) = style.get("border-bottom-width").and_then(parse_length) {
        edges.bottom = value;
    }
    if let Some(value) = style.get("border-left-width").and_then(parse_length) {
        edges.left = value;
    }
    edges
}

fn parse_length(value: &str) -> Option<f32> {
    let value = value.trim();
    let numeric = value.strip_suffix("px").unwrap_or(value).trim();
    numeric.parse::<f32>().ok().filter(|number| number.is_finite())
}

fn finite_non_negative(value: f32) -> f32 {
    if value.is_finite() { value.max(0.0) } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::{Display, LayoutTree};
    use bweb_css::{StyleEngine, Stylesheet};
    use bweb_html::{HtmlDocument, NodeKind};

    fn layout(markup: &str, css: &str) -> (HtmlDocument, LayoutTree) {
        let document = HtmlDocument::parse(markup);
        let sheet = Stylesheet::parse(css).expect("valid CSS");
        let styles = StyleEngine::new().compute(&document, &[sheet]);
        let tree = LayoutTree::build(&document, &styles, 800.0);
        (document, tree)
    }

    #[test]
    fn display_none_subtrees_are_excluded() {
        let (document, tree) = layout(
            "<main><aside><b>hidden</b></aside><p>shown</p></main>",
            "aside { display:none }",
        );
        let aside = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "aside")
                    .then_some(id)
            })
            .expect("aside");
        assert!(!contains(&tree.root, aside));
    }

    #[test]
    fn block_and_inline_boxes_are_distinguished() {
        let (document, tree) = layout(
            "<main><span>hello</span></main>",
            "main { display:block } span { display:inline }",
        );
        let span = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "span")
                    .then_some(id)
            })
            .expect("span");
        assert_eq!(find(&tree.root, span).expect("span box").display, Display::Inline);
    }

    #[test]
    fn computes_container_geometry_with_margin_and_padding() {
        let (document, tree) = layout(
            "<main></main>",
            "main { display:block; margin:10px 20px; padding:5px; height:40px }",
        );
        let main = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "main")
                    .then_some(id)
            })
            .expect("main");
        let box_node = find(&tree.root, main).expect("main box");
        assert_eq!(box_node.rect.x, 20.0);
        assert_eq!(box_node.rect.y, 10.0);
        assert_eq!(box_node.rect.width, 760.0);
        assert_eq!(box_node.rect.height, 50.0);
        assert_eq!(box_node.padding.left, 5.0);
        assert_eq!(box_node.border.left, 0.0);
    }

    #[test]
    fn includes_border_and_respects_content_box_width() {
        let (document, tree) = layout(
            "<main></main>",
            "main { display:block; width:200px; padding:10px; border-width:2px }",
        );
        let main = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "main")
                    .then_some(id)
            })
            .expect("main");
        let box_node = find(&tree.root, main).expect("main box");
        assert_eq!(box_node.rect.width, 224.0);
        assert_eq!(box_node.padding.left, 10.0);
        assert_eq!(box_node.border.left, 2.0);
    }

    #[test]
    fn border_box_width_includes_padding_and_border() {
        let (document, tree) = layout(
            "<main></main>",
            "main { display:block; box-sizing:border-box; width:200px; padding:10px; border-width:2px }",
        );
        let main = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "main")
                    .then_some(id)
            })
            .expect("main");
        let box_node = find(&tree.root, main).expect("main box");
        assert_eq!(box_node.rect.width, 200.0);
    }

    #[test]
    fn creates_line_boxes_and_preserves_text_node_identity() {
        let (document, tree) = layout("<main>hello   world</main>", "main { display:block }");
        let main = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "main")
                    .then_some(id)
            })
            .expect("main");
        let main_box = find(&tree.root, main).expect("main layout box");
        assert_eq!(main_box.line_boxes.len(), 1);
        let fragments = &main_box.line_boxes[0].fragments;
        assert!(fragments.iter().any(|fragment| fragment.text == "hello"));
        assert!(fragments.iter().any(|fragment| fragment.text == "world"));
        let text_id = document
            .iter()
            .find_map(|(id, node)| matches!(node.kind(), NodeKind::Text(_)).then_some(id))
            .expect("text node");
        assert!(fragments.iter().all(|fragment| fragment.node == text_id));
    }

    #[test]
    fn pre_white_space_preserves_newlines_as_line_breaks() {
        let (document, tree) =
            layout("<main>first\nsecond</main>", "main { display:block; white-space:pre }");
        let main = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "main")
                    .then_some(id)
            })
            .expect("main");
        let main_box = find(&tree.root, main).expect("main layout box");
        assert_eq!(main_box.line_boxes.len(), 2);
        assert_eq!(main_box.line_boxes[0].fragments[0].text, "first");
        assert_eq!(main_box.line_boxes[1].fragments[0].text, "second");
    }

    fn contains(root: &super::LayoutBox, id: bweb_html::NodeId) -> bool {
        root.node == id || root.children.iter().any(|child| contains(child, id))
    }

    fn find(root: &super::LayoutBox, id: bweb_html::NodeId) -> Option<&super::LayoutBox> {
        if root.node == id {
            return Some(root);
        }
        root.children.iter().find_map(|child| find(child, id))
    }
}
