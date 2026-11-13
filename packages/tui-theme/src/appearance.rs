//! The effective appearance: which palette, profile, overlays and light/dark variant a Bcode
//! program presents.
//!
//! Configuration (`[tui.theme]` in `bcode.toml`) selects a palette (`name`), an optional
//! `profile`, further `overlays`, a light/dark `variant` and extra theme `paths`. The appearance
//! picker saves a palette and a profile in user state; those replace the configured ones when
//! the catalog can present them. Every Bcode program (the chat interface, the review app and the
//! git client) resolves its theme through [`resolve_appearance`] or [`apply_saved_appearance`],
//! so they all present the same theme.

use std::path::{Path, PathBuf};

use bmux_tui::capabilities::TerminalBackground;
use serde::Deserialize;

use crate::definition::{
    ResolvedThemeDefinition, ResolvedThemeVariant, ThemeCatalog, ThemeError, ThemeKind,
    ThemeSelection,
};
use crate::discovery::{ThemeDiscoveryDiagnostic, default_theme_roots, discover_themes};

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
    /// Variant resolved for the terminal.
    pub variant: ResolvedThemeVariant,
    /// The resolved theme, or why [`Self::preferences`] do not resolve.
    pub resolved: Result<ResolvedThemeDefinition, ThemeError>,
    /// The discovered catalog the theme was resolved from.
    pub catalog: ThemeCatalog,
    /// Saved choices that could not apply.
    pub notices: Vec<AppearanceNotice>,
    /// Theme files skipped during discovery.
    pub diagnostics: Vec<ThemeDiscoveryDiagnostic>,
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
    let (catalog, diagnostics) = match discover_themes(&roots) {
        Ok(discovered) => (discovered.catalog, discovered.diagnostics),
        Err(_) => (ThemeCatalog::bundled().unwrap_or_default(), Vec::new()),
    };
    let variant = configured.variant.resolve(background);
    let (preferences, notices) = apply_saved_appearance(configured, saved, &catalog, variant);
    let resolved = catalog.resolve(&preferences.selection(variant));
    AppearanceResolution {
        preferences,
        variant,
        resolved,
        catalog,
        notices,
        diagnostics,
    }
}

/// Everything an appearance resolves from, as a program reads it.
#[derive(Debug, Clone, Default)]
pub struct AppearanceSources {
    /// Configured `[tui.theme]` selection.
    pub configured: ThemePreferences,
    /// Choices saved in user state.
    pub saved: SavedAppearance,
    /// Files whose change may change [`Self::configured`] or [`Self::saved`]: configuration
    /// candidates (present or not) and the user state file.
    pub watched_files: Vec<PathBuf>,
    /// Bcode's user configuration directory, which holds user themes.
    pub user_config_dir: PathBuf,
    /// Project whose `.bcode/themes` holds project themes.
    pub project_root: PathBuf,
}

impl AppearanceSources {
    /// Resolve these sources for a terminal with `background` ([`resolve_appearance`]).
    #[must_use]
    pub fn resolve(&self, background: TerminalBackground) -> AppearanceResolution {
        resolve_appearance(
            &self.user_config_dir,
            &self.project_root,
            &self.configured,
            &self.saved,
            background,
        )
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
