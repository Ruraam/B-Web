//! Minimal renderer port implemented by layout-side adapters.
//!
//! This module intentionally contains no DOM, CSS, or layout-engine types.

use crate::{Edges, Rect, Color};

/// Paint properties already resolved by the producer.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PaintStyle {
    pub background: Option<Color>,
    pub border_color: Option<Color>,
    /// Border widths in logical pixels.
    pub border_widths: Edges,
    /// Whether descendants must be clipped to this node's rectangle.
    pub clips_children: bool,
}

/// A paintable node with absolute logical-pixel geometry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaintNode {
    /// DFS pre-order depth; the root must have depth zero.
    pub depth: u32,
    pub rect: Rect,
    pub style: PaintStyle,
}

/// Adapter contract between a layout engine and the renderer.
pub trait PaintLayout {
    /// Logical-pixel viewport.
    fn viewport(&self) -> Rect;

    /// Visits nodes in deterministic DFS pre-order, parent before children.
    fn visit(&self, visitor: &mut dyn FnMut(PaintNode));
}
