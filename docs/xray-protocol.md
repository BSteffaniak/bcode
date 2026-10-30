# X-ray v1 protocol

This describes the implemented source contracts in this checkout. Offline
engine, protocol and whole-app acceptance tests have passed, including readiness,
idle multi-turn observation, recording, provider errors and exact cancellation.
This is not a live-provider or complete-coverage certification. See
[Session X-ray](xray.md) for validation scope, usage, architecture and remaining limits.

X-ray is live, session-scoped, read-only diagnostics. Dropping/finalizing an
observer closes capture, never cancels or mutates the observed execution. It is
separate from public frontend events and canonical session trace export.

## Compatibility and identities

The envelope/request boundary is `schema_version: 1`. Reject unknown versions and
unsupported representations instead of guessing an older shape. Unknown domain
names/event names within a supported bounded envelope are preservable diagnostic
data, not new execution instructions. Requests deny unknown fields. Independently
versioned provider projections, native diagnostic negotiation and recording bundles
must be checked at their own boundaries, not inferred from the envelope version.

An envelope contains:

* `schema_version`, `observation_id`, `sequence`, `timestamp_unix_ms`.
* `event`: `domain`, `name`, `correlation`, `data`, `sensitive`.
* Correlation contains exact `session_id` and optional `turn_id`, `request_id`,
  `tool_call_id`, `run_id`, `activation_id`, `attempt_id`.

Sequences belong to one observation, start at one, and are not canonical session
positions or durable cursors. Correlation is supplied by owning domains; absence
means unknown, not permission to infer identity from text. Wall timestamps support
joins, not cross-process elapsed-duration arithmetic. Domain elapsed intervals use
monotonic clocks. Neither event content nor diagnostic correlation grants access.

## Bootstrap and authorization

A matching application client can open a temporary one-session endpoint after the
normal artifact/build/state-location handshake and bounded selected-session access
checks. The returned lease contains endpoint metadata and a private credential-file
locator, not a bearer inside a URL. Opening an endpoint is not the ready barrier.
It authorizes the requested session and explicit deep scope; HTTP observation still
performs session-health/read admission. Keep the lease connection for owner-scoped
revocation, and explicitly close it on ordinary exit. Shutdown/unused expiry is a
fallback, not proof that a lost caller already revoked its capability.

An external client may use an explicitly provisioned endpoint and bearer without
daemon IPC or private storage access. Such clients speak the same versioned HTTP
contract; they must not infer subject artifact/state identity from a port number.
The provisioner is responsible for selecting the intended subject. The ordinary
HTTP client accepts literal loopback HTTP origins only, disables redirects and
ambient proxies, and rejects URL credentials/query/fragment/non-root paths.

The HTTP host binds loopback only and requires an exact Host authority. It rejects
query strings, cookies, absolute-form request targets and `Last-Event-ID` rather
than accepting URL credentials or implying replay. Every non-preflight operation
requires one `Authorization: Bearer …` header. The bearer is an independent private
capability, not a provider API key. Public failures are fixed JSON error codes, not
raw provider/OS errors. Transport/listener capacity, body/header sizes and timeouts
are bounded separately from engine queues.

Browser Origins are exact allowlist entries, with no wildcard or `null` grant.
Allowed entries must be HTTP loopback origins; preflight permits only the supported
POST/header use. **Automatic bootstrap currently uses the empty allowlist. There
is no CLI/bootstrap Origin option or declarative runtime setting for it.** Browser
use therefore requires a host embedding to explicitly construct the HTTP
configuration with a narrowly chosen Origin and provision the scoped bearer out
of band. Do not disable Origin checks or repurpose provider credentials.

## HTTP operations

All routes below are POSTs with JSON request bodies and bearer authorization.
No query parameters, SSE replay IDs, cookie authentication or execution routes are
supported. Errors use non-success HTTP status and `{"code":"…"}`.

### `/v1/xray/observe`

```json
{"schema_version":1,"session_id":"<canonical-session-id>","deep_capture":false}
```

