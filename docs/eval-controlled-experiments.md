# Controlled eval experiments

## Purpose and scope

`fixtures/evals/introspection/` calibrates independent correctness and retained
observations before attempting causal diagnosis. It does not tune prompts,
completion thresholds, delegation policy, or performance acceptance gates. No
benchmark or speedup claim follows from these fixtures.

The controlled suite pairs `direct` (ordinary agent session), `single`
(no-delegation workflow), `auto`, and `collaboration` (`collaborate` runtime
strategy) on identical tasks and independent judges:

* `tiny`: one integer copy, useful for observing orchestration overhead.
* `separable`: independent east/west sums, eligible for independent work.
* `handoff`: integer normalization followed by ordering and weighted checksum.
  Both intermediate and final files must be correct. Correct files alone do not
  prove that a worker consumed a predecessor's output; inspect semantic handoff
  evidence before attributing a failure to scheduling or context transfer.

Oracles and command-control producers reside **outside** fixture workspaces.
The oracle uses fixed expectations, checks input preservation, bounds answer
reads, and rejects missing, malformed, wrong or symlinked answers. Producers do
not import the oracle. `oracle-controls.toml` must accept all three positive
cases and reject all three wrong and all three missing cases, even when the
producer falsely announces completion. These are artifact-grading controls,
not a claim that model completion judgement is calibrated.

## Declared experiment settings

`controlled.toml` uses three repetitions, paired scheduling seed `20261006`,
fixed case order and temporary copied workspaces. Each repetition has a 180,000
ms execution deadline. Workflows have two iterations, concurrency two, and 12
additional worker attempts; these are existing experiment allowances, not new
runtime thresholds. Ordinary sessions do not receive a workflow-attempt budget.
No token/cost cap is claimed. Independent judges have a 5,000 ms deadline.

Both request and tool capture are explicitly enabled through isolated trial
configuration overlays. Capture persists sensitive fixture text and tool output;
credential redaction does not make arbitrary user content safe. Keep artifacts
private, use synthetic input only, and do not copy raw payloads into public
reports. A bounded trace reference is not proof of exported bytes, completeness,
or absence of truncation. Preserve missing, unavailable, truncated and unsupported
capture states, not reconstructed model requests.

Pin and record executable hash/artifact identity, fixture revision, dirty checkout
scope, suite bytes, resolved provider/model/profile, judge selection, configuration
provenance, seed and resource settings **before** any authorized real-provider
run. Resolve layered/inline configuration explicitly rather than assuming that a
profile name reproduces it. Changing a selected provider requires authorization.
Never put credentials in these manifests. The controlled suite deliberately has
an executable marker; materialization requires an explicitly identified binary.

## Execution timeout contract

Timeout mode is explicit. Omitting `timeout_mode` retains the legacy
`"wall_clock"` meaning of `timeout_ms`; a historical 180,000 ms deadline does
not become an idle interval. Opt-in configuration uses:

```toml
[run]
timeout_mode = "idle"
timeout_ms = 180000
# Optional independent total execution limit:
# max_duration_ms = 900000
```

Case `timeout_mode`, `timeout_ms`, and `max_duration_ms` overrides inherit
independently from the run. An omitted case total limit inherits the run cap;
it does not disable it. Resolution is a lightweight model utility; execution
validation must reject zero intervals and zero supplied total limits before
admission. Unknown mode variants are rejected rather than guessed.

Idle mode specifies 180 seconds without substantive model, tool, or stage
progress, including descendants, not 180 seconds of total elapsed execution.
Polls, heartbeats, and observations that work is running do not reset it. A
separate total limit does not reset with progress. This contract does not certify
that every executor has implemented idle observation or cancellation; adapter
wiring and validation must be verified before claiming an idle-controlled run.
Judge deadlines and post-execution capture allowances are separate budgets.

