//! Whether frontends still confirm stepping a session into permission bypass.
//!
//! Runtime state, not configuration: frontends confirm the first step into bypass of each
//! launch until the user chooses not to be asked again, which is remembered here.
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ConfigError;

/// The remembered choice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BypassConfirmation {
    /// The user chose not to be asked before stepping into bypass.
    #[serde(default)]
    pub dismissed: bool,
}

/// Default location of the state.
#[must_use]
pub fn default_path() -> PathBuf {
    crate::default_state_dir().join("permission-bypass.toml")
}

/// Load the state; a missing file has the confirmation still asked.
///
/// # Errors
/// Returns an error when the file exists but cannot be read or parsed.
pub fn load_from(path: &Path) -> Result<BypassConfirmation, ConfigError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BypassConfirmation::default());
        }
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    toml::from_str(&raw).map_err(|_| ConfigError::Composition {
        message: format!("invalid permission bypass state: {}", path.display()),
    })
}

/// Remember that the user chose not to be asked again.
///
/// # Errors
/// Returns an error when the state cannot be created or written.
pub fn dismiss_at(path: &Path) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let raw = toml::to_string(&BypassConfirmation { dismissed: true }).map_err(|_| {
        ConfigError::Composition {
            message: "permission bypass state could not be encoded".into(),
        }
    })?;
    std::fs::write(path, raw).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_confirmation_is_asked_until_dismissed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/permission-bypass.toml");
        assert!(!load_from(&path).unwrap().dismissed);
        dismiss_at(&path).unwrap();
        assert!(load_from(&path).unwrap().dismissed);
        std::fs::write(&path, "dismissed = [").unwrap();
        assert!(load_from(&path).is_err());
    }
}
