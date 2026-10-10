# Automatic advisory judgement

Bcode's six optional enhancements default to `auto`. Auto means using a valid eligible
judgement credential at a useful, bounded feature trigger—not judging every event or
replacing the conversational model. Disabling a consumer plugin leaves ordinary behavior
available. These enhancements do not grant permission, execute recommended tools, retry
operations, certify completion, or change authoritative outcomes.

## Configuration

```toml
[judgement]
mode = "auto"                  # "off" disables all six
tool_result_triage = "auto"
context_reranking = "auto"
progress_monitoring = "auto"
recommendations = "auto"
session_categorization = "auto"
eval_categorization = "auto"

# Optional exact provider/model/account pin:
[judgement.selection]
provider_plugin_id = "bcode.jev"
model_id = "jev-1.13.0"
auth_profile = "jev"
```

All mode fields default to `auto`; set an individual field to `off` to disable only that
feature. Off is checked before provider discovery, verification, or inference: it makes
zero optional provider calls. Explicit judgement and explicit verification are separate
operations, not controlled by these optional-feature switches.

Session-bound normal-turn consumers use the resolved session configuration for judgement
policy and credential resolution; unscoped application/CLI optional calls use daemon startup
configuration. Without a pin, automatic
selection requires exactly one eligible account/provider candidate supporting the question
kinds; multiple profiles, providers, or profile-plus-environment credentials conservatively
fall back rather than choosing an account. Model preference follows provider advertisement.
An exact pin is never silently switched to another account.

## Features and actual scope

| Feature | Useful trigger and effect |
| --- | --- |
| Tool-result triage | A settled tool error with recognized diagnostic indicators. Returns fixed plugin-owned advisory text as request-only next-model context and a live Supplemental Transient frontend contribution; preserves the original result, success/error flag, and side-effect accounting. The service also supports trustworthy partial-result facts, but the current host hook does not supply a partial flag. |
| Context ranking | Under catalog byte-budget pressure, optional skill metadata is ranked before budgeting. Bounded transient evidence candidates are also ranked before request-only incorporation when their whole snippets exceed the optional-context budget. `filesystem.grep` supplies original match-line candidates; explicit reads and chronological tool responses remain untouched. SDK memory retrieval and `SessionHistoryContextRetriever` rank bounded optional candidates before admission. History retrieval uses the client search boundary without hydrating or replaying canonical history; unavailable or invalid advice preserves baseline ordering. |
| Recommendations | Relevance ordering of eligible skill metadata when catalog context ranking is not applicable, and eligible tool definitions after deterministic allowlist filtering. Ordering alone does not activate skills, authorize, or execute a tool. |
| Progress monitoring | Goal/loop iteration evidence supplies advisory progress/stall/drift probabilities. Ordinary sessions and applicable workflow activations also supply bounded ephemeral settled-work observations to the loop plugin, which can return transient/request-only stall or drift advice. Advice never changes execution, permissions, objectives, or completion authority; workflow advice is checked against current durable execution ownership. |
| Session categorization | Recognized task/domain signals from the newest admitted request. Fixed advisory labels are separate from canonical events and do not require replaying history. |
| Evaluation categorization | Measured failure facts with deterministic diagnosis labels in workflow evaluation. Adds a separate model-label field to `diagnosis.json`; does not replace oracle results or verify root cause. |

The catalog uses at most one ranking: enabled context ranking under genuine budget pressure,
otherwise enabled recommendations. This precedence avoids spending twice to overwrite the
same skill order. Each feature remains independently gated; context scores are not relabeled
as recommendation scores. Catalog-off/zero-budget rendering makes neither catalog call.
Required repository/system instructions and loaded explicit skill context remain unchanged;
protected shortlist entries retain their slots. Ranking returns every eligible candidate
exactly once, never an exclusion list.

## Verification, fallback, and limits

The application owns resolution, credential delivery, verification caching, cancellation,
timeouts, and backoff. Consumers own semantic questions and bounded triggers, not HTTP or
authentication. Providers may advertise `bcode.judgement-verification/v1`; authentication
acceptance and exact-model readiness are distinct facts. A model listing is not proof that
inference can run, and a non-inference request is not guaranteed free.

