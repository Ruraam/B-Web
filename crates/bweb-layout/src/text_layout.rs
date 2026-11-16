//! Font-metric-based inline text layout and glyph positioning.
//!
//! This module is the first bridge between `bweb-text` and the layout tree.
//! It performs simple character-level layout (without shaping or kerning),
//! wraps at whitespace boundaries, and resolves bitmap bearings into final
//! physical-pixel glyph positions.

use bweb_renderer::PositionedGlyph;
use bweb_text::Font;

/// Rasterized glyphs and measured geometry for one text run.
///
/// Glyph positions are relative to the supplied origin and are expressed in
/// the same pixel coordinate space as the font size passed to
/// [`layout_text`]. The caller applies device scaling when producing paint
/// commands.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TextLayout {
    /// Glyph masks in visual order, positioned at their bitmap top-left.
    pub glyphs: Vec<PositionedGlyph>,
    /// Maximum measured line advance, excluding the origin.
    pub width: f32,
    /// Total block height.
    pub height: f32,
    /// Number of generated lines.
    pub line_count: usize,
}

/// Lays out and rasterizes text using real font advances and vertical metrics.
///
/// Whitespace is collapsed to one space, explicit newlines are preserved, and
/// wrapping occurs before a word when it would exceed `max_width`. This is a
/// deliberately unshaped first pass: ligatures, kerning, bidi, and script
/// shaping remain future text-engine work.
///
/// Invalid or non-positive font sizes produce an empty layout. A non-positive
/// line height uses the font's natural line height. A non-positive maximum
/// width disables automatic wrapping.
pub fn layout_text(
    font: &mut Font,
    text: &str,
    origin_x: f32,
    origin_y: f32,
    max_width: f32,
    px_size: f32,
    requested_line_height: f32,
) -> TextLayout {
    if text.is_empty()
        || !origin_x.is_finite()
        || !origin_y.is_finite()
        || !px_size.is_finite()
        || px_size <= 0.0
    {
        return TextLayout::default();
    }

    let metrics = font.metrics(px_size);
    let natural_line_height = metrics.line_height().max(1.0);
    let line_height = if requested_line_height.is_finite() && requested_line_height > 0.0 {
        requested_line_height
    } else {
        natural_line_height
    };
    let wrap_width = if max_width.is_finite() && max_width > 0.0 {
        max_width
    } else {
        f32::INFINITY
    };
    let baseline_offset = (line_height - natural_line_height) * 0.5 + metrics.ascent;

    let mut result = TextLayout { line_count: 1, ..TextLayout::default() };
    let mut pen_x = origin_x;
    let mut line_index = 0_usize;
    let mut pending_space = false;
    let mut word = String::new();

    let flush_word = |word: &str,
                      font: &mut Font,
                      result: &mut TextLayout,
                      pen_x: &mut f32,
                      line_index: &mut usize,
                      pending_space: &mut bool| {
        if word.is_empty() {
            return;
        }

        let space_advance = if *pending_space && *pen_x > origin_x {
            font.glyph_for(' ').map_or(0.0, |glyph| font.advance(glyph, px_size))
        } else {
            0.0
        };
        let word_advance: f32 = word
            .chars()
            .filter_map(|character| font.glyph_for(character))
            .map(|glyph| font.advance(glyph, px_size))
            .sum();

        if *pen_x > origin_x
            && *pen_x - origin_x + space_advance + word_advance > wrap_width
        {
            *line_index += 1;
            result.line_count = result.line_count.max(*line_index + 1);
            *pen_x = origin_x;
        } else {
            *pen_x += space_advance;
        }
        *pending_space = false;

        let baseline_y = origin_y + *line_index as f32 * line_height + baseline_offset;
        for character in word.chars() {
            let Some(glyph) = font.glyph_for(character) else {
                continue;
            };
            if let Some(coverage) = font.rasterize(glyph, px_size) {
                result.glyphs.push(PositionedGlyph {
                    x: *pen_x + coverage.left as f32,
                    y: baseline_y - coverage.top as f32,
                    coverage,
                });
            }
            *pen_x += font.advance(glyph, px_size);
        }
        result.width = result.width.max(*pen_x - origin_x);
    };

    for character in text.chars() {
        if character == '\\n' {
            flush_word(
                &word,
                font,
                &mut result,
                &mut pen_x,
                &mut line_index,
                &mut pending_space,
            );
            word.clear();
            pending_space = false;
            line_index += 1;
            result.line_count = result.line_count.max(line_index + 1);
            result.width = result.width.max(pen_x - origin_x);
            pen_x = origin_x;
        } else if character.is_whitespace() {
            flush_word(
                &word,
                font,
                &mut result,
                &mut pen_x,
                &mut line_index,
                &mut pending_space,
            );
            word.clear();
            pending_space = true;
        } else {
            word.push(character);
        }
    }
    flush_word(
        &word,
        font,
        &mut result,
        &mut pen_x,
        &mut line_index,
        &mut pending_space,
    );

    result.width = result.width.max(pen_x - origin_x);
    result.height = result.line_count as f32 * line_height;
    result
}

