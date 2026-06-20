locals {
  production_vars = {
    DYNAMIC_PROVIDERS         = "bedrock"
    BEDROCK_DISCOVERY_REGIONS = join(",", var.bedrock_regions)
    LIVE_FRESH_FOR_SECONDS    = tostring(var.live_fresh_for_seconds)
    LIVE_MAX_STALE_SECONDS    = tostring(var.live_max_stale_seconds)
  }

  preview_vars = {
    DYNAMIC_PROVIDERS         = "bedrock"
    BEDROCK_DISCOVERY_REGIONS = join(",", var.preview_bedrock_regions)
    LIVE_FRESH_FOR_SECONDS    = tostring(var.live_fresh_for_seconds)
    LIVE_MAX_STALE_SECONDS    = tostring(var.live_max_stale_seconds)
  }
}
