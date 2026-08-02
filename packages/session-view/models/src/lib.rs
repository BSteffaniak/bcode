#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Renderer-neutral session view models for Bcode renderers.
//!
//! These types are intentionally presentation-semantic instead of renderer-specific: terminal,
//! web, and future renderers should be able to consume them without depending on terminal frames,
//! browser DOM primitives, daemon clients, or application orchestration.

use bcode_session_models::{
    ClientId, ModelTurnOutcome, RequestContextOccupancy, RuntimeWorkKind, RuntimeWorkStatus,
    SessionForkResult, SessionId, SessionSummary, SessionTokenUsage, ToolArtifact,
    ToolInvocationResult, WorkId,
};
pub use bcode_session_models::{
    ToolPresentationIdentity, ToolPresentationRetention, ToolPresentationScopeState,
    ToolPresentationUpdate, ToolPresentationUpdateError, ToolPresentationUpdateScope,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

#[cfg(test)]
mod renderer_fixtures;
#[cfg(test)]
mod tests;

/// Easing curve used to expose accepted live text to renderer-neutral views.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamingInterpolationCurve {
    /// Expose text at a constant rate.
    #[default]
    Linear,
    /// Start slowly and accelerate toward the presentation deadline.
    EaseIn,
    /// Start quickly and decelerate toward the presentation deadline.
    EaseOut,
    /// Start and finish slowly around a faster midpoint.
    EaseInOut,
}

/// Renderer-neutral policy for smoothing accepted live text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamingPresentationPolicy {
    /// Whether accepted live text should be exposed progressively.
    pub enabled: bool,
    /// Curve used to distribute visible progress over the lag budget.
    pub curve: StreamingInterpolationCurve,
    /// Maximum nominal age of hidden accepted text in milliseconds.
    pub max_lag_ms: u64,
}

impl StreamingPresentationPolicy {
    /// Default maximum nominal presentation lag.
    pub const DEFAULT_MAX_LAG_MS: u64 = 40;
    /// Largest accepted presentation lag from configuration.
    pub const MAX_LAG_MS: u64 = 250;

    /// Return an immediate whole-chunk presentation policy.
    #[must_use]
    pub const fn immediate() -> Self {
        Self {
            enabled: false,
            curve: StreamingInterpolationCurve::Linear,
            max_lag_ms: 0,
        }
    }

    /// Return whether this policy exposes accepted text immediately.
    #[must_use]
    pub const fn is_immediate(self) -> bool {
        !self.enabled || self.max_lag_ms == 0
    }

    /// Return this policy with its lag bounded to the supported range.
    #[must_use]
    pub const fn normalized(mut self) -> Self {
        if self.max_lag_ms > Self::MAX_LAG_MS {
            self.max_lag_ms = Self::MAX_LAG_MS;
        }
        self
    }
}

impl Default for StreamingPresentationPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            curve: StreamingInterpolationCurve::Linear,
            max_lag_ms: Self::DEFAULT_MAX_LAG_MS,
        }
    }
}

/// Monotonic revision for renderer-visible view state.
pub type ViewRevision = u64;

/// Stable, source-derived identifier for a transcript item.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TranscriptViewItemId(String);

impl TranscriptViewItemId {
    /// Create an identifier from a stable namespaced key.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Create an identifier for an event-owned transcript item.
    #[must_use]
    pub fn event(sequence: u64) -> Self {
        Self(format!("event:{sequence}"))
    }

    /// Create an identifier for a provider-reported reasoning activity.
    #[must_use]
    pub fn reasoning(turn_id: &str, activity_id: &str) -> Self {
        Self(format!("reasoning-turn:{turn_id}:{activity_id}"))
    }

    /// Create an identifier for a tool invocation.
    #[must_use]
    pub fn tool(tool_call_id: &str) -> Self {
        Self(format!("tool:{tool_call_id}"))
    }

    /// Create an identifier for a live tool request draft generation.
    #[must_use]
    pub fn tool_request_draft(tool_call_id: &str, generation: u64) -> Self {
        Self(format!("tool-draft:{tool_call_id}:{generation}"))
    }

    /// Create an identifier for historical tool request context retained after completion.
    #[must_use]
    pub fn tool_request(tool_call_id: &str) -> Self {
        Self(format!("tool-request:{tool_call_id}"))
    }

    /// Create an identifier for a supplemental tool presentation.
    #[must_use]
    pub fn tool_supplemental(tool_call_id: &str, supplemental_id: &str) -> Self {
        Self(format!(
            "tool:{tool_call_id}:supplemental:{supplemental_id}"
        ))
    }

    /// Create an identifier for a legacy semantic presentation slot owned by a tool invocation.
    ///
    /// This decode-compatibility constructor must not be used by new producers. Historical
    /// request/progress/result placements map to the invocation primary identity, while explicit
    /// supplemental placements retain their independent stable identity.
    ///
    /// # Panics
    ///
    /// Panics when `placement` is supplemental and `supplemental_id` is absent.
    #[must_use]
    pub fn tool_presentation_slot(
        tool_call_id: &str,
        placement: bcode_session_models::ToolContributionPlacement,
        supplemental_id: Option<&str>,
    ) -> Self {
        match placement {
            bcode_session_models::ToolContributionPlacement::Request
            | bcode_session_models::ToolContributionPlacement::Progress
            | bcode_session_models::ToolContributionPlacement::Result => Self::tool(tool_call_id),
            bcode_session_models::ToolContributionPlacement::Supplemental => {
                Self::tool_supplemental(
                    tool_call_id,
                    supplemental_id
                        .expect("supplemental presentation slots require stable identity"),
                )
            }
            bcode_session_models::ToolContributionPlacement::Hidden => {
                Self(format!("tool:{tool_call_id}:hidden"))
            }
        }
    }

    /// Create an identifier for a permission request.
    #[must_use]
    pub fn permission(permission_id: &str) -> Self {
        Self(format!("permission:{permission_id}"))
    }

    /// Create an identifier for an interaction.
    #[must_use]
    pub fn interaction(interaction_id: &str) -> Self {
        Self(format!("interaction:{interaction_id}"))
    }

    /// Return the stable identifier value.
    #[must_use]
    pub fn get(&self) -> &str {
        &self.0
    }
}

/// Renderer-neutral daemon/session connection state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionConnectionViewStatus {
    /// No daemon connection or session attachment has been established.
    #[default]
    Disconnected,
    /// The daemon is connected, but no persisted session is attached.
    Connected,
    /// A persisted session is attached and receiving updates.
    Attached,
    /// The host is attempting to restore an interrupted session watch.
    Reconnecting,
    /// The host is rebuilding an authoritative view after detecting stale or missing updates.
    Resyncing,
    /// The connection failed and requires user attention.
    Error(String),
}

/// Renderer-neutral persistent session catalog state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionCatalogViewStatus {
    /// Discovery has not started yet.
    #[default]
    NotStarted,
    /// Discovery is in progress.
    Loading,
    /// Discovery completed successfully.
    Loaded,
    /// Discovery produced partial results.
    Degraded(String),
    /// Discovery failed.
    Failed(String),
}

/// Renderer-neutral user-facing application notice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionViewNotice {
    /// Semantic severity.
    pub level: SessionViewNoticeLevel,
    /// Human-readable message without transport implementation detail.
    pub message: String,
}

