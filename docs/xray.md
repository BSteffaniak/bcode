# Session X-ray

X-ray is an opt-in, session-scoped live diagnostic channel. Attach to an idle or
active session, receive readiness, submit work through the ordinary application
API, and keep observing across turns and idle periods. Stopping observation does
not cancel session work. X-ray is neither canonical history nor a durable replay,
repair, steering, cross-session aggregation, or web-inspector facility.

**Implementation status:** the current checkout contains the engine, application
admission, matching-client endpoint bootstrap, local HTTP/SSE transport, CLI,
recording sink, native plugin bridge and eval consumer. Integration validation
passed workspace checking, strict all-target Clippy, architecture boundaries,
foundation and focused X-ray tests, and isolated offline engine/protocol/whole-app
acceptance (8 engine and 10 server tests). A real fake-provider dynamic-library
capture test and the offline prompt-cache eval regression suite also passed.
These checks do not certify live-provider quality, complete provider coverage,
workflow descendant capture, or whole-app eval X-ray evidence completeness.
The coverage table below describes implemented source, not a tested release claim.

See [protocol and browser example](xray-protocol.md),
[frontend contracts](frontend-contracts.md),
[pipeline observability](pipeline-observability.md), and the independent
[retained session trace export](session-trace-export.md) contract.

## Using the CLI

The session command has automatic matching-client bootstrap:

```text
bcode session xray <session-id>
bcode session xray <session-id> --deep --record /absolute/new-private-bundle
bcode session xray <session-id> --duration-seconds 60
```

These examples describe source-defined syntax; they were not executed. Use the
executable/configuration/state root that targets the subject session. Bootstrap
uses the normal application's artifact/build/state-location handshake and selected
session read boundary. It creates a revocable one-session loopback HTTP endpoint
and private credential file, rather than searching another daemon's private state.
The CLI retains the bootstrap connection and closes its endpoint on ordinary exit.
Server shutdown and bounded unused-endpoint expiry also release credentials.

For an already provisioned authorized endpoint, supply both `--endpoint` and
`--token-file`. The endpoint must be a literal loopback HTTP origin with no path,
query, credentials or fragment. A token file is sensitive, must be a stable private
regular file, and must not be printed or copied into a URL or command-line token
argument. Explicit endpoint mode does not bootstrap or revoke that listener.

Stdout contains flushed JSONL envelopes only. The initial `xray.ready` is written
before waiting for more events. Wait for it before submitting the workload through
your normal client. Ctrl-C or the duration option requests an observation cutoff;
the CLI drains through `xray.finalized` and finishes any recording. The drain timeout
is separately bounded by `--finalize-timeout-seconds` (default 15 seconds). Broken
stdout, absent finalization, payload export failure or sink failure is incomplete
evidence, never a request to cancel execution. The default HTTP observation lifetime
is 30 minutes, so an otherwise idle observer is not unlimited-lifetime capture.

### Sensitive capture

`--deep` is an explicit sensitive-data decision, not a synonym for attachment.
Without it, producers emit content-free diagnostic facts. Deep serialization is
gated by current observer activity; a stored context does not keep capture enabled
after detachment/cutoff. No observer means no deep retention. Context must be passed
explicitly across task, thread and native plugin boundaries.

Deep content can contain prompts, reasoning, tool arguments/results, and provider
body projections. Treat stdout, bundles and eval artifacts as sensitive. Producers
exclude known credentials and private configuration, and some projections redact
credential-shaped fields, but this is not a general secret-removal guarantee.
Content can itself contain secrets. Captured projections identify exclusions or
lossy redaction and must not be described as exact raw wire exchanges.

## Architecture and trust boundaries

Portable versioned envelopes live in a lightweight model leaf. The engine owns
transient observation queues, retention bounds and sequence allocation. Domain
owners supply event meaning, explicit correlation and safe diagnostic projections.
The runtime does not gain execution authority from diagnostic data.

The server authorizes exact session scope and sensitive access, performs bounded
current session-health/read admission, then attaches to its existing instrumentation
hub. It does not replay canonical event history to fill a missing capture. HTTP is
a read-only adaptation of that application boundary. Session/workflow mutation
continues through normal application APIs, not X-ray routes.

Provider wire details stay provider-owned, separate from public frontend events.
The optional native diagnostic channel is version-negotiated on the plugin callback
boundary, separate from ordinary service events. The host pins origin correlation
before dispatch and replaces incoming plugin correlation. Older/unsupported
negotiation disables diagnostics without requiring unrelated plugin functionality
to fail. Native callbacks support bounded inline events; they do **not** support
hub payload retention across the ABI. Larger native captures therefore report
omission rather than exporting an invented storage reference.

