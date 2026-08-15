# Session View Event Coverage Matrix

This matrix tracks renderer-semantic coverage between the renderer-neutral `SessionView` projection and the established TUI event application in `BmuxApp`. It is intentionally explicit: an event is not considered shared merely because its model variant exists. The current matrix has no rows classified **Partial** or **Missing**; those definitions remain so future event additions must be classified honestly.

Status meanings:

* **Complete**: shared projection represents the renderer-relevant semantics currently required by renderers.
* **Partial**: shared projection represents some semantics, but the TUI or daemon attachment exposes additional renderer-relevant state.
* **Missing**: renderer-relevant semantics exist in the TUI or host flow but are absent from the shared projection.
* **Intentional no-op**: the event has no renderer-semantic state to share at present.

## Live stream presentation cadence

`AssistantTextStreamUpdated`, `AssistantReasoningTextStreamUpdated`, and their legacy delta adapters
always update canonical generation/revision/offset integrity immediately. A configured frontend may
temporarily expose only a grapheme-safe prefix through `SessionView`; accepted bytes and terminal
state remain authoritative and presentation prefixes are never durable history. Checkpoints,
generation replacement, durable finalization, model-turn completion, history-window rebuild, and
runtime disablement reconcile pending presentation directly. TUI and HyperChad schedule the same
shared deadlines and must not independently split chunks or evaluate interpolation curves.

## Updateable tool transcript migration

The coverage statuses below describe the implemented shared projection. `SessionView` is now the sole
semantic transcript authority consumed by the TUI and portable HyperChad frontend. The TUI adapts
shared items through one stable-ID/revision terminal boundary; raw durable/live event handling is
limited to terminal interaction, status, and activity side effects and cannot construct canonical
transcript rows.

The target composition for tools is one stable primary transcript item per invocation, updated by
monotonic revision and closed without a separate final presentation object. Provider output position
refines ordering only within a contiguous run of positioned items from the same application turn;
unpositioned rows and rows from another turn preserve canonical transcript boundaries. Existing
request/progress/result placement rows remain supported-history compatibility coverage until an
explicit version cutoff; they are not the target extension contract.

Required migration invariants:

* request metadata, current plugin visual, lifecycle/timing, and canonical model result converge in
  one invocation-owned primary item;
* completion preserves the latest retained presentation and rejects later updates;
* active-only request/progress state is absent from durable replay;
* TUI and HyperChad consume revision-checked shared patches/snapshots rather than independently
  resolving source precedence;
* final live, reconnect, and durable replay projection agree for closed invocations.

## Durable `SessionEventKind` coverage

