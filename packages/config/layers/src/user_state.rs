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
    #[serde(default)]
    apps: std::collections::BTreeMap<String, AppSection>,
}

#[derive(Default, Deserialize)]
struct AppSection {
    #[serde(default)]
    theme: Option<ThemeSection>,
}

#[derive(Default, Deserialize)]
struct ThemeSection {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    profile: Option<String>,
}

impl From<ThemeSection> for SavedTuiAppearance {
    fn from(section: ThemeSection) -> Self {
        Self {
            theme: section.name,
            profile: section.profile,
        }
    }
}

fn parse_document(text: &str) -> Result<StateDocument, LayersError> {
    toml::from_str::<StateDocument>(text).map_err(|error| LayersError::Composition {
        message: format!("failed to parse TUI state: {error}"),
    })
}

fn read_state(path: &Path) -> Result<Option<String>, LayersError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(LayersError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn located(path: &Path, error: &LayersError) -> LayersError {
    LayersError::Composition {
        message: format!("{}: {error}", path.display()),
    }
}

impl SavedTuiAppearance {
    /// Read the theme choices from state file text. Other sections are ignored.
    ///
    /// # Errors
    ///
    /// * [`LayersError::Composition`] when the text is not TOML or `[theme]` is malformed.
    pub fn parse(text: &str) -> Result<Self, LayersError> {
        Ok(parse_document(text)?.theme.into())
    }

    /// Read the theme choices from the state file at `path`; a missing file saves nothing.
    ///
    /// # Errors
    ///
    /// * [`LayersError::Io`] when the file exists but cannot be read.
    /// * [`LayersError::Composition`] when it cannot be parsed.
    pub fn load(path: &Path) -> Result<Self, LayersError> {
        read_state(path)?.map_or_else(
            || Ok(Self::default()),
            |text| Self::parse(&text).map_err(|error| located(path, &error)),
        )
    }
}

/// The appearance choices one app reads from the state file: Bcode's own choice and the app's
/// own choice (`[apps.<app>.theme]`), which replaces Bcode's for that app while present.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedAppAppearance {
    /// Bcode's own choice (`[theme]`).
    pub bcode: SavedTuiAppearance,
    /// The app's own choice, or `None` when the app follows Bcode's.
    pub app: Option<SavedTuiAppearance>,
}

impl SavedAppAppearance {
    /// The choice the app presents: its own when it has one, else Bcode's.
    #[must_use]
    pub fn effective(&self) -> &SavedTuiAppearance {
        self.app.as_ref().unwrap_or(&self.bcode)
    }

    /// Read the choices of `app` from state file text. Other sections are ignored.
    ///
    /// # Errors
    ///
    /// * [`LayersError::Composition`] when the text is not TOML or a theme section is
    ///   malformed.
    pub fn parse(text: &str, app: &str) -> Result<Self, LayersError> {
        let mut document = parse_document(text)?;
        Ok(Self {
            bcode: document.theme.into(),
            app: document
                .apps
                .remove(app)
                .and_then(|section| section.theme)
                .map(Into::into),
        })
    }

    /// Read the choices of `app` from the state file at `path`; a missing file saves nothing.
    ///
    /// # Errors
    ///
    /// * [`LayersError::Io`] when the file exists but cannot be read.
    /// * [`LayersError::Composition`] when it cannot be parsed.
    pub fn load(path: &Path, app: &str) -> Result<Self, LayersError> {
        read_state(path)?.map_or_else(
            || Ok(Self::default()),
            |text| Self::parse(&text, app).map_err(|error| located(path, &error)),
        )
    }
}

/// Replace Bcode's theme choices (`[theme]`) in the state file at `path`.
///
/// Every other section and key stays as it is. `None` removes a choice, so the configured value
/// applies again. The file is replaced atomically, so readers see either the previous or the new
/// state.
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
    edit_state(path, |document| {
        let theme =
            table_entry(document, "theme").ok_or_else(|| "[theme] is not a table".to_owned())?;
        write_choices(theme, appearance);
        Ok(())
    })
}

