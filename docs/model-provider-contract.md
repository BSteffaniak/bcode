# Model provider contract

This document is the normative behavioral contract for the versioned
`bcode.model-provider/v1` plugin service. The Rust payload types and operation inventory live in
`bcode_model`; the reusable deterministic harness lives in
`bcode_model_provider_runtime`.

The contract applies equally to bundled providers, third-party native plugins, in-process test
adapters, and proxies that expose the same typed operations. A provider must not advertise a
capability that it cannot satisfy through this contract.

## Version and compatibility

`bcode.model-provider/v1` identifies one wire-compatible family of JSON request and response
payloads. Operation names, payload types, and their requirement levels are published as
`bcode_model::MODEL_PROVIDER_OPERATIONS`.

Within `v1`:

* providers must ignore unknown additive JSON fields when deserializing host requests;
* hosts must tolerate unknown provider metadata keys and provider extension content;
* a semantic or wire change that invalidates an existing conforming implementation requires a new
  interface version;
* an unknown interface returns `unsupported_interface` and an unknown operation returns
  `unsupported_operation` as a plugin service error;
* optional operations may return `unsupported_operation`; required operations may not.

A plugin service error means the operation could not enter its typed protocol, such as malformed
JSON, unavailable routing, or an unsupported operation. A model/provider failure after
`start_turn` succeeds is represented by normalized `ProviderTurnEvent::Error` followed by
`TurnFinished(Error)`, not by making later polls fail with an opaque service error.

## Required operations

Every provider implements these operations:

* `capabilities: () -> ProviderCapabilities`
* `models: ModelListRequest -> ModelList`
* `validate_config: ValidateConfigRequest -> ValidateConfigResponse`
* `start_turn: ModelTurnRequest -> StartTurnResponse`
* `poll_turn_events: PollTurnEventsRequest -> PollTurnEventsResponse`
* `cancel_turn: CancelTurnRequest -> AckResponse`
* `finish_turn: FinishTurnRequest -> AckResponse`

`cancel_turn` is a required lifecycle operation even when active transport cancellation is not
advertised. This gives every host one safe cleanup path. Advertising `Cancellation` additionally
promises that active provider work observes cancellation promptly and emits the cancellation
terminal sequence when cancellation wins the completion race.

Context compaction and native web search are required only when their corresponding provider
capability is advertised. Verification and provider-auth usage/reset operations are optional
extensions. The complete machine-readable classification is
`bcode_model::MODEL_PROVIDER_OPERATIONS`.

## Discovery and capability truthfulness

`ProviderCapabilities` returns a stable, non-empty provider ID and display name. `ModelList`
returns non-empty, unique model IDs with non-empty display names. A selected model must advertise
`StreamingText` because normalized polling is the baseline `v1` generation transport.

Provider and model declarations must agree:

* model `ToolCalls` requires provider `Tools`;
* parallel tool calls require ordinary tool calls at both levels;
* model JSON mode, prompt caching, native web search, and code search require their corresponding
  provider capability;
* capability absence means callers must reject or avoid that behavior before a turn;
* capability presence means the behavior is implemented, not merely accepted and ignored.

Advisory hints such as cache placement may be ignored only where their type documentation says so.
Correctness-sensitive controls such as `ToolChoice::None`, `ToolChoice::Required`, a named tool
choice, structured output, and cancellation cannot be silently ignored. A provider that cannot
honor one returns a normalized `UnsupportedFeature` or `InvalidRequest` error.

## Turn lifecycle and polling

A successful `start_turn` allocates a unique, non-empty provider turn ID and an isolated active
turn. All later lifecycle operations use that ID.

The event protocol is ordered and draining:

1. `TurnStarted` is first and appears exactly once.
2. Zero or more content, tool, usage, warning, retry, projection, compaction, or metadata events
   follow.
3. Exactly one `TurnFinished` is the final event.
4. Polling drains events. A later poll must not replay events returned by an earlier poll.
5. Empty poll batches are valid while work remains active.
6. No event may be emitted after `TurnFinished`.

A host may poll, cancel, and finish from different tasks. Providers must synchronize active-turn
state and handle completion races without panics, duplicate terminal events, leaked work, or reused
turn IDs.

