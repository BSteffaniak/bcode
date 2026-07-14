# Session migration fixtures

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
runtime/loop architecture migration. They are compatibility inputs only and
must never become templates for new writes.

Keep fixtures small, intentional, and documented with the schema version and
expected migration/status outcome.
