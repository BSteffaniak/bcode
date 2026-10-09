# bcode-model-catalog

## Scoped support evidence

`EvidenceStore::new(state_root)` owns the optional
`model-support-evidence/evidence-v1.json` domain store, not the session database.
Provider integrations supply normalized observations after complete generation or
an explicitly classified definitive error. No prompt, response, raw error,
credential, endpoint URL or account identifier is represented in this contract.
Endpoint and account fingerprints must be domain-separated SHA-256 digests.
Admission order `(attempt_started_at_ms, attempt_ordinal)` governs reconciliation,
not response arrival. Assign ordinals before each actual attempt, including retries.
Success proves only its explicit exercised capabilities; a text-only request does
not establish tools, image, structured-output or cache support.

The document is bounded to 1 MiB/4096 retained capability observations. Evidence
lifetimes are limited to 30 days. Expired latest facts remain as tombstones, so an
older success cannot reappear after a newer denial expires. Version mismatches,
corruption and capacity failures preserve state and degrade only this optional
domain. Writers use a create-new lock and private temporary file, sync data, and
atomically rename on the same filesystem. A crashed writer lock or temporary file
requires explicit inspection/maintenance; normal reads never repair state.

`merge_provider_models_with_evidence` is the central discovered/catalog/evidence
union and emits `ModelCatalogHints.availability` decisions. Missing declarations
are unverified, not unsupported. Evidence is exact provider/auth/surface/endpoint/
account scoped; definitive provider-surface rejection can reconcile other accounts
on the same endpoint, while account denial cannot. Apply local ignore/visibility
policy after this union.

## Offline trusted support publication

Shared publication is an explicit local operation, with no automatic upload or
external central service. The catalog maintainer authenticates/reviews evidence
origins and supplies a JSON `SupportTrustPolicy` containing `trusted_scopes` and
`removal_account_quorum` (at least two distinct accounts). Merely copying a local
evidence file does not establish trust. Allowlist only reviewed public serving
endpoints; private/custom endpoint behavior must never be promoted as another
public deployment's support. Account-denied observations never remove support.

```sh
bcode-model-catalog support-patch-prepare \
  --catalog catalog.json --evidence evidence-v1.json --policy trusted-scopes.json \
  --now-ms 1791468000000 --output support-patch.json
bcode-model-catalog support-patch-apply \
  --catalog catalog.json --patch support-patch.json --policy trusted-scopes.json \
  --now-ms 1791468000000 --revision reviewed-support-1 --output catalog-reviewed.json
```

Inputs and outputs are portable JSON documents (catalog input may come from
`build --format pretty-json`). Output is an atomic new artifact; existing files
are refused. Application revalidates trust and exact base revision, applies changes
to a clone, validates the resulting catalog, and only then publishes the result.
Additions create minimal missing entries without inferred capability/pricing data.
Removals record exact `unsupported_by` deployment targets; other deployments,
capabilities, global model status and operational metadata remain independent.
Later reviewed success restores only its target. The generated catalog artifact
is usable by the existing catalog overlay/distribution pipeline.


Loads hand-authored TOML model catalog definitions and builds static catalog artifacts for Bcode and
 the models site.

## Offline xAI inventory

`catalog/models/providers/xai.toml` is embedded and published by the same generic artifact builder
as OpenAI and Bedrock. Grok 4.7 is the recommended coding model; 4.6, 4.5, 4.3, documented 4.20
variants and Build 0.1 remain available choices. Only exact documented aliases are included.
Prices include the inclusive 200k-prompt-token threshold and documented batch discounts. Sources
are the public xAI model Markdown pages, verified on October 8, 2026. Output caps are not inferred.

Legacy Grok 4, Grok 3 and Grok 4 Fast reasoning/non-reasoning concrete IDs are retained with
unknown current availability and pricing:
current model pages are unavailable, while legacy API documentation still names these IDs.
Grok 4's historical 256k context comes from its official launch announcement. Grok 3 limits and
support targets remain unknown. OpenAI's Grok family patterns continue to enrich explicitly
configured compatibility deployments; they are not xAI inventory or evidence of API availability.
The multi-agent research variant requires Responses, has no client-side/custom function tools,
and does not support `max_tokens`; it has no Bcode deployment and is marked unsupported for
ordinary coding. These limitations come from the dedicated multi-agent guide rather than the
model page's generic function-calling claim. No credentialed
requests or paid inference were used to verify catalog data; public documentation is not a live
account entitlement or end-to-end runtime acceptance test.

`load_cached_catalog(RemoteCatalogOptions::from_env())` is the synchronous offline startup loader.
It supplements the embedded baseline from the existing cache without fetching or writing.
Each cache file is limited to 16 MiB; expired, malformed, oversized and unknown-version overlays
are ignored. Partial remote fallback lists append to, rather than replace, the embedded baseline.
The worker publishes xAI as curated data; only Bedrock currently has scheduled live discovery.

## Provider inventory audit

The registered provider plugins are OpenAI-compatible, Bedrock, JEV and fake. OpenAI-compatible
registers OpenAI, OpenRouter and xAI startup routes. OpenAI and Bedrock have embedded catalogs;
xAI now does too. OpenRouter still lacks a provider-owned embedded catalog and catalog hint,
so its remote discovery cannot supply a dependable offline first-run choice. Other custom
OpenAI-compatible endpoints likewise have no authoritative provider-owned baseline. JEV owns a
narrow verified inventory; fake owns test inventory. No registered standalone Anthropic/Gemini
provider was found. Extending those inventories requires separate authoritative provider work,
not deriving concrete IDs, capabilities or prices from OpenAI wildcard enrichment.
