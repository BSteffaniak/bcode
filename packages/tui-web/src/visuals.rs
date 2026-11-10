//! Plugin visuals of the browser transcript (spec L8, D9, E1–E3, E10), outside drawing.
//!
//! The browser mirrors the native visual sync: every resident plugin-owned item (a request
//! draft, an invocation presentation, an artifact result or a contribution) is projected to its
//! canonical adapter input, prepared by the statically linked portable registry at the unit's
//! body width and accepted, bounded, with its rows, source correspondence, images and action
//! regions for one exact revision. The accepted visuals reach the engine through its render
//! configuration; nothing here paints.
//!
//! * An item is re-routed only when its routing inputs change (item revision, registry and
//!   artifact state, body width, theme and presentation), at most [`MAX_VISUALS_PER_SYNC`]
//!   adapter calls per sync; the rest are routed by the next sync.
//! * A decoded artifact image replaces the item's visual with reserved rows and one image
//!   placement ([`crate::image`]), fenced by item identity, item revision and body width.
//! * Producer identities are scoped `<plugin>:<identity>`, as the native host scopes them.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bcode_session_view_models::{
    ToolResultView, TranscriptViewItem, TranscriptViewItemId, TranscriptViewItemKind,
};
use bcode_tui_components::plugin_visual::canonical::{
    CanonicalPluginVisual, CanonicalToolVisual, presentation_contribution,
};
use bcode_tui_components::plugin_visual::output::PortableVisualOutput;
use bcode_tui_components::plugin_visual::portable::{
    PortableArtifactChunk, PortableVisualContext, PortableVisualPolicy, PortableVisualRegistry,
};
use bcode_tui_transcript::engine::TranscriptEngine;
use bcode_tui_transcript::layout::context::{AcceptedPluginVisual, PluginVisualState, stable_hash};
use bcode_tui_transcript::layout::producers::visual::{
    VisualAnchor, VisualCorrespondence, VisualSelectionCell, VisualSelectionRow, VisualSourceRange,
};
use bcode_tui_transcript::units::UnitKey;
use bmux_tui::prelude::Line;
use bmux_tui_components::theme::ComponentTheme;

/// Most adapter calls of one sync; further items are routed by the next sync.
pub const MAX_VISUALS_PER_SYNC: usize = 32;

/// A decoded artifact image accepted for one item ([`crate::image`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedImage {
    /// Item revision the image was decoded for.
    pub item_revision: u64,
    /// Body width the placement was measured at.
    pub width: u16,
    /// Reserved rows the image covers.
    pub rows: u16,
    /// The placement, in adapter coordinates.
    pub image: bmux_tui::image::ImagePlacement,
}

/// The plugin visual one resident item renders through.
pub struct ItemVisual {
    /// Adapter input.
    pub visual: CanonicalPluginVisual,
    /// Header title of a visual whose adapter supplies none.
    pub fallback_title: String,
}

/// The plugin visual `item` renders through, as the native host routes it, without request
/// schemas (the browser has no plugin presentation declarations):
///
/// * A live request draft (no presentation or result yet).
/// * A plugin-owned invocation presentation: its primary contribution.
/// * A final artifact result.
/// * A visible plugin contribution.
#[must_use]
pub fn item_visual(item: &TranscriptViewItem) -> Option<ItemVisual> {
    let visual = |visual: CanonicalToolVisual, title: &str| {
        let CanonicalToolVisual::Plugin(visual) = visual;
        ItemVisual {
            visual,
            fallback_title: title.to_owned(),
        }
    };
    let draft = |draft| {
        visual(
            CanonicalToolVisual::from_draft_view(draft, &serde_json::json!(item.revision)),
            "Tool request draft",
        )
    };
    match &item.kind {
        TranscriptViewItemKind::ToolInvocation { tool } => {
            if let Some(pending) = tool.request_draft.as_ref()
                && tool.presentation.is_none()
                && tool.result.is_none()
                && tool.result_text.is_none()
            {
                return Some(draft(pending));
            }
            if let Some(contribution) = presentation_contribution(tool) {
                return Some(visual(
                    CanonicalToolVisual::from_contribution(&contribution),
                    "Tool contribution",
                ));
            }
            match &tool.result {
                Some(ToolResultView::Artifact { artifact }) => Some(visual(
                    CanonicalToolVisual::from_artifact(&artifact.artifact),
                    artifact.artifact.title.as_deref().unwrap_or("Tool result"),
                )),
                _ => None,
            }
        }
        TranscriptViewItemKind::ToolRequestDraft { draft: pending } => Some(draft(pending)),
        TranscriptViewItemKind::ToolContribution {
            contribution,
            placement,
            ..
        } if *placement != bcode_session_models::ToolContributionPlacement::Hidden => Some(visual(
            CanonicalToolVisual::from_contribution(contribution),
            "Tool contribution",
        )),
        _ => None,
    }
}

