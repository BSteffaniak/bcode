# Code Review V2 architecture

Status: implemented (phases 0 to 7). This document is the contract for the Code Review V2
rewrite. Phases landed in order; each phase was accepted against the criteria in section 11. V2 is
the only review implementation: V1 was deleted in phase 7 (section 10). Appendix A keeps V1's
behavior inventory, with its V1 file locations, as the record the parity decisions in Appendix B
were made against.

## 1. Purpose and scope

### 1.1 Purpose

`bcode review` must be a best-in-class terminal code review product:

* Smooth, bounded-cost scrolling through reviews of any size: thousands of files and hundreds of
  thousands of diff lines.
* One continuous review document across all files, not a one-file-at-a-time viewer.
* Correct, order-independent context expansion against the exact revisions under review.
* A cohesive, colorful, themed interface that matches the rest of Bcode.
* A home screen of previous reviews and a guided way to start new ones from the working tree,
  branches, commits, pull requests, and Graphite-style stacked pull requests.
* Unified and side-by-side diffs in the first release.

### 1.2 Relation to V1

V2 was a parallel rewrite. V1 received no new features and no bug fixes while V2 was selectable
at startup during development; once the parity criteria passed, V2 became the only
implementation and V1 was deleted together with the host-side review launcher code (section 10,
phase 7).

Durable review data is shared. V2 reads and writes the same workspaces, drafts, threads, AI
exchanges, suggestions, publish records, and external imports. Schema additions are additive and
migrated through Switchy's typed schema APIs (no raw SQL, per `AGENTS.md`).

### 1.3 Placement rule: BMUX first, generic upstream, bespoke in the plugin

* **Use BMUX components wherever one fits.** V2 composes `bmux_tui_components` rather than
  painting rows by hand. Section 8.1 maps every screen region to its component.
* **Generic gaps go upstream into BMUX.** If a needed capability is a general terminal UI mechanism
  (list virtualization, sticky headers, rich tree labels, row-level diff layout, whole-file syntax
  projection), it is implemented in BMUX with generic APIs and tests, developed through
  `scripts/bmux-dev.sh` (`docs/bmux-development.md`). Nothing Bcode-specific goes upstream.
* **Review semantics stay in Bcode.** Review sources, anchors, drafts, threads, stacks, publish
  mapping, viewed state, and review presentation policy live in `plugins/code-review-plugin`
  (UI and service) and `packages/code-review/models` (contracts). Pull-request provider behavior
  stays in provider plugins.
* **The host provides plumbing only.** No code-review routing, outcome types, or product flow in
  `packages/tui` or `packages/cli`.

## 2. Diagnosis summary (V1)

Verified in the V1 code; these are the defects V2 is designed to remove.

1. **Eager, unbounded loading.** `review.workspace.materialize` runs the full diff for every source,
   parses every hunk, lists every repository file, every branch, and recent commits, and returns
   it as one JSON payload before the first frame (`materialize_review_workspace_for_request`).
2. **No virtualization.** Rows are painted by hand from a scroll offset. BMUX `VirtualList` and
   `MeasuredListIndex` are unused. The diff pane shows only the selected file.
3. **Per-frame recomputation.** A `SyntaxHighlighter` is constructed every frame and each visible
   row is re-highlighted every frame. The view-document cache returns a full clone on each access.
   The file tree rebuilds labels and tree state every frame.
4. **Incorrect context expansion.**
   * Expanded lines are read from the working-tree file on disk (`RepositoryFileRequest` has no
     revision), so commit, range, branch, staged, and deleted-file reviews show wrong or
     misaligned content.
   * Expansion state is keyed by file index plus a line range that changes after every step,
     making the result order-dependent and stale after refresh or reordering.
   * Only new-side line numbers are shown, the step is fixed at 20 lines, and expanded lines
     cannot be commented on.
5. **No visual system.** The review theme has five styles. File-tree and home rows are single
   formatted strings in one style; status letters, counts, and badges are unstyled; selection is
   reverse video. Home has a separate, unrelated theme.
6. **Weak entry flow.** "New review" asks for a title and opens a source-building mode; five
   single-key presets cover only working-tree states, last commit, and repository browsing. No
   branch, commit, pull-request, or stack selection exists.
7. **Host-owned product flow.** Home→review→home navigation lives in `packages/tui`
   (`code_review_launcher.rs`, `terminal_shell.rs`), duplicates `ReviewHomeOutcome`, and reloads
   the plugin runtime and reinitializes the TUI on every transition.
8. **Duplication.** The TUI redefines request/response types that exist in
   `bcode_code_review_models`.

### 2.1 V1 baseline measurements

Measured by the ignored test `v2::testing::v1_baseline::v1_baseline_benchmark`
(`cargo test -p bcode_code_review_plugin v1_baseline -- --ignored --nocapture`) on the phase 0
fixture (`FixtureSpec::benchmark()`). The fixture is 5,000 modified files with 50 rewritten lines
each, plus 20 added, 20 deleted, and 20 renamed files, a binary file, a unicode file, and a
40,000-line file with a quarter of its lines rewritten. Its working tree has 500 staged files,
500 unstaged files, and one file with both. A three-layer stacked branch chain sits on top of
the head commit.

Machine: Apple Silicon macOS (Apple M5 Pro, 15 cores, 48 GiB, macOS 27.2), rustc 1.99.0.
Profile: **debug** (unoptimized `test` profile; release builds were not run). The tables show
one run; repeat runs came within about 20%. Absolute values are much higher than an optimized
build would show, but they are the reference that V2 is compared against under the same
profile. Materialize times also depend on the filesystem cache: an earlier cold run measured
2.75 s for the branch compare. Fixture generation takes about 3.4 s.

| Measurement | Branch compare (`fixture/base...main`) | Working tree + index |
| --- | --- | --- |
| Diff files / added / removed lines | 5,063 / 264,024 / 264,024 | 1,000 / 10,002 / 10,002 |
| Repository files / branches / commits also returned | 5,044 / 13 / 5 | 5,044 / 13 / 5 |
| `review.workspace.materialize` (direct call) | 956 ms | 348 ms |
| JSON payload size | 104.3 MiB | 4.2 MiB |
| JSON encode / decode | 2,915 ms / 1,387 ms | 115 ms / 56 ms |
| Materialization → `ReviewSummary` conversion | 71 ms | 2.6 ms |
| `ReviewApp::new` | 74 ms | 6.3 ms |

Render frames, branch-compare review, 200×60, harness frame driver (render only; input
handling timed separately). Each interaction was sampled until 60 s had elapsed, with at least
10 frames:

| Interaction | Frames | Input avg | Render avg | Render p95 | Render max |
| --- | --- | --- | --- | --- | --- |
| First frame (first file, 200 added lines) | 1 | — | 3,077 ms | — | — |
| Next file (`n`), ordinary modified files (200 lines, 10 hunks) | 21 | 0.40 ms | 2,862 ms | 3,879 ms | 4,784 ms |
| First frame of the 40k-line file (31,997 diff lines) | 1 | — | 12,581 ms | — | — |
| Wheel scroll in the 40k-line file | 10 | 113 ms | 6,966 ms | 7,301 ms | 7,301 ms |

Next-file frames are steady at 2.6–2.7 s with two outliers. Renamed files (one hunk) and
deleted files (whole-file removal) sort before the ordinary files and are excluded from these
samples; their next-file frames measured about 0.5 s and 2.9 s respectively. Acceptance
criterion B targets a steady scroll frame of ≤ 2 ms p95.

#### V2 data layer (phase 1)

Measured by the ignored test `data::benchmark::data_layer_benchmark` in `bcode_code_review`
(`cargo test -p bcode_code_review data_layer_benchmark -- --ignored --nocapture`) on the
same fixture, machine, and **debug** profile as the V1 table. Times include the service call and
the JSON encode and decode of its response. Two runs were made; the commit-to-commit rows show
the later run (the earlier one came within about 10%), and the working-tree rows show the range
across both runs and both listings of a run, because they vary with git's stat refresh.

| Measurement | V1 (`review.workspace.materialize`) | V2 data layer |
| --- | --- | --- |
| Branch compare: resolve sources | — | 77 ms |
| Branch compare: file list (5,063 entries) | 956 ms + 2,915 / 1,387 ms JSON, all content | `review.index.get`: 523 ms cold (all pages), 73 ms warm (cached listing) |
| Branch compare: payload before the first file can render | 104.3 MiB | 330 KiB (first 1,000-entry index page); 1.68 MiB for the whole index |
| Hunks of one ordinary file (10 hunks, 100 changed lines) | included in the 104.3 MiB payload | 16.5 ms avg, 18.2 ms p95; 19.7 KiB |
| Hunks of the 40k-line file (31,997 diff lines) | included | 41 ms; 2.8 MiB at the default 20,000-line budget (truncated), 4.5 MiB untruncated |
| 20 context lines of the 40k-line file | read from the working tree (wrong revision) | 2.2 ms avg via `cat-file --batch`; < 0.01 ms from the blob cache |
| Working tree + index: file list (1,000 entries) | 348 ms + 115 / 56 ms JSON | `review.index.get`: 252–609 ms; 311 KiB |
| Working tree change token | — | `review.sources.token`: 225–310 ms |

Most of the cold index time is `git diff --numstat` computing line counts for every file; the
listing is cached for commit-to-commit sources, so later pages and reopening are cheap. Working-tree
listings and the change token are dominated by git re-checking files whose modification time
equals the index's (the fixture writes both within the same second); in an ordinary working tree
git's refreshed stat data makes these calls much cheaper. Listings with an index or working-tree
side are not cached: each page re-runs the listing, and a cursor from a listing that has since
changed is rejected with `cursor_stale`.

#### V2 document (phase 2)

Measured by the ignored test `document::tests::benchmark::document_benchmark` (`bcode_code_review`)
(`cargo test --release -p bcode_code_review document_benchmark -- --ignored --nocapture`) on the
same fixture, machine, and **debug** profile. The document is built from the real 5,063-entry
branch-compare index; scrolling then pages through the whole review 60 items at a time, keeping
the viewport anchored through splices as a virtual list does. Every hunk and blob request the
document issues is answered at once with synthetic content shaped by the entry's `+/-` counts, so
the numbers measure the document (needs, item construction, splices, residency), not git. Each
page read also resolves every item's key back to its index, as scroll anchoring does. The table
shows the first of two runs; the second came within 5%.

| Measurement | V2 document |
| --- | --- |
| Build from the index (5,063 files, 10,126 items) | 18 ms |
| Scroll the whole review (15,909 pages, 5,063 hunk loads, 18,564 splices) | 2.0 s total, page steps and page reads |
| Page step: viewport, needs, loads, and splices | 0.064 ms avg, 0.12 ms p95, 21 ms max (one large file load) |
| Page read: `item`, `sections`, and `index_of` for 60 items | 0.062 ms avg, 0.076 ms p95 |
| Resident content | peak 64.0 MiB at the default 64 MiB budget; 1,647 files resident at the end |

#### V2 screen (phase 3a)

Measured by the ignored test `tests::benchmark::screen_benchmark` (`bcode_code_review_ui`)
(`cargo test --release -p bcode_code_review_ui screen_benchmark -- --ignored --nocapture`) on the
same fixture, machine, and **debug** profile, at 200×60 through the harness frame driver against
the real V2 service in-process (branch compare, 5,063 files). Only `render` is timed; service calls
and highlighting run between frames, as on the host. One run is shown; a second came within
about 10%.

| Measurement | V1 | V2 screen |
| --- | --- | --- |
| Open: resolve sources and list the whole index | 956 ms materialize + 4.3 s JSON | 738 ms |
| First frame after the index (document built) | 3,077 ms first frame after `ReviewApp::new` | 68 ms first render (10,126 items measured); 208 ms until the first files' hunks are loaded and painted |
| Scroll the 5k-file review (wheel, page every 10th frame, 50-file jump every 100th) | 2,862 ms avg next-file frame, 3,879 ms p95 | 3,000 frames: 3.4 ms avg, 8.0 ms p95, 107 ms max (a frame measuring a freshly loaded file) |
| Jump to the 40k-line file, then load and show it | 12,581 ms first frame | 1.1 ms jump frame; 1.14 s until loaded (31,997 diff lines measured) |
| Scroll the 40k-line file | 6,966 ms avg, 7,301 ms p95 | 2,000 frames: 2.9 ms avg, 6.3 ms p95, 11 ms max |
| Resident content after the 5k-file scroll | whole review | 15.7 MiB, 436 files |

Two generic BMUX additions came out of profiling: `VirtualItemSource::measure_hint` lets a source
report exact heights of fixed-height rows and placeholders without constructing them (the first
render measured every item by construction before), and `diff_viewer::diff_row_height` measures a
wrapped diff row with the same wrap geometry as `layout_diff_row` without producing rows (loading
the 40k-line file previously built every line once only to measure it: 1.76 s). Criterion B's
2 ms p95 target is for an optimized build; these debug numbers are the like-for-like comparison
with the V1 table.

#### V2 screen with chrome and panels (phases 3b and 4)

The same benchmark after the review chrome landed: header, footer, and the file panel on the left
(44 columns at 200×60, so the document list is about 150 columns wide and still side by side).
Wheel events are sent over the document list. The "after the index" latency now starts when the
index arrives, so it includes building the document and the 5,063-file tree. One run is shown.

| Measurement | Phase 3a | Phases 3b and 4 |
| --- | --- | --- |
| Open: resolve sources and list the whole index | 738 ms | 829 ms |
| File tree from the opened document (5,170 rows) | — | 23 ms |
| First frame after the index | 68 ms first render; 208 ms with content | 73 ms first render; 251 ms with content (includes document and tree construction) |
| Scroll the 5k-file review | 3.4 ms avg, 8.0 ms p95, 107 ms max | 4.9 ms avg, 9.8 ms p95, 81 ms max |
| Jump to the 40k-line file, then load and show it | 1.1 ms jump; 1.14 s until loaded | 3.2 ms jump; 1.11 s until loaded |
| Scroll the 40k-line file | 2.9 ms avg, 6.3 ms p95, 11 ms max | 3.7 ms avg, 5.7 ms p95, 11 ms max |
| Resident content after the 5k-file scroll | 15.7 MiB, 436 files | 15.5 MiB, 432 files |

A frame now lays the body out twice (a probe pass that sizes the document, then the real pass)
and paints the header, the file tree's visible rows, and the footer. A side experiment over 600
scroll frames measured 3.6 ms avg (4.9 ms p95) with the file panel shown and 4.1 ms (6.6 ms) with
it hidden, where the document is wider: the panel itself is cheap and the list's width dominates.

#### V2 screen with review interaction (phase 3c)

The same benchmark after review interaction landed (annotations, composer, pointer, search,
selection). One run is shown.

| Measurement | Phases 3b and 4 | Phase 3c |
| --- | --- | --- |
| First frame after the index | 73 ms first render; 251 ms with content | 83 ms first render; 423 ms with content (now also lists the review's annotations) |
| Scroll the 5k-file review | 4.9 ms avg, 9.8 ms p95, 81 ms max | 6.9 ms avg, 12.9 ms p95, 54 ms max |
| Jump to the 40k-line file, then load and show it | 3.2 ms jump; 1.11 s until loaded | 2.7 ms jump; 1.11 s until loaded |
| Scroll the 40k-line file | 3.7 ms avg, 5.7 ms p95, 11 ms max | 4.9 ms avg, 7.9 ms p95, 37 ms max |

Most of the added frame time is registering the painted diff text as BMUX selection fragments
(one per grapheme, so selections and copies map to exact cells). The fragments are built once per
item and width with its rows; each frame clones and translates the visible ones. Generating them
per frame instead measured 8.6 ms avg (13.5 ms p95) on the 5k-file scroll. BMUX has no way for a
paint pass to say that no selection controller will read the scene; such an opt-in would remove
this cost where selection is not used.

#### V2 screen at phase 3 close (review pass)

The same benchmark after the phase 3 review. The benchmark now starts the "after the index"
clock as soon as the document exists and reaches first content by running async work only
(`Scenario::step_async`); the harness runs blocking-pool work such as syntax highlighting
serially on the test thread, so settling it there (as earlier rows did) added every visible
file's highlighting to the latency. Rows paint unhighlighted until highlighting lands. One run is
shown.

| Measurement | Phase 3c | Phase 3 close |
| --- | --- | --- |
| First frame after the index | 83 ms first render; 423 ms with content (settled, including highlighting) | 59 ms first render; 289 ms with content; 379 ms highlighted |
| Scroll the 5k-file review | 6.9 ms avg, 12.9 ms p95, 54 ms max | 7.2 ms avg, 13.8 ms p95, 58 ms max |
| Jump to the 40k-line file, then load and show it | 2.7 ms jump; 1.11 s until loaded | 2.9 ms jump; 1.14 s until loaded |
| Scroll the 40k-line file | 4.9 ms avg, 7.9 ms p95, 37 ms max | 5.2 ms avg, 8.4 ms p95, 40 ms max |

Criterion B status (debug build only; an optimized build was not run, so the optimized targets
are unverified):

* First frame with tree and content ≤ 150 ms: 289 ms in debug. Of that, about 40 ms is building
  the document and the file tree, 59 ms the first render (measuring all 10,126 items through
  `measure_hint`), and the rest the in-process hunk service calls for the first files (git,
  JSON encode and decode) and the concurrent annotation listing.
* Steady scroll ≤ 2 ms p95: 13.8 ms p95 in debug. A sampling profile of the scroll frames
  attributes about 64% to painting (diff text writes, the file tree's visible rows, and 15%
  registering selection fragments), 26% to laying out the list (building newly visible rows,
  of which selection fragments are 40%, and measuring newly loaded files), and 5.5% to the
  canvas fill. No frame rebuilds unaffected items, and no algorithmic hotspot remains; the
  selection-fragment cost is BMUX's per-grapheme fragment model (the host does not yet read
  plugin-surface selections).
* Resident content stays at the 64 MiB budget (15.5 MiB, 432 files after the 5k-file scroll).
* No service call or syntax highlighting on the render path: confirmed by review (`render`
  only reports the viewport; `poll` issues requests and highlighting runs on the blocking pool).

#### V2 screen, optimized build (performance pass)

The same benchmark built with the release profile (`cargo test --release -p
bcode_code_review_ui screen_benchmark -- --ignored --nocapture`), same machine and
fixture. "Before" is the phase 3 close code measured at the start of this pass (two runs; the
earlier 2.82 ms p95 recorded during phase 4 was not reproduced); "after" is two runs after the
changes below. Timings include the harness's own per-frame work (a freshly allocated 200×60
buffer and the cloned selection scene of every frame), which the host does not repeat.

| Measurement | Before | After |
| --- | --- | --- |
| Open: resolve sources and list the whole index | 534 ms | 526–544 ms |
| First frame with content after the index | 136 ms; highlighted after 163 ms | 142–165 ms; highlighted after 167–190 ms |
| Scroll the 5k-file review (3,000 frames) | 2.4–2.5 ms avg, 4.6–5.4 ms p95, 122–154 ms max | 0.74–0.86 ms avg, **1.39–1.56 ms p95**, 7–13 ms max |
| Jump to the 40k-line file, then load and show it | 0.6–0.8 ms jump; 538–573 ms until loaded | 0.6 ms jump; 497–616 ms until loaded |
| Scroll the 40k-line file (2,000 frames) | 1.2 ms avg, 2.0 ms p95 | 0.59–0.61 ms avg, 0.87–0.90 ms p95 |

Profile state (macOS `sample` over the 5k-file scroll, unstripped release build):

* **Before.** About 40% of each frame was selection metadata: cloning one BMUX
  `SelectionFragment` per painted grapheme (three heap strings each) into the frame, the clone
  of the frame's scene, and dropping it, plus formatting fragment ids when rows were built. The
  rest: laying out the list (20%, mostly building newly visible rows), painting text and fills,
  the file tree's rows, and the harness's buffer allocation.
* **Change.** BMUX gained `SelectionTextRun`: a row of selectable text registered as one value
  (with a shared cell table) and materialized into fragments only when the scene is queried.
  Diff rows register one run per painted row segment from `DiffRowLayout::selection_cells`, built
  once per item and width with the row cache.
* **After.** No selection cost remains on the frame path. The frame is now generic painting
  (`write_line` and `fill` cell writes, about a third), laying out the list (about a quarter:
  rows built for newly visible items and measurement of newly loaded files), the file panel's
  visible rows (about a fifth), and the harness's buffer allocation (about a tenth). No single
  hotspot remains; page and jump frames (one frame in ten) set the p95.
* Criterion B: the steady scroll target (≤ 2 ms p95) is met in an optimized build for both the
  5k-file review and the 40k-line file. The first frame with content stays near the 150 ms
  target (142–165 ms across runs): its two renders take 11–15 ms, and the rest is the in-process
  service calls (git, JSON) for the first files' hunks.

#### V2 screen on large terminals (phase 8)

Measured at 200×60, 300×90, and 400×120 with `screen_benchmark_sizes`, and in the real app
under the chat host; the tables and their attribution are in 13.2.

## 3. Layered architecture

```
 packages/code-review/models     contracts: sources, revisions, index, hunks, blobs, anchors,
                                 stacks, workspace schema, service + provider interfaces
            ▲
 plugins/code-review-plugin
   service/   (daemon side)      git access, index/hunk/blob ops, workspace persistence,
                                 live refresh, stack resolution via provider interface
   document/  (renderer-neutral) review document: files, sections, gaps, expansion sets,
                                 threads, anchors, viewed state, navigation targets
   tui/       (BMUX adapter)     routes (home, wizard, review), components, theme, input
            ▲
 plugins/github-review-publisher-plugin
                                 publisher + importer + NEW review-source provider (PR list,
                                 stack graph) built on packages/forge
```

* `document/` must not import BMUX, crossterm, or terminal types. It is unit-testable and is the
  only owner of review structure. `tui/` only adapts it. If mechanical enforcement is wanted, the
  module can be extracted to a domain crate later; it is not created speculatively now.
* V2 source lives beside V1 in the plugin (`src/v2/…`) so V1 can be deleted wholesale.

## 4. Data layer

### 4.1 Revision-pinned sources

Every diff source resolves to concrete object IDs when a review is opened or refreshed:

* `ResolvedSource { source_id, base: RevisionRef, head: RevisionRef, mode, paths, line_range,
  part }` where a `RevisionRef` is a commit OID, the empty tree (base of a root commit or an
  unborn branch), the index, or the working tree. `mode` is `diff` or `browse` (repository, file,
  and file-range sources browse whole files at the working tree, limited to `paths` and
  `line_range`).
* `review.sources.resolve` resolves a workspace's included sources:
  * unstaged: index → working tree; staged: `HEAD` → index; both: `HEAD` → working tree;
  * last commit and `Commit { rev }`: first parent (or the empty tree) → commit;
  * ranges and branch compares: base (or the merge base with merge-base semantics) → head;
  * `CurrentBranch`: merge base of the base and `HEAD` → `HEAD`; without an explicit base, the
    base is the branch's upstream, else `origin/HEAD`, else the first of `main`, `master`,
    `trunk` that exists and is not the current branch;
  * `Commits { revs }`: one range when the selection is a single first-parent chain; otherwise one
    part per commit (against its first parent) in selection order, each carrying `part { index,
    count, label }`;
  * pull-request kinds resolve from provider answers carried in the request (4.4): `PullRequest`
    is the merge base of its base branch and head → head; `StackLayer` is the merge base of the
    layer below's head (the trunk for the bottom layer) and the layer's head → the layer's head;
    `Stack` is the merge base of the trunk and the top layer → the top layer's head. Stack and
    stack-layer sources also return their pinned layers (`ResolvedReviewStack`). Fetched object
    ids must exist locally, else `unresolved_revision`;
  * a pull-request kind without a provider answer (no provider enabled, or none with that
    provider id) returns `provider_required`; a provider error returns `provider_failed`; unknown
    kinds return `unsupported_source_kind`; revisions that do not exist return
    `unresolved_revision`; file paths that fail confinement return `path_rejected`. A failing
    source never fails the others.
* Workspaces persist the last resolved OIDs per source (`reviewed_base_oid`, `reviewed_head_oid`)
  so re-review can compute what changed since the previous pass (section 7.4).
* New source kinds (additive): `Commits { revs }` (non-contiguous selection), `PullRequest {
  provider, repository, number }`, `StackLayer { provider, repository, number }`, `Stack {
  provider, repository, top }`, and `CurrentBranch { base: Option<String> }` (auto-detected base).

### 4.2 Service operations (additive to `bcode.code_review/v1`)

| Operation | Returns | Bound |
| --- | --- | --- |
| `review.sources.resolve` | `ResolvedSource` values plus per-source diagnostics (4.1) | one `rev-parse` per revision |
| `review.sources.token` | cheap change token for working-tree and index sources | three git calls; no diff |
| `review.index.get` | per-source file list: paths, status, rename info and similarity, old/new blobs (object id, working-tree marker, or absent), modes, +/- counts, flags (binary, mode changed, symlink, submodule) | paged (default 1,000, max 5,000); cheap (`--raw -z --numstat`) |
| `review.file.hunks.get` | parsed hunks (old/new start and length; lines with kind, old and new line numbers, text) for one file of one resolved source, plus the file's patch-id | per file (memory proportional to that file's diff); a line budget (default 20,000) returns a truncation marker |
| `review.blob.lines.get` | line range and total line count of a blob, by object id or by (revision, path) for commits, the index, and the working tree | range-bounded (max 5,000 lines) |
| `review.refs.search` | branches/tags matching a query | paged |
| `review.commits.page` | commit log page with subject, author, date, refs, parents | paged; anchored to the first page's start commit |
| `review.files.search` | repository paths matching a query (repository browse sources) | paged |
| `review.repo.state` | current branch, `HEAD`, the base `CurrentBranch` detects, its merge base, commits ahead (capped at 10,000), unstaged and staged file and line counts | two `diff --shortstat`, base detection, one `merge-base`, one capped `rev-list --count` |
| `review.revisions.compare` | whether a previously reviewed commit still exists, whether it is an ancestor of the current one (fast-forward), and the commits added (capped at 10,000) | one `cat-file -e` per commit, one `merge-base --is-ancestor`, one capped `rev-list --count` |
| `review.patch_ids.get` | patch ids (the `review.file.hunks.get` identity) of up to 500 paths of one revision pair; paths without a textual difference and binary files are absent | one `git diff -U0` limited to the paths |
| `review.publish.bundle.preview` / `review.publish.bundle.submit` | a built-in publisher's preview or submit of a bundle the client built (`PublishBundleRequest`); no publish record is saved by the operation | the bundle's size |
| `review.suggestion.delete` | deletes one suggestion from one review scope (`DeleteReviewSuggestionRequest`); used to move suggestions when a transient review is saved (7.5) | one row |

* Paged operations return an opaque cursor bound to the listing it came from; a cursor from a
  listing that has since changed is rejected with `cursor_stale`.
* The patch-id is part of the hunks response, not the index: computing it needs the file's diff,
  which the index deliberately does not load. It is a SHA-256 over the changed lines in order (`+`
  or `-` plus the raw line text, without line numbers or context) and any mode change; it is
  computed over the whole diff even when the response is truncated, and is absent for binary
  files and whitespace-ignoring requests. A type change (for example a file replaced by a symbolic
  link), which git prints as a deletion and an addition of the same path, is returned as one file
  whose hunks cover both.
