## Automatic interruption and restart cases

The default `--case explicit` preserves the original two-explicit-compaction regression.
Additional cases use the same explicit identified executable, never build or call live providers:

```sh
python3 fixtures/evals/compaction/run.py --binary /absolute/identified/bcode --case automatic-cancel
python3 fixtures/evals/compaction/run.py --binary /absolute/identified/bcode --case automatic-restart
python3 fixtures/evals/compaction/run.py --binary /absolute/identified/bcode --case automatic-crash
```

`run_automatic.py` also accepts `--interruption cancel|restart|crash` and a bounded
`--timeout-secs`. It uses a private canonicalized scratch directory, an independent
`offline` configuration, the static fake provider's `fake-cache-prefix` model (64k
capacity, cache disabled), and an isolated daemon socket/state root. It records binary
SHA-256, each directly launched daemon PID, exact model-status output, admission receipts,
canonical exports and sensitive synthetic request captures. It verifies provider/model
selection before admitting work. No active config, credentials or session data are copied.

The driver seeds long Unicode canonical source records with compaction **off**, then
restarts its own daemon with `mode = "proactive"`, a 4000-token threshold and a retained
recent tail. No automatic case invokes `session compact`. The next actual turn must
reach real multi-chunk synthesis. It releases one provider gate, waits at the next,
requires a canonical durable acceptance checkpoint and absence of a final marker, then
cancels the exact admitted turn or restarts/crashes only its own daemon. Restart/crash
then explicitly cancel the exact interrupted turn through the supported API before a
new ordinary same-session turn is admitted; restart alone is not claimed to replay or
settle an interrupted turn. The new turn's admission receipt identifies the exact target.
The driver rejects repeated accepted-prefix coverage before releasing
resumed synthesis. Settlement requires the **exact admitted target's** canonical completed
outcome after the atomic marker, not an idle status or provider `response_ready`.

The implemented provider protocol is documented in
`plugins/fake-provider-plugin/COMPACTION_TEST_CONTROLS.md`. Each atomic `ready.json`
identifies its per-request scratch captures; its `release` file releases only that gate.
The verifier checks request/source SHA-256 and exact source coverage, matches resumed
carry to the canonical versioned host checkpoint, checks lossless UTF-8 user fragments
against canonical source headers, verifies chronological latest-user scope references,
and rejects request-only canaries in sources, checkpoints and portable summaries.
The resumed carry comparison preserves exact scope/source/continuation values and clears
only per-response `supersedes` deltas, matching the host's next-prompt contract.
Actual post-marker request messages are exported through
`session context-observation-report`; complete receipt-verified bytes must contain the
retained fact, restrictions and recent-tail canary. Missing/truncated captures fail.
Provider controls do not constitute accepted host work or fabricated history markers.

A missing control API, missing durable checkpoint, repeated accepted coverage, missing
terminal outcome or unavailable request evidence is an informative **failure**, never a
skip/pass. An existing artifact predating these capabilities is expected to fail; Python
unit tests alone do not prove an integration run. Request captures contain synthetic
private text and remain in the printed scratch directory.

