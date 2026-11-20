//! The appearance pickers' contrast rows: a contrast slider, a minimum-contrast control, and
//! vibrance, richness and background-tint sliders.
//!
//! Contrast and color are kept per palette. The rows show the values the edited palette presents
//! (configured, saved, or edited while the picker is open); an edit changes only the edited
//! value of that palette. [`ContrastControls`] remembers each edited palette's values from
//! before its first edit, so cancelling can restore them, and the edits alone are what applying
//! saves ([`ContrastOverride::saved_over`] merges one palette's edits into the values saved
//! for it).
//!
//! Keys reach the rows as [`ContrastKey`]s: the horizontal keys change the focused row, `0`
//! resets it, and the vertical and focus keys move between the rows or leave them
//! ([`ContrastExit`]), so each picker decides where focus goes next.

use std::collections::BTreeMap;

#[cfg(feature = "components")]
pub mod view;

use crate::adjustment::{
    BackgroundTint, ContrastFactor, ContrastOverride, ContrastSettings, MinimumContrast, Richness,
    Vibrance,
};

/// One contrast row of a picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ContrastRow {
    /// The contrast factor slider.
    Factor,
    /// The minimum-contrast floor.
    Minimum,
    /// The vibrance slider.
    Vibrance,
    /// The richness slider.
    Richness,
    /// The background-tint slider.
    BackgroundTint,
}

impl ContrastRow {
    /// The rows, in painted order.
    pub const ALL: [Self; 5] = [
        Self::Factor,
        Self::Minimum,
        Self::Vibrance,
        Self::Richness,
        Self::BackgroundTint,
    ];

    /// The first row.
    pub const FIRST: Self = Self::Factor;
    /// The last row.
    pub const LAST: Self = Self::BackgroundTint;

    /// Row label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Factor => "Contrast",
            Self::Minimum => "Minimum",
            Self::Vibrance => "Vibrance",
            Self::Richness => "Richness",
            Self::BackgroundTint => "Background tint",
        }
    }

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|row| *row == self)
            .unwrap_or_default()
    }

    /// The row after this one, if any.
    #[must_use]
    pub fn next(self) -> Option<Self> {
        Self::ALL.get(self.index() + 1).copied()
    }

    /// The row before this one, if any.
    #[must_use]
    pub fn previous(self) -> Option<Self> {
        self.index()
            .checked_sub(1)
            .and_then(|index| Self::ALL.get(index).copied())
    }

    /// Where `settings`' value of this slider row sits in its accepted range, in hundredths;
    /// `None` for the minimum-contrast row, which is not a slider.
    #[must_use]
    pub fn slider_position(self, settings: &ContrastSettings) -> Option<u64> {
        let position = match self {
            Self::Factor => return Some(factor_position(settings.factor)),
            Self::Minimum => return None,
            Self::Vibrance => settings.vibrance.position(),
            Self::Richness => settings.richness.position(),
            Self::BackgroundTint => settings.background_tint.position(),
        };
        // The position lies in 0..=1, so its hundredths fit.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let hundredths = (position * 100.0).round().clamp(0.0, 100.0) as u64;
        Some(hundredths)
    }

    /// The label a slider row shows for `settings`' value; `None` for the minimum-contrast row.
    #[must_use]
    pub fn slider_label(self, settings: &ContrastSettings) -> Option<String> {
        match self {
            Self::Factor => Some(format!("{}×", settings.factor)),
            Self::Minimum => None,
            Self::Vibrance => {
                Some(format!("{:+.2}", settings.vibrance.get()).replace("+0.00", "0.00"))
            }
            Self::Richness => Some(settings.richness.to_string()),
            Self::BackgroundTint => Some(format!("{}×", settings.background_tint)),
        }
    }
}

