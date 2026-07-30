# Session migration fixtures

Current fixture baseline schema: **42**.

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
* trustworthy unknown event envelopes

The schema-41 fixture covers the historical correlated assistant response segment. The schema-42 fixture covers the current positioned assistant response segment. The schema-43 fixture covers a trustworthy future envelope. The schema-40 fixture records the immediately preceding but unreleased current development shape and is rejected by strict current reads rather than treated as released migration input. Schema-39 fixtures cover plugin status, unknown event kinds, malformed JSON, mismatched identity, and sequence gaps. Named interactive-tool, plugin-automation, presentation, and other pre-cutover typed fixtures were removed with their runtime decoders; old stores are rejected by format epoch before normal event replay.

Keep fixtures small, intentional, and documented with the schema version and
expected migration/status outcome.