/// Severity for a renderer-neutral application notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionViewNoticeLevel {
    /// Informational status.
    Info,
    /// Degraded state that may recover or require attention.
    Warning,
    /// Action or connection failure.
    Error,
}

/// Snapshot of the renderer-neutral state for one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionViewSnapshot {
    /// Snapshot schema version.
    pub schema_version: u16,
    /// Current view revision.
    pub revision: ViewRevision,
    /// Active session identifier, when attached to a persisted session.
    pub session_id: Option<SessionId>,
    /// Human-readable session title.
    pub title: Option<String>,
    /// Current session working directory, when known.
    pub working_directory: Option<PathBuf>,
    /// Last source event sequence included in this snapshot.
    pub latest_sequence: Option<u64>,
    /// Current daemon/session connection state.
    #[serde(default)]
    pub connection_status: SessionConnectionViewStatus,
    /// Persistent session catalog state for user-facing loading/degraded/error presentation.
    #[serde(default)]
    pub catalog_status: SessionCatalogViewStatus,
    /// Human-readable action or connection notice, when the host needs to surface one.
    #[serde(default)]
    pub notice: Option<SessionViewNotice>,
    /// Renderer-neutral transcript items.
    pub transcript: TranscriptViewDocument,
    /// Renderer-neutral integrity state for active ordered text streams.
    #[serde(default)]
    pub text_streams: BTreeMap<TranscriptViewItemId, TextStreamViewState>,
    /// Active opaque contributions keyed by invocation and contribution identity.
    #[serde(default)]
    pub contributions: BTreeMap<String, bcode_session_models::ToolContributionEvent>,
    /// Active renderer-neutral exchange requests keyed by invocation and exchange identity.
    #[serde(default)]
    pub active_exchanges: BTreeMap<String, bcode_session_models::ToolExchangeRequest>,
    /// Active invocation lifecycle keyed by invocation identifier.
    #[serde(default)]
    pub active_invocations: BTreeMap<String, bcode_session_models::ToolInvocationLifecycleEvent>,
    /// Active or recently observed tool invocations keyed by provider tool call id.
    pub tools: BTreeMap<String, ToolInvocationView>,
    /// Pending permission requests visible to renderers.
    pub permissions: Vec<PermissionView>,
    /// Runtime work entries visible to renderers.
    pub runtime_work: Vec<RuntimeWorkView>,
    /// Active skills selected for the session.
    #[serde(default)]
    pub active_skills: BTreeSet<String>,
    /// Latest plugin-owned status notes keyed by plugin and note identity.
    #[serde(default)]
    pub plugin_status: BTreeMap<String, PluginStatusView>,
    /// Composer state.
    pub composer: ComposerViewState,
    /// Current reasoning/thinking display state.
    pub thinking: ThinkingViewState,
    /// Renderer-neutral runtime/model/agent/turn state.
    #[serde(default)]
    pub runtime: SessionRuntimeViewState,
    /// Known interactive requests.
    pub interactions: Vec<InteractionViewSummary>,
    /// Session summary metadata, when supplied by the daemon/catalog.
    pub session_summary: Option<SessionSummary>,
}

impl SessionViewSnapshot {
    /// Current snapshot schema version.
    pub const SCHEMA_VERSION: u16 = 16;

    /// Create an empty snapshot.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            revision: 0,
            session_id: None,
            title: None,
            working_directory: None,
            latest_sequence: None,
            connection_status: SessionConnectionViewStatus::default(),
            catalog_status: SessionCatalogViewStatus::default(),
            notice: None,
            transcript: TranscriptViewDocument::default(),
            text_streams: BTreeMap::new(),
            contributions: BTreeMap::new(),
            active_exchanges: BTreeMap::new(),
            active_invocations: BTreeMap::new(),
            tools: BTreeMap::new(),
            permissions: Vec::new(),
            runtime_work: Vec::new(),
            active_skills: BTreeSet::new(),
            plugin_status: BTreeMap::new(),
            composer: ComposerViewState::default(),
            thinking: ThinkingViewState::default(),
            runtime: SessionRuntimeViewState::default(),
            interactions: Vec::new(),
            session_summary: None,
        }
    }

    /// Apply a renderer-neutral patch to this snapshot.
    ///
    /// Full snapshot resets remain the correctness fallback: when `patch.reset` is present, the
    /// entire snapshot is replaced after base-revision validation. Otherwise, transcript operations
    /// are applied and collection fields in the patch are upserted.
    ///
    /// # Errors
    ///
    /// Returns an error when the snapshot, patch, or reset snapshot uses an unsupported schema
    /// version; when the snapshot or transcript revision does not match the patch base; or when a
    /// transcript operation references a missing or duplicate item.
    pub fn apply_patch(
        &mut self,
        patch: &SessionViewPatch,
    ) -> Result<(), TranscriptViewPatchError> {
        if self.schema_version != Self::SCHEMA_VERSION {
            return Err(TranscriptViewPatchError::UnsupportedSnapshotSchemaVersion {
                actual: self.schema_version,
                expected: Self::SCHEMA_VERSION,
            });
        }
        if patch.schema_version != SessionViewPatch::SCHEMA_VERSION {
            return Err(TranscriptViewPatchError::UnsupportedPatchSchemaVersion {
                actual: patch.schema_version,
                expected: SessionViewPatch::SCHEMA_VERSION,
            });
        }
        if self.revision != patch.base_revision {
            return Err(TranscriptViewPatchError::RevisionMismatch {
                expected: patch.base_revision,
                actual: self.revision,
            });
        }
        if let Some(reset) = &patch.reset {
            if reset.schema_version != Self::SCHEMA_VERSION {
                return Err(TranscriptViewPatchError::UnsupportedSnapshotSchemaVersion {
                    actual: reset.schema_version,
                    expected: Self::SCHEMA_VERSION,
                });
            }
            if reset.revision != patch.revision {
                return Err(TranscriptViewPatchError::ResetRevisionMismatch {
                    expected: patch.revision,
                    actual: reset.revision,
                });
            }
            *self = reset.as_ref().clone();
            return Ok(());
        }

        self.transcript.apply_patch(patch)?;
        if let Some(latest_sequence) = patch.latest_sequence {
            self.latest_sequence = Some(latest_sequence);
        }
        for key in &patch.removed_contributions {
            self.contributions.remove(key);
        }
        self.contributions.extend(patch.contributions.clone());
        self.active_exchanges.extend(patch.active_exchanges.clone());
        self.active_invocations
            .extend(patch.active_invocations.clone());
        self.tools.extend(patch.tools.clone());
        upsert_permissions(&mut self.permissions, &patch.permissions);
        if let Some(runtime_work) = &patch.runtime_work {
            self.runtime_work.clone_from(runtime_work);
        }
        if let Some(active_skills) = &patch.active_skills {
            self.active_skills.clone_from(active_skills);
        }
        self.plugin_status.extend(patch.plugin_status.clone());
        if let Some(composer) = &patch.composer {
            self.composer = composer.clone();
        }
        if let Some(thinking) = &patch.thinking {
            self.thinking = thinking.clone();
        }
        if let Some(runtime) = &patch.runtime {
            self.runtime = runtime.clone();
        }
        upsert_interactions(&mut self.interactions, &patch.interactions);
        self.revision = patch.revision;
        Ok(())
    }
}

