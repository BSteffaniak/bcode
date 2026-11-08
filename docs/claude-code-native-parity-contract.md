# Claude Code native-parity shared contract

This document records the shared types, helpers and storage fences that let a managed
external harness session (Claude Code via `plugins/claude-code-plugin`) present like a native
Bcode session: streaming assistant text, reasoning, tool cards with inputs and results, usage,
native permission/question dialogs, mid-session model and permission-mode changes, and recovery
after daemon restart.

Every type below is a portable contract. Behavior (normalization, publication, rendering,
dispatch) is owned by the producing/consuming domains. Native observed tools never dispatch
Bcode tools; native text, reasoning, tool input and tool output are untrusted display content.

## 1. Observation variants

`bcode_harness_models::HarnessObservationKind` (`packages/harness/models/src/lifecycle.rs`):

| Variant | Fields | Bounds (`HarnessObservation::validate`) |
| --- | --- | --- |
| `Text` | `text: String`, `segment_id: Option<String>` (serde default, skipped when `None`) | text ≤ `MAX_HARNESS_TEXT_BYTES`; segment is an identity (≤ `MAX_HARNESS_ID_BYTES`, no control/padding) |
| `Tool` | `native_tool_id`, `name`, `status`, `input_json: Option<String>` (serde default, skipped when `None`) | `input_json` ≤ `MAX_HARNESS_TOOL_INPUT_BYTES` (16 KiB) and must parse as JSON |
| `Reasoning` (new) | `segment_id: Option<String>`, `text: String`, `opaque: bool` | as `Text`; `opaque` marks redacted reasoning whose text is a placeholder |
| `ToolResult` (new) | `native_tool_id: String`, `output: String`, `is_error: bool`, `truncated: bool` | tool id is an identity; output ≤ `MAX_HARNESS_TEXT_BYTES` |
| `Usage` (new) | `input_tokens`, `output_tokens`, `cache_read_input_tokens`, `cache_creation_input_tokens: u64`, `total_cost_micros: Option<u64>`, `duration_ms: Option<u64>`, `model: Option<String>` | `model` is an identity; optional fields are skipped when `None` |

Wire tags are snake_case: `"text"`, `"tool"`, `"reasoning"`, `"tool_result"`, `"usage"`.
Absent optional fields keep the pre-existing wire shape, so older observations and their
retention digests are unchanged.

Behavior at producers and consumers:

* `plugins/claude-code-plugin` emits every variant and optional field; see the "Native parity"
  section of `plugins/claude-code-plugin/README.md`. Durable `Text`/`Reasoning` segment ids
  are `<message.id>:<n>` (per-message block counter matching stream-event block indices),
  `ToolResult.output` may end with a truncation marker, `Tool.input_json` may be a bounded
  `{"truncated":true,"original_bytes":N,"preview":"…"}` summary, and `Usage.model` may be absent.
* SessionView projects the variants onto ordinary transcript items (assistant messages,
  reasoning activities, generic tool cards, runtime usage); see "Native-like transcript
  projection" in `docs/managed-external-sessions.md`. `bcode_session_view::external` keeps the
  generic fallback text (`native_display_content` in `packages/session-view/src/external.rs`).
* Native `Usage` is display-only runtime usage keyed `external:{generation}:{observation_id}`.
  The daemon usage summary does not fold it in, so SessionView keeps these entries across
  summary refreshes.
* Session search excludes the new variants (`packages/session-search/src/projection.rs`).
* Retention inspectors account the new strings (`packages/session/models/src/retention_external.rs`).

## 2. Interaction display metadata

`bcode_harness_models::HarnessInteraction` gains (both serde default, skipped when `None`):

* `tool_name: Option<String>` — identity bounds (≤ `MAX_HARNESS_ID_BYTES`).
* `input_preview: Option<String>` — ≤ `MAX_HARNESS_PREVIEW_BYTES` (8 KiB), untrusted preview.

Both are display only and never authorize or dispatch anything.

## 3. Live sidecar (transient)

