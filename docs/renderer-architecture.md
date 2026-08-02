# Renderer Architecture

Bcode's target renderer architecture uses a shared semantic session-view layer rather than session event logs or another renderer's UI state.

Reasoning content remains shared semantic state, while visibility, readable-representation selection (`all`, `summary`, or `raw`), disclosure, labels, and styling are frontend presentation policy. `SessionView` accepts a renderer/client-selected policy so each attached frontend can derive its own bounded projection; the policy never changes provider requests or durable history.

Plugin fallback remains contract-driven: `SessionView` transports invocation identity, lifecycle, typed results, artifacts, and opaque schema-versioned plugin payloads without interpreting tool domains. Each frontend first attempts its own plugin adapter and otherwise renders the bounded generic invocation/result contract.

TUI visual selection is an ordered frontend policy over manifest-declared routes. Stable adapter
references combine plugin ID and manifest adapter ID. Bundled/static native implementations are
registered by exact adapter ID; user-installed dynamic implementations execute through the
versioned serialized `bcode.tui-visual-adapter/v1` service. Its portable contract lives in
`bcode_plugin_sdk::tui_visual` and cannot depend on terminal, IPC, web, or plugin implementation
types. Dynamic work is bounded and precomputed outside frame rendering. The selected adapter returns
rows plus render-mode/header hints, but the TUI retains ownership of composition, timing/lifecycle
chrome, viewport, input, and paint. Adapter failure advances through the ordered candidates before
the canonical fallback.

Canonical transcript content crosses the single `SessionViewTerminalAdapter` boundary into the TUI. Runtime-work label composition and other terminal activity strings live in TUI modules rather than shared session-view models.

## Transcript eligibility catalog

Every `TranscriptViewItemKind` is intentional chronological product history:

* `UserMessage` — user-authored conversation content.
* `AssistantMessage` — assistant-authored conversation content.
* `ReasoningMessage` — supported legacy reasoning history whose richer source representation is unavailable.
* `ReasoningActivity` — structured assistant reasoning history with stable lifecycle and ordering.
* `ToolInvocation` — the one canonical primary history item for a tool invocation.
* `ToolRequestDraft` — live provisional request content occupying the canonical invocation position until superseded or removed.
* `ToolRequest` — supported historical request context retained after a result supersedes an older active representation.
* `Permission` — a user-visible authorization checkpoint and its resolution in the conversation flow.
* `Compaction` — a user-visible semantic explanation that model context was compacted.
* `Interaction` — a user-visible plugin interaction checkpoint and its resolution.
* `Skill` — user-visible skill activation, context, suggestion, or failure history.
* `SystemMessage` — shared product-authored conversation status, never renderer-local notices.
* `ToolContribution` — an explicitly placed plugin-owned history contribution with a portable fallback.

Usage, generic runtime work, provider progress, occupancy, active invocation progress, and renderer-local notices are deliberately absent from this catalog.

## Current migration status

The target boundary is active for tool transcript semantics in both established renderers:

* `packages/session-view/models` defines renderer-neutral snapshot, transcript, tool, permission, runtime-work, composer, interaction, visual, action, and patch contracts.
* `packages/session-view` projects bounded history and renderer-relevant live events and executes daemon-backed semantic actions.
* `packages/hyperchad` consumes this layer as Bcode's HyperChad application host; selected Cargo features choose the concrete HyperChad backend.
* `packages/hyperchad/ui` owns portable HyperChad presentation built from canonical HyperChad templates, routes, forms, actions, and renderer APIs.
* `packages/tui` retains a `SessionView` projection and adapts its transcript through one `SessionViewTerminalAdapter`, keyed by stable `TranscriptViewItemId` and revision, into terminal-native rows. `SessionView` is the sole semantic transcript authority: durable and live events are applied there once, while the TUI consumes the resulting document and does not construct assistant, reasoning, tool, permission, compaction, skill, interaction, or system rows from raw events. Usage and runtime-work remain non-transcript runtime state. Assistant and reasoning streams preserve terminal render identity across incremental replacement and durable finalization even when usage state, permission, supplemental, or tool updates are interleaved. Terminal viewport, scroll anchoring, hit testing, wrapping, diff layout, animation, and plugin visual/artifact adapter state remain TUI-owned.

