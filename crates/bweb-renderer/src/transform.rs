//! CSS `transform` parsing for the software rasterizer.
//!
//! Only translate and scale are supported: they keep rectangles axis-aligned,
//! which the rasterizer requires. Rotations and perspective are not handled.
//! Percentages in `translate()` resolve against the element's own box.

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
}

#[derive(Debug, Clone, Copy)]
enum Component {
    Value(f32, bool), // value, is_percentage
}

fn number(value: &str) -> Option<Component> {
    let value = value.trim();
    if let Some(percent) = value.strip_suffix('%') {
        return percent.trim().parse::<f32>().ok().map(|v| Component::Value(v, true));
    }
    let value = value.strip_suffix("px").unwrap_or(value);
    value.trim().parse::<f32>().ok().map(|v| Component::Value(v, false))
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
pub fn parse_transform(value: Option<&str>, width: f32, height: f32) -> Option<Transform> {
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
            _ => return None, // unsupported function: drop the whole transform
        };
        result = result.compose(local);
    }
    (result != Transform::IDENTITY).then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_resolves_percentages() {
        let transform = parse_transform(Some("translate(50%, 10px)"), 100.0, 40.0).unwrap();
        assert_eq!(transform.point(0.0, 0.0), (50.0, 10.0));
    }

    #[test]
    fn scale_multiplies_coordinates() {
        let transform = parse_transform(Some("scale(2, 0.5)"), 100.0, 40.0).unwrap();
        assert_eq!(transform.point(10.0, 10.0), (20.0, 5.0));
    }

    #[test]
    fn compose_applies_parent_then_child() {
        let parent = parse_transform(Some("translate(10px, 0)"), 100.0, 40.0).unwrap();
        let child = parse_transform(Some("scale(2)"), 100.0, 40.0).unwrap();
        let composed = parent.compose(child);
        assert_eq!(composed.point(5.0, 0.0), (20.0, 0.0));
    }

    #[test]
    fn unsupported_rotation_is_dropped() {
        assert!(parse_transform(Some("rotate(45deg)"), 100.0, 40.0).is_none());
    }
}
