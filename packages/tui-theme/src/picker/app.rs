//! An app's appearance picker as one list of rows.
//!
//! The list starts with a "Follow Bcode" row, then the palettes and the profiles under their
//! headings. The cursor previews the row it is on: the follow row previews Bcode's choice, a
//! palette or profile row previews that value as the app's own choice. Each axis marks its
//! chosen row. A palette or profile pinned by the app's configuration is shown as a note
//! instead of that axis's rows, since choosing one could not take effect.
//!
//! The model is UI-agnostic: an app renders [`AppPicker::rows`] with its own list component and
//! styles, and presents each resolution the picker returns.

use std::path::Path;

use super::{AppearanceAxis, AppearanceSelection, LiveAppearance};
use crate::appearance::{AppearanceResolution, SavedAppearance};

/// What one picker row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppPickerRowKind {
    /// Follow Bcode's choice instead of keeping the app's own.
    Follow,
    /// The heading of an axis.
    Heading(AppearanceAxis),
    /// The app's configuration pins this axis; not selectable.
    Pinned(AppearanceAxis),
    /// A palette row (index on the palette axis).
    Palette(usize),
    /// A profile row (index on the profile axis).
    Profile(usize),
}

impl AppPickerRowKind {
    /// Whether the cursor can rest on this row.
    #[must_use]
    pub const fn selectable(self) -> bool {
        matches!(self, Self::Follow | Self::Palette(_) | Self::Profile(_))
    }
}

/// One picker row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPickerRow {
    /// What the row is.
    pub kind: AppPickerRowKind,
    /// Main text.
    pub label: String,
    /// Secondary text, shown dimmed after the label.
    pub detail: Option<String>,
    /// Whether this is the chosen value of its axis (or, for the follow row, whether the app
    /// follows Bcode).
    pub chosen: bool,
}

/// An app's appearance picker: the rows, the cursor and the selection they stand for.
#[derive(Debug, Clone)]
pub struct AppPicker {
    app: String,
    selection: AppearanceSelection,
    pinned_by: Option<String>,
    pinned_palette: String,
    profile_pinned_by: Option<String>,
    pinned_profile: String,
    bcode_summary: String,
    cursor: usize,
}

impl AppPicker {
    /// Open the picker of the app named `app` (as the user knows it, for example "review")
    /// over `live`, which holds watched appearances back until [`Self::apply`] or
    /// [`Self::cancel`]. `display_path` shows the path of skipped theme files.
    #[must_use]
    pub fn open(
        app: impl Into<String>,
        live: &LiveAppearance,
        display_path: impl Fn(&Path) -> String,
    ) -> Self {
        let opened = live.begin_preview();
        let bcode = live.bcode_appearance();
        let selection = opened.app_selection(AppearanceAxis::Palette, display_path);
        let mut picker = Self {
            app: app.into(),
            pinned_by: opened.theme_pinned_by.clone(),
            pinned_palette: display_name(&opened, &opened.preferences.name),
            profile_pinned_by: opened.profile_pinned_by.clone(),
            pinned_profile: profile_name(&opened),
            bcode_summary: summary(&bcode),
            selection,
            cursor: 0,
        };
        picker.cursor = picker.chosen_row();
        picker
    }

    /// The app's name, as given to [`Self::open`].
    #[must_use]
    pub fn app(&self) -> &str {
        &self.app
    }

    /// The selection the rows stand for.
    #[must_use]
    pub const fn selection(&self) -> &AppearanceSelection {
        &self.selection
    }

    /// Whether the selection follows Bcode's choice.
    #[must_use]
    pub fn following_bcode(&self) -> bool {
        self.selection.following_bcode() == Some(true)
    }

    /// One line saying which choice the app presents.
    #[must_use]
    pub fn mode_label(&self) -> String {
        if self.following_bcode() {
            format!("{} follows Bcode's appearance", self.app)
        } else {
            format!("{} keeps its own appearance", self.app)
        }
    }

    /// What pins `axis`, when the app's configuration does.
    #[must_use]
    pub fn pinned_by(&self, axis: AppearanceAxis) -> Option<&str> {
        match axis {
            AppearanceAxis::Palette => self.pinned_by.as_deref(),
            AppearanceAxis::Profile => self.profile_pinned_by.as_deref(),
        }
    }

    /// One sentence saying which axes the app's configuration pins and what still applies, or
    /// `None` when nothing is pinned.
    #[must_use]
    pub fn pinned_note(&self) -> Option<String> {
        match (&self.pinned_by, &self.profile_pinned_by) {
            (None, None) => None,
            (Some(palette), None) => Some(format!(
                "Palette pinned by {palette}; only the profile applies."
            )),
            (None, Some(profile)) => Some(format!(
                "Profile pinned by {profile}; only the palette applies."
            )),
            (Some(palette), Some(profile)) if palette == profile => Some(format!(
                "Palette and profile pinned by {palette}; choices here do not apply."
            )),
            (Some(palette), Some(profile)) => Some(format!(
                "Palette pinned by {palette} and profile by {profile}; choices here do not apply."
            )),
        }
    }

