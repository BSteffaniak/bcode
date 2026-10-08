# bcode-model-catalog

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
