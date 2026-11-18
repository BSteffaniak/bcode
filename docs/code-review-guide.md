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
  sections), the `citations` its sections share, and the ids of sections removed from it
  (`retired_sections`, never used again in it).
* **`Section`**: a stable id (`s3`, `s3.1`, never reused within its version), a kind (purpose, scope,
  reading order, step, topic, findings, layer link, expansion, note), a title, a markdown body
  with citation tokens `[[c7]]`, findings, citation ids in reading order, child sections, a state
  (planned, written, or provisional from an expand or ask), and verifier checks.
* **`Finding`**: a risk, question, suggestion, or note, with an optional review thread severity
  (info, nit, warning, blocker), its citations, and the draft thread it became, when it did.
* **`Citation`**: an optional source id, path, side, line interval, the revision read, the blob
  identity (for working-tree and index heads that have no commit), a hash of the normalized quoted
  text, a bounded excerpt, its relation to the review (changed, hunk context, unchanged, in the
  review without a diff, or outside the review), and its verification (verified, possibly adjusted from another range;
  unverified; or a problem).
* **`ClaimCheck`**: a verifier's judgement of one claim against its citations (supported,
  unsupported, unclear), with a note. Checks never delete content.
* **`GuideArea`**: one fan-out child's result for one area of one version, written only by that
  child, so children never contend on the version row.
* **`GuideThread`**: an "ask" conversation pinned to a section. It does not bump the version.
* **`GuideSectionMark`**: a section the reviewer marked reviewed, with a fingerprint of its
  citations' quote hashes; a mismatch un-reviews it.
