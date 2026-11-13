//! The git UI's appearance picker: its own palette and profile, or bcode's.
//!
//! The picker is a popup menu over the rows of [`AppPicker`] (Follow Bcode, the palettes, the
//! profiles). Moving the cursor restyles the whole UI at once; confirming saves the git UI's
//! own choice in user state (`[apps.git.theme]`), or removes it on the Follow Bcode row so the
//! UI follows bcode's again; closing restores what was presented. bcode's own choice is never
//! written here. The appearance watcher keeps running, but its results wait while the picker
//! previews ([`bcode_tui_theme::picker::LiveAppearance`]).

use bcode_config_layers::user_state::{SavedTuiAppearance, save_app_tui_appearance};
use bcode_tui_theme::appearance::AppearanceResolution;
use bcode_tui_theme::picker::app::{AppPicker, AppPickerRowKind};
use bmux_keyboard::KeyStroke;
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
    notes.push(Line::from("enter apply · f follow Bcode · esc cancel"));
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
            let loaded = LoadedConfig {
                saved_appearance: saved(&self.saved().bcode),
                app_appearance: self.saved().app.as_ref().map(saved),
                config_dir: self.dir.path().to_path_buf(),
                ..LoadedConfig::default()
            };
            self.offer(&loaded);
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
}
