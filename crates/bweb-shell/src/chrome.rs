//! Minimal browser chrome drawn over the software framebuffer.
//!
//! The page owns the whole framebuffer below the address bar; the bar is
//! composited on top after rasterization so layout never has to know about it.

use bweb_renderer::Framebuffer;
use bweb_text::Font;

/// Address-bar height in CSS pixels.
pub const ADDRESS_BAR_HEIGHT_CSS: f32 = 32.0;

/// Converts the address-bar height to physical pixels for a device scale.
#[must_use]
pub fn address_bar_height(scale_factor: f64) -> u32 {
    if !scale_factor.is_finite() || scale_factor <= 0.0 {
        return ADDRESS_BAR_HEIGHT_CSS as u32;
    }
    (ADDRESS_BAR_HEIGHT_CSS * scale_factor as f32).round().max(1.0) as u32
}

/// Draws a light address bar with the supplied text at the top of `framebuffer`.
pub fn draw_address_bar(
    font: &mut Font,
    framebuffer: &mut Framebuffer,
    text: &str,
    scale_factor: f64,
) {
    let width = framebuffer.width();
    let height = framebuffer.height();
    let bar_height = address_bar_height(scale_factor).min(height);
    if width == 0 || bar_height == 0 {
        return;
    }
    framebuffer.fill_rect(0, 0, width, bar_height, [244, 246, 248, 255]);
    framebuffer.fill_rect(0, (bar_height - 1) as i32, width, 1, [173, 181, 189, 255]);

    let px_size = 14.0 * scale_factor.max(0.25) as f32;
    let metrics = font.metrics(px_size);
    let baseline = (bar_height as f32 - metrics.line_height()) * 0.5 + metrics.ascent;
    let mut pen_x = 8.0_f32;

    for character in text.chars() {
        let Some(glyph) = font.glyph_for(character) else { continue };
        let Some(coverage) = font.rasterize(glyph, px_size) else { continue };
        let origin_x = pen_x + coverage.left as f32;
        let origin_y = baseline - coverage.top as f32;
        for row in 0..coverage.height {
            for column in 0..coverage.width {
                let alpha = coverage.alpha_at(column, row);
                if alpha == 0 {
                    continue;
                }
                let x = (origin_x + column as f32).round();
                let y = (origin_y + row as f32).round();
                if x < 0.0 || y < 0.0 || x >= width as f32 || y >= bar_height as f32 {
                    continue;
                }
                framebuffer.set_pixel(x as u32, y as u32, [24, 28, 32, alpha]);
            }
        }
        pen_x += font.advance(glyph, px_size);
        if pen_x > width as f32 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{address_bar_height, draw_address_bar};
    use bweb_renderer::Framebuffer;
    use bweb_text::Font;

    fn font() -> Font {
        Font::from_bytes(include_bytes!("../../bweb-text/tests/data/test-font.ttf").to_vec())
            .expect("fixture font should load")
    }

    #[test]
    fn scales_bar_height_with_device_scale() {
        assert_eq!(address_bar_height(1.0), 32);
        assert_eq!(address_bar_height(2.0), 64);
        assert_eq!(address_bar_height(f64::NAN), 32);
    }

    #[test]
    fn paints_bar_without_touching_page_below_it() {
        let mut framebuffer = Framebuffer::new(100, 64).expect("framebuffer");
        framebuffer.clear([255, 255, 255, 255]);
        draw_address_bar(&mut font(), &mut framebuffer, "https://example.test/", 1.0);

        assert_eq!(framebuffer.pixels()[0..4], [244, 246, 248, 255]);
        let bottom = (63 * framebuffer.stride()) + 99 * 4;
        assert_eq!(framebuffer.pixels()[bottom..bottom + 4], [255, 255, 255, 255]);
    }
}