Successful response is `text/event-stream`. SSE frames use `event: xray` and JSON
envelope `data`; no `id` is emitted. Heartbeat comments are transport activity, not
session progress. A transport failure event is not an envelope/finalization receipt
and must fail evidence closed.

The first envelope is `xray.ready`, sequence 1. Its data advertises:
`schema_version`, effective `deep_capture`, queue capacity, event/payload/retention/
range bounds, and `durable_replay: false`. Verify session, version, observation
identity and effective deep mode before submitting work. Headers alone are not
readiness. Submission itself uses the ordinary application API.

Capture lasts across idle and multiple turns until cutoff/drop. The default host
observation duration is 30 minutes; host configuration permits bounded alternatives,
not an unbounded transport lifetime. Nonblocking producers do not wait for queue
capacity, disk, browser or network. Explicitly propagate originating contexts across
spawn/thread/native boundaries; task-local inheritance is not assumed.

### Ordering, gaps and omissions

Ordinary envelopes increment sequence by one. `xray.gap` has inclusive
`first_sequence`, `last_sequence`, and `reason`; its envelope sequence is
`first_sequence`. The next expected sequence is `last_sequence + 1`, **not** the gap
envelope sequence plus one. Ready, gap and final controls cannot be crowded out by
the ordinary queue. A missing unannounced sequence is a protocol/incomplete-delivery
error for the HTTP client. The sink can preserve a discontinuous supplied stream,
but counts it as missing evidence rather than silently certifying continuity.

Domain `*_omitted` controls preserve size/encoding/capture exclusions. They are not
successful captures, empty payloads or proof of cancellation. Unknown event names
remain diagnostic content. Queue drops, hub contention losses, evicted payloads,
domain omissions and failed consumer export must remain distinguishable.

### `/v1/xray/finalize`

```json
{"schema_version":1,"observation_id":"<ready-observation-id>"}
```

The response reports the accepted engine cutoff with `observation_id`,
`final_sequence`, `dropped_events`, `hub_contention_drops`, and `evicted_payloads`.
Concurrent/retried cutoff requests coalesce while the observation is registered.
A pending/unavailable response is not permission to assume finalization succeeded.
Continue consuming SSE until the matching `xray.finalized` envelope with the same
identity and final sequence. Drain queued events and tail gaps first. EOF before
that envelope is incomplete. Retry support does not promise later replay after
registration/retention ends.

`hub_contention_drops` is conservative hub-wide loss during the observation, not an
exact selected-session drop count. Finalization stops further capture for this
observer, not all observers or session execution. It proves neither provider
termination, receipt publication, canonical settlement, transport delivery nor
successful local recording. Those require their own outcomes.

### `/v1/xray/payload`

```json
{"schema_version":1,"observation_id":"<id>","session_id":"<session>","payload_id":"<id>","offset":0,"length":65536}
```

Only authorized deep observations can export their retained bytes. A typed
reference contains `schema_version`, `payload_id`, `session_id`, `content_type`,
`byte_len`, `sha256`. It is not a filesystem locator. Capture is all-or-nothing,
observation-owned and eviction-bounded. A range response contains `schema_version`,
`observation_id`, the complete `payload` reference, `offset`, `bytes` (JSON byte
array), and `eof`. Zero requested length, overflow, oversized ranges, foreign/
shallow/missing observations and unavailable/evicted content fail closed. EOF can
be an empty successful range at declared length; it does not upgrade completeness.

The current full-export consumer caps payloads at one MiB and ranges at 64 KiB,
validates version/identity/offset/progress/EOF on every response, assembles bounded
bytes, and verifies full length/SHA-256 in both the client and sink. Reference
discovery traverses bounded event data and treats objects with `payload_id` or
`sha256` plus `byte_len` as typed references; malformed/future/conflicting candidates
fail closed. This has a known collision risk: arbitrary nested provider/tool/user
content can contain those fields without being a reference. An explicit typed
reference location/discriminator is needed to remove that ambiguity; domain authors
cannot guarantee arbitrary content avoids it. Expected references are registered
before fetching so failed export remains missing in the manifest.

