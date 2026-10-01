# Shell delivered snapshot verification

## Model-provider representation

Strict OpenAI-compatible response schemas expand local references within a bounded
1 MiB expansion-work budget before dispatch. Live Codex probing of the admitted
`LoopWorkflowIteration` schema on 2026-09-30 reproduced HTTP 400 with nested
`Check -> CheckExecution` references; the same full schema was accepted after
inlining. Constraints and the canonical schema remain unchanged. Recursive,
external or unresolved references and unsafe reference siblings fail locally.
The opt-in provider test `live_structured_schema_acceptance` accepts an explicit
`BCODE_SCHEMA_PROBE` JSON schema path, `BCODE_SCHEMA_PROBE_MODEL` and
`BCODE_SCHEMA_PROBE_PROFILE`; it sends only a tool-free compatibility prompt
through normal auth resolution, not the original goal or repository content.


The canonical `files` map remains unchanged. For strict OpenAI-compatible structured
responses, the provider adapter encodes dynamic maps as arrays of exact `key`/`value`
records and decodes the completed response before emitting canonical text. Duplicate
keys and malformed records reject; encoded text is buffered with a 1 MiB bound and
is not emitted on cancellation, failed or incomplete responses. Canonical workflow
schema and delivery validation still apply after decoding. Unsupported compositions
fail before dispatch rather than silently changing map semantics.

Closed-object normalization now rejects unencoded dynamic maps. Other providers
without a reversible adapter (including the current Bedrock structured-output path)
report incompatibility instead of constraining `files` to an empty object. Local
normalization tests are not evidence of acceptance by a live model endpoint.


`shell.exec` command-plan version 2 accepts optional `delivered_snapshot`:

```json
{"version":1,"files":{"src/example.txt":"complete delivered bytes\n"}}
```

This is the **entire delivered target**, not a file-selection manifest for the
workspace. Version 1 supports 1–64 UTF-8 regular non-executable files, at most
256 KiB including paths, with implicit parent directories. Paths are relative,
portable and traversal-free. Binary content, symlinks, special files, executable
modes, explicit empty directories and larger targets are unsupported. There is
no ignore list. Omitted workspace files are not implicitly part of this target.
Callers must not relabel this evidence as verification of a repository checkout.

The shell materializes these bytes into a private temporary directory and runs
the existing authorized command plan there. At least one command is required;
selected-file observations and preconditions cannot be combined with this mode.
After every command the complete directory is compared, rejecting extra, removed
or altered files/directories and executable mode changes. Commands retain their
normal exit, timeout, cancellation and artifact outcomes. Temporary materialization
is execution scratch, not the delivery location. The complete inline target is
retained in `snapshot_verification` in the canonical output.

The portable `bcode_shell_models::{DeliveredSnapshot, SnapshotVerification}`
contract provides `validate()` and `accept(&DeliveredSnapshot)`. At acceptance,
load the checksum-verified canonical output from the shell producer, authenticate
its operation and exact command plan through existing workflow evidence APIs,
then call `accept` against the exact inline target being delivered. Agent-written
JSON is not evidence. Unknown versions, failed commands, changed target content,
incomplete targets and mismatched delivered bytes reject. Command identity and
criterion coverage remain the consumer's responsibility; a successful arbitrary
command is not proof of an acceptance criterion.

Freshness is immutable-delivery semantics: acceptance covers only the retained
inline bytes, never a mutable live workspace copy or a later export. No scheduler
claim is treated as a filesystem lock. This is not a sandbox or hermetic build:
executables, absolute paths, external dependencies, network, environment and
concurrent command-spawned processes are not certified. Checks must be selected
with these limitations in mind. Intermediate mutations restored by a command are
not detectable and no claim about every input read by arbitrary commands is made.

Legacy observations and V1 goal-delivery rejection are unchanged. Goal integration
must explicitly deliver this target, authenticate its canonical shell output,
bind observed commands to original criteria and distinguish review/unverified
criteria. It must not remove the V1 gate globally. Live-model swarm acceptance
and full-checkout delivery are separate, currently unverified integration work.
