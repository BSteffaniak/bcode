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
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use bcode_config_layers::{ConfigEnvironment, ConfigLoadOverrides, ProcessConfigEnvironment};
use bcode_git::models::CommitMessageSource;
use serde::{Deserialize, Serialize};

/// Behavior and presentation settings.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
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
    /// Commit editor settings.
    pub commit: CommitConfig,
    /// Diff presentation in the main view.
    pub diff: DiffConfig,
    /// Staging view settings.
    pub staging: StagingConfig,
    /// Branch colors and merged-branch dimming.
    pub branches: crate::branches::BranchesConfig,
    /// Pull request state from the repository's forge.
    pub pull_requests: PullRequestsConfig,
    /// Maximum commits loaded per page.
    pub log_page_size: usize,
    /// Show the commit graph.
    pub show_graph: bool,
    /// Syntax highlighting settings.
    pub highlight: HighlightConfig,
    /// Refresh settings.
    pub refresh: RefreshConfig,
    /// How running work is shown.
    pub activity: ActivityConfig,
    /// Confirmation settings.
    pub confirm: ConfirmConfig,
    /// Editor command; defaults to `$GIT_EDITOR`, `$VISUAL`, `$EDITOR`, then `vi`.
    pub editor: Option<String>,
    /// Relative ("3h ago") or absolute dates.
    pub relative_dates: bool,
    /// User-defined commands.
    pub custom_commands: Vec<CustomCommand>,
    /// Keybinding overrides, `scope -> key -> action id` (`""`, `"none"` or `"unbind"`
    /// removes a binding). Applied over `[tui.keybindings.scopes."git.*"]` in `bcode.toml`.
    pub keybindings: BTreeMap<String, BTreeMap<String, String>>,
    /// Theme; unset fields fall back to `[tui.theme]` in `bcode.toml`.
    pub theme: ThemeSettings,
}

impl Default for GitUiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            diff_context: 3,
            layout: LayoutConfig::default(),
            files: FilesConfig::default(),
            commit: CommitConfig::default(),
            diff: DiffConfig::default(),
            staging: StagingConfig::default(),
            branches: crate::branches::BranchesConfig::default(),
            pull_requests: PullRequestsConfig::default(),
            log_page_size: 300,
            show_graph: true,
            highlight: HighlightConfig::default(),
            refresh: RefreshConfig::default(),
            activity: ActivityConfig::default(),
            confirm: ConfirmConfig::default(),
            editor: None,
            relative_dates: true,
            custom_commands: Vec::new(),
            keybindings: BTreeMap::new(),
            theme: ThemeSettings::default(),
        }
    }
}

/// How much of the screen the focused window takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CollapsedWindows {
    /// One-line title bars that can be clicked.
    #[default]
    Titles,
    /// Not shown.
    Hidden,
}

/// How the staged and unstaged sections of the main view are arranged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LayoutConfig {
    /// Screen mode at startup; `+` and `_` change it for the session.
    pub screen_mode: ScreenMode,
    /// Percentage of the width used by the side windows (10–90). Normal and half mode share
    /// one width, which the user can drag; this is its starting value.
    pub side_panel_width: u16,
    /// Starting side width instead of `side_panel_width` when starting in half mode (10–90).
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
    /// Side windows that start collapsed to their title, by any of their view names.
    pub collapsed: Vec<String>,
    /// Reopen each launch directory as it was left: layout, focus, tabs, selections, modes
    /// and history (in `$XDG_STATE_HOME/bcode-git/layouts`). The configuration is the
    /// starting point.
    pub remember: bool,
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
            collapsed: Vec::new(),
            remember: true,
        }
    }
}

/// Command log settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilesConfig {
    /// Show files as a directory tree rather than a flat list; `` ` `` toggles it.
    pub tree: bool,
    /// Lines added and removed beside files, folders and the panel title.
    pub line_counts: LineCountsConfig,
}

impl Default for FilesConfig {
    fn default() -> Self {
        Self {
            tree: true,
            line_counts: LineCountsConfig::default(),
        }
    }
}

/// Line count settings (`[plugins.config."bcode.git".ui.files.line_counts]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LineCountsConfig {
    /// What the counts measure.
    pub show: LineCountsShow,
    /// Where the counts sit on a row.
    pub align: LineCountsAlign,
    /// Keep the counts visible by shortening names that would push them out of view.
    pub floating: bool,
}

