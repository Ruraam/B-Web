//! CSS `transform` parsing for the software rasterizer.
//!
//! Supports translate, scale, rotate, skew and their combinations as 2D affine
//! transforms. The rasterizer maps each destination pixel back through the
//! inverse matrix, so rotated/skewed boxes are rendered correctly. Perspective
//! is not handled. Percentages in `translate()` resolve against the element's
//! own box.

use crate::Rect;

/// A 2D affine transform in the form `[a, b, c, d, e, f]`:
/// `x' = a*x + c*y + e`, `y' = b*x + d*y + f`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

impl Transform {
    pub const IDENTITY: Self = Self { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 };

    /// Composes `self` (parent) with `local` (child): `self ∘ local`.
    #[must_use]
    pub fn compose(self, local: Self) -> Self {
        Self {
            a: self.a * local.a + self.c * local.b,
            b: self.b * local.a + self.d * local.b,
            c: self.a * local.c + self.c * local.d,
            d: self.b * local.c + self.d * local.d,
            e: self.a * local.e + self.c * local.f + self.e,
            f: self.b * local.e + self.d * local.f + self.f,
        }
    }

    /// Applies the transform to a point.
    #[must_use]
    pub fn point(self, x: f32, y: f32) -> (f32, f32) {
        (self.a * x + self.c * y + self.e, self.b * x + self.d * y + self.f)
    }

    /// Applies the transform to an axis-aligned rectangle and returns its
    /// bounding box.
    #[must_use]
    pub fn rect(self, rect: Rect) -> Rect {
        let (x1, y1) = self.point(rect.x, rect.y);
        let (x2, y2) = self.point(rect.x + rect.width, rect.y);
        let (x3, y3) = self.point(rect.x, rect.y + rect.height);
        let (x4, y4) = self.point(rect.x + rect.width, rect.y + rect.height);
        let left = x1.min(x2).min(x3).min(x4);
        let top = y1.min(y2).min(y3).min(y4);
        let right = x1.max(x2).max(x3).max(x4);
        let bottom = y1.max(y2).max(y3).max(y4);
        Rect { x: left, y: top, width: right - left, height: bottom - top }
    }

    /// Applies this transform around a specific origin point, returning the
    /// composed `T(origin) ∘ self ∘ T(-origin)` matrix. CSS transforms use the
    /// element's center as the default origin.
    #[must_use]
    pub fn around_origin(self, origin_x: f32, origin_y: f32) -> Self {
        let to_origin = Self { e: -origin_x, f: -origin_y, ..Self::IDENTITY };
        let from_origin = Self { e: origin_x, f: origin_y, ..Self::IDENTITY };
        from_origin.compose(self).compose(to_origin)
    }

    /// Whether the transform preserves axis-aligned rectangles (no rotation/skew).
    #[must_use]
    pub const fn is_axis_aligned(self) -> bool {
        self.b.abs() < 1e-9 && self.c.abs() < 1e-9
    }

    /// Returns the inverse transform, or `None` when the matrix is singular.
    /// Used by the rasterizer to map destination pixels back to local space.
    #[must_use]
    pub fn inverse(self) -> Option<Self> {
        let determinant = self.a * self.d - self.b * self.c;
        if !determinant.is_finite() || determinant.abs() < 1e-12 {
            return None;
        }
        let inv = 1.0 / determinant;
        let a = self.d * inv;
        let b = -self.b * inv;
        let c = -self.c * inv;
        let d = self.a * inv;
        let e = -(a * self.e + c * self.f);
        let f = -(b * self.e + d * self.f);
        Some(Self { a, b, c, d, e, f })
    }
}

#[derive(Debug, Clone, Copy)]
enum Component {
    Value(f32, bool), // value, is_percentage
}

fn number(value: &str) -> Option<Component> {
    let value = value.trim();
    if let Some(percent) = value.strip_suffix('%') {
        return percent
            .trim()
            .parse::<f32>()
            .ok()
            .filter(|v| v.is_finite())
            .map(|v| Component::Value(v, true));
    }
    let value = value.strip_suffix("px").unwrap_or(value);
    value.trim().parse::<f32>().ok().filter(|v| v.is_finite()).map(|v| Component::Value(v, false))
}

fn resolve(component: Component, basis: f32) -> f32 {
    match component {
        Component::Value(value, true) => basis * value / 100.0,
        Component::Value(value, false) => value,
    }
}