/// The resident item `id` of `engine`.
#[must_use]
pub fn resident_item<'a>(
    engine: &'a TranscriptEngine,
    id: &TranscriptViewItemId,
) -> Option<&'a TranscriptViewItem> {
    engine
        .session_view()
        .snapshot()
        .transcript
        .items
        .iter()
        .find(|item| item.id == *id)
}

/// Width plugin rows of `item` are laid out at: the unit's body width at the engine width.
#[must_use]
pub fn body_width(engine: &TranscriptEngine, item: &TranscriptViewItem) -> u16 {
    let structure = engine
        .structure()
        .unit_presentation(&UnitKey::Item(item.id.clone()));
    let width = engine.geometry().width;
    let body = bcode_tui_transcript::layout::producers::tool::body_width(
        item,
        structure,
        &engine.render_config().theme,
        width,
    );
    u16::try_from(body.min(width)).unwrap_or(u16::MAX)
}

/// Accepted plugin visuals of the browser transcript and the registry that prepares them.
#[derive(Default)]
pub struct BrowserVisuals {
    registry: Option<PortableVisualRegistry>,
    registry_generation: u64,
    accepted: PluginVisualState,
    /// Routing inputs each accepted (or declined) visual was routed with.
    keys: BTreeMap<TranscriptViewItemId, u64>,
    /// Decoded artifact images by item.
    images: BTreeMap<TranscriptViewItemId, DecodedImage>,
    /// Last acceptance revision handed out; every acceptance takes a new one, so a hit
    /// measured from a replaced visual never resolves (E2).
    revision: u64,
    /// Items wait for routing beyond the per-sync bound.
    pending: bool,
}

