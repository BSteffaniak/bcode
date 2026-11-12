//! Configuration of the review app.
//!
//! Settings live in the ordinary bcode configuration files, so `bcode review` and the
//! standalone `bcode-review` binary read identical values:
//!
//! * `[plugins.config."bcode.code_review"]` — the review's plugin table: where review state is
//!   stored ([`CodeReviewConfig`]), when viewed files reset (`viewed_reset`), and presentation
//!   settings.
//! * `[tui.keybindings.scopes."code-review*"]` — keybinding overrides of the review scopes.
//! * `[tui.theme]` — theme selection shared with the rest of bcode.
//! * `[tui.diff_viewer]` — bcode's diff layout preference, which the review follows unless its
//!   own table sets a layout ([`view`]).
//!
//! Files are discovered and composed by `bcode_config_layers`, the same policy bcode uses.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bcode_code_review::CodeReviewConfig;
use bcode_config_layers::{ConfigEnvironment, ConfigLoadOverrides, ProcessConfigEnvironment};

pub mod view;

pub use view::{ConfiguredLayout, DiffLayout, ReviewViewConfig, resolve};

/// Plugin id whose configuration table holds the review settings.
pub const PLUGIN_ID: &str = "bcode.code_review";

/// Prefix of every review keymap scope.
pub const SCOPE_PREFIX: &str = "code-review";

/// Effective theme selection.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ThemeChoice {
    /// Theme id; empty selects the default.
    pub name: String,
    /// Overlays applied after the base theme.
    pub overlays: Vec<String>,
    /// Extra theme files or directories.
    pub paths: Vec<PathBuf>,
}

/// Terminal color depth policy (`[tui] color_depth`), shared with bcode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ColorDepthChoice {
    /// Follow the environment: no colors under `NO_COLOR` or a dumb terminal, else full color.
    #[default]
    Auto,
    /// 24-bit colors unchanged.
    Truecolor,
    /// Colors reduced to the 256-color palette.
    Ansi256,
    /// Colors reduced to the 16 ANSI colors.
    Ansi16,
    /// No colors; modifiers only.
    None,
}

impl ColorDepthChoice {
    /// The choice named `name`, as bcode spells it.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "auto" => Self::Auto,
            "truecolor" => Self::Truecolor,
            "ansi256" => Self::Ansi256,
            "ansi16" => Self::Ansi16,
            "none" => Self::None,
            _ => return None,
        })
    }

    /// The depth to present with, given the terminal's detected capabilities.
    ///
    /// `auto` honors only `NO_COLOR` and dumb terminals, as bcode does: color variables are
    /// often dropped by multiplexers and SSH on terminals that show full color, so reduced
    /// depths are explicit choices.
    #[must_use]
    pub const fn resolve(
        self,
        detected: bmux_tui::capabilities::TerminalColorDepth,
    ) -> bmux_tui::capabilities::TerminalColorDepth {
        use bmux_tui::capabilities::TerminalColorDepth;
        match self {
            Self::Auto => match detected {
                TerminalColorDepth::Monochrome => TerminalColorDepth::Monochrome,
                TerminalColorDepth::Ansi16
                | TerminalColorDepth::Ansi256
                | TerminalColorDepth::TrueColor => TerminalColorDepth::TrueColor,
            },
            Self::Truecolor => TerminalColorDepth::TrueColor,
            Self::Ansi256 => TerminalColorDepth::Ansi256,
            Self::Ansi16 => TerminalColorDepth::Ansi16,
            Self::None => TerminalColorDepth::Monochrome,
        }
    }
}

/// Everything the app reads from configuration files.
#[derive(Debug, Clone, Default)]
pub struct LoadedConfig {
    /// The review's plugin table (`[plugins.config."bcode.code_review"]`).
    pub plugin: toml::Table,
    /// Where review state is stored, from the plugin table.
    pub store: CodeReviewConfig,
    /// Effective `scope -> key -> action` overrides of the review scopes.
    pub keybindings: BTreeMap<String, BTreeMap<String, String>>,
    /// Presentation settings from the plugin table.
    pub view: ReviewViewConfig,
    /// bcode's diff layout preference (`[tui.diff_viewer]`), when configured.
    pub diff_layout: Option<DiffLayout>,
    /// Effective theme selection.
    pub theme: ThemeChoice,
    /// Terminal color depth from `[tui] color_depth`.
    pub color_depth: ColorDepthChoice,
    /// Whether `[plugins] disabled` lists the code review plugin.
    pub plugin_disabled: bool,
    /// Problems found while loading; defaults are used for affected sections.
    pub diagnostics: Vec<String>,
    /// bcode user configuration directory.
    pub config_dir: PathBuf,
    /// Files that were read, in precedence order.
    pub files: Vec<PathBuf>,
}

