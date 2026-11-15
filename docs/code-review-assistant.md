# Code review assistant

The review app (`bcode review`, `bcode-review`) has one assistant for the whole app: a docked
conversation with a Bcode session that is available on the reviews home, in the new-review
wizard and in every review, and that stays open as the user moves between them. It can see
exactly what the user is looking at, act on any review, and do in the app what the user can do,
under Bcode's permission rules. This document specifies the panel, its entry point, the
conversation history, the context every message carries, the tools, the agent profile and its
permission categories, how earlier per-review conversations carry over, and the order to build
it in.

Related: [code review app](code-review-app.md) (the screens, keys and review tools this builds
on), [embedded sessions](embedded-sessions.md) (session origin, app-provided tools with claims,
app-contributed agent profiles, turn context blocks), [permissions](permissions.md),
[review stack history](review-stack-history.md) (comparisons), [TUI rendering](tui-rendering.md).

## Status

* **Built.** Every build step below is built, with the deviations listed under
  [As built](#as-built).
* **Built: conversation plumbing and history model (build step 1).** `ReviewHost` submits turns
  with context blocks and an agent, lists the app's sessions, reads summaries, renames, deletes,
  counts turns and lists pending permissions; the origin's `layer` entry; the binding's
  `started` and `detached_at_ms`; `review.assistant.list` and `review.assistant.unbind`; the
  history merge.
* **Built: app-wide panel, Ask AI button and history picker (build step 2).** As specified, with
  the deviations listed under [As built](#as-built).
* **Built: context blocks, `current_view`, profile and categories (build step 3).** Every
  message carries the stored view block and the request-only detail block rendered from one
  snapshot of the screen; a message whose request-only context the daemon no longer holds at
  admission is sent again once with a fresh snapshot. The app registers
  `bcode.code-review.current_view` and the profile `bcode.code-review.assistant` (built from the
  category configuration) on its own provider connection, reconnecting after Bcode restarts;
  creates new conversations' sessions on that connection and claims a session before each of
  its turns; names the profile on every turn and holds messages while the connection is not
  registered (fail closed).
* **Built: plugin data tools (build step 4).** `create_review` (every kind, and the new
  `working_tree_paths` source kind), `update_review`, `delete_review`, the `mark_viewed`
  selectors, `add_comments`, `resolve_threads` and the extended `list_reviews`; every review tool
  works on any saved review by id. Every writer of a saved review writes only what it changed
  (field-level merge over the stored row, checked against the row's revision), so nothing undoes
  another writer's change or brings back a deleted review.
* **Built: app action tools and acceptance (build step 5).** `find_pull_requests`, `go_to`
  (with `version`), `set_comparison` (every baseline, including `pushes` and `stack_at`),
  `set_view`, `open_review` (including the prefilled `new_review`), `refresh`,
  `complete_pass`, `pull_request_review` and `publish`; the profile instructions carry the
  recipes. Only the panel's active conversation may use them. The acceptance requests were run
  live against a debug daemon with the fake model provider (see [As built](#as-built)).
* **Built and relied on.** Session origin, client tools with claims, contributed agent profiles
  and turn context blocks ([embedded sessions](embedded-sessions.md#what-is-built)); the review
  app's per-review dock (`surface/dock.rs`), AI cards (`agent.rs`), the `code_review.*` plugin
  tools and the `review_assistant_sessions` binding table.
* **Built: parity with bdata.** Model, effort and permission chips, the tools list, remembered
  choices, Ask AI card model selection, failures and usage, and conversation management, as
  designed under [Parity with bdata](#parity-with-bdata), with the deviations listed under
  [Parity as built](#parity-as-built).
* **Superseded by this design.** The per-review dock: one conversation per review, reachable
  only inside a review, with no visible entry point and a context line appended to the message
  text.

## Model

* **One panel per app.** The panel belongs to `ReviewApp`, not to a route. Opening, focusing,
  closing and its size are app state; moving between home, the wizard and reviews never closes
  it or changes its conversation.
* **Conversations are Bcode sessions.** Each is a canonical session created by the app with
  origin `bcode.code-review`, listable in `bcode tui` and `bcode session list --origin
  bcode.code-review`, and continued there or in the app. The app keeps no conversation store of
  its own.
* **The active conversation follows the user.** When the user opens another review, goes home,
  or changes layer, file or comparison, the conversation stays; the next message's context
  describes the new place and the session's attachment (which review the review tools default
  to) moves with it. Conversations remember where they started (their tag) and never change it.
* **Tags.** A conversation is tagged with the review it started in and the stack layer that was
  current then, or with none (started on home or in the wizard). Ask AI card conversations are
  tagged with their review and labelled by origin (a card's question, or an AI command).

## Panel

### Placement

The app splits its whole area between the route and the panel; each route renders unchanged in
the rectangle it is given (its own header, body and footer), so no route lays the panel out.
The decision is made per frame from the app's area, in this order:

1. **Right dock.** At least 100 columns: the panel takes the right column from the top row to
   the bottom row. Its width is the user's width (default 40% of the area, 36..=100 columns),
   clamped so the route keeps 60 columns. The divider on its left edge drags.
2. **Bottom dock.** Fewer columns and at least 16 rows: the panel takes the bottom rows across
   the whole width (default 40%, at least 8 rows), clamped so the route keeps 8 rows. The divider
   on its top edge drags.
3. **Zoomed.** Smaller than both: the focused panel takes the whole area; an unfocused open
   panel is hidden (it stays open, and the Ask AI button shows that it is). `esc` returns to the
   route.

The width and height chosen by dragging are app state for the run; `assistant.width` and
`assistant.height` in configuration set the defaults. The review body loses its own assistant
pane: `panels.rs` keeps the file and threads panels only.

Right dock (at least 100 columns):

```
┌ Code reviews · bcode ──── 3 needs attention  [✦ Ask AI] ┐┌ ✦ Assistant · Retry backoff #103 ┐
│ suggestions                                             ││ ⌖ src/retry.rs:42   + new  ⟲  ↗  │
│ ┌ reviews ────────────────────────────────────────────┐ ││ ▍you why did the cap move?       │
│ │ ...                                                 │ ││ ✓ code_review.file_diff          │
│ │                                                     │ ││ The cap moved in #102 because …  │
│ └─────────────────────────────────────────────────────┘ ││ ┌──────────────────────────────┐ │
│ ctrl+k Ask AI  n new  / filter  ? help                  ││ │ Ask about this review        │ │
└─────────────────────────────────────────────────────────┘└──────────────────────────────────┘
```

Bottom dock (fewer than 100 columns):

```
┌ Retry backoff  +120 −40   [✦ AI] ┐
│ document                          │
│                                   │
├ ✦ Assistant · Retry backoff ⟲ ↗ ──┤
│ ▍you why did the cap move?        │
│ ┌───────────────────────────────┐ │
│ │ Ask about this review         │ │
└───────────────────────────────────┘
```

### States

| State | Shows |
| --- | --- |
| Closed | Nothing; the Ask AI button is in the route's header. |
| Open, empty conversation | The title row, the chips row, a short line naming what it can do on this screen ("Ask about this review, or ask me to mark files viewed, change the comparison, …"; on home: "Ask me to create or open a review, …"), and the input. |
| Open, conversation | Turns as AI cards show them (`▍you`, reasoning, tool lines, answers), a spinner while working, tokens and cost on the title row. |
| Needs you | A permission prompt replaces the input (`y` allow, `a` always, `n` decline, `d` never; Always is not offered for publishing or deleting). |
| Not connected | Why (Bcode not found, the daemon failed to start) and what to do; the input stays usable and a send retries. |
| History open | The history picker replaces the conversation area (below). |

The title row names the active conversation: `✦ Assistant · <title> · <tag>` (the tag is the
review title and `#layer`, or `home`), cut from the right; then `+ new`, `⟲` (history) and `↗`
(open in `bcode tui`) as buttons, like bdata's. The chips row shows the context chip (`⌖ src/retry.rs:42`
on a review, `⌖ home` on home; `ctrl+x` detaches it). The chip is computed from the route on
screen every frame, so it never names a place the user has left.

Below 24 columns or 6 rows the panel shows its title row and input only; below 12 columns the
title row only. Hit regions are registered only for what was painted, from the same layout
plan.

### Ask AI button

The entry point is a header button, styled as bdata's `✦ ask AI` header button within the
review app's theme (`theme.accent` text on `theme.header_bar`, a filled pill while the panel is
open), at the right end of the header on home, in the wizard and in reviews (after the publish
split button). Its glyph is a new icon `icons.assistant` (`✦`; `*` in the ASCII icon set).

```
home:    Code reviews · bcode                 3 needs attention · 2 in progress   ✦ Ask AI
review:  Retry backoff  github #101 #102 #103  +120 −40  viewed 3/9  [Publish ▾]  ✦ Ask AI
narrow:  Retry backoff  +120 −40  [Publish]  ✦ AI
tiny:    Retry b…  ✦
open:    …                                                                         [✦ Ask AI]
working: …                                                                         ✦ Ask AI ⠋
needs:   …                                                                         ✦ Ask AI ?
```

* The button is part of the header plan: it is measured by its own text width and placed by
  `plan_header` (home gets the same planner), and it is the last part to degrade: `✦ Ask AI`,
  then `✦ AI`, then `✦`, before the title is cut below its kept width. The badge (`⠋` spinner or
  `?`) shows while the panel is closed or zoomed away and the conversation works or needs the
  user.
* A click does what `ctrl+k` does. Keyboard: `ctrl+k` on every route; the palette entry
  "Ask AI" on home (home gains the command palette, `:` and `ctrl+p`) and in reviews; the footer
  hint `ctrl+k Ask AI` on home, in the wizard and in reviews.
* `ctrl+k` opens and focuses the panel, focuses it when it is open, and closes it when it is
  focused. A focused text field other than the panel's (the home filter, wizard fields, the
  comment composer) keeps `ctrl+k` for editing; the button and the palette still work there.

### Keys

New scope `code-review-assistant-history` for the picker; the rest extend existing scopes. None
collides with an existing default in its scope (`code-review-home` has no `ctrl+k`, `:` or
`ctrl+p`; `code-review-wizard` has no `ctrl+k`; the text input's `alt` bindings are
`alt+b`/`alt+f`/arrows/backspace/delete only).

| Scope | Keys | Action |
| --- | --- | --- |
| `code-review-home` | `ctrl+k` | Open, focus or close the assistant (`home_assistant`) |
| `code-review-home` | `:`, `ctrl+p` | Command palette (`home_palette`) |
| `code-review-wizard` | `ctrl+k` | Open, focus or close the assistant (`wizard_assistant`) |
| `code-review` | `ctrl+k` | As today |
| `code-review-assistant` | `alt+h` | Conversation history (`assistant_history`) |
| `code-review-assistant` | `ctrl+n`, `alt+n` | New conversation (`assistant_new`) |
| `code-review-assistant` | `alt+r` | Rename this conversation (`assistant_rename`) |
| `code-review-assistant` | `alt+d` | Delete this conversation, after a confirmation (`assistant_delete`) |
| `code-review-assistant` | `ctrl+k`, `esc`, `ctrl+c`, `ctrl+x`, `ctrl+o`, `pageup`, `pagedown` | As today; `esc` returns to the route, whichever it is |
| `code-review-assistant` | `alt+m`, `shift+tab`, `alt+p`, `alt+t` | Model, effort, permissions, tools (see [Parity with bdata](#keys-1)) |
| `code-review-assistant-history` | `up`, `down`, typing | Move, search |
| `code-review-assistant-history` | `enter` | Switch to the conversation |
| `code-review-assistant-history` | `alt+r` | Rename |
| `code-review-assistant-history` | `alt+d` | Mark for deletion; again deletes |
| `code-review-assistant-history` | `ctrl+o` | Open in `bcode tui` |
| `code-review-assistant-history` | `esc` | Back to the conversation |

## History

### Picker

```
┌ ✦ Conversations ─────────────────────────────────────────── 14 ┐
│ ⌕ retry                                                        │
│ ● Why the retry cap moved  Retry backoff #103  2m  6  needs you │
│   Explain the change       Retry backoff #101  1h  2  card      │
│   Create review for stack  home                1d  4            │
│   Review assistant: Auth…  Auth cleanup        3d  9            │
│ enter switch  alt+r rename  alt+d delete  ctrl+o bcode  esc     │
└─────────────────────────────────────────────────────────────────┘
```
* **Rows** (newest first by last activity): `●` on the active conversation, title, tag (review
  title and `#layer`, `home`, or `deleted review`), relative time, turns, and needs-you
  (`needs you` while a permission request waits, `working` while a turn runs). Card
  conversations add their origin label (`card <path:line>`, `AI command`). Columns drop from
  the right as the panel narrows (origin, turns, time, tag); the title is cut last.
* **Search** matches title, tag, review id, layer and origin label, with the palette's matcher.
* **Switch** makes the conversation active, follows it, and attaches it to the review on screen
  (or detaches it on home). **New** starts an empty conversation; its session is created on the
  first message, with the tag of the screen it is sent from.
* **Rename** is Bcode's rename (explicit name), so `bcode tui` shows it too.
* **Delete** is Bcode's delete: in the picker `alt+d` marks the row and a second `alt+d` (or the
  row's `✕` twice) deletes; from the conversation, `alt+d` and the palette ask in a confirmation
  dialog. A running turn is stopped first. The session's binding row is removed from the review
  store. Deleting the active conversation leaves an empty new one.
* **Open in Bcode** runs `bcode tui <session>` as `o` does on a card, and the app resumes after.

### Listing and where tags live

* **Source of rows.** One bounded Bcode catalog page of sessions with origin app id
  `bcode.code-review` in the app's repository (at most 200; a cursor loads more on scroll),
  merged by session id with the review store's binding rows for the repository (legacy sessions
  created before origins were recorded are found only there; their summaries are read with
  `session_summary`, at most 200 per load). Sessions that the binding table names but Bcode no
  longer has are left out of the list (nothing is deleted on read). A degraded catalog page is
  shown with a note.
* **Turns and needs-you** load for visible rows only, like home's stats: turns from one bounded
  history page (counted user messages; `20+` beyond the page), needs-you from one
  `list_permissions` call grouped by session, plus the live state of followed sessions.
* **The tag** (where a conversation started) is immutable and read in this order:
  1. The session origin's context: `purpose` (`assistant`, `question`), `workspace` (the review's
     annotation scope id) and the new `layer` entry (`#<number>` of the current stack layer, when
     any). Written once at creation; absent `workspace` means started on home or in the wizard.
  2. The binding's new `started` field (below), for sessions whose origin lacks the workspace.
  3. The binding's `workspace`, for rows written before `started` existed.
  Review titles come from the repository's review list; a workspace id it no longer has shows as
  `deleted review`.
* **The attachment** (which review the `code_review.*` tools default to) is the binding row,
  rewritten by every sender before it sends: the panel before each message and on every route
  change while the conversation is active, and an AI card before each question. On home and in
  the wizard the panel marks the binding detached, so a review tool called without `review`
  fails with "no review is on screen; pass `review`" instead of using the last one.
* **Binding changes** (`ReviewAssistantBinding`, schema version unchanged, fields optional and
  omitted when unset):
  * `started: Option<ReviewAssistantTag>` (workspace id, title, layer number). The store keeps
    the first value ever saved for the session; when an existing row has none, it records the
    existing row's workspace as `started` before replacing the row, so retagging is impossible.
  * `detached_at_ms: Option<u64>`: set while the session is attached to no review. The plugin's
    `ReviewRef::Session` lookup treats a detached binding as unbound.
  * Store operations `review.assistant.list` (bindings of a repository, newest first, bounded)
    and `review.assistant.unbind` (remove one session's row, used by delete).

## Context

### Turn context blocks

Every message from the panel is submitted with two turn context blocks (Bcode's `TurnContext`;
the message text is exactly what the user typed). Both are rendered from one `ViewContext`
snapshot taken on the UI thread at the moment of sending, from the route on screen only; nothing
is cached across routes, so no field can survive from a previous screen.

| Block | Label | Kept | Bound | Holds |
| --- | --- | --- | --- | --- |
| View | `review app view` | durable (stored with the message, read by later turns) | 4,096 bytes | Where the user is: screen, review, layer, file, cursor, comparison summary, counts, and where to read more. |
| Detail | `review app detail` | request-only (this turn only) | 11,264 bytes | Bulky and fast-changing content: selection text, threads on the file, the focused thread, the per-layer comparison table, changed-since files, pull request state. |

* **Why durable versus request-only.** The view block is small and answers "where was the user
  when they said this", which later turns need to read the history correctly after the user
  moved. The detail block holds code and comment text that is large, stale by the next turn and
  readable again by tools; keeping it request-only keeps the durable history small and makes no
  source excerpt durable implicitly.
* **Bounds.** The two blocks and their labels stay within Bcode's 16 KiB per message, which
  Bcode enforces by rejecting (never truncating); the renderer cuts each section to its own
  budget first, then the block to its total, and every cut says so: `… 7 more layers (code_review.summary)`.
  Lines are single-line `key: value` text; strings are quoted and control characters escaped.
* **Detached context.** `ctrl+x` detaches the chip: the detail block is left out and the view
  block shrinks to the `screen`, `repo` and `review` lines.
* **Daemon restart.** A request-only block the daemon no longer holds fails the turn with
  `request_only_context_unavailable`; the panel resubmits once with a fresh snapshot.
* **Movement during a turn.** Context describes the screen at send time. A running turn reads
  the live screen with `bcode.code-review.current_view`; the attachment moves at once, so review
  tools called without `review` follow the screen.
* **Cards.** Ask AI cards keep their own prompts (see [AI cards](code-review-app.md#ai-cards)).

### View block schema

Lines appear in this order; lines that do not apply are omitted (never printed empty), and a
value that is not known yet says so (`computing`, `not loaded`).

```text
screen: review                      # home | wizard | review | opening
repo: bcode  /Users/me/src/bcode
review: id=rv_7f3c  title="Retry backoff"  kind=stack_step_through  saved=yes
  # kind: stack_step_through | whole_stack | pull_request | branch | current_branch | range
  #       | commits | commit | working_tree | staged | unstaged | files | mixed
sources: 5  #101 feat/retry-a@3e1c2a9  #102 feat/retry-b@77ab012  #103 feat/retry-c@91c0d4e  +2 (code_review.summary)
stack: layers=#101,#102,#103,#104,#105  current=#103 (3/5)  view=one_pr   # one_pr | all_prs
comparison: baseline=last_looked  view=changes  only_changed=yes  marks=both+words  seen=show  removed_inline=no
  # baseline: off | last_looked | previous_push | ago=24h | between=2026-11-12T10:00..now
  #           | pushes=3..5 | versions=#103:v2..v4 | stack_at=2026-11-10T09:00
layers: #101 unchanged · #102 changed 3 files +12 −4 · #103 rebased_only · #104 new_to_you · #105 computing
counts: changed=1 rebased=1 unchanged=1 new_to_you=1 pending=1  hidden_layers=2  hidden_files=14
file: src/retry.rs  status=modified  viewed=no  since=changed +3 −1  version=v2→v4 of 5
cursor: line 42 new  selection=40-48 new (9 lines, text in detail)
threads: file open=2 resolved=1  focused=gh:BSteffaniak/bcode#103:PRRT_x (pull_request, 3 comments)
pull_request: #103 open  approvals=1  changes_requested=0  ci=failing 2/14  read 3m ago
more: bcode.code-review.current_view, code_review.summary, code_review.list_files, code_review.file_diff, code_review.list_threads
```

Home and the wizard use the same schema with their own lines:

```text
screen: home
repo: bcode  /Users/me/src/bcode
branch: feat/retry-c  stack=#101..#105 (graphite)  uncommitted=3 files
reviews: needs_attention=3 in_progress=2 published=7 archived=4  selected=rv_7f3c "Retry backoff"
filter: "retry"
suggestions: stack #101..#105 · pull_request #210 (review requested) · working_tree 3 files  (bcode.code-review.find_pull_requests)
more: bcode.code-review.current_view, code_review.list_reviews

screen: wizard
repo: bcode  /Users/me/src/bcode
wizard: step=stack  kind=stack_step_through  editing=rv_7f3c  sources=#101..#105
```

Section budgets inside the 4,096 bytes: `sources` 8 entries, `stack` 32 layer numbers, `layers`
12 layers, `suggestions` 5; the `more` line is always kept.

### Detail block schema

```text
selection: src/retry.rs 40-48 new
  ```rust
  …the selected lines, at most 120 lines / 4 KiB, cut in the middle with a marker…
  ```
threads_on_file: (at most 12; then "… N more (code_review.list_threads path=src/retry.rs)")
  - id=… origin=local kind=issue severity=major resolved=no line=44 "first 160 bytes…"
focused_thread: id=…  origin=pull_request  outdated=no  2 KiB of the first and latest comments
layer_deltas: (at most 32 layers)  #102 changed: src/retry.rs +3 −1, src/lib.rs new +9 −0 …
changed_since: (current layer, at most 40 files) src/retry.rs changed +3 −1 …
hidden_files: 14 unchanged since the baseline: first 20 paths …
pull_request_state: reviews (latest per reviewer), failing checks (names), as last read
```

Budgets: selection 4 KiB, threads 2 KiB, focused thread 2 KiB, deltas 2 KiB, the rest share the
remainder; sections are dropped from the end (pull request state first, selection last) when the
total would be exceeded, each with a marker.

### `bcode.code-review.current_view`

A read-only client tool that returns the same `ViewContext` as JSON, live at call time, with
`as_of_ms` and a `screen_generation` that changes on every route, review, layer, file or
comparison change. Input: `sections` (any of `screen`, `review`, `stack`, `comparison`,
`file`, `selection`, `threads`, `pull_request`, `home`, `wizard`; default all) and `max_files`.
Bounds: 64 KiB of JSON; lists carry `total` and `omitted`, and selection text up to 16 KiB. It
reads only what the app already holds (no forge calls); fields not loaded say so.

## Tools

### Where each tool runs

* **Plugin tools** (`code_review.*`, served by the code review plugin in the daemon) for data
  operations: they work in any session, with the app closed, from `bcode tui` and in AI cards.
  Every review tool takes `review` (a review id from `code_review.list_reviews`) and defaults to
  the session's attached review.
* **Client tools** (`bcode.code-review.*`, registered by the app on its own provider connection,
  provider id = origin app id) for the screen: what is on it, moving the user, and actions that
  need the app's live state or its in-process review-source providers (forge pull requests and
  stacks; plugin services cannot call each other, so only the client can combine them). The
  app's sessions are claimed by creating them with the app's origin on that connection, and
  claimed again when a conversation from history is switched to or the connection is replaced.
  Without the app, these calls fail with `client_tool_unavailable` and the plugin tools still
  work.

Each tool's policy class (fixed at registration for client tools) and category decide its
permission; see [Permissions](#permissions).

### Plugin tools

Existing tools keep their behavior; `review` now defaults to the attached review (detached:
error).

| Tool | Category | Class | What it does | Why plugin |
| --- | --- | --- | --- | --- |
| `code_review.list_reviews` (extended) | review reads | read | Reviews of the repository with id, title, kind, sources, state group, archived, updated time; `include_archived`, `query`. | Store data. |
| `code_review.summary`, `list_files`, `file_diff`, `file_content`, `list_threads`, `get_thread`, `pull_request` | review reads | read | As documented in the app, on any review. | Store and git data. |
| `code_review.create_review` | review writes | mutating | Creates a saved review from sources: `pull_request` (number or URL), `stack` (top number: whole stack), `stack_layers` (numbers: step-through), `branch` (base, head), `current_branch`, `range` (base, head, merge_base), `commits` (revs), `commit`, `working_tree` / `staged` / `unstaged` with optional `paths` (repository-relative directories or files) and `directory` (any directory inside a repository: the review is created in that repository). Returns the id, title, sources and the repository. Pull request kinds record the provider and repository (from the `origin` remote when omitted); the app resolves them on open. | A store write that must work without the app. |
| `code_review.update_review` | review writes | mutating | Renames, archives or restores one review, or many (`reviews`). | Store write. |
| `code_review.delete_review` | review writes | mutating | Deletes reviews; the panel never offers Always. | Store write. |
| `code_review.mark_viewed` (extended) | review writes | mutating | Marks files viewed or unviewed by `paths`, `glob`, `layer` (number or source id), `all`, or `unchanged_since_look` (every file of the selected layers that is unchanged since the reviewer last looked). Returns marked, already viewed and not found. | Store write; the comparison of looks is computed by the data layer. |
| `code_review.add_comments` | review writes | mutating | Several draft comments in one call (each like `add_comment`). | Batch of a store write. |
| `code_review.resolve_threads` | review writes | mutating | Resolves or reopens several local threads. | Batch of a store write. |
| `code_review.add_comment`, `reply`, `resolve_thread`, `suggest_change` | review writes | mutating | As documented. | Store writes. |
| `code_review.publish` | publish | mutating | Local publishers, as documented. | Writes files the plugin can confine. |
| `code_review.navigate` | navigation | read | As documented; kept for cards and `bcode tui`. Left out of the assistant profile, which uses `go_to`. | Works without a live client. |

The working-tree kinds with `paths` are a new source kind `working_tree_paths` (`paths`,
`staged`, `unstaged`), not a field on existing kinds, so an older build preserves it as an
unsupported source instead of silently reviewing the whole repository.

### Client tools

| Tool | Category | Class | What it does | Why client |
| --- | --- | --- | --- | --- |
| `bcode.code-review.current_view` | review reads | read-only | The live screen (above). | Only the app knows the screen. |
| `bcode.code-review.find_pull_requests` | review reads | read-only | The providers' pull requests and stacks: `mine` (authored), `review_requested`, `current_branch` (its stack), ordered by last update, each with its layers in order and a ready `create_review` source. Bounded to 50. | Needs the in-process providers and discovery. |
| `bcode.code-review.go_to` | navigation | read-only | Moves within the review on screen: `layer`, `path`, `line`, `side`, `thread`, `version`. Fails naming `open_review` when the target is another review. Returns what is now on screen. | Screen state. |
| `bcode.code-review.set_comparison` | navigation | read-only | Any Compare menu choice: `baseline` (`off`, `last_looked`, `previous_push`, `ago` with a duration, `between` with times, `pushes` with timeline indices, `versions` for one pull request), `view` (`full`, `changes`), `only_changed`, `mark_style`, `word_marks`, `seen_hunks`, `removed_inline`, `all_layers` (one pull request or all). Returns the comparison as `current_view` reports it once computed (or `computing`). | View state of the open review; the app saves it as the menu does. |
| `bcode.code-review.set_view` | navigation | read-only | Layout (unified, side by side), wrapping, hidden resolved threads, hidden viewed files, file list (tree, flat), panels shown, folding all files or threads. | Screen state. |
| `bcode.code-review.open_review` | navigation | mutating | Opens any review of the repository (by id), optionally at a layer, file, line or thread; also `home` and `new_review` (the wizard, prefilled). Moving the user to another screen asks unless allowed. | Changes the screen. |
| `bcode.code-review.refresh` | navigation | read-only | Refreshes home or the open review. | Screen state. |
| `bcode.code-review.complete_pass` | review writes | mutating | Marks the review pass complete, as `shift+m`. | Needs the resolved revisions on screen. |
| `bcode.code-review.pull_request_review` | publish | mutating | Approves, requests changes or comments on a pull request of the open review, or approves every fully viewed stack layer. | Forge providers in the app. |
| `bcode.code-review.publish` | publish | mutating | Previews or publishes the review's comments with a publisher plugin (GitHub), as `shift+p`. | Publisher plugins are reached through the app. |

Every client tool answers with what it did (or why not, checked before acting, as the app's own
keys check), and the screen shows it at once because the app performs it. Plugin writes are
reflected as today: the app watches finished `code_review.*` calls in followed sessions and
reloads; `create_review`, `update_review` and `delete_review` reload home, and deleting the
review on screen returns home with a toast.

### Action parity

Every keymap action and flow is covered by a tool or deliberately left out:

| App action | Tool |
| --- | --- |
| Home open, continue, new, rename, archive, restore, suggestions | `open_review`, `update_review`, `find_pull_requests` + `create_review` |
| Wizard (all source kinds, stack step, editing sources) | `create_review`; `open_review new_review` to show the wizard prefilled |
| Layer stepping, all layers, file and line moves, threads | `go_to`, `set_comparison all_layers` |
| Compare menu, versions, pushes, stack history in place | `set_comparison` |
| Viewed marks (one, all, by layer) | `mark_viewed` |
| Comment, reply, resolve, kind, severity, suggestions | `add_comment(s)`, `reply`, `resolve_thread(s)`, `suggest_change` |
| Pass complete, publish, pull request verdicts | `complete_pass`, `publish`, `pull_request_review` |
| Layout, wrap, hide resolved or viewed, folding, panels | `set_view` |
| Refresh | `refresh` |
| Left out: copying to the clipboard, the appearance picker, keyboard help, search-as-you-type | Presentation for the user only. |

The acceptance requests resolve as:

* "Create a review for my most recent PR stack": `find_pull_requests mine` (the newest stack),
  `create_review stack_layers=[…]`, `open_review` (asks unless allowed).
* "Create a review for my uncommitted changes in `<directory>`": `create_review working_tree
  directory=<dir>` (paths scoped to the directory, in its repository), then `open_review`. A
  review in another repository is created there; `open_review` answers with the repository and
  the command line that opens it, since the app shows one repository.
* "Mark all these ten files as viewed": `mark_viewed paths=[…]` on the attached review (or
  `glob`, `layer`, `unchanged_since_look`).
* A pull request number or URL, a branch, a range, commits: `create_review` with that kind.
* Archive, rename, delete: `update_review`, `delete_review` (asks).

## Permissions

### Agent profile

The app registers one contributed profile with its tools: `bcode.code-review.assistant`
("Review assistant"), based on `build`, restrict-only. Every panel turn names it, so a turn
whose profile is unavailable fails closed (`agent_profile_unavailable`). Its instructions
describe the context blocks, the tools and the recipes above, and say to ask before acting on
anything the user did not ask for. AI cards keep the default agent.

The profile's tool selection is the union of the enabled categories. Restrict-only means
Bcode's `build` policy decides every exposed call, and a category can only remove tools.

| Category | Tools | Default | Decided by default as |
| --- | --- | --- | --- |
| `review_reads` | read plugin tools, `current_view`, `find_pull_requests` | on | allowed (read-only); `code_review.pull_request` declares the repository's web address, so it asks unless a web rule allows it |
| `review_writes` | write plugin tools, `complete_pass` | on | asks (mutating) |
| `publish` | `code_review.publish`, `bcode.code-review.publish`, `pull_request_review` | on | asks every time in the panel (Always is not offered) |
| `navigation` | `go_to`, `set_comparison`, `set_view`, `refresh`, `open_review` | on | allowed; `open_review` asks (it changes the screen) |
| `file_reads` | `filesystem.read`, `list`, `find`, `grep`, `stat`, `exists` | on | Bcode `read` rules (allowed) |
| `file_writes` | `filesystem.write`, `edit`, `multi_edit` | on | Bcode `write`/`edit` rules (ask) |
| `shell` | `shell.run`, `shell.terminal` | on | Bcode `command` rules (ask) |

Configuration (read by the app when it registers; a change applies on the next registration,
which happens on start and on reconnect):

```toml
[plugins.config."bcode.code_review".assistant]
# width = 60      # default panel width in columns (right dock)
# height = 14     # default panel height in rows (bottom dock)
# model = "fake-echo"   # app default model until one is chosen (`provider/model` too)
# effort = "high"       # app default reasoning effort until one is chosen

[plugins.config."bcode.code_review".assistant.tools]
review_reads = true
review_writes = true
publish = true
navigation = true
file_reads = true
file_writes = true
shell = true
extra = []        # exact names of other Bcode tools to expose, such as "web.fetch"
```

Allow, ask and deny stay Bcode's, under the base agent, for every session that uses `build`:

```toml
# bcode.toml
[agent.build.permission.tools]
"bcode.code-review.open_review" = "allow"   # move between screens without asking
"code_review.mark_viewed" = "allow"
"code_review.delete_review" = "deny"
```

* A category set to `false` removes its tools from the profile; a misspelled key or a non-boolean
  value is reported and the default is used. `extra` names that are not exposed by `build` stay
  hidden (restrict-only).
* The category table is kept in the app next to the profile; a test checks every listed name is
  a tool the static bundled plugins register, so a renamed Bcode tool fails the build instead of
  silently disappearing from the assistant.
* Prompts show in the panel (as above) and in `bcode tui`, which may answer them; the panel
  withholds Always for `publish`, `pull_request_review` and `delete_review` (a remembered rule
  can still be written with Bcode's own tools).
* Without the app, a conversation continued in `bcode tui` has its plugin tools and no client
  tools; Bcode's handling of an unavailable selected profile applies.

## Migration

Nothing is rewritten and nothing is lost:

* Per-review assistant sessions are listed from the binding table (and from their origin, when
  they have one) and tagged with their review, as described under
  [Listing and where tags live](#listing-and-where-tags-live).
* The first time one is attached elsewhere, the store records its old review as `started`
  before replacing the row, so the tag survives.
* Their earlier messages carry the old appended context line in the text; they render as they
  always have.
* Reopening a review no longer switches the panel to that review's newest conversation; the
  history picker lists it, tagged and searchable.

## Build plan

Each step is committed on its own with its tests; behavior tests drive the app through its test
harness (`testing.rs`) and the plugin through its tool tests, and the live checks use a debug
daemon built from this repository in an isolated state with the fake model provider.

1. **Conversation plumbing and history model.** `ReviewHost` gains submit with context blocks
   and agent, catalog pages by origin, rename, delete, pending permissions and a client tool
   provider connection; origin `layer`; binding `started` and `detached_at_ms`; store operations
   `review.assistant.list` and `review.assistant.unbind`; the history merge (catalog rows plus
   legacy bindings, tags, sorting, deleted reviews) as a pure function. Tests: legacy bindings
   appear tagged; `started` survives rebinding; detached bindings make tools without `review`
   fail with the documented message; sessions missing from Bcode are left out; ordering and
   paging bounds.
2. **App-wide panel, Ask AI button and history picker.** Move the dock from `ReviewSurface` to
   `ReviewApp` (placement, states, keys, palette on home), the header button with its
   degradation and badges, the history picker with switch, new, rename, delete and open in
   Bcode, attachment on every route change, live reflection forwarded to the route. Tests: the
   panel opens on home and stays open with the same conversation through opening a review,
   stepping layers and returning home; button click and `ctrl+k` on each route; the button's hit
   region equals its painted cells at 200, 80, 30, 12 and 1 columns, with CJK and emoji titles,
   and after resize; right, bottom and zoomed placement at their thresholds; picker search,
   switch, rename, delete twice and confirmation, needs-you rows.
3. **Context blocks, `current_view`, profile and categories.** `ViewContext` from home, the
   wizard and the review; the bounded renderer; blocks on every submit with the resubmit on
   `request_only_context_unavailable`; client tool registration and claims; the profile from
   the category configuration. Tests: the view block for a stack step-through with a comparison
   (layers, current layer, baseline per layer, deltas, hidden counts), for home and for a single
   pull request; after moving layer, file and comparison and after going home, no field of the
   previous place remains; every section cut carries its marker and the blocks pass
   `TurnContext` validation at maximum sizes; `current_view` returns the same values as the
   block; category defaults and toggles produce the documented tool selection; the category
   table's names exist in the bundled tool catalog.
4. **Plugin data tools.** `create_review` with every kind and `working_tree_paths`,
   `update_review`, `delete_review`, the `mark_viewed` selectors, `add_comments`,
   `resolve_threads`, `review` defaulting. Tests: each creation kind stores the expected sources
   (including a directory in another repository); bulk viewed by list, glob, layer and unchanged
   since look; batch writes report per item; writes ask and reads do not under the default
   policy; the app reloads home or the review after each finished call.
5. **App action client tools and acceptance.** `find_pull_requests`, `go_to`,
   `set_comparison`, `set_view`, `open_review`, `refresh`, `complete_pass`,
   `pull_request_review`, `publish`; the profile instructions with the recipes; docs
   ([code review app](code-review-app.md) assistant section, keys and configuration;
   [embedded sessions](embedded-sessions.md) for the review app's adoption). Tests: each tool
   changes the screen as its key does and returns the new state; `open_review` asks and a denied
   call never changes the screen; tools for another review work by id; with a live debug daemon
   and the fake provider, the three acceptance requests run end to end against the smoke
   fixtures with a fake `gh`, and nothing is posted to a forge.

## As built

Where the built panel differs from the design above:

* **Button degradation.** The button's label is chosen from the header's width (`✦ Ask AI` from
  48 cells, `✦ AI` from 24, `✦` from 8; none below, and the screen's title keeps 4 cells) rather
  than being planned as the last part of each screen's header. Each screen leaves the button's
  cells free at the end of its header row and the app paints the button there, so painting and
  the hit region still come from one plan.
* **Deleting from the conversation** asks in the panel's input row (`y` delete, `n` keep), not in
  a dialog; picker rows have no `✕` (`alt+d` twice deletes).
* **Turn counts** are read for the first 30 rows after a load, not per visible row.
* **The wizard** binds `ctrl+k` before its pickers, so `ctrl+k` opens the panel there even while
  typing a picker filter.
* **Context snapshot.** The view block also carries a `files:` line (files and viewed). A
  `pull_request:` line says `not loaded` when the review has pull requests whose conversation
  the app has not read; versions show as `version=vN of M` on the `file:` line. Selection text is
  the selected diff lines (`+`/`-`/` ` prefixed) as Ask AI cards send them. Section budgets are
  as specified; within a section, items are kept while they fit, each cut to 400 bytes (240 for a
  quoted value).
* **Categories.** `shell` holds `shell.run` only: `shell.terminal` is not among the `build`
  agent's tools, which the profile can only narrow (the test that checks every listed name
  against `build` caught it). `navigation` holds `go_to`, `set_comparison`, `set_view`, `refresh`
  and `open_review`; `review_reads` also holds `find_pull_requests`, `review_writes` the new
  plugin writes and `complete_pass`, and `publish` the app's `publish` and `pull_request_review`.
  Every panel message names the profile. While the app's tool connection is not registered
  (Bcode starting or restarting, a failed registration) messages wait in the panel, which says
  why, and go once it is; a host that cannot provide tools refuses to send. So no panel turn
  runs without the profile's category limits.
* **`current_view`** returns the snapshot's JSON with at most `max_files` files per layer or
  source (100 by default). An answer over 64 KiB shortens lists (files per layer, selection,
  threads, sources, pushes), then drops sections (pull request, threads, selection, comparison,
  sources, home, wizard), then keeps only the screen; it is always valid JSON, and a `truncated`
  object lists what was shortened or dropped and where to read it in full. A shortened list
  carries a count of what it left out: `files_omitted` beside a layer's or source's `files`,
  `omitted` in a bounded list (`items`, `total`, `omitted`). The comparison also
  lists the stack's newest 20 pushes (numbered as `set_comparison pushes` takes them) and, for
  sources that are not pull request layers but are compared with their last completed pass,
  each source's result (`sources` in JSON; `source_results` in the view block, `source_deltas`
  in the detail block).
* **Resubmission** covers a submit rejected with `request_only_context_unavailable`. A queued turn
  that fails later for that reason shows its failure in the conversation and is not resent.
* **Saved reviews by id.** Plugin tools resolve a saved review's pull request and stack sources
  without the network: a stack from the review's `stack_snapshots` (what it last opened with),
  else the stack or pull request the provider last reported (the on-disk review cache). Heads
  are the recorded head commits; the bottom layer's base is its recorded base commit, else
  `origin/<base>` or `<base>`. When those commits are not local the tool says to open the review
  in the app (`open_review`) once.
* **`update_review` and `delete_review`** need `review` or `reviews`; they never default to the
  attached review.
* **`mark_viewed` selectors** narrow each other (layers, then paths, glob, unchanged since the
  last look); `all` takes every file of the selected layers. "Unchanged since the last look"
  compares each file's patch id now with its patch id at the reviewer's latest look at the layer
  (a pull request's latest look, from its fork point when recorded), or at the revisions a
  completed pass recorded for other sources. A file without a look is left out.
* **`create_review`** takes `sources` (each with `kind`) rather than one source; a stack named by
  `stack_layers` takes optional `titles` for its labels. Pull requests named by number use the
  `origin` remote's repository on the `github` provider. With `directory`, uncommitted-change
  sources without `paths` are limited to the directory; the answer says `other_repository` when
  the review was created in another repository.
* **`find_pull_requests`** chains the provider's listed pull requests into stacks itself (a pull
  request whose base is another's head is the layer above it) instead of resolving each stack:
  one provider call. `mine` means a layer's head is a local branch of the repository (the
  user's own work, as Graphite stacks are); the provider does not report the viewer's login in
  its list.
* **`publish` and `pull_request_review`** open the app's dialog (as `shift+p` and the pull
  request card's actions do) for the reviewer to confirm; they do not submit by themselves.
* **`set_comparison`** checks every argument before applying any, so a call with one bad
  argument changes nothing. `pushes` takes `from_push` and `to_push` (1-based, oldest first): the
  stack just before `from_push` against the stack after `to_push` (now when absent), as the push
  timeline picks them; `stack_at` takes `at_ms`. `versions`, `pushes` and `stack_at` need the
  stack history: when it is not loaded yet the call starts loading it and asks to be repeated.
* **`go_to version`** shows one layer (`layer`, else the current one) at that version: its whole
  change then, from its fork point then, read-only where it differs from now. It is also a
  place `open_review` can name.
* **`open_review new_review`** takes `title` and `sources` (as `create_review` takes them; the
  source planning lives in the code review crate, shared with the plugin) and shows the wizard at
  its last step with them filled in; nothing is created until the user confirms.
* **`open_review`** reads the review from the store, opens it, and answers once it loaded when a
  place was named (or after 20 seconds, saying it is still loading). A review of another
  repository is not found; the profile instructions say to run `bcode review` there. Every move
  to another screen leaves the review on screen through its close path (presentation persisted,
  final and pending saves finished first); an unsaved review with comments, AI answers or
  suggestions is not left: the call fails with `unsaved_review` and the review's status tells
  the user how to save it.
* **Claims.** A new conversation's session is created on the tool connection (Bcode claims it
  there before it returns), and every message claims its session before submitting, so each
  turn, the first included, is offered the app's tools. Starting or switching a conversation
  releases the previous one's claim, and the app answers its tools only for the panel's active
  conversation: any other session gets `not_active_conversation`. The connection reclaims at
  most the 32 newest claimed sessions after a reconnect.
* **Concurrent writers.** The open review saved its whole workspace, undoing viewed marks,
  renames and archiving written meanwhile and recreating a review deleted meanwhile. Workspace
  rows now carry a revision that every write checks and bumps. `review.workspace.update` takes
  the writer's `base` (the row as it last read or wrote it); the store then writes only what
  the writer changed from it (whole fields, and per file and per thread for viewed state) over
  the stored row, reading again when another write came first, and refuses with
  `review_workspace_deleted` instead of inserting a saved review that is gone. The open review,
  home's rename and restore, source edits and the plugin's `mark_viewed` and `update_review`
  send their base. The open review rebases its copy onto every save answer and reload (renames
  and archives show at once), reloads after any `create_review`, `update_review` or
  `delete_review` call it sees and when the terminal regains focus, and returns home with a
  toast when its review was deleted.

## Parity with bdata

bdata's assistant panel is the measuring stick for the review assistant: everything it offers
that applies to a review app is offered here too, with the same keys where they do not collide
with the review app's own. This section lists every feature, how bdata behaves, the review
app's state, and the design for each gap. Features bdata has for its SQL editor (inline edits,
applying SQL) have review analogs or are marked not applicable.

### Gaps

| Feature | bdata | Review app | Design |
| --- | --- | --- | --- |
| Model chip and picker | `● <model> ▾` chip, `alt+m`; a filterable picker of the models Bcode offers (name, context window, price per million tokens, effort range, `no tools`), `●` on the current one; Bcode's default until chosen | Missing: every turn uses the session's or Bcode's default model | Chips row gains the model chip and the picker (below). The chip names the conversation's model, or `Bcode's model (<name>)` while none was chosen. |
| Reasoning effort chip | `◆ effort <value>`, `shift+tab` cycles the model's values (wrapping); shown only for a model with effort values | Missing | The effort chip and `shift+tab`, over the effective model's values (the chosen model, else Bcode's default model, so effort works before a model is chosen); a model without effort shows no chip and `shift+tab` says so. |
| Policy chips | `runs`, `data`, `edits` chips (`alt+r`, `alt+d`, `alt+p`), switchable for the session | Missing: the categories are configuration only | Chips for `review_writes`, `file_writes`, `shell`, `publish` and `navigation`, each on or off per conversation (below). |
| Tools list | `⚒ tools` chip, `alt+t`: every app tool and how Bcode decides it by default; choosing one says how to change its rule | Missing | `⚒` chip and `alt+t`: the profile's tools by category, `reads: runs` or `changes: asks`, and (for categories turned off) `off`; choosing one says how to set its Bcode rule. |
| Remembered model and effort | Saved in bdata's UI state as the choice for every later conversation; the configured `effort` seeds it | Missing | Per conversation and as the app default (below). |
| Ask AI cards | Not applicable (inline edits run in-process with Bcode's configuration) | Card sessions use Bcode's default model, no way to change it | Cards follow the same choice (below). |
| Usage | Panel title: `<tokens> tok · $<cost>` for the conversation | Present: tokens and estimated cost on the title row's right | Kept; formatted as bdata's and covered by a test. |
| Failures | A turn that ends without an answer says why in the conversation: provider unreachable or no model ("check the model Bcode uses and its sign-in"), tool-round limit ("ask the assistant to continue"), the model stopped answering, other failures with Bcode's message | Partial: `Error`, `IdleTimeout` and `ProviderUnavailable` show Bcode's message (or a generic line); the tool-round limit shows nothing; a refused submit (`agent_profile_unavailable`, no model configured) shows only in the notice line | Every turn outcome other than completed or cancelled adds an error line to its turn, with Bcode's message or the reason as bdata words it; a refused submit adds an error line to the conversation with Bcode's code and message (profile unavailable says the app's tool connection is not registered; no model says to choose one with the model chip or in `bcode tui`). |
| Open in Bcode | `↗` (only while a conversation is open) and the palette's "Open this conversation in Bcode's TUI"; bdata asks, then exits and hands the terminal to `bcode tui`; without a conversation a toast says to start one | `↗` always shown and `ctrl+o`; runs `bcode tui <session>` and the app resumes after; without a conversation nothing happens | `↗` only while the conversation has a session; `ctrl+o` and the palette entry without one say "start or open a conversation first". The app keeps suspending and resuming rather than exiting: nothing of the review is lost, so no confirmation is needed. |
| Delete this conversation | Palette entry, asks first | Present: `alt+d` and the palette ask in the input row | Kept. |
| Delete every conversation | Palette entry "Delete every conversation bdata started", asks first | Missing | Palette entry "Delete every conversation the review app started": a confirmation with the count (the app's sessions in this repository, as the history lists them, legacy bindings included); running turns are stopped, each session is deleted and unbound; sessions Bcode refuses (open in another client) are counted in the result notice and kept. |
| History rows | `alt+d` or the row's `✕` marks, a second press deletes; the footer says which row is armed | Partial: `alt+d` twice; no `✕` | `✕` at the right of the hovered (else selected) visible row, painted and registered only there; the armed row's `✕` turns to the danger style and the footer names it. |
| History features | Open, delete | Present and beyond: search, rename, tags, turns, needs-you, open in Bcode | Kept. |
| Title-row controls | `✕` closes, `»` collapses to a rail, `⤡` restores a zoomed panel, double-click on the title zooms | `+ new`, `⟲`, `↗` only; zoom only below the dock thresholds | `✕` closes the panel; double-click on the title row zooms the panel over the whole area and again restores it. No rail: the Ask AI button with its badges is the closed panel's indicator. |
| Empty state | What it can do, four example requests, and what the chips control | One generic paragraph, the same on every screen | Per screen (home, wizard, review) a line of what it can do, three example requests, and one line on the chips; while models fail to load, the reason. |
| Edit cards | Proposed SQL as diff cards: Apply (`alt+a`), Apply & run (`alt+g`), Copy, dismiss | Not applicable as such: the review app has no editor, and file edits are Bcode tool calls behind its permission prompt; panel answers have no actions | Answers in the panel get Copy and, when the conversation is attached to a review with a cursor line, Draft comment (a draft thread at that line, as an Ask AI card's Draft does). File edits keep Bcode's permission prompt. |
| Reasoning and tool output | Reasoning folds; a tool's full output opens | Present: reasoning and tool output fold | Kept. |
| Permission prompts | `y`, `a`, `n`, `d`; Always withheld for unsafe calls | Present; Always withheld for publishing and deleting | Kept. |
| Context | Always the screen | Present and beyond: the context chip, `ctrl+x` detaches | Kept. |
| Scrolling | `pageup`/`pagedown`, wheel | Present | Kept. |
| Composer selection, paste | Text input selection, mouse, paste | Present (the same text input) | Kept. |
| Notices | Notes in the conversation (stopped, moved conversations) | Present: notice line under the chips, `stopped` in the turn | Kept. |
| Bridge and version errors | Bcode missing, too old or too new: the panel says which to update | Not applicable (the app is part of Bcode); daemon unreachable shows the not-connected state | Model listing failures show on the model chip (`⚠ models unavailable`) with the reason on click, as bdata's chip does. |
| Configuration | `effort` (initial effort), `profile`, `context`, policy defaults | Panel size and tool categories | Adds `model` and `effort` (below) as the app default before anything was chosen in the app. |
| Footer hints | Send, stop, model, effort, runs, apply, new, close | Present without model and effort | Adds model, effort and permissions. |
| Inline edit | `alt+k` in the editor | Not applicable | The Ask AI card is the review app's inline request. |
| Earlier conversations | Imported into Bcode once | Present: per-review sessions listed with their tags | Kept. |

### Model and effort

* **Listing.** The models are those Bcode offers: for each model-provider plugin Bcode serves
  (as the TUI's model switcher finds them), that provider's model list, each request bounded in
  time; Bcode's default model status names the default. A provider that fails shows as a
  disabled row with its reason. Rows show name, provider, context window, price per million
  tokens when known, and the effort range; `●` marks the current model. The first row is
  `Bcode's default (<name>)`, which clears the choice.
* **The picker** is a centered overlay with a filter, reachable from the panel, a card and the
  composer; it uses the palette's matcher and the overlay's layout, so its hit regions come
  from the painted rows.
* **Per conversation.** The choice is the session's own Bcode model and reasoning selection
  (Bcode's session model and session reasoning settings), so it survives restarts, shows in
  `bcode tui`, and the chip reads it back from the session's model status when a conversation
  becomes active. Changing it while a turn runs applies from the next turn. A session's model
  is read once; one that cannot be read (deleted in `bcode tui`, a daemon error) shows
  `⚠ model unknown`, the picker's first row says why, and choosing a model for it reads it
  again.
* **App default.** The latest choice made anywhere in the app is also saved as the app default,
  a single user-wide record in the review store (`review.assistant.settings.get` and
  `review.assistant.settings.save`, a typed table). New conversations and new card sessions get
  it right after their session is created and before their first message; with no default,
  nothing is set and Bcode's default applies. Choosing `Bcode's default` clears it; while Bcode
  cannot name its default model, choosing it changes nothing and the conversation says why (a
  session goes back to Bcode's default only by naming that model). A default that cannot be read
  is read again when the picker opens; until it is read, effort changes are refused rather than
  saved over it.
* **Configuration** seeds the app default until one is saved:

  ```toml
  [plugins.config."bcode.code_review".assistant]
  # model = "fake-echo"          # model id; `provider/model` picks the provider too
  # effort = "high"              # used when the model offers it
  ```

* **Effort** cycles the effective model's values from the current one (else the model's default
  effort), wrapping; a value the newly chosen model does not offer is dropped. The app default
  saved with it names the model it was chosen for (a card on another model saves that model),
  so new conversations never get an effort their model does not offer; while the default
  follows Bcode's default model and the target runs it, the default keeps following it.
* **Ask AI cards** show `<model> · <effort>` on the card's action row; clicking it opens the
  picker for that card's session (and saves the app default). In the composer's question mode
  the same chip sits beside the question kind, with `alt+m` and `shift+tab`. A follow-up (a
  question at a place whose card already has a session) shows and changes that session's model;
  a new question shows and changes the app default its new session will get. When Bcode refuses
  the app default for a new card session, the card says why and the question runs on Bcode's
  default.

### Policy chips

* Chips `✎ review`, `▤ files`, `$ shell`, `⇪ publish` and `→ move` stand for `review_writes`,
  `file_writes`, `shell`, `publish` and `navigation`; an off chip is dim and struck through.
  Read categories have no chip.
* A chip toggles that category for the active conversation only, for the rest of the run;
  switching to another conversation and back keeps it (remembered for the newest 64
  conversations of the run), while new conversations and a restart start from the configured
  categories. A category off in configuration is not
  in the profile, so its chip shows `off` and does not toggle.
* Each panel message carries a per-turn tool allowlist (Bcode's turn execution options) of the
  profile's tools minus the categories turned off, so the next turn is not offered them;
  allow, ask and deny stay Bcode's rules.
* `alt+p` opens the permissions overlay listing the five categories with their state and
  tools; `enter` or a click toggles one.
* Narrow panels: the chips row wraps to a second row, then shows short labels (glyphs only),
  then drops chips from the right; the model chip is kept last. Every chip's hit region is the
  cells it painted.

### Keys

| Scope | Keys | Action | bdata |
| --- | --- | --- | --- |
| `code-review-assistant` | `alt+m` | Choose the model (`assistant_model`) | Same |
| `code-review-assistant` | `shift+tab` | Cycle reasoning effort (`assistant_effort`) | Same |
| `code-review-assistant` | `alt+p` | Permissions overlay (`assistant_permissions`) | `alt+r`, `alt+d`, `alt+p` cycle one policy each; here `alt+r` renames and `alt+d` deletes, as before, so one overlay holds the categories |
| `code-review-assistant` | `alt+t` | List the assistant's tools (`assistant_tools`) | Same |
| `code-review-assistant` | none (palette) | Delete every conversation the review app started (`assistant_delete_all`) | Same |
| `code-review-assistant` | `ctrl+o` (and palette) | Open this conversation in `bcode tui` | Palette only |
| `code-review-composer` | `alt+m`, `shift+tab` | Model and effort for the question (question mode) | Not applicable |
| `code-review-feed` | `alt+m`, `shift+tab` | Model and effort for the focused card's session | Not applicable |

None of these keys is bound in the scopes resolved with it (`code-review-assistant` with the
text input; `code-review-composer` with `code-review`; `code-review-feed` with `code-review`);
a test checks that no stroke resolves to two actions in any scope combination the app resolves
together.

### Host interface

`ReviewHost` gains, implemented on Bcode's client like the session bridge's model listing:

* `model_catalog()`: the default model status and each model provider's models (the model
  provider plugins from the plugin services list, each listed with the session model list for
  that provider).
* `session_model(session)`: the session's model status (provider, model, display name,
  reasoning values, selected effort).
* `set_session_model(session, provider, model)` and `set_session_reasoning(session, effort)`.
* `TurnRequest.tool_allowlist`, sent as the turn execution options' tool allowlist.

The scripted test host records each of these calls so tests assert the order (selection
before the first message) and the values.

### Build plan

1. **Model and effort in the panel.** Host interface; the settings record and its store
   operations; model and effort chips, the picker, `alt+m` and `shift+tab`; the app default
   applied to new conversations; configuration keys; the model-listing failure chip. Tests: the
   picker lists the host's models and Bcode's default; choosing sets the active session's
   model, and a new conversation's session gets the default before its first submit; effort
   cycles and wraps over the effective model's values, and a model without effort says so; the
   default survives an app restart over the same store, and each conversation shows its own
   model after switching; chip and picker hit regions equal their painted cells at 100, 40, 24,
   12 and 1 columns with CJK and emoji model names; live, with the fake provider, the next
   turn runs with the chosen model.
2. **Policy chips, tools list, failures and usage.** Category chips, the permissions overlay and
   `alt+p`, the tool allowlist on every panel turn, `⚒` and `alt+t`; failure lines for every
   turn outcome and refused submit; usage formatting; footer hints. Tests: turning `shell` off
   removes `shell.run` from the next turn's allowlist and back on restores it (live: the fake
   provider is not offered it); a configuration-disabled category's chip does not toggle; each
   failure kind renders its reason; usage shows tokens and cost; the key-conflict test.
3. **Ask AI cards.** Card sessions get the app default at creation; the card's model label and
   picker; composer question-mode chip with `alt+m` and `shift+tab`; feed-scope keys. Tests: a
   new card's session is set to the default before its question; changing a card's model
   changes only that session and the default; the composer chip's hit region at narrow widths.
4. **Conversation management and panel finish.** History row `✕`, delete every conversation,
   palette entries (model, effort, permissions, tools, open in Bcode, delete this, delete
   every), `↗` only with a session, title-row `✕` and double-click zoom, per-screen empty
   states, Copy and Draft comment on answers. Tests: the `✕` region exists only for the
   visible hovered or selected row and arms then deletes; delete every conversation confirms
   with the count, stops running turns, deletes and unbinds only the app's sessions and reports
   refusals; the palette lists the entries on home and in a review; the empty state per
   screen; Draft comment creates a draft at the cursor line.

### Parity as built

Built in the four steps above, with these deviations and notes:

* **Choosing Bcode's default.** Bcode has no operation that clears a session's model selection,
  so choosing `Bcode's default` sets the session to Bcode's current default model (its id and
  provider) and clears the app default; later conversations then follow Bcode's default again.
* **A model Bcode does not list.** Bcode accepts any model id for a session and runs it as
  given, so its turns fail at the provider. The app reads the session's model status back after
  every choice and after a new session gets the app default; when neither the id the session
  asked for nor the id it resolved to is in the listing of its provider (and that listing
  succeeded), the conversation says "Bcode does not list the model X". An alias Bcode resolves
  to a listed model is not reported, and the picker marks the model it resolves to.
* **Effort across models.** Switching models keeps the effort only when the new model offers
  it (a status without a selected effort counts as the model's default). Bcode restores the
  effort a session last used with a model when it switches back to that model, and the turn
  runs with it.
* **Categories** are per conversation for the run (not stored): a restart or a new conversation
  starts from the configured categories; switching back to a conversation keeps what was turned
  off in it. The allowlist is sent only while some category is off, so the default turn shape
  is unchanged.
* **Delete every conversation** pages Bcode's catalog for up to 1000 conversations per run; with
  more, the confirmation says it deletes the newest 1000 and the result notice says more remain.
* **Failures** cover the last turn of a conversation (the session view reports the last turn's
  outcome) and every refused message; earlier turns' failures stay in the session's transcript.
  Error lines wrap under their text in cards and the panel.
* **Usage** shows `N tok` and the estimated cost when Bcode estimates one (a model with prices);
  the title is cut to keep them on the title row.
* **Model labels on cards** are read for the newest 16 cards' sessions (and the session of a
  follow-up being written); the model part of a `model · effort` label is cut at 24 cells and the
  whole label at 40 cells on cards and in the composer.
* **Draft comment** drafts at the review's cursor (selected range, line or file header), as the
  composer anchors a new comment.
* **Live check** (debug daemon, isolated state, fake model provider): the picker listed Bcode's
  fake models and the next turn's provider request carried the chosen model
  (`fake-vision-panels`) and effort (`high`); after a restart a new conversation and a new Ask AI
  card session got the same default before their first message; a card's model changed from its
  card and its follow-up ran with the new model; turning `shell` off removed `shell.run` from
  the next request's tools and turning it on restored it; a scripted provider error showed its
  reason in the conversation; the title showed `12.3k tok`.

## Decisions

* The panel is app state and the route renders in the remaining rectangle; no route lays out
  the panel.
* Tags live in the session origin (immutable, visible to every Bcode client), with the binding's
  `started` as the fallback for sessions created before origins carried the workspace.
* The attachment is rewritten by every sender before sending and detached on home and in the
  wizard, so review tools never default to a review the user has left.
* The view block is durable and small; the detail block is request-only. Both are rebuilt from
  the live route at send time.
* Data operations are plugin tools; screen, navigation and forge-provider actions are client
  tools. `code_review.navigate` stays for cards and `bcode tui`; the panel uses `go_to`.
* Moving the user to another screen (`open_review`) is a mutating client tool and asks; moves
  within the review and comparison or view changes are read-only and run.
* Categories toggle exposure in a restrict-only profile; allow, ask and deny remain Bcode's
  rules on the base agent.
* Scoped working-tree reviews are a new source kind so older builds never broaden them.

## Open questions

* Bcode has no per-tool "never remember" for prompts; the panel withholds Always for publishing
  and deleting, but `bcode tui` may offer it.
* A contributed profile must list exact tool names; a Bcode capability to subtract categories
  from the base agent's tools would remove the category table.
* Opening a review of another repository in the running app (switching its repository) is not
  covered; `open_review` says the review is not in this repository and the instructions tell
  the model to point the user at `bcode review` there.
* `find_pull_requests mine` relies on local branches; a provider that reports the viewer's login
  with its list would allow matching by author too.
* A review renamed, archived or deleted by another client is noticed on the open review's next
  save, after a review-list tool call the app sees, or when the terminal regains focus; the
  store does not notify the app of other processes' writes.
