//! DOM-backed layout tree and first-pass CSS box-model implementation.
//!
//! Layout uses logical CSS pixels. Inline formatting is intentionally a
//! first-pass flow model; text shaping, line wrapping, floats and positioning
//! are tracked in the roadmap.

mod paint_adapter;
mod text_layout;
mod view;
pub use paint_adapter::PaintAdapter;
pub use text_layout::{TextLayout, layout_text, prepare_text_runs};
pub use view::{ResizeOutcome, View, ViewBackend, Viewport};

use std::{collections::HashMap, sync::Arc};

use bweb_css::{ComputedStyle, ComputedStyleMap};
use bweb_html::{HtmlDocument, Node, NodeId, NodeKind};
use bweb_text::Font;

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
    Flex,
    Grid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Position {
    Static,
    Relative,
    Sticky,
    Absolute,
    Fixed,
}

fn parse_position(style: Option<&bweb_css::ComputedStyle>) -> Position {
    match style.and_then(|style| style.get("position")) {
        Some("relative") => Position::Relative,
        Some("sticky") => Position::Sticky,
        Some("absolute") => Position::Absolute,
        Some("fixed") => Position::Fixed,
        _ => Position::Static,
    }
}

/// A sticky shift is applied only after scrolling past its constraint rectangle.
/// `bweb-layout` has no scroll state, so it records the threshold and the shell
/// applies the shift at paint time.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StickyShift {
    pub active: bool,
    pub threshold: f32,
    pub dx: f32,
    pub dy: f32,
}

fn parse_offset(style: Option<&bweb_css::ComputedStyle>, property: &str) -> Option<f32> {
    style.and_then(|style| style.get(property)).and_then(parse_length)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FloatSide {
    Left,
    Right,
}

fn parse_float(style: Option<&bweb_css::ComputedStyle>) -> Option<FloatSide> {
    match style.and_then(|style| style.get("float")) {
        Some("left") => Some(FloatSide::Left),
        Some("right") => Some(FloatSide::Right),
        _ => None,
    }
}

fn parse_clear(style: Option<&bweb_css::ComputedStyle>) -> (bool, bool) {
    match style.and_then(|style| style.get("clear")) {
        Some("left") => (true, false),
        Some("right") => (false, true),
        Some("both") => (true, true),
        _ => (false, false),
    }
}

/// A geometric box associated with one DOM arena node.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutBox {
    pub node: NodeId,
    pub display: Display,
    /// Border-box geometry including any relative-position offset.
    pub rect: Rect,
    /// Border-box geometry before relative offsets, used by parent flow.
    pub flow_rect: Rect,
    /// Whether the box is removed from normal flow (`absolute`/`fixed`).
    pub out_of_flow: bool,
    /// Scroll-dependent shift for `position: sticky`.
    pub sticky: StickyShift,
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
    /// Style override for generated `::before`/`::after` content.
    pub style: Option<Arc<ComputedStyle>>,
}

/// Generated content for one element.
#[derive(Debug, Clone, PartialEq)]
pub struct GeneratedText {
    pub text: String,
    pub style: Arc<ComputedStyle>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PseudoContent {
    pub before: Option<GeneratedText>,
    pub after: Option<GeneratedText>,
}

/// Layout tree rooted at the DOM document node.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutTree {
    pub root: LayoutBox,
    pub box_count: usize,
}

/// Intrinsic size of a decoded image, in CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImageSize {
    pub width: f32,
    pub height: f32,
}

/// Text advance provider used by both block sizing and inline formatting.
///
/// With a font, advances are the real glyph metrics used later during text
/// preparation. Without one, the documented first-pass approximation of half
/// the font size per character is used so pure CSS/DOM tests stay lightweight.
struct TextMeasurer<'a> {
    font: Option<&'a Font>,
}

impl TextMeasurer<'_> {
    fn char_advance(&self, character: char, px_size: f32) -> f32 {
        match self.font {
            Some(font) => {
                font.glyph_for(character).map_or(0.0, |glyph| font.advance(glyph, px_size))
            }
            None => px_size * 0.5,
        }
    }

    fn text(&self, text: &str, px_size: f32) -> f32 {
        match self.font {
            Some(font) => font.shape(text, px_size).iter().map(|glyph| glyph.advance).sum(),
            None => text.chars().count() as f32 * px_size * 0.5,
        }
    }
}

