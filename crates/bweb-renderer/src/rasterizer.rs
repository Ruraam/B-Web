//! Deterministic CPU rasterization of the renderer display list.
//!
//! Handles solid rectangles, rounded corners, borders, linear gradients,
//! glyph masks and scaled image blits, with rectangular and rounded clipping,
//! composable color filters, subtree opacity and 2D transforms. Coverage is
//! evaluated at pixel centers. Subpixel glyph positioning remains planned.

use crate::{
    Color, DisplayList, Edges, Framebuffer, GradientStopPosition, LinearGradient, PaintItem, Rect,
    Transform,
};

fn effective_color(color: Color, opacities: &[f32], filters: &[crate::ColorFilter]) -> Color {
    let alpha = *opacities.last().expect("root opacity");
    let filter = *filters.last().expect("root filter");
    let filtered = filter.apply(color);
    Color::rgba(
        filtered.red,
        filtered.green,
        filtered.blue,
        (f32::from(color.alpha) * alpha).round() as u8,
    )
}

/// Returns the destination bounding box of a transformed rect and, when the
/// transform is not axis-aligned, an inverse mapping so each pixel is sampled
/// in local space. This makes rotate()/skew() render correctly.
fn mapped_rect(rect: Rect, transform: Transform) -> (Rect, Option<Mapping>) {
    let bounds = transform.rect(rect);
    let mapping = if transform.is_axis_aligned() {
        None
    } else {
        transform.inverse().map(|inverse| Mapping { inverse, local_rect: rect })
    };
    (bounds, mapping)
}

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
        let mut clip_radii: Vec<f32> = vec![0.0];
        let mut transforms = vec![Transform::IDENTITY];
        let mut opacities: Vec<f32> = vec![1.0];
        let mut filters: Vec<crate::ColorFilter> = vec![crate::ColorFilter::default()];
        for item in &list.items {
            match item {
                PaintItem::FillRect { rect, color } => {
                    let clip = *clips.last().expect("root clip is always present");
                    let (bounds, mapping) =
                        mapped_rect(*rect, *transforms.last().expect("root transform"));
                    paint_rect(
                        framebuffer,
                        bounds,
                        clip,
                        *clip_radii.last().expect("root radius"),
                        effective_color(*color, &opacities, &filters),
                        mapping,
                    );
                }
                PaintItem::BorderRect { rect, widths, color, .. } => {
                    let clip = *clips.last().expect("root clip is always present");
                    let (bounds, mapping) =
                        mapped_rect(*rect, *transforms.last().expect("root transform"));
                    paint_border(
                        framebuffer,
                        bounds,
                        *widths,
                        clip,
                        *clip_radii.last().expect("root radius"),
                        effective_color(*color, &opacities, &filters),
                        mapping,
                    );
                }
                PaintItem::LinearGradient { rect, gradient } => {
                    let clip = *clips.last().expect("root clip is always present");
                    let (bounds, mapping) =
                        mapped_rect(*rect, *transforms.last().expect("root transform"));
                    paint_linear_gradient(
                        framebuffer,
                        bounds,
                        gradient,
                        clip,
                        *clip_radii.last().expect("root radius"),
                        *opacities.last().expect("root opacity"),
                        mapping,
                    );
                }
                PaintItem::Text { glyphs, color, decoration } => {
                    let clip = *clips.last().expect("root clip is always present");
                    let transform = *transforms.last().expect("root transform");
                    let moved: Vec<_> = glyphs
                        .iter()
                        .map(|glyph| {
                            let (x, y) = transform.point(glyph.x, glyph.y);
                            crate::PositionedGlyph { coverage: glyph.coverage.clone(), x, y }
                        })
                        .collect();
                    let text_color = effective_color(*color, &opacities, &filters);
                    blit_glyphs(
                        framebuffer,
                        &moved,
                        clip,
                        *clip_radii.last().expect("root radius"),
                        text_color,
                    );
                    if *decoration != crate::TextDecoration::None {
                        let height = moved
                            .iter()
                            .filter_map(|glyph| {
                                (glyph.coverage.height > 0).then_some(glyph.coverage.height)
                            })
                            .max()
                            .unwrap_or(0);
                        if height > 0 {
                            let thickness = (height as f32 * 0.08).max(1.0);
                            for glyph in &moved {
                                let line_y = if *decoration == crate::TextDecoration::Underline {
                                    glyph.y + height as f32 + 1.0
                                } else {
                                    glyph.y + height as f32 * 0.5
                                };
                                let rect = Rect {
                                    x: glyph.x,
                                    y: line_y,
                                    width: glyph.coverage.width as f32,
                                    height: thickness,
                                };
                                paint_rect(
                                    framebuffer,
                                    rect,
                                    clip,
                                    *clip_radii.last().expect("root radius"),
                                    effective_color(*color, &opacities, &filters),
                                    None,
                                );
                            }
                        }
                    }
                }
                PaintItem::Image { rect, image } => {
                    let clip = *clips.last().expect("root clip is always present");
                    let (bounds, mapping) =
                        mapped_rect(*rect, *transforms.last().expect("root transform"));
                    paint_image(
                        framebuffer,
                        image,
                        bounds,
                        clip,
                        *clip_radii.last().expect("root radius"),
                        *opacities.last().expect("root opacity"),
                        mapping,
                    );
                }
                PaintItem::PushSticky { .. } | PaintItem::PopSticky => {
                    // Interpreted by the shell's scroll translation pass.
                }
                PaintItem::PushFilter { filter } => {
                    let parent = *filters.last().expect("root filter");
                    filters.push(parent.compose(*filter));
                }
                PaintItem::PopFilter => {
                    if filters.len() > 1 {
                        filters.pop();
                    }
                }
                PaintItem::PushOpacity { alpha } => {
                    let parent = *opacities.last().expect("root opacity");
                    opacities.push(parent * alpha.clamp(0.0, 1.0));
                }
                PaintItem::PopOpacity => {
                    if opacities.len() > 1 {
                        opacities.pop();
                    }
                }
                PaintItem::PushTransform { transform } => {
                    let parent = *transforms.last().expect("root transform");
                    transforms.push(parent.compose(*transform));
                }
                PaintItem::PopTransform => {
                    if transforms.len() > 1 {
                        transforms.pop();
                    }
                }
                PaintItem::PushClip { rect } => {
                    let parent = *clips.last().expect("root clip is always present");
                    let transformed = transforms.last().expect("root transform").rect(*rect);
                    clips.push(intersect(parent, transformed).unwrap_or_default());
                    clip_radii.push(0.0);
                }
                PaintItem::PushRoundClip { rect, radius } => {
                    let parent = *clips.last().expect("root clip is always present");
                    let transformed = transforms.last().expect("root transform").rect(*rect);
                    clips.push(intersect(parent, transformed).unwrap_or_default());
                    clip_radii.push(radius.max(0.0));
                }
                PaintItem::PopClip => {
                    if clips.len() > 1 {
                        clips.pop();
                        clip_radii.pop();
                    }
                }
            }
        }
    }
}

