# Collaboration evaluation inputs

## Contribution and corrective-feedback contract

New implementation nodes use `retained_contribution`: the model returns a bounded
report, while the host replaces only the declared field in the exact activation
input. Both direct completion and recovered receipt observation apply the same
policy. All other fields, including goal, limits, verdict and prior feedback, remain
host-retained. The designated field is explicit; it is not a general merge of
model-authored state.

Semantic delegation copies that policy from the authenticated source. Each
integration layer prefixes the retained-state path with the literal `source` key.
Nested corrective delegation therefore reports integrated contributions without
rewriting sibling results or losing the original goal. Advanced staging explicitly
supplies its contribution policy and remains subject to normal graph authorization.

The v6 judgement block retains bounded feedback (`attempted_action`,
`made_progress`, `next_action`) across iterations. Evaluation must distinguish
product progress from repeated inspection and notes. An exact no-progress action
repeated as the next action becomes an explicit replanning directive; it neither
claims success nor invents an external dependency. This deterministic guard does
not detect semantic paraphrases or guarantee model compliance.

Compatibility: existing serialized output policies retain their meaning; unknown
policies fail decoding on unsupported binaries. Existing graph definitions are not
rewritten. Historical states without feedback deserialize, but historical judgement
blocks must still pass exact version/schema admission; upgrading does not silently
execute a v5 block as v6. Active runs require their compatible artifact or an explicit
supported continuation, not automatic topology replacement.

`prompts/goal-collaboration-cases.json` is consumed by the owning-domain generation/launch/correction and explicit continuation tests. It supplies parallel implementation, independent review, absent collaboration requirements, and completed-prerequisite/repeated-nonproductive-scope cases. `review_focus` is a human/model review rubric, not an executable verdict.

Run `cargo test -p bcode_loop_plugin --lib` for deterministic coverage. The generation host returns scripted generic prompts; the tests verify original objective and guidance retention independently of those responses. The workflow-store harness exercises plugin graph lowering, named result collection, corrective delegation and unchanged goal input delivered to evaluation. Explicit allowance continuation retains original criteria, evidence and evaluator configuration. These tests do not execute workers against repository files or prove that a model chooses useful scopes, overlaps work, or integrates contributions.

A live model evaluation remains required: submit each objective/guidance through normal goal generation, inspect both generated prompts against `review_focus`, execute authorized independent scopes, then inspect bounded canonical contributions, timing, correction, integrated artifacts and combined validation. For the fourth case verify actual replanning rather than another audit or completed-prerequisite assignment. For the third case reject invented collaboration requirements or worker counts. Missing evidence is unverified, not success. Do not infer strategy quality from the scripted correction graph.

Cross-domain integration still needs the workflow application/server to provide authorized graph/output inspection and real worker dispatch, cancellation and execution authority. Actual overlap/backpressure and optional concurrency policy require owning runtime/store integration; this plugin slice does not change those contracts or claim their acceptance. No provider-backed evaluation or end-to-end daemon execution is included here.