* Blob reads use one long-lived `git cat-file --batch` process per repository root, held in a
  bounded registry (at most 4 processes, least recently used evicted, processes idle for 5
  minutes stopped on the next service call, a failed process restarted once per request, and a
  process that reports an object above the 64 MiB read bound stopped instead of drained) and a
  32 MiB blob cache keyed by full object id. Index reads name stage 0 explicitly (`:0:<path>`).
  Repository-relative paths are confined in every operation: absolute paths, `.`/`..` segments,
  and `.git` segments are rejected. Working-tree reads come from disk: a symbolic link reads as
  its target path (git's blob content for links), and any other path must canonicalize inside
  the repository root.
* `review.workspace.materialize` served V1 only (V2 never called it) and was removed in phase 7.
  The bundle operations still materialize workspaces internally (`review.bundle.get`).
* **Refresh (deviation from the original plan).** `bcode_git_live` is not available on `master`,
  so working-tree and index sources are refreshed on an explicit refresh and when the terminal
  regains focus. The client first calls `review.sources.token` and only re-lists sources when the
  token changed. The token covers the `HEAD` OID, the staged index content (`git ls-files
  --stage`), and, when a working-tree source is open, a digest of `git status --porcelain=v2`
  plus the size and modification time of every path with unstaged changes (status lines do not
  change when an already modified file is edited again). Index stat data is excluded because git
  rewrites the index when it refreshes stat information. There is no timer polling; the token can
  be driven by `bcode_git_live` notifications once that lands.

### 4.3 Client-side residency

* The file index is fully resident (it is small per file).
* Hunks are loaded for the viewport plus a prefetch window in both directions and evicted
  least-recently-used beyond a configured memory budget. Eviction never removes files at or
  around the scroll anchor.
* Unloaded files occupy estimated height (from `+/-` counts) so the scrollbar and jump targets work
  across the whole review before content arrives.

* **Implementation (phase 2).** The client reports the visible item range; the document derives
  a protected window (default 120 estimated rows each side of the viewport, loaded even over the
  budget) and a prefetch window (default 400 rows, loaded while under the budget). Content beyond
  the budget (default 64 MiB, counting hunk and context text plus per-line and per-item
  overhead) is evicted least recently used, never inside the prefetch window, which keeps the
  window from thrashing. Resident files are kept ordered by last use, so enforcing the budget
  costs the window plus the evicted files, not the whole review. Requests carry tickets; results
  for tickets that are no longer outstanding (the file was evicted or re-requested) are ignored.

### 4.4 Review-source provider interface (new)

`bcode.review_source/v1` lets provider plugins advertise and resolve pull-request sources, keeping
provider behavior out of the code review plugin (same pattern as publisher/importer). Contracts live
in `packages/code-review/models` (`review_source.rs`) and carry no provider-specific fields.

| Operation | Request → response |
| --- | --- |
| `review_source.manifest` | → provider id (stored in source kinds), label, capabilities (`pulls_list`, `stack_resolve`, `pull_fetch`) |
| `review_source.pulls.list` | repository (or detect from remotes) → repository, default branch, open `PullRequestSummary` values (number, title, author, base and head branch, head OID, base OID if known, review requested from me, draft, updated, URL), review-requested-from-me first, then most recently updated |
| `review_source.stack.resolve` | pull request number → `ReviewStack`: trunk, whether it is the default branch, layers trunk → top (each a summary plus its parent layer), siblings, warnings |
| `review_source.pull.fetch` | pull request number and optional base branch → `ReviewPullFetchResult`: local head/base refs and their OIDs |

* **Client-side routing (deviation from a service-side lookup).** Plugin services cannot call
  other plugin services: the host's nested-service bridge serves host interfaces only. As with
  publisher routing, the client discovers providers (`plugin_services()` filtered by interface,
  then `review_source.manifest` to match the provider id), asks the matching provider to resolve
  stacks and fetch every needed layer, and sends the answers with `review.sources.resolve` as
  `provider_resolutions` (`Resolved { stack, fetched }` or `Failed { code, message }`). The
  routing lives in `packages/code-review/src/provider.rs` behind a `ProviderTransport` trait (the review
  app's host plugin services in production, fakes in tests). With the provider plugin disabled nothing answers, and only the
  provider-backed sources report `provider_required`.
* **Stack detection** (GitHub implementation): from the open pull requests whose head branch is in
  the repository itself (forks are excluded), the chain extends down while some pull request's
  head is the current base branch and stops at the default branch; it extends up while exactly one
  pull request targets the current head. Several pull requests on one layer stop the upward walk
  and are reported as siblings; several open pull requests with the same head branch below pick
  the most recently updated, with a warning. A missing layer (for example a closed pull request
  in the middle) ends the chain with `rooted_at_default_branch: false` and a warning. Cycles stop
  at the first repeated pull request with a warning. Chains are cut at 64 layers. No
  Graphite-specific metadata is used.
* **Fetching** runs `git fetch --no-tags --no-write-fetch-head --refmap= <remote>` with explicit
  refspecs `+refs/pull/N/head:refs/bcode-review/pull/N/head` and
  `+refs/heads/<base>:refs/bcode-review/pull/N/base`, so only review-owned refs change (no
  branches, no remote-tracking refs). The remote is the local remote that names the repository
  (`origin` preferred), else its HTTPS URL. Each stack layer is fetched with its own base
  branch. Layers are fetched one at a time when a source resolves (at most 64), and fetched
  review refs are not pruned yet.
* **Whole-stack attribution** (`review.stack.attribution`, code review service): given a
  `ResolvedReviewStack` and one path (plus the base path for renames), returns for the
  whole-stack diff the new-side line ranges each layer wrote last and the old-side line ranges
  each layer removed. The file is followed from the stack base through each layer head, applying
  the zero-context diff between consecutive heads (for a restacked stack, exactly each layer's
  own diff) and mapping earlier lines forward through later hunks; renames and deletions are
  followed. Work is one blob read and one `git diff -U0` per layer for one file, with at most
  2,000,000 tracked lines. A comment publishes to the layer that wrote its anchor line (7.3).
  Each range also carries `layer_line` (serde-defaulted): where the range starts in that layer's
  own diff, on the new side the line in the layer's head and on the old side the line in the head
  below it, so a comment can be published at the right line of the layer's pull request; ranges
  merge only while those lines stay consecutive.

The GitHub implementation lives in `plugins/github-review-publisher-plugin` (`review_source.rs`)
using `packages/forge`, which now also reports each pull request's author, base OID, and whether a
review is requested from the authenticated user, and the repository's default branch.

## 5. Review document (`document/`)

### 5.1 Items

The review is one keyed sequence of items:

* `SourceHeader` (when more than one source or a stack layer is shown)
* `FileHeader { file_key }`
* `HunkHeader { file_key, hunk }`
* `Line { file_key, old_line, new_line, kind }` (unified) or
  `LinePair { file_key, old, new }` (side-by-side; pairing computed in `document/`)
* `Gap { file_key, old_range, new_range }` — unexpanded context between, before, or after hunks
* `Thread { thread_id }`, `Composer { anchor }`, `AgentThread { thread_id }`
* `FileNotice { file_key, kind }` — binary, too large, loading, unavailable

Item keys are stable identities (`file_key` = source id + part index for multi-part sources +
path; lines keyed by side and line number; threads by id), never list indices. Persisted keys
(`ReviewItemKey`, `ReviewFileExpansion`) carry the optional part index for the same reason. Layout mode (unified/split) changes item
construction but not item identity for threads and anchors.

**Implementation (phase 2).** `packages/code-review/src/document/` implements this section with a renderer-neutral
`ReviewDocument`:

* Items are built per file block; a file whose content is not resident is its `FileHeader` plus
  one `Placeholder` item carrying an estimated height (changed lines plus a header and six
  context lines per estimated hunk of ten changed lines). File-level annotations stay visible for
  non-resident files; line annotations appear once the file is resident.
* The sequence API is item count, item at index, index of key (layout-mapped), sticky sections
  (file header level 0, hunk header level 1; expanded context and gaps belong to the file only),
  and splices (`start`, `removed`, `inserted`, `revision`) with prefix/suffix trimming so an
  expansion only replaces the gap region. Block offsets live in a Fenwick tree and each block
  keeps a lookup of its items (line items by side and line number, other items by key), so
  `item`, `sections`, and `index_of` cost `O(log n)`; thread keys find their block through a
  key map. Switching layout rebuilds every file, because placeholder estimates depend on it.
* Side-by-side pairs removed and added runs of a hunk positionally; a row with both a removed and
  an added line carries an intra-line emphasis hint. A line key resolves to the item showing
  that side's line in the current layout: a unified line key to the row containing it, a row
  key to its new-side line (its old-side line when it has none).
* Navigation (6.2) reports a target inside a non-resident file at that file's header with
  `needs_load`, at the position of its placeholder in the motion's order (after the file's
  file-level annotations); the client loads the file and repeats the motion from its original
  position.
* `SourceHeader` items appear when there is more than one source (or part) or a stack source.
  Whole-stack sources request `review.stack.attribution` for each resident text file and label
  file headers, hunk headers, and changed lines with the layer that wrote or removed them.
* Browse sources have no hunks: their files are resident without a request, and their
  `line_range` (or the whole file) starts revealed. Browse expansion is not persisted.
* The document performs no I/O. It does not depend on BMUX, crossterm, or terminal types; this is
  enforced by review within the plugin crate (section 3).

### 5.2 Context expansion

* Per file, expansion state is an interval set of revealed new-side lines (`BTreeMap`-backed,
  merged on insert). Old-side lines are derived from hunk offsets, so both line numbers are always
  shown.
* Gaps are recomputed from hunks minus revealed intervals. Expansion is therefore idempotent,
  order-independent, and survives refresh as long as the file's hunks are unchanged. If the hunks
  change, revealed intervals are clamped to the new gaps.
* Actions on a gap: reveal N above, reveal N below (default 20; numeric count prefix supported),
  reveal all, and reveal the whole file. Mouse: clicking the upper or lower half of a gap row
  reveals in that direction.
* Content comes from `review.blob.lines.get` on the correct side's blob OID.
* Expanded context lines are anchorable. Publishers that cannot anchor outside diff hunks use the
  existing unmapped-to-summary fallback.

**Implementation (phase 2).**

* The context side is the new side, or the old side when the new side is absent (deleted
  files); revealed intervals, gap keys, and blob reads use that side. A region between hunks has
  a constant offset to the other side, so both line numbers come from hunk headers.
* The file's line count comes from the first of: a whole-file hunk (added and deleted files),
  a last hunk with fewer trailing context lines than were requested (git ran out of lines), or a
  `review.blob.lines.get` response (a zero-line probe, or the first revealed chunk). Until then
  the region after the last hunk is an open-ended gap. A truncated diff has no gap after its last
  returned hunk; binary files and submodules have no gaps.
* "Reveal N above" reveals the gap's last N lines (directly above the content below it); "reveal
  N below" reveals its first N lines. Operations on a gap address it by key (file and first hidden
  line); a stale key whose first line is already revealed does nothing. The revealed set is
  normalized after every change (hunk coverage removed, clamped to the line count), so its
  representation is independent of operation and load order.
* Revealed lines whose text is not resident are `Loading` until their blob range arrives;
  failed ranges are `Unavailable` with the reason and are not retried automatically.

### 5.3 Anchors

Anchors keep the existing source-identity model and add the resolved head OID. An anchor whose
line no longer exists after refresh is shown as outdated in its thread rather than silently moved.

**Implementation (phase 2).** Drafts sharing a thread id form one `Thread` item; AI exchanges
are `AgentThread` items; suggestions attach to the exchange from the same session, else to an
annotation with an identical anchor, else to their own agent item. A line anchor is shown after
its last line (in either layout), or after the gap that hides that line. It is outdated when a
range endpoint no longer exists, or when a single-line anchor's line exists but is no longer the
same kind (for example a removed line that is now context); outdated anchors are shown at the
file level, and anchors to files no longer in the review at the review level.

**Implementation (phase 3c).** `DraftAnchor::head_oid` (serde-defaulted, omitted when absent)
records the commit the file's source head resolved to when the anchor was made, when the head is
a commit (working-tree and index heads record none). Every V2 anchor carries it
(`ReviewDocument::head_oid`). A line anchor made at a head other than the source's current head
is outdated with `OutdatedReason::RevisionChanged` and shown at the file level, since its line
numbers may name other content; anchors without a recorded head keep the line checks above.
Thread identity ignores the head: `thread_id` hashes the anchor with `head_oid` cleared, so V1
(which records none) and V2 address the same thread, and anchors without a head hash exactly as
before. The service stores it in `draft_threads.head_oid` (migration `019_thread_head_oid_column`);
AI exchanges and suggestions keep it in their anchor JSON.

**Implementation (exact outdated anchors).** `DraftAnchor::blob_oid` (serde-defaulted, omitted
when absent) records the blob of the anchored file side when a line anchor is made: the new
side, or the old side for anchors ending on a removed line, when that side is a blob object
(working-tree content records none). A line anchor with a recorded blob is placed by its lines
while the file's current blob on that side is the same, whatever the head commit, and is
outdated with `OutdatedReason::ContentChanged` (labeled "file changed") once it differs or is not
a pinned object. Anchors without a blob (V1, and V2 anchors made before the field) keep the
head check above. Thread identity also ignores the blob. The service stores it in
`draft_threads.blob_oid` (migration `020_thread_blob_oid_column`). For a range spanning both
sides only the end side's blob is recorded; the other side's lines are checked by existence and
kind.

### 5.4 Viewed state

A viewed mark is `ViewedMark { path, old_oid, new_oid, patch_id }`: the identity of both blob
sides (an object id, `absent` for a missing side, or unknown for working-tree content) and, when
hunks were loaded, the patch-id. A file matches a mark with the same path as follows, in order:

1. A mark of unknown identity shows as viewed until the file's identity is first known; the
   client then re-records it with the current identity. A mark has an identity when it carries a
   patch-id or both blob sides are pinned; migrated path-only marks and working-tree content
   marked before its hunks were loaded have none.
2. Both blob identities known on the mark and the file, and equal: viewed. This is checked first
   because the index carries blob identities without loading hunks.
3. Otherwise, both patch-ids known and equal: still viewed (the patch survived a rebase or new
   commits elsewhere); the client re-records the mark with the new blob identities. Patch-ids are
   compared once the file's hunks are loaded, so until then such a file shows as unviewed.
4. Otherwise: unviewed.

`viewed_files` stays the authoritative set of viewed paths so V1 and the home screen keep
working: V2 writes both, a viewed path without a mark is a legacy mark of unknown identity, and a
mark whose path V1 has unmarked is ignored. V1 viewed toggles (and mark-all / unmark-all) also
drop the V2 marks of the paths they change, so a path V1 marks viewed again reads as a legacy
mark rather than being judged by a stale identity.

**Implementation (phase 2).** The document matches the workspace's effective marks against each
file's index blob identities and, once its hunks were loaded, its patch id (which is kept after
eviction), reports files whose marks should be re-recorded, and counts viewed files for progress.

## 6. Review screen (`tui/`)

### 6.1 Layout

```
┌ header ─ title · source chips · viewed 12/48 ▰▰▰▱▱ · 3 drafts · publish ▾ ─────────────┐
│ files (tree)        │ ▍src/lib.rs  M  +12 −3                      (sticky file header) │
│ ▾ src            ●3 │ @@ fn load  (sticky hunk header)                                 │
│   ✓ lib.rs M +12−3  │  41  41   let x = …                                              │
│     ui.rs  A +80    │      42 + let y = …                                              │
│ ▸ tests             │ ┄┄┄ ↑ 20 · 132 hidden lines · ↓ 20 · all ┄┄┄                     │
│                     │ 💬 thread …                                                      │
├─────────────────────┴──────────────────────────────────────────────────────────────────┤
│ key hints (contextual)                                       status / toasts            │
└────────────────────────────────────────────────────────────────────────────────────────┘
```

* Resizable file panel; collapsible to give the diff the full width.
* Optional threads panel (right or bottom), toggled; not a sidebar tab.
* Unified, side-by-side, or automatic by width (BMUX `DiffViewerLayout`). The default follows the
  existing `[tui.diff_viewer]` settings (`layout = "auto" | "unified" | "side_by_side"`,
  `side_by_side_breakpoint = 120`), overridable in the code review plugin's configuration. The
  in-review layout toggle persists only to user state, never to configuration files.
* Wrap toggle; hide-whitespace toggle (re-requests hunks with `-w`).
* Scrollbar over the whole review, with markers for threads and the current file.

**Implementation (phases 3b and 4).** `packages/code-review-ui/src/` (`chrome.rs`, `panels.rs`, `files.rs`):

* **Body.** A BMUX `panel_group` of bordered BMUX `pane`s: the file panel, the review document,
  and the threads panel. Dividers lie on the panes' shared borders and can be dragged; the focused
  pane's border uses the focused-border role. Each frame lays the body out twice, first with a
  probe in the document's place (to learn its exact size), then for real after the document list
  is prepared at that size; painting, hit regions, and event routing all use the second layout.
* **Narrow-width degradation** (`panels.rs`), decided per frame from the body size:
  1. Below 24 columns or 5 rows the document takes the whole body without pane chrome and no
     side panel shows.
  2. The file panel shows when the user opened it (from 60 columns of body), or when nobody chose
     and the body has at least 100 columns. Its width is the user's (default a quarter of the
     body, 18 to 44 columns), clamped so the document keeps 40 columns.
  3. The threads panel shows when opened: to the right from 150 columns, under the document from
     20 rows, otherwise not at all.
  4. Focus never stays on a hidden pane; it returns to the document.
* **File panel.** A BMUX `text_input` filter row over a BMUX `tree_view` in a `scroll_view` (the
  pane border carries its scrollbar). One group per source (or part) when there is more than one;
  directories in path order with single-directory chains compacted (`packages/code-review-ui/src/`); files in
  document order. File rows: status letter in its status color, the name (muted when viewed,
  crossed out when deleted), and right-aligned decorations, leftmost dropped first on narrow
  rows: stack layers, drafts, threads, a five-cell change bar (cells by size, split by
  added/removed share), `+/−` counts (`bin` for binary files), and the viewed mark. Directory
  rows: threads, aggregate counts, and viewed progress (`v/n`, or `✓ n` when all are viewed).
  The tree is built once per document (23 ms for 5,063 files); afterwards only files whose
  header the document rebuilt (noted from splices) refresh their row and ancestors in place, and
  the tree view's `items_revision` changes only then, so its projection is reused across frames.
  Clicking a directory toggles it (BMUX `toggle_on_click`).
* **Filter.** `f` focuses the filter; every word must occur in the path (case-insensitive);
  matches keep their directories, which cannot collapse while filtering. `enter` opens the first
  match and moves to the tree, `down`/`tab` move to the tree, `esc` clears (then leaves). Typing
  never reaches review actions.
* **Tree ↔ document.** Selecting a file row (click, `j`/`k`, arrows, pages) moves the document
  cursor to its header; `enter` also focuses the document. When the cursor reaches another file
  (keys, scrolling, wheel), the tree selects it, or its outermost collapsed directory, and
  scrolls it into view. Tree-originated jumps do not echo back.
* **Header** (`chrome.rs`): title, source chips (BMUX `badge`; a single source named like the
  review is not repeated, more than two collapse into `+N`), `+/−` totals, `viewed x/y` with a
  BMUX `progress_bar`, the draft count, the layout mode, and a BMUX `split_button` that opens
  the publish flow (its menu: publish, import an external review, changes since the last review,
  mark the pass complete; 7.6). Parts are measured by their own
  component or text width. Elision order when narrow: publish, the second chip (`+N`), drafts,
  totals, the first chip, the progress bar, the mode, the viewed count; parts are dropped while
  the title would get fewer than its width (at most 40) cells, then the title is truncated.
* **Footer.** BMUX `key_hint_bar` with contextual hints from the keymap (expansion first on a
  gap; tree, filter, and threads hints when those have focus); hints that do not fit whole are
  dropped from the end, never cut. A BMUX `status_bar` message (and the count prefix) sits on
  the right, using at most half the row.
* **Toasts.** BMUX `toast_stack` (card style) at the bottom right of the body for async results
  and errors: review refreshed, refresh failed, failing sources, layout save failed, the publish
  placeholder. Information expires after 4 s, errors after 10 s; at most three show.
* **Threads panel.** Lists threads and AI answers from the document (selecting one moves the
  cursor to it); until annotations load (phase 3c) it shows a BMUX `empty_state`.
* **Persistence.** File panel visibility and width, and threads panel visibility and size, are
  kept in the workspace's `presentation_v2.panels` (`ReviewPanelState`) and restored when the
  surface opens a workspace. Durable workspaces are saved with `review.workspace.update` (changes
  during a save are saved after it); transient workspaces keep the state in memory.
* **Keys** (scope `code-review`; `code-review-v2` before phase 7): `tab` next pane, `t`
  show/hide the file panel, `f` filter, `>`/`<` widen/narrow the file panel, `T` threads panel.
  In the tree: `j`/`k`/arrows, pages, `g`/`G`, `enter` open, `space` fold, `left` collapse or go
  to the parent, `right` expand, `esc` back to the document (scope `code-review-files` since phase 6 part 2; the threads
  panel's `enter`, `esc`, and `left`/`right` filters are scope `code-review-threads`).

### 6.2 Interaction

* Keyboard: line/page/half-page scrolling, next/previous file, hunk, thread, and unviewed file;
  `v` mark viewed and advance; count prefixes; `/` search within the review; `:` or `ctrl-p`
  command palette for every action; `?` help generated from the keymap.
* Mouse: click files, gaps, thread actions; drag across lines to select a range and comment on it;
  hover shows a `+` comment affordance in the gutter; wheel scrolling uses `ScrollMotion`.
* Composer: inline under the anchor, using the shared prose editor; markdown preview toggle.
* AI questions keep using `PluginTuiAction::OpenSession`; returning restores the exact position.
* All keys are declared in the plugin manifest keymap (configurable), as today.

**Implementation (phase 3c).** `packages/code-review-ui/src/` (`notes.rs`, `composer.rs`, `agent.rs`,
`palette.rs`, `surface/annotate.rs`, `surface/pointer.rs`, `surface/find.rs`):

* **Annotations** (`notes.rs`). Drafts, AI exchanges, and suggestions load through the existing
  operations (`draft.list`, `review.ai_exchanges.list`, `review.suggestions.list`) with V1's
  workspace scope, so V1 and V2 read and write the same threads. Changes are optimistic and go
  to the document at once; their calls run one at a time in order (`draft.save`, `draft.update`,
  `draft.delete`, `thread.resolve`, the new `thread.classify` for kind and severity,
  `review.ai_exchange.delete`), each reconciled by its job and rolled back with a toast when it
  fails. A draft keeps a local id until saved and cannot be edited or deleted meanwhile.
* **Composer** (`composer.rs`). The document's composer item under its anchor: a title (anchor,
  kind, severity), a BMUX `TextInputComponent` with the shared chat-composer editing policy
  (`enter` submits, `shift+enter` adds a line, 3 to 8 rows), and clickable buttons (submit,
  switch comment/question, preview, cancel). `ctrl+p` swaps the editor for a markdown preview.
  Its geometry is one `ComposerLayout` read by measuring, painting, and clicks; the list
  re-measures it on every edit. Scope `code-review-composer`: `ctrl+s` save, `ctrl+p`
  preview, `ctrl+t` comment/question, `alt+k` kind, `alt+s` severity, `esc` cancel. Typing never
  reaches review actions. Empty text is discarded.
* **Threads and answers.** Thread rows show a header (count, kind, severity, open/resolved,
  outdated reason, saving), markdown comments (bounded), and actions (Reply, Edit, Delete,
  Resolve/Reopen, Ask Bcode), all clickable where painted. AI answer rows show live status, the
  question, the latest answer (markdown, bounded) or status, suggestions, and Open session,
  Retry (after the stream stopped), and Delete. Keys: `c` comment (reply on a thread), `a` ask,
  `F` file comment, `C` review comment, `i` edit, `D` delete, `R` resolve/reopen, `K` kind, `!`
  severity, `o` open session, `)`/`(` next/previous thread.
* **AI questions** (`agent.rs`). Submitting records a pending exchange at once; the next poll
  saves it, creates a session titled `Review: <path>` in the repository, sends a bounded prompt
  (anchor, ranges, up to 80 anchored diff lines, the question, the review service operations),
  links the session (`thread.link_session`), saves the exchange as linked, and returns
  `PluginTuiAction::OpenSession`. The cursor and list position are kept in memory and restored
  by `session_navigation_finished`. Linked sessions are observed with
  `PluginTuiHost::subscribe_session_view` (status waiting, working, answered, or failed, plus the
  latest completed answer). The backend gained `create_session` and `send_message`.
* **Pointer** (`surface/pointer.rs`). Items report clicks and pointer events through the list's
  routing, so only painted, visible items receive them. Hovering a diff line shows a `+` on its
  first gutter cell (a hit region on exactly that cell); clicking it comments on the line.
  Pressing on a line moves the cursor; dragging selects a line range within the file (every row
  of wrapped lines is painted selected) and scrolls a row at a time when the pointer leaves the
  list, extending to the line on the first uncovered row at that edge (below the pinned headers
  at the top); releasing over another line opens a range composer. A resize drops the hover and any
  drag. The wheel starts (or extends from its target) a keyed BMUX `ScrollMotion` (90 ms, ease
  out; the list's own wheel handling is off); polls ask for frames while it runs.
* **Selection.** The document list is a BMUX selection scope; every painted diff row registers
  its text as one BMUX `SelectionTextRun` per row segment, with the units of the row layout's own
  grapheme placement (`DiffRowLayout::selection_cells`), so materialized fragments cover exactly
  the painted cells (a tab's whole expansion, a wide grapheme's two cells) and rows outside the
  viewport register none. Runs materialize per-grapheme fragments only when the scene is
  queried. `V` selects lines from the keyboard and `y` copies the selected lines (else the cursor
  line) through `PluginTuiHost::copy_text`.
  * **Open: host consumption of plugin-surface selections.** The host does not route terminal
    drag-select over plugin surfaces through a BMUX `SelectionController` (only the transcript
    engine owns one), so drag-to-copy text does not work in V2 yet; V2's own press-and-drag
    selects a line range for commenting. Wiring it is generic host plumbing (a controller over
    the committed scene for the surface region, the copy and clear-after-copy policy shared with
    the transcript, and the paint of its highlights), but it first needs a gesture policy that
    separates text selection from a surface's own drag handling (for example a modifier, or a
    surface declaring which scopes the host may select in). It is left for a separate change.
* **Tabs.** Diff text expands tabs to tab stops of the whole line (BMUX
  `DiffRowOptions::tab_width`); the width comes from
  `[plugins.config."bcode.code_review".review_v2] tab_width` (default 4, clamped to 1–16).
* **Threads panel.** Rows come from the annotations (sorted by path and line): status,
  severity, location, and the first line. `←`/`→` cycle the filter (all, open, needs attention
  (unresolved warnings, blockers, questions, and to-dos, plus failed questions), general
  (review-level)). Selecting a row jumps to the thread; one in a file whose content is not
  resident lands on the file header and finishes once the file loads.
* **Viewed** (`v`). Marks the cursor's file with its identity (`viewed::mark_viewed`, which keeps
  `viewed_files` in step) and moves to the next unviewed file; on a viewed file it removes the
  mark. `u`/`U` move between unviewed files. Marks the document reports as needing a new
  identity are re-recorded as content loads. Durable workspaces are saved.
* **Search** (`/`, `surface/find.rs`). A footer input (BMUX text input) searches resident
  content on every edit (smart case) and moves to the first match at or after the cursor;
  matches are restyled over exactly their cells (`DiffRowLayout::source_cells`), the current one
  distinctly. The status shows `i/n` and `N unloaded files pending` for files with changes whose
  content is not resident; the search re-runs as the document changes. `enter` keeps the
  results, `n`/`N` step (wrapping), `esc` clears.
* **Palette and help** (`palette.rs`). `:` or `ctrl+p` opens a BMUX `picker_frame` with a filter
  input over a BMUX `menu` of every declared review action with its effective keys; `?` opens
  the same menu without the input, with sections for the review and the composer. Both are
  generated from the manifest keymap and run the chosen action; pointer events route through the
  painted layout, and a press outside the panel closes it.
* **Presentation** (spec 9). The scroll anchor (the item on the viewport's first row with a
  persisted form and the rows of it scrolled above), the layout toggle, the panels, and the
  revealed context are compared with the stored state at most once a second and saved when they
  changed, and always on exit. On open they are restored once; a scroll anchor in a file whose
  content is not resident applies when the file loads. Exiting while a workspace save or an
  annotation call is still running shows `saving…` and closes once they finish (at most 5 s;
  exiting again closes at once), because the host may end the process as soon as the surface
  closes.
* **Implementation (phase 6, part 2: V1 parity).** `packages/code-review-ui/src/surface/assist.rs` and the
  changes listed here close the remaining inventory items (Appendix B):
  * **Composer completion.** The composer's editor is offered to the host as a focused text
    field through the shared `FocusedTextFieldAdapter` (`PluginTuiSurface::focused_text_field`,
    `apply_focused_text_field_edit_result`, routed by the app to the review route). It is
    eligible only while the composer has keyboard focus, nothing covers it (palette, help, a
    modal, the publish menu), the terminal has focus, it is not in preview, and its editor was
    painted in the frame; its content rectangle and caret come from the painted editor. A
    completion is one undoable edit; any edit, focus, or visibility change makes earlier
    completion requests stale.
  * **Folding.** `z` (`fold`) folds the cursor's thread to its header (a click on the header
    does the same) and, on an AI answer, shows the whole answer (at most 400 rows) or the
    8-row preview again. `shift+z` (`fold_all`) collapses every thread, or expands them all when
    all are collapsed. `shift+h` (`resolved_toggle`) hides resolved threads from the document
    (a thread is resolved when every comment is). Collapsed threads, expanded answers, and the
    resolved filter are saved in `presentation_v2`.
  * **AI.** A question on an anchor that already has a linked session (an earlier exchange, or
    a draft linked with `thread.link_session`) is a follow-up: it is sent to that session, no
    session is created or linked, and the review stays on screen (only a new session opens).
    `a` on an AI answer asks a follow-up. `shift+a` (`ai_menu`) lists the structured commands
    (`ai_explain`, `ai_risks`, `ai_suggest_comment`, `ai_tests`, `ai_summarize`, with V1's fixed
    instructions) and a free question; on a thread or an answer they ask about its anchor.
    Prompts also carry the review's sources and up to 12 other threads of the file (160
    characters each). An answered exchange can be copied (`shift+y`, `copy_answer`), turned into
    a suggested comment (`s`, `suggest_comment`; a pending duplicate is refused), or turned into
    a draft at its anchor (`m`, `draft_answer`; an identical draft is refused).
  * **Suggestions.** Suggestion rows show their state (suggested, refining, accepted,
    rejected); the latest pending one has Accept, Refine, and Reject buttons. Accept (`;`,
    `accept_suggestion`) saves it as accepted and adds a draft with its body at its anchor;
    Refine (`refine_suggestion`) saves it as refining and asks for a refinement in the linked
    session; Reject (`reject_suggestion`) saves it as rejected. Changes are optimistic, saved
    with `review.suggestion.save`, and rolled back when the save fails.
  * **Item menus.** `enter` (`activate`) on a thread or an AI answer opens a BMUX menu of the
    actions that apply to it (with their effective keys); on a gap it reveals lines below.
  * **Navigation.** `open_thread_next`/`open_thread_previous` (threads with an unresolved
    comment), `ai_thread_next`/`ai_thread_previous`, and `attention_next` (the next unviewed
    file, else the next open thread). A palette query that is a line number offers "Go to line
    N": the cursor's file's new-side line N (else the old-side line), revealing hidden context
    when needed (`goto_line` opens the palette for it).
  * **Viewed.** `view_all`/`unview_all` mark every file viewed or unviewed (marks are recorded
    in one batch). `h` (`viewed_files_toggle`) hides viewed files from the file panel (saved in
    `presentation_v2`); a file that becomes viewed leaves the panel at once.
  * **Report.** `copy_report` copies a Markdown review report: title, repository, sources,
    viewed progress, up to 50 unviewed files, up to 500 threads (location, kind, severity,
    state, linked session, comments), and the AI questions with their sessions. The
    `local_markdown`, `markdown_file`, and `json_file` publishers remain the file reports.
  * **Threads panel.** The filters are all, open, needs attention, general, and resolved.
  * **Copying.** Clipboard writes from clicks (which have no host at hand) are queued and
    written after the event or on the next poll, with a status message.
