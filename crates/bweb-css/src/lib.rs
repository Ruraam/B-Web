//! CSS model and style calculation foundation.

/// A single CSS property/value pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    /// Property name, for example `color`.
    pub property: String,
    /// Raw property value, for example `#fff`.
    pub value: String,
}

impl Declaration {
    /// Creates a CSS declaration.
    #[must_use]
    pub fn new(property: impl Into<String>, value: impl Into<String>) -> Self {
        Self { property: property.into(), value: value.into() }
    }
}
