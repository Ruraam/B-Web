//! Backend-independent rendering primitives.
//!
//! The GPU backend and layout-to-display-list pipeline are future milestones.

/// A rectangle in CSS-independent logical coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    /// Horizontal origin.
    pub x: f32,
    /// Vertical origin.
    pub y: f32,
    /// Width.
    pub width: f32,
    /// Height.
    pub height: f32,
}
