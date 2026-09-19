# Image input transport and verification

## Goal and status

Reduce repeated image transfer without changing the complete semantic context presented to the
model. Wire references and retained conversations do not reduce model context occupancy by
themselves, and prompt-cache savings are distinct from transport savings.

Implemented first slice:

* Artifact hydration skips capability/model discovery when no artifact-backed tool image exists.
  Within one request, finalized artifact bytes are reused with a 32-entry/5 MiB retention bound;
  every image occurrence stays in the semantic context. Active artifacts are not cached. Range
  reads reject inconsistent totals, revisions, offsets, oversized chunks, and non-progress.
  This is request-local reuse, not deferred provider-boundary hydration.

* `bcode model verify-images`: bounded provider-operation probes for image acknowledgement,
  inline follow-up, repeated inline follow-up, and explicitly authorized continuation. Add
  `--tool-result` to test correlated tool-result image history rather than user images; this
  requires independent provider and model `ToolResultImage` claims. No host tool is executed.
* OpenAI-compatible JSON request-body byte observations at the send boundary. Each attempt
  constructs and serializes its body once; the measured byte buffer is passed directly to HTTP.
  Local HTTP capture tests compare observations with received image-bearing bodies for both
  Chat Completions and Responses. Codex routing tests continue to cover request scoping.
* Responses continuation requires enabled reuse, a nonempty response ID, and an in-range history
  boundary. Otherwise the full inline context is projected.
* Deterministic provider-operation tests and an actual Responses request-projection test.
  Verifier fault tests cover empty polls/timeouts, missing terminal usage, excessive events,
  oversized output, stale post-terminal text, and cleanup failures. Errors remain normalized;
  failed probes attempt cancellation and finish and do not start further requests.
* `fake-vision-panels` is a dedicated fake-provider model for generated 256x128 PNG panel
  fixtures. It decodes bounded PNG bytes and checks uniform panel pixels in order, rather than
  returning a configured answer. Public-operation round trips cover user and tool-result images,
  the no-image control, and failure after image reordering. It does not implement general vision,
  conversation storage, uploads, or transport measurements. Existing fake models remain unchanged.

* `bash scripts/check-image-input-eval.sh` builds a real CLI/daemon with bundled plugins in a
  scrubbed, isolated environment. It generates a PNG, reads it through `filesystem.read`,
  restarts the daemon, and verifies exact pixel-derived answers and completed outcomes in both
  exported transcripts. Missing-file and truncated-PNG fault cases each require one tool error
  and `UNKNOWN` answers before and after restart, so failed reads cannot count as visual context.
  Artifacts are retained for inspection. Requires Python 3 and Cargo;
  no credentials or remote provider calls are needed. This proves inline artifact hydration and
  replay, not native conversation storage or upload reuse. The script also runs the matrix
  runner end to end against `fake-vision-panels` for two seeds and both image sources. Fault
  assertions count unique failed invocation IDs so duplicate result delivery is not mistaken
  for an additional tool execution.

Not implemented or verified by this slice: provider uploads/file IDs, authorized hosted URLs,
request compression, lazy image hydration, durable reference lifecycle,
fault-injection evals, and live provider results.
No provider capability claims have been upgraded based on offline tests.

## Ownership and target request pipeline

Canonical session history and authorized artifacts define a complete semantic request. Images bind
to immutable content identities, MIME types, detail settings, roles, and conversation positions.
A provider request may transmit a continuation or image reference instead of those bytes, but it
must preserve that semantic request. Provider state is derived, never canonical history.

The target pipeline is:

1. Resolve the intended bounded context and authorized image sources through application APIs.
2. Resolve provider/model capabilities through the model catalog and negotiation path.
3. Verify continuation coverage against the exact compatible semantic prefix and account scope.
4. Prepare uncovered content using provider references, authorized URLs, or inline bytes.
5. Apply existing prompt-cache planning without inventing different context-accounting rules.
6. Execute and publish normalized observations; preserve private handles inside provider ownership.

Image source resolution should become lazy: an image covered by validated continuation or a valid
upload reference should not be read/base64-encoded again. A plugin must receive bounded authorized
content access, not private session database handles or unrestricted artifact paths.

