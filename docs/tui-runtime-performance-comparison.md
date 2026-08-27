# TUI runtime performance comparison

The fixed-workload probe was captured with
`scripts/capture-live-shell-tui-performance-baseline.sh` before and after the BMUX runtime
migration. Raw local artifacts are `target/tui-runtime-before.jsonl` and
`target/tui-runtime-after.jsonl`; both contain the same 30-case matrix plus the schema record.

These are deterministic work-shape probes, not an interactive latency or process-RSS benchmark.
Timing values are local debug-build observations and should be interpreted as regression signals,
not release targets.

## Structural results

* All nine shell-output cases retained identical publication counts and recording byte sizes.
* All nine shell-replay cases retained exact output/emulation/frame byte equality.
* All three targeted transcript-update cases rebuilt exactly one entry and one row.
* All five pending-live-fanout cases retained identical event counts, pending bytes, and serialized
  byte totals, with zero rejected or pass-through updates.
* Renderer parse/layout emitted identical row counts for Markdown and JSON.
* Telemetry enabled/disabled runs retained their expected pending-observation counts.

No measured workload showed amplification or loss after the runtime migration.

## Timing observations

* Shell replay median wall time changed by **-0.5%** across nine cases.
* Pending live fan-out median wall time changed by **-5.3%** and median p95 push latency by
  **-3.0%** across five cases.
* Markdown parse/layout changed from 1,655,816 µs to 1,611,807 µs (**-2.7%**); JSON changed from
  660,178 µs to 636,813 µs (**-3.5%**).
* Telemetry-disabled wall time changed by **-5.0%** and telemetry-enabled by **-7.4%**.
* Targeted transcript updates retained one-entry rebuild behavior. Their microsecond-scale wall
  values were noisy: two improved by 28–30%, while the 2,000-entry case moved from 25 µs to 32 µs.
* Shell-output median wall time changed by **+3.0%**. Individual short cases ranged from -30% to
  +90%, while publication counts and retained bytes were identical; this spread is consistent with
  local scheduling noise and does not indicate work amplification.

### Rendering correctness verification

The complete Bcode TUI library suite (565 passed, 3 ignored), frame-sequence harness, plugin
presentation guard, Markdown projection guard, and PTY acceptance collectively cover the migrated
presentation contract:

* PTY acceptance performs narrow/wide resize, live composer editing, Markdown focus/activation,
  viewport detach/jump/re-detach, permission input, streaming cancellation, and terminal cleanup.
* Frame-sequence and viewport tests preserve stable transcript anchors across live updates, async
  row-height changes, history prepend, resize, and request-draft handoff.
* Root-program tests retain plugin surfaces through temporary native-session navigation and prove
  request-draft updates wait for committed paint.
* Renderer/plugin tests exercise generic fallbacks and rich Markdown image/Mermaid placement,
  while the architecture guards keep projection and plugin-presentation ownership intact.

## Process-memory comparison

Peak resident memory was sampled from the actual pre-migration and post-migration `bcode_tui` test
processes while running the same unchanged large-rich-history boundedness workload:
`tests::large_rich_history_remains_bounded_in_resident_events_rows_and_payload`. The before binary
was built from starting revision `458664eb`; the after binary was built from this migration
worktree. Each process was sampled with `ps -o rss=` every 2 ms for seven independent runs.

* Before peak RSS samples (KiB): 33,488; 33,008; 33,488; 33,488; 33,472; 33,008; 33,440.
* After peak RSS samples (KiB): 32,912; 32,912; 32,912; 32,672; 33,088; 32,608; 32,912.
* Median before: **33,472 KiB**; median after: **32,912 KiB**.
* Median delta: **-560 KiB (-1.67%)**.

This fixed-workload process comparison shows no memory regression after migration. It complements,
rather than replaces, the structural boundedness assertions and interactive queue telemetry.

## Interactive plugin-surface performance contracts

Stable-extent plugin interaction updates use committed surface geometry and terminal damage rather
than rebuilding unrelated transcript, Markdown, or image presentation. A surface that changes its
preferred height takes the relayout path; missing or stale committed geometry falls back to normal
full frame preparation. These are generic host contracts rather than question-specific routing.

Question surfaces retain one bounded layout for the current semantic layout revision and terminal
width. Focus-only updates reuse that layout. Width or extent changes replace it. Sliced rendering
binary-searches ordered question spans and visits only spans intersecting the requested viewport;
the renderer does not retain historical layouts or materialize a full-surface scratch buffer.

The deterministic work-shape checks are:

```sh
cargo test -p bcode_question_plugin --features static-bundled question_tui
cargo test -p bcode_plugin_sdk --features tui typed_interaction_surface_tests
cargo test -p bcode_tui interactive_surface
cargo test -p bcode_tui stable_interaction_redraw
cargo test -p bcode_tui question_interaction_to_committed_presentation_latency_report -- --ignored --nocapture
```

These checks cover maximum-valid question bounds, visible-slice traversal, snapshot and measurement
reuse, generic renderer fallback, clipping, cursor and mouse projection, redraw/relayout behavior,
and equality between localized and full presentation. The ignored question probe measures semantic
Up/Down events through committed terminal presentation against one 17 ms frame cadence and reports
changed cells and full repaints. Its timings are local debug-build regression evidence, not a
universal release guarantee.

For machine-attributed product latency, use the existing capture command:

```sh
scripts/capture-question-interaction-performance.sh target/question-interaction-performance.json
scripts/compare-question-interaction-performance.py \
  target/question-interaction-performance-before.json \
  target/question-interaction-performance-after.json \
  --output target/question-interaction-performance-comparison.json
scripts/capture-tui-product-latency.sh target/tui-product-latency.json
```

The artifacts record revision, toolchain, machine, dirty state, distributions, and gate outcomes.
The comparator rejects incompatible artifact schemas, sample counts, or locked budgets. A meaningful
before/after claim requires separately captured revisions or clearly identified implementations;
repeated samples from one dirty checkout are useful only for validating the comparison machinery and
observing local run-to-run variance.
Do not present one machine's timing as a universal guarantee, and do not substitute runtime queue
admission latency for a question event reaching committed terminal presentation.

## Product verification closure

Managed-runtime terminal and timer latency are covered by a Bcode-bound acceptance test against the
pinned runtime artifact: with 10,000 reliable messages queued, independently admitted input and an
immediately due timer must reach the serialized update within 100 ms. Together with the structural,
timing, rendering, PTY, and process-RSS evidence above, no product verification item remains open.

The final merged-dependency distribution was captured with
`scripts/capture-tui-product-latency.sh target/tui-product-latency.json` using five independent test
processes. Across 250 committed-presentation samples, terminal-input p99 was **0.140 ms** and
due-timer p99 was **0.068 ms**, both below the locked **100 ms** budget. Peak process RSS p95 was
**16,941,056 bytes**, below the locked **33,554,432-byte** budget. The generated artifact records the
exact toolchain, machine, revision, dirty-state marker, sample distributions, and gate outcomes so
later captures remain directly attributable rather than being described as durable or
reconnect-safe state.
