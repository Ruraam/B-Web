//! Navigation history and URL normalization for the shell window.
//!
//! This module is window-independent and deliberately free of network code.

use url::Url;

/// A linear browsing history with a cursor for back/forward navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    entries: Vec<Url>,
    index: usize,
}

impl History {
    /// Starts a history containing a single entry.
    #[must_use]
    pub fn new(url: Url) -> Self {
        Self { entries: vec![url], index: 0 }
    }

    /// Returns the current entry.
    #[must_use]
    pub fn current(&self) -> &Url {
        &self.entries[self.index]
    }

    /// Appends a new entry, discarding any forward entries.
    pub fn push(&mut self, url: Url) {
        if self.current() == &url {
            return;
        }
        self.entries.truncate(self.index + 1);
        self.entries.push(url);
        self.index = self.entries.len() - 1;
    }

    /// Moves to the previous entry, returning whether the cursor moved.
    pub fn back(&mut self) -> bool {
        if self.index == 0 {
            return false;
        }
        self.index -= 1;
        true
    }

    /// Moves to the next entry, returning whether the cursor moved.
    pub fn forward(&mut self) -> bool {
        if self.index + 1 >= self.entries.len() {
            return false;
        }
        self.index += 1;
        true
    }

    #[must_use]
    pub const fn can_back(&self) -> bool {
        self.index > 0
    }

    #[must_use]
    pub fn can_forward(&self) -> bool {
        self.index + 1 < self.entries.len()
    }
}

/// Resolves a link `href` against a page URL and the document's `<base href>`,
/// keeping only web schemes.
#[must_use]
pub fn resolve_link_with_base(page_url: &str, base_href: Option<&str>, href: &str) -> Option<Url> {
    let page = Url::parse(page_url).ok()?;
    let base = match base_href {
        Some(href) if !href.trim().is_empty() => page.join(href.trim()).ok()?,
        _ => page,
    };
    let resolved = base.join(href).ok()?;
    matches!(resolved.scheme(), "http" | "https").then_some(resolved)
}

/// Normalizes address-bar input into an absolute HTTP(S) URL.
#[must_use]
pub fn address_to_url(input: &str) -> Option<Url> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    let has_scheme =
        input.split(['/', '?', '#']).next().is_some_and(|segment| segment.contains(':'));
    let candidate = if has_scheme { input.to_owned() } else { format!("https://{input}") };
    let url = Url::parse(&candidate).ok()?;
    matches!(url.scheme(), "http" | "https").then_some(url)
}

#[cfg(test)]
mod tests {
    use super::{History, address_to_url, resolve_link_with_base};
    use url::Url;

    fn url(value: &str) -> Url {
        Url::parse(value).expect("absolute URL")
    }

    #[test]
    fn push_truncates_forward_entries() {
        let mut history = History::new(url("https://example.test/one"));
        history.push(url("https://example.test/two"));
        history.push(url("https://example.test/three"));
        assert!(history.back());
        assert_eq!(history.current().as_str(), "https://example.test/two");

        history.push(url("https://example.test/four"));
        assert!(!history.can_forward());
        assert_eq!(history.current().as_str(), "https://example.test/four");
    }

    #[test]
    fn back_and_forward_are_bounded() {
        let mut history = History::new(url("https://example.test/one"));
        assert!(!history.back());
        assert!(!history.forward());
        history.push(url("https://example.test/two"));
        assert!(history.back());
        assert!(history.forward());
        assert!(!history.forward());
    }

    #[test]
    fn duplicate_push_does_not_extend_history() {
        let mut history = History::new(url("https://example.test/one"));
        history.push(url("https://example.test/one"));
        assert!(!history.can_back());
    }

    #[test]
    fn resolves_relative_and_absolute_links() {
        let page = "https://example.test/dir/index.html";
        assert_eq!(
            resolve_link_with_base(page, None, "/root").map(|url| url.to_string()),
            Some("https://example.test/root".to_owned())
        );
        assert_eq!(
            resolve_link_with_base(page, None, "next.html").map(|url| url.to_string()),
            Some("https://example.test/dir/next.html".to_owned())
        );
        assert_eq!(
            resolve_link_with_base(page, None, "https://other.test/x").map(|url| url.to_string()),
            Some("https://other.test/x".to_owned())
        );
    }

    #[test]
    fn rejects_non_web_link_schemes() {
        assert_eq!(resolve_link_with_base("https://example.test/", None, "mailto:a@b.test"), None);
        assert_eq!(
            resolve_link_with_base("https://example.test/", None, "javascript:alert(1)"),
            None
        );
    }

    #[test]
    fn resolves_links_against_the_document_base() {
        let page = "https://example.test/dir/page.html";
        assert_eq!(
            resolve_link_with_base(page, Some("/assets/"), "style.css").map(|url| url.to_string()),
            Some("https://example.test/assets/style.css".to_owned())
        );
        assert_eq!(
            resolve_link_with_base(page, Some("https://cdn.test/"), "app.js")
                .map(|url| url.to_string()),
            Some("https://cdn.test/app.js".to_owned())
        );
        assert_eq!(
            resolve_link_with_base(page, None, "next.html").map(|url| url.to_string()),
            Some("https://example.test/dir/next.html".to_owned())
        );
        assert_eq!(resolve_link_with_base(page, Some("mailto:x@y.z"), "a.css"), None);
    }

    #[test]
    fn address_input_defaults_to_https_but_keeps_explicit_schemes() {
        assert_eq!(
            address_to_url("example.test/path").map(|url| url.to_string()),
            Some("https://example.test/path".to_owned())
        );
        assert_eq!(
            address_to_url("http://example.test/").map(|url| url.to_string()),
            Some("http://example.test/".to_owned())
        );
        assert_eq!(address_to_url("  "), None);
        assert_eq!(address_to_url("mailto:a@b.test"), None);
    }
}
