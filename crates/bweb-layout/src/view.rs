//! Viewport state and resize invalidation policy.
//!
//! A width or scale change invalidates layout and text geometry. A height-only
//! change reuses that geometry and requests painting only.

/// A validated viewport in physical pixels and its output scale factor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    width: u32,
    height: u32,
    scale_factor: f64,
}

impl Viewport {
    /// Creates a viewport, rejecting empty dimensions and invalid scale factors.
    pub fn new(width: u32, height: u32, scale_factor: f64) -> Option<Self> {
        if width == 0 || height == 0 || !scale_factor.is_finite() || scale_factor <= 0.0 {
            return None;
        }
        Some(Self { width, height, scale_factor })
    }

    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(self) -> u32 {
        self.height
    }

    #[must_use]
    pub const fn scale_factor(self) -> f64 {
        self.scale_factor
    }

    /// Returns this viewport with a new height, rejecting zero.
    #[must_use]
    pub fn with_height(self, height: u32) -> Option<Self> {
        Self::new(self.width, height, self.scale_factor)
    }
}

/// Work performed in response to a viewport change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeOutcome {
    /// The complete viewport configuration was unchanged.
    Unchanged,
    /// Layout and text geometry were recalculated, then the view was painted.
    Reflowed,
    /// Existing layout and text geometry were reused; only painting was requested.
    Repainted,
}

/// Operations required from the rendering pipeline when a view changes size.
pub trait ViewBackend {
    type Error;

    /// Recalculates layout and prepared text for a new width and output scale.
    fn reflow(&mut self, width: u32, scale_factor: f64) -> Result<(), Self::Error>;

    /// Paints the current layout into the supplied viewport.
    fn repaint(&mut self, viewport: Viewport) -> Result<(), Self::Error>;
}

/// Tracks viewport state and applies the minimal required invalidation.
pub struct View<B> {
    backend: B,
    viewport: Option<Viewport>,
    layout_width: Option<u32>,
    layout_scale_factor: Option<f64>,
}

impl<B: ViewBackend> View<B> {
    /// Creates a view whose first resize performs its initial layout and paint.
    #[must_use]
    pub const fn new(backend: B) -> Self {
        Self { backend, viewport: None, layout_width: None, layout_scale_factor: None }
    }

    /// Applies a new viewport configuration without repeating redundant work.
    ///
    /// Width or scale changes trigger reflow (including text preparation) and
    /// painting. A height-only change triggers painting without reflow.
    pub fn resize(&mut self, next: Viewport) -> Result<ResizeOutcome, B::Error> {
        let needs_reflow = self.layout_width != Some(next.width)
            || self.layout_scale_factor != Some(next.scale_factor);
        let needs_paint = self.viewport != Some(next) || needs_reflow;

        if !needs_paint {
            return Ok(ResizeOutcome::Unchanged);
        }

        if needs_reflow {
            self.backend.reflow(next.width, next.scale_factor)?;
            // Record successful geometry independently: if painting fails, a
            // retry at the same width must not repeat the expensive reflow.
            self.layout_width = Some(next.width);
            self.layout_scale_factor = Some(next.scale_factor);
        }

        self.backend.repaint(next)?;
        self.viewport = Some(next);

        Ok(if needs_reflow { ResizeOutcome::Reflowed } else { ResizeOutcome::Repainted })
    }

