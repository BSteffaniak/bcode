# Judgement model provider contract

`bcode.judgement-model-provider/v1` is a non-conversational typed service, separate from
`bcode.model-provider/v1–v3`. It evaluates bounded application state against caller-owned
question IDs. Advertising it does not grant chat, tool-call, turn-stream, permission, or
completion capabilities. Portable types live in `bcode_model::judgement`; vendor wire schemas
are not this contract.

## Provider interface

* `models` returns provider/model question-kind capabilities. Absent support is not inferred.
* `judge` receives a typed `Request` plus transient host-resolved `ProviderRequestContext`.
  `Request` contains `model_id`, `State` (text or structured), and named `Question` values
  (Choice, Score, YesNo). `Response` contains typed answers and optional normalized usage.

Responses must have exactly the requested IDs, matching answer kinds, finite in-range values,
and consistent option IDs/cardinality. Unknown variants fail decoding; unsupported capabilities
fail before invocation. Encoded requests and responses are limited to 256 KiB, batches to 64
questions, and choice/rubric options to 128; providers may impose stricter limits. Unsupported
interfaces fail closed; additive non-semantic fields must not redefine semantic controls.
Public errors are normalized, bounded, and secret-safe. Credentials, vendor-private metadata,
and judgement input must not implicitly become conversational session content.

## Optional verification interface

Providers may additionally advertise `bcode.judgement-verification/v1`, operation `verify`.
`VerificationRequest` names the exact centrally resolved model and transient provider context.
`VerificationResult` independently reports authentication, model readiness, and verification
method. Unsupported verification is explicit, not inferred from a successful model listing.

Authenticated discovery and lightweight judgement are different methods. Model advertisement
is not inference-readiness proof; `not_checked` requires independent central capability checks
before use. Non-inference discovery is not guaranteed free. A hosted lightweight probe may
be billed; it uses fixed probe state, not consumer application state. Rejected authentication,
quota/rate limits, model unavailability, and temporary failure must remain distinct. The
[Jev provider](jev-provider.md) documents its separate official and hosted credential backends.

## Application access and optional consumers

The host application interface is `bcode.judgement-application/v1`, not a provider interface:

* `judge` accepts explicit `WorkflowJudgementRequest` with provider route, owned auth-profile
  selection, and typed request. Empty profile explicitly selects provider-declared environment
  credentials. Application/client explicit access uses the same central routing boundaries.
* `optional_judge` accepts `OptionalRequest { feature, selection, request }`, where `selection`
  is an optional exact `SetupSelection`. It returns `OptionalResponse { response }`; `None`
  means retain ordinary caller behavior, never a negative answer or completion verdict.

`Feature` has six independently gated variants: `ToolResultTriage`, `ContextReranking`,
`ProgressMonitoring`, `Recommendations`, `SessionCategorization`, `EvalCategorization`.
Global/per-feature modes default to auto. Off performs zero optional provider discovery,
verification, or inference calls. Auto uses an eligible valid credential at meaningful bounded
consumer triggers. Missing/ambiguous credentials, unsupported capabilities, busy access,
verification failures, timeouts, or provider errors fall back without account guessing.
An exact selection is not silently replaced with another account.

Central application access owns model resolution, auth, verification/cache, timeout, backoff,
and cancellation; consumers must not duplicate HTTP/auth or feature-local retry machinery.
Feature prompts and semantic interpretation remain consumer-plugin/domain owned. The host
checks deterministic eligibility and constrained output before applying advice. Session-bound
consumers use resolved session policy/credentials; unscoped optional application calls use
daemon startup configuration. See [automatic judgement](automatic-judgement.md) for actual triggers, limits,
privacy, and persistence lifetimes.

## Consumer scope

`bcode.skills-selection/v1` prepares and applies bounded advisory ranking. Context mode ranks
actual optional skill metadata before the prompt catalog's existing byte budget; it is not a
general retrieval facility or chronological history reorder. The catalog chooses enabled context
ranking under genuine byte-budget pressure, otherwise enabled recommendations, not two
successive rankings that overwrite each other. Recommendations also order eligible tool
definitions without activation or execution authority. Protected entries keep their slots,
and a complete permutation preserves every candidate; uncertain/error results retain baseline.
Loop-owned triage adds fixed request-only next-model diagnostic context and a live Supplemental
Transient frontend contribution, not canonical history. Goal/loop iteration progress adds
advisory probabilities, never retry/cancel/completion
commands. Session and evaluation labels stay distinct from canonical events and measured
oracle outcomes. Explicit loop judgement evaluation remains separately selected and does not
turn automatic progress monitoring into completion authority.

Application-route, IPC/client, plugin, and loopback tests exercise typed validation, owned auth,
failures, and cancellation. Opt-in historical live hosted verification is evidence of that
bounded call only—not a claim of current account readiness or official endpoint compatibility.
Snapshots or transient replies do not establish durable replay/resume semantics.
