# Stack History and Comparisons

A pull request, and a stack of them, has two axes: its layers, bottom to top, and time, as each
layer is pushed again. The review screen shows every layer as it is now, and answers, in place,
the question a returning reviewer has first: *what actually changed since I last looked?* It also
compares any two points in a pull request's or a stack's history without leaving the review.

The central problem is noise. A stack tool's restack pushes every layer above a change, even
when only one layer changed. Rebasing a layer onto a moved parent changes its commits without
changing what it does, and a naive "old head to new head" diff of a rebased layer shows
everything that landed below it. Every comparison separates *what the author changed* from *what
moved underneath*, at every level: stack, pull request, file, and line.

## Principles

* **One engine.** Every comparison, whether automatic or picked by hand, uses the replay-based,
  rebase-aware comparison (`history::compare`). There is no second "since last review" path.
* **One screen.** A comparison changes how the current review shows its layers; it never opens
  another review. Viewed state, comments, AI asks, search and navigation keep working because
  the review's sources, files and anchors stay the same.
* **The obvious default needs no setup.** A reviewer's baseline for each pull request is recorded
  automatically as they work, and a review whose stack moved since opens comparing against it.
* **Arbitrary comparisons stay one step away.** Any version of a pull request against any other,
  and any push or time of the stack against any other, are picked from the same header control.
* **Bounded work.** Opening a changed stack costs a bounded, batched, cached amount of git work,
  with progress shown, and never recomputes the whole stack on a keystroke.

Comparisons apply to every review whose sources are pull requests: single pull requests, stack
layers stepped through, and whole-stack sources. Reviews of local branches and commits have no
version history and show no comparison control.

## The model

### Layer versions

* **Layer version:** one state of one pull request: its head commit, the commit it was based on,
  its base branch, when it was first seen, who pushed it when known, and what saw it.
* **Version log:** every layer version ever seen, per repository and pull request number, shared
  by every review of it, in the review store (`layer_versions`). History cannot be recomputed
  once a force push drops commits from the branch, so the log is durable, not cache.
* **Kind of change**, from the previous version of the same layer:
  * *Content:* the layer's own change differs.
  * *Rebase only:* the old change replayed on the new fork point is exactly the new version.
  * *Commits added:* a fast-forward on top of the previous head.
  * *Retargeted:* the base branch changed.
  * *Unavailable:* its commits are not in the local repository.
* **Stack push:** versions of several layers seen within the push window (two minutes by
  default). A restack of 30 layers is one push: "2 layers changed, 28 rebased".
* **Version number:** a layer's versions with a change of their own, oldest first, numbered
  `v1..vN` for display. Numbers are derived from the timeline, never stored.

Versions come from the forge's push timeline (`review_source.pulls.history`, fetched when the
version picker, timeline or matrix needs more than the local log), from observation (every stack
a review resolves or confirms), from branch reflogs, and from looks (below). They are merged and
deduplicated by head commit.

### Looks: the "since you last looked" baseline

A **look** records that the reviewer saw a specific version of a pull request.

| Field | Meaning |
|---|---|
| provider, repository, number | The pull request. |
| `head_oid` | The head the review showed. |
| `fork_oid` | Where that version's own change starts, when known. |
| `kind` | `opened`, `viewed`, `commented`, `asked`, `pass`, `reviewed` (a verdict on the forge). |
| `looked_at_ms` | When (the latest time for this head and kind). |
| `review_id` | The review it happened in, when durable. |

* **Where:** a `layer_looks` table in the review store (migration `027_layer_looks_table`), per
  user, keyed by pull request, head and kind, with upsert semantics (a repeated look moves its
  time forward). It is shared by every review: looking at a pull request in a single-PR review
  counts when it later appears in a stack review, and the other way round. At most 32 heads are
  kept per pull request; the oldest are pruned with their refs.
* **Operations:** `review.looks.record` (batched upsert) and `review.looks.list` (every look of a
  set of pull requests, one query).