Provider plugins own upload wire formats, credentials, remote reference interpretation, deletion,
and retry safety. The application supplies policy and content authorization. Portable model
contracts describe semantics and capabilities. Verification currently belongs alongside provider
conformance in `bcode_model_provider_runtime::image_verification`; do not move provider-specific
behavior into the generic turn scheduler or create speculative packages.

## Capability and policy evolution

Keep independent claims for inline images, upload/file references, remote-fetch URLs,
image-preserving conversation continuation, image prompt-cache eligibility, and accepted request
compression. Claims must include applicable role, format, size, endpoint, model, retention, and
scope constraints. A generic `ImageReference` claim alone is not evidence of an upload API.
Unknown claims remain unknown. Verification records observations separately from catalog claims.

Remote storage requires explicit authorization and retention policy. Do not silently publish local
images, mint publicly accessible URLs, or treat request permission as indefinite-storage permission.
URL fetching requires confinement/access controls and immutable content binding. Remote handles
must be scoped to content plus provider endpoint and auth/account identity; do not share across
accounts merely because bytes match.

The eventual reference store must support scoped concurrent-upload deduplication, expiry,
bounded cleanup, cancellation, daemon restart, and isolated degraded operation. It is derived and
disposable. Unknown durable versions fail closed without blocking unrelated daemon startup.
Missing original content is surfaced, not replaced with OCR or a summary. Retrying an expired
reference is safe only before execution or after verified reconciliation; an ambiguous generation
outcome does not authorize blind replay. Remote cleanup failure is reported explicitly.

Resizing, lossy re-encoding, summarization, and OCR substitution are outside the lossless optimizer.
They need a separate explicit fidelity choice. Mechanisms may compose; uploading a one-off small
image is not necessarily better than inline transfer.

## Current probe usage

Use normal global provider/model selection and catalog resolution. Candidate count defaults to one;
`--id-pattern` narrows the catalog list. Inspect candidates first:

```sh
bcode model verify-images --dry-run
bcode model verify-images --id-pattern 'MODEL_ID' \
  --image ./fixture.png \
  --question 'What color is the square? Reply with one word.' \
  --expected-answer blue
```

For a nonsensitive generated probe, no local files or expected-answer arguments are needed:

```sh
bcode model verify-images --generated-seed 726
bcode model verify-images --generated-seed 726 --tool-result
```

The optional `image-fixtures` provider-runtime feature generates two PNGs with two color panels
each. The CLI enables it; provider-only builds need not acquire image encoding dependencies.
Seeds select reproducible four-color sequences (1,296 combinations). They are not secrets or
cryptographic randomness. The question contains no selected colors; the expected sequence remains
local. Decode-based tests verify the actual PNG pixels and ordering. Run different seeds to reduce
accidental agreement, and retain the no-image control; generated fixtures are evidence, not proof.

Add `--allow-conversation-storage` only when provider-side conversation retention is acceptable.
This authorizes the existing provider conversation-reuse mechanism, not a new upload service.
The harness does not delete retained conversations: no portable deletion operation exists yet.
Provider retention policy continues to apply. It finishes local provider turn handles after each
probe and attempts cancellation plus finish on failures.

PNG/JPEG/GIF/WebP fixtures are signature-checked. Repeat `--image` to supply up to eight images
in order, bounded to 3 MiB total in the CLI (5 MiB total base64 in the harness). Duplicate images
remain separate context occurrences. Use an order-sensitive question, such as asking for each
image's color in sequence, to test ordering. The same ordered set is replayed in baseline and
continuation variants, for either user or tool-result input. Use nonsensitive
fixtures: the image/question are sent to the selected provider. Command-line arguments, including
expected answers, can appear in shell history/process listings. Harness reports omit image bytes,
answers, and remote IDs; they are not a promise to suppress separately configured provider tracing.

The suite starts at most five turns per model (provider-internal retries may issue more requests).
It limits output/event accumulation and uses a per-turn timeout. Blocking invoker operations also
require configured network timeouts; this is not a hard preemptive wall-clock or monetary budget.

A fresh-session `no_image_control` asks the visual question without an image and with reuse off.
If it guesses the expected answer, visual successes and the continuation transfer verdict are
inconclusive. This is evidence calibration, not proof that a model cannot guess. A failed or empty
control response is not accepted as successful calibration.