| Event | Shared semantic state | Transcript eligibility | Frontend-native presentation | Status |
|---|---|---|---|---|
| `SessionCreated` | Session title and working directory. | non-transcript | Header and session metadata. | **Complete**. |
| `ClientAttached` | None. | none | None. | **Intentional no-op**. |
| `ClientDetached` | None. | none | None. | **Intentional no-op**. |
| `UserMessage` | Canonical user message and derived title when absent. | transcript | Input history, pending-submission reconciliation, and viewport navigation. | **Complete**. |
| `AssistantDelta` | Legacy in-flight assistant text. | transcript | Streaming cadence and viewport behavior. | **Complete** for legacy history. |
| `AssistantMessage` | Legacy complete assistant message. | transcript | Markdown projection and viewport behavior. | **Complete** for legacy history. |
| `ToolCallRequested` | Canonical invocation request metadata under one stable tool identity. | transcript | Plugin visual adaptation, fallback rows, and activity chrome. | **Complete**. |
| `PermissionRequested` | Typed pending permission and tool correlation. | transcript | Native permission dialog and terminal input handling. | **Complete**. |
| `PermissionResolved` | Typed terminal permission decision. | transcript | Resolution styling and dialog closure. | **Complete**. |
| `ModelChanged` | Provider and model selection. | non-transcript | Header, picker, and footer labels. | **Complete**. |
| `SystemMessage` | Shared product-authored system message. | transcript | Text-format rendering and layout. | **Complete**. |
| `AgentChanged` | Selected agent identity. | non-transcript | Header, theme, and picker presentation. | **Complete**. |
| `ModelTurnStarted` | Active turn identity and cancellation state. | non-transcript | Activity chrome and submission controls. | **Complete**. |
| `ModelFeatureFidelityNegotiated` | Durable diagnostic record of negotiated provider/model feature mechanism and fidelity. | non-transcript | No presentation; available to bounded diagnostics and inspection. | **Complete**. |
| `ModelTurnFinished` | Terminal turn outcome and message; error outcomes also publish a shared failure message. | transcript | Activity settlement and error-message rendering. | **Complete**. |
| `ModelUsage` | Latest usage and cumulative metered tokens. | non-transcript | Footer and accounting display. | **Complete**. |
| `ContextCompacted` | Shared compaction history note. | transcript | Compaction styling and layout. | **Complete**. |
| `SessionRenamed` | Session title. | non-transcript | Header and catalog title presentation. | **Complete**. |
| `TraceEvent` | None in `SessionView`; trace data is not transcript semantics. | none | TUI diagnostics and transient activity side effects. | **Intentional no-op** in shared projection. |
| `SkillInvoked` | Typed skill invocation history. | transcript | Skill-note styling. | **Complete**. |
| `SkillSuggested` | Typed skill suggestion history. | transcript | Skill-note styling. | **Complete**. |
| `SkillActivated` | Typed active-skill state and history. | transcript | Activity and skill-note styling. | **Complete**. |
| `SkillDeactivated` | Typed terminal skill state and history. | transcript | Skill-note styling. | **Complete**. |
| `SkillContextLoaded` | Typed skill context state and history. | transcript | Skill-note styling. | **Complete**. |
| `SkillInvocationFailed` | Typed skill failure history. | transcript | Failure styling. | **Complete**. |
| `AssistantReasoningDelta` | Legacy in-flight reasoning text. | transcript | Visibility, representation, disclosure, and streaming cadence. | **Complete** for legacy history. |
| `AssistantReasoningMessage` | Legacy complete reasoning text. | transcript | Visibility, representation, and disclosure. | **Complete** for legacy history. |
| `RuntimeWorkStarted` | Typed active runtime work. | non-transcript | Activity and cancellation surfaces. | **Complete**. |
| `RuntimeWorkCancelRequested` | Typed cancelling runtime work. | non-transcript | Cancellation activity. | **Complete**. |
| `RuntimeWorkFinished` | Removes active work and records an internal stale-revival tombstone. | non-transcript | Activity settlement. | **Complete**. |
| `RuntimeWorkProgress` | Typed active-work progress. | non-transcript | Activity detail and progress display. | **Complete**. |
| `ModelTurnCancelRequested` | Active turn cancellation state. | non-transcript | Cancellation activity and controls. | **Complete**. |
| `WorkingDirectoryChanged` | Working directory and shared path-safety warning. | transcript | Header path plus warning layout. | **Complete**. |
| `SessionImported` | None in the session view; provenance remains catalog/session metadata-owned. | none | Catalog provenance when available. | **Intentional no-op**. |
| `SessionDerived` | Generic lineage remains session/application metadata; renderers do not reinterpret copied history. | none | Catalog or plugin-owned presentation when contributed. | **Intentional no-op**. |
| Historical `session_forked` | Not a current session event. Classification/decoding is migration-owned and no old-to-new lineage conversion occurs. | none | Explicit compatibility or migration diagnostics only. | **Rejected by current runtime**. |
| `RalphLifecycle` | Portable loop lifecycle history. | transcript | Status-note styling and plugin status refresh. | **Complete**. |
| `ReasoningChanged` | Reasoning effort and summary selection. | non-transcript | Frontend-selected labels and controls. | **Complete**. |
| `ToolExchangeRequested` | Typed pending interaction correlated to its invocation. | transcript | Plugin-owned interaction adapter and native controls. | **Complete**. |
| `ToolExchangeResolved` | Typed terminal interaction resolution. | transcript | Resolution presentation and surface closure. | **Complete**. |
| `ProviderContextCompacted` | Provider/model compaction history and cleared stale occupancy. | transcript | Compaction styling and footer refresh. | **Complete**. |
| `RequestContextObserved` | Authoritative request-context occupancy with stale-update rejection. | non-transcript | Footer context accounting. | **Complete**. |
| `PluginStatusNote` | Plugin/note-keyed portable status and stable shared history item. | transcript | Plugin label and status-note styling. | **Complete**. |
| `InertHistory` | None; migration-retained payload is deliberately non-semantic. | none | None. | **Intentional no-op**. |
| `ToolInvocationLifecycle` | Host-owned invocation lifecycle and timing on the canonical invocation aggregate. | transcript | Plugin visual adaptation, activity, and timing layout. | **Complete**. |
| `ToolContribution` | Opaque plugin contribution retained under host-owned invocation identity. | transcript | Plugin adapter or generic portable fallback according to placement. | **Complete**. |
| `ToolInvocationResultRecorded` | Canonical typed result and terminal invocation state. | transcript | Plugin adapter or generic result fallback. | **Complete**. |
| `ToolContributionPlaced` | Opaque plugin contribution with explicit semantic placement. | transcript | Plugin adapter or generic portable fallback according to placement. | **Complete**. |
| `ExecutionSessionCreated` | None in transcript/session-view state; execution provenance is runtime metadata. | none | Native execution diagnostics when available. | **Intentional no-op**. |
| `AssistantReasoningActivity` | Stable structured reasoning lifecycle and readable provider parts. | transcript | Frontend-selected visibility, representation, disclosure, and styling. | **Complete**. |
| `AssistantResponseSegment` | Stable complete assistant segment and turn-local order. | transcript | Markdown projection and viewport behavior. | **Complete**. |
| `PositionedAssistantResponseSegment` | Stable assistant segment with provider-authoritative cross-type position. | transcript | Markdown projection and viewport behavior. | **Complete**. |
| `PositionedAssistantReasoningActivity` | Stable structured reasoning with provider-authoritative cross-type position. | transcript | Frontend-selected visibility, representation, disclosure, and styling. | **Complete**. |
| `PositionedToolCallRequested` | Canonical invocation request with provider-authoritative cross-type position. | transcript | Plugin visual adaptation, fallback rows, and activity chrome. | **Complete**. |