Fetch promptly while consuming events. HTTP keeps final retention only for bounded
body-drain/disconnect grace; do not defer all exports until after closing SSE.
Larger native-plugin payloads cannot be retained across the current callback ABI:
they report omission. Never substitute canonical session trace files, reconstruct
content from history or reinterpret unavailable payloads as empty complete data.

## Minimal browser authenticated fetch example

This is an illustrative source-level example, not exercised code or a full viewer.
It requires the explicit host Origin grant described above; it will be rejected by
default automatic-bootstrap endpoints. Obtain `endpoint` and a short-lived scoped
`bearer` through trusted out-of-band provisioning; keep the bearer in memory only.
Native `EventSource` cannot send the required authorization header, so use fetch.
No token appears in a URL, local storage, DOM, log, history or cookie.

```javascript
async function observeXray({ endpoint, bearer, sessionId, deep = false,
                             signal, onEnvelope, onReady }) {
  const origin = new URL(endpoint);
  if (origin.protocol !== "http:" || origin.pathname !== "/" ||
      origin.search || origin.hash || origin.username || origin.password ||
      !["127.0.0.1", "[::1]"].includes(origin.hostname)) {
    throw new Error("Expected provisioned literal loopback origin");
  }
  const headers = { Authorization: `Bearer ${bearer}`,
                    "Content-Type": "application/json" };
  const post = (path, body) => fetch(new URL(path, origin), {
    method: "POST", headers, body: JSON.stringify(body), signal,
    credentials: "omit", cache: "no-store", redirect: "error",
    referrerPolicy: "no-referrer"
  });
  const response = await post("/v1/xray/observe", {
    schema_version: 1, session_id: sessionId, deep_capture: deep
  });
  if (!response.ok || !response.body ||
      !response.headers.get("content-type")?.startsWith("text/event-stream")) {
    throw new Error("Observation unavailable");
  }
  const reader = response.body.getReader();
  const utf8 = new TextDecoder("utf-8", { fatal: true });
  let buffer = "", observationId, next = 1, finalized = false;
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) throw new Error("EOF without finalized envelope");
      buffer += utf8.decode(value, { stream: true });
      if (buffer.length > 2 * 1024 * 1024) throw new Error("SSE bound");
      for (;;) {
        const separator = /\r?\n\r?\n/.exec(buffer);
        if (!separator) break;
        const frame = buffer.slice(0, separator.index);
        buffer = buffer.slice(separator.index + separator[0].length);
        const data = frame.split(/\r?\n/).filter(s => s.startsWith("data:"))
          .map(s => s.slice(5).replace(/^ /, "")).join("\n");
        if (!data) continue; // heartbeat/comment, not progress
        const e = JSON.parse(data);
        if (e.schema_version !== 1 || !e.observation_id ||
            e.event?.correlation?.session_id !== sessionId ||
            (observationId && observationId !== e.observation_id) ||
            !Number.isSafeInteger(e.sequence) || e.sequence !== next) {
          throw new Error("Incompatible or discontinuous stream");
        }
        const control = e.event.domain === "xray";
        if (!observationId) {
          if (!control || e.event.name !== "ready" || e.sequence !== 1 ||
              e.event.data.schema_version !== 1 ||
              e.event.data.deep_capture !== deep || e.event.data.durable_replay !== false) {
            throw new Error("Ready barrier mismatch");
          }
          observationId = e.observation_id;
          // Callback may now submit through a separate ordinary application API.
          // Do not await a whole workload here: that would stall observation.
          onReady({ finalize: async () => {
            const cutoff = await post("/v1/xray/finalize", {
              schema_version: 1, observation_id: observationId
            });
            if (!cutoff.ok) throw new Error("Cutoff unconfirmed");
          }});
        }
        next = e.sequence + 1;
        if (control && e.event.name === "gap") {
          const gap = e.event.data;
          if (gap.first_sequence !== e.sequence ||
              !Number.isSafeInteger(gap.last_sequence) || gap.last_sequence < e.sequence) {
            throw new Error("Invalid gap");
          }
          next = gap.last_sequence + 1; // preserve/display loss, never hide it
        }
        if (control && e.event.name === "finalized") {
          if (e.event.data.observation_id !== observationId ||
              e.event.data.final_sequence !== e.sequence) throw new Error("Invalid cutoff");
          finalized = true;
        }
        onEnvelope(e); // bounded consumer; textContent only if rendering untrusted data
        if (finalized) return e.event.data; // delivery cutoff, NOT task/sink success
      }
    }
  } finally {
    await reader.cancel().catch(() => {}); // observation only; no session cancellation
  }
}
```

