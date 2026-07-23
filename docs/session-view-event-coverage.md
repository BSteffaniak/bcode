# Session View Event Coverage Matrix

This matrix tracks renderer-semantic coverage between the renderer-neutral `SessionView` projection and the established TUI event application in `BmuxApp`. It is intentionally explicit: an event is not considered shared merely because its model variant exists. The current matrix has no rows classified **Partial** or **Missing**; those definitions remain so future event additions must be classified honestly.

Status meanings:

* **Complete**: shared projection represents the renderer-relevant semantics currently required by renderers.
* **Partial**: shared projection represents some semantics, but the TUI or daemon attachment exposes additional renderer-relevant state.
* **Missing**: renderer-relevant semantics exist in the TUI or host flow but are absent from the shared projection.
* **Intentional no-op**: the event has no renderer-semantic state to share at present.

## Durable `SessionEventKind` coverage

| Event | `SessionView` behavior | Established TUI behavior | Shared status / next action |
|---|---|---|---|
| `SessionCreated` | Sets title and working directory. | Header/session metadata consumes the shared projection. | **Complete** for event semantics. |
| `ClientAttached` | No-op. | No-op. | **Intentional no-op**. |
| `ClientDetached` | No-op. | No-op. | **Intentional no-op**. |
| `UserMessage` | Derives a title when absent and appends a semantic user message. | Clears terminal-local tool presentation, updates input history/submission/anchor state, derives a title, and appends a terminal transcript item. | **Complete** for renderer semantics; input history, optimistic submission reconciliation, anchoring, and terminal presentation caches remain renderer-local. |
| `AssistantDelta` | Appends to an in-flight assistant message. | Appends to the terminal assistant stream and updates anchoring. | **Complete** for semantic content. |
| `AssistantMessage` | Finalizes/replaces the in-flight assistant message without changing its semantic identity. | Replaces terminal presentation by shared `TranscriptViewItemId`, retaining renderer-local identity and finishing anchoring/activity. | **Complete** for semantic content and canonical projection consumption. |
| `ToolCallRequested` | Updates `ToolInvocationProjection`, tool map, and transcript tool item. | Updates tool contexts, active calls, activity, and transcript presentation. | **Complete** for generic tool semantics; terminal live-preview/activity behavior stays TUI-owned. |
| `ToolCallFinished` | Updates the shared tool item and semantic/raw result. | Consumes the generic lifecycle/result while updating terminal activity, artifact retrieval, and presentation caches. | **Complete** for renderer semantics; artifact transport, activity transitions, and terminal caches remain renderer/host concerns. |
| `ToolInvocationResultRecorded` | Finalizes the shared tool item from the canonical renderer-neutral semantic result record. | Consumes the shared result, routes artifact schemas through plugin TUI adapters, and replaces generic request/result presentation. | **Complete** for current generic tool-result semantics. |
| `ToolContribution` / `ToolContributionPlaced` | Retains legacy/unplaced and explicitly placed plugin-owned contribution state. Legacy/unplaced and hidden contributions create no transcript item. Request, progress, and result placements use one stable slot per invocation; supplemental placement uses independently keyed stable slots. | Routes supported producer/schema/version payloads through retained plugin TUI registries. Compact requests and unsupported contributions never expose raw arguments or contribution JSON in normal transcript UI. | **Complete** for renderer-neutral placement and hidden legacy semantics. |
| `PermissionRequested` | Adds permission state and a permission transcript item; renderer hosts hydrate authoritative pending permissions including batch, policy, tool, and agent metadata. | Native permission dialog consumes the same shared permission model while retaining terminal-only focus/input behavior. | **Complete** for event and attach semantics. |
| `PermissionResolved` | Updates both permission side state and its transcript item. | Terminal permission-result presentation and tool-call correlation consume the resolved shared permission item. | **Complete** for event semantics. |
| `ModelChanged` | Updates selected provider and requested/effective model state. | Updates selected provider/model and model display state. | **Complete** for durable selection semantics; authoritative attach hydration comes from `SessionModelStatus`. |
| `SystemMessage` | Appends a semantic system transcript item. | Appends a system transcript item. | **Complete**. |
| `AgentChanged` | Updates selected agent state. | Updates current agent selection. | **Complete** for durable selection semantics. |
| `ModelTurnStarted` | Records active turn identity and clears cancellation state. | Sets live activity to preparing a model request. | **Complete** for semantic turn identity; renderer activity presentation remains local. |
| `ModelTurnFinished` | Clears matching active turn, records outcome/message, and emits a system error item for failures. | Finishes turn activity and appends an error system message when needed. | **Complete** for semantic outcome/status. |
| `ModelUsage` | Stores most recently observed provider-neutral usage, cumulative metered tokens, and a semantic usage transcript item. | Terminal usage presentation and footer spend-token accounting consume shared usage; pricing/cache/request-trace enrichment remains terminal-local. | **Complete** for generic accounting and transcript semantics; richer renderer presentation remains local. |
| `ContextCompacted` | Appends a semantic context-compaction system item. | Appends compaction transcript presentation. | **Complete** for portable semantic status. |
| `SessionRenamed` | Updates title. | Header consumes shared title metadata and keeps terminal-local status feedback. | **Complete** for persistent session metadata. |
| `TraceEvent` | No-op. | Applies live trace telemetry only; replay ignores it. | **Intentional no-op** for shared transcript until a concrete cross-renderer trace view is required. |
| `SkillInvoked` | Appends a renderer-neutral skill invocation status item. | Appends a skill transcript item. | **Complete** for semantic status. |
| `SkillSuggested` | Appends a suggestion status item when a reason exists. | Appends a suggestion when a reason exists. | **Complete**. |
| `SkillActivated` | Adds the skill to renderer-neutral active-skill state. | Consumes the shared active-skill set for counts and keeps a terminal-local status message. | **Complete** for semantic state. |
| `SkillDeactivated` | Removes the skill from renderer-neutral active-skill state. | Consumes the shared active-skill set for counts and keeps a terminal-local status message. | **Complete** for semantic state. |
| `SkillContextLoaded` | Appends renderer-neutral source/size/truncation/preview status. | Updates skill context status. | **Complete** for user-visible semantics. |
| `SkillInvocationFailed` | Appends renderer-neutral skill failure status. | Appends a skill error item. | **Complete**. |
| `AssistantReasoningDelta` | Appends reasoning content and updates thinking state without overriding renderer-selected visibility. | Consumes shared visibility while updating the terminal reasoning stream. | **Complete** for semantic content and visibility. |
| `AssistantReasoningMessage` | Finalizes reasoning content and thinking state while preserving split semantic identities. | Replaces shared-identity terminal reasoning items subject to renderer visibility. | **Complete** for semantic content and canonical projection consumption. |
| `RuntimeWorkStarted` | Upserts typed runtime-work state (kind, stable label, status, cancellability) and adds its transcript item. | Consumes the shared projection for live activity; authoritative attach/reconnect snapshots replace shared active state. | **Complete** for generic runtime semantics. |
| `RuntimeWorkCancelRequested` | Marks shared runtime work as cancelling while retaining its semantic label/progress. | Consumes the shared projection for cancelling activity. | **Complete** for generic runtime semantics. |
| `RuntimeWorkFinished` | Removes work from shared active state, preserves a terminal transcript item, and blocks late revival. | Consumes the shared generic projection and lets Ralph-specific terminal behavior remain plugin/TUI-owned. | **Complete** for generic renderer semantics; Ralph-specific behavior intentionally does not belong in the shared projection. |
| `RuntimeWorkProgress` | Updates shared message and progress units. | Consumes the shared projection for live activity. | **Complete** for generic runtime semantics. |
| `ModelTurnCancelRequested` | Records active turn identity and cancellation state. | Marks the active turn as cancelling and updates status. | **Complete** for semantic turn cancellation state. |
| `ToolInvocationStream` | Updates tool projection/output and appends generic visual updates. | Updates terminal output, preview, artifacts, and visual state. | **Complete** for generic stream semantics; terminal preview/artifact rendering remains renderer-specific. |
| `WorkingDirectoryChanged` | Updates working directory and appends the path-safety warning as a semantic system item. | Consumes shared working-directory metadata and warning text while retaining terminal-local status formatting. | **Complete** for renderer semantics. |
| `SessionImported` | No-op. | No-op in `BmuxApp`. | **Intentional no-op**; provenance remains session metadata/catalog-owned. |
| `SessionForked` | No-op. | No-op in `BmuxApp`. | **Intentional no-op**; provenance remains session metadata/catalog-owned. |
| `RalphLifecycle` | Appends portable Ralph lifecycle status. | Appends a Ralph system message; host also refreshes plugin status. | **Complete** for durable semantic status. |
| `ReasoningChanged` | Updates reasoning effort/summary selection. | Updates reasoning effort/summary selection and label. | **Complete** for selection metadata; renderer label presentation remains local. |
| `InteractiveToolRequestCreated` | Adds `InteractionViewSummary`, including owner surface kind and request snapshot, and a transcript interaction item. | Native surface opening consumes the shared interaction model after projection; native surface rendering/input remains terminal-only. | **Complete** for durable generic metadata and live opening semantics. |
| `InteractiveToolRequestResolved` | Marks the interaction resolved, stores resolution JSON, and updates its transcript item; authoritative hydration removes stale unresolved interactions. | Native surface lifecycle consumes the shared projected request while terminal-only rendering/input stays local. | **Complete** for durable and hydrated generic semantics. |
| `ProviderContextCompacted` | Clears stale occupancy and appends portable provider/model compaction status. | Appends provider compaction presentation. | **Complete** for portable semantic status. |
| `RequestContextObserved` | Projects authoritative occupancy from the durable observation with epoch/sequence stale-update rejection. | Footer context accounting consumes shared occupancy; attach/model-status hydration updates the same shared state. | **Complete** for authoritative occupancy semantics. |
| `PluginStatusNote` | Upserts plugin/note-keyed structured status and one stable transcript item. | TUI transcript projection renders durable plugin status and the statusline consumes shared hydrated plugin-status semantics. | **Complete** for durable and hydrated status-note semantics. |
| `LegacyEvent` | No-op. | Compatibility-only event with no application behavior. | **Intentional no-op**. |

