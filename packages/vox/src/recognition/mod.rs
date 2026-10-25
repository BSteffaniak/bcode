//! Recognition adapters selected explicitly by configuration.
pub mod cloud;
pub mod local;
use crate::{Error, Result, config::Config, contracts::Recognizer};
/// Construct only the configured recognizer. Local failure never selects cloud.
///
/// # Errors
/// Returns invalid backend selection, missing models or credentials, unavailable
/// capabilities, and backend initialization errors.
pub fn recognizer(config: &Config) -> Result<Box<dyn Recognizer>> {
    match config.backend.as_str() {
        "local" => local::recognizer(config),
        "cloud" => cloud::recognizer(config),
        _ => Err(Error::Invalid("unknown recognition backend".into())),
    }
}