/// Incremental renderer-neutral session view update prepared for future patch streaming.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionViewPatch {
    /// Patch schema version.
    pub schema_version: u16,
    /// Revision before applying this patch.
    pub base_revision: ViewRevision,
    /// Revision after applying this patch.
    pub revision: ViewRevision,
    /// Target session identifier, when known.
    pub session_id: Option<SessionId>,
    /// Latest durable sequence included in the target snapshot, when it advanced.
    #[serde(default)]
    pub latest_sequence: Option<u64>,
    /// Full snapshot reset used when an incremental patch would not be correctness-preserving.
    #[serde(default)]
    pub reset: Option<Box<SessionViewSnapshot>>,
    /// Transcript item operations.
    pub transcript: Vec<TranscriptViewPatchOp>,
    /// Opaque contribution updates keyed by invocation and contribution identity.
    pub contributions: BTreeMap<String, bcode_session_models::ToolContributionEvent>,
    /// Opaque contribution keys removed from current transient state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_contributions: Vec<String>,
    /// Active exchange updates keyed by invocation and exchange identity.
    pub active_exchanges: BTreeMap<String, bcode_session_models::ToolExchangeRequest>,
    /// Invocation lifecycle updates keyed by invocation identifier.
    pub active_invocations: BTreeMap<String, bcode_session_models::ToolInvocationLifecycleEvent>,
    /// Tool updates keyed by tool call id.
    pub tools: BTreeMap<String, ToolInvocationView>,
    /// Permission updates.
    pub permissions: Vec<PermissionView>,
    /// Active runtime-work replacement, when changed.
    #[serde(default)]
    pub runtime_work: Option<Vec<RuntimeWorkView>>,
    /// Active skill-set replacement, when changed.
    pub active_skills: Option<BTreeSet<String>>,
    /// Plugin status updates keyed by plugin and note identity.
    pub plugin_status: BTreeMap<String, PluginStatusView>,
    /// Composer replacement, when changed.
    pub composer: Option<ComposerViewState>,
    /// Thinking state replacement, when changed.
    pub thinking: Option<ThinkingViewState>,
    /// Runtime/model/agent/turn state replacement, when changed.
    pub runtime: Option<SessionRuntimeViewState>,
    /// Interaction updates.
    pub interactions: Vec<InteractionViewSummary>,
}

impl SessionViewPatch {
    /// Current patch schema version.
    pub const SCHEMA_VERSION: u16 = 16;

    /// Create an empty patch between two revisions.
    #[must_use]
    pub const fn empty(base_revision: ViewRevision, revision: ViewRevision) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            base_revision,
            revision,
            session_id: None,
            latest_sequence: None,
            reset: None,
            transcript: Vec::new(),
            contributions: BTreeMap::new(),
            removed_contributions: Vec::new(),
            active_exchanges: BTreeMap::new(),
            active_invocations: BTreeMap::new(),
            tools: BTreeMap::new(),
            permissions: Vec::new(),
            runtime_work: None,
            active_skills: None,
            plugin_status: BTreeMap::new(),
            composer: None,
            thinking: None,
            runtime: None,
            interactions: Vec::new(),
        }
    }

    /// Build a transcript-only patch between two bounded transcript documents.
    ///
    /// This helper keeps full-snapshot correctness as the baseline: it emits item-level append,
    /// replace, and remove operations only when the next document preserves the same bounded-window
    /// metadata, retained item ordering is unchanged, and newly inserted identities append after
    /// retained items. Otherwise it falls back to a transcript reset carrying the complete next
    /// document.
    #[must_use]
    pub fn transcript_between(
        base_revision: ViewRevision,
        revision: ViewRevision,
        session_id: Option<SessionId>,
        base: &TranscriptViewDocument,
        next: &TranscriptViewDocument,
    ) -> Self {
        let mut patch = Self::empty(base_revision, revision);
        patch.session_id = session_id;
        patch.transcript = transcript_patch_ops(base, next);
        patch
    }

    /// Build a correctness-preserving patch between two snapshots.
    ///
    /// Transcript item operations remain incremental when collection changes are additive or
    /// replace existing keyed entries. Changes that require deletion, reordering, or replacement of
    /// non-keyed collections fall back to a complete snapshot reset.
    #[must_use]
    pub fn between_snapshots(base: &SessionViewSnapshot, next: &SessionViewSnapshot) -> Self {
        let mut patch = Self::transcript_between(
            base.revision,
            next.revision,
            next.session_id,
            &base.transcript,
            &next.transcript,
        );
        if snapshot_requires_reset(base, next) {
            patch.transcript.clear();
            patch.reset = Some(Box::new(next.clone()));
            return patch;
        }
        if base.latest_sequence != next.latest_sequence {
            patch.latest_sequence = next.latest_sequence;
        }
        patch.contributions = changed_map_entries(&base.contributions, &next.contributions);
        patch.removed_contributions = removed_map_keys(&base.contributions, &next.contributions);
        patch.active_exchanges =
            changed_map_entries(&base.active_exchanges, &next.active_exchanges);
        patch.active_invocations =
            changed_map_entries(&base.active_invocations, &next.active_invocations);
        patch.tools = changed_map_entries(&base.tools, &next.tools);
        if base.runtime_work != next.runtime_work {
            patch.runtime_work = Some(next.runtime_work.clone());
        }
        if base.runtime != next.runtime {
            patch.runtime = Some(next.runtime.clone());
        }
        patch.plugin_status = changed_map_entries(&base.plugin_status, &next.plugin_status);
        patch
    }
}

/// Error applying transcript patch operations to a bounded transcript document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptViewPatchError {
    /// The current or reset snapshot schema is unsupported.
    UnsupportedSnapshotSchemaVersion {
        /// Schema version carried by the snapshot.
        actual: u16,
        /// Schema version supported by this build.
        expected: u16,
    },
    /// The patch schema is unsupported.
    UnsupportedPatchSchemaVersion {
        /// Schema version carried by the patch.
        actual: u16,
        /// Schema version supported by this build.
        expected: u16,
    },
    /// The document revision did not match the patch base revision.
    RevisionMismatch {
        /// Revision required by the patch.
        expected: ViewRevision,
        /// Current document revision.
        actual: ViewRevision,
    },
    /// A reset operation carried a document or snapshot whose revision did not match the patch.
    ResetRevisionMismatch {
        /// Revision required by the patch.
        expected: ViewRevision,
        /// Revision carried by the reset payload.
        actual: ViewRevision,
    },
    /// A replace or remove operation referenced an item that is not present.
    MissingItem {
        /// Missing item identifier.
        id: TranscriptViewItemId,
    },
    /// A replace operation did not advance the existing item revision.
    NonMonotonicItemRevision {
        /// Item whose replacement revision was stale or unchanged.
        id: TranscriptViewItemId,
        /// Existing accepted item revision.
        current: ViewRevision,
        /// Revision carried by the replacement.
        replacement: ViewRevision,
    },
    /// An append operation attempted to add an item that is already present.
    DuplicateItem {
        /// Duplicate item identifier.
        id: TranscriptViewItemId,
    },
}

/// Transcript patch operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TranscriptViewPatchOp {
    /// Append a new transcript item.
    Append { item: TranscriptViewItem },
    /// Replace an existing transcript item by id.
    Replace { item: TranscriptViewItem },
    /// Remove a transcript item by id using a monotonic tombstone revision.
    Remove {
        id: TranscriptViewItemId,
        revision: ViewRevision,
    },
    /// Replace the entire bounded transcript window.
    Reset { document: TranscriptViewDocument },
}

