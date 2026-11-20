//! The git UI's appearance picker: its own palette and profile, or bcode's.
//!
//! The picker is a popup menu over the rows of [`AppPicker`] (Follow Bcode, the palettes, the
//! profiles) with the shared contrast rows below them
//! ([`bcode_tui_theme::picker::contrast::view::ContrastRowsComponent`]). Moving the cursor
//! restyles the whole UI at once; `tab` reaches the contrast slider, the minimum contrast and
//! the vibrance, richness and background-tint sliders of the previewed palette, where `←`/`→` change the value (previewed at once) and `0` resets it;
//! a click focuses a contrast row and the wheel steps it. Confirming saves the git UI's own
//! choice in user state (`[apps.git.theme]`), or removes it on the Follow Bcode row so the UI
//! follows bcode's again, and saves the contrast edits per theme
//! (`[theme.overrides.<theme-id>]`, shared with bcode and the review); closing restores what was
//! presented. bcode's own choice is never written here. The appearance watcher keeps running,
//! but its results wait while the picker previews
//! ([`bcode_tui_theme::picker::LiveAppearance`]).

use bcode_config_layers::user_state::{
    SavedTuiAppearance, save_app_tui_appearance, update_theme_override,
};
use bcode_tui_theme::appearance::AppearanceResolution;
use bcode_tui_theme::picker::app::{AppContrastOutcome, AppPicker, AppPickerRowKind};
use bcode_tui_theme::picker::contrast::view::{
    ContrastPointer, ContrastRowsComponent, ContrastRowsStyles, hit_row, pointer_action,
};
use bcode_tui_theme::picker::contrast::{ContrastKey, ContrastRow};
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::component::Component;
use bmux_tui::event::MouseEvent;
use bmux_tui::geometry::Point;
use bmux_tui::text::{Line, Span};
use bmux_tui_runtime::Update;

use crate::app::{App, Msg};
use crate::config::APPEARANCE_APP;
use crate::keymap::Action;
use crate::popup::{Menu, MenuItem, Popup, Then};
use crate::theme::GitTheme;

impl App {
    /// Open the appearance picker, or say why the theme cannot be chosen here.
    pub(crate) fn appearance_picker(&mut self) -> Update<Msg> {
        let Some(inputs) = &self.appearance else {
            self.toast("The theme is set by the program hosting the git UI", false);
            return Update::redraw();
        };
        let picker = AppPicker::open("git", &inputs.live, |path| path.display().to_string());
        let mut menu = Menu::new("Appearance · git", Vec::new());
        fill(&mut menu, picker, None);
        self.open(Popup::Menu(menu))
    }

    /// The watcher reported a new appearance: present the latest one unless the picker
    /// previews.
    pub(crate) fn present_watched_appearance(&mut self) -> Update<Msg> {
        match &self.appearance {
            Some(inputs) if !inputs.live.is_previewing() => {
                let latest = inputs.live.current();
                self.present_appearance(&latest)
            }
            _ => Update::none(),
        }
    }

    fn present_appearance(&mut self, resolution: &AppearanceResolution) -> Update<Msg> {
        let (theme, _) = GitTheme::from_resolution(resolution);
        self.set_theme(theme)
    }

    /// A pointer event over a painted contrast row of the open picker: a press focuses the
    /// row, a wheel notch steps it. `None` when the event is not over one.
    pub(crate) fn appearance_pointer(&mut self, mouse: MouseEvent) -> Option<Update<Msg>> {
        if !matches!(self.popups.last(), Some(Popup::Menu(menu)) if menu.appearance.is_some()) {
            return None;
        }
        let row = {
            let hits = self.hits.borrow();
            hits.hit_test(Point::new(mouse.position.x, mouse.position.y))
                .and_then(|hit| hit_row(CONTRAST_ID, hit.region.id.as_str()))
        }?;
        let action = pointer_action(mouse.kind, row)?;
        let live = std::sync::Arc::clone(&self.appearance.as_ref()?.live);
        let Some(Popup::Menu(mut menu)) = self.popups.pop() else {
            return None;
        };
        let mut picker = *menu.appearance.take()?;
        let update = match action {
            ContrastPointer::Focus(row) => {
                picker.focus_contrast(Some(row));
                Update::none()
            }
            ContrastPointer::Step(row, raise) => picker
                .step_contrast(row, raise, &live)
                .map_or_else(Update::none, |preview| self.present_appearance(&preview)),
        };
        fill(&mut menu, picker, None);
        self.popups.push(Popup::Menu(menu));
        Some(Update::redraw().merge(update))
    }
}

