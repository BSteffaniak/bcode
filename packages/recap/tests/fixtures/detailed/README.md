# Detailed deterministic fixtures

`corpus.json` is annotation format version 1, independent of the portable document
schema. All scenarios are synthetic. Times are nonempty half-open millisecond
intervals. Gaps are intentional: timestamps model positions in a longer video, not
an acquired full transcript.

## Scenarios and expected substance

`interview.json` extends the same annotation format through `corpus()` with
`qualified_interview_disagreement`: a non-randomized 24-machine comparison,
degrees versus percent, attributed disagreement, a confounded causal hypothesis,
a humidity-conditional recommendation, and a late correction to the median's
22-machine denominator. Neither speaker's claim implies consensus. The reason
for exclusion (failed logging) must not become evidence of success.

* `dense_numbers_examples_caveats`: six units covering baseline quantities, a
  latency/memory tradeoff, worked arithmetic and its arrival assumption,
  measurement limitations, a sustained alert threshold, and ambiguous-write
  retry safety. A separate hostile transcript observation tests data separation.
* `late_video_details`: sixteen independent calibration checks spanning thirty
  minutes. The final four cover alternate-probe termination and bias, a
  firmware-specific exception, and a per-device correction at 29:40. Dropping
  content after twelve units is an observable failure, not an editorial choice.
* `tutorial_order`: six ordered operations with backup retention, separate build
  path, count AND checksum verification, conditional atomic replacement, and
  rollback safety. `after` records prerequisites without requiring chapters.
* `repetition`: three substantive units despite four observations. Either of the
  first two evidence IDs can support the single TTL explanation. The worked
  example and late permission-change exception remain separate requirements.
* `visual_only`: two visible panel states with no transcript. Mode, temperature,
  lamp color, and causal uncertainty matter. Descriptions are annotations, not
  OCR results, supplied model images, or fabricated speech.

Every item has a canonical summary, required deterministic phrases, and source
support. `standard_required` defines a deliberately selective fixture answer,
not a universal editorial prescription. Every item is required for Detailed;
there is no required chapter count. `evidence_ids` are alternative acceptable
support observations, not a requirement to duplicate equivalent citations.

## Reuse

From another library integration test:

```rust,ignore
#[path = "fixtures/detailed/mod.rs"]
mod detailed;

let corpus = detailed::corpus();
let case = &corpus.cases[0];
let source = case.grounded(bcode_recap::contracts::Depth::Detailed);
// Supply actual extracted media and run the public pipeline here.
// Then validate the output contract before applying deterministic coverage.
```

`grounded` constructs current `RecapDocument`, `Evidence`, `TranscriptSegment`,
and `Asset` contracts. It never includes answer summaries or visual descriptions
in source transcript text. The source title and hostile transcript remain
untrusted source data. `expected_document` supplies canonical precomposed answers
solely for oracle/contract tests; it is not a model implementation.

`coverage` looks only at emitted content blocks, not headings, unused transcript,
or source metadata. It detects missing canonical items, wrong annotated evidence,
wrong visual asset, repeated full explanations, and reversed present prerequisites.
It allows multiple substantive items in one block or section. Matching ignores
case and whitespace but is deliberately narrow: paraphrases can fail. Optional
`forbidden_claims` flag specific source-annotated false assertions even when all
required phrases survive. These are synthetic negative controls, not a general
contradiction detector: unlisted additions, negations, or quotations of a forbidden
claim can be misclassified. Unknown claims require separate source review.
Empty `sections` in a grounding document do not constitute a composed recap.

## Baseline/candidate comparison rubric (not yet run live)

Use identical source bytes/transcript, media sampling, resource allowances,
configuration/profile, focus and depth. Record artifact identity and source hashes.
Evaluate Standard separately: selective coverage is legitimate there, not in
Detailed. Keep the answer key out of all model inputs. Preserve both failures
and successes; exporting is not permission to waive semantic failures.

1. **Mechanical gate:** contract/evidence validity, complete source-window transport,
   cancellation/resource failure behavior, stable prior revision, retained earlier
   repairs, focus not excluding unrelated material, and final audit. Run both
   `detailed_quality` and `detailed_acceptance` targets. Fixture phrase/known-false
   checks are deterministic regression signals, not semantic acceptance scores.
2. **Blind source review:** shuffle baseline/candidate labels. Independently mark
   each annotated unit preserved / partially preserved / absent / contradicted,
   with a source timestamp and output block. Check numbers with units/denominators,
   attribution, logical conditions, causal direction/uncertainty, examples and
   procedure prerequisites. Review *every added claim* for source support, not
   just matched IDs. Any substantive omission, distortion, or unsafe procedure
   change blocks a Detailed quality pass; no aggregate score may hide it.
3. **Editorial comparison, separately reported:** judge readable video adaptation,
   explanatory continuity, useful visuals, nonredundancy and navigation on a 1–5
   anchored scale (1 impedes understanding, 3 understandable with friction,
   5 clear without substantive loss). Do not penalize synonyms, capitalization,
   whitespace, headings or chapter grouping as factual errors. A concise rendering
   cannot win by discarding an example or caveat. Resolve reviewer disagreements
   against the source, recording uncertainty rather than guessing.
4. **Performance, separately reported:** on authorized future paired runs record
   wall time, model calls, input/output usage, repair rounds and failures. Compare
   per case and report medians/ranges, not just pooled totals; use repeated pairs
   before a speed claim. No persistent prompt/checkpoint cache is assumed.

The new controls are hand-authored synthetic annotations, not independent human
inter-rater validation or a representative video benchmark. Only two silent
panel states are present: there is no motion, real OCR, perception or extraction
quality evidence. Late content has intentional transcript gaps. No baseline or
candidate live inference, browser export, real source acquisition, or measured
speed/quality improvement is established by these fixtures.

## Acceptance still requiring integration

* Real local video generation/extraction, source-aligned frames and visual-only
  composition. Reserved PNG paths are metadata only; no fake frame bytes are
  supplied, and no acquisition/extraction success is claimed.
* Multi-window/multi-pass Detailed generation retaining all units, with bounded
  input/output/media resources, cancellation, failure semantics, and no silent
  omission when a resource limit is reached.
* Detailed refinement against actual stored prior revisions, optimistic conflicts,
  stable identities, failed/cancelled-operation preservation, and offline export.
* Standard-versus-Detailed behavior through standalone/headless/plugin entry
  points. These fixtures exercise existing library contracts only.
* Real model/source comparison for completeness, numeric fidelity, logical
  qualifications, tutorial readability, nonredundancy, causal uncertainty, and
  hostile-source resistance. No live inference is performed by this suite.

`detailed_quality.rs` is the independently runnable Cargo integration target for
fixture/oracle/current-contract behavior. The shared-build owner can execute
`cargo test -p bcode_recap --test detailed_quality`; fixture creation itself does
not require a provider, network, ffmpeg, or Cargo manifest changes.
