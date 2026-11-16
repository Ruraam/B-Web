//! URL and networking foundation.
//!
//! All future network transports must consult B-Web Shield before opening a connection.

use url::Url;

/// Errors returned while validating a web URL.
#[derive(Debug)]
pub enum WebUrlError {
    /// The input could not be parsed as a URL.
    Parse(url::ParseError),
    /// The URL uses a scheme other than HTTP or HTTPS.
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

#[cfg(test)]
mod tests {
    use super::{parse_web_url, WebUrlError};

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
}
