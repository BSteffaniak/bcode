# Optional judgement evaluation: corpus v3 and readiness

## Truth and evidence

`fixtures/evals/judgement/development.json` is development **schema version 3**, with 36
separately authored task-family IDs: six tasks per feature. Each feature has positive and
negative/misleading controls; missing-evidence cases exercise abstention or explicit evidence
gaps. Families are independently authored semantic problems, not repetitions or production
samples. Six families meet a five-task *development* minimum structurally, not live CI
acceptance: validation, capture completeness, actual consumer execution and quality gates
still apply. Adversarial failures may reveal real weaknesses; no outcome is scripted to win.

`held-out.json` remains byte-identical version 1 with twelve authored cases. Its SHA-256 is
`0c1383c21250cc1a2aa1d60cbb142cbb3b59facd25082e7cce0042df7a6ddaa1`.
No new held-out freeze was made. It does not meet five independent tasks per feature and
must not be presented as adequate CI/population evidence. Both splits are **authored
synthetic**, visible to implementers, not independently collected or hidden production truth.
External truth requires consent, bounded sanitized public exports, blinded independent
labels/adjudication and disjoint task families. No live gain claims are supplied.

Cases carry independent semantic `expected`, rationale, typed artifact `answer`, and exact
input preservation oracles. Success prose/self-judgement cannot substitute for an artifact.
Quoted error strings, candidate instruction injection, fresh evidence, incomplete captures,
false-completion claims and unknown cost exercise misleading evidence and abstention.
Version-one inputs are semantic adapter data, not invented domain wire requests. Domain
vocabulary/protected-position assumptions need review before live acceptance.

## Stable development fields and coordinator conversion

Top level: `schema_version:3`, `split:"development"`, `truth_provenance:"authored_synthetic"`,
`cases`. Case fields: `id`, `family_id`, `feature`, `control`, `fault`, `input`, `expected`,
`rationale`, `task`, `files`, `answer`. Controls remain positive/negative/adversarial.
Faults are none/missing_evidence/malformed_input/misleading_evidence; the corpus currently
uses evidence faults, not deliberately invalid public API schemas.

Ranking expected fields:

* `kind:"ranking"`, legacy `prefix`, `all_ids`.
* `relevance`: every ID to independent graded relevance in [0,4].
* `costs`: every ID to nonnegative integer charge in the shared ranking envelope.
  **Context only:** protected candidates have cost zero; production always admits them
  outside the optional budget. Optional total cost exceeds `budget` for admission pressure.
  **Recommendations:** no admission operation exists; costs retain physical sizes and
  `budget` equals the sum of all costs. Full permutation admission is valid for every order.
  Physical candidate `size_units` stays nonzero, including protected candidates.
* `k`: preregistered useful relevance cutoff. Recommendations use the count of independently
  positive-relevance candidates (minimum one); `dev-recommend-read` has k=1. This measures
  useful top-k ordering, not imaginary admission. Context retains its existing cutoff.
* `required`: mandatory admitted IDs; `protected_positions`: protected ID to zero-based original position.
* `ideal_admission_gain`: independently enumerated best feasible sum of `2^relevance - 1`
  under costs/capacity/required constraints, verified exhaustively by the validator.

For recommendations, `ideal_admission_gain` is the sum of **all** independent gains,
with budget equal to all costs: the shared admission term is neutral, not a safety penalty.
The captured full admitted permutation is correct; missing/duplicate IDs remain structurally
invalid. Read-first versus write-first is distinguished by relevance/nDCG at k=1, without
changing either candidate's authored relevance. Context-only budgets and ideal gains stay
unchanged. This is a producer-contract correction, not tuning to any observed answer.
Triage's permission label and progress stall onset 3000 ms remain unchanged: an auth
classification and a 2000 ms detection still fail those existing independent oracles.

Inputs supply candidate physical `size_units` and context optional `budget_units`. The capture
mapping is exactly **128 bytes per unit**: each snippet is padded to `size_units * 128`
UTF-8 bytes and the optional byte budget is `budget_units * 128`. Protected snippets retain
their full physical bytes but have oracle cost zero; they are always admitted and never
subtract optional capacity. For example context-timezone has optional costs 1 and 2,
protected physical size 3/cost 0, and optional budget 2 (not 3). Required policy plus the
relevant offset is feasible with ideal gain 16; both optional candidates cannot fit.
This corrects the development-only oracle/production budget mismatch, not any observed
quality result. Ideal gains are recomputed by exhaustive feasible-subset enumeration and
checked against greedy whole-snippet admission over every candidate order. Frozen held-out
v1 remains unchanged. Do not substitute string byte lengths, unit costs or unconstrained
full capacity. Graded relevance reflects task evidence, not candidate claims or outputs.
Protected mandatory content is distinct from ranking movement. Prefix legacy diagnostics
are not the full constrained metrics oracle; the Rust grader uses actual order/admitted IDs.

