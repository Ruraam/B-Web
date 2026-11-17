//! CSS color parsing for the software renderer.
//!
//! This is the single place where textual CSS color values become RGBA8
//! colors. It deliberately covers the color syntax that B-Web paints today:
//! named colors, hexadecimal colors, `rgb()`/`rgba()` and `hsl()`/`hsla()`.
//! `transparent` resolves to `None`, meaning "paint nothing" for backgrounds.

use crate::Color;

/// Parses a CSS color value into a straight-alpha RGBA8 color.
///
/// Returns `None` for `transparent`, malformed values and colors outside the
/// supported subset. Channel values are clamped to their valid range.
#[must_use]
pub fn parse_color(value: &str) -> Option<Color> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let lowercase = value.to_ascii_lowercase();

    if lowercase == "transparent" {
        return None;
    }

    if let Some((red, green, blue)) = named_color(&lowercase) {
        return Some(Color::rgba(red, green, blue, 255));
    }

    if let Some(hex) = lowercase.strip_prefix('#') {
        let expand = |digit: u8| digit * 17;
        let channels = match hex.len() {
            3 => {
                let values = hex_digits(hex)?;
                [expand(values[0]), expand(values[1]), expand(values[2]), 255]
            }
            4 => {
                let values = hex_digits(hex)?;
                [expand(values[0]), expand(values[1]), expand(values[2]), expand(values[3])]
            }
            6 | 8 => {
                let mut channels = [0_u8; 4];
                let count = if hex.len() == 8 { 4 } else { 3 };
                let bytes = hex.as_bytes();
                for (index, channel) in channels.iter_mut().take(count).enumerate() {
                    let high = hex_digit(bytes[index * 2])?;
                    let low = hex_digit(bytes[index * 2 + 1])?;
                    *channel = high * 16 + low;
                }
                if hex.len() == 6 {
                    channels[3] = 255;
                }
                channels
            }
            _ => return None,
        };
        return Some(Color::rgba(channels[0], channels[1], channels[2], channels[3]));
    }

    if let Some(inner) = function_value(&lowercase, "rgb") {
        return parse_rgb(inner);
    }
    if let Some(inner) = function_value(&lowercase, "rgba") {
        return parse_rgb(inner);
    }
    if let Some(inner) = function_value(&lowercase, "hsl") {
        return parse_hsl(inner);
    }
    if let Some(inner) = function_value(&lowercase, "hsla") {
        return parse_hsl(inner);
    }

    None
}

/// Returns the inner text of `name(...)` when `value` is exactly that function.
fn function_value<'a>(value: &'a str, name: &str) -> Option<&'a str> {
    let rest = value.strip_prefix(name)?;
    let rest = rest.strip_prefix(' ').unwrap_or(rest);
    let inner = rest.strip_prefix('(')?;
    inner.strip_suffix(')')
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn hex_digits(hex: &str) -> Option<Vec<u8>> {
    hex.bytes().map(hex_digit).collect()
}

/// Splits function arguments on commas, falling back to spaces for the modern
/// syntax. A trailing `/ alpha` is returned as a separate component.
fn function_args(inner: &str) -> Vec<&str> {
    let trimmed = inner.trim();
    let comma_parts: Vec<&str> = trimmed.split(',').map(str::trim).collect();
    let parts: Vec<&str> = if comma_parts.len() >= 3 {
        comma_parts
    } else {
        trimmed.split_ascii_whitespace().collect()
    };
    let mut result = Vec::new();
    let mut index = 0;
    while index < parts.len() {
        let part = parts[index].trim();
        if part.is_empty() {
            index += 1;
            continue;
        }
        if part == "/" {
            if let Some(alpha) =
                parts[index + 1..].iter().map(|part| part.trim()).find(|part| !part.is_empty())
            {
                result.push(alpha);
            }
            break;
        }
        if let Some((value, alpha)) = part.split_once('/') {
            if !value.trim().is_empty() {
                result.push(value.trim());
            }
            if !alpha.trim().is_empty() {
                result.push(alpha.trim());
            }
        } else {
            result.push(part);
        }
        index += 1;
    }
    result
}

