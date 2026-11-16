# B-Web

**B-Web** is an experimental, Rust-first browser project. Its long-term goal is an independently developed web engine: HTML/DOM, CSS/layout, JavaScript runtime, networking, rendering and privacy filtering.

The project deliberately does **not** embed Chromium, WebKit, Gecko or Wry. The first commits establish a clean, compilable workspace; they do not yet constitute a standards-compliant browser.

## Workspace

| Crate | Responsibility |
| --- | --- |
| bweb-core | Shared engine types and version information |
| bweb-html | Arena-backed DOM, limited HTML parser, and guarded document resource loader |
| bweb-css | CSS model and style foundation |
| bweb-js | Reserved foundation for the future Rust JavaScript runtime |
| bweb-net | Rustls HTTP/HTTPS transport with mandatory Shield admission and privacy headers |
| bweb-shield | Brave's Rust adblock engine integration and bounded local filter-list cache |
| bweb-filterlists | HTTPS-only, bounded downloader for the built-in EasyList and EasyPrivacy sources |
| bweb-renderer | Rendering abstraction |
| bweb-shell | Initial executable, filter-list update command, and document loading command |

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

On startup, the shell loads cached lists and compiles them into an in-memory Shield engine. It reports whether Shield is ready or inactive because no lists have been cached. Invalid or unreadable cached lists are reported as errors rather than silently ignored.

The `bweb-filterlists` crate retrieves only the built-in EasyList and EasyPrivacy HTTPS endpoints. It uses Rustls, finite timeouts, HTTPS-only redirects (maximum five), and an 8 MiB streaming response limit per list. Responses must be non-empty UTF-8 text and contain the expected list title before being written to the cache.

The downloader sends a fixed B-Web user-agent and no machine ID, installation UUID, or other persistent device identifier. Downloading these two built-in lists is a bootstrap operation and therefore does not pass through the ad-block rules being downloaded. The cache replaces each file atomically; the pair is downloaded before either cache file is changed, but a filesystem failure during the second replacement can leave the first list refreshed.

## Network admission policy

`bweb-net::HttpTransport` is the only HTTP/HTTPS client boundary. It uses reqwest's Rustls TLS backend (no OpenSSL feature), owns its private client, and requires a `NetworkPolicy`. Every request is checked before `send`; redirects are followed manually and each destination is checked again. Cross-origin redirects strip Authorization, Proxy-Authorization, and Cookie headers. Redirects are capped at ten, timeouts are finite, and response bodies are streamed.

Every outgoing request forcibly includes `Sec-GPC: 1` and `DNT: 1`. Non-HTTP(S) URLs are rejected. The shell builds this transport with cached Shield rules, or an empty ruleset when no lists are cached.

## HTML document and resource loading

The first `bweb-html` milestone introduces an arena-backed DOM. A `NodeId` is a stable index into a `Vec<Node>`; nodes contain parent and child IDs, and element attributes are stored as owned name/value pairs. This avoids per-node pointer ownership and makes tree traversal and later arena allocation strategies explicit.

The initial parser handles document, element, text, and comment nodes, quoted/unquoted attributes, closing tags, and common void elements. It is deliberately a limited parser, not a conforming HTML5 tokenizer/tree builder. Entity decoding, foster parenting, template contents, raw-text parsing rules, scripting, and full malformed-markup recovery remain future work.

`DocumentLoader` receives an already constructed `HttpTransport`. It has no independent HTTP client. The main HTML document is fetched as `ResourceType::Document`; linked stylesheets, images, and external scripts are resolved against the document URL and fetched only through the same transport as `Stylesheet`, `Image`, and `Script`. Therefore Shield admission, privacy headers, HTTP(S)-only validation, and redirect rechecks apply to each of these requests. Failed or blocked subresources are recorded without discarding the parsed document.

Document and subresource bodies have separate finite size limits (8 MiB HTML, 4 MiB CSS, 8 MiB scripts, 16 MiB images). This milestone does not yet parse CSS imports or CSS `url()` references, discover `srcset`/picture sources, execute JavaScript, decode images, or render a page.

Try the loader with:

```sh
b-web open https://example.com/
```

When no filter lists are cached, the shell clearly warns that Shield is inactive; requests still pass through the transport and its policy, but an empty ruleset cannot block advertising or tracking.

## Current status

B-Web is an incremental engine foundation, not yet a complete browser. HTML5 parsing, CSS cascade/layout, JavaScript execution, page lifecycle integration, image decoding, and GPU rendering remain future milestones. No third-party browser engine is embedded.

## Design principles

- Rust for the browser's own components.
- No embedded third-party browser engine.
- Every page and subresource request passes through the privacy-filtered transport.
- Bounded response sizes and finite network timeouts.
- Small, independently testable crates.
- Standards support grows incrementally, with compatibility limits documented.
