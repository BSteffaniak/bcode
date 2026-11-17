//! Review assistant sessions: which review a Bcode session works on.
//!
//! A review app binds a session to the review it is showing, so the review tools the session
//! calls act on that review without the model naming it. The binding records the review's
//! workspace (a durable workspace is read again by its id; a transient one is used as recorded)
//! and the revisions its sources resolved to when the app last bound it, so tools read exactly
//! what the app shows, including sources only a review-source provider can resolve.

use crate::{ResolvedReviewStack, ResolvedSource, ResolvedStackLayer, ReviewWorkspace};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Current schema version of [`ReviewAssistantBinding`].
pub const REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION: u16 = 1;

/// The review a Bcode session works on.
///
/// Its size stays proportional to the review's sources and distinct stack layers: rebinding
/// replaces the binding, durable workspaces are recorded without their viewed and presentation
/// state (they are read again by id), and stacks record each layer once (see
/// [`ReviewAssistantStacks`]). Older records with every stack in full still read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "BindingRecord")]
pub struct ReviewAssistantBinding {
    /// Persisted schema version.
    pub schema_version: u16,
    /// Bound Bcode session id.
    pub session_id: String,
    /// The review's workspace when it was bound. Durable workspaces are read again by id;
    /// transient (unsaved) ones are used as recorded.
    pub workspace: ReviewWorkspace,
    /// Resolved sources of the review when it was bound, in review order.
    #[serde(default)]
    pub resolved: Vec<ResolvedSource>,
    /// Stacks the sources resolved into.
    #[serde(
        default,
        rename = "stack_set",
        skip_serializing_if = "ReviewAssistantStacks::is_empty"
    )]
    pub stacks: ReviewAssistantStacks,
    /// When the binding was first made, in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// When the binding was last refreshed, in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
    /// Where the session's conversation started. The store keeps the first value ever saved
    /// for a session, so rebinding never changes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<ReviewAssistantTag>,
    /// Set while the session is attached to no review (its app shows no review), in
    /// milliseconds since the Unix epoch. Review tools treat a detached session as unbound;
    /// `workspace` keeps the review it was last attached to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detached_at_ms: Option<u64>,
}

impl ReviewAssistantBinding {
    /// Whether the session is attached to no review.
    #[must_use]
    pub const fn is_detached(&self) -> bool {
        self.detached_at_ms.is_some()
    }
}

/// A [`ReviewAssistantBinding`] as stored: stacks are read from `stack_set`, or from `stacks`
/// (every stack in full) in records written before stacks recorded each layer once.
#[derive(Deserialize)]
struct BindingRecord {
    schema_version: u16,
    session_id: String,
    workspace: ReviewWorkspace,
    #[serde(default)]
    resolved: Vec<ResolvedSource>,
    #[serde(default)]
    stack_set: ReviewAssistantStacks,
    #[serde(default)]
    stacks: Vec<ResolvedReviewStack>,
    created_at_ms: u64,
    updated_at_ms: u64,
    #[serde(default)]
    started: Option<ReviewAssistantTag>,
    #[serde(default)]
    detached_at_ms: Option<u64>,
}

impl From<BindingRecord> for ReviewAssistantBinding {
    fn from(record: BindingRecord) -> Self {
        let stacks = if record.stack_set.is_empty() {
            ReviewAssistantStacks::from_stacks(&record.stacks)
        } else {
            record.stack_set
        };
        Self {
            schema_version: record.schema_version,
            session_id: record.session_id,
            workspace: record.workspace,
            resolved: record.resolved,
            stacks,
            created_at_ms: record.created_at_ms,
            updated_at_ms: record.updated_at_ms,
            started: record.started,
            detached_at_ms: record.detached_at_ms,
        }
    }
}

/// The stacks of a review assistant binding, recording each distinct layer once.
///
/// A review stepped through a stack one pull request at a time resolves one stack per layer,
/// each running from the trunk up to that layer, so the stacks share their lower layers.
/// Recorded in full they would grow with the square of the stack's height; here each stack
/// names its layers as runs of positions in one table of distinct layers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewAssistantStacks {
    /// Distinct layers of every stack.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    layers: Vec<ResolvedStackLayer>,
    /// The stacks, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    stacks: Vec<StackRecord>,
}

/// One stack of [`ReviewAssistantStacks`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StackRecord {
    source_id: String,
    trunk: String,
    trunk_oid: String,
    base_oid: String,
    /// Its layers from the trunk up, as `[start, end)` runs of positions in the layer table.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    runs: Vec<[usize; 2]>,
}

/// Identity of a layer in [`ReviewAssistantStacks::from_stacks`]: every field of it.
type LayerKey<'a> = (u64, &'a str, &'a str, &'a str, &'a str, &'a str);