* **Key bindings (phase 6 review).** Every V2 action is declared in the manifest keymap and is
  listed by the palette and help; help has sections for the review, the composer, the file
  panel (`code-review-files`: `enter` open, `space` fold, `left`/`right` collapse/expand,
  `esc` back), and the threads panel (`code-review-threads`: `enter` and `esc` back to the
  review, `left`/`right` filters). Rarely used actions (structured AI commands, refine and
  reject, retry, open-thread and AI-answer navigation, `attention_next`, `view_all`,
  `unview_all`, `goto_line`, `copy_report`, `edit_sources`) have no default key: they run
  from the palette, help, or menus and can be bound. Conventions shared with Bcode's other
  surfaces: `q`/`esc` leave (eval viewers, git UI back), `?` help, `ctrl+p` palette (host and
  git UI), `y` copies (git UI), `/` searches or filters, `j`/`k`/`g`/`G`/pages move, `tab`
  focuses the next pane, `ctrl+r` reloads (also on home). Digits are count prefixes in the
  review, so V1's `1`–`5` AI commands moved into the AI menu. No key is bound twice in a scope
  (the manifest test checks it). V2 drops V1's `ctrl+d` exit (it is the half-page motion here,
  as in the git UI); `q` and `esc` leave the review, `esc` first clearing a selection or
  search. Text inputs, pickers, forms, and modals keep the shared BMUX editing and picker
  policies (arrows, `enter`, `esc`, `tab`), as host pickers do.

### 6.3 Visual system

* `ReviewTheme` is derived from the presentation theme's `ComponentTheme` with semantic roles:
  file status (added, modified, deleted, renamed, binary), `+`/`−` counts and change bars,
  badges (draft, open, resolved, outdated, AI), gutter and line numbers, hunk headers, gap rows,
  sticky headers, cursor line versus selected range, focused-pane border, muted metadata.
* Every row is composed from styled spans; no formatted single-string rows.
* Icons have ASCII fallbacks selected by presentation policy.
* Home and review share one theme derivation.

**Implementation (phase 4).** `packages/code-review-ui/src/theme.rs`:

* `ReviewTheme::derive` takes the host's `PluginTuiTheme`: its `ComponentTheme` (when the
  contract version matches) and its diff styles. Without a host theme, the roles derive from a
  built-in dark seed palette through `ComponentTheme::derive`, so tests and older hosts still get
  a complete, colorful theme.
* Roles: status (added and removed follow the diff's added/removed colors; modified uses
  warning, renamed info, binary muted italic), file path (bold; muted once viewed), directory,
  `+`/`−` counts and change-bar cells (the diff's colors, unused cells in the border color),
  badges (draft warning, open info, resolved success, outdated muted italic, AI accent italic),
  viewed mark, gutter (the diff layout's gutter style, so line numbers and the role agree), hunk
  header text and a full-row hunk fill (overlay surface), file header fill (raised surface; both
  stay pinned as sticky headers), gaps and their actions (link), threads, the cursor line
  (inactive-selection background) versus a selected range (selection background, kept distinct),
  pane border versus focused border, header and footer bars, filter input and placeholder, and
  scrollbar marks. The BMUX component styles (tree focused and unfocused, dividers, badges,
  progress bar, key hints, status bar, toasts, publish button) derive from the same theme.
* A theme change restyles every row (the row cache keys on a paint revision) and rebuilds the
  tree's items once.
* Icons: `Icons::UNICODE` or `Icons::ASCII`, chosen by
  `[plugins.config."bcode.code_review".review_v2] icons = "unicode" | "ascii"`. The set covers
  viewed/unviewed marks, thread, draft and AI icons, the file-header accent bar, separators, the
  rename arrow, the minus sign, gap rules and reveal arrows, ellipses, change-bar and progress
  cells, the publish menu toggle, the filter prompt, and the panel's hide action; the loading
  spinner switches to ASCII frames too. BMUX-owned glyphs (pane borders, tree disclosure
  markers) are not part of it.
* Every row is composed from styled spans (rows, tree labels and decorations, header parts,
  hints, toasts).

### 6.4 Performance pipeline

* Item layout is identity-keyed and cached per width/layout mode; measurement is incremental
  (`MeasuredListIndex`).
* Syntax highlighting is computed per whole blob (correct multi-line constructs), off the render
  path, cached by `(blob OID, syntax theme)`, and projected onto diff lines by line number. Until
  ready, lines render unhighlighted without layout change.
* Intra-line change emphasis is computed per changed line pair and cached with the item.
* Frames are pure: one snapshot per frame, no service calls, no recomputation of unchanged items,
  regional damage on scroll.

**Implementation (phase 3a).** `packages/code-review-ui/src/` implements the review screen core:

* **Launch.** `bcode review <subcommand> --ui v2` (on every interactive review subcommand) or
  `BCODE_REVIEW_UI=v2` opens the `code-review-v2` surface kind; since phase 5 `bcode review`
  without a subcommand opens its home route (7.1). The host opens any registered plugin surface
  as a standalone startup surface (7.1); V1 routing is unchanged. (Since phase 7, `bcode
  review` and every subcommand open the `code-review` surface; there is no `--ui` switch.)
* **Loading.** The first `poll` spawns the open task: provider routing, `review.sources.resolve`,
  a change token, and every index page. A BMUX `empty_state` with a spinner shows index progress
  until the document exists; input is never blocked. Afterwards `poll` applies finished work and
  serves `take_needs` with at most four service calls in flight. Rendering only reports the
  visible range to the document; frames make no service calls.
* **List.** The document is a lazy BMUX `VirtualItemSource` keyed by item keys, with sticky file
  (level 0) and hunk (level 1) sections, document splices noted on the list state, wheel
  scrolling and a scrollbar with marks for the cursor's file and (once annotations exist) threads.
  Items report exact heights without construction (`measure_hint`), so only visible items are
  built.
* **Rows.** Diff lines and pairs are laid out by BMUX `layout_diff_row` with one gutter per
  document (growing with the largest line number seen, at least four digits), intra-line
  emphasis (side-by-side pairs, and unified removed/added runs paired positionally), syntax
  ranges, and an explicit overflow (wrap toggle: `Wrap` or `Truncate`). Headers, hunk headers,
  notices, placeholders, gaps (`↑ 20 · N hidden lines · ↓ 20 · all`, degrading to `↑20 N ↓20 all`
  and `⋯ N` when narrow, with click segments only while fully shown), and minimal annotation rows
  are styled spans clipped by BMUX text primitives. Rows are cached per item, width, and
  presentation revision in a bounded two-generation cache; measurement, painting, and hit
  regions read the same rows.
* **Layout.** Unified, side-by-side, or automatic by width: the host's `[tui.diff_viewer]`
  preference arrives through the generic `PluginTuiHost::diff_layout`, the plugin's
  `[plugins.config."bcode.code_review".review_v2]` (`diff_layout`, `side_by_side_breakpoint`)
  overrides it, and the in-review toggle overrides both for the session. Switching keeps the top
  item and the cursor on the same lines (`map_key`).
* **Highlighting.** Both sides of visible resident text files are read whole (bounded to 100,000
  lines and 8 MiB), highlighted on the blocking pool with BMUX's whole-text projection, and cached
  by blob identity (working-tree content by path and refresh generation) and palette (64 entries,
  400,000 lines). Rows look tokens up by line number verified against their text.
* **Input.** Keys are declared in the manifest under the `code-review` scope (`code-review-v2`
  before phase 7): `j`/`k`, half
  pages (`ctrl+d`/`ctrl+u`), pages, `g`/`G`, files (`n`/`p`), hunks (`}`/`{`), changes
  (`]`/`[`), gap expansion on the cursor gap (`e` below, `E` above, `x` all, `X` whole file;
  count prefixes set the lines), layout (`S`), wrap (`W`), refresh (`r`), and exit (`q`, `esc`).
  The cursor is a keyed item revealed below the sticky headers; viewport scrolling drags it
  along. Motions into unloaded files land on the file header and repeat once it loads.
* **Refresh.** `r` and terminal focus regain compare `review.sources.token`; a changed token
  re-opens the review keeping expansions, layout, the cursor, and the top item.
* **Not yet.** Tabs render as one replacement cell (BMUX's control-character policy for painted
  text); tab expansion belongs in BMUX's row layout. Wheel and page scrolling move immediately
  rather than through `ScrollMotion`.

## 7. Home and new-review flow

### 7.1 One surface, internal routes

The plugin owns one surface with internal routes: `Home`, `NewReview` (wizard), `Review`. The
plugin runtime loads once per `bcode review` invocation; moving between routes never reloads it
or reinitializes the TUI. The host gains one generic capability: run any registered plugin surface
as a standalone startup surface by plugin id and surface kind. All `code-review` special cases are
removed from `packages/tui` and `packages/cli`.

**Implementation (phase 3a).** The generic capability landed first: `packages/tui`'s
`plugin_surface_launcher` loads the plugin runtime once, finds the single loaded plugin whose
manifest declares the requested surface kind, opens it with the caller's string options, and runs
it until it closes. Startup surface kinds without a host special case (such as `code-review-v2`)
go through it. The V1 `code-review` special cases remain until phase 7.

**Implementation (phase 7).** The host special cases are gone: `packages/tui` has no
`code_review_launcher` and no `code-review` routing in `terminal_shell.rs` or `run_plugin_surface`,
and `packages/tui` no longer depends on `bcode_code_review_models`. `packages/cli` never had
review-specific code (the plugin contributes `bcode review` through its static CLI registration).
The app's surface kind is now the canonical `code-review` (it was `code-review-v2`), so every
startup request for `code-review` reaches V2 through the generic launcher. The surface still
accepts the option shapes V1's host launcher sent (`workspace`, or a JSON `target`), so an older
caller opening `code-review` gets the matching V2 review.

**Implementation (phase 9).** The review no longer runs as a plugin surface (13.3), so the
`code-review` surface declaration and the generic `plugin_surface_launcher` were removed: no
other plugin used the launcher (eval and metrics startup surfaces have their own adapters). A
startup surface kind without an adapter now fails with `unsupported_startup_surface`.

**Implementation (phase 5).** `packages/code-review-ui/src/app.rs` is the `code-review-v2` surface (`code-review`
since phase 7)
(`ReviewAppV2`):

* Routes: `Home` (`home.rs`), `NewReview` (`wizard.rs`), `Opening` (an entry or a workspace
  creation resolving on a task: a spinner, or the failure with `esc`/`enter` back to home), and
  `Review` (`ReviewSurfaceV2`). A route change replaces the route value only; the plugin runtime,
  the terminal, and the backend stay. Home is kept while a review is open (its filter, selection,
  and folded groups survive) and reloads when the review closes.
* Transitions: a review's own exit (after its saves finished, as before) returns to home instead
  of closing the surface; `q` on home closes it. Home opens reviews (enter, continue-latest,
  suggestions), starts transient reviews (suggestions), or the wizard (`n`); the wizard cancels
  to home or creates a durable workspace (`review.workspace.create`) and opens it.
* Entries (`entry.rs`): the CLI passes `entry` (JSON `Entry`: `home`, `sources`,
  `pull_request`, `stack`, `resume`, `workspace`) as the surface option; the older `workspace`
  and `target` options still open those directly. Pull request and stack entries ask the first
  enabled provider with the needed capabilities: a number or a URL (its last numeric path
  segment, matched against the providers' pull request URLs, so nothing provider-specific is
  parsed), the repository from `review_source.pulls.list`, and for `stack` without a reference
  the open pull request whose head is the current branch (`review.repo.state`), resolved to its
  top layer. `resume` without an id opens the most recently updated unarchived review; an
  archived review is refused with a pointer to home.
* Routes read the keymap again after every route change, so hosts need not call
  `prepare_keymap` themselves. Home and the wizard derive the same `ReviewTheme` as the review.

### 7.2 Home

* **Suggestions** at the top, derived from repository state: uncommitted changes, current branch
  ahead of its base, PRs requesting my review, a stack containing the current branch, reviews with
  new commits since my last pass.
* **Reviews table** (BMUX `table`): title, sources, viewed progress bar, drafts/threads, publish
  status, updated. Grouped: needs attention, in progress, published, archived (collapsed).
* Type-to-filter search, preview panel (file stats, sources, last publish), rename, archive,
  restore, and one-key continue-latest.

**Implementation (phase 5).** `packages/code-review-ui/src/home.rs` (screen), `home/model.rs` (groups, filter,
rows, suggestions; pure and unit-tested), `home/load.rs` (service calls):

* **Loading.** A refresh (first show, returning from a review, `ctrl+r`, terminal focus regain)
  spawns three tasks: `review.workspace.list` (archived included), `review.repo.state`, and the
  first provider's `review_source.pulls.list`; once the branch and its pull request are known,
  `review_source.stack.resolve` for it. Per-review stats resolve the review's sources without
  provider answers (no network; provider-backed sources count as unresolved) and read one index
  page per resolved source for its totals; they load for the visible rows first, then for the most
  recent reviews, two at a time and at most 40 per refresh. A review has new commits when a
  source's current head commit differs from its recorded `reviewed_head_oid` (the first part of a
  multi-part source). Nothing blocks input; rows and suggestions update as answers arrive.
  Answers carry the refresh generation they were started in: a listing, repository state,
  pull request list, or stack from an older refresh that is still running never replaces a
  newer refresh's answer. Stats scheduling returns at once while the in-flight or per-refresh
  cap is reached, so polling costs nothing once the cap is used.
* **Suggestions** (a BMUX `menu`, keys `1`–`9`, or `tab` to focus it): uncommitted changes
  (unstaged, staged, or both, with file and line counts), the current branch ahead of its base
  (commit count), the stack of the current branch's pull request (or the pull request itself when
  it has one layer), up to three pull requests requesting my review, up to two reviews with new
  commits since the last pass, and always "New review…" last. Without a provider the
  pull-request suggestions are absent; a failing provider (for example without credentials) is
  reported in the footer and the local suggestions stay.
* **Groups** (V1 health mapped): *Needs attention*: no included sources (V1 `setup`, `needs
  sources`), drafts changed since the last publish (`changed since publish`), or new commits since
  the last pass. *In progress*: unpublished drafts (`drafts`) and everything else active
  (`active`). *Published*: published with no draft changes since (`published`). *Archived*:
  archived, folded by default. Group headings are table rows (`enter`/`space` fold); reviews sort
  by last update within a group. Selection follows the review (or heading) identity across
  reloads.
* **Table** (BMUX `table`): title, sources, viewed progress (five progress cells and
  `viewed/total` once the total is known), drafts and threads, publish state (`published` and its
  age, `changed`, or `—`), and the update age. Columns degrade by width: publish below 96
  columns, sources below 80, notes below 70, viewed below 56; title and age always show.
* **Filter** (`/`): a BMUX text input; every word must occur in the title, a source label, or the
  id; archived matches show while filtering; `enter` keeps it, `esc` clears it.
* **Preview** (a BMUX `pane`, from 100 columns, `d` toggles): sources (included or excluded),
  file totals, viewed count, drafts and threads, the last publish (age, publisher, message,
  changed since), and new commits; a group heading explains the group.
* **Actions** (scope `code-review-home`): `enter` open (an archived review refuses, pointing at
  restore), `c` continue the latest unarchived review, `n` new review, `r` rename (footer input;
  an empty title is refused), `x` archive, `shift+r` restore (`review.workspace.update` with the
  archive mark cleared), one change at a time with a toast; `ctrl+r` reload, `?` help (the
  keymap, as in the review), `q` quit. A press on the selected row opens it.
* An empty repository shows "Your next review starts here"; a filter without matches "No
  matching reviews".

### 7.3 New-review wizard

BMUX `stepper` + `picker_frame`. Step 1 picks a source type; step 2 picks its parameters; step 3
confirms title and optional extra sources (composite reviews).

| Source type | Picker |
| --- | --- |
| Working tree | unstaged / staged / both, with counts |
| Current branch | base auto-detected (upstream, else default branch), editable; merge-base semantics |
| Compare branches | fuzzy base and head pickers over `review.refs.search` |
| Commits | paged log; single commit, contiguous range, or multi-select |
| Pull request | provider PR list; review-requested-from-me first |
| Stack | detected stacks; whole stack, a single layer, or step-through |
| Files | repository path search; whole files or line ranges |

**Stacks (Graphite-style, also GitHub native stacked PRs).** A stack is a linear chain of PRs
where each PR's base branch is the previous PR's head branch, ending at the default branch.
Detection uses PR base/head relationships from `review_source.stack.resolve`; no Graphite-specific
metadata is required.

* **Layer mode (primary):** review one PR against its parent layer, exactly the PR's own diff.
  Comments publish to that PR.
* **Step-through:** a stack rail shows every layer with per-layer viewed progress and draft counts;
  next/previous layer keeps per-layer positions.
* **Whole-stack mode:** one document from trunk merge-base to the top layer, with each file and
  hunk labeled by the layer that introduced it (from per-layer diffs). A comment publishes to the
  layer whose diff contains its anchor line; if none can be determined it falls back to the top
  PR's review summary.

**Implementation (phase 5).** `packages/code-review-ui/src/wizard.rs` over `picker.rs` (a BMUX `picker_frame`
with an optional query input over a BMUX `menu`, or a BMUX `empty_state` for loading, empty, and
error states), under a BMUX `stepper` (Source type, Details, Title & create):

* Step 2 by type: *Working tree*: unstaged, staged, or both with counts from `review.repo.state`.
  *Current branch*: the detected base prefilled and focused; any reference can be searched
  (`review.refs.search`); keeping the detected base stores `CurrentBranch { base: None }`, so the
  base stays automatic. *Compare branches*: a base, then a head reference (merge-base
  semantics). *Commits*: the paged log (`review.commits.page`, message search); `tab` marks a
  commit, `shift+tab` marks every commit from the last mark to the focused one; one commit
  becomes `Commit`, several `Commits` (oldest first; the resolver makes a contiguous chain one
  range). *Pull request*: the provider's open pull requests, review requested from me first,
  filterable. *Stack*: the current branch's pull request first, then its stack resolved: whole
  stack (`Stack`), step through every layer (one `StackLayer` source per layer; the stack rail of
  7.3 is phase 6), or one layer. *Files*: `review.files.search`; `tab` marks files; a trailing
  `:START-END` makes the picked file a `FileRange`.
* Step 3: the title (prefilled from the sources, kept when typed) above "Create review", "Add
  another source…" (back to step 1 for a composite review), and the sources (`enter` removes
  one). Creating calls `review.workspace.create` and opens the durable review.
* Listings page on demand (100 entries, the next page when the focus comes within eight rows of
  the end); a newer query drops older answers, and a query typed while a request runs is asked
  again when it finishes. The request sequence keeps counting across steps, so an answer to an
  earlier step's listing is never taken for the current step's. `enter` pressed before the
  listing answers the current query is deferred until it does (any other key cancels it), so
  it never picks a row of an older query. Going back to step 1 focuses the type that was
  being configured.
* Without a review-source provider the pull request and stack steps say so (other types still
  work); a provider failure (for example `unauthenticated`) shows its message.
* Keys (scope `code-review-wizard`): `tab` mark, `shift+tab` mark range, `esc` back one step
  (on step 1 to step 3 when sources exist, else to home); arrows, pages, `enter`, and clicks on
  rows come from the picker.
* **Editing a review's sources (phase 6 part 2; closes INV-SRC-01's open question).**
  `edit_sources` (palette) leaves the review after its saves (close outcome
  `{"edit_sources": true}`, which the app turns into the wizard instead of home) and opens the
  wizard at step 3 with the review's title and sources ("Edit sources", "Save changes"). Sources
  are added through step 1 ("Add another source…"), removed with `enter`, and included or
  excluded with `tab`; at least one must stay included. Saving keeps existing source ids (so
  threads, viewed marks, and reviewed revisions stay attached) and gives new sources unused
  `source-N` ids; a durable review is saved with `review.workspace.update` and reopened, a
  transient one reopens with the new sources. `esc` returns to the review unchanged.

**Implementation (phase 6, stacks in the review).** `packages/code-review-ui/src/surface/stack.rs`:

* **Layer mode** is a `StackLayer` source: the merge base of the layer below's head and the
  layer's head → the layer's head (4.1), so exactly the pull request's own diff; its comments
  publish to that pull request (7.6). **Step-through** is one `StackLayer` source per layer.
  **Whole-stack** is a `Stack` source: file headers, hunk headers, and changed lines carry the
  layer that introduced them from `review.stack.attribution` (5.1).
* **Stack rail.** A BMUX `tab_bar` row under the header whenever the review has stack or
  stack-layer sources and the screen has at least 9 rows: one tab per such source group with
  `#N Title`, its position in the stack (`2/4`), five viewed-progress cells and `viewed/files`,
  and the draft threads anchored in it; a whole-stack group shows `Whole stack #first–#last`. The
  tab of the layer the cursor is in (by its item's source; items outside every layer keep the
  last one) is selected. Labels degrade by display width: full, then compact (`#101 2/5 ✎1`),
  then the numbers only; a bar that still does not fit is truncated by the component.
* **Stepping.** `.`/`,` (`layer_next`/`layer_previous`, wrapping) and clicks on a tab enter a
  layer. Leaving a layer remembers the cursor in it (persisted form); entering returns to the
  remembered position (resolved by key, loading its file first when needed), else to the layer's
  header. Positions are saved in `presentation_v2.layer_positions` and restored with the
  workspace.

### 7.4 Re-review

When a review's sources resolve to new OIDs, the review offers "changes since last review". For
fast-forward updates this is the diff between the previous and current head. After a rebase,
files are compared by patch-id: unchanged patches stay viewed and are de-emphasized, changed
patches are shown.

**Implementation (phase 6).** `packages/code-review-ui/src/rereview.rs`, `packages/code-review-ui/src/surface/flows.rs`, and
`packages/code-review/src/data/rereview.rs`:

* **Passes.** A pass is recorded (`reviewed_revisions`, each source's first resolved part, from
  the whole review even while the changes-since view is shown) only when the reviewer marks it
  complete (`shift+m`, `complete_pass`, or the publish menu) or publishes (7.6). Leaving a review
  no longer records one, so a review never marked complete has no baseline and offers nothing.
* **Detection.** On the first open of a durable review, every source whose current head commit
  differs from the recorded `reviewed_head_oid` is compared with `review.revisions.compare`. A
  toast summarizes the result (`N new commits since your last review`, `rebased since your last
  review`) and names the key of the changes-since view.
* **Changes since last review** (`shift+l`, `since_last_review`, a toggle): fast-forwarded
  sources are replaced by `CommitRange { old head, new head, two-dot }` with the same source id
  (so annotations keep their source), rebased sources stay whole, and unchanged sources are left
  out; toggling again reopens the whole review. The view reopens like a refresh (expansions,
  cursor, and top item carried by key). Comments made in it anchor to the new head's lines, which
  are the same in the whole review; old-side line numbers there refer to the old head. Viewed
  marks recorded in the view carry the delta's old blob and so do not match the whole review's
  files (by design: the whole file was not viewed).
* **Rebased sources.** When both reviewed commits are known and the old head still exists, the
  patch ids of the source's files in the reviewed diff (`reviewed base..reviewed head`) and the
  current diff are compared in chunks of 500 paths. Files whose patch is unchanged are marked
  viewed with their current blob identity and patch id (5.4), and the document marks them
  `carried_over`: the file header shows the path muted with `unchanged since last review`. The
  set is kept across reopens of the same surface.

### 7.5 CLI

`bcode review` opens Home. Direct entry points open the Review route with a transient workspace,
which can be saved:

* existing: `unstaged`, `staged`, `all`, `last-commit`, `range`, `repo`, `publish-github`
* new: `branch [BASE]`, `compare BASE HEAD`, `commit REV...`, `pr NUMBER|URL`,
  `stack [NUMBER|URL]`, `resume [ID]`

**Implementation (phase 5).** `plugins/code-review-plugin/src/cli.rs` passes an `entry` option
(7.1) to the `code-review-v2` surface with `--ui v2` (or `BCODE_REVIEW_UI=v2`): `bcode review`
opens home; the existing subcommands and `branch` (`CurrentBranch`), `compare` (`BranchCompare`,
`--two-dot` available), and `commit` (`Commit`, or `Commits` for several) open transient reviews;
`pr`, `stack`, and `resume` resolve first (7.1). With V1 the new subcommands fail with a message
that they need `--ui v2`; V1 routing of the existing subcommands and home is unchanged until
phase 7.

**Implementation (phase 7).** `bcode review` and every subcommand open the `code-review` surface
unconditionally with an `entry` option: the existing subcommands open transient `Sources` entries
with the matching source kind (`unstaged` is `WorkingTreeUnstaged`, `range` is `CommitRange`,
and so on), and `branch`, `compare`, `commit`, `pr`,
`stack`, and `resume` behave as above. The `--ui` flag and the `BCODE_REVIEW_UI` environment
variable are removed outright: passing `--ui` is a usage error from the argument parser, and the
environment variable is ignored. `publish-github` is unchanged.

* **Saving transient reviews.** `ctrl+s` (`save_review`, review scope, shown in the hints while
  the review is transient) creates a durable workspace with the same title and sources, copies
  its viewed marks, presentation state, and reviewed revisions, and moves its drafts (saved in the
  new scope with their kind, severity, and resolution, then deleted from the transient scope).
  Since phase 6 part 2 its AI exchanges and suggestions move too: their ids are unique across
  scopes (a save of an existing id updates it where it is), so each is deleted from the
  transient scope and then saved in the new one, and saved back when that fails
  (`review.suggestion.delete` is additive for this).
* **Transient persistence (decision, phase 6 part 2).** A transient review is not listed on
  home and keeps no presentation state; its annotations live in a per-target transient scope,
  so reopening the same target shows them again and transient reviews of other targets do not.
  Leaving a transient review that has drafts, AI
  questions, or suggestions offers saving once: the first exit shows "this review is not saved:
  ctrl+s saves it with its comments; exit again to leave without saving" (any other key
  cancels the offer), and saving makes it a durable review on home. Without annotations, exit
  leaves at once.
* **Transient annotation scope.** `packages/code-review/src/scope.rs` owns the scope id every annotation request
  carries (`ReviewScope::Workspace { workspace_id, .. }`, stored under the review key
  `review-workspace-{workspace_id}`): drafts, threads (resolution, classification, linked
  sessions), AI exchanges, and suggestions all use it, from the client (`NoteScope::for_workspace`)
  and from the service when it derives a workspace's key itself (`review.bundle.get` and the
  publish preview and submit operations), and the V2 publish flow's bundle `review_id`.
  * A durable workspace's scope id is its own id, so durable keys are unchanged.
  * A transient workspace's scope id is `transient-review-workspace-{fingerprint}`: the first
    32 hex digits of a SHA-256 over the repository root and each source's kind (its JSON
    encoding, in order). Source ids, labels, titles, and the included flag are not part of it, so
    a target opened from the CLI, a home suggestion, or the wizard shares one scope; the
    repository is part of it because the default state database is shared by every repository.
  * The repository root is the working tree root: hosts pass the working directory, so the
    surface factory resolves it to the nearest ancestor with a `.git` entry
    (`working_tree_root`) before the app opens. A review started from a subdirectory shares the
    scope of one started at the root.
  * The scope's fallback `target` (the first included source's target) is unchanged; the
    service still uses it for provider operations, not for the key.
  * Saving moves the annotations from the fingerprinted scope into the new workspace's scope, as
    above.
  * Before this change every transient review used the bare id `transient-review-workspace`
    (the scope's target was ignored by the key), so annotations from any unsaved review showed in
    every other unsaved review, across repositories in the shared database. Annotations written
    under that shared key are left in place and not migrated: they cannot be attributed to a
    target (each row records only the first source's legacy target, which is the repository for
    pull-request, stack, file, multi-commit, and current-branch reviews). No review shows them
    any more; `is_transient_scope_id` still recognizes the bare id so it is never treated as a
    durable workspace.
* **Reviewed revisions.** Each source's resolved commit sides (`reviewed_revisions`, the first
  part of a multi-part source) are recorded when the pass is marked complete or published (7.4)
  and saved with the workspace; home compares them with the current heads (7.2).

### 7.6 Publishing and import

**Implementation (phase 6).** `packages/code-review/src/publish_plan.rs` (renderer-neutral planning),
`packages/code-review-ui/src/publish.rs`, `packages/code-review-ui/src/import.rs`, `packages/code-review-ui/src/options_form.rs`,
`packages/code-review-ui/src/modal.rs`, and `packages/code-review-ui/src/surface/flows.rs`:

* **Publish flow** (`shift+p`, `publish`, the palette, or the header's split button; V1's `x`
  expands context in V2). A modal over a BMUX `picker_frame`:
  1. *Checklist* (a BMUX `menu`): readiness (complete or incomplete), viewed files with the first
     three unviewed paths, open, resolved, and draft counts with the first three open threads,
     then every persisted thread with an include toggle (`space`; resolved and already published
     threads are labeled). `enter` continues.
  2. *Publisher*: built-in publishers (`review.publishers.list`) and every external publisher
     discovered over `bcode.review_publisher/v1` (manifest per plugin), with capability labels. A
     publisher whose `pull_request_provider` is the provider of one of the review's pull-request
     sources is preselected, else `markdown_file`.
  3. *Options*: the schema as BMUX `form`/`form_field` (`options_form.rs`): strings are text
     inputs (every key types, fixing V1's `j`/`k` defect), enums and booleans cycle with
     `←`/`→`/`space`, required fields block `enter`. When the publisher publishes the review's
     pull requests, the targets are listed from the sources (`Publishes to owner/repo#101, …`);
     nothing provider-specific is asked or prefilled by key name.
  4. *Preview*: a scrollable BMUX `text_view` with every bundle's preview (a heading per pull
     request when there are several). `enter` asks for confirmation; `enter` again submits.
  5. *Submit*: bundles are submitted one at a time; each success saves a publish record
     (`review.publish.record.save`, durable reviews) with the bundle's persisted thread anchors.
     A failure returns to the preview with the error; retrying skips the bundles already
     published. Success records the pass (7.4), reloads the records (threads show `published`),
     and toasts each provider message.
