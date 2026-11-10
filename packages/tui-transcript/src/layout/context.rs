//! Explicit render inputs of item layout (L2, L3, L5).
//!
//! Every layout-affecting input reaches producers through [`ItemRenderContext`]; every
//! paint-only input through [`PaintInputs`]. V2 declares no thread-locals and no mutable
//! statics (L2, X10). [`LayoutFingerprint`] is a deterministic hash of the layout-affecting
//! inputs only: it never contains pointer addresses, wall-clock time, transcript length,
//! history-loading state, focus, selection, clock phase or elapsed time (L3).

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use bcode_presentation_policy::models::{Disclosure, ResolvedPresentation};
use bcode_session_view_models::TranscriptViewItemId;
use bcode_tui_theme::definition::ResolvedThemeDefinition;
use bmux_tui::text::Line;

use crate::focus::FocusTargetId;
use crate::frame::FrameClock;
use crate::selection::SelectionState;
use crate::units::UnitKey;
use crate::units::structure::{DisclosureTarget, ResolvedUnitPresentation};

/// Deterministic 64-bit FNV-1a hasher for fingerprints and signatures.
///
/// Unlike the standard library's randomly keyed hashers, equal inputs hash equally in every
/// process; inputs never include pointers or time (L3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StableHasher(u64);

impl StableHasher {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;

    /// A hasher in its initial state.
    #[must_use]
    pub const fn new() -> Self {
        Self(Self::OFFSET)
    }
}

impl Default for StableHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl Hasher for StableHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }
}

/// Stable hash of one value.
#[must_use]
pub fn stable_hash(value: &impl Hash) -> u64 {
    let mut hasher = StableHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// Hash every field of a resolved presentation (the policy type does not implement `Hash`).
pub fn hash_presentation(presentation: &ResolvedPresentation, state: &mut impl Hasher) {
    presentation.disclosure.hash(state);
    presentation.detail.hash(state);
    presentation.preview.hash(state);
    presentation.group.hash(state);
    presentation.reveal.hash(state);
    presentation.placement.hash(state);
    presentation.container.hash(state);
    presentation.sections.hash(state);
    presentation.header.hash(state);
    presentation.summary.hash(state);
    presentation.spacing.hash(state);
    presentation.indent.hash(state);
    presentation.plugin.hash(state);
}

/// Hash of every layout-affecting input of an item except width (L3).
///
/// Never contains pointer addresses, wall-clock time, transcript length, history-loading state,
/// focus, selection, clock phase or elapsed time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LayoutFingerprint(pub u64);

/// Markdown document identity of a unit (`transcript:<unit>`). Producers scope a unit's
/// `<details>` state with it; only entries under `<document id>:` reach the unit's
/// fingerprint (L3).
#[must_use]
pub fn markdown_document_id(unit: &UnitKey) -> String {
    format!("transcript:{unit}")
}

/// Markdown rendering options that affect layout.
///
/// The Markdown producer may add fields; every field must be layout-affecting and hashable.
/// [`Self::details_open`] is per unit: it is fingerprinted per unit, by document prefix, and
/// excluded from the fingerprint shared by every unit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarkdownOptions {
    /// `<details>` blocks opened by the user, by content key (L2: replaces V1's thread-local
    /// open state). Keys start with the owning unit's [`markdown_document_id`] and `:`; a
    /// toggle re-measures only that unit.
    pub details_open: BTreeMap<String, bool>,
    /// Whether Mermaid diagrams render as visuals (placeholders reserve rows until accepted).
    pub mermaid: bool,
    /// Whether inline images render (placeholders reserve rows until accepted).
    pub images: bool,
    /// Accepted image and Mermaid visuals by contribution id (computed outside drawing, L8).
    /// Per unit like [`Self::details_open`]: ids start with the owning unit's
    /// [`markdown_document_id`] and `:`, and an acceptance re-measures only that unit.
    pub visuals: crate::layout::producers::markdown::MarkdownVisualState,
    /// Directory relative local link destinations resolve against (the session's working
    /// directory); without one they stay unresolved and inert.
    pub base_directory: Option<std::path::PathBuf>,
}

