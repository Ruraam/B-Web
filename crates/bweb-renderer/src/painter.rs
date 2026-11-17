//! Converts layout-port data into a deterministic physical-pixel DisplayList.

use std::sync::Arc;

use crate::display_list::DisplayList;
use crate::paint_layout::{PaintLayout, PaintNode};
use crate::{Edges, PaintItem, Rect};

/// Error returned when the device scale factor is invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PainterError {
    InvalidScaleFactor,
}

impl std::fmt::Display for PainterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidScaleFactor => {
                formatter.write_str("device scale factor must be finite and positive")
            }
        }
    }
}

impl std::error::Error for PainterError {}

/// Builds a display list from any implementation of the renderer's layout port.
#[derive(Debug, Clone, Copy)]
pub struct Painter<'a, L: PaintLayout> {
    layout: &'a L,
    scale_factor: f32,
}

impl<'a, L: PaintLayout> Painter<'a, L> {
    /// Creates a painter using a finite, strictly positive device scale factor.
    pub fn new(layout: &'a L, scale_factor: f32) -> Result<Self, PainterError> {
        if !scale_factor.is_finite() || scale_factor <= 0.0 {
            return Err(PainterError::InvalidScaleFactor);
        }
        Ok(Self { layout, scale_factor })
    }

    /// Paints nodes in adapter order and clips the complete list to the viewport.
    #[must_use]
    pub fn paint(self) -> DisplayList {
        let mut list = DisplayList::default();
        let viewport = scale_rect(self.layout.viewport(), self.scale_factor);
        list.push(PaintItem::PushClip { rect: viewport });

        // Each entry is the depth of a clip pushed for that node. A clip remains
        // active across all descendants and is popped before the next sibling.
        let mut clip_depths: Vec<u32> = Vec::new();
        let mut sticky_depths: Vec<u32> = Vec::new();

        self.layout.visit(&mut |node: PaintNode| {
            while clip_depths.last().is_some_and(|depth| *depth >= node.depth) {
                clip_depths.pop();
                list.push(PaintItem::PopClip);
            }
            while sticky_depths.last().is_some_and(|depth| *depth >= node.depth) {
                sticky_depths.pop();
                list.push(PaintItem::PopSticky);
            }
            if let Some((active, threshold, dx, dy)) = node.sticky {
                if active {
                    list.push(PaintItem::PushSticky {
                        threshold: threshold * self.scale_factor,
                        dx: dx * self.scale_factor,
                        dy: dy * self.scale_factor,
                    });
                    sticky_depths.push(node.depth);
                }
            }

            let rect = scale_rect(node.rect, self.scale_factor);
            if let Some(color) = node.style.background {
                list.push(PaintItem::FillRect { rect, color });
            }
            // Background images paint above the background color but below
            // borders, inline content and descendants.
            if let Some(image) = node.background_image {
                if let crate::BackgroundImage::Linear(gradient) = image.as_ref() {
                    list.push(PaintItem::LinearGradient {
                        rect,
                        gradient: Arc::new(gradient.clone()),
                    });
                }
            }
            if let Some(image) = node.background_image_bitmap {
                // CSS background images stretch to fill the padding box for now.
                list.push(PaintItem::Image { rect, image });
            }

            let widths = scale_edges(node.style.border_widths, self.scale_factor);
            if has_visible_width(widths) {
                if let Some(color) = node.style.border_color {
                    list.push(PaintItem::BorderRect { rect, widths, radius: 0.0, color });
                }
            }

            // Images and text are already positioned in physical pixels by
            // layout. Keep them after this node's border and before descendants.
            if let Some(image) = node.image {
                list.push(PaintItem::Image { rect, image });
            }
            if let Some(run) = node.text {
                list.push(PaintItem::Text { glyphs: run.glyphs.clone(), color: run.color });
            }

            if node.style.clips_children {
                list.push(PaintItem::PushClip { rect });
                clip_depths.push(node.depth);
            }
        });

        while clip_depths.pop().is_some() {
            list.push(PaintItem::PopClip);
        }
        while sticky_depths.pop().is_some() {
            list.push(PaintItem::PopSticky);
        }
        list.push(PaintItem::PopClip);
        list
    }
}

