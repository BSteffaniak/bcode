resource "cloudflare_dns_record" "models" {
  zone_id = var.cloudflare_zone_id
  name    = var.models_hostname
  type    = "A"
  content = var.dns_placeholder_ipv4
  ttl     = 1
  proxied = true
  comment = "Proxied placeholder record for the models catalog Worker route."
}

resource "cloudflare_dns_record" "models_preview" {
  zone_id = var.cloudflare_zone_id
  name    = var.preview_hostname
  type    = "A"
  content = var.dns_placeholder_ipv4
  ttl     = 1
  proxied = true
  comment = "Proxied placeholder record for the preview models catalog Worker route."
}