The initial image turn asks only for `READY`. The answer to the visual follow-up is withheld from
all requests, and the same acknowledgement history is used for inline and continuation variants.
This prevents an earlier assistant description from trivially answering the later question.
Exact trimmed, ASCII-case-insensitive matching is intentionally simple: use an unambiguous fixture
and short answer. A model's verbal answer alone is not proof of identical image processing.

Reports use schema version 1, with an additive `source` field identifying user or tool-result
images (absent in older reports means user). Readers must reject unknown report versions. Cases distinguish
passed, failed, inconclusive, blocked, and unsupported. Zero process exit means no executed
assertion failed; it does not turn unsupported/inconclusive cases into passes. No selected models
or an invocation/cleanup failure produces a command error.

## Measurements

`ProviderRequestProjection.serialized_body_bytes` is optional additive telemetry: absent means
unmeasured, not zero. It measures uncompressed serialized JSON, excluding HTTP/TLS headers,
compression, upload calls, provider URL fetches, and actual socket transfer. Summing it across
attempts is preparation volume, not proof every byte reached the provider. Never describe it as
observed network bandwidth. Legacy adapters can omit it.

The continuation case compares its measured body total to the equivalent inline follow-up and
requires every attempt to report continuation with a consistent nonzero omitted-message boundary,
a verified inline baseline, and a correct continued visual answer before claiming a reduction.
The optional additive `workload_transfer` report compares seed plus inline follow-up against
seed plus continuation, counting all measured attempts. Both variants share the same observed
seed, so this does not measure storage-on versus storage-off overhead. A separate total includes
all verification requests (negative control and repeat included). Missing observations or overflow
remain unknown, and unverified visual context cannot produce a successful workload comparison.
Upload costs must be included when upload mechanisms are added; currently there are none.
Latency is local elapsed time. Future upload/transport observations must remain independently labeled and must not
contain signed URLs or credentials. Cache usage analysis remains owned by `bcode_prompt_cache`.

## Upload lifecycle probe (initial implementation)

An optional typed provider operation `verify_image_upload` accepts only schema version 1 and
explicit `allow_remote_storage`. The OpenAI-compatible implementation requires API-key Responses
mode. It posts a bounded image to `/files` with purpose `vision` and requested one-hour expiry,
retrieves bytes for exact comparison, then attempts deletion of only the returned file ID.
Redirects are disabled; endpoints require HTTPS except literal loopback HTTP for local tests.
Embedded URL credentials, queries, and fragments are rejected before transmission. Each HTTP
operation has a timeout, responses are bounded, and file IDs
are validated before URL construction. Reports expose neither IDs nor response bodies. Unknown
upload outcomes are not retried; invalid receipts report that cleanup is unknown. Expiry is a
requested provider backstop, not a local guarantee. The receipt must confirm a positive lifetime
of at most one hour (`expires_at - created_at`) for the probe to succeed; the additive
`expiry_confirmed` field distinguishes this evidence from the request. Missing or invalid expiry
still triggers deletion, but returns an explicit expiry-unconfirmed diagnostic. Cleanup failures
are explicit. Service cancellation before dispatch prevents upload. After dispatch, the bounded
receipt is awaited so a returned ID can still be deleted; cancellation skips a not-yet-started
verification download but does not skip cleanup. In-flight HTTP phases remain bounded by their
30-second timeout rather than being immediately aborted. This is not durable cancellation or
crash recovery; loss of the process can still leave cleanup unknown.

`bcode model verify-image-upload --dry-run` prints the plan without loading plugins.
`--allow-remote-storage` authorizes one generated-image probe using the configured provider;
`--generated-seed` selects its fixture. The command fails unless both exact bytes and deletion
are verified. It never accepts caller-supplied remote IDs and never uploads local user files.
The typed operation additionally accepts an optional `visual_probe` (resolved model, question,
withheld answer). It sends two bounded, nonstreaming Responses requests using the same file ID,
with `store=false`, then deletes the file even if reference verification fails. Only assistant
output text in completed responses is judged. Local HTTP tests verify repeated file-ID use,
withheld answers, and deletion. Failure tests cover HTTP rejection, incomplete generation,
wrong answers, and oversized responses; each stops before a second generation and still deletes
the created file without exposing upstream diagnostics. `reference_reuse_verified` is absent when
not requested. Add `--verify-reference` to the upload CLI to authorize the two model requests
using the configured model and the first generated image's withheld answer. Storage-only remains
the default; dry-run prints both upload and generation budgets. The command requires successful
reference verification when requested, in addition to bytes, expiry, and cleanup. This probe
currently has no no-image negative control, so visual correctness alone is not proof of use.

