# models-catalog Worker

Runtime API for `models.bmux.dev`.

The Worker keeps the committed curated catalog as the baseline. Dynamic provider
snapshots refresh on demand with stale-while-revalidate caching. Generated live
snapshots are stored in R2 and are not committed to source control.

## Bedrock pricing ownership

Bedrock inventory is refreshed dynamically by the Worker, but AWS price-list interpretation is
owned by the Rust model-discovery/catalog domain. Deployments run `bcode-model-discovery bedrock
--require-pricing` and publish its normalized snapshot at `/v1/live/bedrock.json`. The Worker uses
that versioned snapshot only as a pricing seed and joins it to current inventory by exact model ID;
it does not parse AWS pricing products.

The Rust discovery reads both AWS Price List offers: `AmazonBedrock` (first-party and open-weight
models) and `AmazonBedrockFoundationModels` (Marketplace-listed models, which is where every
Anthropic model newer than Claude 3 is priced). Inventory names are matched to price-list names
by normalized display name, model ID, and variant-qualifier stripping (`Llama 3.1 70B Instruct` →
`Llama 3.1 70B`).

A refresh fails before writing R2 if the seed is missing, has an unsupported schema, contains no
pricing, or does not price any currently discovered model. The existing R2 snapshot therefore
remains the last known-good value. A newly introduced model may remain unpriced until the next
pricing-seed deployment as long as another current model matched. Deployments run on relevant
source changes, manual dispatch, and a daily schedule.

## Required bindings

```toml
[[r2_buckets]]
binding = "LIVE_SNAPSHOTS"
bucket_name = "models-catalog-live"

[assets]
directory = "../../target/models-site"
binding = "ASSETS"
```

## Required secrets for Bedrock discovery

```sh
wrangler secret put AWS_ACCESS_KEY_ID
wrangler secret put AWS_SECRET_ACCESS_KEY
# optional
wrangler secret put AWS_SESSION_TOKEN
```

## Useful vars

```toml
[vars]
BEDROCK_DISCOVERY_REGIONS = "us-east-1,us-west-2"
LIVE_FRESH_FOR_SECONDS = "900"
LIVE_MAX_STALE_SECONDS = "21600"
```
