# Filesystem multi-edit (implementation in progress)

`filesystem.multi_edit` accepts JSON:

```json
{"files":[{"path":"src/example.rs","edits":[{"old_text":"existing text","new_text":"replacement text"}]}]}
```

Every search matches exactly once against the original file snapshot. Edits do not
match text inserted by preceding edits. Empty batches, empty edit lists, empty
search text, ambiguous/missing matches, overlaps, duplicate canonical targets and
file-identity aliases are rejected before publication. Existing `filesystem.edit`
and `filesystem.write` retain their separate interfaces.

## Mutation semantics

The native TUI hydrates bounded retained old/new source previews through the generic
artifact stream and reuses the single-file diff viewer. It retains at most 64 source
windows of 8192 bytes each; multipart sources preview only their first part. These
are explicitly labeled partial source comparisons, not complete diffs. Full retained
source and unified-diff references remain available through artifact tools. Preview
cache eviction affects presentation only, never persisted outcomes.

The batch validates all snapshots before publication. Replacement is literal:
untouched UTF-8 bytes, BOM, mixed line endings and final-newline state are not
normalized. Multi-edit rejects hard-linked targets. It publishes through
same-directory staging, preserving Unix permission bits and checking staged
content and permissions. This is not a promise to preserve every filesystem
attribute (such as ACLs, extended attributes or ownership).

Bcode mutation coordination is advisory and does not serialize external writers.
Snapshots and identities are checked before publication, but external changes can
still race the final check and rename. There is no portable byte-conditional
replacement guarantee and no cross-file transaction. Earlier commits are not
rolled back or automatically retried when later work fails.

Cancellation stops further work; an already completed publication remains
committed. An invocation that cannot settle must not be interpreted as having
made no changes. End-to-end cancellation and recovery verification remains part
of the unfinished integration work.

## Batch artifact contract

Producer: `bcode.filesystem`. Schema: `bcode.filesystem.batch`, schema version `1`.
This is separate from the existing `bcode.filesystem.change` single-file schema.
Artifact identity and `tool_call_id` associate the result with its invocation;
they do not authorize a retry or establish a durable resume protocol.

Metadata contains `version: 1`, `is_error`, and an ordered `files` array. Each file
contains `path`, `status`, `error` (string or null), and `change` (object or null).
Statuses are:

* `committed`: publication completed.
* `unchanged`: the requested result equals the snapshot.
* `failed`: work failed before a successful publication was reported.
* `cancelled`: cancellation stopped this file before publication.
* `not_attempted`: an earlier stop prevented processing this file.
* `unknown`: a rename was attempted but its outcome is not established. Inspect
  the target before deciding what to do; do not automatically retry.

Consumers must reject unsupported schema/metadata versions and must not interpret
unknown statuses as success. Unknown representations should remain available via
the generic artifact fallback rather than being guessed as a known version.
Consumer enforcement and native batch presentation are not yet complete.

Only confirmed commits contain change data. Exact changed-region before/after pairs
have `omitted: false`, `old_text`, `new_text`, `old_start_line`, and
`new_start_line`. Identical whole-line prefixes and suffixes are excluded before
budgeting; start lines are one-based positions in the original and resulting
sources, not necessarily 1. Empty sides represent line insertion or deletion.
Changed bytes, including final-newline differences, are never truncated or
normalized. These are source text, not terminal layout or execution input.
The current display budget is 16 KiB combined raw text per file and 64 KiB per
batch. JSON escaping can expand text bytes by up to six times. Oversized pairs
instead contain `omitted: true`, `reason: "display_budget"`, `old_bytes`, and
`new_bytes`. Omission never means unchanged. No partial text is presented as a
complete diff. Oversized changes additionally retain an exact whole-file unified
text diff under `retained.diff`, exposed through standard `file-N-diff` artifact
references (or ordered multipart references). Fixed `before`/`after` headers avoid
path injection. This linear-time presentation is not a minimal diff or a mutation
interface. It preserves CRLF and explicit missing-final-newline markers. Its byte
size is at most twice the combined source bytes plus 128 bytes, and only one file's
diff is built at a time. Interactive large-change diff consumption remains unfinished.

The compact text response preserves paths, statuses and errors, replacing change
text with availability and omission reason. Artifact content is not duplicated
into that text response. This alone does not establish how every model-context
or frontend consumer handles artifacts.

## Delivery status and exclusions

Omitted changes attempt to retain complete old/new sources through the host artifact
sink. Each unavailable source includes a bounded reason: `size_limit` (with
`max_bytes`), `cancelled`, `storage_failure`, or `bridge_unavailable`. Arbitrary
host error text is not copied into the result. When a complete source exceeds the
host write limit, retention retries as at most 64 ordered binary parts, each no
larger than that limit. A multipart source has `version: 1`, total `byte_len`, and
`parts` carrying byte `offset`, `byte_len`, artifact identity and reference. Parts
may split UTF-8 sequences; concatenate bytes before decoding. Unsupported multipart
versions must not be interpreted. Each part is exposed as a standard artifact
reference. Single-reference sources retain their existing shape. If any part fails,
the source is unavailable; already written parts are not a complete source.
Native oversized-change summaries show unified-diff references before source
snapshots, including byte-ordered multipart references and explicit unavailable
states. Interactive multipart diff consumption remains unfinished.
Retention failure does not change an already committed outcome.

The editing engine, partial outcomes, outcome artifact and bounded source pairs
are implemented. The native batch adapter reuses single-file diff presentation,
validates version/status/display bounds, and namespaces file source anchors.
Complete exposure/configuration verification, large-change access and end-to-end
acceptance are not yet complete.
This document is not a production-readiness claim.

JSON is the initial portable representation. No measured superiority over
single-edit, JSON-wrapped patches or grammar-constrained patches is claimed.
The behavioral comparison `batch_and_independent_single_edits_produce_identical_bytes`
executes source-symbol replacements, escaped JSON configuration edits, and
BOM/mixed-newline Unicode edits through the plugin tool boundary. For each task,
one batch invocation and two independent single-edit invocations produce identical
expected bytes. This is deterministic execution evidence, not model-generated
accuracy, latency, token usage, permission-dialog, or provider-round measurement.
The replacements are independent; sequentially dependent replacements intentionally
have different semantics and are not claimed equivalent.

Format evaluation remains limited: replacement arrays require JSON escaping of
both search and replacement strings; JSON-wrapped patches also escape their patch
string and add patch syntax/context. Grammar-constrained freeform patches can avoid
that JSON-string escaping but require provider support and a separately validated
parser. Encoded byte counts are not tokenizer counts. No provider/model trial has
established relative edit accuracy, failure rates, or actual tool rounds; there is
no evidence justifying an alternate production interface yet.
Provider-specific encodings, if justified later, belong at provider boundaries
and must reuse normalized authorization and mutation logic.

Fuzzy/regex matching, create/delete/rename languages, automatic shell verification
or LSP diagnostics in the critical path, cross-file transactions and crash-proof
exact mutation accounting are excluded.
