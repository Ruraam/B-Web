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
- CSS `url()` backgrounds discovered, fetched through Shield and painted, with
  size (`cover`/`contain`/lengths), position and repeat tiling.

### HTML and DOM
- Arena-backed DOM with stable `NodeId`s.
- Parser for elements, text, comments, attributes and void elements.
- HTML5 insertion modes with implied `html`/`head`/`body`, head-element routing
  and merged attributes; doctype skipping and a bounded open-element stack.
- Full WHATWG named character reference table with longest-match decoding,
  numeric reference C1 remapping and strict attribute-value rules.
- Raw-text/RCDATA scanning; active formatting element reconstruction and a
  simplified adoption agency across misnested special blocks.
- Inert `<template>` content subtree, excluded from styling, layout, scripts
  and resource loading.
- Implied end tags for paragraphs, list items, table rows/cells and similar
  elements; foster parenting of misplaced table content.
- Hyperlink lookup for hit-testing and navigation, `<base href>` resolution for
  subresources and clicked links.

### CSS
- Lexer, rule/declaration parser and selector engine.
- Combinators (descendant, child, adjacent/general sibling), attribute operators,
  structural and negation pseudo-classes.
- `::before`/`::after` generated content with computed styles.
- Cascade with specificity, `!important`, source order, inline styles,
  user-agent/author origins and `@layer` ordering (reversed for `!important`).
- Custom properties with `var()` substitution; shorthand expansion.
- `background` shorthand expanded into all longhands with comma-separated layer
  support; named, hex, `rgb()`/`rgba()` and `hsl()`/`hsla()` color values.
- `@media` feature queries evaluated against the active viewport.

### JavaScript
- Lexer, parser and tree-walking evaluator implemented in Rust.
- Variables, functions/closures/arrows, template literals, control flow,
  arrays, objects and operators.
- Built-ins: `Math`, number parsing helpers, string methods, array methods.
- Host bridge for `document.title`, `getElementById`, `querySelector(All)`,
  `createElement`/`appendChild`, `setAttribute`/`getAttribute`, `classList`,
  `textContent`, inline styles and `document.write`; scripts run on load and can
  change the rendered page.
- `addEventListener('click', …)` with bubbling dispatch on real clicks and
  `event.preventDefault()` to cancel navigation.

### Layout and rendering
- Box tree with margins (including auto), padding, borders, `max-width`,
  em/rem sizing, inline formatting, `text-align`, `text-indent` and letter/word
  spacing.
- Font-metric-based text layout with Swash shaping (kerning and ligatures),
  using the same advances as glyph rasterization.
- Images sized from attributes or intrinsic dimensions and painted inline.
- Deterministic display list, software rasterizer and RGBA framebuffer.
- Linear-gradient backgrounds rasterized from CSS angles/`to <side>` directions
  and percentage/length color stops.
- Composable `transform` translate/scale with rasterizer application to rects,
  glyphs and clips; subtree `opacity`.
- Single-line flexbox: direction, grow/shrink, justification, alignment, gaps
  and the `flex`/`gap` shorthands.
- `position: relative/absolute/fixed` with offsets and positioned containing
  blocks; out-of-flow boxes stay outside normal flow.
- CSS grid: `fr`/length/`auto`/`minmax()`/`repeat()` tracks, gaps, implicit
  rows, auto-placement, explicit `grid-column`/`grid-row` line/span placement
  and named `grid-template-areas`/`grid-area`.
- Floats with inline wrapping, `clear` and per-formatting-context containment;
  multiple floats per line.

### Browser shell
- `b-web open`, `update-filters`, `--render-ppm`, `--dump-layout`, `--window`.
- Address bar, clickable links, history (`Alt+←`/`Alt+→`) and vertical scroll.
- Navigation on a background thread with stale-result rejection.

## In progress

- Remaining HTML5 tree construction: the complete adoption-agency inner loop
  and the full tokenizer state tables.
- Additional CSS layout: `minmax()` grid tracks, subgrid and orthogonal writing
  modes.
- CSS property coverage: background-origin/clip, filters, rotate/skew/perspective
  transforms and remaining shorthand variants.

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
