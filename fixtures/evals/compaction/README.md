## Deterministic adversarial synthesis cases

The explicit driver accepts `--outcome repair|oversize-repair|omit-scope-repair|paraphrase-scope-repair|near-limit`
with an explicitly identified offline binary. The default remains `success`. The check script runs
these repeated-compaction/restart cases in addition to the automatic interruption cases. No live
provider or semantic quality prerequisite is involved. Example (coordinator-run validation):

```sh
python3 fixtures/evals/compaction/run.py --binary /absolute/identified/bcode --outcome omit-scope-repair
```

The opt-in fake setting `fake_compaction_outcome` also accepts persistent `malformed`, `oversize`,
`omit-scope`, and `paraphrase-scope`, which corrupt correction attempts too. Repair variants corrupt
initial output only. Empty scope cannot be omitted/paraphrased; the delivery verifier requires an
actual effective fault elsewhere, never treating a configured setting as evidence. `near-limit`
pads complete serialized output to 5924 characters, preserving nested continuation state; an already
larger output fails instead of clipping. This is a deterministic fixture, not a model of semantic
summarization or a host input-budget oracle.

Scratch control v1 gains an additive `response.json` file containing exact emitted text, including
malformed/oversized text. Like request captures, this is sensitive synthetic diagnostic evidence,
not portable history or proof of acceptance. `verify_injections.py` checks observed fault delivery
and same-source correction separately from canonical export verification. Every actual marker now
checks independent scope, original provenance for unchanged restatements, supported same-key
supersession, duplicate scopes, and request-only exclusion. A later correct marker cannot hide an
earlier incorrect acceptance. The simulator does not prove native carry compatibility, durable
checkpoint fencing, automatic recovery, or live-model quality; those retain their own owning tests.

Rust simulator tests additionally cover 32 serialized near-budget carry rounds with an unfinished
directive, canonical completion after intervening tool claims, and every Unicode fragment boundary.
Python vector tests run without builds or daemon control:

```sh
scripts/check-compaction-eval.sh --python-only
```

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
now require automatic canonical error settlement of the exact original admission/activation,
explicitly reporting unresolved provider outcome without any targeted old-turn cancellation,
before a new ordinary same-session turn is admitted. The driver requests a bounded reconnect via
`session watch ID --limit 32 --json`; its exact-session initial snapshot is required before
terminal observation. History/export polling stays non-mutating. The earlier missing descriptor
was caused by fixture configuration outside the session's authorized root, not a demonstrated
requirement to add TUI prepare-open calls: placing the real config at `repo/offline.toml` permits
normal descriptor capture and watcher recovery without broadening authorized roots.
The historical workaround is available
only as `--legacy-restart-cancel`; its result is `legacy_workaround_only`, not automatic recovery
success. The new turn's admission receipt identifies the exact target.
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

**Historical failures:** the coordinator reported all offline compaction E2E cases passing
on artifact prefix `af4ee8de` with explicit old-turn cancellation. Those historical results do not
establish the strengthened restart/crash requirement above. The strengthened driver was executed
against SHA-256 `85a7d00806f3c3aeacd2d538d2a7319a5c18d729783b528de984f8ee5d78be45`:
restart and crash both failed waiting 60 seconds for the exact original turn's canonical error
settlement. Both reached accepted synthesis carry and a held second gate; replacement daemon
readiness/identity succeeded, but public export polling observed no original-turn terminal or new
marker. Neither used targeted cancellation or replacement admission. Scratch artifacts are retained
at `/private/tmp/bcode-auto-compaction-ic4r8f9m` (restart) and
`/private/tmp/bcode-auto-compaction-h67y0321` (crash). Export-only polling was insufficient:
normal attachment, not a history read, is the recovery entry point. Follow-up runs on the same
unchanged hash added `session watch --limit 32 --json` before terminal observation. Both watchers
received exact-session `snapshot` output, remained alive with empty stderr, but still no original
terminal appeared within 60 seconds: `/private/tmp/bcode-auto-compaction-v63_85pm` (restart),
`/private/tmp/bcode-auto-compaction-sok4qf41` (crash). Original starts were at sequence 109; no
replacement submission, targeted cancellation or final marker was observed. These are failures
on the reserved artifact, not passing evidence for the later source-level attach regression.
Offline evidence tests lifecycle/coverage/attribution and actual request composition using
simulated synthesis. Later diagnosis found `execution_descriptor: null` and explicit recovery
deferral because the fixture's sibling config lay outside the session cwd's authorized roots.

