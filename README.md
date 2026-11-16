# B-Web

**B-Web** is an experimental, Rust-first browser project. Its long-term goal is an independently developed web engine: HTML/DOM, CSS/layout, JavaScript runtime, networking, rendering and privacy filtering.

The project deliberately does **not** embed Chromium, WebKit, Gecko or Wry. The first commits establish a clean, compilable workspace; they do not yet constitute a standards-compliant browser.

## Workspace

| Crate | Responsibility |
| --- | --- |
| bweb-core | Shared engine types and version information |
| bweb-html | HTML document and DOM foundation |
| bweb-css | CSS model and style foundation |
| bweb-js | Reserved foundation for the future Rust JavaScript runtime |
| bweb-net | URL and networking foundation |
| bweb-shield | Brave's Rust adblock engine integration and bounded local filter-list cache |
| bweb-filterlists | HTTPS-only, bounded downloader for the built-in EasyList and EasyPrivacy sources |
| bweb-renderer | Rendering abstraction |
| bweb-shell | Initial executable |

## Build

Requires Rust 1.85 or newer.

```sh
cargo build --release
cargo test --workspace
cargo run --release -p bweb-shell
```

## Filter-list downloads

The bweb-filterlists crate retrieves only the built-in EasyList and EasyPrivacy HTTPS endpoints. It uses Rustls, a five-second connection timeout, a twenty-second total request timeout, HTTPS-only redirects (maximum five), and an 8 MiB streaming response limit per list. Responses must be non-empty UTF-8 text and contain the expected EasyList or EasyPrivacy title before they are written to the cache.

The downloader sends a fixed B-Web user-agent and no machine ID, installation UUID, or other persistent device identifier. Downloading these two built-in lists is a bootstrap operation and therefore does not pass through the ad-block rules being downloaded. The cache replaces each file atomically; the pair is downloaded before either cache file is changed, but a filesystem failure during the second replacement can leave the first list refreshed.

Example integration:

```rust
use bweb_filterlists::FilterListDownloader;
use bweb_shield::FilterListCache;

let cache = FilterListCache::new("/path/to/b-web/filter-lists");
let downloader = FilterListDownloader::new()?;
let update = downloader.update_cache(&cache)?;
let shield = cache.load_shield()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

This is a library API; automatic scheduling and shell commands for refreshing lists are not wired up yet.

## Current status

This is an incremental engine foundation. HTML parsing, CSS layout, JavaScript execution, general-purpose HTTP/TLS page transport, and GPU rendering remain separate milestones. The filter-list downloader and cache are implemented as standalone components, but the browser shell does not yet invoke them automatically.

## Design principles

- Rust for the browser's own components.
- No embedded third-party browser engine.
- Network requests must pass through the privacy filter before bytes are downloaded.
- Small, independently testable crates.
- Standards support grows incrementally, with explicit compatibility limits.
