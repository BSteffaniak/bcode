//! On-demand live model discovery helpers for providers that support it.

use bcode_model::ModelMetadataSource;

/// Returns true if the given base URL or dialect indicates an xAI provider.
#[must_use]
pub fn is_xai_provider(base_url: Option<&str>, dialect: Option<&str>) -> bool {
    if let Some(url) = base_url
        && (url.contains("api.x.ai") || url.contains("x.ai"))
    {
        return true;
    }
    if let Some(d) = dialect
        && (d.eq_ignore_ascii_case("xai") || d.eq_ignore_ascii_case("x.ai"))
    {
        return true;
    }
    false
}

/// Returns true if this provider supports live model discovery.
#[must_use]
pub fn is_discoverable_provider(base_url: Option<&str>, dialect: Option<&str>) -> bool {
    is_xai_provider(base_url, dialect)
}

/// Tag a slice of `ModelInfo` entries with `ProviderLive` source.
pub fn tag_as_live(models: &mut [bcode_model::ModelInfo]) {
    for m in models {
        m.metadata_source = Some(ModelMetadataSource::ProviderLive);
    }
}
