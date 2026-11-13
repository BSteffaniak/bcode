//! The effective appearance: which palette, profile, overlays and light/dark variant a Bcode
//! program presents.
//!
//! Configuration (`[tui.theme]` in `bcode.toml`) selects a palette (`name`), an optional
//! `profile`, further `overlays`, a light/dark `variant` and extra theme `paths`. The appearance
//! picker saves a palette and a profile in user state; those replace the configured ones when
//! the catalog can present them. Every Bcode program (the chat interface, the review app and the
//! git client) resolves its theme through [`resolve_appearance`] or [`apply_saved_appearance`].
//!
//! An app (the review app, the git client) may keep its own saved choice, which replaces
//! Bcode's for that app only ([`AppearanceSources::app_saved`]); without one it follows Bcode's.
//! An app's own configuration may pin its palette ([`AppearanceSources::theme_pinned_by`]) or
//! its profile ([`AppearanceSources::profile_pinned_by`]), which then wins over every saved
//! palette or profile. Precedence for an app, per axis, highest first:
//!
//! * the palette or profile its configuration pins;
//! * its own saved choice;
//! * Bcode's saved choice;
//! * the configured `[tui.theme]` selection and its defaults.

use std::path::{Path, PathBuf};

use bmux_tui::capabilities::TerminalBackground;
use serde::Deserialize;

use crate::definition::{
    ResolvedThemeDefinition, ResolvedThemeVariant, ThemeCatalog, ThemeError, ThemeKind,
    ThemeSelection,
};
use crate::discovery::{
    ThemeDiscoveryDiagnostic, ThemeExternalSource, default_theme_roots, discover_themes,
};

/// Palette presented when configuration names none.
pub const DEFAULT_THEME: &str = "terminal-native";

/// Light/dark variant preference (`[tui.theme] variant`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VariantPreference {
    /// Follow the terminal background.
    #[default]
    Auto,
    /// Always the dark variant.
    Dark,
    /// Always the light variant.
    Light,
}

impl VariantPreference {
    /// The variant to resolve for a terminal with `background`. `auto` selects light only for a
    /// light background; an unknown background selects dark.
    #[must_use]
    pub const fn resolve(self, background: TerminalBackground) -> ResolvedThemeVariant {
        match self {
            Self::Dark => ResolvedThemeVariant::Dark,
            Self::Light => ResolvedThemeVariant::Light,
            Self::Auto => match background {
                TerminalBackground::Light => ResolvedThemeVariant::Light,
                TerminalBackground::Dark | TerminalBackground::Unknown => {
                    ResolvedThemeVariant::Dark
                }
            },
        }
    }
}

/// Configured theme selection: the `[tui.theme]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ThemePreferences {
    /// Palette or complete theme id.
    pub name: String,
    /// Profile overlay applied over the palette before [`Self::overlays`].
    pub profile: Option<String>,
    /// Further overlays, applied in order.
    pub overlays: Vec<String>,
    /// Light/dark variant preference.
    pub variant: VariantPreference,
    /// Extra theme files or directories.
    pub paths: Vec<PathBuf>,
}

impl Default for ThemePreferences {
    fn default() -> Self {
        Self {
            name: DEFAULT_THEME.to_owned(),
            profile: None,
            overlays: Vec::new(),
            variant: VariantPreference::Auto,
            paths: Vec::new(),
        }
    }
}

impl ThemePreferences {
    /// Read a `[tui.theme]` table. Keys this selection does not use (such as accent settings)
    /// are ignored; absent keys take their defaults.
    ///
    /// # Errors
    ///
    /// * A message naming the problem when a selection key has the wrong type or an unknown
    ///   value.
    pub fn from_toml(table: &toml::Value) -> Result<Self, String> {
        table
            .clone()
            .try_into::<Self>()
            .map_err(|error| error.message().trim().to_owned())
    }

    /// Overlays in application order: the profile first, then explicit overlays.
    #[must_use]
    pub fn effective_overlays(&self) -> Vec<String> {
        self.profile
            .iter()
            .filter(|profile| !profile.trim().is_empty())
            .chain(&self.overlays)
            .cloned()
            .collect()
    }

    /// The catalog selection these preferences resolve for `variant`.
    #[must_use]
    pub fn selection(&self, variant: ResolvedThemeVariant) -> ThemeSelection {
        ThemeSelection::new(self.name.clone())
            .overlays(self.effective_overlays())
            .variant(variant)
    }
}

