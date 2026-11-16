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
| bweb-net | Rustls HTTP/HTTPS transport with mandatory Shield admission and privacy headers |
| bweb-shield | Brave's Rust adblock engine integration and bounded local filter-list cache |
| bweb-filterlists | HTTPS-only, bounded downloader for the built-in EasyList and EasyPrivacy sources |
| bweb-renderer | Rendering abstraction |
| bweb-shell | Initial executable and explicit filter-list update command |

## Build

Requires Rust 1.85 or newer.

```sh
cargo build --release
cargo test --workspace
cargo run --release -p bweb-shell
```

## Filter-list downloads and activation

The shell exposes an explicit command to download the built-in EasyList and EasyPrivacy sources:

```sh
b-web update-filters
```

The command stores lists in `$XDG_CACHE_HOME/b-web/filter-lists` when `XDG_CACHE_HOME` is an absolute path, or falls back to `$HOME/.cache/b-web/filter-lists`. It does not require root privileges. Running `b-web` without a command performs no network access and does not update the lists automatically.

On a normal `b-web` startup, the shell loads the cached lists and compiles them into an in-memory Shield engine. It reports whether Shield is ready or inactive because no lists have been cached. Invalid or unreadable cached lists are reported as errors rather than silently ignored. The compiled engine remains alive for the shell invocation.

The `bweb-filterlists` crate retrieves only the built-in EasyList and EasyPrivacy HTTPS endpoints. It uses Rustls, a five-second connection timeout, a twenty-second total request timeout, HTTPS-only redirects (maximum five), and an 8 MiB streaming response limit per list. Responses must be non-empty UTF-8 text and contain the expected EasyList or EasyPrivacy title before they are written to the cache.

The downloader sends a fixed B-Web user-agent and no machine ID, installation UUID, or other persistent device identifier. Downloading these two built-in lists is a bootstrap operation and therefore does not pass through the ad-block rules being downloaded. The cache replaces each file atomically; the pair is downloaded before either cache file is changed, but a filesystem failure during the second replacement can leave the first list refreshed.

## Network admission policy

`bweb-net::HttpTransport` is the HTTP/HTTPS client boundary. It is built with reqwest’s Rustls TLS backend (no OpenSSL feature) and takes ownership of a `NetworkPolicy`. Its underlying client and policy are private: downstream code cannot extract the client or submit requests outside the guarded `execute` method. The shell constructs this transport at startup using locally compiled Shield rules (or an empty ruleset when no lists are cached).

Every request is checked by `NetworkPolicy::authorize_web_request` before the transport calls `send`. Automatic redirects are disabled in reqwest; the transport follows supported redirect statuses itself and revalidates each destination through Shield before opening the next connection. Cross-origin redirects strip Authorization, Proxy-Authorization, and Cookie headers. Redirects are capped at ten, connection and total request timeouts are finite, and response bodies are returned as streams rather than buffered wholesale.

Every outgoing request forcibly includes `Sec-GPC: 1` and `DNT: 1`, overriding caller-supplied values. The transport rejects non-HTTP(S) URLs and returns a distinct blocked error without opening a connection. The lower-level `NetworkPolicy::authorize` remains available for callers that already validate URLs, but the physical HTTP client is not exposed to those callers.

**Privacy boundary:** startup compiles cached rules locally and performs no network request. Filter-list downloads remain explicit through `b-web update-filters`.

## Current status

This is an incremental engine foundation. HTML parsing, CSS layout, JavaScript execution, browser page lifecycle integration and GPU rendering remain separate milestones. The Rustls HTTP/HTTPS transport is implemented and constructed by the shell, but no rendered page or DOM resource loader calls it yet.

## Design principles

- Rust for the browser's own components.
- No embedded third-party browser engine.
- Network requests must pass through the privacy filter before bytes are downloaded.
- Small, independently testable crates.
- Standards support grows incrementally, with explicit compatibility limits.