/// What file line counts measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LineCountsShow {
    /// Staged and unstaged changes together, against `HEAD`.
    #[default]
    Total,
    /// Staged and unstaged changes side by side.
    Split,
    /// No counts.
    Off,
}

/// Where line counts sit on a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LineCountsAlign {
    /// Right after the name.
    #[default]
    AfterName,
    /// Against the right edge of the panel.
    Right,
}

/// Commit editor settings (`[plugins.config."bcode.git".ui.commit]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CommitConfig {
    /// Prefill the commit editor from the first source with text.
    pub prefill: bool,
    /// Prefill sources in precedence order; omitted sources are not read. Names: `env`,
    /// `bcode-draft`, `lazygit-draft`, `merge`, `squash`, `edit-msg`, `template`.
    pub sources: Vec<String>,
    /// Keep an unsent message in `BCODE_GIT_COMMIT_MSG` in the git directory.
    pub save_draft: bool,
    /// Wrap description lines longer than this many columns when committing (git's
    /// convention is 72); `0` keeps lines as typed. The summary is never wrapped.
    pub wrap_width: u16,
    /// Commit without hooks (`--no-verify`) when the summary starts with this text, for
    /// example `"WIP"`; empty disables it.
    pub skip_hook_prefix: String,
}

impl Default for CommitConfig {
    fn default() -> Self {
        Self {
            prefill: true,
            sources: CommitMessageSource::ALL
                .iter()
                .map(|source| source.name().to_owned())
                .collect(),
            save_draft: true,
            wrap_width: 72,
            skip_hook_prefix: String::new(),
        }
    }
}

impl CommitConfig {
    /// Prefill sources in order, with problems reported and the bad entries skipped.
    #[must_use]
    pub fn resolve_sources(&self) -> (Vec<CommitMessageSource>, Vec<String>) {
        let mut sources = Vec::new();
        let mut diagnostics = Vec::new();
        if !self.prefill {
            return (sources, diagnostics);
        }
        for name in &self.sources {
            match CommitMessageSource::from_name(name) {
                Some(source) if !sources.contains(&source) => sources.push(source),
                Some(_) => diagnostics.push(format!(
                    "commit.sources: \"{name}\" appears more than once; later uses are ignored"
                )),
                None => diagnostics.push(format!(
                    "commit.sources: unknown source \"{name}\" (expected one of {})",
                    CommitMessageSource::ALL
                        .iter()
                        .map(|source| source.name())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            }
        }
        (sources, diagnostics)
    }
}

/// Diff presentation in the main view (`[diff]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiffConfig {
    /// Wrap long lines to the view width instead of cutting them off. Continuation rows keep
    /// the line-number gutter blank.
    pub wrap_lines: bool,
}

impl Default for DiffConfig {
    fn default() -> Self {
        Self { wrap_lines: true }
    }
}

/// Pull request state from GitHub (`[pull_requests]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PullRequestsConfig {
    /// Show pull requests for repositories on GitHub (or a listed Enterprise host).
    pub enabled: bool,
    /// How GitHub is reached: `auto` (HTTPS API with a token, falling back to `gh`), `http`,
    /// or `cli` (`gh`).
    pub transport: bcode_forge_models::Transport,
    /// Seconds between refreshes while the client is open (also refreshed after fetch, pull
    /// and push).
    pub refresh_seconds: u64,
    /// Remote whose host and repository are asked about.
    pub remote: String,
    /// GitHub Enterprise hosts to treat as GitHub.
    pub enterprise_hosts: Vec<String>,
    /// Environment variable to read a token from before `GITHUB_TOKEN` and `GH_TOKEN`.
    pub token_env: Option<String>,
}

impl Default for PullRequestsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            transport: bcode_forge_models::Transport::Auto,
            refresh_seconds: 120,
            remote: "origin".into(),
            enterprise_hosts: Vec::new(),
            token_env: None,
        }
    }
}

/// Staging view settings (`[staging]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StagingConfig {
    /// Start in hunk selection instead of line selection.
    pub hunk_mode: bool,
}

/// Syntax highlighting settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
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

/// Spinner frames: a preset name or the frames themselves.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum SpinnerSetting {
    /// `braille`, `dot` or `ascii`.
    Preset(String),
    /// Frames shown in order.
    Frames(Vec<String>),
}