* **Routing** (`packages/code-review/src/publish_plan.rs`). For a publisher of the sources' provider: a pull-request or
  stack-layer source publishes its threads to its own pull request (layer mode: the layer's own
  diff); a whole-stack source maps each line comment with `review.stack.attribution` (fetched per
  anchored file when publishing) to the layer that introduced or removed its lines and re-anchors
  it at the line's number in that layer's own diff (`layer_line`), reading its selected lines from
  that diff; a range across layers narrows to its last line's layer; a comment no layer accounts
  for (a context line, or a file without attribution) and file-level comments go to the top pull
  request's summary (`summary_only`). Review-level comments go to the top pull request of a whole
  stack, else to the last pull-request source. Other sources, and every thread for other
  publishers, form one bundle for the publisher's default target. Bundles are ordered bottom of
  the stack first.
* **Bundles** are built by the client (V2 never materializes the review): files from the
  document's index entries, threads with their selected lines and hunk context from
  `review.file.hunks.get` of the target diff (lines outside every hunk select nothing, and the
  publisher's unmapped fallback applies, 5.2), the target as a two-dot commit range when the diff
  is commit to commit, and `review_id` `review-workspace-<id>` as in V1. Built-in publishers are
  called with `review.publish.bundle.preview/submit`; external ones with
  `ExternalPublishReviewRequest`. Additive, provider-neutral contract fields: publisher manifest
  `pull_request_provider`, `ReviewBundle::pull_request` (`ReviewPullRequestTarget { provider,
  repository, number }`), `ReviewBundleThread::summary_only`, and
  `ExternalReviewImportRequest::pull_request`. The GitHub plugin declares `github`, uses the
  target when its options name no repository or pull request (options always win, and a target
  of another provider is ignored), and summarizes `summary_only` threads.
* **Import** (`shift+i`, `import`, or the publish menu; V1's `X` reveals a whole file in V2).
  Importers are discovered over `bcode.review_importer/v1`; one is picked from a BMUX `menu`, its
  options come from the same form, and `enter` imports. The request carries the review's files
  with their hunks (at most 500 files and 200,000 diff lines) and, when the review is one pull
  request or stack layer, that pull request; mapped anchors without a source get that source.
  The importer id must match its manifest. The snapshot replaces the previous one from the same
  importer (`review.external_import.save`, durable reviews; in memory for transient ones) and is
  listed again with the annotations on open.
* **Imported threads** are read-only thread items: mapped threads at their anchor (outdated
  placement rules apply), unmappable ones at the review level. Their header shows `imported`, the
  provider, the comment count, the remote state, and `approximate` or `not in this review`
  mappings with the provider's message; comments show their authors; there are no actions, and
  reply, edit, delete, resolve, kind, and severity are refused with a status message. The threads
  panel lists them with an `imported` badge. (V1 showed them only in its sidebar.)

## 8. BMUX usage and upstream work

### 8.1 Component map

| Region | BMUX component |
| --- | --- |
| Review document | `virtual_list` (`VirtualListComponent`, `MeasuredListIndex`, `ScrollMotion`, anchor policies), `resident_window` for paged files |
| Diff rows | `diff_viewer` styles, layout, and intra-line emphasis via a row-level API (8.2) |
| Expanded file/whole-file views | `source_viewer` |
| File tree | `tree_view` (with 8.2 extensions) |
| Panels | `panel_group`, `pane` |
| Header/footer | `status_bar`, `key_hint_bar`, `badge`, `progress_bar`, `breadcrumbs`, `split_button` (publish) |
| Home | `table`, `empty_state`, `text_input_box` (search) |
| Wizard and pickers | `stepper`, `picker_frame`, `completion`, `selectable_list`, `checkbox`, `radio_group` |
| Command palette | `picker_frame` + `completion` + `menu` |
| Dialogs | `dialog`, `modal_frame`, `form`, `form_field` |
| Notifications | `toast_stack`, `spinner` |
| Selection/copy | `selection` integration |

### 8.2 Generic upstream work (BMUX)

Each item is a general mechanism with BMUX-owned tests; none carries review semantics.

* **Sticky item headers for `VirtualList`:** keyed sections whose header item pins to the viewport
  top while any of its section is visible, with nested levels (file, then hunk) and correct hit
  regions.
* **Row-level diff layout in `diff_viewer`:** lay out a single unified line or side-by-side pair
  (gutters, markers, intra-line emphasis, syntax spans, wrapping) at a given width, so a
  virtualized caller renders only visible rows. The existing whole-document API becomes a client
  of it.
* **Whole-blob syntax projection:** highlight an old and new text once and project tokens onto
  diff lines by line number, with a background-computation-friendly API. Bcode's `git-ui`
  `diffview` already does this locally and moves onto it.
* **Rich `tree_view` items:** styled span labels, right-aligned trailing decorations (counts,
  badges), and virtualization for very large trees.
* **Scrollbar markers:** caller-supplied marks (positions with styles) on the `virtual_list`
  scrollbar.
* Any performance work found by the phase 0 benchmarks in virtual-list measurement, painting, or
  damage tracking.
* **Found while building the chrome (phases 3b and 4):** `TreeViewComponent::toggle_on_click`
  (a click on an expandable row toggles it as well as selecting it), and event routing in
  `PanelGroupComponent` and `PaneComponent`, which added a child's absolute position to an
  already translated event context, so panels and panes not at the screen origin (or nested)
  delivered pointer events at doubled offsets. Both route children relative to themselves now,
  with BMUX tests for placed and nested containers.
* **Found in the performance pass:** `selection::SelectionTextRun` (lazily materialized
  selection rows; `PaintCx::push_selection_text_run`, `painted_text_cells`), and tab expansion
  in row-level diff layout (`DiffRowOptions::tab_width`, column-aware stops, tabs painted as
  spaces with one unit mapped to the tab byte) with `DiffRowLayout::selection_cells`. BMUX
  `source_viewer` has a related tab defect (its `TextBlock` rows paint a tab as one replacement
  cell while its selection registration expands tabs to eight cells); fixing it needs tab
  expansion in `TextBlock`, which V2 does not use, and is left open.
* **Found in the phase 3 review:** `ToastStack` measured its close button by byte length and
  painted it after the title while its hit region (and `close_at`) used the toast's last column;
  the glyph is now painted on that column and titles are truncated by display width before it.

## 9. Persistence and compatibility

* Additive columns/fields: resolved OIDs per source, new source kinds, patch-id viewed marks, V2
  presentation state (scroll anchor by item identity, layout mode, panel sizes, expansion sets).
  * Migration `018_workspace_v2_state_columns` adds three nullable JSON columns to
    `review_workspaces`: `viewed_marks_json` (`Vec<ViewedMark>`), `reviewed_revisions_json`
    (source id → `ReviewedRevision { reviewed_base_oid, reviewed_head_oid, recorded_at_ms }`),
    and `presentation_v2_json` (`ReviewV2PresentationState`). Rows written before the migration
    read as empty.
  * V2 presentation state is disposable: a value that cannot be read, or whose `schema_version`
    is newer than this build's, is dropped instead of failing the workspace or being read as an
    older form. Viewed marks and reviewed
    revisions are durable review data and must parse.
  * The fields are omitted from workspace JSON when empty, so existing payloads are unchanged.
  * Migration `019_thread_head_oid_column` adds the nullable `draft_threads.head_oid` (5.3). The
    additive operation `thread.classify` (`ClassifyThreadRequest`) sets a thread's kind and
    severity, creating the thread like `thread.resolve` does.
  * Migration `020_thread_blob_oid_column` adds the nullable `draft_threads.blob_oid` (5.3).
  * `ReviewV2PresentationState::layer_positions` (source id → `ReviewItemKey`, omitted when
    empty) keeps the last position in each stack layer (7.3); no migration is needed (it is part
    of `presentation_v2_json`).
  * Phase 6 part 2 adds `collapsed_threads` and `expanded_answers` (sets, omitted when empty),
    `hide_resolved_threads`, and `hide_viewed_files` (omitted when false) to the same JSON; no
    migration is needed, and older states read with everything expanded and shown.
* Unknown future source kinds are preserved and surfaced as unsupported, not guessed: they
  deserialize as `ReviewSourceKind::Unsupported { kind, raw }`, serialize back verbatim, and
  resolve to an `unsupported_source_kind` diagnostic.
* Workspaces V1 created open in V2 (phase 7 keeps this as a test,
  `a_review_written_before_v2_opens_with_its_viewed_files_and_drafts`): path-only viewed files
  read as legacy viewed marks (5.4), drafts whose anchors record neither head nor blob attach to
  their lines, and the V1 presentation state is ignored. Migration `017` and its
  `presentation_state_json` column stay so the migration history is unchanged, but since phase 7
  nothing reads or writes the column and the model no longer has the field; older clients' JSON
  that still carries `presentation_state` decodes (the field is ignored).
* Transient annotation keys (7.5): transient reviews are keyed per target
  (`review-workspace-transient-review-workspace-{fingerprint}`); durable keys
  (`review-workspace-{id}`) are unchanged. Rows under the former shared key
  `review-workspace-transient-review-workspace` stay in the database unmigrated and are no
  longer shown; no schema change is involved.
* Target-keyed drafts of early workspaces: workspaces written before drafts were scoped to
  workspaces kept their drafts under the target key (`review-{sha256(repo, target)}` of the first
  included source; the service's `workspace_legacy_review_key`). The first review interface
  stopped reading that key for workspaces once drafts were scoped, and V2 matches it: the review
  route lists only the workspace key. Home's draft and thread counts still fall back to the
  target key when the workspace key has no drafts, so such a workspace can show a count on home
  that its review does not list. The rows are not adopted into the workspace key: the target key
  is shared by every workspace whose first source has the same target (and by target-scoped
  requests for that target), so adopting them would assign one review's drafts to another.

## 10. Phases

0. **Spec, inventory, harness, baseline.** Accept this document; inventory V1 behaviors to keep
   (keymap actions, anchors, drafts, AI threads, suggestions, publish/import, report, presentation
   restore); build a deterministic multi-event scenario harness and a fixture generator (5k files,
   500k diff lines); record V1 baseline numbers.
1. **Data layer.** Contracts in models, revision-pinned resolution, index/hunks/blob/search ops,
   `cat-file --batch`, token-based refresh (4.2), schema additions.
2. **Document.** Items, gaps and interval-set expansion, side-by-side pairing, anchors, threads,
   viewed state, navigation targets; exhaustive unit tests.
3. **BMUX upstream and review screen.** Section 8.2 items; virtualized review screen with sticky
   headers, unified/split, highlighting pipeline, file tree, scrollbar, input, composer, threads
   panel. Selected via `bcode review --ui v2` or `BCODE_REVIEW_UI=v2`.
4. **Visual system.** Theme roles, styled spans everywhere, palette, help, toasts; review against the
   manual smoke checklist.
5. **Home, wizard, routes.** Single-surface routing, generic host standalone-surface capability,
   suggestions, table, wizard, new CLI entry points.
6. **PRs, stacks, re-review, parity.** Review-source provider interface and GitHub implementation
   (contracts, provider, client routing, provider-backed resolution, and whole-stack attribution
   landed ahead of the UI, 4.4), stack modes, re-review, publish/import, AI questions, report;
   close every inventory item.
7. **Default and deletion.** V2 becomes the only implementation; delete V1 files, the
   `--ui`/environment switch, host review launcher code and duplicated types; update
   `docs/code-review-plugin-extension-points.md`.

   **Implementation (phase 7).** Done.

   * Deleted V1: `code_review_tui.rs`, `code_review_tui_render.rs`, `code_review_tui_view.rs`,
     `code_review_tui_display.rs`, `code_review_controls.rs`, `code_review_home.rs`,
     `async_values.rs`, `terminal_events.rs`, and `tui_host_types.rs` in the plugin; the V1
     keymap scopes (`code-review` V1, `code-review-build`, `code-review-home` V1 and its
     `-search`, `-new`, `-rename` scopes, `code-review-comment`, `code-review-prompt`,
     `code-review-import`, `code-review-import-options`, `code-review-publish`,
     `code-review-publish-options`) and V1 control adaptation in `keybindings.rs`; the
     `code-review-home` and V1 `code-review` manifest surfaces; the V1 harness pieces
     (`v1_smoke.rs`, `v1_baseline.rs`, `materialize_v1`). The V2 harness, fixtures, and
     benchmarks are unchanged.
   * Deleted host special cases: `packages/tui/src/code_review_launcher.rs`, the `code-review`
     arms in `TerminalShell::dispatch_plugin_surface` and `run_plugin_surface`, and
     `run_code_review*` / `run_review*` (7.1).
   * Deleted V1-only service surface: `review.workspace.materialize` (4.2) and its request and
     response models, the materialization picker fields (`repository_files`,
     `repository_branches`, `repository_commits`, `ReviewRepositoryCommit`) and diagnostics,
     `ReviewWorkspaceMaterialization` (now private to the plugin, used by the bundle
     operations), and the V1 presentation state (`ReviewWorkspacePresentationState`, 9). Every
     other `bcode.code_review/v1` operation is still served: agents use the context operations
     the AI prompt names, and the remaining ones are part of the published interface (A.21).
   * CLI: `--ui` and `BCODE_REVIEW_UI` removed (7.5).
   * Naming: the surface kind is `code-review` and the keymap scopes are `code-review`,
     `code-review-home`, `code-review-wizard`, `code-review-files`, `code-review-threads`, and
     `code-review-composer` (each was `code-review-v2…`). Internal type and module names
     (`src/v2/`, `ReviewAppV2`, `ReviewSurfaceV2`) are unchanged.
   * Keymap configuration compatibility (decision): user overrides are keyed by scope and
     action id. Overrides written for V1's `code-review` and `code-review-home` scopes now apply
     to V2 where V2 declares the same action id (most navigation and annotation ids are shared,
     such as `bcode.code_review.down`, `bcode.code_review.next_file`, and
     `bcode.code_review.ask`); overrides naming V1-only actions are kept by the registry but never
     dispatched and are reported as unresolved actions. A V1 override still owns its key in the
     scope, so a V1 binding of a key V2 uses for another action shadows that action until the
     override is removed. Overrides for the development-only `code-review-v2…` scopes are not
     aliased: they need renaming to the scopes above (they also show as unresolved). No alias
     mechanism was added to the generic keymap registry for this.
8. **Measure the large-terminal frame.** Attribute frame time on a large terminal between the
   host root loop, the review surface, and terminal diff/flush; add a large-terminal benchmark
   case (13.2).

   **Implementation (phase 8).** Measured (13.2): the review surface paints a 400×120 scroll
   frame in 1.5–2.5 ms; the chat-root host adds about 6 ms per frame, 4–5 ms per key, an idle
   redraw loop, and one-event-per-cycle input that closes the review past 32 queued events.
9. **Standalone app.** `packages/code-review-ui` and the `bcode-review` binary with their own
   runtime loop, direct data calls, and `bcode_tui_theme` theme loading; the UI written against
   a `ReviewHost` trait; the host's theme mapping and session live stream made public; V2 names
   flattened; the review PTY smoke cases rewritten for the app (13.3).
10. **Visual system.** Semantic surface, text, and accent tiers derived from bcode theme roles;
    rounded panels, pill chips and buttons, selection bar, spinners, collapsible and resizable
    rails; generic pieces upstream in BMUX (13.4).
11. **Inline AI live feed.** AI cards follow the linked session live in place: thinking, tool
    lines, streaming answer, follow-up composer, stop, inline permission prompts, attention
    marks (13.5).
12. **Review assistant.** A docked assistant with chips, feed, and composer; review tools as
    daemon plugin tools governed by bcode permission rules; tool writes reflected live (13.6).
13. **Embedded-session model (Bcode-wide).** Specified and signed off before implementation:
    the daemon as the single session runtime, client-hosted tools, session origin metadata, and a
    sanctioned daemon start for consumer apps (13.7).
14. **Close.** Re-measure, parity, documentation, and tmux smoke at narrow, normal, and large
    sizes (13.8).

## 11. Acceptance criteria

### A. Correctness

* Expanded context always matches the reviewed revision on the correct side, including staged,
  commit, range, branch, PR, and deleted files.
* Expansion is order-independent and idempotent; refresh preserves it when hunks are unchanged.
* Both line numbers are shown for context and expanded lines.
* Anchors never move silently; outdated anchors are labeled.

### B. Performance and boundedness (fixture: 5k files, 500k diff lines)

* First frame with file tree and first file content: ≤ 150 ms after the index returns.
* Steady scroll frame (layout + paint): ≤ 2 ms p95; no frame rebuilds unaffected items.
* Resident memory bounded by the configured hunk budget, independent of review size.
* No service call or syntax highlighting on the render path.

### C. UX

* `bcode review` opens Home; every source type in 7.3 can start a review without typing revisions.
* Route changes never show a loading screen for runtime reinitialization.
* Unified and side-by-side both work at all widths with defined narrow-width degradation.
* All terminal layout rules in `AGENTS.md` (BMUX TUI Layout) hold: zero/tiny widths, CJK, emoji,
  combining marks, resize anchoring, and clipping/selection/hit-region agreement are tested.

### D. Ownership

* No code-review product logic in host crates; no GitHub-specific fields in shared review
  contracts; everything generic added to BMUX carries no review semantics.
* The code-review and GitHub provider plugins remain independently disableable.

## 12. Decisions

* Parallel rewrite; V1 is deleted after V2 reaches parity (done in phase 7).
* Stacks are linear PR chains (Graphite-style, GitHub native stacked PRs); no tool-specific
  metadata.
* Side-by-side ships in the first release. Layout is configurable as `auto`, `unified`, or
  `side_by_side`, matching `[tui.diff_viewer]`, with `auto` at a 120-column breakpoint by default.
* Hunk residency budget defaults to 64 MiB and is configurable.
* Whole-stack mode ships with comment attribution and publishing as described in 7.3.
* BMUX components are used wherever they fit; generic gaps are added upstream without Bcode
  semantics; review-specific behavior stays in the code review plugin.

## 13. Next: standalone app, visual system, inline AI, review assistant, embedded sessions

Phases 8–14 (section 10) follow V2 becoming the only review UI. They move the review out of the
chat host into its own app, give it a deliberate visual system, make AI work visible where it
happens, add a review assistant whose writes are governed by bcode permissions, and define how
any app embeds real bcode sessions.

### 13.1 Current state that motivates the plan

* **The review runs inside the chat program.** `bcode review` opens the `code-review` plugin
  surface through `run_prepared_plugin_surface` (`packages/tui/src/runtime.rs`). Its
  `prepare_plugin_surface_model` calls `initialize_tui_with_presentation`, which builds the chat
  state, the bcode client, and settings, and then the surface runs under `root_program::run`, the
  chat root loop. Every review frame therefore passes through the chat program's loop and its
  per-frame work, although the review uses none of the chat UI. The review's own frame path is
  measured (2.1: 0.87–1.56 ms p95 at 200×60 in an optimized build); the host loop and terminal
  diff/flush are not, and on a large full-screen terminal the review is visibly sluggish.
* **Data goes through a JSON service hop in-process.** The UI reaches the data layer through
  `ReviewBackend` (`src/v2/tui/backend.rs`) and `call_json`: every request and response is
  encoded to JSON and dispatched as a plugin service operation, even though the UI and the
  service are the same crate in the same process.
* **Theme and live sessions are host-private.** The theme arrives as `PluginTuiTheme` from the
  host; the mapping from a resolved bcode theme definition to `ComponentTheme` (and the markdown
  and syntax palettes) is `resolved_definition_theme` in `packages/tui/src/theme.rs`, which is
  `pub(crate)`. Linked AI sessions are followed through the host's `stream_plugin_session_view`
  (`packages/tui/src/plugin_surface_host.rs`), a private function. A program outside
  `packages/tui` cannot reuse either.
* **A standalone precedent exists.** `bcode-git` is `packages/git-ui` (`bcode_git_ui`) with
  `packages/git-ui/cli` (`bcode_git_ui_cli`, binary `bcode-git`); the git plugin's
  `plugins/git-plugin/src/cli.rs` runs the same `bcode_git_ui::run` as the bundled `bcode git`
  subcommand. It runs its own `bmux_tui_runtime` loop and loads themes itself through
  `bcode_tui_theme::discovery` (`default_theme_roots`, `discover_themes`), with unset fields
  falling back to `[tui.theme]` in `bcode.toml` (`packages/git-ui/src/config.rs`).
* **AI cards show only the final answer.** `AgentState::from_snapshot` (`packages/code-review-ui/src/agent.rs`)
  reduces a linked session's `SessionViewSnapshot` to a phase and the last completed assistant
  message. The snapshot already carries what a live view needs: transcript items (user,
  reasoning, tool invocation, permission, assistant), text streams, pending `permissions`,
  `thinking` state, and runtime cost and token totals.
* **Consumer apps cannot start the daemon.** `BcodeClient::for_daemon` (the explicit,
  verified-target constructor for independent consumers) never launches a daemon: it requires
  one already running. Only the default client auto-starts (`DaemonAvailability::AutoStart`).
  There is no way for a client to register tools on its connection, and sessions carry no
  record of which app created them.

### 13.2 Phase 8: measure the large-terminal frame

Measure before changing anything. Attribute each frame's time at narrow (80×24), normal
(200×60), and large (a full-screen large monitor, at least 400×110) sizes between:

* the host root loop (event dispatch, chat-program work, invalidation) outside the surface;
* the review surface (layout, row building, paint);
* terminal diff and flush (changed cells, bytes written, write time).

Add a large-terminal case to `screen_benchmark` (today 200×60) and record the numbers in 2.1
alongside the existing tables. The results decide which costs phase 9 removes by leaving the
chat loop, which belong to the review surface, and which are generic terminal costs to fix in
BMUX. Criterion B's steady scroll target (≤ 2 ms p95) applies at the large size too.

**Implementation (phase 8).** Measured; no product code changed. The review surface is cheap
on a large terminal; the sluggishness comes from the chat-root host and its input pipeline, plus
the bytes written for every frame.

* **Benchmark.** `screen_benchmark_sizes` (`packages/code-review-ui/src/tests/benchmark.rs`, ignored; run
  with `cargo test --release -p bcode_code_review_ui screen_benchmark_sizes --
  --ignored --nocapture --test-threads 1`) runs 200×60, 300×90, and 400×120 in both layouts
  (`BCODE_BENCH_SIZES` chooses others) and reports paint, encode, and input time plus bytes per
  frame, presenting into an in-memory BMUX terminal with the themed host colors. The narrow
  80×24 size was not measured in this phase.
* **Real app.** `bcode review` against the exported 5k-file fixture, built with the
  `debug-release` profile (`codegen-units=16`, `debug=line-tables-only`), run in tmux under
  `script(1)` with isolated state; CPU attributed with macOS `sample`, causes confirmed with
  lldb line breakpoints. tmux plus `script` is two pty layers, so terminal-write blocking is
  probably overstated compared with a fast native terminal.

Review surface alone (optimized benchmark; averages; side-by-side unless noted):

| Measurement | 200×60 | 300×90 | 400×120 |
| --- | --- | --- | --- |
| 5k-file scroll: paint / encode | 0.64 / 0.15 ms | 0.95 / 0.26 ms | 1.51 / 0.44 ms |
| 5k-file scroll: paint p95 | 1.31 ms | 1.63 ms | 2.39 ms |
| 5k-file scroll: bytes per frame (avg / max) | 21 / 54 KB | 36 / 90 KB | 54 / 129 KB |
| 5k-file scroll, unified: paint / encode / bytes | 0.58 / 0.13 ms / 16 KB | 1.05 / 0.29 ms / 27 KB | 1.59 / 0.42 ms / 42 KB |
| 40k-line file scroll: paint / encode / bytes | 0.53 / 0.14 ms / 24 KB | 1.56 / 0.40 ms / 49 KB | 1.63 / 0.55 ms / 76 KB |
| Held PageDown: paint / encode / bytes | 0.91 / 0.18 ms / 24 KB | 1.99 / 0.35 ms / 43 KB | 2.51 / 0.52 ms / 69 KB |
| Held `j`: paint / encode / bytes | 0.43 / 0.12 ms / 18 KB | 1.91 / 0.35 ms / 27 KB | 1.36 / 0.36 ms / 32 KB |
| 10 queued `j` per frame: paint / encode | 0.69 / 0.15 ms | 1.38 / 0.36 ms | 1.78 / 0.54 ms |
| Full redraw: paint / encode / bytes | 3.3 / 0.21 ms / 67 KB | 3.5 / 0.39 ms / 111 KB | 3.8 / 0.52 ms / 159 KB |
| Input handling per event | ≤ 0.006 ms | ≤ 0.006 ms | ≤ 0.005 ms |
| First content frame after the index | 114–189 ms | 121–123 ms | 111–120 ms |

Paint grows roughly with the cell count. Isolated maximums of about 30 ms happen when jumping
into the 40k-line file. Criterion B's steady scroll target is met at 200×60 and missed narrowly
at 400×120 (2.39–2.47 ms p95 scrolling the 5k-file review, 2.7–2.9 ms p95 under held PageDown).

Real app (ms per presented frame from `sample`; held PageDown at 40 Hz):

| Component | 200×50 | 400×120 |
| --- | --- | --- |
| Whole frame (`present_root_frame`) | 9.1 | 14.3 |
| Review surface `render_with_host` | 2.5 | 5.1 (2.7 of it row measurement in `layout_list`) |
| Host backdrop copy + extra full buffer | 0.6 + 0.2 | 1.8 + 0.8 |
| Keymap copy before each frame (`prepare_root_presentation`) | 2.2 | 1.3 |
| Other chat-frame work (chrome, transcript sync) | about 2.1 | about 2.5 |
| BMUX diff + ANSI encode | 0.16 | 0.9 |
| stdout write (blocking) | 1.4 | 1.9 (15 per frame while wheel scrolling) |
| Event handling per key (`update`) | 5.2 (3.3 of it the keymap copy) | 3.9 (2.6 of it the keymap copy) |
| Idle frames with nothing changing | 48/s at 2.9 ms each | 34/s at 6.9 ms each |
| Main thread busy holding PageDown | 47% | 54% (96% wheel scrolling at 30 Hz) |
| Content frames/s: PageDown 40 Hz / `j` 60 Hz / wheel 30 Hz | 27 / 40 / not measured | 26 / 36 / 16 |
| Frames per input event | 0.66–0.70 | 0.57–0.67 (2.7 per isolated wheel notch) |
| Catch-up after releasing a held key | up to 1443 ms (PageDown) | 116–577 ms (PageDown), about 16 ms (`j`) |
| Bytes per content frame: PageDown / `j` | 9.6 / 13 KB | 45–64 / 28 KB |

Excluding the surface render, encode, and write, the host adds about 5 ms per frame at 200×50
and 6.4 ms at 400×120, plus 4–5 ms of handling per key and a continuous idle frame loop. The
surface is 2.5–5 ms of a 14 ms frame.

Attribution (`packages/tui` paths are the chat host; line numbers at this phase's revision):

* **Idle redraw loop (host).** `root_program.rs` requests a redraw after every committed frame
  while the hidden chat transcript is invalidated (`presentation_committed`), and the flag is
  cleared only when the transcript paints (`transcript_v2_host/paint.rs`), which it never does
  while a plugin owns the screen (`chat_loop.rs`, `!foreground_owns_canvas`). lldb on an idle
  400×120 review: 26 of 30 redraw requests came from that branch, the rest from timers.
* **Keymap copied per frame, event, and poll (host).** `fresh_session_settings`
  (`root_program.rs`) copies the whole `KeymapRegistry` (`plugin_surface_host.rs`) from
  `prepare_root_presentation`, every poll, and input routing: 19–32% of busy main-thread time,
  including dropping the copies on worker threads.
* **One input event per cycle, and closing on overflow (host).** Input that arrives while the
  surface is out for `drain_effects` is queued and replayed one event per poll and drain
  (`chat_loop.rs`); frames are skipped meanwhile (`root_plugin_paint_pending`). Past 32 queued
  events the host sets `close_after_drain`: a burst of 100 `j` closes `bcode review` every time
  (60 does not). This is a correctness bug as well as the cause of the catch-up lag.
* **Backdrop copy (host).** For the plugin screen, `paint_backdrop` (`chat_loop.rs`,
  `foreground.rs`) allocates a second full-size buffer every frame and copies it back cell by
  cell: about 2.6 ms per frame at 400×120 and 17.5% of each idle frame.
* **Full damage for every plugin input (host and BMUX).** `root_program.rs` sets
  `Damage::Full` for plugin input, so BMUX's scroll-region shortcut never applies; it also needs
  a full-width region (`scroll_plan`), which the file panel prevents.
* **Blocking terminal write (BMUX).** `packages/tui/src/terminal.rs` in BMUX writes and flushes
  stdout on the UI thread: 1.4–1.9 ms per frame under held PageDown and about 15 ms per frame
  (27% of busy time) wheel scrolling at 400×120 under tmux.
* **Row measurement (surface and BMUX).** `layout_list` (`surface.rs`) with
  `VirtualSourceList::layout_pinned`, `ItemView::layout`, `RowContext::build`, and
  `layout_diff_row`: about 2.7 of the 5.1 ms surface render per PageDown frame at 400×120.
* **Syntax highlighting (surface).** `schedule_syntax` (`surface.rs`) highlights whole old and
  new blobs on the blocking pool; paging through the 5k-file review kept one core busy for the
  whole run, and each completion triggers another poll, drain, and frame.
* **Wheel animation (surface).** The 90 ms scroll animation makes about 2.7 frames per wheel
  notch, each paying the full host overhead.

Consequences for later phases:

* Removed by phase 9, provided the app runs its own loop and does not reuse the root
  plugin-surface host: the idle redraw loop, the keymap copies (the app holds its keymap by
  reference), the one-event-per-cycle input replay and its close-on-overflow (the app handles
  events directly, replays all queued input before a drain, coalesces repeated motion keys, and
  does not take the surface out when no effects are pending), and the backdrop copy. If the chat
  host keeps embedding plugin surfaces, the same fixes apply there; the close-on-overflow is a
  bug to fix regardless.
* Needed regardless of phase 9:
  * BMUX: move terminal output off the UI thread on macOS (as `LinuxTerminalOutput` does on
    Linux) and drop superseded frames under backpressure; support scroll regions that are not
    full width where the terminal allows it (left/right margins).
  * App and surface: report scroll damage for the list area instead of full damage; cache row
    measurements per row key and width (about 1–2.5 ms per PageDown frame at 400×120); bound
    syntax highlighting to visible windows, cancel jobs for files scrolled past, and batch
    completions into one redraw.
  * Keep the 90 ms wheel animation and re-check it once the host costs are gone; shorten it on
    large terminals or merge wheel input into the running animation if it still saturates.

**Chat-host fixes (after phase 8).** The generic host costs are fixed in the chat host itself,
since they affect every plugin surface:

* A foreground that owns the canvas hides the transcript, so pending transcript work neither
  requests frames after a commit nor turns other updates into redraws; the transcript stays
  invalidated and repaints in the frame that closes the foreground. Composer-cursor and status
  animation under a plugin surface advance without presenting.
* `BmuxKeyMap`, the app's effective keymap, and `FreshSessionSettings` share the
  `KeymapRegistry` through an `Arc`; nothing copies it per frame, event, or poll.
* `PluginTuiSurface::has_pending_effects` (default true) lets a surface say it has no
  `drain_effects` work; the host then never lends it out. Input queued during a drain is
  replayed in one poll, stopping only at an action the root must apply; queued pointer motion
  and resizes coalesce; the queue is bounded (4096, newer input dropped and logged) and never
  closes the surface. Esc / Ctrl-C request a close only while an operation holds the surface.
* No backdrop is painted (or allocated) under a canvas-owning foreground.
* Plugin input keeps whole-surface damage: surfaces report no damage, BMUX diffs cells so
  full damage writes only changed cells, and scroll-region acceleration would need a surface
  scroll hint and a full-width scrolled region.

Measured at 400×120 with the same `debug-release` setup (tmux plus `script(1)`, `sample`):

| Measurement | Before | After |
| --- | --- | --- |
| Idle frames | 43/s, 17% main thread busy | 0/s, idle |
| `update` per key (held `j`, 60 Hz) | 1.23 ms | 0.06 ms |
| Frame (`present_root_frame`), held `j` | 4.9 ms | 1.9 ms |
| Frame, held PageDown (40 Hz) | 7.9 ms | 3.8 ms |
| Content frames/s: `j` 60 Hz / PageDown 40 Hz | 32 / 27 | 44 / 38 |
| Catch-up after held PageDown | 47 ms | 6 ms |
| Burst of 100 `j` (and 300 `k`) | closes the review | stays open, last frame after 23 ms |

**Surface and terminal-output costs (after phase 9).** The costs phase 8 attributed to the
surface and to BMUX, which leaving the chat host does not remove, are fixed in the standalone app
and in BMUX:

* **Row measurement.** BMUX already retained item layouts by key, width, and layout revision;
  paging was slow because every page shows rows never built before. While idle after each frame
  the review now builds the rows of the viewport and the pages above and below it and has the list
  lay them out (`VirtualSourceList::prepare_rows`, `VirtualListComponent::prepare_around`), so the
  frame that reveals them only paints. Rows were also rebuilt whenever any highlight landed (one
  global epoch in the row cache key); the key now holds a per-blob stamp
  (`SyntaxStore::stamp`), so only the rows of the file whose highlight landed are rebuilt (in idle
  time too). BMUX fills and writes printable ASCII a row slice at a time.
* **Syntax highlighting.** Highlighting uses BMUX's incremental `TextHighlight` in steps of 2,000
  lines on the blocking pool: the first step's lines show at once, and each job carries a cancel
  token that `schedule_syntax` sets when its file leaves the screen (a blob read arriving after
  that is dropped). Highlights of files off screen land without a frame; the run loop coalesces
  wakes, so completions arriving together are drawn by one frame. On the 5k-file fixture the
  highlighting CPU while paging is unchanged (about 20% of one core in both runs below): its files
  are small, so jobs finish before their files leave the screen. Cancellation bounds the large
  files.
* **Scroll damage.** Virtual lists report how far their rows scrolled while painting
  (`PaintCx::hint_scroll`), keeping full-repaint semantics. The app presents with
  `ScrollRegionAcceleration::Auto`: BMUX counts the cells left to rewrite with and without the
  scroll and scrolls full rows when that saves cells, also when the list is narrower than the
  screen (the file panel's rows are then rewritten), or, under `AutoWithMargins`, only the list's
  columns with left/right margins (DECLRMM). Margins are selected only for terminals detected by
  `TerminalCapabilities::left_right_margins` (iTerm2, Ghostty, xterm, never inside tmux or screen).
* **Terminal output.** BMUX's `TtyOutput` (Linux-only before) reopens stdout's terminal device on
  every Unix, and the app presents through `AsyncTerminalPresenter`: frames are written by an
  output thread and the runtime prepares no frame while one is in flight, then draws the latest
  state, so superseded frames are never written. `Terminal::stage` moves the retained cells into
  the staged frame instead of copying them.
* **Wheel animation.** Wheel notches extend the running 90 ms motion. A notch now costs about
  three frames of 0.8–1.0 ms of UI-thread work (paint, encode, idle preparation) at 400×120, with
  the write on the output thread, so the animation was kept.

Optimized `screen_benchmark_sizes` (paint p95 / average, bytes per frame; side-by-side in
parentheses where different; `Auto` acceleration after):

| Measurement | Before | After |
| --- | --- | --- |
| 200×60 5k-file scroll paint p95 | 0.92 (1.05) ms | 0.52 (0.45) ms |
| 200×60 held PageDown paint p95 | 0.87 (0.72) ms | 0.33 (0.30) ms |
| 300×90 5k-file scroll paint p95 | 1.45 (1.59) ms | 0.83 (0.83) ms |
| 300×90 held PageDown paint p95 | 1.33 (1.72) ms | 0.54 (0.91) ms |
| 400×120 5k-file scroll paint p95 / avg | 2.20 / 1.51 (2.16 / 1.31) ms | 1.14 / 0.83 (1.16 / 0.83) ms |
| 400×120 40k-line scroll paint p95 | 1.82 (2.01) ms | 0.85 (0.85) ms |
| 400×120 held `j` paint p95 | 1.21 (1.22) ms | 0.88 (0.87) ms |
| 400×120 held PageDown paint p95 / avg | 2.44 / 1.94 (2.48 / 2.18) ms | 1.18 / 0.83 (1.23 / 0.93) ms |
| 400×120 10 queued `j` paint p95 | 2.11 (2.44) ms | 1.17 (1.27) ms |
| 400×120 encode average | 0.42–0.50 ms | 0.38–0.58 ms (includes the scroll evaluation) |
| 400×120 bytes, 5k-file scroll | 42 (54) KB | 28 (36) KB; 19 (28) KB with margins |
| 400×120 bytes, 40k-line scroll | 56 (76) KB | 28 (34) KB; 20 (25) KB with margins |
| 400×120 bytes, held `j` | 19 (32) KB | 6.3 (9.8) KB; 3.1 (6.8) KB with margins |
| 400×120 bytes, held PageDown | 59 (69) KB | unchanged (a page moves every row) |

Real app (`bcode-review range fixture/base main` on the exported 5k-file fixture, `debug-release`
build with `codegen-units=16`, 400×120 tmux pane under `script(1)`, 15 s of input, `sample`;
before is the same app without these changes; per presented frame):

| Measurement | Before | After |
| --- | --- | --- |
| PageDown: render / encode | 1.64 / 0.37 ms | 0.76 / 0.40 ms |
| PageDown: blocking write on the UI thread | 1.52 ms | none (1.48 ms on the output thread) |
| PageDown: idle preparation after the frame | none | 1.08 ms |
| `j`: render / encode / write on the UI thread | 1.09 / 0.28 / 1.04 ms | 0.70 / 0.27 / none |
| `j`: bytes per frame | 38 KB | 18 KB (scroll regions in 171 of 375 frames) |
| Wheel: render / encode / write on the UI thread | 0.93 / 0.22 / 0.94 ms | 0.48 / 0.37 / none |
| Wheel: bytes per frame | 40 KB | 19 KB (scroll regions in 288 of 671 frames) |
| Highlighting CPU while paging | 3.2 s of 15 s | 3.0 s of 15 s |

### 13.3 Phase 9: standalone app

* **Crates.** `packages/code-review-ui` (the review UI, moved from `src/v2/tui/`) and
  `packages/code-review-ui/cli` with a `bcode-review` binary, mirroring `bcode-git`. The data
  layer the UI calls (service operations, document) becomes callable as Rust from the UI crate,
  in `packages/code-review` (the implementation crate beside the existing
  `packages/code-review/models`); the code review plugin keeps its service interface, CLI
  registration, and (phase 12) tools as adapters over the same functions, so other consumers
  (agents, other plugins) are unaffected.
* **Own loop, direct calls.** The app runs its own `bmux_tui_runtime` loop and calls the data
  layer directly (no JSON encoding, no service dispatch). It uses the same state database and
  code paths as the service, so the app and the daemon plugin agree on every record.
* **Theme.** Loaded like `bcode-git`: `bcode_tui_theme` discovery and `[tui.theme]` from the
  bcode configuration.
* **Entry points.** `bcode review` remains a bundled subcommand and runs the app in-process the
  way `bcode git` runs `bcode-git`; `bcode-review` takes the same subcommands (7.5).
* **`ReviewHost`.** The UI is written against a small host trait with four parts:
  * terminal: size, input events, and frame submission (the loop);
  * theme: the `ComponentTheme`, diff styles, icon policy, and change notifications;
  * sessions: create, send, follow a session's live view, open a session in Bcode;
  * clipboard.

  The app provides the standalone implementation. Embedding the review in the chat TUI later is
  another implementation of the same trait over the plugin surface host, with no change to the
  UI. Chat embedding is not maintained in phase 9; the boundary is what keeps it cheap.
* **Shared functions.** The host's theme mapping (`resolved_definition_theme`) moves to
  `bcode_tui_theme` as a public function, and the live session view stream
  (`stream_plugin_session_view`) becomes a public function of `bcode_session_view` /
  `bcode_client`. The chat host calls the public versions, so there is one implementation of
  each.
* **Names.** Internal V2 names are flattened: `src/v2/` modules move to the crate root,
  `ReviewAppV2` becomes `ReviewApp`, `ReviewSurfaceV2` becomes `ReviewSurface`, and similar.
  Persisted and configured spellings (`presentation_v2_json`, the `presentation_v2` JSON field,
  the `review_v2` configuration table) keep their names or gain a read alias; only Rust names
  change.
* **Smoke.** The 11 cases in `scripts/smoke-continuous-tui-pty.py` that open `bcode review`
  (`completion-review` and the ten `review-*` cases) are rewritten for the app.

**Implementation (phase 9).**

* **Data crate.** `packages/code-review` (`bcode_code_review`) holds the repository reads
  (`ReviewData`), the stored state (`ReviewStore`), provider routing, viewed marks, and scopes,
  plus the renderer-neutral review document (`document`) and publish planning
  (`publish_plan`). The document now lives in a crate with no terminal dependency, so its
  portability is enforced by the compiler. The plugin's `src/service.rs` adapts the same
  functions to `bcode.code_review/v1` JSON operations.
* **UI crate.** `packages/code-review-ui` (`bcode_code_review_ui`) holds the routes (`app`,
  `home`, `wizard`, `surface`) and every module that was under the plugin's review UI, at the
  crate root. Screens implement `view::View` (`prepare_keymap`, `render`, `handle_event`,
  `poll`) and return `action::Action` (`None`, `Redraw`, `Close { edit_sources }`,
  `OpenSession`).
* **Typed data access.** `services::ReviewServices` replaces the JSON backend: every request
  type implements `services::Operation` (its response type, the operation name, and the call),
  so a screen calls `services.call(request).await` and gets the typed response. Calls run on
  the blocking pool against a shared `ReviewData` and the configured `ReviewStore`; providers,
  publishers, and importers go through the host's plugin services.
* **Host boundary.** `host::ReviewHost` covers the theme, sessions (create, send, follow,
  the command that opens one in Bcode), plugin services, clipboard, external URLs, task and
  blocking-task spawning, and redraw requests; a host requests a frame whenever a spawned task
  finishes, through the wake-up the driving loop hands it (`connect_redraw`). Only the standalone
  host talks to the daemon, so `bcode_code_review` has no daemon client dependency. `context::UiContext` pairs the host with the resolved keymap and the
  `[tui.diff_viewer]` layout preference. Host-provided completion of the comment composer
  (the chat host's focused-text-field protocol) has no standalone provider and was removed with
  the plugin surface.
* **Run loop.** `run.rs` drives the app with `bmux_tui_runtime`: input goes straight to the app;
  host wakes (coalesced) poll it and present; every presented frame polls again, so animations
  run at the runtime cadence and the loop idles when nothing changes. `o` on a linked session
  ends the runtime, runs `bcode tui <session>` with the terminal handed over, and resumes the
  app. Inside `bcode review` that command is the running Bcode executable and the client may
  start the daemon; the standalone `bcode-review` uses `bcode` from the path and only attaches
  to a running daemon (13.7's sanctioned daemon start is still missing).
* **Entry points.** `cli.rs` is the shared command line: the plugin's `bcode review`
  registration runs it in process, and `bcode-review` takes the same subcommands.
* **Tests and benchmarks.** The scenario harness (`testing/harness.rs`) drives any `View` with
  a `ScriptedHost` that captures tasks, redraws, clipboard writes, sessions, and followed
  session views, and serves the fake provider and publisher. `testing/backend.rs` gives screens
  the real `ReviewServices` over a private state directory, answering on the calling task so
  completions arrive in a fixed order. `screen_benchmark`, `screen_benchmark_sizes`
  (`bcode_code_review_ui`), `document_benchmark`, and `data_layer_benchmark`
  (`bcode_code_review`) keep their measurements.
* **Plugin.** `plugins/code-review-plugin` keeps the `bcode.code_review/v1` JSON adapters over
  `bcode_code_review` (`src/service.rs`), the `diff.toggle` palette command, the keymap
  declarations in its manifest (registered with bcode's keymap catalog, and read at build time
  by the app's `keymap.rs`, so user overrides of the `code-review*` scopes keep working in
  both), and, under `static-bundled`, the `bcode review` CLI registration. It declares no TUI
  surface and depends on the UI crate only for that registration. `packages/tui` lost the
  generic standalone-surface launcher with it (7.1).
* **Daemon availability.** `publish-github` is the only subcommand that needs the daemon before
  running: `bcode review` starts it first, while the standalone binary attaches only to a running
  one and reports "Bcode is not running" otherwise (questions to Bcode report the same).
  `publish-github` builds the review bundle with `ReviewStore::review_bundle` directly and uses
  the daemon only for the GitHub publisher call. (The command previously fetched the bundle with
  `review.bundle.get` and a `ReviewContextRequest` payload, which that operation rejects because
  it takes a `PublishReviewRequest`.)
* **Smoke.** The review cases of `scripts/smoke-continuous-tui-pty.py` drive the app:
  `review-home-cancel`, `review-invalid-repository`, `review-home-new-workspace` (wizard, then
  reopen after restart), `review-ask-open-session` (ask Bcode, open the linked session, return;
  replaces the chat-routing cases `review-linked-navigation` and `review-home-workspace-chat`),
  `review-click-save-resume` and `review-wizard-unstaged` (replacing the preset-click cases),
  `review-tree-scroll-resize`, `review-range-edit-thread`, and `review-pointer-no-rewrite`.
  `completion-review` was dropped with the chat host's composer completion. A step may set
  `handoff: true` to allow the alternate screen to be left and re-entered while the app hands
  the terminal to `bcode tui`. Review fixtures enable the harness plugin, which real sessions
  need. App behavior is documented in `docs/code-review-app.md`.

### 13.4 Phase 10: visual system

The design learns from bdata (`packages/tui-components/src/theme.rs`, `draw.rs`, and
`packages/app/src/view` in that repository): a small set of semantic tiers, panels, and pills
used consistently.

* **Semantic tiers.** Surfaces `canvas`, `surface`, `surface_alt`, `overlay`; text `text`,
  `strong`, `muted`, `dim`; accents `accent`, `accent_alt`, `on_accent`. Each tier is derived from
  bcode theme roles (`surface.raised`, `surface.overlay`, `control.focused`, the theme accent, and
  so on) with fallbacks through `ComponentTheme::derive`, so every bcode theme, including ones
  that define few roles, produces a complete review theme.
* **Warning means "needs you".** Warning color is reserved for things that need the reviewer
  (a pending permission, an unsaved review with comments, a failed AI request). Status roles stop
  using it: modified files use warning today (6.3) and move to an accent or neutral role.
* **Components.** Rounded panels whose border and title take the accent when focused, with a
  right-aligned title for status; layered surfaces (canvas, panel, overlay); a header of pill
  chips; real button pills for actions such as publish; a `▌` selection bar on the selected row;
  spinners next to the thing that is busy (a loading file, a running AI card) rather than one
  global indicator; collapsible rails with drag-resize and a zoom that gives one pane the whole
  screen.
* **Placement.** Generic pieces (rounded titled panels with right titles, pills, rail collapse and
  resize) go upstream into BMUX without review semantics (1.3); review-specific composition stays
  in the review UI.

### 13.5 Phase 11: inline AI live feed

Asking an AI question keeps the reviewer where they are: the card at the anchor follows the
linked session live, from the session's `SessionViewSnapshot` stream (13.3) instead of
`AgentState::from_snapshot`'s last answer.

* **Card contents.** The question (marked `▍`); reasoning collapsed as `▸ thinking · N words`;
  tool lines with `◌` running, `✓` done, `✗` failed, showing the last few and `+K earlier`; the
  answer as streaming markdown; a footer with a spinner, elapsed time, and tokens and cost.
* **In place.** A follow-up composer inside the card, stop (cancels the running turn), and inline
  permission prompts (Allow, Always, Deny) answered without leaving the review.
* **More.** "Show all" opens the full session transcript in a panel inside the review; `o` still
  opens the session in Bcode.
* **Attention.** When an off-screen card needs the reviewer (a permission prompt, a failure, a
  finished answer), a toast appears and the scrollbar marks the card's position.
* **Bounds.** Cards keep a bounded window of transcript items and reuse the identity-keyed layout
  cache (6.4); streaming updates relayout only the card that changed.

### 13.6 Phase 12: review assistant

* **Dock.** `ctrl+k` opens a docked assistant rail in the bdata style: chips for model, effort,
  agent, and permission mode; the session feed (rendered like 13.5); a composer; the rail
  collapses like other rails (13.4).
* **Review tools.** Review operations are exposed to agents as daemon plugin tools of the code
  review plugin, so any bcode session (the assistant, an AI card's session, or a chat session)
  can use them:
  * read: review summary, sources, pull request and stack metadata, file and hunk context,
    threads, CI status;
  * write: draft a comment, reply, resolve, mark viewed, suggest a change;
  * external: publish the review, post a pull request comment.
* **Permissions.** Each tool declares a policy operation (`ToolPolicyOperation`), so bcode
  permission rules (ask, allow, deny, remembered decisions) govern them like any other tool.
  External tools are high-risk and ask by default.
* **Live reflection.** The app reflects tool writes as they happen (a drafted comment appears at
  its anchor, a resolve updates the thread), through the same annotation store and refresh path
  as the reviewer's own edits.

### 13.7 Phase 13: embedded-session model (Bcode-wide)

This phase changes Bcode beyond the review and needs its own specification and sign-off before
implementation. The goal: bdata and any other consumer, embedded or not, create real bcode
sessions that are visible and resumable everywhere.

* **One runtime.** The daemon is the single session runtime. "Embedded" means an in-process
  client of the daemon, not a second runtime.
* **Client-hosted tools.** An app registers typed tools on its connection; the daemon routes
  calls for them back to that connection. When the app is not connected, a call fails with a
  clear tool error and the session history stays intact.
* **Session origin.** Sessions record their origin (app and app context) for listing and
  filtering.
* **Sanctioned daemon start.** Consumer apps get a supported way to start the daemon (today the
  explicit constructor, `BcodeClient::for_daemon`, only attaches to a running one).
* **SDK mode.** The in-process SDK mode remains for stateless use and app-owned persistence.
* bdata's migration to this model is follow-up work in that repository.

### 13.8 Phase 14: close

Re-measure (13.2's sizes and the 2.1 benchmarks), close parity (Appendix B) for everything the
move touched, update this document and `docs/code-review-plugin-extension-points.md`, and run
the tmux smoke at narrow, normal, and large sizes.

## Appendix A. V1 behavior inventory

This is the parity checklist for deleting V1. Every item has a stable ID, a one-line
description, and the V1 location. Items marked **(replaced: …)** are ones the spec deliberately
supersedes; V2 closes them through the replacement, not by copying V1. Items marked
**[V1 defect]** or **[unreachable]** are V1 quirks V2 must not reproduce. Each one needs an
explicit keep/drop decision in phase 6.

**Location legend.** Paths are relative to the worktree root. Line numbers are taken from commit
`e05285ed`. The phase 0 commit adds test hooks to `code_review_tui.rs`, which shifts its lines
after 254 by +32 and its lines after 1,830 by +20.

| Short name | Path |
| --- | --- |
| `tui.rs` | `plugins/code-review-plugin/src/code_review_tui.rs` |
| `render.rs` | `plugins/code-review-plugin/src/code_review_tui_render.rs` |
| `view.rs` | `plugins/code-review-plugin/src/code_review_tui_view.rs` |
| `display.rs` | `plugins/code-review-plugin/src/code_review_tui_display.rs` |
| `controls.rs` | `plugins/code-review-plugin/src/code_review_controls.rs` |
| `home.rs` | `plugins/code-review-plugin/src/code_review_home.rs` |
| `keys.rs` | `plugins/code-review-plugin/src/keybindings.rs` |
| `lib.rs` | `plugins/code-review-plugin/src/lib.rs` |
| `cli.rs` | `plugins/code-review-plugin/src/cli.rs` |
| `plugtui.rs` | `plugins/code-review-plugin/src/tui.rs` |
| `manifest` | `plugins/code-review-plugin/bcode-plugin.toml` |
| `models` | `packages/code-review/models/src/lib.rs` |
| `gh.rs` | `plugins/github-review-publisher-plugin/src/lib.rs` |
| `gh-manifest` | `plugins/github-review-publisher-plugin/bcode-plugin.toml` |
| `launcher.rs` | `packages/tui/src/code_review_launcher.rs` |
| `shell.rs` | `packages/tui/src/terminal_shell.rs` |
| `ext-doc` | `docs/code-review-plugin-extension-points.md` |

### A.1 Entry points and CLI

* **INV-ENT-01** `bcode review` with no subcommand opens the review home for the current
  directory (resolved to an absolute path in the CLI process). `cli.rs:182` (`run`),
  `cli.rs:59` (`resolve_repo_argument`)
* **INV-ENT-02** `bcode review unstaged [--repo P]` opens a review of unstaged working-tree
  changes. `cli.rs:88`, `cli.rs:189`
* **INV-ENT-03** `bcode review staged [--repo P]` opens a review of the index. `cli.rs:94`
* **INV-ENT-04** `bcode review all [--repo P]` opens a review of staged and unstaged changes
  together. `cli.rs:100`
* **INV-ENT-05** `bcode review last-commit [--repo P]` opens a review of HEAD's changes.
  `cli.rs:106`
* **INV-ENT-06** `bcode review range BASE HEAD [--two-dot] [--repo P]` reviews a revision range.
  Merge-base (`...`) semantics are the default; `--two-dot` switches to `..`. `cli.rs:112`,
  `cli.rs:194`
* **INV-ENT-07** `bcode review repo [--repo P]` opens a repository browser where any file can be
  commented. `cli.rs:125`
* **INV-ENT-08** `bcode review publish-github` previews or submits to a GitHub PR without opening
  the TUI.
  * Flags: `--github-repo`, `--pr`, `--token-env` (default `GITHUB_TOKEN`),
    `--submit-event comment|request-changes|approve`, `--summary`,
    `--fallback-unmapped-to-summary`, `--submit` (preview is the default),
    `--target unstaged|staged|all|last-commit`.
  * Output: prints the preview, or the submit message plus the review URL.
  * This is the only subcommand that requires the daemon.
  * `cli.rs:131`, `cli.rs:254` (`publish_github_review`), `cli.rs:47` (`requires_daemon`)
* **INV-ENT-09** `publish-github` auto-detects the repository from the `origin` remote, and the
  PR number from the branch name (`pull/N/...` or `pr/N`). If detection fails it errors with
  "pass --pr". `cli.rs:378`, `cli.rs:394`
* **INV-ENT-10** Direct CLI targets open the `code-review` surface with a transient (unsaved)
  workspace via `OpenTuiSurface`. `cli.rs:68` (`surface_outcome`), `tui.rs:604` (factory),
  `plugtui.rs:53`, `models:149` (`ReviewWorkspace::from_target`)
  * (replaced: §7.5 — direct entry opens the Review route with a transient workspace *that can
    be saved*. New entry points are `branch`, `compare`, `commit`, `pr`, `stack`, `resume`.)
* **INV-ENT-11** When a review exits, the user returns to home. Exiting home ends
  `bcode review`. `shell.rs:565` (`run_review_home` loop)
  * (replaced: §7.1 — internal routes inside one plugin surface; no runtime reload or TUI
    reinit between routes.)
* **INV-ENT-12** Each home↔review transition shows a loading screen ("Opening review home…",
  "Opening review workspace…", "Opening review target…") and reloads the plugin runtime.
  `shell.rs:572`, `shell.rs:617`, `shell.rs:651`, `launcher.rs:95` (`prepare_surface`)
  * (replaced: §7.1 / §11.C — route changes never show a reinitialization loading screen.)
* **INV-ENT-13** Opening a review that cannot be prepared shows an in-app error screen instead of
  crashing, when the screen was released cleanly. `shell.rs:667` (`review_preparation_error`)
* **INV-ENT-14** `Ctrl+D` exits the review surface and the home surface, ignoring keymap
  overrides of other modifiers. `Esc` never exits a review. `tui.rs:2785`, `tui.rs:2533`,
  `home.rs:1411`, test `tui.rs:15040`
* **INV-ENT-15** The plugin contributes the palette command `diff.toggle` ("Diff: Toggle
  Panel"), which toggles the host inline diff surface. `lib.rs:214`, `lib.rs:235`
* **INV-ENT-16** Two TUI surfaces are declared: `code-review-home` and `code-review`.
  * `code-review` opens with options `{target, build_mode, workspace}`.
  * A missing or invalid target falls back to `Repository`.
  * `manifest:1668`, `manifest:1673`, `plugtui.rs:27`, `plugtui.rs:46`
  * (replaced: §7.1 — one surface with Home/NewReview/Review routes, launched through the
    generic host standalone-surface capability.)
* **INV-ENT-17** Code Review and the GitHub provider are independently disableable through the
  `static-bundled-code-review-plugin` and `static-bundled-github-review-publisher-plugin`
  features. `ext-doc` "Disableability"

### A.2 Home screen

* **INV-HOME-01** Header reads "Bcode Reviews": visible / active / need-attention / drafts /
  published / setup counts, plus an active-filter label
  `[archived:on|off drafts:on|all attention:on|all search:…]`. `home.rs:1630`, `home.rs:1653`,
  `home.rs:1685`
* **INV-HOME-02** A preset list sits at the top with six entries; selecting one creates a
  workspace and opens it immediately in review mode. `home.rs:193` (`HOME_PRESETS`),
  `home.rs:1306`
  * Unstaged changes (`u`), Staged changes (`s`), Worktree + staged (`w`), Repository (`v`),
    Last commit (`l`), Custom review… (`n`).
  * Preset rows show the effective key hint for their action. `home.rs:225`
  * (replaced: §7.2 suggestions derived from repository state, plus the §7.3 wizard.)
* **INV-HOME-03** "Custom review…" (`n`/`e`) prompts for a title (an empty title becomes
  "Untitled review"), creates a workspace with no sources, and opens it in Build mode.
  `home.rs:549`, `home.rs:1213`, `home.rs:907`
  * (replaced: wizard step 3 title plus extra sources.)
* **INV-HOME-04** The workspace list is sorted most recently updated first. `home.rs:1932`
  * Each row shows: title · sources summary · health · draft/thread counts · last publish
    ("published Xm ago via P" or "never published") · viewed count · `next: <action>` ·
    relative time · archived marker.
  * `home.rs:2083` (`workspace_row_text`), `home.rs:2145` (relative time)
  * (replaced: §7.2 BMUX `table` with title, sources, viewed progress bar, drafts/threads,
    publish status, updated.)
* **INV-HOME-05** Health labels, evaluated in order: `archived`, `setup` (no sources),
  `needs sources` (none included), `changed since publish`, `drafts`, `published`, `active`.
  `home.rs:2020`
  * (replaced: §7.2 groups needs-attention / in-progress / published / archived. Mapping these
    labels into the groups must be defined.)
* **INV-HOME-06** "Needs attention" means health is setup / needs sources / changed since
  publish / drafts, or the workspace has any thread. `home.rs:2048`
* **INV-HOME-07** Next-action text per workspace: restore / add sources / include a source /
  publish draft updates / publish drafts / continue draft review / continue review.
  `home.rs:2055`
* **INV-HOME-08** A details panel (toggled by `d`) shows: title, health, id, repo, updated,
  archived, draft counts, last publish (publisher, output, result message), sources
  included/total, next action, the per-source list, the viewed-file count, and viewed paths.
  `home.rs:1803`, `home.rs:1902`
  * (replaced: §7.2 preview panel — file stats, sources, last publish.)
* **INV-HOME-09** An empty-state panel appears when there are no workspaces ("Your next review
  starts here") or when filters match nothing ("No matching reviews"). `home.rs:1732`
* **INV-HOME-10** Enter opens the selected workspace. A workspace with no included sources opens
  in Build. An archived workspace refuses to open: "restore it before opening". `home.rs:455`,
  `home.rs:420`, `home.rs:2016`
* **INV-HOME-11** `c` continues the most recent non-archived visible review. `home.rs:432`
  * (kept as §7.2 one-key continue-latest.)
* **INV-HOME-12** `o` forces open in review mode; `b` forces open in Build mode. `home.rs:470`,
  `home.rs:463`
  * (replaced: Build mode removed; see INV-SRC-01.)
* **INV-HOME-13** `r` renames the selected workspace inline; an empty title is rejected; Esc
  cancels. `home.rs:592`, `home.rs:1384`, `home.rs:960`
* **INV-HOME-14** `x` archives the selected workspace. `home.rs:1342`, `home.rs:925`
* **INV-HOME-15** `R` restores the selected archived workspace; if it is not archived, shows
  "selected review is not archived". `home.rs:1361`, `home.rs:947`
* **INV-HOME-16** `a` toggles inclusion of archived workspaces by reloading from the service.
  `home.rs:1332`
  * (replaced: archived group collapsed by default.)
* **INV-HOME-17** Jumping between workspaces of a given health, cycling through the visible list:
  * next/previous `setup` (`S`/`U`), `drafts` (`F`/`B`), `published` (`p`/`P`), attention
    (`M`/`N`).
  * `home.rs:487`, `home.rs:517`–`home.rs:545`
* **INV-HOME-18** `g`/`G` jump to the first or last review; `j`/`k`/↑/↓ move. `home.rs:390`,
  `home.rs:402`, `home.rs:1295`
* **INV-HOME-19** Tab switches keyboard focus between the preset list and the workspace list.
  With presets focused: j/k/↑/↓/g/G/Home/End navigate and Enter activates. `home.rs:1145`
* **INV-HOME-20** Only one async home action runs at a time; extra keys show "review action
  already in progress". Async failures show "failed to … workspace: …". `home.rs:1197`,
  `home.rs:863`
* **INV-HOME-21** Selection is preserved by workspace identity across reloads, archive, restore,
  and rename (not by row index). `home.rs:361`, tests `home.rs:2282`, `home.rs:2345`,
  `home.rs:2378`
* **INV-HOME-22** The footer shows status or prompt text ("search: …", "rename: …", "new review
  title: …") or default hints (Help, Open, Focus next, Search, New, Exit). `home.rs:2211`,
  `home.rs:2190`
* **INV-HOME-23** The home uses its own theme (`ReviewHomeTheme`), resolved from the
  presentation theme. `home.rs:36`
  * (replaced: §6.3 — home and review share one theme derivation.)

### A.3 Workspace and source management (V1 "Build" mode)

* **INV-SRC-01** There are two UX modes, Build (assemble sources) and Review. `m` toggles between
  them. Leaving Build is refused when there are no sources, none are included, or a source has
  materialization errors. `B` forces Build mode, which switches the sidebar to Sources.
  `tui.rs:3427`, `tui.rs:6094`, `tui.rs:6103`
  * (replaced: §7.3 wizard and composite sources. **Open question:** V2 still needs a way to
    edit an existing review's sources, covering every INV-SRC item below.)
* **INV-SRC-02** Source kinds: unstaged, staged, worktree (both), last commit, commit `rev`,
  commit range (`..` / `...`), branch compare (`..` / `...`), whole file, file range
  `path:start-end`, repository browser. `models:216`
  * Short labels. `render.rs:539`
  * V2 adds `Commits`, `PullRequest`, `StackLayer`, `Stack`, and `CurrentBranch` per §4.1, and
    must preserve unknown kinds (§9).
* **INV-SRC-03** Quick-add in Build: `u`/`s`/`w`/`l` add unstaged/staged/worktree/last-commit
  sources. `tui.rs:2754`–`tui.rs:2757`, `tui.rs:7991`
* **INV-SRC-04** `A`/`a` opens the add-source menu (unstaged, staged, working tree, last commit,
  file, repository browser, advanced…). The advanced menu offers branch compare, file range,
  commit, and commit range. Typed aliases are accepted, e.g. `commit`/`c`, `range`, `branch`,
  `file`/`f`, `staged`/`index`. `tui.rs:5568`, `tui.rs:5610`, `tui.rs:6134`, `tui.rs:6364`,
  `tui.rs:6394`, `tui.rs:6425`
* **INV-SRC-05** Commit source uses a fuzzy picker over recent commits, falling back to typing the
  rev. `tui.rs:6211`, `tui.rs:6474`, `tui.rs:7052`
* **INV-SRC-06** Commit range uses a base-commit picker, then a head-commit picker, falling back
  to typing `base..head` / `base...head`. `tui.rs:6226`, `tui.rs:6498`, `tui.rs:6517`,
  `tui.rs:4261`
* **INV-SRC-07** Branch compare uses a base-branch picker, then a head-branch picker, falling back
  to typing. `tui.rs:6245`, `tui.rs:6574`, `tui.rs:6591`, `tui.rs:7070`
* **INV-SRC-08** File source (`+`) uses a fuzzy picker over repository files. If no file list is
  available, it adds the selected file. Duplicates are rejected ("already included").
  `tui.rs:6264`, `tui.rs:7999`
* **INV-SRC-09** File range: pick a file, then enter `start-end`, falling back to typing
  `path:start-end`. `tui.rs:6279`, `tui.rs:4270`
* **INV-SRC-10** `C` edits the selected source's specification for commit, range, branch, file,
  and file-range kinds. `tui.rs:6160`, `tui.rs:4283`, `tui.rs:6739`
* **INV-SRC-11** Source inclusion: Space toggles one, `I` includes all, `E` excludes all, `V`
  inverts. `tui.rs:8309`, `tui.rs:8054`, `tui.rs:8059`, `tui.rs:8064`
* **INV-SRC-12** Empty sources: `n`/`N` jump to next/previous empty included source; `z` excludes
  all empty sources. `tui.rs:8089`, `tui.rs:8128`
* **INV-SRC-13** Diagnostics: `d` jumps to the next source with diagnostics; `Z` excludes sources
  with errors. `tui.rs:8157`, `tui.rs:8191`
* **INV-SRC-14** `M` toggles merge-base semantics on a range or branch source. `tui.rs:8225`
* **INV-SRC-15** `X` removes excluded sources; `P` removes duplicates. `tui.rs:8261`,
  `tui.rs:8281`
* **INV-SRC-16** `T` renames the workspace; `r` renames a source; `[`/`]` reorder sources; `-`
  removes a source. `tui.rs:6148`, `tui.rs:6184`, `tui.rs:8336`, `tui.rs:8355`, `tui.rs:8371`
  * **[unreachable]** `r`, `[`, `]`, `-` are not declared in the `code-review-build` keymap
    scope, so unbound keys are dropped as paste. `tui.rs:2774`–`tui.rs:2777`, `tui.rs:2501`
* **INV-SRC-17** `O` opens the selected source's first surface in the review. `Y` selects the
  source that owns the current surface. Enter activates a build row. `tui.rs:7819`,
  `tui.rs:7847`, `tui.rs:7869`
* **INV-SRC-18** `R` rematerializes sources (shows "materializing review sources…"). Workspace
  edits auto-save; the header shows pending save/reload. `tui.rs:7982`, `tui.rs:722`,
  `render.rs:307`
* **INV-SRC-19** Build workspace view shows:
  * summary line (sources, included, surfaces, empty, duplicates, diagnostics);
  * workspace title;
  * materialization state (in progress / failed with guidance / ready);
  * quick-add hints;
  * included sources with status, diagnostics (severity, code, message);
  * review surfaces list.
  * `render.rs:1035`, `render.rs:1123`, `render.rs:1163`, `render.rs:1268`
* **INV-SRC-20** Materialization diagnostics carry actionable recovery text (invalid revision,
  missing branch, missing head, invalid file source). One failing source does not fail the rest.
  `lib.rs:1858`, `lib.rs:1896`, tests `lib.rs:4623`, `lib.rs:4705`
* **INV-SRC-21** Multiple sources produce one review; each file or surface carries its source
  label (`[unstaged]` etc.) and source identity. `tui.rs:3494`, `render.rs:574`
  * (V2: `SourceHeader` items, §5.1.)
* **INV-SRC-22** Workspace persistence fields: id, title, repo root, sources (id, label, kind,
  included), created/updated/archived timestamps, viewed files, presentation state.
  `models:149`, `models:202`, `lib.rs:2752`
* **INV-SRC-23** Repository-browser review: whole-file surfaces with line numbers. File content is
  loaded on demand up to 1 MB; otherwise an unavailable reason is shown. File-range surfaces show
  only their slice. `lib.rs:128`, `lib.rs:2600`, `lib.rs:2008`, `lib.rs:2039`, `tui.rs:821`,
  `render.rs:1906`, `render.rs:1863`

### A.4 Diff navigation

* **INV-NAV-01** The diff pane shows one file at a time; selecting another file replaces it. The
  scroll and selected line are remembered per file path. `tui.rs:9432`, `tui.rs:9296`,
  `tui.rs:9309`, test `tui.rs:15502`
  * (replaced: §5.1 / §6 — one continuous virtualized document across all files.)
* **INV-NAV-02** `j`/`k`/↑/↓ move the selection by one semantic row (diff line, hunk header,
  context row, thread, comment, action). If the threads or repo sidebar is visible, they move the
  sidebar selection instead. `tui.rs:7646`, `tui.rs:7665`, `tui.rs:9703`, `tui.rs:9718`, test
  `tui.rs:14302`
* **INV-NAV-03** `g`/`G` go to the top or bottom of the review document. `tui.rs:9784`,
  `tui.rs:9795`
* **INV-NAV-04** `n`/`p` go to the next/previous file. `n` goes to the next search match instead
  when a search is active. `tui.rs:2850`, `tui.rs:9563`, `tui.rs:9698`
* **INV-NAV-05** `J`/`K` go to the next/previous hunk. `tui.rs:9810`, `tui.rs:9822`,
  `view.rs:526`
* **INV-NAV-06** `W`/`I` go to the next/previous unviewed file. `tui.rs:9664`, `tui.rs:9669`
* **INV-NAV-07** `M` goes to the next attention item: the next unviewed file, else the next open
  thread across the review. `tui.rs:9656`
* **INV-NAV-08** `}`/`{` go to the next/previous inline draft comment in the current file.
  `tui.rs:9002`, `tui.rs:9011`
* **INV-NAV-09** `]`/`[` go to the next/previous inline thread in the current file.
  `tui.rs:9128`, `tui.rs:9147`
* **INV-NAV-10** `u`/`i` go to the next/previous open thread in the current file. `tui.rs:9056`,
  `tui.rs:9065`
* **INV-NAV-11** `P`/`O` go to the next/previous open thread across all files. `tui.rs:9080`,
  `tui.rs:9089`, test `tui.rs:14846`
* **INV-NAV-12** `L` goes to the next AI-linked thread. `tui.rs:9104`
  * **[unreachable]** previous AI-linked thread (`B` in code) is shadowed by `B` = build mode and
    is not declared. `tui.rs:2882`, `tui.rs:9109`
* **INV-NAV-13** `:` opens jump-to-line. The number is a rendered row index in the current file,
  not a source line. `tui.rs:6296`, `tui.rs:6867`
  * (replaced: §6.2 `:` is the command palette. Jump to line must move into the palette and use
    real source line numbers.)
* **INV-NAV-14** `v` toggles a keyboard range selection anchored at the selected line; moving
  extends it; the status shows a range label. `tui.rs:10119`, `tui.rs:10173`
  * (conflict: §6.2 assigns `v` to "mark viewed and advance". The range key must be reassigned.)
* **INV-NAV-15** Esc clears, in order: range selection, then selected view target, then expands
  all collapsed threads. `tui.rs:2818`
* **INV-NAV-16** Enter activates, in order: the repo-tree row (when the repo sidebar is visible),
  the selected inline action / omitted-context row / thread header toggle, else jumps to the
  selected sidebar thread. `tui.rs:2837`, `tui.rs:12146`, `tui.rs:9245`
* **INV-NAV-17** Selecting a sidebar thread jumps the diff to the thread anchor (file and row).
  Review-level threads focus the thread. `tui.rs:9245`, `tui.rs:9228`
* **INV-NAV-18** The header shows title, file path, file x/N, surface kind, hunk x/N, row, draft
  count, thread counts and filter, viewed x/N, readiness ("ready" or "incomplete: N unviewed, M
  open"), and +/− totals. `render.rs:307`, `render.rs:403`, `tui.rs:11394`, `tui.rs:12941`
  * (replaced: §6.1 styled header with source chips, viewed progress bar, draft count, publish
    split button.)
* **INV-NAV-19** The footer shows the status message, or "Tab controls · ←/→ select · Enter
  activate · Esc content" plus effective key hints. `render.rs:422`
* **INV-NAV-20** Navigation over large reviews stays bounded (tested). Test `tui.rs:14198`
  * (replaced: §11.B performance criteria.)
* **INV-NAV-21** **[V1 defect]** PageUp, PageDown, Home, and End are declared in the
  `code-review` scope but have no handler in the review. `manifest:388`–`manifest:399`,
  `tui.rs:2795`
  * (V2: §6.2 line/page/half-page scrolling.)

### A.5 File tree and sidebar

* **INV-TREE-01** The sidebar has 7 modes, shown as a header tab bar: Included, Repository,
  Threads, General, Summary, Sources, Attention. `t` cycles through them; clicking a tab selects
  one. `tui.rs:5290`, `tui.rs:7630`, `tui.rs:7471`, `controls.rs:27`
  * (replaced: §6.1 — a file tree panel plus an optional threads panel, not a sidebar tab.
    Summary and Sources move to header chips and source headers.)
* **INV-TREE-02** `b` shows or hides the sidebar. The sidebar is auto-hidden below 80 columns; its
  width is 34 columns, capped at width − 30. `tui.rs:2823`, `tui.rs:13294`, `tui.rs:103`
  * (replaced: §6.1 resizable, collapsible file panel.)
* **INV-TREE-03** The Repository mode is a hierarchical tree (BMUX `TreeView`). Each file label
  shows opened marker `●`, viewed `✓`/`○`, status letter, source label, `💬drafts`, and
  `⚠open threads`. `render.rs:574`, `tui.rs:7145`
  * (replaced: §8.2 rich `tree_view` with styled spans and right-aligned decorations, §6.3
    styled status.)
* **INV-TREE-04** Tree keyboard: ↑/↓/j/k move focus without opening the file; → expands; ←
  collapses; Enter opens a file or toggles a directory. `tui.rs:8388`, `tui.rs:8466`,
  `tui.rs:8471`, `tui.rs:7241`, test `tui.rs:15623`
* **INV-TREE-05** Tree mouse: clicking a file opens it; clicking a directory toggles it; the wheel
  scrolls the tree independently of the diff. `tui.rs:7313`, `tui.rs:9598`, test `tui.rs:15652`
* **INV-TREE-06** The tree expands the selected file's directories and reveals the selection on
  file change and refresh. Duplicate paths from different sources keep separate identities.
  `tui.rs:9530`, `tui.rs:7300`, test `tui.rs:15559`
* **INV-TREE-07** Included mode lists `[✓] label` for each workspace source. `render.rs:453`
* **INV-TREE-08** Sources mode lists source kind, label, inclusion, and diagnostics, with an
  "A add source" hint. `render.rs:485`
* **INV-TREE-09** Summary mode shows title, repo, viewed x/N, +/−, open/resolved threads, general
  thread count, and sources/surfaces/commits counts. `render.rs:677`
* **INV-TREE-10** Threads mode lists threads filtered by All/Open/Resolved. Each row shows:
  marker, state tokens (open|resolved, draft:N, AI-exchange, suggestion, linked-session,
  external, published), kind/severity, path:line, external status
  `[provider:mapped|stale|unmappable]`, suggestion counts, latest body, and AI live state
  `🤖 …`. `render.rs:729`, `render.rs:848`, `render.rs:883`, `render.rs:896`
* **INV-TREE-11** General mode lists only review-level threads. Attention mode lists only open
  threads. `!` focuses Attention mode. `tui.rs:8860`, `tui.rs:7618`
* **INV-TREE-12** A thread detail panel under the list shows tokens, kind, severity, location,
  body, and the agent block (status, activity, answer preview), plus action buttons Reply /
  Resolve-reopen / Edit / Delete / Session / Ask. `render.rs:917`, `controls.rs:73`
* **INV-TREE-13** A thread filter toolbar has All / Open / Resolved / +Review note buttons.
  `render.rs:1008`, `controls.rs:81`
* **INV-TREE-14** Header actions are "File note" and "+Review note" buttons. `controls.rs:69`,
  `render.rs:181`
* **INV-TREE-15** Tab / Shift+Tab move keyboard focus into the header tabs and action rows; ←/→
  select; Enter activates; Esc returns to content. Focus is suspended on resize or focus loss.
  `controls.rs:289`, `controls.rs:345`, `controls.rs:497`
* **INV-TREE-16** `H` hides viewed files from the changed-file list (and advances off a viewed
  file). `tui.rs:9390`, `tui.rs:9329`

### A.6 Context expansion

* **INV-CTX-01** Hidden unchanged context appears as rows above the first hunk (expand up),
  between hunks (down, plus up when more than 20 lines are hidden), and after the last hunk
  (expand down). `view.rs:1297`, `view.rs:1428`
* **INV-CTX-02** Activating a hidden-context row (Enter, `x`, or click) reveals 20 lines in its
  direction and leaves a new hidden row for the remainder. Repeating reveals the next step.
  `view.rs:16`, `view.rs:1367`, `tui.rs:12075`, tests `view.rs:2026`, `view.rs:2062`
  * (replaced: §5.2 — interval-set expansion; N above/below with count prefix; reveal all;
    reveal the whole file; gap half-click mouse.)
* **INV-CTX-03** Expansion shows Loading and Unavailable(reason) states while full-file content
  is fetched. `view.rs:1159`, `view.rs:1445`, `render.rs:1680`
* **INV-CTX-04** Expansion keeps the viewport stable (`preserve_viewport_while`). `tui.rs:9756`,
  `tui.rs:12075`
* **INV-CTX-05** **[V1 defect]** Expanded lines are read from the working-tree file regardless of
  reviewed revision. Expansion state is keyed by file index plus a shifting range. The old-side
  column repeats the new line number. Expanded lines are not commentable ("expanded context lines
  are not commentable"). `tui.rs:9465`, `view.rs:1102`, `render.rs:1759`, `tui.rs:9855`
  * (replaced: §5.2 / §11.A — revision-correct blob reads, both line numbers, anchorable
    expanded lines.)

### A.7 Comments, drafts, and threads

* **INV-CMT-01** Anchor kinds:
  * line or range (`c` on a selected diff line, or on a range from `v` or mouse drag);
  * file-level (`F`, or the "File note" button);
  * review-level (`C`, or the "+Review note" button), stored under the path `<review>`.
  * Hunk headers are not commentable.
  * `tui.rs:10316`, `tui.rs:10280`, `tui.rs:10250`, `tui.rs:3923`, `tui.rs:12843`, test
    `tui.rs:15004`
* **INV-CMT-02** Anchor identity fields: kind, file path, diff row (and start/end), old/new
  start/end, old/new line, line kind, file flag, surface id, source id. `models:970`,
  `tui.rs:3927`
  * V2 keeps this model and adds the resolved head OID, per §5.3.
* **INV-CMT-03** The composer is an inline popup under the anchor row. Its title varies: "Review
  composer · action", "Reply to review thread · action", "Edit draft comment", or the preview
  variants. It has a placeholder and a markdown preview toggle (`Ctrl+P`). `render.rs:2610`,
  `render.rs:2672`, `render.rs:2786`, `tui.rs:2709`
* **INV-CMT-04** Composer submit actions: Save draft / Ask Bcode / Publish. Tab or → cycles to the
  next action; ← to the previous. The action row is clickable (Save draft, Ask Bcode, Publish,
  Cancel). `tui.rs:5181`, `tui.rs:10353`, `tui.rs:10368`, `controls.rs:94`
* **INV-CMT-05** Enter or `Ctrl+S` submits. Esc cancels ("comment draft canceled"). Empty or
  whitespace text is discarded. Paste inserts text. `tui.rs:2695`, `tui.rs:10435`, `tui.rs:2678`
  * **[V1 defect]** `shift+enter` (`tui.control.newline`) is intercepted by the Enter-submits
    branch, so a newline cannot be typed in the composer. `tui.rs:2722`, `manifest:1025`
* **INV-CMT-06** The composer integrates with host focused-text-field completions and history.
  Completions are only accepted while the composer is visible and focused. `tui.rs:5915`,
  `tui.rs:5929`, `tui.rs:5946`, tests `tui.rs:13864`–`tui.rs:14049`
  * (V2: shared prose editor, §6.2.)
* **INV-CMT-07** "Publish" from the composer saves the draft and opens the publish checklist,
  skipping the readiness warning. `tui.rs:10460`
* **INV-CMT-08** Draft save is optimistic: a local id is assigned, saves are queued and
  serialized, completions are reconciled by request identity (never by body or position), and
  late or mismatched completions never overwrite or resurrect. `tui.rs:10484`, `tui.rs:10524`,
  `tui.rs:788`, tests `tui.rs:13398`, `tui.rs:13456`
* **INV-CMT-09** `e` (or the Edit button or inline action) edits the latest draft on the
  selection, or the selected comment. It refuses unpersisted drafts ("try again after save").
  Saving sends an update; on failure the previous body is restored. `tui.rs:10383`,
  `tui.rs:11262`, test `tui.rs:15396`
* **INV-CMT-10** `D` (or the Delete button or inline action) deletes the latest draft on the
  selection, or the selected comment. Delete is optimistic and restored on failure.
  `tui.rs:12413`, `tui.rs:11268`, test `tui.rs:13426`
* **INV-CMT-11** `r` toggles resolved/reopened on the selected thread (also via buttons and inline
  actions). The change is optimistic, persisted with `thread.resolve`, and rolled back on
  failure. `tui.rs:12171`, `tui.rs:12201`, `tui.rs:12459`, `lib.rs:2309`
* **INV-CMT-12** `R` shows or hides resolved threads inline. Hiding them clears a selection that
  was on a resolved thread. `tui.rs:12151`, test `tui.rs:14486`
* **INV-CMT-13** Threads render inline under their anchor row as:
  * a header (count, collapsed/resolved state);
  * markdown comment rows;
  * suggestion rows;
  * an agent block;
  * action rows.
  * Clicking or activating the header collapses it. `U` expands all; `Z` collapses all.
  * `view.rs:199`, `view.rs:365`, `render.rs:1410`, `render.rs:1601`, `tui.rs:12058`,
    `tui.rs:12031`, `tui.rs:12042`
* **INV-CMT-14** Per-thread inline actions, in visual order, chosen by state: Reply, Edit, Delete
  draft, Delete AI exchange, Ask Bcode or (Follow-up + Open session [+ Retry stream]), Expand
  answer, Copy answer, Suggest comment, Draft answer, Accept / Refine / Reject suggestion,
  Publish, Resolve or Reopen. Each shows its shortcut label. `view.rs:907`, `view.rs:950`,
  `view.rs:1018`, `render.rs:1664`
* **INV-CMT-15** Thread kinds (note, question, todo, finding, summary) and severities (info, nit,
  warning, blocker) are persisted and displayed in the sidebar and reports. New drafts are always
  note/info. Threads with an AI exchange display as question. There is no V1 UI to change kind or
  severity. `models:1018`, `models:1035`, `tui.rs:3406`, `tui.rs:10506`, `tui.rs:8537`,
  `lib.rs:4298`
* **INV-CMT-16** The gutter shows a draft marker (`💬N`, or `🤖💬N` when AI-linked), a `✎`
  comment affordance on the selected line, and range markers. `tui.rs:10220`, `tui.rs:10186`,
  `render.rs:1719`, `render.rs:1969`
  * (replaced: §6.2 hover `+` affordance and §6.3 cursor-versus-range styling.)
* **INV-CMT-17** Drafts and threads persist per review key: the workspace scope, falling back to
  the legacy target key for drafts created before workspaces existed. `lib.rs:2095`,
  `lib.rs:1690`, `tui.rs:1002`
* **INV-CMT-18** Persisted drafts are reloaded into local state on open, with anchors re-resolved
  against current files. `tui.rs:12698`, `tui.rs:12722`, test `tui.rs:15446`
  * (V2: §5.3 outdated anchors are labeled, never silently moved.)

### A.8 AI questions, agent threads, and OpenSession

* **INV-AI-01** `a` (or the Ask button or inline action) opens the composer in Ask-Bcode mode on a
  line, range, file, or review anchor. Submitting asks Bcode. `tui.rs:2905`, `tui.rs:11994`,
  `tui.rs:11998`, test `tui.rs:15282`
* **INV-AI-02** Structured AI commands on the selection, each with a fixed instruction: `1`
  explain change, `2` find risk, `3` suggest comment, `4` check tests, `5` summarize file.
  `tui.rs:4379`, `tui.rs:4394`, `tui.rs:11953`
* **INV-AI-03** The first question on a thread creates a session in the repository directory
  titled "Review: <path>", sends the prompt, and links the session to the thread
  (`thread.link_session`). Later questions send follow-ups to the linked session. `tui.rs:1038`,
  `tui.rs:2284`, `tui.rs:2305`, `lib.rs:2283`
* **INV-AI-04** The prompt is bounded and contains: review, repo, sources, file, diff and old/new
  ranges, line kind, other threads (up to 12 × 160 chars), the current draft, selected lines (up
  to 80), nearby hunk context (up to 120), and pointers to the `review.context.get` /
  `review.comments.list` / `review.thread.get` / `review.diff.get` service operations.
  `tui.rs:12257`, `tui.rs:12302`, `tui.rs:95`
* **INV-AI-05** AI exchanges persist as a schema-versioned record (question, anchor, session id,
  status pending/linked/failed, error). Pending and failed exchanges are restored on reopen. An
  unverified save never re-creates a session. `models:762`, `models:773`, `tui.rs:10665`,
  `tui.rs:2055`, `tui.rs:2048`, `tui.rs:12613`, tests `tui.rs:15151`, `tui.rs:15191`,
  `tui.rs:15203`
* **INV-AI-06** The inline agent block shows live state (waiting / creating session / working /
  writing answer / answered / failed), status, context summary with prompt size, activity, stream
  warnings, bounded session items (tool calls, results), and an answer preview. `tui.rs:4443`,
  `tui.rs:4472`, `tui.rs:4518`, `tui.rs:4865`, `tui.rs:4967`, `view.rs:826`, `render.rs:1646`
* **INV-AI-07** Live answers stream through `PluginTuiHost::subscribe_session_view` (bounded
  snapshots, reconnect, resync) for every linked session. `tui.rs:4552`, `tui.rs:4559`,
  `tui.rs:4603`, `ext-doc` "Plugin semantic session observation"
* **INV-AI-08** `y` (or the Retry inline action) retries a disconnected linked-session stream.
  `tui.rs:11029`
* **INV-AI-09** Expand answer toggles between compact preview and full answer. The expanded set
  persists in presentation state. `tui.rs:11060`, `view.rs:1765`
  * **[unreachable by key]** `A` is shadowed by add-source and not declared for this action; it
    works only via the inline action.
* **INV-AI-10** `d` (or the inline action) deletes the latest AI exchange at the selection,
  persisted with retry. `tui.rs:10726`, `tui.rs:2021`
* **INV-AI-11** `o` (or the Session button or inline action) opens the linked session through
  `PluginTuiAction::OpenSession`. The host suspends the review surface; on return the status reads
  "returned from linked Bcode session" and the exact position is retained in memory. Failure
  shows "could not open linked session". `tui.rs:11924`, `tui.rs:580`, `tui.rs:505`,
  `tui.rs:11946`
* **INV-AI-12** Draft answer converts the latest answer into a draft comment, deduplicated,
  keeping the session link. `tui.rs:11085`
  * **[unreachable by key]** `m` is shadowed by the mode toggle; works only via the inline
    action.

### A.9 Suggestions

* **INV-SUG-01** `s` (or the inline action) turns the latest Bcode answer into a suggested comment
  with status `suggested` and rationale "from linked Bcode answer". Duplicate pending suggestions
  are rejected. `tui.rs:10985`
* **INV-SUG-02** `;` (or the inline action) accepts the pending suggestion: it becomes a draft
  (keeping the session link) and is marked `accepted`. `tui.rs:10891`
* **INV-SUG-03** Refine marks the suggestion `refining` and asks Bcode to refine it in the linked
  session. `tui.rs:10913`
  * **[unreachable by key]** `f` is shadowed by the file picker; works only via the inline
    action.
* **INV-SUG-04** Reject marks the suggestion `rejected`. `tui.rs:10935`
  * **[unreachable by key]** `:` is shadowed by jump-to-line; works only via the inline action.
* **INV-SUG-05** Suggestions persist (suggested/refining/accepted/rejected) and reload on open.
  Inline suggestion rows and sidebar suggestion counts are shown. `models:839`, `models:852`,
  `tui.rs:2087`, `tui.rs:12666`, `view.rs:199`, `render.rs:1621`

### A.10 Publish

* **INV-PUB-01** `x` publishes; when an inline action is selected, `x` activates that action
  instead. The first press on an incomplete review shows "review incomplete: …; press x again to
  publish anyway". `tui.rs:2897`, `tui.rs:11526`, `tui.rs:11413`
* **INV-PUB-02** The checklist modal shows:
  * readiness line;
  * files viewed x/N;
  * unviewed count with the first 3 paths (`W` jumps there);
  * open threads with the first 3 (`P` jumps there);
  * resolved count;
  * draft count;
  * `!` for the attention sidebar;
  * a per-thread include/exclude list, where Space toggles the selected thread.
  * `tui.rs:11758`, `tui.rs:11466`, `tui.rs:11488`, `tui.rs:2424`, `render.rs:2352`
* **INV-PUB-03** The publisher picker lists built-in and external publishers with capability
  labels. The default selection is `markdown_file`. `tui.rs:11540`, `tui.rs:94`, `tui.rs:3068`,
  `render.rs:2411`
* **INV-PUB-04** Built-in publishers: `local_markdown` ("Local review markdown" notebook),
  `markdown_file`, and `json_file` (pretty bundle). Each has an `output_path` option, defaulting
  to `<repo>/.bcode/reviews/<review_id>.<ext>`. `lib.rs:1056`, `lib.rs:1093`, `lib.rs:1130`,
  `lib.rs:1426`, `lib.rs:1446`
* **INV-PUB-05** External publishers are discovered over `bcode.review_publisher/v1` and routed by
  plugin. The bundle excludes deselected threads. `tui.rs:1689`, `tui.rs:1723`, `tui.rs:1753`,
  `models:1301`
* **INV-PUB-06** The options form is generated from the publisher's JSON schema: label (marked
  required), default, enum choices. ←/→/Space cycle choices; text is typed or pasted; Tab /
  Shift+Tab move between fields. `tui.rs:1577`, `tui.rs:1617`, `tui.rs:1640`, `tui.rs:1652`,
  `tui.rs:1528`, `tui.rs:1560`, `render.rs:2467`
  * **[V1 defect]** `j`/`k` are bound to up/down in the options scopes, so they cannot be typed
    into values. `manifest:1512`, `manifest:1517`
* **INV-PUB-07** Preview modal: provider preview text with the thread-selection summary,
  scrollable with j/k. Enter goes to submit confirmation; `p` goes back from confirmation to
  preview; Esc closes. A preview failure keeps the options; a submit failure returns to preview
  for a safe retry. `tui.rs:11560`, `tui.rs:11829`, `tui.rs:11806`, `tui.rs:11572`,
  `tui.rs:11591`, `render.rs:2534`
* **INV-PUB-08** A successful submit marks included threads as published (thread token
  `published`), resets the readiness acknowledgement, and shows the provider message.
  `tui.rs:11612`, `tui.rs:11446`
* **INV-PUB-09** Publish records (publisher, submitted, published thread anchors, output, message,
  time) are saved for built-in and external publishes, reloaded on open into published markers,
  and surfaced on home as last publish. `lib.rs:1338`, `lib.rs:1375`, `tui.rs:1493`,
  `tui.rs:12583`, `models:462`
* **INV-PUB-10** GitHub publisher (`github_pr_review`):
  * options `repository`, `pull_request`, `token_env`, `submit_event`
    COMMENT/REQUEST_CHANGES/APPROVE, `summary`, `fallback_file_comments_to_summary` (default
    true), `fallback_unmapped_to_summary` (default false);
  * repository from `gh repo view`, then the origin remote;
  * PR from `gh pr view`, then branch patterns;
  * auth from the token env, `GITHUB_TOKEN`, `GH_TOKEN`, then `gh auth`.
  * `gh.rs:118`, `gh.rs:580`–`gh.rs:663`
* **INV-PUB-11** GitHub mapping:
  * resolved threads are skipped (with a warning);
  * file-level and expanded-context threads go to the summary;
  * line and range threads become inline comments with side and start_line;
  * unmappable threads either fail the submit or are appended to the summary when the fallback is
    set;
  * the body joins comments with `---` and appends surface, source, and session footers.
  * `gh.rs:303`, `gh.rs:342`, `gh.rs:366`, `gh.rs:384`, `gh.rs:446`, `gh.rs:231`
* **INV-PUB-12** The GitHub preview lists repository and PR with their resolution source, auth
  source, event, counts, summary, repository comments, warnings, and inline comments.
  `gh.rs:170`

### A.11 External import

* **INV-IMP-01** `X` discovers importers over `bcode.review_importer/v1` and shows a picker (j/k,
  Enter, Esc). `tui.rs:8835`, `tui.rs:1149`, `tui.rs:8679`, `tui.rs:2380`, `render.rs:2209`
* **INV-IMP-02** Importer options use the same schema-driven form as publish options. Enter
  imports. `tui.rs:8745`, `tui.rs:8788`, `tui.rs:8798`, `tui.rs:8808`
* **INV-IMP-03** An import validates the importer id, then atomically saves a snapshot that
  replaces the previous one from the same importer. The status reads "imported N external review
  thread(s)". Snapshots reload on open. `tui.rs:1089`, `tui.rs:1181`, `tui.rs:1219`,
  `lib.rs:2800`, test `lib.rs:4904`
* **INV-IMP-04** External threads appear in the sidebar (read-only) with provider and mapping
  status (mapped / stale / unmappable) and resolved state. They are not shown inline in the
  diff. `tui.rs:8641`, `render.rs:883`
* **INV-IMP-05** The GitHub importer is read-only: it groups replies into threads, maps anchors
  (Exact / Approximate / Unmappable, where GitHub "outdated" lowers to Approximate), sets state
  Open / Resolved / Outdated, and carries author and URL identities. `gh.rs:61`, `gh.rs:88`,
  `gh.rs:909`, `gh.rs:993`, `gh.rs:1031`

### A.12 Report generation

* **INV-RPT-01** The `local_markdown` publisher preview is the local review report. It contains:
  header (review id, repo, generated time, file count, open/resolved threads, comment count), a
  files list with status and +/−, review-level threads, then file threads. Each thread shows id,
  linked session, resolved time, comments, and diff context. `lib.rs:1471`, `lib.rs:1542`, test
  `lib.rs:5560`
* **INV-RPT-02** The `markdown_file` export (title, review id, repo, generated, surfaces, threads)
  and the `json_file` bundle export. `lib.rs:1578`, `lib.rs:1130`
* **INV-RPT-03** In-TUI `local_review_report_markdown` (viewed counts, unviewed files, threads
  with kind/severity and session) is only exercised by tests; it has no V1 UI entry.
  `tui.rs:11337`
  * Decide whether V2 exposes it, e.g. via the command palette.
* **INV-RPT-04** `review.bundle.get` returns a provider-neutral bundle (files, surfaces, threads
  with selected and diff lines) used by all publishers and by the CLI. `lib.rs:938`,
  `models:1236`, `models:1258`

### A.13 Viewed files

* **INV-VIEW-01** `w` toggles the selected file as viewed. The mark persists on the workspace
  immediately. `tui.rs:9404`
  * (replaced: §5.4 patch-id keyed marks with migration; §6.2 `v` marks viewed and advances.)
* **INV-VIEW-02** `V` marks all files viewed; `E` marks all unviewed. `tui.rs:9640`,
  `tui.rs:9625`
* **INV-VIEW-03** Viewed counts appear in the header, summary, checklist, readiness label, home
  rows, and details. `tui.rs:9378`, `tui.rs:11394`
  * (replaced: §6.1 progress bar.)
* **INV-VIEW-04** With hide-viewed on, marking a file viewed advances to the next unviewed file.
  `tui.rs:9419`

### A.14 Presentation-state restore

* **INV-PRES-01** Saved per workspace (schema-versioned): selected path, selected thread key,
  selected line, sidebar mode and visibility, thread filter, show-resolved, hide-viewed,
  collapsed thread keys, expanded agent-answer keys. Saved automatically when changed.
  `models:112`, `tui.rs:7883`, `tui.rs:726`
  * (V2 adds item-identity scroll anchor, layout mode, panel sizes, and expansion sets per §9.)
* **INV-PRES-02** Restore on reopen is best-effort. An unknown schema version shows a status and
  falls back. A stale path falls back to the first file. The line is matched by old or new
  number, falling back to row 0. `tui.rs:7914`, `ext-doc` "Workspaces also persist…"
* **INV-PRES-03** The exact return position after OpenSession is kept in memory, not persisted.
  `ext-doc`, `tui.rs:505`

### A.15 Search and filters

* **INV-SRCH-01** `f` or `Ctrl+P` opens a fuzzy file picker over review files; Enter opens the
  match. `tui.rs:6204`, `tui.rs:7023`, `tui.rs:6849`, `render.rs:3150`
  * (conflict: §6.2 `ctrl-p` is the command palette.)
* **INV-SRCH-02** `/` searches the currently loaded file only (repository-file content). `n` finds
  the next match and `N` the previous; "no active search; press / first" shows otherwise.
  `tui.rs:6303`, `tui.rs:6877`, `tui.rs:6913`, `tui.rs:6918`, `tui.rs:3646`
  * (replaced: §6.2 search within the whole review.)
* **INV-SRCH-03** `T` cycles the thread filter All → Open → Resolved. Clickable filter buttons set
  it directly. `tui.rs:8937`, `tui.rs:7483`
* **INV-SRCH-04** Home search (`/`): typing filters by title, id, sources, health, and "needs
  attention". Enter opens; Esc clears. `home.rs:664`, `home.rs:685`, `home.rs:1947`,
  `home.rs:1969`, `home.rs:1990`
  * (replaced: §7.2 type-to-filter.)
* **INV-SRCH-05** Home toggles: `D` drafts-only filter, `A` attention-only filter. `home.rs:642`,
  `home.rs:653`
* **INV-SRCH-06** **[V1 defect]** In prompts, `j`, `k`, and `Ctrl+P`/`Ctrl+N` are bound to
  selection up/down, so `j` and `k` cannot be typed into picker queries. `manifest:671`,
  `manifest:676`, `tui.rs:2646`

### A.16 Keymap actions

All keys are declared in the manifest with defaults. The host keymap remaps or unbinds them.
Shown hints follow the effective bindings. Undeclared keys are dropped: in review scopes they
become paste, which is ignored outside text fields. `keys.rs:48`, `keys.rs:86`, `keys.rs:140`,
`tui.rs:2486`, tests `tui.rs:13485`, `tui.rs:15049`

**Shared `tui.control.*` set.** These are declared in every scope; the manifest line ranges per
scope are given below.

* left←, right→, home, end, page_up, page_down, submit Enter, cancel Esc, focus_next Tab,
  focus_previous Shift+Tab, toggle Space, backspace, delete.
* word_left ctrl/alt+←, alt+b; word_right ctrl/alt+→, alt+f; delete_word_backward ctrl+w,
  ctrl/alt+backspace; delete_word_forward ctrl/alt+delete.
* line_start ctrl+a; line_end ctrl+e; delete_to_start ctrl+u; delete_to_end ctrl+k.
* select_left/right/up/down shift+arrows; select_word_left/right ctrl/alt+shift+←/→.

**Per-scope variations.**

* Comment, prompt, and options scopes add up/down with ctrl+p/ctrl+n (prompt and options also add
  j/k), left/right with ctrl+b/ctrl+f, and line_start/line_end with super+←/→.
* The comment scope adds `newline` shift+enter.
* Prompt, comment, and option scopes add `bcode.code_review.text.select_start` shift+home and
  `text.select_end` shift+end (`manifest:1622`–`manifest:1643`).

**Scope `code-review-home`** (`manifest:7`–`manifest:189`; selected in `home.rs:272`)

| ID | Action | Keys | Behavior | Line |
| --- | --- | --- | --- | --- |
| INV-KEY-H01 | search | `/` | toggle home search | 7 |
| INV-KEY-H02 | rename | `r` | rename workspace | 10 |
| INV-KEY-H03 | down / up | `j`,↓ / `k`,↑ | move selection | 13 / 17 |
| INV-KEY-H04 | details | `d` | toggle details panel | 21 |
| INV-KEY-H05 | draft_filter | `D` | drafts-only filter | 24 |
| INV-KEY-H06 | attention_filter | `A` | attention-only filter | 27 |
| INV-KEY-H07 | help | `?` | help overlay | 30 |
| INV-KEY-H08 | open | Enter | open selected | 33 |
| INV-KEY-H09 | recent | `c` | continue most recent | 36 |
| INV-KEY-H10 | review | `o` | open in review mode | 39 |
| INV-KEY-H11 | build | `b` | open in Build mode | 42 |
| INV-KEY-H12 | archive | `x` | archive | 45 |
| INV-KEY-H13 | new | `n`,`e` | custom review (title → Build) | 48 |
| INV-KEY-H14 | unstaged / staged / working / last_commit / repository | `u` / `s` / `w` / `l` / `v` | create preset review | 52–64 |
| INV-KEY-H15 | setup_next / setup_previous | `S` / `U` | cycle setup reviews | 67 / 70 |
| INV-KEY-H16 | draft_next / draft_previous | `F` / `B` | cycle draft reviews | 73 / 76 |
| INV-KEY-H17 | published_next / published_previous | `p` / `P` | cycle published reviews | 79 / 82 |
| INV-KEY-H18 | attention_next / attention_previous | `M` / `N` | cycle attention reviews | 85 / 88 |
| INV-KEY-H19 | archived | `a` | toggle archived listing | 91 |
| INV-KEY-H20 | first / last | `g` / `G` | first / last review | 94 / 97 |
| INV-KEY-H21 | restore | `R` | restore archived | 100 |
| INV-KEY-H22 | exit | ctrl+d | exit home | 103 |

Scopes `code-review-home-search` (`manifest:1099`), `-home-new` (`manifest:1198`), and
`-home-rename` (`manifest:1297`) hold only `tui.control.*` plus exit ctrl+d (INV-KEY-H23).
`tui.control.cancel` Esc closes help or the active text prompt; `tui.control.focus_next` Tab
swaps preset/workspace focus (INV-KEY-H24).

**Scope `code-review`** (review mode; `manifest:193`–`manifest:468`; dispatch `tui.rs:2795`,
`tui.rs:2864`)

| ID | Action | Keys | Behavior | Line |
| --- | --- | --- | --- | --- |
| INV-KEY-R01 | sidebar | `b` | show or hide sidebar | 193 |
| INV-KEY-R02 | build_mode | `B` | enter Build mode | 196 |
| INV-KEY-R03 | mode | `m` | toggle review/build | 199 |
| INV-KEY-R04 | sidebar_mode | `t` | cycle sidebar mode | 202 |
| INV-KEY-R05 | file_picker | `f`, ctrl+p | fuzzy file picker | 205 |
| INV-KEY-R06 | add_file | `+` | add file source (Build only; no-op in review) | 209 |
| INV-KEY-R07 | add_source | `A` | add-source menu (Build only; review shows a hint) | 212 |
| INV-KEY-R08 | down / up | `j`,↓ / `k`,↑ | move selection (sidebar-aware) | 215 / 219 |
| INV-KEY-R09 | first / last | `g` / `G` | top / bottom | 223 / 226 |
| INV-KEY-R10 | help | `?` | toggle help overlay | 229 |
| INV-KEY-R11 | attention_next | `M` | next attention item | 232 |
| INV-KEY-R12 | jump | `:` | jump to row | 235 |
| INV-KEY-R13 | search | `/` | search current file | 238 |
| INV-KEY-R14 | search_previous | `N` | previous match | 241 |
| INV-KEY-R15 | next_file | `n` | next file, or next match | 244 |
| INV-KEY-R16 | previous_file | `p` | previous file | 247 |
| INV-KEY-R17 | draft_next / draft_previous | `}` / `{` | inline draft navigation | 250 / 271 |
| INV-KEY-R18 | hunk_next / hunk_previous | `J` / `K` | hunk navigation | 253 / 256 |
| INV-KEY-R19 | file_comment | `F` | file-level note | 259 |
| INV-KEY-R20 | delete_answer | `d` | delete latest AI exchange | 262 |
| INV-KEY-R21 | import | `X` | import external review | 265 |
| INV-KEY-R22 | copy_answer | `Y` | copy Bcode answer to clipboard | 268 |
| INV-KEY-R23 | thread_next / thread_previous | `]` / `[` | inline thread navigation | 274 / 277 |
| INV-KEY-R24 | attention | `!` | attention sidebar | 280 |
| INV-KEY-R25 | thread_filter | `T` | cycle thread filter | 283 |
| INV-KEY-R26 | unview_all / view_all | `E` / `V` | mark all unviewed / viewed | 286 / 289 |
| INV-KEY-R27 | unviewed_previous / unviewed_next | `I` / `W` | unviewed file navigation | 292 / 295 |
| INV-KEY-R28 | viewed | `w` | toggle viewed | 298 |
| INV-KEY-R29 | open_thread_next / open_thread_previous | `u` / `i` | open threads in file | 301 / 304 |
| INV-KEY-R30 | thread_previous_global / thread_next_global | `O` / `P` | open threads, global | 307 / 310 |
| INV-KEY-R31 | linked_next | `L` | next AI-linked thread | 313 |
| INV-KEY-R32 | resolved_visibility | `R` | show or hide resolved | 316 |
| INV-KEY-R33 | resolve | `r` | toggle resolved | 319 |
| INV-KEY-R34 | hide_viewed | `H` | hide viewed files | 322 |
| INV-KEY-R35 | expand_threads / collapse_threads | `U` / `Z` | expand / collapse all threads | 325 / 328 |
| INV-KEY-R36 | delete | `D` | delete draft | 331 |
| INV-KEY-R37 | edit | `e` | edit draft | 334 |
| INV-KEY-R38 | range | `v` | toggle range selection | 337 |
| INV-KEY-R39 | comment | `c` | line or range comment, or reply | 340 |
| INV-KEY-R40 | review_comment | `C` | review-level note | 343 |
| INV-KEY-R41 | publish | `x` | inline action, or publish | 346 |
| INV-KEY-R42 | ask | `a` | Ask Bcode composer | 349 |
| INV-KEY-R43 | suggest | `s` | suggestion from answer | 352 |
| INV-KEY-R44 | accept | `;` | accept suggestion | 355 |
| INV-KEY-R45 | retry | `y` | retry linked stream | 358 |
| INV-KEY-R46 | open_linked | `o` | open linked session (OpenSession) | 361 |
| INV-KEY-R47 | explain / risk / suggest_ai / tests / summarize | `1` / `2` / `3` / `4` / `5` | structured AI commands | 364–376 |
| INV-KEY-R48 | exit | ctrl+d | exit review (back to home) | 379 |

Additional rows for the `code-review` scope:

* **INV-KEY-R49** `tui.control.*` in this scope (`manifest:382`–`manifest:468`):
  * Enter activates (INV-NAV-16); Esc clears (INV-NAV-15).
  * Tab / Shift+Tab focus the controls (INV-TREE-15).
  * ← / → collapse / expand tree rows.
  * Home, End, PageUp, and PageDown have no handler (INV-NAV-21).
* **INV-KEY-R50** **[unreachable]** Bindings that exist in code but are shadowed or undeclared,
  reachable only via inline action rows: refine `f`, reject `:`, draft answer `m`, toggle answer
  `A`, previous AI-linked `B`. `tui.rs:2906`–`tui.rs:2912`, `tui.rs:2882`
  * The inline actions still display these misleading shortcut labels. `view.rs:1018`

**Scope `code-review-build`** (`manifest:472`–`manifest:661`; dispatch `tui.rs:2746`)

| ID | Action | Keys | Behavior | Line |
| --- | --- | --- | --- | --- |
| INV-KEY-B01 | sidebar / build_mode / mode / sidebar_mode / file_picker / help / exit | as in review | same as review | 472–509, 575 |
| INV-KEY-B02 | add_file | `+` | file source picker | 488 |
| INV-KEY-B03 | add_source | `A`,`a` | add-source menu | 491 |
| INV-KEY-B04 | down / up / first / last | `j`/↓, `k`/↑, `g`, `G` | build row navigation | 495–506 |
| INV-KEY-B05 | refresh | `R` | rematerialize | 512 |
| INV-KEY-B06 | unstaged / staged / working / last_commit | `u` / `s` / `w` / `l` | quick-add source | 515–524 |
| INV-KEY-B07 | include_all / exclude_all / invert | `I` / `E` / `V` | inclusion | 527–533 |
| INV-KEY-B08 | empty_next / empty_previous / exclude_empty | `n` / `N` / `z` | empty sources | 536–542 |
| INV-KEY-B09 | diagnostic_next / exclude_errors | `d` / `Z` | diagnostics | 545 / 548 |
| INV-KEY-B10 | edit_source | `C` | edit source spec | 551 |
| INV-KEY-B11 | merge_base | `M` | toggle merge base | 554 |
| INV-KEY-B12 | remove_excluded / remove_duplicates | `X` / `P` | cleanup | 557 / 560 |
| INV-KEY-B13 | include | Space | toggle inclusion | 563 |
| INV-KEY-B14 | rename_workspace | `T` | rename workspace | 566 |
| INV-KEY-B15 | source_surface / source_for_surface | `O` / `Y` | source↔surface jump | 569 / 572 |

All of `code-review-build` is (replaced: wizard plus source editing; see INV-SRC-01).

**Overlay scopes.** The active scope is chosen by `keys.rs:65`.

| ID | Scope / action | Keys | Behavior | Line |
| --- | --- | --- | --- | --- |
| INV-KEY-P01 | `code-review-prompt` save | ctrl+s | declared "Submit prompt"; no prompt handler (Enter submits) | 665 |
| INV-KEY-P02 | `code-review-prompt` exit + controls | ctrl+d, Enter / Esc / ↑↓ (j/k, ctrl+p/n) | submit / cancel / move picker selection | 668–772 |
| INV-KEY-C01 | `code-review-comment` save | ctrl+s | save comment | 986 |
| INV-KEY-C02 | `code-review-comment` preview | ctrl+p | toggle markdown preview | 989 |
| INV-KEY-C03 | `code-review-comment` exit + controls | ctrl+d; Enter submit; Tab/→ next action; ← previous action; Esc cancel | composer | 992–1098 |
| INV-KEY-I01 | `code-review-import` down / up / exit | `j`/↓, `k`/↑, ctrl+d (Enter confirm, Esc close) | importer picker | 775–872 |
| INV-KEY-I02 | `code-review-import-options` | Tab / Shift+Tab fields; ←/→/Space cycle; typing edits; Enter import | importer options | 1396–1637 |
| INV-KEY-U01 | `code-review-publish` attention | `!` | close checklist, open attention sidebar | 876 |
| INV-KEY-U02 | `code-review-publish` unviewed_next | `W` | close checklist, next unviewed file | 879 |
| INV-KEY-U03 | `code-review-publish` thread_next_global | `P` | close checklist, next open thread | 882 |
| INV-KEY-U04 | `code-review-publish` down / up | `j`/↓, `k`/↑ | picker selection / preview scroll | 885 / 889 |
| INV-KEY-U05 | `code-review-publish` publish_back | `p` | confirmation → preview | 1619 |
| INV-KEY-U06 | `code-review-publish` controls | Space toggles thread inclusion (checklist); Enter advances; Esc closes; ctrl+d exits | publish flow | 896–985 |
| INV-KEY-U07 | `code-review-publish-options` | `!`, `W`, `P` declared (`manifest:1503`–`manifest:1509`); Tab / Shift+Tab; ←/→/Space cycle; Enter preview; Esc close | publisher options | 1503–1643 |

**Keymap conflicts V2 must resolve against §6.2.**

* **INV-KEY-X01** `v`: V1 range selection versus V2 mark-viewed-and-advance.
* **INV-KEY-X02** `:`: V1 jump-to-line versus V2 command palette.
* **INV-KEY-X03** `ctrl+p`: V1 file picker (review) and markdown preview (composer) versus V2
  command palette.
* **INV-KEY-X04** `w`: V1 toggle viewed; its role under V2 is undecided.

### A.17 Mouse behaviors

* **INV-MOUSE-01** Wheel over the sidebar moves the thread selection by 3 (threads and attention
  modes) or scrolls the file list or tree by 3. Wheel over the diff scrolls 3 rows.
  `tui.rs:2940`
  * (replaced: §6.2 `ScrollMotion`.)
* **INV-MOUSE-02** Click on a header tab selects the sidebar mode. Header buttons "File note" and
  "+Review note" open the composer. `controls.rs:558`, `controls.rs:69`
* **INV-MOUSE-03** Click on a thread detail button runs it: Reply / Resolve-reopen / Edit /
  Delete / Session / Ask. Filter buttons All / Open / Resolved / +Review note. `controls.rs:73`,
  `controls.rs:81`, `tui.rs:7402`
* **INV-MOUSE-04** Click on a composer action button: Save draft / Ask Bcode / Publish / Cancel,
  or Save edit / Cancel. `controls.rs:94`, `render.rs:2753`
* **INV-MOUSE-05** Click on a sidebar thread row selects it and jumps to its anchor.
  `render.rs:829`, `tui.rs:7417`
* **INV-MOUSE-06** Click on a repo-tree row opens a file or toggles a directory. `tui.rs:7313`
* **INV-MOUSE-07** Click on a diff source line selects it and starts a range. Dragging extends the
  range (status shows the label). Release finishes; a click without drag clears the range. The
  diff captures the gesture until release, independent of the pane under the pointer, and the
  capture is cancelled on resize or focus loss. `tui.rs:9855`, `tui.rs:9946`, `tui.rs:9965`,
  `tui.rs:9979`, `tui.rs:2591`, tests `tui.rs:13569`, `tui.rs:13649`
  * (V2: drag-to-comment, §6.2.)
* **INV-MOUSE-08** Click on a hidden-context row expands it. Click on a thread header toggles
  collapse. Click on an inline action row runs it. Click on a hunk header shows "select a diff
  line to comment". Click on an expanded context line shows "not commentable". `tui.rs:9855`,
  tests `tui.rs:14342`, `tui.rs:14388`
* **INV-MOUSE-09** Home: click a preset to create a review; click a workspace to open it; the
  wheel scrolls lists; the help list scrolls; releasing outside clears the pressed state;
  selection resolves against the painted identity. `home.rs:1041`
* **INV-MOUSE-10** Hit regions are cleared each frame and confined to the painted area. Modal
  overlays isolate background controls. `tui.rs:7352`, `tui.rs:7375`, `controls.rs:168`, test
  `controls.rs:747`
* **INV-MOUSE-11** **[unreachable]** Defined but never painted: the `ThreadStatus` button group
  (Drafts / All / Open / Resolved-hidden / Cycle filter) and the mouse actions `SelectFile`,
  `ToggleFileViewed`, `ActivateTreeRow`, `JumpSelectedThread`, `Publish`. `controls.rs:87`,
  `tui.rs:3005`

### A.18 Clipboard

* **INV-CLIP-01** Copy Bcode answer (`Y` or the inline action) writes the trimmed latest answer
  through `PluginTuiHost::copy_text`. The status reads "copied Bcode answer to clipboard" or
  "failed to copy Bcode answer: …". `tui.rs:10953`, `tui.rs:10973`, `tui.rs:571`
* **INV-CLIP-02** Bracketed paste inserts text into the composer, prompts, and option fields.
  `tui.rs:2678`, `tui.rs:2570`, `tui.rs:2415`, `tui.rs:2371`
  * (V2 adds selection and copy integration per §8.1.)

### A.19 Help

* **INV-HELP-01** Review `?` toggles a centered overlay (at most 68×18) listing effective
  `keys description` hints for the active scope. It is truncated and not scrollable.
  `tui.rs:2920`, `render.rs:2096`, `tui.rs:439`
  * (replaced: §6.2 help generated from the keymap, which must be complete and scrollable.)
* **INV-HELP-02** Home `?` shows a scrollable help list: explanatory lines plus effective hints,
  with focus, cancel, and next controls. Esc closes it. `home.rs:2161`, `home.rs:206`,
  `home.rs:1126`
* **INV-HELP-03** Overlay titles (checklist, publisher, options, preview, import) embed effective
  scope hints. `render.rs:2209`, `render.rs:2292`
* **INV-HELP-04** Status messages give contextual guidance on every refused action (for example
  "select a diff line or range to comment", "switch to build mode with m to add sources"). Spread
  throughout `tui.rs` (for example `tui.rs:10320`, `tui.rs:6134`)
  * (replaced: §6.1 key hints plus toasts.)

### A.20 Display and visual (cross-cutting)

* **INV-DISP-01** Unified diff with both old and new line-number gutters and `+`/`-`/space
  markers, syntax highlighted from the presentation theme's syntax palette. `display.rs:49`,
  `display.rs:205`, `render.rs:1989`, `tui.rs:105`
  * (replaced: §6.4 whole-blob highlighting off the render path, §6.1
    unified/side-by-side/auto.)
* **INV-DISP-02** Inline comments render as markdown. Text uses BMUX layout with resize anchoring
  to source ranges. `view.rs:714`, `view.rs:462`, test `view.rs:1701`
* **INV-DISP-03** The review theme is derived from the plugin TUI theme (canvas, focused,
  selection, muted, overlay, diff styles). A theme change invalidates the document cache.
  `render.rs:65`, `render.rs:87`, test `tui.rs:13784`
  * (replaced: §6.3 `ReviewTheme` semantic roles.)
* **INV-DISP-04** Empty states: "no files", an empty review, no threads ("no <filter> review
  threads"). `render.rs:1849`, `render.rs:729`

### A.21 Service contracts that must keep working (shared durable data)

* **INV-DATA-01** `bcode.code_review/v1` operations that V2 must continue to serve and read
  (§1.2):
  * `create_review`, `draft.list/save/delete/update`, `thread.link_session`, `thread.resolve`;
  * `review.context.get`, `review.comments.list`, `review.thread.get`, `review.diff.get`,
    `review.bundle.get`;
  * `review.repo.file.get`;
  * `review.workspace.list/create/get/update/archive/materialize` (materialize is V1-only until
    deletion, §4.2);
  * `review.publishers.list`, `review.publish.preview/submit`, `review.publish.record.save`,
    `review.publish.records.list`;
  * `review.suggestions.list`, `review.suggestion.save`;
  * `review.ai_exchanges.list`, `review.ai_exchange.save/delete`;
  * `review.external_import.save`, `review.external_imports.list`.
  * `lib.rs:174`, `models:13`–`models:70`
* **INV-DATA-02** Provider interfaces `bcode.review_publisher/v1` (manifest / preview / submit)
  and `bcode.review_importer/v1` (manifest / import). `models:62`–`models:70`, `gh-manifest`
* **INV-DATA-03** State location:
  * the `[code_review]` config takes `state_location = user|repo` (default user state dir
    `/code-review`; repo uses `.bcode/code-review`) and `state_dir`;
  * the `BCODE_CODE_REVIEW_STATE_DIR` environment variable overrides both;
  * the database is `code-review.db`, with typed Switchy migrations and lock-retry on open.
  * `lib.rs:119`–`lib.rs:127`, `lib.rs:133`, `lib.rs:3531`, `lib.rs:3541`, `lib.rs:3585`,
    `lib.rs:3818`
* **INV-DATA-04** Declared but not dispatched in V1, so not user-visible:
  `review.assignment.validate`, `review.assignments.materialize`, `review.report.validate`,
  `review.consensus.validate`, and the adversarial review contracts. `lib.rs:101`–`lib.rs:109`,
  `models:1589`+
  * Out of scope for V1 parity; V2 must not delete them without their owner's decision.

## Appendix B. V2 parity status

Status of every Appendix A item after phase 6 (scope and surface names as renamed in phase 7). **Done**: V2 has the behavior (where the item
names it, with the same keys or the keys given). **Replaced by X**: the spec supersedes the V1
behavior and X covers its purpose. **Dropped**: V2 deliberately does not reproduce it, with the
reason. V1 defects and unreachable bindings are dropped by the rule of Appendix A. Keys are V2
defaults (every action is in the manifest keymap, the palette, and help; 6.2 "Key bindings").
Behavioral coverage is in `packages/code-review-ui/src/tests/` (`parity.rs` for the phase 6 part 2 items).

### B.1 Entry points and CLI

| ID | Status |
| --- | --- |
| INV-ENT-01 | Done: `bcode review` opens the home route (7.1, 7.5). |
| INV-ENT-02 | Done: `unstaged` opens a transient review (7.5). |
| INV-ENT-03 | Done: `staged`. |
| INV-ENT-04 | Done: `all`. |
| INV-ENT-05 | Done: `last-commit`. |
| INV-ENT-06 | Done: `range BASE HEAD [--two-dot]`; also `compare` and `commit`. |
| INV-ENT-07 | Done: `repo` opens a browse source (5.1). |
| INV-ENT-08 | Done: `publish-github` is a UI-independent CLI command over `review.bundle.get`; unchanged. |
| INV-ENT-09 | Done: unchanged (same command). |
| INV-ENT-10 | Replaced by transient reviews that can be saved (`ctrl+s`) and the new entry points (7.5). |
| INV-ENT-11 | Replaced by internal routes: leaving a review returns to home (7.1). |
| INV-ENT-12 | Replaced by route changes without runtime reloads (7.1, 11.C). |
| INV-ENT-13 | Done: entries that fail show the opening route's error with a way back (7.1). |
| INV-ENT-14 | Replaced by `q`/`esc` to leave (after saves finish; `esc` first clears a selection or search; an unsaved review with annotations offers saving, 7.5). `ctrl+d` is the half-page motion. Leaving is safe: everything is saved and home is one key from the review. |
| INV-ENT-15 | Done: the plugin's `diff.toggle` palette command is unchanged. |
| INV-ENT-16 | Replaced by the single `code-review` surface with routes (7.1). |
| INV-ENT-17 | Done: unchanged disableability features. |

### B.2 Home

| ID | Status |
| --- | --- |
| INV-HOME-01 | Replaced by group headings and per-group counts in the header, and the filter input (7.2). |
| INV-HOME-02 | Replaced by repository-state suggestions and the wizard (7.2, 7.3). |
| INV-HOME-03 | Replaced by the wizard's step 3 (title and sources). |
| INV-HOME-04 | Replaced by the BMUX table (7.2). |
| INV-HOME-05 | Replaced by groups; the label mapping is in 7.2. |
| INV-HOME-06 | Replaced by the *Needs attention* group (7.2). Threads alone no longer mean attention; open threads are a review's own state, shown in the notes column. |
| INV-HOME-07 | Replaced by the preview pane, which explains a group and shows what changed (new commits, drafts since publish). |
| INV-HOME-08 | Replaced by the preview pane (`d`). |
| INV-HOME-09 | Done: both empty states. |
| INV-HOME-10 | Done: `enter` opens; an archived review refuses with a pointer to restore. A review without included sources opens with a diagnostic, and `edit_sources` fixes it. |
| INV-HOME-11 | Done: `c`. |
| INV-HOME-12 | Replaced: Build mode is gone; sources are edited with `edit_sources` (7.3). |
| INV-HOME-13 | Done: `r` rename, empty titles refused, `esc` cancels. |
| INV-HOME-14 | Done: `x` archive. |
| INV-HOME-15 | Done: `shift+r` restore. |
| INV-HOME-16 | Replaced by the archived group, folded by default. |
| INV-HOME-17 | Replaced by groups: reviews of one state are adjacent under one heading, and headings fold. |
| INV-HOME-18 | Done: `g`/`G`, `j`/`k`, arrows, pages. |
| INV-HOME-19 | Done: `tab` moves between suggestions and reviews. |
| INV-HOME-20 | Done: one change at a time, failures as toasts. |
| INV-HOME-21 | Done: selection follows the review identity. |
| INV-HOME-22 | Done: footer hints from the keymap and input prompts. |
| INV-HOME-23 | Replaced by the shared `ReviewTheme` (6.3). |

### B.3 Workspace and sources

| ID | Status |
| --- | --- |
| INV-SRC-01 | Replaced by the wizard and `edit_sources` (7.3), which closes the open question. |
| INV-SRC-02 | Done: every kind resolves (4.1); unknown kinds are preserved (9). |
| INV-SRC-03 | Replaced by the wizard's working-tree step and home suggestions. |
| INV-SRC-04 | Replaced by wizard step 1 (seven source types). |
| INV-SRC-05 | Replaced by the wizard's paged, searchable commit log, and `bcode review commit REV`. |
| INV-SRC-06 | Replaced by commit ranges in the wizard (`shift+tab`), typed revisions in compare, and `range`. |
| INV-SRC-07 | Done: compare branches (base, then head; typed revisions). |
| INV-SRC-08 | Done: wizard file search; a source already chosen is not added twice. |
| INV-SRC-09 | Done: `:START-END` in the file step. |
| INV-SRC-10 | Replaced by removing and re-adding a source in `edit_sources`. |
| INV-SRC-11 | Replaced by `tab` (include or exclude) per source in `edit_sources`; reviews have few sources, so bulk inclusion keys are not kept. |
| INV-SRC-12 | Replaced: sources resolve lazily, an empty source adds no files, and `edit_sources` removes it. |
| INV-SRC-13 | Replaced by the failing-sources toast with each diagnostic, and `edit_sources`. |
| INV-SRC-14 | Replaced by re-adding the source (compare uses merge-base semantics) or `range`/`compare --two-dot`. |
| INV-SRC-15 | Replaced by removal in `edit_sources`. |
| INV-SRC-16 | Done for renaming the review (home `r`, or the title in `edit_sources`); renaming and reordering sources were unreachable in V1 and are dropped. |
| INV-SRC-17 | Replaced by source headers in the document and per-source groups in the file panel. |
| INV-SRC-18 | Done: `r`/`ctrl+r` refresh (token-based, 4.2); saves are automatic. |
| INV-SRC-19 | Replaced by `edit_sources` and the header's source chips. |
| INV-SRC-20 | Done: per-source diagnostics with codes and messages; one failure never fails the others (4.1). |
| INV-SRC-21 | Done: source headers (5.1). |
| INV-SRC-22 | Done: same workspace fields, plus additive V2 state (9). |
| INV-SRC-23 | Done: browse sources read whole files at the working tree, bounded (4.1, 4.2). |

### B.4 Navigation

| ID | Status |
| --- | --- |
| INV-NAV-01 | Replaced by one continuous document (5.1). |
| INV-NAV-02 | Done: `j`/`k`/arrows by item; in a focused panel they move its selection. |
| INV-NAV-03 | Done: `g`/`G`. |
| INV-NAV-04 | Done: `n`/`p`; `n` steps search matches while a search is active. |
| INV-NAV-05 | Replaced by `}`/`{` (hunks); `J`/`K` are free for V2. |
| INV-NAV-06 | Replaced by `u`/`shift+u`. |
| INV-NAV-07 | Done: `attention_next`. |
| INV-NAV-08 | Replaced by `)`/`(` over every thread (drafts are threads). |
| INV-NAV-09 | Replaced by `)`/`(`. |
| INV-NAV-10 | Replaced by `open_thread_next`/`open_thread_previous` across the review. |
| INV-NAV-11 | Done: `open_thread_next`/`open_thread_previous`. |
| INV-NAV-12 | Done: `ai_thread_next`, and `ai_thread_previous` (unreachable in V1). |
| INV-NAV-13 | Replaced by "Go to line N" in the palette, with source line numbers (6.2). |
| INV-NAV-14 | Replaced by `shift+v` (keyboard range); `v` marks viewed. |
| INV-NAV-15 | Done: `esc` clears a selection, then a search; folded threads expand with `shift+z`. |
| INV-NAV-16 | Done: `enter` (`activate`): a thread's or answer's action menu, a gap's reveal; panels have their own `enter`. |
| INV-NAV-17 | Done: selecting a threads-panel row jumps to the thread (loading its file first). |
| INV-NAV-18 | Replaced by the styled header (6.1). |
| INV-NAV-19 | Done: footer status and contextual key hints. |
| INV-NAV-20 | Replaced by criterion B (2.1 measurements). |
| INV-NAV-21 | Dropped (V1 defect): V2 handles pages, half pages, home, and end. |

### B.5 File tree and panels

| ID | Status |
| --- | --- |
| INV-TREE-01 | Replaced by the file panel and the threads panel (6.1). |
| INV-TREE-02 | Replaced by `t` (show or hide), `<`/`>` (width), and width-based degradation (6.1). |
| INV-TREE-03 | Replaced by rich tree rows (6.1). |
| INV-TREE-04 | Done: arrows and `j`/`k` move; `right`/`left` expand/collapse; `enter` opens (scope `code-review-files`). |
| INV-TREE-05 | Done: clicks and wheel. |
| INV-TREE-06 | Done: the tree follows the cursor's file; sources are separate groups. |
| INV-TREE-07 | Replaced by header chips and `edit_sources`. |
| INV-TREE-08 | Replaced by `edit_sources` and source diagnostics. |
| INV-TREE-09 | Replaced by the header (viewed progress, totals, drafts), the threads panel filters, and the home preview. |
| INV-TREE-10 | Replaced by the threads panel (status, severity, location, first line, imported and AI rows) with filters all, open, needs attention, general, and resolved; thread headers carry published, imported, and outdated badges. |
| INV-TREE-11 | Replaced by the general and needs-attention filters (`!` changes severity in V2). |
| INV-TREE-12 | Replaced by inline thread and answer rows with clickable actions, and the `enter` menu. |
| INV-TREE-13 | Replaced by the threads panel's `left`/`right` filters. |
| INV-TREE-14 | Replaced by `shift+f` and `shift+c` (and the palette). |
| INV-TREE-15 | Replaced by `tab` focusing panes and the palette for every action. |
| INV-TREE-16 | Done: `h` hides viewed files from the file panel. |

### B.6 Context expansion

| ID | Status |
| --- | --- |
| INV-CTX-01 | Done: gaps above, between, and after hunks (5.2). |
| INV-CTX-02 | Replaced by interval-set expansion: `e`/`shift+e` with counts, `x`, `shift+x`, gap clicks, `enter` (5.2). |
| INV-CTX-03 | Done: loading and unavailable states. |
| INV-CTX-04 | Done: keyed list positions keep the viewport. |
| INV-CTX-05 | Dropped (V1 defect): revision-correct reads, both line numbers, anchorable expanded lines. |

### B.7 Comments and threads

| ID | Status |
| --- | --- |
| INV-CMT-01 | Done: `c` (line or range), `shift+f`, `shift+c`; hunk headers are not commentable. |
| INV-CMT-02 | Done: the same anchor model plus `head_oid` and `blob_oid` (5.3). |
| INV-CMT-03 | Done: inline composer with titles per mode and `ctrl+p` preview. |
| INV-CMT-04 | Done: Save draft, Ask Bcode (switch), preview, cancel buttons; publishing is `shift+p` after saving. |
| INV-CMT-05 | Done: `enter`/`ctrl+s` submit, `esc` cancels, empty text discarded, paste; `shift+enter` adds a line (V1 defect fixed). |
| INV-CMT-06 | Done: host completion through `FocusedTextFieldAdapter` (6.2). |
| INV-CMT-07 | Replaced by saving the comment, then `shift+p`; the checklist shows readiness instead of a second press. |
| INV-CMT-08 | Done: optimistic, serialized, reconciled by job (6.2). |
| INV-CMT-09 | Done: `i` edits the thread's latest comment; unsaved comments are refused. |
| INV-CMT-10 | Done: `shift+d`, optimistic with rollback. |
| INV-CMT-11 | Done: `shift+r` resolves or reopens. |
| INV-CMT-12 | Done: `shift+h` hides or shows resolved threads (persisted). |
| INV-CMT-13 | Done: inline rows; header clicks and `z` fold a thread; `shift+z` collapses or expands every thread (persisted). |
| INV-CMT-14 | Done: thread actions Reply, Edit, Delete, Resolve or Reopen, Ask Bcode; answer actions Open session, Follow up, More/Less, Copy, Suggest comment, Draft, Retry, Delete; suggestion actions Accept, Refine, Reject. Per-thread publish is replaced by the checklist's include toggles. |
| INV-CMT-15 | Done: kinds and severities persist and can be changed (`shift+k`, `!`, composer `alt+k`/`alt+s`). |
| INV-CMT-16 | Replaced by the hover `+` and cursor/range styling (6.2, 6.3). |
| INV-CMT-17 | Done: V1's scope derivation (`NoteScope`). |
| INV-CMT-18 | Done: reloaded on open; outdated anchors are labeled (5.3). |

### B.8 AI questions and sessions

| ID | Status |
| --- | --- |
| INV-AI-01 | Done: `a`, or Ask Bcode on a thread. |
| INV-AI-02 | Done: `shift+a` menu with the five commands (digits are count prefixes in V2). |
| INV-AI-03 | Done: first questions create and link a session; later questions on the anchor are follow-ups in it. |
| INV-AI-04 | Done: bounded prompt with review, repository, sources, file, ranges, up to 80 lines, up to 12 other threads, the question, and the service operations. Line kinds are the diff markers; nearby hunk context and the draft being edited are reachable through `review.context.get` and `review.thread.get`, which the prompt names. |
| INV-AI-05 | Done: persisted exchanges; a failed save is not retried into a second session. |
| INV-AI-06 | Replaced by live status and the answer preview; tool calls and activity are in the linked session (`o`). |
| INV-AI-07 | Done: `subscribe_session_view`. |
| INV-AI-08 | Done: Retry button and `retry`. |
| INV-AI-09 | Done: `z`/More shows the whole answer (persisted); reachable by key now. |
| INV-AI-10 | Done: `shift+d` on an answer. |
| INV-AI-11 | Done: `o`; the position is restored on return. |
| INV-AI-12 | Done: `m` (`draft_answer`), deduplicated. |

### B.9 Suggestions

| ID | Status |
| --- | --- |
| INV-SUG-01 | Done: `s` (`suggest_comment`), duplicates refused. |
| INV-SUG-02 | Done: `;` accepts into a draft. |
| INV-SUG-03 | Done: `refine_suggestion` (button, menu, palette). |
| INV-SUG-04 | Done: `reject_suggestion`. |
| INV-SUG-05 | Done: persisted states shown inline on answers and threads. |

### B.10 Publish

| ID | Status |
| --- | --- |
| INV-PUB-01 | Replaced by `shift+p` (`x` reveals context in V2); readiness is shown in the checklist rather than by a second press. |
| INV-PUB-02 | Done: checklist with readiness, viewed, first unviewed paths and open threads, and per-thread include toggles; jumping from it is replaced by closing it and `u`/`open_thread_next`. |
| INV-PUB-03 | Done: publisher picker with capability labels; the provider's publisher is preselected for pull-request reviews, else `markdown_file`. |
| INV-PUB-04 | Done: built-in publishers through `review.publish.bundle.*`. |
| INV-PUB-05 | Done: external publishers, deselected threads excluded. |
| INV-PUB-06 | Done: schema form; `j`/`k` type into values (V1 defect fixed). |
| INV-PUB-07 | Done: scrollable preview, confirm, retry after a failure. |
| INV-PUB-08 | Done: published threads are marked; the pass is recorded. |
| INV-PUB-09 | Done: records saved and reloaded; home shows the last publish. |
| INV-PUB-10 | Done: GitHub publisher unchanged; it also takes the review's pull request target (7.6). |
| INV-PUB-11 | Done: unchanged mapping; `summary_only` threads go to the summary. |
| INV-PUB-12 | Done: unchanged preview. |

### B.11 External import

| ID | Status |
| --- | --- |
| INV-IMP-01 | Replaced by `shift+i` (`X` reveals a whole file in V2). |
| INV-IMP-02 | Done: the shared options form. |
| INV-IMP-03 | Done: importer id validated, snapshot replaced and reloaded. |
| INV-IMP-04 | Replaced by inline read-only threads plus threads-panel rows (7.6). |
| INV-IMP-05 | Done: GitHub importer unchanged. |

### B.12 Report

| ID | Status |
| --- | --- |
| INV-RPT-01 | Done: `local_markdown` in the publish flow. |
| INV-RPT-02 | Done: `markdown_file` and `json_file`. |
| INV-RPT-03 | Done: `copy_report` copies the Markdown report (6.2). |
| INV-RPT-04 | Done: `review.bundle.get` is still served (`publish-github`); V2 builds bundles itself (7.6). |

### B.13 Viewed files

| ID | Status |
| --- | --- |
| INV-VIEW-01 | Replaced by `v` (mark viewed and advance) with identity-keyed marks (5.4). |
| INV-VIEW-02 | Done: `view_all`, `unview_all`. |
| INV-VIEW-03 | Replaced by the header progress bar, checklist, home table, and preview. |
| INV-VIEW-04 | Done: `v` always advances to the next unviewed file. |

### B.14 Presentation state

| ID | Status |
| --- | --- |
| INV-PRES-01 | Done: `presentation_v2` keeps the scroll anchor, layout, panels, expansions, layer positions, collapsed threads, expanded answers, and the resolved and viewed-file filters (9). The selected thread is the scroll anchor; the threads-panel filter is per session. |
| INV-PRES-02 | Done: disposable, version-checked state; stale keys resolve best-effort (9). |
| INV-PRES-03 | Done: the return position is kept in memory. |

### B.15 Search and filters

| ID | Status |
| --- | --- |
| INV-SRCH-01 | Replaced by the file panel filter (`f`); `ctrl+p` is the palette. |
| INV-SRCH-02 | Replaced by `/` over the whole review, `n`/`shift+n`. |
| INV-SRCH-03 | Replaced by the threads panel filters (`left`/`right`). |
| INV-SRCH-04 | Replaced by home `/`. |
| INV-SRCH-05 | Replaced by home groups (drafts and attention are groups). |
| INV-SRCH-06 | Dropped (V1 defect): picker queries take every character. |

### B.16 Keymap actions

Home (`code-review-home`):

| ID | Status |
| --- | --- |
| INV-KEY-H01 | Done: `/`. |
| INV-KEY-H02 | Done: `r`. |
| INV-KEY-H03 | Done: `j`/`k`/arrows. |
| INV-KEY-H04 | Done: `d` (preview). |
| INV-KEY-H05 | Replaced by groups (INV-SRCH-05). |
| INV-KEY-H06 | Replaced by the *Needs attention* group. |
| INV-KEY-H07 | Done: `?`. |
| INV-KEY-H08 | Done: `enter`. |
| INV-KEY-H09 | Done: `c`. |
| INV-KEY-H10 | Replaced: `enter` always opens the review (no Build mode). |
| INV-KEY-H11 | Replaced by `edit_sources` inside the review. |
| INV-KEY-H12 | Done: `x`. |
| INV-KEY-H13 | Replaced by `n` (wizard). |
| INV-KEY-H14 | Replaced by suggestions `1`–`9` and the wizard. |
| INV-KEY-H15 | Replaced by groups (INV-HOME-17). |
| INV-KEY-H16 | Replaced by groups. |
| INV-KEY-H17 | Replaced by groups. |
| INV-KEY-H18 | Replaced by groups. |
| INV-KEY-H19 | Replaced by the archived group. |
| INV-KEY-H20 | Done: `g`/`G`. |
| INV-KEY-H21 | Done: `shift+r`. |
| INV-KEY-H22 | Replaced by `q` (Bcode surfaces close with `q`). |
| INV-KEY-H23 | Done: the filter and rename inputs use the shared BMUX text-input policy. |
| INV-KEY-H24 | Done: `esc` closes help and prompts; `tab` switches suggestions and reviews. |

Review (`code-review`):

| ID | Status |
| --- | --- |
| INV-KEY-R01 | Replaced by `t` (file panel). |
| INV-KEY-R02 | Replaced by `edit_sources`. |
| INV-KEY-R03 | Replaced by `edit_sources`. |
| INV-KEY-R04 | Replaced by `t` and `shift+t` (panels). |
| INV-KEY-R05 | Replaced by `f` (file filter). |
| INV-KEY-R06 | Replaced by `edit_sources`. |
| INV-KEY-R07 | Replaced by `edit_sources`. |
| INV-KEY-R08 | Done: `j`/`k`/arrows. |
| INV-KEY-R09 | Done: `g`/`G`. |
| INV-KEY-R10 | Done: `?`. |
| INV-KEY-R11 | Done: `attention_next`. |
| INV-KEY-R12 | Replaced by the palette's go-to-line. |
| INV-KEY-R13 | Done: `/` (whole review). |
| INV-KEY-R14 | Done: `shift+n`. |
| INV-KEY-R15 | Done: `n`. |
| INV-KEY-R16 | Done: `p`. |
| INV-KEY-R17 | Replaced by `)`/`(`. |
| INV-KEY-R18 | Replaced by `}`/`{`. |
| INV-KEY-R19 | Done: `shift+f`. |
| INV-KEY-R20 | Replaced by `shift+d` on an answer. |
| INV-KEY-R21 | Replaced by `shift+i`. |
| INV-KEY-R22 | Done: `shift+y`. |
| INV-KEY-R23 | Replaced by `)`/`(`. |
| INV-KEY-R24 | Replaced by the threads panel's needs-attention filter. |
| INV-KEY-R25 | Replaced by the threads panel filters. |
| INV-KEY-R26 | Done: `view_all`, `unview_all`. |
| INV-KEY-R27 | Replaced by `u`/`shift+u`. |
| INV-KEY-R28 | Replaced by `v`. |
| INV-KEY-R29 | Replaced by `open_thread_next`/`open_thread_previous`. |
| INV-KEY-R30 | Done: `open_thread_next`/`open_thread_previous`. |
| INV-KEY-R31 | Done: `ai_thread_next`. |
| INV-KEY-R32 | Done: `shift+h`. |
| INV-KEY-R33 | Replaced by `shift+r` (`r` refreshes). |
| INV-KEY-R34 | Done: `h` (file panel). |
| INV-KEY-R35 | Done: `shift+z` (collapse all, or expand all when all are collapsed) and `z` per thread. |
| INV-KEY-R36 | Done: `shift+d`. |
| INV-KEY-R37 | Replaced by `i` (`e` reveals context in V2). |
| INV-KEY-R38 | Replaced by `shift+v`. |
| INV-KEY-R39 | Done: `c`. |
| INV-KEY-R40 | Done: `shift+c`. |
| INV-KEY-R41 | Replaced by `shift+p`; inline actions run with `enter` or clicks. |
| INV-KEY-R42 | Done: `a`. |
| INV-KEY-R43 | Done: `s`. |
| INV-KEY-R44 | Done: `;`. |
| INV-KEY-R45 | Replaced by `retry` and the Retry button (`y` copies lines in V2). |
| INV-KEY-R46 | Done: `o`. |
| INV-KEY-R47 | Replaced by the `shift+a` menu (digits are count prefixes). |
| INV-KEY-R48 | Replaced by `q`/`esc` (INV-ENT-14). |
| INV-KEY-R49 | Done: `enter` activates, `esc` clears, `tab` focuses panes, panel `left`/`right`, and pages, half pages, home, and end all move. |
| INV-KEY-R50 | Done where V1 hid them: refine, reject, draft answer (`m`), expand answer (`z`), and previous AI thread are reachable actions with honest labels. |

Build mode (`code-review-build`):

| ID | Status |
| --- | --- |
| INV-KEY-B01 | Replaced by the review's keys and `edit_sources` (no Build mode). |
| INV-KEY-B02 | Replaced by the wizard's file step in `edit_sources`. |
| INV-KEY-B03 | Replaced by "Add another source…" in `edit_sources`. |
| INV-KEY-B04 | Replaced by the wizard's picker navigation. |
| INV-KEY-B05 | Replaced by `r`/`ctrl+r` refresh. |
| INV-KEY-B06 | Replaced by the working-tree step. |
| INV-KEY-B07 | Replaced by per-source `tab` (INV-SRC-11). |
| INV-KEY-B08 | Replaced as INV-SRC-12. |
| INV-KEY-B09 | Replaced as INV-SRC-13. |
| INV-KEY-B10 | Replaced as INV-SRC-10. |
| INV-KEY-B11 | Replaced as INV-SRC-14. |
| INV-KEY-B12 | Replaced as INV-SRC-15. |
| INV-KEY-B13 | Replaced by `tab` in `edit_sources`. |
| INV-KEY-B14 | Replaced by home `r` and the title in `edit_sources`. |
| INV-KEY-B15 | Replaced by source headers and file panel groups. |

Overlays:

| ID | Status |
| --- | --- |
| INV-KEY-P01 | Dropped (V1 defect: declared without a handler); wizard and palette inputs submit with `enter`. |
| INV-KEY-P02 | Done: pickers move with arrows, `enter` submits, `esc` cancels, and every character types. |
| INV-KEY-C01 | Done: composer `ctrl+s`. |
| INV-KEY-C02 | Done: composer `ctrl+p`. |
| INV-KEY-C03 | Done: `enter` submits, `shift+enter` adds a line, buttons are clickable, `esc` cancels, `ctrl+t` switches comment and question. |
| INV-KEY-I01 | Done: importer menu (arrows, `enter`, `esc`). |
| INV-KEY-I02 | Done: the shared options form. |
| INV-KEY-U01 | Replaced: the checklist lists open threads; the threads panel's needs-attention filter replaces the attention sidebar. |
| INV-KEY-U02 | Replaced by closing the checklist and `u`. |
| INV-KEY-U03 | Replaced by closing the checklist and `open_thread_next`. |
| INV-KEY-U04 | Done: arrows in the picker and preview. |
| INV-KEY-U05 | Done: `p` (or `esc`) returns from the confirmation to the preview. |
| INV-KEY-U06 | Done: `space` toggles a thread, `enter` advances, `esc` closes. |
| INV-KEY-U07 | Done: form keys; the checklist jump keys are replaced as U01–U03. |
| INV-KEY-X01 | Resolved: `v` marks viewed, `shift+v` selects a range. |
| INV-KEY-X02 | Resolved: `:` is the palette, with go-to-line. |
| INV-KEY-X03 | Resolved: `ctrl+p` is the palette in the review and the preview in the composer (focus decides). |
| INV-KEY-X04 | Resolved: `w` is free; `shift+w` toggles wrapping. |

### B.17 Mouse

| ID | Status |
| --- | --- |
| INV-MOUSE-01 | Replaced by keyed wheel motion and per-pane wheel scrolling. |
| INV-MOUSE-02 | Replaced by stack-rail tabs, the publish split button, and `shift+f`/`shift+c`. |
| INV-MOUSE-03 | Done: inline thread and answer buttons. |
| INV-MOUSE-04 | Done: composer buttons. |
| INV-MOUSE-05 | Done: threads panel rows. |
| INV-MOUSE-06 | Done: tree rows. |
| INV-MOUSE-07 | Done: press-and-drag selects a range and opens a range composer (6.2). |
| INV-MOUSE-08 | Done: gap halves reveal, thread headers fold, inline actions run, expanded context is commentable. |
| INV-MOUSE-09 | Done: home clicks. |
| INV-MOUSE-10 | Done: hit regions only for painted content; overlays and modals isolate the background. |
| INV-MOUSE-11 | Dropped (V1 unreachable code). |

### B.18 Clipboard

| ID | Status |
| --- | --- |
| INV-CLIP-01 | Done: `shift+y` and the Copy button, with V1's status messages. |
| INV-CLIP-02 | Done: paste into the composer, inputs, and forms; `y` copies selected lines. |

### B.19 Help

| ID | Status |
| --- | --- |
| INV-HELP-01 | Replaced by complete, scrollable help generated from the keymap (review, composer, file panel, threads panel). |
| INV-HELP-02 | Done: home help. |
| INV-HELP-03 | Done: modal footers carry their keys. |
| INV-HELP-04 | Replaced by status messages for refused actions, key hints, and toasts. |

### B.20 Display

| ID | Status |
| --- | --- |
| INV-DISP-01 | Replaced by whole-blob highlighting and unified/side-by-side layouts (6.1, 6.4). |
| INV-DISP-02 | Done: markdown comments and answers. |
| INV-DISP-03 | Replaced by `ReviewTheme` (6.3). |
| INV-DISP-04 | Done: empty states for the document, the threads panel, and home. |

### B.21 Service contracts

| ID | Status |
| --- | --- |
| INV-DATA-01 | Done: every operation is still served except the V1-only `review.workspace.materialize` (removed in phase 7, 4.2); V2 adds operations additively (4.2), including `review.suggestion.delete`. |
| INV-DATA-02 | Done: unchanged; additive pull-request fields (7.6). |
| INV-DATA-03 | Done: unchanged. |
| INV-DATA-04 | Done: left in place, not dispatched, for their owner's decision. |