`bcode_harness_models::HarnessObservationPage` (`packages/harness/models/src/service.rs`)
gains `live: Vec<HarnessLiveUpdate>` (serde default, skipped when empty).

Defined in `packages/harness/models/src/live.rs`:

```rust
pub enum HarnessLiveStream { Text, Reasoning }          // snake_case
pub struct HarnessLiveUpdate {
    pub stream: HarnessLiveStream,
    pub segment_id: String,   // identity bounds
    pub offset: u64,          // UTF-8 byte offset of `text` within the segment's live text
    pub text: String,         // ≤ MAX_HARNESS_TEXT_BYTES
}
pub fn validate_live_updates(&[HarnessLiveUpdate]) -> Result<(), HarnessContractError>;
pub const MAX_HARNESS_LIVE_UPDATES: usize = 128;
```

Rules:

* `HarnessObservationPage::validate_retention` bounds `live`.
* `live` is **not** part of `harness_observations_digest` (the digest covers `observations` only).
* **Strip before commit.** The daemon removes `live` before any canonical persistence:
  `ManagedHarnessRuntime::observe` (`packages/server/src/harness/runtime.rs`) takes the sidecar
  with `std::mem::take`, commits the page, then reattaches it to the in-memory result only.
  After the commit the runtime publishes it as session live events
  (`packages/server/src/harness/live.rs`); live updates for a segment the same page finalizes
  are dropped.
* Pages with nothing to transfer (no observations, pending page, gap or hydration request),
  such as idle waits and live-only streaming pages, carry no retention page. Identical empty
  pages then commit as exact retries instead of one canonical write per coalesced live window.
  The response contract (`validate_observation_page_for`) therefore requires a retention page
  after an acknowledged predecessor only when the page carries observations, hydration requests
  or a gap. Because an empty page has no successor cursor, the next page reuses the input cursor
  of an earlier page; the canonical page retry check treats such a recurrence as a new page when
  it extends the current committed predecessor, never as a conflicting retry.
* Defense in depth: `ExternalHarnessOperation::validate` rejects an `ObservationPage` record
  whose `page.live` is non-empty, so an unstripped page can never be persisted.
* Consumers must reject non-contiguous offsets instead of guessing. A durable `Text`/`Reasoning`
  observation with the same identity supersedes the live stream.

## 4. Stream identity (single source of truth)

`bcode_harness_models::harness_stream_identity(generation: u64, segment_id: &str) -> HarnessStreamIdentity`
(`packages/harness/models/src/live.rs`):

```rust
pub struct HarnessStreamIdentity {
    pub turn_id: String,      // "harness:{generation}:{segment_id}"
    pub segment_id: String,   // native segment id, unchanged
    pub segment_order: u32,   // always 0: each native segment is its own synthetic turn
}
impl HarnessStreamIdentity {
    pub fn assistant_item_id(&self) -> String;     // "assistant-turn:{turn_id}:segment:{segment_id}"
    pub fn reasoning_activity_id(&self) -> &str;   // = segment_id
    pub fn reasoning_part_id(&self) -> &str;       // = segment_id
    pub fn reasoning_item_id(&self) -> String;     // "reasoning-turn:{turn_id}:{segment_id}"
}
```

Mapping to live events:

* Text: `SessionLiveEventKind::AssistantTextStreamUpdated { turn_id, segment_id, segment_order: 0, .. }`.
* Reasoning: `SessionLiveEventKind::AssistantReasoningTextStreamUpdated { turn_id,
  activity_id: segment_id, activity_order: 0, part_id: segment_id, part_order: 0,
  kind: ReasoningContentKind::Legacy, role: ReasoningContentRole::Unknown, .. }`, matching the
  durable projection so reasoning display filtering agrees.

Durable `Text { segment_id: Some(..) }` / `Reasoning { segment_id: Some(..) }` observations
must be projected by SessionView onto the same `assistant_item_id()` / `reasoning_item_id()` so
live presentation is replaced in place. The generation prefix ensures a recovered generation
never reopens items from an earlier one. Ordering is by first appearance / canonical sequence,
not `segment_order`.