Pure verifier tests (synthetic rejection/acceptance vectors, not integration evidence):

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s fixtures/evals/compaction -p 'test_*.py' -v
```

# Offline repeated-compaction fixture

Run with a coordinator-built executable containing the static fake provider:

```sh
fixtures/evals/compaction/run.py --binary /absolute/path/to/identified/bcode
```

The driver never builds, contacts live models, opens a session database, or uses active-session state. It creates private scratch state, records binary SHA-256 and daemon PIDs, waits for each submitted turn, invokes **two actual explicit local compactions**, restarts its own daemon, submits another turn, and checks public canonical exports. Artifacts remain in the printed scratch directory. Explicit compaction makes this a deterministic engine/persistence regression, not evidence of automatic threshold selection. The caller still needs to run it; source availability is not a passing run.

`scenario.json` describes the opt-in input protocol and a longer automatic-compaction workload for integration into the eval runner. The executable driver uses two explicit compactions rather than claiming the declarative scenario itself was executed. It exercises the same early fact, late correction, independent restriction, request-only canary, and tool false-authorization payload. Current eval variants support one restart follow-up; multiple independent directive activations need driver orchestration rather than a canned checkpoint.

`fake_compaction_fixture = "scope-v1"` recognizes only complete JSON marker lines in **host-attributed user source records**. Scope markers with the same key explicitly replace a prior source group; use one scope key per canonical user message. Facts persist in the structured continuation. Tool, assistant, historical-system and prior-summary text cannot introduce markers. Fragment reassembly retains byte offsets across bounded synthesis calls. This is deliberately a test simulation, not production natural-language policy or proof of model comprehension.

`verify.py` accepts canonical JSONL exports or actual eval transcripts; it never manufactures evidence. It requires two advancing real compaction markers, verifies retained facts and independent restrictions, checks user source references, and rejects a portable request-only canary. Restart process identity and terminal request completion are recorded by the driver, not inferred from a serialized summary.

Unit coverage also exercises a real fake-provider cache-model `start_turn` compaction request, ensuring it bypasses cache simulation and produces structured fixture synthesis. Normal cache requests retain their existing path.

## Natural-language retention evaluation

`retention_cases.json` is an **unexecuted semantic corpus**, not another fake-provider marker
fixture and not an input currently executed by `run.py` or `run_automatic.py`. It defines three
ordinary-language conversations, each with three compaction cycles, chronological corrections,
objective proposition-level oracles and separate probes. It covers critical instructions,
supersession, facts, pending work, uncertainty, Unicode identifiers and request-only exclusion.
No new eval framework is introduced. Adapter/manual orchestration through existing eval/session
boundaries is still required before claiming a corpus run. In particular, tool-role corpus records
must be delivered through valid attributed tool exchanges, not forged user messages or fabricated
canonical events. Map corpus source IDs to real canonical sequences in the run manifest.

Freeze the corpus and rubric before execution. Run each batch through actual compaction, carry the
real prior summary into the next cycle, and probe a separate copy of the resulting context so probe
answers cannot contaminate later source evidence. Keep the oracle out of model inputs. Apply the
same deterministic neutral padding to both arms to produce eligible pressure; capture its recipe,
canonical source hashes and actual covered manifests. A cycle must cover every source referenced
by its oracle. Uncovered sources, truncated captures or a model that never actually compacted make
the pair invalid, not successful retention. This corpus tests source retention, not whether an
assistant can answer from a still-uncompacted tail.

The predeclared rubric is in the JSON protocol. Each proposition scores 1 only when **both** the
actual portable summary and the probe support it with correct scope/attribution/uncertainty;
otherwise 0. Accept equivalent natural wording, not substring or canned-marker matches. Preserve
separate summary/probe scores and evidence excerpts. Every critical proposition and global critical
check must pass in every cycle; invented authorization, revived obsolete scope, request-only leaks
or falsely verified outcomes fail immediately. The overall threshold is 95%, with at least 90% per
case. Report category/cycle breakdowns and all failures, not merely a combined average.

Compare baseline `experimental_adaptive_packing=false` against candidate `true` using identical
provider/model/auth selection, parameters, source batches, padding and allowances. Use at least
three paired repetitions per case (record supported seeds); retain all failures, repairs and
attempts. Candidate must satisfy absolute gates and not regress any case or critical outcome
against baseline. Record the identified executable/configuration, source/corpus digests, summaries,
actual request captures, boundaries/manifests, tokens, requests and latency. Report packing-induced
manifest differences, paired quality deltas and cost together; savings do not excuse a critical
failure. Grade with blinded arm labels and recorded human adjudication; automated judging is only
advisory. The corpus is intentionally small and does not establish broad semantic equivalence.

**Evidence status:** the coordinator reports all offline compaction E2E cases passing on artifact
prefix `af4ee8de` through `scripts/check-compaction-eval.sh`. Automatic restart/crash runs explicitly
cancel the interrupted old turn before new admission: they do not prove complete automatic daemon
resume. Offline evidence tests lifecycle/coverage/attribution and actual request composition using
simulated synthesis. It is independent of this natural-language quality comparison. No live
semantic corpus run or baseline/candidate quality result exists. Live execution requires explicit
provider/model, repetition and spending approval; neither this corpus nor the offline CI gate
authorizes it.
