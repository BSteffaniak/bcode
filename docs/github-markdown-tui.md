# GitHub Markdown in the transcript TUI

Bcode renders assistant, reasoning, and explicitly typed user Markdown as terminal-native styled text. The renderer preserves a typed semantic sidecar for interaction and richer presentation; plain-text and JSON transcript items remain distinct and are not reparsed as Markdown.

## Supported syntax

The terminal renderer supports the CommonMark and GitHub-oriented syntax used by Bcode transcripts, including:

* headings, paragraphs, emphasis, strong text, strikethrough, and inline code;
* ordered, unordered, nested, tight, loose, and task lists;
* fenced and indented code blocks with bounded syntax highlighting;
* block quotes and GitHub NOTE, TIP, IMPORTANT, WARNING, and CAUTION alerts;
* tables with alignment, Unicode display-width handling, body-row dividers, multiline visual-row alignment, and a stacked narrow-width fallback;
* links, images, footnotes, inline/display math, thematic breaks, and Mermaid fences;
* safe `<details><summary>…</summary>…</details>` blocks, including nesting;
* readable best-effort output for incomplete streaming Markdown.

Renderer-owned options are included in a versioned layout signature so width, theme, trusted document context, Mermaid bounds, streaming state, and semantic contributions invalidate retained rows correctly.

## Terminal differences and fallbacks

A terminal cannot reproduce browser layout or behavior exactly. Bcode uses explicit readable fallbacks rather than silently dropping content:

* Images retain alt text while unavailable, loading, failed, or unsupported. Image escape sequences are never emitted directly by Bcode.
* Mermaid retains highlighted source and a diagnostic when rendering is disabled or fails.
* Details content is readable in noninteractive output. Incomplete streamed details expose their summary and body without requiring a closing tag.
* Footnote references render as stable numbers and definitions render in a Footnotes section with return markers.
* Inline and display math use bounded Unicode-oriented terminal projection and preserve source when no faithful conversion exists.
* Wide tables become labeled stacked rows when they cannot fit the available columns.
* Safe raw HTML may remain visible as text. Scripts, event handlers, dangerous markup, and Mermaid directives are never executed.

## Link policy

Link destinations are classified before interaction:

* Absolute `http` and `https` URLs are safe web destinations.
* Relative URLs require explicit trusted base URL or base-directory context.
* Local paths are actionable only after resolution under trusted local context.
* Fragments are reserved for internal document navigation.
* Unsupported schemes, invalid destinations, and unresolved relatives are inert.

Only classified web and trusted local destinations may reach Bcode's private operating-system opener or copy-destination adapter. Bcode does not emit OSC 8 hyperlinks.

## Image policy

Bcode owns image source classification, network and redirect policy, encoded/decoded resource limits, caching, cancellation, alt text, and transcript row reservation. BMUX owns terminal capability detection, protocol selection, registry identity, clipping, scrolling, removal, transport, and compositing.

The intended image loader is bounded and non-eager:

* normal history discovery does not fetch remote images;
* every redirect and final destination must be revalidated;
* encoded bytes are limited before decoding;
* dimensions and decoded pixels are limited before allocation growth;
* concurrent requests are deduplicated by a stable cache key;
* obsolete and nonresident work is cancelled;
* cache size is bounded by entry and byte limits.

Until the interactive loader is available, semantic image contributions and readable alt-text fallback remain authoritative.

## Details policy

Details parsing accepts only balanced `<details>` blocks with a complete `<summary>`. The source `open` attribute is preserved as typed metadata. Nested blocks retain independent contributions. Unsupported attributes, missing summaries, and finalized malformed markup remain visible rather than being interpreted. Streaming-only incomplete blocks use a readable summary/body projection without claiming a finalized contribution.

## Math policy

Math rendering is deterministic and bounded. It does not execute TeX, shell commands, HTML, or external programs. Unsupported constructs retain readable source instead of disappearing.

## Mermaid policy

Mermaid uses Bcode-owned request/result/error types and a private pure-Rust backend. Source, output, dimensions, directives, timeout, and cancellation are validated. Directives are rejected. Backend failures become visible typed diagnostics, and source remains available as the fallback. Successful terminal-image presentation must pass through BMUX image contributions.

## Cache and resource policy

Retained transcript layout is width- and signature-aware. Isolated updates rebuild only changed entries; unaffected row allocations are reused. Resident transcript history is bounded and old history remains reloadable. Rich rendering must remain limited to the resident projection and must not trigger eager remote loading or full-history replay.

Image and Mermaid caches must be bounded LRUs with deterministic versioned keys. Cache keys include all inputs that can affect output. In-flight work must be deduplicated and cancelled when its owning transcript item changes or leaves the resident window.

## Security guarantees

* No arbitrary HTML, JavaScript, terminal image escape, OSC hyperlink, or Mermaid directive is executed from Markdown.
* Repository identity and trusted local/base URL context are never inferred.
* Unsafe and unresolved destinations remain inert.
* Browser-only features retain a visible terminal fallback.
* Parser/backend implementation types stay private; consumers receive Bcode-owned semantic types.

## Capability behavior

Color and Unicode improve presentation but are not required to retain text. Interactive details, links, images, network access, and Mermaid rendering are capability-dependent. Unsupported capabilities must leave readable fallback content and must not create actionable hit regions or background network work.
