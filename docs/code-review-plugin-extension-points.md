# Code Review Plugin Extension Points

Code Review is product behavior owned by its own crates: the data layer `packages/code-review` (`bcode_code_review`), the app `packages/code-review-ui` (`bcode_code_review_ui`, with the standalone `bcode-review` binary), and the plugin `plugins/code-review-plugin`. GitHub review integration is provider behavior owned by `plugins/github-review-publisher-plugin`. Host and runtime crates provide only generic routing, CLI registration, session, and service mechanisms; they contain no review screens, launchers, or types. See [`code-review-app.md`](code-review-app.md) for the app itself.

## Stable generic capabilities used

The review consumes these generic capabilities:

* Manifest-declared native plugin services with versioned interface IDs and typed JSON payloads. The plugin serves `bcode.code_review/v1` as thin adapters over `bcode_code_review`, so agents and other plugins reach the same records the app writes.
* Plugin tools (`bcode.tool/v1`). The plugin offers the review to agents as `code_review.*` tools whose operations are `bcode_code_review::assistant`. Each declares a policy operation through the standard tool preparation (`ToolPolicyOperation::ReadOnly` for reads and navigation, `Mutating` for writes and publishing) and takes the calling session and its working directory from the host's workspace context, so Bcode's ordinary permission rules and prompts govern them and the host knows nothing about reviews.
* Static CLI registration. When statically bundled, the plugin registers `bcode review`, which runs the `bcode_code_review_ui` app in the same process (the same way the git plugin registers `bcode git`). Only `publish-github` asks the host to start the daemon before it runs.
* Contributed keymap actions. The review's scopes are `code-review` (the review), `code-review-home`, `code-review-wizard`, `code-review-files`, `code-review-threads`, `code-review-composer`, `code-review-feed` (an AI card's permission answers and stop), and `code-review-assistant` (the assistant dock); user keymap configuration overrides them by scope like any other contributor. The app's `packages/code-review-ui/keymap.toml` is the one declaration: the app reads it at build time and the statically bundled plugin contributes it to bcode's keymap through its TUI registry.
* Generic daemon client operations: creating sessions, sending messages, answering permission requests (`resolve_permission_with_remember`), stopping a running turn (`cancel_session_turn`), and following a session's live view through `bcode_session_view::follow::follow_session_view`, which owns attachment, bounded `SessionView` projection, reconnect, and resynchronization. The review receives complete `SessionViewSnapshot` updates, never raw session events.
* Generic plugin service discovery and invocation for publisher, importer, and review-source providers.
* Theme discovery and component styles from `bcode_tui_theme`, shared with the chat TUI and `bcode-git`.

The app reaches all of these through its `ReviewHost` trait, so a host that embeds the review elsewhere (for example in the chat TUI) is another implementation of that trait.

The Code Review domain contracts are provider-neutral and live in `packages/code-review/models`:

* `bcode.code_review/v1` owns local review workspace, draft, suggestion, publish-record, and external-import persistence operations.
* `bcode.review_publisher/v1` lets provider plugins advertise, preview, and submit review output.
* `bcode.review_importer/v1` lets provider plugins advertise and import external review state read-only.
* `bcode.review_source/v1` lets provider plugins list pull requests, resolve stacks of pull requests, and fetch a pull request's commits into review-owned local refs. Plugin services cannot call each other, so the client routes these calls and passes the answers to `review.sources.resolve`.

Pull-request targets are provider-neutral. A publisher manifest may name the review-source provider whose pull requests it publishes (`pull_request_provider`); Code Review then sends one bundle per pull request of that provider's sources, with `ReviewBundle::pull_request` naming the target and `ReviewBundleThread::summary_only` marking threads the publisher should summarize rather than anchor inline (for example a whole-stack comment no single layer introduced). An import request names the review's pull request the same way (`ExternalReviewImportRequest::pull_request`). Providers use these only when their own options name no target. Built-in publishers also accept client-built bundles (`review.publish.bundle.preview` and `review.publish.bundle.submit`).

Provider-specific repository, pull-request, authentication, API, and anchor behavior must stay in the provider plugin. New host capabilities are appropriate only when the SDK cannot express a required mechanism generically; product-specific behavior must not be added to host/runtime crates.

## Linked sessions

Questions about the code run in ordinary Bcode sessions. The review creates the session, sends
the question, and follows the session's bounded live view in the question's card without leaving
the review: the streamed answer, collapsed reasoning, tool lines with the tail of their output,
errors, a pending permission request (answered from the card through the session's ordinary
permission resolution), and stop (an ordinary turn cancellation). The card keeps a bounded
summary of the view, never the transcript. Opening a linked session (`o`) hands the terminal to
`bcode tui <session>` and resumes the review where it was when that exits, so full session
interaction stays with the ordinary session frontend and the review keeps only its own state.

The review assistant dock is the same mechanism with one session per review: the app creates it,
records a review assistant binding (`review_assistant_sessions`: the session id, the review's
workspace, and the revisions its sources resolved to), and follows it in the dock. AI cards'
sessions are bound the same way. The binding is review-domain state; the session itself stays
session-owned. Tools find the review through the binding of the calling session, which the host
names in the tool's workspace context.

Reviewer AI exchanges persist only review-domain identity, anchor, question, linked session ID, and
lifecycle metadata. Session transcript and runtime state remain session-owned.

Workspaces also persist a versioned Code Review-owned semantic presentation state
(`presentation_v2`): the scroll anchor by item identity, layout mode, panel sizes, context
expansions, per-layer positions, collapsed threads, expanded answers, and the resolved-thread and
viewed-file filters. Raw terminal rows, focus internals, transcript, permissions, interactions,
tools, and runtime state are excluded. The state is disposable: an unreadable or newer value is
dropped, and stale keys resolve best-effort. The older presentation column
(`presentation_state_json`) stays in the schema but is not read or written; workspaces that only
have it open with their sources, viewed files, drafts, AI exchanges, suggestions, and publish
records.

## Disableability

The bundled Code Review and GitHub provider plugins are enabled by the aggregate `static-bundled-plugins` feature, but each has an independent feature:

* `static-bundled-code-review-plugin`
* `static-bundled-github-review-publisher-plugin`

Both dependencies are optional. Their static plugin registration, the `bcode review` subcommand, and the review keymap registration are feature-gated. A build can omit either or both features; without them, no Code Review product behavior is registered and the rest of Bcode remains usable. The standalone `bcode-review` binary does not depend on either feature.

Dynamic builds use the same plugin manifests and versioned interfaces. Disabling or removing the provider leaves local Code Review and built-in local publishers usable. Disabling Code Review leaves the GitHub provider undiscovered because no consumer requests its review interfaces, without changing host behavior.

## Contract-change checklist

When changing these extension points:

* Keep payloads typed, serializable, provider-neutral, and versioned.
* Keep Code Review UI/state out of host/runtime crates.
* Keep GitHub fields and API calls out of shared Code Review models and UI state.
* Add a real plugin consumer before adding a generic host capability.
* Preserve independent feature gates and test builds with Code Review/provider features omitted.
* Add architecture coverage whenever a new plugin-host contract is introduced.
