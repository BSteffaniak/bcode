# Review Loading Architecture

How a review gets from "open" to a fully loaded, current screen. The goal: what the reviewer
looks at is on screen as soon as the facts it needs are known locally (milliseconds), everything
else arrives around it without ever blocking it, and the cost of a review grows with what is
looked at, not with the size of the stack or the repository.

This replaces the current open path, where one loader resolves every source (asking providers,
resolving revisions, listing every file of every source) and the screen appears only when all of
it is done. Measured on an 87-layer stack: ~10 s to the first frame, most of it spent on work the
first frame does not need (resolving the stack a second time, listing 86 other layers' files).

## Decisions

* Built-in providers (GitHub, git history, Graphite) are libraries the review links and calls
  in-process; the Bcode plugins wrap them, and a running Bcode's plugins can add more.
* Stack layers found by local evidence are shown before the forge confirms them, labelled
  unconfirmed, and never used for anything that acts on the forge.
* On-disk caches live in a separate, disposable cache database, not the review store.
* Loading moves into a UI-independent `ReviewSession` in `packages/code-review`, replacing the
  app's all-or-nothing loader.

## Principles

1. **Render first.** The screen never waits for work it does not display. A review opens on
   whatever is already known and shows, explicitly, what is still loading.
2. **Snapshot, then revalidate.** Every fact that can change remotely (a pull request, a stack)
   has a local snapshot that renders immediately and a background revalidation that produces
   deltas. Staleness is shown, never hidden, and is short-lived.
3. **Address immutable work by content.** Work whose inputs are commit ids (a merge base, a
   file index between two commits, hunks of a blob pair) never changes. It is cached under those
   ids, shared across reviews and sessions, and never invalidated, only evicted.
4. **Demand-driven, prioritized, cancellable.** Work is requested by what needs it (the
   viewport, the focused layer, the rail, a revalidation) at a priority. A scheduler runs it with
   bounded concurrency per resource; superseded work is cancelled.
5. **Batch and coalesce.** Identical requests in flight share one execution (single flight).
   Batchable requests issued close together are sent as one (a DataLoader-style batcher):
   branch lookups, pull requests by number, file indexes, fetches.
6. **No global locks on the data path.** Git work is stateless and runs in parallel on a bounded
   pool; caches are concurrent. Nothing serializes unrelated work.
7. **Incremental model, incremental document.** Results arrive as events that update one part
   of the review (one source, one layer, one file); the document applies them as splices. A
   whole-document rebuild happens only when the review's sources change.
8. **Measured.** Every stage is timed; first-frame and full-load latencies have budgets checked
   in tests against a synthetic 100-layer stack.

## Layers

```
  UI (code-review-ui)        screen, rail, banner, file panel: subscribes to session events
        │
  ReviewSession              review model + scheduler + caches; emits events     (code-review)
        │
  ┌─────┴──────────────┬──────────────────────┬──────────────────────────┐
  Forge facts          Discovery              Git engine
  (providers: PRs,     (local evidence:       (local, immutable work: revisions, merge bases,
  stacks; mutable)     links, annotations)    indexes, hunks, blobs; content-addressed)
        │                     │
  Provider registry: built-in providers linked in-process (GitHub, git, Graphite), plus any a
  running Bcode offers through its plugins
```

## Providers are libraries; plugins wrap them

The review is its own application: `bcode-review` runs without Bcode, and `bcode review` runs
the same code inside Bcode. Its integrations follow the same rule.

* **Built-in providers are crates the review links.** The traits live in
  `packages/code-review/provider` (`bcode_code_review_provider`, with JSON adapters both ways)
  (`ReviewSourceProvider`: pull requests by number, branch, and query, branch neighborhoods,
  stack resolution with hints, batched local-first fetching; `DiscoveryContributor`: links,
  related branches, annotations). Implementations:
  * GitHub: `packages/code-review-github` (the stack walk, lookups, and fetching, over
    `bcode_forge`);
  * git history: `packages/code-review` itself (`discovery::git`);
  * Graphite: `packages/code-review-graphite` (Graphite's branch metadata).
* **The session calls them in-process,** as typed function calls: no daemon, no serialization,
  ordinary concurrency, cancellation, and progress. `bcode-review` needs no installed Bcode for
  pull requests or stacks.
* **Bcode plugins wrap the same crates.** `github-review-publisher-plugin` and `graphite-plugin`
  keep serving `bcode.review_source/v1` and `bcode.review_discovery/v1` for agents and other
  plugins, as thin adapters over those crates.
* **Plugins still extend the review.** The registry adds the providers and contributors that a
  running Bcode's plugins offer (the existing host routing), skipping those it already has
  in-process (by provider and contributor id). A third-party provider works in both `bcode
  review` and `bcode-review` when Bcode is running; nothing built in depends on it.

Today, by contrast, the standalone binary reaches GitHub and Graphite only through the Bcode
daemon (located, verified, and started on first use), every call is serialized through it, the
GitHub plugin runs at most two calls at once, and progress events and cancellation do not cross
the host.

### Git engine (`packages/code-review`, data layer)

* **Stateless operations, parallel.** Resolving revisions, merge bases, file indexes, hunks, and
  blob reads take `&self` and run on a bounded pool sized to the machine. The long-lived
  `cat-file --batch` processes stay (one per repository, behind their own lock); nothing else
  shares a lock. Today every data call holds one `Mutex<ReviewData>`, so a background index
  blocks the hunks the viewport is waiting for; that lock goes.
* **Batch operations.** `review.index.get_many` lists many revision pairs in one call (in
  parallel inside); `review.commits.exist` checks many commits with one
  `cat-file --batch-check`; merge bases for a stack are computed together (as `resolve` now
  primes them).
* **Content-addressed caches.** Keys are commit ids (and options such as whitespace handling):
  `merge_base(a, b)`, `index(base, head)`, `hunks(base_blob, head_blob, options)`. In memory
  (bounded LRU) and on disk (see Caches). Immutable, so they are shared freely and need no
  invalidation; a force-push produces new ids and therefore new keys.

### Forge facts (providers, `bcode.review_source/v1`)

* **Facts, not answers.** The client keeps per-repository facts: pull requests by number (head
  and base branches and commits, state, title, updated time) and branch neighborhoods (open pull
  requests from and into a branch). A stack is derived from facts, so resolving a stack whose
  layers are already known costs nothing, and revalidating it is one batched query.
* **Operations.**
  * `review_source.pulls.get`: many pull requests by number in one request (new; aliased
    GraphQL). The revalidation workhorse: one request confirms an 87-layer stack.
  * `review_source.stack.resolve` with `hints` (exists), `pulls.list` with `query` (exists),
    `pulls.fetch` batched and local-first (exists).
  * Progress for long operations: a cold stack walk reports layers as it finds them, so the rail
    fills in progressively. In-process this is a callback; for plugin-backed providers it needs
    the host to forward the plugin SDK's service events and cancellation, which it does not today.
* **Concurrency.** Built-in providers are called in-process and concurrently; a provider sends
  independent batches in parallel rather than serializing them on one connection lock.
  Plugin-backed providers are bounded by their plugin's declared concurrency.

### Discovery (`bcode.review_discovery/v1`)

Unchanged in role ([`review-discovery.md`](review-discovery.md)): local, fast evidence. Two
additions: its answers are cached and invalidated by the tips of the refs they read (Graphite's
metadata refs, branch tips), and they feed **provisional** stack layers (below).

### ReviewSession (`packages/code-review`, UI-agnostic)

One object per open review, owning:

* **The model:** sources (pinned revisions), per-source load state (`known` → `indexing` →
  `ready` / `failed`), the stack (snapshot, provisional layers, confirmed layers), annotations,
  and freshness (when each fact was confirmed).
* **The scheduler:** a priority queue per resource (git pool, each provider, the network), with
  three classes: *interactive* (what is on screen: the focused layer's index, visible hunks),
  *near* (the next and previous layers, the rail's facts), *background* (other layers,
  revalidation, discovery). Units are small (one layer's index, one batch), so interactive work
  starts within one unit of being requested. Every unit carries a generation; superseded units
  are dropped before they run and cancelled when the provider supports it.
* **Events:** `SourceAdded`, `SourceUpdated(load state)`, `StackUpdated(delta)`,
  `FactsConfirmed`, `Progress(stage, done, total)`, `Failed(scope, error)`. The UI applies them;
  agent tools and other frontends use the same session.

Moving this out of the UI's loader is what makes the rest possible: the screen subscribes to a
model that becomes complete over time, instead of waiting for a finished `Opened`.

### Document (`ReviewDocument`)

* **Incremental groups.** `upsert_group(input)` inserts or replaces one source's group and emits
  splices; a group can be *pending* (its banner and a "loading N files…" row) or *ready*.
  Together with `set_focus` (exists), a layer can appear, fill in, or be re-pinned without
  rebuilding the document or moving the cursor.
* Everything else (lazy hunks, residency budget, viewed state) is unchanged.

## Opening a review

| Path | Known at t=0 | First frame | Then |
| --- | --- | --- | --- |
| From the wizard | the stack the wizard resolved (every layer's commits and branches) | the focused layer's index (one local diff, ~30 ms; cached after the first time) | other layers' indexes in the background (near first); revalidation is unnecessary (just resolved) |
| Reopening a saved review | the stack snapshot saved with it | same | revalidation: one `pulls.get` for every layer plus the top's children; deltas re-pin only what moved |
| Cold (`bcode review stack 52129`) | a pull request number | the screen with "finding #52129…"; the layer itself as soon as `pulls.get` answers (~0.5 s) | discovery proposes provisional layers immediately; the stack walk confirms them and adds the rest as it goes |

Commits that are not local are fetched in one batched fetch at near priority for the focused
layer's neighbors and background priority for the rest; a layer whose commits are missing shows
as "fetching" rather than blocking anything else.

### Provisional layers

Discovery (Graphite metadata, git topology) knows the chain of branches in ~0.1 s, before the
forge confirms any pull request. Provisional layers render in the rail and the wizard by branch
name, marked as unconfirmed; they become pull requests when the forge confirms them and
disappear if it does not. Anything that acts on the forge (publishing comments, which pull request
a comment belongs to) uses confirmed layers only.

## Showing progress

* The rail marks each layer's state: ready, indexing, fetching, unconfirmed, failed.
* The focused layer's banner says what is still loading in the stack ("loading 52 more pull
  requests…", "checking with GitHub…") and when the stack was last confirmed.
* Stepping to a layer that is not ready shows its banner with its state and loads it at
  interactive priority; nothing blocks.
* The opening screen exists only for the cold path's first forge answer, and names the stage.

## Caches

| Cache | Key | Validity | Where |
| --- | --- | --- | --- |
| merge base, index, hunks | commit / blob ids (+ options) | forever (evicted by size) | memory LRU + on-disk cache database |
| commit exists locally | commit id | until a fetch or gc (cheap to recheck) | memory |
| forge facts | (provider, repository, number or branch) | confirmed-at + revalidation | memory + on-disk cache database |
| discovery answers | contributor + tips of the refs it read | until those refs move | memory |
| stack snapshot of a review | review source | revalidated on open | review store (with the workspace) |

The on-disk cache database is separate from the review store: disposable, size-bounded, never
needed for correctness, safe to delete.

## Performance budgets (checked in tests)

On a synthetic 100-layer stack with local commits and a fake provider with realistic latency:

* first frame of a step-through review opened from the wizard: < 50 ms after the request;
* first frame of a saved review: < 50 ms; revalidation complete: one provider round trip;
* every layer indexed: < 1 s, without delaying any interactive request by more than one unit;
* a cold open: the screen immediately; the requested layer after one provider round trip.

## Phases

0. **Providers as libraries** (done): the provider and contributor traits in
   `packages/code-review/provider`;
   `code-review-github` and `code-review-graphite` extracted from the plugins, which become
   wrappers; git discovery called in-process; the registry merging built-in and plugin-backed
   providers.
1. **Git engine:** remove the global data lock; bounded parallel pool; content-addressed
   in-memory caches; `review.index.get_many` and batched commit checks.
2. **Session and document:** `ReviewSession` with model, scheduler, and events; incremental
   document groups; render-first open from the wizard and from saved stack snapshots; progress
   in the rail and banner.
3. **Forge facts:** the client fact cache, `review_source.pulls.get`, revalidation and deltas;
   concurrent provider calls and parallel batches.
4. **Cold open:** provisional layers from discovery; progress events from providers through the
   host; cancellation.
5. **Persistence and budgets:** the on-disk cache database; latency budget tests in CI.
