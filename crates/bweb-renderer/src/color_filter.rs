//! Parsing and application of the color-filter subset of CSS `filter`.
//!
//! `brightness()`, `contrast()`, `grayscale()` and `invert()` are per-channel
//! transforms with no spatial component, so they can be applied directly to
//! each emitted color. `opacity()` reuses the subtree alpha path. `blur()` is
//! not supported.

use crate::Color;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorFilter {
    brightness: f32,
    contrast: f32,
    grayscale: f32,
    invert: f32,
}

impl Default for ColorFilter {
    fn default() -> Self {
        Self { brightness: 1.0, contrast: 1.0, grayscale: 0.0, invert: 0.0 }
    }
}

impl ColorFilter {
    fn is_identity(self) -> bool {
        self == Self::default()
    }

    /// Composes `local` on top of `self`, in the order CSS applies functions.
    #[must_use]
    pub fn compose(self, local: Self) -> Self {
        Self {
            brightness: self.brightness * local.brightness,
            contrast: self.contrast * local.contrast,
            grayscale: 1.0 - (1.0 - self.grayscale) * (1.0 - local.grayscale),
            invert: 1.0 - (1.0 - self.invert) * (1.0 - local.invert),
        }
    }

    /// Applies the filter to a color, preserving alpha.
    #[must_use]
    pub fn apply(self, color: Color) -> Color {
        if self.is_identity() {
            return color;
        }
        let mut channels = [f32::from(color.red), f32::from(color.green), f32::from(color.blue)];
        for channel in &mut channels {
            *channel = (*channel / 255.0 - 0.5) * self.contrast + 0.5;
            *channel *= self.brightness;
            *channel = *channel * (1.0 - self.invert) + (1.0 - *channel) * self.invert;
        }
        if self.grayscale > 0.0 {
            let luma = channels[0] * 0.2126 + channels[1] * 0.7152 + channels[2] * 0.0722;
            for channel in &mut channels {
                *channel = *channel * (1.0 - self.grayscale) + luma * self.grayscale;
            }
        }
        let clamp = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
        Color::rgba(clamp(channels[0]), clamp(channels[1]), clamp(channels[2]), color.alpha)
    }
}

fn parse_number(value: &str) -> Option<f32> {
    value
        .trim()
        .trim_end_matches('%')
        .parse::<f32>()
        .ok()
        .map(|v| if value.trim().ends_with('%') { v / 100.0 } else { v })
}

/// Parses a `filter` value, ignoring functions that are not color filters.
/// Returns `None` when no supported function is present or the value is `none`.
#[must_use]
pub fn parse_filter(value: Option<&str>) -> Option<ColorFilter> {
    let value = value.unwrap_or("").trim();
    if value.is_empty() || value.eq_ignore_ascii_case("none") {
        return None;
    }
    let mut result = ColorFilter::default();
    let bytes = value.as_bytes();
    let mut index = 0;
    let mut found = false;
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
        let amount = parse_number(args).unwrap_or(1.0);
        let local = match name.as_str() {
            "brightness" => ColorFilter { brightness: amount, ..ColorFilter::default() },
            "contrast" => ColorFilter { contrast: amount, ..ColorFilter::default() },
            "grayscale" => {
                ColorFilter { grayscale: amount.clamp(0.0, 1.0), ..ColorFilter::default() }
            }
            "invert" => ColorFilter { invert: amount.clamp(0.0, 1.0), ..ColorFilter::default() },
            _ => continue,
        };
        found = true;
        result = result.compose(local);
    }
    found.then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brightness_scales_channels() {
        let filter = parse_filter(Some("brightness(0.5)")).unwrap();
        let out = filter.apply(Color::rgba(200, 100, 50, 255));
        assert_eq!((out.red, out.green, out.blue), (100, 50, 25));
        assert_eq!(out.alpha, 255);
    }

    #[test]
    fn grayscale_equalizes_channels() {
        let filter = parse_filter(Some("grayscale(1)")).unwrap();
        let out = filter.apply(Color::rgba(255, 0, 0, 255));
        assert_eq!(out.red, out.green);
        assert_eq!(out.green, out.blue);
    }

    #[test]
    fn ignores_unsupported_functions() {
        assert!(parse_filter(Some("blur(4px)")).is_none());
        assert!(parse_filter(Some("none")).is_none());
    }
}
