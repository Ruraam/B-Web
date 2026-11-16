//! Privacy filtering integration point.
//!
//! This crate embeds Brave's Rust adblock library. Rule-list loading and request matching
//! will be implemented before the networking crate is allowed to fetch page resources.

/// Upstream engine used by B-Web Shield.
pub const FILTER_ENGINE: &str = "Brave adblock-rust";