/// Palette and profile the appearance picker saved in user state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedAppearance {
    /// Saved palette or theme id.
    pub theme: Option<String>,
    /// Saved profile id.
    pub profile: Option<String>,
}

/// Why a saved choice was not applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppearanceNotice {
    /// The saved palette does not resolve here; the configured one is presented instead.
    SavedThemeUnavailable {
        /// The saved palette.
        theme: String,
        /// The configured palette presented instead.
        configured: String,
    },
    /// The saved profile is not a profile in the catalog; the configured one is kept.
    SavedProfileUnavailable {
        /// The saved profile.
        profile: String,
    },
}

impl std::fmt::Display for AppearanceNotice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SavedThemeUnavailable { theme, configured } => write!(
                formatter,
                "saved theme {theme} is unavailable; using configured default {configured}"
            ),
            Self::SavedProfileUnavailable { profile } => {
                write!(formatter, "saved profile {profile} is unavailable")
            }
        }
    }
}

/// Apply `saved` over `configured`.
///
/// The saved palette replaces the configured one when it resolves in `catalog` with the
/// configured overlays and `variant`; then the saved profile replaces the configured one when
/// the catalog defines it as a profile. A saved choice that cannot apply keeps the configured
/// value and is reported.
#[must_use]
pub fn apply_saved_appearance(
    configured: &ThemePreferences,
    saved: &SavedAppearance,
    catalog: &ThemeCatalog,
    variant: ResolvedThemeVariant,
) -> (ThemePreferences, Vec<AppearanceNotice>) {
    let mut effective = configured.clone();
    let mut notices = Vec::new();
    if let Some(theme) = saved.theme.as_deref().filter(|theme| !theme.is_empty()) {
        let candidate = ThemePreferences {
            name: theme.to_owned(),
            ..effective.clone()
        };
        if catalog.resolve(&candidate.selection(variant)).is_ok() {
            effective = candidate;
        } else {
            notices.push(AppearanceNotice::SavedThemeUnavailable {
                theme: theme.to_owned(),
                configured: configured.name.clone(),
            });
        }
    }
    if let Some(profile) = saved
        .profile
        .as_deref()
        .filter(|profile| !profile.is_empty())
    {
        let known = catalog
            .definition(profile)
            .is_some_and(|definition| definition.kind() == ThemeKind::Profile);
        if known {
            effective.profile = Some(profile.to_owned());
        } else {
            notices.push(AppearanceNotice::SavedProfileUnavailable {
                profile: profile.to_owned(),
            });
        }
    }
    (effective, notices)
}

/// The appearance a program presents, and how it was reached.
#[derive(Debug, Clone)]
pub struct AppearanceResolution {
    /// Configuration with the saved choices that applied.
    pub preferences: ThemePreferences,
    /// Configuration the saved choices were applied over.
    pub configured: ThemePreferences,
    /// Bcode's choices saved in user state, including any this program ignores.
    pub saved: SavedAppearance,
    /// The program's own saved choices, which replace [`Self::saved`] while present.
    pub app_saved: Option<SavedAppearance>,
    /// What pins this program's palette, when its configuration does
    /// ([`AppearanceSources::theme_pinned_by`]).
    pub theme_pinned_by: Option<String>,
    /// What pins this program's profile, when its configuration does
    /// ([`AppearanceSources::profile_pinned_by`]).
    pub profile_pinned_by: Option<String>,
    /// Variant resolved for the terminal.
    pub variant: ResolvedThemeVariant,
    /// The resolved theme, or why [`Self::preferences`] do not resolve.
    pub resolved: Result<ResolvedThemeDefinition, ThemeError>,
    /// The discovered catalog the theme was resolved from.
    pub catalog: ThemeCatalog,
    /// Where each external theme of [`Self::catalog`] came from; bundled themes are absent.
    pub sources: std::collections::BTreeMap<String, ThemeExternalSource>,
    /// Saved choices that could not apply.
    pub notices: Vec<AppearanceNotice>,
    /// Theme files skipped during discovery.
    pub diagnostics: Vec<ThemeDiscoveryDiagnostic>,
}

impl AppearanceResolution {
    /// The saved choices this appearance presents: the program's own when it has them, else
    /// Bcode's.
    #[must_use]
    pub fn effective_saved(&self) -> &SavedAppearance {
        self.app_saved.as_ref().unwrap_or(&self.saved)
    }