Record resolved timeout settings on new results. Keep idle exhaustion, total
limit exhaustion, and legacy wall-clock exhaustion distinct in diagnostics;
do not relabel old failures or infer an idle diagnosis from elapsed time alone.
The example above does not change the checked-in experiment suite or its grading.

## Subsequent complete frozen idle runs (2026-10-08)

These diagnostic runs are separate from the first complete `c39bda1a…` run
and the historical baseline below; neither replaces their retained results.

* `corrected-runs/corrected-idle-paired`, executable SHA-256
  `9669f96280e3f0b0abcc03eb4695cdaea10e698b377c9f497238c04cb94b7765`,
  is complete: **3 execution passes and 33 failures in 36 repetitions**.
  All three passes are direct sessions; no workflow passes. The retained summary
  reports nine repetitions per variant. Correct artifact answers in failed
  trials do not establish canonical completion or verified cancellation.
* `validated-runs/validated-idle-paired`, executable SHA-256
  `6f8911703923eeb18289c34dfa5667571009253744d58f55bd5bd668d3d1b0c8`,
  is complete: **14 execution passes and 22 failures in 36 repetitions**.
  Each variant has nine trials: direct passes 4, single 0, auto 7, collaboration
  3. The command artifact judge passes 30/36: 16 correct-artifact trials still
  fail execution. Successful workflows have retained completed outcomes, but
  independent capture-completeness receipts remain false; execution success
  does not establish complete telemetry or causal attribution.

The latter run's 22 failures partition as follows:

* Five direct child-discovery failures: tiny/1,3; separable/2; handoff/1,3.
* Ten pending-readiness failures: all nine single trials and
  collaboration/separable/3.
* Five daemon connection/handshake failures: auto/tiny/1, auto/separable/1,
  collaboration/tiny/3, collaboration/separable/1, collaboration/handoff/3.
  The separable auto and handoff collaboration failures occur on scan close.
* Two unavailable linked-session coverage failures: collaboration/handoff/1,2.

All nine single cancellations are terminal-verified. Workflow cancellation is
also verified for auto/tiny/1 and collaboration/tiny/3, separable/1, handoff/2.
Four workflow failures retain paused, unverified outcomes: auto/separable/1
and collaboration/separable/3, handoff/1,3. Only handoff/1 requested cancellation;
requests for the other three failed with `workflow_unavailable`. Artifacts are
correct in three of these four trials, but not collaboration/handoff/1.
All nine direct cleanup receipts separately leave execution lifetime unverified
following endpoint unavailability after acknowledgement; a successful canonical
turn is not proof of daemon shutdown or released execution ownership.

The source-level shared-parent readiness defect is a missing durable execution
session association in the `SharedParentSequential` producer path. Parent
provenance must not be rewritten to impersonate a child: a fenced execution
permit, canonical node context and workspace association are required. Current
source adds the association, but the retained `shared-parent-tests.log` has two
producer-test failures (four other tests pass): the first node succeeds while
the next remains prepared. This remains unresolved, not a validated fix.
The frozen `c275e167…` candidate is unlaunched and predates these latest
shared-parent changes. Future typed read-only connection retries and bounded
child-discovery retries do not change the completed `6f891170…` artifact;
mutations must not be retried and polling must not reset the idle clock.

Source validation has subsequently advanced: `shared-parent-tests-reconciled.log`
reports 7/7 server tests passing, including the actual typed shared-parent
producer and durable-link test; `shared-permit-tests.log` reports 2/2 session
permit tests passing. These supersede the earlier source-test failures, not the
completed `6f891170…` results, and do not establish end-to-end acceptance.