fn parse_channel(value: &str) -> Option<u8> {
    if let Some(percent) = value.strip_suffix('%') {
        let fraction = percent.trim().parse::<f32>().ok()? / 100.0;
        return Some(clamp_channel(fraction * 255.0));
    }
    Some(clamp_channel(value.trim().parse::<f32>().ok()?))
}

fn parse_alpha(value: Option<&&str>) -> u8 {
    let Some(value) = value else {
        return 255;
    };
    if let Some(percent) = value.strip_suffix('%') {
        let fraction = percent.trim().parse::<f32>().unwrap_or(100.0) / 100.0;
        return clamp_channel(fraction * 255.0);
    }
    let number = value.trim().parse::<f32>().unwrap_or(1.0);
    let fraction = if number > 1.0 { number / 255.0 } else { number };
    clamp_channel(fraction * 255.0)
}

fn parse_rgb(inner: &str) -> Option<Color> {
    let args = function_args(inner);
    if args.len() < 3 {
        return None;
    }
    let red = parse_channel(args[0])?;
    let green = parse_channel(args[1])?;
    let blue = parse_channel(args[2])?;
    let alpha = parse_alpha(args.get(3));
    Some(Color::rgba(red, green, blue, alpha))
}

fn parse_hsl(inner: &str) -> Option<Color> {
    let args = function_args(inner);
    if args.len() < 3 {
        return None;
    }
    let hue = args[0].trim().trim_end_matches("deg").parse::<f32>().ok()?;
    let saturation = args[1].trim().trim_end_matches('%').parse::<f32>().ok()? / 100.0;
    let lightness = args[2].trim().trim_end_matches('%').parse::<f32>().ok()? / 100.0;
    let alpha = parse_alpha(args.get(3));
    let (red, green, blue) = hsl_to_rgb(hue, saturation, lightness);
    Some(Color::rgba(red, green, blue, alpha))
}

fn hsl_to_rgb(hue: f32, saturation: f32, lightness: f32) -> (u8, u8, u8) {
    let saturation = saturation.clamp(0.0, 1.0);
    let lightness = lightness.clamp(0.0, 1.0);
    let hue = hue.rem_euclid(360.0) / 60.0;
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let second = chroma * (1.0 - (hue % 2.0 - 1.0).abs());
    let match_value = lightness - chroma / 2.0;
    let (red, green, blue) = match hue as u8 {
        0 => (chroma, second, 0.0),
        1 => (second, chroma, 0.0),
        2 => (0.0, chroma, second),
        3 => (0.0, second, chroma),
        4 => (second, 0.0, chroma),
        _ => (chroma, 0.0, second),
    };
    (
        clamp_channel((red + match_value) * 255.0),
        clamp_channel((green + match_value) * 255.0),
        clamp_channel((blue + match_value) * 255.0),
    )
}

fn clamp_channel(value: f32) -> u8 {
    if !value.is_finite() {
        return 0;
    }
    value.clamp(0.0, 255.0).round() as u8
}

