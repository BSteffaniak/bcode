//! Git UI configuration and its loader.
//!
//! Settings live in the ordinary bcode configuration files so `bcode git` and the standalone
//! `bcode-git` binary read identical values:
//!
//! * `[plugins.config."bcode.git".ui]` — [`GitUiConfig`]
//! * `[tui.keybindings.scopes."git.*"]` — keybinding overrides
//! * `[tui.theme]` — theme selection shared with the rest of bcode
//!
//! Files are discovered and composed by `bcode_config_layers`, the same policy bcode uses.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bcode_config_layers::ConfigLoadOverrides;
use serde::Deserialize;

/// Behavior and presentation settings.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
// Each flag is an independent user-facing toggle in the configuration file.
#[allow(clippy::struct_excessive_bools)]
pub struct GitUiConfig {
    /// Whether `bcode git` is available. The standalone binary ignores this.
    pub enabled: bool,
    /// Lines of diff context.
    pub diff_context: u32,
    /// Window arrangement and sizing.
    pub layout: LayoutConfig,
    /// Files panel settings.
    pub files: FilesConfig,
    /// Maximum commits loaded per page.
    pub log_page_size: usize,
    /// Show the commit graph.
    pub show_graph: bool,
    /// Syntax highlighting settings.
    pub highlight: HighlightConfig,
    /// Refresh settings.
    pub refresh: RefreshConfig,
    /// Confirmation settings.
    pub confirm: ConfirmConfig,
    /// Editor command; defaults to `$GIT_EDITOR`, `$VISUAL`, `$EDITOR`, then `vi`.
    pub editor: Option<String>,
    /// Relative ("3h ago") or absolute dates.
    pub relative_dates: bool,
    /// User-defined commands.
    pub custom_commands: Vec<CustomCommand>,
}

impl Default for GitUiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            diff_context: 3,
            layout: LayoutConfig::default(),
            files: FilesConfig::default(),
            log_page_size: 300,
            show_graph: true,
            highlight: HighlightConfig::default(),
            refresh: RefreshConfig::default(),
            confirm: ConfirmConfig::default(),
            editor: None,
            relative_dates: true,
            custom_commands: Vec::new(),
        }
    }
}

/// How much of the screen the focused window takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScreenMode {
    /// Every side window beside the main view.
    #[default]
    Normal,
    /// Only the focused side window beside the main view.
    Half,
    /// The focused window fills the screen.
    Full,
}

/// How side windows other than the focused one appear in half screen mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CollapsedWindows {
    /// One-line title bars that can be clicked.
    #[default]
    Titles,
    /// Not shown.
    Hidden,
}

/// How the staged and unstaged sections of the main view are arranged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MainSplit {
    /// Side by side on wide terminals, stacked otherwise.
    #[default]
    Flexible,
    /// One above the other.
    Stacked,
    /// Next to each other.
    SideBySide,
}

/// When side windows are stacked above the main view instead of beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Portrait {
    /// Below `portrait_below_width` columns.
    #[default]
    Auto,
    /// Always.
    Always,
    /// Never.
    Never,
}

/// Window arrangement and sizing (`[plugins.config."bcode.git".ui.layout]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LayoutConfig {
    /// Screen mode at startup; `+` and `_` change it for the session.
    pub screen_mode: ScreenMode,
    /// Percentage of the width used by the side windows in normal mode (10–90).
    pub side_panel_width: u16,
    /// Percentage of the width used by the side window in half mode (10–90).
    pub half_side_panel_width: u16,
    /// How other side windows appear in half mode.
    pub collapsed_windows: CollapsedWindows,
    /// View focused at startup: a view name (`files`, `branches`, `commits`, …) or `main`.
    pub initial_window: String,
    /// Side windows from top to bottom, each an ordered list of tabs. Views left out are
    /// hidden. Names: `status`, `files`, `worktrees`, `submodules`, `branches`, `remotes`,
    /// `tags`, `commits`, `reflog`, `stash`.
    pub windows: Vec<Vec<String>>,
    /// Give the focused side window more height.
    pub expand_focused: bool,
    /// Height weight of the focused side window relative to the others.
    pub expanded_weight: u16,
    /// Arrangement of the staged and unstaged sections.
    pub main_split: MainSplit,
    /// Terminal width from which the flexible split puts sections side by side.
    pub main_split_min_width: u16,
    /// When to stack side windows above the main view.
    pub portrait: Portrait,
    /// Terminal width below which the automatic portrait layout applies.
    pub portrait_below_width: u16,
    /// Command log settings.
    pub command_log: CommandLogConfig,
    /// Show the bottom key-hint bar. When hidden, it still appears for progress and messages.
    pub status_bar: bool,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            screen_mode: ScreenMode::Normal,
            side_panel_width: 33,
            half_side_panel_width: 50,
            collapsed_windows: CollapsedWindows::Titles,
            initial_window: "files".to_owned(),
            windows: [
                &["status"][..],
                &["files", "worktrees", "submodules"],
                &["branches", "remotes", "tags"],
                &["commits", "reflog"],
                &["stash"],
            ]
            .iter()
            .map(|tabs| tabs.iter().map(|tab| (*tab).to_owned()).collect())
            .collect(),
            expand_focused: true,
            expanded_weight: 3,
            main_split: MainSplit::Flexible,
            main_split_min_width: 180,
            portrait: Portrait::Auto,
            portrait_below_width: 84,
            command_log: CommandLogConfig::default(),
            status_bar: true,
        }
    }
}

