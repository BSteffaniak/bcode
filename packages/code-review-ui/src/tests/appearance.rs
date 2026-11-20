//! The appearance picker: opened from home and the palette, each row previewed on the whole
//! app, `esc` restoring, `enter` saving the review's own choice (or following Bcode again)
//! without the watcher bringing the previous appearance back, the contrast rows (live preview,
//! reset, per-theme saving shared with Bcode, mouse) and tiny terminals.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bcode_config_layers::user_state::SavedAppAppearance;
use bcode_tui_theme::appearance::{
    AppearanceResolution, AppearanceSources, SavedAppearance, ThemePreferences,
};
use bmux_tui::capabilities::TerminalBackground;

use super::app::{fixture, open};
use crate::app::ReviewApp;
use crate::appearance::AppearanceControl;
use crate::config::APPEARANCE_APP;
use crate::entry::Entry;
use crate::testing::backend::TestBackend;
use crate::testing::harness::{Scenario, Screen};
use crate::theme::HostTheme;

/// The state file and the themes the control presented, newest last.
struct Appearance {
    _dir: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    presented: Arc<Mutex<Vec<HostTheme>>>,
    control: Arc<AppearanceControl>,
    configured: ThemePreferences,
    profile_pinned_by: Option<String>,
}

impl Appearance {
    /// Bcode's saved palette is nord; the review has no choice of its own.
    fn new() -> Self {
        Self::configured(ThemePreferences::default(), None)
    }

    /// As [`Self::new`], with the review's configured selection `configured`, whose profile
    /// `profile_pinned_by` pins when set.
    fn configured(configured: ThemePreferences, profile_pinned_by: Option<String>) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let state = root.join("tui.toml");
        std::fs::write(
            &state,
            "[theme]\nname = \"nord\"\nprofile = \"structured\"\n",
        )
        .expect("state");
        let presented = Arc::new(Mutex::new(Vec::new()));
        let control = Arc::new(AppearanceControl::new(
            resolve(&root, &state, &configured, profile_pinned_by.clone()),
            {
                let presented = Arc::clone(&presented);
                Box::new(move |resolution: &AppearanceResolution| {
                    let (theme, _) = HostTheme::from_resolution(resolution);
                    presented.lock().expect("presented").push(theme);
                })
            },
            state.clone(),
        ));
        Self {
            _dir: dir,
            root,
            state,
            presented,
            control,
            configured,
            profile_pinned_by,
        }
    }

    /// The id of the theme presented last.
    fn presented(&self) -> Option<String> {
        self.presented
            .lock()
            .expect("presented")
            .last()
            .map(|theme| theme.id.clone())
    }

    /// The fingerprint of the theme presented last; it changes with the contrast.
    fn fingerprint(&self) -> Option<u64> {
        self.presented
            .lock()
            .expect("presented")
            .last()
            .map(|theme| theme.fingerprint)
    }

    /// The `[theme.overrides]` table saved in the state file.
    fn saved_overrides(&self) -> Option<toml::Value> {
        let text = std::fs::read_to_string(&self.state).expect("state");
        let document: toml::Table = text.parse().expect("toml");
        document.get("theme")?.get("overrides").cloned()
    }

    fn presentations(&self) -> usize {
        self.presented.lock().expect("presented").len()
    }

    fn saved(&self) -> SavedAppAppearance {
        SavedAppAppearance::load(&self.state, APPEARANCE_APP).expect("state")
    }

    /// What the appearance watcher resolves from the state file now.
    fn watched(&self) -> AppearanceResolution {
        resolve(
            &self.root,
            &self.state,
            &self.configured,
            self.profile_pinned_by.clone(),
        )
    }
}

fn resolve(
    root: &Path,
    state: &Path,
    configured: &ThemePreferences,
    profile_pinned_by: Option<String>,
) -> AppearanceResolution {
    let saved = SavedAppAppearance::load(state, APPEARANCE_APP).expect("state");
    // Contrast saved per theme refines the configured values, as configuration loading does.
    let mut configured = configured.clone();
    configured.contrast.apply_saved(
        &bcode_config_layers::user_state::load_saved_theme_overrides(state).expect("state"),
    );
    let convert = |saved: &bcode_config_layers::user_state::SavedTuiAppearance| SavedAppearance {
        theme: saved.theme.clone(),
        profile: saved.profile.clone(),
    };
    AppearanceSources {
        saved: convert(&saved.bcode),
        app_saved: saved.app.as_ref().map(convert),
        configured,
        user_config_dir: root.to_path_buf(),
        project_root: root.to_path_buf(),
        profile_pinned_by,
        ..AppearanceSources::default()
    }
    .resolve(TerminalBackground::Unknown)
}

