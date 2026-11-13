# Review Separators and File Folding

Two changes to the review's source viewer:

1. Replace the torn-page separator with a small set of clean, themeable separator styles.
2. Make folding a file an ordinary, GitHub-like action: viewed files fold by themselves, any
   file can be folded or opened by hand, and folding never changes the viewed state.

## Why the tear looks bad

The current tear (`rows/file.rs::tear_row`) builds each edge from random runs of three glyphs
(`▀`, `▔`, and a blank), at three weights:

* **It reads as noise.** Runs of 1–4 cells at three unrelated heights make a sawtooth that looks
  like a rendering glitch, not a torn edge.
* **The holes look like bugs.** The blank cells let the desk show through the page.
* **The glyphs do not line up.** `▀` is half a cell and `▔` is an eighth, so the edge jumps
  between two heights with nothing in between.
* **Its colors are fixed.** The desk is the canvas shaded by a fixed amount, so on some themes
  it is a muddy band.
* **Nothing else in Bcode looks like it.** Panels, cards and the composer all use solid edges.

## Separator styles

`file_separator` chooses one style. Every style is built from theme roles (below), so a theme
can restyle it without code.

### `card` (the new default)

Each file is a card on the desk: a solid half-block edge under the page, a band of desk, and a
solid half-block lip that raises the next file's header. Nothing is random, every edge is a
straight line, and it is the same shape as the dock's cards.

```text
│   12 +     let value = compute(12);
▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀   ← page bottom (page color over desk)
                                                   ← desk (comfortable and spacious only)
▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄   ← header lip (header color over desk)
▌ M  src/review/  surface.rs              +120 −34 ■■■··  ○
```

Rows by density:

| Density | Rows |
|---|---|
| Compact | 1: the bottom edge and the lip in one cell row (`▀` in the page color, with the desk as its background) |
| Comfortable | 2 |
| Spacious | 3 |

**Optional insets** (`file_separator_inset`, off by default) keep a one-column desk margin on
the left and right of the edge rows. The edges then read as a card's corners without changing
any content row.

### `rule`

A labeled hairline. The label is the next file's position, and the row is quiet enough for long
reviews.

```text
│   12 +     let value = compute(12);
──────────────────────────────────────── 4 of 15 ──
▌ M  src/review/  surface.rs              +120 −34 ■■■··  ○
```

The line glyph comes from the theme's border set (`─`, `┄`, `━`, or `-` in ASCII). The label is
optional (`file_separator_label`).

### `gap`

Desk-colored blank rows with no edges: the plainest style.

### `none`

No separator rows (today's `none`).

## Theme roles

These are new `source` roles in `bcode_tui_theme::roles`. They are not derived from the seed
palette: unset, the review derives them from its tiers. Themes set them like any other role,
and `bcode theme roles` lists them.

| Role | Used for | Unset |
|---|---|---|
| `source.file.header` | the file header row, and the lip under the previous card | the accent mixed into the canvas |
| `source.file.desk` | the space between files | the canvas stepped just far enough to tell apart (contrast 1.18): darker, or lighter on a near-black canvas |
| `source.file.edge` | a card's bottom edge (its foreground) and the rule | the page color over the desk; the rule is dim |
| `source.file.label` | the rule's `N of M` label | muted text |

Without a desk (no RGB canvas, `color_depth = "none"`, or `file_page = "off"`) and with the ASCII
icon set, `card` falls back to `rule`.

## Configuration

These are `[plugins.config."bcode.code_review".review_v2]` keys:

* `file_separator`: `card` (the default), `rule`, `gap` or `none`. Older values keep working:
  `torn` reads as `card`, `space` as `gap`.
* `file_separator_rows`: `1`–`3`; unset follows the density.
* `file_separator_label`: whether `rule` shows `N of M` (the default is true).
* `file_separator_inset`: a column of desk at each side of a card's edges (the default is false).
* `file_page`: unchanged (`auto`, `on`, `off`). With `off`, there is no desk, and cards fall back
  to a rule.
* `fold_viewed_files`: whether viewed files fold by themselves (the default is true).

## Folding files

### Behavior (as on GitHub)

* **Folding is its own state.** Folding or opening a file never changes its viewed mark.
* **Marking a file viewed folds it.** This covers `v`, the header's mark, *mark every file
  viewed*, a carried-over file, and an assistant's mark.
* **Unmarking a viewed file opens it.**
* **A viewed file can be opened by hand.** It stays open until you fold it, or until it is marked
  viewed again, which folds it again, as on GitHub.
* **Any file can be folded by hand**, viewed or not:
  * with `z` on its header;
  * with the ▾/▸ chevron at the start of its header row, in every density;
  * with the Collapse button in the details row.
* **Fold all / open all:**
  * `shift+z` on a file's header folds every file, or opens them all when all are folded;
  * the palette actions *Fold every file* and *Open every file*.
* **Config:** `fold_viewed_files` (the default is true). With false, viewed files stay open unless
  folded by hand.

### Model

* **`ReviewDocument`** keeps a `FileFold` per file:
  * `Auto`, which follows the viewed mark;
  * `Open`, opened by hand;
  * `Folded`, folded by hand.

  `is_collapsed` resolves it against the viewed state and `DocumentConfig::fold_viewed`.
  `set_fold` and `set_collapsed` (by hand) rebuild the file, and the file before it, whose break
  depends on it.
* **Marking or unmarking a file viewed** in the review resets its fold to `Auto`.
* **Persistence:**
  * `collapsed_files` (folded by hand) keeps its meaning.
  * `opened_files` (opened by hand) is new.
  * `Auto` is not stored.

### What a folded file looks like

A folded file is its header row alone, with ▸, and no details row. The break after it is a
card edge in the header's color. Between two folded files the break is one row, a sliver of desk,
or an unlabeled rule. A run of viewed files then reads as a list of headers, as on GitHub.

### Navigation

* **File motions** (`n`, `p`, the panel, *next ↓*) stop on folded headers and do not open them.
* **Other targets inside a folded file open it.** This covers:
  * hunk, change, thread, open-thread and AI-answer motions;
  * attention;
  * a jump to an annotation (the threads panel, a pull request thread's link);
  * a search match.

  The document reports these targets at the folded file's header with `NavTarget::opens`. The
  review opens the file and runs the motion again.
* **Marking the cursor's file viewed** folds it. When the cursor was inside the file, it goes to
  the header. As before, marking then moves on to the next unviewed file.

## Status

Implemented, not committed.

* **Separators:** cards, the labeled rule, gaps, the rows, label and inset settings, the theme
  roles, the contrast-derived desk, and the fallbacks. The torn edge is removed.
* **Folding:** viewed files fold; hand folds and opens are persisted; there is the header
  chevron, fold all and open all; jumps open their file; breaks between folded files are tight.
* **Tests:**
  * document tests for the fold resolution, tight breaks, and motions into folded files;
  * screen tests for cards, the rule and its settings, the chevron, viewed files folding and
    opening by hand, the folded list, motions opening files, and theme roles.
* **Not done:**
  * `bcode theme check` does not yet warn about a desk too close to the page;
  * the theme gallery does not show each style;
  * the file panel does not mark folded files;
  * a file whose content changed keeps a hand fold.
