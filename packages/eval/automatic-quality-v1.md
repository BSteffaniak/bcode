# Automatic quality acceptance v1

`fixtures/automatic-quality-v1.json` contains 18 synthetic request-only acceptance inputs
and independent ground truth across all six optional enhancements.
`fixtures/automatic-quality-progress-v1.json` supplies the exact loop-owned version-1
requests keyed by the four progress case IDs (bounded scope/objective/settled observations).
It is not a model-answer
script. `bcode_eval::quality::{representative_quality_suite, compare_quality}` scores captured
consumer outputs. No provider call, CLI process, session read, or implicit input persistence
occurs in the scorer.

## Capture contract

Supply one `QualityObservation` per case per `QualityArm`: `global_off`, `feature_off`,
`auto_eligible`, `auto_no_key`, `auto_error`. Use a deterministic offline provider in the
application harness; do not use live credentials. Instrument central access, counting actual
discovery, verification and inference separately. Neither a configured mode nor a provider
answer alone establishes an executed consumer path.

Capture final consumer results as `QualityOutput`: labels, full ranking, or absence. Label
normalization is caller-owned: triage `category`; progress `state` (`stall`, `productive`,
`drift`, `uncertain`); session `task`/`domain`; eval `failure_category`. Progress probabilities
are not converted into invented categories: the actual service emits `RepeatedFailure` or
`Drift` advice (normalized to `stall`/`drift`), while productive and unknown controls expect
absence. The service does not currently emit a productive-progress label.
Ranking outputs must contain every candidate exactly once. Prefix accuracy leaves irrelevant
candidate ties unconstrained. Ambiguous session input deliberately expects no result: the
existing allowlisted signal extractor does not trigger on that input.

`path` distinguishes actual application consumers, domain consumer tests, and scorer-unit
fixtures. `operation` identifies the bounded public operation/test (maximum 256 bytes);
never include raw output, diagnostics, credentials, or source input. Instrument
`authority_preserved` and `request_only`; do not default them to true without assertions.
Request-only means no *additional implicit* persistence of optional judgement input; normal
explicit prompt acceptance remains canonical behavior.

Global/per-feature off must have zero discovery, verification and inference calls. No-key
must have zero inference. All fallback arms must equal captured global-off output. Eligible
non-abstention cases must actually infer. Constraint failures and missing/non-application
arms are explicit report gaps. Only complete application-path pairs contribute to the
reported auto-minus-off accuracy delta. A positive score from injected expected answers
is consumer/scorer validation, not evidence that a model improves product quality.

## Actual inspected product paths

* Normal `SubmissionActivated` triggers session categorization in server model-turn preparation,
  only outside recovery/request-context execution. The source is UTF-8 bounded; the plugin
  extracts fixed vocabulary before central optional judgement. Results enter the disposable
  sequence-checked cache. `bcode_client::Client::session_categorization` reads those labels via
  `Request::SessionCategorization`, without history replay. The app harness must drive a real
  accepted/activated turn and read this method; invoking the plugin alone is insufficient.
* Workflow eval computes measured oracle facts, calls `diagnosis::classify`, constructs the
  allowlisted optional request, invokes `judgement optional-judge --request-json`, applies
  `diagnosis::attach_advisory`, and writes `diagnosis.json`. The harness must inspect that file
  and unchanged `oracle.json`/deterministic labels. Advisory labels never replace the oracle.
* Context integration uses `bcode.context-selection/v1` prepare/apply over already retrieved
  candidates; the acceptance inputs cover file, history, memory and tool-result candidates.
  Only bounded supplied candidates are permitted—no canonical replay or retrieval repair.
* Ordinary-session/applicable-workflow progress uses the loop-owned bounded observation
  service in `plugins/loop-plugin/src/session_progress.rs`. Adapter construction belongs to
  that domain: version 1 scope/objective and strictly ordered recent settled observations.
  Stall evidence must use repeated failure identity without fresh evidence; productive cases
  include fresh evidence, drift is relative to the existing objective, and empty evidence
  must not fabricate progress or completion authority.

## Coordination and current evidence

Domain lanes own context, recommendations, triage and progress fixture adapters/tests.
Integration owns public app/CLI hooks and central off/no-key/error tests. Quality owns this
fixture/scorer contract and actual diagnosis/session extraction tests. No shared server,
IPC/client/CLI files are modified by the quality lane.

Source-ready does **not** mean validated quality closure. No captured application observations
are supplied yet, therefore no measured improvement or baseline CLI/cache failure is claimed.
An empty capture set reports 90 missing arms, zero eligible pairs and no accuracy delta.
Coordinator must run focused eval/session-derivation tests and shared validation, then capture
all five arms through actual app consumers and generate the measured report. Live app/provider
verification remains outside this offline lane and is not authorized here.