fn named_color(name: &str) -> Option<(u8, u8, u8)> {
    const NAMES: &[(&str, u8, u8, u8)] = &[
        ("aliceblue", 240, 248, 255),
        ("antiquewhite", 250, 235, 215),
        ("aqua", 0, 255, 255),
        ("aquamarine", 127, 255, 212),
        ("azure", 240, 255, 255),
        ("beige", 245, 245, 220),
        ("bisque", 255, 228, 196),
        ("black", 0, 0, 0),
        ("blanchedalmond", 255, 235, 205),
        ("blue", 0, 0, 255),
        ("blueviolet", 138, 43, 226),
        ("brown", 165, 42, 42),
        ("burlywood", 222, 184, 135),
        ("cadetblue", 95, 158, 160),
        ("chartreuse", 127, 255, 0),
        ("chocolate", 210, 105, 30),
        ("coral", 255, 127, 80),
        ("cornflowerblue", 100, 149, 237),
        ("cornsilk", 255, 248, 220),
        ("crimson", 220, 20, 60),
        ("cyan", 0, 255, 255),
        ("darkblue", 0, 0, 139),
        ("darkcyan", 0, 139, 139),
        ("darkgoldenrod", 184, 134, 11),
        ("darkgray", 169, 169, 169),
        ("darkgreen", 0, 100, 0),
        ("darkgrey", 169, 169, 169),
        ("darkkhaki", 189, 183, 107),
        ("darkmagenta", 139, 0, 139),
        ("darkolivegreen", 85, 107, 47),
        ("darkorange", 255, 140, 0),
        ("darkorchid", 153, 50, 204),
        ("darkred", 139, 0, 0),
        ("darksalmon", 233, 150, 122),
        ("darkseagreen", 143, 188, 143),
        ("darkslateblue", 72, 61, 139),
        ("darkslategray", 47, 79, 79),
        ("darkslategrey", 47, 79, 79),
        ("darkturquoise", 0, 206, 209),
        ("darkviolet", 148, 0, 211),
        ("deeppink", 255, 20, 147),
        ("deepskyblue", 0, 191, 255),
        ("dimgray", 105, 105, 105),
        ("dimgrey", 105, 105, 105),
        ("dodgerblue", 30, 144, 255),
        ("firebrick", 178, 34, 34),
        ("floralwhite", 255, 250, 240),
        ("forestgreen", 34, 139, 34),
        ("fuchsia", 255, 0, 255),
        ("gainsboro", 220, 220, 220),
        ("ghostwhite", 248, 248, 255),
        ("gold", 255, 215, 0),
        ("goldenrod", 218, 165, 32),
        ("gray", 128, 128, 128),
        ("green", 0, 128, 0),
        ("greenyellow", 173, 255, 47),
        ("grey", 128, 128, 128),
        ("honeydew", 240, 255, 240),
        ("hotpink", 255, 105, 180),
        ("indianred", 205, 92, 92),
        ("indigo", 75, 0, 130),
        ("ivory", 255, 255, 240),
        ("khaki", 240, 230, 140),
        ("lavender", 230, 230, 250),
        ("lavenderblush", 255, 240, 245),
        ("lawngreen", 124, 252, 0),
        ("lemonchiffon", 255, 250, 205),
        ("lightblue", 173, 216, 230),
        ("lightcoral", 240, 128, 128),
        ("lightcyan", 224, 255, 255),
        ("lightgoldenrodyellow", 250, 250, 210),
        ("lightgray", 211, 211, 211),
        ("lightgreen", 144, 238, 144),
        ("lightgrey", 211, 211, 211),
        ("lightpink", 255, 182, 193),
        ("lightsalmon", 255, 160, 122),
        ("lightseagreen", 32, 178, 170),
        ("lightskyblue", 135, 206, 250),
        ("lightslategray", 119, 136, 153),
        ("lightslategrey", 119, 136, 153),
        ("lightsteelblue", 176, 196, 222),
        ("lightyellow", 255, 255, 224),
        ("lime", 0, 255, 0),
        ("limegreen", 50, 205, 50),
        ("linen", 250, 240, 230),
        ("magenta", 255, 0, 255),
        ("maroon", 128, 0, 0),
        ("mediumaquamarine", 102, 205, 170),
        ("mediumblue", 0, 0, 205),
        ("mediumorchid", 186, 85, 211),
        ("mediumpurple", 147, 112, 219),
        ("mediumseagreen", 60, 179, 113),
        ("mediumslateblue", 123, 104, 238),
        ("mediumspringgreen", 0, 250, 154),
        ("mediumturquoise", 72, 209, 204),
        ("mediumvioletred", 199, 21, 133),
        ("midnightblue", 25, 25, 112),
        ("mintcream", 245, 255, 250),
        ("mistyrose", 255, 228, 225),
        ("moccasin", 255, 228, 181),
        ("navajowhite", 255, 222, 173),
        ("navy", 0, 0, 128),
        ("oldlace", 253, 245, 230),
        ("olive", 128, 128, 0),
        ("olivedrab", 107, 142, 35),
        ("orange", 255, 165, 0),
        ("orangered", 255, 69, 0),
        ("orchid", 218, 112, 214),
        ("palegoldenrod", 238, 232, 170),
        ("palegreen", 152, 251, 152),
        ("paleturquoise", 175, 238, 238),
        ("palevioletred", 219, 112, 147),
        ("papayawhip", 255, 239, 213),
        ("peachpuff", 255, 218, 185),
        ("peru", 205, 133, 63),
        ("pink", 255, 192, 203),
        ("plum", 221, 160, 221),
        ("powderblue", 176, 224, 230),
        ("purple", 128, 0, 128),
        ("rebeccapurple", 102, 51, 153),
        ("red", 255, 0, 0),
        ("rosybrown", 188, 143, 143),
        ("royalblue", 65, 105, 225),
        ("saddlebrown", 139, 69, 19),
        ("salmon", 250, 128, 114),
        ("sandybrown", 244, 164, 96),
        ("seagreen", 46, 139, 87),
        ("seashell", 255, 245, 238),
        ("sienna", 160, 82, 45),
        ("silver", 192, 192, 192),
        ("skyblue", 135, 206, 235),
        ("slateblue", 106, 90, 205),
        ("slategray", 112, 128, 144),
        ("slategrey", 112, 128, 144),
        ("snow", 255, 250, 250),
        ("springgreen", 0, 255, 127),
        ("steelblue", 70, 130, 180),
        ("tan", 210, 180, 140),
        ("teal", 0, 128, 128),
        ("thistle", 216, 191, 216),
        ("tomato", 255, 99, 71),
        ("turquoise", 64, 224, 208),
        ("violet", 238, 130, 238),
        ("wheat", 245, 222, 179),
        ("white", 255, 255, 255),
        ("whitesmoke", 245, 245, 245),
        ("yellow", 255, 255, 0),
        ("yellowgreen", 154, 205, 50),
    ];
    NAMES.iter().find(|(candidate, _, _, _)| *candidate == name).map(|(_, r, g, b)| (*r, *g, *b))
}