/// Renderer-neutral transcript document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptViewDocument {
    /// Document revision.
    pub revision: ViewRevision,
    /// Ordered transcript items.
    pub items: Vec<TranscriptViewItem>,
    /// First source event sequence covered by this bounded window.
    #[serde(default)]
    pub source_start_sequence: Option<u64>,
    /// Last source event sequence covered by this bounded window.
    #[serde(default)]
    pub source_end_sequence: Option<u64>,
    /// Whether older history exists before this document window.
    pub has_older_history: bool,
    /// Whether newer history exists after this document window.
    pub has_newer_history: bool,
}

impl TranscriptViewDocument {
    /// Apply transcript operations from a `SessionViewPatch`.
    ///
    /// This updates only transcript document state. Renderers must still treat full snapshots as the
    /// correctness baseline and reset from a snapshot whenever patch ordering, revision continuity,
    /// or transport reliability is uncertain.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    ///
    /// * the document revision does not match `patch.base_revision`
    /// * a replace or remove operation targets a missing item
    /// * an append operation would duplicate an existing item id
    pub fn apply_patch(
        &mut self,
        patch: &SessionViewPatch,
    ) -> Result<(), TranscriptViewPatchError> {
        if self.revision != patch.base_revision {
            return Err(TranscriptViewPatchError::RevisionMismatch {
                expected: patch.base_revision,
                actual: self.revision,
            });
        }
        for operation in &patch.transcript {
            self.apply_patch_operation(operation, patch.revision)?;
        }
        self.revision = patch.revision;
        self.refresh_source_bounds();
        Ok(())
    }

    fn apply_patch_operation(
        &mut self,
        operation: &TranscriptViewPatchOp,
        target_revision: ViewRevision,
    ) -> Result<(), TranscriptViewPatchError> {
        match operation {
            TranscriptViewPatchOp::Append { item } => self.append_patch_item(item.clone()),
            TranscriptViewPatchOp::Replace { item } => self.replace_patch_item(item.clone()),
            TranscriptViewPatchOp::Remove { id, revision } => self.remove_patch_item(id, *revision),
            TranscriptViewPatchOp::Reset { document } => {
                if document.revision != target_revision {
                    return Err(TranscriptViewPatchError::ResetRevisionMismatch {
                        expected: target_revision,
                        actual: document.revision,
                    });
                }
                *self = document.clone();
                Ok(())
            }
        }
    }

    fn append_patch_item(
        &mut self,
        item: TranscriptViewItem,
    ) -> Result<(), TranscriptViewPatchError> {
        if self.items.iter().any(|existing| existing.id == item.id) {
            return Err(TranscriptViewPatchError::DuplicateItem { id: item.id });
        }
        self.items.push(item);
        Ok(())
    }

    fn replace_patch_item(
        &mut self,
        item: TranscriptViewItem,
    ) -> Result<(), TranscriptViewPatchError> {
        let Some(existing) = self
            .items
            .iter_mut()
            .find(|existing| existing.id == item.id)
        else {
            return Err(TranscriptViewPatchError::MissingItem { id: item.id });
        };
        if item.revision <= existing.revision {
            return Err(TranscriptViewPatchError::NonMonotonicItemRevision {
                id: item.id,
                current: existing.revision,
                replacement: item.revision,
            });
        }
        *existing = item;
        Ok(())
    }

    fn remove_patch_item(
        &mut self,
        id: &TranscriptViewItemId,
        revision: ViewRevision,
    ) -> Result<(), TranscriptViewPatchError> {
        let Some(index) = self.items.iter().position(|item| item.id == *id) else {
            return Err(TranscriptViewPatchError::MissingItem { id: id.clone() });
        };
        let current = self.items[index].revision;
        if revision <= current {
            return Err(TranscriptViewPatchError::NonMonotonicItemRevision {
                id: id.clone(),
                current,
                replacement: revision,
            });
        }
        self.items.remove(index);
        Ok(())
    }

    fn refresh_source_bounds(&mut self) {
        self.source_start_sequence = self.items.iter().find_map(|item| item.sequence);
        self.source_end_sequence = self.items.iter().rev().find_map(|item| item.sequence);
    }
}

fn changed_map_entries<K, V>(base: &BTreeMap<K, V>, next: &BTreeMap<K, V>) -> BTreeMap<K, V>
where
    K: Clone + Ord,
    V: Clone + PartialEq,
{
    next.iter()
        .filter(|(key, value)| base.get(*key) != Some(*value))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn removed_map_keys<K, V>(base: &BTreeMap<K, V>, next: &BTreeMap<K, V>) -> Vec<K>
where
    K: Clone + Ord,
{
    base.keys()
        .filter(|key| !next.contains_key(*key))
        .cloned()
        .collect()
}

fn map_has_removals<K, V>(base: &BTreeMap<K, V>, next: &BTreeMap<K, V>) -> bool
where
    K: Ord,
{
    base.keys().any(|key| !next.contains_key(key))
}

fn snapshot_requires_reset(base: &SessionViewSnapshot, next: &SessionViewSnapshot) -> bool {
    base.schema_version != next.schema_version
        || base.session_id != next.session_id
        || base.title != next.title
        || base.working_directory != next.working_directory
        || map_has_removals(&base.active_exchanges, &next.active_exchanges)
        || map_has_removals(&base.active_invocations, &next.active_invocations)
        || map_has_removals(&base.tools, &next.tools)
        || base.permissions != next.permissions
        || base.active_skills != next.active_skills
        || map_has_removals(&base.plugin_status, &next.plugin_status)
        || base.composer != next.composer
        || base.thinking != next.thinking
        || base.interactions != next.interactions
        || base.session_summary != next.session_summary
}

fn upsert_permissions(target: &mut Vec<PermissionView>, updates: &[PermissionView]) {
    for update in updates {
        upsert_by(target, update.clone(), |permission| {
            permission.permission_id.as_str()
        });
    }
}

fn upsert_interactions(
    target: &mut Vec<InteractionViewSummary>,
    updates: &[InteractionViewSummary],
) {
    for update in updates {
        if let Some(existing) = target
            .iter()
            .find(|existing| existing.interaction_id == update.interaction_id)
            && existing.resolved
        {
            continue;
        }
        upsert_by(target, update.clone(), |interaction| {
            interaction.interaction_id.as_str()
        });
    }
}

fn upsert_by<T, F>(target: &mut Vec<T>, update: T, key: F)
where
    F: Fn(&T) -> &str,
{
    if let Some(existing) = target
        .iter_mut()
        .find(|existing| key(existing) == key(&update))
    {
        *existing = update;
    } else {
        target.push(update);
    }
}

fn transcript_patch_ops(
    base: &TranscriptViewDocument,
    next: &TranscriptViewDocument,
) -> Vec<TranscriptViewPatchOp> {
    if !transcript_window_metadata_matches(base, next)
        || !transcript_items_are_incrementally_compatible(base, next)
    {
        return vec![TranscriptViewPatchOp::Reset {
            document: next.clone(),
        }];
    }

    let mut operations = Vec::new();
    let next_ids = next
        .items
        .iter()
        .map(|item| item.id.clone())
        .collect::<BTreeSet<_>>();
    for item in &base.items {
        if !next_ids.contains(&item.id) {
            operations.push(TranscriptViewPatchOp::Remove {
                id: item.id.clone(),
                revision: item.revision.saturating_add(1),
            });
        }
    }
    let base_by_id = base
        .items
        .iter()
        .map(|item| (&item.id, item))
        .collect::<BTreeMap<_, _>>();
    for item in &next.items {
        match base_by_id.get(&item.id) {
            Some(existing) if *existing != item => {
                operations.push(TranscriptViewPatchOp::Replace { item: item.clone() });
            }
            Some(_) => {}
            None => operations.push(TranscriptViewPatchOp::Append { item: item.clone() }),
        }
    }
    operations
}

fn transcript_window_metadata_matches(
    base: &TranscriptViewDocument,
    next: &TranscriptViewDocument,
) -> bool {
    base.source_start_sequence == next.source_start_sequence
        && base.has_older_history == next.has_older_history
        && base.has_newer_history == next.has_newer_history
}

fn transcript_items_are_incrementally_compatible(
    base: &TranscriptViewDocument,
    next: &TranscriptViewDocument,
) -> bool {
    let base_ids = base
        .items
        .iter()
        .map(|item| item.id.clone())
        .collect::<BTreeSet<_>>();
    let next_ids = next
        .items
        .iter()
        .map(|item| item.id.clone())
        .collect::<BTreeSet<_>>();
    if base_ids.len() != base.items.len() || next_ids.len() != next.items.len() {
        return false;
    }
    let retained_base = base
        .items
        .iter()
        .filter(|item| next_ids.contains(&item.id))
        .map(|item| &item.id);
    let retained_next = next
        .items
        .iter()
        .filter(|item| base_ids.contains(&item.id))
        .map(|item| &item.id);
    if !retained_base.eq(retained_next) {
        return false;
    }
    let mut saw_new_item = false;
    for item in &next.items {
        if base_ids.contains(&item.id) {
            if saw_new_item {
                return false;
            }
        } else {
            saw_new_item = true;
        }
    }
    true
}

/// Renderer-neutral integrity state for an ordered live text stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextStreamViewState {
    /// Current stream generation.
    pub generation: u64,
    /// Last accepted operation revision.
    pub revision: u64,
    /// Total contiguous source bytes represented when healthy.
    pub accepted_bytes: usize,
    /// Whether retained text omits an earlier prefix.
    pub truncated: bool,
    /// Current reducer health/lifecycle.
    pub status: TextStreamViewStatus,
}