fn scale_rect(rect: Rect, scale: f32) -> Rect {
    Rect {
        x: rect.x * scale,
        y: rect.y * scale,
        width: rect.width * scale,
        height: rect.height * scale,
    }
}

fn scale_edges(edges: Edges, scale: f32) -> Edges {
    Edges {
        top: edges.top * scale,
        right: edges.right * scale,
        bottom: edges.bottom * scale,
        left: edges.left * scale,
    }
}

fn has_visible_width(edges: Edges) -> bool {
    edges.top > 0.0 || edges.right > 0.0 || edges.bottom > 0.0 || edges.left > 0.0
}

#[cfg(test)]
mod tests {
    use super::{Painter, PainterError};
    use crate::paint_layout::{PaintLayout, PaintNode, PaintStyle};
    use crate::{Color, Edges, PaintItem, PositionedGlyph, Rect, TextRun};
    use bweb_text::Coverage;
    use std::sync::Arc;

    const RED: Color = Color::rgba(255, 0, 0, 255);
    const BLUE: Color = Color::rgba(0, 0, 255, 255);
    const BLACK: Color = Color::rgba(0, 0, 0, 255);
    const WHITE: Color = Color::rgba(255, 255, 255, 255);

    struct MockLayout {
        viewport: Rect,
        nodes: Vec<PaintNode>,
    }

    impl PaintLayout for MockLayout {
        fn viewport(&self) -> Rect {
            self.viewport
        }

        fn visit(&self, visitor: &mut dyn FnMut(PaintNode)) {
            for node in &self.nodes {
                visitor(node.clone());
            }
        }
    }

    fn viewport() -> Rect {
        Rect { x: 0.0, y: 0.0, width: 10_000.0, height: 10_000.0 }
    }

    fn node(depth: u32, rect: Rect, style: PaintStyle) -> PaintNode {
        PaintNode { depth, rect, style, ..PaintNode::default() }
    }

    fn text_node(depth: u32, rect: Rect, style: PaintStyle, run: TextRun) -> PaintNode {
        PaintNode { depth, rect, style, text: Some(Arc::new(run)), ..PaintNode::default() }
    }

    fn simple_run() -> TextRun {
        TextRun {
            glyphs: vec![PositionedGlyph {
                coverage: Coverage { width: 1, height: 1, left: 0, top: 0, data: vec![255] },
                x: 12.0,
                y: 8.0,
            }],
            color: BLACK,
        }
    }

    #[test]
    fn emits_text_after_border_and_before_children_without_rescaling_positions() {
        let run = simple_run();
        let layout = MockLayout {
            viewport: viewport(),
            nodes: vec![
                text_node(
                    0,
                    Rect { x: 0.0, y: 0.0, width: 100.0, height: 40.0 },
                    PaintStyle {
                        background: Some(WHITE),
                        border_color: Some(BLACK),
                        border_widths: Edges { top: 1.0, right: 1.0, bottom: 1.0, left: 1.0 },
                        clips_children: false,
                    },
                    run.clone(),
                ),
                node(
                    1,
                    Rect { x: 10.0, y: 10.0, width: 20.0, height: 20.0 },
                    PaintStyle { background: Some(RED), ..PaintStyle::default() },
                ),
            ],
        };

        let list = Painter::new(&layout, 2.0).expect("valid scale").paint();
        assert!(matches!(list.items[1], PaintItem::FillRect { .. }));
        assert!(matches!(list.items[2], PaintItem::BorderRect { .. }));
        match &list.items[3] {
            PaintItem::Text { glyphs, color } => {
                assert_eq!(glyphs, &run.glyphs);
                assert_eq!(*color, run.color);
                assert_eq!(glyphs[0].x, 12.0);
                assert_eq!(glyphs[0].y, 8.0);
            }
            other => panic!("expected text before child, got {other:?}"),
        }
        assert!(matches!(list.items[4], PaintItem::FillRect { color: RED, .. }));
    }