    /// The rows, top to bottom.
    #[must_use]
    pub fn rows(&self) -> Vec<AppPickerRow> {
        let following = self.following_bcode();
        let mut rows = vec![AppPickerRow {
            kind: AppPickerRowKind::Follow,
            label: "Follow Bcode".to_owned(),
            detail: Some(self.bcode_summary.clone()),
            chosen: following,
        }];
        rows.push(heading(AppearanceAxis::Palette));
        if let Some(pinned_by) = &self.pinned_by {
            rows.push(pinned(
                AppearanceAxis::Palette,
                &self.pinned_palette,
                pinned_by,
            ));
        } else {
            let selected = self.selection.palettes().selected_index();
            rows.extend(self.selection.palettes().entries().iter().enumerate().map(
                |(index, entry)| AppPickerRow {
                    kind: AppPickerRowKind::Palette(index),
                    label: entry.display_name.clone(),
                    detail: (entry.source != "bundled").then(|| entry.source.clone()),
                    chosen: !following && selected == Some(index),
                },
            ));
        }
        rows.push(heading(AppearanceAxis::Profile));
        if let Some(pinned_by) = &self.profile_pinned_by {
            rows.push(pinned(
                AppearanceAxis::Profile,
                &self.pinned_profile,
                pinned_by,
            ));
            return rows;
        }
        let selected = self.selection.profiles().selected_index();
        rows.extend(
            self.selection
                .profiles()
                .entries()
                .iter()
                .enumerate()
                .map(|(index, row)| AppPickerRow {
                    kind: AppPickerRowKind::Profile(index),
                    label: row.display_name.clone(),
                    detail: None,
                    chosen: !following && selected == index,
                }),
        );
        rows
    }

    /// Index of the row the cursor is on.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// The row of the presented choice: the follow row when following, else the chosen
    /// palette row (the chosen profile row when the palette is pinned; the follow row when
    /// both are).
    fn chosen_row(&self) -> usize {
        if self.following_bcode() {
            return 0;
        }
        let rows = self.rows();
        let wanted = |kind| rows.iter().position(|row| row.kind == kind);
        let palette = self
            .selection
            .palettes()
            .selected_index()
            .filter(|_| self.pinned_by.is_none())
            .and_then(|index| wanted(AppPickerRowKind::Palette(index)));
        palette
            .or_else(|| {
                wanted(AppPickerRowKind::Profile(
                    self.selection.profiles().selected_index(),
                ))
            })
            .unwrap_or(0)
    }

    /// Put the cursor on row `row` and preview it. Returns the appearance to present, or
    /// `None` when the row cannot be selected.
    pub fn select(&mut self, row: usize, live: &LiveAppearance) -> Option<AppearanceResolution> {
        let kind = self.rows().get(row)?.kind;
        let preview = match kind {
            AppPickerRowKind::Follow => live.set_following(&mut self.selection, true),
            AppPickerRowKind::Palette(index) => {
                let _ = self.selection.select_row(AppearanceAxis::Palette, index);
                live.preview(&self.selection)
            }
            AppPickerRowKind::Profile(index) => {
                let _ = self.selection.select_row(AppearanceAxis::Profile, index);
                live.preview(&self.selection)
            }
            AppPickerRowKind::Heading(_) | AppPickerRowKind::Pinned(_) => return None,
        };
        self.cursor = row;
        Some(preview)
    }

    /// Move the cursor `delta` selectable rows (negative is up), clamped to the ends, and
    /// preview the row it lands on. Returns the appearance to present, or `None` when the
    /// cursor did not move.
    pub fn step(&mut self, delta: isize, live: &LiveAppearance) -> Option<AppearanceResolution> {
        let selectable: Vec<usize> = self
            .rows()
            .iter()
            .enumerate()
            .filter(|(_, row)| row.kind.selectable())
            .map(|(index, _)| index)
            .collect();
        let current = selectable
            .iter()
            .position(|row| *row >= self.cursor)
            .unwrap_or(0);
        let target = current
            .saturating_add_signed(delta)
            .min(selectable.len().checked_sub(1)?);
        let row = selectable[target];
        if row == self.cursor {
            return None;
        }
        self.select(row, live)
    }

    /// Save the previewed choice ([`LiveAppearance::apply`]); `persist` receives the app's own
    /// choice to write, `None` to follow Bcode's, and `present` shows the resulting appearance.
    ///
    /// # Errors
    ///
    /// * The error of `persist`.
    pub fn apply<E>(
        &self,
        live: &LiveAppearance,
        persist: impl FnOnce(Option<&SavedAppearance>) -> Result<(), E>,
        present: impl FnOnce(&AppearanceResolution),
    ) -> Result<AppearanceResolution, E> {
        live.apply(&self.selection, persist, present)
    }