fn home(appearance: &Appearance) -> Scenario<ReviewApp> {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let (mut scenario, _) = open(&fixture, &backend, Entry::Home, 100, 30);
    scenario.set_appearance(Arc::clone(&appearance.control));
    scenario
}

/// Render with the theme presented last, as the standalone app restyles from its host.
fn render(scenario: &mut Scenario<ReviewApp>, appearance: &Appearance) -> Screen {
    let styles = appearance
        .presented
        .lock()
        .expect("presented")
        .last()
        .map(|theme| theme.styles);
    scenario.set_theme(styles.as_ref());
    scenario.render()
}

/// Move the cursor down until it previews `palette`.
fn preview(scenario: &mut Scenario<ReviewApp>, appearance: &Appearance, palette: &str) {
    for _ in 0..64 {
        if appearance.presented().as_deref() == Some(palette) {
            return;
        }
        scenario.key("down");
    }
    panic!("{palette} was never previewed");
}

#[test]
fn moving_the_cursor_restyles_the_app_and_esc_restores_it() {
    let appearance = Appearance::new();
    let mut scenario = home(&appearance);
    let before = render(&mut scenario, &appearance);
    let canvas = before.style_at(0, 29);

    scenario.key("ctrl+t");
    let picker = scenario
        .surface()
        .appearance_picker()
        .expect("ctrl+t opens the picker on home");
    assert!(picker.picker().following_bcode());
    let screen = render(&mut scenario, &appearance);
    for text in ["Review appearance", "Follow Bcode", "Palette"] {
        assert!(screen.contains(text), "{text}:\n{}", screen.text());
    }
    assert!(screen.contains("review follows Bcode's appearance"));

    preview(&mut scenario, &appearance, "dracula");
    let previewed = render(&mut scenario, &appearance);
    assert_ne!(
        previewed.style_at(0, 29),
        canvas,
        "the whole window takes the previewed palette"
    );
    assert!(previewed.contains("review keeps its own appearance"));
    // The watcher resolves meanwhile: the preview stays.
    let shown = appearance.presentations();
    appearance.control.offer(appearance.watched());
    assert_eq!(appearance.presentations(), shown);

    scenario.key("esc");
    assert!(scenario.surface().appearance_picker().is_none());
    assert_eq!(appearance.presented().as_deref(), Some("nord"));
    assert_eq!(appearance.saved().app, None, "cancel saves nothing");
}

#[test]
fn enter_saves_the_reviews_own_choice_and_the_watcher_does_not_revert_it() {
    let appearance = Appearance::new();
    let mut scenario = home(&appearance);
    // A stale resolution from before the save, as a watcher tick that read the old file.
    let stale = appearance.watched();

    scenario.key("ctrl+t");
    preview(&mut scenario, &appearance, "dracula");
    scenario.key("enter");
    assert!(scenario.surface().appearance_picker().is_none());
    let saved = appearance.saved();
    assert_eq!(
        saved.app.as_ref().and_then(|app| app.theme.as_deref()),
        Some("dracula")
    );
    assert_eq!(
        saved.bcode.theme.as_deref(),
        Some("nord"),
        "Bcode's choice is untouched"
    );
    assert_eq!(appearance.presented().as_deref(), Some("dracula"));

    let shown = appearance.presentations();
    appearance.control.offer(stale);
    assert_eq!(appearance.presentations(), shown, "no flicker back");
    appearance.control.offer(appearance.watched());
    assert_eq!(appearance.presented().as_deref(), Some("dracula"));

    // Bcode's choice changing elsewhere leaves the review's own choice in place.
    bcode_config_layers::user_state::save_tui_appearance(
        &appearance.state,
        &bcode_config_layers::user_state::SavedTuiAppearance {
            theme: Some("catppuccin-mocha".to_owned()),
            profile: None,
        },
    )
    .expect("bcode's choice");
    appearance.control.offer(appearance.watched());
    assert_eq!(appearance.presented().as_deref(), Some("dracula"));

    // Following Bcode again clears the review's choice and takes Bcode's, live.
    scenario.key("ctrl+t");
    assert!(
        !scenario
            .surface()
            .appearance_picker()
            .expect("open")
            .picker()
            .following_bcode()
    );
    scenario.key("f");
    assert_eq!(appearance.presented().as_deref(), Some("catppuccin-mocha"));
    scenario.key("enter");
    assert_eq!(appearance.saved().app, None);
    bcode_config_layers::user_state::save_tui_appearance(
        &appearance.state,
        &bcode_config_layers::user_state::SavedTuiAppearance {
            theme: Some("nord".to_owned()),
            profile: None,
        },
    )
    .expect("bcode's choice");
    appearance.control.offer(appearance.watched());
    assert_eq!(appearance.presented().as_deref(), Some("nord"));
}

