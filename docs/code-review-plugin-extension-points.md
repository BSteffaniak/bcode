# Code Review Plugin Extension Points

Code Review is product behavior owned by `plugins/code-review-plugin`. GitHub review integration is provider behavior owned by `plugins/github-review-publisher-plugin`. Host and runtime crates provide only generic routing, TUI, session, and service mechanisms.

## Stable generic capabilities used

Code Review consumes these generic plugin-host capabilities:

* Manifest-declared native plugin services with versioned interface IDs and typed JSON payloads.
* Manifest-declared native TUI surfaces registered through `PluginTuiRegistry`.
* Renderer-neutral keyboard, mouse, clipboard, redraw, and surface-exit actions from the plugin SDK TUI host.
* Generic session creation, user-message submission, session opening, and linked-session event subscriptions.
* Generic plugin service discovery and invocation for publisher and importer providers.

The Code Review domain contracts are provider-neutral and live in `packages/code-review/models`:

* `bcode.code_review/v1` owns local review workspace, draft, suggestion, publish-record, and external-import persistence operations.
* `bcode.review_publisher/v1` lets provider plugins advertise, preview, and submit review output.
* `bcode.review_importer/v1` lets provider plugins advertise and import external review state read-only.

Provider-specific repository, pull-request, authentication, API, and anchor behavior must stay in the provider plugin. New host capabilities are appropriate only when the SDK cannot express a required mechanism generically; product-specific behavior must not be added to host/runtime crates.

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