The separate `final-runs/final-idle-paired` mismatched-harness run was interrupted,
with four retained results, not a completed 36-trial experiment. The subject candidate
identity records SHA-256
`f28dc5617e360234f08229488687a0558dbbf9eeb0ea62acb6f9bbf6f5c12b09`
and source-diff SHA-256
`515fb70802f00b30f1ba7bfe2b57b64e13e7c209a19576f6825de1ffa6ca1f1d`.
Its first direct/tiny/1 result fails before session creation: daemon startup
coordination-lock timeout for canonical artifact `bcode-d2b592…`, with a failed
artifact judge. This is infrastructure setup failure, not demonstrated candidate
model/output failure. The retained suite snapshot omits `timeout_mode`, the
first result omits timeout-policy telemetry, and the direct variant has no
subject binary entry. Do not label this run idle180 or assume candidate harness
retries execute merely because the workflow subject binary is pinned. The
operational coordinator is distinct from the product-under-test executable;
coordinator/harness identity and resolved policy must be retained separately.
The canonical coordinator reported for this launch has executable hash
`168a7ae970bc0b430f80af8cb8f75e11a42758f0f0bb56bd952bd6fc1d465024`;
the canonical-artifact lock corroborates the boundary, but the candidate
manifest alone does not verify coordinator executable bytes. No complete final
report, retry validation in that harness, or benchmark claim follows. Its auto
and collaboration trial diagnostics also reject the newer `session_child_linked`
event variant: a reader compatibility failure, not demonstrated model failure.
Retained `mismatched-cleanup-verified.json` records five idle-stop
acknowledgements and an unavailable direct-daemon endpoint; these acknowledgements
must not be promoted to independent process-lifetime or effect verification.

`coherent-runs/coherent-idle-paired` is a separate, explicitly isolated product
test, not replacement of the operational CLI. `coherent-launch.json` records
`f28dc561…` for both coordinator and subject, with a distinct coordinator state
root. Its retained suite snapshot explicitly resolves idle `180000` ms, no total
cap, and separate 15,000 ms activity/inspection IPC allowances. The first
retained direct/tiny/1 result passes both execution and artifact judge, with
capture complete, 59,167 reported tokens, 12,210 ms agent time and 15,127 ms
repetition wall time. Its cleanup still records execution lifetime unverified;
turn success is not verified daemon shutdown. At the next retained checkpoint,
four results exist: **2 execution passes and 2 failures**. Single/tiny/1 is a
completed workflow with passing independent artifact judge, establishing live
shared-parent readiness for that trial, not all cases. Its capture remains
incomplete: `events-capture-error.json` reports typed `workflow_unavailable`
and `history-capture-error.json` reports incomplete events. Capture took
99,412 ms, below the 120,000 ms allowance; this failure is not explained by
capture-budget exhaustion. Execution observation took 45,684 ms, repetition
wall time 153,865 ms, and reported tokens total 128,548. These are separate
measurements, not active-work or performance claims.

Coherent collaboration/tiny/1 and auto/tiny/1 retain activity connection/handshake
failures after 5 seconds. Collaboration cancellation was requested but terminal
unverified; auto cancellation is terminal-verified. The artifact judge fails
collaboration but passes auto, which still fails execution. Read-only retry
classification must use actual CLI operation identities (`activity-run` rather
than `activity`); subsequent source changes do not modify frozen `f28dc561…`.
Source inspection separates shared-parent link verification from public event
history representation routing. The latter's legacy-only dispatch is an
investigation target for missing events; a postterminal shared-parent provenance
mismatch has not been established by the generic unavailable error. Complete
capture acceptance is **not met**, even on passing workflows. The coherent f28
run is now complete: **25/36 execution passes and 11 failures**, with nine
trials per variant: direct 9 passes, single 9, auto 4, collaboration 3. This
supersedes its partial checkpoints; the separate completed `6f891170…` run
remains 14/36, not regraded. The independent command artifact judge passes
34/36; nine correct-artifact trials still fail execution.

The 11 failed coherent trials are:

