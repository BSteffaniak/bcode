# models-catalog Worker

Runtime API for `models.bmux.dev`.

The Worker keeps the committed curated catalog as the baseline. Dynamic provider
snapshots refresh on demand with stale-while-revalidate caching. Generated live
snapshots are stored in R2 and are not committed to source control.

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
