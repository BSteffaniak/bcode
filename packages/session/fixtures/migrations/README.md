# Session migration fixtures

This directory is reserved for committed binary/text fixtures that exercise
session persistence migrations across released schema versions.

Fixture suites should cover:

* derived index rebuilds
* canonical event-log rewrites
* future-version logs
* corrupt tails / repair-required logs
* idempotent re-apply behavior

Keep fixtures small, intentional, and documented with the schema version and
expected migration/status outcome.