/// Ordered live text stream reducer status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextStreamViewStatus {
    /// All accepted operations are contiguous and current.
    Healthy,
    /// The retained checkpoint is contiguous but omits an earlier prefix.
    Incomplete,
    /// A gap or conflicting duplicate requires authoritative resync.
    Degraded,
    /// The stream reached an absorbing terminal state.
    Terminal(bcode_session_models::TextStreamTerminalStatus),
}

/// Cross-type semantic location of one transcript item within a model turn.
///
/// This refines ordering only inside a contiguous run of positioned items from the same turn.
/// Unpositioned items and items from another turn preserve canonical transcript boundaries.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TurnOutputLocation {
    /// Application model turn that owns the output unit.
    pub turn_id: String,
    /// Stable position within the turn.
    pub position: bcode_session_models::TurnOutputPosition,
}

/// Renderer-neutral transcript item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptViewItem {
    /// Stable item identifier.
    pub id: TranscriptViewItemId,
    /// Item revision.
    pub revision: ViewRevision,
    /// Source event sequence that first produced this item, when known.
    pub sequence: Option<u64>,
    /// Source event timestamp in Unix milliseconds, when known.
    pub timestamp_ms: Option<u64>,
    /// Cross-type semantic output location within a turn, when provider-authoritative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_location: Option<TurnOutputLocation>,
    /// Whether this item is currently receiving streamed updates.
    pub streaming: bool,
    /// Semantic item kind.
    pub kind: TranscriptViewItemKind,
}

/// Semantic renderer-neutral transcript item kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptViewItemKind {
    /// User-authored chat message.
    UserMessage { message: ChatMessageView },
    /// Assistant-authored chat message.
    AssistantMessage { message: ChatMessageView },
    /// Assistant reasoning/thinking content retained for legacy compatibility.
    ///
    /// This projection has already lost representation kind, part identity, order, and lifecycle.
    /// Structured producers use [`Self::ReasoningActivity`].
    ReasoningMessage { message: ChatMessageView },
    /// Provider-neutral structured reasoning activity.
    ReasoningActivity { activity: ReasoningActivityView },
    /// Tool request/result/stream block.
    ToolInvocation { tool: Box<ToolInvocationView> },
    /// Live-only provider argument assembly state.
    ToolRequestDraft { draft: ToolRequestDraftView },
    /// Historical request context retained after a tool result supersedes the active invocation row.
    ToolRequest { tool: Box<ToolInvocationView> },
    /// Permission request block.
    Permission { permission: PermissionView },
    /// Context compaction transcript note.
    Compaction { compaction: CompactionView },
    /// Interactive request block.
    Interaction { interaction: InteractionViewSummary },
    /// Skill-related transcript note.
    Skill { skill: SkillView },
    /// System/status message.
    SystemMessage { message: ChatMessageView },
    /// Opaque schema-versioned tool contribution with explicit semantic placement.
    ToolContribution {
        contribution: bcode_session_models::ToolContributionEvent,
        placement: bcode_session_models::ToolContributionPlacement,
        /// Current semantic state of the owning invocation, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        invocation: Option<Box<ToolInvocationView>>,
    },
}

/// Provider-neutral reasoning activity prepared for renderers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningActivityView {
    /// Correlated model turn.
    pub turn_id: String,
    /// Stable activity identifier within the turn.
    pub activity_id: String,
    /// Provider order within the turn.
    pub order: u32,
    /// Current activity lifecycle.
    pub status: bcode_session_models::ReasoningActivityStatus,
    /// Readable parts selected by local presentation policy.
    pub parts: Vec<bcode_session_models::ReasoningPart>,
    /// Whether opaque activity evidence exists.
    pub opaque: bool,
}

impl ReasoningActivityView {
    /// Return selected readable text with explicit part boundaries.
    #[must_use]
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .map(|part| part.text.as_str())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// Renderer-neutral context compaction transcript note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionView {
    /// Semantic compaction status.
    pub status: CompactionViewStatus,
    /// Renderer-ready compaction note text.
    pub text: String,
    /// Provider plugin identifier for provider-owned compaction, when known.
    pub provider_plugin_id: Option<String>,
    /// Model identifier for provider-owned compaction, when known.
    pub model_id: Option<String>,
}

/// Semantic status for a compaction transcript note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionViewStatus {
    /// Local context was compacted by Bcode.
    Local,
    /// Provider-owned context was compacted.
    Provider,
}

/// Renderer-neutral skill transcript note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillView {
    /// Skill identifier.
    pub skill_id: String,
    /// Semantic skill note status.
    pub status: SkillViewStatus,
    /// Renderer-ready skill note text.
    pub text: String,
}

/// Semantic status for a skill transcript note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillViewStatus {
    /// Skill was invoked.
    Invoked,
    /// Skill was suggested.
    Suggested,
    /// Skill context was loaded.
    ContextLoaded,
    /// Skill invocation failed.
    Failed,
}