## 5. Mid-session configuration

In `packages/harness/models/src/service.rs`:

```rust
pub enum HarnessPermissionMode { Default, Plan, AcceptEdits }          // snake_case
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HarnessSessionSetting {
    Model { model_id: String },                   // identity bounds
    PermissionMode { mode: HarnessPermissionMode },
}
pub struct HarnessConfigureRequest {
    pub target: HarnessSessionTarget,
    pub mutation_id: String,
    pub setting: HarnessSessionSetting,
}
HarnessRequestKind::Configure(HarnessConfigureRequest)   // wire: {"operation":"configure","request":{..}}
HarnessOperation::Configure                              // as_str() == "configure"
HarnessRequestKind::control_mutation_id(&self) -> Option<&str>   // interrupt/close/configure
```

* `HarnessRequest::validate` checks the mutation identity, target instance and setting.
* Response: `HarnessServiceResponseKind::Acknowledged { mutation_id }` (must equal the request's
  `mutation_id`) or `Failure(..)`. Acknowledgement is not proof of native effect; ambiguous
  outcomes are never replayed automatically.
* Capabilities: `HarnessCapabilities` gains `model_switch` and `permission_mode`
  (`HarnessCapability`, serde default `HarnessCapability::unsupported()`, skipped on
  serialization while equal to that default via `HarnessCapability::is_unsupported_default`).
* IPC: `bcode_ipc::harness::ManagedSessionOperation::Configure` (`as_str() == "configure"`,
  not an auth operation). `ManagedSessionRequest::decode_call` admits it only with
  `HarnessRequestKind::Configure` and requires `harness.request_id == mutation_id`.
* Persistence: `bcode_session_models::ExternalHarnessRequestKind::Configure(HarnessConfigureRequest)`;
  canonical mutation key `configure:{mutation_id}`; claims require the current target.
