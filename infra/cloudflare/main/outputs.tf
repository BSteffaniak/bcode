output "live_bucket_name" {
  description = "Production R2 bucket for live snapshots."
  value       = cloudflare_r2_bucket.live.name
}

output "preview_live_bucket_name" {
  description = "Preview R2 bucket for live snapshots."
  value       = cloudflare_r2_bucket.live_preview.name
}

output "models_url" {
  description = "Production models site URL."
  value       = "https://${var.models_hostname}"
}

output "preview_models_url" {
  description = "Preview models site URL."
  value       = "https://${var.preview_hostname}"
}

output "production_worker_vars" {
  description = "Non-secret production Worker variables mirrored in wrangler.toml."
  value       = local.production_vars
}

output "preview_worker_vars" {
  description = "Non-secret preview Worker variables mirrored in wrangler.toml."
  value       = local.preview_vars
}
