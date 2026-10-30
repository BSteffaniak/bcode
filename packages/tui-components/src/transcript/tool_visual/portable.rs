//! Statically linked producer presentation, with no service or native-library execution.
//!
//! This is a BMUX boundary API, not a portable model/ABI. A producer may implement
//! it in a WASM-compatible implementation module and reuse that module natively.
//! Registration never implies that a native dynamic adapter can execute in WASM.

use super::CanonicalPluginVisual;
use crate::transcript::{actions::TranscriptActionRegion, transcript_layout::TranscriptLayoutRows};
use std::collections::BTreeSet;

/// Explicit selection policy captured when constructing one immutable registry.
#[derive(Debug, Clone, Default)]
pub struct PortableVisualPolicy {
    /// Only these enabled producers may contribute implementations.
    pub enabled_plugins: BTreeSet<String>,
    /// Stable `plugin/adapter` references, in descending user preference order.
    pub preferred: Vec<String>,
    /// Disabled adapter references, removed before compatibility selection.
    pub disabled: BTreeSet<String>,
}

/// Producer declaration of one statically available implementation.
#[derive(Debug, Clone)]
pub struct PortableVisualDescriptor {
    /// Plugin owning the implementation.
    pub plugin_id: String,
    /// Stable adapter ID within that plugin.
    pub adapter_id: String,
    /// Exact schema/version pairs understood by this implementation.
    pub schemas: BTreeSet<(String, u32)>,
    /// Higher manifest priorities sort first.
    pub priority: i32,
    /// Prefer this implementation for its own producer after manifest priority.
    pub producer_default: bool,
}

impl PortableVisualDescriptor {
    /// Stable configured adapter reference.
    #[must_use]
    pub fn reference(&self) -> String {
        format!("{}/{}", self.plugin_id, self.adapter_id)
    }
}

/// Host presentation inputs. Canonical semantic content lives in the visual.
pub struct PortableVisualContext<'a> {
    /// Total available item width; producer chrome must fit inside it.
    pub width: u16,
    /// Theme resolved at the BMUX boundary, never in portable model contracts.
    pub theme: Option<&'a bmux_tui_components::theme::ComponentTheme>,
}

/// Complete producer output, accepted atomically with source geometry.
#[derive(Debug, Clone, Default)]
pub struct PortableVisualOutput {
    /// Rows and their original source mappings/anchors, never server-made cells.
    pub rows: TranscriptLayoutRows,
    /// Item-local image geometry captured with the rows.
    pub images: Vec<bmux_tui::image::ImagePlacement>,
    /// Narrow intents only; registering an adapter does not authorize execution.
    pub actions: Vec<TranscriptActionRegion>,
}

impl PortableVisualOutput {
    /// Attach bounded copy intents from the exact retained source-cell geometry.
    /// Raster text is never used as copy authority. Unmapped decoration is inert.
    #[must_use]
    pub fn with_source_copy_actions(mut self) -> Self {
        if let TranscriptLayoutRows::Selected { selection, .. } = &self.rows {
            for (row, source) in selection {
                let Ok(y) = u16::try_from(*row) else { continue };
                if !source.is_valid() {
                    continue;
                }
                for (index, cell) in source.cells.iter().enumerate() {
                    if self.actions.len() >= 1024 {
                        return self;
                    }
                    let start = cell.bytes.start - source.byte_start;
                    let end = cell.bytes.end - source.byte_start;
                    let Some(text) = source.text.get(start..end).filter(|text| !text.is_empty())
                    else {
                        continue;
                    };
                    self.actions.push(TranscriptActionRegion {
                        id: format!("copy:{row}:{index}"),
                        area: bmux_tui::geometry::Rect::new(cell.column, y, cell.width, 1),
                        action: crate::transcript::actions::TranscriptAction::Copy(text.to_owned()),
                    });
                }
            }
        }
        self
    }
}