/// Replace the app's own theme choice (`[apps.<app>.theme]`) in the state file at `path`.
///
/// Every other section and key stays as it is. `None` removes it, so the app follows Bcode's
/// choice again. The file is replaced atomically.
///
/// # Errors
///
/// * [`LayersError::Io`] when the file cannot be read or written.
/// * [`LayersError::Composition`] when `app` is empty, the existing file is not TOML or a
///   section on the way is not a table.
pub fn save_app_tui_appearance(
    path: &Path,
    app: &str,
    appearance: Option<&SavedTuiAppearance>,
) -> Result<(), LayersError> {
    if app.trim().is_empty() {
        return Err(LayersError::Composition {
            message: "app name must not be empty".to_owned(),
        });
    }
    edit_state(path, |document| {
        let not_table = || format!("[apps.{app}] is not a table");
        let apps =
            table_entry(document, "apps").ok_or_else(|| "[apps] is not a table".to_owned())?;
        match appearance {
            Some(appearance) => {
                let section = table_entry(apps, app).ok_or_else(not_table)?;
                let theme = table_entry(section, "theme")
                    .ok_or_else(|| format!("[apps.{app}.theme] is not a table"))?;
                theme.clear();
                write_choices(theme, appearance);
            }
            None => {
                if let Some(section) = apps.get_mut(app) {
                    let section = section.as_table_mut().ok_or_else(not_table)?;
                    section.remove("theme");
                    if section.is_empty() {
                        apps.remove(app);
                    }
                }
            }
        }
        if apps.is_empty() {
            document.remove("apps");
        }
        Ok(())
    })
}

/// The table at `key` of `table`, created when absent; `None` when `key` holds another value.
fn table_entry<'a>(table: &'a mut toml::Table, key: &str) -> Option<&'a mut toml::Table> {
    table
        .entry(key)
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
}

fn write_choices(theme: &mut toml::Table, appearance: &SavedTuiAppearance) {
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
}

/// Read the state file at `path` as a TOML document (empty when missing), change it with
/// `edit` and replace the file atomically.
fn edit_state(
    path: &Path,
    edit: impl FnOnce(&mut toml::Table) -> Result<(), String>,
) -> Result<(), LayersError> {
    let mut document = match read_state(path)? {
        Some(text) => text
            .parse::<toml::Table>()
            .map_err(|error| LayersError::Composition {
                message: format!("{}: failed to parse TUI state: {error}", path.display()),
            })?,
        None => toml::Table::new(),
    };
    edit(&mut document).map_err(|message| LayersError::Composition {
        message: format!("{}: {message}", path.display()),
    })?;
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

    #[test]
    fn an_app_keeps_its_own_choice_apart_from_bcode_and_other_apps() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tui.toml");
        // A file written before apps had their own choice reads as following Bcode.
        std::fs::write(&path, "[theme]\nname = \"nord\"\n").expect("write");
        let following = SavedAppAppearance::load(&path, "review").expect("state");
        assert_eq!(following.app, None);
        assert_eq!(following.effective().theme.as_deref(), Some("nord"));

        let own = SavedTuiAppearance {
            theme: Some("dracula".to_owned()),
            profile: Some("bare".to_owned()),
        };
        save_app_tui_appearance(&path, "review", Some(&own)).expect("saved");
        let review = SavedAppAppearance::load(&path, "review").expect("state");
        assert_eq!(review.app.as_ref(), Some(&own));
        assert_eq!(review.effective(), &own);
        assert_eq!(review.bcode.theme.as_deref(), Some("nord"));
        assert_eq!(
            SavedAppAppearance::load(&path, "git").expect("state").app,
            None
        );

        // Bcode's own writer leaves the app's choice alone.
        save_tui_appearance(
            &path,
            &SavedTuiAppearance {
                theme: Some("catppuccin-mocha".to_owned()),
                profile: None,
            },
        )
        .expect("saved");
        let review = SavedAppAppearance::load(&path, "review").expect("state");
        assert_eq!(review.app.as_ref(), Some(&own));
        assert_eq!(review.bcode.theme.as_deref(), Some("catppuccin-mocha"));
        assert_eq!(
            SavedTuiAppearance::load(&path)
                .expect("state")
                .theme
                .as_deref(),
            Some("catppuccin-mocha")
        );

        // Clearing the app's choice returns it to following Bcode and leaves no empty tables.
        save_app_tui_appearance(&path, "review", None).expect("cleared");
        let review = SavedAppAppearance::load(&path, "review").expect("state");
        assert_eq!(review.app, None);
        assert_eq!(
            review.effective().theme.as_deref(),
            Some("catppuccin-mocha")
        );
        assert!(
            !std::fs::read_to_string(&path)
                .expect("read")
                .contains("apps")
        );
        assert!(save_app_tui_appearance(&path, " ", None).is_err());
    }
}