| Trial | Retained observation failure | Cancellation terminal verified |
|---|---|---|
| auto/tiny/1 | Activity handshake timeout after 5 s | Yes |
| auto/tiny/2 | Activity handshake timeout after 5 s | Yes |
| auto/tiny/3 | Activity handshake timeout after 5 s | Yes |
| auto/separable/1 | Activity handshake timeout after 5 s | Yes |
| auto/handoff/2 | Activity handshake timeout after 5 s | Yes |
| collaboration/tiny/1 | Activity handshake timeout after 5 s | No |
| collaboration/tiny/2 | Linked-session progress coverage unavailable | Yes |
| collaboration/separable/3 | Activity handshake timeout after 5 s | Yes |
| collaboration/handoff/1 | Linked-session progress coverage unavailable | No |
| collaboration/handoff/2 | Activity handshake timeout after 5 s | No |
| collaboration/handoff/3 | Activity handshake timeout after 5 s | Yes |

All three unverified cancellations were requested, not proven settled. Artifact
judges fail collaboration/tiny/1 and handoff/2; all other failed executions have
correct artifacts, including unverified collaboration/handoff/1. Neither a
correct answer nor cancellation acceptance proves absence of continuing effects.
All nine singles pass, so readiness failures do not characterize this frozen
run. Two additional single trials retain activity errors and cancellation
receipts despite eventual PASS; error-file presence alone is not an execution
failure classification. History-capture failures remain independent of outcomes;
no complete semantic capture or performance closure follows.

The next frozen candidate, `current-capture-identity.json`, records SHA-256
`c9bdc5a19eb8f5e371949c0079d324c9bac25770783b811c4c74f600dcd43867`.
Its prelaunch identity is retained unchanged; subsequent `current-capture-launch.json`
and `current-capture-controlled.log` record a separate full36 run admitted
sequentially in `current-capture-runs`, after coherent f28 completed. The
`current-capture-idle-paired` report is now complete: **32 execution passes and
4 failures in 36 trials**. Each variant has nine trials: direct 9 passes, auto
9, single 7, collaboration 7. This is the latest complete experiment; prior
f28 25/36 and 6f 14/36 remain distinct retained outcomes, not regraded. The
command artifact judge passes **35/36**: three correct-artifact executions still
fail. The all36 execution-success target is **not met**.

| Failed c9bd trial | Retained activity diagnostic | Artifact judge | Cancellation |
|---|---|---|---|
| collaboration/separable/1 | `cli_failure`, phase `operation` | Fail | Requested; terminal verified |
| single/handoff/1 | `cli_failure`, phase `operation` | Pass | Request failed; terminal unverified |
| single/separable/2 | `cli_failure`, phase `operation` | Pass | Request failed; terminal unverified |
| collaboration/separable/2 | Session progress coverage unavailable | Pass | Requested; terminal unverified |

The three generic CLI diagnostics explicitly record subprocess exit 1 and
`timeout=false`; they do not identify an underlying handshake, request deadline,
server-unavailable condition or model failure. Classification masks non-connect
errors as `cli_failure`, so underlying operation cause remains unresolved.
The two failed cancellation requests independently report `workflow_unavailable`;
that fact must not be attributed backwards to the activity failure. The coverage
failure cannot establish idle expiration. All three terminal-unverified trials
have correct artifacts, which proves observed answer effects, not canonical
completion, settled cancellation or absence of further effects.

Its source fixes retry classification against
production-constructed `workflow activity-run` arguments, not a changed
five-second connection threshold. Capture now consumes typed representation
capabilities: unavailable legacy semantic events remain explicitly unsupported;
attempts and bounded scalar observations have a separate pinned high-water
scope. `semantic_history_complete` remains false, and unsupported events must
not be inferred from an error string or empty response. The retained
`current-capture-integration-tests.log` verifies 1/1 public-API integration test
passing. These fixes execute in the separate c9bd experiment, not retroactively
in frozen f28. Finite scalar observation completeness at pinned high-water is
not semantic execution history: retained receipts keep events unsupported,
`semantic_history_complete = false`, provider wire not captured, and overall
`complete = false`; individual judgement artifacts may remain unavailable even
when attempt/session-history boundaries are complete. Existing opt-in model
request/tool-I/O persistence is not broadened by this distinction. Full execution
and complete-evidence acceptance remain open; scoped scalar completeness must
not silently substitute for the latter. No semantic-trace or performance closure
is claimed.

