# Pull Request Conversation in the Review

Opening a pull request, a stack layer, or a whole stack in the review should feel like opening it
on the forge: the discussion is just there, inline and current. That covers who approved, who
asked for changes, who is still asked to review, whether checks pass, the description, and the
conversation. Replying, resolving, approving and requesting changes happen where the thing is,
with a button.

Today the review can publish drafts as a review and import inline threads by hand, read-only,
as a snapshot. This plan replaces the snapshot with a live, two-way conversation. It also keeps
the review provider-neutral: GitHub is one implementation of an interface any forge can serve.

## Principles

* **Information shows up where it applies, and only when it does.**
  * A layer's approval state is on its tab.
  * A thread is on its line.
  * The description sits at the top of its layer.
  * Nothing new means one quiet line, not a panel.
* **Every action is a visible button that works with the mouse.**
  * The buttons are Reply, Resolve, Approve, Request changes, Open on the forge and Refresh.
  * Keys are shortcuts for them, and the footer hints mirror the buttons.
  * Nothing is reachable only through a key.
* **Current without waiting.**
  * Cached activity paints with the first frame, and a refresh follows in the background.
  * A whole stack costs one batched provider call per refresh, never one per layer.
* **New is visible.** What arrived since you last looked at a layer is marked. While the review
  is open, a toast with a *Show* button announces it.
