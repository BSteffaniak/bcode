# Code Review V2 architecture

Status: proposed. This document is the contract for the Code Review V2 rewrite. Phases land in
order; each phase is accepted against the criteria in section 11.

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

V2 is a parallel rewrite. V1 receives no new features and no bug fixes. V2 is selectable at
startup during development, becomes the default once parity criteria pass, and then V1 is
deleted together with the host-side review launcher code (section 10, phase 7).

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

Measured by the ignored test `v2::testing::v2_data_benchmark::v2_data_layer_benchmark`
(`cargo test -p bcode_code_review_plugin v2_data_layer_benchmark -- --ignored --nocapture`) on the
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
* `review.workspace.materialize` remains for V1 until deletion; V2 never calls it.
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
  routing lives in `src/v2/provider.rs` behind a `ProviderTransport` trait (the daemon client in
  production, fakes in tests). With the provider plugin disabled nothing answers, and only the
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

### 5.3 Anchors

Anchors keep the existing source-identity model and add the resolved head OID. An anchor whose
line no longer exists after refresh is shown as outdated in its thread rather than silently moved.

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

### 6.2 Interaction

* Keyboard: line/page/half-page scrolling, next/previous file, hunk, thread, and unviewed file;
  `v` mark viewed and advance; count prefixes; `/` search within the review; `:` or `ctrl-p`
  command palette for every action; `?` help generated from the keymap.
* Mouse: click files, gaps, thread actions; drag across lines to select a range and comment on it;
  hover shows a `+` comment affordance in the gutter; wheel scrolling uses `ScrollMotion`.
* Composer: inline under the anchor, using the shared prose editor; markdown preview toggle.
* AI questions keep using `PluginTuiAction::OpenSession`; returning restores the exact position.
* All keys are declared in the plugin manifest keymap (configurable), as today.

### 6.3 Visual system

* `ReviewTheme` is derived from the presentation theme's `ComponentTheme` with semantic roles:
  file status (added, modified, deleted, renamed, binary), `+`/`−` counts and change bars,
  badges (draft, open, resolved, outdated, AI), gutter and line numbers, hunk headers, gap rows,
  sticky headers, cursor line versus selected range, focused-pane border, muted metadata.
* Every row is composed from styled spans; no formatted single-string rows.
* Icons have ASCII fallbacks selected by presentation policy.
* Home and review share one theme derivation.

### 6.4 Performance pipeline

* Item layout is identity-keyed and cached per width/layout mode; measurement is incremental
  (`MeasuredListIndex`).
* Syntax highlighting is computed per whole blob (correct multi-line constructs), off the render
  path, cached by `(blob OID, syntax theme)`, and projected onto diff lines by line number. Until
  ready, lines render unhighlighted without layout change.
* Intra-line change emphasis is computed per changed line pair and cached with the item.
* Frames are pure: one snapshot per frame, no service calls, no recomputation of unchanged items,
  regional damage on scroll.

## 7. Home and new-review flow

### 7.1 One surface, internal routes

The plugin owns one surface with internal routes: `Home`, `NewReview` (wizard), `Review`. The
plugin runtime loads once per `bcode review` invocation; moving between routes never reloads it
or reinitializes the TUI. The host gains one generic capability: run any registered plugin surface
as a standalone startup surface by plugin id and surface kind. All `code-review` special cases are
removed from `packages/tui` and `packages/cli`.

### 7.2 Home

* **Suggestions** at the top, derived from repository state: uncommitted changes, current branch
  ahead of its base, PRs requesting my review, a stack containing the current branch, reviews with
  new commits since my last pass.
* **Reviews table** (BMUX `table`): title, sources, viewed progress bar, drafts/threads, publish
  status, updated. Grouped: needs attention, in progress, published, archived (collapsed).
* Type-to-filter search, preview panel (file stats, sources, last publish), rename, archive,
  restore, and one-key continue-latest.

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

### 7.4 Re-review

When a review's sources resolve to new OIDs, the review offers "changes since last review". For
fast-forward updates this is the diff between the previous and current head. After a rebase,
files are compared by patch-id: unchanged patches stay viewed and are de-emphasized, changed
patches are shown.

### 7.5 CLI

`bcode review` opens Home. Direct entry points open the Review route with a transient workspace,
which can be saved:

* existing: `unstaged`, `staged`, `all`, `last-commit`, `range`, `repo`, `publish-github`
* new: `branch [BASE]`, `compare BASE HEAD`, `commit REV...`, `pr NUMBER|URL`,
  `stack [NUMBER|URL]`, `resume [ID]`

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
* Unknown future source kinds are preserved and surfaced as unsupported, not guessed: they
  deserialize as `ReviewSourceKind::Unsupported { kind, raw }`, serialize back verbatim, and
  resolve to an `unsupported_source_kind` diagnostic. V1 reports the new and unknown kinds as an
  unsupported-source materialization diagnostic.
* V1 continues to open workspaces that contain only V1 source kinds until it is deleted.

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

* Parallel rewrite; V1 is deleted after V2 reaches parity.
* Stacks are linear PR chains (Graphite-style, GitHub native stacked PRs); no tool-specific
  metadata.
* Side-by-side ships in the first release. Layout is configurable as `auto`, `unified`, or
  `side_by_side`, matching `[tui.diff_viewer]`, with `auto` at a 120-column breakpoint by default.
* Hunk residency budget defaults to 64 MiB and is configurable.
* Whole-stack mode ships with comment attribution and publishing as described in 7.3.
* BMUX components are used wherever they fit; generic gaps are added upstream without Bcode
  semantics; review-specific behavior stays in the code review plugin.

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