/// Command log settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CommandLogConfig {
    /// Show the command log at startup; `@` toggles it.
    pub show: bool,
    /// Height in rows, including the border.
    pub height: u16,
}

impl Default for CommandLogConfig {
    fn default() -> Self {
        Self {
            show: false,
            height: 8,
        }
    }
}

/// Files panel settings (`[plugins.config."bcode.git".ui.files]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilesConfig {
    /// Show files as a directory tree rather than a flat list; `` ` `` toggles it.
    pub tree: bool,
}

impl Default for FilesConfig {
    fn default() -> Self {
        Self { tree: true }
    }
}

/// Syntax highlighting settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HighlightConfig {
    /// Enable language-aware highlighting.
    pub enabled: bool,
    /// Highlight whole files up to this size; larger files highlight per hunk.
    pub max_file_bytes: u64,
}

impl Default for HighlightConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_file_bytes: 1024 * 1024,
        }
    }
}

/// Refresh settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RefreshConfig {
    /// Watch the worktree and git directory for changes.
    pub watch: bool,
    /// Poll interval in seconds when watching is unavailable; `0` disables polling.
    pub poll_seconds: u64,
    /// Background fetch interval in seconds; `0` disables automatic fetching.
    pub auto_fetch_seconds: u64,
}

impl Default for RefreshConfig {
    fn default() -> Self {
        Self {
            watch: true,
            poll_seconds: 10,
            auto_fetch_seconds: 0,
        }
    }
}

/// Which destructive actions ask first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
// Each flag independently controls one confirmation prompt.
#[allow(clippy::struct_excessive_bools)]
pub struct ConfirmConfig {
    /// Discarding changes.
    pub discard: bool,
    /// Force pushes.
    pub force_push: bool,
    /// Hard resets.
    pub hard_reset: bool,
    /// Dropping commits or stashes and deleting branches.
    pub delete: bool,
}

impl Default for ConfirmConfig {
    fn default() -> Self {
        Self {
            discard: true,
            force_push: true,
            hard_reset: true,
            delete: true,
        }
    }
}

/// A user-defined command bound to a key in a scope.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomCommand {
    /// Default key (any bcode key spelling, including sequences such as `g c`).
    pub key: String,
    /// Scope name such as `git.files`, `git.commits` or `git.global`.
    #[serde(default = "default_custom_scope")]
    pub scope: String,
    /// Shell command template. Placeholders: `{file}`, `{branch}`, `{commit}`, `{remote}`,
    /// `{tag}`, `{stash}`, `{current_branch}`, `{repo}` and `{prompt.<name>}`.
    pub command: String,
    /// Help text.
    #[serde(default)]
    pub description: Option<String>,
    /// Values collected before running.
    #[serde(default)]
    pub prompts: Vec<CustomPrompt>,
    /// Ask for confirmation first.
    #[serde(default)]
    pub confirm: bool,
    /// Suspend the UI and attach the command to the terminal.
    #[serde(default)]
    pub interactive: bool,
    /// Show command output in a popup when it finishes.
    #[serde(default)]
    pub show_output: bool,
}