impl MarkdownOptions {
    /// `<details>` entries of one Markdown document (keys under `<document_id>:`), in key
    /// order.
    pub fn details_for<'a>(
        &'a self,
        document_id: &str,
    ) -> impl Iterator<Item = (&'a String, &'a bool)> + 'a {
        let prefix = format!("{document_id}:");
        self.details_open
            .range::<str, _>((
                std::ops::Bound::Included(prefix.as_str()),
                std::ops::Bound::Unbounded,
            ))
            .take_while(move |(key, _)| key.starts_with(&prefix))
    }

    /// Hash of the options shared by every unit (everything except `<details>` state).
    fn hash_shared(&self, state: &mut impl Hasher) {
        // Destructured so a new field must be classified as shared or per unit.
        let Self {
            details_open: _,
            mermaid,
            images,
            visuals: _,
            base_directory,
        } = self;
        mermaid.hash(state);
        images.hash(state);
        base_directory.hash(state);
    }
}

/// Diff viewer layout choice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum DiffLayout {
    /// Side by side at or above the breakpoint, unified below it.
    #[default]
    Auto,
    /// Always unified.
    Unified,
    /// Always side by side.
    SideBySide,
}

/// Diff viewer options that affect layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DiffViewerOptions {
    /// Layout choice.
    pub layout: DiffLayout,
    /// Width in cells at which `Auto` switches to side by side.
    pub side_by_side_breakpoint: u16,
}

impl Default for DiffViewerOptions {
    fn default() -> Self {
        Self {
            layout: DiffLayout::Auto,
            side_by_side_breakpoint: 120,
        }
    }
}

/// One accepted plugin visual (L8, D9): what a plugin adapter produced outside drawing.
///
/// It is accepted for an exact item revision. Acceptance is an item revision; the revision
/// covers every field (rows, correspondence, images and actions), so it alone enters the
/// fingerprint (L3). Hosts bound every field before accepting it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcceptedPluginVisual {
    /// Adapter identity.
    pub adapter: String,
    /// Acceptance revision; part of the fingerprint.
    pub revision: u64,
    /// Accepted, bounded, styled rows (renderer-neutral content, already validated).
    pub rows: Arc<[Line]>,
    /// How the rows sit in the host chrome.
    pub mode: crate::layout::producers::visual::VisualRenderMode,
    /// Adapter title for the host header.
    pub title: Option<String>,
    /// Header title of a transcript-block visual whose adapter supplies none, chosen by the
    /// host for the visual's source (the tool name of a request, the artifact title of a
    /// result); the unit's label when absent.
    pub fallback_title: Option<String>,
    /// Configured invocation timeout in milliseconds, when the adapter knows it.
    pub timeout_ms: Option<u64>,
    /// Bounded producer-supplied operation target.
    pub target: Option<String>,
    /// Bounded one-line producer summary (`plugin` summary source).
    pub summary: Option<String>,
    /// Ordered, bounded named sections.
    pub sections: Arc<[crate::layout::producers::visual::VisualSection]>,
    /// Anchors and selection rows of `rows` (E3, C34).
    pub correspondence: Arc<crate::layout::producers::visual::VisualCorrespondence>,
    /// Images of `rows` in adapter coordinates (`y` is the adapter row; L8), validated by the
    /// host. Painted from the retained rows that show them, clipped to their cells.
    pub images: Arc<[bmux_tui::image::ImagePlacement]>,
    /// Action regions of `rows` in adapter coordinates (E2), validated by the host. Their hits
    /// are [`crate::layout::geometry::ItemAction::Visual`], resolved by
    /// [`PluginVisualState::resolve_action`].
    pub actions: Arc<[bcode_tui_components::plugin_visual::action::VisualActionRegion]>,
}

impl AcceptedPluginVisual {
    /// A visual of `rows` alone (inline, no header facts, sections or correspondence).
    #[must_use]
    pub fn rows(adapter: impl Into<String>, revision: u64, rows: Arc<[Line]>) -> Self {
        Self {
            adapter: adapter.into(),
            revision,
            rows,
            ..Self::default()
        }
    }
}

/// Accepted plugin visual rows per item (computed outside drawing, D9).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginVisualState {
    accepted: BTreeMap<TranscriptViewItemId, AcceptedPluginVisual>,
}