/// Load configuration for a starting directory from the process environment.
#[must_use]
pub fn load(start: &Path) -> LoadedConfig {
    load_in(start, &ProcessConfigEnvironment)
}

/// [`load`] reading environment variables (including `$XDG_CONFIG_HOME` and `$HOME`, which
/// locate the user files) from `environment` instead of the process.
#[must_use]
pub fn load_in(start: &Path, environment: &impl ConfigEnvironment) -> LoadedConfig {
    let config_dir = user_config_home(environment).join("bcode");
    match bcode_config_layers::load_layered_from_paths(
        &bcode_config_layers::default_config_paths_from_with_environment(start, environment),
        &ConfigLoadOverrides::from_config_environment_with_cli(environment, None, None),
        toml::Table::new(),
    ) {
        Ok(layered) => {
            let mut loaded = from_table(&layered.value, config_dir);
            loaded.files = layered.files;
            loaded
        }
        Err(error) => LoadedConfig {
            config_dir,
            diagnostics: vec![format!("bcode.toml configuration not loaded: {error}")],
            ..LoadedConfig::default()
        },
    }
}

/// Interpret an already-merged `bcode.toml` table.
#[must_use]
pub fn from_table(bcode: &toml::Table, config_dir: PathBuf) -> LoadedConfig {
    let mut loaded = LoadedConfig {
        config_dir,
        ..LoadedConfig::default()
    };
    loaded.plugin_disabled = string_list(lookup(bcode, &["plugins", "disabled"]))
        .iter()
        .any(|id| id == PLUGIN_ID);
    if let Some(value) = lookup(bcode, &["plugins", "config", PLUGIN_ID]) {
        match value.as_table() {
            Some(table) => {
                loaded.plugin = table.clone();
                loaded.view = ReviewViewConfig::from_plugin_table(table);
                if let Some(reset) = table.get("viewed_reset")
                    && reset
                        .clone()
                        .try_into::<bcode_code_review::viewed::ViewedReset>()
                        .is_err()
                {
                    loaded.diagnostics.push(format!(
                        "plugins.config.\"{PLUGIN_ID}\".viewed_reset must be \"diff\" or \"file\""
                    ));
                }
                match toml::Value::Table(table.clone()).try_into::<CodeReviewConfig>() {
                    Ok(store) => loaded.store = store,
                    Err(error) => loaded.diagnostics.push(format!(
                        "plugins.config.\"{PLUGIN_ID}\": {}",
                        first_line(&error.to_string())
                    )),
                }
            }
            None => loaded
                .diagnostics
                .push(format!("plugins.config.\"{PLUGIN_ID}\" must be a table")),
        }
    }
    if let Some(toml::Value::Table(scopes)) = lookup(bcode, &["tui", "keybindings", "scopes"]) {
        for (scope, bindings) in scopes {
            if !scope.starts_with(SCOPE_PREFIX) {
                continue;
            }
            let toml::Value::Table(bindings) = bindings else {
                loaded
                    .diagnostics
                    .push(format!("keybinding scope {scope} must be a table"));
                continue;
            };
            let resolved = loaded.keybindings.entry(scope.clone()).or_default();
            for (key, action) in bindings {
                match action.as_str() {
                    Some(action) => {
                        resolved.insert(key.clone(), action.to_owned());
                    }
                    None => loaded
                        .diagnostics
                        .push(format!("keybinding {scope}.{key} must be an action id")),
                }
            }
        }
    }
    if let Some(viewer) = lookup(bcode, &["tui", "diff_viewer"]) {
        loaded.diff_layout = diff_viewer_layout(viewer);
        if loaded.diff_layout.is_none() {
            loaded
                .diagnostics
                .push("tui.diff_viewer: unknown layout".to_owned());
        }
    }
    if let Some(depth) = lookup(bcode, &["tui", "color_depth"]) {
        match depth.as_str().and_then(ColorDepthChoice::parse) {
            Some(depth) => loaded.color_depth = depth,
            None => loaded.diagnostics.push(
                "tui.color_depth: expected auto, truecolor, ansi256, ansi16 or none".to_owned(),
            ),
        }
    }
    if let Some(toml::Value::Table(theme)) = lookup(bcode, &["tui", "theme"]) {
        if let Some(name) = theme.get("name").and_then(toml::Value::as_str) {
            name.clone_into(&mut loaded.theme.name);
        }
        loaded.theme.overlays = string_list(theme.get("overlays"));
        loaded.theme.paths = string_list(theme.get("paths"))
            .into_iter()
            .map(PathBuf::from)
            .collect();
    }
    loaded
}

