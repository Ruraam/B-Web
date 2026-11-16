//! Font loading, metrics, character mapping, and glyph rasterization for B-Web.
//!
//! This crate deliberately does not perform text shaping or line breaking.
//! Those responsibilities belong to `bweb-layout`. The renderer will consume
//! positioned glyph data and coverage masks without depending on this crate.
//!
//! Swash is kept behind this small API so its implementation can be replaced
//! without coupling layout or rendering to Swash types.

use std::fmt;

use swash::scale::image::Content;
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::{CacheKey, FontRef};

/// Errors that can occur while loading font data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontError {
    /// The bytes do not contain a valid font at the requested collection index.
    Invalid,
}

impl fmt::Display for FontError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid => f.write_str("invalid or unsupported font data"),
        }
    }
}

impl std::error::Error for FontError {}

/// Global font metrics scaled to a pixel size.
///
/// The descent is positive and measures the distance below the baseline.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Metrics {
    pub ascent: f32,
    pub descent: f32,
    pub line_gap: f32,
    pub px_size: f32,
}

impl Metrics {
    /// Returns the natural line height: ascent + descent + line gap.
    pub fn line_height(&self) -> f32 {
        self.ascent + self.descent + self.line_gap
    }
}

/// An 8-bit grayscale glyph coverage mask.
///
/// `left` and `top` are bearings relative to the glyph origin at the baseline.
/// The bitmap's top-left corner is placed at (origin_x + left, baseline_y - top).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Coverage {
    pub width: u32,
    pub height: u32,
    pub left: i32,
    pub top: i32,
    pub data: Vec<u8>,
}

impl Coverage {
    /// Returns true when the glyph has no bitmap area.
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Returns the coverage at a pixel, or zero when the coordinates are outside.
    pub fn alpha_at(&self, x: u32, y: u32) -> u8 {
        if x >= self.width || y >= self.height {
            return 0;
        }

        let index = (y as usize)
            .saturating_mul(self.width as usize)
            .saturating_add(x as usize);
        self.data.get(index).copied().unwrap_or(0)
    }
}

/// An owned font face with a reusable Swash scaling context.
///
/// Font bytes remain owned by this value. The selected collection face is
/// represented by its validated table offset and cache key.
pub struct Font {
    data: Vec<u8>,
    offset: u32,
    key: CacheKey,
    scale_context: ScaleContext,
}

impl Font {
    /// Loads the first face from TTF, OTF, or TTC font data.
    pub fn from_bytes(data: Vec<u8>) -> Result<Self, FontError> {
        Self::from_index(data, 0)
    }

    /// Loads a face by zero-based index from TTF, OTF, or TTC font data.
    pub fn from_index(data: Vec<u8>, index: usize) -> Result<Self, FontError> {
        let font = FontRef::from_index(&data, index).ok_or(FontError::Invalid)?;

        Ok(Self {
            data,
            offset: font.offset,
            key: font.key,
            scale_context: ScaleContext::new(),
        })
    }