impl ReviewAssistantStacks {
    /// Record `stacks`, in order.
    #[must_use]
    pub fn from_stacks<'a>(stacks: impl IntoIterator<Item = &'a ResolvedReviewStack>) -> Self {
        let mut layers: Vec<ResolvedStackLayer> = Vec::new();
        let mut positions: BTreeMap<LayerKey<'a>, usize> = BTreeMap::new();
        let mut records = Vec::new();
        for stack in stacks {
            let mut runs: Vec<[usize; 2]> = Vec::new();
            for layer in &stack.layers {
                let key = (
                    layer.number,
                    layer.title.as_str(),
                    layer.base_oid.as_str(),
                    layer.head_oid.as_str(),
                    layer.base_branch.as_str(),
                    layer.head_branch.as_str(),
                );
                let position = *positions.entry(key).or_insert_with(|| {
                    layers.push(layer.clone());
                    layers.len() - 1
                });
                match runs.last_mut() {
                    Some(run) if run[1] == position => run[1] += 1,
                    _ => runs.push([position, position + 1]),
                }
            }
            records.push(StackRecord {
                source_id: stack.source_id.clone(),
                trunk: stack.trunk.clone(),
                trunk_oid: stack.trunk_oid.clone(),
                base_oid: stack.base_oid.clone(),
                runs,
            });
        }
        Self {
            layers,
            stacks: records,
        }
    }

    /// The stacks in full, in order. Runs outside the layer table (a damaged record) are
    /// left out.
    #[must_use]
    pub fn to_stacks(&self) -> Vec<ResolvedReviewStack> {
        self.stacks
            .iter()
            .map(|stack| ResolvedReviewStack {
                source_id: stack.source_id.clone(),
                trunk: stack.trunk.clone(),
                trunk_oid: stack.trunk_oid.clone(),
                base_oid: stack.base_oid.clone(),
                layers: stack
                    .runs
                    .iter()
                    .filter_map(|[start, end]| self.layers.get(*start..*end))
                    .flatten()
                    .cloned()
                    .collect(),
            })
            .collect()
    }

    /// Number of stacks.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.stacks.len()
    }

    /// Whether there are no stacks.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.stacks.is_empty()
    }
}

/// Where a review assistant conversation started: a review (and the stack layer that was
/// current), or no review (the reviews home or the new-review wizard).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewAssistantTag {
    /// Annotation scope id of the review; `None` when no review was on screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// The review's title then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Number of the stack layer (pull request) that was current, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<u64>,
}

impl ReviewAssistantTag {
    /// The tag of a conversation started where no review was on screen.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            workspace_id: None,
            title: None,
            layer: None,
        }
    }
}

/// Request to bind a session to a review (or refresh its binding).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindReviewAssistantRequest {
    /// Repository path where the review's state is stored.
    pub repo_path: PathBuf,
    /// The binding to record; its timestamps are set by the store.
    pub binding: ReviewAssistantBinding,
}

/// Response to [`BindReviewAssistantRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindReviewAssistantResponse {
    /// The recorded binding.
    pub binding: ReviewAssistantBinding,
}

/// Request for the binding of one session, or the newest binding of one review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetReviewAssistantRequest {
    /// Repository path where the review's state is stored.
    pub repo_path: PathBuf,
    /// Which binding to read.
    pub lookup: ReviewAssistantLookup,
}

/// Which assistant binding to read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewAssistantLookup {
    /// The binding of one session.
    Session {
        /// Bcode session id.
        session_id: String,
    },
    /// The most recently refreshed binding of a review, by its annotation scope id.
    Review {
        /// Annotation scope id of the review's workspace.
        workspace_id: String,
    },
}

/// Operation listing the assistant bindings of a repository.
pub const OP_REVIEW_ASSISTANT_LIST: &str = "review.assistant.list";
/// Operation removing one session's assistant binding.
pub const OP_REVIEW_ASSISTANT_UNBIND: &str = "review.assistant.unbind";
/// Bindings listed at most by one [`ListReviewAssistantsRequest`].
pub const MAX_REVIEW_ASSISTANT_LIST: u32 = 200;

/// Request for the assistant bindings of a repository, most recently refreshed first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListReviewAssistantsRequest {
    /// Repository path where the review state is stored.
    pub repo_path: PathBuf,
    /// Bindings returned at most (at most [`MAX_REVIEW_ASSISTANT_LIST`], which is also the
    /// default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Response to [`ListReviewAssistantsRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListReviewAssistantsResponse {
    /// Readable bindings, most recently refreshed first.
    pub bindings: Vec<ReviewAssistantBinding>,
    /// Whether more bindings exist than were returned.
    #[serde(default)]
    pub truncated: bool,
}

/// Request to remove one session's assistant binding (its conversation was deleted).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnbindReviewAssistantRequest {
    /// Repository path where the review state is stored.
    pub repo_path: PathBuf,
    /// Bcode session id.
    pub session_id: String,
}

/// Response to [`UnbindReviewAssistantRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnbindReviewAssistantResponse {
    /// Whether a binding was removed.
    pub removed: bool,
}

/// Response to [`GetReviewAssistantRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetReviewAssistantResponse {
    /// The binding, when one exists.
    pub binding: Option<ReviewAssistantBinding>,
}

