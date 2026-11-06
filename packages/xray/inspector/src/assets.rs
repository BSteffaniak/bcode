//! Assets embedded in this exact CLI artifact; no runtime filesystem or Node dependency.

/// Return URL paths (leading `/`) and their immutable, build-verified bytes.
///
/// An ordinary build without `BCODE_XRAY_WEB_DIST` returns an empty bundle.
/// Distribution builds require a valid explicit distribution at build time.
#[must_use]
pub const fn bundled_assets() -> &'static [(&'static str, &'static [u8])] {
    include!(concat!(env!("OUT_DIR"), "/xray_assets.rs"))
}