* **`GuideGrant`**: what one session may change in one guide (see [Grants](#grants)), on a named
  version or the guide's current one.
* **`Freshness`**: derived on read, never stored: fresh, moved (with its new lines), stale (with
  a reason), or gone.

### Bounds

Per version or area: 200 sections, 1,000 citations, and 1 MiB of document JSON. Per section body:
16 KiB. Per citation excerpt: 2 KiB. Titles: 1 KiB; finding text and claim checks: 4 KiB; thread
questions: 16 KiB. Writes over a bound are rejected with the bound named. The store also rejects
structurally inconsistent documents: empty or duplicate section and finding ids, citation keys
that differ from their citation's id, and sections, findings, or checks that cite an unknown
citation id, and sections that use an id retired from the version. The write tool retires the
ids of the sections it removes (with their children).

### Storage

Guides live in the review store as typed Switchy tables; no SQL text is written by hand.

| Table | Key | Holds |
|---|---|---|
| `review_guides` | `guide_id` | review key, scope, parent, title, current version, row revision, timestamps, archive time |
| `review_guide_versions` | `guide_id`, `version` | schema version, heads, based-on, origin, status and its message, generation, document (outline and citations), row revision, timestamps |
| `review_guide_areas` | `guide_id`, `version`, `area_id` | child session, status and its message, document, row revision, timestamps |
| `review_guide_threads` | `thread_id` | guide, version, section, session, status, question, timestamps |
| `review_guide_marks` | `guide_id`, `section_id` | reviewed time, fingerprint |

* **Compare-and-swap writes.** Guide, version and area rows carry a revision that every write
  increments. A write names the revision it read and replaces the row only while it is still that
  revision, and otherwise fails with `StoreError::RevisionConflict` carrying the current revision.
  `ReviewStore::update_guide_version` applies a change in one turn at the database: without a
  named revision a lost race re-reads and re-applies the change, a bounded number of times; with
  one, the conflict is returned, which the guide write tool passes to the model so it can re-read
  and retry. A change that refuses (for example an operation outside the session's grant) writes
  nothing.
* **Creating** a guide writes the guide row and its first version together; creating one that
  exists is a conflict, never a replacement. Areas are likewise created once and then replaced by
  revision.
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
are rejected and reads fail with `StoreError::Guide` naming the version found, rather than
guessing. Listing versions reads no documents, so it still shows such a version with its schema
version. Statuses this build does not know are likewise reported, not mapped to known ones.

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
4. **Range.** `invalid_range` for an empty or reversed range; `range_out_of_bounds` with the
   file's line count.
5. **Quote.** Text is normalized (trailing whitespace trimmed, CRLF folded, blank lines around the
   quote dropped; indentation-insensitive as a fallback). A quote found within the cited lines
   verifies the citation, so a range may be wider than its quote. Otherwise the file is searched: a quote found exactly once
   elsewhere adjusts the citation to that range and is verified as adjusted; several exact hits
   are `quote_ambiguous` (with candidate ranges); a close match (line similarity at least 0.8) is
   `quote_mismatch` (with the best range and its similarity); nothing is `quote_not_found`, with
   the symbol's definition sites as suggestions when a symbol was given. Without a quote the
   citation is kept unverified with the hint to add a quote. A quote of more than 200 lines is
   not matched at all (part of it proves nothing about the rest): it is `quote_too_long`.
6. **Relation.** The range is intersected with the source's hunks: changed, hunk context, or
   unchanged; a file outside the diff is outside the review. A model that said the range is in the
   diff when no changed line is gets the `not_in_diff` warning; the citation stays, shown as
   unchanged.
7. **Store.** The revision, blob identity, quote hash, and excerpt.

A citation is never dropped. In strict mode a write with any problem is rejected as a whole,
which suits a model's self-check loop. Every problem has a machine-readable code, one actionable
sentence, and a structured suggestion.

The checks are written against a small read interface, `CitationReader` (sources, diff files,
hunks, file lines, path suggestions, symbol sites), so they are deterministic and tested without a
repository. `ReviewDataReader` implements it over the review data layer, reading each listing and
file once per batch; its symbol sites are the symbol's definitions found by the code search at
the cited revision (at most five). Files over 200,000 lines, binary files, and
read errors are reported as `unreadable`.

## Freshness

`guide::freshness` compares a version's pinned heads with the review's current resolution. For
each citation:

* **Same commit** (and path): fresh.
* **Source gone:** a citation whose source is no longer in the review is gone. Repository
  context (no source id) follows the primary source.
* **New-side citations of commit heads with fork points:** the layer's delta with the rebase taken
  out (`history::delta`: unchanged, rebased only, or changed with per-file line marks) says which
  current lines are new since the pinned version. The citation is re-found by its excerpt and
  quote hash, nearest its old lines first (fresh when they did not move, moved otherwise); if a
  changed span overlaps those lines, or the text is not found, it is stale ("lines 14–20 changed
  since the guide was written"). A rebase-only push therefore keeps the guide fresh.
* **Everything else** (old-side citations, working tree or index heads, no fork point, or a delta
  that could not take the rebase out): the excerpt is re-found in the current content (fresh or
  moved); if it or the file is missing, gone.

A section is stale when any citation it or its findings use is stale or gone. Answers whose
revisions are all commits are cached in the review cache (`CacheKind::Freshness`, keyed by the
pinned and current commits, range and quote hash), so repeated reads stay bounded.

## Tools

### Plugin tools (`code_review.*`)

Everything that reads or writes durable guide data, or repository content at pinned revisions, is
a code review plugin tool, so any session can use it under Bcode policy. Each acts on the review
the session is bound to, or the saved review named by `review`. The operations live in
`bcode_code_review::assistant::guide` and `::code`; the plugin adapts arguments and declares
what each call reads.

`code_review.guide.start` (guide write)
* Arguments: `guide_id`, `scope` (`{kind: review | stack | layer | since, layer, baseline}`),
  `mode` (`new` | `new_version` | `resume`), `version` (to resume, or to base a new version on),
  `copy` (a new version starts with a copy of its base's sections), `title`, `depth`,
  `strategy` (`single` | `per_layer` | `areas`, recorded with the version), `template`,
  `directions`, `outline` (planned sections: `{id, kind, key, title, children}`).
* `new` creates a guide of the scope, or a new version when the scope already has one;
  `new_version` forks the next version and makes it current; `resume` sets a version back to
  generating and makes the calling session its writer.
* Pins the heads the review's sources resolve to now (the first part of a source that resolves
  to several), records the origin (session, depth, strategy, template, directions, context
  digest), and plans the outline with `set_outline`.
* Returns `{message, guide_id, title, version, revision, status, based_on, heads, grant,
  outline, template: {name, required_sections, sections, adaptive_sections}, context, next}`.

`code_review.guide.write` (guide write)
* Arguments: `guide_id`, `version` (default: the version the session's grant names, else the
  current one), `expected_revision`, `strict`, and `ops` (1 to 50), applied in order, all or
  none:
  * `set_outline {sections}`: the whole tree. New ids become planned sections; existing
    sections keep their content (a planned section's kind and title may change). Sections left
    out are removed and their ids retired.
  * `put_section {id, parent, after, kind, key, title, body, citations, findings}`: creates a
    section (kind and title required) or changes the fields given; a planned section becomes
    written. `kind` is `purpose`, `scope`, `reading_order`, `step`, `findings`, `note`, `topic`
    with `key`, or a catalog or template key such as `risks`.
  * `add_expansion {under, id, title, body, citations, findings}`: a provisional expansion
    (`<under>.x<n>` by default), from an ask when the grant is `Ask`.
  * `promote {section}`, `discard {section}` (provisional sections only), `remove_section {id}`,
    `move_section {id, parent, after}`.
  * `record_checks {section, checks: [{claim, status, note, citations}]}`: replaces the
    section's claim checks, recorded with the calling session.
  * `area_result {area_id, sections, status, message}`: writes one fan-out area's row; sent
    alone.
  * `set_status {status: generating | complete | failed | cancelled, message}`.
* Citations are inline objects `{id, source | layer, path, side, start_line, end_line, quote,
  symbol, relation_hint}` (checked at the version's pinned heads and stored under the id
  wanted when it is free or names the same lines, else the next free `c<n>`) or existing ids.
  Finding citations may also be positions (from 1) in the operation's `citations`. Body tokens
  are `[[c7]]` or `[[2]]` (the operation's second citation, rewritten to its id). Citations
  nothing uses any more are dropped.
* Returns `{written: true, guide_id, version, revision, status, sections_written, citations:
  [{id, op_index, status, path, side, lines, relation, problems, warnings}], lint}`. A refusal
  is an error result with `written: false` and either `errors: [{op_index, code, message}]`
  (`outside_grant`, `invalid_op`), or `code`: `citation_problems` (strict, with every check),
  `conflict` (with `current_revision`), or `invalid_document` (a bound or structure the store
  refused). Nothing is written on a refusal.
* Lint: `missing_required` (template sections), `planned_unwritten` (once complete),
  `unknown_citation_token`, `uncited_section` (written steps, topics, findings sections and
  expansions), `boilerplate` (an adaptive section that says only that there is nothing to say).

`code_review.guide.get` (read)
* Arguments: `guide_id` (default the review's only unarchived guide; with none or several, the
  review's guides are listed), `version`, `sections` (bodies of only these), `include`
  (`outline`, `bodies`, `citations` by default; also `freshness`, `areas`, `threads`,
  `versions`, `marks`).
* Fits 120 KiB of JSON: the outline is always whole; section bodies and then citations that do
  not fit are left out and named in `cut`, as are large areas, threads, versions or marks.

`code_review.guide.check` (read)
* Arguments: `guide_id`, `version`, `citations` (at most 100), `section` (as `put_section`
  would write it). Checks against the guide's pinned heads (the review's current heads when it
  has no guide); a section is applied to a scratch copy and linted. Nothing is written.

`code_review.guide.plan` (read)
* Arguments: `strategy` (`auto` | `single` | `fan_out`), `depth`, `template`.
* Returns `{plan: {strategy, reason, depth, verifier, metrics, areas, max_parallel, estimate,
  guidance}, template, context}` (see [Orchestration](#orchestration)).

`code_review.guide.export` (read)
* Arguments: `guide_id`, `version`, `format` (`markdown` only), `sections`,
  `include_unverified` (default true), `links` (`permalink` | `relative`).
* Returns markdown (see [Export](#export)).

General reads at the review's pinned revisions:
* `code_review.search_code {query, mode: text | word | regex | symbol, paths, side, source, max,
  ignore_case}`: `git grep` at the head (or base) of a source (the newest head by default; the
  working tree or index for those heads), stopped past `max` (100, at most 200). A symbol search
  finds definitions with per-language patterns, then other uses as a word. Each hit carries
  `in_review` and `relation`.
* `code_review.diff_hunks {path, source, side, start_line, end_line, context}`: the hunks
  overlapping the range, numbered.
* `code_review.related_files {path, kinds: [tests, callers, config, docs], source, max}`: at the
  head of the file's source, each with why it matched.
* `code_review.file_content` takes `revision`: `head`, `base`, or a commit id (at least seven hex
  digits) among the review's sources, stack layers, recorded pull request versions, and guide
  heads.

### Grants

Each write operation is checked against the session's `GuideGrant`, recorded in its review
binding (`guide_grant`, set by the app). A grant names a guide and a version (the guide's current
one when absent) and applies only to that version. Without a grant for the guide, a session may
write, as `Generate`, only the versions whose origin is that session; `guide.start` with
`resume` makes a session a version's writer. Starting or resuming versions of a guide is refused
when the session's grant for it is narrower than `Generate`. A session holding a grant writes
only the granted guide: writing, resuming or starting versions of any other guide, and starting
a new guide, are refused. The grant applies however the review is reached, including a saved
review named by `review`. A grant naming a version lets its session resume only that version
and start no new one.

Without a grant, `resume` takes over a version only when it is the session's own, its writing
failed or was stopped, or the conversation writing it has not written for ten minutes (its
writer is taken to have gone). Another conversation's complete or superseded version is not
reopened; a new version is started instead. These checks run in the same store turn as the
takeover.

| Grant | Allowed operations |
|---|---|
| `Expand{s}` | `add_expansion` under `s` |
| `Ask{s}` | `add_expansion` under `s`, and `promote` |
| `Refresh{ids}` | `put_section` on those sections |
| `Verify` | `record_checks` |
| `Area{a}` | `area_result` for `a` |
| `Generate` | everything on its own version, except moving or removing written sections (and leaving them out of `set_outline`) |
| `Restructure` | everything, including moving and removing written sections |

A refused operation returns its index, the code `outside_grant`, and a sentence telling the model
what it may do instead.

An inline citation that names a stored citation id at the same lines replaces the stored
citation only for a `Generate` or `Restructure` writer (or an area's writer, in its area). Any
narrower writer leaves the stored citation as it is, since other sections cite it, and its own
citation is stored under a free id unless it says the same (text, revision, relation and
outcome), in which case the id is shared.

### Permissions

* Guide writes (`code_review.guide.start`, `code_review.guide.write`) change only a versioned,
  derived artifact. They are allowed by default like reads (a read-only policy operation that
  does not require permission), under their own permission category `code_review.guide`; an
  exact-name rule such as `"code_review.guide.write" = "ask"` asks. The `build` agent is offered
  them; the read-only `plan` agent is not.
* In the review assistant, guide writes are their own tool category, `guides` (chip `§ guides`),
  on by default; turning review writes off for a conversation leaves them.
* Every guide call declares what it reads, so read rules apply: `guide.write` and `guide.check`
  the files their citations name, `guide.start` and `guide.plan` the configured context files
  (an invocation reads only the context files its preparation declared), `search_code` its
  path globs or the repository.
* Fanning out to child sessions spends money: the `guide_fan_out` action asks unless allowed, or
  unless the reviewer started it from the panel.
* Publishing always asks.

### App client tools (`bcode.code-review.*`)

Only actions that need the live screen or the host's session APIs are app tools:
`guide_show` (open the panel and step through the guide exactly as the keys do) and
`guide_fan_out` (write the guide with child agents; see [Fan-out from the app](#fan-out-from-the-app)). `current_view` reports the active guide, version, current section,
and counts of reviewed, verified and stale sections.

## Orchestration

`guide::plan` is deterministic. With `strategy = "auto"`: a review within `single_max_files`,
`single_max_lines` and two layers gets one streamed agent; a stack stepped through three or more
layers gets per-layer children whose layer guides a stack guide links; anything else gets area
children. Areas cluster the changed files by package root (the nearest directory holding a
`Cargo.toml`, `package.json` or `pyproject.toml` at the newest head; the repository root does not
count) or else by top directory, go up to three directory levels deeper when everything falls in
one cluster, and pack the clusters into at most `max_children` areas, largest first into the
lightest. A forced `single` is one agent; a forced `fan_out` is per layer for a stack of two or
more stepped layers, else areas. `depth` (brief, standard, deep) adjusts what the guide covers
(given as `guidance`), and `verify` decides the verifier (`deep`: only for deep guides).

The estimate is made from the changed lines (60 bytes each), the context bytes, four bytes per
token, a depth factor (1, 1.5, 2.5), 20,000 tokens for the writer and 30,000 per child; a fan-out
whose estimate passes `confirm_above_tokens` needs confirmation.

* **Streaming.** The writer calls `start` with a planned outline, then one `write` per section.
  Each finished call reaches the review app through tool reflection, which reloads just that
  version, so sections fill in one at a time.
* **Fan-out** uses app-driven child sessions (below). Moving fan-out onto workflow task groups is
  a later step.
* **Cancellation** is end to end: stopping cancels the parent and every child turn, marks the
  version cancelled, and keeps what was written.
* **Cost** is estimated before fan-out from diff size, depth, children and catalog prices; above
  `confirm_above_tokens` the reviewer confirms, and `budget_tokens` is a soft cap.
* **Deep mode** runs a verifier child that records claim checks; unsupported claims go back to the
  writer under a `Refresh` grant.

### Fan-out from the app

* **Starting.** `f` in the guide panel (or "Guide: walk me through this review with child agents"
  in the palette and the AI menu) is a fan-out the reviewer asked for: it is not a tool call, so it
  does not prompt, and it fans out even where one writer would do. The assistant starts one with
  `bcode.code-review.guide_fan_out` (`strategy` auto, `per_layer` or `areas`; `depth`,
  `directions`, `title`), which is mutating, so Bcode's permission rules ask unless allowed; with
  `auto` a review the plan gives one writer is refused with the reason, for the assistant to
  write directly. The children are children of the assistant's conversation, so it must have one.
* **Confirming.** The app plans the review from the conversation's binding. When the estimate
  passes `confirm_above_tokens`, the panel (and a waiting tool call's answer) shows
  "Fan out: 5 agents · ~350k tok · ≈$1.05 on <model>", the cost from the model's catalog input
  price; `f` or `enter` starts it and `esc` drops it.
* **Versions.** The conversation starts the version (strategy `areas` or `per_layer`) with the
  core outline. By layer, the version is a stack guide's and each layer gets its own layer guide
  (`parent_guide_id` the stack guide). The areas are recorded queued in the version's fan-out
  progress (`generation`: each area's paths or layer, child session, status, tokens and message;
  the budget, `max_parallel`, whether a verifier runs, and a note), which is what survives the app.
* **Children.** One at a time, up to `max_parallel` running, the app creates a child of the
  conversation (`ReviewHost::create_child_session`, IPC `CreateChildSession`), gives it the
  conversation's model and effort, binds it to the review with `Area {area_id}` on the version
  (by layer, `Generate` on its layer guide), and submits its prompt under the
  `bcode.code-review.guide-writer` profile. Children are followed like AI cards: their tokens
  show per area, and a child's permission request shows in the panel and the guide card with the
  feed keys (`y`/`a`/`n`/`d`), which answer it from the guide panel. When a child's turn ends, its
  area is ready when its area row (by layer, its layer guide) is written, else failed.
* **Citations.** An area writer's citation ids carry the area's prefix (`crates-net-1f0c4a2e.c1`:
  the area id's readable part, bounded, then a hash of the whole id), so the ids of every area
  are distinct even for areas whose ids read alike; a version write naming an area citation by id (in a citation
  list, a finding, or a body token) copies it into the version as it was checked.
* **Synthesis.** When every area is done the version is marked synthesizing and the conversation
  is asked, with a `Generate` grant for that message, to write purpose, scope and one reading
  order across the areas, citing the areas' citations by id (by layer: the stack guide across the
  layer guides; the app adds a Layers section of `LayerLink` sections, one per written layer
  guide, and `enter` on one shows that layer's guide). When the conversation's turn ends the
  version is complete.
* **Budget.** Children's tokens add up in the progress; past `budget_tokens`, queued areas stop
  with a note and the guide is synthesized from what exists, the note telling the writer so.
* **Verifier.** When a verifier runs (`verify`), a child under `bcode.code-review.guide-verifier`
  bound with `Verify` records claim checks. Checks render under the section ("Checked claims":
  the verified glyph for supported, the problem glyph for unsupported, the dimmed unverified
  glyph for unclear, with the note) and never replace its content. Sections with unsupported
  claims go back to the conversation with a `Refresh` grant for them.
* **Stopping and resuming.** Stopping the conversation's turn, `shift+x` in the guide panel or
  "Guide: stop the child agents writing the guide" cancels the conversation and every child and
  marks the version cancelled; written areas stay. A child whose creation was still under way
  when the fan-out stopped is not recorded running, and its turn is cancelled once it exists. A
  fan-out version that is not complete and that this app is not running (stopped, or the app
  closed) shows "4/5 areas ready · f Resume": the turns of children still recorded running are
  cancelled first, so no area has two writers, then areas not ready are queued again for new
  children, and once every area is ready the conversation is asked to synthesize. Resume waits
  until the fan-out's earlier work (such as a stop) has finished.
* **Layers.** While stepping through a stack's layers (`guide.follow_layer`), the panel shows the
  layer's own guide when it has one, and the stack guide when stepping to a layer without one.

## Presentation

The review app reads the review's guide once the review is open: the review's guides, the one
shown (the newest whole-review guide), its current version, its reviewed sections, and its
citations' freshness against the revisions on screen. The screen only reads guides; they are
written through the plugin tools.

* **Guide panel** (`alt+g`). Beside the document when the body is at least 100 columns (at least
  36 wide; the file panel gives way first and is left out when both would leave the document too
  little); stacked above the threads panel in the right column when that is open too; under the
  document (at least 8 rows) on narrower bodies with at least 16 rows; otherwise not shown, and
  while it is open the header shows a `Guide 3/9` pill instead (dropped from a narrow header
  just before the viewed count).
  Visibility and the sizes dragged are kept with the review's presentation state
  (`guide_panel_visible`, `guide_right_share`, `guide_bottom_share`).
  * Beside the document, the outline (numbers, titles, citation badges, and a state glyph) sits
    above the current section; the outline scrolls to keep the current section in view. The
    rule between them is the divider of a BMUX vertical panel group: dragging it resizes the
    two (the outline at most its rows, the section at least three), two fifths for the outline
    until dragged.
  * Under the document, the current section shows alone; its header says where it is (`3.1/9`).
  * Narrower than 24 columns, only the outline's numbers and titles show.
  * The current section shows its body as rendered markdown (citation tokens as badges, written
    as the badges' text so they are measured and wrapped with the prose), its citations (path, lines,
    and how they relate to the review: changed, context, unchanged, or outside the review), and
    its findings. A banner says when the version is being written, failed, was stopped, is not
    the guide's current version, or has sections whose code changed since.
  * The pane's title names the guide and version; its status counts reviewed sections,
    verified citations, and stale sections.
  * Every row is composed from styled spans and fitted with BMUX text primitives; one geometry
    drives painting, hit regions, and clicks, and only painted rows register hits. Clicking an
    outline row or a citation shows it in the diff; the header's `◂` / `▸` step sections.
* **Glyphs stay distinct.** Reviewed sections use the viewed pair (`✓` / `○`). A verified
  citation shows `◈`; an unverified one a dimmed `◇`; a citation problem or stale code `⚠`. `✓`
  keeps meaning reviewed or approved, and the guide never shows the approval mark. ASCII icons
  use `#`, `?` and `!`.
* **Inline markers.** The first row of each run of a cited range the document shows carries a
  trailing badge such as `[3]◈`, styled by verification. The badge is a BMUX diff-row trailing
  adornment that reserves its cells (`DiffTrailingFit::Reserve`), so measurement, painting and
  the badge's click target come from one layout; a row too narrow for it omits it. Clicking a
  badge shows the section citing it. The current section's lines get the `guide.focus` tint in
  their gutter. `guide.markers` (`current`, `all`, or `off`) chooses which citations get badges;
  the current section's tint shows whatever it says. Nothing is marked until the guide is used
  (its panel opened or a guide key pressed).
* **Navigation.** A guide jump remembers where the cursor was (64 deep) and moves to the cited
  line. A range hidden in a gap of a loaded diff file is revealed first; a file still loading is
  moved to once it loads; a file the review does not show (or a citation outside the review)
  opens a read-only **peek** in the document's place, labelled as outside the review, which
  `esc` or `backspace` closes.
* **Keys** (`code-review`): `alt+g` opens, focuses, or closes the panel; `alt+down` / `alt+up`
  step through the current section's citations, then to the next (previous) section; `alt+u`
  goes to the next section not reviewed; `'` shows the section citing the line at the cursor;
  `backspace` jumps back. The `code-review-guide` focus scope moves (`j` / `k`), shows the
  section and its first citation (`enter`), folds (`space`), steps citations (`]` / `[`), and
  copies the section as markdown (`y`). Its review action keys are
  listed under [Review actions](#review-actions); refresh (`r`, `alt+r`) and versions
  (`shift+v`) under [Lifecycle and comparisons](#lifecycle-and-comparisons).
* **Theme roles.** `guide.focus` (the gutter tint), `guide.badge`, `guide.unverified`,
  `guide.context` (unchanged and outside-review labels), and `guide.problem` derive from the
  review theme's tiers.

### Writing a guide from the assistant

The review assistant writes a guide with one agent (see
[the assistant's Guides](code-review-assistant.md#guides)):

* **Asking.** "Walk me through this review" (the palette's `Guide: walk me through this
  review`, the AI menu, or `enter` on the empty guide panel) sends that message to the
  assistant's conversation; the variant with directions puts it in the assistant's input for the
  reviewer to add what to focus on or skip, which the assistant passes to `start.directions`.
* **Streaming.** The assistant plans, starts the guide with its outline, then writes one section
  per `guide.write`. Each finished call of its session reaches the review through tool
  reflection, which reads that guide again: the skeleton (planned sections as `◌`) appears at
  `start`, and sections fill in as their writes land. A start opens the guide panel where the
  body has room for it.
* **Card.** The conversation shows the turn's guide calls as one live card read from the same
  guide state as the panel; a click on a section drives the diff there.
* **Guide tab.** Where the body has no room for the panel, the assistant panel's title row has
  `Chat` and `Guide` tabs and the Guide tab shows the guide, with its keys and clicks.
* **`guide_show`** opens the guide and moves through it as the keys do; `current_view` reports
  the guide. A call naming a guide not on screen is answered once the review's guides are read
  again (a guide just started may not be yet): with what it showed, or that the review has no
  such guide.
* **Stop** cancels the turn and marks every version of the review's guides that the session is
  still writing cancelled, on screen or not, with revision-checked writes that re-check the
  writer each attempt (a version another conversation resumed meanwhile is left to it), keeping
  what was written; the panel's banner says `stopped · N of M sections written`.

### Lifecycle and comparisons

`bcode_code_review::guide::lifecycle` holds the domain side (refresh forks, the heads a version
is pinned to, since guides and their delta check); the review screen drives it
(`surface/guide_lifecycle.rs`).

* **Stale.** A current version with stale sections shows the banner `N sections stale since
  #102 1a2b3c4 · r refresh · alt+r refresh stale · V versions` (the version's first pinned
  head; the keys as bound), stale outline rows end in `⚠ stale`, and the guide card counts
  them (`⚠ 2 stale`).
* **Refresh.** In the guide panel, `r` refreshes the current section and `alt+r` every stale
  one (`Guide: refresh stale sections` in the palette). When the version is pinned to other
  heads than the review's now, the app forks the next version (`based_on` the shown one,
  pinned to the heads now, complete): every section is copied, citations that are fresh or
  moved are remapped to the current revision and lines, stale and gone ones are kept for the
  rewrite, and sections that stayed fresh record `refreshed_from`. The fork becomes current
  and the review assistant's session is granted `Refresh{ids}` on it; a later refresh while
  that version is still pinned to the heads now edits it in place, adding to the granted
  ids. The assistant is then asked to rewrite just those sections with `put_section`. A
  version being written, or an older version, is not refreshed.
* **Grants from the app.** The review records the grant it gives the assistant's session in
  that session's binding (`guide_grant`); a changed grant binds the session again before its
  next message. Asking for a new walkthrough drops the grant, so the session writes the
  versions it starts. Because the binding is the session's, a message that binds another
  grant (a guide action's, or an ordinary message after one) waits while the conversation has a
  turn running or a sent message whose turn has not ended, and is sent once they end, so a
  running turn keeps the grant it was sent with. The fan-out's synthesis counts as done only
  when its own message's turn ends. The grant also ends once the guide moves past it: when another version
  of the guide becomes current, or when no section a `Refresh` names is stale any more.
* **Versions.** `V` (`Guide: versions…`) lists the guide's versions, newest first, with
  status, the version each was forked from, and when it last changed. Picking an older one
  shows it read-only with the banner `v1 · pinned to #102 1a2b3c4 · read-only · current is v2
  · V versions`; picking the current one (or a guide write landing) shows the current one
  again. `guide_show` takes `version` and shows that version the same way.
* **Cards.** Each turn's guide card names the version that turn wrote, read from the `version`
  its guide writes answered with (else the version a write named). A turn of another version
  than the one on screen shows a compact card for that version without sections.
* **Since guides.** "Guide these changes" (the first row of the Compare menu while a pull
  request is compared, `enter` on the empty guide panel then, or `Guide: walk me through what
  changed since I last looked` in the palette) creates a guide of scope `Since` with the
  comparison's baseline and its current (not historical) layer pairs, pinned to the heads now
  and planning, grants the assistant's session `Generate` on its version 1, and asks it to
  write it without starting another. A guide whose version is pinned to a head the baseline
  compares from (bounded: 8 guides, 8 versions each) is named in the request, so the
  walkthrough opens with what changed in its sections. Writes to a since guide refuse
  (`outside_delta`, nothing written) citations that are not about what changed: new-side
  lines that are not new since the baseline with the rebase taken out (the refusal names the
  new lines of the file), sources not compared, unchanged or rebased-only layers, and,
  where the new lines cannot be told, citations of unchanged code. The check compares from the
  baseline to the heads the written version is pinned to, so a since guide refreshed after
  another push is checked at its new heads.

## Review actions

Keys of the `code-review-guide` scope; an older version is read-only and says so.

* **Reviewed** (`v`) marks the current section with its citations' fingerprint (pressed again,
  it un-reviews it). The files it marks viewed go through the ordinary viewed path, so they are
  saved with the review and sync with home and other clients; `guide.mark_files_viewed` chooses
  them (`guide::coverage`): `covered` (default) marks a cited in-diff file once every added and
  removed line of its diff falls in ranges the reviewed sections cite on that side, `cited`
  marks every in-diff file a reviewed section cites, `never` none. A section also counts as
  reviewed when every in-diff file it cites is viewed. Unless `mark_files_viewed` is `never`,
  un-viewing a file (from anywhere) clears the marks of the sections citing it, and
  un-reviewing a section reviewed only through its viewed files un-views them. Under `never`
  the guide leaves viewed files alone both ways: un-viewing keeps the sections' marks, and a
  section reviewed only through its viewed files stays reviewed until they are un-viewed. A mark whose fingerprint no longer matches (the cited code
  changed) does not count.
* **Next unreviewed** (`n` in the panel, `alt+u` anywhere) steps to the next written section
  not reviewed and shows its first citation.
* **Finding to comment** (`c`) opens the comment composer on the current section's first
  finding not drafted yet, at its first in-diff citation (at the review otherwise), prefilled
  with its text, its kind (risk and suggestion as a finding thread, question, note) and
  severity. Once the draft is saved, its thread is recorded on the finding
  (`drafted_thread_id`), which shows `drafted → thread`.
* **Expand** (`e`) sends "Expand §n …" to the assistant's conversation with the section (its
  markdown and citations) as a `guide section` context block. For that message the conversation
  is bound with an `Expand` grant on the section of the version shown; the next ordinary message
  binds it without a grant again. What it writes is a provisional subsection under the section;
  nothing else in the guide may change.
* **Ask** (`a`) opens the question composer. The question goes to a session of its own titled
  `Guide: §n …`, bound to the review with an `Ask` grant on the section, and is recorded as a
  guide thread (question, section, session). Follow-ups about the same section go to the same
  session. The section shows each open thread's question and its answer (the matching turn of
  the session, followed live), and the threads are read again with the guide, so they stay
  after reopening.
* **Promote** (`p`) makes the current provisional section written (the app applies it through
  the store, revision-checked). On a section with an open answer, it marks the thread promoted
  and asks its session to write the answer as a cited expansion and promote it.
* **Discard** (`x`) removes the current provisional section (its id is retired), or hides the
  section's latest open answer. Neither calls a model.
* **Restructure** (`shift+r`) asks for directions in the composer, forks the next version as a
  copy of the version shown (`based_on` it, generating, with the directions in its origin),
  makes it current, and sends the directions to the assistant bound with a `Restructure` grant
  on that new version. The old version is kept unchanged. It is the only path that grants
  restructuring.

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

Configuration layers as usual (global, then project). Each entry is read on its own
(`guide::config`): an invalid one, or an unknown key, is reported and its default used, so one
typo never disables guides. Bounds: 64 context files and directories, 64 context files read in
all (a directory's files as the working tree lists them, tracked and untracked, not ignored),
1 MiB of context content, 16 KiB of notes, 64 templates of 32 sections each.

Context files and directories are confined to the repository (relative paths, no `..`, and a
symbolic link must not lead outside it) and declared as reads, so path rules apply. Files are read
from the working tree, or at the review's newest head with `context_at = "head"`, up to
`max_context_bytes`; files past the bound are listed with their sizes only. The context's SHA-256
digest (files, notes, and template) is stored in the version's origin.

Templates resolve through `extends` (at most eight deep, without loops) to a built-in one:
`default` (purpose, scope, reading order required), `explain` (the default for a review without a
diff; adds architecture, entry points, invariants, risks), and `freeform` (purpose required). The
configuration's `required_sections` apply to the default template.

## Export

A guide can be copied or exported as markdown (`code_review.guide.export`): a `# title · vN`
heading, the pinned heads, numbered sections (planned ones say they are not written yet;
provisional ones are marked), findings as bullets, and citations numbered by first use and listed
at the end with side, relation and verification as text. A citation of a GitHub pull request,
stack layer or stack source at a commit links to its permalink; repository context links through
the review's only forge repository when it has one; others render as `path:12-40 @ <short sha>`
(or name the working tree or index). Without `include_unverified`, unverified citations and
their tokens are left out. Writing a file goes through the markdown file publisher with an output
path inside the repository, so write-path rules apply.

On the review screen:

* **Copy.** `y` in the guide panel copies the current section and the palette's `Guide: Copy as
  markdown` the whole version, both with permalinks for GitHub pull request, stack layer and
  stack sources.
* **Export to file.** `Guide: Export to file` writes the same markdown to
  `.bcode/guides/<guide>-v<N>.md` through the built-in `markdown_file` publisher, given the
  document as its `document` option. With a document the publisher writes only inside the
  repository: the output path must be relative, without `..`, and must not lead outside through
  a symbolic link; the status line says where it went.
* **Templates.** `Guide: walk me through this review, with a template…` (also in the AI menu)
  lists the built-in and configured templates with the sections each requires; choosing one puts
  "Walk me through this review using the `<name>` template. Directions: " in the assistant's
  input, and the assistant passes the template to `plan` and `start`. Lint reports a template's
  missing required sections with every write; `freeform` is complete with purpose alone. A guide
  written with `explain` (the default for file-set and repository reviews) cites lines as in
  review or outside review, and the panel's banner says `explaining files (no diff)`.
* **Opening the guide.** Clicking the header's `Guide 3/9` pill opens the guide: its panel where
  the body has room, else the assistant's Guide tab. Starting a guide (or its first section
  arriving) where the panel has no room switches the assistant to its Guide tab; the Chat tab
  keeps its dot while the conversation needs the reviewer.
* **Outline split.** The dragged split between the outline and the current section is kept with
  the review's presentation state (`guide_outline_share`, a share of the rows the two share) and
  restored when the review opens again.

## Status

| Phase | Scope | Built |
|---|---|---|
| P1 | Models, store, citation checks, freshness | Yes |
| P2 | Plugin tools, grants in the review assistant binding, configuration and context | Yes |
| P3 | Read-only guide panel (markdown bodies, draggable outline split, header pill), diff markers, navigation | Yes |
| P4 | Single-agent generation from the assistant, guide card, tab fallback, stop | Yes |
| P5 | Reviewed and viewed, finding to comment, expand, ask, promote, discard, restructure | Yes |
| P6 | Freshness in the UI, refresh forks, versions picker, since guides | Yes |
| P7 | Fan-out child sessions, synthesis, cost confirmation, Resume, verifier | Yes |
| P8 | Templates and lint, `explain` and `freeform`, copy and export to file as markdown | Yes |

The app sets grants: expand (`Expand`) and restructure (`Restructure`) bind the assistant's
conversation for one message, ask sessions are bound with `Ask`, and refresh (`Refresh`) and
since guides (`Generate`) are recorded on the review's binding until a new walkthrough. Other
sessions have no grant and write the versions they started or resumed. Fan-out children are
bound with `Area` (by layer, `Generate` on their layer guide) and the verifier with `Verify`; the
conversation synthesizes under `Generate` for one message. Guide freshness is told in
the panel (badges, tint and moved lines follow the review's current sources, and the peek of a
moved citation reads the current revision), and stale banners, refresh forks and the versions
picker are described under [Lifecycle and comparisons](#lifecycle-and-comparisons).
