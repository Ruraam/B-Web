//! Font-metric-based inline text layout and glyph positioning.
//!
//! This module bridges `bweb-text` and the layout tree. Words are shaped with
//! Swash (kerning and ligatures), the layout fragments are wrapped at
//! whitespace boundaries, and bitmap bearings are resolved into final
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
    let wrap_width =
        if max_width.is_finite() && max_width > 0.0 { max_width } else { f32::INFINITY };
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
        let shaped = font.shape(word, px_size);
        let word_advance: f32 = shaped.iter().map(|glyph| glyph.advance).sum();

        if *pen_x > origin_x && *pen_x - origin_x + space_advance + word_advance > wrap_width {
            *line_index += 1;
            result.line_count = result.line_count.max(*line_index + 1);
            *pen_x = origin_x;
        } else {
            *pen_x += space_advance;
        }
        *pending_space = false;

        let baseline_y = origin_y + *line_index as f32 * line_height + baseline_offset;
        for glyph in shaped {
            if let Some(coverage) = font.rasterize(glyph.glyph, px_size) {
                result.glyphs.push(PositionedGlyph {
                    x: *pen_x + glyph.x + coverage.left as f32,
                    y: baseline_y - coverage.top as f32,
                    coverage,
                });
            }
            *pen_x += glyph.advance;
        }
        result.width = result.width.max(*pen_x - origin_x);
    };

    for character in text.chars() {
        if character == '\n' {
            flush_word(&word, font, &mut result, &mut pen_x, &mut line_index, &mut pending_space);
            word.clear();
            pending_space = false;
            line_index += 1;
            result.line_count = result.line_count.max(line_index + 1);
            result.width = result.width.max(pen_x - origin_x);
            pen_x = origin_x;
        } else if character.is_whitespace() {
            flush_word(&word, font, &mut result, &mut pen_x, &mut line_index, &mut pending_space);
            word.clear();
            pending_space = true;
        } else {
            word.push(character);
        }
    }
    flush_word(&word, font, &mut result, &mut pen_x, &mut line_index, &mut pending_space);

    result.width = result.width.max(pen_x - origin_x);
    result.height = result.line_count as f32 * line_height;
    result
}