#[cfg(test)]
mod tests {
    use super::parse_color;
    use crate::Color;

    #[test]
    fn parses_hex_and_named_colors() {
        assert_eq!(parse_color("#0f8"), Some(Color::rgba(0, 255, 136, 255)));
        assert_eq!(parse_color("#11223380"), Some(Color::rgba(17, 34, 51, 128)));
        assert_eq!(parse_color("RebeccaPurple"), Some(Color::rgba(102, 51, 153, 255)));
        assert_eq!(parse_color("transparent"), None);
        assert_eq!(parse_color("not-a-color"), None);
    }

    #[test]
    fn parses_rgb_and_rgba_function_syntax() {
        assert_eq!(parse_color("rgb(255, 0, 0)"), Some(Color::rgba(255, 0, 0, 255)));
        assert_eq!(parse_color("rgba(255, 0, 0, 0.5)"), Some(Color::rgba(255, 0, 0, 128)));
        assert_eq!(parse_color("rgb(100% 0% 0% / 25%)"), Some(Color::rgba(255, 0, 0, 64)));
    }

    #[test]
    fn parses_hsl_colors() {
        assert_eq!(parse_color("hsl(0, 100%, 50%)"), Some(Color::rgba(255, 0, 0, 255)));
        assert_eq!(parse_color("hsl(120 100% 50%)"), Some(Color::rgba(0, 255, 0, 255)));
        assert_eq!(parse_color("hsla(240, 100%, 50%, 0.5)"), Some(Color::rgba(0, 0, 255, 128)));
    }
}
