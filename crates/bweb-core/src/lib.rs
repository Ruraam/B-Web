//! Shared primitives for the B-Web engine.

/// Version of the B-Web workspace.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Identifies the current engine milestone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineStage {
    /// Workspace and crate boundaries are established.
    Bootstrap,
}
