resource "cloudflare_r2_bucket" "live" {
  account_id = var.cloudflare_account_id
  name       = var.live_bucket_name
}

resource "cloudflare_r2_bucket" "live_preview" {
  account_id = var.cloudflare_account_id
  name       = var.preview_live_bucket_name
}
