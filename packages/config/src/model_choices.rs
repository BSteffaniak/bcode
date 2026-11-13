//! Models the user recently chose or marked as favorites in the model switcher.
//!
//! Runtime state, not configuration. Each choice is an opaque, nonsecret key that the
//! switcher derives from a model, its route, and its account reference; this module only
//! keeps their order and membership.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ConfigError;

/// Most recent choices retained.
pub const RECENT_LIMIT: usize = 8;

/// Recent and favorite model choices.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelChoices {
    /// Most recent first, without duplicates, at most [`RECENT_LIMIT`].
    #[serde(default)]
    pub recent: Vec<String>,
    /// Choices the user marked as favorites.
    #[serde(default)]
    pub favorites: BTreeSet<String>,
}

impl ModelChoices {
    /// Move `key` to the front of the recent choices.
    pub fn record_recent(&mut self, key: &str) {
        self.recent.retain(|existing| existing != key);
        self.recent.insert(0, key.to_owned());
        self.recent.truncate(RECENT_LIMIT);
    }

    /// Add `key` to the favorites, or remove it when it is one; returns whether it is now one.
    pub fn toggle_favorite(&mut self, key: &str) -> bool {
        if self.favorites.remove(key) {
            false
        } else {
            self.favorites.insert(key.to_owned());
            true
        }
    }
}

/// Default location of the model choice state.
#[must_use]
pub fn default_path() -> PathBuf {
    crate::default_state_dir().join("model-choices.toml")
}

/// Load the choices; a missing file has none.
///
/// # Errors
/// Returns an error when the file exists but cannot be read or parsed.
pub fn load_from(path: &Path) -> Result<ModelChoices, ConfigError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ModelChoices::default());
        }
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    toml::from_str(&raw).map_err(|_| ConfigError::Composition {
        message: format!("invalid model choice state: {}", path.display()),
    })
}

/// Apply `change` to the stored choices and write them back.
///
/// # Errors
/// Returns an error when the state cannot be read, created, or written.
pub fn update_at(
    path: &Path,
    change: impl FnOnce(&mut ModelChoices),
) -> Result<ModelChoices, ConfigError> {
    let mut choices = load_from(path)?;
    change(&mut choices);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let raw = toml::to_string(&choices).map_err(|_| ConfigError::Composition {
        message: "model choice state could not be encoded".into(),
    })?;
    std::fs::write(path, raw).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(choices)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recents_move_to_front_without_duplicates_and_stay_bounded() {
        let mut choices = ModelChoices::default();
        for index in 0..RECENT_LIMIT + 3 {
            choices.record_recent(&index.to_string());
        }
        choices.record_recent("5");
        assert_eq!(choices.recent.len(), RECENT_LIMIT);
        assert_eq!(choices.recent[0], "5");
        assert_eq!(choices.recent.iter().filter(|key| *key == "5").count(), 1);
    }

    #[test]
    fn favorites_toggle_and_choices_persist() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/choices.toml");
        assert_eq!(load_from(&path).unwrap(), ModelChoices::default());
        let saved = update_at(&path, |choices| {
            assert!(choices.toggle_favorite("a"));
            choices.record_recent("b");
        })
        .unwrap();
        assert_eq!(load_from(&path).unwrap(), saved);
        update_at(&path, |choices| assert!(!choices.toggle_favorite("a"))).unwrap();
        assert!(load_from(&path).unwrap().favorites.is_empty());
        std::fs::write(&path, "recent = [").unwrap();
        assert!(load_from(&path).is_err());
    }
}