`finish_turn` is idempotent. It releases all provider-visible state and requests cancellation of
remaining work. Calling it for an already-finished or unknown ID succeeds. `cancel_turn` is also
idempotent and succeeds for an already-completed or unknown ID. Polling a finished-and-released or
unknown ID returns an empty event batch.

## Capability negotiation

Broad `ProviderCapability` and `ModelCapability` values remain compatibility/discovery summaries;
they are not sufficient evidence for correctness-sensitive request controls. Providers publish
`ModelFeatureSupport` at the active provider surface, and models publish an independent
`ModelFeatureSupport` only when model-specific evidence exists. Claims cover every neutral model
parameter, JSON-schema versus strict JSON-schema output, each tool-choice mode and parallelism,
automatic/explicit/TTL cache hints, and inline/reference/tool-result image input.

Every affirmative or negative claim includes `CapabilitySource` provenance. Missing claims are
`CapabilitySupport::Unknown`, which must never be presented as guaranteed behavior.
`ModelTurnRequest::requested_features` inventories the controls exercised by one turn, and
`ModelFeatureSupport::negotiate` returns `Guaranteed` only when provider and model both affirm the
feature. It otherwise identifies the provider/model scope that is unknown or explicitly
unsupported. Adapters remain authoritative and reject unsupported controls before network work;
negotiation metadata does not replace request validation.

## Generation and stream content

Text and reasoning deltas are non-empty UTF-8 strings and preserve provider order. Concatenating
text deltas yields the assistant text for that provider round. Providers may omit reasoning even
when the upstream service generated hidden reasoning.

Provider-specific state that must survive continuation uses typed provider-extension content or
`ProviderMetadata`; it must not be smuggled into user-visible text. Metadata keys are non-empty and
values are opaque strings. Hosts preserve unknown metadata but do not interpret it as trusted
application data.

Provider-native request controls that are not portable use `ProviderRequestExtension` payloads
owned and documented by the provider crate. `ProviderRequestContext::set_extension` serializes each
payload under a provider-scoped envelope; an adapter reads only its own extension and rejects
foreign owners, malformed payloads, and unsupported API surfaces before network work. Typed
extension fields are reserved from the untyped `ProviderRequestContext::request` escape hatch so a
caller cannot ambiguously supply both forms. The untyped map is for newly released provider fields
that do not yet have a typed extension, not a stability boundary.

`Warning` is optional, non-terminal, and contains a non-empty actionable message. Warnings report
degradation or fallback that did not invalidate the result. They must not replace a terminal error
when correctness was lost.

## Structured output

A provider/model advertising `JsonMode` accepts `StructuredOutputRequest` and returns one JSON value
as its text result. When `strict` is false, the result must still be syntactically valid JSON. When
`strict` is true, the provider must enforce the supplied JSON Schema or return a normalized
`UnsupportedFeature`/`InvalidRequest` error before claiming successful completion. It must not emit
ordinary prose, silently drop the schema, or report `EndTurn` for a value known not to satisfy the
schema.

The SDK independently parses and validates final structured output. Provider-native enforcement is
an additional guarantee, not a replacement for application-side validation.

## Tool calls and continuation

Tool-call IDs and names are non-empty. IDs are unique within one provider round and correlate all
`ToolCallStarted`, optional non-empty `ToolCallDelta`, and exactly one `ToolCallFinished`. Finished
arguments are a JSON object. A tool call is complete before `TurnFinished(ToolCall)` and no call may
remain open at the terminal event.

The provider obeys `ToolCallRequestPolicy`:

* `None` emits no tool call;
* `Required` emits at least one registered tool call or a normalized error;
* `Tool { name }` emits that registered tool or a normalized error;
* `parallel: false` emits at most one call in a round;
* `parallel: true` permits, but does not require, multiple calls.

For continuation, the host appends assistant `ToolCall` blocks and matching tool-role `ToolResult`
blocks, preserving call IDs and provider order. The provider must accept successful and
model-visible error results and continue the conversation without inventing new IDs for those
results. With tool choice `None`, a valid continuation completes without another tool call.

## Usage and stop reasons

Every successfully completed generation or tool-call round emits at least one `Usage` event before
`TurnFinished`. Usage may be incremental; hosts aggregate all events. Known fields are
non-negative. When input, output, and total are all reported, total is not smaller than input plus
output. Cache and reasoning fields refine usage and do not increase the meaning of total unless the
upstream provider defines them that way.

