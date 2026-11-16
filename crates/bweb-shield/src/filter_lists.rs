//! Local cache for trusted filter-list sources.
//!
//! Downloading is intentionally handled by a separate networking component.
//! This module only validates, stores and loads UTF-8 list contents.

use crate::Shield;
use std::{
    error::Error,
    fmt,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// Maximum accepted size for one cached filter list (8 MiB).
pub const MAX_FILTER_LIST_BYTES: usize = 8 * 1024 * 1024;

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A filter list supported by the initial B-Web Shield configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterListId {
    EasyList,
    EasyPrivacy,
}

impl FilterListId {
    /// Official HTTPS source for this list.
    #[must_use]
    pub const fn source_url(self) -> &'static str {
        match self {
            Self::EasyList => "https://easylist.to/easylist/easylist.txt",
            Self::EasyPrivacy => "https://easylist.to/easylist/easyprivacy.txt",
        }
    }

    const fn file_name(self) -> &'static str {
        match self {
            Self::EasyList => "easylist.txt",
            Self::EasyPrivacy => "easyprivacy.txt",
        }
    }
}

/// Errors returned by the local filter-list cache.
#[derive(Debug)]
pub enum FilterListError {
    Io(std::io::Error),
    TooLarge { size: usize, limit: usize },
    EmptyList,
    InvalidUtf8,
    NoCachedLists,
}

impl fmt::Display for FilterListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "filter-list cache I/O error: {error}"),
            Self::TooLarge { size, limit } => {
                write!(f, "filter list is too large ({size} bytes; limit is {limit})")
            }
            Self::EmptyList => f.write_str("refusing to cache an empty filter list"),
            Self::InvalidUtf8 => f.write_str("filter list is not valid UTF-8"),
            Self::NoCachedLists => f.write_str("no supported filter lists are cached"),
        }
    }
}

impl Error for FilterListError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for FilterListError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Filesystem-backed cache for EasyList and EasyPrivacy.
#[derive(Debug, Clone)]
pub struct FilterListCache {
    directory: PathBuf,
}

impl FilterListCache {
    /// Creates a cache rooted at the supplied application data directory.
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self { directory: directory.into() }
    }

    /// Stores a downloaded list atomically after checking its size and encoding.
    ///
    /// The caller is responsible for HTTPS-only transport, timeouts and download
    /// limits before passing response bytes to this method.
    pub fn store(&self, id: FilterListId, contents: &str) -> Result<(), FilterListError> {
        let bytes = contents.as_bytes();
        validate_list(bytes)?;

        fs::create_dir_all(&self.directory)?;
        let destination = self.directory.join(id.file_name());
        let (temporary_path, mut temporary_file) = self.create_temporary_file()?;

        let write_result = (|| -> Result<(), std::io::Error> {
            temporary_file.write_all(bytes)?;
            temporary_file.sync_all()?;
            drop(temporary_file);
            fs::rename(&temporary_path, &destination)?;
            Ok(())
        })();

        if write_result.is_err() {
            let _ = fs::remove_file(&temporary_path);
        }
        write_result.map_err(FilterListError::Io)
    }

    /// Loads all supported cached lists and compiles a new Shield engine.
    ///
    /// Missing individual lists are allowed; an entirely empty cache is not.
    pub fn load_shield(&self) -> Result<Shield, FilterListError> {
        let mut contents = Vec::new();

        for id in [FilterListId::EasyList, FilterListId::EasyPrivacy] {
            let path = self.directory.join(id.file_name());
            if !path.exists() {
                continue;
            }

            let bytes = fs::read(path)?;
            validate_list(&bytes)?;
            let list = String::from_utf8(bytes).map_err(|_| FilterListError::InvalidUtf8)?;
            contents.push(list);
        }

        if contents.is_empty() {
            return Err(FilterListError::NoCachedLists);
        }

        Ok(Shield::from_filter_lists(contents.iter().map(String::as_str)))
    }

    fn create_temporary_file(&self) -> Result<(PathBuf, fs::File), FilterListError> {
        loop {
            let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = self.directory.join(format!(
                ".filter-list-{}-{sequence}.tmp",
                std::process::id()
            ));

            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok((path, file)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(FilterListError::Io(error)),
            }
        }
    }

    /// Returns the cache directory.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }
}

fn validate_list(bytes: &[u8]) -> Result<(), FilterListError> {
    if bytes.len() > MAX_FILTER_LIST_BYTES {
        return Err(FilterListError::TooLarge {
            size: bytes.len(),
            limit: MAX_FILTER_LIST_BYTES,
        });
    }
    if bytes.is_empty() || bytes.iter().all(u8::is_ascii_whitespace) {
        return Err(FilterListError::EmptyList);
    }
    std::str::from_utf8(bytes).map_err(|_| FilterListError::InvalidUtf8)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{FilterListCache, FilterListId};
    use std::{fs, path::PathBuf, time::{SystemTime, UNIX_EPOCH}};

    fn temporary_directory() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("bweb-filter-cache-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn stores_and_loads_a_filter_list() {
        let directory = temporary_directory();
        let cache = FilterListCache::new(&directory);

        cache.store(FilterListId::EasyList, "||ads.example^").expect("store list");
        let shield = cache.load_shield().expect("load cached list");
        let decision = shield
            .check(
                "https://ads.example/ad.js",
                "https://news.example/",
                crate::ResourceType::Script,
                "GET",
            )
            .expect("valid request");

        assert_eq!(decision, crate::Decision::Block);
        fs::remove_dir_all(directory).expect("remove test cache");
    }

    #[test]
    fn rejects_empty_lists() {
        let directory = temporary_directory();
        let cache = FilterListCache::new(&directory);

        assert!(cache.store(FilterListId::EasyPrivacy, " \n ").is_err());
        assert!(!directory.exists());
    }
}