The operation is not wired into normal generation. It does not implement
session reference reuse, durable recovery, cancellation reconciliation, or live-tested cleanup.
Authorization/version tests and local HTTP lifecycle tests pass. The HTTP tests verify multipart
purpose/expiry fields and exact bytes, cleanup after oversized download, unconfirmed deletion,
and rejection of unsafe returned IDs. Remote probing remains required
before enabling this in sessions. Existing providers may reject the optional operation without
impacting ordinary turns. A live CLI invocation with authorized `astra-full.toml` was rejected
before upload: `diagnostic = image_upload_requires_api_key`, `upload_attempted = false`.
That configuration uses subscription authentication, not an API-key file endpoint. No remote
file was created. The additive `upload_attempted` report field distinguishes local preflight
rejections from dispatched uploads/unknown cleanup; absence in older reports remains unknown.
Report: `/tmp/astra-upload-probe.json`. The authorized `grok-4-6.toml` upload invocation likewise
returned `image_upload_requires_api_key` and `upload_attempted = false` (`/tmp/xai-upload-probe.json`).
Its declared auth scheme is API-key, so this is unresolved credential availability/resolution,
not evidence that the remote file endpoint lacks support. No remote upload was dispatched.
The adapter now distinguishes missing/empty credentials (`image_upload_credentials_unavailable`)
from subscription credentials (`image_upload_requires_api_key`); both are pre-dispatch failures.
The historical xAI report predates this diagnostic distinction and has not been rerun.
A subsequent supported auth-status check (`BCODE_CONFIG=.../grok-4-6.toml bcode auth status xai
--profile xai`) reported `Configured: true`, `Available: false`, and
`auth_vault_profile_missing`: profile `xai` does not exist. The reported remediation is provider
login. No vault contents were read directly and no alternate credentials were substituted.

## Opt-in request compression

The OpenAI-compatible provider accepts provider setting `request_compression = "gzip"` only
as explicit endpoint-owner opt-in. Default/`off` sends identity encoding; unknown values fail
before HTTP transmission. Gzip is used only when smaller than the original serialized body.
Both Chat Completions and Responses use the same preparation path. No automatic retry with a
different encoding is performed on rejection; this avoids duplicating ambiguous generations.
No provider/model capability is inferred or promoted by this option. Enable it only for endpoints
known to accept gzip. `model verify-images --request-compression gzip` explicitly probes acceptance
without modifying provider config; `off` supplies an identity baseline. This setting is currently
implemented only by the OpenAI-compatible adapter, so check encoded-byte observations rather than
assuming another adapter applied the requested mode.

`serialized_body_bytes` remains the uncompressed JSON size. Optional `encoded_body_bytes` records
the prepared HTTP body after encoding, not actual socket traffic. Image verification cases expose
this separately from serialized bytes, summing all measured attempts. Missing measurements and
arithmetic overflow remain unknown rather than becoming zero or estimated compression savings.
Tests decode the exact request
builder body and assert byte identity, encoding headers, and measurement agreement. This is
lossless transport compression, never image resizing/re-encoding.

## Opt-in provider matrix runner

`scripts/verify-image-matrix.py` accepts repeated `--config PATH --model EXACT_ID` pairs and
runs user/tool-result generated fixtures against each selection. Without `--live`, it prints the
request budget without invoking Bcode. With `--live`, normal configured authentication is used;
model calls may cost money. Conversation storage additionally requires
`--allow-conversation-storage`. Reports are retained in a private temporary directory; no remote
file upload service is enabled. Defaults are two seeds, at most five started turns per source and
seed; provider retries may add calls. The subprocess deadline stops further probes on timeout,
but does not establish remote cancellation. Captured stdout and report parsing are capped at
1 MiB per probe; excess output terminates local work and stops the matrix with an unknown-remote-
completion diagnostic. On POSIX, the probe has its own process group for local termination.
Timeout, output-overflow, and successful-process behavior have offline subprocess tests.

The aggregate verdict covers inline visual verification only, requiring the negative control,
acknowledgement, baseline, and repeat to pass. Unsupported/inconclusive results are not successes.
Continuation and transfer evidence remain separate in each raw report.

### Recorded live evidence