/// How running work is shown (`[activity]`): spinners on what it affects, git's progress,
/// and a brief result mark.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ActivityConfig {
    /// Spinner: `braille`, `dot`, `ascii`, or a list of frames.
    pub spinner: SpinnerSetting,
    /// Milliseconds each spinner frame shows.
    pub rate_ms: u64,
    /// Milliseconds local work runs before it shows (network work shows at once), so quick
    /// work does not flicker.
    pub show_delay_ms: u64,
    /// Milliseconds a finished operation's `✓` or `✗` stays on what it affected; `0` turns
    /// the mark off.
    pub result_ms: u64,
    /// Animate spinners; when off, running work shows a still `…`.
    pub animate: bool,
}

impl Default for ActivityConfig {
    fn default() -> Self {
        Self {
            spinner: SpinnerSetting::Preset("braille".into()),
            rate_ms: 80,
            show_delay_ms: 150,
            result_ms: 1500,
            animate: true,
        }
    }
}

/// Which destructive actions ask first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
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
    /// Keep the command's live output popup open when it succeeds (it always stays open when
    /// the command fails).
    #[serde(default)]
    pub show_output: bool,
}

/// A prompt shown before a custom command runs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
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

/// Theme settings in the git UI schema. Each unset field falls back to `[tui.theme]`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeSettings {
    /// Theme id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Overlays applied after the base theme.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlays: Option<Vec<String>>,
    /// Extra theme files or directories.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<PathBuf>>,
}

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

/// Where the git UI settings live inside `bcode.toml`; the dedicated files use the same schema
/// at their root.
pub const BCODE_SECTION: [&str; 4] = ["plugins", "config", "bcode.git", "ui"];
/// Environment variable naming an extra dedicated config file (layered on top; must exist).
pub const GIT_CONFIG_ENV: &str = "BCODE_GIT_CONFIG";
/// Environment variable holding inline dedicated config TOML.
pub const GIT_CONFIG_TOML_ENV: &str = "BCODE_GIT_CONFIG_TOML";
/// Dedicated config file name in the user directory.
pub const GIT_CONFIG_FILE_NAME: &str = "config.toml";

/// Command-line configuration layers, the topmost of the dedicated family.
#[derive(Debug, Clone, Default)]
pub struct ConfigOptions {
    /// `--config FILE`.
    pub file: Option<PathBuf>,
    /// `--config-toml TEXT`.
    pub toml: Option<String>,
}

/// Candidate files of both configuration families, in precedence order (later wins).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigPaths {
    /// `bcode.toml` family.
    pub bcode: Vec<PathBuf>,
    /// Dedicated `bcode-git` family.
    pub git: Vec<PathBuf>,
}

/// Everything the UI reads from configuration files.
#[derive(Debug, Clone, Default)]
pub struct LoadedConfig {
    /// UI settings, keybindings and theme included as configured.
    pub ui: GitUiConfig,
    /// Effective `scope -> key -> action` overrides for `git.*` scopes.
    pub keybindings: BTreeMap<String, BTreeMap<String, String>>,
    /// Effective theme selection.
    pub theme: ThemeChoice,
    /// Problems found while loading; defaults are used for affected sections.
    pub diagnostics: Vec<String>,
    /// bcode user configuration directory.
    pub config_dir: PathBuf,
    /// Files that were read, in precedence order: the `bcode.toml` family first.
    pub files: Vec<PathBuf>,
    /// Dedicated `bcode-git` files that were read, in precedence order.
    pub git_files: Vec<PathBuf>,
    /// Active `[composition]` profile of the `bcode.toml` family.
    pub bcode_profile: Option<String>,
    /// Active `[composition]` profile of the dedicated family.
    pub git_profile: Option<String>,
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
    user_config_home(&ProcessConfigEnvironment).join("bcode")
}

/// Dedicated user configuration file (`$XDG_CONFIG_HOME/bcode-git/config.toml` or
/// `~/.config/bcode-git/config.toml`).
#[must_use]
pub fn user_git_config_file() -> PathBuf {
    user_git_config_file_in(&ProcessConfigEnvironment)
}