/// Blits pre-rasterized glyph masks. Bearings are intentionally ignored:
/// producers must supply final bitmap top-left positions in physical pixels.
fn blit_glyphs(
    framebuffer: &mut Framebuffer,
    glyphs: &[crate::PositionedGlyph],
    clip: Rect,
    clip_radius: f32,
    color: Color,
) {
    if !valid_rect(clip) || color.alpha == 0 {
        return;
    }

    for glyph in glyphs {
        if !glyph.x.is_finite() || !glyph.y.is_finite() {
            continue;
        }
        let coverage = &glyph.coverage;
        if coverage.width == 0 || coverage.height == 0 {
            continue;
        }

        // TODO(renderer-4): retain fractional positions for quarter-pixel
        // coverage-cache lookup; this milestone deliberately snaps to pixels.
        let origin_x = glyph.x.round() as i64;
        let origin_y = glyph.y.round() as i64;
        let Some(expected_len) = (coverage.width as usize).checked_mul(coverage.height as usize)
        else {
            continue;
        };

        for (index, &mask_alpha) in coverage.data.iter().take(expected_len).enumerate() {
            if mask_alpha == 0 {
                continue;
            }
            let local_x = index % coverage.width as usize;
            let local_y = index / coverage.width as usize;
            let (Ok(local_x), Ok(local_y)) = (i64::try_from(local_x), i64::try_from(local_y))
            else {
                continue;
            };
            let (Some(x), Some(y)) = (origin_x.checked_add(local_x), origin_y.checked_add(local_y))
            else {
                continue;
            };
            if x < 0
                || y < 0
                || x >= i64::from(framebuffer.width())
                || y >= i64::from(framebuffer.height())
            {
                continue;
            }

            let pixel_center_x = x as f32 + 0.5;
            let pixel_center_y = y as f32 + 0.5;
            if pixel_center_x < clip.x
                || pixel_center_x >= clip.x + clip.width
                || pixel_center_y < clip.y
                || pixel_center_y >= clip.y + clip.height
                || !inside_rounded(clip, clip_radius, pixel_center_x, pixel_center_y)
            {
                continue;
            }

            let alpha = ((u16::from(mask_alpha) * u16::from(color.alpha) + 127) / 255) as u8;
            if alpha == 0 {
                continue;
            }
            framebuffer.set_pixel(x as u32, y as u32, [color.red, color.green, color.blue, alpha]);
        }
    }
}