/// Layout id of the picker body with the contrast rows (and prefix of the rows' hit regions).
const CONTRAST_ID: &str = "appearance.contrast";
/// Rows the picker's list keeps above the contrast rows.
const CONTRAST_MIN_BODY_HEIGHT: u64 = 3;
/// Rows the contrast rows add to the picker.
// The shared rows are a handful, so the count fits.
#[allow(clippy::cast_possible_truncation)]
pub const CONTRAST_ROWS: u16 = ContrastRow::ALL.len() as u16;

/// The picker body `body` with the contrast rows of `picker` below it.
pub fn with_contrast<'a>(
    body: impl Component + 'a,
    picker: &'a AppPicker,
    theme: &GitTheme,
) -> ContrastRowsComponent<'a> {
    ContrastRowsComponent::new(
        CONTRAST_ID,
        body,
        picker.contrast(),
        ContrastRowsStyles {
            label: theme.muted,
            focused: theme.accent,
            value: theme.text,
            muted: theme.muted,
            selected: theme.cursor,
            background: theme.overlay,
        },
    )
    .min_body_height(CONTRAST_MIN_BODY_HEIGHT)
}

/// Rebuild `menu` from `picker`: one item per row, the cursor on the picker's row, and the
/// active mode (and `error`) above the items.
fn fill(menu: &mut Menu, picker: AppPicker, error: Option<String>) {
    menu.items = picker
        .rows()
        .into_iter()
        .map(|row| MenuItem::plain(row.label, row.detail.unwrap_or_default(), Then::Nothing))
        .collect();
    menu.selected = picker.cursor();
    let mut state = menu.list.get();
    state.set_focused(Some(menu.selected));
    state.set_selected(Some(menu.selected));
    menu.list.set(state);
    let mut notes = vec![Line::from(picker.mode_label())];
    if let Some(note) = picker.pinned_note() {
        notes.push(Line::from(note));
    }
    if let Some(error) = error {
        notes.push(Line::from(error));
    }
    notes.push(Line::from(
        "enter apply · tab contrast & color · ←/→ adjust · 0 reset · f follow Bcode · esc cancel",
    ));
    menu.notes = notes;
    menu.appearance = Some(Box::new(picker));
}

/// The rows of the open picker as list lines: headings in the title style, the chosen row of
/// each axis marked.
#[must_use]
pub fn lines(picker: &AppPicker, theme: &GitTheme) -> Vec<Line> {
    picker
        .rows()
        .into_iter()
        .map(|row| {
            if matches!(row.kind, AppPickerRowKind::Heading(_)) {
                Line::from_spans(vec![Span::styled(row.label, theme.title_focused)])
            } else {
                let mut spans = vec![
                    Span::styled(if row.chosen { "● " } else { "  " }, theme.accent),
                    Span::styled(row.label, theme.text),
                ];
                if let Some(detail) = row.detail {
                    spans.push(Span::styled(format!("   {detail}"), theme.muted));
                }
                Line::from_spans(spans)
            }
        })
        .collect()
}