/// Builds renderer-ready text runs from the inline fragments in a layout tree.
///
/// The caller supplies the selected font face. Font-family matching and fallback
/// are intentionally left to the font-selection layer. Fragment geometry and CSS
/// font sizes are logical pixels; glyph masks and positions are produced at the
/// requested device scale so the Painter must not scale them a second time.
pub fn prepare_text_runs(
    tree: &crate::LayoutTree,
    styles: &bweb_css::ComputedStyleMap,
    font: &mut Font,
    scale_factor: f32,
) -> std::collections::HashMap<bweb_html::NodeId, std::sync::Arc<bweb_renderer::TextRun>> {
    use bweb_html::NodeId;
    use bweb_renderer::{Color, TextRun};
    use std::{collections::HashMap, sync::Arc};

    fn collect<'a>(layout: &'a crate::LayoutBox, output: &mut Vec<&'a crate::TextFragment>) {
        for line in &layout.line_boxes {
            output.extend(line.fragments.iter());
        }
        for child in &layout.children {
            collect(child, output);
        }
    }

    if !scale_factor.is_finite() || scale_factor <= 0.0 {
        return HashMap::new();
    }

    let mut fragments = Vec::new();
    collect(&tree.root, &mut fragments);
    let mut runs: HashMap<NodeId, TextRun> = HashMap::new();

    for fragment in fragments {
        let Some(style) = fragment.style.as_deref().or_else(|| styles.get(fragment.node)) else {
            continue;
        };
        if style.get("visibility") == Some("hidden") || style.get("color") == Some("transparent") {
            continue;
        }
        let font_size = style.get("font-size").and_then(parse_css_px).unwrap_or(16.0);
        if font_size <= 0.0 {
            continue;
        }
        let line_height = fragment.rect.height.max(1.0) * scale_factor;
        let color = bweb_renderer::parse_color(style.get("color").unwrap_or("black"))
            .unwrap_or(Color::rgba(0, 0, 0, 255));
        let decoration = match style.get("text-decoration") {
            Some("underline") => bweb_renderer::TextDecoration::Underline,
            Some("line-through") => bweb_renderer::TextDecoration::LineThrough,
            _ => bweb_renderer::TextDecoration::None,
        };
        let shadows: Vec<bweb_renderer::TextShadow> = style
            .get("text-shadow")
            .filter(|value| *value != "none")
            .map(|value| {
                bweb_renderer::split_shadow_layers(value)
                    .into_iter()
                    .filter_map(|layer| {
                        bweb_renderer::parse_text_shadow(layer.trim(), |token| {
                            token
                                .trim()
                                .trim_end_matches("px")
                                .trim()
                                .parse::<f32>()
                                .ok()
                                .map(|value| value * scale_factor)
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let layout = layout_text(
            font,
            &fragment.text,
            fragment.rect.x * scale_factor,
            fragment.rect.y * scale_factor,
            0.0,
            font_size * scale_factor,
            line_height,
        );
        if layout.glyphs.is_empty() {
            continue;
        }
        let run = runs.entry(fragment.node).or_insert_with(|| TextRun {
            glyphs: Vec::new(),
            color,
            decoration,
            shadows,
        });
        run.glyphs.extend(layout.glyphs);
    }

    runs.into_iter().map(|(node, run)| (node, Arc::new(run))).collect()
}

fn parse_css_px(value: &str) -> Option<f32> {
    let value = value.trim();
    let number = value.strip_suffix("px").unwrap_or(value).trim().parse::<f32>().ok()?;
    (number.is_finite() && number > 0.0).then_some(number)
}

#[cfg(test)]
mod tests {
    use super::{layout_text, prepare_text_runs};
    use crate::{LayoutTree, PaintAdapter, Rect};
    use bweb_css::{StyleEngine, Stylesheet};
    use bweb_html::HtmlDocument;
    use bweb_renderer::{Framebuffer, PaintItem, Painter, Rasterizer};
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
    fn measured_width_uses_shaped_advances() {
        let mut font = font();
        let shaped = font.shape("MM", 20.0);
        let expected: f32 = shaped.iter().map(|glyph| glyph.advance).sum();
        let layout = layout_text(&mut font, "MM", 0.0, 0.0, 200.0, 20.0, 24.0);
        assert!((layout.width - expected).abs() < 0.01);
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
        let layout = layout_text(&mut font, "M   M\nM", 0.0, 0.0, 500.0, 18.0, 22.0);

        assert_eq!(layout.line_count, 2);
        assert_eq!(layout.glyphs.len(), 3);
        assert_eq!(layout.height, 44.0);
    }

    #[test]
    fn rejects_invalid_font_sizes_and_coordinates() {
        let mut font = font();
        assert!(layout_text(&mut font, "text", 0.0, 0.0, 100.0, 0.0, 16.0).glyphs.is_empty());
        assert!(layout_text(&mut font, "text", f32::NAN, 0.0, 100.0, 16.0, 20.0).glyphs.is_empty());
    }

    #[test]
    fn prepares_scaled_runs_from_layout_fragments_and_computed_styles() {
        use super::prepare_text_runs;
        use bweb_css::{StyleEngine, Stylesheet};
        use bweb_html::{HtmlDocument, NodeKind};

        let document = HtmlDocument::parse("<main>MMMMM</main>");
        let sheet = Stylesheet::parse("main { display:block; font-size:20px; color:#123456 }")
            .expect("valid CSS");
        let styles = StyleEngine::new().compute(&document, &[sheet]);
        let tree = crate::LayoutTree::build(&document, &styles, 320.0);
        let text_node = document
            .iter()
            .find_map(|(id, node)| matches!(node.kind(), NodeKind::Text(_)).then_some(id))
            .expect("text node");
        let mut font = font();

        let runs = prepare_text_runs(&tree, &styles, &mut font, 2.0);
        let run = runs.get(&text_node).expect("prepared text run");

        assert_eq!(run.color, bweb_renderer::Color::rgba(0x12, 0x34, 0x56, 255));
        assert_eq!(run.glyphs.len(), 5);
        assert!(run.glyphs.iter().all(|glyph| glyph.x >= 0.0 && glyph.y >= 0.0));
    }

    #[test]
    fn viewport_width_change_reflows_lines_and_prepared_glyph_positions() {
        use bweb_html::NodeKind;

        let document = HtmlDocument::parse("<main>MMMMMMMMMMMM</main>");
        let stylesheet =
            Stylesheet::parse("main { display:block; font-size:20px }").expect("valid stylesheet");
        let styles = StyleEngine::new().compute(&document, &[stylesheet]);
        let main = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "main")
                    .then_some(id)
            })
            .expect("main element");

        let wide_tree = LayoutTree::build(&document, &styles, 400.0);
        let narrow_tree = LayoutTree::build(&document, &styles, 40.0);
        let wide_main = find_layout_box(&wide_tree.root, main).expect("wide main box");
        let narrow_main = find_layout_box(&narrow_tree.root, main).expect("narrow main box");

        assert_eq!(wide_main.line_boxes.len(), 1);
        assert!(narrow_main.line_boxes.len() > wide_main.line_boxes.len());

        let mut wide_font = font();
        let mut narrow_font = font();
        let wide_runs = prepare_text_runs(&wide_tree, &styles, &mut wide_font, 1.0);
        let narrow_runs = prepare_text_runs(&narrow_tree, &styles, &mut narrow_font, 1.0);
        let wide_glyphs = &wide_runs.get(&main_text_node(&document)).expect("wide text run").glyphs;
        let narrow_glyphs =
            &narrow_runs.get(&main_text_node(&document)).expect("narrow text run").glyphs;

        assert_eq!(wide_glyphs.len(), 12);
        assert_eq!(narrow_glyphs.len(), 12);
        assert!(wide_glyphs.iter().all(|glyph| glyph.y == wide_glyphs[0].y));
        assert!(
            narrow_glyphs.iter().any(|glyph| glyph.y > narrow_glyphs[0].y),
            "narrower viewport must reposition glyphs onto subsequent lines",
        );
    }

    #[test]
    fn viewport_height_change_does_not_recompute_layout_or_text_geometry() {
        let document = HtmlDocument::parse("<main>MMMMMMMMMMMM</main>");
        let stylesheet =
            Stylesheet::parse("main { display:block; font-size:20px }").expect("valid stylesheet");
        let styles = StyleEngine::new().compute(&document, &[stylesheet]);

        // Height belongs to the paint viewport, not the top-down layout inputs.
        let first_layout = LayoutTree::build(&document, &styles, 80.0);
        let second_layout = LayoutTree::build(&document, &styles, 80.0);
        assert_eq!(first_layout, second_layout);

        let mut first_font = font();
        let mut second_font = font();
        let first_runs = prepare_text_runs(&first_layout, &styles, &mut first_font, 1.0);
        let second_runs = prepare_text_runs(&second_layout, &styles, &mut second_font, 1.0);

        let first_text = first_runs.values().next().expect("prepared text");
        let second_text = second_runs.values().next().expect("prepared text");
        assert_eq!(first_text.glyphs.len(), second_text.glyphs.len());
        for (first, second) in first_text.glyphs.iter().zip(&second_text.glyphs) {
            assert_eq!((first.x, first.y), (second.x, second.y));
        }
    }

    fn main_text_node(document: &HtmlDocument) -> bweb_html::NodeId {
        use bweb_html::NodeKind;
        document
            .iter()
            .find_map(|(id, node)| matches!(node.kind(), NodeKind::Text(_)).then_some(id))
            .expect("main text node")
    }

    fn find_layout_box(
        layout: &crate::LayoutBox,
        node: bweb_html::NodeId,
    ) -> Option<&crate::LayoutBox> {
        if layout.node == node {
            return Some(layout);
        }
        layout.children.iter().find_map(|child| find_layout_box(child, node))
    }

    #[test]
    fn prepared_dom_text_reaches_the_framebuffer_through_the_full_paint_pipeline() {
        let document = HtmlDocument::parse("<main>MMMMM</main>");
        let stylesheet = Stylesheet::parse("main { display:block; font-size:20px; color:#123456 }")
            .expect("valid stylesheet");
        let styles = StyleEngine::new().compute(&document, &[stylesheet]);
        let tree = LayoutTree::build(&document, &styles, 320.0);
        let mut font = font();
        let text_runs = prepare_text_runs(&tree, &styles, &mut font, 1.0);
        assert!(!text_runs.is_empty(), "DOM text should produce prepared glyphs");

        let adapter =
            PaintAdapter::new(&tree, &styles, Rect { x: 0.0, y: 0.0, width: 320.0, height: 100.0 })
                .with_text_runs(&text_runs);
        let display_list = Painter::new(&adapter, 1.0).expect("valid device scale").paint();
        assert!(
            display_list.items.iter().any(|item| matches!(item, PaintItem::Text { .. })),
            "prepared text should become a display-list text command",
        );

        let mut framebuffer = Framebuffer::new(320, 100).expect("framebuffer");
        Rasterizer.rasterize(&display_list, &mut framebuffer);

        assert!(
            framebuffer.pixels().chunks_exact(4).any(|pixel| {
                pixel[0] == 0x12 && pixel[1] == 0x34 && pixel[2] == 0x56 && pixel[3] > 0
            }),
            "rasterized glyph pixels should retain the computed CSS text color",
        );
    }
}