    /// Close without saving; `present` shows the latest appearance again
    /// ([`LiveAppearance::cancel`]), which is returned.
    pub fn cancel(
        &self,
        live: &LiveAppearance,
        present: impl FnOnce(&AppearanceResolution),
    ) -> AppearanceResolution {
        live.cancel(present)
    }
}

fn heading(axis: AppearanceAxis) -> AppPickerRow {
    AppPickerRow {
        kind: AppPickerRowKind::Heading(axis),
        label: axis.label().to_owned(),
        detail: None,
        chosen: false,
    }
}

fn pinned(axis: AppearanceAxis, label: &str, pinned_by: &str) -> AppPickerRow {
    AppPickerRow {
        kind: AppPickerRowKind::Pinned(axis),
        label: label.to_owned(),
        detail: Some(format!("pinned by {pinned_by}")),
        chosen: true,
    }
}

fn display_name(resolution: &AppearanceResolution, id: &str) -> String {
    resolution.catalog.definition(id).map_or_else(
        || id.to_owned(),
        |definition| definition.display_name().to_owned(),
    )
}

/// The profile `resolution` presents, as the user knows it.
fn profile_name(resolution: &AppearanceResolution) -> String {
    resolution.preferences.profile.as_deref().map_or_else(
        || "theme default".to_owned(),
        |profile| {
            display_name(resolution, profile)
                .trim_end_matches(" (profile)")
                .to_owned()
        },
    )
}

/// "Palette · Profile" of `resolution`.
fn summary(resolution: &AppearanceResolution) -> String {
    let palette = display_name(resolution, &resolution.preferences.name);
    format!("{palette} · {}", profile_name(resolution))
}

#[cfg(test)]
mod tests {
    use bmux_tui::capabilities::TerminalBackground;

    use super::*;
    use crate::appearance::{AppearanceSources, ThemePreferences};

    fn live(dir: &Path, app: Option<&str>, pinned: bool) -> LiveAppearance {
        LiveAppearance::new(
            AppearanceSources {
                configured: ThemePreferences {
                    name: "nord".to_owned(),
                    ..ThemePreferences::default()
                },
                saved: SavedAppearance {
                    theme: Some("catppuccin-mocha".to_owned()),
                    profile: None,
                },
                app_saved: app.map(|theme| SavedAppearance {
                    theme: Some(theme.to_owned()),
                    profile: None,
                }),
                user_config_dir: dir.to_path_buf(),
                project_root: dir.to_path_buf(),
                theme_pinned_by: pinned.then(|| "review config".to_owned()),
                ..AppearanceSources::default()
            }
            .resolve(TerminalBackground::Unknown),
        )
    }

    fn id(resolution: &AppearanceResolution) -> &str {
        &resolution.resolved.as_ref().expect("resolves").id
    }

    fn row_of(picker: &AppPicker, kind: AppPickerRowKind) -> usize {
        picker
            .rows()
            .iter()
            .position(|row| row.kind == kind)
            .expect("row")
    }

    fn palette_row(picker: &AppPicker, palette: &str) -> usize {
        let index = picker
            .selection()
            .palettes()
            .entries()
            .iter()
            .position(|entry| entry.id == palette)
            .expect("palette");
        row_of(picker, AppPickerRowKind::Palette(index))
    }

    #[test]
    fn a_following_app_opens_on_the_follow_row_and_the_cursor_previews_each_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let live = live(dir.path(), None, false);
        let mut picker = AppPicker::open("review", &live, |path| path.display().to_string());
        assert_eq!(picker.cursor(), 0);
        assert!(picker.following_bcode());
        assert_eq!(picker.mode_label(), "review follows Bcode's appearance");
        let rows = picker.rows();
        assert!(rows[0].chosen);
        assert_eq!(
            rows[0].detail.as_deref(),
            Some("Catppuccin Mocha · theme default")
        );
        assert!(!rows.iter().skip(1).any(|row| row.chosen));
        assert!(live.is_previewing());

        // Down from the follow row lands on the first palette, skipping the heading.
        let first = picker.step(1, &live).expect("moved");
        assert_eq!(picker.cursor(), 2);
        assert!(!picker.following_bcode());
        let first_id = picker.selection().palettes().entries()[0].id.clone();
        assert_eq!(id(&first), first_id);
        // Back up returns to following.
        assert_eq!(
            id(&picker.step(-1, &live).expect("moved")),
            "catppuccin-mocha"
        );
        assert!(picker.following_bcode());
        assert!(picker.step(-1, &live).is_none());