fn user_git_config_file_in(environment: &impl ConfigEnvironment) -> PathBuf {
    user_config_home(environment)
        .join("bcode-git")
        .join(GIT_CONFIG_FILE_NAME)
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

/// Candidate configuration files for a starting directory, existing or not.
#[must_use]
pub fn config_files(start: &Path, options: &ConfigOptions) -> ConfigPaths {
    config_files_in(start, options, &ProcessConfigEnvironment)
}

fn config_files_in(
    start: &Path,
    options: &ConfigOptions,
    environment: &impl ConfigEnvironment,
) -> ConfigPaths {
    let mut bcode =
        bcode_config_layers::default_config_paths_from_with_environment(start, environment);
    bcode.extend(
        ConfigLoadOverrides::from_config_environment_with_cli(environment, None, None)
            .env_config_path,
    );
    let mut git = git_config_paths_in(start, environment);
    let overrides = git_overrides(options, environment);
    git.extend(overrides.env_config_path);
    git.extend(overrides.cli_config_path);
    ConfigPaths { bcode, git }
}

/// Dedicated files discovered for `start`: the user file, then the repository's
/// `bcode-git.toml` and `.bcode-git/config.toml`, then the per-clone `bcode-git.toml` in
/// the git directory.
#[must_use]
pub fn git_config_paths(start: &Path) -> Vec<PathBuf> {
    git_config_paths_in(start, &ProcessConfigEnvironment)
}

fn git_config_paths_in(start: &Path, environment: &impl ConfigEnvironment) -> Vec<PathBuf> {
    let mut paths = vec![user_git_config_file_in(environment)];
    if let Some(root) = bcode_config_layers::discover_config_root(start) {
        paths.push(root.join("bcode-git.toml"));
        paths.push(root.join(".bcode-git").join(GIT_CONFIG_FILE_NAME));
        if let Some(git_dir) = git_dir(&root) {
            paths.push(git_dir.join("bcode-git.toml"));
        }
    }
    paths
}

/// The git directory of a worktree root: `.git` itself, or where a `.git` file points.
fn git_dir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let target = text
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))?
        .trim();
    Some(root.join(target))
}

fn git_overrides(
    options: &ConfigOptions,
    environment: &impl ConfigEnvironment,
) -> ConfigLoadOverrides {
    let var = |name: &str| {
        environment
            .var(name)
            .filter(|value| !value.trim().is_empty())
    };
    ConfigLoadOverrides {
        env_config_path: var(GIT_CONFIG_ENV).map(PathBuf::from),
        env_config_toml: var(GIT_CONFIG_TOML_ENV),
        cli_config_path: options.file.clone(),
        cli_config_toml: options.toml.clone(),
        ..ConfigLoadOverrides::default()
    }
}

/// Load configuration with no command-line layers. See [`load_with`].
#[must_use]
pub fn load(start: &Path) -> LoadedConfig {
    load_with(start, &ConfigOptions::default())
}

/// Load configuration: bcode's `bcode.toml` family composed exactly as bcode does, then the
/// dedicated `bcode-git` family composed on top of its git UI section, each with its own
/// `[composition]` profiles.
///
/// Order, lowest first: defaults; `bcode.toml` layers (user, repository, `$BCODE_CONFIG`,
/// `$BCODE_CONFIG_TOML`); the dedicated family's active profile; dedicated files (user,
/// repository, per clone); `$BCODE_GIT_CONFIG`; `$BCODE_GIT_CONFIG_TOML`; `options`.
#[must_use]
pub fn load_with(start: &Path, options: &ConfigOptions) -> LoadedConfig {
    load_in(start, options, &ProcessConfigEnvironment)
}