    #[test]
    fn text_only_node_emits_a_text_command() {
        let layout = MockLayout {
            viewport: viewport(),
            nodes: vec![text_node(
                0,
                Rect { x: 0.0, y: 0.0, width: 100.0, height: 20.0 },
                PaintStyle::default(),
                simple_run(),
            )],
        };
        let list = Painter::new(&layout, 1.0).expect("valid scale").paint();
        assert!(matches!(list.items[1], PaintItem::Text { .. }));
    }

    #[test]
    fn rejects_invalid_scale_factors() {
        assert!(matches!(
            Painter::new(&MockLayout { viewport: viewport(), nodes: vec![] }, 0.0),
            Err(PainterError::InvalidScaleFactor)
        ));
        assert!(matches!(
            Painter::new(&MockLayout { viewport: viewport(), nodes: vec![] }, f32::NAN),
            Err(PainterError::InvalidScaleFactor)
        ));
        assert!(matches!(
            Painter::new(&MockLayout { viewport: viewport(), nodes: vec![] }, f32::INFINITY),
            Err(PainterError::InvalidScaleFactor)
        ));
    }

    #[test]
    fn empty_tree_yields_viewport_clip_only() {
        let layout = MockLayout { viewport: viewport(), nodes: vec![] };
        let list = Painter::new(&layout, 1.0).expect("valid scale").paint();
        assert_eq!(list.items.len(), 2);
        assert!(matches!(list.items[0], PaintItem::PushClip { .. }));
        assert_eq!(list.items[1], PaintItem::PopClip);
    }

    #[test]
    fn scales_viewport_and_filled_rectangle() {
        let layout = MockLayout {
            viewport: Rect { x: 0.0, y: 0.0, width: 800.0, height: 600.0 },
            nodes: vec![node(
                0,
                Rect { x: 10.0, y: 20.0, width: 100.0, height: 50.0 },
                PaintStyle { background: Some(RED), ..PaintStyle::default() },
            )],
        };
        let list = Painter::new(&layout, 2.0).expect("valid scale").paint();
        assert_eq!(
            list.items[0],
            PaintItem::PushClip { rect: Rect { x: 0.0, y: 0.0, width: 1600.0, height: 1200.0 } }
        );
        assert_eq!(
            list.items[1],
            PaintItem::FillRect {
                rect: Rect { x: 20.0, y: 40.0, width: 200.0, height: 100.0 },
                color: RED,
            }
        );
        assert_eq!(list.items[2], PaintItem::PopClip);
    }

    #[test]
    fn paints_background_before_border_with_independent_widths() {
        let layout = MockLayout {
            viewport: viewport(),
            nodes: vec![node(
                0,
                Rect { x: 0.0, y: 0.0, width: 100.0, height: 50.0 },
                PaintStyle {
                    background: Some(WHITE),
                    border_color: Some(BLACK),
                    border_widths: Edges { top: 1.0, right: 2.0, bottom: 3.0, left: 4.0 },
                    clips_children: false,
                },
            )],
        };
        let list = Painter::new(&layout, 1.0).expect("valid scale").paint();
        assert!(matches!(list.items[1], PaintItem::FillRect { .. }));
        assert_eq!(
            list.items[2],
            PaintItem::BorderRect {
                rect: Rect { x: 0.0, y: 0.0, width: 100.0, height: 50.0 },
                widths: Edges { top: 1.0, right: 2.0, bottom: 3.0, left: 4.0 },
                radius: 0.0,
                color: BLACK,
            }
        );
    }