Failed or cancelled rounds emit usage when the upstream provider made it available, but absence is
allowed because work may stop before metering exists. `ExactRequestInputTokens` is emitted only for
a provider-confirmed complete request count. `RequestProjection` describes what was actually sent,
not intended configuration.

Terminal stop reasons have these meanings:

* `EndTurn`: a normal assistant result that needs no tool execution;
* `ToolCall`: one or more complete tool calls require host results;
* `MaxTokens`: generation reached the provider output limit;
* `StopSequence`: a configured/provider stop sequence ended generation;
* `Cancelled`: cancellation won the race and is preceded by one `Cancelled` event;
* `Error`: generation failed and is preceded by one normalized `Error` event.

An `Error` event must pair with `TurnFinished(Error)`. A `Cancelled` event must pair with
`TurnFinished(Cancelled)`. Normal completion that wins a cancellation race may retain its original
normal stop reason; cancellation must not append a second terminal sequence.

## Errors, retries, and rate limits

`ProviderError` always has a stable non-empty code, a non-empty safe message, and the closest
`ProviderErrorCategory`. Auth and configuration failures populate `failure` with a
`ProviderFailureContext`: the responsible provider ID, a typed non-secret source kind and source
identifier, the blocked capability/operation, and concrete remediation. Configuration validation
returns the same context in `ValidateConfigResponse.failures`; successful validation has no
failures. Source identifiers name environment variables, auth/model profiles, config keys,
credential stores, provider response statuses, or runtime chains—not credential values.
`provider_message` may preserve upstream diagnostic text but must not contain credentials or
secrets. `request_id` preserves an upstream request/correlation ID when one
is available. `diagnostic_context` contains only adapter-allowlisted non-secret fields such as HTTP
status and upstream error type; it must not copy arbitrary headers or bodies. Ordered `sources`
preserve safe source subsystem, code, and message information without requiring an opaque dynamic
error chain to cross the provider wire boundary. `retryable` describes whether repeating the
operation can reasonably succeed; it does not authorize the host to retry. Retry timing is
represented by `ProviderRetryHint` with provider timing preserved when known.

`provider_message`, source messages, and any normalized safe upstream message must pass through
`bcode_model_provider_runtime::sanitize_provider_diagnostic` before storage. The sanitizer redacts
common header, JSON, form, query, URL-userinfo, bearer/basic-token, and AWS access-key credential
shapes and bounds diagnostic length. Unstructured response bodies and opaque transport error strings
must not be copied into normalized errors.

Internal provider retries preserve event order and cancellation. `RetryScheduled` is optional but,
when emitted, has a non-empty message and an absolute Unix retry time. A human-facing `Warning` may
accompany it. A retry that eventually succeeds has one terminal success sequence. Exhausted retries
produce one normalized terminal error. Rate-limit failures use `RateLimit`, transient saturation
uses `Overloaded`, provider/network deadlines use `Timeout`, and host-requested cancellation uses
`Cancelled`.

## Timeouts and cancellation

The host owns the whole-turn timeout. On timeout it requests `cancel_turn`, then `finish_turn`, and
returns a host timeout even if provider completion races with cleanup. Providers must make both
operations bounded and must not leave background work or active-turn state after finish.

A provider-native request deadline is a normalized `Timeout` error followed by
`TurnFinished(Error)`. Active cancellation checks must cover network waits, retry sleeps, and stream
processing. Cancellation does not permit partial tool calls to be reported as complete.

## Conformance

Provider authors implement
`bcode_model_provider_runtime::BlockingModelProviderInvoker` over their adapter and call
`run_provider_conformance_suite`. The deterministic suite validates discovery, configuration,
ordered draining events, successful generation, usage, idempotent cleanup, and all advertised tool,
structured-output, and cancellation behavior. `ProviderEventValidator` can validate additional
provider-specific scenarios such as warnings, metadata, retry schedules, normalized errors, and
provider-native timeouts.

Skipped cases mean the provider and selected model did not advertise that optional capability;
they are not passes. Invocation failures distinguish routing/codec failures from behavioral
violations, and every failure identifies its conformance case.

Bundled providers must run this same public harness. Credential-gated network acceptance remains a
separate layer because deterministic conformance must not depend on an external service.
