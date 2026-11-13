//! The appearance picker: opened from home and the palette, each row previewed on the whole
//! app, `esc` restoring, `enter` saving the review's own choice (or following Bcode again)
//! without the watcher bringing the previous appearance back, and tiny terminals.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bcode_config_layers::user_state::SavedAppAppearance;
use bcode_tui_theme::appearance::{AppearanceResolution, AppearanceSources, SavedAppearance};
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
}

impl Appearance {
    /// Bcode's saved palette is nord; the review has no choice of its own.
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let state = root.join("tui.toml");
        std::fs::write(&state, "[theme]\nname = \"nord\"\n").expect("state");
        let presented = Arc::new(Mutex::new(Vec::new()));
        let control = Arc::new(AppearanceControl::new(
            resolve(&root, &state),
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

    fn presentations(&self) -> usize {
        self.presented.lock().expect("presented").len()
    }

    fn saved(&self) -> SavedAppAppearance {
        SavedAppAppearance::load(&self.state, APPEARANCE_APP).expect("state")
    }

    /// What the appearance watcher resolves from the state file now.
    fn watched(&self) -> AppearanceResolution {
        resolve(&self.root, &self.state)
    }
}

fn resolve(root: &Path, state: &Path) -> AppearanceResolution {
    let saved = SavedAppAppearance::load(state, APPEARANCE_APP).expect("state");
    let convert = |saved: &bcode_config_layers::user_state::SavedTuiAppearance| SavedAppearance {
        theme: saved.theme.clone(),
        profile: saved.profile.clone(),
    };
    AppearanceSources {
        saved: convert(&saved.bcode),
        app_saved: saved.app.as_ref().map(convert),
        user_config_dir: root.to_path_buf(),
        project_root: root.to_path_buf(),
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