Launch selection records
provider `bcode.openai-compatible`, model `gpt-6.1-sol`, profile `sol` and
experimental auth profile `openai`; it does not reproduce a request selecting
`openai-2`. Capture provenance, resolved policy and both executable identities
must accompany any eventual complete report. Current-format explicit shared
execution association must preserve legacy behavior rather than rewriting
parent provenance or manufacturing historical child links.

Both completed runs resolve `timeout_mode = "idle"`, `timeout_ms = 180000`, without a
`max_duration_ms` cap. The latter suite separately records 15,000 ms activity
and inspection IPC allowances. Its failures include incomplete agent child
discovery, pending-readiness allowance failure, unavailable linked-session
coverage, and daemon connection/handshake failure after 5 seconds. An IPC or
coverage failure is not an idle-expiration diagnosis or an established model
failure. Cancellation-request acceptance, verified terminal settlement, observed
artifact effects, and capture completeness must be reported independently.
For example, latest-run auto/separable/1 cancellation failed and remains
unverified; collaboration/handoff/1 requested cancellation but did not verify
terminal settlement. Neither is proof that work had no effects.

The two runs overlapped. Provider contention, authentication-selection provenance,
changed IPC budgets, terminal-observation handling, and the isolated debug-zero
build artifact prevent treating differences as a single-variable causal
experiment. Provider/model/profile are retained as `bcode.openai-compatible`,
`gpt-6.1-sol`, and `sol`; recorded `openai` authentication selection does not
establish exact reproduction of a coordinator using `openai-2`. The latest
retained quiescent regression log reports **1/1 passed**, superseding its earlier
0/1 result, without retroactively modifying either frozen executable. Future
connection-timeout source work is not behavior of the running artifact. No
full-run acceptance, benchmark improvement, or latest-workspace validation
closure is claimed for the latest shared-parent changes. The completed run spans
`1791431406706` to `1791436226964` (4,820,258 ms); this includes execution,
setup, capture and cleanup, not active model time. The rounded retained summary
reports the following observations, not performance acceptance:

| Variant | Avg repetition wall | Retained total tokens |
|---|---:|---:|
| direct | 18.6 s | 318.0k |
| single | 98.4 s | 924.7k |
| auto | 170.6 s | 1.81M |
| collaboration | 247.4 s | 2.99M |

Missing direct-failure usage and incomplete workflow capture prevent treating
these totals as complete spend. The 120,000 ms capture allowance is independent
of idle policy. Execution observation time, active-attempt intervals, repetition
wall time and capture time remain separate; summary `-0ms` agent values for
workflows are not meaningful active-duration evidence. No causal interpretation
of generated efficiency deltas or benchmark improvement is warranted.

## First complete idle run (2026-10-07)

`/Volumes/ehdd/bcode-eval-artifacts/remaining-20261007/runs/idle-paired`
retains all 36 case/variant/repetition identities and a `run_finished` record
at `1791425500289` (start `1791422998484`), with **0/36 execution passes**.
The retained `identity.json` records executable SHA-256
`c39bda1aac8b28b0f2ed92550b91172bde01664a42fbf005199c7104cb878109`,
HEAD `7e9e805d61718e733c62413bd038c245ffb18ae6`, and source-diff SHA-256
`9318258f9f17beac32a72c45b0accecefedc026c990676036b6552d11c924f79`.
All results resolve to idle `180000` ms without a total-duration cap. None
records idle or wall exhaustion. Preserve this run independently of corrected
pilots and reruns; it is an infrastructure/observation failure experiment, not
performance-comparison evidence.