    #[test]
    fn children_are_painted_after_parent() {
        let layout = MockLayout {
            viewport: viewport(),
            nodes: vec![
                node(
                    0,
                    Rect { x: 0.0, y: 0.0, width: 100.0, height: 100.0 },
                    PaintStyle { background: Some(WHITE), ..PaintStyle::default() },
                ),
                node(
                    1,
                    Rect { x: 10.0, y: 10.0, width: 20.0, height: 20.0 },
                    PaintStyle { background: Some(RED), ..PaintStyle::default() },
                ),
            ],
        };
        let list = Painter::new(&layout, 1.0).expect("valid scale").paint();
        assert_eq!(
            list.items[1],
            PaintItem::FillRect {
                rect: Rect { x: 0.0, y: 0.0, width: 100.0, height: 100.0 },
                color: WHITE,
            }
        );
        assert_eq!(
            list.items[2],
            PaintItem::FillRect {
                rect: Rect { x: 10.0, y: 10.0, width: 20.0, height: 20.0 },
                color: RED,
            }
        );
    }

    #[test]
    fn clip_is_kept_for_all_siblings_and_closed_before_next_parent_sibling() {
        let layout = MockLayout {
            viewport: viewport(),
            nodes: vec![
                node(
                    0,
                    Rect { x: 0.0, y: 0.0, width: 100.0, height: 100.0 },
                    PaintStyle { clips_children: true, ..PaintStyle::default() },
                ),
                node(
                    1,
                    Rect { x: 5.0, y: 5.0, width: 10.0, height: 10.0 },
                    PaintStyle { background: Some(RED), ..PaintStyle::default() },
                ),
                node(
                    1,
                    Rect { x: 20.0, y: 5.0, width: 10.0, height: 10.0 },
                    PaintStyle { background: Some(BLUE), ..PaintStyle::default() },
                ),
                node(
                    0,
                    Rect { x: 200.0, y: 0.0, width: 100.0, height: 100.0 },
                    PaintStyle { background: Some(WHITE), ..PaintStyle::default() },
                ),
            ],
        };
        let list = Painter::new(&layout, 1.0).expect("valid scale").paint();
        let items = &list.items;
        assert_eq!(items.len(), 7);
        assert!(matches!(items[0], PaintItem::PushClip { .. }));
        assert_eq!(
            items[1],
            PaintItem::PushClip { rect: Rect { x: 0.0, y: 0.0, width: 100.0, height: 100.0 } }
        );
        assert!(matches!(items[2], PaintItem::FillRect { color: RED, .. }));
        assert!(matches!(items[3], PaintItem::FillRect { color: BLUE, .. }));
        assert_eq!(items[4], PaintItem::PopClip);
        assert!(matches!(items[5], PaintItem::FillRect { color: WHITE, .. }));
        assert_eq!(items[6], PaintItem::PopClip);
    }

    #[test]
    fn nested_clips_are_closed_in_lifo_order() {
        let layout = MockLayout {
            viewport: viewport(),
            nodes: vec![
                node(
                    0,
                    Rect { x: 0.0, y: 0.0, width: 500.0, height: 500.0 },
                    PaintStyle { clips_children: true, ..PaintStyle::default() },
                ),
                node(
                    1,
                    Rect { x: 50.0, y: 50.0, width: 200.0, height: 200.0 },
                    PaintStyle { clips_children: true, ..PaintStyle::default() },
                ),
                node(
                    2,
                    Rect { x: 60.0, y: 60.0, width: 50.0, height: 50.0 },
                    PaintStyle { background: Some(RED), ..PaintStyle::default() },
                ),
            ],
        };
        let list = Painter::new(&layout, 1.0).expect("valid scale").paint();
        assert_eq!(list.items.len(), 7);
        assert!(matches!(list.items[0], PaintItem::PushClip { .. }));
        assert!(matches!(list.items[1], PaintItem::PushClip { .. }));
        assert!(matches!(list.items[2], PaintItem::PushClip { .. }));
        assert!(matches!(list.items[3], PaintItem::FillRect { color: RED, .. }));
        assert_eq!(list.items[4], PaintItem::PopClip);
        assert_eq!(list.items[5], PaintItem::PopClip);
        assert_eq!(list.items[6], PaintItem::PopClip);
    }
}