/// Handle a key in the open picker (`menu` was taken off the popup stack).
pub fn key(
    app: &mut App,
    mut menu: Menu,
    action: Option<Action>,
    stroke: KeyStroke,
) -> Update<Msg> {
    let Some(mut picker) = menu.appearance.take().map(|picker| *picker) else {
        return Update::redraw();
    };
    let Some(live) = app
        .appearance
        .as_ref()
        .map(|inputs| std::sync::Arc::clone(&inputs.live))
    else {
        return Update::redraw();
    };
    if let Some(key) = contrast_key(&picker, action, stroke) {
        let update = match picker.contrast_key(key, &live) {
            AppContrastOutcome::Preview(preview) => app.present_appearance(&preview),
            AppContrastOutcome::Moved | AppContrastOutcome::Ignored => Update::none(),
        };
        fill(&mut menu, picker, None);
        app.popups.push(Popup::Menu(menu));
        return Update::redraw().merge(update);
    }
    let follow =
        bcode_keymap::controls::LiteralText::from_stroke(stroke).is_some_and(|text| text.0 == 'f');
    let preview = match action {
        Some(Action::MenuClose) => {
            let restored = picker.cancel(&live, |_| {});
            return Update::redraw().merge(app.present_appearance(&restored));
        }
        Some(Action::MenuConfirm) => {
            let state_file = app
                .appearance
                .as_ref()
                .map(|inputs| inputs.state_file.clone())
                .unwrap_or_default();
            let saved = picker.apply(
                &live,
                |edits| {
                    edits.iter().try_for_each(|(theme, edits)| {
                        update_theme_override(&state_file, theme, |prior| edits.saved_over(prior))
                    })
                },
                |own| {
                    let own = own.map(|own| SavedTuiAppearance {
                        theme: own.theme.clone(),
                        profile: own.profile.clone(),
                    });
                    save_app_tui_appearance(&state_file, APPEARANCE_APP, own.as_ref())
                },
                |_| {},
            );
            return match saved {
                Ok(applied) => Update::redraw().merge(app.present_appearance(&applied)),
                Err(error) => {
                    let current = live.current();
                    let update = app.present_appearance(&current);
                    let reopened = AppPicker::open("git", &live, |path| path.display().to_string());
                    fill(
                        &mut menu,
                        reopened,
                        Some(format!("could not save the appearance: {error}")),
                    );
                    app.popups.push(Popup::Menu(menu));
                    Update::redraw().merge(update)
                }
            };
        }
        Some(Action::MenuUp) => picker.step(-1, &live),
        Some(Action::MenuDown) => picker.step(1, &live),
        _ if follow => picker.select(0, &live),
        _ => None,
    };
    let update = preview.map_or_else(Update::none, |preview| app.present_appearance(&preview));
    fill(&mut menu, picker, None);
    app.popups.push(Popup::Menu(menu));
    Update::redraw().merge(update)
}

