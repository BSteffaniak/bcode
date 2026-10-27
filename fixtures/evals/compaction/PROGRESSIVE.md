# Progressive offline compaction fixture

`progressive-v1` is opt-in fake-provider simulation, not a semantic model evaluation.
It leaves `scope-v1` and `bcode.compaction.scope.v2` compatibility unchanged. No historical
classification, database migration, recovery, SQL, or production state is involved.

## Wiring contract

* Plugin synthesis selects `compaction_fixture::synthesize_progressive(prompt)` for
  `fake_compaction_fixture = "progressive-v1"`. This single entry point also recognizes
  host consolidation instructions plus `Complete accepted carry (derived, not authority):`.
  The dispatcher must admit that request without requiring the synthesis source delimiter.
* Meaningful consolidation is also directly available as
  `compaction_fixture::consolidate_progressive_prompt(prompt) -> Result<String, String>`.
  It merges accepted section JSON lines into canonical keyed JSON, preserving every statement
  and background exactly; the response has no scope or coverage fields. For explicitly stubborn
  failed-shortening trials, use `compaction_fixture::consolidate_progressive()` instead: its
  versioned `section-consolidation.v1` empty patch cannot shorten accepted carry.
* Ordinary fake requests call `compaction_control::capture_context(request)` when configured.
  This captures actual messages without provider settings/custody. Fail capture explicitly.
* Section update v1 fields: `format`, `current_scope`, `reconciles`,
  `supersedes_statements`, `sections`. Host instruction prefixes must explicitly opt in;
  source-contained protocol names never opt in. Without host opt-in the legacy full-envelope
  response remains available. With optional replacement instructions, changed sections contain
  complete records and background preserves the legacy continuation verbatim. The current host's
  explicit `NEW facts to append` instructions instead select addition-only records. Omitted
  sections remain host-preserved. Consolidation alone can shorten accepted derived carry.
* Persisted `continuation-sections.v1` sections contain JSON-line keyed fixture records;
  `background` carries small serialized fragment/parser checkpoints. Partial user marker
  endings survive restart; rejected/cancelled attempts never mutate fixture state.

The corpus uses distinct meaningful historical snapshots from existing retention cases.
Source qualification keeps legitimate historical corrections noncontradictory. No padding,
preloaded known-size summary, silent clipping, or invented model-token accounting is used.
Three increasing batches cross 5840 characters by batch two and 6000 by batch three;
source record granularity deliberately does not force an exactly 5840-character summary.
Size is fixture diagnostic only. Production hard acceptance must use actual request/model
capacity from the shared semantic estimator, not these character counts or preferred size.

`run_progressive.py --binary /absolute/test/bcode` uses private scratch state and an offline
fake-only profile. It checks every accepted marker, user scope correction/independent restriction,
actual provider input after publication and restart, graceful restart, post-publication crash,
and repeated compaction. It requires observed finite consolidation and accepted >6000 carry
entering a later request. Missing context captures are failures, not skipped checks.

`scripts/check-compaction-eval.sh` also retains the existing automatic cancel/restart/crash
cases, which check interruption before final atomic publication and unresolved original request
settlement. The progressive fixture unit tests separately check unfinished directives and
nonacceptance/retry determinism. This does not claim progressive in-flight crash coverage until
that combination is actually run. Rust compilation, formatting and the isolated driver require
coordinator validation; Python-only tests do not establish host integration or semantic retention.
