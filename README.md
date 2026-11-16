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
| bweb-net | HTTP(S) URL validation and Shield-backed network admission policy |
| bweb-shield | Brave's Rust adblock engine integration and bounded local filter-list cache |
| bweb-filterlists | HTTPS-only, bounded downloader for the built-in EasyList and EasyPrivacy sources |
| bweb-renderer | Rendering abstraction |
| bweb-shell | Initial executable and explicit filter-list update command |

## Build

Requires Rust 1.85 or newer.

\`\`\`sh
cargo build --release
cargo test --workspace
cargo run --release -p bweb-shell
\`\`\`

## Filter-list downloads and activation

The shell exposes an explicit command to download the built-in EasyList and EasyPrivacy sources:

\`\`\`sh
b-web update-filters
\`\`\`

The command stores lists in \`$XDG_CACHE_HOME/b-web/filter-lists\` when \`XDG_CACHE_HOME\` is an absolute path, or falls back to \`$HOME/.cache/b-web/filter-lists\`. It does not require root privileges. Running \`b-web\` without a command performs no network access and does not update the lists automatically.

On a normal \`b-web\` startup, the shell loads the cached lists and compiles them into an in-memory Shield engine. It reports whether Shield is ready or inactive because no lists have been cached. Invalid or unreadable cached lists are reported as errors rather than silently ignored. The compiled engine remains alive for the shell invocation.

The \`bweb-filterlists\` crate retrieves only the built-in EasyList and EasyPrivacy HTTPS endpoints. It uses Rustls, a five-second connection timeout, a twenty-second total request timeout, HTTPS-only redirects (maximum five), and an 8 MiB streaming response limit per list. Responses must be non-empty UTF-8 text and contain the expected EasyList or EasyPrivacy title before they are written to the cache.

The downloader sends a fixed B-Web user-agent and no machine ID, installation UUID, or other persistent device identifier. Downloading these two built-in lists is a bootstrap operation and therefore does not pass through the ad-block rules being downloaded. The cache replaces each file atomically; the pair is downloaded before either cache file is changed, but a filesystem failure during the second replacement can leave the first list refreshed.

## Network admission policy

\`bweb-net::NetworkPolicy\` owns a compiled Shield instance and exposes \`authorize_web_request\` as the high-level admission check for future transports. The method validates that both the resource URL and page URL are absolute HTTP(S) URLs, then asks Shield whether the resource is allowed or blocked. A blocked decision must stop the request before socket creation or response-body reads. URL and Shield failures are returned as explicit errors.

The lower-level \`authorize\` method remains available for callers that already validate their URLs. Future HTTP/TLS transports must use the high-level gate before performing network I/O. The crate currently provides policy and validation only; it does not yet implement a page transport, and the shell has not yet connected a page lifecycle to this policy.

**Privacy boundary:** startup compiles cached rules locally and performs no network request. Filter-list downloads remain explicit through \`b-web update-filters\`.

## Current status

This is an incremental engine foundation. HTML parsing, CSS layout, JavaScript execution, general-purpose HTTP/TLS page transport, and GPU rendering remain separate milestones. The filter-list downloader, cache, startup Shield initialization, URL validation, and Shield-backed network admission policy are implemented as independent components.

## Design principles

- Rust for the browser's own components.
- No embedded third-party browser engine.
- Network requests must pass through the privacy filter before bytes are downloaded.
- Small, independently testable crates.
- Standards support grows incrementally, with explicit compatibility limits.
