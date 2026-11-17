# Guided Review

A guided review answers "walk me through this review": a model writes a structured, cited
walkthrough of the change, which the review screen shows beside the diff and steps through. Each
claim points at exact lines at the revisions the review shows, so the reviewer can check it in
one keystroke, and the guide knows when a later push makes part of it stale.

A guide is a derived, versioned artifact of a review. It never replaces the diff, viewed state,
or comments: it reads them, links to them, and helps produce them.

## Principles

* **Cited, not narrated.** Every step and finding cites line ranges at pinned revisions. A
  citation is checked mechanically (source, path, range, quoted text) when it is written, and the
  model is told how to fix a citation that does not check out. Verification never drops a
  citation; an unverified one is kept and shown as unverified.
* **Expanding never reshapes the guide.** A conversation may only change what its grant allows.
  Asking about or expanding one section adds under that section; the outline changes only when
  the reviewer explicitly restructures, which writes a new version.
* **Versions are kept.** A guide is pinned to the heads it was written against. Refreshing after
  new pushes forks a new version; old versions stay readable, pinned to their push.
* **Plugin-owned data and tools.** Guide storage, citation checks, freshness and the guide tools
  live in the code review domain and its plugin, so every session (the review assistant, Ask AI
  sessions, child sessions, `bcode tui`) uses the same tools under Bcode policy. The review app
  owns only presentation and the host actions that need the live screen.
* **Bounded.** Guide documents, sections, citations, excerpts and tool outputs have fixed bounds;
  freshness and context reads are cached and bounded.

## Content

A guide has a fixed core and adaptive parts:

* **Core sections**, always present: *purpose*, *scope*, and *reading order* (the walkthrough
  steps, each a stop in the diff).
* **Adaptive sections**, included only with cited evidence: risks, behavior changes, API and
  contract changes, data and migrations, security, performance, concurrency, error handling,
  tests and coverage gaps, configuration and rollout, dependencies, UI, deleted code, follow-ups,
  open questions. Boilerplate such as "No risks found" is linted away.
* **Templates** add required sections (for example a `threat_model` section for security
  reviews); `explain` suits file-set and repository reviews with no diff; `freeform` requires
  only purpose.
* **Free-form** notes and expansions sit under any section.

User context comes from configuration (context files and directories, notes, the template) and
from ad-hoc directions in chat ("focus on the retry semantics; skip tests"). Directions are
stored with the version that used them, so refresh and regenerate reuse them.

## Data model

The types are plain data in `bcode_code_review_models` (`guide.rs`); validation, freshness and
planning live in `bcode_code_review::guide`.

### Guide and versions

* **`ReviewGuide`**: one per review and scope. It belongs to a review through `review_key` (the
  key AI exchanges and suggestions use), names its `scope`, an optional `parent_guide_id` (a
  layer guide under a stack guide), a title, its `current_version`, and timestamps. Archived
  guides are kept until deleted.
* **`GuideScope`**: `Review` (the whole review, a file set, or a whole-stack diff), `Stack`
  (the story across stepped layers), `Layer` (one pull request layer), or `Since` (changes since
  a comparison baseline, with its layer pairs).