impl BrowserVisuals {
    /// Replace the registry with the producers `policy` enables. Every visual is re-routed.
    ///
    /// # Errors
    ///
    /// Rejects policies or registrations beyond the registry's bounds.
    pub fn configure(
        &mut self,
        policy: &crate::policy::BrowserVisualPolicy,
    ) -> Result<(), &'static str> {
        self.registry_generation = self.registry_generation.saturating_add(1);
        let mut registrations = bcode_question_plugin::portable_visual_registrations();
        registrations.extend(bcode_filesystem_plugin::portable_visual_registrations());
        registrations.extend(bcode_shell_plugin::portable_visual_registrations());
        self.registry = Some(PortableVisualRegistry::new(
            self.registry_generation,
            PortableVisualPolicy {
                enabled_plugins: policy.enabled_plugins.clone(),
                preferred: policy.preferred.clone(),
                disabled: policy.disabled.clone(),
            },
            registrations,
        )?);
        self.keys.clear();
        Ok(())
    }

    /// Accepted visuals for the engine's render configuration.
    #[must_use]
    pub const fn accepted(&self) -> &PluginVisualState {
        &self.accepted
    }

    /// Items still wait for routing beyond the last sync's bound.
    #[must_use]
    pub const fn pending(&self) -> bool {
        self.pending
    }

    /// Accept a decoded image for `item`; it is shown from the next sync.
    pub fn accept_image(&mut self, item: TranscriptViewItemId, image: DecodedImage) {
        self.keys.remove(&item);
        self.images.insert(item, image);
    }

    /// Revision of the visual accepted for `item` now, if any.
    #[must_use]
    pub fn revision_of(&self, item: &TranscriptViewItemId) -> Option<u64> {
        self.accepted.accepted(item).map(|visual| visual.revision)
    }

    /// Route the resident items of `engine` whose routing inputs changed and accept their
    /// visuals. `style` identifies the presentation policy the adapters render with. Returns
    /// whether any accepted visual changed.
    pub fn sync(
        &mut self,
        engine: &TranscriptEngine,
        theme: Option<&ComponentTheme>,
        style: u64,
    ) -> bool {
        let items = &engine.session_view().snapshot().transcript.items;
        let mut changed = false;
        let mut routed = 0;
        let mut seen = BTreeSet::new();
        self.pending = false;
        for item in items {
            let Some(source) = item_visual(item) else {
                continue;
            };
            seen.insert(item.id.clone());
            let width = body_width(engine, item);
            // A decoded image belongs to one item revision at one body width (stale-identity
            // fence): any other identity drops it.
            if self
                .images
                .get(&item.id)
                .is_some_and(|image| image.item_revision != item.revision || image.width != width)
            {
                self.images.remove(&item.id);
            }
            let image = self.images.get(&item.id);
            let structure = engine
                .structure()
                .unit_presentation(&UnitKey::Item(item.id.clone()))
                .layout_hash();
            let key = stable_hash(&(
                item.revision,
                self.registry_generation,
                self.registry
                    .as_ref()
                    .map(PortableVisualRegistry::artifact_revision),
                width,
                engine.render_config().theme.fingerprint,
                structure,
                style,
                image.is_some(),
            ));
            if self.keys.get(&item.id) == Some(&key) {
                continue;
            }
            if routed == MAX_VISUALS_PER_SYNC {
                self.pending = true;
                break;
            }
            routed += 1;
            self.keys.insert(item.id.clone(), key);
            let next = match image {
                Some(image) => Some(image_visual(image, &source.fallback_title)),
                None => self.route(&source, width, theme),
            };
            changed |= match next {
                Some(mut visual) => {
                    self.revision = self.revision.wrapping_add(1);
                    visual.revision = self.revision;
                    self.accepted.accept(item.id.clone(), visual);
                    true
                }
                None => self.accepted.remove(&item.id),
            };
        }
        if !self.pending {
            self.images.retain(|id, _| seen.contains(id));
            let before = self.keys.len();
            self.keys.retain(|id, _| seen.contains(id));
            if self.keys.len() != before {
                self.accepted.retain(|id| seen.contains(id));
                changed = true;
            }
        }
        changed
    }

    fn route(
        &self,
        source: &ItemVisual,
        width: u16,
        theme: Option<&ComponentTheme>,
    ) -> Option<AcceptedPluginVisual> {
        let registry = self.registry.as_ref()?;
        if width == 0 {
            return None;
        }
        let prepared = registry.prepare(&source.visual, &PortableVisualContext { width, theme })?;
        let adapter = prepared.adapter_reference().to_owned();
        let output = registry.resolve(prepared, &source.visual, width)?;
        Some(accepted_visual(&adapter, output, &source.fallback_title))
    }

    /// Whether the registry prepares a visual for `item` at its body width.
    #[must_use]
    pub fn has_visual(&self, engine: &TranscriptEngine, item: &TranscriptViewItem) -> bool {
        self.prepared_reference(engine, item).is_some()
    }

    fn prepared_reference(
        &self,
        engine: &TranscriptEngine,
        item: &TranscriptViewItem,
    ) -> Option<(String, CanonicalPluginVisual, u16)> {
        let registry = self.registry.as_ref()?;
        let source = item_visual(item)?;
        let width = body_width(engine, item);
        let prepared = registry.prepare(
            &source.visual,
            &PortableVisualContext { width, theme: None },
        )?;
        Some((
            prepared.adapter_reference().to_owned(),
            source.visual,
            width,
        ))
    }

    /// Bytes the selected adapter of `item` wants from `reference_key`, if any.
    #[must_use]
    pub fn artifact_preview_limit(
        &self,
        engine: &TranscriptEngine,
        item: &TranscriptViewItem,
        reference_key: &str,
        content_type: Option<&str>,
    ) -> Option<u64> {
        let source = item_visual(item)?;
        self.registry.as_ref()?.artifact_preview_limit(
            &source.visual,
            &PortableVisualContext {
                width: body_width(engine, item),
                theme: None,
            },
            reference_key,
            content_type,
        )
    }

    /// Deliver a verified artifact chunk to the adapter selected for `item`. Accepted bytes
    /// re-route every registry visual at the next sync.
    #[must_use]
    pub fn deliver_artifact(
        &self,
        engine: &TranscriptEngine,
        item: &TranscriptViewItem,
        chunk: &PortableArtifactChunk<'_>,
    ) -> bool {
        let Some((reference, _, _)) = self.prepared_reference(engine, item) else {
            return false;
        };
        self.registry
            .as_ref()
            .is_some_and(|registry| registry.deliver_artifact(&reference, chunk))
    }
}