    /// Whether the program follows Bcode's saved choices (it has none of its own).
    #[must_use]
    pub const fn follows_bcode(&self) -> bool {
        self.app_saved.is_none()
    }

    /// The appearance these inputs present with Bcode's saved choices `saved` and the
    /// program's own `app_saved` instead of [`Self::saved`] and [`Self::app_saved`]: the same
    /// configuration, catalog and variant, so nothing is discovered again. A pinned palette or
    /// profile ignores the saved one, as [`AppearanceSources::resolve`] does.
    #[must_use]
    pub fn with_saved(&self, saved: &SavedAppearance, app_saved: Option<&SavedAppearance>) -> Self {
        let effective_saved = effective_saved(
            app_saved.unwrap_or(saved),
            self.theme_pinned_by.as_deref(),
            self.profile_pinned_by.as_deref(),
        );
        let (preferences, notices) = apply_saved_appearance(
            &self.configured,
            &effective_saved,
            &self.catalog,
            self.variant,
        );
        let resolved = self.catalog.resolve(&preferences.selection(self.variant));
        Self {
            preferences,
            configured: self.configured.clone(),
            saved: saved.clone(),
            app_saved: app_saved.cloned(),
            theme_pinned_by: self.theme_pinned_by.clone(),
            profile_pinned_by: self.profile_pinned_by.clone(),
            variant: self.variant,
            resolved,
            catalog: self.catalog.clone(),
            sources: self.sources.clone(),
            notices,
            diagnostics: self.diagnostics.clone(),
        }
    }
}

/// The saved choices that apply when configuration pins the palette (`theme_pinned_by`) or
/// the profile (`profile_pinned_by`): the saved value of a pinned axis is dropped, the other
/// still applies.
fn effective_saved(
    saved: &SavedAppearance,
    theme_pinned_by: Option<&str>,
    profile_pinned_by: Option<&str>,
) -> SavedAppearance {
    SavedAppearance {
        theme: saved.theme.clone().filter(|_| theme_pinned_by.is_none()),
        profile: saved
            .profile
            .clone()
            .filter(|_| profile_pinned_by.is_none()),
    }
}

/// Resolve the appearance `configured` and `saved` select for a terminal with `background`.
///
/// Discovers the themes `configured` can reach (bundled, `<user_config_dir>/themes`,
/// `<project_root>/.bcode/themes` and the configured paths), applies `saved` over `configured`
/// ([`apply_saved_appearance`]) and resolves the result.
///
/// Discovery failures (only possible when embedded themes are invalid) fall back to the bundled
/// catalog.
#[must_use]
pub fn resolve_appearance(
    user_config_dir: &Path,
    project_root: &Path,
    configured: &ThemePreferences,
    saved: &SavedAppearance,
    background: TerminalBackground,
) -> AppearanceResolution {
    let roots = default_theme_roots(user_config_dir, project_root, &configured.paths);
    let (catalog, sources, diagnostics) = match discover_themes(&roots) {
        Ok(discovered) => (
            discovered.catalog,
            discovered.sources,
            discovered.diagnostics,
        ),
        Err(_) => (
            ThemeCatalog::bundled().unwrap_or_default(),
            std::collections::BTreeMap::new(),
            Vec::new(),
        ),
    };
    let variant = configured.variant.resolve(background);
    let (preferences, notices) = apply_saved_appearance(configured, saved, &catalog, variant);
    let resolved = catalog.resolve(&preferences.selection(variant));
    AppearanceResolution {
        preferences,
        configured: configured.clone(),
        saved: saved.clone(),
        app_saved: None,
        theme_pinned_by: None,
        profile_pinned_by: None,
        variant,
        resolved,
        catalog,
        sources,
        notices,
        diagnostics,
    }
}

