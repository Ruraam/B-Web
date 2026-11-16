# B-Web

**B-Web** is an experimental, Rust-first browser project. Its long-term goal is an independently developed web engine: HTML/DOM, CSS/layout, JavaScript runtime, networking, rendering and privacy filtering.

The project deliberately does **not** embed Chromium, WebKit, Gecko or Wry. The first commits establish a clean, compilable workspace; they do not yet constitute a standards-compliant browser.

## Workspace

| Crate | Responsibility |
| --- | --- |
| `bweb-core` | Shared engine types and version information |
| `bweb-html` | HTML document and DOM foundation |
| `bweb-css` | CSS model and style foundation |
| `bweb-js` | Reserved foundation for the future Rust JavaScript runtime |
| `bweb-net` | URL and networking foundation |
| `bweb-shield` | Brave's Rust `adblock` engine integration point |
| `bweb-renderer` | Rendering abstraction |
| `bweb-shell` | Initial executable |

## Build

Requires Rust 1.85 or newer.

```sh
cargo build --release
cargo test --workspace
cargo run --release -p bweb-shell
```

## Current status

This is the workspace/bootstrap milestone. HTML parsing, CSS layout, JavaScript execution, HTTP/TLS transport, actual ad-filter loading and GPU rendering are planned as separate, testable milestones. No feature is advertised as implemented until it has working code and tests.

## Design principles

- Rust for the browser's own components.
- No embedded third-party browser engine.
- Network requests must pass through the privacy filter before bytes are downloaded.
- Small, independently testable crates.
- Standards support grows incrementally, with explicit compatibility limits.