    /// Repaints the current viewport without changing its configuration.
    ///
    /// Returns whether a repaint was performed. This is used for content-only
    /// changes such as scrolling, which must not invalidate layout or text.
    pub fn repaint(&mut self) -> Result<bool, B::Error> {
        match self.viewport {
            Some(viewport) => {
                self.backend.repaint(viewport)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Changes only the viewport height and repaints without running layout.
    /// Returns None if the view is not initialized or height is zero.
    pub fn set_viewport_height(&mut self, height: u32) -> Result<Option<ResizeOutcome>, B::Error> {
        let Some(current) = self.viewport else { return Ok(None) };
        let Some(next) = current.with_height(height) else { return Ok(None) };
        self.resize(next).map(Some)
    }

    #[must_use]
    pub const fn viewport(&self) -> Option<Viewport> {
        self.viewport
    }

    #[must_use]
    pub const fn backend(&self) -> &B {
        &self.backend
    }

    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    #[must_use]
    pub fn into_backend(self) -> B {
        self.backend
    }
}

#[cfg(test)]
mod tests {
    use super::{ResizeOutcome, View, ViewBackend, Viewport};
    use std::convert::Infallible;

    #[derive(Debug, Default)]
    struct MockLayout {
        reflow_calls: usize,
        repaint_calls: usize,
        last_reflow: Option<(u32, f64)>,
        last_paint: Option<Viewport>,
    }

    impl ViewBackend for MockLayout {
        type Error = Infallible;

        fn reflow(&mut self, width: u32, scale_factor: f64) -> Result<(), Self::Error> {
            self.reflow_calls += 1;
            self.last_reflow = Some((width, scale_factor));
            Ok(())
        }

        fn repaint(&mut self, viewport: Viewport) -> Result<(), Self::Error> {
            self.repaint_calls += 1;
            self.last_paint = Some(viewport);
            Ok(())
        }
    }

    fn viewport(width: u32, height: u32, scale: f64) -> Viewport {
        Viewport::new(width, height, scale).expect("valid viewport")
    }

    #[test]
    fn first_resize_performs_initial_reflow_and_paint() {
        let mut view = View::new(MockLayout::default());

        assert_eq!(view.resize(viewport(800, 600, 1.0)), Ok(ResizeOutcome::Reflowed));
        assert_eq!(view.backend().reflow_calls, 1);
        assert_eq!(view.backend().repaint_calls, 1);
        assert_eq!(view.backend().last_reflow, Some((800, 1.0)));
    }

    #[test]
    fn identical_resize_is_idempotent() {
        let mut view = View::new(MockLayout::default());
        let size = viewport(800, 600, 1.0);

        assert_eq!(view.resize(size), Ok(ResizeOutcome::Reflowed));
        assert_eq!(view.resize(size), Ok(ResizeOutcome::Unchanged));
        assert_eq!(view.backend().reflow_calls, 1);
        assert_eq!(view.backend().repaint_calls, 1);
    }

    #[test]
    fn width_change_reflows_and_repaints() {
        let mut view = View::new(MockLayout::default());
        view.resize(viewport(800, 600, 1.0)).expect("initial render");

        assert_eq!(view.resize(viewport(640, 600, 1.0)), Ok(ResizeOutcome::Reflowed));
        assert_eq!(view.backend().reflow_calls, 2);
        assert_eq!(view.backend().repaint_calls, 2);
        assert_eq!(view.backend().last_reflow, Some((640, 1.0)));
    }

    #[test]
    fn height_only_change_repaints_without_reflow() {
        let mut view = View::new(MockLayout::default());
        view.resize(viewport(800, 600, 1.0)).expect("initial render");

        assert_eq!(view.resize(viewport(800, 720, 1.0)), Ok(ResizeOutcome::Repainted));
        assert_eq!(view.backend().reflow_calls, 1);
        assert_eq!(view.backend().repaint_calls, 2);
        assert_eq!(view.backend().last_paint, Some(viewport(800, 720, 1.0)));
    }

    #[test]
    fn explicit_repaint_reuses_geometry_without_reflow() {
        let mut view = View::new(MockLayout::default());
        assert_eq!(view.repaint(), Ok(false));
        view.resize(viewport(800, 600, 1.0)).expect("initial render");

        assert_eq!(view.repaint(), Ok(true));
        assert_eq!(view.backend().reflow_calls, 1);
        assert_eq!(view.backend().repaint_calls, 2);
    }

    #[test]
    fn set_viewport_height_repaints_without_reflow() {
        let mut view = View::new(MockLayout::default());
        view.resize(viewport(800, 600, 1.0)).expect("initial render");
        assert_eq!(view.set_viewport_height(720), Ok(Some(ResizeOutcome::Repainted)));
        assert_eq!(view.backend().reflow_calls, 1);
        assert_eq!(view.backend().repaint_calls, 2);
        assert_eq!(view.viewport(), Some(viewport(800, 720, 1.0)));
    }

    #[test]
    fn set_viewport_height_is_idempotent_and_rejects_zero() {
        let mut view = View::new(MockLayout::default());
        view.resize(viewport(800, 600, 1.0)).expect("initial render");
        assert_eq!(view.set_viewport_height(600), Ok(Some(ResizeOutcome::Unchanged)));
        assert_eq!(view.set_viewport_height(0), Ok(None));
        assert_eq!(view.backend().reflow_calls, 1);
        assert_eq!(view.backend().repaint_calls, 1);
    }

    #[test]
    fn scale_change_reflows_and_repaints() {
        let mut view = View::new(MockLayout::default());
        view.resize(viewport(800, 600, 1.0)).expect("initial render");

        assert_eq!(view.resize(viewport(800, 600, 1.5)), Ok(ResizeOutcome::Reflowed));
        assert_eq!(view.backend().reflow_calls, 2);
        assert_eq!(view.backend().repaint_calls, 2);
        assert_eq!(view.backend().last_reflow, Some((800, 1.5)));
    }

    #[test]
    fn invalid_viewport_dimensions_and_scale_are_rejected() {
        assert!(Viewport::new(0, 600, 1.0).is_none());
        assert!(Viewport::new(800, 0, 1.0).is_none());
        assert!(Viewport::new(800, 600, 0.0).is_none());
        assert!(Viewport::new(800, 600, f64::NAN).is_none());
        assert!(Viewport::new(800, 600, f64::INFINITY).is_none());
    }
}