    fn font_ref(&self) -> FontRef<'_> {
        FontRef {
            data: &self.data,
            offset: self.offset,
            key: self.key,
        }
    }

    /// Returns global metrics scaled to the requested pixel size.
    ///
    /// Invalid sizes return zeroed metrics. The size is pixels per em.
    pub fn metrics(&self, px_size: f32) -> Metrics {
        if !px_size.is_finite() || px_size <= 0.0 {
            return Metrics::default();
        }

        let metrics = self.font_ref().metrics(&[]).scale(px_size);
        Metrics {
            ascent: metrics.ascent,
            descent: metrics.descent,
            line_gap: metrics.leading,
            px_size,
        }
    }

    /// Maps a Unicode scalar to its nominal glyph identifier.
    ///
    /// Glyph zero is the missing-glyph entry and is exposed as `None` so
    /// callers can perform font fallback.
    pub fn glyph_for(&self, character: char) -> Option<u16> {
        let glyph = self.font_ref().charmap().map(character);
        (glyph != 0).then_some(glyph)
    }

    /// Returns the unshaped horizontal advance of a glyph at the requested size.
    ///
    /// Kerning and other positioning adjustments are intentionally outside
    /// this API and will be handled by the future shaping stage.
    pub fn advance(&self, glyph: u16, px_size: f32) -> f32 {
        if !px_size.is_finite() || px_size <= 0.0 {
            return 0.0;
        }

        self.font_ref()
            .glyph_metrics(&[])
            .scale(px_size)
            .advance_width(glyph)
    }

    /// Rasterizes a glyph outline into an 8-bit alpha coverage mask.
    ///
    /// Glyphs without an outline (for example, a space) return `None`.
    /// Color glyphs and embedded bitmap strikes are intentionally excluded
    /// from this first milestone.
    pub fn rasterize(&mut self, glyph: u16, px_size: f32) -> Option<Coverage> {
        if !px_size.is_finite() || px_size <= 0.0 {
            return None;
        }

        let font = FontRef {
            data: &self.data,
            offset: self.offset,
            key: self.key,
        };
        let mut scaler = self
            .scale_context
            .builder(font)
            .size(px_size)
            .hint(false)
            .build();

        let image = Render::new(&[Source::Outline])
            .format(swash::zeno::Format::Alpha)
            .strike_with(StrikeWith::BestFit)
            .render(&mut scaler, glyph)?;

        if image.content != Content::Mask {
            return None;
        }

        Some(Coverage {
            width: image.placement.width,
            height: image.placement.height,
            left: image.placement.left,
            top: image.placement.top,
            data: image.data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Coverage, Font, FontError};

    fn test_font_bytes() -> Vec<u8> {
        include_bytes!("../tests/data/test-font.ttf").to_vec()
    }

    fn load() -> Font {
        Font::from_bytes(test_font_bytes()).expect("fixture font should load")
    }

    #[test]
    fn loading_invalid_data_fails() {
        assert_eq!(Font::from_bytes(vec![0; 16]).err(), Some(FontError::Invalid));
    }

    #[test]
    fn cmap_maps_ascii_and_distinguishes_case() {
        let font = load();
        let upper = font.glyph_for('A').expect("uppercase A is mapped");
        let lower = font.glyph_for('a').expect("lowercase a is mapped");

        assert_ne!(upper, lower);
    }

    #[test]
    fn cmap_returns_none_for_unmapped_scalar() {
        assert_eq!(load().glyph_for('\u{10FFFF}'), None);
    }

    #[test]
    fn metrics_are_positive_and_scale_with_pixel_size() {
        let font = load();
        let metrics_16 = font.metrics(16.0);
        let metrics_32 = font.metrics(32.0);

        assert!(metrics_16.ascent > 0.0);
        assert!(metrics_16.descent > 0.0);
        assert!(metrics_16.line_height() > metrics_16.ascent);
        assert_eq!(metrics_16.px_size, 16.0);
        assert!((metrics_32.ascent / metrics_16.ascent - 2.0).abs() < 0.05);
        assert!((metrics_32.descent / metrics_16.descent - 2.0).abs() < 0.05);
    }

    #[test]
    fn unshaped_advance_is_positive_and_scales_linearly() {
        let font = load();
        let glyph = font.glyph_for('M').expect("M is mapped");
        let advance_16 = font.advance(glyph, 16.0);
        let advance_32 = font.advance(glyph, 32.0);

        assert!(advance_16 > 0.0);
        assert!((advance_32 / advance_16 - 2.0).abs() < 0.05);
    }

    #[test]
    fn rasterized_glyph_has_consistent_coverage_storage() {
        let mut font = load();
        let glyph = font.glyph_for('M').expect("M is mapped");
        let coverage = font.rasterize(glyph, 32.0).expect("M has an outline");

        assert!(!coverage.is_empty());
        assert_eq!(
            coverage.data.len(),
            coverage.width as usize * coverage.height as usize
        );
        assert!(coverage.data.iter().any(|&alpha| alpha > 0));
        assert!(coverage.data.iter().any(|&alpha| alpha == 255));
    }

    #[test]
    fn rasterize_rejects_invalid_pixel_sizes() {
        let mut font = load();
        let glyph = font.glyph_for('M').expect("M is mapped");

        assert!(font.rasterize(glyph, 0.0).is_none());
        assert!(font.rasterize(glyph, f32::NAN).is_none());
        assert!(font.rasterize(glyph, f32::INFINITY).is_none());
    }

    #[test]
    fn space_has_no_outline_bitmap() {
        let mut font = load();
        let glyph = font.glyph_for(' ').expect("space is mapped");

        assert!(font.rasterize(glyph, 16.0).is_none());
    }

    #[test]
    fn coverage_access_is_bounds_checked() {
        let coverage = Coverage {
            width: 2,
            height: 1,
            left: 0,
            top: 1,
            data: vec![0, 255],
        };

        assert_eq!(coverage.alpha_at(0, 0), 0);
        assert_eq!(coverage.alpha_at(1, 0), 255);
        assert_eq!(coverage.alpha_at(2, 0), 0);
        assert_eq!(coverage.alpha_at(0, 1), 0);
    }
}
