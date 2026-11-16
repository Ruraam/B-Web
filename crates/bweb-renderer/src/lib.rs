//! Backend-independent rendering primitives.
//!
//! This crate deliberately has no dependency on the DOM, CSS, or layout engine.

mod display_list;
mod framebuffer;

pub use display_list::{Color, DisplayList, PaintItem};
pub use framebuffer::{Framebuffer, FramebufferError};

/// A rectangle in physical pixel coordinates for rendering commands.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Four-sided values, stored as top/right/bottom/left.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Edges {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}
