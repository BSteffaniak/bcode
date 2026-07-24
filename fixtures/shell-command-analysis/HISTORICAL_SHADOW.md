# Historical shell shadow report

Generated read-only on 2026-07-24 by:

```sh
cargo test -p bcode_shell_command_analysis --test historical_shadow -- --ignored --nocapture
```

The harness opens each session database with SQLite `READ_ONLY`, reads only durable `tool_call_requested` events, deduplicates exact `shell.run` command sources, and performs no writes or repair operations.

## Current corpus

* Session databases inspected: 331
* Unique historical shell sources: 16,620
* Complete structured analyses: 16,563
* Explicitly incomplete analyses: 51
* Parse errors: 6
* Sources containing quoted `;` or `|`: 5,377
* Quoted-separator sources where the legacy raw splitter produced more fragments than structured executable subjects: 5,233
* Newline-containing sources with multiple structured executable subjects: 2,095
* Single-background-operator sources with multiple structured executable subjects: 41

## Before/after interpretation

The previous focused denial investigation scanned 324 databases and found 339 plan-agent shell denials, including 260 conservatively likely false positives, 160 denials containing quoted separators, and confirmed newline/background bypasses. The larger current read-only corpus is not directly policy-comparable because historical sessions used different policies and includes all shell calls, not only denials.

The structural before/after evidence is nevertheless decisive:

* Legacy quote-insensitive splitting would over-split 5,233 currently observed unique sources; structured analysis preserves quoted separators as arguments.
* Structured analysis independently exposes 2,095 newline-delimited and 41 background-delimited multi-command sources, closing the confirmed boundary classes that legacy splitting missed.
* All 57 sources that are unsupported/incomplete or syntactically invalid fail closed rather than automatically allowing.

Policy outcome comparisons remain grounded in the sanitized reviewed corpus, where every known bypass denies, every reviewed parser false positive allows when explicit rules permit it, and policy gaps remain denied.