* **No provider is special.** See [Provider neutrality](#provider-neutrality). The review and its
  core never name a provider. What a forge can do comes from its manifest's capabilities, and
  buttons for things it cannot do do not appear.

## The model (provider-neutral, `bcode_code_review_models::conversation`)

* **Participant:** the provider id, an opaque id, a login, a display name, a web URL, and
  whether this is the viewer.
* **Pull activity:** one pull request's conversation as the provider saw it at one time:
  * **State:** open, draft, merged or closed.
  * **Decision:** approved, changes requested, review required, or none.
  * **Reviews:** each has a reviewer, a verdict (approved, changes requested, commented,
    dismissed or pending), a body, a time, the commit it was given on, and a URL.
  * **Requested reviewers:** people or teams.
  * **Threads:** extends today's `ExternalReviewThread`. Each carries a raw anchor:
    * the path and side;
    * the line range, both on the commit it was written against and its original lines;
    * the diff context around it;
    * that commit's id.
  * **Conversation comments:** top-level comments on the pull request.
  * **Checks:** an overall state, plus each check's name, state and URL.
  * **The description.**
  * **Viewer permissions:** whether you can review it (the author cannot approve their own),
    whether you can resolve threads, and whether you can comment.
* **Verdict:** comment, approve or request changes. Providers map their own vocabulary onto
  it, and providers without "request changes" do not advertise it.
* **Anchors are mapped by the core, not the provider.** The provider reports where a comment
  was written (commit, path, side, lines). The core maps that to the current document:
  * an exact match on the same head;
  * otherwise, carried through the diff between that commit and the current head;
  * otherwise, through the layer's versions in [stack history](review-stack-history.md).

  A thread whose code is gone attaches to its file, marked as being on an older version, with
  a button to open that version's comparison. Providers stop doing anchor mapping (today the
  GitHub importer receives the review's files to map against). Every forge then gets the same
  mapping quality.

## The provider interface

New operations on `ReviewSourceProvider` (in-process, so the standalone `bcode-review` has them
without a daemon). Each is served to plugins through the existing JSON adapter, and each is
advertised by a capability:

| Operation | Capability | What it does |
| --- | --- | --- |
| `review_source.pulls.activity` | `activity` | Pull activity for many pull requests in one call (GitHub: one aliased GraphQL request per batch), with a cancellation token like the other batched reads. |
| `review_source.thread.reply` | `thread_reply` | Reply to a thread now. |
| `review_source.thread.resolve` | `thread_resolve` | Resolve or reopen a thread. |
| `review_source.review.submit` | `review_submit { verdicts }` | Submit a review: a verdict, a body, inline comments (`DraftAnchor`s against the head it was reviewed at), and replies to existing threads. |
| `review_source.comment.post` | `comment_post` | Post a conversation comment. |

* **Publishing to the reviewed pull request** goes through `review.submit` on the pull
  request's own provider. The publisher and importer plugin services stay for destinations that
  are not the reviewed pull request (an export, another tool). The GitHub publisher's
  GitHub-only `submit_event` option becomes the neutral verdict.
* **Errors are typed:**
  * not permitted;
  * stale (the pull request moved since the read);
  * unavailable;
  * failed.

  The UI rolls back an optimistic change and says which of these it was.

## Freshness: change signals, not polling

The review never polls a forge. It refetches a pull request's activity only when something
says that pull request changed. Each *change signal* names the pull requests it is about (or
the repository, when the source can't tell). Changes close together are merged into one
batched `pulls.activity` call, half a second apart at most, so a burst of comments on one
layer still means one read.

* **Local signals** work with every provider:
  * the review opens or regains focus (from another app, or returning from a comparison);
  * one of your writes (reply, resolve, review) succeeds;
  * a local or remote-tracking ref of a layer moves (a fetch, a push, `gt sync`). The review
    already reads the reflogs; it watches the ref files the same way.
  * the header's refresh button is clicked.
* **Provider signals** come from a new `review_source.pulls.watch` operation, advertised by a
  `watch` capability that names its mechanism:
  * `push`: the forge sends events. Examples are a streaming endpoint (such as Gerrit's
    `stream-events`), or webhooks through a relay that a team runs.
  * `conditional`: the provider checks a cheap change marker. The provider, not the review,
    decides how often, and should honor the forge's own guidance (such as GitHub's
    `X-Poll-Interval` and conditional requests that cost nothing when nothing changed).

  The watch is a stream the review subscribes to with the open review's pull requests. It is
  cancelled like any other provider call when the review closes, and it is re-subscribed when
  the stack changes. Whatever is behind it, the review only receives *these pull requests
  changed*.
* **GitHub has no push channel to a local client.** Webhooks need a public endpoint, and its
  events API is documented as not real-time (latency of 30 seconds to hours). The GitHub
  provider therefore implements `conditional`:
  * one check per open review covers every layer;
  * it uses conditional requests where GitHub supports them, so an unchanged check is free;
  * it backs off when the review is idle or the terminal loses focus.

  Which marker is cheapest and catches reviews, comments and resolutions is settled by measuring
  in phase 6. A team that runs a webhook relay could later serve `push` for GitHub without the
  review changing.
* **Providers without `watch`** rely on the local signals and the refresh button.
* **Activity is cached.** It is a new cache kind (`Activity`), keyed by provider, repository
  and pull request number. The first frame paints from the cache, then a signal (the open
  itself) refreshes it.
* **The last-seen marker** is kept per workspace: the newest activity time you saw on each
  layer. It drives the *new* marks and survives restarts. The activity itself is cache, not
  review state, because the forge is authoritative.

## Configuration

These live in the review's plugin table, `[plugins.config."bcode.code_review".conversation]`:

* `replies`: `send_now` (the default), where a reply goes out at once with an *Add to review*
  button, or `with_review`, where it is held for your next publish with a *Send now* button.
* `overview`: `expanded` (the default), `auto` (folded to its status line unless something is
  new) or `collapsed`.
* `signals`:
  * whether provider signals are used at all;
  * a minimum gap between activity reads;
  * whether to pause while the terminal is unfocused;
  * per-provider settings passed through to the provider, such as GitHub's check gap within
    the forge's limits.
* `notify`: which new activity makes a toast: reviews, comments on your threads, all comments,
  or none.
* `threads`: whether resolved remote threads start folded, and whether outdated threads that
  could not be placed on current code show on their file.

## The view

**Header.** For the current pull request or layer:
* a decision pill: ✓ Approved, ✕ Changes requested, or ◌ Review required;
* a checks pill: ● 12/14. Clicking it opens a list of checks, each with *Open*.

The header's existing narrow-width order decides what drops first.

**Stack rail.** Each layer's tab gains its decision glyph and a count of unresolved remote
threads, using the existing compact degradation (`#101 ✓ 2/5 ●1 💬3`). Reading an 87-layer
rail then answers "what's approved, what's blocked, where's the discussion" without opening
anything.

**Layer overview.** Each pull request's card comes before its files, like the top of a pull
request's page, and is set apart from them by the same torn edge that separates files. In a
stack, the cards are what break the document into pull requests. The card holds:
* title, author, state and base;
* reviewers, each with their verdict, and *on an older version* when the layer changed in content
  since they reviewed (a rebase-only change keeps an approval current; see stack history);
* requested reviewers and checks;
* the description (markdown, folded after a few lines);
* the conversation (the newest few, *Show all*);
* buttons: **Approve**, **Request changes**, **Comment**, **Open on {provider label}**.

The card's status line (`✓ Approved by ana, raj · checks pass · 3 comments`) carries the
buttons and a ▾/▸ toggle; folded, the card is that line alone. Clicking the toggle, or `z` on
the card, folds or opens it.

**Inline threads.** Remote threads render like local ones:
* author, relative time and markdown;
* chips: Resolved, Outdated, New;
* buttons: **Reply**, **Resolve** / **Reopen**, **Open ↗**.

Resolved threads fold, under the existing resolved-thread filter. Your pending reply shows as a
draft under the thread until it is sent.

**Threads panel.** Remote and local threads appear together, filterable by all, unresolved,
mine, *waiting on me* (threads where the last word is someone else's on your change, or yours
to answer), and new.

**Publishing.** The publish flow puts the verdict choice up front as three buttons: Comment,
Approve, Request changes. Only the verdicts the provider supports and your permissions allow
appear. On a stack, a review goes to each layer that has drafts or a verdict. **Approve
reviewed layers** approves every layer you fully viewed, after listing them.

**History.** On the history panel's timeline, reviews appear as marks on the version they were
given on. The panel shows when an approval stopped being current.

**Toasts.** New reviews or comments on the open review produce a toast ("raj requested changes
on #52131"). It has a *Show* button that jumps to the layer or thread.

## Provider neutrality

* **The review's own crates name no provider.** `packages/code-review` and `packages/code-review-ui` name no
  provider. Provider names and formats live only in provider crates (`code-review-github`,
  `code-review-graphite`) and their plugin adapters. The single place that lists built-in
  providers is the composition root (`run.rs`). Labels come from the manifest (*Open on
  GitHub* is `Open on {manifest.label}`).
* **Capabilities hide what a forge cannot do.** Each button checks its capability and the
  viewer's permissions. A forge with only `activity` gets a read-only conversation with
  everything else working.
* **Vocabulary is the model's, never a forge's.** Verdicts, states and checks are neutral
  enums. Provider-specific extras (labels, merge queue position, Graphite's stack metadata) can
  come later as generic annotations, the way discovery contributors annotate branches today.
* **Graphite needs nothing of its own.** Its pull request pages read and write GitHub's reviews
  and comments, so the GitHub provider covers them. Graphite stays a discovery contributor.
* **Tests use a second provider.** The scenario tests drive everything through the existing
  fake provider, with different capabilities, including one without request-changes and one
  read-only. The JSON adapter round-trip test covers the new operations. Optionally, a check in
  `xtask architecture` rejects provider names in the neutral crates.
* **Existing leaks get removed:**
  * `bcode review publish-github` becomes a neutral `bcode review publish --provider <id>`,
    keeping `publish-github` as an alias for a release.
  * The GitHub-only `submit_event` option becomes the neutral verdict.
  * The importer message that names GitHub becomes generic.

## Phases

1. **Model and read path.**
   * The conversation models; `pulls.activity` on the trait, the JSON adapter and the GitHub
     provider (one batched GraphQL read);
   * the `Activity` cache kind;
   * fake-provider support.
2. **Status everywhere.** Decision and checks pills in the header, glyphs and counts on the rail,
   the checks list.
3. **Inline threads, automatically.**
   * Core anchor mapping: exact, then through the diff, then through history.
   * Remote threads in the document and the threads panel, with the last-seen marker and *new*
     chips.
   * Manual import stays for importers that are not the reviewed pull request's provider.
4. **Layer overview card.** Description, reviewers with currency, requested reviewers,
   conversation, buttons.
5. **Writes.**
   * Reply, resolve and reopen; verdicts in the publish flow; conversation comments.
   * *Approve reviewed layers*.
   * Optimistic updates with typed rollbacks.
6. **Live.**
   * Change signals: local signals; `pulls.watch` on the trait, the JSON adapter and the fake
     provider; GitHub's conditional watch, after measuring which marker to use.
   * Toasts with *Show*, and the *waiting on me* filter.
7. **History.** Reviews on the timeline, stale-approval marking.
8. **Neutrality cleanup.** Neutral publish CLI, the verdict option, and the architecture check.

## Decisions

1. **Replies to a remote thread:** sent at once by default, with *Add to review* beside it.
   This is configurable (`replies`). New comments stay batched as drafts, as today.
2. **Publishing and importing for the reviewed pull request** move to the provider interface.
   The publisher and importer plugin services stay for other destinations.
3. **Freshness comes from change signals, never review-side polling.** It is configurable
   (`signals`). Where a forge offers no push, the provider's watch does its own cheap
   conditional checks behind the same interface.
4. **The overview card** sits before the pull request's files. It starts open; folding it
   leaves its one status line. This is configurable (`overview`).

## Status

Implemented end to end (not yet committed).

* **Model and interface:**
  * `bcode_code_review_models::conversation` holds the model.
  * The `ReviewSourceProvider` has the seven new operations (default unsupported), served over
    JSON for plugins.
  * `ConversationCapabilities` is on the provider manifest.
  * Drafts gain `DraftAnchor::reply_to` (store migration 024), which is how a held reply is
    kept.
* **Core (`bcode_code_review::conversation`):**
  * Cached and fetched activity (the `Activity` cache kind, 7 days, 2,000 entries).
  * Thread placement:
    * exact on the same head;
    * otherwise carried through one `git diff -U0` per pair of commits;
    * otherwise on the file, or on the layer.
  * Review currency from stack history, and *waiting on you*.
* **GitHub (`bcode_code_review_github::conversation`, `bcode_forge`):**
  * Batched GraphQL activity reads.
  * Mutations for replies, resolving, comments and reviews. Held replies go through a pending
    review, which is deleted if a step fails.
  * The `stale`, `not_permitted` and `unavailable` error mapping.
  * A conditional watch. Its fingerprint covers update time, head, decision, state, threads'
    resolution and comment counts, and the checks rollup. One rate-limit point per 50 pull
    requests per check, every 30 seconds by default, backing off while GitHub is unavailable.
  * The publisher and importer plugin is now a thin delegate of the provider.
* **The app:**
  * The pull request bar: one line (summary and buttons) whose summary goes to the card.
  * Pull request cards in the document (`SourceIntro` items), before each pull request's files
    and torn off from them. A whole-stack review's card has one status line per layer.
  * Remote threads with their buttons, and replies sent now or held (configurable).
  * Rail marks, and the threads panel's *on the pull request* and *waiting on you* filters.
  * Provider-backed publishing with a verdict.
  * Approve and Request changes buttons that open it preset to one pull request, and *Approve
    reviewed layers*.
  * Review currency in the history panel and the overview.
  * Toasts for new activity.
  * Signals and watches, the `conversation` configuration table, and palette actions (`ctrl+o`
    opens the overview).
* **Neutrality:**
  * The CLI is `bcode review publish --provider`, in-process with no daemon. `publish-github`
    remains an alias that reads its old flags.
  * The importer message names no provider.
  * `xtask architecture` rejects provider crates under the review's core crates.
  * Built-in providers are named only in `run::builtin_providers`.

**Where it differs from the plan:**

* The decision and checks pills are in the pull request bar rather than the header. The bar is
  the pull request's header row, and showing them twice was noise.
* A toast cannot carry a button (the toast component only closes). Instead, the bar's *N new*
  and *waiting on you* are buttons that go to what is new, and the threads panel filters it.
* A whole-stack review's bar shows the stack's top pull request. Threads of every layer are
  placed on the whole diff.
* GitHub's watch interval is a plain per-check gap: GraphQL has no conditional requests. Measured
  cost is above.
* Leaving a pull request (moving to another layer) or collapsing its overview marks it seen.
* The overview is not a panel under the bar but a card in the document, before the files, so it
  scrolls with the review and separates pull requests in a whole-stack view. The bar stays one
  line. Its summary goes to the card. The cursor skips a file's details and torn edge.

**Verification:**

* Unit tests:
  * placement (exact, moved, dropped, file, layer);
  * currency and *waiting on you*;
  * signal merging, arrivals and notify modes, and watch bookkeeping;
  * configuration;
  * review marks on the timeline;
  * the GitHub parsers and mappings.
* Screen tests (`tests/conversation.rs`) against the fake provider:
  * the bar, and the card before the files with its torn edge, folding and reopening;
  * resolving a thread on the provider;
  * a reply sent now;
  * a held reply submitted with an approval started from the bar;
  * a read-only provider showing no actions, and a provider without *request changes*.
* Checks: workspace clippy with `-D warnings`, and `xtask architecture`.
* No write was tried against a real GitHub repository. Live reads (activity and an unchanged
  watch) were checked read-only on a real pull request.