* **`GuideVersion`**: one immutable-heads snapshot of the guide. It records
  `schema_version` (see [Compatibility](#compatibility)), the `heads` it is pinned to (per source:
  base, head, and label), what it was `based_on`, its `origin` (session, model, effort, strategy,
  depth, template, directions, and a digest of the context used), its `status` (planning,
  generating, synthesizing, verifying, complete, failed, cancelled, superseded), optional
  `generation` progress (areas and their child sessions, tokens used), the `outline` (a tree of
  sections), and the `citations` its sections share.
* **`Section`**: a stable id (`s3`, `s3.1`, never reused within a guide), a kind (purpose, scope,
  reading order, step, topic, findings, layer link, expansion, note), a title, a markdown body
  with citation tokens `[[c7]]`, findings, citation ids in reading order, child sections, a state
  (planned, written, or provisional from an expand or ask), and verifier checks.
* **`Finding`**: a risk, question, suggestion, or note, with an optional review thread severity,
  its citations, and the draft thread it became, when it did.
* **`Citation`**: an optional source id, path, side, line interval, the revision read, the blob
  identity (for working-tree and index heads that have no commit), a hash of the normalized quoted
  text, a bounded excerpt, its relation to the review (changed, hunk context, unchanged, or
  outside the review), and its verification (verified, possibly adjusted from another range;
  unverified; or a problem).
* **`ClaimCheck`**: a verifier's judgement of one claim against its citations (supported,
  unsupported, unclear), with a note. Checks never delete content.
* **`GuideArea`**: one fan-out child's result for one area of one version, written only by that
  child, so children never contend on the version row.
* **`GuideThread`**: an "ask" conversation pinned to a section. It does not bump the version.
* **`GuideSectionMark`**: a section the reviewer marked reviewed, with a fingerprint of its
  citations' quote hashes; a mismatch un-reviews it.