struct LayoutContext<'a> {
    document: &'a HtmlDocument,
    styles: &'a ComputedStyleMap,
    measurer: &'a TextMeasurer<'a>,
    images: Option<&'a HashMap<NodeId, ImageSize>>,
    pseudo: Option<&'a HashMap<NodeId, PseudoContent>>,
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

    /// Builds a simple normal-flow layout with approximate text advances.
    #[must_use]
    pub fn build(document: &HtmlDocument, styles: &ComputedStyleMap, viewport_width: f32) -> Self {
        let images = HashMap::new();
        let pseudo = HashMap::new();
        Self::build_with_all(document, styles, viewport_width, None, &images, &pseudo)
    }

    /// Builds a layout whose text metrics come from the same font used to
    /// rasterize glyphs, so fragment widths, wraps and painted text agree.
    #[must_use]
    pub fn build_with_font(
        document: &HtmlDocument,
        styles: &ComputedStyleMap,
        viewport_width: f32,
        font: &Font,
    ) -> Self {
        let images = HashMap::new();
        let pseudo = HashMap::new();
        Self::build_with_all(document, styles, viewport_width, Some(font), &images, &pseudo)
    }

    /// Builds a layout with approximate text advances and decoded images.
    #[must_use]
    pub fn build_with_images(
        document: &HtmlDocument,
        styles: &ComputedStyleMap,
        viewport_width: f32,
        images: &HashMap<NodeId, ImageSize>,
    ) -> Self {
        let pseudo = HashMap::new();
        Self::build_with_all(document, styles, viewport_width, None, images, &pseudo)
    }

    /// Builds a layout with real font advances and decoded images.
    #[must_use]
    pub fn build_with_font_and_images(
        document: &HtmlDocument,
        styles: &ComputedStyleMap,
        viewport_width: f32,
        font: &Font,
        images: &HashMap<NodeId, ImageSize>,
    ) -> Self {
        let pseudo = HashMap::new();
        Self::build_with_all(document, styles, viewport_width, Some(font), images, &pseudo)
    }

    /// Builds a layout with real font advances, decoded images and generated
    /// `::before`/`::after` content.
    #[must_use]
    pub fn build_with_all(
        document: &HtmlDocument,
        styles: &ComputedStyleMap,
        viewport_width: f32,
        font: Option<&Font>,
        images: &HashMap<NodeId, ImageSize>,
        pseudo: &HashMap<NodeId, PseudoContent>,
    ) -> Self {
        let measurer = TextMeasurer { font };
        let width = finite_non_negative(viewport_width);
        let mut count = 0;
        let context = LayoutContext {
            document,
            styles,
            measurer: &measurer,
            images: Some(images),
            pseudo: Some(pseudo),
        };
        let viewport = Rect { x: 0.0, y: 0.0, width, height: width };
        let root = layout_node(&context, document.root(), 0.0, 0.0, width, viewport, &mut count)
            .unwrap_or_else(|| LayoutBox {
                node: document.root(),
                display: Display::Block,
                rect: Rect { x: 0.0, y: 0.0, width, height: 0.0 },
                flow_rect: Rect { x: 0.0, y: 0.0, width, height: 0.0 },
                out_of_flow: false,
                sticky: StickyShift::default(),
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
    context: &LayoutContext,
    id: NodeId,
    x: f32,
    y: f32,
    available_width: f32,
    containing_block: Rect,
    count: &mut usize,
) -> Option<LayoutBox> {
    let document = context.document;
    let styles = context.styles;
    let node = document.node(id)?;
    let style = styles.get(id);
    if style.and_then(|style| style.get("display")) == Some("none") {
        return None;
    }
    if matches!(
        node.kind(),
        NodeKind::Element { tag_name, .. } if is_non_rendered_element(tag_name)
    ) {
        return None;
    }
    let is_text = matches!(node.kind(), NodeKind::Text(_));
    let position = parse_position(style);
    let display_value = style.and_then(|style| style.get("display"));
    let is_flex = matches!(display_value, Some("flex" | "inline-flex"));
    let is_grid = matches!(display_value, Some("grid" | "inline-grid"));
    let display = if is_text || display_value == Some("inline") {
        Display::Inline
    } else if is_flex {
        Display::Flex
    } else if is_grid {
        Display::Grid
    } else {
        Display::Block
    };
    let mut margin = parse_edges(style, "margin");
    let padding = parse_edges(style, "padding");
    let border = parse_border_widths(style);
    let content_x = x + margin.left + border.left + padding.left;
    let content_y = y + margin.top + border.top + padding.top;
    let available_outer_width = (available_width - margin.left - margin.right).max(0.0);
    let horizontal_insets = padding.left + padding.right + border.left + border.right;
    let box_sizing = style.and_then(|style| style.get("box-sizing")).unwrap_or("content-box");
    let font_size = style
        .and_then(|style| style.get("font-size"))
        .and_then(|value| parse_css_length(value, 16.0))
        .unwrap_or(16.0);
    let specified_width = style
        .and_then(|style| style.get("width"))
        .and_then(|value| parse_css_length(value, font_size));
    let mut outer_width = match specified_width {
        Some(width) if box_sizing == "border-box" => width.min(available_outer_width),
        Some(width) => (width + horizontal_insets).min(available_outer_width),
        None => available_outer_width,
    };
    if let Some(max_width) = style
        .and_then(|style| style.get("max-width"))
        .and_then(|value| parse_css_length(value, font_size))
    {
        let max_outer_width = max_width + horizontal_insets;
        outer_width = outer_width.min(max_outer_width).min(available_outer_width);
    }
    let (auto_left, auto_right) = horizontal_auto_margins(style);
    let remaining = (available_outer_width - outer_width).max(0.0);
    match (auto_left, auto_right) {
        (true, true) => {
            margin.left += remaining / 2.0;
            margin.right += remaining / 2.0;
        }
        (true, false) => margin.left += remaining,
        (false, true) => margin.right += remaining,
        (false, false) => {}
    }
    let image_size = context
        .images
        .and_then(|images| images.get(&id))
        .copied()
        .map(|intrinsic| sized_image(node, intrinsic, available_outer_width));
    if let Some(size) = image_size {
        outer_width = size.width.min(available_outer_width);
    }
    let content_width = (outer_width - horizontal_insets).max(0.0);

    *count += 1;
    let child_containing_block = if matches!(position, Position::Static) {
        containing_block
    } else {
        Rect {
            x: content_x,
            y: content_y,
            width: content_width,
            height: style
                .and_then(|s| s.get("height"))
                .and_then(parse_length)
                .unwrap_or(containing_block.height),
        }
    };
    let (mut children, flow_height) = if is_flex {
        layout_flex_children(
            context,
            id,
            content_x,
            content_y,
            content_width,
            child_containing_block,
            style,
            count,
        )
    } else if is_grid {
        layout_grid_children(
            context,
            id,
            content_x,
            content_y,
            content_width,
            child_containing_block,
            style,
            count,
        )
    } else {
        layout_flow_children(
            context,
            id,
            content_x,
            content_y,
            content_width,
            child_containing_block,
            count,
        )
    };
    let mut content_height = image_size.map_or(0.0, |size| size.height).max(flow_height);

    let mut line_boxes =
        if display == Display::Block && matches!(node.kind(), NodeKind::Element { .. }) {
            build_line_boxes(
                context,
                id,
                &children,
                LineGeometry {
                    origin_x: content_x,
                    origin_y: content_y,
                    available_width: content_width,
                    style,
                    pseudo: context.pseudo.and_then(|pseudo| pseudo.get(&id)),
                },
            )
        } else {
            Vec::new()
        };
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
    let flow_rect = Rect { x: x + margin.left, y: y + margin.top, width, height };
    let mut rect = flow_rect;
    let mut sticky = StickyShift::default();
    if position == Position::Sticky {
        let dx = parse_offset(style, "left")
            .or_else(|| parse_offset(style, "right").map(|value| -value))
            .unwrap_or(0.0);
        let dy = parse_offset(style, "top")
            .or_else(|| parse_offset(style, "bottom").map(|value| -value))
            .unwrap_or(0.0);
        sticky = StickyShift { active: dx != 0.0 || dy != 0.0, threshold: rect.y - dy, dx, dy };
    }
    if position == Position::Relative {
        let dx = parse_offset(style, "left")
            .or_else(|| parse_offset(style, "right").map(|value| -value))
            .unwrap_or(0.0);
        let dy = parse_offset(style, "top")
            .or_else(|| parse_offset(style, "bottom").map(|value| -value))
            .unwrap_or(0.0);
        for child in &mut children {
            shift_box(child, dx, dy);
        }
        for line in &mut line_boxes {
            line.rect.x += dx;
            line.rect.y += dy;
            for fragment in &mut line.fragments {
                fragment.rect.x += dx;
                fragment.rect.y += dy;
            }
        }
        rect.x += dx;
        rect.y += dy;
    }
    Some(LayoutBox {
        node: id,
        display,
        rect,
        flow_rect,
        out_of_flow: matches!(position, Position::Absolute | Position::Fixed),
        sticky,
        margin,
        padding,
        border,
        children,
        line_boxes,
    })
}

fn shift_box(box_: &mut LayoutBox, dx: f32, dy: f32) {
    box_.rect.x += dx;
    box_.rect.y += dy;
    for child in &mut box_.children {
        shift_box(child, dx, dy);
    }
    for line in &mut box_.line_boxes {
        line.rect.x += dx;
        line.rect.y += dy;
        for fragment in &mut line.fragments {
            fragment.rect.x += dx;
            fragment.rect.y += dy;
        }
    }
}

fn float_bounds(
    floats: &[(Rect, FloatSide)],
    line_y: f32,
    line_height: f32,
    left: f32,
    right: f32,
) -> (f32, f32) {
    let mut start = left;
    let mut end = right;
    for (rect, side) in floats {
        if rect.y < line_y + line_height && rect.y + rect.height > line_y {
            match side {
                FloatSide::Left => start = start.max(rect.x + rect.width),
                FloatSide::Right => end = end.min(rect.x),
            }
        }
    }
    (start, end)
}

fn layout_flow_children(
    context: &LayoutContext,
    id: NodeId,
    content_x: f32,
    content_y: f32,
    content_width: f32,
    containing_block: Rect,
    count: &mut usize,
) -> (Vec<LayoutBox>, f32) {
    let Some(node) = context.document.node(id) else {
        return (Vec::new(), 0.0);
    };
    let document = context.document;
    let styles = context.styles;
    let measurer = context.measurer;
    let mut children = Vec::new();
    let mut floats: Vec<(Rect, FloatSide)> = Vec::new();
    let mut cursor_y = content_y;
    let mut inline_x = content_x;
    let mut inline_line_height: f32 = 0.0;
    let mut content_height: f32 = 0.0;
    let mut out_of_flow = Vec::new();
    for child_id in node.children() {
        let Some(child_node) = document.node(*child_id) else { continue };
        let child_is_text = matches!(child_node.kind(), NodeKind::Text(_));
        let child_style = styles.get(*child_id);
        let child_display = child_style.and_then(|computed| computed.get("display"));
        if child_display == Some("none") {
            continue;
        }
        let child_position = parse_position(child_style);
        if matches!(child_position, Position::Absolute | Position::Fixed) {
            out_of_flow.push((*child_id, child_position));
            continue;
        }

        let (clear_left, clear_right) = parse_clear(child_style);
        if clear_left || clear_right {
            for (rect, side) in &floats {
                let matches = (clear_left && *side == FloatSide::Left)
                    || (clear_right && *side == FloatSide::Right);
                if matches {
                    cursor_y = cursor_y.max(rect.y + rect.height);
                }
            }
        }

        if let Some(side) = parse_float(child_style) {
            let specified = child_style.and_then(|s| s.get("width")).and_then(parse_length);
            let float_width = specified.unwrap_or_else(|| (content_width * 0.5).min(160.0));
            let float_x = match side {
                FloatSide::Left => content_x,
                FloatSide::Right => content_x + content_width - float_width,
            };
            if let Some(mut float_box) = layout_node(
                context,
                *child_id,
                float_x,
                cursor_y,
                float_width,
                containing_block,
                count,
            ) {
                let width = float_box.rect.width;
                float_box.rect.x = match side {
                    FloatSide::Left => content_x,
                    FloatSide::Right => content_x + content_width - width,
                };
                float_box.rect.y = cursor_y;
                floats.push((float_box.rect, side));
                content_height =
                    content_height.max(float_box.rect.y + float_box.rect.height - content_y);
                children.push(float_box);
            }
            continue;
        }

        let child_is_inline = child_is_text || child_display == Some("inline");
        if child_is_inline {
            let font_size = child_style
                .and_then(|computed| computed.get("font-size"))
                .and_then(parse_length)
                .unwrap_or(16.0)
                .max(1.0);
            let child_image = context
                .images
                .and_then(|images| images.get(child_id))
                .copied()
                .map(|intrinsic| sized_image(child_node, intrinsic, content_width));
            let text_width = match child_node.kind() {
                NodeKind::Text(text) => measurer.text(text, font_size),
                NodeKind::Element { tag_name, .. } if tag_name == "img" => {
                    child_image.map_or(content_width, |size| size.width)
                }
                _ => content_width.min(80.0),
            };
            let line_height = if inline_line_height > 0.0 { inline_line_height } else { 19.2 };
            let (line_left, line_right) =
                float_bounds(&floats, cursor_y, line_height, content_x, content_x + content_width);
            if inline_x < line_left {
                inline_x = line_left;
            }
            let mut child_width = text_width.min((line_right - inline_x).max(0.0));
            if inline_x + child_width > line_right && inline_x > line_left {
                cursor_y += line_height;
                inline_line_height = 0.0;
                let (next_left, next_right) = float_bounds(
                    &floats,
                    cursor_y,
                    line_height,
                    content_x,
                    content_x + content_width,
                );
                inline_x = next_left;
                child_width = text_width.min((next_right - next_left).max(0.0));
            }
            if let Some(child) = layout_node(
                context,
                *child_id,
                inline_x,
                cursor_y,
                child_width,
                containing_block,
                count,
            ) {
                inline_x += child.rect.width;
                inline_line_height = inline_line_height.max(child.rect.height);
                content_height = content_height.max(cursor_y - content_y + inline_line_height);
                children.push(child);
            }
        } else {
            if inline_x > content_x {
                cursor_y += if inline_line_height > 0.0 { inline_line_height } else { 19.2 };
                inline_x = content_x;
                inline_line_height = 0.0;
            }
            if let Some(child) = layout_node(
                context,
                *child_id,
                content_x,
                cursor_y,
                content_width,
                containing_block,
                count,
            ) {
                cursor_y = child.flow_rect.y + child.flow_rect.height + child.margin.bottom;
                content_height = cursor_y - content_y;
                children.push(child);
            }
        }
    }
    children.extend(layout_out_of_flow(context, &out_of_flow, containing_block, count));
    (children, content_height)
}

fn layout_out_of_flow(
    context: &LayoutContext,
    items: &[(NodeId, Position)],
    containing_block: Rect,
    count: &mut usize,
) -> Vec<LayoutBox> {
    let mut result = Vec::new();
    for (id, position) in items {
        let style = context.styles.get(*id);
        let block = if *position == Position::Fixed {
            Rect { x: 0.0, y: 0.0, width: containing_block.width, height: containing_block.height }
        } else {
            containing_block
        };
        let specified_width = style.and_then(|s| s.get("width")).and_then(parse_length);
        let left = parse_offset(style, "left");
        let right = parse_offset(style, "right");
        let child_width = specified_width
            .or_else(|| left.zip(right).map(|(left, right)| (block.width - left - right).max(0.0)))
            .unwrap_or(block.width);
        let x = left.map(|left| block.x + left).unwrap_or_else(|| {
            right.map(|right| block.x + block.width - child_width - right).unwrap_or(block.x)
        });
        let y = parse_offset(style, "top").map(|top| block.y + top).unwrap_or_else(|| {
            parse_offset(style, "bottom")
                .map(|bottom| block.y + block.height - bottom)
                .unwrap_or(block.y)
        });
        let mut box_ = layout_node(context, *id, x, y, child_width, block, count);
        if let Some(ref mut box_) = box_ {
            if let Some((top, bottom)) =
                parse_offset(style, "top").zip(parse_offset(style, "bottom"))
            {
                box_.rect.height = (block.height - top - bottom).max(0.0);
            }
        }
        if let Some(box_) = box_ {
            result.push(box_);
        }
    }
    result
}

/// Lays out flex items on a single main axis. Grow/shrink, justification,
/// alignment and gaps are applied to measured boxes, which are then positioned
/// with their final geometry.
#[allow(clippy::too_many_arguments)]
fn layout_flex_children(
    context: &LayoutContext,
    id: NodeId,
    x: f32,
    y: f32,
    width: f32,
    containing_block: Rect,
    style: Option<&bweb_css::ComputedStyle>,
    count: &mut usize,
) -> (Vec<LayoutBox>, f32) {
    let Some(node) = context.document.node(id) else {
        return (Vec::new(), 0.0);
    };
    let direction = style.and_then(|s| s.get("flex-direction")).unwrap_or("row");
    let row = !matches!(direction, "column" | "column-reverse");
    let reverse = matches!(direction, "row-reverse" | "column-reverse");
    let justify = style.and_then(|s| s.get("justify-content")).unwrap_or("flex-start");
    let align = style.and_then(|s| s.get("align-items")).unwrap_or("stretch");
    let column_gap =
        style.and_then(|s| s.get("column-gap")).and_then(parse_length).unwrap_or(0.0).max(0.0);
    let row_gap =
        style.and_then(|s| s.get("row-gap")).and_then(parse_length).unwrap_or(0.0).max(0.0);
    let gap = if row { column_gap } else { row_gap };
    let cross_gap = if row { row_gap } else { column_gap };

    let mut out_of_flow = Vec::new();
    let mut child_ids: Vec<NodeId> = node
        .children()
        .iter()
        .copied()
        .filter(|child_id| {
            let Some(child_node) = context.document.node(*child_id) else { return false };
            let child_style = context.styles.get(*child_id);
            if child_style.and_then(|s| s.get("display")) == Some("none") {
                return false;
            }
            let position = parse_position(child_style);
            if matches!(position, Position::Absolute | Position::Fixed) {
                out_of_flow.push((*child_id, position));
                return false;
            }
            !matches!(
                child_node.kind(),
                NodeKind::Element { tag_name, .. } if is_non_rendered_element(tag_name)
            )
        })
        .collect();

    let mut items: Vec<(LayoutBox, f32, f32, f32)> = Vec::new();
    for child_id in &child_ids {
        let Some(child) = layout_node(context, *child_id, 0.0, 0.0, width, containing_block, count)
        else {
            continue;
        };
        let child_style = context.styles.get(*child_id);
        let grow = child_style
            .and_then(|s| s.get("flex-grow"))
            .and_then(|value| value.parse::<f32>().ok())
            .unwrap_or(0.0)
            .max(0.0);
        let shrink = child_style
            .and_then(|s| s.get("flex-shrink"))
            .and_then(|value| value.parse::<f32>().ok())
            .unwrap_or(1.0)
            .max(0.0);
        let basis = child_style
            .and_then(|s| s.get("flex-basis"))
            .and_then(parse_length)
            .unwrap_or(0.0)
            .max(0.0);
        let natural = if row {
            child.rect.width + child.margin.left + child.margin.right
        } else {
            child.rect.height + child.margin.top + child.margin.bottom
        };
        items.push((child, grow, shrink, if basis > 0.0 { basis } else { natural }));
    }
    if items.is_empty() {
        return (Vec::new(), 0.0);
    }

    let main_available =
        if row { Some(width) } else { style.and_then(|s| s.get("height")).and_then(parse_length) };
    let wrap = matches!(style.and_then(|s| s.get("flex-wrap")), Some("wrap" | "wrap-reverse"));
    if reverse {
        items.reverse();
        child_ids.reverse();
    }

    // Split items into flex lines using their hypothetical main sizes.
    let mut lines: Vec<Vec<usize>> = Vec::new();
    let mut current_line: Vec<usize> = Vec::new();
    let mut used = 0.0_f32;
    for (index, item) in items.iter().enumerate() {
        let needed = item.3 + if current_line.is_empty() { 0.0 } else { gap };
        if !current_line.is_empty()
            && wrap
            && main_available.is_some_and(|available| used + needed > available)
        {
            lines.push(std::mem::take(&mut current_line));
            used = 0.0;
        }
        used += needed;
        current_line.push(index);
    }
    if !current_line.is_empty() {
        lines.push(current_line);
    }
    if lines.is_empty() {
        return (Vec::new(), 0.0);
    }

    let cross_sizes: Vec<f32> = items
        .iter()
        .map(|(child, _, _, _)| if row { child.rect.height } else { child.rect.width })
        .collect();
    let mut line_layouts: Vec<(Vec<f32>, f32, f32, f32)> = Vec::new();
    for line in &lines {
        let n = line.len();
        let total_base: f32 =
            line.iter().map(|&index| items[index].3).sum::<f32>() + gap * (n - 1) as f32;
        let mut sizes = vec![0.0_f32; n];
        let mut leading = 0.0;
        let mut between = 0.0;
        if let Some(available) = main_available {
            let free = available - total_base;
            let total_grow: f32 = line.iter().map(|&index| items[index].1).sum();
            if free > 0.0 && total_grow > 0.0 {
                for (size, &index) in sizes.iter_mut().zip(line) {
                    *size = items[index].3 + free * items[index].1 / total_grow;
                }
            } else {
                for (size, &index) in sizes.iter_mut().zip(line) {
                    *size = items[index].3;
                }
                if free > 0.0 {
                    match justify {
                        "flex-end" | "end" => leading = free,
                        "center" => leading = free / 2.0,
                        "space-between" if n > 1 => between = free / (n - 1) as f32,
                        "space-around" => {
                            leading = free / (2 * n) as f32;
                            between = free / n as f32;
                        }
                        "space-evenly" => {
                            leading = free / (n + 1) as f32;
                            between = leading;
                        }
                        _ => {}
                    }
                } else if free < 0.0 {
                    let weighted: f32 =
                        line.iter().map(|&index| items[index].2 * items[index].3).sum();
                    if weighted > 0.0 {
                        for (size, &index) in sizes.iter_mut().zip(line) {
                            *size = (*size + free * items[index].2 * items[index].3 / weighted)
                                .max(0.0);
                        }
                    }
                }
            }
        } else {
            for (size, &index) in sizes.iter_mut().zip(line) {
                *size = items[index].3;
            }
        }
        let line_cross = line.iter().map(|&index| cross_sizes[index]).fold(0.0, f32::max);
        line_layouts.push((sizes, leading, between, line_cross));
    }

    let total_cross: f32 = line_layouts.iter().map(|(_, _, _, cross)| *cross).sum::<f32>()
        + cross_gap * (lines.len() - 1) as f32;
    let align_content = style.and_then(|s| s.get("align-content")).unwrap_or("stretch");
    let cross_lead = if row {
        0.0
    } else {
        let extra = (width - total_cross).max(0.0);
        match align_content {
            "center" => extra / 2.0,
            "flex-end" | "end" => extra,
            _ => 0.0,
        }
    };

    let mut result = Vec::new();
    let mut cross_offset = cross_lead;
    let mut main_extent = 0.0_f32;
    for (line_index, line) in lines.iter().enumerate() {
        let (sizes, leading, between, line_cross) = &line_layouts[line_index];
        let line_items: Vec<_> = line.iter().map(|&index| items[index].clone()).collect();
        let line_ids: Vec<_> = line.iter().map(|&index| child_ids[index]).collect();
        let line_crosses: Vec<_> = line.iter().map(|&index| cross_sizes[index]).collect();
        let (line_x, line_y) = if row { (x, y + cross_offset) } else { (x + cross_offset, y) };
        result.extend(position_flex_items(
            context,
            line_items,
            sizes.clone(),
            line_crosses,
            line_ids,
            row,
            align,
            line_x,
            line_y,
            *leading,
            *between,
            gap,
            *line_cross,
            containing_block,
            count,
        ));
        main_extent = main_extent
            .max(*leading + sizes.iter().sum::<f32>() + *between * (line.len() - 1) as f32);
        cross_offset += *line_cross + cross_gap;
    }
    let content_height = if row { total_cross } else { main_extent };
    result.extend(layout_out_of_flow(context, &out_of_flow, containing_block, count));
    (result, content_height)
}

#[allow(clippy::too_many_arguments)]
fn position_flex_items(
    context: &LayoutContext,
    items: Vec<(LayoutBox, f32, f32, f32)>,
    sizes: Vec<f32>,
    cross_sizes: Vec<f32>,
    child_ids: Vec<NodeId>,
    row: bool,
    align: &str,
    x: f32,
    y: f32,
    leading: f32,
    between: f32,
    gap: f32,
    line_cross: f32,
    containing_block: Rect,
    count: &mut usize,
) -> Vec<LayoutBox> {
    let mut result = Vec::new();
    let mut main_cursor = x + leading;
    for (index, (mut child, _, _, _)) in items.into_iter().enumerate() {
        let outer_main = sizes[index];
        let align_self = child_ids
            .get(index)
            .and_then(|id| context.styles.get(*id))
            .and_then(|style| style.get("align-self"))
            .unwrap_or("auto");
        let effective = if align_self == "auto" { align } else { align_self };
        let cross_size = if effective == "stretch" { line_cross } else { cross_sizes[index] };
        let cross_offset = match effective {
            "center" => (line_cross - cross_size) / 2.0,
            "flex-end" | "end" => line_cross - cross_size,
            _ => 0.0,
        };
        if row {
            let width = (outer_main - child.margin.left - child.margin.right).max(0.0);
            child.rect = Rect {
                x: main_cursor + child.margin.left,
                y: y + cross_offset,
                width,
                height: cross_size,
            };
        } else {
            let height = (outer_main - child.margin.top - child.margin.bottom).max(0.0);
            child.rect = Rect {
                x: x + cross_offset,
                y: main_cursor + child.margin.top,
                width: cross_size,
                height,
            };
        }
        // Re-lay out the item at its final size so descendants use the same box.
        if let Some(relayouted) = layout_node(
            context,
            child.node,
            child.rect.x - child.margin.left,
            child.rect.y - child.margin.top,
            child.rect.width,
            containing_block,
            count,
        ) {
            child.children = relayouted.children;
            child.line_boxes = relayouted.line_boxes;
        }
        main_cursor += outer_main + gap + between;
        result.push(child);
    }
    result
}

#[derive(Debug, Clone, PartialEq)]
enum TrackSize {
    Fixed(f32),
    Fr(f32),
    MinMax { min: f32, max: Box<TrackSize> },
    Auto,
}

fn split_track_tokens(value: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut paren = 0usize;
    let mut quote = None;
    for (index, character) in value.char_indices() {
        if let Some(current) = quote {
            if character == current {
                quote = None;
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '(' => paren += 1,
            ')' => paren = paren.saturating_sub(1),
            c if c.is_ascii_whitespace() && paren == 0 => {
                if start < index {
                    result.push(&value[start..index]);
                }
                start = index + c.len_utf8();
            }
            _ => {}
        }
    }
    if start < value.len() {
        result.push(&value[start..]);
    }
    result
}

fn parse_track(token: &str, available: f32) -> Option<TrackSize> {
    let lower = token.trim().to_ascii_lowercase();
    if lower == "auto" {
        return Some(TrackSize::Auto);
    }
    if let Some(inner) = lower.strip_prefix("minmax(").and_then(|rest| rest.strip_suffix(')')) {
        let parts: Vec<&str> = inner.splitn(2, ',').map(str::trim).collect();
        if parts.len() != 2 {
            return None;
        }
        let min = parse_track(parts[0], available)?;
        let min = match min {
            TrackSize::Fixed(value) => value,
            TrackSize::Auto => 0.0,
            TrackSize::Fr(_) => return None,
            TrackSize::MinMax { .. } => return None,
        };
        let max = parse_track(parts[1], available)?;
        return Some(TrackSize::MinMax { min, max: Box::new(max) });
    }
    if let Some(fraction) = lower.strip_suffix("fr") {
        return fraction.trim().parse::<f32>().ok().map(|value| TrackSize::Fr(value.max(0.0)));
    }
    if let Some(percent) = lower.strip_suffix('%') {
        return percent
            .trim()
            .parse::<f32>()
            .ok()
            .map(|value| TrackSize::Fixed(available * value.max(0.0) / 100.0));
    }
    parse_length(&lower).map(TrackSize::Fixed)
}

fn parse_tracks(value: Option<&str>, available: f32) -> Vec<TrackSize> {
    let Some(value) = value else { return Vec::new() };
    if value == "none" {
        return Vec::new();
    }
    let mut tracks = Vec::new();
    for token in split_track_tokens(value) {
        if let Some(inner) = token.strip_prefix("repeat(").and_then(|rest| rest.strip_suffix(')')) {
            let parts: Vec<&str> = inner.splitn(2, ',').map(str::trim).collect();
            let count =
                parts.first().and_then(|value| value.parse::<usize>().ok()).unwrap_or(0).min(64);
            if let Some(track) = parts.get(1).and_then(|value| parse_track(value, available)) {
                for _ in 0..count {
                    tracks.push(track.clone());
                }
            }
            continue;
        }
        if let Some(track) = parse_track(token, available) {
            tracks.push(track);
        }
    }
    tracks
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GridPlacement {
    start: Option<usize>,
    span: usize,
}

fn parse_grid_placement(value: Option<&str>) -> GridPlacement {
    let Some(value) = value else {
        return GridPlacement { start: None, span: 1 };
    };
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() || value == "auto" {
        return GridPlacement { start: None, span: 1 };
    }
    let parts: Vec<&str> = value.split('/').map(str::trim).collect();
    let mut start = None;
    let mut span = 1;
    if let Some(count) = parts[0].strip_prefix("span ") {
        span = count.trim().parse::<usize>().unwrap_or(1).max(1);
    } else {
        start = parts[0].parse::<usize>().ok().map(|line| line.saturating_sub(1));
    }
    if let Some(end) = parts.get(1) {
        if let Some(count) = end.strip_prefix("span ") {
            span = count.trim().parse::<usize>().unwrap_or(1).max(1);
        } else if let Ok(line) = end.parse::<usize>() {
            if let Some(first) = start {
                span = line.saturating_sub(first + 1).max(1);
            }
        }
    }
    GridPlacement { start, span }
}

fn free_grid_slot(
    occupied: &std::collections::HashSet<(usize, usize)>,
    columns: usize,
    rows: usize,
    column: Option<usize>,
    row: Option<usize>,
    column_span: usize,
    row_span: usize,
) -> Option<(usize, usize)> {
    let fits = |start_row: usize, start_col: usize| {
        (start_row..start_row + row_span)
            .all(|r| (start_col..start_col + column_span).all(|c| !occupied.contains(&(r, c))))
    };
    match (row, column) {
        (Some(r), Some(c)) => fits(r, c).then_some((r, c)),
        (Some(r), None) => (0..columns).map(|c| (r, c)).find(|(r, c)| fits(*r, *c)),
        (None, Some(c)) => (0..rows).map(|r| (r, c)).find(|(r, c)| fits(*r, *c)),
        (None, None) => {
            (0..rows).flat_map(|r| (0..columns).map(move |c| (r, c))).find(|(r, c)| fits(*r, *c))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn layout_grid_children(
    context: &LayoutContext,
    id: NodeId,
    x: f32,
    y: f32,
    width: f32,
    containing_block: Rect,
    style: Option<&bweb_css::ComputedStyle>,
    count: &mut usize,
) -> (Vec<LayoutBox>, f32) {
    let Some(node) = context.document.node(id) else {
        return (Vec::new(), 0.0);
    };
    let column_gap =
        style.and_then(|s| s.get("column-gap")).and_then(parse_length).unwrap_or(0.0).max(0.0);
    let row_gap =
        style.and_then(|s| s.get("row-gap")).and_then(parse_length).unwrap_or(0.0).max(0.0);

    let mut out_of_flow = Vec::new();
    let child_ids: Vec<NodeId> = node
        .children()
        .iter()
        .copied()
        .filter(|child_id| {
            let child_style = context.styles.get(*child_id);
            if child_style.and_then(|s| s.get("display")) == Some("none") {
                return false;
            }
            let position = parse_position(child_style);
            if matches!(position, Position::Absolute | Position::Fixed) {
                out_of_flow.push((*child_id, position));
                return false;
            }
            !matches!(
                context.document.node(*child_id).map(|node| node.kind()),
                Some(NodeKind::Element { tag_name, .. }) if is_non_rendered_element(tag_name)
            )
        })
        .collect();

    // Named areas: parse quoted row strings and let grid-area refer to a name.
    let mut named_areas: std::collections::HashMap<String, (usize, usize, usize, usize)> =
        std::collections::HashMap::new();
    if let Some(areas) = style.and_then(|s| s.get("grid-template-areas")) {
        let bytes = areas.as_bytes();
        let mut cursor = 0;
        let mut row_index = 0;
        while cursor < bytes.len() {
            let quote = match bytes[cursor] {
                b'\'' => b'\'',
                b'"' => b'"',
                _ => {
                    cursor += 1;
                    continue;
                }
            };
            cursor += 1;
            let start = cursor;
            while cursor < bytes.len() && bytes[cursor] != quote {
                cursor += 1;
            }
            let row_string = &areas[start..cursor.min(areas.len())];
            cursor += 1;
            for (col_index, name) in row_string.split_whitespace().enumerate() {
                if name == "." {
                    continue;
                }
                named_areas
                    .entry(name.to_owned())
                    .and_modify(|(start_row, start_col, row_span, col_span)| {
                        *start_row = (*start_row).min(row_index);
                        *start_col = (*start_col).min(col_index);
                        *row_span = row_index + 1 - *start_row;
                        *col_span = col_index + 1 - *start_col;
                    })
                    .or_insert((row_index, col_index, 1, 1));
            }
            row_index += 1;
        }
    }

    let mut measured: Vec<(NodeId, LayoutBox, GridPlacement, GridPlacement)> = Vec::new();
    for child_id in &child_ids {
        let child_style = context.styles.get(*child_id);
        let Some(child) = layout_node(context, *child_id, 0.0, 0.0, width, containing_block, count)
        else {
            continue;
        };
        let mut column = parse_grid_placement(child_style.and_then(|s| s.get("grid-column")));
        let mut row = parse_grid_placement(child_style.and_then(|s| s.get("grid-row")));
        if let Some(area) = child_style.and_then(|s| s.get("grid-area")) {
            if let Some(&(start_row, start_col, row_span, col_span)) = named_areas.get(area.trim())
            {
                column = GridPlacement { start: Some(start_col), span: col_span };
                row = GridPlacement { start: Some(start_row), span: row_span };
            }
        }
        measured.push((*child_id, child, column, row));
    }
    if measured.is_empty() {
        return (layout_out_of_flow(context, &out_of_flow, containing_block, count), 0.0);
    }

    let template_columns = parse_tracks(style.and_then(|s| s.get("grid-template-columns")), width);
    let template_rows = parse_tracks(style.and_then(|s| s.get("grid-template-rows")), width);
    let explicit_columns = measured
        .iter()
        .map(|(_, _, column, _)| column.start.unwrap_or(0) + column.span)
        .max()
        .unwrap_or(0);
    let explicit_rows =
        measured.iter().map(|(_, _, _, row)| row.start.unwrap_or(0) + row.span).max().unwrap_or(0);
    let column_count = template_columns.len().max(explicit_columns).max(1);
    let row_count =
        template_rows.len().max(explicit_rows).max(measured.len().div_ceil(column_count));

    let mut occupied = std::collections::HashSet::new();
    let mut assigned: Vec<(usize, usize)> = Vec::with_capacity(measured.len());
    let mut pending: Vec<usize> = Vec::new();
    for (index, (_, _, column, row)) in measured.iter().enumerate() {
        if column.start.is_none() && row.start.is_none() {
            pending.push(index);
            continue;
        }
        let span_c = column.span.min(column_count);
        let span_r = row.span.max(1);
        if let Some((slot_row, slot_col)) = free_grid_slot(
            &occupied,
            column_count,
            row_count,
            column.start,
            row.start,
            span_c,
            span_r,
        ) {
            for r in slot_row..slot_row + span_r {
                for c in slot_col..slot_col + span_c {
                    occupied.insert((r, c));
                }
            }
            assigned.push((slot_row, slot_col));
        } else {
            assigned.push((0, 0));
        }
    }
    let mut placed: Vec<Option<(usize, usize)>> = vec![None; measured.len()];
    let mut assignment_iter = assigned.into_iter();
    for index in 0..measured.len() {
        if measured[index].2.start.is_some() || measured[index].3.start.is_some() {
            placed[index] = assignment_iter.next();
        }
    }
    for index in pending {
        if let Some((slot_row, slot_col)) = free_grid_slot(
            &occupied,
            column_count,
            row_count,
            None,
            None,
            measured[index].2.span.min(column_count),
            measured[index].3.span.max(1),
        ) {
            for r in slot_row..slot_row + measured[index].3.span.max(1) {
                for c in slot_col..slot_col + measured[index].2.span.min(column_count) {
                    occupied.insert((r, c));
                }
            }
            placed[index] = Some((slot_row, slot_col));
        }
    }

    let track_space = (width - column_gap * (column_count.saturating_sub(1)) as f32).max(0.0);
    let mut column_sizes = vec![0.0_f32; column_count];
    let mut fixed_total = 0.0;
    let mut fr_total = 0.0;
    for (index, track) in template_columns.iter().enumerate() {
        match track {
            TrackSize::Fixed(value) => {
                column_sizes[index] = *value;
                fixed_total += *value;
            }
            TrackSize::Fr(fraction) => fr_total += fraction,
            TrackSize::MinMax { min, max } => {
                fixed_total += *min;
                column_sizes[index] = *min;
                if let TrackSize::Fr(fraction) = max.as_ref() {
                    fr_total += fraction;
                } else if let TrackSize::Fixed(value) = max.as_ref() {
                    column_sizes[index] = value.max(*min);
                    fixed_total += value.max(*min) - min;
                }
            }
            TrackSize::Auto => {}
        }
    }
    if fr_total > 0.0 {
        let remaining = (track_space - fixed_total).max(0.0);
        for (index, track) in template_columns.iter().enumerate() {
            match track {
                TrackSize::Fr(fraction) => column_sizes[index] = remaining * fraction / fr_total,
                TrackSize::MinMax { min, max } => {
                    if let TrackSize::Fr(fraction) = max.as_ref() {
                        column_sizes[index] = (remaining * fraction / fr_total).max(*min);
                    }
                }
                _ => {}
            }
        }
    } else {
        let autos: Vec<usize> = (0..column_count)
            .filter(|index| {
                template_columns.get(*index) == Some(&TrackSize::Auto)
                    || template_columns.get(*index).is_none()
            })
            .collect();
        if template_columns.is_empty() {
            column_sizes[0] = track_space;
        } else if !autos.is_empty() {
            let remaining = (track_space - fixed_total).max(0.0) / autos.len() as f32;
            for index in autos {
                column_sizes[index] = remaining;
            }
        }
    }

    let auto_row = parse_tracks(
        style.and_then(|s| s.get("grid-auto-rows")).filter(|value| *value != "auto"),
        width,
    )
    .into_iter()
    .next();
    let mut row_sizes = vec![0.0_f32; row_count];
    for (row, size) in row_sizes.iter_mut().enumerate() {
        let track =
            template_rows.get(row).cloned().or_else(|| auto_row.clone()).unwrap_or(TrackSize::Auto);
        *size = match track {
            TrackSize::Fixed(value) => value,
            TrackSize::Fr(fraction) => fraction * 50.0,
            TrackSize::MinMax { min, max } => match max.as_ref() {
                TrackSize::Fr(fraction) => (fraction * 50.0).max(min),
                TrackSize::Fixed(value) => value.max(min),
                TrackSize::Auto | TrackSize::MinMax { .. } => min,
            },
            TrackSize::Auto => 0.0,
        };
        if *size == 0.0 {
            *size = measured
                .iter()
                .zip(placed.iter())
                .filter(|(_, slot)| slot.map(|(slot_row, _)| slot_row == row).unwrap_or(false))
                .map(|((_, child, _, row_placement), _)| {
                    child.rect.height / row_placement.span.max(1) as f32
                })
                .fold(0.0, f32::max);
        }
    }
    let content_height = row_sizes.iter().sum::<f32>() + row_gap * (row_count - 1) as f32;

    let mut boxes = Vec::new();
    for (index, (_, mut child, column, row)) in measured.into_iter().enumerate() {
        let (slot_row, slot_col) = placed[index].unwrap_or((0, 0));
        let cell_x =
            x + column_sizes.iter().take(slot_col).sum::<f32>() + column_gap * slot_col as f32;
        let cell_y = y + row_sizes.iter().take(slot_row).sum::<f32>() + row_gap * slot_row as f32;
        let cell_width = (column_sizes.iter().skip(slot_col).take(column.span).sum::<f32>()
            + column_gap * column.span.saturating_sub(1) as f32)
            .max(0.0);
        let cell_height = (row_sizes.iter().skip(slot_row).take(row.span).sum::<f32>()
            + row_gap * row.span.saturating_sub(1) as f32)
            .max(0.0);
        child.rect = Rect {
            x: cell_x,
            y: cell_y,
            width: cell_width,
            height: if cell_height > 0.0 { cell_height } else { child.rect.height },
        };
        if let Some(relayouted) =
            layout_node(context, child.node, cell_x, cell_y, cell_width, containing_block, count)
        {
            child.children = relayouted.children;
            child.line_boxes = relayouted.line_boxes;
        }
        boxes.push(child);
    }
    boxes.extend(layout_out_of_flow(context, &out_of_flow, containing_block, count));
    (boxes, content_height)
}

struct LineGeometry<'a> {
    origin_x: f32,
    origin_y: f32,
    available_width: f32,
    style: Option<&'a bweb_css::ComputedStyle>,
    pseudo: Option<&'a PseudoContent>,
}

fn build_line_boxes(
    context: &LayoutContext,
    owner: NodeId,
    children: &[LayoutBox],
    geometry: LineGeometry,
) -> Vec<LineBox> {
    let LineGeometry { origin_x, origin_y, available_width, style, pseudo } = geometry;
    let white_space = style.and_then(|s| s.get("white-space")).unwrap_or("normal");
    let font_size =
        style.and_then(|s| s.get("font-size")).and_then(parse_length).unwrap_or(16.0).max(1.0);
    let line_height = style
        .and_then(|s| s.get("line-height"))
        .and_then(|value| parse_line_height(value, font_size))
        .unwrap_or(font_size * 1.2)
        .max(1.0);
    let preserve_spaces = matches!(white_space, "pre" | "pre-wrap");
    let preserve_newlines = matches!(white_space, "pre" | "pre-wrap" | "pre-line");
    let wrap = !matches!(white_space, "nowrap" | "pre");
    let base_line_height = line_height;
    let space_advance = context.measurer.char_advance(' ', font_size);
    let mut items = Vec::new();
    if let Some(before) = pseudo.and_then(|content| content.before.as_ref()) {
        items.push(InlineItem::Text {
            node: owner,
            text: &before.text,
            style: Some(Arc::clone(&before.style)),
        });
    }
    for child in children {
        collect_inline_items(context, child, &mut items);
    }
    if let Some(after) = pseudo.and_then(|content| content.after.as_ref()) {
        items.push(InlineItem::Text {
            node: owner,
            text: &after.text,
            style: Some(Arc::clone(&after.style)),
        });
    }

    // A line containing an image is as tall as the image; text on that line is
    // centered vertically against it.
    let close_line =
        |lines: &mut Vec<LineBox>, fragments: &mut Vec<TextFragment>, y: f32, height: f32| {
            finish_line(lines, fragments, origin_x, y, height);
            let offset = (height - base_line_height) * 0.5;
            if offset > 0.0 {
                if let Some(line) = lines.last_mut() {
                    for fragment in &mut line.fragments {
                        fragment.rect.y += offset;
                    }
                }
            }
        };

    let mut lines = Vec::new();
    let mut fragments: Vec<TextFragment> = Vec::new();
    let mut x = origin_x;
    let mut y = origin_y;
    let mut current_line_height = base_line_height;
    let mut pending_space = false;

    for item in items {
        match item {
            InlineItem::Text { node, text, style: item_style } => {
                for ch in text.chars() {
                    if ch == '\n' && preserve_newlines {
                        pending_space = false;
                        close_line(&mut lines, &mut fragments, y, current_line_height);
                        x = origin_x;
                        y += current_line_height;
                        current_line_height = base_line_height;
                        continue;
                    }

                    if ch.is_whitespace() && !preserve_spaces {
                        pending_space = true;
                        continue;
                    }

                    let advance = context.measurer.char_advance(ch, font_size);
                    if pending_space && x > origin_x {
                        if wrap && x + space_advance + advance > origin_x + available_width {
                            close_line(&mut lines, &mut fragments, y, current_line_height);
                            x = origin_x;
                            y += current_line_height;
                            current_line_height = base_line_height;
                        } else {
                            append_fragment_char(
                                &mut fragments,
                                FragmentSpec {
                                    node,
                                    ch: ' ',
                                    x,
                                    y,
                                    advance: space_advance,
                                    line_height: base_line_height,
                                    style: item_style.clone(),
                                },
                            );
                            x += space_advance;
                        }
                    }
                    pending_space = false;

                    if wrap && x > origin_x && x + advance > origin_x + available_width {
                        close_line(&mut lines, &mut fragments, y, current_line_height);
                        x = origin_x;
                        y += current_line_height;
                        current_line_height = base_line_height;
                    }

                    append_fragment_char(
                        &mut fragments,
                        FragmentSpec {
                            node,
                            ch,
                            x,
                            y,
                            advance,
                            line_height: base_line_height,
                            style: item_style.clone(),
                        },
                    );
                    x += advance;
                }
            }
            InlineItem::Image { width, height } => {
                if wrap && x > origin_x && x + width > origin_x + available_width {
                    close_line(&mut lines, &mut fragments, y, current_line_height);
                    x = origin_x;
                    y += current_line_height;
                    current_line_height = base_line_height;
                }
                current_line_height = current_line_height.max(height);
                x += width;
                pending_space = false;
            }
        }
    }

    if !fragments.is_empty() {
        close_line(&mut lines, &mut fragments, y, current_line_height);
    }
    lines
}

enum InlineItem<'a> {
    Text { node: NodeId, text: &'a str, style: Option<Arc<ComputedStyle>> },
    Image { width: f32, height: f32 },
}

fn collect_inline_items<'a>(
    context: &LayoutContext<'a>,
    layout: &LayoutBox,
    output: &mut Vec<InlineItem<'a>>,
) {
    let Some(node) = context.document.node(layout.node) else {
        return;
    };
    match node.kind() {
        NodeKind::Text(value) if !value.is_empty() => {
            output.push(InlineItem::Text { node: layout.node, text: value, style: None });
        }
        NodeKind::Element { tag_name, .. } => {
            if tag_name == "img"
                && context.images.is_some_and(|images| images.contains_key(&layout.node))
            {
                output.push(InlineItem::Image {
                    width: layout.rect.width,
                    height: layout.rect.height,
                });
            }
            if layout.display == Display::Inline {
                for child in &layout.children {
                    collect_inline_items(context, child, output);
                }
            }
        }
        _ => {}
    }
}

fn sized_image(node: &Node, intrinsic: ImageSize, available_width: f32) -> ImageSize {
    let attributes = match node.kind() {
        NodeKind::Element { attributes, .. } => attributes.as_slice(),
        _ => &[],
    };
    let attribute = |name: &str| {
        attributes.iter().find(|item| item.name == name).and_then(|item| parse_length(&item.value))
    };
    let width = attribute("width").unwrap_or(intrinsic.width).min(available_width.max(1.0));
    let height = attribute("height").unwrap_or_else(|| {
        if intrinsic.width > 0.0 { width * intrinsic.height / intrinsic.width } else { 0.0 }
    });
    ImageSize { width: width.max(1.0), height: height.max(1.0) }
}

fn is_non_rendered_element(tag_name: &str) -> bool {
    matches!(
        tag_name,
        "base" | "head" | "link" | "meta" | "script" | "style" | "template" | "title"
    )
}

struct FragmentSpec {
    node: NodeId,
    ch: char,
    x: f32,
    y: f32,
    advance: f32,
    line_height: f32,
    style: Option<Arc<ComputedStyle>>,
}

fn append_fragment_char(fragments: &mut Vec<TextFragment>, spec: FragmentSpec) {
    let FragmentSpec { node, ch, x, y, advance, line_height, style } = spec;
    if let Some(last) = fragments.last_mut() {
        if last.node == node
            && last.style == style
            && !last.text.chars().last().is_some_and(char::is_whitespace)
            && !ch.is_whitespace()
            && (last.rect.x + last.rect.width - x).abs() < f32::EPSILON
        {
            last.text.push(ch);
            last.rect.width += advance;
            return;
        }
    }
    fragments.push(TextFragment {
        node,
        text: ch.to_string(),
        rect: Rect { x, y, width: advance, height: line_height },
        style,
    });
}

fn finish_line(
    lines: &mut Vec<LineBox>,
    fragments: &mut Vec<TextFragment>,
    origin_x: f32,
    y: f32,
    line_height: f32,
) {
    let width =
        fragments.last().map_or(0.0, |fragment| fragment.rect.x + fragment.rect.width - origin_x);
    lines.push(LineBox {
        rect: Rect { x: origin_x, y, width, height: line_height },
        fragments: std::mem::take(fragments),
    });
}

fn dump_box(output: &mut String, document: &HtmlDocument, layout: &LayoutBox, depth: usize) {
    use std::fmt::Write as _;
    let indent = "  ".repeat(depth);
    let label = match document.node(layout.node).map(|node| node.kind()) {
        Some(NodeKind::Document) => "#document".to_owned(),
        Some(NodeKind::Element { tag_name, .. }) => tag_name.clone(),
        Some(NodeKind::Text(text)) => format!("#text {:?}", text.trim()),
        Some(NodeKind::Comment(_)) => "#comment".to_owned(),
        Some(NodeKind::TemplateContent) => "#template-content".to_owned(),
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

/// Reads a shorthand edge property and then applies per-side longhands.
fn parse_edges(style: Option<&bweb_css::ComputedStyle>, property: &str) -> Edges {
    let Some(style) = style else { return Edges::default() };
    let mut edges = Edges::default();
    if let Some(value) = style.get(property) {
        let values: Vec<f32> = value.split_ascii_whitespace().filter_map(parse_length).collect();
        edges = match values.as_slice() {
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
        };
    }
    for (side, edge) in [
        ("top", &mut edges.top),
        ("right", &mut edges.right),
        ("bottom", &mut edges.bottom),
        ("left", &mut edges.left),
    ] {
        if let Some(value) = style.get(&format!("{property}-{side}")).and_then(parse_length) {
            *edge = value;
        }
    }
    edges
}

fn parse_border_widths(style: Option<&bweb_css::ComputedStyle>) -> Edges {
    parse_edges(style, "border-width")
}

/// CSS `line-height` accepts a multiplier when no unit is given.
fn parse_line_height(value: &str, font_size: f32) -> Option<f32> {
    let value = value.trim();
    let number = value.strip_suffix("px").unwrap_or(value).trim().parse::<f32>().ok()?;
    if !number.is_finite() || number <= 0.0 {
        return None;
    }
    let result = if value.ends_with("px") { number } else { number * font_size };
    result.is_finite().then_some(result)
}

fn parse_length(value: &str) -> Option<f32> {
    parse_css_length(value, 16.0)
}

fn parse_css_length(value: &str, font_size: f32) -> Option<f32> {
    let value = value.trim();
    let (numeric, scale) = if let Some(number) = value.strip_suffix("px") {
        (number, 1.0)
    } else if let Some(number) = value.strip_suffix("em") {
        (number, font_size)
    } else if let Some(number) = value.strip_suffix("rem") {
        (number, 16.0)
    } else {
        (value, 1.0)
    };
    numeric
        .trim()
        .parse::<f32>()
        .ok()
        .map(|number| number * scale)
        .filter(|number| number.is_finite() && *number >= 0.0)
}

fn horizontal_auto_margins(style: Option<&bweb_css::ComputedStyle>) -> (bool, bool) {
    let Some(style) = style else { return (false, false) };
    let shorthand = style.get("margin").unwrap_or("");
    let values: Vec<&str> = shorthand.split_ascii_whitespace().collect();
    let (left, right) = match values.as_slice() {
        [_] => (values[0], values[0]),
        [_, horizontal] => (*horizontal, *horizontal),
        [_, horizontal, _] => (*horizontal, *horizontal),
        [_, right, _, left] => (*left, *right),
        _ => ("", ""),
    };
    (
        left == "auto" || style.get("margin-left") == Some("auto"),
        right == "auto" || style.get("margin-right") == Some("auto"),
    )
}

fn finite_non_negative(value: f32) -> f32 {
    if value.is_finite() { value.max(0.0) } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::{Display, GeneratedText, ImageSize, LayoutTree, PseudoContent};
    use bweb_css::{PseudoElement, StyleEngine, Stylesheet};
    use bweb_html::{HtmlDocument, NodeKind};
    use bweb_text::Font;
    use std::{collections::HashMap, sync::Arc};

    fn layout(markup: &str, css: &str) -> (HtmlDocument, LayoutTree) {
        let document = HtmlDocument::parse(markup);
        let sheet = Stylesheet::parse(css).expect("valid CSS");
        let styles = StyleEngine::new().compute(&document, &[sheet]);
        let tree = LayoutTree::build(&document, &styles, 800.0);
        (document, tree)
    }

    #[test]
    fn html_metadata_and_executable_content_are_not_rendered() {
        let (document, tree) = layout(
            "<html><head><title>Example Domain</title><meta name='description' content='hidden'><style>body { color:red }</style><script>const hidden = true;</script></head><body><p>Visible text</p><script>hiddenAgain()</script></body></html>",
            "",
        );

        for tag in ["head", "title", "meta", "style", "script"] {
            for id in document.iter().filter_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == tag)
                    .then_some(id)
            }) {
                assert!(!contains(&tree.root, id), "{tag} must not enter the layout tree");
            }
        }

        let visible_text = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Text(text) if text == "Visible text").then_some(id)
            })
            .expect("visible body text");
        assert!(contains(&tree.root, visible_text));
    }

    #[test]
    fn nested_inline_text_is_collected_once_by_its_block_container() {
        let (document, tree) = layout(
            "<main>before <span>inside <em>deep</em></span> after</main>",
            "main { display:block } span, em { display:inline }",
        );
        let main = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "main")
                    .then_some(id)
            })
            .expect("main");
        let span = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "span")
                    .then_some(id)
            })
            .expect("span");
        let em = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "em")
                    .then_some(id)
            })
            .expect("em");

        let main_box = find(&tree.root, main).expect("main layout box");
        let span_box = find(&tree.root, span).expect("span layout box");
        let em_box = find(&tree.root, em).expect("em layout box");
        assert_eq!(main_box.line_boxes.len(), 1);
        assert!(span_box.line_boxes.is_empty());
        assert!(em_box.line_boxes.is_empty());

        let text: String = main_box.line_boxes[0]
            .fragments
            .iter()
            .map(|fragment| fragment.text.as_str())
            .collect();
        assert_eq!(text, "before inside deep after");
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
    fn max_width_in_em_and_auto_margins_center_the_box() {
        let (document, tree) = layout(
            "<main>centered</main>",
            "main { display:block; max-width:26em; margin:auto; padding:8px }",
        );
        let main = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "main")
                    .then_some(id)
            })
            .expect("main");
        let box_node = find(&tree.root, main).expect("main box");
        assert_eq!(box_node.rect.width, 432.0);
        assert_eq!(box_node.rect.x, 184.0);
        assert_eq!(box_node.margin.left, 184.0);
        assert_eq!(box_node.margin.right, 184.0);
    }

    #[test]
    fn unitless_line_height_is_a_font_size_multiplier() {
        let (document, tree) =
            layout("<main>text</main>", "main { display:block; font-size:20px; line-height:1.5 }");
        let main = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "main")
                    .then_some(id)
            })
            .expect("main element");
        let box_ = find(&tree.root, main).expect("main box");
        assert_eq!(box_.line_boxes[0].rect.height, 30.0);
    }

    #[test]
    fn applies_edge_longhands_after_the_shorthand() {
        let (document, tree) = layout(
            "<main></main>",
            "main { display:block; margin:10px 20px; margin-top:30px; padding:5px; padding-left:8px }",
        );
        let main = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "main")
                    .then_some(id)
            })
            .expect("main element");
        let box_ = find(&tree.root, main).expect("main box");
        assert_eq!((box_.rect.x, box_.rect.y), (20.0, 30.0));
        assert_eq!((box_.padding.left, box_.padding.top), (8.0, 5.0));
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

    #[test]
    fn nested_inline_fragments_keep_leaf_text_node_ids() {
        let (document, tree) = layout(
            "<main>before <span>inside <em>deep</em></span> after</main>",
            "main { display:block } span, em { display:inline }",
        );
        let main = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "main")
                    .then_some(id)
            })
            .expect("main");
        let main_box = find(&tree.root, main).expect("main layout box");
        let fragments: Vec<_> =
            main_box.line_boxes.iter().flat_map(|line| &line.fragments).collect();
        let text_nodes: Vec<_> = document
            .iter()
            .filter_map(|(id, node)| matches!(node.kind(), NodeKind::Text(_)).then_some(id))
            .collect();
        assert!(fragments.iter().any(|fragment| fragment.text == "before"));
        assert!(fragments.iter().any(|fragment| fragment.text == "inside"));
        assert!(fragments.iter().any(|fragment| fragment.text == "deep"));
        assert!(fragments.iter().all(|fragment| text_nodes.contains(&fragment.node)));
        assert!(
            fragments
                .iter()
                .any(|fragment| { fragment.node == text_nodes[2] && fragment.text == "deep" })
        );
    }

    #[test]
    fn collapsed_whitespace_is_trimmed_at_line_boundaries() {
        let (document, tree) = layout("<main>   alpha   beta   </main>", "main { display:block }");
        let main = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "main")
                    .then_some(id)
            })
            .expect("main");
        let main_box = find(&tree.root, main).expect("main layout box");
        let text: String = main_box.line_boxes[0]
            .fragments
            .iter()
            .map(|fragment| fragment.text.as_str())
            .collect();
        assert_eq!(text, "alpha beta");
        let fragments = &main_box.line_boxes[0].fragments;
        assert!(!fragments.first().expect("first fragment").text.starts_with(' '));
        assert!(!fragments.last().expect("last fragment").text.ends_with(' '));
    }

    #[test]
    fn wraps_without_leading_or_trailing_collapsed_spaces() {
        let (document, tree) =
            layout("<main>one two three</main>", "main { display:block; width:32px }");
        let main = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "main")
                    .then_some(id)
            })
            .expect("main");
        let main_box = find(&tree.root, main).expect("main layout box");
        assert!(main_box.line_boxes.len() >= 2);
        for line in &main_box.line_boxes {
            let text: String =
                line.fragments.iter().map(|fragment| fragment.text.as_str()).collect();
            assert!(!text.starts_with(' '));
            assert!(!text.ends_with(' '));
        }
    }

    #[test]
    fn layout_dump_is_stable_and_contains_geometry_and_fragment_identity() {
        let (document, tree) = layout(
            "<main>hello <span>world</span></main>",
            "main { display:block } span { display:inline }",
        );
        let dump = tree.dump(&document);
        assert!(dump.starts_with("Layout Tree (CSS px)"));
        assert!(dump.contains("main Block x=0.0 y=0.0 w=800.0"));
        assert!(dump.contains("LineBox x=0.0 y=0.0"));
        assert!(dump.contains("Fragment node="));
        assert!(dump.contains("\"hello\""));
        assert!(dump.contains("\"world\""));
    }

    #[test]
    fn generated_content_participates_in_the_inline_flow() {
        let document = HtmlDocument::parse("<main>mid</main>");
        let sheet = Stylesheet::parse(
            "main { display:block } main::before { content:\"A\"; color:red } main::after { content:\"B\" }",
        )
        .expect("valid stylesheet");
        let (styles, generated) =
            StyleEngine::new().compute_generated(&document, &[sheet], 400.0, 300.0);
        let main = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "main")
                    .then_some(id)
            })
            .expect("main element");
        let pseudo = HashMap::from([(
            main,
            PseudoContent {
                before: Some(GeneratedText {
                    text: "A".into(),
                    style: Arc::new(generated[&(main, PseudoElement::Before)].clone()),
                }),
                after: Some(GeneratedText {
                    text: "B".into(),
                    style: Arc::new(generated[&(main, PseudoElement::After)].clone()),
                }),
            },
        )]);
        let images = HashMap::new();
        let tree = LayoutTree::build_with_all(&document, &styles, 400.0, None, &images, &pseudo);
        let box_ = find(&tree.root, main).expect("main box");
        let line = &box_.line_boxes[0];
        let rendered: Vec<_> = line
            .fragments
            .iter()
            .map(|fragment| (fragment.text.as_str(), fragment.style.is_some()))
            .collect();
        assert_eq!(rendered, [("A", true), ("mid", false), ("B", true)]);
    }

    #[test]
    fn small_image_shares_its_line_with_adjacent_text() {
        let document =
            HtmlDocument::parse("<main>before <img src='x.png' width='20'> after</main>");
        let sheet = Stylesheet::parse("main { display: block }").expect("valid CSS");
        let styles = StyleEngine::new().compute(&document, &[sheet]);
        let img = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "img")
                    .then_some(id)
            })
            .expect("img node");
        let main = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "main")
                    .then_some(id)
            })
            .expect("main element");
        let sizes = HashMap::from([(img, ImageSize { width: 20.0, height: 40.0 })]);
        let tree = LayoutTree::build_with_images(&document, &styles, 400.0, &sizes);

        let image_box = find(&tree.root, img).expect("image box");
        let main_box = find(&tree.root, main).expect("main box");
        assert_eq!(main_box.line_boxes.len(), 1, "a small image stays on the text line");
        let line = &main_box.line_boxes[0];
        assert_eq!(line.rect.height, 40.0, "the line grows to the image height");
        let text: String = line.fragments.iter().map(|fragment| fragment.text.as_str()).collect();
        assert_eq!(text, "before after");
        let after = line.fragments.iter().find(|fragment| fragment.text == "after").expect("after");
        assert!(after.rect.x >= image_box.rect.x + image_box.rect.width);
    }

    #[test]
    fn image_height_pushes_following_content_down() {
        let document = HtmlDocument::parse("<main><img src='x.png' width='40'><p>after</p></main>");
        let sheet = Stylesheet::parse("main, p { display: block }").expect("valid CSS");
        let styles = StyleEngine::new().compute(&document, &[sheet]);
        let find_node = |tag: &str| {
            document
                .iter()
                .find_map(|(id, node)| {
                    matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == tag)
                        .then_some(id)
                })
                .expect("node")
        };
        let main = find_node("main");
        let img = find_node("img");
        let text = document
            .iter()
            .find_map(|(id, node)| matches!(node.kind(), NodeKind::Text(_)).then_some(id))
            .expect("text node");
        let sizes = HashMap::from([(img, ImageSize { width: 20.0, height: 10.0 })]);
        let tree = LayoutTree::build_with_images(&document, &styles, 200.0, &sizes);

        let image_box = find(&tree.root, img).expect("image box");
        assert_eq!((image_box.rect.width, image_box.rect.height), (40.0, 20.0));
        let text_box = find(&tree.root, text).expect("text box");
        assert!(text_box.rect.y >= 20.0, "text must start below the image");
        assert!(find(&tree.root, main).expect("main box").rect.height >= 20.0);
    }

    #[test]
    fn font_aware_fragments_use_real_glyph_advances_and_wrap() {
        let document = HtmlDocument::parse("<main>MMMMMMMMMM</main>");
        let sheet = Stylesheet::parse("main { display:block; font-size:20px }").expect("valid CSS");
        let styles = StyleEngine::new().compute(&document, &[sheet]);
        let font =
            Font::from_bytes(include_bytes!("../../bweb-text/tests/data/test-font.ttf").to_vec())
                .expect("fixture font should load");
        let tree = LayoutTree::build_with_font(&document, &styles, 60.0, &font);
        let main = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "main")
                    .then_some(id)
            })
            .expect("main element");
        let main_box = find(&tree.root, main).expect("main layout box");

        assert!(main_box.line_boxes.len() > 1, "real advances must force wrapping");
        let glyph = font.glyph_for('M').expect("M is mapped");
        let advance = font.advance(glyph, 20.0);
        for line in &main_box.line_boxes {
            let measured: f32 = line
                .fragments
                .iter()
                .map(|fragment| advance * fragment.text.chars().count() as f32)
                .sum();
            assert!((line.rect.width - measured).abs() < 0.01);
            assert!(line.rect.width <= 60.0 + 0.01, "a line must fit the viewport");
        }
    }

    fn element(document: &HtmlDocument, tag: &str) -> bweb_html::NodeId {
        document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == tag)
                    .then_some(id)
            })
            .expect("element")
    }

    #[test]
    fn floats_are_contained_to_their_formatting_context() {
        let (document, tree) = layout(
            "<main><section><div>x</div></section>outside text</main>",
            "main { display:block; width:200px } section { display:block; width:100px } \
             div { float:left; width:60px; height:30px }",
        );
        // The float is inside <section>; sibling text of <main> must not wrap
        // around it because each block manages its own float list.
        let text = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Text(value) if value == "outside text")
                    .then_some(id)
            })
            .expect("text node");
        let text_box = find(&tree.root, text).expect("text box");
        assert_eq!(text_box.rect.x, 0.0);
    }

    #[test]
    fn multiple_floats_share_a_line() {
        let (document, tree) = layout(
            "<main><div id=a>x</div><div id=b>y</div>text</main>",
            "main { display:block; width:200px } #a { float:left; width:50px; height:20px } \
             #b { float:right; width:40px; height:20px }",
        );
        let text = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Text(value) if value == "text").then_some(id)
            })
            .expect("text node");
        let text_box = find(&tree.root, text).expect("text box");
        assert!(text_box.rect.x >= 50.0);
        assert!(text_box.rect.x + text_box.rect.width <= 160.0 + 1.0);
    }

    #[test]
    fn grid_places_named_areas() {
        let (document, tree) = layout(
            "<main><header>h</header><aside>a</aside><article>c</article></main>",
            "main { display:grid; width:300px; \
             grid-template-columns:repeat(2, 1fr); \
             grid-template-areas:'head head' 'side body' } \
             header { grid-area:head } aside { grid-area:side } article { grid-area:body }",
        );
        let box_for =
            |tag: &str| find(&tree.root, element(&document, tag)).expect("item box").clone();
        let header = box_for("header");
        let aside = box_for("aside");
        let article = box_for("article");
        assert_eq!(header.rect.x, 0.0);
        assert_eq!(header.rect.width, 300.0);
        assert_eq!(aside.rect.y, header.rect.height);
        assert_eq!(aside.rect.x, 0.0);
        assert_eq!(article.rect.x, 150.0);
        assert_eq!(article.rect.y, header.rect.height);
    }

    #[test]
    fn grid_honors_explicit_lines_and_spans() {
        let (document, tree) = layout(
            "<main><span id=a>1</span><span id=b>2</span><span id=c>3</span></main>",
            "main { display:grid; width:300px; grid-template-columns:repeat(2, 1fr) } \
             #a { grid-column:1 } #b { grid-column:2 } #c { grid-column:1 / span 2 }",
        );
        let box_for = |id: &str| {
            let node = document.element_by_id(id).expect("item");
            find(&tree.root, node).expect("item box").clone()
        };
        let a = box_for("a");
        let b = box_for("b");
        let c = box_for("c");
        assert_eq!(a.rect.x, 0.0);
        assert_eq!(b.rect.x, 150.0);
        assert_eq!(c.rect.x, 0.0);
        assert_eq!(c.rect.width, 300.0);
        assert!(c.rect.y > a.rect.y, "the spanning item should take the next row");
    }

    #[test]
    fn left_float_pushes_inline_text_to_its_right() {
        let (document, tree) = layout(
            "<main><div>x</div>hello world</main>",
            "main { display:block; width:200px } div { float:left; width:50px; height:40px }",
        );
        let text = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Text(value) if value == "hello world").then_some(id)
            })
            .expect("text node");
        let text_box = find(&tree.root, text).expect("text box");
        assert!(text_box.rect.x >= 50.0, "text should wrap beside the float: {:?}", text_box.rect);
    }

    #[test]
    fn right_float_anchors_to_the_content_edge() {
        let (document, tree) = layout(
            "<main><div>x</div></main>",
            "main { display:block; width:200px } div { float:right; width:50px; height:40px }",
        );
        let float_box = find(&tree.root, element(&document, "div")).expect("float box");
        assert_eq!(float_box.rect.x, 150.0);
    }

    #[test]
    fn clear_moves_a_block_below_the_float() {
        let (document, tree) = layout(
            "<main><div>x</div><p>after</p></main>",
            "main { display:block; width:200px } div { float:left; width:50px; height:40px } \
             p { display:block; clear:left }",
        );
        let paragraph = find(&tree.root, element(&document, "p")).expect("paragraph box");
        assert!(paragraph.rect.y >= 40.0, "clear should pass the float: {:?}", paragraph.rect);
    }

    #[test]
    fn grid_places_items_into_fractional_columns_and_implicit_rows() {
        let (document, tree) = layout(
            "<main><span>1</span><span>2</span><span>3</span><span>4</span></main>",
            "main { display:grid; width:300px; grid-template-columns:repeat(2, 1fr) } \
             span { display:block; height:20px }",
        );
        let main = find(&tree.root, element(&document, "main")).expect("grid box");
        let spans: Vec<_> = document
            .iter()
            .filter_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "span")
                    .then_some(id)
            })
            .collect();
        let boxes: Vec<_> = spans
            .iter()
            .map(|id| main.children.iter().find(|child| child.node == *id).expect("item"))
            .collect();
        assert_eq!(boxes[0].rect.width, 150.0);
        assert_eq!(boxes[1].rect.x, 150.0);
        assert_eq!(boxes[2].rect.y, 20.0);
        assert_eq!(boxes[3].rect.x, 150.0);
        assert_eq!(boxes[3].rect.y, 20.0);
    }

    #[test]
    fn grid_minmax_respects_the_minimum() {
        let (document, tree) = layout(
            "<main><span>1</span><span>2</span></main>",
            "main { display:grid; width:100px; grid-template-columns:minmax(80px, 1fr) 1fr } \
             span { display:block; height:10px }",
        );
        let main = find(&tree.root, element(&document, "main")).expect("grid box");
        let spans: Vec<_> = document
            .iter()
            .filter_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "span")
                    .then_some(id)
            })
            .collect();
        let boxes: Vec<_> = spans
            .iter()
            .map(|id| main.children.iter().find(|child| child.node == *id).expect("item"))
            .collect();
        assert!(boxes[0].rect.width >= 80.0, "minmax floor must be honored: {:?}", boxes[0].rect);
    }

    #[test]
    fn grid_honors_fixed_tracks_and_gaps() {
        let (document, tree) = layout(
            "<main><span>1</span><span>2</span></main>",
            "main { display:grid; width:300px; grid-template-columns:100px 50px; \
             column-gap:10px; row-gap:5px } span { display:block; height:10px }",
        );
        let main = find(&tree.root, element(&document, "main")).expect("grid box");
        let spans: Vec<_> = document
            .iter()
            .filter_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "span")
                    .then_some(id)
            })
            .collect();
        let boxes: Vec<_> = spans
            .iter()
            .map(|id| main.children.iter().find(|child| child.node == *id).expect("item"))
            .collect();
        assert_eq!(boxes[0].rect.width, 100.0);
        assert_eq!(boxes[1].rect.x, 110.0);
        assert_eq!(boxes[1].rect.width, 50.0);
    }

    #[test]
    fn relative_position_shifts_geometry_without_moving_siblings() {
        let (document, tree) = layout(
            "<main><div>one</div><div id=shift>two</div><div>three</div></main>",
            "div { display:block; width:40px; height:20px } #shift { position:relative; top:10px; left:5px }",
        );
        let divs: Vec<_> = document
            .iter()
            .filter_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "div")
                    .then_some(id)
            })
            .collect();
        let boxes: Vec<_> = divs.iter().map(|id| find(&tree.root, *id).expect("div box")).collect();
        assert_eq!(boxes[0].rect.y, 0.0);
        assert_eq!(boxes[1].rect.y, 30.0);
        assert_eq!(boxes[1].rect.x, 5.0);
        assert_eq!(boxes[1].flow_rect.y, 20.0);
        assert_eq!(boxes[2].rect.y, 40.0);
    }

    #[test]
    fn absolute_position_is_placed_in_its_containing_block() {
        let (document, tree) = layout(
            "<main><div><p>x</p><span>y</span></div></main>",
            "div { display:block; position:relative; width:100px; height:50px } \
             p { position:absolute; left:10px; top:5px; width:20px; height:10px } \
             span { display:block; width:10px; height:8px }",
        );
        let absolute = find(&tree.root, element(&document, "p")).expect("absolute box");
        assert!(absolute.out_of_flow);
        assert_eq!(absolute.rect.x, 10.0);
        assert_eq!(absolute.rect.y, 5.0);

        let sibling = find(&tree.root, element(&document, "span")).expect("sibling box");
        assert_eq!(sibling.rect.y, 0.0, "absolute items must not consume flow space");
    }

    #[test]
    fn flex_row_places_items_side_by_side() {
        let (document, tree) = layout(
            "<main><span>one</span><span>two</span></main>",
            "main { display:flex; width:300px } span { display:block; width:50px; height:20px }",
        );
        let main = find(&tree.root, element(&document, "main")).expect("main box");
        let spans: Vec<_> = document
            .iter()
            .filter_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "span")
                    .then_some(id)
            })
            .collect();
        let boxes: Vec<_> = spans
            .iter()
            .map(|id| main.children.iter().find(|child| child.node == *id).expect("span box"))
            .collect();
        assert_eq!(boxes[0].rect.x, 0.0);
        assert_eq!(boxes[0].rect.width, 50.0);
        assert_eq!(boxes[1].rect.x, 50.0);
        assert_eq!(boxes[1].rect.y, boxes[0].rect.y);
    }

    #[test]
    fn flex_wrap_creates_multiple_lines() {
        let (document, tree) = layout(
            "<main><span>1</span><span>2</span><span>3</span></main>",
            "main { display:flex; flex-wrap:wrap; width:100px; row-gap:5px } \
             span { display:block; width:60px; height:20px }",
        );
        let main = find(&tree.root, element(&document, "main")).expect("flex box");
        let spans: Vec<_> = document
            .iter()
            .filter_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "span")
                    .then_some(id)
            })
            .collect();
        let boxes: Vec<_> = spans
            .iter()
            .map(|id| main.children.iter().find(|child| child.node == *id).expect("item"))
            .collect();
        assert_eq!(boxes[0].rect.y, 0.0);
        assert_eq!(boxes[1].rect.y, 25.0);
        assert_eq!(boxes[2].rect.y, 50.0);
        assert_eq!(boxes[1].rect.x, 0.0);
    }

    #[test]
    fn flex_grow_distributes_free_space() {
        let (document, tree) = layout(
            "<main><span>one</span><span>two</span></main>",
            "main { display:flex; width:300px } span { display:block; flex:1 }",
        );
        let main = find(&tree.root, element(&document, "main")).expect("main box");
        let spans: Vec<_> = document
            .iter()
            .filter_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "span")
                    .then_some(id)
            })
            .collect();
        let widths: Vec<f32> = spans
            .iter()
            .map(|id| {
                main.children.iter().find(|child| child.node == *id).expect("span box").rect.width
            })
            .collect();
        assert_eq!(widths, vec![150.0, 150.0]);
    }

    #[test]
    fn justify_content_center_offsets_the_group() {
        let (document, tree) = layout(
            "<main><span>one</span><span>two</span></main>",
            "main { display:flex; width:300px; justify-content:center } span { display:block; width:50px }",
        );
        let main = find(&tree.root, element(&document, "main")).expect("main box");
        let first = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "span")
                    .then_some(id)
            })
            .expect("span");
        assert_eq!(
            main.children.iter().find(|child| child.node == first).expect("span box").rect.x,
            100.0
        );
    }

    #[test]
    fn flex_column_stacks_items_vertically() {
        let (document, tree) = layout(
            "<main><span>one</span><span>two</span></main>",
            "main { display:flex; flex-direction:column; height:100px } span { display:block; width:40px; height:20px }",
        );
        let main = find(&tree.root, element(&document, "main")).expect("main box");
        let spans: Vec<_> = document
            .iter()
            .filter_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "span")
                    .then_some(id)
            })
            .collect();
        let boxes: Vec<_> = spans
            .iter()
            .map(|id| main.children.iter().find(|child| child.node == *id).expect("span box"))
            .collect();
        assert_eq!(boxes[0].rect.y, 0.0);
        assert_eq!(boxes[1].rect.y, 20.0);
        assert_eq!(boxes[0].rect.x, boxes[1].rect.x);
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