#[test]
fn the_review_palette_opens_the_picker() {
    let appearance = Appearance::new();
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let entry = Entry::Sources {
        title: None,
        sources: vec![bcode_code_review_models::ReviewSourceKind::LastCommit],
    };
    let (mut scenario, _) = open(&fixture, &backend, entry, 100, 30);
    scenario.set_appearance(Arc::clone(&appearance.control));
    scenario.key("ctrl+p");
    for character in "appearance".chars() {
        scenario.key(&character.to_string());
    }
    let screen = scenario.render();
    assert!(
        screen.contains("Appearance: the review's palette and profile"),
        "{}",
        screen.text()
    );
    scenario.key("enter");
    assert!(scenario.surface().appearance_picker().is_some());
    scenario.key("esc");
    assert!(scenario.surface().appearance_picker().is_none());
    scenario.key("ctrl+t");
    assert!(scenario.surface().appearance_picker().is_some());
}

#[test]
fn a_host_owned_theme_says_so() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let (mut scenario, _) = open(&fixture, &backend, Entry::Home, 100, 30);
    scenario.key("ctrl+t");
    assert!(scenario.surface().appearance_picker().is_none());
    let screen = scenario.render();
    assert!(
        screen.contains("the theme is set by the program hosting the review"),
        "{}",
        screen.text()
    );
}

#[test]
fn tiny_terminals_keep_the_picker_inside_the_screen() {
    let appearance = Appearance::new();
    let mut scenario = home(&appearance);
    scenario.key("ctrl+t");
    for (width, height) in [(24, 8), (12, 4), (3, 2), (1, 1)] {
        scenario.resize(width, height);
        let screen = render(&mut scenario, &appearance);
        let area = bmux_tui::geometry::Rect::new(0, 0, width, height);
        for hit in screen.hits() {
            assert!(
                hit.area.right() <= area.right() && hit.area.bottom() <= area.bottom(),
                "{width}x{height}: {hit:?} outside the screen"
            );
        }
        // Keys still work at every size.
        scenario.key("down");
    }
    scenario.resize(80, 24);
    let screen = render(&mut scenario, &appearance);
    assert!(screen.contains("Review appearance"), "{}", screen.text());
    scenario.key("esc");
    assert_eq!(appearance.presented().as_deref(), Some("nord"));
}

#[test]
fn a_profile_pinned_by_the_review_config_is_stated_and_wins_over_bcodes() {
    let pin = "plugins.config.\"bcode.code_review\".theme.profile";
    let appearance = Appearance::configured(
        ThemePreferences {
            profile: Some("bare".to_owned()),
            ..ThemePreferences::default()
        },
        Some(pin.to_owned()),
    );
    // Bcode saved "structured"; the review's own profile setting wins.
    let current = appearance.control.current();
    assert_eq!(current.preferences.name, "nord");
    assert_eq!(current.preferences.profile.as_deref(), Some("bare"));

    let mut scenario = home(&appearance);
    scenario.resize(60, 30);
    scenario.key("ctrl+t");
    let screen = render(&mut scenario, &appearance);
    // The note wraps inside the picker instead of being cut off.
    for part in [
        "Profile pinned by",
        &format!("{pin};"),
        "only the palette applies.",
    ] {
        assert!(screen.contains(part), "{part}:\n{}", screen.text());
    }

    preview(&mut scenario, &appearance, "dracula");
    assert_eq!(
        appearance.control.current().preferences.profile.as_deref(),
        Some("bare")
    );
    scenario.key("enter");
    let applied = appearance.control.current();
    assert_eq!(applied.preferences.name, "dracula");
    assert_eq!(applied.preferences.profile.as_deref(), Some("bare"));
    assert_eq!(
        appearance.watched().preferences.profile.as_deref(),
        Some("bare")
    );
}

