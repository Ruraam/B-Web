# B-Web

**B-Web** is a browser engine written from scratch in Rust. It implements its own HTML parser, CSS engine, layout, text and image rasterization, JavaScript runtime, network stack and privacy filter, and ships a native browser shell built on those components.

No third-party browser engine is embedded: not Chromium, WebKit, Gecko, Wry, or an external JavaScript engine. B-Web is an independent implementation whose components are built and tested in this workspace. It is under active development toward web-platform compatibility; the current scope and remaining work are listed in [ROADMAP.md](ROADMAP.md).

## Workspace

| Crate | Responsibility |
| --- | --- |
| bweb-core | Shared engine types and version information |
| bweb-html | Arena-backed DOM, limited HTML parser, hyperlink lookup and guarded document resource loader |
| bweb-css | CSS lexer/parser, selector matching, cascade and NodeId-indexed computed styles |
| bweb-js | Reserved foundation for the future Rust JavaScript runtime |
| bweb-net | Rustls HTTP/HTTPS transport with mandatory Shield admission and privacy headers |
| bweb-shield | Brave's Rust adblock engine integration and bounded local filter-list cache |
| bweb-filterlists | HTTPS-only, bounded downloader for the built-in EasyList and EasyPrivacy sources |
| bweb-text | Font loading, metrics and glyph rasterization via Swash; no text shaping |
| bweb-layout | First-pass CSS box model, inline text layout and viewport resize policy |
| bweb-renderer | Software display list, painter, rasterizer and RGBA framebuffer |
| bweb-shell | Executable: status, filter-list update, document loading and native window |

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

The `bweb-html` milestone introduces an arena-backed DOM. A `NodeId` is a stable index into a `Vec<Node>`; nodes contain parent and child IDs, and element attributes are stored as owned name/value pairs. This avoids per-node pointer ownership and makes tree traversal and later arena allocation strategies explicit.

The initial parser handles document, element, text, and comment nodes, quoted/unquoted attributes, closing tags, and common void elements. A small HTML5-style recovery pass implicitly closes `<p>` before a following block element or paragraph, `<li>`, `<dt>`/`<dd>`, `<tr>`, `<td>`/`<th>`, `<option>` and table section elements when a sibling of the same kind starts, so omitted end tags produce correct siblings for selectors and layout. Text or elements found directly inside a `<table>` (outside cells) are foster-parented before the table, while table-structure tags stay in place. Text nodes decode common named and numeric character references (`&amp;`, `&#8212;`, `&#x42;`…), while `<script>` and `<style>` keep their content raw. It is deliberately a limited parser, not a conforming HTML5 tokenizer/tree builder. Full entity tables, template contents, proper raw-text tokenization, scripting, and full malformed-markup recovery remain future work. The DOM exposes a read-only stable-order iterator yielding each NodeId and node.

`DocumentLoader` receives an already constructed `HttpTransport`. It has no independent HTTP client. The main HTML document is fetched as `ResourceType::Document`; linked stylesheets, images, and external scripts are resolved against the document URL and fetched only through the same transport as `Stylesheet`, `Image`, and `Script`. Therefore Shield admission, privacy headers, HTTP(S)-only validation, and redirect rechecks apply to each of these requests. Failed or blocked subresources are recorded without discarding the parsed document.

Document and subresource bodies have separate finite size limits (8 MiB HTML, 4 MiB CSS, 8 MiB scripts, 16 MiB images). Fetched images are decoded and painted; this milestone does not yet parse CSS imports or CSS `url()` references, discover `srcset`/picture sources, or execute JavaScript.

`HtmlDocument::hyperlink_href` walks a node's ancestors to find the nearest `<a href>`. Combined with the DOM node identities retained by prepared text runs, this lets the window hit-test glyphs and follow links without a separate layout duplicate.

Try the loader with:

```sh
b-web open https://example.com/
```

When no filter lists are cached, the shell clearly warns that Shield is inactive; requests still pass through the transport and its policy, but an empty ruleset cannot block advertising or tracking.

## CSS parsing and computed styles

The `bweb-css` milestone provides a CSS token lexer (including comments, strings, identifiers, numbers and punctuation), a qualified-rule/declaration parser, and a selector engine for type, universal, ID and class selectors, attribute presence plus `=`, `~=`, `|=`, `^=`, `$=` and `*=` operators (with the `i` flag), and pseudo-classes `:root`, `:first-child`, `:last-child`, `:only-child`, `:first-of-type`, `:last-of-type`, `:empty`, `:link`/`:any-link`, `:nth-child()`, `:nth-last-child()` and `:not()`. Descendant, child, adjacent-sibling (`+`) and general-sibling (`~`) combinators and comma-separated selector lists are supported. `::before` and `::after` selectors with string `content` produce generated text that participates in inline layout with its own computed style. Unsupported pseudo-classes produce parse errors; unsupported at-rules are skipped rather than partially applied.