/// The layout of a `[tui.diff_viewer]` table: `layout` (`auto`, `unified`, `side_by_side`) and
/// `side_by_side_breakpoint`.
fn diff_viewer_layout(viewer: &toml::Value) -> Option<DiffLayout> {
    let breakpoint = viewer
        .get("side_by_side_breakpoint")
        .and_then(toml::Value::as_integer)
        .and_then(|value| u16::try_from(value).ok())
        .unwrap_or(view::DEFAULT_BREAKPOINT);
    match viewer.get("layout").map(toml::Value::as_str) {
        None | Some(Some("auto")) => Some(DiffLayout::Auto { breakpoint }),
        Some(Some("unified")) => Some(DiffLayout::Unified),
        Some(Some("side_by_side")) => Some(DiffLayout::SideBySide),
        Some(_) => None,
    }
}

fn first_line(message: &str) -> &str {
    message.lines().next().unwrap_or_default().trim()
}

fn user_config_home(environment: &impl ConfigEnvironment) -> PathBuf {
    if let Some(home) = environment.var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(home);
    }
    if let Some(home) = environment.var_os("HOME") {
        return PathBuf::from(home).join(".config");
    }
    std::env::temp_dir()
}

fn lookup<'a>(table: &'a toml::Table, path: &[&str]) -> Option<&'a toml::Value> {
    let (first, rest) = path.split_first()?;
    rest.iter()
        .try_fold(table.get(*first)?, |value, key| value.get(*key))
}

