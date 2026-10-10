# Child sessions

Sessions may have children, which can create further children without a fixed nesting-depth limit. Parentage is immutable and independent of workflow origin, copied context, workspace, model selection, and execution authority.

## Create and discover

```sh
bcode session create "API implementation" --parent <parent-id> --child-id <retained-child-id> --cwd <absolute-checkout> --json
bcode session children <parent-id> --limit 32 --json
bcode session children <parent-id> --after <next-after-id> --limit 32 --json
bcode session activity <session-id> --children --limit 32 --json
```

Retain `--child-id` to reconcile an identical creation request after a lost response. Conflicting retries fail closed. Creating an ordinary child does not submit work, copy the parent's conversation, change permissions, or create a worktree.

Children remain independently openable at their session-ID-derived canonical paths in the parent's verified state location. Parent lifecycle actions do not implicitly cancel or delete descendants. There is no adoption or reparenting operation.

Discovery uses bounded parent-local pages, not a full-tree replay or a global catalog scan. Follow `next_after`, including across sparse pages. `complete` and `issues` express discovery trust independently of pagination. Pending publication, missing or inconsistent child storage, and stale or damaged checkpoints are surfaced rather than treated as an empty tree.

For historical or damaged relationship projections, explicit maintenance is available:

```sh
bcode session reconcile-children <parent-id> --apply --yes --json
```

Maintenance requires exclusive verified ownership, preserves canonical history, and may scan the session location. Ordinary discovery, opening, and activity observation never invoke it automatically.

## Parent experience

Confirmed child publication creates a durable parent transcript card with a stable child identity and a verified title snapshot. Its link opens the child through normal session navigation, not an external browser. Publication intent is not presented as successful creation. Child transcripts are not copied into the parent; refresh does not append history.

The TUI offers a compact child activity overview and a raised, bordered monitor with a contrasting background. `/monitor` opens or refocuses it; Ctrl-G opens/refocuses it from the composer and closes it when already focused. Escape releases monitor focus to the composer without closing the panel or changing its draft. Loaded rows retain relationship order as observations refresh, and selection follows the session identity rather than an activity rank.

Rows use hierarchy connectors and a separate expansion chevron. Clicking a row selects and focuses it without opening the session; clicking its chevron selects and toggles that branch. With monitor focus, Up/Down select, Right toggles expansion, Enter opens the selected session, and Left returns to the attached session's parent when available. The footer shows effective keybindings. Child navigation preserves parent presentation anchors.

Running, queued, attention-needed, failed, idle, and unknown observations have both textual/icon labels and distinct status styling. Multiple facts may coexist. Taller panels show selected identity, observation age, parent relationship, and own-session evidence; wide panels place these details beside the tree. The header identifies the current parent with “You are here”. Collapsed branches show known descendant activity explicitly marked partial. Narrow/short panels prioritize title and status and omit secondary overview/detail rows to retain bounded navigation. Idle is not completion. Observation counts are not subtree totals; freshness, incomplete coverage, and monitoring failures do not erase accepted evidence.

Hierarchy is explored incrementally, including loaded collapsed coordinators. Continuation cursors and incomplete coverage remain visible; a bounded resident view does not impose a limit on durable tree depth or size.

## Workflow children

Workflow execution-session parentage is separate from run-origin provenance. Authenticated graph publication binds generated work to the actual delegating session; callers cannot forge that binding through authored prompt metadata. Fresh isolated and pinned-generation execution sessions are children. Shared-parent sequential execution stays in its existing session and does not create a fictitious child. Context-copy and worktree policies remain independent of tree membership.

## Activity and authority

Running, queued, permission wait, input wait, idle, failed, interrupted, and unknown are normalized observations; multiple facts may coexist. Idle does not mean accepted or completed, and an idle coordinator can have active descendants. Assignment acceptance remains orchestration-owned.

Own-session evidence counts do not claim subtree totals. Nonresident sessions, event gaps, evicted evidence, and unobserved descendants remain unknown, partial, or degraded. Activity pages are timestamped observations, not durable subscriptions or reconnect checkpoints. Monitoring installs no scheduler and acquires no execution authority. Existing workflow ownership and cancellation fencing remain unchanged.

## Persistence and compatibility

The child's canonical `ChildSessionCreated` request records immutable parentage. Parent `SessionChildLinked` intent and `SessionChildPublished` acknowledgment fence publication; `SessionChildrenReconciled` records explicit maintenance coverage. The parent-local relationship projection advances atomically with canonical event checkpoints. Session summaries and catalogs remain derived.

Session event schema 50 and writer epoch 14 protect this representation; historical classification and upgrade policy belong to session migration. IPC protocol 44 adds typed explicit reconciliation alongside creation, relationship pages, metadata, and activity operations. Portable session contracts contain no terminal layout or daemon implementation types.