## Live `SessionLiveEventKind` coverage

| Event | Shared semantic state | Transcript eligibility | Frontend-native presentation | Status |
|---|---|---|---|---|
| `AssistantTextStreamUpdated` | Revision-checked assistant stream state and stable segment identity. | transcript | Streaming cadence, Markdown projection, and viewport behavior. | **Complete**. |
| `AssistantTextDelta` | Legacy in-flight assistant text. | transcript | Streaming cadence and viewport behavior. | **Complete** for compatibility. |
| `AssistantReasoningTextStreamUpdated` | Revision-checked structured reasoning-part text. | transcript | Frontend-selected visibility, representation, disclosure, and cadence. | **Complete**. |
| `AssistantReasoningDelta` | Legacy in-flight reasoning text. | transcript | Visibility, disclosure, and streaming cadence. | **Complete** for compatibility. |
| `AssistantReasoningActivity` | Incremental stable reasoning lifecycle. | transcript | Frontend-selected visibility, representation, disclosure, and styling. | **Complete**. |
| `ToolContributionPlaced` | Opaque plugin contribution with explicit semantic placement. | transcript | Plugin adapter or generic portable fallback. | **Complete**. |
| `ToolPresentationUpdated` | Revision-checked plugin presentation on the canonical invocation or supplemental identity. | transcript | Renderer-specific plugin adapter or generic fallback. | **Complete**. |
| `RequestContextOccupancyChanged` | Authoritative current occupancy with stale-update rejection. | non-transcript | Footer context accounting. | **Complete**. |
| `ToolInvocationProgress` | Active invocation progress while the invocation remains open. | non-transcript | Activity chrome and plugin presentation refresh. | **Complete**. |
| `ProviderStreamProgress` | Turn-correlated provider progress and retry timing. | non-transcript | Activity chrome, timers, and animation. | **Complete**. |
| `ToolRequestDraft` | Revision-checked live request assembly under the canonical tool identity. | transcript | Plugin-native provisional visual or bounded generic fallback. | **Complete**. |

## Migration order derived from the matrix

1. Build parity fixtures for all rows marked **Complete** before moving TUI consumers and removing duplicate projection.

The matrix must be updated whenever a new `SessionEventKind` or `SessionLiveEventKind` variant is added. `scripts/check-session-architecture.sh` enforces one explicit row per variant and requires each row to classify shared semantic state, transcript eligibility, and frontend-native presentation.
