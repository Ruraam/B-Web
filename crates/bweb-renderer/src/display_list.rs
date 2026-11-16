//! Backend-independent, deterministic paint commands.
//!
//! Rectangles are expressed in physical pixels as floating-point values.
//! The painter is responsible for applying the device scale factor before
//! adding commands to this list. Text and image commands will be introduced
//! by later rendering milestones.

use crate::{Edges, Rect};
use bweb_text::Coverage;

/// An sRGB color with straight (non-premultiplied) alpha.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Color {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub alpha: u8,
}

impl Color {
    /// Creates a color from sRGB channel values and straight alpha.
    #[must_use]
    pub const fn rgba(red: u8, green: u8, blue: u8, alpha: u8) -> Self {
        Self { red, green, blue, alpha }
    }

    /// Returns the color as tightly ordered RGBA8 channels.
    #[must_use]
    pub const fn to_rgba8(self) -> [u8; 4] {
        [self.red, self.green, self.blue, self.alpha]
    }
}

/// A rasterized glyph positioned by its bitmap's top-left corner.
///
/// Coordinates are in physical pixels. Font bearings and baseline placement
/// must already have been applied by the producer (normally `bweb-layout`).
#[derive(Debug, Clone, PartialEq)]
pub struct PositionedGlyph {
    pub coverage: Coverage,
    pub x: f32,
    pub y: f32,
}

/// One ordered drawing or clipping command.
#[derive(Debug, Clone, PartialEq)]
pub enum PaintItem {
    /// Paints a solid rectangle.
    FillRect { rect: Rect, color: Color },
    /// Paints a border with independently sized sides.
    ///
    /// The current first rasterizer milestone may ignore `radius`; retaining
    /// it here keeps the display-list representation ready for rounded corners.
    BorderRect { rect: Rect, widths: Edges, radius: f32, color: Color },
    /// Blits pre-rasterized glyph masks at final physical-pixel positions.
    ///
    /// Glyph coordinates are snapped to the nearest pixel by the rasterizer.
    /// TODO(renderer-4): add quarter-pixel subpixel positioning and a matching
    /// glyph coverage cache without changing `PositionedGlyph`.
    Text { glyphs: Vec<PositionedGlyph>, color: Color },
    /// Intersects subsequent drawing with this physical-pixel rectangle.
    PushClip { rect: Rect },
    /// Restores the previous clipping rectangle.
    PopClip,
}

/// An ordered, snapshot-friendly sequence of paint commands.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DisplayList {
    /// Commands in painter-defined order.
    pub items: Vec<PaintItem>,
}

impl DisplayList {
    /// Appends a paint command without reordering it.
    pub fn push(&mut self, item: PaintItem) {
        self.items.push(item);
    }

    /// Returns the number of paint commands.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Returns whether the list contains no commands.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{Color, DisplayList, PaintItem, PositionedGlyph};
    use crate::{Edges, Rect};
    use bweb_text::Coverage;

    #[test]
    fn color_preserves_straight_rgba_channels() {
        let color = Color::rgba(12, 34, 56, 78);
        assert_eq!(color.to_rgba8(), [12, 34, 56, 78]);
    }

    #[test]
    fn display_list_keeps_commands_in_insertion_order() {
        let rect = Rect { x: 1.5, y: 2.5, width: 30.0, height: 40.0 };
        let fill = PaintItem::FillRect { rect, color: Color::rgba(1, 2, 3, 255) };
        let clip = PaintItem::PushClip { rect };
        let border = PaintItem::BorderRect {
            rect,
            widths: Edges { top: 1.0, right: 2.0, bottom: 3.0, left: 4.0 },
            radius: 0.0,
            color: Color::rgba(4, 5, 6, 255),
        };

        let mut list = DisplayList::default();
        assert!(list.is_empty());
        list.push(clip.clone());
        list.push(fill.clone());
        list.push(border.clone());
        list.push(PaintItem::PopClip);

        assert_eq!(list.len(), 4);
        assert_eq!(list.items, vec![clip, fill, border, PaintItem::PopClip]);
    }

    #[test]
    fn text_command_keeps_positioned_coverage_and_order() {
        let glyph = PositionedGlyph {
            coverage: Coverage { width: 1, height: 1, left: 99, top: 99, data: vec![255] },
            x: 2.25,
            y: 3.75,
        };
        let mut list = DisplayList::default();
        list.push(PaintItem::Text {
            glyphs: vec![glyph.clone()],
            color: Color::rgba(10, 20, 30, 255),
        });

        assert!(matches!(&list.items[0], PaintItem::Text { glyphs, .. } if glyphs == &[glyph]));
    }

    #[test]
    fn display_list_is_cloneable_for_deterministic_snapshots() {
        let mut list = DisplayList::default();
        list.push(PaintItem::FillRect {
            rect: Rect { x: 0.25, y: 0.5, width: 10.0, height: 20.0 },
            color: Color::rgba(255, 0, 0, 128),
        });
        assert_eq!(list.clone(), list);
    }
}
