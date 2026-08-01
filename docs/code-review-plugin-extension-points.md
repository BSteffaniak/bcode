# Code Review Plugin Extension Points

Code Review is product behavior owned by `plugins/code-review-plugin`. GitHub review integration is provider behavior owned by `plugins/github-review-publisher-plugin`. Host and runtime crates provide only generic routing, TUI, session, and service mechanisms.

## Stable generic capabilities used

Code Review consumes these generic plugin-host capabilities:

* Manifest-declared native plugin services with versioned interface IDs and typed JSON payloads.
* Manifest-declared native TUI surfaces registered through `PluginTuiRegistry`.
* Renderer-neutral keyboard, mouse, clipboard, redraw, and surface-exit actions from the plugin SDK TUI host.
* Generic semantic linked-session observation through complete bounded `SessionViewSnapshot` updates; the TUI host owns attachment, `SessionView` projection, reconnect, and authoritative resynchronization, while plugin surfaces never receive raw session events.
* Typed `PluginTuiAction::OpenSession` suspension: the host retains the plugin surface in memory, runs the ordinary native session viewer with the caller-owned input stream, and redraws the same surface after the viewer exits.
* Generic plugin service discovery and invocation for publisher and importer providers.

The Code Review domain contracts are provider-neutral and live in `packages/code-review/models`:

* `bcode.code_review/v1` owns local review workspace, draft, suggestion, publish-record, and external-import persistence operations.
* `bcode.review_publisher/v1` lets provider plugins advertise, preview, and submit review output.
* `bcode.review_importer/v1` lets provider plugins advertise and import external review state read-only.

Provider-specific repository, pull-request, authentication, API, and anchor behavior must stay in the provider plugin. New host capabilities are appropriate only when the SDK cannot express a required mechanism generically; product-specific behavior must not be added to host/runtime crates.

## Plugin semantic session observation

Native plugin surfaces that need a bounded embedded view call
`PluginTuiHost::subscribe_session_view`. The request identifies a session, a hard-bounded
`ProjectionWindowRequest`, and renderer-local reasoning policy. The TUI host attaches through the
normal bounded projection path, hydrates a `SessionView`, and emits complete
`SessionViewSnapshot` values. It applies durable, live, and runtime-work events only at the shared
`SessionView` boundary.

A reconnect or daemon resynchronization request replaces the observer with a newly attached
bounded view before updates resume. The host may expose `Reconnecting` or `Resyncing` connection
state in snapshots, but plugin consumers always replace state by session ID; they do not merge raw
events or retain transcript authority. Multiple plugin-domain anchors may share one host
subscription and one current snapshot.

The SDK deliberately does not expose raw session event subscriptions to plugin surfaces. This
keeps transcript semantics frontend-independent and prevents plugins from recreating event-log
reducers. Full native session interaction remains owned by the ordinary session frontend.

`PluginTuiAction::OpenSession` provides generic nested navigation without transferring session
behavior to a plugin. The host suspends and retains the requesting surface, runs the ordinary
native session viewer with the existing terminal input stream, then redraws the same surface after
the viewer exits. Immediate review return state therefore remains plugin-owned in memory; the host
receives only the typed session ID. Attach failure returns an error through the ordinary viewer path
without converting the session into plugin state.

Reviewer AI exchanges persist only review-domain identity, anchor, question, linked session ID, and
lifecycle metadata. Session transcript and runtime state remain session-owned.

Workspaces also persist a versioned Code Review-owned semantic presentation state: selected stable
path/thread/line, sidebar and filter modes, and collapsed/expanded thread identities. Raw terminal
rows, focus internals, transcript, permissions, interactions, tools, and runtime state are excluded.
Reopen restoration is best-effort and deterministically falls back to the first valid file/line when
a saved path or anchor is stale. Exact native-session return remains in-memory through retained
surface suspension.

## Disableability

The bundled Code Review and GitHub provider plugins are enabled by the aggregate `static-bundled-plugins` feature, but each has an independent feature:

* `static-bundled-code-review-plugin`
* `static-bundled-github-review-publisher-plugin`

Both dependencies are optional. Their static plugin registration and Code Review TUI registry entries are feature-gated. A build can omit either or both features; without them, no Code Review product behavior is registered and the rest of Bcode remains usable.

Dynamic builds use the same plugin manifests and versioned interfaces. Disabling or removing the provider leaves local Code Review and built-in local publishers usable. Disabling Code Review leaves the GitHub provider undiscovered because no consumer requests its review interfaces, without changing host behavior.

## Contract-change checklist

When changing these extension points:

* Keep payloads typed, serializable, provider-neutral, and versioned.
* Keep Code Review UI/state out of host/runtime crates.
* Keep GitHub fields and API calls out of shared Code Review models and UI state.
* Add a real plugin consumer before adding a generic host capability.
* Preserve independent feature gates and test builds with Code Review/provider features omitted.
* Add architecture coverage whenever a new plugin-host contract is introduced.
