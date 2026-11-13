//! Where Bcode keeps interactive user state, and the appearance saved there.
//!
//! Interactive presentation choices (the appearance picker's palette and profile) are stored in
//! a state file instead of rewriting declarative `bcode.toml` files. Every program that presents
//! Bcode's theme reads the same file through these functions, so a choice made in one is honored
//! by all of them.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{ConfigEnvironment, LayersError};

/// Environment variable naming Bcode's state directory.
pub const BCODE_STATE_DIR_ENV: &str = "BCODE_STATE_DIR";
/// Environment variable naming the interactive TUI state file.
pub const BCODE_TUI_STATE_ENV: &str = "BCODE_TUI_STATE";
/// File name of the interactive TUI state inside the state directory.
pub const TUI_STATE_FILE_NAME: &str = "tui.toml";

/// Bcode's state directory as the environment selects it.
///
/// Resolution precedence:
///
/// * `$BCODE_STATE_DIR` when set and not empty.
/// * `$XDG_STATE_HOME/bcode`.
/// * `$HOME/.local/state/bcode`.
/// * `bcode` in the system temporary directory.
#[must_use]
pub fn default_state_dir_with_environment(environment: &impl ConfigEnvironment) -> PathBuf {
    if let Some(path) = environment
        .var(BCODE_STATE_DIR_ENV)
        .filter(|value| !value.is_empty())
    {
        return PathBuf::from(path);
    }
    if let Some(state_home) = environment.var("XDG_STATE_HOME") {
        return PathBuf::from(state_home).join("bcode");
    }
    if let Some(home) = environment.var("HOME") {
        return PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("bcode");
    }
    std::env::temp_dir().join("bcode")
}

/// The interactive TUI state file for a state directory: `$BCODE_TUI_STATE` when set, else
/// [`TUI_STATE_FILE_NAME`] inside `state_dir`.
#[must_use]
pub fn tui_state_path_in(environment: &impl ConfigEnvironment, state_dir: &Path) -> PathBuf {
    environment
        .var(BCODE_TUI_STATE_ENV)
        .map_or_else(|| state_dir.join(TUI_STATE_FILE_NAME), PathBuf::from)
}

/// The interactive TUI state file in the state directory the environment selects.
#[must_use]
pub fn default_tui_state_path_with_environment(environment: &impl ConfigEnvironment) -> PathBuf {
    tui_state_path_in(
        environment,
        &default_state_dir_with_environment(environment),
    )
}

/// Theme choices saved in the interactive TUI state (`[theme]` in the state file).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedTuiAppearance {
    /// Saved palette or theme id, which replaces the configured `[tui.theme] name`.
    pub theme: Option<String>,
    /// Saved profile id, which replaces the configured `[tui.theme] profile`.
    pub profile: Option<String>,
}

#[derive(Deserialize)]
struct StateDocument {
    #[serde(default)]
    theme: ThemeSection,
}

#[derive(Default, Deserialize)]
struct ThemeSection {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    profile: Option<String>,
}

impl SavedTuiAppearance {
    /// Read the theme choices from state file text. Other sections are ignored.
    ///
    /// # Errors
    ///
    /// * [`LayersError::Composition`] when the text is not TOML or `[theme]` is malformed.
    pub fn parse(text: &str) -> Result<Self, LayersError> {
        let document =
            toml::from_str::<StateDocument>(text).map_err(|error| LayersError::Composition {
                message: format!("failed to parse TUI state: {error}"),
            })?;
        Ok(Self {
            theme: document.theme.name,
            profile: document.theme.profile,
        })
    }