/// Narrow verified artifact delivery at the BMUX boundary, not a service call.
/// Bytes are opaque to the registry and interpreted only by the selected producer.
pub struct PortableArtifactChunk<'a> {
    /// Stable tool invocation identity.
    pub invocation_id: &'a str,
    /// Canonical artifact identity.
    pub artifact_id: &'a str,
    /// Producer-owned reference key.
    pub reference_key: &'a str,
    /// Owning producer, matched against the selected implementation.
    pub producer_plugin_id: &'a str,
    /// Exact producer schema.
    pub schema: &'a str,
    /// Exact producer schema version.
    pub schema_version: u32,
    /// Declared artifact content type.
    pub content_type: Option<&'a str>,
    /// Byte offset inside the verified artifact.
    pub offset: u64,
    /// Artifact byte length.
    pub total_bytes: u64,
    /// Immutable artifact revision.
    pub revision: u64,
    /// Whether the artifact is complete.
    pub finalized: bool,
    /// Bounded bytes obtained through the host's artifact capability.
    pub bytes: &'a [u8],
}

/// Producer-owned pure, bounded presentation. No host/service capability is passed.
///
/// Implementations must bound their own computation and allocations before returning;
/// the registry validates retained output but cannot preempt arbitrary Rust code.
/// Stateful artifact acquisition remains an explicit separate producer capability.
pub trait PortableVisualAdapter {
    /// Maximum bytes needed from this reference; `None` disables automatic fetching.
    fn artifact_preview_limit(
        &self,
        _reference_key: &str,
        _content_type: Option<&str>,
    ) -> Option<u64> {
        None
    }

    /// Accept already verified bounded artifact bytes, with no service access.
    /// The default declines producers that do not need artifact-backed presentation.
    fn artifact_chunk(&self, _chunk: &PortableArtifactChunk<'_>) -> bool {
        false
    }

    /// Render a complete snapshot, or decline malformed/inapplicable content.
    fn render(
        &self,
        visual: &CanonicalPluginVisual,
        context: &PortableVisualContext<'_>,
    ) -> Option<PortableVisualOutput>;
}

/// An explicit static producer registration; no dynamic discovery or service call.
pub struct PortableVisualRegistration {
    /// Compatibility and preference declaration.
    pub descriptor: PortableVisualDescriptor,
    /// Implementation supplied by its producer, not by the host.
    pub adapter: Box<dyn PortableVisualAdapter>,
}

/// Immutable selected adapter set. Replace it with a new host generation on reload.
pub struct PortableVisualRegistry {
    generation: u64,
    artifact_revision: std::cell::Cell<u64>,
    policy: PortableVisualPolicy,
    registrations: Vec<PortableVisualRegistration>,
}

/// Output identity retained across preparation/acceptance; fields cannot be forged.
pub struct PreparedPortableVisual {
    generation: u64,
    artifact_revision: u64,
    input: CanonicalPluginVisual,
    width: u16,
    adapter_reference: String,
    output: PortableVisualOutput,
}

impl PreparedPortableVisual {
    /// Exact implementation that produced this snapshot.
    #[must_use]
    pub fn adapter_reference(&self) -> &str {
        &self.adapter_reference
    }
}

