resource "cloudflare_workers_route" "models" {
  zone_id = var.cloudflare_zone_id
  pattern = "${var.models_hostname}/*"
  script  = var.worker_script_name
}

resource "cloudflare_workers_route" "models_preview" {
  zone_id = var.cloudflare_zone_id
  pattern = "${var.preview_hostname}/*"
  script  = var.preview_worker_script_name
}