/// The contrast key `stroke` (resolved to `action`) stands for: `tab` and `shift+tab` move
/// between the rows and the contrast rows; while a contrast row has focus, the horizontal keys
/// change it, `0` resets it and the vertical keys move between the contrast rows.
fn contrast_key(
    picker: &AppPicker,
    action: Option<Action>,
    stroke: KeyStroke,
) -> Option<ContrastKey> {
    let shift_only = bmux_keyboard::Modifiers {
        shift: true,
        ..bmux_keyboard::Modifiers::NONE
    };
    match (stroke.key, stroke.modifiers) {
        (KeyCode::Tab, modifiers) if modifiers.is_empty() => return Some(ContrastKey::Next),
        (KeyCode::Tab, modifiers) if modifiers == shift_only => return Some(ContrastKey::Previous),
        _ => {}
    }
    picker.contrast().focus()?;
    match action {
        Some(Action::MenuUp) => return Some(ContrastKey::Up),
        Some(Action::MenuDown) => return Some(ContrastKey::Down),
        Some(Action::MenuClose | Action::MenuConfirm) => return None,
        _ => {}
    }
    if !stroke.modifiers.is_empty() {
        return None;
    }
    match stroke.key {
        KeyCode::Left | KeyCode::Char('h') => Some(ContrastKey::Lower),
        KeyCode::Right | KeyCode::Char('l') => Some(ContrastKey::Raise),
        KeyCode::Char('0') => Some(ContrastKey::Reset),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bcode_config_layers::user_state::{SavedAppAppearance, save_tui_appearance};
    use bmux_tui::event::Event;
    use bmux_tui_runtime::{Program, RuntimeEvent};

    use super::*;
    use crate::config::LoadedConfig;

    struct Harness {
        _repo: tempfile::TempDir,
        dir: tempfile::TempDir,
        app: App,
    }

    impl Harness {
        /// bcode's saved palette is nord; the git UI has no choice of its own.
        fn new() -> Self {
            Self::pinned(None)
        }

        /// As [`Self::new`], with the palette pinned by `theme_pinned_by` when set.
        fn pinned(theme_pinned_by: Option<&str>) -> Self {
            let repo = tempfile::tempdir().expect("repo");
            let status = std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(repo.path())
                .status()
                .expect("git");
            assert!(status.success());
            let dir = tempfile::tempdir().expect("state");
            let state = dir.path().join("tui.toml");
            save_tui_appearance(
                &state,
                &SavedTuiAppearance {
                    theme: Some("nord".to_owned()),
                    profile: None,
                },
            )
            .expect("state");
            let mut config = LoadedConfig {
                state_file: state,
                config_dir: dir.path().to_path_buf(),
                theme_pinned_by: theme_pinned_by.map(str::to_owned),
                ..LoadedConfig::default()
            };
            config.saved_appearance.theme = Some("nord".to_owned());
            config.ui.refresh.watch = false;
            config.ui.refresh.poll_seconds = 0;
            let mut app = crate::build_app(crate::RunOptions {
                path: repo.path().to_path_buf(),
                config,
                host: Arc::new(crate::host::StandaloneHost),
                commit_message: None,
                state_dir: Some(dir.path().to_path_buf()),
                pull_requests: None,
                daemon: None,
            })
            .expect("app");
            app.set_inline(true);
            Self {
                _repo: repo,
                dir,
                app,
            }
        }

        fn key(&mut self, key: &str) {
            let stroke = bmux_keyboard::parse_key_stroke(key).expect("stroke");
            self.app
                .update(RuntimeEvent::Terminal(Event::Key(stroke)))
                .expect("update");
        }

        fn live(&self) -> Arc<bcode_tui_theme::picker::LiveAppearance> {
            Arc::clone(&self.app.appearance.as_ref().expect("appearance").live)
        }

        fn picker(&self) -> Option<&AppPicker> {
            match self.app.popups.last() {
                Some(Popup::Menu(menu)) => menu.appearance.as_deref(),
                _ => None,
            }
        }

        fn theme_of(&self, palette: &str) -> GitTheme {
            let live = self.live();
            let current = live.current();
            let mut sources = bcode_tui_theme::appearance::AppearanceSources {
                configured: current.configured,
                user_config_dir: self.dir.path().to_path_buf(),
                project_root: self.dir.path().to_path_buf(),
                ..Default::default()
            };
            sources.saved.theme = Some(palette.to_owned());
            GitTheme::from_resolution(
                &sources.resolve(bmux_tui::capabilities::TerminalBackground::Unknown),
            )
            .0
        }

        fn saved(&self) -> SavedAppAppearance {
            SavedAppAppearance::load(&self.dir.path().join("tui.toml"), APPEARANCE_APP)
                .expect("state")
        }

        /// The watcher read the state file now and reported it.
        fn watch(&mut self) {
            let loaded = self.loaded();
            self.offer(&loaded);
        }

        /// The configuration the watcher loads from the state file now, saved contrast folded
        /// in as configuration loading does.
        fn loaded(&self) -> LoadedConfig {
            let mut loaded = LoadedConfig {
                saved_appearance: saved(&self.saved().bcode),
                app_appearance: self.saved().app.as_ref().map(saved),
                config_dir: self.dir.path().to_path_buf(),
                ..LoadedConfig::default()
            };
            loaded.theme.contrast.apply_saved(
                &bcode_config_layers::user_state::load_saved_theme_overrides(
                    &self.dir.path().join("tui.toml"),
                )
                .expect("state"),
            );
            loaded
        }

        /// The `[theme.overrides]` table saved in the state file.
        fn saved_overrides(&self) -> Option<toml::Value> {
            let text = std::fs::read_to_string(self.dir.path().join("tui.toml")).expect("state");
            let document: toml::Table = text.parse().expect("toml");
            document.get("theme")?.get("overrides").cloned()
        }

        fn screen(&mut self, width: u16, height: u16) -> String {
            crate::render_text(&mut self.app, width, height).join("\n")
        }

        /// The painted area of contrast row `suffix` (`contrast` or `minimum`).
        fn contrast_row(&self, suffix: &str) -> Option<bmux_tui::geometry::Rect> {
            self.app
                .hit_regions()
                .into_iter()
                .find(|(id, _)| *id == format!("{CONTRAST_ID}.{suffix}"))
                .map(|(_, area)| area)
        }

        fn mouse(&mut self, kind: bmux_tui::event::MouseEventKind, x: u16, y: u16) {
            self.app
                .update(RuntimeEvent::Terminal(Event::Mouse(MouseEvent::new(
                    kind,
                    Point::new(x, y),
                ))))
                .expect("update");
        }

        fn offer(&mut self, loaded: &LoadedConfig) {
            let resolution = crate::config::appearance_sources(loaded, self.dir.path())
                .resolve(bmux_tui::capabilities::TerminalBackground::Unknown);
            if self.live().offer(resolution, |_| {}) {
                self.app
                    .update(RuntimeEvent::Message(Msg::Appearance))
                    .expect("update");
            }
        }
    }

    fn saved(saved: &SavedTuiAppearance) -> bcode_tui_theme::appearance::SavedAppearance {
        bcode_tui_theme::appearance::SavedAppearance {
            theme: saved.theme.clone(),
            profile: saved.profile.clone(),
        }
    }

    fn preview(harness: &mut Harness, palette: &str) {
        let wanted = harness.theme_of(palette);
        for _ in 0..64 {
            if harness.app.theme == wanted {
                return;
            }
            harness.key("down");
        }
        panic!("{palette} was never previewed");
    }

    #[test]
    fn moving_previews_on_the_whole_ui_and_escape_restores() {
        let mut harness = Harness::new();
        let nord = harness.theme_of("nord");
        assert_eq!(harness.app.theme, nord);
        harness.key("ctrl+w");
        harness.key("t");
        assert!(harness.picker().expect("open").following_bcode());
        let screen = crate::render_text(&mut harness.app, 100, 40).join("\n");
        assert!(screen.contains("Follow Bcode"), "{screen}");
        assert!(
            screen.contains("git follows Bcode's appearance"),
            "{screen}"
        );

        preview(&mut harness, "dracula");
        // The watcher reports meanwhile: the preview stays.
        harness.watch();
        assert_eq!(harness.app.theme, harness.theme_of("dracula"));
        harness.key("escape");
        assert!(harness.picker().is_none());
        assert_eq!(harness.app.theme, nord);
        assert_eq!(harness.saved().app, None);
    }

    #[test]
    fn enter_saves_the_git_uis_own_choice_isolated_from_bcode() {
        let mut harness = Harness::new();
        let stale = LoadedConfig {
            saved_appearance: saved(&harness.saved().bcode),
            config_dir: harness.dir.path().to_path_buf(),
            ..LoadedConfig::default()
        };
        harness.key("ctrl+w");
        harness.key("t");
        preview(&mut harness, "dracula");
        harness.key("enter");
        assert!(harness.picker().is_none());
        let saved_state = harness.saved();
        assert_eq!(
            saved_state
                .app
                .as_ref()
                .and_then(|app| app.theme.as_deref()),
            Some("dracula")
        );
        assert_eq!(saved_state.bcode.theme.as_deref(), Some("nord"));
        let dracula = harness.theme_of("dracula");
        // A watcher tick that read the file before the write does not flicker back.
        harness.offer(&stale);
        assert_eq!(harness.app.theme, dracula);
        harness.watch();
        assert_eq!(harness.app.theme, dracula);
        // bcode's picker changing bcode's choice leaves the git UI's own alone.
        save_tui_appearance(
            &harness.dir.path().join("tui.toml"),
            &SavedTuiAppearance {
                theme: Some("catppuccin-mocha".to_owned()),
                profile: None,
            },
        )
        .expect("bcode's choice");
        harness.watch();
        assert_eq!(harness.app.theme, dracula);

        // Following bcode again takes bcode's choice, live.
        harness.key("ctrl+w");
        harness.key("t");
        harness.key("f");
        assert_eq!(harness.app.theme, harness.theme_of("catppuccin-mocha"));
        harness.key("enter");
        assert_eq!(harness.saved().app, None);
        save_tui_appearance(
            &harness.dir.path().join("tui.toml"),
            &SavedTuiAppearance {
                theme: Some("nord".to_owned()),
                profile: None,
            },
        )
        .expect("bcode's choice");
        harness.watch();
        assert_eq!(harness.app.theme, harness.theme_of("nord"));
    }

    #[test]
    fn tiny_terminals_render_the_picker() {
        let mut harness = Harness::new();
        harness.key("ctrl+w");
        harness.key("t");
        for (width, height) in [(30, 8), (10, 3), (1, 1)] {
            let rows = crate::render_text(&mut harness.app, width, height);
            assert_eq!(rows.len(), usize::from(height));
            harness.key("down");
        }
        harness.key("escape");
        assert_eq!(harness.app.theme, harness.theme_of("nord"));
    }

    #[test]
    fn a_long_pinned_by_note_wraps_inside_the_picker() {
        let origin = "/home/someone/.config/bcode/bcode-git.toml: theme.name";
        let mut harness = Harness::pinned(Some(origin));
        harness.key("ctrl+w");
        harness.key("t");
        assert_eq!(
            harness
                .picker()
                .expect("open")
                .pinned_by(bcode_tui_theme::picker::AppearanceAxis::Palette),
            Some(origin)
        );
        for (width, height) in [(100, 40), (60, 40)] {
            let screen = crate::render_text(&mut harness.app, width, height).join("\n");
            // Words wrap at spaces, so the origin's file and key may land on separate rows.
            for part in [
                "Palette pinned by",
                "/home/someone/.config/bcode/bcode-git.toml:",
                "theme.name;",
                "only the profile applies.",
                "Follow Bcode",
                "enter apply",
            ] {
                assert!(screen.contains(part), "{width}x{height}: {part}\n{screen}");
            }
        }
        harness.key("escape");
    }

    #[test]
    fn contrast_previews_live_resets_and_enter_saves_only_the_edited_value_for_the_theme() {
        let mut harness = Harness::new();
        let nord = harness.theme_of("nord");
        let stale = harness.loaded();
        harness.key("ctrl+w");
        harness.key("t");
        let screen = harness.screen(100, 40);
        for text in [
            "Contrast",
            "1.00×",
            "Minimum",
            "off   3.0   4.5   7.0",
            "tab contrast & color",
            "0 reset",
        ] {
            assert!(screen.contains(text), "{text}\n{screen}");
        }
        // Tab reaches the slider of the palette the follow row presents (bcode's nord).
        harness.key("tab");
        for _ in 0..8 {
            harness.key("right");
        }
        assert!(harness.screen(100, 40).contains("1.40×"));
        let raised = harness.app.theme.clone();
        assert_ne!(raised, nord, "the whole UI previews the contrast");
        harness.key("0");
        assert_eq!(harness.app.theme, nord);
        for _ in 0..8 {
            harness.key("right");
        }
        assert_eq!(harness.app.theme, raised);

        harness.key("enter");
        assert!(harness.picker().is_none());
        let overrides = harness.saved_overrides().expect("overrides saved");
        let themes = overrides.as_table().expect("table");
        assert_eq!(
            themes.keys().collect::<Vec<_>>(),
            ["nord"],
            "only the edited theme"
        );
        let saved_nord = themes["nord"].as_table().expect("nord");
        assert_eq!(
            saved_nord.keys().collect::<Vec<_>>(),
            ["contrast"],
            "only the edited value"
        );
        assert_eq!(saved_nord["contrast"].as_float(), Some(1.4));
        assert_eq!(
            harness.saved().app,
            None,
            "the git UI's own choice is untouched"
        );
        assert_eq!(harness.saved().bcode.theme.as_deref(), Some("nord"));
        assert_eq!(harness.app.theme, raised);
        // A watcher tick that read the file before the save does not bring the old contrast
        // back; one after it presents the saved contrast.
        harness.offer(&stale);
        assert_eq!(harness.app.theme, raised);
        harness.watch();
        assert_eq!(harness.app.theme, raised);

        // The minimum cycles from off; saving it keeps the saved factor.
        harness.key("ctrl+w");
        harness.key("t");
        assert!(harness.screen(100, 40).contains("1.40×"));
        // Shift+tab reaches the last row, the background tint.
        harness.key("shift+tab");
        assert!(harness.screen(100, 40).contains("› Background tint"));
        let before = harness.app.theme.clone();
        harness.key("left");
        assert_ne!(harness.app.theme, before, "the tint previews live");
        assert!(harness.screen(100, 40).contains("0.95×"));
        for _ in 0..3 {
            harness.key("up");
        }
        harness.key("right");
        harness.key("right");
        assert!(harness.screen(100, 40).contains("› Minimum"));
        harness.key("enter");
        let overrides = harness.saved_overrides().expect("overrides saved");
        assert_eq!(overrides["nord"]["contrast"].as_float(), Some(1.4));
        assert_eq!(overrides["nord"]["minimum_contrast"].as_float(), Some(4.5));
        assert_eq!(overrides["nord"]["background_tint"].as_float(), Some(0.95));
    }

    #[test]
    fn escape_restores_the_contrast_and_saves_nothing() {
        let mut harness = Harness::new();
        let nord = harness.theme_of("nord");
        harness.key("ctrl+w");
        harness.key("t");
        preview(&mut harness, "dracula");
        let dracula = harness.app.theme.clone();
        harness.key("tab");
        harness.key("left");
        harness.key("down");
        harness.key("right");
        assert_ne!(harness.app.theme, dracula);
        // Up leaves the contrast rows for the list; the cursor stayed on dracula meanwhile.
        harness.key("up");
        harness.key("up");
        let picker = harness.picker().expect("open");
        assert_eq!(picker.contrast().focus(), None);
        assert_eq!(picker.contrast_palette(), "dracula");
        harness.key("escape");
        assert_eq!(harness.app.theme, nord);
        assert_eq!(harness.saved_overrides(), None);
        assert_eq!(harness.saved().app, None);
    }

    #[test]
    fn a_click_focuses_a_contrast_row_and_the_wheel_steps_it() {
        use bmux_tui::event::{MouseButton, MouseEventKind};
        let mut harness = Harness::new();
        harness.key("ctrl+w");
        harness.key("t");
        let screen = harness.screen(100, 40);
        let factor = harness.contrast_row("contrast").expect("slider painted");
        let minimum = harness.contrast_row("minimum").expect("minimum painted");
        let rows: Vec<&str> = screen.lines().collect();
        assert!(rows[usize::from(factor.y)].contains("Contrast"), "{screen}");
        assert!(rows[usize::from(minimum.y)].contains("Minimum"), "{screen}");

        harness.mouse(MouseEventKind::ScrollUp, factor.right() - 1, factor.y);
        assert!(harness.screen(100, 40).contains("1.05×"));
        assert_ne!(harness.app.theme, harness.theme_of("nord"));
        harness.mouse(
            MouseEventKind::Down(MouseButton::Left),
            minimum.x,
            minimum.y,
        );
        assert!(harness.screen(100, 40).contains("› Minimum"));
        harness.key("right");
        let screen = harness.screen(100, 40);
        assert!(
            screen
                .lines()
                .nth(usize::from(minimum.y))
                .is_some_and(|row| row.contains("3.0")),
            "{screen}"
        );
        harness.key("escape");
        assert_eq!(harness.saved_overrides(), None);
    }

    #[test]
    fn the_contrast_rows_drop_labels_when_narrow_and_go_when_short() {
        let mut harness = Harness::new();
        harness.key("ctrl+w");
        harness.key("t");
        let screen = harness.screen(30, 40);
        let factor = harness
            .contrast_row("contrast")
            .unwrap_or_else(|| panic!("slider painted\n{screen}"));
        assert!(
            screen
                .lines()
                .nth(usize::from(factor.y))
                .is_some_and(|row| row.contains("1.00×")),
            "{screen}"
        );
        assert!(!screen.contains("Contrast"), "{screen}");
        for (_, area) in harness.app.hit_regions() {
            assert!(area.right() <= 30 && area.bottom() <= 40, "{area:?}");
        }

        let screen = harness.screen(100, 10);
        assert!(harness.contrast_row("contrast").is_none(), "{screen}");
        assert!(!screen.contains("Minimum"), "{screen}");
        // The keys still adjust the contrast.
        let before = harness.app.theme.clone();
        harness.key("tab");
        harness.key("right");
        assert_ne!(harness.app.theme, before);
        harness.key("escape");
        assert_eq!(harness.app.theme, harness.theme_of("nord"));
    }
}