/// Chat text plus renderer-neutral annotations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessageView {
    /// Plain text or markdown-compatible message content.
    pub text: String,
    /// Optional renderer-neutral role/display label suffix.
    pub display_label: Option<String>,
    /// Message format hint.
    pub format: TextFormat,
}

impl ChatMessageView {
    /// Create a markdown-compatible message.
    #[must_use]
    pub fn markdown(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            display_label: None,
            format: TextFormat::Markdown,
        }
    }

    /// Create a plain text message.
    #[must_use]
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            display_label: None,
            format: TextFormat::PlainText,
        }
    }
}

/// Renderer text format hint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextFormat {
    /// Plain text.
    PlainText,
    /// Markdown-compatible text.
    #[default]
    Markdown,
    /// JSON text.
    Json,
}

const fn default_tool_request_draft_placement() -> bcode_session_models::ToolContributionPlacement {
    bcode_session_models::ToolContributionPlacement::Request
}

/// Renderer-neutral live tool request draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRequestDraftView {
    /// Cross-type semantic output location within the turn, when provider-authoritative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_location: Option<TurnOutputLocation>,
    /// Model turn that owns this generation.
    pub turn_id: String,
    /// Provider tool-call identifier.
    pub tool_call_id: String,
    /// Model-visible tool name.
    pub tool_name: String,
    /// Plugin that owns request-draft presentation, when resolved.
    pub producer_plugin_id: Option<String>,
    /// Plugin-owned request-draft schema.
    pub schema: String,
    /// Version of `schema` used by the preview.
    pub schema_version: u32,
    /// Semantic transcript slot updated by this draft.
    #[serde(default = "default_tool_request_draft_placement")]
    pub placement: bcode_session_models::ToolContributionPlacement,
    /// Monotonic draft generation.
    pub generation: u64,
    /// Latest accepted revision.
    pub revision: u64,
    /// Total provider argument bytes observed.
    pub argument_bytes: usize,
    /// Original stream offset represented by `preview` byte zero.
    pub preview_start_offset: usize,
    /// Bounded retained UTF-8 argument preview.
    pub preview: String,
    /// Whether bytes were omitted from the retained preview.
    pub truncated: bool,
}

/// Renderer-neutral current plugin presentation attached to a tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPresentationView {
    pub producer_id: String,
    pub generation: u64,
    pub revision: u64,
    pub retention: ToolPresentationRetention,
    pub schema: String,
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<bcode_session_models::ToolContributionArtifact>,
    pub payload: serde_json::Value,
}

impl From<&ToolPresentationUpdate> for ToolPresentationView {
    fn from(update: &ToolPresentationUpdate) -> Self {
        Self {
            producer_id: update.producer_id.clone(),
            generation: update.generation,
            revision: update.revision,
            retention: update.retention,
            schema: update.schema.clone(),
            schema_version: update.schema_version,
            artifact: update.artifact.clone(),
            payload: update.payload.clone(),
        }
    }
}

/// Renderer-neutral tool invocation view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInvocationView {
    /// Provider tool call identifier.
    pub tool_call_id: String,
    /// Producer plugin id, when known.
    pub producer_plugin_id: Option<String>,
    /// Tool name, when known.
    pub tool_name: Option<String>,
    /// Raw JSON arguments requested by the model, when retained.
    pub arguments_json: Option<String>,
    /// Working directory captured for this invocation, when known.
    pub working_directory: Option<PathBuf>,
    /// Current live request-draft state for this invocation, when active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_draft: Option<ToolRequestDraftView>,
    /// Current lifecycle status.
    pub status: ToolInvocationViewStatus,
    /// Raw final text result, when finished.
    pub result_text: Option<String>,
    /// Whether the final result represents an error.
    pub is_error: Option<bool>,
    /// Semantic result, when supplied by the tool.
    pub result: Option<ToolResultView>,
    /// Current plugin-owned visual presentation, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<ToolPresentationView>,
    /// Tool timing metadata.
    pub timing: ToolTimingView,
}

/// Renderer-neutral tool invocation lifecycle status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolInvocationViewStatus {
    /// Request was observed but no stream/final result has been seen.
    #[default]
    Requested,
    /// Canonical invocation lifecycle reported the tool as running.
    Running,
    /// The invocation is active but waiting for external input or a resource.
    Waiting,
    /// Final result was observed or lifecycle completed successfully.
    Finished,
    /// The owning invocation or turn was cancelled.
    Cancelled,
    /// The invocation lifecycle completed with an error.
    Failed,
}

impl From<bcode_session_models::ToolInvocationProjectionStatus> for ToolInvocationViewStatus {
    fn from(value: bcode_session_models::ToolInvocationProjectionStatus) -> Self {
        match value {
            bcode_session_models::ToolInvocationProjectionStatus::Requested => Self::Requested,
            bcode_session_models::ToolInvocationProjectionStatus::Running => Self::Running,
            bcode_session_models::ToolInvocationProjectionStatus::Waiting => Self::Waiting,
            bcode_session_models::ToolInvocationProjectionStatus::Finished => Self::Finished,
            bcode_session_models::ToolInvocationProjectionStatus::Cancelled => Self::Cancelled,
            bcode_session_models::ToolInvocationProjectionStatus::Failed => Self::Failed,
        }
    }
}

/// Renderer-neutral tool timing metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolTimingView {
    /// Tool start time as Unix milliseconds.
    pub started_at_ms: Option<u64>,
    /// Tool finish time as Unix milliseconds.
    pub finished_at_ms: Option<u64>,
    /// Timeout duration in milliseconds, when known.
    pub timeout_ms: Option<u64>,
    /// Whether the tool timed out, when known.
    pub timed_out: Option<bool>,
    /// Final duration in milliseconds, when known.
    pub duration_ms: Option<u64>,
}

/// Renderer-neutral tool result payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultView {
    /// Plain textual result.
    Text { text: String },
    /// Structured JSON result encoded as JSON text.
    Json { value: String },
    /// Plugin-owned artifact result.
    Artifact { artifact: ToolArtifactView },
}

impl From<ToolInvocationResult> for ToolResultView {
    fn from(value: ToolInvocationResult) -> Self {
        match value {
            ToolInvocationResult::Text { text } => Self::Text { text },
            ToolInvocationResult::Json { value } => Self::Json { value },
            ToolInvocationResult::Artifact { artifact } => Self::Artifact {
                artifact: ToolArtifactView::from(*artifact),
            },
        }
    }
}

/// Renderer-neutral plugin artifact view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolArtifactView {
    /// Raw artifact data.
    pub artifact: ToolArtifact,
    /// Generic renderer payload for structured display.
    pub generic_payload: serde_json::Value,
}

impl From<ToolArtifact> for ToolArtifactView {
    fn from(artifact: ToolArtifact) -> Self {
        let generic_payload = serde_json::to_value(&artifact).unwrap_or(serde_json::Value::Null);
        Self {
            artifact,
            generic_payload,
        }
    }
}

/// Renderer-neutral authorization-batch correlation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionBatchView {
    /// Host-assigned batch identifier.
    pub batch_id: String,
    /// Zero-based provider-order call index.
    pub call_index: usize,
    /// Total calls in the authorization batch.
    pub call_count: usize,
}

