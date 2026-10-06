# Child sessions

Any ordinary session can create a fresh ordinary child. Children may themselves create children; there is no fixed nesting-depth limit. Parentage is immutable and independent of workflow provenance, copied-context derivation, workspace selection, model selection, and execution authority.

## Creating and discovering children

```sh
bcode session create "API implementation" --parent <parent-id> --child-id <retained-child-id> --cwd <absolute-checkout> --json
bcode session children <parent-id> --limit 32 --json
bcode session children <parent-id> --after <next-after-id> --limit 32 --json
bcode session activity <session-id> --children --limit 32 --json
```

`--child-id` is optional, but retaining it before creation allows an identical request to be retried after a lost response. Reusing an identity with different creation facts is rejected. Child creation does not submit a turn, copy conversation history, change permissions, create a worktree, or grant control over the child. The model's authenticated request-only session context supplies its current session ID for `--parent`.

Children remain independently openable sessions at their normal session-ID-derived canonical storage paths in the parent's verified state location. Stopping, archiving, or deleting a parent does not implicitly cancel or delete its descendants. No adoption or reparenting operation is provided.

Discovery and activity use bounded direct-child pages. Recursively expand each returned child's page rather than requesting a full-tree replay. Follow `next_after` even when a candidate page contains no matching children. `complete` and `issues` describe discovery trust separately from pagination: an exhausted cursor does not prove that a damaged or incompletely indexed tree has no other children.

The global parent index is derived. Persistent discovery currently reports unverified completeness because canonical child publication and catalog publication are separate operations; a crash may publish the child before indexing it. Reads verify returned relationships against bounded canonical facts and do not repair, migrate, or scan all histories to conceal missing entries. An exact retry reconciles an already published child and refreshes its derived metadata.

## Terminal navigation and monitoring

The session picker presents loaded parent/child relationships as a tree, preserves ancestry for search results, and supports lazy expansion and parent navigation. `Alt-N` loads the next child page.

`Ctrl-G` opens or closes the child monitor. Within it:

* Up/down selects a loaded child.
* Enter opens the selected session.
* Right expands a coordinator's direct children.
* Left opens the current session's parent.
* `n` requests the next bounded child page.
* Escape closes the panel.

The monitor shows titles and normalized activity evidence, refreshes observed coordinators incrementally, and preserves reading positions when returning between sessions. A compact statusline indicator reports **known active** children, not an inferred whole-subtree total. Deep nesting uses a compact depth label rather than unlimited indentation. Loaded monitoring state is a bounded disposable presentation cache, not a restriction on session-tree size.

A child's canonical relationship produces a compact transcript parent link. Child transcripts are not copied into the coordinator and polling does not append transcript events. Parent-side delegation cards are not automatically persisted by creation.

## Activity and authority

Running, queued, permission wait, input wait, idle, failed, interrupted, and unknown are normalized observation facts. Multiple facts may coexist. Idle is not task completion or acceptance; an idle coordinator may have running descendants. Assignment review and acceptance remain orchestration-owned.

Activity observation never installs a scheduler or acquires execution ownership. Nonresident sessions, event gaps, evicted observations, and incomplete evidence remain unknown or degraded. Refreshable activity pages carry observation timestamps; they are not a resumable subscription or durable reconnect cursor. Existing workflow execution ownership and cancellation fencing remain unchanged.

## Persistence and compatibility

`ChildSessionCreated` is appended to both current public and persisted event enums. Its immutable creation request is canonical in the child history; `SessionSummary.parent_id` and the catalog parent index are projections. Session event schema 49 and writer epoch 13 protect this representation, with historical classification and upgrades owned by session migration. IPC protocol 43 adds typed creation, child-page, metadata, and activity application operations. Portable session models contain no terminal layout or daemon implementation types.