/// A key the focused contrast row handles, as a picker maps its bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContrastKey {
    /// Raise the focused value (right).
    Raise,
    /// Lower the focused value (left).
    Lower,
    /// Move up.
    Up,
    /// Move down.
    Down,
    /// Focus the next control (tab).
    Next,
    /// Focus the previous control (shift+tab).
    Previous,
    /// Reset the focused value (`0`).
    Reset,
}

/// Where focus leaves the contrast rows to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContrastExit {
    /// Above the rows (up from the slider).
    Above,
    /// The control after the rows (next from the minimum).
    After,
    /// The control before the rows (previous from the slider).
    Before,
}

/// What a [`ContrastKey`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContrastKeyOutcome {
    /// The focused value changed; preview it.
    Changed,
    /// Focus moved to this row.
    Focused(ContrastRow),
    /// Focus left the rows; the picker moves it on.
    Left(ContrastExit),
    /// Nothing changed.
    Ignored,
}

/// Contrast row focus, shown values and the edits made while a picker is open.
#[derive(Debug, Clone, Default)]
pub struct ContrastControls {
    focus: Option<ContrastRow>,
    shown: ContrastSettings,
    edits: BTreeMap<String, ContrastOverride>,
    originals: BTreeMap<String, Option<ContrastOverride>>,
}

impl ContrastControls {
    /// The focused row, when a contrast row has focus instead of the picker's lists.
    #[must_use]
    pub const fn focus(&self) -> Option<ContrastRow> {
        self.focus
    }

    /// Focus `row`, or return focus to the lists with `None`; returns whether it changed.
    pub fn set_focus(&mut self, row: Option<ContrastRow>) -> bool {
        std::mem::replace(&mut self.focus, row) != row
    }

    /// The values the edited palette presents.
    #[must_use]
    pub const fn shown(&self) -> ContrastSettings {
        self.shown
    }

    /// Show `settings`, the values of the newly edited palette.
    pub const fn show(&mut self, settings: ContrastSettings) {
        self.shown = settings;
    }

    /// Handle `key` on the focused row for `palette`. Without a focused row nothing is
    /// handled.
    pub fn handle_key(&mut self, palette: &str, key: ContrastKey) -> ContrastKeyOutcome {
        let Some(row) = self.focus else {
            return ContrastKeyOutcome::Ignored;
        };
        let changed = |changed: bool| {
            if changed {
                ContrastKeyOutcome::Changed
            } else {
                ContrastKeyOutcome::Ignored
            }
        };
        let moved = |controls: &mut Self, to: ContrastRow| {
            controls.focus = Some(to);
            ContrastKeyOutcome::Focused(to)
        };
        match key {
            ContrastKey::Raise => changed(self.step(palette, row, true)),
            ContrastKey::Lower => changed(self.step(palette, row, false)),
            ContrastKey::Reset => changed(self.reset(palette, row)),
            ContrastKey::Up | ContrastKey::Previous => match row.previous() {
                Some(previous) => moved(self, previous),
                None if key == ContrastKey::Up => self.leave(ContrastExit::Above),
                None => self.leave(ContrastExit::Before),
            },
            ContrastKey::Down | ContrastKey::Next => match row.next() {
                Some(next) => moved(self, next),
                None if key == ContrastKey::Next => self.leave(ContrastExit::After),
                None => ContrastKeyOutcome::Ignored,
            },
        }
    }

    const fn leave(&mut self, exit: ContrastExit) -> ContrastKeyOutcome {
        self.focus = None;
        ContrastKeyOutcome::Left(exit)
    }

    /// Change `row` of `palette` one step (`forward` raises a slider or moves the floor to the
    /// next preset); returns whether the value changed.
    pub fn step(&mut self, palette: &str, row: ContrastRow, forward: bool) -> bool {
        let steps = if forward { 1 } else { -1 };
        match row {
            ContrastRow::Vibrance => self.set_vibrance(palette, self.shown.vibrance.stepped(steps)),
            ContrastRow::Richness => self.set_richness(palette, self.shown.richness.stepped(steps)),
            ContrastRow::BackgroundTint => {
                self.set_background_tint(palette, self.shown.background_tint.stepped(steps))
            }
            ContrastRow::Factor => self.set_factor(
                palette,
                self.shown.factor.stepped(if forward { 1 } else { -1 }),
            ),
            ContrastRow::Minimum => {
                let minimum = if forward {
                    self.shown.minimum.next()
                } else {
                    self.shown.minimum.previous()
                };
                self.set_minimum(palette, minimum)
            }
        }
    }

