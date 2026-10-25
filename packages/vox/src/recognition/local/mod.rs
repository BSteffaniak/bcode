//! Local, final-only recognition. No model downloads or cloud fallback occur here.

#![forbid(unsafe_code)]

use crate::{Result, config::Config, contracts::Recognizer};

#[cfg(feature = "local-inference")]
mod inference;

/// Load a local model and retain its session for use on the recognizer's worker.
///
/// CPU is selected explicitly by the default configuration. `auto` is opt-in;
/// explicit accelerator selections fail rather than silently falling back.
/// Model loading is synchronous; call this on the dedicated recognition worker.
///
/// # Errors
///
/// * Returns [`crate::Error::Unsupported`] when `local-inference` or the
///   requested compute/model capabilities or recognition hints are unavailable,
///   or the native library is incompatible with this build.
/// * Returns [`crate::Error::Invalid`] for invalid configuration, a missing
///   model path/file, or malformed recognition options.
/// * Returns [`crate::Error::Backend`] for model loading or session
///   initialization failures, without exposing native diagnostics.
/// * Returns [`crate::Error::Busy`] or [`crate::Error::Cancelled`] if native
///   initialization reports an occupied resource or cancellation.
pub fn recognizer(config: &Config) -> Result<Box<dyn Recognizer>> {
    #[cfg(feature = "local-inference")]
    {
        inference::recognizer(config)
    }
    #[cfg(not(feature = "local-inference"))]
    {
        let _ = config;
        Err(crate::Error::Unsupported(
            "local recognition requires the local-inference feature".into(),
        ))
    }
}

#[cfg(all(test, not(feature = "local-inference")))]
mod tests {
    use super::*;

    #[test]
    fn unavailable_feature_is_explicit_even_without_a_model() {
        assert!(matches!(
            recognizer(&Config::default()),
            Err(crate::Error::Unsupported(_))
        ));
    }
}
