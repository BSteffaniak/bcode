//! Home page for the models catalog site.

use bcode_model_catalog_models::CatalogDocument;
use hyperchad::template::{Containers, container};

/// Render the models catalog landing page.
#[must_use]
pub fn home(catalog: &CatalogDocument) -> Containers {
    let provider_count = catalog.providers.len();
    let model_count = catalog
        .providers
        .values()
        .map(|provider| provider.models.len())
        .sum::<usize>();
    let live_model_count = catalog
        .providers
        .values()
        .flat_map(|provider| provider.models.values())
        .filter(|model| model.live.is_some())
        .count();

    container! {
        div padding=32 background="#0d1117" color="#c9d1d9" font-family="monospace" {
            h1 color="#7ee787" font-size=42 margin-bottom=8 { "models.bmux.dev" }
            div font-size=16 color="#8b949e" margin-bottom=24 {
                "A canonical model catalog for Bcode and BMUX: pricing, context windows, capabilities, support status, and generated live provider availability."
            }
            div background="#161b22" padding=16 border-radius=8 margin-bottom=12 border="1, #30363d" {
                div color="#f0f6fc" font-size=22 { (provider_count.to_string()) }
                div color="#8b949e" font-size=12 { "providers" }
            }
            div background="#161b22" padding=16 border-radius=8 margin-bottom=12 border="1, #30363d" {
                div color="#f0f6fc" font-size=22 { (model_count.to_string()) }
                div color="#8b949e" font-size=12 { "models" }
            }
            div background="#161b22" padding=16 border-radius=8 margin-bottom=32 border="1, #30363d" {
                div color="#f0f6fc" font-size=22 { (live_model_count.to_string()) }
                div color="#8b949e" font-size=12 { "live seen" }
            }
            h2 color="#f0f6fc" font-size=24 margin-bottom=16 { "Providers" }
            @for provider in catalog.providers.values() {
                div background="#161b22" padding=16 border-radius=8 margin-bottom=16 border="1, #30363d" {
                    h3 color="#f0f6fc" font-size=18 margin-bottom=4 { (provider.display_name.clone()) }
                    div color="#8b949e" font-size=13 margin-bottom=12 {
                        (provider.provider_id.clone()) " · " (provider.models.len().to_string()) " models"
                    }
                    @for model in provider.models.values() {
                        div background="#0d1117" padding=12 border-radius=6 margin-bottom=8 border="1, #30363d" {
                            div color="#f0f6fc" font-size=14 { (model.display_name.clone()) }
                            div color="#8b949e" font-size=12 margin-bottom=4 { (model.model_id.clone()) }
                            div color="#c9d1d9" font-size=12 {
                                "context " (optional_u32(model.context_window))
                                " · output " (optional_u32(model.max_output_tokens))
                                " · support " (format!("{:?}", model.bcode_support))
                            }
                            div color="#8b949e" font-size=12 { "live: " (live_label(model.live.as_ref())) }
                        }
                    }
                }
            }
        }
    }
}

fn optional_u32(value: Option<u32>) -> String {
    value.map_or_else(|| "—".to_string(), |value| value.to_string())
}

fn live_label(live: Option<&bcode_model_catalog_models::LiveModelMetadata>) -> String {
    let Some(live) = live else {
        return "curated".to_string();
    };
    let regions = if live.regions.is_empty() {
        "live".to_string()
    } else {
        live.regions.iter().cloned().collect::<Vec<_>>().join(", ")
    };
    if let Some(status) = &live.status {
        format!("{regions} · {status}")
    } else {
        regions
    }
}
