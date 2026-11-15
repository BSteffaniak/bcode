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

* **Design.** Nothing in this document is built yet unless this section says so.
* **Built and relied on.** Session origin, client tools with claims, contributed agent profiles
  and turn context blocks ([embedded sessions](embedded-sessions.md#what-is-built)); the review
  app's per-review dock (`surface/dock.rs`), AI cards (`agent.rs`), the `code_review.*` plugin
  tools and the `review_assistant_sessions` binding table.
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
| `review_reads` | read plugin tools, `current_view`, `find_pull_requests` | on | allowed (read-only) |
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
  covered; the tool returns the command line instead.