    /// Reset `row` of `palette` to the value that keeps the theme's colors (the factor and the
    /// background tint to `1.0`, the floor to off, vibrance and richness to `0.0`); returns
    /// whether the value changed.
    pub fn reset(&mut self, palette: &str, row: ContrastRow) -> bool {
        match row {
            ContrastRow::Factor => self.set_factor(palette, ContrastFactor::EXACT),
            ContrastRow::Minimum => self.set_minimum(palette, MinimumContrast::OFF),
            ContrastRow::Vibrance => self.set_vibrance(palette, Vibrance::EXACT),
            ContrastRow::Richness => self.set_richness(palette, Richness::EXACT),
            ContrastRow::BackgroundTint => self.set_background_tint(palette, BackgroundTint::EXACT),
        }
    }

    fn set_vibrance(&mut self, palette: &str, vibrance: Vibrance) -> bool {
        if vibrance == self.shown.vibrance {
            return false;
        }
        self.shown.vibrance = vibrance;
        self.edits.entry(palette.to_owned()).or_default().vibrance = Some(vibrance);
        true
    }

    fn set_richness(&mut self, palette: &str, richness: Richness) -> bool {
        if richness == self.shown.richness {
            return false;
        }
        self.shown.richness = richness;
        self.edits.entry(palette.to_owned()).or_default().richness = Some(richness);
        true
    }

    fn set_background_tint(&mut self, palette: &str, tint: BackgroundTint) -> bool {
        if tint == self.shown.background_tint {
            return false;
        }
        self.shown.background_tint = tint;
        self.edits
            .entry(palette.to_owned())
            .or_default()
            .background_tint = Some(tint);
        true
    }

    fn set_factor(&mut self, palette: &str, factor: ContrastFactor) -> bool {
        if factor == self.shown.factor {
            return false;
        }
        self.shown.factor = factor;
        self.edits.entry(palette.to_owned()).or_default().contrast = Some(factor);
        true
    }

    fn set_minimum(&mut self, palette: &str, minimum: MinimumContrast) -> bool {
        if minimum == self.shown.minimum {
            return false;
        }
        self.shown.minimum = minimum;
        self.edits
            .entry(palette.to_owned())
            .or_default()
            .minimum_contrast = Some(minimum);
        true
    }

    /// Remember `original`, the per-theme values `palette` had before its first edit.
    pub fn remember_original(&mut self, palette: &str, original: Option<ContrastOverride>) {
        self.originals.entry(palette.to_owned()).or_insert(original);
    }

    /// The per-theme values `palette` presents while edited: its original values with the
    /// edits over them.
    #[must_use]
    pub fn edited_values(&self, palette: &str) -> Option<ContrastOverride> {
        let edits = self.edits.get(palette)?;
        let original = self.originals.get(palette).copied().flatten();
        Some(original.unwrap_or_default().overlaid_with(*edits))
    }

    /// The edited values of every palette, to preview and save.
    #[must_use]
    pub const fn edits(&self) -> &BTreeMap<String, ContrastOverride> {
        &self.edits
    }

    /// The per-theme values every edited palette had when the picker opened, to restore.
    #[must_use]
    pub const fn originals(&self) -> &BTreeMap<String, Option<ContrastOverride>> {
        &self.originals
    }
}

/// The minimum-contrast choices a picker lists: off, then each preset.
pub fn minimum_choices() -> impl Iterator<Item = MinimumContrast> {
    std::iter::once(MinimumContrast::OFF).chain(
        crate::adjustment::MINIMUM_CONTRAST_PRESETS
            .iter()
            .filter_map(|ratio| MinimumContrast::new(*ratio).ok()),
    )
}