/// Everything an appearance resolves from, as a program reads it.
#[derive(Debug, Clone, Default)]
pub struct AppearanceSources {
    /// Configured `[tui.theme]` selection.
    pub configured: ThemePreferences,
    /// Bcode's choices saved in user state.
    pub saved: SavedAppearance,
    /// The program's own choices saved in user state, which replace [`Self::saved`] while
    /// present; `None` follows Bcode's.
    pub app_saved: Option<SavedAppearance>,
    /// Files whose change may change [`Self::configured`] or [`Self::saved`]: configuration
    /// candidates (present or not) and the user state file.
    pub watched_files: Vec<PathBuf>,
    /// Bcode's user configuration directory, which holds user themes.
    pub user_config_dir: PathBuf,
    /// Project whose `.bcode/themes` holds project themes.
    pub project_root: PathBuf,
    /// Set when the program's own configuration pins the palette (for example
    /// `bcode-git.toml [theme] name`): names what pins it. Every saved palette is then
    /// ignored; the saved profile still applies.
    pub theme_pinned_by: Option<String>,
    /// Set when the program's own configuration pins the profile (for example the review's
    /// `[plugins.config."bcode.code_review".theme] profile`): names what pins it. Every saved
    /// profile is then ignored; the saved palette still applies.
    pub profile_pinned_by: Option<String>,
}

impl AppearanceSources {
    /// Resolve these sources for a terminal with `background` ([`resolve_appearance`]).
    #[must_use]
    pub fn resolve(&self, background: TerminalBackground) -> AppearanceResolution {
        let mut resolution = resolve_appearance(
            &self.user_config_dir,
            &self.project_root,
            &self.configured,
            &effective_saved(
                self.app_saved.as_ref().unwrap_or(&self.saved),
                self.theme_pinned_by.as_deref(),
                self.profile_pinned_by.as_deref(),
            ),
            background,
        );
        resolution.saved.clone_from(&self.saved);
        resolution.app_saved.clone_from(&self.app_saved);
        resolution.theme_pinned_by.clone_from(&self.theme_pinned_by);
        resolution
            .profile_pinned_by
            .clone_from(&self.profile_pinned_by);
        resolution
    }

    /// Signature of the current contents of every input: the watched files and the theme files
    /// discovery would read. It changes when any of them does.
    #[must_use]
    pub fn input_signature(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for path in &self.watched_files {
            path.hash(&mut hasher);
            std::fs::read(path).ok().hash(&mut hasher);
        }
        crate::discovery::theme_input_signature(&default_theme_roots(
            &self.user_config_dir,
            &self.project_root,
            &self.configured.paths,
        ))
        .hash(&mut hasher);
        hasher.finish()
    }
}

/// Keeps a program's appearance current.
///
/// A background thread re-reads the sources whenever their files change (checked every
/// `interval`) or the terminal reports a different background, and hands each new resolution to
/// the program. Dropping it stops the thread.
#[derive(Debug)]
pub struct AppearanceWatcher {
    messages: std::sync::mpsc::Sender<WatchMessage>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Debug, Clone, Copy)]
enum WatchMessage {
    Background(TerminalBackground),
    Stop,
}

/// Reports terminal backgrounds to an [`AppearanceWatcher`] from any thread.
#[derive(Debug, Clone)]
pub struct BackgroundReporter(std::sync::mpsc::Sender<WatchMessage>);

impl BackgroundReporter {
    /// The terminal reported `background`; the watcher resolves the appearance for it.
    pub fn report(&self, background: TerminalBackground) {
        let _ = self.0.send(WatchMessage::Background(background));
    }
}

impl AppearanceWatcher {
    /// Watch the sources `load` reads, starting from `initial` (which the program already
    /// presents) on a terminal with `background`. `apply` receives every changed resolution.
    #[must_use]
    pub fn spawn(
        initial: AppearanceSources,
        background: TerminalBackground,
        interval: std::time::Duration,
        mut load: impl FnMut() -> AppearanceSources + Send + 'static,
        mut apply: impl FnMut(AppearanceResolution) + Send + 'static,
    ) -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();
        // Taken now, so a change made while the thread starts is still seen as a change.
        let mut signature = initial.input_signature();
        let thread = std::thread::Builder::new()
            .name("bcode-appearance".to_owned())
            .spawn(move || {
                let mut sources = initial;
                let mut background = background;
                loop {
                    match receiver.recv_timeout(interval) {
                        Ok(WatchMessage::Background(reported)) => {
                            if reported == background {
                                continue;
                            }
                            background = reported;
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            if sources.input_signature() == signature {
                                continue;
                            }
                            sources = load();
                            signature = sources.input_signature();
                        }
                        Ok(WatchMessage::Stop)
                        | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    apply(sources.resolve(background));
                }
            })
            .ok();
        Self {
            messages: sender,
            thread,
        }
    }

    /// A handle that reports terminal backgrounds to this watcher.
    #[must_use]
    pub fn background_reporter(&self) -> BackgroundReporter {
        BackgroundReporter(self.messages.clone())
    }
}

