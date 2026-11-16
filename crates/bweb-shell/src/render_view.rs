//! Retained software-rendering backend used by the native window resize path.

use bweb_css::ComputedStyleMap;
use bweb_html::HtmlDocument;
use bweb_layout::{LayoutTree, PaintAdapter, Rect, ViewBackend, Viewport};
use bweb_renderer::{DisplayList, Framebuffer, PaintItem, Painter, Rasterizer};
use bweb_text::Font;
use std::io;

/// Retains layout, prepared text, and paint commands across viewport changes.
pub struct BrowserViewBackend {
    document: HtmlDocument,
    styles: ComputedStyleMap,
    font: Font,
    display_list: Option<DisplayList>,
    framebuffer: Option<Framebuffer>,
}

impl BrowserViewBackend {
    #[must_use]
    pub fn new(document: HtmlDocument, styles: ComputedStyleMap, font: Font) -> Self {
        Self { document, styles, font, display_list: None, framebuffer: None }
    }

    #[must_use]
    pub fn framebuffer(&self) -> Option<&Framebuffer> {
        self.framebuffer.as_ref()
    }
}

impl ViewBackend for BrowserViewBackend {
    type Error = String;

    fn reflow(&mut self, width: u32, scale_factor: f64) -> Result<(), Self::Error> {
        let scale = scale_factor as f32;
        if !scale.is_finite() || scale <= 0.0 {
            return Err("device scale factor is outside the supported f32 range".into());
        }
        let width_css = width as f32 / scale;
        if !width_css.is_finite() || width_css <= 0.0 {
            return Err("logical viewport width is invalid".into());
        }

        let layout = LayoutTree::build(&self.document, &self.styles, width_css);
        let text_runs =
            bweb_layout::prepare_text_runs(&layout, &self.styles, &mut self.font, scale);

        // The viewport clip height is patched during repaint. Keeping it out of
        // layout inputs lets height-only changes reuse this complete display list.
        let adapter = PaintAdapter::new(
            &layout,
            &self.styles,
            Rect { x: 0.0, y: 0.0, width: width_css, height: 0.0 },
        )
        .with_text_runs(&text_runs);
        let display_list =
            Painter::new(&adapter, scale).map_err(|error| error.to_string())?.paint();

        self.display_list = Some(display_list);
        Ok(())
    }

    fn repaint(&mut self, viewport: Viewport) -> Result<(), Self::Error> {
        let display_list = self
            .display_list
            .as_mut()
            .ok_or_else(|| "cannot repaint before the initial layout".to_owned())?;
        let Some(PaintItem::PushClip { rect }) = display_list.items.first_mut() else {
            return Err("display list is missing its viewport clip".into());
        };
        rect.width = viewport.width() as f32;
        rect.height = viewport.height() as f32;

        let mut framebuffer = Framebuffer::new(viewport.width(), viewport.height())
            .map_err(|error| format!("could not allocate render frame: {error:?}"))?;
        framebuffer.clear([255, 255, 255, 255]);
        Rasterizer.rasterize(display_list, &mut framebuffer);
        self.framebuffer = Some(framebuffer);
        Ok(())
    }
}