impl PluginVisualState {
    /// No accepted visuals.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            accepted: BTreeMap::new(),
        }
    }

    /// The accepted visual of an item, if any.
    #[must_use]
    pub fn accepted(&self, item: &TranscriptViewItemId) -> Option<&AcceptedPluginVisual> {
        self.accepted.get(item)
    }

    /// Accept a visual for an item, replacing any previous one.
    pub fn accept(&mut self, item: TranscriptViewItemId, visual: AcceptedPluginVisual) {
        self.accepted.insert(item, visual);
    }

    /// Drop the accepted visual of an item; returns whether one existed.
    pub fn remove(&mut self, item: &TranscriptViewItemId) -> bool {
        self.accepted.remove(item).is_some()
    }

    /// Keep only visuals of items `keep` accepts (residency reconciliation).
    pub fn retain(&mut self, keep: impl Fn(&TranscriptViewItemId) -> bool) {
        self.accepted.retain(|id, _| keep(id));
    }

    /// The action `id` of `item`'s accepted visual, only while that visual is still the one
    /// from `adapter` accepted at `revision` (E2): a hit measured from a replaced visual
    /// resolves to nothing.
    #[must_use]
    pub fn resolve_action(
        &self,
        item: &TranscriptViewItemId,
        adapter: &str,
        revision: u64,
        id: &str,
    ) -> Option<&bcode_tui_components::plugin_visual::action::VisualAction> {
        let visual = self.accepted.get(item)?;
        if visual.adapter != adapter || visual.revision != revision {
            return None;
        }
        visual
            .actions
            .iter()
            .find(|region| region.id == id)
            .map(|region| &region.action)
    }

    /// Fingerprint contribution of a unit: the accepted adapter and revision, if any.
    #[must_use]
    pub fn revision_for(&self, unit: &UnitKey) -> Option<(&str, u64)> {
        let UnitKey::Item(id) = unit else {
            return None;
        };
        self.accepted
            .get(id)
            .map(|visual| (visual.adapter.as_str(), visual.revision))
    }
}

/// Owned render configuration the engine builds contexts from.
#[derive(Debug, Clone)]
pub struct RenderConfig {
    /// Resolved theme (roles, containers, chrome, density, icons, motion).
    pub theme: Arc<ResolvedThemeDefinition>,
    /// Markdown options.
    pub markdown: MarkdownOptions,
    /// Diff viewer options.
    pub diff_viewer: DiffViewerOptions,
    /// Accepted plugin visual rows.
    pub plugin_visuals: PluginVisualState,
    /// Horizontal offsets of canonical shell screens (C39); per unit like the plugin visuals.
    pub shell_scroll: crate::layout::producers::visual::ShellScroll,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            theme: Arc::new(ResolvedThemeDefinition::empty("default", "Default")),
            markdown: MarkdownOptions::default(),
            diff_viewer: DiffViewerOptions::default(),
            plugin_visuals: PluginVisualState::default(),
            shell_scroll: crate::layout::producers::visual::ShellScroll::default(),
        }
    }
}

impl RenderConfig {
    /// Configuration rendering with `theme` and default Markdown, diff and plugin-visual
    /// options. [`Self::default`] uses the empty theme (no roles, containers or chrome), which
    /// headless tests use for chrome-free rows.
    #[must_use]
    pub fn for_theme(theme: Arc<ResolvedThemeDefinition>) -> Self {
        Self {
            theme,
            ..Self::default()
        }
    }

    /// Fingerprint of the inputs shared by every unit: theme layout roles, chrome, density,
    /// Markdown options other than `<details>` state, and diff options. A change re-measures
    /// every resident unit.
    #[must_use]
    pub fn shared_fingerprint(&self) -> u64 {
        let mut hasher = StableHasher::new();
        self.theme.fingerprint.hash(&mut hasher);
        self.theme.id.hash(&mut hasher);
        self.markdown.hash_shared(&mut hasher);
        self.diff_viewer.hash(&mut hasher);
        hasher.finish()
    }