`Specificity` is represented as the (IDs, classes/attributes/pseudo-classes, types) tuple. `StyleEngine::compute` applies author rules and inline style declarations, respects `!important`, source order and selector specificity, and returns a `ComputedStyleMap` keyed by the existing DOM `NodeId`. `compute_with_viewport` additionally evaluates `@media` conditions (`min/max-width`, `min/max-height`, `width`, `height`, `orientation`, with comma lists and `screen`/`all` qualifiers). `StyleEngine::compute_generated` additionally returns the computed styles of `::before`/`::after` rules; they inherit from their originating element and are finalized like ordinary styles. Custom properties declared as `--name` are inherited and `var(--name, fallback)` references are substituted, including nested fallbacks. Shorthands are expanded into longhands (`margin`, `padding`, `background`, `border`, per-side `border-*`, `font`) after the cascade; an explicitly declared longhand always wins over its shorthand. Inherited values are carried to descendants and text/comment nodes; a small set of initial values is provided. This is not yet the full CSS cascade: origins, layers, media *types* beyond screen/all, every shorthand variant, non-string `content` functions, full escape handling, and layout-dependent computed-value resolution remain future work.

## JavaScript runtime

`bweb-js` implements a JavaScript lexer, recursive-descent parser and tree-walking evaluator entirely in Rust. The supported language includes variables (`var`/`let`/`const`), functions, closures and arrow functions, template literals, conditionals, `while`/`for` loops, arrays, objects, comparisons and `print`/`console.log`. Built-ins include `Math`, `parseInt`/`parseFloat`/`isNaN`, string methods and array methods (`push`, `join`, `forEach`, `map`). A host bridge exposes `document.title`, `getElementById`, `querySelector`, `textContent`, `style` properties and `document.write`, so scripts can mutate the document before styles are computed and the page is laid out. `addEventListener('click', handler)` registers callbacks on elements; real mouse clicks dispatch them with bubbling through ancestors, and `event.preventDefault()` cancels link navigation. Inline `<script>` elements and fetched external scripts run on page load; errors are reported on stderr without discarding the page. DOM APIs, event types, built-ins and language features remain incremental.

## Layout and software rendering

`bweb-layout` builds a DOM-backed box tree with a first-pass CSS box model: block/inline display, margins and padding (shorthands plus per-side longhands), borders, explicit width/height, `box-sizing`, and simplified line boxes with collapsed whitespace and word wrapping. `LayoutTree::build_with_font` measures text with the same font used to rasterize glyphs; `bweb-text::Font::shape` applies Swash kerning and ligatures, and both fragment measurement and glyph positioning use the shaped advances so wrap points and painted text agree. `LayoutTree::build` retains a font-free approximation for lightweight DOM/CSS tests. A unitless `line-height` is interpreted as a multiplier of `font-size`. Text runs are prepared from the layout fragments using real font advances and glyph masks from `bweb-text`; shaping, kerning, bidi, floats, positioning, flex and grid are not implemented.

`bweb-renderer` turns that geometry into a deterministic physical-pixel display list (fills, borders, text, images, clipping), rasterizes it into an RGBA framebuffer, and `bweb-shell` copies the frame into a winit + softbuffer surface. Layout coordinates are CSS pixels; painting applies the device scale factor.

Fetched images are decoded with the Rust `image` crate (PNG, JPEG and GIF only) into bounded RGBA8 bitmaps. Each `<img>` is matched to its fetched body by resolved URL and sized from `width`/`height` attributes or its intrinsic aspect ratio. Those sizes are passed into `LayoutTree`: an image is an inline replaced element, consuming horizontal space so adjacent text flows beside it. The line grows to the image height and the text on that line is centered vertically against it; a block child still starts on a new line. The image is blitted into the display list with nearest-neighbor scaling. Animated images use their first decoded frame, and CSS `background-image` is not implemented.

The view is retained across resizes: a height-only change repaints without reflowing, while a width or scale change rebuilds layout and text geometry. Resize events are coalesced to the latest viewport configuration before dispatch.

`b-web open <URL> --window` opens the native window. A minimal address bar is composited above the page framebuffer: clicking text inside an `<a href>` follows the link, typing a URL and pressing Enter navigates, `Ctrl+L` clears the bar, and `Alt+Left` / `Alt+Right` walk the history. The mouse wheel scrolls vertically by translating the display list before rasterization; the offset is clamped to the laid-out content height and does not invalidate layout or text geometry. Hit-testing adds the scroll offset so links remain clickable after scrolling.

Navigations run on a background thread and are delivered to the event loop as a user event, so the window stays responsive during network I/O. Stale results from superseded navigations are ignored. Every navigation goes through the same `DocumentLoader` and shielded transport as the initial page. A failed navigation keeps the current page, restores the address bar and history position, and reports the error on stderr. The first page is still loaded synchronously before the window is created.

`--render-ppm FILE` writes the same software frame as a binary P6 PPM without opening a window, and `--dump-layout` prints the box tree with geometry and fragment identities. Set `BWEB_FONT` to select a TTF/OTF font; otherwise common Linux sans fonts are tried.

## Current status

B-Web currently implements a documented subset of the web platform and is developed milestone by milestone. The main areas still missing are full HTML5 tree construction, the complete CSS cascade and layout model, a JavaScript runtime, CSS backgrounds and additional image formats, GPU rendering, horizontal scrolling, and richer browser chrome. See [ROADMAP.md](ROADMAP.md) for the plan and current status. No third-party browser engine is embedded.

## Design principles

- Rust for the browser's own components.
- No embedded third-party browser engine.
- Every page and subresource request passes through the privacy-filtered transport.
- Bounded response sizes and finite network timeouts.
- Small, independently testable crates.
- Standards support grows incrementally, with compatibility limits documented.