    /// Read the theme choices from the state file at `path`; a missing file saves nothing.
    ///
    /// # Errors
    ///
    /// * [`LayersError::Io`] when the file exists but cannot be read.
    /// * [`LayersError::Composition`] when it cannot be parsed.
    pub fn load(path: &Path) -> Result<Self, LayersError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(|error| LayersError::Composition {
                message: format!("{}: {error}", path.display()),
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(LayersError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }
}

/// Replace the theme choices in the state file at `path`, keeping every other section and key
/// as it is. `None` removes a choice, so the configured value applies again. The file is
/// replaced atomically, so readers see either the previous or the new state.
///
/// # Errors
///
/// * [`LayersError::Io`] when the file cannot be read or written.
/// * [`LayersError::Composition`] when the existing file is not TOML or `[theme]` is not a
///   table.
pub fn save_tui_appearance(
    path: &Path,
    appearance: &SavedTuiAppearance,
) -> Result<(), LayersError> {
    let mut document = match std::fs::read_to_string(path) {
        Ok(text) => text
            .parse::<toml::Table>()
            .map_err(|error| LayersError::Composition {
                message: format!("{}: failed to parse TUI state: {error}", path.display()),
            })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(source) => {
            return Err(LayersError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let theme = document
        .entry("theme")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| LayersError::Composition {
            message: format!("{}: [theme] is not a table", path.display()),
        })?;
    for (key, value) in [
        ("name", &appearance.theme),
        ("profile", &appearance.profile),
    ] {
        match value {
            Some(value) => {
                theme.insert(key.to_owned(), toml::Value::String(value.clone()));
            }
            None => {
                theme.remove(key);
            }
        }
    }
    let contents = toml::to_string(&document).map_err(|error| LayersError::Composition {
        message: format!("failed to serialize TUI state: {error}"),
    })?;
    write_atomically(path, contents.as_bytes()).map_err(|source| LayersError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn write_atomically(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    sync_directory(parent)
}

/// Make the rename into `directory` durable where directories can be synced.
#[cfg(unix)]
fn sync_directory(directory: &Path) -> std::io::Result<()> {
    std::fs::File::open(directory)?.sync_all()
}

/// Make the rename into `directory` durable where directories can be synced.
#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
const fn sync_directory(_directory: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;

    use super::*;

    struct Environment(BTreeMap<&'static str, &'static str>);

    impl ConfigEnvironment for Environment {
        fn var(&self, name: &str) -> Option<String> {
            self.0.get(name).map(|value| (*value).to_owned())
        }

        fn var_os(&self, name: &str) -> Option<OsString> {
            self.var(name).map(OsString::from)
        }

        fn current_dir(&self) -> PathBuf {
            PathBuf::from("/")
        }
    }

    #[test]
    fn state_file_follows_the_state_directory_environment() {
        let home = Environment(BTreeMap::from([("HOME", "/home/user")]));
        assert_eq!(
            default_tui_state_path_with_environment(&home),
            PathBuf::from("/home/user/.local/state/bcode/tui.toml")
        );
        let xdg = Environment(BTreeMap::from([
            ("HOME", "/home/user"),
            ("XDG_STATE_HOME", "/xdg"),
        ]));
        assert_eq!(
            default_tui_state_path_with_environment(&xdg),
            PathBuf::from("/xdg/bcode/tui.toml")
        );
        let explicit_dir = Environment(BTreeMap::from([
            ("XDG_STATE_HOME", "/xdg"),
            (BCODE_STATE_DIR_ENV, "/state"),
        ]));
        assert_eq!(
            default_tui_state_path_with_environment(&explicit_dir),
            PathBuf::from("/state/tui.toml")
        );
        let explicit_file = Environment(BTreeMap::from([
            (BCODE_STATE_DIR_ENV, "/state"),
            (BCODE_TUI_STATE_ENV, "/elsewhere/ui.toml"),
        ]));
        assert_eq!(
            default_tui_state_path_with_environment(&explicit_file),
            PathBuf::from("/elsewhere/ui.toml")
        );
    }

    #[test]
    fn saved_appearance_reads_theme_and_profile_and_ignores_other_state() {
        let saved = SavedTuiAppearance::parse(
            r#"
            [theme]
            name = "nord"
            profile = "structured"

            [presentation.streaming]
            enabled = false
            "#,
        )
        .expect("state");
        assert_eq!(saved.theme.as_deref(), Some("nord"));
        assert_eq!(saved.profile.as_deref(), Some("structured"));
        assert_eq!(
            SavedTuiAppearance::parse("").expect("empty"),
            SavedTuiAppearance::default()
        );
        assert!(SavedTuiAppearance::parse("[theme]\nname = 3").is_err());
    }

    #[test]
    fn missing_state_file_saves_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            SavedTuiAppearance::load(&dir.path().join("tui.toml")).expect("missing"),
            SavedTuiAppearance::default()
        );
    }

    #[test]
    fn saving_the_appearance_replaces_only_the_theme_choices() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state").join("tui.toml");
        save_tui_appearance(
            &path,
            &SavedTuiAppearance {
                theme: Some("nord".to_owned()),
                profile: Some("bare".to_owned()),
            },
        )
        .expect("created");
        std::fs::write(
            &path,
            format!(
                "{}\n[presentation.streaming]\nenabled = false\n",
                std::fs::read_to_string(&path).expect("read")
            ),
        )
        .expect("append");
        save_tui_appearance(
            &path,
            &SavedTuiAppearance {
                theme: Some("dracula".to_owned()),
                profile: None,
            },
        )
        .expect("replaced");
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(
            SavedTuiAppearance::parse(&text).expect("state"),
            SavedTuiAppearance {
                theme: Some("dracula".to_owned()),
                profile: None,
            }
        );
        let document = text.parse::<toml::Table>().expect("toml");
        assert_eq!(
            document["presentation"]["streaming"]["enabled"],
            toml::Value::Boolean(false)
        );
        std::fs::write(&path, "theme = 3\n").expect("write");
        assert!(save_tui_appearance(&path, &SavedTuiAppearance::default()).is_err());
    }
}
