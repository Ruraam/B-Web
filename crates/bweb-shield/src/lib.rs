//! In-memory network filtering powered by Brave's adblock-rust engine.
//!
//! The networking layer must call Shield::check before opening a socket or
//! downloading response bytes. Filter lists are compiled once and queried for
//! every outgoing resource request.

mod filter_lists;

pub use filter_lists::{
    FilterListCache, FilterListError, FilterListId, MAX_FILTER_LIST_BYTES,
};

use adblock::{
    Engine,
    lists::{FilterSet, ParseOptions, RuleTypes},
    request::Request,
};
use std::{error::Error, fmt};

/// Name of the network filtering engine used by B-Web Shield.
pub const FILTER_ENGINE: &str = "Brave adblock-rust";

/// Resource category passed to the filter engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceType {
    Document,
    Subdocument,
    Stylesheet,
    Script,
    Image,
    Font,
    Media,
    XmlHttpRequest,
    Fetch,
    WebSocket,
    Other,
}

impl ResourceType {
    const fn as_filter_type(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Subdocument => "subdocument",
            Self::Stylesheet => "stylesheet",
            Self::Script => "script",
            Self::Image => "image",
            Self::Font => "font",
            Self::Media => "media",
            Self::XmlHttpRequest => "xmlhttprequest",
            Self::Fetch => "fetch",
            Self::WebSocket => "websocket",
            Self::Other => "other",
        }
    }
}

/// Result of a request filtering decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The request may proceed to the transport.
    Allow,
    /// A filter matched; the transport must not download this resource.
    Block,
}

/// Errors raised while constructing a filter request.
#[derive(Debug)]
pub enum ShieldError {
    InvalidRequest(adblock::request::RequestError),
}

impl fmt::Display for ShieldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(error) => write!(f, "invalid adblock request: {error}"),
        }
    }
}

impl Error for ShieldError {}

impl From<adblock::request::RequestError> for ShieldError {
    fn from(error: adblock::request::RequestError) -> Self {
        Self::InvalidRequest(error)
    }
}

/// Compiled adblocking rules held in memory.
pub struct Shield {
    engine: Engine,
}

impl Shield {
    /// Compiles complete ABP/uBlock-compatible list contents into memory.
    #[must_use]
    pub fn from_filter_lists<'a>(lists: impl IntoIterator<Item = &'a str>) -> Self {
        let mut filter_set = FilterSet::new(false);
        let options =
            ParseOptions { rule_types: RuleTypes::NetworkOnly, ..ParseOptions::default() };

        for list in lists {
            filter_set.add_filter_list(list.to_owned(), options);
        }

        Self { engine: Engine::new_with_filter_set(filter_set) }
    }

    /// Compiles individual ABP/uBlock network rules.
    #[must_use]
    pub fn from_rules<'a>(rules: impl IntoIterator<Item = &'a str>) -> Self {
        let list = rules.into_iter().collect::<Vec<_>>().join("\n");
        Self::from_filter_lists([list.as_str()])
    }

    /// Creates an empty filter engine.
    #[must_use]
    pub fn empty() -> Self {
        Self::from_filter_lists(std::iter::empty())
    }

    /// Checks a resource request. Call this before any network I/O.
    pub fn check(
        &self,
        resource_url: &str,
        page_url: &str,
        resource_type: ResourceType,
        method: &str,
    ) -> Result<Decision, ShieldError> {
        let request = Request::new(resource_url, page_url, resource_type.as_filter_type(), method)?;
        let result = self.engine.check_network_request(&request);

        Ok(if result.should_block() { Decision::Block } else { Decision::Allow })
    }
}

impl Default for Shield {
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{Decision, ResourceType, Shield};

    #[test]
    fn blocks_matching_script_request() {
        let shield = Shield::from_rules(["||ads.example^"]);
        let result = shield
            .check(
                "https://ads.example/banner.js",
                "https://news.example/",
                ResourceType::Script,
                "GET",
            )
            .expect("valid request");
        assert_eq!(result, Decision::Block);
    }

    #[test]
    fn allows_exception_rule() {
        let shield = Shield::from_rules(["||ads.example^", "@@||ads.example^$script"]);
        let result = shield
            .check(
                "https://ads.example/banner.js",
                "https://news.example/",
                ResourceType::Script,
                "GET",
            )
            .expect("valid request");
        assert_eq!(result, Decision::Allow);
    }

    #[test]
    fn allows_unmatched_request() {
        let shield = Shield::from_rules(["||ads.example^"]);
        let result = shield
            .check(
                "https://cdn.example/article.js",
                "https://news.example/",
                ResourceType::Script,
                "GET",
            )
            .expect("valid request");
        assert_eq!(result, Decision::Allow);
    }
}
