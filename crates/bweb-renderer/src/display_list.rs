//! Backend-independent, deterministic paint commands.
//!
//! Rectangles are expressed in physical pixels as floating-point values.
//! The painter is responsible for applying the device scale factor before
//! adding commands to this list. Text and image commands will be introduced
//! by later rendering milestones.

use crate::{Edges, Rect};

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
    use super::{Color, DisplayList, PaintItem};
    use crate::{Edges, Rect};

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
    fn display_list_is_cloneable_for_deterministic_snapshots() {
        let mut list = DisplayList::default();
        list.push(PaintItem::FillRect {
            rect: Rect { x: 0.25, y: 0.5, width: 10.0, height: 20.0 },
            color: Color::rgba(255, 0, 0, 128),
        });
        assert_eq!(list.clone(), list);
    }
}
