# Renderer Architecture

Bcode's target renderer architecture uses a shared semantic session-view layer rather than session event logs or another renderer's UI state.

## Current migration status

The target boundary exists but is not yet the single application boundary:

* `packages/session-view/models` defines renderer-neutral snapshot, transcript, tool, permission, runtime-work, composer, interaction, visual, action, and patch contracts.
* `packages/session-view` projects bounded history and renderer-relevant live events and executes daemon-backed semantic actions.
* `packages/hyperchad` consumes this layer as Bcode's HyperChad application host; selected Cargo features choose the concrete HyperChad backend.
* `packages/hyperchad/ui` owns portable HyperChad presentation built from canonical HyperChad templates, routes, forms, actions, and renderer APIs.
* `packages/tui` dual-projects session history/live events into `SessionView` for parity protection and adapts shared transcript items into terminal presentation. Assistant and reasoning streams now consume the shared projection by stable `TranscriptViewItemId`, preserving terminal render identity across incremental replacement and durable finalization even when usage or tool rows are interleaved. The TUI also consumes shared generic user/system/usage items and shared runtime-work, active-skill, plugin-status, model/agent/reasoning-selection, reasoning-visibility, context-occupancy, cumulative-usage, session-metadata, authoritative pending-permission/interaction, and live interaction semantics. Its bounded history-window rebuilds retain authoritative hydrated shared state, while specialized tool/permission/interaction/runtime projections still use established terminal projection paths pending focused migration.

Until TUI migration and projection parity are complete, the shared session view is an extraction target and HyperChad application contract, not yet the canonical state path for every renderer. The implementation progress tracker should be kept aligned with the audited gaps described here.

## Shared renderer contract

New renderers should depend on:

* `bcode_session_view_models` for serializable snapshots, transcript items, patches, permissions, interactions, composer state, and `SessionViewAction`.
* `bcode_session_view` for semantic projection and `execute_session_view_action`.
* `bcode_client` only for daemon connectivity, bounded attach/hydration, and renderer-host state flow that is not a daemon effect, such as selecting which session to display.
* `bcode_tool::InteractionInput` for renderer-neutral interactive-tool input.

Renderers must not depend on TUI frame, key, mouse, or BMUX drawing types. They also must not full-replay session event logs during normal attach, refresh, or history paths.

## Layer ownership

`packages/session-view/models` owns renderer-neutral data contracts. These types describe presentation semantics without terminal or browser primitives.

`packages/session-view` owns generic projection from bounded daemon/session state into `SessionViewSnapshot` and generic execution of daemon-backed `SessionViewAction` values. It must remain domain-focused rather than becoming a miscellaneous application-state crate.

`packages/tui` owns terminal layout, terminal-event mapping, viewport and anchoring behavior, frame rendering, and terminal-specific polish. Terminal-specific plugin surfaces remain TUI-only. During migration it also temporarily retains legacy projection logic that should move behind the shared semantic boundary after parity is demonstrated.

`packages/hyperchad` owns the HyperChad application host, daemon connection, bounded snapshot hydration, session selection, semantic action mapping, and selected-backend integration. Backend selection flows from consuming package features into `bcode_hyperchad` and then into HyperChad.

`packages/hyperchad/ui` owns portable HyperChad presentation. It uses canonical HyperChad `container!`, `hx-*`, `fx-*`, route, form, action, responsive, and renderer APIs; backend implementations own those semantics. Plugin visuals, artifacts, and interaction snapshots have a generic structured-data fallback. Rich visual adapters are registered by exact plugin-owned `(schema, schema_version)` keys and must retain that fallback.

The initial HTML/Actix backend binds to loopback unless the CLI receives explicit non-loopback opt-in. Each launch generates a capability token; page and action routes validate it before reading daemon state or executing effects, and generated links/forms propagate it. This is a local companion security model, not by itself a production remote-access design.

### Non-loopback access review

The current access model has been reviewed specifically for non-loopback use and is **not approved as production remote access**. Explicit bind opt-in only bypasses the accidental-exposure guard; it does not upgrade the security model.

Current limitations are deliberate and blocking for production remote/mobile deployment:

* The launch capability is carried in URLs and generated form targets. URLs can leak through browser history, logs, copied links, screenshots, referrer handling, and intermediary diagnostics.
* The capability identifies a renderer launch, not an authenticated human or independently authorized client. There are no user identities, roles, per-session ACLs, or multi-user ownership checks.
* The host does not provide TLS termination, trusted-proxy validation, origin enforcement, or a complete cross-site request protection policy suitable for hostile networks.
* Capability rotation is launch-scoped; there is no remote-client enrollment, selective revocation, expiration policy, or security audit trail.
* The current controls do not promise production-grade abuse protection such as request limits, lockout, or externally observable security events.