A bounded live run using the user-authorized `astra-full.toml` configuration and exact model
`gpt-6-astra` passed on seed 726 for both user and tool-result images. The no-image control,
acknowledgement, inline follow-up, and inline repeat all passed. Prepared JSON body bytes were:

| Source | Control | Acknowledgement | Follow-up | Repeat |
| --- | ---: | ---: | ---: | ---: |
| User | 652 | 2881 | 3283 | 3283 |
| Tool result | 949 | 3438 | 3840 | 3840 |

A subsequent user-image run on the same Astra configuration and seed explicitly enabled gzip.
All four executed cases passed with `end_turn`: control 652 → 412 bytes, acknowledgement
2881 → 650, follow-up 3283 → 810, repeat 3283 → 810 (serialized → encoded body). The shared
seed plus follow-up was 6164 → 1460 prepared body bytes. Report: `/tmp/astra-gzip-image-report.json`.
This verifies gzip acceptance for that configured endpoint/model only, not other compatible
providers or actual socket traffic. A subsequent tool-result gzip run on the same model and seed
also passed all four executed cases: control 949 → 525 bytes, acknowledgement 3438 → 816,
follow-up 3840 → 966, repeat 3840 → 966. Seed plus follow-up was 7278 → 1782 prepared body
bytes. Report: `/tmp/astra-gzip-tool-image-report.json`. Continuation remains unverified.

Continuation was policy-blocked: conversation storage was not enabled. No remote file uploads
were performed. These observations establish only inline visual correctness for this configured
surface/model and fixture; they do not establish upload, URL, compression, or continuation support.
The run used `--seed 726 --timeout-seconds 45 --live`; local reports were retained under
`bcode-image-matrix-0tohtu20` in the system temporary directory. Credentials and config contents
are deliberately not included here. A bounded `bedrock-fable.toml` probe against
`global.anthropic.claude-fable-5-1`, seed 726, failed for both sources with normalized terminal
reason `error` and category `auth` in both the no-image control and acknowledgement. Reports
are retained under `bcode-image-matrix-1g29s1_z`. This is an authentication-blocked verification,
not evidence that the model lacks vision. No capability claim is changed. Remaining configurations
are unverified. Reports now include optional normalized `stop_reason` and `error_category` fields
so provider failures are distinguishable from incorrect visual answers without exposing raw errors.

An authorized `grok-4.6` matrix run with `grok-4-6.toml`, seed 726, reported `unsupported` for
both user and tool-result input at capability negotiation, before generation. Reports are under
`bcode-image-matrix-p_nwpfbh`. This means Bcode did not obtain affirmative claims for both scopes;
it is not a live demonstration of remote vision failure. No claims were promoted.

```sh
python3 scripts/verify-image-matrix.py --config ./provider.toml --model EXACT_MODEL_ID
# Add --live only to authorize actual model requests.
PYTHONDONTWRITEBYTECODE=1 python3 scripts/test-verify-image-matrix.py
```

## Remaining implementation and acceptance gates

1. Add actual transport counters where observable, retaining the distinction between measured
   serialized bodies and socket delivery. Exact outbound-body serialization is implemented.
2. Add typed transport/retention capability negotiation and policy before any upload effects.
3. Introduce immutable, lazy, bounded image-source access through the authorized artifact boundary.
4. Implement one documented real provider upload/reference mechanism, including account isolation,
   expiry, cancellation, ambiguous upload recovery, deletion, and restart-safe derived state.
5. Add opt-in URL/compression mechanisms only where endpoint behavior is documented and tested.
6. Extend deterministic scenarios for user/tool images, multiple images/order, changed bytes at the
   same path, missing sources, expired references, invalid continuation, edits/compaction, account
   switches, limit rejection, cancellation, and ambiguous execution outcomes.
7. Extend fake-provider daemon evals with inline/optimized comparisons, restart, and fault injection.
8. Run opt-in live provider/model matrices with generated withheld-fact fixtures, bounded budgets,
   retention/cleanup reporting, and separate cache/transfer observations. Start with existing
   Responses, Chat Completions, Codex, and Bedrock surfaces; do not assume equal support.

Acceptance requires preserved semantic context and safe fallback, workload-level transfer evidence,
secret-safe reports, and truthful unsupported/inconclusive outcomes. Tests inspect behavior and
serialized provider requests, not repository source text. No invariant exception is intended.
