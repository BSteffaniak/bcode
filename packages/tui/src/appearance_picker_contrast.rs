//! The appearance picker's contrast rows: a contrast slider and a minimum-contrast control.
//!
//! Contrast is kept per palette. The rows show the values the selected palette presents
//! (configured, saved, or edited while the picker is open); an edit changes only the edited
//! value of that palette. The picker remembers each edited palette's values from before its
//! first edit, so cancelling restores them, and applying saves only the edited values.

use std::collections::BTreeMap;

use bcode_tui_theme::adjustment::{ContrastFactor, ContrastOverride, ContrastSettings};

/// One contrast row of the picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContrastRow {
    /// The contrast factor slider.
    Factor,
    /// The minimum-contrast floor.
    Minimum,
}

impl ContrastRow {
    /// Row label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Factor => "Contrast",
            Self::Minimum => "Minimum",
        }
    }
}

/// Contrast row focus, shown values and the edits made while the picker is open.
#[derive(Debug, Clone, Default)]
pub struct ContrastControls {
    focus: Option<ContrastRow>,
    shown: ContrastSettings,
    edits: BTreeMap<String, ContrastOverride>,
    originals: BTreeMap<String, Option<ContrastOverride>>,
}

impl ContrastControls {
    /// The focused row, when a contrast row has focus instead of an axis column.
    #[must_use]
    pub const fn focus(&self) -> Option<ContrastRow> {
        self.focus
    }

    /// Focus `row`, or return focus to the axis columns with `None`; returns whether it
    /// changed.
    pub fn set_focus(&mut self, row: Option<ContrastRow>) -> bool {
        std::mem::replace(&mut self.focus, row) != row
    }

    /// The values the selected palette presents.
    #[must_use]
    pub const fn shown(&self) -> ContrastSettings {
        self.shown
    }

    /// Show `settings`, the values of the newly selected palette.
    pub const fn show(&mut self, settings: ContrastSettings) {
        self.shown = settings;
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
            ContrastRow::Minimum => {
                self.set_minimum(palette, bcode_tui_theme::adjustment::MinimumContrast::OFF)
            }
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

    fn set_minimum(
        &mut self,
        palette: &str,
        minimum: bcode_tui_theme::adjustment::MinimumContrast,
    ) -> bool {
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

    /// The edited values of every palette, to save.
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

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_tui_theme::adjustment::MinimumContrast;

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
}