/// Nearest-neighbor image blit with source-over alpha and clipping.
fn paint_image(
    framebuffer: &mut Framebuffer,
    image: &crate::Image,
    rect: Rect,
    clip: Rect,
    clip_radius: f32,
    opacity: f32,
    mapping: Option<Mapping>,
) {
    let Some(area) = intersect(rect, clip) else {
        return;
    };
    let source_width = image.width();
    let source_height = image.height();
    let pixels = image.pixels();
    for_each_pixel(framebuffer, area, clip_radius, mapping, |target, x, y, local_x, local_y| {
        let source_x = (f64::from(local_x) - f64::from(rect.x)) * f64::from(source_width)
            / f64::from(rect.width);
        let source_y = (f64::from(local_y) - f64::from(rect.y)) * f64::from(source_height)
            / f64::from(rect.height);
        let source_x = (source_x as usize).min(source_width as usize - 1);
        let source_y = (source_y as usize).min(source_height as usize - 1);
        let offset = (source_y * source_width as usize + source_x) * 4;
        if let Some(pixel) = pixels.get(offset..offset + 4) {
            let alpha = (f32::from(pixel[3]) * opacity).round() as u8;
            let rgba = [pixel[0], pixel[1], pixel[2], alpha];
            target.set_pixel(x, y, rgba);
        }
    });
}

/// Paints a linear gradient across a rectangle. Stop positions are resolved
/// against the gradient line, then sampled at every pixel center.
fn paint_linear_gradient(
    framebuffer: &mut Framebuffer,
    rect: Rect,
    gradient: &LinearGradient,
    clip: Rect,
    clip_radius: f32,
    opacity: f32,
    mapping: Option<Mapping>,
) {
    let Some(area) = intersect(rect, clip) else {
        return;
    };
    if gradient.stops.is_empty() {
        return;
    }

    let (sine, cosine) = gradient.angle.to_radians().sin_cos();
    // The gradient line crosses the center in the direction (sin, -cos).
    let line_length = (rect.width * sine.abs() + rect.height * cosine.abs()).max(f32::EPSILON);
    let center_x = rect.x + rect.width / 2.0;
    let center_y = rect.y + rect.height / 2.0;
    let positions = resolve_stop_positions(&gradient.stops, line_length);

    for_each_pixel(framebuffer, area, clip_radius, mapping, |target, x, y, local_x, local_y| {
        let pixel_x = local_x;
        let pixel_y = local_y;
        let projection =
            ((pixel_x - center_x) * sine - (pixel_y - center_y) * cosine) / line_length + 0.5;
        let color = interpolate_stops(&gradient.stops, &positions, projection);
        let alpha = (f32::from(color.alpha) * opacity).round() as u8;
        target.set_pixel(x, y, Color::rgba(color.red, color.green, color.blue, alpha).to_rgba8());
    });
}

