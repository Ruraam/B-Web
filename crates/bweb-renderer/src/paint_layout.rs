//! Minimal renderer port implemented by layout-side adapters.
//!
//! This module intentionally contains no DOM, CSS, or layout-engine types.

use std::sync::Arc;

use crate::{BackgroundImage, Color, Edges, Image, Rect, TextRun};

/// Paint properties already resolved by the producer.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PaintStyle {
    pub background: Option<Color>,
    pub border_color: Option<Color>,
    /// Border widths in logical pixels.
    pub border_widths: Edges,
    /// Whether descendants must be clipped to this node's rectangle.
    pub clips_children: bool,
    /// Corner radius in logical pixels for background, border and clipping.
    pub border_radius: f32,
}

/// A paintable node with absolute logical-pixel geometry and optional text.
///
/// Text runs contain final physical-pixel glyph positions. The Arc lets an
/// adapter share one prepared run across visits without cloning glyph masks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PaintNode {
    /// DFS pre-order depth; the root must have depth zero.
    pub depth: u32,
    pub rect: Rect,
    pub style: PaintStyle,
    /// Prepared text run, if this node paints text.
    pub text: Option<Arc<TextRun>>,
    /// Decoded image, if this node paints a replaced element.
    pub image: Option<Arc<Image>>,
    /// Background image, if this node paints a CSS gradient or image layer.
    pub background_image: Option<Arc<BackgroundImage>>,
    /// Decoded bitmap for a `url()` background, matched by resolved URL.
    pub background_image_bitmap: Option<Arc<Image>>,
    /// Computed `background-size`, `background-position` and `background-repeat`
    /// (first layer) for the image background.
    pub background_size: Option<String>,
    pub background_position: Option<String>,
    pub background_repeat: Option<String>,
    /// CSS `transform` value for this node.
    pub transform: Option<String>,
    /// CSS `opacity` for this node and its subtree.
    pub opacity: Option<String>,
    /// CSS `filter` value for this node and its subtree.
    pub filter: Option<String>,
    /// Scroll-dependent sticky shift: `(active, threshold, dx, dy)`.
    pub sticky: Option<(bool, f32, f32, f32)>,
}

/// Adapter contract between a layout engine and the renderer.
pub trait PaintLayout {
    /// Logical-pixel viewport.
    fn viewport(&self) -> Rect;

    /// Visits nodes in deterministic DFS pre-order, parent before children.
    fn visit(&self, visitor: &mut dyn FnMut(PaintNode));
}