/// Where `factor` sits on the slider, in hundredths of the accepted range (exact for every
/// value on the slider's step grid).
#[must_use]
pub fn factor_position(factor: ContrastFactor) -> u64 {
    let range = crate::adjustment::CONTRAST_FACTOR_MAX - crate::adjustment::CONTRAST_FACTOR_MIN;
    let position = (factor.get() - crate::adjustment::CONTRAST_FACTOR_MIN) / range;
    // The position lies in 0..=1, so its hundredths fit.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let hundredths = (position * 100.0).round().clamp(0.0, 100.0) as u64;
    hundredths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_keep_only_the_edited_values_over_the_originals() {
        let mut controls = ContrastControls::default();
        let original = ContrastOverride {
            minimum_contrast: Some(MinimumContrast::new(3.0).expect("floor")),
            ..ContrastOverride::default()
        };
        controls.show(ContrastSettings {
            minimum: MinimumContrast::new(3.0).expect("floor"),
            ..ContrastSettings::EXACT
        });
        controls.remember_original("nord", Some(original));
        assert!(controls.step("nord", ContrastRow::Factor, true));
        assert!(controls.step("nord", ContrastRow::Factor, true));
        assert_eq!(controls.shown().factor.get(), 1.1);
        let values = controls.edited_values("nord").expect("edited");
        assert_eq!(values.contrast.map(ContrastFactor::get), Some(1.1));
        assert_eq!(values.minimum_contrast, original.minimum_contrast);
        assert_eq!(controls.edits()["nord"].minimum_contrast, None);
        // A second original for the same palette is ignored.
        controls.remember_original("nord", None);
        assert_eq!(controls.originals()["nord"], Some(original));

        assert!(controls.reset("nord", ContrastRow::Factor));
        assert!(!controls.reset("nord", ContrastRow::Factor));
        assert_eq!(controls.shown().factor, ContrastFactor::EXACT);
        assert!(controls.step("nord", ContrastRow::Minimum, true));
        assert_eq!(controls.shown().minimum.ratio(), Some(4.5));
        assert!(controls.reset("nord", ContrastRow::Minimum));
        assert_eq!(controls.shown().minimum, MinimumContrast::OFF);
        assert!(controls.edited_values("dawn").is_none());
    }

    #[test]
    fn keys_step_reset_move_between_the_rows_and_leave_them() {
        let mut controls = ContrastControls::default();
        assert_eq!(
            controls.handle_key("nord", ContrastKey::Raise),
            ContrastKeyOutcome::Ignored,
            "nothing is focused"
        );
        controls.set_focus(Some(ContrastRow::Factor));
        assert_eq!(
            controls.handle_key("nord", ContrastKey::Raise),
            ContrastKeyOutcome::Changed
        );
        assert_eq!(controls.shown().factor.get(), 1.05);
        assert_eq!(
            controls.handle_key("nord", ContrastKey::Reset),
            ContrastKeyOutcome::Changed
        );
        assert_eq!(
            controls.handle_key("nord", ContrastKey::Reset),
            ContrastKeyOutcome::Ignored
        );
        assert_eq!(
            controls.handle_key("nord", ContrastKey::Next),
            ContrastKeyOutcome::Focused(ContrastRow::Minimum)
        );
        let floors: Vec<String> = (0..4)
            .map(|_| {
                controls.handle_key("nord", ContrastKey::Raise);
                controls.shown().minimum.to_string()
            })
            .collect();
        assert_eq!(floors, ["3.0", "4.5", "7.0", "off"]);
        for row in [
            ContrastRow::Vibrance,
            ContrastRow::Richness,
            ContrastRow::BackgroundTint,
        ] {
            assert_eq!(
                controls.handle_key("nord", ContrastKey::Next),
                ContrastKeyOutcome::Focused(row)
            );
        }
        assert_eq!(
            controls.handle_key("nord", ContrastKey::Down),
            ContrastKeyOutcome::Ignored
        );
        assert_eq!(
            controls.handle_key("nord", ContrastKey::Next),
            ContrastKeyOutcome::Left(ContrastExit::After)
        );
        assert_eq!(controls.focus(), None);
        controls.set_focus(Some(ContrastRow::Minimum));
        assert_eq!(
            controls.handle_key("nord", ContrastKey::Up),
            ContrastKeyOutcome::Focused(ContrastRow::Factor)
        );
        assert_eq!(
            controls.handle_key("nord", ContrastKey::Previous),
            ContrastKeyOutcome::Left(ContrastExit::Before)
        );
        controls.set_focus(Some(ContrastRow::Factor));
        assert_eq!(
            controls.handle_key("nord", ContrastKey::Up),
            ContrastKeyOutcome::Left(ContrastExit::Above)
        );
    }

    #[test]
    fn saved_values_merge_the_edits_and_the_rows_list_their_choices() {
        let floor = MinimumContrast::new(4.5).expect("floor");
        let factor = ContrastFactor::new(1.4).expect("factor");
        let prior = ContrastOverride {
            minimum_contrast: Some(floor),
            ..ContrastOverride::default()
        };
        let edits = ContrastOverride {
            contrast: Some(factor),
            ..ContrastOverride::default()
        };
        assert_eq!(
            edits.saved_over(Some(prior)),
            Some(ContrastOverride {
                contrast: Some(factor),
                minimum_contrast: Some(floor),
                ..ContrastOverride::default()
            })
        );
        assert_eq!(ContrastOverride::default().saved_over(None), None);
        assert_eq!(
            minimum_choices()
                .map(|choice| choice.to_string())
                .collect::<Vec<_>>(),
            ["off", "3.0", "4.5", "7.0"]
        );
        assert_eq!(factor_position(ContrastFactor::EXACT), 33);
        assert_eq!(factor_position(factor), 60);
    }

    #[test]
    fn color_rows_step_reset_and_edit_only_their_value() {
        let mut controls = ContrastControls::default();
        controls.remember_original("nord", None);
        controls.set_focus(Some(ContrastRow::Vibrance));
        for _ in 0..3 {
            assert_eq!(
                controls.handle_key("nord", ContrastKey::Raise),
                ContrastKeyOutcome::Changed
            );
        }
        assert_eq!(controls.shown().vibrance.get(), 0.15);
        assert!(controls.step("nord", ContrastRow::Vibrance, false));
        assert_eq!(controls.shown().vibrance.get(), 0.1);
        assert!(!controls.step("nord", ContrastRow::Richness, false), "at 0");
        assert!(controls.step("nord", ContrastRow::Richness, true));
        assert!(controls.step("nord", ContrastRow::BackgroundTint, false));
        assert_eq!(controls.shown().background_tint.get(), 0.95);
        let values = controls.edited_values("nord").expect("edited");
        assert_eq!(values.vibrance.map(Vibrance::get), Some(0.1));
        assert_eq!(values.richness.map(Richness::get), Some(0.05));
        assert_eq!(values.background_tint.map(BackgroundTint::get), Some(0.95));
        assert_eq!(values.contrast, None);
        controls.set_focus(Some(ContrastRow::BackgroundTint));
        assert_eq!(
            controls.handle_key("nord", ContrastKey::Reset),
            ContrastKeyOutcome::Changed
        );
        assert_eq!(controls.shown().background_tint, BackgroundTint::EXACT);
        assert_eq!(
            ContrastRow::Vibrance
                .slider_label(&controls.shown())
                .as_deref(),
            Some("+0.10")
        );
        assert_eq!(
            ContrastRow::Vibrance.slider_position(&controls.shown()),
            Some(55)
        );
        assert_eq!(
            ContrastRow::Minimum.slider_position(&controls.shown()),
            None
        );
    }
}
