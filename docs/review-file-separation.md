# File Separation in the Review

In a long review, files run into each other. Today a file's start is one row:

* the accent bar;
* the status letter and path;
* flags, layers, threads, `+/−`, the change bar, and the viewed mark.

That row is painted in `surface_alt`, the same tier as the header bar and the footer, so it reads
as more chrome rather than as "a new file starts here". The sticky header works well and stays.
This plan makes file boundaries unmistakable at a glance and gives the header more information
and more buttons when the screen has room for them. When it has none, the result is still a
better one-row header than today's.

## The parts

Each file in the document gets up to three parts instead of one item:

1. **The break** (`FileBreak`, new, not sticky). The space between the previous file and this
   one: the previous page torn off, a gap, and this page's top edge. It is not drawn before the
   first file of a group, since a source or layer banner already starts there.
2. **The header** (`FileHeader`, sticky as today, level 0). One row in every density, and a
   second row only in the spacious density. It is the part that pins, so pinned rows stay
   bounded however rich the rest is.
3. **The details** (`FileDetails`, new, not sticky). The rest of the information, and the
   file's buttons, under the header. Densities that have no room for it leave it out.

The header must be the one sticky part. BMUX pins a header item with all of its rows, so a
four-row header would cover four rows of every screen. Keeping the extra information in a
separate item that scrolls away keeps the pinned region small. A later BMUX change could let a
header paint differently while pinned (see [Phases](#phases)).

All three are document items keyed by the file (`ItemKey::FileBreak(FileRef)`,
`ItemKey::FileDetails(FileRef)`). Persisted positions keep naming the header, so stored scroll
anchors and layer positions stay valid. Heights are exact and computed without building rows,
as `RowContext::height` does today. They depend only on the density, which joins the layout
revision, so a density change relays out once and an ordinary frame does not.

## Densities

The density is decided per frame from the review pane's size. It follows the same pattern as
the panels' narrow-width degradation, with hysteresis so a one-row resize does not make
headers jump.

| Density | When (default) | Break | Header | Details |
|---|---|---|---|---|
| **Compact** | under 24 rows or 90 columns | 1 row | 1 row | none |
| **Comfortable** | otherwise | 2 rows | 1 row | 1 row |
| **Spacious** | 45+ rows and 140+ columns | 3 rows | 2 rows | 1–2 rows |

Configuration can pin a density (see [Configuration](#configuration)).

### Compact: today's row, but distinct

```text
▔▔╲▁╱▔▔▔╲╱▔▔▔▔╲▁▁╱▔▔▔╲╱▔▔▔▔▔▔▔╲▁╱▔▔▔▔╲╱▔▔▔▔▔▔▔▔╲▁▁╱▔▔▔▔▔▔╲╱▔▔▔▔▔▔▔▔▔▔
▌ M  src/review/  surface.rs                    💬2  +120 −34 ■■■··  ○
```

Without changing the row count, it becomes distinct:

* **Its own style.** A new `file_header` style that is no longer the header-bar tier: a stronger
  surface, and an accent bar the full row height, colored by the file's status.
* **The path in two weights.** The directory is muted and the file name bright and bold. The
  eye finds the name, which is what changes from file to file.
* **A pill for the status letter**, colored by status.
* **A one-row break** above it, the tear (below), so even compact files never touch.

### Comfortable: one more row, and buttons

```text
  ▔▔╲▁╱▔▔▔╲╱▔▔▔▔╲▁▁╱▔▔▔╲╱▔▔▔▔▔▔▔╲▁╱▔▔▔▔╲╱▔▔▔▔▔▔▔▔╲▁▁╱▔▔▔▔▔▔╲╱▔▔▔▔▔▔
  ▁▁╱▔╲▁▁▁╱╲▁▁▁▁╱▔▔╲▁▁▁╱╲▁▁▁▁▁▁▁╱▔╲▁▁▁▁╱╲▁▁▁▁▁▁▁▁╱▔▔╲▁▁▁▁▁▁╱╲▁▁▁▁▁▁
▌ M  src/review/  surface.rs                    💬2  +120 −34 ■■■··  ○ Viewed
     Rust · 4,846 lines · 6 hunks · #52131 #52133   [Comment] [Ask] [Whole file] [Collapse]
```

The details row holds the file's facts and its buttons:

* **Facts**, as many as fit, in order: the rename (`from old/path.rs`, with similarity), the
  language, the new line count, the hunks, a mode change (`100644 → 100755`), the layers
  that change the file, the size for binaries, and *generated* for files `.gitattributes`
  marks `linguist-generated`.
* **Buttons**:
  * **Viewed** (a checkbox: `○ Viewed` / `✓ Viewed`; clicking toggles, like `v`);
  * **Comment** (a file-level comment);
  * **Ask** (Bcode about this file);
  * **Whole file**;
  * **Collapse**;
  * **Through the stack**, on stacks (the file's layers, the existing stack file list);
  * **Copy path**.

  Buttons that do not fit drop from the end, and every one is also a palette action.

### Spacious: the top of a page

```text
  ▔▔╲▁╱▔▔▔╲╱▔▔▔▔╲▁▁╱▔▔▔╲╱▔▔▔▔▔▔▔╲▁╱▔▔▔▔╲╱▔▔▔▔▔▔▔▔╲▁▁╱▔▔▔▔▔▔╲╱▔▔▔▔▔▔

  ▁▁╱▔╲▁▁▁╱╲▁▁▁▁╱▔▔╲▁▁▁╱╲▁▁▁▁▁▁▁╱▔╲▁▁▁▁╱╲▁▁▁▁▁▁▁▁╱▔▔╲▁▁▁▁▁▁╱╲▁▁▁▁▁▁
▌ M  surface.rs                                       ✓ Viewed   3 of 41 files
▌    packages/code-review-ui/src/  ·  renamed from view.rs (92%)
     +120 −34 ■■■··  ·  Rust  ·  4,846 lines  ·  6 hunks  ·  💬 2 open, 1 resolved  ·  ↗ 1 on #52131
     [Comment] [Ask Bcode] [Whole file] [Collapse] [Through the stack] [Copy path]   ↑ prev  next ↓
```

* **Two header rows**, both pinned. One row is the name, viewed state, and the position among
  the files; the other is the directory and the rename.
* **Up to two details rows.** The facts include thread counts split into open and resolved, and
  the pull request threads on the file. There is room for **prev** and **next** file buttons.

## The tear

The "ripped page" separation, reading as paper torn between files:

* **Each file is a sheet.** File bodies are painted on a page tier (`file_page`): the canvas or
  a tier just above it. The break between files is canvas. Diff colors (added and removed
  lines) keep their own backgrounds over the page.
* **The bottom of the previous sheet is torn** into the break: one row of characters painted in
  the page color over the canvas. The rows are a jagged mix of `▔`, `╲`, `▁` and `╱`, or upper
  and lower half blocks when the terminal draws them reliably. They draw the paper's ragged
  edge.
* **The top of the next sheet** is the mirror of the same tear (comfortable and spacious). The
  two edges read as one sheet torn in two, with a gap of canvas between them.
* **The tear is deterministic.** It is derived from a hash of the file's path, so it never
  shimmers while scrolling and each file's edge is its own. It is built once per width and
  cached with the row.
* **It degrades:**
  * With the ASCII icon set the tear is `~^~-~^~~-^`.
  * Under `color_depth = "none"`, or a theme without a usable page tier, it becomes a plain
    rule (`┄┄┄` in muted).
  * Configuration can choose `rule`, `space` or `none` instead.

Today, between files, the previous file's last line meets the next header. After this there
are always at least one row of break and a header of a different style, and a clear edge at
every size.

## The pinned header

Pinning stays as it is: the file header pins, and the hunk header pins under it. Two
improvements:

* **Pinned rows stay bounded.** The header is 1 row (2 when spacious) and the hunk header is 1,
  so at most 3 rows are pinned. `StickyHeaders::max_rows` makes that explicit, so a future
  taller header cannot grow the pinned region.
* **A shadow under the pinned stack** (phase 3, needs BMUX). This is one row of `▀` painted in
  the header's color over the content: an edge that says "you are inside this file" while
  scrolling. It needs a small BMUX addition: a way for an item source to paint a header
  differently while it is pinned, or a pinned-region decoration hook. BMUX's sticky module has
  the geometry (`PinnedHeader::visible`) but no hook to paint with it.

## Mouse

* **Clicks:**
  * clicking the file name moves the cursor to the header, as today, and nothing else;
  * the status pill and the change bar do nothing;
  * every button in the details row is a target, with hover styles as the thread buttons have;
  * the **Viewed** checkbox in the header row works in every density, including compact,
    where it is the `○`/`✓` mark that is already there.
* **Pinned headers are clickable.** A click on a pinned header goes to that file's header;
  on its Viewed mark it toggles. Other targets show a hover cursor style.

## Configuration

These are `[plugins.config."bcode.code_review".review_v2]` keys:

* `file_header`: `auto` (the default; by the pane's size), `compact`, `comfortable` or
  `spacious`.
* `file_separator`: `torn` (the default), `rule`, `space` or `none`.
* `file_page`: `auto` (the default: a page tier when the theme gives one distinct from the
  canvas), `on` or `off`. With `off`, file bodies stay on the canvas and the tear becomes a
  rule.

The density thresholds are constants, not settings, unless use shows they need to be.

## Phases

1. **The compact header made distinct.**
   * The new `file_header` style, separate from the header bar.
   * The status pill, the directory and name in two weights, and the full-height accent.
   * The one-row break with a rule.
   * Screen tests that the header row differs in style from the header bar and from content.
2. **Parts and densities.**
   * The `FileBreak` and `FileDetails` items and keys, with exact heights.
   * The density in the layout revision, and the comfortable and spacious layouts.
   * The details facts and buttons, with click routing and palette actions.
   * The `file_header` setting.
   * Screen tests at the three sizes; scroll anchors and layer positions surviving density
     changes; a benchmark that a 2,000-file review does not get slower to scroll.
3. **The tear and the page.**
   * The page tier in the theme (checked in every bundled theme, light and dark).
   * Deterministic tears with ASCII and no-color fallbacks, and the `file_separator` and
     `file_page` settings.
   * Theme gallery entries.
4. **The pinned header** (with a BMUX change).
   * A pinned-paint hook in BMUX's virtual list.
   * The shadow under the pinned stack, and clicks on pinned headers.

## Decisions

1. **Paper sheets:** file bodies get a page tier when the theme gives a distinct one
   (`file_page = "auto"`).
2. **Torn edges:** both edges in comfortable and spacious, only the bottom in compact.
3. **BMUX:** add a pinned-paint hook to BMUX's virtual list for the pinned shadow and pinned
   clicks.

## Status

Implemented, not committed.

* **Phases 1–3:**
  * The new header style, pill, and the two-weight path.
  * `FileDetails` and `FileEnd` document items (`FileChrome` on the document, off by default
    for other users of it).
  * Densities by the review list's size with hysteresis, and the `file_header`,
    `file_separator` and `file_page` settings.
  * Deterministic torn edges over a desk color, with fallbacks.
  * The details buttons: Comment, Ask Bcode, Whole file, Collapse/Expand, Through the stack,
    Copy path, and prev/next when spacious.
  * Per-file collapse (`z` on a file's header, the `collapse_file` action, kept with the review),
    and the clickable viewed mark.
* **The page:** it is the canvas, and the gap between pages is a *desk* tier (the canvas
  shaded, or lifted when it is nearly black). That keeps every diff row's background as it was,
  and still reads as paper torn over a desk.
* **Phase 4:**
  * BMUX gains `VirtualItemSource::build_pinned` with a `PinnedPlacement` (deepest, pushed).
  * The review underlines the deepest pinned header. A drawn shadow row would need a row of its
    own, which pinning cannot add.
  * Pinned headers already received clicks, so the pinned viewed mark and buttons work as they
    are.
* **Not done:** the *generated* fact (it needs a `git check-attr` pass per file), and the open
  and resolved split of thread counts.