/// [`load_with`] reading environment variables (including `$XDG_CONFIG_HOME` and `$HOME`,
/// which locate the user files) from `environment` instead of the process.
#[must_use]
pub fn load_in(
    start: &Path,
    options: &ConfigOptions,
    environment: &impl ConfigEnvironment,
) -> LoadedConfig {
    let mut loaded = LoadedConfig {
        config_dir: user_config_home(environment).join("bcode"),
        ..LoadedConfig::default()
    };
    let bcode = match bcode_config_layers::load_layered_from_paths(
        &bcode_config_layers::default_config_paths_from_with_environment(start, environment),
        &ConfigLoadOverrides::from_config_environment_with_cli(environment, None, None),
        toml::Table::new(),
    ) {
        Ok(layered) => Some(layered),
        Err(error) => {
            loaded
                .diagnostics
                .push(format!("bcode.toml configuration not loaded: {error}"));
            None
        }
    };
    let bcode_table = bcode
        .as_ref()
        .map(|layered| layered.value.clone())
        .unwrap_or_default();
    let base = section(&bcode_table);
    let git = match bcode_config_layers::load_layered_from_paths(
        &git_config_paths_in(start, environment),
        &git_overrides(options, environment),
        base.clone(),
    ) {
        Ok(layered) => Some(layered),
        Err(error) => {
            loaded
                .diagnostics
                .push(format!("bcode-git configuration not loaded: {error}"));
            None
        }
    };
    if let Some(layered) = &bcode {
        loaded.files.clone_from(&layered.files);
        loaded
            .bcode_profile
            .clone_from(&layered.resolution.selected_profile);
    }
    if let Some(layered) = &git {
        loaded.git_files.clone_from(&layered.files);
        loaded
            .git_profile
            .clone_from(&layered.resolution.selected_profile);
    }
    let ui = git.as_ref().map_or(base, |layered| layered.value.clone());
    let locate = |path: &[String]| -> String {
        let git_origin = git.as_ref().and_then(|layered| {
            layered
                .origin(path)
                .filter(|origin| origin.layer != bcode_config_layers::LayerOrigin::Defaults)
        });
        if let Some(origin) = git_origin {
            return format!(
                "{}: {}",
                describe_origin(&origin, GIT_CONFIG_TOML_ENV),
                dotted(path)
            );
        }
        let mut full: Vec<String> = BCODE_SECTION.iter().map(|key| (*key).to_owned()).collect();
        full.extend_from_slice(path);
        bcode
            .as_ref()
            .and_then(|layered| layered.origin(&full))
            .map_or_else(
                || dotted(&full),
                |origin| {
                    format!(
                        "{}: {}",
                        describe_origin(&origin, bcode_config_layers::BCODE_CONFIG_TOML_ENV),
                        dotted(&full)
                    )
                },
            )
    };
    apply(&bcode_table, &ui, &locate, &mut loaded);
    loaded
}

/// Name an origin, spelling inline layers as the variable or flag that supplied them.
fn describe_origin(origin: &bcode_config_layers::ValueOrigin, environment: &str) -> String {
    use bcode_config_layers::{InlineLayer, LayerOrigin};
    let layer = match &origin.layer {
        LayerOrigin::Inline(InlineLayer::Environment) => format!("${environment}"),
        LayerOrigin::Inline(InlineLayer::Cli) => "--config-toml".to_owned(),
        other => other.to_string(),
    };
    match &origin.profile {
        Some(profile) => format!("{layer} (profile {profile})"),
        None => layer,
    }
}

/// Interpret an already-merged `bcode.toml` table, with no dedicated files.
#[must_use]
pub fn from_table(table: &toml::Table) -> LoadedConfig {
    from_tables(table, None)
}

/// Interpret an already-merged `bcode.toml` table and, optionally, an already-merged dedicated
/// table composed over its git UI section.
#[must_use]
pub fn from_tables(bcode: &toml::Table, git: Option<&toml::Table>) -> LoadedConfig {
    let mut loaded = LoadedConfig {
        config_dir: default_config_dir(),
        ..LoadedConfig::default()
    };
    let mut ui = section(bcode);
    if let Some(git) = git {
        let mut merged = toml::Value::Table(ui);
        bcode_config_layers::merge_config_values(&mut merged, toml::Value::Table(git.clone()));
        ui = match merged {
            toml::Value::Table(table) => table,
            _ => toml::Table::new(),
        };
    }
    apply(bcode, &ui, &|path: &[String]| dotted(path), &mut loaded);
    loaded
}

/// The git UI section of a `bcode.toml` table.
fn section(bcode: &toml::Table) -> toml::Table {
    lookup(bcode, &BCODE_SECTION)
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default()
}