/// The painted area of the contrast row whose hit region ends with `suffix`.
fn contrast_row(screen: &Screen, suffix: &str) -> Option<bmux_tui::geometry::Rect> {
    screen
        .hits()
        .iter()
        .find(|hit| hit.id.as_str() == format!("review.appearance.contrast.{suffix}"))
        .map(|hit| hit.area)
}

#[test]
fn contrast_previews_live_resets_and_enter_saves_only_the_edited_value_for_the_theme() {
    let appearance = Appearance::new();
    let mut scenario = home(&appearance);
    let exact = appearance.control.current();
    let exact_fingerprint = exact.resolved.as_ref().expect("nord").fingerprint;
    let stale = appearance.watched();
    scenario.key("ctrl+t");
    let screen = render(&mut scenario, &appearance);
    for text in [
        "Contrast",
        "1.00×",
        "Minimum",
        "off   3.0   4.5   7.0",
        "tab contrast & color",
    ] {
        assert!(screen.contains(text), "{text}:\n{}", screen.text());
    }

    // Tab reaches the slider of the palette the follow row presents (Bcode's nord).
    scenario.key("tab");
    for _ in 0..8 {
        scenario.key("right");
    }
    let screen = render(&mut scenario, &appearance);
    assert!(screen.contains("1.40×"), "{}", screen.text());
    assert_eq!(appearance.presented().as_deref(), Some("nord"));
    let raised = appearance.fingerprint();
    assert_ne!(
        raised,
        Some(exact_fingerprint),
        "the app previews the contrast live"
    );
    // `0` resets to the theme's exact colors; the edit is previewed again afterwards.
    scenario.key("0");
    assert_eq!(appearance.fingerprint(), Some(exact_fingerprint));
    for _ in 0..8 {
        scenario.key("right");
    }
    assert_eq!(appearance.fingerprint(), raised);

    scenario.key("enter");
    assert!(scenario.surface().appearance_picker().is_none());
    let overrides = appearance.saved_overrides().expect("overrides saved");
    let themes = overrides.as_table().expect("table");
    assert_eq!(
        themes.keys().collect::<Vec<_>>(),
        ["nord"],
        "only the edited theme"
    );
    let nord = themes["nord"].as_table().expect("nord");
    assert_eq!(
        nord.keys().collect::<Vec<_>>(),
        ["contrast"],
        "only the edited value"
    );
    assert_eq!(nord["contrast"].as_float(), Some(1.4));
    assert_eq!(
        appearance.saved().app,
        None,
        "the app's own choice is untouched"
    );
    assert_eq!(appearance.saved().bcode.theme.as_deref(), Some("nord"));
    assert_eq!(appearance.fingerprint(), raised);
    // A watcher tick that read the file before the save does not bring the old contrast back,
    // and one after it resolves the saved contrast.
    let shown = appearance.presentations();
    appearance.control.offer(stale);
    assert_eq!(appearance.presentations(), shown);
    let watched = appearance.watched();
    assert_eq!(
        Some(watched.resolved.as_ref().expect("nord").fingerprint),
        raised
    );
    appearance.control.offer(watched);
    assert_eq!(appearance.fingerprint(), raised);

    // The minimum cycles off, 3.0, 4.5, 7.0; shift+tab reaches the last row (background
    // tint) and up moves back to it. Saving keeps the saved factor.
    scenario.key("ctrl+t");
    let screen = render(&mut scenario, &appearance);
    assert!(screen.contains("1.40×"), "{}", screen.text());
    scenario.key("shift+tab");
    let screen = render(&mut scenario, &appearance);
    assert!(screen.contains("› Background tint"), "{}", screen.text());
    for _ in 0..3 {
        scenario.key("up");
    }
    scenario.key("right");
    scenario.key("right");
    let screen = render(&mut scenario, &appearance);
    assert!(screen.contains("› Minimum"), "{}", screen.text());
    // Vibrance is the row below; its edit previews and saves like the others.
    scenario.key("down");
    let before = appearance.fingerprint();
    scenario.key("right");
    assert_ne!(appearance.fingerprint(), before, "vibrance previews live");
    let screen = render(&mut scenario, &appearance);
    assert!(
        screen.contains("› Vibrance") && screen.contains("+0.05"),
        "{}",
        screen.text()
    );
    scenario.key("enter");
    let overrides = appearance.saved_overrides().expect("overrides saved");
    assert_eq!(overrides["nord"]["contrast"].as_float(), Some(1.4));
    assert_eq!(overrides["nord"]["minimum_contrast"].as_float(), Some(4.5));
    assert_eq!(overrides["nord"]["vibrance"].as_float(), Some(0.05));
    assert!(overrides["nord"].get("richness").is_none());
}