/// A prompt shown before a custom command runs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomPrompt {
    /// Placeholder name (`{prompt.<name>}`).
    pub name: String,
    /// Prompt title.
    pub title: String,
    /// Initial value.
    #[serde(default)]
    pub initial: String,
}

fn default_custom_scope() -> String {
    "git.global".to_owned()
}

/// Theme selection from `[tui.theme]`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ThemeChoice {
    /// Theme id; empty selects the default.
    pub name: String,
    /// Overlays applied after the base theme.
    pub overlays: Vec<String>,
    /// Extra theme files or directories.
    pub paths: Vec<PathBuf>,
}

/// Everything the UI reads from configuration files.
#[derive(Debug, Clone, Default)]
pub struct LoadedConfig {
    /// UI settings.
    pub ui: GitUiConfig,
    /// `scope -> key -> action` overrides for `git.*` scopes.
    pub keybindings: BTreeMap<String, BTreeMap<String, String>>,
    /// Theme selection.
    pub theme: ThemeChoice,
    /// Problems found while loading; defaults are used for affected sections.
    pub diagnostics: Vec<String>,
    /// bcode user configuration directory.
    pub config_dir: PathBuf,
    /// Files that were read, in precedence order.
    pub files: Vec<PathBuf>,
    /// Whether `[plugins] disabled` lists the `bcode.git` plugin.
    pub plugin_disabled: bool,
}

impl LoadedConfig {
    /// Whether `bcode git` may run: the plugin is not disabled and `ui.enabled` is set.
    #[must_use]
    pub const fn ui_enabled(&self) -> bool {
        self.ui.enabled && !self.plugin_disabled
    }
}

/// bcode user configuration directory (`$XDG_CONFIG_HOME/bcode` or `~/.config/bcode`).
#[must_use]
pub fn default_config_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(home).join("bcode");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".config").join("bcode");
    }
    std::env::temp_dir().join("bcode")
}

/// Candidate configuration files for a starting directory in precedence order (later wins),
/// whether or not they exist. Matches bcode's own discovery, including `$BCODE_CONFIG`.
#[must_use]
pub fn config_files(start: &Path) -> Vec<PathBuf> {
    let mut files = bcode_config_layers::default_config_paths_from(start);
    if let Some(path) = ConfigLoadOverrides::from_environment().env_config_path {
        files.push(path);
    }
    files
}

/// Load configuration exactly as bcode composes it: discovered files, `$BCODE_CONFIG`,
/// `$BCODE_CONFIG_TOML`, and `[composition]` profiles.
#[must_use]
pub fn load(start: &Path) -> LoadedConfig {
    let mut loaded = LoadedConfig {
        config_dir: default_config_dir(),
        ..LoadedConfig::default()
    };
    match bcode_config_layers::load_layered(
        start,
        &ConfigLoadOverrides::from_environment(),
        toml::Table::new(),
    ) {
        Ok(layered) => {
            loaded.files = layered.files;
            apply(&layered.value, &mut loaded);
        }
        Err(error) => loaded
            .diagnostics
            .push(format!("configuration not loaded, using defaults: {error}")),
    }
    loaded
}

/// Interpret an already-merged configuration table.
#[must_use]
pub fn from_table(table: &toml::Table) -> LoadedConfig {
    let mut loaded = LoadedConfig {
        config_dir: default_config_dir(),
        ..LoadedConfig::default()
    };
    apply(table, &mut loaded);
    loaded
}

fn apply(table: &toml::Table, loaded: &mut LoadedConfig) {
    loaded.plugin_disabled = string_list(lookup(table, &["plugins", "disabled"]))
        .iter()
        .any(|id| id == "bcode.git");
    if let Some(ui) = lookup(table, &["plugins", "config", "bcode.git", "ui"]) {
        match ui.clone().try_into::<GitUiConfig>() {
            Ok(config) => loaded.ui = config,
            Err(error) => loaded
                .diagnostics
                .push(format!("[plugins.config.\"bcode.git\".ui]: {error}")),
        }
    }
    if let Some(toml::Value::Table(scopes)) = lookup(table, &["tui", "keybindings", "scopes"]) {
        for (scope, bindings) in scopes {
            if !scope.starts_with("git.") {
                continue;
            }
            let toml::Value::Table(bindings) = bindings else {
                loaded
                    .diagnostics
                    .push(format!("keybinding scope {scope} must be a table"));
                continue;
            };
            let mut resolved = BTreeMap::new();
            for (key, action) in bindings {
                match action.as_str() {
                    Some(action) => {
                        let action = if matches!(action, "none" | "unbind") {
                            ""
                        } else {
                            action
                        };
                        resolved.insert(key.clone(), action.to_owned());
                    }
                    None => loaded
                        .diagnostics
                        .push(format!("keybinding {scope}.{key} must be an action id")),
                }
            }
            loaded.keybindings.insert(scope.clone(), resolved);
        }
    }
    if let Some(toml::Value::Table(theme)) = lookup(table, &["tui", "theme"]) {
        if let Some(name) = theme.get("name").and_then(toml::Value::as_str) {
            name.clone_into(&mut loaded.theme.name);
        }
        loaded.theme.overlays = string_list(theme.get("overlays"));
        loaded.theme.paths = string_list(theme.get("paths"))
            .into_iter()
            .map(PathBuf::from)
            .collect();
    }
}

