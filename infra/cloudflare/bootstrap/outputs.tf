output "state_bucket_name" {
  description = "R2 bucket name for OpenTofu state."
  value       = cloudflare_r2_bucket.tofu_state.name
}
