#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Renderer-neutral session view models for Bcode renderers.
//!
//! These types are intentionally presentation-semantic instead of renderer-specific: terminal,
//! web, and future renderers should be able to consume them without depending on terminal frames,
//! browser DOM primitives, daemon clients, or application orchestration.

use bcode_session_models::{
    ClientId, ModelTurnOutcome, PluginVisualDescriptor, RequestContextOccupancy, RuntimeWorkKind,
    RuntimeWorkStatus, SessionForkResult, SessionId, SessionSummary, SessionTokenUsage,
    ToolArtifact, ToolInvocationResult, WorkId,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

#[cfg(test)]
mod tests;

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

    /// Create an identifier for a tool invocation.
    #[must_use]
    pub fn tool(tool_call_id: &str) -> Self {
        Self(format!("tool:{tool_call_id}"))
    }

    /// Create an identifier for historical tool request context retained after completion.
    #[must_use]
    pub fn tool_request(tool_call_id: &str) -> Self {
        Self(format!("tool-request:{tool_call_id}"))
    }

    /// Create an identifier for a semantic presentation slot owned by a tool invocation.
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
        let slot = match placement {
            bcode_session_models::ToolContributionPlacement::Request => "request".to_owned(),
            bcode_session_models::ToolContributionPlacement::Progress => "progress".to_owned(),
            bcode_session_models::ToolContributionPlacement::Result => "result".to_owned(),
            bcode_session_models::ToolContributionPlacement::Supplemental => format!(
                "supplemental:{}",
                supplemental_id.expect("supplemental presentation slots require stable identity")
            ),
            bcode_session_models::ToolContributionPlacement::Hidden => "hidden".to_owned(),
        };
        Self(format!("tool-slot:{tool_call_id}:{slot}"))
    }

    /// Create an identifier for a permission request.
    #[must_use]
    pub fn permission(permission_id: &str) -> Self {
        Self(format!("permission:{permission_id}"))
    }

    /// Create an identifier for runtime work.
    #[must_use]
    pub fn runtime_work(work_id: &WorkId) -> Self {
        Self(format!("runtime-work:{work_id}"))
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
    /// Renderer-neutral transcript items.
    pub transcript: TranscriptViewDocument,
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
    pub const SCHEMA_VERSION: u16 = 9;

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
            transcript: TranscriptViewDocument::default(),
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
    /// Returns an error when the snapshot or transcript revision does not match the patch base, or
    /// when a transcript operation references a missing or duplicate item.
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
        if let Some(reset) = &patch.reset {
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
        self.contributions.extend(patch.contributions.clone());
        self.active_exchanges.extend(patch.active_exchanges.clone());
        self.active_invocations
            .extend(patch.active_invocations.clone());
        self.tools.extend(patch.tools.clone());
        upsert_permissions(&mut self.permissions, &patch.permissions);
        upsert_runtime_work(&mut self.runtime_work, &patch.runtime_work);
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
    /// Full snapshot reset used when an incremental patch would not be correctness-preserving.
    #[serde(default)]
    pub reset: Option<Box<SessionViewSnapshot>>,
    /// Transcript item operations.
    pub transcript: Vec<TranscriptViewPatchOp>,
    /// Opaque contribution updates keyed by invocation and contribution identity.
    pub contributions: BTreeMap<String, bcode_session_models::ToolContributionEvent>,
    /// Active exchange updates keyed by invocation and exchange identity.
    pub active_exchanges: BTreeMap<String, bcode_session_models::ToolExchangeRequest>,
    /// Invocation lifecycle updates keyed by invocation identifier.
    pub active_invocations: BTreeMap<String, bcode_session_models::ToolInvocationLifecycleEvent>,
    /// Tool updates keyed by tool call id.
    pub tools: BTreeMap<String, ToolInvocationView>,
    /// Permission updates.
    pub permissions: Vec<PermissionView>,
    /// Runtime-work updates.
    pub runtime_work: Vec<RuntimeWorkView>,
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
    pub const SCHEMA_VERSION: u16 = 9;

    /// Create an empty patch between two revisions.
    #[must_use]
    pub const fn empty(base_revision: ViewRevision, revision: ViewRevision) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            base_revision,
            revision,
            session_id: None,
            reset: None,
            transcript: Vec::new(),
            contributions: BTreeMap::new(),
            active_exchanges: BTreeMap::new(),
            active_invocations: BTreeMap::new(),
            tools: BTreeMap::new(),
            permissions: Vec::new(),
            runtime_work: Vec::new(),
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
    /// metadata and item ordering remains source-prefix compatible. Otherwise it falls back to a
    /// transcript reset carrying the complete next document.
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
        patch.contributions = changed_map_entries(&base.contributions, &next.contributions);
        patch.active_exchanges =
            changed_map_entries(&base.active_exchanges, &next.active_exchanges);
        patch.active_invocations =
            changed_map_entries(&base.active_invocations, &next.active_invocations);
        patch.tools = changed_map_entries(&base.tools, &next.tools);
        patch.plugin_status = changed_map_entries(&base.plugin_status, &next.plugin_status);
        patch
    }
}

