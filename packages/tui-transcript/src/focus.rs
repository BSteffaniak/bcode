//! Composer/transcript keyboard focus (spec section 6, rules FO1–FO10).
//!
//! Phase 2 skeleton: focus types only; traversal and key handling land with the focus layer.

use crate::units::UnitKey;

/// One focus target inside a unit.
///
/// Targets are derived from the unit's retained layout (FO7, B12). `local` is the target's
/// position among the unit's focus targets in layout order; `0` is the unit itself.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FocusTargetId {
    /// Owning unit.
    pub unit: UnitKey,
    /// Target within the unit (`0`: the unit itself).
    pub local: u32,
}

impl FocusTargetId {
    /// The unit-level target of a unit.
    #[must_use]
    pub const fn unit(unit: UnitKey) -> Self {
        Self { unit, local: 0 }
    }
}

/// Where keyboard focus returns when transcript focus ends (FO3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ReturnFocus {
    /// The composer.
    #[default]
    Composer,
}

/// Which surface has keyboard focus (FO1). Ephemeral; never persisted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub enum ChatFocus {
    /// The composer (default).
    #[default]
    Composer,
    /// Transcript unit focus (`alt+u`).
    Transcript {
        /// Focused target.
        target: FocusTargetId,
        /// Where focus returns.
        return_to: ReturnFocus,
    },
}

impl ChatFocus {
    /// The focused transcript target, if the transcript has focus.
    #[must_use]
    pub const fn transcript_target(&self) -> Option<&FocusTargetId> {
        match self {
            Self::Composer => None,
            Self::Transcript { target, .. } => Some(target),
        }
    }
}