For [Jev](jev-provider.md), official TypeSafe API keys and hosted Jev keys are separate
credential types/backends. Official authenticated model discovery establishes authentication
and advertised model presence, not inference readiness. Hosted verification uses one fixed
lightweight judgement probe, which may be billed. Its payload contains no application task
or history. Static hosted model advertisement alone does not validate a key.

Missing credentials, ambiguous selection, unsupported capability, rejected credentials,
quota/rate limits, timeouts, provider errors, or unavailable consumer plugins preserve
baseline behavior. Optional access admits one concurrent call; busy access also falls back.
Verification has a 10-second timeout and optional work a 40-second overall timeout. Successful
verification is cached for five minutes; unsuccessful verification backs off for 30 seconds,
and a failed judgement imposes a 30-second retry delay. The verification cache is bounded to
64 entries, keyed by a digest of credential/settings identity. Supported local credential
refresh invalidates the cache. The plugin registry is immutable for the host lifetime; no
hot-reload notification mechanism is promised. External revocation is not instantly observable:
unchanged credentials can retain cached verification until TTL expiry or a provider failure.
Advice itself
is not durably cached as a session turn.

Portable requests/responses are at most 256 KiB, batches at most 64 questions, and choices or
rubrics at most 128 options; providers may be stricter. Selection accepts at most 32 candidates,
8192-byte task text, 2048-byte descriptions, and 128-byte IDs. Protected-only, empty, malformed,
or uninformative shortlists make no judgement request. Context and recommendation selection
promote candidates with usefulness probability above `0.65` in stable baseline order.
Uncertain probabilities in `[0.35, 0.65]` do not veto confident useful candidates or imply
irrelevance: uncertain and confidently negative candidates remain together afterward in
original order. Protected positions stay fixed; no candidate is removed. With no confident
useful candidate, baseline order is retained. Incomplete answers and invalid values reject
the entire batch; probabilities within either group do not establish ranking. Progress evidence is limited to 64
nonempty items and a 16-KiB encoded request. Triage accepts at most a 16-KiB service request
and an 8192-byte excerpt.

## Privacy and persistence

Optional does not mean local-only. Context/recommendation ranking sends bounded task text
and candidate descriptions or optional retrieved snippets to the selected provider. Progress monitoring sends bounded
stop-condition/evidence/summary/feedback state or recent settled-work observations and the explicit objective. These inputs may contain sensitive task data;
turn the relevant features or all judgement off when that transfer is not appropriate.

Triage reduces a transient excerpt to fixed diagnostic indicators and settled boolean facts
before provider transfer; tool arguments and artifact contents are not judgement inputs.
Session categorization sends fixed task/domain vocabulary rather than raw request text,
paths, or credentials. Evaluation categorization sends allowlisted measured booleans/status,
not raw outputs, prompts, or provider diagnostics.

Central request data and credentials are not appended as conversational turns. Consumer
outputs have separate lifetimes: request-only next-model tool diagnostics and live transient
frontend contributions (not history), disposable in-memory session
labels, workflow-owned iteration summaries, and explicit evaluation report fields. Canonical
history, permissions, terminal outcomes, and completion authority remain unchanged.

## Validation scope

Offline comparative tests exercise all six consumers across global-off, feature-off,
no-key, provider-error, and eligible-provider modes. Normal session tests capture
categorization, tool recommendation order, and request-only triage/progress advice;
client-boundary tests capture constrained context admission and evaluation diagnosis.
Controlled fixture probabilities demonstrate relevant candidate promotion, not live-model
quality improvement. Cancellation and authority-transfer tests reject late advice.

The public evaluation runner also compares all five modes using an explicitly selected
fake-provider CLI artifact. Its ordinary model turns complete, while unavailable workflow
history collections remain explicitly degraded (`workflow_unavailable`). Advisory labels
must not change that capture status, independent oracle failure, preservation result,
timeout status, or deterministic diagnosis. This is not complete workflow-capture support.

Current request-only guidance follows paired canonical history in model requests. It is
not persisted or reconstructed as historical guidance. Restart-cache validation checks
that removing old ephemeral guidance does not invalidate the canonical history prefix.

See [the provider contract](judgement-model-provider-contract.md) for typed interfaces and
[goal/loop judgement evaluation](loop-judgement-evaluation.md) for explicitly selected
completion evaluation, which is distinct from automatic progress monitoring.
