//! HTML document and DOM foundation.
//!
//! Parsing and standards-compliant tree construction will be implemented in a later milestone.

/// A source document awaiting parsing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HtmlDocument {
    source: String,
}

impl HtmlDocument {
    /// Creates a document from its source markup.
    #[must_use]
    pub fn new(source: impl Into<String>) -> Self {
        Self { source: source.into() }
    }

    /// Returns the original markup.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }
}

#[cfg(test)]
mod tests {
    use super::HtmlDocument;

    #[test]
    fn preserves_source_markup() {
        let document = HtmlDocument::new("<title>B-Web</title>");
        assert_eq!(document.source(), "<title>B-Web</title>");
    }
}
