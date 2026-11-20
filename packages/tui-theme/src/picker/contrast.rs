//! The appearance pickers' contrast rows: a contrast slider and a minimum-contrast control.
//!
//! Contrast is kept per palette. The rows show the values the edited palette presents
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

use crate::adjustment::{ContrastFactor, ContrastOverride, ContrastSettings, MinimumContrast};

/// One contrast row of a picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ContrastRow {
    /// The contrast factor slider.
    Factor,
    /// The minimum-contrast floor.
    Minimum,
}

impl ContrastRow {
    /// The rows, in painted order.
    pub const ALL: [Self; 2] = [Self::Factor, Self::Minimum];

    /// Row label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Factor => "Contrast",
            Self::Minimum => "Minimum",
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
        match (key, row) {
            (ContrastKey::Raise, _) => changed(self.step(palette, row, true)),
            (ContrastKey::Lower, _) => changed(self.step(palette, row, false)),
            (ContrastKey::Reset, _) => changed(self.reset(palette, row)),
            (ContrastKey::Up, ContrastRow::Factor) => self.leave(ContrastExit::Above),
            (ContrastKey::Previous, ContrastRow::Factor) => self.leave(ContrastExit::Before),
            (ContrastKey::Next, ContrastRow::Minimum) => self.leave(ContrastExit::After),
            (ContrastKey::Down | ContrastKey::Next, ContrastRow::Factor) => {
                self.focus = Some(ContrastRow::Minimum);
                ContrastKeyOutcome::Focused(ContrastRow::Minimum)
            }
            (ContrastKey::Up | ContrastKey::Previous, ContrastRow::Minimum) => {
                self.focus = Some(ContrastRow::Factor);
                ContrastKeyOutcome::Focused(ContrastRow::Factor)
            }
            (ContrastKey::Down, ContrastRow::Minimum) => ContrastKeyOutcome::Ignored,
        }
    }

    const fn leave(&mut self, exit: ContrastExit) -> ContrastKeyOutcome {
        self.focus = None;
        ContrastKeyOutcome::Left(exit)
    }

    /// Change `row` of `palette` one step (`forward` raises the factor or moves the floor to
    /// the next preset); returns whether the value changed.
    pub fn step(&mut self, palette: &str, row: ContrastRow, forward: bool) -> bool {
        match row {
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

    /// Reset `row` of `palette`: the factor to `1.0`, the floor to off; returns whether the
    /// value changed.
    pub fn reset(&mut self, palette: &str, row: ContrastRow) -> bool {
        match row {
            ContrastRow::Factor => self.set_factor(palette, ContrastFactor::EXACT),
            ContrastRow::Minimum => self.set_minimum(palette, MinimumContrast::OFF),
        }
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
                contrast_parts: None,
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
}
