//! Shared primitives for the B-Web engine.

/// Version of the B-Web workspace.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Identifies the stage of the engine's development plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineStage {
    /// Workspace and crate boundaries established.
    Bootstrap,
    /// DOM, CSS parsing, cascade and computed styles available.
    StyledDom,
    /// Box tree, inline formatting, text and image painting available.
    LayoutAndPaint,
    /// Navigation, history and browser chrome wired to the engine.
    Interactive,
    /// Full standards compliance across the supported web platform.
    StandardsCompliant,
}

/// Stage reached by the current tree.
pub const STAGE: EngineStage = EngineStage::Interactive;
