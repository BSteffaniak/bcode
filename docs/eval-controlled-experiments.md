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
