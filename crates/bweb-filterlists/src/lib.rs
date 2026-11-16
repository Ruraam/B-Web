//! Secure downloader for B-Web's built-in EasyList and EasyPrivacy sources.
//!
//! Only the two HTTPS URLs defined by FilterListId can be downloaded. The
//! client uses bounded streaming reads, finite timeouts and HTTPS-only
//! redirects. It sends no machine identifier or installation ID.
//!
//! These downloads are a bootstrap operation and are not passed through the
//! ad-block engine whose rules they are intended to retrieve.

use bweb_shield::{FilterListCache, FilterListError, FilterListId, MAX_FILTER_LIST_BYTES};
use reqwest::{
    blocking::{Client, Response},
    redirect::{Attempt, Policy},
    StatusCode,
};
use std::{
    error::Error,
    fmt,
    io::{self, Read},
    time::Duration,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_REDIRECTS: usize = 5;

/// Errors returned while retrieving or caching a built-in filter list.
#[derive(Debug)]
pub enum FilterListDownloadError {
    Client(reqwest::Error),
    HttpStatus(StatusCode),
    TooLarge { limit: usize },
    EmptyResponse,
    InvalidUtf8,
    InvalidListFormat,
    Cache(FilterListError),
    Io(io::Error),
}

impl fmt::Display for FilterListDownloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(error) => write!(f, "filter-list request failed: {error}"),
            Self::HttpStatus(status) => write!(f, "filter-list server returned HTTP {status}"),
            Self::TooLarge { limit } => {
                write!(f, "filter-list response exceeds the {limit}-byte limit")
            }
            Self::EmptyResponse => f.write_str("filter-list response is empty"),
            Self::InvalidUtf8 => f.write_str("filter-list response is not valid UTF-8"),
            Self::InvalidListFormat => {
                f.write_str("response does not identify itself as the requested filter list")
            }
            Self::Cache(error) => write!(f, "could not update filter-list cache: {error}"),
            Self::Io(error) => write!(f, "could not read filter-list response: {error}"),
        }
    }
}

impl Error for FilterListDownloadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Client(error) => Some(error),
            Self::Cache(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::HttpStatus(_)
            | Self::TooLarge { .. }
            | Self::EmptyResponse
            | Self::InvalidUtf8
            | Self::InvalidListFormat => None,
        }
    }
}

impl From<reqwest::Error> for FilterListDownloadError {
    fn from(error: reqwest::Error) -> Self {
        Self::Client(error)
    }
}

impl From<FilterListError> for FilterListDownloadError {
    fn from(error: FilterListError) -> Self {
        Self::Cache(error)
    }
}

impl From<io::Error> for FilterListDownloadError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Summary of a successful cache refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilterListUpdate {
    /// Number of bytes downloaded for EasyList.
    pub easylist_bytes: usize,
    /// Number of bytes downloaded for EasyPrivacy.
    pub easyprivacy_bytes: usize,
}

/// Downloads the two built-in lists with a bounded, HTTPS-only client.
pub struct FilterListDownloader {
    client: Client,
}

impl FilterListDownloader {
    /// Builds a client with finite timeouts and restricted redirects.
    pub fn new() -> Result<Self, FilterListDownloadError> {
        let client = Client::builder()
            .https_only(true)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(Policy::custom(redirect_https_only))
            .user_agent(concat!("B-Web/", env!("CARGO_PKG_VERSION")))
            .build()?;

        Ok(Self { client })
    }

    /// Downloads and validates one built-in list without modifying the cache.
    pub fn download_list(
        &self,
        id: FilterListId,
    ) -> Result<String, FilterListDownloadError> {
        let response = self.client.get(id.source_url()).send()?;
        let response = ensure_success(response)?;
        let contents = read_response_bounded(response)?;
        let contents =
            String::from_utf8(contents).map_err(|_| FilterListDownloadError::InvalidUtf8)?;
        validate_list_identity(id, &contents)?;
        Ok(contents)
    }

