//! Deterministic CPU rasterization of the renderer display list.
//!
//! The first milestone handles solid axis-aligned rectangles, rectangular
//! clipping, and square-corner borders. Coverage is evaluated at pixel centers.
//! Rounded corners and text/image rasterization are intentionally deferred.

use crate::{Color, DisplayList, Edges, Framebuffer, PaintItem, Rect};

/// CPU rasterizer for the backend-independent display list.
#[derive(Debug, Default, Clone, Copy)]
pub struct Rasterizer;

impl Rasterizer {
    /// Executes paint commands into an existing RGBA8 framebuffer.
    ///
    /// The framebuffer bounds are always the root clip. Unmatched PopClip
    /// commands are ignored, and any remaining nested clips are discarded when
    /// the list ends.
    pub fn rasterize(self, list: &DisplayList, framebuffer: &mut Framebuffer) {
        let root = Rect {
            x: 0.0,
            y: 0.0,
            width: framebuffer.width() as f32,
            height: framebuffer.height() as f32,
        };
        let mut clips = vec![root];

        for item in &list.items {
            match *item {
                PaintItem::FillRect { rect, color } => {
                    let clip = *clips.last().expect("root clip is always present");
                    paint_rect(framebuffer, rect, clip, color);
                }
                PaintItem::BorderRect { rect, widths, color, .. } => {
                    let clip = *clips.last().expect("root clip is always present");
                    paint_border(framebuffer, rect, widths, clip, color);
                }
                PaintItem::PushClip { rect } => {
                    let parent = *clips.last().expect("root clip is always present");
                    clips.push(intersect(parent, rect).unwrap_or_default());
                }
                PaintItem::PopClip => {
                    if clips.len() > 1 {
                        clips.pop();
                    }
                }
            }
        }
    }
}

fn paint_rect(framebuffer: &mut Framebuffer, rect: Rect, clip: Rect, color: Color) {
    let Some(area) = intersect(rect, clip) else {
        return;
    };
    for_each_pixel(framebuffer, area, |target, x, y| {
        target.set_pixel(x, y, color.to_rgba8());
    });
}

fn paint_border(
    framebuffer: &mut Framebuffer,
    rect: Rect,
    widths: Edges,
    clip: Rect,
    color: Color,
) {
    if !valid_rect(rect) || !valid_rect(clip) {
        return;
    }
    let widths = Edges {
        top: finite_non_negative(widths.top).min(rect.height),
        right: finite_non_negative(widths.right).min(rect.width),
        bottom: finite_non_negative(widths.bottom).min(rect.height),
        left: finite_non_negative(widths.left).min(rect.width),
    };
    if widths.top == 0.0 && widths.right == 0.0 && widths.bottom == 0.0 && widths.left == 0.0 {
        return;
    }
    let Some(area) = intersect(rect, clip) else {
        return;
    };

    for_each_pixel(framebuffer, area, |target, x, y| {
        let px = x as f32 + 0.5;
        let py = y as f32 + 0.5;
        let on_left = widths.left > 0.0 && px < rect.x + widths.left;
        let on_right = widths.right > 0.0 && px >= rect.x + rect.width - widths.right;
        let on_top = widths.top > 0.0 && py < rect.y + widths.top;
        let on_bottom = widths.bottom > 0.0 && py >= rect.y + rect.height - widths.bottom;
        if on_left || on_right || on_top || on_bottom {
            target.set_pixel(x, y, color.to_rgba8());
        }
    });
}

fn for_each_pixel(
    framebuffer: &mut Framebuffer,
    area: Rect,
    mut paint: impl FnMut(&mut Framebuffer, u32, u32),
) {
    if !valid_rect(area) || framebuffer.width() == 0 || framebuffer.height() == 0 {
        return;
    }

    let left = area.x.max(0.0).floor().max(0.0) as u32;
    let top = area.y.max(0.0).floor().max(0.0) as u32;
    let right = (area.x + area.width).min(framebuffer.width() as f32).ceil().max(0.0) as u32;
    let bottom = (area.y + area.height).min(framebuffer.height() as f32).ceil().max(0.0) as u32;

    for y in top..bottom.min(framebuffer.height()) {
        for x in left..right.min(framebuffer.width()) {
            let center_x = x as f32 + 0.5;
            let center_y = y as f32 + 0.5;
            if center_x >= area.x
                && center_x < area.x + area.width
                && center_y >= area.y
                && center_y < area.y + area.height
            {
                paint(framebuffer, x, y);
            }
        }
    }
}