/// Operation reading the review app's assistant settings.
pub const OP_REVIEW_ASSISTANT_SETTINGS_GET: &str = "review.assistant.settings.get";
/// Operation saving the review app's assistant settings.
pub const OP_REVIEW_ASSISTANT_SETTINGS_SAVE: &str = "review.assistant.settings.save";

/// The model and reasoning effort the review app gives new assistant conversations and Ask AI
/// card sessions: the latest choice made anywhere in the app. One record per review store.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewAssistantSettings {
    /// Model-provider plugin of the model; Bcode's default provider when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_plugin_id: Option<String>,
    /// Model id; Bcode's default model when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// Reasoning effort; the model's default when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// When the record was last saved, in milliseconds since the Unix epoch (set by the store).
    #[serde(default)]
    pub updated_at_ms: u64,
}

impl ReviewAssistantSettings {
    /// Whether the record chooses nothing (Bcode's default model and its default effort).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.model_id.is_none() && self.effort.is_none()
    }
}

/// Request for the review app's assistant settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetReviewAssistantSettingsRequest {
    /// Repository path whose review store holds the record.
    pub repo_path: PathBuf,
}

/// Response to [`GetReviewAssistantSettingsRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetReviewAssistantSettingsResponse {
    /// The record, once one was saved.
    pub settings: Option<ReviewAssistantSettings>,
}

/// Request to replace the review app's assistant settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveReviewAssistantSettingsRequest {
    /// Repository path whose review store holds the record.
    pub repo_path: PathBuf,
    /// The record; its timestamp is set by the store.
    pub settings: ReviewAssistantSettings,
}

/// Response to [`SaveReviewAssistantSettingsRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveReviewAssistantSettingsResponse {
    /// The recorded settings.
    pub settings: ReviewAssistantSettings,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stacks of a stack of `height` layers stepped through one layer at a time: one stack
    /// per layer, from the trunk up to it.
    fn stepped(height: u64) -> Vec<ResolvedReviewStack> {
        let layer = |number: u64| ResolvedStackLayer {
            number,
            title: format!("Layer {number}"),
            base_oid: format!("{:040}", number - 1),
            head_oid: format!("{number:040}"),
            base_branch: format!("branch-{}", number - 1),
            head_branch: format!("branch-{number}"),
        };
        (1..=height)
            .map(|top| ResolvedReviewStack {
                source_id: format!("source-{top}"),
                trunk: "main".to_owned(),
                trunk_oid: format!("{:040}", 0),
                base_oid: format!("{:040}", 0),
                layers: (1..=top).map(layer).collect(),
            })
            .collect()
    }

    fn encoded_len(height: u64) -> usize {
        serde_json::to_string(&ReviewAssistantStacks::from_stacks(&stepped(height)))
            .expect("encode")
            .len()
    }

    #[test]
    fn stacks_read_back_as_recorded() {
        let stacks = stepped(12);
        let recorded = ReviewAssistantStacks::from_stacks(&stacks);
        assert_eq!(recorded.len(), 12);
        assert_eq!(recorded.to_stacks(), stacks);

        // Stacks that share no layers, and layers that differ in one field only.
        let mut other = stepped(3);
        other[1].layers[0].title = "Renamed".to_owned();
        other.push(ResolvedReviewStack {
            source_id: "lone".to_owned(),
            trunk: "develop".to_owned(),
            trunk_oid: "t".to_owned(),
            base_oid: "b".to_owned(),
            layers: Vec::new(),
        });
        let recorded = ReviewAssistantStacks::from_stacks(&other);
        let json = serde_json::to_string(&recorded).expect("encode");
        let read: ReviewAssistantStacks = serde_json::from_str(&json).expect("decode");
        assert_eq!(read.to_stacks(), other);
    }

    #[test]
    fn a_stepped_through_stack_records_in_size_proportional_to_its_height() {
        let small = encoded_len(50);
        let large = encoded_len(200);
        assert!(
            large < small * 5,
            "four times the layers took {large} bytes against {small}"
        );
    }

    #[test]
    fn bindings_with_every_stack_in_full_still_read() {
        let stacks = stepped(5);
        let workspace =
            ReviewWorkspace::from_target(PathBuf::from("/repo"), crate::ReviewTarget::LastCommit);
        let legacy = serde_json::json!({
            "schema_version": REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION,
            "session_id": "s",
            "workspace": workspace,
            "stacks": stacks,
            "created_at_ms": 1,
            "updated_at_ms": 2,
            "started": { "workspace_id": "rv", "title": "T", "layer": 3 },
        });
        let binding: ReviewAssistantBinding =
            serde_json::from_value(legacy).expect("legacy binding reads");
        assert_eq!(binding.stacks.to_stacks(), stacks);
        assert_eq!(binding.started.as_ref().and_then(|tag| tag.layer), Some(3));

        let json = serde_json::to_value(&binding).expect("encode");
        assert!(json.get("stacks").is_none(), "{json}");
        let again: ReviewAssistantBinding = serde_json::from_value(json).expect("decode");
        assert_eq!(again, binding);
    }
}