fn dotted(path: &[String]) -> String {
    path.iter()
        .map(|key| {
            if key
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
            {
                key.clone()
            } else {
                format!("\"{key}\"")
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// Decode the composed git UI table and combine it with bcode's shared sections.
fn apply(
    bcode: &toml::Table,
    ui: &toml::Table,
    locate: &dyn Fn(&[String]) -> String,
    loaded: &mut LoadedConfig,
) {
    loaded.plugin_disabled = string_list(lookup(bcode, &["plugins", "disabled"]))
        .iter()
        .any(|id| id == "bcode.git");
    match toml::Value::Table(ui.clone()).try_into::<GitUiConfig>() {
        Ok(config) => loaded.ui = config,
        Err(error) => {
            // The location is reported separately, with the file it came from.
            let message = error.to_string();
            let message = message.lines().next().unwrap_or_default().trim();
            loaded
                .diagnostics
                .push(format!("{}: {message}", locate(&failing_path(ui))));
        }
    }

    // Keybindings: bcode's shared scopes first, then the git UI schema's own.
    if let Some(toml::Value::Table(scopes)) = lookup(bcode, &["tui", "keybindings", "scopes"]) {
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
            let resolved = loaded.keybindings.entry(scope.clone()).or_default();
            for (key, action) in bindings {
                match action.as_str() {
                    Some(action) => {
                        resolved.insert(key.clone(), binding(action));
                    }
                    None => loaded
                        .diagnostics
                        .push(format!("keybinding {scope}.{key} must be an action id")),
                }
            }
        }
    }
    for (scope, bindings) in &loaded.ui.keybindings {
        if !scope.starts_with("git.") {
            loaded.diagnostics.push(format!(
                "keybindings.\"{scope}\": git UI scopes start with \"git.\""
            ));
            continue;
        }
        let resolved = loaded.keybindings.entry(scope.clone()).or_default();
        for (key, action) in bindings {
            resolved.insert(key.clone(), binding(action));
        }
    }

    // Theme: bcode's shared selection, with the git UI schema's fields taking precedence.
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
    let own = &loaded.ui.theme;
    if let Some(name) = &own.name {
        loaded.theme.name.clone_from(name);
    }
    if let Some(overlays) = &own.overlays {
        loaded.theme.overlays.clone_from(overlays);
    }
    if let Some(paths) = &own.paths {
        loaded.theme.paths.clone_from(paths);
    }
}

fn binding(action: &str) -> String {
    if matches!(action, "none" | "unbind") {
        String::new()
    } else {
        action.to_owned()
    }
}

/// Key path of the first value the schema rejects, for locating it in a file.
fn failing_path(ui: &toml::Table) -> Vec<String> {
    use bmux_config_composition::{Document, PathSegment, SourceId, TomlCodec};
    let Ok(value) = TomlCodec::from_toml(toml::Value::Table(ui.clone())) else {
        return Vec::new();
    };
    match Document::new(value, SourceId(0)).decode_located::<GitUiConfig>() {
        Ok(_) => Vec::new(),
        Err(failure) => failure
            .path
            .iter()
            .map_while(|segment| match segment {
                PathSegment::Key(key) => Some(key.clone()),
                PathSegment::Index(_) => None,
            })
            .collect(),
    }
}

/// The effective settings as a dedicated config file (or, with `nested`, as the `bcode.toml`
/// section), with keybindings and theme as configured.
///
/// # Errors
/// * The settings cannot be expressed as TOML.
pub fn to_toml(config: &GitUiConfig, nested: bool) -> Result<String, String> {
    let value = toml::Value::try_from(config).map_err(|error| error.to_string())?;
    let toml::Value::Table(mut table) = value else {
        return Err("settings are not a table".into());
    };
    if nested {
        for key in BCODE_SECTION.iter().rev() {
            let mut parent = toml::Table::new();
            parent.insert((*key).to_owned(), toml::Value::Table(table));
            table = parent;
        }
    }
    toml::to_string(&table).map_err(|error| error.to_string())
}

/// Human-readable report of both configuration families: each candidate file with whether it
/// was read, the active profiles, and any problems.
#[must_use]
pub fn describe_files(start: &Path, options: &ConfigOptions) -> String {
    let loaded = load_with(start, options);
    let paths = config_files(start, options);
    let mut out = String::new();
    let mut family =
        |title: &str, candidates: &[PathBuf], read: &[PathBuf], profile: &Option<String>| {
            out.push_str(title);
            out.push('\n');
            for path in candidates {
                let state = if read.contains(path) {
                    "read   "
                } else {
                    "missing"
                };
                let _ = writeln!(out, "  {state} {}", path.display());
            }
            if let Some(profile) = profile {
                let _ = writeln!(out, "  profile {profile}");
            }
        };
    family(
        "bcode.toml ([plugins.config.\"bcode.git\".ui], tui.keybindings, tui.theme):",
        &paths.bcode,
        &loaded.files,
        &loaded.bcode_profile,
    );
    family(
        "bcode-git (same schema at the root, composed on top):",
        &paths.git,
        &loaded.git_files,
        &loaded.git_profile,
    );
    for (variable, inline) in [
        (bcode_config_layers::BCODE_CONFIG_TOML_ENV, "bcode.toml"),
        (GIT_CONFIG_TOML_ENV, "bcode-git"),
    ] {
        if std::env::var(variable).is_ok_and(|value| !value.trim().is_empty()) {
            let _ = writeln!(out, "inline ${variable} ({inline})");
        }
    }
    if options.toml.is_some() {
        out.push_str("inline --config-toml (bcode-git)\n");
    }
    for diagnostic in &loaded.diagnostics {
        let _ = writeln!(out, "problem: {diagnostic}");
    }
    out
}

/// A configuration command offered by both `bcode-git` and `bcode git`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigCommand {
    /// `--print-config-paths`.
    PrintPaths,
    /// `--print-config [--nested]`.
    Print {
        /// Emit the `bcode.toml` form instead of a dedicated file.
        nested: bool,
    },
    /// `--import-lazygit [FILE] [--nested]`.
    ImportLazygit {
        /// lazygit `config.yml`; found automatically when absent.
        source: Option<PathBuf>,
        /// Emit the `bcode.toml` form instead of a dedicated file.
        nested: bool,
    },
}

/// Run a configuration command and return what it prints.
///
/// # Errors
/// * The lazygit configuration cannot be found or read, or settings cannot be written as TOML.
pub fn run_command(
    start: &Path,
    options: &ConfigOptions,
    command: ConfigCommand,
) -> Result<String, String> {
    match command {
        ConfigCommand::PrintPaths => Ok(describe_files(start, options)),
        ConfigCommand::Print { nested } => {
            let loaded = load_with(start, options);
            let mut text = String::new();
            for diagnostic in &loaded.diagnostics {
                let _ = writeln!(text, "# problem: {diagnostic}");
            }
            text.push_str(&to_toml(&loaded.ui, nested)?);
            Ok(text)
        }
        ConfigCommand::ImportLazygit { source, nested } => {
            let (path, import) = crate::lazygit::import_file(source, nested)?;
            Ok(format!("# Source: {}\n{}", path.display(), import.toml))
        }
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

    /// No variables at all, with the user configuration home inside the test's own directory,
    /// so the developer's real user files and `$BCODE_*` variables never leak into a test.
    struct IsolatedEnvironment {
        config_home: PathBuf,
    }

    impl ConfigEnvironment for IsolatedEnvironment {
        fn var(&self, name: &str) -> Option<String> {
            self.var_os(name)?.into_string().ok()
        }

        fn var_os(&self, name: &str) -> Option<std::ffi::OsString> {
            (name == "XDG_CONFIG_HOME").then(|| self.config_home.clone().into_os_string())
        }

        fn current_dir(&self) -> PathBuf {
            self.config_home.clone()
        }
    }

    // Shadow the process-environment loaders for every test in this module.
    fn load_with(start: &Path, options: &ConfigOptions) -> LoadedConfig {
        let environment = IsolatedEnvironment {
            config_home: start.join(".test-config-home"),
        };
        load_in(start, options, &environment)
    }

    fn load(start: &Path) -> LoadedConfig {
        load_with(start, &ConfigOptions::default())
    }

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

    /// The same settings body, in every part of the schema.
    const BODY: &str = r#"
        diff_context = 9
        [layout]
        screen_mode = "half"
        windows = [["files"], ["commits"]]
        [files]
        tree = false
        [commit]
        sources = ["merge"]
        [keybindings."git.list"]
        n = "bcode.git.list.up"
        x = "none"
        [theme]
        name = "nord"
        [[custom_commands]]
        key = "ctrl+b"
        command = "echo {branch}"
    "#;

    fn nest(body: &str) -> toml::Table {
        let mut table: toml::Table = body.parse().unwrap();
        for key in BCODE_SECTION.iter().rev() {
            let mut parent = toml::Table::new();
            parent.insert((*key).to_owned(), toml::Value::Table(table));
            table = parent;
        }
        table
    }

    #[test]
    fn dedicated_files_and_bcode_toml_share_one_schema() {
        let nested = from_table(&nest(BODY));
        let dedicated = from_tables(&toml::Table::new(), Some(&BODY.parse().unwrap()));
        assert!(nested.diagnostics.is_empty(), "{:?}", nested.diagnostics);
        assert_eq!(nested.ui, dedicated.ui);
        assert_eq!(nested.keybindings, dedicated.keybindings);
        assert_eq!(nested.theme, dedicated.theme);
        assert_eq!(nested.ui.diff_context, 9);
        assert_eq!(nested.keybindings["git.list"]["x"], "");
        assert_eq!(nested.theme.name, "nord");
    }

    #[test]
    fn git_ui_keybindings_and_theme_refine_bcode_wide_ones() {
        let mut bcode = nest(
            "[keybindings.\"git.files\"]\nx = \"bcode.git.files.discard\"\n\
             [theme]\noverlays = [\"mine\"]\n",
        );
        let shared: toml::Table = r#"
            [tui.keybindings.scopes."git.files"]
            x = "bcode.git.files.commit"
            y = "bcode.git.files.stash"
            [tui.theme]
            name = "nord"
            overlays = ["high-contrast"]
        "#
        .parse()
        .unwrap();
        bcode.extend(shared);
        let loaded = from_table(&bcode);
        assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
        assert_eq!(
            loaded.keybindings["git.files"]["x"],
            "bcode.git.files.discard"
        );
        assert_eq!(
            loaded.keybindings["git.files"]["y"],
            "bcode.git.files.stash"
        );
        assert_eq!(loaded.theme.name, "nord");
        assert_eq!(loaded.theme.overlays, ["mine"]);
    }

    fn repository() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join(".bcode-git")).unwrap();
        dir
    }

    #[test]
    fn dedicated_layers_compose_over_bcode_toml_in_order() {
        let dir = repository();
        let root = dir.path();
        let write = |name: &str, text: &str| std::fs::write(root.join(name), text).unwrap();
        write(
            "bcode.toml",
            "[plugins.config.\"bcode.git\".ui]\ndiff_context = 1\nlog_page_size = 11\n\
             [plugins.config.\"bcode.git\".ui.layout]\nside_panel_width = 21\n\
             expanded_weight = 7\n",
        );
        write(
            "bcode-git.toml",
            "diff_context = 2\n[layout]\nside_panel_width = 22\n",
        );
        write(
            ".bcode-git/config.toml",
            "[layout]\nside_panel_width = 23\n[composition]\nactive_profile = \"p\"\n\
             [composition.profiles.p.patch]\nrelative_dates = false\n\
             [composition.profiles.p.patch.layout]\nportrait_below_width = 99\n",
        );
        write(
            ".git/bcode-git.toml",
            "[layout]\nhalf_side_panel_width = 44\n",
        );
        let options = ConfigOptions {
            file: None,
            toml: Some("[files]\ntree = false\n".into()),
        };
        let loaded = load_with(root, &options);
        assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
        let ui = &loaded.ui;
        // bcode.toml values survive where nothing overrides them; tables merge deeply.
        assert_eq!(ui.log_page_size, 11);
        assert_eq!(ui.layout.expanded_weight, 7);
        // Each dedicated layer overrides the ones before it.
        assert_eq!(ui.diff_context, 2);
        assert_eq!(ui.layout.side_panel_width, 23);
        assert_eq!(ui.layout.half_side_panel_width, 44);
        assert!(!ui.files.tree);
        // The dedicated family's own profile applies on top of the bcode base.
        assert!(!ui.relative_dates);
        assert_eq!(ui.layout.portrait_below_width, 99);
        assert_eq!(loaded.git_profile.as_deref(), Some("p"));
        assert!(
            loaded
                .git_files
                .contains(&root.join(".git").join("bcode-git.toml"))
        );
    }

    #[test]
    fn problems_name_the_dedicated_file_and_missing_files_are_reported() {
        let dir = repository();
        let root = dir.path();
        std::fs::write(root.join("bcode-git.toml"), "[layout]\nscreen_mode = 3\n").unwrap();
        let loaded = load(root);
        assert!(
            loaded.diagnostics.iter().any(|problem| problem
                .contains(&root.join("bcode-git.toml").display().to_string())
                && problem.contains("layout.screen_mode")),
            "{:?}",
            loaded.diagnostics
        );
        let missing = load_with(
            root,
            &ConfigOptions {
                file: Some(root.join("nope.toml")),
                toml: None,
            },
        );
        assert!(
            missing
                .diagnostics
                .iter()
                .any(|problem| problem.contains("bcode-git configuration not loaded")),
            "{:?}",
            missing.diagnostics
        );
    }

    #[test]
    fn printed_configuration_reads_back_identically() {
        let loaded = from_table(&nest(BODY));
        let flat = to_toml(&loaded.ui, false).unwrap();
        let again = from_tables(&toml::Table::new(), Some(&flat.parse().unwrap()));
        assert_eq!(again.ui, loaded.ui);
        let nested = to_toml(&loaded.ui, true).unwrap();
        assert_eq!(from_table(&nested.parse().unwrap()).ui, loaded.ui);
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
