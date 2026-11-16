//! URL validation and the mandatory admission gate for network transports.
//!
//! A transport must call NetworkPolicy::authorize_web_request before opening a
//! connection or downloading response bytes. This crate intentionally does not
//! yet implement an HTTP/TLS transport.

use bweb_shield::{Decision, ResourceType, Shield, ShieldError};
use std::{error::Error, fmt};
use url::Url;

/// Errors returned while validating a web URL.
#[derive(Debug)]
pub enum WebUrlError {
    Parse(url::ParseError),
    UnsupportedScheme(String),
}

impl fmt::Display for WebUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(error) => write!(f, "invalid web URL: {error}"),
            Self::UnsupportedScheme(scheme) => write!(f, "unsupported URL scheme: {scheme}"),
        }
    }
}

impl Error for WebUrlError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Parse(error) => Some(error),
            Self::UnsupportedScheme(_) => None,
        }
    }
}

impl From<url::ParseError> for WebUrlError {
    fn from(error: url::ParseError) -> Self {
        Self::Parse(error)
    }
}

/// Errors returned by the network admission policy.
#[derive(Debug)]
pub enum NetworkPolicyError {
    InvalidUrl(WebUrlError),
    Shield(ShieldError),
}

impl fmt::Display for NetworkPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl(error) => write!(f, "network request rejected: {error}"),
            Self::Shield(error) => write!(f, "network request could not be checked: {error}"),
        }
    }
}

impl Error for NetworkPolicyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidUrl(error) => Some(error),
            Self::Shield(error) => Some(error),
        }
    }
}

impl From<WebUrlError> for NetworkPolicyError {
    fn from(error: WebUrlError) -> Self {
        Self::InvalidUrl(error)
    }
}

impl From<ShieldError> for NetworkPolicyError {
    fn from(error: ShieldError) -> Self {
        Self::Shield(error)
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
    ///
    /// This low-level method assumes the caller has already validated both URLs.
    pub fn authorize(
        &self,
        resource_url: &str,
        page_url: &str,
        resource_type: ResourceType,
        method: &str,
    ) -> Result<Decision, ShieldError> {
        self.shield.check(resource_url, page_url, resource_type, method)
    }

    /// Validates both URLs and checks the request against Shield.
    ///
    /// Transports should use this method rather than opening a connection
    /// directly. A Block decision means no socket or response-body I/O may
    /// occur for the resource.
    pub fn authorize_web_request(
        &self,
        resource_url: &str,
        page_url: &str,
        resource_type: ResourceType,
        method: &str,
    ) -> Result<Decision, NetworkPolicyError> {
        let resource_url = parse_web_url(resource_url)?;
        let page_url = parse_web_url(page_url)?;

        self.authorize(resource_url.as_str(), page_url.as_str(), resource_type, method)
            .map_err(NetworkPolicyError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::{NetworkPolicy, NetworkPolicyError, WebUrlError, parse_web_url};
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
    fn policy_blocks_matching_request() {
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

    #[test]
    fn web_request_gate_rejects_unsupported_resource_scheme() {
        let policy = NetworkPolicy::new(Shield::empty());
        let result = policy.authorize_web_request(
            "file:///etc/passwd",
            "https://news.example/",
            ResourceType::Document,
            "GET",
        );
        assert!(matches!(
            result,
            Err(NetworkPolicyError::InvalidUrl(WebUrlError::UnsupportedScheme(_)))
        ));
    }

    #[test]
    fn web_request_gate_checks_shield_after_url_validation() {
        let policy = NetworkPolicy::new(Shield::from_rules(["||ads.example^"]));
        let decision = policy
            .authorize_web_request(
                "https://ads.example/ad.js",
                "https://news.example/",
                ResourceType::Script,
                "GET",
            )
            .expect("valid web request");
        assert_eq!(decision, Decision::Block);
    }
}