fn string_list(value: Option<&toml::Value>) -> Vec<String> {
    value
        .and_then(toml::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_code_review::CodeReviewStateLocation;

    fn table(text: &str) -> toml::Table {
        toml::from_str(text).expect("valid TOML")
    }

    #[test]
    fn viewed_reset_reads_diff_or_file_and_reports_anything_else() {
        use bcode_code_review::viewed::ViewedReset;
        let reset = |text: &str| {
            let loaded = from_table(&table(text), PathBuf::from("/config"));
            (loaded.view.viewed_reset, loaded.diagnostics)
        };
        assert_eq!(reset(""), (ViewedReset::Diff, Vec::new()));
        assert_eq!(
            reset("[plugins.config.\"bcode.code_review\"]\nviewed_reset = \"file\""),
            (ViewedReset::File, Vec::new())
        );
        assert_eq!(
            reset("[plugins.config.\"bcode.code_review\"]\nviewed_reset = \"diff\""),
            (ViewedReset::Diff, Vec::new())
        );
        let (fallback, diagnostics) =
            reset("[plugins.config.\"bcode.code_review\"]\nviewed_reset = \"lines\"");
        assert_eq!(fallback, ViewedReset::Diff);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    }

    #[test]
    fn reads_the_plugin_table_review_keybindings_and_theme() {
        let loaded = from_table(
            &table(
                r#"
                [plugins]
                disabled = ["bcode.code_review"]

                [plugins.config."bcode.code_review"]
                state_location = "repo"

                [plugins.config."bcode.code_review".review_v2]
                icons = "ascii"

                [tui.keybindings.scopes.code-review]
                x = "bcode.code_review.down"

                [tui.keybindings.scopes."code-review-home"]
                q = "none"

                [tui.keybindings.scopes."git.global"]
                q = "bcode.git.quit"

                [tui.theme]
                name = "nord"
                overlays = ["high-contrast"]

                [tui.diff_viewer]
                layout = "unified"
                "#,
            ),
            PathBuf::from("/config"),
        );
        assert!(loaded.plugin_disabled);
        assert_eq!(loaded.store.state_location, CodeReviewStateLocation::Repo);
        assert!(loaded.plugin.contains_key("review_v2"));
        assert_eq!(loaded.view.icons, Some(crate::theme::IconStyle::Ascii));
        assert_eq!(loaded.diff_layout, Some(DiffLayout::Unified));
        assert_eq!(
            loaded
                .keybindings
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["code-review", "code-review-home"]
        );
        assert_eq!(
            loaded.keybindings["code-review"]["x"],
            "bcode.code_review.down"
        );
        assert_eq!(loaded.theme.name, "nord");
        assert_eq!(loaded.theme.overlays, ["high-contrast"]);
        assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
    }

    #[test]
    fn color_depth_follows_bcode_and_honors_no_color_on_auto() {
        use bmux_tui::capabilities::TerminalColorDepth;
        let depth = |source: &str| from_table(&table(source), PathBuf::from("/config"));
        let loaded = depth("[tui]\ncolor_depth = \"ansi16\"\n");
        assert_eq!(loaded.color_depth, ColorDepthChoice::Ansi16);
        assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
        let loaded = depth("[tui]\ncolor_depth = \"sepia\"\n");
        assert_eq!(loaded.color_depth, ColorDepthChoice::Auto);
        assert!(
            loaded
                .diagnostics
                .iter()
                .any(|message| message.contains("tui.color_depth"))
        );
        // `auto` drops colors only when the environment asks for none.
        assert_eq!(
            ColorDepthChoice::Auto.resolve(TerminalColorDepth::Monochrome),
            TerminalColorDepth::Monochrome
        );
        assert_eq!(
            ColorDepthChoice::Auto.resolve(TerminalColorDepth::Ansi16),
            TerminalColorDepth::TrueColor
        );
        assert_eq!(
            ColorDepthChoice::None.resolve(TerminalColorDepth::TrueColor),
            TerminalColorDepth::Monochrome
        );
    }

    #[test]
    fn invalid_sections_report_diagnostics_and_keep_defaults() {
        let loaded = from_table(
            &table(
                r#"
                [plugins.config."bcode.code_review"]
                state_location = "elsewhere"

                [tui.keybindings.scopes]
                code-review = 3
                "#,
            ),
            PathBuf::from("/config"),
        );
        assert_eq!(loaded.store, CodeReviewConfig::default());
        assert_eq!(loaded.diagnostics.len(), 2, "{:?}", loaded.diagnostics);
    }

    #[test]
    fn layered_files_compose_from_the_starting_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(home.join(".config/bcode")).expect("config dir");
        std::fs::create_dir_all(repo.join(".git")).expect("repo");
        std::fs::write(
            home.join(".config/bcode/bcode.toml"),
            "[tui.theme]\nname = \"nord\"\n",
        )
        .expect("user config");
        std::fs::write(repo.join("bcode.toml"), "[tui.theme]\nname = \"dracula\"\n")
            .expect("repo config");
        let environment = TestEnvironment(BTreeMap::from([(
            "HOME".to_owned(),
            home.to_string_lossy().into_owned(),
        )]));
        let loaded = load_in(&repo, &environment);
        assert_eq!(loaded.theme.name, "dracula");
        assert_eq!(loaded.config_dir, home.join(".config/bcode"));
        assert_eq!(loaded.files.len(), 2, "{:?}", loaded.files);
    }

    struct TestEnvironment(BTreeMap<String, String>);

    impl ConfigEnvironment for TestEnvironment {
        fn var(&self, name: &str) -> Option<String> {
            self.0.get(name).cloned()
        }

        fn var_os(&self, name: &str) -> Option<std::ffi::OsString> {
            self.0.get(name).map(Into::into)
        }

        fn current_dir(&self) -> PathBuf {
            PathBuf::from("/")
        }
    }
}