/// Parses a CSS `transform` value. Returns `None` for `none` or unsupported
/// functions (notably rotations and perspective).
#[must_use]
pub fn parse_transform(
    value: Option<&str>,
    width: f32,
    height: f32,
    origin: Option<(f32, f32)>,
) -> Option<Transform> {
    let value = value.unwrap_or("").trim();
    if value.is_empty() || value.eq_ignore_ascii_case("none") {
        return None;
    }
    let mut result = Transform::IDENTITY;
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        while index < bytes.len() && (bytes[index].is_ascii_whitespace() || bytes[index] == b',') {
            index += 1;
        }
        let start = index;
        while index < bytes.len() && bytes[index] != b'(' {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        let name = value[start..index].trim().to_ascii_lowercase();
        index += 1;
        let args_start = index;
        while index < bytes.len() && bytes[index] != b')' {
            index += 1;
        }
        let args = &value[args_start..index.min(bytes.len())];
        index += 1;

        // Angles are parsed separately (the translate/scale number parser does
        // not accept `deg`).
        fn parse_angle(args: &str, index: usize) -> Option<f32> {
            let raw = args.split(',').nth(index)?.trim();
            let number = raw
                .strip_suffix("deg")
                .or_else(|| raw.strip_suffix("rad"))
                .unwrap_or(raw)
                .trim()
                .parse::<f32>()
                .ok()?;
            let degrees = if raw.trim().ends_with("rad") { number.to_degrees() } else { number };
            degrees.is_finite().then_some(degrees)
        }

        let components: Vec<Component> = args.split(',').filter_map(number).collect();
        let local = match name.as_str() {
            "translate" | "translate3d" => {
                let x = components.first().copied()?;
                let y = components.get(1).copied().unwrap_or(Component::Value(0.0, false));
                Transform { e: resolve(x, width), f: resolve(y, height), ..Transform::IDENTITY }
            }
            "translatex" => {
                Transform { e: resolve(components.first().copied()?, width), ..Transform::IDENTITY }
            }
            "translatey" => Transform {
                f: resolve(components.first().copied()?, height),
                ..Transform::IDENTITY
            },
            "scale" | "scale3d" => {
                let sx = components.first().copied()?;
                let sy = components.get(1).copied().unwrap_or(sx);
                Transform { a: resolve(sx, 1.0), d: resolve(sy, 1.0), ..Transform::IDENTITY }
            }
            "scalex" => {
                Transform { a: resolve(components.first().copied()?, 1.0), ..Transform::IDENTITY }
            }
            "scaley" => {
                Transform { d: resolve(components.first().copied()?, 1.0), ..Transform::IDENTITY }
            }
            "rotate" | "rotatez" => {
                let radians = parse_angle(args, 0)?.to_radians();
                let (sine, cosine) = radians.sin_cos();
                Transform { a: cosine, b: sine, c: -sine, d: cosine, ..Transform::IDENTITY }
            }
            "skewx" => {
                let tangent = parse_angle(args, 0)?.to_radians().tan();
                if !tangent.is_finite() {
                    return None;
                }
                Transform { c: tangent, ..Transform::IDENTITY }
            }
            "skewy" => {
                let tangent = parse_angle(args, 0)?.to_radians().tan();
                if !tangent.is_finite() {
                    return None;
                }
                Transform { b: tangent, ..Transform::IDENTITY }
            }
            "skew" => {
                let x = parse_angle(args, 0)?.to_radians().tan();
                let y = parse_angle(args, 1).unwrap_or(0.0).to_radians().tan();
                if !x.is_finite() || !y.is_finite() {
                    return None;
                }
                Transform { b: y, c: x, ..Transform::IDENTITY }
            }
            _ => return None, // unsupported function: drop the whole transform
        };
        result = result.compose(local);
    }
    let result = match origin {
        Some((x, y)) => result.around_origin(x, y),
        None => result,
    };
    (result != Transform::IDENTITY).then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_resolves_percentages() {
        let transform = parse_transform(Some("translate(50%, 10px)"), 100.0, 40.0, None).unwrap();
        assert_eq!(transform.point(0.0, 0.0), (50.0, 10.0));
    }

    #[test]
    fn scale_multiplies_coordinates() {
        let transform = parse_transform(Some("scale(2, 0.5)"), 100.0, 40.0, None).unwrap();
        assert_eq!(transform.point(10.0, 10.0), (20.0, 5.0));
    }

    #[test]
    fn compose_applies_parent_then_child() {
        let parent = parse_transform(Some("translate(10px, 0)"), 100.0, 40.0, None).unwrap();
        let child = parse_transform(Some("scale(2)"), 100.0, 40.0, None).unwrap();
        let composed = parent.compose(child);
        assert_eq!(composed.point(5.0, 0.0), (20.0, 0.0));
    }

    #[test]
    fn rotate_and_skew_are_supported() {
        let rotated = parse_transform(Some("rotate(90deg)"), 100.0, 40.0, None).unwrap();
        let (x, y) = rotated.point(10.0, 0.0);
        assert!((x).abs() < 0.001 && (y - 10.0).abs() < 0.001, "got ({x}, {y})");

        let skewed = parse_transform(Some("skewX(45deg)"), 100.0, 40.0, None).unwrap();
        let (x, y) = skewed.point(0.0, 10.0);
        assert!((x - 10.0).abs() < 0.001 && (y - 10.0).abs() < 0.001, "got ({x}, {y})");
    }

    #[test]
    fn inverse_round_trips() {
        let transform =
            parse_transform(Some("translate(10px, 5px) rotate(30deg) scale(2)"), 100.0, 40.0, None)
                .unwrap();
        let inverse = transform.inverse().unwrap();
        let (x, y) = transform.point(7.0, 3.0);
        let (rx, ry) = inverse.point(x, y);
        assert!((rx - 7.0).abs() < 0.001 && (ry - 3.0).abs() < 0.001);
    }

    #[test]
    fn singular_transform_has_no_inverse() {
        let zero = parse_transform(Some("scale(0)"), 100.0, 40.0, None).unwrap();
        assert!(zero.inverse().is_none());
    }

    #[test]
    fn unsupported_function_is_dropped() {
        assert!(parse_transform(Some("perspective(100px)"), 100.0, 40.0, None).is_none());
    }
}
