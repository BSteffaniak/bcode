# Session X-ray

X-ray is an opt-in, session-scoped live diagnostic channel. Attach to an idle or
active session, receive readiness, submit work through the ordinary application
API, and keep observing across turns and idle periods. Stopping observation does
not cancel session work. X-ray is neither canonical history nor a durable replay,
repair, steering, or cross-session aggregation facility. The optional
[web inspector](xray-web.md) adapts the same observation contracts into a local
browser UI; it does not change execution semantics.

**Implementation status:** this checkout contains live capture, typed reference export,
independently negotiated native payload transfer, bounded CLI consumption/recovery,
and explicitly authorized recursive workflow observation. Ordinary events and
sensitive payload captures now use separate bounded global channels rather than
acquiring observer consumer state on producer paths. Source changes and
coordinator-reported package tests are prior-phase validation, not release
certification: fresh integrated workflow, exporter lifecycle, and evidence acceptance
remain pending. Transient lifecycle contention and capacity exhaustion can still
lose diagnostics; passing individual runs does not establish lossless capture or
complete provider coverage. See [validation](#validation-requirements).

### Ordinary-event ingestion and cutoff

Producers capture exact observation-generation targets at admission and publish
without waiting for consumer state or channel capacity. Consumers assign sequences
in publication-processing order, not wall-clock or admission order. Later attaches
do not receive earlier queued commands. Events admitted before expiry remain
eligible for delivery after expiry; new event admission and sensitive-payload
commit enforce their respective scope/deadline checks.

Global ingress rejection occurs before sequence allocation. It contributes
conservative uncertainty to the legacy `hub_contention_drops` field, not an invented
sequence gap. Per-observation delivery-queue overflow remains sequenced and produces
exact gaps. Lifecycle-only registry access can still reject producers, including
payload commit, with truthful uncertainty accounting. Ordinary consumer reads do
not acquire producer admission locks. This is neither a durable channel nor a
guarantee of delivery under arbitrary load.

Engine Rust configuration adds global `ingress_capacity` (default 256),
`max_ingress_bytes` (default 16 MiB serialized content), and
`max_inflight_operations` (default 256). Public models and wire versions are unchanged.
Memory ownership includes ingress plus existing per-observation delivery queues,
bounded admitted-publisher inputs/target metadata, and retained payloads. Serialized
byte limits are not exact heap measurements.

### Bounded payload publication

Payload preparation reserves global pending count, byte credits, and channel capacity
before copying/hashing. Defaults are `max_pending_payloads = 64` (valid 2 through
1,048,576) and `max_pending_payload_bytes = 8 MiB` (at least `max_payload_bytes`).
Zero-byte captures still consume count. Checked payload ownership includes global
pending bytes plus each observation's retained-cache limit; concurrent authorized
range/export copies and metadata are separate bounded ownership.

Commit revalidates only the original observation generations, exact session/run,
deep scope, CLOSED state, and expiry after preparation. No reference is returned if
no original target remains eligible or publication fails. A successful reference
means bytes are owned by bounded host ingress or cache, not a promise to reconstruct
them later. Pending credits remain charged until cache transfer or rejection.
Payloads admitted before expiry can drain afterward; later attachments do not inherit
them. Lifetime progress exhaustion fails explicitly before successful publication.

Immediate payload reads pump a finite prior-publication prefix. If that prefix is
unresolved, the engine returns transient `Busy` rather than pretending bytes are
missing or empty. Asynchronous reads preserve their prefix across notifications;
applications impose bounded waiting and recheck authorization/expiry after awaits.
Both event and payload ingress participate in cutoff settlement. These are Rust
configuration/API changes, not public model or wire-version changes.

Cutoff closes admission once, then completes only after admitted publications,
queued commands, and registered global uncertainty reporters settle. The Rust engine
can return `FinalizePending`; asynchronous completion resumes the same irreversible
cutoff without holding consumer locks across awaits. Unrelated hub reporters can
delay completion. Applications must enforce bounded waiting and preserve pending
rather than fabricate a receipt or cache pending as permanent failure. Finalized
receipts remain frozen; stopping observation never cancels execution.

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
to fail. The optional payload extension negotiates independently of event callbacks:
legacy event-only hosts still accept events but cannot acknowledge payload retention.
Supported hosts assemble ordered bounded chunks, verify complete length/SHA-256 and
return host-origin transient references. Transfers have per-lease/global quotas,
expiry, idempotent abort and lease-release cleanup; retention rechecks deep scope.
Unsupported, rejected or unavailable capture is explicit loss/omission, not empty
success, invented references or permission to read private storage.

The recording interface is a consumer-side sink, independent of daemon persistence;
it can be replaced without changing producers or the public stream. Its current
filesystem implementation is described in the [protocol](xray-protocol.md#recording-bundles).
Synchronous file IO stays on the consumer side, never instrumentation producer
paths. The CLI exporter runs in a directly owned child of the current executable;
its private entry is handled before ordinary runtime/configuration/plugin startup.
The parent owns SSE intake, cutoff, and the endpoint lease; the child owns stdout,
recording, and bounded payload export. Initialization duplicates the endpoint bearer
over framed private stdin IPC after a version, embedded build-digest, and file-identity
handshake, never through argv or environment. The root private entry receives its
embedded build information before runtime construction; the parent checks the
child's loaded build digest before transferring the bearer. The child environment
is cleared. Its client does not share the parent's in-memory revocation flags:
server scope/expiry/revocation and owned-child termination remain the enforcement
boundaries. Internal exporter `Ready` means initialization and recorder creation
succeeded; it is not the session's `xray.ready` envelope, flushed stdout, or evidence
completion.

The consumer queue defaults to 256 envelopes, configurable with
`--consumer-queue-capacity` from 1 through 4096. IPC has bounded frames and typed,
secret-free status messages. Slow output cannot block observed execution;
sustained consumer overflow interrupts capture and preserves incomplete evidence.
Exporter shutdown starts a nonrenewing 30-second drain deadline, followed by bounded
owned-child termination/reaping (up to two five-second wait windows). Startup has
a separate 30-second bound. Final recording success requires matching terminal
input, successful child processing, and successful process exit—not merely delivery
to IPC. Unconfirmed reap is an unresolved local failure and forbids recovery.
Whole-application exporter lifecycle acceptance remains pending.

`--reconnect transport` opts into bounded fresh admission after transport loss
(automatic-bootstrap mode only). Each segment gets new readiness and an observation
identity; recording preserves interrupted segments and explicit gaps. Retry count
and delay are bounded options. This is gapful recovery, not replay, a durable resume
cursor, or a guarantee that missing events/payloads can be recovered.

## Source coverage and unsupported scope

This matrix describes contract boundaries rather than a second symbol inventory.
Rows describe implemented contracts, not a claim that every path passed final acceptance.
First-call request/stream evidence is limited to the exercised model, provider,
transport, and fixture. Only explicitly declared typed references establish expected
payload exports; neither reference-shaped opaque JSON nor successful event counts
prove complete bytes, all stream variants, subsequent calls, or future providers.

| Area | Implemented source behavior | Limits / unsupported claims |
| --- | --- | --- |
| Observation lifecycle | Idle attach, ready barrier, multi-turn observation, explicit cutoff, gaps and final receipt | No replay, resume cursor or reconstruction; EOF alone is incomplete |
| Local access | Matching-client one-session bootstrap; bearer-authenticated loopback HTTP/SSE; bounded admission | No remote binding; default browser Origins denied; no bootstrap Origin-setting CLI/config surface |
| Session/runtime | Live projected session and provider lifecycle/output, submission and turn correlation | Not a lossless event-log/frontend reducer; deep text projections may be lossy |
| Context/compaction | Final normalized assembly/content, selection/lineage and compaction observations; bounded inline/payload capture | Explicit exclusions include provider extensions and image source paths; not proof of raw provider delivery or quality |
| Providers | Provider-owned request/body/stream projections in bundled OpenAI-compatible, Bedrock, JEV, auth-broker and fake integrations; normalized runtime lifecycle/usage | Provider-specific limits/redaction apply; no headers/auth/config export, byte-exact HTTP transcript or complete adapter parity claim |
| Native plugins | Independently negotiated event/payload extensions, host-origin chunk retention, typed references and retained callback leases | Old event-only hosts cannot retain payloads; quota, contention, expiry and unsupported negotiation remain explicit loss |
| Tools/interactions | Dispatch, authorization/exchange waits, outcomes, cancellation and unfinished boundary observations | Cancellation request does not prove rollback or absence of effects; content remains explicitly deep |
| Workflows | Run/activation/attempt observations plus bounded recursive execution watches and pre-dispatch child readiness under exact trusted grants | Parent attachment alone does not authorize descendants; root, recursive and deep scope require explicit expiring authorization; no whole-tree completeness claim |
| Payloads | Observation-owned bounded references, authorized ranged export, consumer full-length/digest verification | Eviction/unavailable bytes remain missing; no private storage path fallback; export promptly before retention ends |
| Recording | Replaceable sink; JSONL + payload bundle; private files, integrity coordinates, atomic lifecycle manifest | Unix only; no database sink, automatic repair/resume or offline reader in this implementation |
| Eval | Readiness before submission, segmented task/workflow capture and separate evidence receipts | Task outcome is independent; recursive coverage and payload completeness fail closed on missing, opaque or interrupted evidence; final positive acceptance pending |
| Validation | Engine tests reported by the coordinator; fresh integrated acceptance remains pending | Required checks and final rebuilt-artifact acceptance are not interchangeable; no release certification |

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
passing evidence result, nor overwrite the task's score/exit status. Root session,
recursive descendant and deep capture are distinct authorization scopes. The bundled
workflow plugin accepts versioned `execution_observation_policy` grants only from
trusted configuration, matching actor, exact root run, namespace, state location,
recursive/deep flags and operation. Grants expire and are rechecked at dispatch;
missing, malformed, expired or mismatched grants deny capture. Diagnostic approval
is not permission to execute work. Bounded watch pages and exact child readiness
support recursive observation without treating a parent attachment as tree access.

An earlier offline cache eval reported `daemon_restart_gap` while its task passed;
that is degraded-evidence reporting, not successful complete capture. Native payload
retention is now implemented, but a previous omission or opaque event is not repaired
retroactively. The current positive workflow completeness run remains unsuccessful
pending final rebuilt-artifact validation.

Workflow evals can select another executable and environment. Without an explicitly
supplied subject endpoint, the implementation guards against bootstrapping a
mismatched consumer binary/environment. An explicit endpoint remains the caller's
responsibility to bind to the intended subject; bearer access is not proof of model
selection, exact provider delivery, task success or whole-run coverage.

## Validation requirements

Required validation is not a list of completed checks. For a final source revision,
run workspace check, strict all-target Clippy, architecture validation and relevant
engine, client/controller, CLI, provider, native-bridge, server and eval tests under
[AGENTS.md](../AGENTS.md). Rebuild the fake-provider dylib and matching host artifacts;
run the ignored native regression with an explicit absolute
`BCODE_XRAY_TEST_PROVIDER_LIBRARY` and record artifact hashes. Run the offline
`scripts/check-xray-e2e.sh` with deliberately selected matching executable/test/library
artifacts, plus the workflow eval positive-completeness and authorization-negative
fixtures. The runner alone does not execute every native/unit regression. Changes
to host cache planning or eval cache paths also require
`scripts/check-prompt-cache-eval.sh`. Retain failures and distinguish source, artifact
identity, command success, task outcome and evidence completeness. No live-provider
quality or complete coverage follows from offline fake-provider success.

## Known integration/security limits

* Browser access requires a host-provisioned exact Origin grant. Automatic bootstrap
  uses an empty Origin allowlist; there is no documented runtime setting to change it.
* Finalization certifies an engine delivery cutoff, not transport delivery, disk flush,
  provider cleanup, canonical settlement or task success. Close all consumer layers.
* Native bridge failures report bounded explicit loss, including failures before
  engine sequence allocation. Conservative loss counters are not unique-callback
  inventories, and absence of engine sequence gaps cannot certify plugin coverage.
* Capture declarations identify reference locations explicitly. Empty declarations
  mean inline/control data with no references; missing declarations remain legacy
  opaque data and cannot establish payload completeness. Arbitrary nested objects
  are not scanned or trusted merely because they look like references.
* Payload export and disk writes consume observer time. Fetch references while the
  observation retains them; final HTTP retention is bounded and not a later export API.
* Bundle digests are corruption coordinates, not signatures. No offline reader is
  supplied; a future reader must reject unknown versions, confine no-follow reference
  opens, verify full declared length/digest, and preserve unfinished states.

These are concrete scope limits, not reasons to read canonical/private storage,
implicitly enable deep capture or broaden authorization.