    /// Downloads both lists before writing either one to the local cache.
    ///
    /// Each cache file is replaced atomically by FilterListCache::store.
    /// A filesystem failure during the second replacement can still leave the
    /// first file updated; neither individual file is ever half-written.
    pub fn update_cache(
        &self,
        cache: &FilterListCache,
    ) -> Result<FilterListUpdate, FilterListDownloadError> {
        let easylist = self.download_list(FilterListId::EasyList)?;
        let easyprivacy = self.download_list(FilterListId::EasyPrivacy)?;

        cache.store(FilterListId::EasyList, &easylist)?;
        cache.store(FilterListId::EasyPrivacy, &easyprivacy)?;

        Ok(FilterListUpdate {
            easylist_bytes: easylist.len(),
            easyprivacy_bytes: easyprivacy.len(),
        })
    }
}

fn redirect_https_only(attempt: Attempt<'_>) -> Attempt<'_> {
    if attempt.previous().len() >= MAX_REDIRECTS || attempt.url().scheme() != "https" {
        attempt.stop()
    } else {
        attempt.follow()
    }
}

fn ensure_success(response: Response) -> Result<Response, FilterListDownloadError> {
    if !response.status().is_success() {
        return Err(FilterListDownloadError::HttpStatus(response.status()));
    }

    if response.content_length().is_some_and(|length| length > MAX_FILTER_LIST_BYTES as u64) {
        return Err(FilterListDownloadError::TooLarge {
            limit: MAX_FILTER_LIST_BYTES,
        });
    }

    Ok(response)
}

fn read_response_bounded(response: Response) -> Result<Vec<u8>, FilterListDownloadError> {
    read_bounded(response, MAX_FILTER_LIST_BYTES)
}

fn read_bounded(
    reader: impl Read,
    limit: usize,
) -> Result<Vec<u8>, FilterListDownloadError> {
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;

    if bytes.len() > limit {
        return Err(FilterListDownloadError::TooLarge { limit });
    }
    if bytes.is_empty() || bytes.iter().all(u8::is_ascii_whitespace) {
        return Err(FilterListDownloadError::EmptyResponse);
    }

    std::str::from_utf8(&bytes).map_err(|_| FilterListDownloadError::InvalidUtf8)?;
    Ok(bytes)
}

fn validate_list_identity(
    id: FilterListId,
    contents: &str,
) -> Result<(), FilterListDownloadError> {
    let expected_title = match id {
        FilterListId::EasyList => "! Title: EasyList",
        FilterListId::EasyPrivacy => "! Title: EasyPrivacy",
    };

    if contents.lines().any(|line| line.trim().eq_ignore_ascii_case(expected_title)) {
        Ok(())
    } else {
        Err(FilterListDownloadError::InvalidListFormat)
    }
}

#[cfg(test)]
mod tests {
    use super::{FilterListDownloadError, read_bounded, validate_list_identity};
    use bweb_shield::FilterListId;
    use std::io::Cursor;

    #[test]
    fn bounded_reader_accepts_content_at_the_limit() {
        let bytes = read_bounded(Cursor::new(b"abcd"), 4).expect("within limit");
        assert_eq!(bytes, b"abcd");
    }

    #[test]
    fn bounded_reader_rejects_content_over_the_limit() {
        let error = read_bounded(Cursor::new(b"abcde"), 4).expect_err("over limit");
        assert!(matches!(error, FilterListDownloadError::TooLarge { limit: 4 }));
    }

    #[test]
    fn bounded_reader_rejects_empty_content() {
        let error = read_bounded(Cursor::new(b" \n "), 4).expect_err("empty content");
        assert!(matches!(error, FilterListDownloadError::EmptyResponse));
    }

    #[test]
    fn bounded_reader_rejects_invalid_utf8() {
        let error = read_bounded(Cursor::new([0xff, 0xfe]), 4).expect_err("invalid UTF-8");
        assert!(matches!(error, FilterListDownloadError::InvalidUtf8));
    }

    #[test]
    fn validates_the_expected_list_identity() {
        assert!(
            validate_list_identity(FilterListId::EasyList, "! Title: EasyList\n||ads.example^")
                .is_ok()
        );
        assert!(
            validate_list_identity(
                FilterListId::EasyPrivacy,
                "! Title: EasyPrivacy\n||tracker.example^"
            )
            .is_ok()
        );
        assert!(matches!(
            validate_list_identity(FilterListId::EasyList, "<html>temporary error</html>"),
            Err(FilterListDownloadError::InvalidListFormat)
        ));
    }
}