        // Cancel restores what was presented.
        let row = palette_row(&picker, "dracula");
        assert_eq!(
            id(&picker.select(row, &live).expect("selectable")),
            "dracula"
        );
        assert_eq!(id(&picker.cancel(&live, |_| {})), "catppuccin-mocha");
        assert!(!live.is_previewing());
    }

    #[test]
    fn an_app_with_its_own_choice_opens_on_it_and_apply_saves_the_cursor_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let live = live(dir.path(), Some("dracula"), false);
        let mut picker = AppPicker::open("git", &live, |path| path.display().to_string());
        assert!(!picker.following_bcode());
        assert_eq!(picker.cursor(), palette_row(&picker, "dracula"));
        assert!(picker.rows()[picker.cursor()].chosen);
        assert!(
            picker.select(1, &live).is_none(),
            "headings are not selectable"
        );

        let row = row_of(&picker, AppPickerRowKind::Profile(1));
        let previewed = picker.select(row, &live).expect("selectable");
        assert_eq!(previewed.preferences.name, "dracula");
        let profile = previewed.preferences.profile.expect("a profile");
        let mut written = None;
        let applied = picker
            .apply(
                &live,
                |app| {
                    written = Some(app.cloned());
                    Ok::<(), ()>(())
                },
                |_| {},
            )
            .expect("saved");
        assert_eq!(
            applied.preferences.profile.as_deref(),
            Some(profile.as_str())
        );
        assert_eq!(
            written,
            Some(Some(SavedAppearance {
                theme: Some("dracula".to_owned()),
                profile: Some(profile),
            }))
        );
    }

    #[test]
    fn a_pinned_palette_is_a_note_instead_of_palette_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let live = live(dir.path(), Some("dracula"), true);
        let picker = AppPicker::open("review", &live, |path| path.display().to_string());
        let rows = picker.rows();
        assert!(
            !rows
                .iter()
                .any(|row| matches!(row.kind, AppPickerRowKind::Palette(_)))
        );
        let pinned = &rows[row_of(&picker, AppPickerRowKind::Pinned(AppearanceAxis::Palette))];
        assert_eq!(pinned.label, "Nord");
        assert_eq!(pinned.detail.as_deref(), Some("pinned by review config"));
        assert_eq!(
            picker.pinned_by(AppearanceAxis::Palette),
            Some("review config")
        );
        assert_eq!(
            picker.pinned_note().as_deref(),
            Some("Palette pinned by review config; only the profile applies.")
        );
        assert!(matches!(
            rows[picker.cursor()].kind,
            AppPickerRowKind::Profile(_)
        ));
    }

    #[test]
    fn a_pinned_profile_is_a_note_and_palette_rows_preview_under_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let live = LiveAppearance::new(
            AppearanceSources {
                configured: ThemePreferences {
                    name: "nord".to_owned(),
                    profile: Some("bare".to_owned()),
                    ..ThemePreferences::default()
                },
                saved: SavedAppearance {
                    theme: Some("catppuccin-mocha".to_owned()),
                    profile: Some("structured".to_owned()),
                },
                user_config_dir: dir.path().to_path_buf(),
                project_root: dir.path().to_path_buf(),
                profile_pinned_by: Some("review config".to_owned()),
                ..AppearanceSources::default()
            }
            .resolve(TerminalBackground::Unknown),
        );
        let mut picker = AppPicker::open("review", &live, |path| path.display().to_string());
        let rows = picker.rows();
        assert!(
            !rows
                .iter()
                .any(|row| matches!(row.kind, AppPickerRowKind::Profile(_)))
        );
        let pinned = &rows[row_of(&picker, AppPickerRowKind::Pinned(AppearanceAxis::Profile))];
        assert_eq!(pinned.label, "Bare");
        assert_eq!(pinned.detail.as_deref(), Some("pinned by review config"));
        assert_eq!(
            picker.pinned_note().as_deref(),
            Some("Profile pinned by review config; only the palette applies.")
        );
        // Bcode's profile does not show through the follow row either.
        assert_eq!(rows[0].detail.as_deref(), Some("Catppuccin Mocha · Bare"));

        let row = palette_row(&picker, "dracula");
        let previewed = picker.select(row, &live).expect("selectable");
        assert_eq!(id(&previewed), "dracula");
        assert_eq!(previewed.preferences.profile.as_deref(), Some("bare"));
        // The last selectable row is the last palette: the pinned note is skipped.
        while picker.step(1, &live).is_some() {}
        assert!(matches!(
            picker.rows()[picker.cursor()].kind,
            AppPickerRowKind::Palette(_)
        ));
        let applied = picker
            .apply(&live, |_| Ok::<(), ()>(()), |_| {})
            .expect("saved");
        assert_eq!(applied.preferences.profile.as_deref(), Some("bare"));
    }
}
