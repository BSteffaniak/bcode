//! Which discovered external credential connections the user has already been told about.
//!
//! Runtime state, not configuration: a connection is announced once, then left to `/model` and
//! `bcode auth connect`. Holds connection binding digests only, never credential material.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ConfigError;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NoticeState {
    #[serde(default)]
    announced: BTreeSet<String>,
}

/// Default location of the announced-connections state.
#[must_use]
pub fn default_path() -> PathBuf {
    crate::default_state_dir().join("external-connection-notices.toml")
}

/// Load announced binding digests; a missing file announces nothing yet.
///
/// # Errors
/// Returns an error when the file exists but cannot be read or parsed.
pub fn load_from(path: &Path) -> Result<BTreeSet<String>, ConfigError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    toml::from_str::<NoticeState>(&raw)
        .map(|state| state.announced)
        .map_err(|_| ConfigError::Composition {
            message: format!(
                "invalid external connection notice state: {}",
                path.display()
            ),
        })
}

/// Record binding digests as announced, keeping earlier ones.
///
/// # Errors
/// Returns an error when the state cannot be read, created, or written.
pub fn record_at(
    path: &Path,
    bindings: impl IntoIterator<Item = String>,
) -> Result<(), ConfigError> {
    let mut state = NoticeState {
        announced: load_from(path)?,
    };
    state.announced.extend(bindings);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let raw = toml::to_string(&state).map_err(|_| ConfigError::Composition {
        message: "external connection notice state could not be encoded".into(),
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
    fn announced_bindings_accumulate_and_missing_state_is_empty() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/notices.toml");
        assert!(load_from(&path).unwrap().is_empty());
        record_at(&path, ["a".to_owned()]).unwrap();
        record_at(&path, ["b".to_owned()]).unwrap();
        assert_eq!(
            load_from(&path).unwrap(),
            BTreeSet::from(["a".to_owned(), "b".to_owned()])
        );
        std::fs::write(&path, "not = [valid").unwrap();
        assert!(load_from(&path).is_err());
    }
}