The recording interface is a consumer-side sink, independent of daemon persistence;
it can be replaced without changing producers or the public stream. Its current
filesystem implementation is described in the [protocol](xray-protocol.md#recording-bundles).
Synchronous file IO must stay on the consumer/blocking side, never instrumentation
producer paths. Current CLI capture serializes consumption with sink IO and payload
fetches; a slow disk/export can cause observation queue gaps, not execution waits.

## Source coverage and unsupported scope

This matrix describes contract boundaries rather than a second symbol inventory.
All implemented rows remain unverified in this phase.

| Area | Implemented source behavior | Limits / unsupported claims |
| --- | --- | --- |
| Observation lifecycle | Idle attach, ready barrier, multi-turn observation, explicit cutoff, gaps and final receipt | No replay, resume cursor or reconstruction; EOF alone is incomplete |
| Local access | Matching-client one-session bootstrap; bearer-authenticated loopback HTTP/SSE; bounded admission | No remote binding; default browser Origins denied; no bootstrap Origin-setting CLI/config surface |
| Session/runtime | Live projected session and provider lifecycle/output, submission and turn correlation | Not a lossless event-log/frontend reducer; deep text projections may be lossy |
| Context/compaction | Final normalized assembly/content, selection/lineage and compaction observations; bounded inline/payload capture | Explicit exclusions include provider extensions and image source paths; not proof of raw provider delivery or quality |
| Providers | Provider-owned request/body/stream projections in bundled OpenAI-compatible, Bedrock, JEV, auth-broker and fake integrations; normalized runtime lifecycle/usage | Provider-specific limits/redaction apply; no headers/auth/config export, byte-exact HTTP transcript or complete adapter parity claim |
| Native plugins | Negotiated, bounded diagnostic callback channel with host-owned origin and retained callback leases | No large-payload retention across ABI; old/unsupported plugins have no diagnostic coverage |
| Tools/interactions | Dispatch, authorization/exchange waits, outcomes, cancellation and unfinished boundary observations | Cancellation request does not prove rollback or absence of effects; content remains explicitly deep |
| Workflows | Run/activation/attempt, writer waits, dispatch, receipt publication, settlement/retirement observations with origin scope | Parent observation does not automatically observe independent child sessions; no whole-tree coverage claim |
| Payloads | Observation-owned bounded references, authorized ranged export, consumer full-length/digest verification | Eviction/unavailable bytes remain missing; no private storage path fallback; export promptly before retention ends |
| Recording | Replaceable sink; JSONL + payload bundle; private files, integrity coordinates, atomic lifecycle manifest | Unix only; no database sink, automatic repair/resume or offline reader in this implementation |
| Eval | Readiness before submission, capture around task/workflow consumption, bundle and separate evidence receipt | Task score/exit outcome is independent; workflow child capture explicitly incomplete; external executable/environment selection cannot silently use consumer defaults |
| Validation | Behavioral tests and an offline end-to-end harness authored | None executed in this phase; source presence is not test evidence |

## Eval use

The eval run command accepts `--xray` for automatic exact-daemon bootstrap and
`--xray-deep` for sensitive capture. An explicit `--xray-endpoint` must be paired
with `--xray-token-file`; endpoint selection must target the actual subject daemon.
Programmatic eval capture is request-scoped rather than declarative configuration
or ambient global capture. No provider credential/config contents are stored in
capture configuration artifacts.

Each captured execution session gets an `xray` bundle and an
`xray-evidence.json` receipt under its eval output directory. Receipts distinguish
readiness, delivery, payload retention, continuity and aggregate evidence status
from the ordinary task outcome. An attach/export/flush failure must not become a
passing evidence result, nor overwrite the task's score/exit status. Workflow-child
coverage is a separate explicit incomplete field; observing run boundaries in the
parent is not observing every child model request.

An offline `--xray-deep` cache eval exercised real attachment and recording across
an isolated daemon restart. Its task passed, while the evidence receipt correctly
reported `daemon_restart_gap`, incomplete continuity, and incomplete aggregate
evidence. Native captures larger than the callback budget were explicitly omitted
because payload retention across the native ABI is not implemented. This run
therefore validates degraded-evidence reporting, not complete eval capture.

Workflow evals can select another executable and environment. Without an explicitly
supplied subject endpoint, the implementation guards against bootstrapping a
mismatched consumer binary/environment. An explicit endpoint remains the caller's
responsibility to bind to the intended subject; bearer access is not proof of model
selection, exact provider delivery, task success or whole-run coverage.

## Known integration/security limits

* Browser access requires a host-provisioned exact Origin grant. Automatic bootstrap
  uses an empty Origin allowlist; there is no documented runtime setting to change it.
* Finalization certifies an engine delivery cutoff, not transport delivery, disk flush,
  provider cleanup, canonical settlement or task success. Close all consumer layers.
* Native bridge contention/negotiation failures can disable or drop diagnostic
  callbacks before engine sequence allocation; engine gap counters alone cannot
  certify complete plugin coverage. That loss needs bridge-owned accounting before
  an absence-of-loss claim can cover native diagnostics.
* Client reference discovery recursively recognizes reference-shaped event JSON.
  Ordinary nested tool/provider/user data with `payload_id` or `sha256` plus
  `byte_len` can collide with this heuristic and fail export. A typed/discriminated
  reference location is needed; this cannot safely be fixed by trusting private paths.
* Payload export and disk writes consume observer time. Fetch references while the
  observation retains them; final HTTP retention is bounded and not a later export API.
* Bundle digests are corruption coordinates, not signatures. No offline reader is
  supplied; a future reader must reject unknown versions, confine no-follow reference
  opens, verify full declared length/digest, and preserve unfinished states.

These are concrete scope limits, not reasons to read canonical/private storage,
implicitly enable deep capture or broaden authorization.
