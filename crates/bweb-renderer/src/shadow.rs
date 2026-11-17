//! CSS `box-shadow` parsing and shadow-geometry helpers.
//!
//! A shadow is defined by two offsets, a blur radius, a spread radius, a color
//! and an optional `inset` flag. Geometry is expressed in physical pixels here;
//! parsing resolves lengths with a supplied parser callback so this crate stays
//! independent of the CSS engine.

use crate::{Color, Rect};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoxShadow {
    pub offset_x: f32,
    pub offset_y: f32,
    pub blur: f32,
    pub spread: f32,
    pub color: Color,
    pub inset: bool,
    /// Corner radius of the originating box, carried for rounded shadows.
    pub radius: f32,
}

impl BoxShadow {
    /// The rectangle covered by an outer shadow (the box grown by spread and
    /// blur, then offset). Used as the rasterization area.
    #[must_use]
    pub fn outer_area(self, box_rect: Rect) -> Rect {
        let grow = self.spread + self.blur;
        Rect {
            x: box_rect.x + self.offset_x - grow,
            y: box_rect.y + self.offset_y - grow,
            width: box_rect.width + grow * 2.0,
            height: box_rect.height + grow * 2.0,
        }
    }

    /// The shadow-casting shape: the box grown by spread and offset, before
    /// blur. For inset shadows the shape is the box shifted by the offsets and
    /// shrunk by the spread (CSS inset offsets move the inner shadow inward).
    #[must_use]
    pub fn shape(self, box_rect: Rect) -> Rect {
        if self.inset {
            Rect {
                x: box_rect.x + self.offset_x + self.spread,
                y: box_rect.y + self.offset_y + self.spread,
                width: (box_rect.width - self.offset_x.abs() - self.spread * 2.0).max(0.0),
                height: (box_rect.height - self.offset_y.abs() - self.spread * 2.0).max(0.0),
            }
        } else {
            Rect {
                x: box_rect.x + self.offset_x - self.spread,
                y: box_rect.y + self.offset_y - self.spread,
                width: box_rect.width + self.spread * 2.0,
                height: box_rect.height + self.spread * 2.0,
            }
        }
    }
}

/// Signed distance from a point to a rounded rectangle (negative inside,
/// positive outside). Used for anti-aliased shadow coverage.
#[must_use]
pub fn rounded_rect_distance(rect: Rect, radius: f32, px: f32, py: f32) -> f32 {
    let r = radius.min(rect.width * 0.5).min(rect.height * 0.5).max(0.0);
    let cx = rect.x + rect.width * 0.5;
    let cy = rect.y + rect.height * 0.5;
    let half_w = rect.width * 0.5 - r;
    let half_h = rect.height * 0.5 - r;
    let dx = (px - cx).abs() - half_w;
    let dy = (py - cy).abs() - half_h;
    let outside = dx.max(0.0).hypot(dy.max(0.0));
    let inside = dx.max(dy).min(0.0);
    outside + inside - r
}

/// Cover from a signed distance and blur radius (approximated smoothstep).
#[must_use]
pub fn shadow_coverage(distance: f32, blur: f32) -> f32 {
    if blur <= 0.0 {
        return if distance <= 0.0 { 1.0 } else { 0.0 };
    }
    // Three-sigma Gaussian falloff: at `blur` pixels the coverage is ~0.5.
    let t = 0.5 - distance / (blur * 2.0);
    t.clamp(0.0, 1.0)
}

/// Parses one shadow layer. Lengths are resolved by `parse_len` (which knows
/// the CSS units and basis); colors by `parse_color`. Returns `None` on
/// malformed input so the whole declaration is ignored.
#[must_use]
pub fn parse_box_shadow(
    value: &str,
    radius: f32,
    parse_len: impl Fn(&str) -> Option<f32>,
) -> Option<BoxShadow> {
    let mut offset_x = 0.0;
    let mut offset_y = 0.0;
    let mut blur = 0.0;
    let mut spread = 0.0;
    let mut color = Color::rgba(0, 0, 0, 255);
    let mut inset = false;
    let mut lengths_seen = 0;

    for token in split_tokens(value) {
        let lower = token.to_ascii_lowercase();
        if lower == "inset" {
            inset = true;
        } else if let Some(parsed) = crate::parse_color(token) {
            color = parsed;
        } else {
            let length = parse_len(token)?;
            match lengths_seen {
                0 => offset_x = length,
                1 => offset_y = length,
                2 => blur = length.max(0.0),
                3 => spread = length,
                _ => return None,
            }
            lengths_seen += 1;
        }
    }
    if lengths_seen < 2 {
        return None;
    }
    Some(BoxShadow { offset_x, offset_y, blur, spread, color, inset, radius })
}

