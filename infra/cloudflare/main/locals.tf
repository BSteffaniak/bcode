locals {
  production_vars = {
    DYNAMIC_PROVIDERS                     = "bedrock"
    BEDROCK_DISCOVERY_REGIONS             = join(",", var.bedrock_regions)
    LIVE_FRESH_FOR_SECONDS                = tostring(var.live_fresh_for_seconds)
    LIVE_MAX_STALE_SECONDS                = tostring(var.live_max_stale_seconds)
    LIVE_REFRESH_LOCK_SECONDS             = tostring(var.live_refresh_lock_seconds)
    LIVE_REFRESH_FAILURE_COOLDOWN_SECONDS = tostring(var.live_refresh_failure_cooldown_seconds)
    LIVE_WRITE_HISTORY                    = "false"
  }

  preview_vars = {
    DYNAMIC_PROVIDERS                     = "bedrock"
    BEDROCK_DISCOVERY_REGIONS             = join(",", var.preview_bedrock_regions)
    LIVE_FRESH_FOR_SECONDS                = tostring(var.live_fresh_for_seconds)
    LIVE_MAX_STALE_SECONDS                = tostring(var.live_max_stale_seconds)
    LIVE_REFRESH_LOCK_SECONDS             = tostring(var.live_refresh_lock_seconds)
    LIVE_REFRESH_FAILURE_COOLDOWN_SECONDS = tostring(var.live_refresh_failure_cooldown_seconds)
    LIVE_WRITE_HISTORY                    = "false"
  }
}