/// Pending permission request visible to renderers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionView {
    /// Permission identifier.
    pub permission_id: String,
    /// Session containing the checkpoint, when supplied by authoritative hydration.
    #[serde(default)]
    pub session_id: Option<SessionId>,
    /// Associated provider tool call identifier.
    pub tool_call_id: String,
    /// Tool name.
    #[serde(default)]
    pub tool_name: String,
    /// Raw tool argument JSON.
    #[serde(default)]
    pub arguments_json: String,
    /// Complete-batch correlation, when this checkpoint belongs to a batch.
    #[serde(default)]
    pub batch: Option<PermissionBatchView>,
    /// Agent requesting permission.
    #[serde(default)]
    pub agent_id: String,
    /// Human-readable title.
    pub title: Option<String>,
    /// Policy source requesting approval.
    #[serde(default)]
    pub policy_source: Option<String>,
    /// Human-readable detail/body text.
    pub detail: Option<String>,
    /// Whether the permission has been resolved.
    pub resolved: bool,
    /// Decision, when resolved.
    pub approved: Option<bool>,
    /// Whether a remember option is available.
    pub can_remember: bool,
}

/// Latest plugin-owned status note visible to renderers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginStatusView {
    /// Plugin that owns the status.
    pub plugin_id: String,
    /// Stable note identity within the plugin/session.
    pub note_id: String,
    /// Human-readable status text.
    pub text: String,
    /// Lower values are retained before higher values in constrained layouts.
    #[serde(default)]
    pub priority: u16,
    /// Plugin-owned structured status metadata.
    pub metadata: BTreeMap<String, serde_json::Value>,
}

/// Runtime work visible to renderers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeWorkView {
    /// Work identifier.
    pub work_id: WorkId,
    /// Runtime work category.
    #[serde(default)]
    pub kind: RuntimeWorkKind,
    /// Stable human-readable work label.
    #[serde(default)]
    pub label: String,
    /// Current status.
    pub status: RuntimeWorkStatus,
    /// Whether the work accepts cancellation requests.
    #[serde(default)]
    pub cancellable: bool,
    /// Latest human-readable message.
    pub message: Option<String>,
    /// Completed units, when known.
    pub completed_units: Option<u64>,
    /// Total units, when known.
    pub total_units: Option<u64>,
    /// Last status/progress timestamp in Unix milliseconds.
    pub updated_at_ms: Option<u64>,
}

impl RuntimeWorkView {
    /// Return whether this work has reached a terminal status.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            RuntimeWorkStatus::Completed
                | RuntimeWorkStatus::Cancelled
                | RuntimeWorkStatus::Failed
                | RuntimeWorkStatus::TimedOut
        )
    }
}

/// Renderer-neutral model, agent, context, and turn state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRuntimeViewState {
    /// Selected provider plugin, when known.
    pub provider_plugin_id: Option<String>,
    /// User-facing requested model selection, when known.
    pub requested_model_id: Option<String>,
    /// Concrete effective model, when known.
    pub effective_model_id: Option<String>,
    /// Selected agent, when known.
    pub agent_id: Option<String>,
    /// Selected reasoning effort, when configured.
    pub reasoning_effort: Option<String>,
    /// Selected reasoning summary mode, when configured.
    pub reasoning_summary: Option<String>,
    /// Authoritative active request-context occupancy.
    pub context_occupancy: Option<RequestContextOccupancy>,
    /// Cumulative metered tokens observed across model usage events in the current projection.
    #[serde(default)]
    pub cumulative_metered_tokens: u64,
    /// Most recently observed model usage.
    pub latest_usage: Option<SessionTokenUsage>,
    /// Active model turn identifier, when a turn is running or cancelling.
    pub active_turn_id: Option<String>,
    /// Whether cancellation has been requested for the active turn.
    pub cancelling: bool,
    /// Most recent completed turn outcome.
    pub last_turn_outcome: Option<ModelTurnOutcome>,
    /// Most recent completed turn message, when supplied.
    pub last_turn_message: Option<String>,
    /// Current provider-stream progress, when an active stream exposed status.
    pub provider_progress: Option<ProviderProgressView>,
}

/// Renderer-neutral provider stream progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderProgressView {
    /// Model turn associated with the progress.
    pub turn_id: String,
    /// Human-readable semantic progress detail.
    pub detail: String,
    /// Scheduled retry time in Unix seconds, when waiting to retry.
    pub retry_at_unix: Option<u64>,
}

/// Composer state shared by renderers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposerViewState {
    /// Current draft text.
    pub draft: String,
    /// Whether submitting is currently allowed.
    pub can_submit: bool,
    /// Human-readable disabled reason when submit is unavailable.
    pub disabled_reason: Option<String>,
}

/// Renderer-selected reasoning presentation policy.
///
/// This policy is local to a renderer/client and never changes provider request construction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningPresentationPolicy {
    /// Display every readable representation exposed by the provider.
    #[default]
    All,
    /// Display provider summaries and milestones only.
    Summary,
    /// Display raw or detailed reasoning only.
    Raw,
    /// Hide readable reasoning while preserving neutral activity chrome.
    Hidden,
}

/// Renderer-selected readable reasoning representation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningDisplayMode {
    /// Display every readable representation exposed by the provider.
    #[default]
    All,
    /// Display provider summaries and milestones only.
    Summary,
    /// Display raw or detailed reasoning only.
    Raw,
}

/// Assistant reasoning/thinking display state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingViewState {
    /// Whether readable reasoning content should be visible.
    pub visible: bool,
    /// Renderer-selected readable reasoning representation.
    #[serde(default)]
    pub mode: ReasoningDisplayMode,
    /// Current in-flight reasoning text.
    pub active_text: Option<String>,
    /// Whether the current reasoning text is streaming.
    pub streaming: bool,
}

impl Default for ThinkingViewState {
    fn default() -> Self {
        Self {
            visible: true,
            mode: ReasoningDisplayMode::All,
            active_text: None,
            streaming: false,
        }
    }
}

/// Renderer-neutral lifecycle state for an interactive request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionViewState {
    /// Awaiting input.
    #[default]
    Pending,
    /// A semantic submission is being processed.
    Submitting,
    /// Controller validation rejected the latest input.
    ValidationError,
    /// Host or daemon action failed while preserving the pending request.
    ActionError,
    /// The request was resolved successfully.
    Resolved,
    /// The request was cancelled or dismissed.
    Cancelled,
}

/// Renderer-neutral interactive request summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionViewSummary {
    /// Interaction identifier.
    pub interaction_id: String,
    /// Producer that owns the exchange schema, when projected from a tool exchange.
    #[serde(default)]
    pub producer_id: Option<String>,
    /// Producer-owned exchange request schema, when available.
    #[serde(default)]
    pub exchange_schema: Option<String>,
    /// Version of the producer-owned exchange schema, when available.
    #[serde(default)]
    pub exchange_schema_version: Option<u32>,
    /// Interaction kind.
    pub kind: String,
    /// Associated tool call identifier, when known.
    pub tool_call_id: Option<String>,
    /// Optional title for display.
    pub title: Option<String>,
    /// Whether the interaction requires a response before the turn can continue.
    #[serde(default)]
    pub required: bool,
    /// Optional snapshot payload for generic rendering.
    pub snapshot: Option<serde_json::Value>,
    /// Current renderer-neutral lifecycle state for adjacent control status.
    #[serde(default)]
    pub state: InteractionViewState,
    /// Human-readable action or validation detail associated with the current state.
    #[serde(default)]
    pub status_detail: Option<String>,
    /// Whether the interaction has been durably resolved.
    #[serde(default)]
    pub resolved: bool,
    /// Canonical durable resolution, when resolved.
    #[serde(default)]
    pub resolution: Option<bcode_session_models::ToolExchangeResolution>,
}