/// A parsed CSS `text-shadow` layer (no inset/spread).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextShadow {
    pub offset_x: f32,
    pub offset_y: f32,
    pub blur: f32,
    pub color: Color,
}

/// Parses one `text-shadow` layer. Same grammar as box-shadow minus `inset`
/// and spread. Multi-layer splitting is done by the caller.
#[must_use]
pub fn parse_text_shadow(
    value: &str,
    parse_len: impl Fn(&str) -> Option<f32>,
) -> Option<TextShadow> {
    let mut offset_x = 0.0;
    let mut offset_y = 0.0;
    let mut blur = 0.0;
    let mut color = Color::rgba(0, 0, 0, 255);
    let mut lengths_seen = 0;
    for token in split_tokens(value) {
        if let Some(parsed) = crate::parse_color(token) {
            color = parsed;
        } else {
            let length = parse_len(token)?;
            match lengths_seen {
                0 => offset_x = length,
                1 => offset_y = length,
                2 => blur = length.max(0.0),
                _ => return None,
            }
            lengths_seen += 1;
        }
    }
    if lengths_seen < 2 {
        return None;
    }
    Some(TextShadow { offset_x, offset_y, blur, color })
}

/// Splits a comma-separated shadow value into layers, respecting parentheses.
#[must_use]
pub fn split_shadow_layers(value: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (index, byte) in value.as_bytes().iter().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                result.push(&value[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    result.push(&value[start..]);
    result
}

/// Splits a shadow value into tokens, keeping `rgb(...)`/`hsl(...)` intact.
fn split_tokens(value: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut depth = 0i32;
    for (index, character) in value.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => depth -= 1,
            c if c.is_ascii_whitespace() && depth == 0 => {
                if start < index {
                    result.push(&value[start..index]);
                }
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    if start < value.len() {
        result.push(&value[start..]);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn len(value: &str) -> Option<f32> {
        value.trim().trim_end_matches("px").trim().parse().ok()
    }

    #[test]
    fn parses_basic_shadow() {
        let shadow = parse_box_shadow("2px 4px 6px rgba(0,0,0,0.5)", 0.0, len).unwrap();
        assert_eq!(shadow.offset_x, 2.0);
        assert_eq!(shadow.offset_y, 4.0);
        assert_eq!(shadow.blur, 6.0);
        assert_eq!(shadow.spread, 0.0);
        assert_eq!(shadow.color.alpha, 128);
    }

    #[test]
    fn parses_inset_with_spread() {
        let shadow = parse_box_shadow("inset 0 1px 3px 2px #123456", 4.0, len).unwrap();
        assert!(shadow.inset);
        assert_eq!(shadow.spread, 2.0);
        assert_eq!(shadow.radius, 4.0);
    }

    #[test]
    fn rejects_single_length() {
        assert!(parse_box_shadow("2px", 0.0, len).is_none());
    }

    #[test]
    fn inset_shape_uses_offsets() {
        let rect = crate::Rect { x: 0.0, y: 0.0, width: 40.0, height: 40.0 };
        let shadow = BoxShadow {
            offset_x: 2.0,
            offset_y: 3.0,
            blur: 0.0,
            spread: 0.0,
            color: Color::rgba(0, 0, 0, 255),
            inset: true,
            radius: 0.0,
        };
        let shape = shadow.shape(rect);
        assert_eq!(shape.x, 2.0);
        assert_eq!(shape.y, 3.0);
        assert_eq!(shape.width, 38.0);
        assert_eq!(shape.height, 37.0);
    }

    #[test]
    fn parses_text_shadow() {
        let shadow = parse_text_shadow("1px 2px 3px rgba(0,0,0,128)", len).unwrap();
        assert_eq!(shadow.offset_x, 1.0);
        assert_eq!(shadow.offset_y, 2.0);
        assert_eq!(shadow.blur, 3.0);
        assert_eq!(shadow.color.alpha, 128);
        assert!(parse_text_shadow("inset 1px 1px", len).is_none());
        let layers = split_shadow_layers("1px 1px red, 2px 2px blue");
        assert_eq!(layers.len(), 2);
    }

    #[test]
    fn blur_produces_a_smooth_falloff() {
        let near = shadow_coverage(1.0, 4.0);
        let edge = shadow_coverage(0.0, 4.0);
        let far = shadow_coverage(8.0, 4.0);
        assert!(edge > near, "at the edge = more coverage than 1px outside");
        assert!(near > far && far >= 0.0, "farther = less, clamped at 0");
        assert!(far < 0.01);
    }

    #[test]
    fn signed_distance() {
        let rect = Rect { x: 0.0, y: 0.0, width: 20.0, height: 20.0 };
        assert!(rounded_rect_distance(rect, 0.0, 10.0, 10.0) < 0.0);
        assert!(rounded_rect_distance(rect, 0.0, 25.0, 10.0) > 0.0);
    }
}
