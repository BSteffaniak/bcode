# Passage quality groundwork (synthetic, offline)

This corpus is independent test input, not a generated inventory of the production implementation. Its five short sources are authored synthetic excerpts. The interview adapts the scenario in `../detailed/interview.json`; that older fixture and its tests are unchanged. No actual video, decoded frame, or live model output is included.

## Reuse

Include `#[path = "fixtures/passage/mod.rs"] mod passage;` from an integration test.

* `passage::corpus() -> Corpus` loads format version 1 from `corpus.json`.
* Each `Case` contains ordered `SourceUnit`s, `Obligation`s, one acceptable rendering, and negative `Control`s. Source IDs identify source intervals, not semantic coverage certificates.
* An obligation's `meaning` is the independently written review rubric; `source_ids` identifies the evidence to inspect together. `phrases` are narrow deterministic controls, not the rubric itself.
* `passage::phrase_controls(&Case, &str) -> PhraseReport` returns unmatched obligation IDs and matched known-bad strings. It does not evaluate entailment, attribution, ordering, readability, captions in context, or arbitrary unsupported additions. Faithful paraphrases can fail; negated accounts can pass. The tests demonstrate both limitations.
* `Corpus::patch_scenario` supplies stale/current revisions, targeted replacement text, and unrelated text/image sentinels. It is data for production tests, not an alternative patch implementation.

## Exact proposed production acceptance cases

These are pending tests through production passage APIs, **not capabilities established by the fixture tests**:

1. **Causal explanation and caveats.** Direct-source passage retains temporary-stall mechanism; 200 records/second × 3 seconds = 600 records; sizing is not a measured throughput gain; sustained overload is delayed, not solved. Valid source IDs with a topic-only sentence must not satisfy local source-to-passage review. Passage-to-source review must identify invented rate gains and reversed causation.
2. **Ordered procedure and quantities.** Retain stop writes → record checksum → copy 128-record batches to staging → verify → conditional atomic swap → resume. Mismatch leaves live data unchanged. Retaining all words while moving swap ahead of verification must fail semantic/order review.
3. **Visual-only demonstration.** Keep transcript distinct from visual annotations. The annotated action is red-tab lift before clockwise rotation, not blue-button activation. Review captions as assertions, not decorative exemptions. The corpus models an observation; it does not establish actual motion extraction. A final still alone cannot establish action order, and absent motion evidence must remain explicitly unresolved rather than invented.
4. **Attributed interview disagreement.** Preserve Lin's non-randomized 24-machine result, 80→68 °C, 12 degrees rather than percent, simultaneous firmware/fan confound; preserve Reyes's disagreement and two humidity failures absent from dry-room tests. Do not produce consensus or fan-only causation. Attribution is substantive, not an optional speaker label.
5. **Boundary-spanning explanation.** Local source units preserve unresolved commit status and the forward reference. Boundary/final source review must resolve that reference using both intervals: retry uses the same durably recorded idempotency key; a new key risks duplicate writes. Including both IDs while omitting this condition is inadequate.
6. **Stale targeted patch.** Apply a revision-7 patch to revision 8 through the actual production API: reject, with the entire document unchanged. Also reject incompatible target preconditions without partial mutation. Fixture validation alone proves neither behavior.
7. **Accepted targeted patch preserves unrelated content.** At a matching revision, patch only the retry explanation. Assert unrelated block identity/order/content, text sentinel, image asset, caption, and evidence references remain exactly unchanged. Source-aligned units remain immutable. Use existing document/patch types rather than inventing a test-local mutation engine.
8. **Source-order assembly and final review.** Completion order must not reorder source passages. Independently review the assembled result against original source, including captions and boundary context; a previous clean local review or matching IDs must not bypass final review.
9. **Depth/focus and resource behavior.** Detailed keeps all substantive rubric obligations even with focus; Standard may remain selective. Cancellation and explicit allowance exhaustion must fail explicitly without committing partial replacements or silently switching depth. These require orchestration APIs and do not follow from lexical fixture checks.

## Human/semantic assessment

Inspect source → passage for missing meaning and passage → source for unsupported assertions. Judge each obligation in context, allowing faithful paraphrases; inspect numbers, negation, qualifiers, attributed disagreement, action order and captions. Require readable explanations rather than headings or disconnected fact lists. Label the outcome `supported`, `missing`, `contradicted`, `unsupported`, or `unresolved`, with the relevant source interval and candidate span. A lack of adequate visual evidence is unresolved, not proof of correctness. Reviewers must not infer completeness from ID counts, string counts, or a model's own clean review.

These synthetic fixtures and hand-authored acceptable passages cannot prove live quality, model reliability, full-video visual coverage, or readiness to replace default Detailed. Live quality requires separate authorized evidence.

## Scripted public-pipeline integration tests

`pipeline.rs`, included by `tests/passage_quality.rs`, calls the public `passage::compose_passages` API with a deterministic typed `ModelExecutor`. It is independent of production-private patch helpers. It exercises:

* a stale patch response against the current passage revision: explicit rejection, no successful result, no changed input;
* a source-grounded scripted contradiction finding, followed by a targeted repair: stable edited-block identity and exact unrelated text/image block preservation, plus re-review and independent final review;
* an unresolved contradiction with no repair allowance: failure rather than an assembled document;
* two boundary units: original primary cues retained exactly once, preceding source context distinct from primary, final reviews retaining immutable source plus neighboring prose, and source-order assembly;
* a final boundary finding after clean local reviews: failure rather than accepting the local reviews as sufficient.

The contradiction is deliberately injected and the review finding is scripted from the independent annotation. These tests check host routing, revisions, preservation and failure behavior, **not automatic semantic contradiction detection**. The image sentinel tests a registered image block and caption, with visual inspection explicitly disabled; it is not decoded-media evidence. They do not establish image extraction or actual frame quality. The boundary test checks supplied source and review mechanics, not an inference engine's ability to resolve forward references. No Standard fallback/global composition is allowed by the test adapter.

The public `passage` module is now exposed. In addition to the initial five pipeline cases, `additional.rs` exercises source-backed omission and unsupported-addition findings, malformed finding evidence/targets/ranges, pre-cancellation, and cancellation racing with each passage-stage response. Every executor call verifies exact primary transcript text/ranges against the independently loaded fixture; review comparisons remain deliberately fixture-specific, not a general semantic oracle. Malformed findings must fail before patch execution. Cancellation must prevent successful assembly and subsequent calls. All edits are still applied exclusively by the production pipeline.

Cargo validation is coordinator-owned and has not been run by this lane. The proposed acceptance cases above remain the broader rubric; the scripted tests are a narrow vertical slice, not closure of all nine cases.
