//! Window-independent resize coalescing and dispatch for the shell.
//! This module has no winit or softbuffer dependency.

use bweb_layout::{ResizeOutcome, View, ViewBackend, Viewport};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeDispatch {
    Skip,
    Repaint,
    Reflow,
}

/// Keeps only the latest resize configuration until the next dispatch.
pub struct ResizeController<B> {
    view: View<B>,
    pending: Option<Viewport>,
}

impl<B: ViewBackend> ResizeController<B> {
    #[must_use]
    pub const fn new(view: View<B>) -> Self {
        Self { view, pending: None }
    }

    pub fn queue(&mut self, viewport: Viewport) {
        self.pending = Some(viewport);
    }

    #[must_use]
    pub fn view_mut(&mut self) -> &mut View<B> {
        &mut self.view
    }

    /// Repaints the current viewport after a content-only change such as scroll.
    pub fn repaint(&mut self) -> Result<ResizeDispatch, B::Error> {
        if self.view.repaint()? { Ok(ResizeDispatch::Repaint) } else { Ok(ResizeDispatch::Skip) }
    }

    /// Applies at most one pending configuration.
    pub fn dispatch(&mut self) -> Result<ResizeDispatch, B::Error> {
        let Some(viewport) = self.pending.take() else {
            return Ok(ResizeDispatch::Skip);
        };
        match self.view.resize(viewport)? {
            ResizeOutcome::Unchanged => Ok(ResizeDispatch::Skip),
            ResizeOutcome::Repainted => Ok(ResizeDispatch::Repaint),
            ResizeOutcome::Reflowed => Ok(ResizeDispatch::Reflow),
        }
    }

    #[must_use]
    pub const fn view(&self) -> &View<B> {
        &self.view
    }
}

#[cfg(test)]
mod tests {
    use super::{ResizeController, ResizeDispatch};
    use bweb_layout::{View, ViewBackend, Viewport};
    use std::convert::Infallible;

    #[derive(Debug, Default)]
    struct MockView {
        reflows: usize,
        repaints: usize,
    }

    impl ViewBackend for MockView {
        type Error = Infallible;
        fn reflow(&mut self, _width: u32, _scale: f64) -> Result<(), Self::Error> {
            self.reflows += 1;
            Ok(())
        }
        fn repaint(&mut self, _viewport: Viewport) -> Result<(), Self::Error> {
            self.repaints += 1;
            Ok(())
        }
    }

    fn vp(width: u32, height: u32) -> Viewport {
        Viewport::new(width, height, 1.0).expect("valid viewport")
    }

    fn controller() -> ResizeController<MockView> {
        ResizeController::new(View::new(MockView::default()))
    }

    #[test]
    fn coalesces_burst_to_latest_viewport() {
        let mut c = controller();
        c.queue(vp(900, 700));
        c.queue(vp(850, 650));
        c.queue(vp(800, 600));
        assert_eq!(c.dispatch(), Ok(ResizeDispatch::Reflow));
        assert_eq!(c.view().viewport(), Some(vp(800, 600)));
        assert_eq!(c.view().backend().reflows, 1);
        assert_eq!(c.view().backend().repaints, 1);
    }

    #[test]
    fn empty_queue_dispatches_skip() {
        let mut c = controller();
        assert_eq!(c.dispatch(), Ok(ResizeDispatch::Skip));
        assert_eq!(c.view().backend().reflows, 0);
        assert_eq!(c.view().backend().repaints, 0);
    }

    #[test]
    fn duplicate_configuration_dispatches_skip() {
        let mut c = controller();
        c.queue(vp(800, 600));
        assert_eq!(c.dispatch(), Ok(ResizeDispatch::Reflow));
        c.queue(vp(800, 600));
        assert_eq!(c.dispatch(), Ok(ResizeDispatch::Skip));
        assert_eq!(c.view().backend().reflows, 1);
        assert_eq!(c.view().backend().repaints, 1);
    }

    #[test]
    fn height_only_dispatches_repaint() {
        let mut c = controller();
        c.queue(vp(800, 600));
        assert_eq!(c.dispatch(), Ok(ResizeDispatch::Reflow));
        c.queue(vp(800, 720));
        assert_eq!(c.dispatch(), Ok(ResizeDispatch::Repaint));
        assert_eq!(c.view().backend().reflows, 1);
        assert_eq!(c.view().backend().repaints, 2);
    }

    #[test]
    fn content_only_repaint_dispatches_repaint() {
        let mut c = controller();
        assert_eq!(c.repaint(), Ok(ResizeDispatch::Skip));
        c.queue(vp(800, 600));
        assert_eq!(c.dispatch(), Ok(ResizeDispatch::Reflow));
        assert_eq!(c.repaint(), Ok(ResizeDispatch::Repaint));
        assert_eq!(c.view().backend().reflows, 1);
        assert_eq!(c.view().backend().repaints, 2);
    }

    #[test]
    fn width_change_dispatches_reflow() {
        let mut c = controller();
        c.queue(vp(800, 600));
        assert_eq!(c.dispatch(), Ok(ResizeDispatch::Reflow));
        c.queue(vp(640, 600));
        assert_eq!(c.dispatch(), Ok(ResizeDispatch::Reflow));
        assert_eq!(c.view().backend().reflows, 2);
        assert_eq!(c.view().backend().repaints, 2);
    }
}
