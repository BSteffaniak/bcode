//! On-demand live model discovery for providers that support it.
//!
//! Provider plugins invoke these helpers during model listing when the user
//! has configured an xAI (or similar) provider with an API key. The resulting
//! `LiveCatalogSnapshot` is merged into the catalog so that `context_window`
//! and `reasoning` metadata from the live provider API are available to the
//! TUI without requiring a pre-generated snapshot file.

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
