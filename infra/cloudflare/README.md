# Cloudflare OpenTofu infrastructure

This directory provisions the Cloudflare resources for `models.bmux.dev`.

OpenTofu owns infrastructure shape:

* R2 buckets for live model snapshots
* DNS records for production and preview hostnames
* Worker routes for production and preview hostnames

Wrangler/GitHub Actions own Worker code, static assets, and runtime secrets.
This keeps secret values out of OpenTofu state.

## Required GitHub secrets

Cloudflare infrastructure:

* `CLOUDFLARE_API_TOKEN`
* `CLOUDFLARE_ACCOUNT_ID`
* `CLOUDFLARE_ZONE_ID`
* `R2_STATE_ACCESS_KEY_ID`
* `R2_STATE_SECRET_ACCESS_KEY`

Runtime discovery/deploy:

* `AWS_ACCESS_KEY_ID`
* `AWS_SECRET_ACCESS_KEY`
* optional `AWS_SESSION_TOKEN`
* `INTERNAL_REFRESH_TOKEN`

The internal refresh token is required. Internal routes fail closed when it is
not configured.

## One-click flow

1. Add the GitHub secrets listed above.
2. Run the **Cloudflare Infrastructure** workflow with `action=bootstrap`.
3. Run the same workflow with `action=apply`.
4. Run the **Deploy Models Site** workflow.

After that, pushes to `master` deploy the Worker and static assets automatically.
The Worker discovers dynamic provider models on demand and caches snapshots in R2.

## Local usage

Bootstrap the remote state bucket once:

```sh
cd infra/cloudflare/bootstrap
tofu init
tofu apply \
  -var cloudflare_api_token="$CLOUDFLARE_API_TOKEN" \
  -var cloudflare_account_id="$CLOUDFLARE_ACCOUNT_ID"
```

Apply the main stack:

```sh
cd ../main
tofu init \
  -backend-config="access_key=$R2_STATE_ACCESS_KEY_ID" \
  -backend-config="secret_key=$R2_STATE_SECRET_ACCESS_KEY" \
  -backend-config="endpoints={s3=\"https://$CLOUDFLARE_ACCOUNT_ID.r2.cloudflarestorage.com\"}"
tofu apply \
  -var cloudflare_api_token="$CLOUDFLARE_API_TOKEN" \
  -var cloudflare_account_id="$CLOUDFLARE_ACCOUNT_ID" \
  -var cloudflare_zone_id="$CLOUDFLARE_ZONE_ID"
```

## Security notes

Use dedicated, least-privilege credentials:

* Cloudflare API token scoped to this account/zone with only Workers, Workers
  Routes, DNS, and R2 bucket permissions needed by the workflows.
* R2 state access key scoped to the OpenTofu state bucket.
* AWS IAM principal scoped to Bedrock model discovery/listing only.

Runtime hardening:

* `/api/internal/*` routes require `INTERNAL_REFRESH_TOKEN` and fail closed when
  it is missing.
* Worker errors are logged server-side and return generic public 500 responses.
* Provider refresh has a bounded lock and failure cooldown.
* R2 history snapshots are disabled by default with `LIVE_WRITE_HISTORY=false`.

## Notes

The bootstrap stack uses local state because it creates the R2 bucket that stores
the main stack state. Keep the bootstrap stack small and rarely changed.