The TUI may retain renderer-local interaction and layout state, but shared transcript lifecycle,
identity, ordering, retention, fallback semantics, and stream integrity come from `SessionView`.
Terminal-local notices created by TUI commands remain presentation state and are not canonical
session transcript facts. The TUI must not create a parallel event-derived transcript authority.
Remaining work is product-parity validation and removal of superseded compatibility interfaces, not
another transcript reconciliation layer.
of proven duplicate adapter/projection helpers, not a different semantic contract.

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

`packages/tui` owns terminal layout, terminal-event mapping, viewport and anchoring behavior, frame rendering, and terminal-specific polish. Terminal-specific plugin surfaces remain TUI-only. Canonical transcript content crosses the single `SessionViewTerminalAdapter` boundary; raw event handling may update terminal interaction/status state but cannot create or finalize shared semantic rows.

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

Native plugin surfaces use the same boundary through `PluginTuiHost::subscribe_session_view`.
The renderer host owns bounded projection attachment and one `SessionView` per observed session; it
emits complete schema-versioned `SessionViewSnapshot` values and replaces the view after reconnect
or resynchronization. Plugin surfaces may adapt those semantic snapshots into domain-owned compact
presentation, but they never receive or reinterpret raw durable/live session events. Several
plugin-domain anchors that reference one session share the same observer state.

Web updates use HyperChad's update/action mechanisms. Missing browser transport, routing, asset, or server capabilities belong upstream in HyperChad rather than in Bcode-specific JavaScript or WebSocket/SSE plumbing.

## Live request drafts and execution progress

`SessionView` is the renderer boundary for both live request assembly and execution progress.
Renderer code does not consume provider deltas or server registries directly.

```text
SessionLiveEvent request-draft append/checkpoint ─┐
placed transient progress upsert/remove ──────────┤
attach/resync active checkpoint ──────────────────┘
                         -> SessionView stable item ID/revision
                         -> TUI or HyperChad native presentation
```

## Updateable transcript document

`SessionView` owns the authoritative renderer-neutral transcript document. Every visible item has a
stable `TranscriptViewItemId`, a monotonic item revision, and a fixed insertion position. Semantic
changes replace the item with the same identity; they do not remove and append a second item merely
because content was produced incrementally or an operation completed.

`SessionViewPatch` carries the required base document revision and resulting revision. A renderer
applies append, replace, and remove operations only when its current revision equals the patch base.
Replacements must advance the item revision and preserve position. Retained-item reordering,
bounded-window metadata changes, duplicate identities, or non-append insertion fall back to a
bounded authoritative reset. Renderers that miss a patch must resynchronize from that reset/snapshot
rather than retaining partially synchronized semantic state.

The target tool contract reserves one primary identity per invocation. Request metadata, current
plugin-owned visual payload, canonical model-visible result, lifecycle, and timing are fields of that
one semantic item rather than separate request/progress/result transcript cards. Repeated output is
an update to the current item. Completion closes the invocation update scope and preserves the last
accepted retained presentation; there is no separate visual promotion mode. Independently meaningful
supporting output may use explicitly keyed supplemental identities.

Plugin payloads remain opaque and versioned. Renderers route them by producer, schema, and version,
but do not infer lifecycle, timing, retention, or transcript identity from tool names or payloads.
Unknown schemas use compact bounded fallback content and never expose raw argument or contribution
JSON in normal transcript UI.

Renderer-specific frame cadence, viewport anchoring, animation, hit testing, layout, and paint
invalidation remain renderer-owned. Semantic generation, ordering, revision acceptance, closure,
cleanup, and persistence classification do not.

## Update and closure semantics

Invocation lifecycle is host-owned and independent from presentation updates. An open invocation
accepts only authorized monotonically newer updates. Terminal closure is absorbing: it fixes outcome
and duration, flushes already accepted updates, rejects later updates, and cannot remove the primary
item. Tool elapsed invalidation is allowed only while authoritative lifecycle is active or waiting;
terminal presentation never derives timing from a generic transcript `streaming` flag.

Presentation retention is orthogonal to whether an update was observed while work was active:

* **Retain latest:** keep one bounded current value and checkpoint the latest accepted value at the
  terminal boundary. Primary shell, file-change, Vim edit, and other history-worthy output use this
  policy.
* **Active only:** keep bounded state only while the invocation is open and remove it at closure.
  Sensitive argument drafts, spinners, and intentionally ephemeral diagnostics use this narrowly.