Progress expected fields: `kind:"progress"`, nullable `label`, integer `onset_ms`,
`deadline_ms`. Input observations have relative authored `at_ms`. The onset is the end of
the independently labeled evidence sequence supporting stall/drift; abstention has no event.
These are synthetic timing windows, not measurements. Real normal-turn advice detection
requires captured timestamps. A one-shot ServiceConsumer has no detection timeline and
must retain unknown detection latency; never copy authored onset into observed detection.

Classification keeps `kind:"labels",values` or `kind:"absent"`. Session task/domain are
separate dimensions, not partially credited mixed labels. Coordinator conversion must
preserve classification/abstention credit and use all explicit ranking/progress fields.
The current metrics materializer must accept development v3 plus frozen held-out v1 and
stop inventing ranking budgets and progress windows. Family IDs identify independent
clusters; additional repeats cannot inflate independent task count.

The shared interface remains
`judgement_benchmark::{BenchmarkPlan,BenchmarkObservation,BenchmarkReport,compare_benchmark}`.
Version-one scheduled feature/task/repetition pairs are Off/On, with missing attempts in the
planned denominator and nullable latency/USD. The full report's per-class precision/recall/F1,
ranking recall and nDCG remain metrics-owner integration requirements; don't equate aggregate
accuracy with those metrics. The Python exploratory `summarize` is not that full report.

## Supported task materialization and offline controls

```sh
PYTHONDONTWRITEBYTECODE=1 python3 fixtures/evals/judgement/benchmark.py validate
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s fixtures/evals/judgement -v
python3 fixtures/evals/judgement/benchmark.py oracle-controls development /ABS/NEW/controls
python3 fixtures/evals/judgement/benchmark.py materialize development direct /ABS/NEW/direct
python3 fixtures/evals/judgement/benchmark.py materialize development editing /ABS/NEW/editing
python3 fixtures/evals/judgement/benchmark.py materialize development swarm /ABS/NEW/swarm --subject-binary /ABS/APPROVED/FRESH/bcode
```

These commands prepare data only, never execute providers or subjects. Set
`TMPDIR=/Volumes/ehdd/bcode-eval-artifacts/tmp`; destinations must be new and outside the
fixture root. Subject freshness, exact identity/config reproduction and permission remain
coordinator prerequisites; merely naming an existing file doesn't verify an approved binary.
No mutable build/PATH or historical swarm fixture binary is substituted.

Direct reuses the existing `fixtures/evals/swarm/coding.toml` direct-agent design: `agent`
executor with isolated `config_toml` judgement policy. This is actual direct **task execution**,
not a tool-only `direct_tool` microbench or a substitute for consumer capture. Editing uses
the same supported agent boundary and bounded file/shell tools. Swarm reuses the supported
`workflow` executor and `collaborate` strategy, explicit binary, workflow publication/edit
plugin config and judgement overlays. Concurrency is one; attempts and iterations are
bounded. Cases have independent command oracles and byte-preserved inputs. No opaque CLI
command adapter or guessed direct/swarm operation is emitted.

Fourteen arms are seeded/counterbalanced within each task/repetition block: global-off,
six only-feature, all-on, six without-feature. Isolated comparisons are global-off versus
only-feature; all-on/leave-one-out are separate interaction studies and cannot set
`isolated_feature:true`. All settings other than the treatment must match. Match model,
auth/config layers, tools, inputs, timeout/retry and cache conditions. No parallel framework.

Offline oracle-control manifests use the existing `command` executor and metric-threshold
judge. Correct/wrong/missing/corrupt producers return zero only if the independent oracle
accepts/rejects as expected. Their source is scorer_calibration, never consumer evidence.
Tests execute all 144 development control commands in fresh copied disposable workspaces,
without Bcode, providers, credentials, network or services. Wrong controls passing calibration
means the oracle rejected them, not that quality passed. Preserve calibration artifacts.

## Actual consumer capture readiness