impl Drop for AppearanceWatcher {
    fn drop(&mut self) {
        let _ = self.messages.send(WatchMessage::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured(source: &str) -> ThemePreferences {
        ThemePreferences::from_toml(&toml::from_str(source).expect("toml")).expect("preferences")
    }

    fn resolve(configured: &ThemePreferences, saved: &SavedAppearance) -> AppearanceResolution {
        let dir = tempfile::tempdir().expect("tempdir");
        resolve_appearance(
            dir.path(),
            dir.path(),
            configured,
            saved,
            TerminalBackground::Unknown,
        )
    }

    fn bundled(selection: &ThemeSelection) -> ResolvedThemeDefinition {
        ThemeCatalog::bundled()
            .expect("bundled")
            .resolve(selection)
            .expect("resolves")
    }

    #[test]
    fn configured_palette_profile_and_variant_are_honored() {
        let preferences = configured(
            r#"
            name = "catppuccin-mocha"
            profile = "structured"
            variant = "light"
            agent_accent = "theme_only"
            "#,
        );
        let resolution = resolve(&preferences, &SavedAppearance::default());
        let expected = bundled(
            &ThemeSelection::new("catppuccin-mocha")
                .overlays(vec!["structured".to_owned()])
                .variant(ResolvedThemeVariant::Light),
        );
        assert_eq!(resolution.variant, ResolvedThemeVariant::Light);
        assert_eq!(
            resolution.resolved.expect("resolved").fingerprint,
            expected.fingerprint
        );
        assert_eq!(resolution.notices, []);
    }

    #[test]
    fn saved_palette_and_profile_replace_the_configured_ones() {
        let preferences = configured("name = \"nord\"\nprofile = \"bare\"\noverlays = []");
        let saved = SavedAppearance {
            theme: Some("catppuccin-mocha".to_owned()),
            profile: Some("structured".to_owned()),
        };
        let resolution = resolve(&preferences, &saved);
        assert_eq!(resolution.preferences.name, "catppuccin-mocha");
        assert_eq!(
            resolution.preferences.profile.as_deref(),
            Some("structured")
        );
        let expected = bundled(
            &ThemeSelection::new("catppuccin-mocha")
                .overlays(vec!["structured".to_owned()])
                .variant(ResolvedThemeVariant::Dark),
        );
        assert_eq!(
            resolution.resolved.expect("resolved").fingerprint,
            expected.fingerprint
        );
    }

    #[test]
    fn unavailable_saved_choices_keep_the_configured_ones_and_say_so() {
        let preferences = configured("name = \"nord\"");
        let saved = SavedAppearance {
            theme: Some("no-such-palette".to_owned()),
            // A palette is not a profile.
            profile: Some("catppuccin-mocha".to_owned()),
        };
        let resolution = resolve(&preferences, &saved);
        assert_eq!(resolution.preferences, preferences);
        assert_eq!(
            resolution
                .notices
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            [
                "saved theme no-such-palette is unavailable; using configured default nord",
                "saved profile catppuccin-mocha is unavailable",
            ]
        );
    }

    #[test]
    fn an_apps_own_choice_replaces_bcodes_and_a_pinned_palette_wins_over_both() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sources = |app_saved: Option<SavedAppearance>, pinned: bool| AppearanceSources {
            configured: configured("name = \"nord\""),
            saved: SavedAppearance {
                theme: Some("catppuccin-mocha".to_owned()),
                profile: Some("structured".to_owned()),
            },
            app_saved,
            user_config_dir: dir.path().to_path_buf(),
            project_root: dir.path().to_path_buf(),
            theme_pinned_by: pinned.then(|| "app config".to_owned()),
            ..AppearanceSources::default()
        };
        let following = sources(None, false).resolve(TerminalBackground::Unknown);
        assert!(following.follows_bcode());
        assert_eq!(following.preferences.name, "catppuccin-mocha");
        assert_eq!(following.preferences.profile.as_deref(), Some("structured"));

        let own = SavedAppearance {
            theme: Some("dracula".to_owned()),
            profile: None,
        };
        let app = sources(Some(own.clone()), false).resolve(TerminalBackground::Unknown);
        assert!(!app.follows_bcode());
        assert_eq!(app.effective_saved(), &own);
        assert_eq!(app.preferences.name, "dracula");
        // The app's own choice is complete: Bcode's profile does not leak into it.
        assert_eq!(app.preferences.profile, None);

        let pinned = sources(Some(own), true).resolve(TerminalBackground::Unknown);
        assert_eq!(pinned.preferences.name, "nord");
    }

    #[test]
    fn a_pinned_profile_wins_over_every_saved_profile_and_keeps_the_saved_palette() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sources = |app_saved: Option<SavedAppearance>| AppearanceSources {
            configured: configured("name = \"nord\"\nprofile = \"bare\""),
            saved: SavedAppearance {
                theme: Some("catppuccin-mocha".to_owned()),
                profile: Some("structured".to_owned()),
            },
            app_saved,
            user_config_dir: dir.path().to_path_buf(),
            project_root: dir.path().to_path_buf(),
            profile_pinned_by: Some("app config".to_owned()),
            ..AppearanceSources::default()
        };
        // Following Bcode: Bcode's palette, the pinned profile.
        let following = sources(None).resolve(TerminalBackground::Unknown);
        assert_eq!(following.preferences.name, "catppuccin-mocha");
        assert_eq!(following.preferences.profile.as_deref(), Some("bare"));
        assert_eq!(following.profile_pinned_by.as_deref(), Some("app config"));

        // The app's own choice: its palette, still the pinned profile.
        let own = SavedAppearance {
            theme: Some("dracula".to_owned()),
            profile: Some("structured".to_owned()),
        };
        let app = sources(Some(own.clone())).resolve(TerminalBackground::Unknown);
        assert_eq!(app.preferences.name, "dracula");
        assert_eq!(app.preferences.profile.as_deref(), Some("bare"));
        let again = following.with_saved(&following.saved, Some(&own));
        assert_eq!(again.preferences.name, "dracula");
        assert_eq!(again.preferences.profile.as_deref(), Some("bare"));
    }

