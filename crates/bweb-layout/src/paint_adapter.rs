//! Adapter from the current layout tree and computed CSS values to the renderer port.

use crate::{LayoutBox, LayoutTree, Rect};
use bweb_css::ComputedStyleMap;
use bweb_renderer::{Color, PaintLayout, PaintNode, PaintStyle, Rect as PaintRect};

/// Exposes a layout tree as an immutable paint-order visitor.
pub struct PaintAdapter<'a> {
    pub tree: &'a LayoutTree,
    pub styles: &'a ComputedStyleMap,
    pub viewport: Rect,
}

impl PaintLayout for PaintAdapter<'_> {
    fn viewport(&self) -> PaintRect {
        to_paint_rect(self.viewport)
    }

    fn visit(&self, visitor: &mut dyn FnMut(PaintNode)) {
        fn walk(
            layout: &LayoutBox,
            styles: &ComputedStyleMap,
            depth: u32,
            visitor: &mut dyn FnMut(PaintNode),
        ) {
            let computed = styles.get(layout.node);
            let style = PaintStyle {
                background: computed
                    .and_then(|style| style.get("background-color"))
                    .and_then(parse_color),
                border_color: computed
                    .and_then(|style| style.get("border-color"))
                    .and_then(parse_color),
                border_widths: bweb_renderer::Edges {
                    top: layout.border.top,
                    right: layout.border.right,
                    bottom: layout.border.bottom,
                    left: layout.border.left,
                },
                clips_children: computed
                    .and_then(|style| style.get("overflow"))
                    .is_some_and(|value| value == "hidden" || value == "clip"),
            };

            visitor(PaintNode { depth, rect: to_paint_rect(layout.rect), style });

            for child in &layout.children {
                walk(child, styles, depth.saturating_add(1), visitor);
            }
        }

        walk(&self.tree.root, self.styles, 0, visitor);
    }
}

fn to_paint_rect(rect: Rect) -> PaintRect {
    PaintRect { x: rect.x, y: rect.y, width: rect.width, height: rect.height }
}

/// Parses the small, explicit CSS color subset currently supported by painting.
fn parse_color(value: &str) -> Option<Color> {
    let value = value.trim().to_ascii_lowercase();
    match value.as_str() {
        "transparent" => return None,
        "black" => return Some(Color::rgba(0, 0, 0, 255)),
        "white" => return Some(Color::rgba(255, 255, 255, 255)),
        "red" => return Some(Color::rgba(255, 0, 0, 255)),
        "green" => return Some(Color::rgba(0, 128, 0, 255)),
        "blue" => return Some(Color::rgba(0, 0, 255, 255)),
        "yellow" => return Some(Color::rgba(255, 255, 0, 255)),
        "gray" | "grey" => return Some(Color::rgba(128, 128, 128, 255)),
        _ => {}
    }

    let hex = value.strip_prefix('#')?;
    let expand = |digit: u8| digit * 17;
    let channels = match hex.len() {
        3 => {
            let values: Vec<u8> = hex
                .bytes()
                .map(|byte| (byte as char).to_digit(16).map(|v| v as u8))
                .collect::<Option<_>>()?;
            [expand(values[0]), expand(values[1]), expand(values[2]), 255]
        }
        4 => {
            let values: Vec<u8> = hex
                .bytes()
                .map(|byte| (byte as char).to_digit(16).map(|v| v as u8))
                .collect::<Option<_>>()?;
            [expand(values[0]), expand(values[1]), expand(values[2]), expand(values[3])]
        }
        6 | 8 => {
            let mut values = [0_u8; 4];
            let mut bytes = hex.bytes();
            for channel in values.iter_mut().take(if hex.len() == 8 { 4 } else { 3 }) {
                let high = (bytes.next()? as char).to_digit(16)? as u8;
                let low = (bytes.next()? as char).to_digit(16)? as u8;
                *channel = high * 16 + low;
            }
            if hex.len() == 6 {
                values[3] = 255;
            }
            values
        }
        _ => return None,
    };
    Some(Color::rgba(channels[0], channels[1], channels[2], channels[3]))
}

#[cfg(test)]
mod tests {
    use super::parse_color;
    use bweb_renderer::Color;

    #[test]
    fn parses_named_and_hex_colors() {
        assert_eq!(parse_color("red"), Some(Color::rgba(255, 0, 0, 255)));
        assert_eq!(parse_color("#0f8"), Some(Color::rgba(0, 255, 136, 255)));
        assert_eq!(parse_color("#11223380"), Some(Color::rgba(17, 34, 51, 128)));
        assert_eq!(parse_color("transparent"), None);
        assert_eq!(parse_color("not-a-color"), None);
    }
}