Consequently:

* Loopback remains the supported default and the only production-safe mode claimed by this implementation.
* Non-loopback opt-in is for explicit development/testing environments whose network boundary is already trusted.
* Documentation and CLI wording must not describe the current opt-in as secure remote access.
* Production non-loopback support requires a separate design covering authenticated identity, authorization, secure token transport, TLS/proxy trust, CSRF/origin policy, revocation/expiry, auditability, and abuse controls before the product can claim it.

Plugins own domain schemas and renderer-neutral interaction controllers. A renderer may add rich schema-specific adapters, but it must preserve the generic fallback and must not move plugin behavior into renderer code.

## Target state and action flow

1. A renderer host connects through `BcodeClient`.
2. It requests bounded history plus renderer-relevant attached state.
3. `SessionView` builds a semantic `SessionViewSnapshot` and hydrates ephemeral daemon state, such as a pending interaction snapshot, through explicit bounded client APIs.
4. The host retains that view and applies live events cumulatively.
5. Gaps, reconnects, and resync requests replace or reconcile state from a trustworthy bounded snapshot.
6. Renderer UI code displays semantic state without interpreting persisted event logs directly.
7. User input is converted to `SessionViewAction` or `InteractionInput`.
8. Daemon-backed actions run through `execute_session_view_action`.
9. Renderer-local actions, such as switching the displayed session or changing a history window, remain in the host and request a new bounded snapshot.

Full snapshots are the correctness baseline. `SessionViewPatch` is an optional later optimization after identity, revision, reconnect, and cumulative-live semantics are stable.

Web updates use HyperChad's update/action mechanisms. Missing browser transport, routing, asset, or server capabilities belong upstream in HyperChad rather than in Bcode-specific JavaScript or WebSocket/SSE plumbing.

## Tool presentation slots

Tool contributions use a versioned `ToolContributionEnvelope` whose renderer-neutral placement is
`request`, `progress`, `result`, `supplemental`, or `hidden`. Placement is host composition
semantics, while the nested `ToolContributionEvent` remains an opaque producer-owned payload.
Legacy unplaced contributions are retained in semantic contribution state but are hidden from normal
transcript presentation.

`SessionView` owns stable slot identity:

* Request, progress, and result each have one replaceable slot per invocation. They coexist; a
  result does not erase request context, and canonical semantic `ToolInvocationResultRecorded`
  remains the authoritative invocation result card.
* Supplemental slots are independently keyed by contribution identity and retain event order.
* Hidden contributions have no transcript item.
* Renderers route visible payloads by producer, schema, and version. Unsupported payloads must not be
  exposed as serialized JSON in normal transcript UI.

Raw tool arguments remain available to permission, policy, audit, and explicit diagnostic paths, but
compact transcript requests do not render them. This contract applies equally to live events and
durable replay; renderers must not infer placement from tool names, schemas, or contribution IDs.

## TUI migration rules

The TUI migration should be incremental:

* Use [`session-view-event-coverage.md`](session-view-event-coverage.md) as the explicit durable/live event parity inventory.

* Compare TUI and shared projection with focused parity fixtures before removing established logic.
* Move generic transcript, tool, permission, interaction, runtime, and session semantics into the shared view.
* Keep terminal viewport, scroll anchoring, hit testing, input editing, cursor behavior, animation, layout, and native plugin surfaces in the TUI.
* Adapt semantic items into terminal presentation rather than exposing terminal types through shared crates.
* Remove duplicate projection and daemon effects only after relevant parity and UX tests pass.

The goal is not to make every renderer look or behave identically. The goal is for them to consume the same product semantics while retaining native presentation and interaction.

## Adding another renderer

A new renderer should start with `SessionViewSnapshot`, generic transcript/tool/artifact rendering, and semantic action mapping. It should add renderer-specific layout and input around that contract, then add rich visual adapters only where actual schemas need them.

A new renderer must not:

* Reuse `packages/tui` application state.
* Fork event projection or daemon-effect behavior.
* Depend on terminal drawing or event types.
* Assume plugin TUI surfaces are portable.
* Full-replay event logs on normal paths.
* Add custom browser/mobile transport inside Bcode when the renderer framework should own it.
