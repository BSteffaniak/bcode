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