fn intersect(left: Rect, right: Rect) -> Option<Rect> {
    if !valid_rect(left) || !valid_rect(right) {
        return None;
    }
    let x = left.x.max(right.x);
    let y = left.y.max(right.y);
    let right_edge = (left.x + left.width).min(right.x + right.width);
    let bottom_edge = (left.y + left.height).min(right.y + right.height);
    if right_edge <= x || bottom_edge <= y {
        return None;
    }
    Some(Rect { x, y, width: right_edge - x, height: bottom_edge - y })
}

fn valid_rect(rect: Rect) -> bool {
    rect.x.is_finite()
        && rect.y.is_finite()
        && rect.width.is_finite()
        && rect.height.is_finite()
        && rect.width > 0.0
        && rect.height > 0.0
        && (rect.x + rect.width).is_finite()
        && (rect.y + rect.height).is_finite()
}

fn finite_non_negative(value: f32) -> f32 {
    if value.is_finite() { value.max(0.0) } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::Rasterizer;
    use crate::{Color, DisplayList, Edges, Framebuffer, PaintItem, Rect};

    const RED: Color = Color::rgba(255, 0, 0, 255);
    const BLUE: Color = Color::rgba(0, 0, 255, 255);
    const GREEN: Color = Color::rgba(0, 255, 0, 255);

    fn rect(x: f32, y: f32, width: f32, height: f32) -> Rect {
        Rect { x, y, width, height }
    }

    fn fill(list: &mut DisplayList, rect: Rect, color: Color) {
        list.push(PaintItem::FillRect { rect, color });
    }

    #[test]
    fn fills_pixels_using_pixel_center_coverage() {
        let mut list = DisplayList::default();
        fill(&mut list, rect(0.5, 0.0, 1.0, 1.0), RED);
        let mut framebuffer = Framebuffer::new(2, 1).expect("framebuffer");

        Rasterizer.rasterize(&list, &mut framebuffer);

        assert_eq!(framebuffer.pixels(), &[255, 0, 0, 255, 0, 0, 0, 0]);
    }

    #[test]
    fn clips_drawing_to_nested_intersections() {
        let mut list = DisplayList::default();
        list.push(PaintItem::PushClip { rect: rect(1.0, 0.0, 3.0, 2.0) });
        list.push(PaintItem::PushClip { rect: rect(2.0, 0.0, 1.0, 2.0) });
        fill(&mut list, rect(0.0, 0.0, 5.0, 2.0), RED);
        list.push(PaintItem::PopClip);
        fill(&mut list, rect(0.0, 0.0, 5.0, 1.0), BLUE);
        list.push(PaintItem::PopClip);
        let mut framebuffer = Framebuffer::new(5, 2).expect("framebuffer");

        Rasterizer.rasterize(&list, &mut framebuffer);

        assert_eq!(
            framebuffer.pixels(),
            &[
                0, 0, 0, 0, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 255, 0, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ]
        );
    }

    #[test]
    fn paints_independent_square_border_sides_without_double_blending() {
        let mut list = DisplayList::default();
        list.push(PaintItem::BorderRect {
            rect: rect(0.0, 0.0, 4.0, 4.0),
            widths: Edges { top: 1.0, right: 2.0, bottom: 1.0, left: 1.0 },
            radius: 0.0,
            color: GREEN,
        });
        let mut framebuffer = Framebuffer::new(4, 4).expect("framebuffer");

        Rasterizer.rasterize(&list, &mut framebuffer);

        for y in 0..4 {
            for x in 0..4 {
                let offset = (y * 4 + x) * 4;
                let expected = if y == 0 || y == 3 || x == 0 || x >= 2 {
                    [0, 255, 0, 255]
                } else {
                    [0, 0, 0, 0]
                };
                assert_eq!(&framebuffer.pixels()[offset..offset + 4], &expected);
            }
        }
    }

    #[test]
    fn ignores_invalid_rectangles_and_unmatched_clip_pops() {
        let mut list = DisplayList::default();
        list.push(PaintItem::PopClip);
        fill(&mut list, rect(f32::NAN, 0.0, 2.0, 2.0), RED);
        fill(&mut list, rect(0.0, 0.0, -1.0, 2.0), BLUE);
        fill(&mut list, rect(0.0, 0.0, 2.0, 2.0), GREEN);
        let mut framebuffer = Framebuffer::new(2, 2).expect("framebuffer");

        Rasterizer.rasterize(&list, &mut framebuffer);

        assert_eq!(framebuffer.pixels(), &[0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255]);
    }
}
