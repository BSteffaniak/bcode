# Session migration fixtures

Current fixture baseline schema: **38**.

When `CURRENT_SESSION_EVENT_SCHEMA_VERSION` changes, update this declared
baseline and add or update a fixture that records the new schema's compatibility
expectations. `scripts/check-session-architecture.sh` enforces that they remain
synchronized.

This directory is reserved for committed binary/text fixtures that exercise
session persistence migrations across released schema versions.

Fixture suites should cover:

* derived index rebuilds
* canonical event-log rewrites
* future-version logs
* corrupt tails / repair-required logs
* idempotent re-apply behavior
* historical event shapes that active migrations must continue to decode

The `plugin-automation-turn-*-v29.json` and `plugin-status-note-v29.json`
fixtures capture the last active schema-29 durable shapes before the generic
runtime/loop architecture migration. The `interactive-tool-request-*-v32.json`
fixtures capture created, resolved, and unresolved retired interactive request
lifecycle records before generic tool exchanges replaced them. The
`mixed-interactive-history-v32-v35.jsonl` fixture captures a contiguous minimal
schema-32 legacy interaction followed by schema-35 history, matching the schema
boundary structure of session `#1e5587bb` without copying private content.
These are compatibility inputs only and must never become templates for new
writes. The `unknown-*-event-kind`, `future-schema-v39`, malformed,
mismatched-identity, and sequence-gap fixtures separate trustworthy opaque
semantics from structural corruption and future-build incompatibility.

Keep fixtures small, intentional, and documented with the schema version and
expected migration/status outcome.