* **`GuideGrant`**: what one session may change (see [Grants](#grants)).
* **`Freshness`**: derived on read, never stored: fresh, moved (with its new lines), stale (with
  a reason), or gone.

### Bounds

Per version: 200 sections, 1,000 citations, and 1 MiB of document JSON. Per section body: 16 KiB.
Per citation excerpt: 2 KiB. Writes over a bound are rejected with the bound named. The store also
rejects structurally inconsistent documents: duplicate section ids, citation keys that differ from
their citation's id, and sections or findings that cite an unknown citation id.

### Storage

Guides live in the review store as typed Switchy tables; no SQL text is written by hand.

| Table | Key | Holds |
|---|---|---|
| `review_guides` | `guide_id` | review key, scope, parent, title, current version, row revision, timestamps, archive time |
| `review_guide_versions` | `guide_id`, `version` | schema version, heads, based-on, origin, status, generation, document (outline and citations), row revision, timestamps |
| `review_guide_areas` | `guide_id`, `version`, `area_id` | child session, status, document, row revision, timestamps |
| `review_guide_threads` | `thread_id` | guide, version, section, session, status, question, timestamps |
| `review_guide_marks` | `guide_id`, `section_id` | reviewed time, fingerprint |

* **Compare-and-swap writes.** Guide, version and area rows carry a revision that every write
  increments. A write names the revision it read and replaces the row only while it is still that
  revision. Writers that did not name a revision re-read and re-apply their change, a bounded
  number of times; a writer that named one gets a conflict with the current revision, which the
  guide write tool returns to the model so it can re-read and retry.
* **Forking** a version inserts the next version number (racing forks take the next free number)
  and moves the guide's `current_version` forward with the same compare-and-swap.
* **Deleting** a guide deletes its versions, areas, threads and marks.
* **Unsaved reviews** keep their guides under their transient review key; saving the review
  re-keys them, as AI exchanges are. Discarding an unsaved review loses its guide.

A guide is never stored inside the review workspace, so saving a review cannot clobber a guide
and guide writes never conflict with viewed marks.

### Compatibility

`GuideVersion.schema_version` is the compatibility boundary for the stored document. A version
whose schema version is not the current one is neither written nor read as a known form: writes
are rejected, and reads report it as unsupported rather than guessing.

## Citation checks

`guide::cite` checks one requested citation `{source | layer, path, side, start_line, end_line,
quote, symbol, relation_hint}` against the version's pinned heads:

1. **Source.** An explicit layer (`#102`) or source id; otherwise the sources whose diff contains
   the path; otherwise the repository context at the primary head. Problems:
   `ambiguous_source` (with candidates) and `unknown_layer` (with the layers).
2. **Revision.** The head for the new side and the base for the old side of the source's pinned
   heads.
3. **Path.** `path_not_found` with fuzzy suggestions; `renamed` (with the new path) when the diff
   renames it; `deleted_on_side` (use the old side) when the file does not exist on that side.
4. **Range.** `range_out_of_bounds` with the file's line count.
5. **Quote.** Text is normalized (trailing whitespace trimmed, CRLF folded; indentation-insensitive
   as a fallback). A match is verified. Otherwise the file is searched: a quote found exactly once
   elsewhere adjusts the citation to that range and is verified as adjusted; several exact hits
   are `quote_ambiguous` (with candidate ranges); a close match (line similarity at least 0.8) is
   `quote_mismatch` (with the best range and its similarity); nothing is `quote_not_found`, with
   the symbol's definition sites as suggestions when a symbol was given. Without a quote the
   citation is kept unverified with the hint to add a quote.
6. **Relation.** The range is intersected with the source's hunks: changed, hunk context, or
   unchanged; a file outside the diff is outside the review. A model that said the range is in the
   diff when no changed line is gets the `not_in_diff` warning; the citation stays, shown as
   unchanged.
7. **Store.** The revision, blob identity, quote hash, and excerpt.

A citation is never dropped. In strict mode a write with any problem is rejected as a whole,
which suits a model's self-check loop. Every problem has a machine-readable code, one actionable
sentence, and a structured suggestion.

The checks are written against a small read interface (sources, file indexes, hunks, file lines,
symbol search), so they are deterministic and tested without a repository; the review data layer
implements it.

## Freshness

`guide::freshness` compares a version's pinned heads with the review's current resolution. For
each citation:

* **Same blob** (or same commit and path): fresh.
* **Commit heads with fork points:** the layer's delta with the rebase taken out
  (`history::delta`) says which new-side lines are new since the pinned version. The citation is
  re-found at its new lines by its excerpt and quote hash (fresh when they did not move, moved
  otherwise); if a changed span overlaps those lines it is stale ("lines 14–20 changed"). A
  rebase-only push therefore keeps the guide fresh.
* **Other heads** (working tree, index, or no fork point): the excerpt is re-found in the new
  content (fresh or moved); if it is missing, gone.

A section is stale when any of its citations is stale or gone. Results are cached by old blob,
new blob, and range in the review cache, so repeated reads stay bounded.

## Tools

### Plugin tools (`code_review.*`)

Everything that reads or writes durable guide data, or repository content at pinned revisions, is
a code review plugin tool, so any session can use it under Bcode policy.

* `code_review.guide.start` pins the heads from the session's binding and creates the guide or a
  new version with status generating. An optional planned outline creates planned skeleton
  sections straight away. It returns the guide id, version, heads, the grant, the resolved
  context, and the template's required sections.
* `code_review.guide.write` applies a batch of operations atomically against one row revision:
  `set_outline`, `put_section`, `add_expansion`, `promote`, `discard`, `remove_section`,
  `move_section`, `record_checks`, `area_result`, `set_status`. Citations are inline objects
  (checked and assigned ids) or existing ids. It returns the new revision, sections written,
  per-citation status and problems, and lint (missing required sections, citation tokens without
  citations, sections without citations, boilerplate in adaptive sections).
* `code_review.guide.get` reads a guide (outline, bodies, citations, freshness, areas, threads,
  versions), bounded at 128 KiB with named cuts.
* `code_review.guide.check` checks citations or a section without writing anything.
* `code_review.guide.plan` returns size metrics, the recommended strategy and partition, cost
  estimate inputs, the template's sections, and the resolved context.
* `code_review.guide.export` renders markdown with numbered citations: pull request sources as
  permalinks, others as `path:12-40 @ <short sha>`, verification as text.
* General reads at the review's pinned revisions: `code_review.search_code` (`git grep` at the
  head; text, word, regex, or symbol mode; each hit tagged with its relation to the review),
  `code_review.diff_hunks` (only the hunks overlapping a range), `code_review.related_files`
  (tests, callers, configuration, docs, each with why it matched), and a `revision` argument on
  `code_review.file_content` limited to versions the stack timeline knows.

### Grants

Each write operation is checked against the session's `GuideGrant`, recorded in its review
binding:

| Grant | Allowed operations |
|---|---|
| `Expand{s}` | `add_expansion` under `s` |
| `Ask{s}` | `add_expansion` under `s`, and `promote` |
| `Refresh{ids}` | `put_section` on those sections |
| `Verify` | `record_checks` |
| `Area{a}` | `area_result` for `a` |
| `Generate` | everything on its own version |
| `Restructure` | everything, including moving and removing written sections |

A refused operation returns its index, the code `outside_grant`, and a sentence telling the model
what it may do instead.

### Permissions

* Guide writes (`code_review.guide.*`) change only a versioned, derived artifact. They are
  allowed by default like reads, under their own permission category `code_review.guide`, which
  users can set to ask.
* Fanning out to child sessions spends money: the `guide_fan_out` action asks unless allowed, or
  unless the reviewer started it from the panel.
* Publishing always asks.

### App client tools (`bcode.code-review.*`)

Only actions that need the live screen or the host's session APIs are app tools:
`guide_show` (open the panel and step through the guide exactly as the keys do) and, with
orchestration, `guide_fan_out`. `current_view` reports the active guide, version, current section,
and counts of reviewed, verified and stale sections.

## Orchestration

`guide::plan` is deterministic. With `strategy = "auto"`: small reviews (by changed lines, files
and layers) get one streamed agent; a stepped stack of three or more layers gets per-layer
children whose layer guides a stack guide links; anything else gets area children, clustered by
package root and top directory and balanced by changed lines. `depth` (brief, standard, deep)
adjusts sections, context reach, and whether a verifier pass runs.

* **Streaming.** The writer calls `start` with a planned outline, then one `write` per section.
  Each finished call reaches the review app through tool reflection, which reloads just that
  version, so sections fill in one at a time.
* **Fan-out** uses app-driven child sessions: each child is a real child of the assistant
  session, with the parent's model and effort, bound to the review with an `Area` grant, writing
  only its own area row. At most `max_parallel` run at once. When every area is terminal, the app
  asks the parent to synthesize, reusing the areas' citation ids. If the app closes mid-run, the
  version stays generating with its finished areas and the panel offers Resume. Moving fan-out
  onto workflow task groups is a later step.
* **Cancellation** is end to end: stopping cancels the parent and every child turn, marks the
  version cancelled, and keeps what was written.
* **Cost** is estimated before fan-out from diff size, depth, children and catalog prices; above
  `confirm_above_tokens` the reviewer confirms, and `budget_tokens` is a soft cap.
* **Deep mode** runs a verifier child that records claim checks; unsupported claims go back to the
  writer under a `Refresh` grant.

## Presentation

* **Guide panel** placement follows the body width: beside the document on wide screens (stacked
  above threads when both are open), under the document on narrow ones, and as a Guide tab of the
  assistant panel when the body has no room. It degrades to titles only, then to a header chip.
* **Glyphs stay distinct.** Reviewed sections use the viewed pair (`✓` / `○`). A verified
  citation shows `◈`; an unverified one a dimmed `◇`; a citation problem `⚠`. `✓` keeps meaning
  reviewed or approved.
* **Inline markers.** The first row of each cited range carries a trailing badge such as `[3]`,
  styled by verification; the current section's ranges are tinted. The badge is a trailing
  adornment that BMUX's diff-row layout measures and places, so geometry, painting and hit regions
  share one layout definition. `guide.markers` is `current`, `all`, or `off`.
* **Context citations** in a diff file's gap reveal the gap; a file outside the review opens a
  read-only peek labelled as outside the review.
* **Keys.** `alt+g` toggles the guide; `alt+down` / `alt+up` step sections; `alt+u` jumps to the
  next unreviewed section; `'` shows the section citing the cursor line; `backspace` jumps back.
  The `code-review-guide` focus scope adds movement, folding, citation stepping, reviewed, expand,
  ask, draft comment, promote, discard, refresh, restructure, copy, and versions.
* **Assistant card.** A turn's guide tool calls collapse into one live card bound to the guide,
  reading the same state as the panel. In `bcode tui` they stay ordinary tool lines.

## Review actions

* **Reviewed** marks a section with its citation fingerprint. Files whose changed hunks are all
  covered by reviewed sections become viewed through the ordinary viewed path
  (`guide.mark_files_viewed`), and a section counts as reviewed when all its cited in-diff files
  are viewed.
* **Finding to comment** opens the comment composer on the finding's first in-diff citation,
  prefilled with its text, kind and severity, and links the draft thread back.
* **Expand** and **ask** add provisional content under one section; **promote** makes it written;
  **discard** hides it. **Restructure** is the only path that grants restructuring, and writes a
  new version.

## Stacks and comparisons

* A **stack guide** tells what each layer contributes and links per-layer guides; stepping layers
  follows the layer's guide (`guide.follow_layer`).
* A **whole-stack diff** has one review guide whose citations name their layer.
* **File-set and repository reviews** use the `explain` template and the relations in review and
  outside review.
* A **since-you-last-looked** guide uses the comparison baseline and the layer deltas with the
  rebase taken out, and links the sections of an older guide for the old heads.
* **After new pushes**, refreshing stale sections forks a new version with fresh and moved
  sections copied and remapped, under a `Refresh` grant for the stale ones.

## Configuration

```toml
[plugins.config."bcode.code_review".guide]
strategy = "auto"              # auto | single | fan_out
depth = "standard"             # brief | standard | deep
single_max_files = 40
single_max_lines = 1500
max_children = 8
max_parallel = 4
confirm_above_tokens = 250000
budget_tokens = 600000
verify = "deep"                # never | deep | always
markers = "current"            # current | all | off
mark_files_viewed = "covered"  # covered | cited | never
follow_layer = true
context_files = [".bcode/review-guide.md", "docs/architecture.md"]
context_dirs = ["docs/adr/"]
context_at = "worktree"        # worktree | head
max_context_bytes = 65536
notes = "We care most about backwards compatibility of the wire format."
template = "default"           # default | explain | freeform | <name>
required_sections = ["purpose", "scope", "reading_order"]

[plugins.config."bcode.code_review".guide.templates.security]
extends = "default"
required_sections = ["purpose", "scope", "reading_order", "threat_model"]
sections = [{ key = "threat_model", title = "Threat model",
              prompt = "Who can reach the changed code; what trust boundaries move.",
              when = ["auth", "crypto", "**/permissions/**"] }]
```

Configuration layers as usual (global, then project). Context files and directories are confined
to the repository and read as `Read` operations, so path rules apply; invalid entries are reported
and defaults used.

## Export

A guide can be copied or exported as markdown with numbered citations. Writing a file goes
through the markdown file publisher with an output path inside the repository, so write-path rules
apply.

## Status

| Phase | Scope | Built |
|---|---|---|
| P1 | Models, store, citation checks, freshness | No |
| P2 | Plugin tools, grants in the binding, configuration and context | No |
| P3 | Read-only guide panel, diff markers, navigation | No |
| P4 | Single-agent generation from the assistant, guide card, tab fallback | No |
| P5 | Reviewed and viewed, finding to comment, expand, ask, promote, discard, restructure | No |
| P6 | Freshness in the UI, refresh forks, versions picker, since guides | No |
| P7 | Fan-out child sessions, synthesis, cost confirmation, Resume, verifier | No |
| P8 | Templates and lint, export to file | No |