The retained failure distribution is:

* Direct (9): three activity-coverage validation failures, three
  `session_activity_unavailable` errors, two `session_children_unavailable`
  errors, and one daemon startup coordination-lock timeout.
* Workflows (27): nine activity RPC timeouts, eight missing execution-session
  links, three linked-session progress snapshots marked `coverage_unavailable`,
  and seven inspection RPC timeouts. These observation failures caused failure
  handling, rather than an established provider or completion-grading defect.
* Three independent artifact oracles pass: auto/handoff/1 and auto/tiny/2–3.
  All three executions nevertheless end cancelled before completion judgement.

All nine collaboration outcomes remain distinct from the historical table below:

| Collaboration trial | Observation failure | Retained lifecycle |
|---|---|---|
| tiny / 1 | Activity RPC timeout | Cancellation verified |
| tiny / 2 | Inspection RPC timeout | Cancellation verified |
| tiny / 3 | Inspection RPC timeout | Cancellation verified |
| separable / 1 | Activity RPC timeout | Paused; cancellation failed/unverified |
| separable / 2 | Activity RPC timeout | Paused; cancellation failed/unverified |
| separable / 3 | Inspection RPC timeout | Cancellation verified |
| handoff / 1 | Activity RPC timeout | Cancellation verified |
| handoff / 2 | Linked progress unavailable | Paused; cancellation failed/unverified |
| handoff / 3 | Inspection RPC timeout | Cancellation verified |

Across workflows, 24 cancellation receipts verify terminal cancellation. Three
roots remain unresolved in retained inspection: separable/1
`a866664d-a630-42fa-9921-0d3a07c075b7`, separable/2
`7684b45e-c2bb-4a98-ad3c-d4d908fe8c16`, and handoff/2
`a838dd2c-3c2d-4451-bc55-a7580d5741e3` (all collaboration). Their cancellation
calls returned `workflow_unavailable`; admitted attempts lack terminal timestamps.
Eight direct sessions acknowledge cancellation but explicitly retain unverified
execution lifetime; direct/tiny/2 failed before session creation. Complete eval
inventory does not certify complete cleanup, no remaining workers, or no effects.
All workflow captures are incomplete; collaboration/separable/2 and handoff/2
separately exhaust the 120000 ms capture allowance. Recorded workflow tokens total
1555026; direct usage is unavailable. Timing and tokens are observations of failed
trials, not successful-work efficiency measurements.

Current source separates transaction-local workflow terminal reads
(`terminal_outcome_in`), closes temporary non-owning session DB handles on error
as well as success, waits boundedly for initial activity coverage without counting
retries as progress, and retries normalized `ProjectionStale` observations.
These mechanisms support infrastructure diagnosis; they do not establish that
one defect explains every retained RPC failure. The corrected immutable
`8b916ecc806324227f8aede0f34f6c926783040131670cce64c4cb1026b33474` pilot and
corrected full36 validation are separate verification boundaries, not replacements
for this failed run. No corrected full36 result is established here.

## Historical full-run evidence (reviewed 2026-10-07)

The historical complete 36-trial report is
`/Volumes/ehdd/bcode-eval-artifacts/closure-20261007/runs/wait-fixed-full-paired`.
Its `run.json` records start `1791401398535` and source HEAD
`3a7bfe85a9b13fe372884c0c22bb352777984110`; the suite snapshot and
`wait-fixed-identity.json` identify executable SHA-256
`44b981b752558d676d52af2ae7ec59b313440bc619f3b4899627a649ff563d60`.
These are recorded provenance, not independent verification of executable bytes
or a reconstruction of dirty source. They do not certify the current checkout.