    /// Fingerprint of the unit-scoped inputs: the unit's `<details>` state, its accepted
    /// Markdown visuals, its accepted plugin visual and its shell screen offset. A change
    /// re-measures only that unit.
    fn unit_fingerprint(&self, unit: &UnitKey) -> Option<u64> {
        let document_id = markdown_document_id(unit);
        let mut details = self.markdown.details_for(&document_id).peekable();
        let mut markdown_visuals = self.markdown.visuals.revisions_for(&document_id).peekable();
        let visual = self.plugin_visuals.revision_for(unit);
        let shell = self.shell_columns(unit);
        if details.peek().is_none()
            && markdown_visuals.peek().is_none()
            && visual.is_none()
            && shell == 0
        {
            return None;
        }
        let mut hasher = StableHasher::new();
        details.for_each(|entry| entry.hash(&mut hasher));
        markdown_visuals.for_each(|entry| entry.hash(&mut hasher));
        visual.hash(&mut hasher);
        if shell > 0 {
            shell.hash(&mut hasher);
        }
        Some(hasher.finish())
    }

    /// Columns the shell screen of `unit` is scrolled by (C39).
    fn shell_columns(&self, unit: &UnitKey) -> u64 {
        match unit {
            UnitKey::Item(id) => self.shell_scroll.columns(id),
            _ => 0,
        }
    }

    /// Render context for one unit at `width`.
    #[must_use]
    pub fn context<'a>(
        &'a self,
        unit: &UnitKey,
        width: u64,
        structure: &'a ResolvedUnitPresentation,
    ) -> ItemRenderContext<'a> {
        ItemRenderContext {
            width,
            theme: &self.theme,
            structure,
            disclosure: DisclosureState::of(structure),
            markdown: &self.markdown,
            diff_viewer: &self.diff_viewer,
            plugin_visuals: &self.plugin_visuals,
            shell_columns: self.shell_columns(unit),
            shared: self.shared_fingerprint(),
            unit: self.unit_fingerprint(unit),
        }
    }
}

/// Effective disclosure of one unit and the target that toggles it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DisclosureState {
    /// Effective disclosure (policy, manual override, visibility floor, group adjustment).
    pub disclosure: Disclosure,
    /// Target toggling the unit, if it has one (a disclosure focus target).
    pub target: Option<DisclosureTarget>,
    /// Whether the unit may collapse (units awaiting a decision may not, A16).
    pub collapsible: bool,
}

impl DisclosureState {
    /// Disclosure state derived from a unit's resolved structure.
    #[must_use]
    pub fn of(structure: &ResolvedUnitPresentation) -> Self {
        Self {
            disclosure: structure.disclosure(),
            target: structure.target().cloned(),
            collapsible: match structure {
                ResolvedUnitPresentation::Item(item) => item.collapsible,
                _ => true,
            },
        }
    }
}

/// All render configuration one producer may read for one unit (L2).
#[derive(Debug, Clone)]
pub struct ItemRenderContext<'a> {
    /// Layout width in cells.
    pub width: u64,
    /// Resolved theme; includes chrome, density, icons and motion.
    pub theme: &'a ResolvedThemeDefinition,
    /// Presentation-policy result for this unit.
    pub structure: &'a ResolvedUnitPresentation,
    /// Effective disclosure of this unit.
    pub disclosure: DisclosureState,
    /// Markdown options.
    pub markdown: &'a MarkdownOptions,
    /// Diff viewer options.
    pub diff_viewer: &'a DiffViewerOptions,
    /// Accepted plugin visual rows (computed outside drawing).
    pub plugin_visuals: &'a PluginVisualState,
    /// Columns this unit's shell screen is scrolled by (C39; part of the unit fingerprint).
    pub shell_columns: u64,
    shared: u64,
    unit: Option<u64>,
}

impl ItemRenderContext<'_> {
    /// Layout fingerprint of this context: every layout-affecting input above except width
    /// (L3).
    #[must_use]
    pub fn fingerprint(&self) -> LayoutFingerprint {
        let mut hasher = StableHasher::new();
        self.shared.hash(&mut hasher);
        self.structure.layout_hash().hash(&mut hasher);
        self.disclosure.hash(&mut hasher);
        self.unit.hash(&mut hasher);
        LayoutFingerprint(hasher.finish())
    }
}

/// Spinner and elapsed-label phase (L5, F7): advanced on accepted invalidations by the engine,
/// never sampled from wall time during drawing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ActivityPhase {
    /// Spinner frame counter.
    pub spinner_frame: u64,
    /// Wall-clock milliseconds accepted with the last invalidation, for elapsed labels.
    pub now_ms: Option<u64>,
}