Coordinator-owned capture jobs and CLI dispatch are separate from task materialization.
Use supported `CaptureJob`/`ServiceConsumer` schemas, not arbitrary command strings or
expected provider answers. The implemented service paths run the actual producer
prepare → central optional judgement → apply algorithms for triage, recommendations,
progress and session categorization. Context and eval categorization public capture APIs
are available. These service microbench paths are actual service algorithm evidence,
**not** proof of automatic ordinary-turn hookup or whole-task consumer selection.

Task manifests may trigger consumers naturally, but an answer.json alone never proves that
an enhancement ran or contributed. Collect corresponding public bounded captures, operation,
subject/pairing identity, consumer result/fallback, actual ranking/admission, discovery,
verification/inference counts, errors/retries and terminal outcome. Missing capture remains
unknown/unavailable. Progress ServiceConsumer cannot establish window detection delay;
ordinary loop/workflow capture must provide that. Workflow history marked unavailable stays
degraded even if advisory diagnosis is useful.

No DB/private persistence/provider secrets or raw request replay. Eval remains a leaf;
runtime telemetry is domain-generic. Advice never changes permissions, retry authority,
completion or stable terminal outcomes, nor persists request-only judgement input implicitly.
Scorer-calibration, offline-consumer and live-consumer provenance remain distinct.

## Public capture and comparison

The selected subject binary provides `judgement benchmark-capture --file JOB.json`
and offline `judgement benchmark-compare --file COMPARISON.json`. Capture requires an
already-running exact-artifact/state daemon; it does not start one. The CLI verifies
its executable path, SHA-256, artifact and runtime state-scope identity before execution.
Worker control continues to use its separately pinned canonical runtime executable.

`packages/eval/src/judgement_capture/materialize.py` converts bounded corpus input into
shared plans and off/on jobs. Generated jobs default to `allow_live: false` and unknown
feature isolation. Generation is not permission to execute or proof of applied overlays.
Legacy held-out cases lacking compatible graded admission or trajectory truth produce
explicit materialization gaps; frozen truth is not silently rewritten.

Capture distinguishes producer service microbenchmarks, public operations and ordinary
turns. Progress trajectory replay records first advice at the supplied corpus event time,
not wall-clock detection. Service invocation counters are not transport retries. Central
observations preserve outcome, effective provider/model, elapsed time, verification cache
status and inference usage; missing verification usage or pricing stays unknown.

Reports include raw shared-oracle scores separately from promotion-eligible comparisons.
Unknown configuration, safety, persistence, billing or required task coverage blocks a
positive verdict. Raw service scores never establish normal-turn quality or efficiency.

## Budget, gates and honest readiness

**No live calls by this lane; any additional approved live pilot is serial and capped at
$1 aggregate.** Full generated schedules are not authorized by the cap. With development
v3, two repetitions × 36 tasks × fourteen arms = 1,008 attempts per mode. A full three-mode
study is 3,024 attempts and cannot be assumed affordable. Coordinator must approve a fresh
subject, exact configuration/model/profile, verified current prices including retry/cache/
verification billing and worst-case per-request charges before admission. Stop before any
request whose worst-case charge exceeds the remaining cap; unknown billing prevents spend.
Never silently relax budgets, claim unknown usage is free, or launch the full schedule.

Select a pilot pair independently of favorable results and only if the complete worst-case
charge fits. Pilot checks capture, triggers, safety and variance, not gains. Preregister
minimum independent tasks (at least five per feature where gates require it), repetitions,
completeness, practical quality gains, reliability/latency noninferiority, USD limits and
multiple-comparison policy. The current twelve-case holdout remains insufficient.

Report per-feature/per-mode scheduled/observed/missing attempts, fallback and terminal
reliability, negative false positives, artifact/preservation quality, per-class precision/
recall/F1, ranking recall/nDCG and constrained admission, progress detection/false alarms,
paired nullable whole-run/incremental latency/usage/USD. Require zero observed off-arm
optional calls and zero unauthorized effects/authority changes; missing telemetry is an
unknown gate, not passed. Include failed/cancelled attempts, don't select only successes.
Cluster by independently authored families, not repeats. Descriptive gates are not
statistical significance; underpowered or incomplete results remain inconclusive.

Source-ready means corpus/configs/oracles are reviewable and offline controls runnable.
It does not claim live CI gate closure, independent production truth, completed metrics
reporting, whole-turn capture coverage, or any measured improvement.