/// Resolves implicit stop positions (first 0, last 1, evenly spaced between
/// known neighbors) and clamps them to a monotonic 0..1 sequence.
fn resolve_stop_positions(stops: &[crate::ColorStop], line_length: f32) -> Vec<f32> {
    let mut positions: Vec<f32> = stops
        .iter()
        .map(|stop| match stop.position {
            Some(GradientStopPosition::Fraction(fraction)) => fraction,
            Some(GradientStopPosition::Length(length)) => length / line_length,
            None => f32::NAN,
        })
        .collect();
    let last = positions.len() - 1;
    if positions[0].is_nan() {
        positions[0] = 0.0;
    }
    if positions[last].is_nan() {
        positions[last] = 1.0;
    }
    for index in 0..last {
        if positions[index].is_finite() {
            continue;
        }
        let previous =
            positions[..index].iter().rposition(|position| position.is_finite()).unwrap_or(0);
        let next = positions[index + 1..]
            .iter()
            .position(|position| position.is_finite())
            .map(|offset| index + 1 + offset)
            .unwrap_or(last);
        let span = (next - previous) as f32;
        let fraction = if span > 0.0 { (index - previous) as f32 / span } else { 0.0 };
        positions[index] = positions[previous] + (positions[next] - positions[previous]) * fraction;
    }
    for index in 1..=last {
        positions[index] = positions[index].max(positions[index - 1]).clamp(0.0, 1.0);
    }
    positions[0] = positions[0].clamp(0.0, 1.0);
    positions
}

fn interpolate_stops(stops: &[crate::ColorStop], positions: &[f32], value: f32) -> Color {
    let value = value.clamp(0.0, 1.0);
    if value <= positions[0] {
        return stops[0].color;
    }
    for index in 1..stops.len() {
        if value <= positions[index] {
            let start = positions[index - 1];
            let span = positions[index] - start;
            let fraction = if span <= f32::EPSILON { 1.0 } else { (value - start) / span };
            return mix_colors(stops[index - 1].color, stops[index].color, fraction);
        }
    }
    stops[stops.len() - 1].color
}

