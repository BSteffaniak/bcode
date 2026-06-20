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

    container! {
        div padding=32 background="#0d1117" color="#c9d1d9" font-family="monospace" {
            h1 color="#7ee787" font-size=42 margin-bottom=8 { "models.bmux.dev" }
            div font-size=16 color="#8b949e" margin-bottom=24 {
                "A canonical model catalog for Bcode and BMUX: pricing, context windows, capabilities, and support status."
            }
            div background="#161b22" padding=16 border-radius=8 margin-bottom=16 {
                div color="#f0f6fc" { (provider_count.to_string()) }
                div color="#8b949e" { "providers" }
            }
            div background="#161b22" padding=16 border-radius=8 margin-bottom=32 {
                div color="#f0f6fc" { (model_count.to_string()) }
                div color="#8b949e" { "models" }
            }
            h2 color="#f0f6fc" font-size=24 margin-bottom=16 { "Providers" }
            @for provider in catalog.providers.values() {
                div background="#161b22" padding=16 border-radius=8 margin-bottom=12 border="1, #30363d" {
                    h3 color="#f0f6fc" font-size=18 margin-bottom=4 { (provider.display_name.clone()) }
                    div color="#8b949e" font-size=13 margin-bottom=8 {
                        (provider.provider_id.clone()) " · " (provider.models.len().to_string()) " models"
                    }
                }
            }
        }
    }
}
