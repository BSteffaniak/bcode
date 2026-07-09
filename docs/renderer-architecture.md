# Renderer Architecture

Bcode renderers consume a shared semantic session-view layer rather than session event logs or another renderer's UI state.

## Shared renderer contract

New renderers should depend on:

* `bcode_session_view_models` for serializable snapshots, transcript items, patches, permissions, interactions, composer state, and `SessionViewAction`.
* `bcode_session_view` for bounded daemon-backed snapshot construction and `execute_session_view_action`.
* `bcode_client` only for daemon connectivity and renderer-host state flow that is not a daemon effect, such as selecting which session to display.
* `bcode_tool::InteractionInput` for renderer-neutral interactive-tool input.

Renderers must not depend on TUI frame, key, mouse, or BMUX drawing types. They also must not replay full session event logs during normal attach or refresh paths.

## Layer ownership

`packages/session-view/models` owns renderer-neutral data contracts. These types describe presentation semantics without terminal or browser primitives.

`packages/session-view` owns projection from bounded daemon/session history into `SessionViewSnapshot` and generic execution of daemon-backed `SessionViewAction` values.

`packages/tui` owns terminal layout and terminal-event mapping. Terminal-specific plugin surfaces remain TUI-only.

`packages/web-render` owns the HyperChad host, daemon connection, session selection, browser routes, and mapping HyperChad form events into shared semantic actions.

`packages/web-render/ui` owns only HyperChad presentation. Plugin visuals, artifacts, and interaction snapshots have a generic structured-data fallback here. Rich visual adapters are registered by exact plugin-owned `(schema, schema_version)` keys and must retain that fallback.

The local web host binds to loopback unless the CLI receives explicit non-loopback opt-in. Each launch generates a capability token; page and action routes must validate it before reading daemon state or executing effects, and generated links/forms propagate it.

Plugins own domain schemas and renderer-neutral interaction controllers. A renderer may add rich schema-specific adapters, but it must preserve the generic fallback and must not move plugin behavior into renderer code.

## State and action flow

1. A renderer host connects through `BcodeClient`.
2. It requests bounded history and builds a `SessionViewSnapshot` through `SessionView`.
3. Renderer UI code displays the snapshot without interpreting persisted event logs directly.
4. User input is converted to `SessionViewAction` or `InteractionInput`.
5. Daemon-backed actions run through `execute_session_view_action`.
6. Renderer-local state-flow actions, such as switching the displayed session or changing a history window, remain in the host and request a new bounded snapshot.

Web updates must use HyperChad's update/action mechanisms. Missing browser transport, routing, asset, or server capabilities belong upstream in HyperChad rather than in Bcode-specific JavaScript or WebSocket/SSE plumbing.

## Adding another renderer

A new renderer should start with `SessionViewSnapshot`, generic transcript/tool/artifact rendering, and semantic action mapping. It should add renderer-specific layout and input code around that contract, then add rich visual adapters only where actual schemas need them. It should not reuse `packages/tui` app state or fork projection and daemon-effect behavior.
