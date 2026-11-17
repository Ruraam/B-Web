//! Linear-gradient primitives and CSS value parsing.
//!
//! Gradients are stored as an angle in the CSS convention (0deg points up,
//! angles increase clockwise) and a list of color stops. Stop positions are
//! deliberately kept as percentages or absolute lengths until rasterization,
//! where the gradient line length is known.

use crate::Color;
use crate::color::parse_color;

/// Position of a gradient stop along the gradient line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GradientStopPosition {
    /// A fraction of the gradient line, from 0.0 to 1.0.
    Fraction(f32),
    /// An absolute length in pixels, resolved against the gradient line.
    Length(f32),
}

/// One color stop in a gradient.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorStop {
    pub color: Color,
    /// `None` means the stop is spaced evenly between its neighbors.
    pub position: Option<GradientStopPosition>,
}

/// A CSS `linear-gradient()` in resolved form.
#[derive(Debug, Clone, PartialEq)]
pub struct LinearGradient {
    /// Angle in degrees; 0deg points up, 90deg points right.
    pub angle: f32,
    pub stops: Vec<ColorStop>,
}

/// A paintable CSS `background-image` value.
#[derive(Debug, Clone, PartialEq)]
pub enum BackgroundImage {
    Linear(LinearGradient),
    /// An `url(...)` reference. Image backgrounds are fetched by the document
    /// loader but are not yet painted; keeping the value documents the subset.
    Url(String),
}

/// Parses the first layer of a `background-image` value.
#[must_use]
pub fn parse_background_image(value: &str) -> Option<BackgroundImage> {
    let first = split_top_level(value, ',').into_iter().next()?.trim();
    if first.is_empty() || first.eq_ignore_ascii_case("none") {
        return None;
    }
    if first
        .get(.."linear-gradient(".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("linear-gradient("))
    {
        return parse_linear_gradient(first).map(BackgroundImage::Linear);
    }
    if let Some(inner) = first.get("url(".len()..).filter(|_| {
        first.get(.."url(".len()).is_some_and(|prefix| prefix.eq_ignore_ascii_case("url("))
    }) {
        return inner
            .strip_suffix(')')
            .map(|url| BackgroundImage::Url(url.trim().trim_matches(['\'', '"']).to_owned()));
    }
    None
}

/// Parses `linear-gradient([<angle> | to <side>], <stop>#)`.
#[must_use]
pub fn parse_linear_gradient(value: &str) -> Option<LinearGradient> {
    let value = value.trim();
    let prefix = value.get(.."linear-gradient(".len())?;
    if !prefix.eq_ignore_ascii_case("linear-gradient(") {
        return None;
    }
    let inner = value.get("linear-gradient(".len()..)?.strip_suffix(')')?;

    let parts: Vec<&str> = split_top_level(inner, ',').into_iter().map(str::trim).collect();
    if parts.is_empty() || parts.iter().any(|part| part.is_empty()) {
        return None;
    }

    let mut stop_start = 0;
    let mut angle = 180.0;
    if let Some(parsed) = parse_direction(parts[0]) {
        angle = parsed;
        stop_start = 1;
    }

    let mut stops = Vec::new();
    for part in &parts[stop_start..] {
        stops.push(parse_stop(part)?);
    }
    if stops.is_empty() {
        return None;
    }

    Some(LinearGradient { angle, stops })
}

fn parse_stop(source: &str) -> Option<ColorStop> {
    let (color, position) = split_last_token(source);
    let color = parse_color(color)?;
    let position = match position {
        None => None,
        Some(token) => {
            if let Some(percent) = token.strip_suffix('%') {
                let fraction = percent.trim().parse::<f32>().ok()? / 100.0;
                Some(GradientStopPosition::Fraction(fraction.clamp(0.0, 1.0)))
            } else if let Some(length) = token.strip_suffix("px") {
                let length = length.trim().parse::<f32>().ok()?;
                if !length.is_finite() || length < 0.0 {
                    return None;
                }
                Some(GradientStopPosition::Length(length))
            } else {
                let length = token.strip_suffix("em")?;
                let length = length.trim().parse::<f32>().ok()? * 16.0;
                if !length.is_finite() || length < 0.0 {
                    return None;
                }
                Some(GradientStopPosition::Length(length))
            }
        }
    };
    Some(ColorStop { color, position })
}