* **When recorded,** always for the version the review currently shows of that layer, and only
  when the review shows the layer's current head (never while comparing against a historical
  *to* version):
  * `viewed`: a file of the layer is marked viewed.
  * `commented` / `asked`: a draft comment, reply, or AI ask is made on the layer.
  * `pass`: the review pass is completed (`shift+m`) or the review is published.
  * `reviewed`: the forge reports a review by the viewer on that commit (read with the
    conversation; no extra call).
  * `opened`: the layer was the current layer, with the screen focused, for
    `compare.dwell_seconds` (default 8). Recorded at most once per head per session.
* **Keeping commits:** recording a look also pins its head under
  `refs/bcode-review/version/<oid>` (the namespace fetched versions already use), so the baseline
  survives garbage collection after a force push. Pruned looks drop their refs.
* **Migration:** the table is created by an ordinary store migration. Existing data is seeded
  idempotently when a review opens: each source's `reviewed_revisions` entry for a pull request
  or stack layer becomes a `pass` look at `recorded_at_ms`, and forge reviews by the viewer
  become `reviewed` looks. `reviewed_revisions` stays written (by `pass`) for compatibility but
  nothing reads it for comparisons. The timeline's `reviewed` flag is derived from looks.

**Choosing the baseline** (`last_looked`, per layer):

1. Take the layer's looks of an *engaged* kind (`viewed`, `commented`, `asked`, `pass`,
   `reviewed`); when there are none, its `opened` looks (`compare.baseline_look = "opened"`
   counts `opened` looks always).
2. The baseline is the newest such look whose head is not the current head. When the newest
   engaged look *is* the current head, the layer is **unchanged since you looked**.
3. A layer never looked at has no baseline: it is **new to you**, shown as its whole change.
4. The baseline is **pinned for the session** when the review opens: looks recorded while
   reviewing do not move it. When the stack moves while the review is open, a moved layer's
   baseline becomes the head shown before the move if the reviewer engaged with it in this
   session, else stays pinned.

