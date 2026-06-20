variable "cloudflare_api_token" {
  description = "Cloudflare API token with R2 bucket write permissions."
  type        = string
  sensitive   = true
}

variable "cloudflare_account_id" {
  description = "Cloudflare account id."
  type        = string
}

variable "state_bucket_name" {
  description = "R2 bucket used for OpenTofu remote state."
  type        = string
  default     = "bcode-tofu-state"
}