* **Durable supplemental:** retain explicitly independent supporting output under its own stable
  supplemental identity.

Intermediate retained-latest frames are supersedable transport state, not append-only session
history. Attach/reconnect hydrates the current bounded open checkpoint plus its generation/revision.
Durable replay reconstructs the latest terminal checkpoint. The final live document, immediate
reconnect document, and replayed closed document must converge to the same semantic item.

## Legacy contribution-slot compatibility

Persisted `ToolContributionEnvelope` placement (`request`, `progress`, `result`, `supplemental`, or
`hidden`) remains a decode/replay compatibility fact for supported historical sessions. Historical events are
applied chronologically into the current invocation presentation; the latest compatible primary
update wins, supplemental identities remain independent, and hidden/unplaced payloads stay absent
from normal transcript UI. New primary presentation APIs must not require plugins to coordinate
separate request, progress, result, promotion, and removal objects.

Request drafts remain bounded, live-only transport facts. During migration, their declared legacy
placement identifies which old slot they update, but the target projection adapts them into the
invocation's current primary presentation. Gaps, lag, reconnect, or truncation require a bounded
replacement checkpoint. Complete request arguments continue to serve permission, policy, audit, and
model execution independently from compact transcript presentation.

## State Authority

Bcode's daemon event log and `SessionView` projection remain the sole authority for session and interaction state. HyperChad receives projected snapshots and routes semantic user intent back to Bcode; its optional shared-state persistence must not journal or independently reconstruct Bcode sessions. Using it as a second store would create conflicting revisions, duplicate recovery rules, and a second repair surface.

## TUI migration rules

The TUI migration should be incremental:

* Use [`session-view-event-coverage.md`](session-view-event-coverage.md) as the explicit durable/live event parity inventory.

* Compare TUI and shared projection with focused parity fixtures before removing established logic.
* Move generic transcript, tool, permission, interaction, runtime, and session semantics into the shared view.
* Keep terminal viewport, scroll anchoring, hit testing, input editing, cursor behavior, animation, layout, and native plugin surfaces in the TUI.
* Adapt semantic items into terminal presentation rather than exposing terminal types through shared crates.
* Remove duplicate projection and daemon effects only after relevant parity and UX tests pass.

The goal is not to make every renderer look or behave identically. The goal is for them to consume the same product semantics while retaining native presentation and interaction.

New primary presentation schemas are added at the plugin boundary: define a bounded versioned payload,
declare the exact schema/version and supported renderer surfaces in the plugin manifest, publish
invocation-owned updates through the presentation handle, and provide renderer-native adapters only
where canonical lifecycle/result fallback is insufficient. Each adapter must reject malformed or
unsupported payloads without exposing opaque data, and manifest inventory/conformance tests must be
updated with synthetic fixtures.

## Adding another renderer

A new renderer consumes bounded `SessionViewSnapshot` values and `SessionViewPatch` updates by stable
item identity and revision. It must render canonical tool name, lifecycle, timing, bounded arguments,
typed result or result text, and safe artifact metadata even when it implements no plugin-specific
adapter. Presentation schemas are optional enhancement hooks; therefore every tool invocation,
including dynamic-plugin invocations, must retain a meaningful bounded canonical fallback and must
not rely on its opaque presentation payload as the only useful transcript result. Renderer code must
not parse another renderer's rows/DOM, replay event logs, infer lifecycle from payloads, or depend on
TUI state.
Renderer-native actions map through `SessionViewAction`/`PresentationContext` equivalents, while
viewport, layout, interaction mechanics, and resource delivery remain owned by that renderer.

## Adding another HyperChad backend

A new HyperChad backend should reuse `bcode_hyperchad_ui`, the `SessionViewSnapshot` contract, semantic action mapping, and generic renderer publication. Bcode should add only the consuming Cargo feature propagation and genuinely backend-specific startup/configuration required to construct the selected HyperChad renderer. Canonical `hx-*`, `fx-*`, form, route, and update semantics remain the responsibility of HyperChad backends.

A new backend must not:

* Create a parallel Bcode presentation stack for semantics already expressed by `bcode_hyperchad_ui`.
* Reuse `packages/tui` application state.
* Fork event projection or daemon-effect behavior.
* Depend on terminal drawing or event types.
* Assume plugin TUI surfaces are portable.
* Full-replay event logs on normal paths.
* Add custom browser/mobile/native transport inside Bcode when HyperChad should own it.