/// Error applying transcript patch operations to a bounded transcript document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptViewPatchError {
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
    /// Remove a transcript item by id.
    Remove { id: TranscriptViewItemId },
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
            TranscriptViewPatchOp::Remove { id } => self.remove_patch_item(id),
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
        *existing = item;
        Ok(())
    }

    fn remove_patch_item(
        &mut self,
        id: &TranscriptViewItemId,
    ) -> Result<(), TranscriptViewPatchError> {
        let Some(index) = self.items.iter().position(|item| item.id == *id) else {
            return Err(TranscriptViewPatchError::MissingItem { id: id.clone() });
        };
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
        || base.latest_sequence != next.latest_sequence
        || map_has_removals(&base.contributions, &next.contributions)
        || map_has_removals(&base.active_exchanges, &next.active_exchanges)
        || map_has_removals(&base.active_invocations, &next.active_invocations)
        || map_has_removals(&base.tools, &next.tools)
        || base.permissions != next.permissions
        || base.runtime_work != next.runtime_work
        || base.active_skills != next.active_skills
        || map_has_removals(&base.plugin_status, &next.plugin_status)
        || base.composer != next.composer
        || base.thinking != next.thinking
        || base.runtime != next.runtime
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

fn upsert_runtime_work(target: &mut Vec<RuntimeWorkView>, updates: &[RuntimeWorkView]) {
    for update in updates {
        upsert_by(target, update.clone(), |work| work.work_id.0.as_str());
    }
}

fn upsert_interactions(
    target: &mut Vec<InteractionViewSummary>,
    updates: &[InteractionViewSummary],
) {
    for update in updates {
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
        || !transcript_items_are_prefix_compatible(base, next)
    {
        return vec![TranscriptViewPatchOp::Reset {
            document: next.clone(),
        }];
    }

    let mut operations = Vec::new();
    let common_len = base.items.len().min(next.items.len());
    for index in 0..common_len {
        if base.items[index] != next.items[index] {
            operations.push(TranscriptViewPatchOp::Replace {
                item: next.items[index].clone(),
            });
        }
    }
    for item in next.items.iter().skip(common_len) {
        operations.push(TranscriptViewPatchOp::Append { item: item.clone() });
    }
    for item in base.items.iter().skip(common_len).rev() {
        operations.push(TranscriptViewPatchOp::Remove {
            id: item.id.clone(),
        });
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

fn transcript_items_are_prefix_compatible(
    base: &TranscriptViewDocument,
    next: &TranscriptViewDocument,
) -> bool {
    base.items
        .iter()
        .zip(&next.items)
        .all(|(base, next)| base.id == next.id)
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
    /// Assistant reasoning/thinking content.
    ReasoningMessage { message: ChatMessageView },
    /// Tool request/result/stream block.
    ToolInvocation { tool: Box<ToolInvocationView> },
    /// Historical request context retained after a tool result supersedes the active invocation row.
    ToolRequest { tool: Box<ToolInvocationView> },
    /// Permission request block.
    Permission { permission: PermissionView },
    /// Runtime work status block.
    RuntimeWork { work: RuntimeWorkView },
    /// Provider-neutral model usage accounting.
    Usage { usage: UsageView },
    /// Context compaction transcript note.
    Compaction { compaction: CompactionView },
    /// Interactive request block.
    Interaction { interaction: InteractionViewSummary },
    /// Skill-related transcript note.
    Skill { skill: SkillView },
    /// System/status message.
    SystemMessage { message: ChatMessageView },
    /// Generic plugin visual payload.
    PluginVisual { visual: PluginVisualView },
    /// Opaque schema-versioned tool contribution with generic fallback rendering.
    ToolContribution {
        contribution: bcode_session_models::ToolContributionEvent,
    },
}

/// Renderer-neutral model usage transcript item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageView {
    /// Model turn identifier.
    pub turn_id: String,
    /// Provider-neutral usage accounting.
    pub usage: SessionTokenUsage,
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
    /// Plugin-owned request visual.
    pub request_visual: Option<PluginVisualView>,
    /// Current lifecycle status.
    pub status: ToolInvocationViewStatus,
    /// Raw final text result, when finished.
    pub result_text: Option<String>,
    /// Whether the final result represents an error.
    pub is_error: Option<bool>,
    /// Semantic result, when supplied by the tool.
    pub result: Option<ToolResultView>,
    /// Raw terminal/text stream output observed for the tool.
    pub output: Option<ToolOutputView>,
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
    /// Stream lifecycle/output was observed.
    Running,
    /// Final result was observed.
    Finished,
}

impl From<bcode_session_models::ToolInvocationProjectionStatus> for ToolInvocationViewStatus {
    fn from(value: bcode_session_models::ToolInvocationProjectionStatus) -> Self {
        match value {
            bcode_session_models::ToolInvocationProjectionStatus::Requested => Self::Requested,
            bcode_session_models::ToolInvocationProjectionStatus::Running => Self::Running,
            bcode_session_models::ToolInvocationProjectionStatus::Finished => Self::Finished,
        }
    }
}

/// Renderer-neutral tool output view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutputView {
    /// Raw stream output text.
    pub text: String,
    /// Terminal columns reported by the producer, when known.
    pub columns: Option<u16>,
    /// Terminal rows reported by the producer, when known.
    pub rows: Option<u16>,
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

/// Renderer-neutral plugin visual view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginVisualView {
    /// Raw plugin visual descriptor.
    pub descriptor: PluginVisualDescriptor,
    /// Generic renderer payload for structured display.
    pub generic_payload: serde_json::Value,
}

impl From<PluginVisualDescriptor> for PluginVisualView {
    fn from(descriptor: PluginVisualDescriptor) -> Self {
        let generic_payload = serde_json::to_value(&descriptor).unwrap_or(serde_json::Value::Null);
        Self {
            descriptor,
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

/// Return the renderer-neutral aggregate activity label for active runtime work.
#[must_use]
pub fn runtime_work_status_label(runtime_work: &[RuntimeWorkView]) -> Option<String> {
    let running_tools = runtime_work
        .iter()
        .filter(|work| {
            work.kind == RuntimeWorkKind::Tool && work.status == RuntimeWorkStatus::Running
        })
        .count();
    if running_tools > 1 {
        return Some(format!("running {running_tools} tools"));
    }
    let work = runtime_work
        .iter()
        .min_by(|left, right| left.work_id.cmp(&right.work_id))?;
    let prefix = match work.status {
        RuntimeWorkStatus::Queued => "queued",
        RuntimeWorkStatus::Cancelling => "cancelling",
        RuntimeWorkStatus::Running => match work.kind {
            RuntimeWorkKind::ModelTurn => "running",
            RuntimeWorkKind::Tool => "running tool",
            RuntimeWorkKind::PluginInvocation => "running plugin",
            RuntimeWorkKind::EventDelivery => "delivering event",
        },
        RuntimeWorkStatus::Completed
        | RuntimeWorkStatus::Cancelled
        | RuntimeWorkStatus::Failed
        | RuntimeWorkStatus::TimedOut => return None,
    };
    let detail = match (work.label.is_empty(), work.message.as_deref()) {
        (true, Some(message)) => message.to_owned(),
        (true, None) => work.work_id.to_string(),
        (false, Some(message)) if message != work.label => {
            format!("{} — {message}", work.label)
        }
        (false, _) => work.label.clone(),
    };
    Some(format!("{prefix}: {detail}"))
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

/// Assistant reasoning/thinking display state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingViewState {
    /// Whether reasoning content should be visible by default.
    pub visible: bool,
    /// Current in-flight reasoning text.
    pub active_text: Option<String>,
    /// Whether the current reasoning text is streaming.
    pub streaming: bool,
}

impl Default for ThinkingViewState {
    fn default() -> Self {
        Self {
            visible: true,
            active_text: None,
            streaming: false,
        }
    }
}

/// Renderer-neutral interactive request summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionViewSummary {
    /// Interaction identifier.
    pub interaction_id: String,
    /// Interaction kind.
    pub kind: String,
    /// Renderer-specific surface key supplied by the interaction owner.
    #[serde(default)]
    pub surface_kind: String,
    /// Associated tool call identifier, when known.
    pub tool_call_id: Option<String>,
    /// Optional title for display.
    pub title: Option<String>,
    /// Whether the interaction requires a response before the turn can continue.
    #[serde(default)]
    pub required: bool,
    /// Optional snapshot payload for generic rendering.
    pub snapshot: Option<serde_json::Value>,
    /// Whether the interaction has been durably resolved.
    #[serde(default)]
    pub resolved: bool,
    /// Durable resolution payload, when resolved.
    #[serde(default)]
    pub resolution: Option<serde_json::Value>,
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