/// The accepted visual of a decoded image: reserved blank rows and one placement over them.
fn image_visual(image: &DecodedImage, fallback_title: &str) -> AcceptedPluginVisual {
    let rows = (0..image.rows).map(|_| Line::raw("")).collect::<Vec<_>>();
    AcceptedPluginVisual {
        adapter: "bcode.web:artifact-image".to_owned(),
        rows: Arc::from(rows),
        fallback_title: Some(fallback_title.to_owned()),
        images: Arc::from(vec![image.image.clone()]),
        ..AcceptedPluginVisual::default()
    }
}

/// `output` of adapter `reference` (`<plugin>/<adapter>`) as an accepted visual, with producer
/// scoped identities. The revision is set by the caller.
fn accepted_visual(
    reference: &str,
    output: PortableVisualOutput,
    fallback_title: &str,
) -> AcceptedPluginVisual {
    let (plugin, adapter) = reference.split_once('/').unwrap_or((reference, ""));
    let scoped = |identity: &str| format!("{plugin}:{identity}");
    let PortableVisualOutput {
        rows,
        images,
        actions,
    } = output;
    let anchors = rows
        .anchors
        .iter()
        .map(|anchor| VisualAnchor {
            row: anchor.row,
            key: scoped(&anchor.key),
            source: anchor.source.as_ref().map(|source| VisualSourceRange {
                identity: scoped(&source.identity),
                start: u64::try_from(source.start).unwrap_or(u64::MAX),
                end: u64::try_from(source.end).unwrap_or(u64::MAX),
            }),
        })
        .collect();
    let selection = rows
        .selection
        .iter()
        .filter(|(_, row)| row.is_valid())
        .map(|(index, row)| {
            (
                *index,
                VisualSelectionRow {
                    identity: scoped(&row.identity),
                    byte_start: row.byte_start,
                    text: row.text.clone(),
                    cells: row
                        .cells
                        .iter()
                        .map(|cell| VisualSelectionCell {
                            column: cell.column,
                            width: cell.width,
                            bytes: cell.bytes.clone(),
                        })
                        .collect(),
                },
            )
        })
        .collect::<Vec<_>>();
    let correspondence = VisualCorrespondence::new(anchors, selection, rows.rows.len());
    AcceptedPluginVisual {
        adapter: format!("{plugin}:{adapter}"),
        rows: Arc::from(rows.rows),
        fallback_title: Some(fallback_title.to_owned()),
        correspondence: Arc::new(correspondence),
        images: Arc::from(images),
        actions: Arc::from(actions),
        ..AcceptedPluginVisual::default()
    }
}
