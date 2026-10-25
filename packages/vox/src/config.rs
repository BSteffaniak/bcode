//! Declarative standalone Vox configuration.
use crate::{
    Error, Result,
    contracts::{DeliveryMode, RecognitionOptions},
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
/// User configuration. No credential values belong here.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Optional input device name/identifier.
    pub audio_device: Option<String>,
    /// Maximum capture duration.
    pub max_recording_seconds: u32,
    /// Local or explicitly selected cloud recognition.
    pub backend: String,
    /// Path to installed local model.
    pub model_path: Option<PathBuf>,
    /// Optional Silero VAD model for speech/noise discrimination.
    pub vad_model_path: Option<PathBuf>,
    /// CPU, auto, or backend-specific GPU selection.
    pub compute: String,
    /// Recognition hints.
    pub recognition: RecognitionOptions,
    /// Explicit delivery mode.
    pub delivery: DeliveryMode,
    /// auto, wayland or x11.
    pub delivery_backend: String,
    /// Allow multiline text (external typing is single-line by default).
    pub multiline: bool,
    /// Optional cloud base URL; no credentials.
    pub cloud_url: Option<String>,
    /// Optional cloud model name.
    pub cloud_model: Option<String>,
    /// Environment variable containing cloud API key.
    pub cloud_key_env: String,
    /// Literal phrase replacements, single pass longest first.
    pub replacements: std::collections::BTreeMap<String, String>,
    /// Interpret spoken punctuation.
    pub spoken_punctuation: bool,
    /// Hold-to-talk binding (platform syntax).
    pub hold_shortcut: Option<String>,
    /// Toggle binding (platform syntax).
    pub toggle_shortcut: Option<String>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            audio_device: None,
            max_recording_seconds: 120,
            backend: "local".into(),
            model_path: None,
            vad_model_path: None,
            compute: "cpu".into(),
            recognition: RecognitionOptions::default(),
            delivery: DeliveryMode::Typing,
            delivery_backend: "auto".into(),
            multiline: false,
            cloud_url: None,
            cloud_model: None,
            cloud_key_env: "VOX_API_KEY".into(),
            replacements: std::collections::BTreeMap::default(),
            spoken_punctuation: true,
            hold_shortcut: None,
            toggle_shortcut: None,
        }
    }
}
impl Config {
    /// Load an explicit TOML file. Missing files are errors, not silent defaults.
    ///
    /// # Errors
    /// Returns file access, malformed configuration, or validation errors.
    pub fn load(path: &Path) -> Result<Self> {
        use std::io::Read;
        const MAX_BYTES: u64 = 1_048_576;
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let file = options.open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > MAX_BYTES {
            return Err(Error::Invalid(
                "configuration must be a regular file of at most 1 MiB".into(),
            ));
        }
        let mut source = String::new();
        file.take(MAX_BYTES + 1).read_to_string(&mut source)?;
        if u64::try_from(source.len()).unwrap_or(u64::MAX) > MAX_BYTES {
            return Err(Error::Invalid("configuration exceeds 1 MiB".into()));
        }
        let value: Self = toml::from_str(&source)
            .map_err(|_| Error::Invalid("invalid Vox configuration".into()))?;
        value.validate()?;
        Ok(value)
    }
    /// Validate resource bounds and backend selection.
    ///
    /// # Errors
    /// Returns an error for unsupported backends or recording limits.
    pub fn validate(&self) -> Result<()> {
        if !(1..=600).contains(&self.max_recording_seconds) {
            return Err(Error::Invalid(
                "max_recording_seconds must be 1..600".into(),
            ));
        }
        if !["local", "cloud"].contains(&self.backend.as_str()) {
            return Err(Error::Invalid("backend must be local or cloud".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_failures_do_not_disclose_values() {
        for source in [
            "cloud_url = 123 # private-sentinel",
            "cloud_url = \"private-sentinel\"\nbackend = [",
        ] {
            let file = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(file.path(), source).unwrap();
            let error = Config::load(file.path()).unwrap_err();
            assert!(!error.to_string().contains("private-sentinel"));
            assert!(!format!("{error:?}").contains("private-sentinel"));
        }
    }

    #[test]
    fn oversized_and_non_regular_files_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        assert!(Config::load(directory.path()).is_err());
        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file().set_len(1_048_577).unwrap();
        assert!(Config::load(file.path()).is_err());
    }
}
