//! Reserved foundation for B-Web's future JavaScript runtime.
//!
//! No existing browser JavaScript engine is embedded in this project.

/// Explicit status of the JavaScript runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStatus {
    /// The runtime has not been implemented yet.
    NotImplemented,
}