/// Prompt placement semantics for renderer-neutral prompt submission.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptPlacementView {
    /// Insert the prompt at the next safe conversation boundary.
    #[default]
    Steering,
    /// Queue the prompt as a follow-up turn after the active turn finishes.
    FollowUp,
}

/// Composer draft scope for renderer-neutral draft updates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ComposerDraftViewScope {
    /// Draft belongs to a persisted session.
    Session { session_id: SessionId },
    /// Draft belongs to the unsaved draft session for the launch working directory.
    DraftSession { launch_working_directory: PathBuf },
}

/// Renderer-neutral message acceptance disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageAcceptanceDispositionView {
    /// Message was applied to the active turn as steering.
    AppliedSteering,
    /// Message was queued as a follow-up.
    QueuedFollowUp,
    /// Message was queued as a future turn.
    QueuedTurn,
    /// Message started a new turn.
    StartedTurn,
}

/// Result of executing a renderer-neutral session action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionViewActionOutcome {
    /// No response payload is required.
    None,
    /// A prompt was accepted and may have created a session.
    MessageAccepted {
        /// Session that received the message.
        session_id: SessionId,
        /// Whether the message was queued.
        queued: bool,
        /// Queue position, when queued.
        queue_position: Option<usize>,
        /// Authoritative admission disposition.
        disposition: MessageAcceptanceDispositionView,
    },
    /// Cancellation request result.
    Cancelled { cancelled: bool },
    /// Permission resolution result.
    PermissionResolved { resolved: bool },
    /// Permission batch resolution result.
    PermissionBatchResolved { resolved_count: usize },
    /// Interaction resolution result.
    InteractionResolved { resolved: bool },
    /// Session rename result.
    SessionRenamed { session: Box<SessionSummary> },
    /// Session deletion result.
    SessionDeleted { session: Box<SessionSummary> },
    /// Session fork result.
    SessionForked { fork: Box<SessionForkResult> },
    /// Session clone result.
    SessionCloned { fork: Box<SessionForkResult> },
    /// Session working-directory change result.
    WorkingDirectoryChanged { session: Box<SessionSummary> },
    /// Runtime-work cancellation request result.
    RuntimeWorkCancellationRequested { cancelled: bool },
    /// Context compaction request result.
    ContextCompacted { message: String },
}

/// Semantic renderer action shared by terminal, web, and future renderers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionViewAction {
    /// Submit a prompt for the active or specified session.
    SubmitMessage {
        /// Target session, when already attached.
        session_id: Option<SessionId>,
        /// Working directory to use when a draft/new session must be created.
        launch_working_directory: Option<PathBuf>,
        /// Prompt text.
        text: String,
        /// Prompt placement semantics.
        placement: PromptPlacementView,
        /// Provider-neutral execution options captured for this exact admitted turn.
        #[serde(default)]
        execution: Box<bcode_session_models::TurnExecutionOptions>,
    },
    /// Invoke a skill for one turn.
    InvokeSkill {
        /// Target session.
        session_id: SessionId,
        /// Skill identifier.
        skill_id: String,
        /// Invocation arguments.
        arguments: String,
        /// User-visible invocation text.
        display_text: String,
        /// Provider-neutral execution options captured for this exact admitted turn.
        #[serde(default)]
        execution: Box<bcode_session_models::TurnExecutionOptions>,
    },
    /// Cancel the active model turn.
    CancelTurn {
        /// Target session.
        session_id: SessionId,
        /// Whether queued work should also be cleared.
        clear_queue: bool,
    },
    /// Resolve a permission request.
    ResolvePermission {
        /// Permission id.
        permission_id: String,
        /// Whether the request is approved.
        approved: bool,
        /// Whether the decision should be remembered.
        remember: bool,
    },
    /// Resolve every pending permission in one authorization batch.
    ResolvePermissionBatch {
        /// Authorization batch id.
        batch_id: String,
        /// Whether the batch is approved.
        approved: bool,
    },
    /// Resolve an invocation exchange with a terminal resolution.
    ResolveExchange {
        /// Interaction id.
        interaction_id: String,
        /// Final exchange resolution.
        resolution: bcode_session_models::ToolExchangeResolution,
    },
    /// Request a switch to another session.
    SwitchSession {
        /// Target session.
        session_id: SessionId,
    },
    /// Update the local composer draft.
    UpdateDraft {
        /// Draft scope to update.
        scope: ComposerDraftViewScope,
        /// Draft text.
        text: String,
    },
    /// Set the selected model for a session.
    SetModel {
        /// Target session.
        session_id: SessionId,
        /// Provider plugin id, when explicitly selected.
        provider_plugin_id: Option<String>,
        /// Model id.
        model_id: String,
    },
    /// Set reasoning selections for a session.
    SetReasoning {
        /// Target session.
        session_id: SessionId,
        /// Reasoning effort selection.
        effort: Option<String>,
        /// Reasoning summary selection.
        summary: Option<String>,
    },
    /// Rename a session.
    RenameSession {
        /// Target session.
        session_id: SessionId,
        /// New title, or `None` to clear/reset the title according to daemon policy.
        name: Option<String>,
    },
    /// Delete a session.
    DeleteSession {
        /// Target session.
        session_id: SessionId,
    },
    /// Fork a session at an optional prompt boundary.
    ForkSession {
        /// Source session.
        session_id: SessionId,
        /// Prompt sequence to fork from.
        prompt_sequence: u64,
        /// New session name override.
        name: Option<String>,
    },
    /// Clone a session.
    CloneSession {
        /// Source session.
        session_id: SessionId,
        /// New session name override.
        name: Option<String>,
    },
    /// Change a session working directory.
    ChangeWorkingDirectory {
        /// Target session.
        session_id: SessionId,
        /// New working directory.
        path: PathBuf,
    },
    /// Request cancellation of a runtime-work item.
    CancelRuntimeWork {
        /// Target session.
        session_id: SessionId,
        /// Runtime work id.
        work_id: WorkId,
    },
    /// Request context compaction for a session.
    CompactContext {
        /// Target session.
        session_id: SessionId,
    },
    /// Set the selected agent for a session.
    SetAgent {
        /// Target session.
        session_id: SessionId,
        /// Agent id.
        agent_id: String,
    },
    /// Activate a skill for a session.
    ActivateSkill {
        /// Target session.
        session_id: SessionId,
        /// Skill id.
        skill_id: String,
    },
    /// Deactivate a skill for a session.
    DeactivateSkill {
        /// Target session.
        session_id: SessionId,
        /// Skill id.
        skill_id: String,
    },
    /// Load older transcript/history content.
    LoadOlderHistory {
        /// Target session.
        session_id: SessionId,
    },
    /// Load newer transcript/history content.
    LoadNewerHistory {
        /// Target session.
        session_id: SessionId,
    },
}

/// Renderer connection/client metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RendererClientView {
    /// Client id assigned by the daemon.
    pub client_id: ClientId,
    /// Human-readable renderer/client name.
    pub name: String,
}