    #[test]
    fn auto_variant_follows_the_terminal_background() {
        assert_eq!(
            VariantPreference::Auto.resolve(TerminalBackground::Light),
            ResolvedThemeVariant::Light
        );
        assert_eq!(
            VariantPreference::Auto.resolve(TerminalBackground::Unknown),
            ResolvedThemeVariant::Dark
        );
        assert_eq!(
            VariantPreference::Dark.resolve(TerminalBackground::Light),
            ResolvedThemeVariant::Dark
        );
    }

    #[test]
    fn malformed_selection_keys_are_reported() {
        let error = ThemePreferences::from_toml(&toml::Value::Table(
            toml::from_str("variant = \"sepia\"").expect("toml"),
        ))
        .expect_err("unknown variant");
        assert!(error.contains("sepia"), "{error}");
        assert_eq!(
            ThemePreferences::from_toml(&toml::Value::Table(toml::Table::new())).expect("empty"),
            ThemePreferences::default()
        );
    }

    #[test]
    fn watcher_resolves_again_when_the_saved_appearance_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("tui.toml");
        let load = {
            let state = state.clone();
            let root = dir.path().to_path_buf();
            move || {
                let text = std::fs::read_to_string(&state).unwrap_or_default();
                // A write in progress reads as no saved choice.
                let table: toml::Table = toml::from_str(&text).unwrap_or_default();
                AppearanceSources {
                    saved: SavedAppearance {
                        theme: table
                            .get("theme")
                            .and_then(|theme| theme.get("name"))
                            .and_then(toml::Value::as_str)
                            .map(str::to_owned),
                        profile: None,
                    },
                    watched_files: vec![state.clone()],
                    user_config_dir: root.clone(),
                    project_root: root.clone(),
                    ..AppearanceSources::default()
                }
            }
        };
        let (sender, receiver) = std::sync::mpsc::channel();
        let watcher = AppearanceWatcher::spawn(
            load.clone()(),
            TerminalBackground::Unknown,
            std::time::Duration::from_millis(10),
            load,
            move |resolution| {
                let _ = sender.send(resolution.preferences.name);
            },
        );
        std::fs::write(&state, "[theme]\nname = \"nord\"\n").expect("write");
        let applied = receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("resolved after the change");
        assert_eq!(applied, "nord");

        let reporter = watcher.background_reporter();
        reporter.report(TerminalBackground::Light);
        let applied = receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("resolved for the new background");
        assert_eq!(applied, "nord");
        // A reporter that outlives the watcher does not keep its thread alive.
        drop(watcher);
        reporter.report(TerminalBackground::Dark);
    }
}