The example does not retain payloads, implement storage, reconnect, submit work or
score task success. An AbortSignal closes observation but cannot certify graceful
finalization. Catch its returned promise and any finalize promise; do not log
sensitive response data/errors. A production consumer also needs bounded retained
UI state, payload handling, safe integer/u64 policy, explicit loss indicators and
separate task/evidence outcomes. Browser mixed-content/private-network policy may
impose additional restrictions; no browser bypass is supplied.

## Recording bundles

The independently versioned bundle contract is version 1. The current replaceable
sink consumes portable envelopes and references, not daemon/private storage types.
Its synchronous methods append envelopes, register expected payloads, store complete
verified payloads, finalize with the received cutoff or explicitly interrupt.
Call it on a consumer/blocking IO task, never from producer instrumentation.

The filesystem implementation creates a **new absolute absent destination** with
existing non-symlink ancestors; it never appends/resumes/overwrites a prior bundle.
Unix directory-descriptor-relative no-follow operations confine writes. Directories
are created mode 0700 and files mode 0600 (a restrictive umask may remove bits).
Other platforms fail unsupported, not permissive fallback. Paths such as symlinked
temporary roots need an intentionally resolved trusted parent, not a reader-chosen
substitute location.

Layout:

* `events.jsonl`: exact serialized envelopes, one newline each; rolling SHA-256,
  byte length, count and last envelope sequence in the manifest.
* `payloads/<sha256-of-id>-<sha256-of-content>.bin`: verified full payload bytes.
  Untrusted payload IDs never become path components. References map IDs to these
  generated relative names; expected but unavailable bytes have no retained path.
* `manifest.json`: version/identity, sensitive classification, lifecycle, event
  integrity, payload references/paths, missing-sequence and omitted-event counts,
  optional engine finalization and fixed failure code.

Manifest publication writes a private temporary file, flushes it, atomically
renames it, and syncs the containing directory. Data/payload files are flushed before
successful close claims. Disk errors poison the recorder; updating a failed manifest
is best effort because a full disk may prevent even that. Drop attempts an interrupted
close. Process death can leave a `recording` manifest, pending manifest or partial
tail: all are incomplete, never auto-repaired. An active manifest can lag the JSONL
prefix; only successful final close claims the declared complete local stream.

Lifecycle is `recording`, `finalized`, `interrupted`, or `failed`. Finalize requires
the matching delivered terminal envelope, not merely the HTTP cutoff response. A
repeated identical final close is idempotent; conflicting/late input is rejected.
`evidence_complete` requires finalized state, no sequence losses/domain omissions,
no reported engine/contention drops or evictions, and all registered payloads stored.
It means complete **registered local observation evidence**, not all-session content,
all native callbacks, raw provider equivalence, task success or full workflow coverage.

Default sink bounds are one MiB per serialized envelope/payload, 4096 references,
one GiB event+payload bytes, and four MiB serialized manifest metadata. These are
configurable recording resource policies, not architectural session/workflow limits.
No database implementation, offline reader, signature scheme, automatic maintenance,
reconstruction or durable replay protocol is provided. Future readers must bound
input, reject unknown versions, no-follow/confine every reference open, verify full
length/digest before returning bytes, and preserve explicit incomplete states.

## Outcome separation

Readiness, observed execution outcome, engine cutoff, transport drain, payload export,
local sink close and independent task evaluation are distinct facts. The CLI exposes
JSONL and errors; eval retains a separate evidence receipt with delivery/payload/
continuity/workflow-child scope. A successful task cannot repair lost evidence, and
a perfect recording does not establish task quality. See [eval use](xray.md#eval-use).