/// Paint-only inputs (L5): they change paint revisions, never layout.
#[derive(Debug, Clone, Copy)]
pub struct PaintInputs<'a> {
    /// The frame's single clock sample (F2).
    pub clock: FrameClock,
    /// Focused transcript target, if any.
    pub focus: Option<&'a FocusTargetId>,
    /// Transcript selection.
    pub selection: &'a SelectionState,
    /// Spinner and elapsed phase.
    pub activity: &'a ActivityPhase,
}

#[cfg(test)]
mod tests {
    use web_time::Instant;

    use super::*;
    use crate::units::structure::{ResolvedUnitPresentation, StructureResolver};

    #[test]
    fn l3_stable_hasher_is_deterministic_and_input_sensitive() {
        assert_eq!(stable_hash(&"abc"), stable_hash(&"abc"));
        assert_ne!(stable_hash(&"abc"), stable_hash(&"abd"));
        assert_eq!(stable_hash(&(1u64, "x")), stable_hash(&(1u64, "x")));
    }

    #[test]
    fn l3_fingerprint_excludes_width_and_paint_inputs() {
        let config = RenderConfig::default();
        let structure = ResolvedUnitPresentation::Boundary;
        let unit = UnitKey::Boundary(crate::units::BoundaryEdge::Older);
        let narrow = config.context(&unit, 10, &structure).fingerprint();
        let wide = config.context(&unit, 200, &structure).fingerprint();
        assert_eq!(narrow, wide, "width is a separate cache key component");
        // Paint inputs are not part of the context at all; building them changes nothing.
        let selection = SelectionState::default();
        let activity = ActivityPhase {
            spinner_frame: 7,
            now_ms: Some(1),
        };
        let _paint = PaintInputs {
            clock: FrameClock {
                now: Instant::now(),
            },
            focus: None,
            selection: &selection,
            activity: &activity,
        };
        assert_eq!(config.context(&unit, 10, &structure).fingerprint(), narrow);
    }

    #[test]
    fn l3_fingerprint_tracks_theme_options_structure_and_visuals() {
        let resolver = StructureResolver::default();
        let id = TranscriptViewItemId::new("a");
        let unit = UnitKey::Item(id.clone());
        let structure = resolver.unit_presentation(&unit).clone();
        let base = RenderConfig::default();
        let fingerprint =
            |config: &RenderConfig| config.context(&unit, 40, &structure).fingerprint();
        let original = fingerprint(&base);

        let mut themed = base.clone();
        let mut theme = (*themed.theme).clone();
        theme.fingerprint = 99;
        themed.theme = Arc::new(theme);
        assert_ne!(fingerprint(&themed), original);

        let mut markdown = base.clone();
        markdown
            .markdown
            .details_open
            .insert(format!("{}:d", markdown_document_id(&unit)), true);
        assert_ne!(fingerprint(&markdown), original);

        // Another unit's `<details>` state is not this unit's input.
        let mut other = base.clone();
        other.markdown.details_open.insert(
            format!(
                "{}:d",
                markdown_document_id(&UnitKey::Item(TranscriptViewItemId::new("b")))
            ),
            true,
        );
        assert_eq!(fingerprint(&other), original);
        // Prefix matching respects the document boundary (`item:a` is not `item:ab`).
        let mut longer = base.clone();
        longer.markdown.details_open.insert(
            format!(
                "{}:d",
                markdown_document_id(&UnitKey::Item(TranscriptViewItemId::new("ab")))
            ),
            true,
        );
        assert_eq!(fingerprint(&longer), original);

        let mut mermaid = base.clone();
        mermaid.markdown.mermaid = true;
        assert_ne!(fingerprint(&mermaid), original);

        let mut diff = base.clone();
        diff.diff_viewer.layout = DiffLayout::Unified;
        assert_ne!(fingerprint(&diff), original);

        let mut visual = base.clone();
        visual.plugin_visuals.accept(
            id,
            AcceptedPluginVisual::rows("p", 1, Arc::from(Vec::new())),
        );
        assert_ne!(fingerprint(&visual), original);

        let hidden = ResolvedUnitPresentation::HiddenMarker;
        assert_ne!(base.context(&unit, 40, &hidden).fingerprint(), original);
    }
}
