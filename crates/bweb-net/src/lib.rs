//! URL validation and the mandatory admission gate for network transports.
//!
//! A transport must call NetworkPolicy::authorize before opening a connection.
//! This crate intentionally does not yet download response data.

use bweb_shield::{Decision, ResourceType, Shield, ShieldError};
use url::Url;

/// Errors returned while validating a web URL.
#[derive(Debug)]
pub enum WebUrlError {
    Parse(url::ParseError),
    UnsupportedScheme(String),
}

impl From<url::ParseError> for WebUrlError {
    fn from(error: url::ParseError) -> Self {
        Self::Parse(error)
    }
}

/// Parses an absolute HTTP or HTTPS URL.
pub fn parse_web_url(input: &str) -> Result<Url, WebUrlError> {
    let url = Url::parse(input)?;
    match url.scheme() {
        "http" | "https" => Ok(url),
        scheme => Err(WebUrlError::UnsupportedScheme(scheme.to_owned())),
    }
}

/// Mandatory privacy gate for future HTTP/TLS transports.
pub struct NetworkPolicy {
    shield: Shield,
}

impl NetworkPolicy {
    /// Creates a network policy using the supplied compiled filter engine.
    #[must_use]
    pub fn new(shield: Shield) -> Self {
        Self { shield }
    }

    /// Checks a resource before the transport is permitted to download it.
    pub fn authorize(
        &self,
        resource_url: &str,
        page_url: &str,
        resource_type: ResourceType,
        method: &str,
    ) -> Result<Decision, ShieldError> {
        self.shield.check(resource_url, page_url, resource_type, method)
    }
}

#[cfg(test)]
mod tests {
    use super::{NetworkPolicy, WebUrlError, parse_web_url};
    use bweb_shield::{Decision, ResourceType, Shield};

    #[test]
    fn accepts_http_and_https() {
        assert!(parse_web_url("https://example.com").is_ok());
        assert!(parse_web_url("http://example.com").is_ok());
    }

    #[test]
    fn rejects_non_web_schemes() {
        assert!(matches!(
            parse_web_url("file:///etc/passwd"),
            Err(WebUrlError::UnsupportedScheme(_))
        ));
    }

    #[test]
    fn policy_blocks_before_transport() {
        let policy = NetworkPolicy::new(Shield::from_rules(["||ads.example^"]));
        let decision = policy
            .authorize(
                "https://ads.example/ad.js",
                "https://news.example/",
                ResourceType::Script,
                "GET",
            )
            .expect("valid request");
        assert_eq!(decision, Decision::Block);
    }
}