#[cfg(test)]
mod tests {
    use super::layout_text;
    use bweb_text::Font;

    fn font() -> Font {
        Font::from_bytes(include_bytes!("../../bweb-text/tests/data/test-font.ttf").to_vec())
            .expect("fixture font should load")
    }

    #[test]
    fn positions_bitmap_using_real_font_bearings_and_baseline() {
        let mut font = font();
        let metrics = font.metrics(24.0);
        let glyph_id = font.glyph_for('M').expect("M is mapped");
        let coverage = font.rasterize(glyph_id, 24.0).expect("M has an outline");
        let expected_advance = font.advance(glyph_id, 24.0);
        let expected_x = 12.5 + coverage.left as f32;
        let expected_y = 30.0 + metrics.ascent - coverage.top as f32;

        let layout = layout_text(&mut font, "M", 12.5, 30.0, 200.0, 24.0, 0.0);

        assert_eq!(layout.glyphs.len(), 1);
        assert_eq!(layout.glyphs[0].x, expected_x);
        assert_eq!(layout.glyphs[0].y, expected_y);
        assert!((layout.width - expected_advance).abs() < 0.01);
        assert_eq!(layout.line_count, 1);
    }

    #[test]
    fn wraps_words_using_real_glyph_advances() {
        let mut font = font();
        let glyph = font.glyph_for('M').expect("M is mapped");
        let two_glyphs = font.advance(glyph, 20.0) * 2.0;
        let layout = layout_text(&mut font, "M M", 0.0, 0.0, two_glyphs, 20.0, 24.0);

        assert_eq!(layout.line_count, 2);
        assert_eq!(layout.glyphs.len(), 2);
        assert_eq!(layout.glyphs[1].y - layout.glyphs[0].y, 24.0);
        assert!((layout.width - font.advance(glyph, 20.0)).abs() < 0.01);
        assert_eq!(layout.height, 48.0);
    }

    #[test]
    fn preserves_explicit_newlines_and_collapses_whitespace() {
        let mut font = font();
        let layout = layout_text(&mut font, "M   M\\nM", 0.0, 0.0, 500.0, 18.0, 22.0);

        assert_eq!(layout.line_count, 2);
        assert_eq!(layout.glyphs.len(), 3);
        assert_eq!(layout.height, 44.0);
    }

    #[test]
    fn rejects_invalid_font_sizes_and_coordinates() {
        let mut font = font();
        assert!(layout_text(&mut font, "text", 0.0, 0.0, 100.0, 0.0, 16.0).glyphs.is_empty());
        assert!(layout_text(&mut font, "text", f32::NAN, 0.0, 100.0, 16.0, 20.0)
            .glyphs
            .is_empty());
    }
}