The report contains all 36 repetitions: direct 9/9, single 9/9, auto 9/9,
collaboration 0/9; total 27 passed, 9 failed, no missing report entries. Every
collaboration failure records wall-clock deadline exhaustion. Independent
artifact grading and retained terminal evidence differ:

| Collaboration trial | Artifact oracle | Retained terminal evidence |
|---|---|---|
| tiny / 1 | Pass | Inspection timed out; terminal unverified |
| tiny / 2 | Pass | Inspection timed out; terminal unverified |
| tiny / 3 | Pass | Inspection timed out; terminal unverified |
| separable / 1 | Pass | Root inspection records `cancelled` |
| separable / 2 | Pass | Inspection timed out; terminal unverified |
| separable / 3 | Fail; final answer absent | Inspection timed out; terminal unverified |
| handoff / 1 | Fail; normalized file exists, final answer absent | Inspection timed out; terminal unverified |
| handoff / 2 | Pass | Inspection timed out; terminal unverified |
| handoff / 3 | Pass | Inspection timed out; terminal unverified |

For separable/1, retained root inspection records cancellation request
`1791402401893` and terminal update `1791402402060`. The generic summary warning
that terminal state requires verification does not supersede this record. For
the other eight, a cancellation request is not proof of terminal cancellation.
Deadline enforcement is the verified proximate failure; the underlying reason
for exceeding the deadline is not established by these reports. Do not attribute
it to remote queueing, provider behavior, or a scheduler defect without evidence.

This run still declares `timeout_ms = 180000` with no idle mode and a separate
120000 ms capture allowance. The earlier `clean-final-20261007/runs/clean-paired`
report starts at `1791354188070` and passes 28/36; it is not the newest full-run
status. Later `critical2-tiny` starts at `1791413570247` but contains only one
collaboration trial, which fails its wall deadline despite correct artifacts.
Its timing reports four observed attempts, 152022 ms active union, 6933 ms gaps,
and no observed overlap; this is not a complete provider/tool/wait decomposition.
None of these historical reports establishes a completed 36-trial idle-policy rerun;
the separate first idle run above does retain all 36 results, all failed.

## Offline validation (coordinator only until validation is released)

The following commands are proposed, **not evidence of validation already run**.
They do not build Rust or format the shared workspace. Python 3.11+ is required
(`hashlib.file_digest`). Bash is required for the shell harness.

```sh
bash -n scripts/check-eval-introspection.sh
python3 - <<'PY'
import ast, pathlib, tomllib
root = pathlib.Path('fixtures/evals/introspection')
for path in root.rglob('*.py'):
    ast.parse(path.read_text(), filename=str(path))
for path in root.glob('*.toml'):
    tomllib.loads(path.read_text())
PY
```

For development validation, build the product binary with its app and bundled
plugin features, then use its explicit path and SHA-256 with isolated test state.
Use the canonical runtime for operational control of existing sessions; do not
substitute a development binary for that runtime. For example:

```sh
cargo build -p bcode --features app,static-bundled-plugins --bin bcode
```

Run the harness with the identified test artifact:

```sh
binary="$(pwd)/target/debug/bcode"
sha256="$(shasum -a 256 "$binary" | awk '{print $1}')"
bash scripts/check-eval-introspection.sh "$binary" "$sha256"
```

If the artifact lacks the new APIs/plugins, rebuild with the required features
and record its new hash before rerunning. Do not silently fall back to PATH or
an unrelated installed artifact. The script verifies the hash
before any product invocation and launches only the fake provider, in a scrubbed
environment with fresh private HOME/config/state. It deliberately does **not**
load the live `sol` provider config/profile or connect to its state root: the
offline fake-provider selection is the controlled harness subject, not a
reproduction of the live selection. No fallback to live credentials is allowed.
If deployment needs plugin discovery paths, report that limitation rather than
silently adding user configuration.

The harness retains artifacts and does not clean them up automatically. It:

1. Materializes manifests outside the checkout, retaining external oracle paths.
2. Runs the nine command controls, expecting suite failure but verifying each
   positive/negative result rather than accepting any nonzero exit as success.
3. Runs a real deterministic fake-cache filesystem read loop, with explicit
   capture-on and capture-off overlays. Checks two calls, no tool errors,
   canonical session/sequence index identity, request-boundary counts, context
   occupancy, provider-round observations, tool output references and omitted
   provider-private request metadata. Joins `transcript.captures.json` receipts
   (`bcode.eval.trace_exports.v1`, `sensitive=true`) by session/sequence to the
   index, verifies confined blob names, byte length, SHA-256, completeness and
   redaction against the request-time references, and parses populated request
   messages from the actual exported bytes without printing them. Capture-on
   must export every request and tool output successfully; capture-off must have
   no export receipts. Unavailable/verification-failed receipts remain explicit
   failures; retained/truncated content remains a named completeness gap.
4. Runs the existing `loop_until:1` fake structured workflow protocol to check
   workflow inspection, attempt timing and child-session index retention.
5. Reports capture gaps separately from artifact correctness. `--require-complete`
   is a fail-closed diagnostic mode; it currently fails because full semantic
   handoff/judgement/queue attribution is not certified. A normal
   smoke pass explicitly does **not** mean complete introspection.

The fake provider does not implement meaningful auto/collaboration task solving.
The four-strategy controlled suite is materialized but **not launched** by the
offline script. Running it needs separate coordinator authorization for models
and resources. Its tasks are deterministic; model behavior is not.

To prepare authorized suites without starting a model:

```sh
python3 fixtures/evals/introspection/materialize.py \
  /absolute/identified/canonical/bcode /absolute/private/suite-directory
```

For a later authorized live trial, preserve the requested context on every
invocation, using the verified canonical executable:

```sh
BCODE_CONFIG=/Users/bsteffaniak/.config/bcode/providers/sol.toml \
  /absolute/identified/canonical/bcode \
  --state-root /Users/bsteffaniak/.local/state/bcode --profile sol \
  eval run /absolute/private/suite-directory/controlled.toml \
  --output-root /absolute/private/run-directory --run-id controlled-seed-20261006
```

Verify effective `bcode.openai-compatible` / `gpt-6.1-sol` / `sol` selection and
configuration provenance through application APIs first. Do not dump the config
or infer identity from its filename. Repeat paired blocks with preregistered
seeds and exactly one changed factor; preserve failures, correction attempts and
capture gaps. A single passing repetition cannot establish causality.

## Evidence limits and integration boundary

The verifier reads only eval-owned retained artifacts, with per-artifact byte
limits; it does not open canonical session databases, repair history, read trace
paths, or invent private daemon APIs. Session capture/export remains application-
and domain-owned. Unsupported introspection schemas fail closed.

Trace-byte export uses the application boundary and writes sensitive receipts
and confined `.captures/session-sequence.blob` files adjacent to transcripts.
The verifier checks actual receipts and exported bytes rather than using the
older introspection summary's `complete_model_inputs_exported` flag. Export error
statuses are not successful captures. The coordinator must align the verifier
with any final contract changes and validate the new models/IPC/client/CLI path.
Full workflow semantic history, descendant pagination, judgement correlation and
provider retry/queue attribution are not certified by this fixture lane. In
particular, preserve request/tool identity, truncation/unavailability and explicit
capture-off absence through the application API, not by opening daemon trace
storage. Inspect workflow handoffs
for named inputs/results and predecessor provenance; useful parallel work is not
established by elapsed-time overlap. Do not sum potentially cumulative usage
rows; canonical telemetry owns accounting.

Attribute findings separately to correctness, completion judgement, missing
capture, provider rounds, tools, context construction, and handoff evidence.
Unknown or missing evidence blocks a causal claim rather than counting as a
quality improvement. These fixtures introduce no new crates, SQL access or
ordinary-session read behavior.