**Corrected-fixture evidence:** configs now reside inside each scratch session cwd (automatic
and explicit drivers), and the corpus adapter copies only deliberate offline fixture config bytes
into its private session root while retaining their digest. No recovery source bounds or provenance
checks are relaxed. On unchanged SHA-256
`5a16b068f584d463875792aa49a6a9f84c469f1b8c65a9aaa0b1c9b8170aca6c`, actual restart and
crash E2Es passed at `/private/tmp/bcode-auto-compaction-pg_zf76k` and
`/private/tmp/bcode-auto-compaction-l3c26gob`. Each persisted a genuine version-2 execution
descriptor at accepted sequence 106; exact original activation 109 settled at 128 with `error`
and `interrupted compaction provider outcome is unresolved; no request replayed`. No targeted
cancellation occurred. Only after settlement was the continuation admitted; accepted carry reuse,
lossless fragments, real post-marker request and canonical completed continuation were verified.
This proves safe error settlement plus later continuation, not automatic replay of ambiguous work.
It is independent of this natural-language quality comparison. No live
semantic corpus run or baseline/candidate quality result exists. Live execution requires explicit
provider/model, repetition and spending approval; neither this corpus nor the offline CI gate
authorizes it.

## Corpus execution adapter and remaining blockers

`run_retention.py` is an executable CLI adapter, not a corpus-only placeholder. It requires
explicit absolute baseline/candidate executables and configs; hashes them before dispatch,
checks scratch daemon PID/artifact identity and effective model/provider, and pins profile
provenance to the immutable config. It never builds, accesses databases, copies active state,
or manages existing sessions. All sessions and foreground daemons use private scratch state.

The supported CLI can import role-attributed Pi records and derive a non-mutating probe copy,
but cannot append a new attributed assistant/tool batch to an existing session. The adapter
therefore executes **independent accumulated-prefix diagnostic trials** for each corpus batch.
Each trial performs three real, advancing compactions carrying the actual previous summary,
probes a derived copy, checks that probes did not mutate the source, and obtains a real
exact-turn completed downstream response. Source text and oracle judgments are never included
in probe instructions. Padding is deterministic tool output. Canonical role/byte coverage,
source hashes, real boundaries, summaries, activation/terminal IDs, captures, failures and
binary/config identities are retained. CLI operations and wall time are bounded; they are
**not** provider request/token/dollar caps.

This deviation is not the frozen corpus protocol: batches do not advance in one summary chain,
request-only context is not injected, and selected manifests/checkpoints still require direct
artifact review. Every observation is explicitly `protocol_valid=false`, `semantic_status=unscorable`,
with blockers. It cannot produce a quality pass. Do not reinterpret successful diagnostic
orchestration, canned fake output or word matching as instruction adherence.

Offline diagnostic invocation (requires a separately identified build with the Pi import plugin):

```sh
python3 fixtures/evals/compaction/run_retention.py \
  --baseline-binary /absolute/baseline/bcode --candidate-binary /absolute/candidate/bcode \
  --baseline-config "$PWD/fixtures/evals/compaction/retention-offline-baseline.toml" \
  --candidate-config "$PWD/fixtures/evals/compaction/retention-offline-candidate.toml" \
  --mode offline --profile offline --provider bcode.fake-provider --model fake-cache-prefix \
  --repetitions 3 --max-operations 20000 --wall-secs 3600
```