* Routing: the daemon admits `Configure` only from interactive clients, checks the declared
  capability, and delivers it with a canonical claim like interrupt (see "Mid-session
  configuration" in `docs/managed-external-sessions.md`). `plugins/claude-code-plugin`
  advertises both capabilities as `Supported` with constraints (between turns only;
  init-advertised model keys only) and answers `Acknowledged` after the native control response,
  or `Failure(InvalidRequest | Conflict | Unavailable | Uncertain)`.

## 6. Storage schema and writer fences

* `bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION = 58`.
* `bcode_session_models::CURRENT_SESSION_STORAGE_WRITER_EPOCH = 26`.
* `bcode_session_models::external_harness`:
  * `SELECTED_OPEN_EVENT_SCHEMA_VERSION = 57`, `SELECTED_OPEN_STORAGE_WRITER_EPOCH = 25` are
    now **minimums** (selected-open checks accept `>= 57` / `>= 25` everywhere, including
    `require_selected_storage_contract`, `harness_prepare`, `verify_operation_writer`, spawn
    storage validation and migration normalization).
  * `NATIVE_PARITY_EVENT_SCHEMA_VERSION = 58`, `NATIVE_PARITY_STORAGE_WRITER_EPOCH = 26`.
  * `observation_requires_native_parity(&HarnessObservationKind) -> bool`: `Reasoning`,
    `ToolResult`, `Usage`, `InteractionWithdrawn`, `Text` with `segment_id`, `Tool` with
    `input_json`, `Interaction` with `tool_name`/`input_preview`.
  * `ExternalHarnessRequest::requires_native_parity()` (`Configure`).
  * `ExternalHarnessOperation::requires_native_parity()` covers delivery claims/completions,
    observation pages, and both recovery records (so recovery inherits the fence once
    published).
  * `validate_event_schema(schema)` / `validate_storage(schema, writer)` reject native-parity
    records below 58/26 and selected-open records below 57/25.
  * Recovery: `recovery_storage_available()` is `true`. `validate_record` validates
    `TransportReleased` via `release.validate_for(owner)` and `RecoveryPrepared` via
    `recovery.validate_for(owner)`, and the migration codec and `validate_recovery_writer`
    accept recovery records at schema >= 58 / writer >= 26.
* `ExternalHarnessOperationRecord::Configured.capabilities` is now `Box<HarnessCapabilities>`
  (serialized transparently) and the session actor's `HarnessAction::Page.page` is boxed.

Migration (`packages/session-migration`):

* Writer 25 / schema 57 is released history: added to `RELEASED_HISTORICAL_WRITER_EPOCHS`,
  `RELEASED_HISTORICAL_EVENT_SCHEMAS`, `RELEASED_WRITER_SCHEMA_COMBINATIONS`;
  `LATEST_HISTORICAL_EVENT_SCHEMA = 57`.
* New identity edge `session-writer-epoch-25-to-26` in `MIGRATION_STEPS` (payloads preserved;
  schema 57 rows are representation-compatible and normalize to 58).
* Historical fences: schema ≤ 57 rejects `reasoning`/`tool_result`/`usage` observations, any
  `segment_id`/`input_json`/`tool_name`/`input_preview` detail, and `configure` requests.
  `normalize_canonical_row_for_writer_epoch` rejects native-parity operations from writers < 26
  or schemas < 58.
* Fixture `fixtures/stores/epoch-25-schema-57-events.jsonl` (schema-55 matrix re-labelled to 57)
  is registered in `fixtures/manifest.json`; writer 25 was added to every all-writers lifecycle
  matrix.

## 7. Pending external interactions in SessionView

`bcode_session_view_models::SessionViewSnapshot::pending_external_interactions:
Vec<external::ExternalInteractionView>` (serde default). Included in
`snapshot_requires_reset` (changes produce a full reset patch) and in snapshot retention
accounting. SessionView fills it from unresolved `Interaction` observations and removes
entries on a successful response, a response record, or an `InteractionWithdrawn { request_id }`
observation. The Claude Code plugin records a withdrawal when Claude cancels a pending callback
(`control_cancel_request`) and for every pending callback when the native connection is lost or
closed; the session store indexes it under the interaction locator so late responses are refused.

`ExternalInteractionView` (`packages/session-view/models/src/external.rs`) now carries:

* `request_id`, `provenance` (`provenance.generation` is the generation a response must target),
* `interaction_kind: ExternalInteractionKind` (`Approval | Question | Assistance`, new, required),
* `kind` (existing label), `prompt`, `scope`, `choices`,
* `tool_name: Option<String>`, `input_preview: Option<String>` (serde default),
* `response: ExternalActionView` (response availability and reason).

`bcode_session_view::external::external_interaction` fills the new fields from
`HarnessInteraction`.

## 8. Client wrappers

* `bcode_client::ManagedSessionCommandHandle::managed_session_configure(generation: u64,
  mutation_id: String, setting: HarnessSessionSetting) -> Result<HarnessServiceResponse, ClientError>`
  (`packages/client/src/managed_bootstrap/attachment.rs`). The caller supplies the retained
  mutation identity, like `interrupt`. Validates the canonical target generation first.
* `bcode_client::ManagedSessionCommandHandle::recover(expected_generation: u64, recovery_id:
  String) -> Result<u64, ClientError>` returns the new ownership generation from the
  `Opened` binding; `Failure` and other families are errors.
* Lower level: `BcodeClient::managed_session_recover(ManagedSessionRecoveryRequest) ->
  Result<HarnessServiceResponse, ClientError>` (`packages/client/src/harness.rs`) wraps
  `ManagedSessionOperation::Recover` and rejects an `Opened` binding for another session,
  instance, or a non-newer generation.
* An exact resume answers `Opened` with the host-verified binding at the new generation
  (`open.resume`), keeping its recorded native version; a later native init must name the same
  native session.
* `ManagedSessionCommandHandle::host_client()` returns a credential-deferred host client for
  allow-listed host operations of a managed conversation; it grants no native authority.