impl PortableVisualRegistry {
    /// Construct a bounded immutable registry with an application-owned reload generation.
    ///
    /// # Errors
    /// Rejects duplicate references, empty identities/schemas or excessive registration
    /// and policy sizes. Hosts must never reuse a generation for a different registry.
    pub fn new(
        generation: u64,
        policy: PortableVisualPolicy,
        registrations: Vec<PortableVisualRegistration>,
    ) -> Result<Self, &'static str> {
        if registrations.len() > 128
            || policy.preferred.len() > 128
            || policy.disabled.len() > 128
            || policy.enabled_plugins.len() > 128
            || policy
                .preferred
                .iter()
                .chain(&policy.disabled)
                .chain(&policy.enabled_plugins)
                .any(|id| id.is_empty() || id.len() > 256)
        {
            return Err("portable visual registry exceeds policy budget");
        }
        let mut seen = BTreeSet::new();
        for registration in &registrations {
            let descriptor = &registration.descriptor;
            if descriptor.plugin_id.is_empty()
                || descriptor.adapter_id.is_empty()
                || descriptor.plugin_id.len() > 128
                || descriptor.adapter_id.len() > 128
                || descriptor.plugin_id.contains('/')
                || descriptor.adapter_id.contains('/')
                || descriptor.schemas.is_empty()
                || descriptor.schemas.len() > 64
                || descriptor.schemas.iter().any(|(schema, version)| {
                    schema.is_empty() || schema.len() > 256 || *version == 0
                })
                || !seen.insert(descriptor.reference())
            {
                return Err("invalid or duplicate portable visual registration");
            }
        }
        Ok(Self {
            generation,
            artifact_revision: std::cell::Cell::new(0),
            policy,
            registrations,
        })
    }

    /// Reload generation. The host owns uniqueness across registry replacements.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Producer artifact-state revision used for retained-output invalidation.
    #[must_use]
    pub const fn artifact_revision(&self) -> u64 {
        self.artifact_revision.get()
    }

    /// Prepare outside painting. Failed, malformed and oversized outputs advance
    /// to the next compatible candidate, using native preference semantics.
    /// No native plugin/service fallback is executed here.
    pub fn prepare(
        &self,
        visual: &CanonicalPluginVisual,
        context: &PortableVisualContext<'_>,
    ) -> Option<PreparedPortableVisual> {
        use bcode_session_models::retention::{
            ObservationRetentionLimits, ObservationRetentionUsage,
        };
        let limits = ObservationRetentionLimits {
            scalar_capacity_bytes: 256 * 1024,
            collection_entries: 8192,
            value_nodes: 16384,
            depth: 32,
            copies: 1,
        };
        if context.width == 0
            || visual.schema.len() > 256
            || visual
                .invocation_id
                .as_ref()
                .is_some_and(|id| id.len() > 1024)
            || visual
                .producer_plugin_id
                .as_ref()
                .is_some_and(|id| id.len() > 128)
            || visual.title.as_ref().is_some_and(|s| s.len() > 64 * 1024)
            || visual
                .subtitle
                .as_ref()
                .is_some_and(|s| s.len() > 64 * 1024)
            || ObservationRetentionUsage::json(&visual.payload, limits).is_err()
        {
            return None;
        }
        let mut candidates: Vec<_> = self
            .registrations
            .iter()
            .filter(|registration| {
                let descriptor = &registration.descriptor;
                self.policy.enabled_plugins.contains(&descriptor.plugin_id)
                    && !self.policy.disabled.contains(&descriptor.reference())
                    && descriptor
                        .schemas
                        .contains(&(visual.schema.clone(), visual.schema_version))
            })
            .collect();
        candidates.sort_by_key(|registration| {
            let descriptor = &registration.descriptor;
            (
                self.policy
                    .preferred
                    .iter()
                    .position(|id| *id == descriptor.reference())
                    .unwrap_or(usize::MAX),
                std::cmp::Reverse(descriptor.priority),
                std::cmp::Reverse(
                    descriptor.producer_default
                        && visual.producer_plugin_id.as_ref() == Some(&descriptor.plugin_id),
                ),
                &descriptor.plugin_id,
                &descriptor.adapter_id,
            )
        });
        for registration in candidates {
            let Some(output) = registration.adapter.render(visual, context) else {
                continue;
            };
            if !super::super::accepted::visual_supported(&output.rows)
                || !super::super::accepted::images_supported(
                    &output.images,
                    context.width,
                    output.rows.len(),
                )
                || !crate::transcript::actions::supported(
                    &output.actions,
                    context.width,
                    output.rows.len(),
                )
                || crate::transcript::actions::retained_bytes(&output.actions) > 4 * 1024 * 1024
            {
                continue;
            }
            return Some(PreparedPortableVisual {
                generation: self.generation,
                artifact_revision: self.artifact_revision.get(),
                input: visual.clone(),
                width: context.width,
                adapter_reference: registration.descriptor.reference(),
                output,
            });
        }
        None
    }

    /// Ask only the selected enabled producer for its bounded acquisition budget.
    pub fn artifact_preview_limit(
        &self,
        visual: &CanonicalPluginVisual,
        context: &PortableVisualContext<'_>,
        reference_key: &str,
        content_type: Option<&str>,
    ) -> Option<u64> {
        let prepared = self.prepare(visual, context)?;
        self.registrations
            .iter()
            .find(|registration| registration.descriptor.reference() == prepared.adapter_reference)
            .and_then(|registration| {
                registration
                    .adapter
                    .artifact_preview_limit(reference_key, content_type)
            })
            .map(|limit| limit.min(64 * 1024))
    }

    /// Resolve only an action from a freshly accepted producer projection.
    /// Callers must additionally check their committed item identity and revision.
    pub fn translate_action(
        &self,
        visual: &CanonicalPluginVisual,
        context: &PortableVisualContext<'_>,
        action_id: &str,
    ) -> Option<crate::transcript::actions::TranscriptAction> {
        if action_id.len() > 1024 {
            return None;
        }
        self.prepare(visual, context)?
            .output
            .actions
            .into_iter()
            .find(|region| region.id == action_id)
            .map(|region| region.action)
    }

    /// Deliver a bounded verified artifact chunk only to an enabled compatible
    /// static producer. Successful delivery invalidates all older prepared output
    /// through the registry's artifact-state revision.
    #[must_use]
    pub fn deliver_artifact(
        &self,
        adapter_reference: &str,
        chunk: &PortableArtifactChunk<'_>,
    ) -> bool {
        if chunk.bytes.len() > 64 * 1024
            || chunk.invocation_id.len() > 1024
            || chunk.artifact_id.is_empty()
            || chunk.artifact_id.len() > 1024
            || chunk.reference_key.len() > 512
            || chunk.producer_plugin_id.len() > 128
            || chunk.schema.len() > 256
            || chunk.content_type.is_some_and(|t| t.len() > 256)
            || chunk
                .offset
                .checked_add(chunk.bytes.len() as u64)
                .is_none_or(|end| end > chunk.total_bytes)
            || self.policy.disabled.contains(adapter_reference)
        {
            return false;
        }
        let accepted = self
            .registrations
            .iter()
            .find(|registration| {
                let descriptor = &registration.descriptor;
                descriptor.reference() == adapter_reference
                    && descriptor.plugin_id == chunk.producer_plugin_id
                    && self.policy.enabled_plugins.contains(&descriptor.plugin_id)
                    && descriptor
                        .schemas
                        .contains(&(chunk.schema.to_owned(), chunk.schema_version))
            })
            .is_some_and(|registration| registration.adapter.artifact_chunk(chunk));
        if accepted {
            self.artifact_revision
                .set(self.artifact_revision.get().wrapping_add(1));
        }
        accepted
    }

    /// Consume only an exact-current completion. Theme/presentation generations
    /// are additionally guarded by `AcceptedTranscript::accept_prepared_visual`.
    pub fn resolve(
        &self,
        prepared: PreparedPortableVisual,
        current: &CanonicalPluginVisual,
        width: u16,
    ) -> Option<PortableVisualOutput> {
        (prepared.generation == self.generation
            && prepared.artifact_revision == self.artifact_revision.get()
            && prepared.input == *current
            && prepared.width == width)
            .then_some(prepared.output)
    }
}

#[cfg(test)]
#[path = "portable_tests.rs"]
mod tests;