#[test]
fn esc_restores_the_contrast_and_saves_nothing() {
    let appearance = Appearance::new();
    let mut scenario = home(&appearance);
    let exact = appearance.control.current();
    let exact_fingerprint = exact.resolved.as_ref().expect("nord").fingerprint;
    scenario.key("ctrl+t");
    preview(&mut scenario, &appearance, "dracula");
    let dracula = appearance.fingerprint();
    scenario.key("tab");
    scenario.key("left");
    scenario.key("down");
    scenario.key("right");
    assert_ne!(appearance.fingerprint(), dracula);
    assert_eq!(appearance.presented().as_deref(), Some("dracula"));
    scenario.key("esc");
    assert_eq!(appearance.fingerprint(), Some(exact_fingerprint));
    assert_eq!(appearance.saved_overrides(), None);
    assert_eq!(appearance.saved().app, None);
}

#[test]
fn a_click_focuses_a_contrast_row_and_the_wheel_steps_it() {
    let appearance = Appearance::new();
    let mut scenario = home(&appearance);
    scenario.key("ctrl+t");
    let screen = render(&mut scenario, &appearance);
    let factor = contrast_row(&screen, "contrast").expect("slider painted");
    let minimum = contrast_row(&screen, "minimum").expect("minimum painted");
    assert!(screen.row_text(factor.y).contains("Contrast"));
    assert!(screen.row_text(minimum.y).contains("Minimum"));

    scenario.mouse(
        bmux_tui::event::MouseEventKind::ScrollUp,
        factor.right() - 1,
        factor.y,
    );
    let screen = render(&mut scenario, &appearance);
    assert!(screen.contains("1.05×"), "{}", screen.text());
    scenario.click(minimum.x, minimum.y);
    let screen = render(&mut scenario, &appearance);
    assert!(screen.contains("› Minimum"), "{}", screen.text());
    scenario.key("right");
    let screen = render(&mut scenario, &appearance);
    assert!(screen.row_text(minimum.y).contains("3.0"));
    scenario.key("esc");
    assert_eq!(appearance.saved_overrides(), None);
}

#[test]
fn the_contrast_rows_drop_labels_when_narrow_and_go_when_short() {
    let appearance = Appearance::new();
    let mut scenario = home(&appearance);
    scenario.key("ctrl+t");
    scenario.resize(30, 24);
    let screen = render(&mut scenario, &appearance);
    let factor = contrast_row(&screen, "contrast").expect("slider painted");
    assert!(
        screen.row_text(factor.y).contains("1.00×"),
        "{}",
        screen.text()
    );
    assert!(!screen.contains("Contrast"), "{}", screen.text());
    for hit in screen.hits() {
        assert!(hit.area.right() <= 30 && hit.area.bottom() <= 24, "{hit:?}");
    }

    scenario.resize(100, 9);
    let screen = render(&mut scenario, &appearance);
    assert!(
        contrast_row(&screen, "contrast").is_none(),
        "{}",
        screen.text()
    );
    assert!(!screen.contains("Minimum"), "{}", screen.text());
    // The keys still adjust the contrast.
    let before = appearance.fingerprint();
    scenario.key("tab");
    scenario.key("right");
    assert_ne!(appearance.fingerprint(), before);
    scenario.key("esc");
}
