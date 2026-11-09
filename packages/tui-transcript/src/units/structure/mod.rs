//! Structure resolution for unit derivation (U1, U2, U5).
//!
//! Structure resolution decides, per transcript item, whether the item is its own unit, a
//! member of a group, hidden behind a count marker, or suppressed. [`UnitStructure`] is the
//! hook unit derivation consults; [`StructureResolver`] implements it with the presentation
//! policy (rules, plugin facts, visibility floor, disclosure overrides) and the
//! renderer-neutral grouping of `bcode_presentation_policy`, resolved incrementally per changed
//! item. [`FlatStructure`] and closures implement it for tests.

pub mod disclosure;
pub mod item_facts;
pub mod layers;
pub mod resolver;

use bcode_session_view_models::{TranscriptViewItem, TranscriptViewItemKind};

pub use disclosure::{DisclosureOverrides, DisclosureTarget, MAX_DISCLOSURE_OVERRIDES};
pub use item_facts::{DeclaredTargetFields, HOST_TARGET_FIELDS, NoPluginFacts, PluginFactSource};
pub use layers::{PolicyLayers, density_layer};
pub use resolver::{
    GroupStructure, ItemStructure, ResolvedUnitPresentation, StructureConfig, StructureResolver,
    StructureUpdate,
};

/// How one grouped item renders under its group's current disclosure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MemberRendering {
    /// No unit: the group header stands for the item.
    Nothing,
    /// The item's unit shows one header row, as in a summarized group.
    HeaderRow,
    /// The item's own presentation, indented under the group.
    Full,
}

impl MemberRendering {
    /// Whether the item has its own unit.
    #[must_use]
    pub const fn has_unit(self) -> bool {
        !matches!(self, Self::Nothing)
    }
}

/// Placement of one transcript item in the unit list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemPlacement {
    /// The item is its own unit.
    Visible,
    /// The item is hidden; consecutive hidden items (within one turn) share one
    /// `HiddenMarker` unit (A16).
    Hidden,
    /// The item renders nothing, not even a count marker (an explicit user choice applied
    /// before the visibility floor, such as reasoning visibility switched off).
    Suppressed,
    /// The item belongs to a group; consecutive items of the same group (within one turn)
    /// share one `GroupHeader` unit, followed by the units of the items that render.
    Grouped {
        /// Caller-chosen group identity; a change of group starts a new group.
        group: u64,
        /// Whether the group is expanded.
        expanded: bool,
        /// Whether the item counts as a group member (bridges and attached units do not).
        member: bool,
        /// How the item renders under the group's disclosure.
        rendering: MemberRendering,
    },
}

impl ItemPlacement {
    /// A counted member of `group` that renders in full when the group is expanded and not at
    /// all when it is collapsed.
    #[must_use]
    pub const fn grouped(group: u64, expanded: bool) -> Self {
        Self::Grouped {
            group,
            expanded,
            member: true,
            rendering: if expanded {
                MemberRendering::Full
            } else {
                MemberRendering::Nothing
            },
        }
    }
}

/// Structure resolver consulted for every item whose placement may have changed.
pub trait UnitStructure {
    /// Placement of one item.
    fn placement(&self, item: &TranscriptViewItem) -> ItemPlacement;

    /// Deterministic signature of everything structural that changes the item unit's layout
    /// (its effective presentation). Unit derivation issues a new layout revision when it
    /// changes. The default is constant.
    fn signature(&self, _item: &TranscriptViewItem) -> u64 {
        0
    }

    /// Deterministic signature of the presentation of the group whose first item is `first`
    /// (header fields, disclosure, counts). The default is constant.
    fn group_signature(&self, _first: &TranscriptViewItem) -> u64 {
        0
    }
}

/// Resolver that makes every item its own unit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FlatStructure;

impl UnitStructure for FlatStructure {
    fn placement(&self, _item: &TranscriptViewItem) -> ItemPlacement {
        ItemPlacement::Visible
    }
}

impl<F> UnitStructure for F
where
    F: Fn(&TranscriptViewItem) -> ItemPlacement,
{
    fn placement(&self, item: &TranscriptViewItem) -> ItemPlacement {
        self(item)
    }
}

/// Whether an item starts a turn. Groups and hidden runs never cross a turn start (A14).
#[must_use]
pub const fn starts_turn(item: &TranscriptViewItem) -> bool {
    matches!(item.kind, TranscriptViewItemKind::UserMessage { .. })
}
