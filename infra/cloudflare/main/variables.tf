variable "cloudflare_api_token" {
  description = "Cloudflare API token with Workers, DNS, and R2 permissions."
  type        = string
  sensitive   = true
}

variable "cloudflare_account_id" {
  description = "Cloudflare account id."
  type        = string
}

variable "cloudflare_zone_id" {
  description = "Cloudflare zone id for the bmux.dev zone."
  type        = string
}

variable "models_hostname" {
  description = "Production hostname for the model catalog Worker."
  type        = string
  default     = "models.bmux.dev"
}

variable "preview_hostname" {
  description = "Preview hostname for the model catalog Worker."
  type        = string
  default     = "models-preview.bmux.dev"
}

variable "worker_script_name" {
  description = "Cloudflare Worker script name deployed by Wrangler."
  type        = string
  default     = "models-catalog"
}

variable "preview_worker_script_name" {
  description = "Preview Cloudflare Worker script name deployed by Wrangler."
  type        = string
  default     = "models-catalog-preview"
}

variable "live_bucket_name" {
  description = "R2 bucket for production live provider snapshots."
  type        = string
  default     = "models-catalog-live"
}

variable "preview_live_bucket_name" {
  description = "R2 bucket for preview live provider snapshots."
  type        = string
  default     = "models-catalog-live-preview"
}

variable "bedrock_regions" {
  description = "AWS regions queried for Bedrock model discovery."
  type        = list(string)
  default     = ["us-east-1", "us-west-2"]
}

variable "preview_bedrock_regions" {
  description = "AWS regions queried by preview Bedrock model discovery."
  type        = list(string)
  default     = ["us-east-1"]
}

variable "live_fresh_for_seconds" {
  description = "Seconds before a live snapshot becomes soft-stale."
  type        = number
  default     = 900
}

variable "live_max_stale_seconds" {
  description = "Seconds before a live snapshot becomes hard-stale."
  type        = number
  default     = 21600
}

variable "live_refresh_lock_seconds" {
  description = "Seconds before a refresh lock expires."
  type        = number
  default     = 120
}

variable "live_refresh_failure_cooldown_seconds" {
  description = "Seconds to suppress automatic refresh after a provider failure."
  type        = number
  default     = 300
}

variable "dns_placeholder_ipv4" {
  description = "Proxied placeholder A record target for Worker-routed hostnames."
  type        = string
  default     = "192.0.2.1"
}