/// Splits `"red 20%"` into `("red", Some("20%"))`, respecting parentheses in
/// color functions such as `rgba(0, 0, 0, .5) 50%`.
fn split_last_token(source: &str) -> (&str, Option<&str>) {
    let mut paren = 0usize;
    let mut quote = None;
    let mut split = None;
    for (index, character) in source.char_indices() {
        if let Some(current) = quote {
            if character == current {
                quote = None;
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '(' => paren += 1,
            ')' => paren = paren.saturating_sub(1),
            character if character.is_ascii_whitespace() && paren == 0 => split = Some(index),
            _ => {}
        }
    }
    match split {
        Some(index) => {
            let head = source[..index].trim();
            let tail = source[index..].trim();
            if tail.is_empty() { (head, None) } else { (head, Some(tail)) }
        }
        None => (source.trim(), None),
    }
}

fn parse_direction(source: &str) -> Option<f32> {
    let source = source.trim().to_ascii_lowercase();
    if let Some(degrees) = source.strip_suffix("deg") {
        let angle = degrees.trim().parse::<f32>().ok()?;
        return Some(angle.rem_euclid(360.0));
    }
    let sides = source.strip_prefix("to ").unwrap_or(&source);
    let angle = match sides {
        "top" => 0.0,
        "top right" | "right top" => 45.0,
        "right" => 90.0,
        "bottom right" | "right bottom" => 135.0,
        "bottom" => 180.0,
        "bottom left" | "left bottom" => 225.0,
        "left" => 270.0,
        "top left" | "left top" => 315.0,
        _ => return None,
    };
    Some(angle)
}

/// Splits on a separator that appears outside parentheses, brackets and
/// quotes. Whitespace inside a color function therefore stays in one piece.
fn split_top_level(source: &str, separator: char) -> Vec<&str> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut paren = 0usize;
    let mut bracket = 0usize;
    let mut quote = None;
    for (index, character) in source.char_indices() {
        if let Some(current) = quote {
            if character == current {
                quote = None;
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '[' => bracket += 1,
            ']' => bracket = bracket.saturating_sub(1),
            '(' => paren += 1,
            ')' => paren = paren.saturating_sub(1),
            value if value == separator && paren == 0 && bracket == 0 => {
                result.push(&source[start..index]);
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    result.push(&source[start..]);
    result
}

#[cfg(test)]
mod tests {
    use super::{
        BackgroundImage, GradientStopPosition, parse_background_image, parse_linear_gradient,
    };
    use crate::Color;

    #[test]
    fn parses_direction_and_color_stops() {
        let gradient = parse_linear_gradient("linear-gradient(to right, red 0%, blue 100%)")
            .expect("gradient");
        assert_eq!(gradient.angle, 90.0);
        assert_eq!(gradient.stops.len(), 2);
        assert_eq!(gradient.stops[0].color, Color::rgba(255, 0, 0, 255));
        assert_eq!(gradient.stops[0].position, Some(GradientStopPosition::Fraction(0.0)));
        assert_eq!(gradient.stops[1].position, Some(GradientStopPosition::Fraction(1.0)));
    }

    #[test]
    fn parses_function_colors_and_implicit_positions() {
        let gradient =
            parse_linear_gradient("linear-gradient(45deg, rgba(0, 0, 0, .5), #fff 10px)")
                .expect("gradient");
        assert_eq!(gradient.angle, 45.0);
        assert_eq!(gradient.stops[0].position, None);
        assert_eq!(gradient.stops[1].position, Some(GradientStopPosition::Length(10.0)));
    }

    #[test]
    fn parses_background_image_layers() {
        assert_eq!(
            parse_background_image("linear-gradient(red, blue)"),
            parse_linear_gradient("linear-gradient(red, blue)").map(BackgroundImage::Linear)
        );
        assert_eq!(
            parse_background_image("url('x.png'), linear-gradient(red, blue)"),
            Some(BackgroundImage::Url("x.png".to_owned()))
        );
        assert_eq!(parse_background_image("none"), None);
    }

    #[test]
    fn rejects_malformed_gradients() {
        assert!(parse_linear_gradient("linear-gradient()").is_none());
        assert!(parse_linear_gradient("linear-gradient(to sideways, red)").is_none());
        assert!(parse_linear_gradient("linear-gradient(red, not-a-color)").is_none());
    }
}