Engagement, not dwell, is the default because a glance at one file must not hide the rest of a
new version. Viewed state complements it: a file unchanged since the baseline but never viewed is
still shown as needing review (see [The two views](#the-two-views)).

## Comparing versions

Every result is content-addressed by commit and tree ids and cached.

* **Replay.** For a layer from version A to version B, A's own change (`fork(A) → head(A)`) is
  replayed onto `fork(B)` with one `git merge-tree --write-tree --stdin` batch for every layer
  being compared. A clean replay yields a tree; it is wrapped in a **deterministic** commit
  (`commit-tree` with fixed author, committer, dates and a message naming its inputs, parent
  `fork(B)`), so the same inputs always produce the same commit id and every downstream cache key
  is stable. No ref is written; a lost replay commit is rebuilt from its inputs.
* **Layer delta** (`history::compare::LayerDelta`): from the replay commit to `head(B)`:
  * *unchanged* when `head(A) == head(B)`;
  * *rebased only* when the replay tree equals `head(B)`'s tree (no diff needed);
  * *changed*, with a per-file summary from one `git diff-tree -r --numstat -z replay head(B)`:
    each path's status, `+`/`−`, and whether the file was added to or dropped from the layer's
    change relative to A;
  * *new to you* when there is no A: the whole change `fork(B) → head(B)`;
  * *with the rebase* when either fork point is unknown or the replay conflicts: head against
    head, said so on the layer and its files;
  * *unavailable* when A's commits cannot be had (after one batched fetch by id).
* **Line marks** for one file (`review.compare.marks`, a document need like whole-stack
  attribution), from two `git diff -U0` runs limited to the file's paths, `replay → head(B)` and
  `fork(B) → replay`:
  * new-side lines added in `replay → head(B)` are **new since the baseline**;
  * a line removed in `replay → head(B)` that maps back through `fork(B) → replay` to an
    unchanged fork line is an old-side line of the current diff that is **newly removed**;
  * a removed line that was part of A's own change is **removed since the baseline**: it does
    not exist in the current diff, so it is reported as a marker (count and text) at the new-side
    position where it was.

  Work is bounded per file like attribution (`MAX_ATTRIBUTION_LINES`).
* **Stack-wide comparisons** are per-layer deltas with each layer's A and B chosen by a rule:
  each layer's version at time T or before push P (`history::version_at`), the stack as it was at
  T (B = the version at T, no A), or, for a whole-stack source, the top layer replayed onto where
  the bottom layer forks now (trunk movement taken out).
* **Range-diff** of two versions' commit series stays available per layer from the version
  picker, for reviewers who want commit-level detail.

## How a comparison is represented: an overlay, not new sources

A comparison is **state on the review screen plus an overlay on the existing document**. The
review's sources, groups, `FileKey`s and resolved revisions never change.

```
Comparison {
    baseline: Baseline,              // LastLooked | PreviousPush | Push(index) | Time(ms)
                                     // | Versions (per-layer picks) | None (full change)
    to: Target,                      // Now | Push(index) | Time(ms) | Versions
    view: CompareView,               // Full | Changes
    only_changed_layers: bool,
    layers: BTreeMap<SourceId, LayerComparison>,
}
LayerComparison {
    from: Option<VersionRef>, to: VersionRef,
    delta: Pending | LayerDelta,
    file_overrides: BTreeMap<Path, (VersionRef, VersionRef)>,   // file header stepping
}
```

The document gains `ReviewDocument::set_comparison(group, GroupComparison)`:

* `files`: per path, `FileSince` = `Unchanged`, `Changed { added, removed }`, `New`, `Dropped`,
  or `Unknown` (delta still loading).
* `changes_base`: the replay (or fallback) commit, used as the base of hunk requests in the
  changes view; `None` in the full view.
* line marks arrive per file through a `DocumentNeed::Marks` need, loaded for the viewport and
  prefetch window like hunks and attribution, evicted with the file's content.

The comparison state lives in the TUI surface; the overlay types and the marks computation live
in the data layer and its models, so other front ends get the same semantics.

**Why an overlay.** Opening a comparison as sources (the retired approach) replaced the resolved
revisions, so file identities, viewed marks, thread anchors, AI asks, layer positions and the
publish target all referred to a different, synthetic diff, and every switch reopened the review.
With an overlay:

* **Viewed state** is always computed from the file's current identity (`fork → head` blobs and
  patch id). Marking a file viewed in either view records the same mark.
* **Comments** anchor to the current diff. The new side of both views is the same blob
  (`head(B)`), so a new-side line has the same anchor, thread placement and publish position in
  both. Old-side anchors of the full view map into the changes view where the line exists on both
  sides, else show at the file under a note.
* **Navigation** keys (`ItemKey` by file and new-side line) survive a view switch; the cursor
  stays on the same line.
* **Caching** is unchanged: hunks are cached by `(base, head, path)`, so both views stay cached
  and switching back is instant.

## The two views

The default (`compare.start_view = "full"`) is the **full view**; `shift+l` switches to the
**changes view** and back. Both are the same document with the same files and keys.

**Full view: the whole change, with what is new marked.**

* Lines new since the baseline are tinted (`theme.since_new`) and carry a one-cell gutter bar
  (`theme.icons.since_bar`). Newly removed old-side lines get the same treatment on their side.
* Code removed since the baseline shows as a one-row marker between lines,
  "▾ 3 lines removed since you looked", which expands (`enter`, click) to the removed text,
  read-only.
* A hunk with marked lines says so in its header ("new since you looked"); `]`/`[` (next and
  previous change) stop on marked lines first while a comparison is active, then on changes.
* Files are folded by state:
  * unchanged since the baseline and viewed: folded to their header, badge
    "unchanged since you looked";
  * unchanged since the baseline but never viewed: open, badge "unchanged since v3 · not viewed";
  * changed: open, badge "new since you looked: +12 −3";
  * dropped from the layer's change: a header-only row, "dropped since you looked".
* A layer that was only rebased collapses to its banner and one line: "rebased, nothing changed
  since you looked", with **Show files** to open it.

**Changes view: only what changed, rebase taken out.**

* Each changed file shows the interdiff `replay → head(B)`: the before side is the baseline
  version's change replayed on today's base, the after side is the current code. Side-by-side
  layout labels the columns *before* (`v3`, when) and *now*.
* Unchanged files collapse into one row per layer, "18 files unchanged since you looked", which
  expands to their headers. Rebased-only layers show only their one line.
* Comments: new-side lines take comments exactly as in the full view. A before-side line that
  maps to the current diff's old side anchors there; a before-side line that exists only in the
  baseline version is read-only ("comment on the current code"), since a forge cannot anchor to
  it.
* `v`, `c`, `a`, `z`, threads, search and AI asks behave as in the full view.

When the comparison's *to* is not the current version (a historical pick), only the changes view
is available for that layer, its rows are read-only for comments, the header says "comparing v2 →
v3 (not current)", and `v` still acts on the file's current identity, labeled so in the status
line.

## The comparison control

The review header shows, whenever the review contains pull request sources:

```
Changes since: [you last looked · 2d ago ▾]   3 of 21 PRs changed · 18 rebased only
```

* **Opening it:** `b` (action `compare_baseline`) or a click opens a menu:
  * **You last looked** (per pull request; the default);
  * **Previous push** (each layer's version before the stack's latest push);
  * **A push…** (focuses the push timeline);
  * **A version of this pull request…** (opens the version picker on the current layer);
  * **A time…** (presets 1 hour, today, yesterday, a week, or a typed duration or date);
  * **Full change** (no comparison: the review exactly as without history);
  * toggles: **Only pull requests with real changes**, **Show only what changed** (`shift+l`).
* **Summary:** counts of layers changed, rebased only, unchanged, new to you, and unavailable,
  with *with the rebase* when some layer fell back. While deltas compute, it shows progress
  ("comparing 6 of 21…").
* **Reopening a changed review** starts in comparison mode with a banner under the header rather
  than a one-time toast: "Changed since you looked 2 days ago: 3 pull requests changed, 18 only
  rebased. **Only what changed** (shift+l) · **Compare with…** (b) · **Dismiss**". Dismissing
  hides the banner and keeps the comparison; **Full change** in the control turns it off. A
  review with nothing new since the baseline shows no banner and the control reads "Changes
  since: you last looked · nothing new".
* The choice of baseline (not the computed result) is kept in the workspace's presentation
  state, so reopening keeps "previous push" or a fixed time; *last looked* re-resolves on each
  open.

## Arbitrary comparisons

### Version picker (per pull request)

Opened from the control, from a click on the file header's version label, or from the palette.
One row per version, newest first:

```
  to ▸ v5  now      ana   changed        +12 −3 · 2 files
       v4  2h ago   ana   rebased
from ▸ v3  2d ago   ana   commits added  +40 −12 · 3 files   ◆ you viewed
       v2  3d ago   bob   retargeted     main → feat-a        ✓ you approved
       v1  4d ago   ana   first
       ·   4d ago   ana   unavailable (commits gone)
```

* `j`/`k` move; `f` sets *from*, `t` sets *to* (`to` defaults to now); `enter` applies; `esc`
  cancels. A click on the left gutter of a row sets *from*; a click with a modifier sets *to*.
  Unavailable rows cannot be picked.
* Marks show looks (◆ viewed, ✓/✕ forge verdicts, ○ opened only).
* `g` toggles a range-diff of the two versions' commits as a read-only detail pane.
* Applying updates that layer's comparison in place; other layers keep theirs, and the control
  then reads "Changes since: [mixed ▾]".

### Push timeline (stack-wide)

A one-row strip under the rail, shown when the stack has two or more pushes and a comparison is
active (or always, with `compare.timeline = "always"`):

```
pushes  ·│·┃··│[·┃··│┃]·  now    last push 12m ago: #12 changed, 28 rebased
```

* One tick per push, evenly spaced by push order; pushes with real changes are emphasized (`┃`,
  `theme.since_new`), rebase-only pushes are faint (`│`). `[` and `]` mark *from* and *to*.
* Keyboard: **A push…** in the control focuses it; `left`/`right` move, `f`/`t` set *from*/*to*,
  `enter` applies, `esc` leaves. Mouse: a click on a tick sets *from*; a click with a modifier
  sets *to*; hovering shows the push's summary in the status line.
* Applying sets every layer's comparison to its versions at those pushes.

### The history matrix

`shift+b` opens the matrix (layers by pushes, cells by kind, looks marked) as an expanded detail
of the timeline. Its keys keep their meaning but act in place: `enter` compares the cursor cell's
layer with its previous version, `p` the cursor's push, `s` from the pin to the cursor column, `r`
since you last looked, `a` the stack as it was at the cursor column, `w` the whole stack since the
pin (whole-stack sources), `f` pins, `i` includes rebased layers. Each closes the matrix and
updates the review.

## Layer badges and filter

Each layer's rail tab and file-panel group row carries a badge from its delta:

| Badge | Meaning |
|---|---|
| `● +40 −12 · 3 files` | Changed since the baseline. |
| `○ rebased` | Only rebased: nothing to review again. |
| `✓ unchanged` | Same head as the baseline. |
| `+ new to you` | Never looked at. |
| `≈ with rebase` | The rebase could not be taken out. |
| `? unavailable` | The baseline's commits are gone; shown as the full change. |
| `…` | Still comparing. |

Glyphs come from `theme.icons` with ASCII fallbacks. Tab labels keep their three widths: full
(`#101 Title  ● +40 −12 · 3 files  2/5`), compact (`#101 ●+40−12`), minimal (`#101●`).

**Only pull requests with real changes** (control toggle, palette, no default key) hides
unchanged and rebased-only layers: the rail folds them into one `+18 rebased` tab, `.`/`,` skip
them, the file panel omits their groups, and the all-layers document hides them
(`ReviewDocument::set_focus`). It is kept in presentation state.

## File header versions

In a comparison, a file header of a layer shows the file's own history in that pull request:

```
src/lib.rs   new since you looked: +12 −3   changed in 2 of 4 pushes · v4 ◂ ▸
```

* "Changed in N of M pushes" counts the layer's version steps whose replayed delta touches the
  file (`git diff-tree --name-only` per consecutive version pair, computed once per layer when
  the layer is first shown with a comparison, cached by commit ids).
* `◂`/`▸` (click, or `alt+[`/`alt+]`, actions `file_version_previous`/`file_version_next`)
  step this file's comparison to the previous or next pair of versions in which it changed. The
  step is a per-file override (`file_overrides`); the rest of the layer keeps its comparison, and
  changing the layer's comparison clears the overrides.
* *This file through the stack* (`stack_file`, palette) lists the layers changing the file, each
  with its badge.

## Comments in comparisons

Threads keep their current placement rules (exact, moved with their code, near their code, on
the file, on the layer), and comparisons add where the data allows:

* **Forge threads written on an older version** (`RemoteAnchor.original_commit_oid`,
  `original_line`, `diff_hunk`): in the changes view, a thread whose original commit is the
  baseline version (or maps to it) shows on the *before* side at its original line, mapped from
  the original commit into the replay commit through `git diff -U0 original → replay` (which, for
  the baseline version, is only base movement). The after side then shows what replaced that
  code. In the full view, a thread whose lines were removed since the baseline attaches to the
  "removed since you looked" marker that holds them, reading "its lines were removed since".
* **Local drafts** carry `head_oid` and `blob_oid`. A draft written on an older version is
  followed the same way: original head → that version's replay → current head (the interdiff),
  becoming *moved with its code* when every line maps, *near its code* when its lines were
  rewritten, and attached to a removal marker when they were removed. Its stored anchor is not
  rewritten; the placement is computed.
* When the original commit is not available and `diff_hunk` is, the hunk's last lines are matched
  against the before side text to place the thread; failing that, it stays on its file.
* Line following uses one `git diff -U0` per (commit, commit) pair for all threads, as
  `conversation::place_threads` does.

## Retired paths

* **`rereview.rs`** is removed. Its fast-forward `old..new` view and its whole-pull-request
  fallback are replaced by layer deltas from the last-looked baseline. Its patch-id carry-over
  (marking files with unchanged patches viewed) is dropped: under `viewed_reset = "diff"` the
  viewed matcher already keeps such files viewed through a rebase, and "unchanged since you
  looked" folding is the comparison's job and never marks anything viewed. `carried_over` on
  `FileHeaderItem` becomes `since: Option<FileSince>`. The data operations it used
  (`review.revisions.compare`, `review.patch_ids.get`) stay for other callers or are removed when
  none remain.
* **The separate-review path** is removed: `Action::OpenReview`, the app's `return_to` stack,
  `history::sources_of`, `Message::HistoryCompared` opening reviews, `since_last` source swapping,
  `Message::Rereview`/`Message::Carried`, and the publish menu's *Changes since last review* entry
  (which becomes *Only what changed*). `history::compare` returns `LayerDelta`s instead of review
  sources.
* **`shift+l`** keeps its default key under a new action id, `compare_view`; the old id
  `since_last_review` is accepted as an alias for existing key overrides. **`shift+m`** (complete
  pass) stays and records `pass` looks.

## Keys

| Keys | Action id | Action |
|---|---|---|
| `b` | `compare_baseline` | Changes since…: choose what to compare with |
| `shift+l` | `compare_view` | Only what changed / the full change with what is new marked |
| `shift+b` | `stack_history` | History matrix (acts in place) |
| `alt+[`, `alt+]` | `file_version_previous`, `file_version_next` | This file's previous / next version |
| palette | `compare_only_changed` | Only pull requests with real changes |
| palette | `compare_off` | Full change (no comparison) |
| palette | `compare_versions` | Compare versions of this pull request… |
| palette | `compare_push` | Compare pushes of the stack… |

`b`, `alt+[` and `alt+]` are unbound in the `code-review` scope. All are ordinary configurable
actions in `keymap.toml`, listed in keyboard help and the palette. Inside the picker and the
timeline, keys are local to that surface (`f`, `t`, `g`, arrows, `enter`, `esc`) and do not reach
the review.

## Caching and performance

* **Cache kinds** (`cache.rs`, content-addressed, no invalidation): `Replay` keyed by
  `(old fork, new fork, old head)` → replay commit or *conflicted*; `Delta` keyed by
  `(replay, head)` → per-file summary; `Marks` keyed by `(fork, replay, head, path)` → line
  marks; `FileSteps` keyed by a layer's version heads → paths changed per step.
* **Opening a changed stack:**
  1. The first frame paints from snapshots and caches exactly as without comparisons.
  2. After the stack is confirmed, the timeline is built from the local log (one commit check,
     one history walk, one `merge-tree` batch), and looks are listed in one store query. No forge
     call is needed for the default: the baseline was looked at, so its commits are local.
  3. Baselines are resolved and replays computed in one `merge-tree --stdin` batch for all layers
     missing from the cache. Rebased-only and unchanged layers are known from tree and head
     equality without diffs.
  4. Deltas of changed layers run on the blocking pool, at most `max_inflight_hunks` at a time,
     starting from the current layer and spreading outward; badges fill in as they land and the
     control shows progress.
  5. Line marks load per file with the viewport, like hunks.
* **Interaction cost:** switching views re-splices the affected groups from resident data and
  cached hunks; changing a baseline recomputes only the layers whose pair changed; file stepping
  recomputes one file. Nothing walks the whole stack per keystroke.
* **Bounds:** marks share attribution's per-file line limit; the forge timeline is fetched only
  by the picker, the timeline's "load older pushes" and the matrix; look history is capped per
  pull request.

## Narrow widths

All text is measured with BMUX text layout; labels choose among precomputed variants by fit,
and hit regions come from the same layout as the painted variant.

* **Header control:** full → `Since: last looked ▾ · 3/21 changed` → `Δ last looked ▾` → `Δ▾`.
  Below the width of `Δ▾` the control is omitted and remains reachable with `b` and the palette.
* **Banner:** wraps to at most two rows; below a minimum it shrinks to its first sentence and the
  buttons become key hints.
* **Rail badges:** follow the tab's full, compact and minimal labels.
* **Push timeline:** with fewer cells than pushes, consecutive pushes share a cell (emphasized
  when any of them changed content); below 12 cells the strip is hidden and the matrix remains.
* **Line marks:** the gutter bar is part of the line prefix layout and is the first prefix cell
  dropped at narrow widths; the tint remains.
* **File header versions:** `changed in 2 of 4 pushes · v4 ◂ ▸` → `v4 ◂ ▸` → omitted (stepping
  keys still work).
* **Version picker:** columns drop in order: line counts, actor, time; version, kind and marks
  always show.
* Tests cover zero and tiny widths, CJK and emoji titles, resize while comparing, and agreement
  of painted variants with hit regions.

## Configuration

`[plugins.config."bcode.code_review".compare]`:

* `default_baseline`: `"last_looked"` (default), `"previous_push"`, a duration (`"24h"`), or
  `"none"`.
* `baseline_look`: `"engaged"` (default) or `"opened"`.
* `dwell_seconds`: seconds on a layer that count as `opened` (default 8).
* `start_view`: `"full"` (default) or `"changes"`.
* `rebased_layers`: `"collapse"` (default), `"hide"`, or `"show"`.
* `timeline`: `"auto"` (default) or `"always"`.

The existing history settings stay: version sources (forge, observation, reflogs), the push
window, and retention of observed versions.

## Implementation plan

Each step is independently committable with its tests, and leaves the app working.

1. **Looks and layer deltas (data layer).** `layer_looks` table and migration,
   `review.looks.record`/`list`, idempotent seeding from `reviewed_revisions` and viewer
   verdicts, refs for looked heads, baseline resolution (engaged rule, pinning inputs),
   deterministic replay commits, `LayerDelta` with per-file summaries, `review.compare.marks`,
   and the new cache kinds. `history::compare` returns deltas; existing callers adapt.
   *Tests:* synthetic restacked repositories: one layer edited and the rest rebased (deltas:
   changed, rebased only, unchanged); a fast-forward; a retarget; a conflicting replay (with the
   rebase); an unavailable baseline; marks for added, newly removed and removed-since lines;
   replay commit ids identical across runs; baseline rules (engaged beats opened, current head
   means unchanged, never looked means new); seeding is idempotent; store round-trip and pruning.
2. **Comparison overlay in the document.** `set_comparison`, `FileSince`, `DocumentNeed::Marks`,
   both views (changes base for hunk needs, unchanged-file collapse rows, dropped files,
   removed-since markers), change navigation preferring marked lines, anchor mapping for old-side
   anchors in the changes view, and the renderer's tint and gutter column in the shared prefix
   layout.
   *Tests:* document tests that switching views keeps `FileKey`s, viewed progress, thread
   placements on new-side lines and the cursor line; unchanged-and-viewed folds while
   unchanged-and-unviewed stays open; rendering tests at tiny widths, with CJK and emoji, wrapped
   lines, and hit regions of markers.
3. **In-place comparison on the review screen; retire the old paths.** Automatic baseline on
   open and on stack moves, look recording (viewed, comment, ask, pass, dwell), header control
   and banner, `b` and `shift+l` (with the alias), layer badges, the only-changed filter, and
   matrix actions acting in place. Remove `rereview.rs`, `since_last`, `OpenReview` and the app's
   `return_to`, and the carry-over.
   *Tests:* UI scenarios: reopening a restacked stack starts in the full view with the banner,
   badges read changed/rebased/unchanged, `shift+l` shows the interdiff and back with viewed
   marks and drafts intact, marking a file viewed records a look and a reopen shows nothing new,
   the filter skips rebased layers with `.`, matrix `enter` updates the document without opening
   a review, and header control variants at narrow widths.
4. **Arbitrary comparisons.** Version picker with looks, range-diff detail, push timeline strip,
   time presets, per-file version stepping with `alt+[`/`alt+]`, and historical *to* (read-only
   changes view).
   *Tests:* picking from/to in the picker and on the timeline updates only the intended layers;
   bucketed timeline ticks and their hit regions at small widths; file stepping overrides one
   file and is cleared by a layer change; historical views refuse comments and record no looks.
5. **Comments that follow their code.** Before-side placement of older forge threads and drafts
   in the changes view, removal markers holding threads whose lines were removed, and
   `diff_hunk` matching when the original commit is missing.
   *Tests:* a thread written on v2 shows on the before side in v2 → v3 and near its code in the
   full view; a thread on removed lines attaches to the marker; a draft from an older head is
   followed without rewriting its anchor; missing commits fall back to `diff_hunk`, then to the
   file.

`docs/code-review-app.md` (stack and history sections, keys, configuration) is updated with
each step that changes behavior it describes.

## Status

Built:

* The version log and its sources (forge timeline, observation, reflogs), the timeline and change
  kinds, the restack summary on the layer strip, the history matrix, and *this file through the
  stack* / *files several layers change*.
* **Step 1 (data layer).** The `layer_looks` table (`027_layer_looks_table`) with
  `review.looks.record` / `review.looks.list` (upsert per pull request, head and kind; at most
  `MAX_LOOK_HEADS_PER_PULL` heads, pruned heads reported so their refs can be released), seeding
  helpers for completed passes and forge verdicts (`history::looks::pass_looks`,
  `forge_review_looks`), baseline selection (`history::looks::last_looked`, `baseline_pair`),
  deterministic replay commits, layer deltas (`review.compare.layers`,
  `history::delta::layer_deltas`), line marks (`review.compare.marks`), keeping looked heads
  (`review.versions.keep`), and the `Replay`, `Delta` and `Marks` cache kinds.
* **Step 2 (document overlay).** `ReviewDocument::set_comparison` / `set_compare_view` /
  `set_compare_files_shown` / `toggle_removal` / `accept_marks`, `FileSince` on file headers,
  `DocumentNeed::Marks`, both views (changes-view hunks are requested from the replay commit),
  the per-source summary item (`CompareNote`), removed-since markers (`RemovedSince`),
  `NavKind::NewSince`, and the renderer's tint (`theme.since.diff`), gutter bar
  (`theme.icons.since_bar`), file badges, summary and marker rows with their click targets.

Not built yet: steps 3 to 5. The review screen does not create comparisons yet (nothing calls
`set_comparison` outside tests), the matrix still opens each comparison as its own review, and
`shift+l` still uses the separate "since last review" path. Also not built: moved-code
detection, checks per version, assistant summaries per push, and the restack summary on home.

### Implementation notes (where the build differs from the text above)

* **Dropped files** are not files of the current change, so they have no header rows: the
  source's summary counts them ("· 2 dropped") and `CompareNoteItem::dropped` lists their paths.
* **Hidden files** use one toggle per source (`set_compare_files_shown`): it shows the unchanged
  files of the changes view and the files of a layer that was only rebased, folded to their
  headers. The summary row carries the toggle and the view switch as buttons.
* **Side-by-side tint:** a row is tinted when either side is new since the baseline; the gutter
  bar is painted per side.
* **Marks** are requested only when the source's base commit is the delta's fork point (the
  old-side numbers of marks count from it); otherwise lines are not marked.
* **Old-side anchors in the changes view** show at their file (placement `File`) instead of
  being mapped onto the before side; mapping is part of step 5.
* **New-side anchors** whose recorded blob is the file's current new blob keep their line in
  either view regardless of the line's kind there (a line added in the full change is context in
  the changes view).
* **Removed-since markers** whose position is inside hidden context are placed before that gap.
* **`history::compare`** (comparisons as review sources) still serves the matrix until step 3
  removes it; its replay commits now use the same deterministic commit as the deltas.
* **`FileSteps`** (the cache kind for file header stepping) is added with step 4, which uses it.
