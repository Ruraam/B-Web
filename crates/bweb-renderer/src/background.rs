//! Resolution of CSS background size, position and repeat into paint rectangles.
//!
//! The renderer has no CSS dependency: this module parses the small numeric
//! subset used by those properties and turns it into destination rectangles
//! for an image tile.

use crate::Rect;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Length {
    Px(f32),
    Percent(f32),
    Auto,
}

impl Length {
    fn resolve(self, basis: f32) -> Option<f32> {
        match self {
            Self::Px(value) => Some(value),
            Self::Percent(value) => Some(basis * value / 100.0),
            Self::Auto => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Offset {
    Keyword(f32),
    Length(Length),
}

impl Offset {
    fn resolve(self, basis: f32) -> f32 {
        match self {
            Self::Keyword(fraction) => basis * fraction,
            Self::Length(Length::Percent(percent)) => basis * percent / 100.0,
            Self::Length(Length::Px(value)) => value,
            Self::Length(Length::Auto) => 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BackgroundSize {
    Explicit { width: Length, height: Length },
    Cover,
    Contain,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BackgroundPosition {
    x: Offset,
    y: Offset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundRepeat {
    Repeat,
    NoRepeat,
    RepeatX,
    RepeatY,
}

fn parse_length(value: &str) -> Option<Length> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("auto") {
        return Some(Length::Auto);
    }
    if let Some(percent) = value.strip_suffix('%') {
        return percent.trim().parse::<f32>().ok().filter(|v| v.is_finite()).map(Length::Percent);
    }
    if let Some(px) = value.strip_suffix("px") {
        return px.trim().parse::<f32>().ok().filter(|v| v.is_finite()).map(Length::Px);
    }
    value.parse::<f32>().ok().filter(|v| v.is_finite()).map(Length::Px)
}

fn first_layer(value: Option<&str>) -> &str {
    value.unwrap_or("").split(',').next().unwrap_or("").trim()
}

/// Parses `background-size`, defaulting to `auto`.
#[must_use]
pub fn parse_background_size(value: Option<&str>) -> BackgroundSize {
    let first = first_layer(value).to_ascii_lowercase();
    if first == "cover" {
        return BackgroundSize::Cover;
    }
    if first == "contain" {
        return BackgroundSize::Contain;
    }
    let mut parts = first_layer(value).split_ascii_whitespace();
    let width = parts.next().and_then(parse_length).unwrap_or(Length::Auto);
    let height = parts.next().and_then(parse_length).unwrap_or(Length::Auto);
    BackgroundSize::Explicit { width, height }
}

fn parse_offset(value: &str) -> Option<Offset> {
    match value.to_ascii_lowercase().as_str() {
        "left" | "top" => Some(Offset::Keyword(0.0)),
        "center" => Some(Offset::Keyword(0.5)),
        "right" | "bottom" => Some(Offset::Keyword(1.0)),
        other => parse_length(other).map(Offset::Length),
    }
}

/// Parses `background-position`, defaulting to `0% 0%`.
#[must_use]
pub fn parse_background_position(value: Option<&str>) -> BackgroundPosition {
    let mut parts = first_layer(value).split_ascii_whitespace();
    let x = parts.next().and_then(parse_offset).unwrap_or(Offset::Length(Length::Percent(0.0)));
    let y = parts.next().and_then(parse_offset).unwrap_or(Offset::Keyword(0.5));
    BackgroundPosition { x, y }
}

/// Parses `background-repeat`, defaulting to `repeat`.
#[must_use]
pub fn parse_background_repeat(value: Option<&str>) -> BackgroundRepeat {
    match first_layer(value).to_ascii_lowercase().as_str() {
        "no-repeat" => BackgroundRepeat::NoRepeat,
        "repeat-x" => BackgroundRepeat::RepeatX,
        "repeat-y" => BackgroundRepeat::RepeatY,
        _ => BackgroundRepeat::Repeat,
    }
}

/// Computes the destination rectangles for one background layer inside
/// `container`. The intrinsic size comes from the decoded bitmap.
#[must_use]
pub fn background_rects(
    container: Rect,
    intrinsic: (f32, f32),
    size: &BackgroundSize,
    position: &BackgroundPosition,
    repeat: BackgroundRepeat,
) -> Vec<Rect> {
    if container.width <= 0.0 || container.height <= 0.0 || intrinsic.0 <= 0.0 || intrinsic.1 <= 0.0
    {
        return Vec::new();
    }
    let (intrinsic_width, intrinsic_height) = intrinsic;
    let (mut tile_width, mut tile_height) = match *size {
        BackgroundSize::Cover => cover_contain_size(container, intrinsic, false),
        BackgroundSize::Contain => cover_contain_size(container, intrinsic, true),
        BackgroundSize::Explicit { width, height } => {
            let mut resolved_width = width.resolve(container.width);
            let mut resolved_height = height.resolve(container.height);
            match (resolved_width, resolved_height) {
                (None, None) => {
                    resolved_width = Some(intrinsic_width);
                    resolved_height = Some(intrinsic_height);
                }
                (None, Some(value)) => {
                    resolved_width = Some(intrinsic_width * value / intrinsic_height);
                }
                (Some(value), None) => {
                    resolved_height = Some(intrinsic_height * value / intrinsic_width);
                }
                (Some(_), Some(_)) => {}
            }
            (resolved_width.unwrap_or(0.0), resolved_height.unwrap_or(0.0))
        }
    };
    tile_width = tile_width.max(0.0);
    tile_height = tile_height.max(0.0);
    if tile_width <= 0.0 || tile_height <= 0.0 {
        return Vec::new();
    }

    let free_x = (container.width - tile_width).max(0.0);
    let free_y = (container.height - tile_height).max(0.0);
    let origin_x = container.x + position.x.resolve(free_x);
    let origin_y = container.y + position.y.resolve(free_y);

    let first = Rect { x: origin_x, y: origin_y, width: tile_width, height: tile_height };
    let repeat_x = matches!(repeat, BackgroundRepeat::Repeat | BackgroundRepeat::RepeatX);
    let repeat_y = matches!(repeat, BackgroundRepeat::Repeat | BackgroundRepeat::RepeatY);
    if !repeat_x && !repeat_y {
        return vec![first];
    }

    // Anchor the tiling grid to the container edges so negative offsets from
    // background-position still cover the whole box.
    let start_x = if repeat_x {
        origin_x - ((origin_x - container.x).div_euclid(tile_width)) * tile_width
    } else {
        origin_x
    };
    let start_y = if repeat_y {
        origin_y - ((origin_y - container.y).div_euclid(tile_height)) * tile_height
    } else {
        origin_y
    };
    let columns = if repeat_x {
        (((container.x + container.width - start_x) / tile_width).ceil().max(0.0) as usize)
            .clamp(1, 512)
    } else {
        1
    };
    let rows = if repeat_y {
        (((container.y + container.height - start_y) / tile_height).ceil().max(0.0) as usize)
            .clamp(1, 512)
    } else {
        1
    };
    let mut result = Vec::with_capacity(columns.saturating_mul(rows).min(4096));
    for row in 0..rows {
        for column in 0..columns {
            result.push(Rect {
                x: start_x + column as f32 * tile_width,
                y: start_y + row as f32 * tile_height,
                width: tile_width,
                height: tile_height,
            });
        }
    }
    result
}

/// Resolves `cover`/`contain` keywords by scaling the intrinsic size to the
/// container, preserving aspect ratio.
#[must_use]
pub fn cover_contain_size(container: Rect, intrinsic: (f32, f32), contain: bool) -> (f32, f32) {
    if intrinsic.0 <= 0.0 || intrinsic.1 <= 0.0 {
        return intrinsic;
    }
    let scale_x = container.width / intrinsic.0;
    let scale_y = container.height / intrinsic.1;
    let scale = if contain { scale_x.min(scale_y) } else { scale_x.max(scale_y) };
    (intrinsic.0 * scale, intrinsic.1 * scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> Rect {
        Rect { x: 0.0, y: 0.0, width: 100.0, height: 50.0 }
    }

    #[test]
    fn no_repeat_centers_the_tile() {
        let size = parse_background_size(Some("auto"));
        let position = parse_background_position(Some("center"));
        let rects =
            background_rects(rect(), (20.0, 10.0), &size, &position, BackgroundRepeat::NoRepeat);
        assert_eq!(rects.len(), 1);
        assert_eq!((rects[0].x, rects[0].y), (40.0, 20.0));
    }

    #[test]
    fn explicit_size_scales_and_repeats() {
        let size = parse_background_size(Some("40px 20px"));
        let position = parse_background_position(Some("0 0"));
        let rects =
            background_rects(rect(), (10.0, 10.0), &size, &position, BackgroundRepeat::Repeat);
        assert!(rects.len() > 2);
        assert_eq!(rects[0].width, 40.0);
        assert_eq!(rects[0].height, 20.0);
        assert_eq!(rects[1].x, 40.0);
    }

    #[test]
    fn non_finite_lengths_are_rejected() {
        let size = parse_background_size(Some("1e999px"));
        assert_eq!(size, BackgroundSize::Explicit { width: Length::Auto, height: Length::Auto });
        let rects = background_rects(
            rect(),
            (10.0, 10.0),
            &size,
            &parse_background_position(None),
            BackgroundRepeat::Repeat,
        );
        assert!(!rects.is_empty() && rects.len() <= 512, "finite, bounded tiling");
        assert!(rects.iter().all(|r| r.width.is_finite() && r.height.is_finite()));
    }

    #[test]
    fn cover_and_contain_preserve_aspect_ratio() {
        let cover = background_rects(
            rect(),
            (20.0, 20.0),
            &BackgroundSize::Cover,
            &BackgroundPosition { x: Offset::Keyword(0.0), y: Offset::Keyword(0.0) },
            BackgroundRepeat::NoRepeat,
        );
        assert_eq!((cover[0].width, cover[0].height), (100.0, 100.0));
        let contain = background_rects(
            rect(),
            (20.0, 20.0),
            &BackgroundSize::Contain,
            &BackgroundPosition { x: Offset::Keyword(0.0), y: Offset::Keyword(0.0) },
            BackgroundRepeat::NoRepeat,
        );
        assert_eq!((contain[0].width, contain[0].height), (50.0, 50.0));
    }
}