## Live `SessionLiveEventKind` coverage

| Live event | `SessionView` behavior | Established TUI behavior | Shared status / next action |
|---|---|---|---|
| `AssistantTextDelta` | Cumulatively updates the active assistant item with stable semantic identity. | Upserts terminal presentation by shared `TranscriptViewItemId` while retaining renderer-local identity and anchoring. | **Complete** for semantic content and canonical projection consumption. |
| `AssistantReasoningDelta` | Cumulatively updates reasoning item and thinking state without overriding renderer-selected visibility. | Upserts terminal reasoning presentation by shared `TranscriptViewItemId` while preserving split-stream boundaries and anchoring. | **Complete** for semantic content, visibility, and canonical projection consumption. |
| `ToolOutputDelta` | Applies the stream event to shared tool projection/output. | Updates terminal tool output and viewport. | **Complete** for generic semantic output. |
| `ToolArgumentPreview` | Replaces one tool-call-keyed plugin visual preview with stable identity. | Updates terminal live preview and viewport. | **Complete** for semantic preview state; viewport behavior remains renderer-owned. |
| `RequestContextOccupancyChanged` | Replaces authoritative current occupancy while rejecting stale epoch/sequence updates. | Footer context accounting consumes shared occupancy. | **Complete**. |
| `ProviderStreamProgress` | Projects turn-correlated human-readable progress and retry timing. | Live provider-stream activity consumes the shared projected detail/timing; trace-only diagnostics remain terminal-local. | **Complete** for semantic progress; animation/timers remain renderer-owned. |

## Migration order derived from the matrix

1. Build parity fixtures for all rows marked **Complete** before moving TUI consumers and removing duplicate projection.

The matrix must be updated whenever a new `SessionEventKind` or `SessionLiveEventKind` variant is added. Architecture tests should eventually enforce that every variant is explicitly classified.
