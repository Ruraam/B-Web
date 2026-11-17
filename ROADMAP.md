# B-Web roadmap

B-Web is a browser engine implemented from scratch in Rust. This document tracks
what the tree actually contains and what is still being built. It is a
development plan, not a claim of full web-platform compatibility.

## Current stage

`Interactive` — pages are fetched, styled, laid out, painted and navigated in a
native window. The engine exposes its own DOM, CSS engine, JavaScript runtime,
layout tree, text and image rasterization, network transport and privacy filtering.

## Delivered

### Networking and privacy
- Dedicated `bweb-net` HTTP/HTTPS client with Rustls and a private reqwest client.
- Mandatory Shield admission gate for every request, including redirects.
- `Sec-GPC: 1` and `DNT: 1` on all outgoing requests.
- EasyList/EasyPrivacy downloader with HTTPS-only redirects, finite timeouts and
  bounded, atomic cache updates.

### HTML and DOM
- Arena-backed DOM with stable `NodeId`s.
- Parser for elements, text, comments, attributes and void elements.
- Character reference decoding, raw `<script>`/`<style>` text.
- Implied end tags for paragraphs, list items, table rows/cells and similar
  elements; foster parenting of misplaced table content.
- Hyperlink lookup for hit-testing and navigation.

### CSS
- Lexer, rule/declaration parser and selector engine.
- Combinators (descendant, child, adjacent/general sibling), attribute operators,
  structural and negation pseudo-classes.
- `::before`/`::after` generated content with computed styles.
- Cascade with specificity, `!important`, source order and inline styles.
- Custom properties with `var()` substitution; shorthand expansion.
- `@media` feature queries evaluated against the active viewport.

### JavaScript
- Lexer, parser and tree-walking evaluator implemented in Rust.
- Variables, functions/closures/arrows, template literals, control flow,
  arrays, objects and operators.
- Built-ins: `Math`, number parsing helpers, string methods, array methods.
- Host bridge for `document.title`, `getElementById`, `querySelector`,
  `textContent`, inline styles and `document.write`; scripts run on load and can
  change the rendered page.
- `addEventListener('click', …)` with bubbling dispatch on real clicks and
  `event.preventDefault()` to cancel navigation.

### Layout and rendering
- Box tree with margins, padding, borders, sizing and inline formatting.
- Font-metric-based text layout with Swash shaping (kerning and ligatures),
  using the same advances as glyph rasterization.
- Images sized from attributes or intrinsic dimensions and painted inline.
- Deterministic display list, software rasterizer and RGBA framebuffer.

### Browser shell
- `b-web open`, `update-filters`, `--render-ppm`, `--dump-layout`, `--window`.
- Address bar, clickable links, history (`Alt+←`/`Alt+→`) and vertical scroll.
- Navigation on a background thread with stale-result rejection.

## In progress

- Broader HTML5 tree construction (implied `html`, `head`, `body`, full
  malformed-markup recovery, template contents).
- Additional CSS layout: positioning, floats, flex and grid.
- Full CSS property coverage, especially backgrounds and shorthands.

## Planned

- Broader JavaScript language and DOM API coverage inside `bweb-js`.
- Bidirectional text and full OpenType feature configuration.
- GPU-accelerated painting behind the renderer port.
- Horizontal scrolling, selection, forms and richer browser chrome.
- Standards conformance test suites and compatibility reporting.

## Design constraints

- Every web-facing component is implemented in this workspace.
- No third-party browser engine or JavaScript runtime is embedded.
- Every request passes through the privacy-filtered transport.
- Response sizes and network timeouts remain bounded.
- Crates stay small, independently testable and documented.