The paired configs differ only in adaptive packing. Existing explicit, automatic and larger
packing-off/on drivers remain unchanged in scope; the new diagnostic adapter does not replace
their structural assertions. The adapter has offline simulated-CLI orchestration tests, not a
claimed executed-binary result. `scripts/check-compaction-eval.sh --python-only` runs these and
all existing verifier tests without builds or providers.

**Live is blocked in this version**, even with `--mode live --profile sol`, explicit binaries,
configs/model/provider, approval reference, `--authorize-live`, `--accept-prefix-deviation`,
repetitions and operation/wall budgets. There is no supported enforceable provider-spending
cap in this adapter; these flags do not authorize spending or bypass that blocker. No live
run has been performed. Full frozen-protocol execution requires a supported role-faithful
batch-append boundary, request-only context delivery, verified selected manifests, and
provider-level allowances. Those changes are outside these fixtures' ownership.

### Exact missing test-only domain boundary

The repeated-chain protocol remains **incomplete**. Independent prefix imports must never be
accepted as a substitute. A session-owned, feature-gated test-fixture application operation is
needed, exposed through typed CLI/IPC plumbing rather than direct database writes:

* A versioned `AppendAttributedFixtureBatch` request scoped to an explicitly created isolated
  evaluation session, with expected session generation/latest sequence, stable batch/idempotency
  identity, bounded event/decoded-byte allowances, and ordered records with fixture ID, canonical
  user/assistant/tool role, exact UTF-8 text, and matched tool-call ID/name/arguments for tool pairs.
* Session ownership fencing and quiescence checks before any append; reject active model work,
  stale generations, unknown versions, unsupported roles, unmatched tool results, conflicting
  duplicate IDs, or non-evaluation sessions. Atomic append returns exact canonical fixture-ID to
  sequence mapping, new generation and digest. Identical retries return the retained receipt.
* Appending must preserve the same session's existing real compaction markers/checkpoints and
  uncompacted tail. Do not reset/import/replace history or inject synthetic compaction summaries.
  Fixture events are explicit synthetic canonical source records, not claims of executed tools.
* An explicit test-only request-context input, separate from durable source events, is needed for
  the corpus's transient routing note. It must reach the actual compaction request without becoming
  source history/checkpoint/portable-summary content. Direct artifact inspection then verifies its
  exclusion; a prompt asking the model to echo a canary is insufficient.
* Each real compaction needs a bounded versioned observation binding its actual selected source
  manifest/digest and covered boundary to the accepted marker and previous marker. Existing public
  captures can supply raw evidence, but the adapter must verify completeness rather than infer a
  selected manifest from boundary alone. Probe derivation must use the exact marker-generation
  snapshot, copy actual compacted context, and leave the source chain unchanged.

With that boundary, the adapter can append batch 1 → compact → isolated probe, append batch 2 to
that **same** session → compact carrying the actual summary → isolated probe, then batch 3 likewise.
No assistant/tool corpus record is rewritten as user authority. Provider-request/token/spending
allowances additionally need an enforceable provider/runtime boundary before live opt-in can run.


`score_retention.py` consumes versioned **blinded human review packets**, not generated keyword
scores. `test_retention.py` documents executable packet examples. Reviews bind to a SHA-256 of
immutable packet content (excluding reviews), identify the grader/blind label, resolve recorded
disagreements, and separately judge summary and probe with exact excerpts and rationales.
Missing/unsupported/unscorable judgments earn zero. All frozen oracle checks remain equally
weighted; global critical checks are additional mandatory gates with direct checkpoint,
summary, manifest and probe artifact inspection. Synthetic/fake producers cannot pass. Reports
include per-case, per-cycle, per-category and paired repetition scores; comparison rejects
unequal coverage and per-case or critical regression, while preserving absolute 95/90/zero-critical
thresholds. The scorer validates review evidence locations and arithmetic, **not** whether a
human's natural-language judgment is correct; it is not an automated semantic oracle.
