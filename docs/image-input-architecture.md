# Image input transport and verification

## Goal and status

Reduce repeated image transfer without changing the complete semantic context presented to the
model. Wire references and retained conversations do not reduce model context occupancy by
themselves, and prompt-cache savings are distinct from transport savings.

Implemented first slice:

* `bcode model verify-images`: bounded provider-operation probes for image acknowledgement,
  inline follow-up, repeated inline follow-up, and explicitly authorized continuation.
* OpenAI-compatible JSON request-body byte observations at the send boundary. Each attempt
  constructs and serializes its body once; the measured byte buffer is passed directly to HTTP.
  Local HTTP capture tests compare observations with received image-bearing bodies for both
  Chat Completions and Responses. Codex routing tests continue to cover request scoping.
* Responses continuation requires enabled reuse, a nonempty response ID, and an in-range history
  boundary. Otherwise the full inline context is projected.
* Deterministic provider-operation tests and an actual Responses request-projection test.

Not implemented or verified by this slice: provider uploads/file IDs, authorized hosted URLs,
request compression, lazy image hydration, durable reference lifecycle, automatic generated visual
fixtures, tool-result/multiple-image probes, daemon restart/fault evals, and live provider results.
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

Add `--allow-conversation-storage` only when provider-side conversation retention is acceptable.
This authorizes the existing provider conversation-reuse mechanism, not a new upload service.
The harness does not delete retained conversations: no portable deletion operation exists yet.
Provider retention policy continues to apply. It finishes local provider turn handles after each
probe and attempts cancellation plus finish on failures.

PNG/JPEG/GIF/WebP fixtures are signature-checked and bounded to 3 MiB in the CLI. Use nonsensitive
fixtures: the image/question are sent to the selected provider. Command-line arguments, including
expected answers, can appear in shell history/process listings. Harness reports omit image bytes,
answers, and remote IDs; they are not a promise to suppress separately configured provider tracing.

The suite starts at most four turns per model (provider-internal retries may issue more requests).
It limits output/event accumulation and uses a per-turn timeout. Blocking invoker operations also
require configured network timeouts; this is not a hard preemptive wall-clock or monetary budget.

The initial image turn asks only for `READY`. The answer to the visual follow-up is withheld from
all requests, and the same acknowledgement history is used for inline and continuation variants.
This prevents an earlier assistant description from trivially answering the later question.
Exact trimmed, ASCII-case-insensitive matching is intentionally simple: use an unambiguous fixture
and short answer. A model's verbal answer alone is not proof of identical image processing.

Reports use schema version 1. Readers must reject unknown report versions. Cases distinguish
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
requires reported continuation use before claiming a reduction. A complete workload assessment must
also include the initial seed/upload and all retries, not just the follow-up. Latency is local
elapsed time. Future upload/transport observations must remain independently labeled and must not
contain signed URLs or credentials. Cache usage analysis remains owned by `bcode_prompt_cache`.

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