fn string_list(value: Option<&toml::Value>) -> Vec<String> {
    value
        .and_then(toml::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn lookup<'a>(table: &'a toml::Table, path: &[&str]) -> Option<&'a toml::Value> {
    let (first, rest) = path.split_first()?;
    let mut value = table.get(*first)?;
    for key in rest {
        value = value.as_table()?.get(*key)?;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ui_keybindings_and_theme_sections() {
        let table: toml::Table = r#"
            [plugins.config."bcode.git".ui]
            diff_context = 5
            show_graph = false
            [[plugins.config."bcode.git".ui.custom_commands]]
            key = "ctrl+b"
            scope = "git.branches"
            command = "gh pr create --head {branch}"

            [tui.keybindings.scopes."git.files"]
            "x" = "bcode.git.files.commit"
            "d" = "none"
            [tui.keybindings.scopes."chat"]
            "x" = "ignored"

            [tui.theme]
            name = "nord"
            overlays = ["high-contrast"]
        "#
        .parse()
        .unwrap();
        let loaded = from_table(&table);
        assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
        assert_eq!(loaded.ui.diff_context, 5);
        assert!(!loaded.ui.show_graph);
        assert_eq!(loaded.ui.custom_commands[0].scope, "git.branches");
        assert_eq!(loaded.keybindings.len(), 1);
        assert_eq!(loaded.keybindings["git.files"]["d"], "");
        assert_eq!(loaded.theme.name, "nord");
        assert_eq!(loaded.theme.overlays, ["high-contrast"]);
        assert!(loaded.ui_enabled());
    }

    #[test]
    fn loads_project_files_and_composition_profiles_like_bcode() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join(".bcode")).unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(
            root.join("bcode.toml"),
            r#"
            [plugins.config."bcode.git".ui]
            diff_context = 7
            [composition]
            active_profile = "laptop"
            [composition.profiles.laptop.patch.plugins.config."bcode.git".ui.layout]
            screen_mode = "half"
            "#,
        )
        .unwrap();
        std::fs::write(
            root.join(".bcode").join("bcode.toml"),
            "[plugins.config.\"bcode.git\".ui.files]\ntree = false\n",
        )
        .unwrap();
        // Discovery walks up from a subdirectory to the repository root.
        let loaded = load(&root.join("sub"));
        assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
        assert_eq!(loaded.ui.diff_context, 7);
        assert_eq!(loaded.ui.layout.screen_mode, ScreenMode::Half);
        assert!(!loaded.ui.files.tree);
        assert!(
            loaded
                .files
                .contains(&root.join(".bcode").join("bcode.toml"))
        );
    }

    #[test]
    fn plugin_disable_and_ui_toggle_turn_the_command_off() {
        let disabled: toml::Table = "[plugins]\ndisabled = [\"bcode.git\"]".parse().unwrap();
        assert!(!from_table(&disabled).ui_enabled());
        let toggled: toml::Table = "[plugins.config.\"bcode.git\".ui]\nenabled = false"
            .parse()
            .unwrap();
        assert!(!from_table(&toggled).ui_enabled());
    }

    #[test]
    fn invalid_sections_are_reported_and_defaulted() {
        let table: toml::Table = r#"
            [plugins.config."bcode.git".ui]
            unknown = 1
        "#
        .parse()
        .unwrap();
        let loaded = from_table(&table);
        assert_eq!(loaded.ui, GitUiConfig::default());
        assert_eq!(loaded.diagnostics.len(), 1);
    }
}