fn mix_colors(from: Color, to: Color, fraction: f32) -> Color {
    let mix = |start: u8, end: u8| {
        (f32::from(start) + (f32::from(end) - f32::from(start)) * fraction)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color::rgba(
        mix(from.red, to.red),
        mix(from.green, to.green),
        mix(from.blue, to.blue),
        mix(from.alpha, to.alpha),
    )
}

fn paint_rect(
    framebuffer: &mut Framebuffer,
    rect: Rect,
    clip: Rect,
    clip_radius: f32,
    color: Color,
    mapping: Option<Mapping>,
) {
    let Some(area) = intersect(rect, clip) else {
        return;
    };
    for_each_pixel(framebuffer, area, clip_radius, mapping, |target, x, y, _local_x, _local_y| {
        target.set_pixel(x, y, color.to_rgba8());
    });
}

fn paint_border(
    framebuffer: &mut Framebuffer,
    rect: Rect,
    widths: Edges,
    clip: Rect,
    clip_radius: f32,
    color: Color,
    mapping: Option<Mapping>,
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

    for_each_pixel(framebuffer, area, clip_radius, mapping, |target, x, y, local_x, local_y| {
        let px = local_x;
        let py = local_y;
        let on_left = widths.left > 0.0 && px < rect.x + widths.left;
        let on_right = widths.right > 0.0 && px >= rect.x + rect.width - widths.right;
        let on_top = widths.top > 0.0 && py < rect.y + widths.top;
        let on_bottom = widths.bottom > 0.0 && py >= rect.y + rect.height - widths.bottom;
        if on_left || on_right || on_top || on_bottom {
            target.set_pixel(x, y, color.to_rgba8());
        }
    });
}

/// True when a pixel center lies inside a rounded rectangle.
fn inside_rounded(rect: Rect, radius: f32, px: f32, py: f32) -> bool {
    let Some(r) = (radius > 0.0).then_some(radius.min(rect.width * 0.5).min(rect.height * 0.5))
    else {
        return true;
    };
    let left = rect.x + r;
    let right = rect.x + rect.width - r;
    let top = rect.y + r;
    let bottom = rect.y + rect.height - r;
    let cx = if px < left {
        left
    } else if px > right {
        right
    } else {
        px
    };
    let cy = if py < top {
        top
    } else if py > bottom {
        bottom
    } else {
        py
    };
    let dx = px - cx;
    let dy = py - cy;
    dx * dx + dy * dy <= r * r
}

/// Describes an inverse mapping from destination (screen) pixels to a rectangle
/// in local space. When present, only pixels whose pre-image lies inside
/// `local_rect` are painted, and the callback receives local coordinates.
#[derive(Clone, Copy)]
struct Mapping {
    inverse: Transform,
    local_rect: Rect,
}

fn for_each_pixel(
    framebuffer: &mut Framebuffer,
    area: Rect,
    clip_radius: f32,
    mapping: Option<Mapping>,
    mut paint: impl FnMut(&mut Framebuffer, u32, u32, f32, f32),
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
            if center_x < area.x
                || center_x >= area.x + area.width
                || center_y < area.y
                || center_y >= area.y + area.height
                || !inside_rounded(area, clip_radius, center_x, center_y)
            {
                continue;
            }
            let (sample_x, sample_y) = match mapping {
                Some(mapping) => {
                    let (local_x, local_y) = mapping.inverse.point(center_x, center_y);
                    if local_x < mapping.local_rect.x
                        || local_x >= mapping.local_rect.x + mapping.local_rect.width
                        || local_y < mapping.local_rect.y
                        || local_y >= mapping.local_rect.y + mapping.local_rect.height
                        || !inside_rounded(mapping.local_rect, clip_radius, local_x, local_y)
                    {
                        continue;
                    }
                    (local_x, local_y)
                }
                None => (center_x, center_y),
            };
            paint(framebuffer, x, y, sample_x, sample_y);
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
    use crate::{
        Color, ColorStop, DisplayList, Edges, Framebuffer, Image, LinearGradient, PaintItem, Rect,
    };
    use std::sync::Arc;

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
    fn scales_a_decoded_image_into_its_rect() {
        let image = Arc::new(Image::new(1, 1, vec![255, 0, 0, 255]).expect("image"));
        let mut list = DisplayList::default();
        list.push(PaintItem::PushClip { rect: Rect { x: 0.0, y: 0.0, width: 4.0, height: 4.0 } });
        list.push(PaintItem::Image {
            rect: Rect { x: 0.0, y: 0.0, width: 2.0, height: 2.0 },
            image,
        });
        list.push(PaintItem::PopClip);

        let mut framebuffer = Framebuffer::new(4, 4).expect("framebuffer");
        framebuffer.clear([255, 255, 255, 255]);
        Rasterizer.rasterize(&list, &mut framebuffer);
        assert_eq!(framebuffer.pixels()[0..4], [255, 0, 0, 255]);
        assert_eq!(framebuffer.pixels()[12..16], [255, 255, 255, 255]);
    }

    #[test]
    fn blits_glyph_coverage_with_color_alpha_and_integer_snapping() {
        let mut list = DisplayList::default();
        list.push(PaintItem::Text {
            glyphs: vec![crate::PositionedGlyph {
                coverage: bweb_text::Coverage {
                    width: 2,
                    height: 1,
                    left: 100,
                    top: 100,
                    data: vec![255, 128],
                },
                x: 0.6,
                y: 0.4,
            }],
            color: Color::rgba(200, 100, 50, 128),
            decoration: crate::TextDecoration::None,
        });
        let mut framebuffer = Framebuffer::new(3, 1).expect("framebuffer");

        Rasterizer.rasterize(&list, &mut framebuffer);

        assert_eq!(&framebuffer.pixels()[0..4], &[0, 0, 0, 0]);
        assert_eq!(&framebuffer.pixels()[4..8], &[200, 100, 50, 128]);
        assert_eq!(&framebuffer.pixels()[8..12], &[200, 100, 50, 64]);
    }

    #[test]
    fn glyph_blit_respects_nested_clip_and_framebuffer_bounds() {
        let mut list = DisplayList::default();
        list.push(PaintItem::PushClip { rect: rect(1.0, 0.0, 2.0, 1.0) });
        list.push(PaintItem::Text {
            glyphs: vec![crate::PositionedGlyph {
                coverage: bweb_text::Coverage {
                    width: 4,
                    height: 1,
                    left: 0,
                    top: 0,
                    data: vec![255; 4],
                },
                x: -1.0,
                y: 0.0,
            }],
            color: RED,
            decoration: crate::TextDecoration::None,
        });
        list.push(PaintItem::PopClip);
        let mut framebuffer = Framebuffer::new(3, 1).expect("framebuffer");

        Rasterizer.rasterize(&list, &mut framebuffer);

        assert_eq!(framebuffer.pixels(), &[0, 0, 0, 0, 255, 0, 0, 255, 255, 0, 0, 255],);
    }

    #[test]
    fn glyph_blit_ignores_invalid_positions_and_malformed_coverage_tail() {
        let mut list = DisplayList::default();
        list.push(PaintItem::Text {
            glyphs: vec![
                crate::PositionedGlyph {
                    coverage: bweb_text::Coverage {
                        width: 2,
                        height: 2,
                        left: 0,
                        top: 0,
                        data: vec![255],
                    },
                    x: 0.0,
                    y: 0.0,
                },
                crate::PositionedGlyph {
                    coverage: bweb_text::Coverage {
                        width: 1,
                        height: 1,
                        left: 0,
                        top: 0,
                        data: vec![255],
                    },
                    x: f32::NAN,
                    y: 0.0,
                },
            ],
            color: RED,
            decoration: crate::TextDecoration::None,
        });
        let mut framebuffer = Framebuffer::new(2, 2).expect("framebuffer");

        Rasterizer.rasterize(&list, &mut framebuffer);

        assert_eq!(&framebuffer.pixels()[0..4], &[255, 0, 0, 255]);
        assert!(framebuffer.pixels()[4..].iter().all(|byte| *byte == 0));
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
    fn rasterizes_a_horizontal_gradient_across_the_box() {
        let gradient = LinearGradient {
            angle: 90.0,
            stops: vec![
                ColorStop { color: RED, position: None },
                ColorStop { color: BLUE, position: None },
            ],
        };
        let mut list = DisplayList::default();
        list.push(PaintItem::LinearGradient {
            rect: rect(0.0, 0.0, 4.0, 1.0),
            gradient: Arc::new(gradient),
        });
        let mut framebuffer = Framebuffer::new(4, 1).expect("framebuffer");

        Rasterizer.rasterize(&list, &mut framebuffer);

        assert_eq!(
            framebuffer.pixels(),
            &[223, 0, 32, 255, 159, 0, 96, 255, 96, 0, 159, 255, 32, 0, 223, 255]
        );
    }

    #[test]
    fn gradient_is_clipped_and_honors_explicit_stop_positions() {
        use crate::GradientStopPosition;
        let gradient = LinearGradient {
            angle: 90.0,
            stops: vec![
                ColorStop { color: RED, position: Some(GradientStopPosition::Fraction(0.5)) },
                ColorStop { color: BLUE, position: Some(GradientStopPosition::Fraction(0.5)) },
            ],
        };
        let mut list = DisplayList::default();
        list.push(PaintItem::PushClip { rect: rect(1.0, 0.0, 1.0, 1.0) });
        list.push(PaintItem::LinearGradient {
            rect: rect(0.0, 0.0, 4.0, 1.0),
            gradient: Arc::new(gradient),
        });
        list.push(PaintItem::PopClip);
        let mut framebuffer = Framebuffer::new(4, 1).expect("framebuffer");

        Rasterizer.rasterize(&list, &mut framebuffer);

        // The clipped pixel at x=1.5 projects before the 50% stop, so it keeps
        // the first stop's color; everything outside the clip stays empty.
        assert_eq!(&framebuffer.pixels()[0..4], &[0, 0, 0, 0]);
        assert_eq!(&framebuffer.pixels()[4..8], &[255, 0, 0, 255]);
        assert_eq!(&framebuffer.pixels()[8..12], &[0, 0, 0, 0]);
    }

    #[test]
    fn underline_draws_a_line_below_glyphs() {
        let glyph = crate::PositionedGlyph {
            coverage: bweb_text::Coverage {
                width: 4,
                height: 6,
                left: 0,
                top: 0,
                data: vec![255; 24],
            },
            x: 1.0,
            y: 1.0,
        };
        let mut list = DisplayList::default();
        list.push(PaintItem::Text {
            glyphs: vec![glyph],
            color: RED,
            decoration: crate::TextDecoration::Underline,
        });
        let mut framebuffer = Framebuffer::new(6, 10).expect("framebuffer");
        Rasterizer.rasterize(&list, &mut framebuffer);

        // Row 8 (below the glyph) should contain the underline pixels.
        let row: Vec<u8> = framebuffer.pixels()[8 * 6 * 4..9 * 6 * 4].to_vec();
        assert!(row.chunks_exact(4).any(|pixel| pixel == [255, 0, 0, 255]));
    }

    #[test]
    fn color_filter_applies_to_subtree_drawing() {
        let mut list = DisplayList::default();
        list.push(PaintItem::PushClip { rect: rect(0.0, 0.0, 2.0, 1.0) });
        list.push(PaintItem::PushFilter {
            filter: crate::parse_filter(Some("brightness(0.5)")).unwrap(),
        });
        fill(&mut list, rect(0.0, 0.0, 1.0, 1.0), RED);
        list.push(PaintItem::PopFilter);
        list.push(PaintItem::FillRect { rect: rect(1.0, 0.0, 1.0, 1.0), color: RED });
        list.push(PaintItem::PopClip);
        let mut framebuffer = Framebuffer::new(2, 1).expect("framebuffer");

        Rasterizer.rasterize(&list, &mut framebuffer);

        assert_eq!(&framebuffer.pixels()[0..4], &[128, 0, 0, 255]);
        assert_eq!(&framebuffer.pixels()[4..8], &[255, 0, 0, 255]);
    }

    #[test]
    fn opacity_multiplies_subtree_alpha() {
        let mut list = DisplayList::default();
        list.push(PaintItem::PushClip { rect: rect(0.0, 0.0, 2.0, 1.0) });
        list.push(PaintItem::PushOpacity { alpha: 0.5 });
        fill(&mut list, rect(0.0, 0.0, 1.0, 1.0), RED);
        list.push(PaintItem::PopOpacity);
        fill(&mut list, rect(1.0, 0.0, 1.0, 1.0), RED);
        list.push(PaintItem::PopClip);
        let mut framebuffer = Framebuffer::new(2, 1).expect("framebuffer");
        framebuffer.clear([255, 255, 255, 255]);

        Rasterizer.rasterize(&list, &mut framebuffer);

        let faded = &framebuffer.pixels()[0..4];
        let opaque = &framebuffer.pixels()[4..8];
        assert_eq!(opaque, &[255, 0, 0, 255]);
        assert_eq!(faded[0], 255);
        assert!(faded[1] > 0 && faded[1] < 255, "red should be blended: {faded:?}");
    }

    #[test]
    fn rotate_45_produces_a_diamond_not_a_filled_bounding_box() {
        let mut list = DisplayList::default();
        list.push(PaintItem::PushClip { rect: rect(0.0, 0.0, 30.0, 30.0) });
        list.push(PaintItem::PushTransform {
            // The Painter applies transform-origin at the box center; emulate it.
            transform: crate::parse_transform(Some("rotate(45deg)"), 10.0, 10.0, None)
                .unwrap()
                .around_origin(15.0, 15.0),
        });
        fill(&mut list, rect(10.0, 10.0, 10.0, 10.0), RED);
        list.push(PaintItem::PopTransform);
        list.push(PaintItem::PopClip);
        let mut framebuffer = Framebuffer::new(30, 30).expect("framebuffer");

        Rasterizer.rasterize(&list, &mut framebuffer);

        let pixel = |x: usize, y: usize| {
            let offset = (y * 30 + x) * 4;
            framebuffer.pixels()[offset..offset + 4].to_vec()
        };
        // The diamond's center is painted.
        assert_eq!(pixel(15, 15), vec![255, 0, 0, 255]);
        // A corner of the axis-aligned bounding box must stay empty: this
        // distinguishes a true rotated diamond from a filled bounding box.
        assert_eq!(pixel(8, 8), vec![0, 0, 0, 0], "corner must be empty");
    }

    #[test]
    fn translate_transform_moves_drawing_and_descendants() {
        let mut list = DisplayList::default();
        list.push(PaintItem::PushClip { rect: rect(0.0, 0.0, 6.0, 2.0) });
        list.push(PaintItem::PushTransform {
            transform: crate::parse_transform(Some("translate(2px, 0)"), 1.0, 1.0, None).unwrap(),
        });
        fill(&mut list, rect(0.0, 0.0, 2.0, 2.0), RED);
        list.push(PaintItem::PopTransform);
        list.push(PaintItem::PopClip);
        let mut framebuffer = Framebuffer::new(6, 2).expect("framebuffer");

        Rasterizer.rasterize(&list, &mut framebuffer);

        assert_eq!(&framebuffer.pixels()[0..4], &[0, 0, 0, 0]);
        assert_eq!(&framebuffer.pixels()[8..12], &[255, 0, 0, 255]);
    }

    #[test]
    fn round_clip_cuts_the_corners_of_a_fill() {
        let mut list = DisplayList::default();
        list.push(PaintItem::PushRoundClip { rect: rect(0.0, 0.0, 8.0, 8.0), radius: 8.0 });
        fill(&mut list, rect(0.0, 0.0, 8.0, 8.0), RED);
        list.push(PaintItem::PopClip);
        let mut framebuffer = Framebuffer::new(8, 8).expect("framebuffer");

        Rasterizer.rasterize(&list, &mut framebuffer);

        assert_eq!(&framebuffer.pixels()[0..4], &[0, 0, 0, 0]);
        let center = (4 * 8 + 4) * 4;
        assert_eq!(&framebuffer.pixels()[center..center + 4], &[255, 0, 0, 255]);
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
                0, 0, 0, 0, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 255, 0, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0,
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

        assert_eq!(
            framebuffer.pixels(),
            &[0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255]
        );
    }
}
