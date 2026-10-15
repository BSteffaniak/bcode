//! Renderer-neutral composer contributions, versioned by `bcode.composer/v1`.
//!
//! Missing manifest collections mean no contributions. Unknown interface IDs are
//! unsupported, never interpreted as v1. Grammar, ranking and search belong to
//! contributors; clients display labels and apply validated text edits only.
//! All offsets are UTF-8 byte offsets, not scalar, grapheme or terminal-cell offsets.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Service interface for bounded candidate search.
pub const COMPOSER_INTERFACE_ID: &str = "bcode.composer/v1";
/// Operation accepting `ComposerSearchRequest` and returning `ComposerSnapshot`.
pub const OP_COMPOSER_SEARCH: &str = "search";
/// Maximum UTF-8 document size accepted by v1.
pub const MAX_COMPOSER_DOCUMENT_BYTES: usize = 262_144;
/// Maximum candidates accepted in one response.
pub const MAX_COMPOSER_CANDIDATES: usize = 64;
/// Maximum query size in UTF-8 bytes.
pub const MAX_COMPOSER_QUERY_BYTES: usize = 4096;
/// Maximum replacement size in UTF-8 bytes.
pub const MAX_COMPOSER_REPLACEMENT_BYTES: usize = 16_384;
/// Maximum encoded service response size; reject before JSON decoding.
pub const MAX_COMPOSER_RESPONSE_BYTES: usize = 2_097_152;
const MAX_LABEL_BYTES: usize = 1024;
const MAX_ID_BYTES: usize = 256;

/// Manifest-declared contribution; no trigger grammar is interpreted by the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposerContribution {
    /// Stable contribution ID within its plugin.
    pub id: String,
    /// Kind registered by the plugin's local activation adapter.
    pub activation_kind: String,
    /// Exact supported service interface; v1 requires `COMPOSER_INTERFACE_ID`.
    pub service_interface_id: String,
    /// Lower values precede higher values; ties use plugin and contribution IDs.
    #[serde(default)]
    pub priority: i32,
}

impl ComposerContribution {
    /// Whether this declaration is valid and supported by v1.
    #[must_use]
    pub fn is_supported(&self) -> bool {
        valid_id(&self.id)
            && valid_id(&self.activation_kind)
            && self.service_interface_id == COMPOSER_INTERFACE_ID
    }
}

/// Contribution with manifest ownership attached by discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginOwnedComposerContribution {
    /// Owning enabled plugin.
    pub plugin_id: String,
    /// Plugin-owned declaration.
    pub contribution: ComposerContribution,
}

/// One immutable document revision. IDs must not be reused across document lifetimes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposerDocument {
    /// Frontend-owned stable document identity.
    pub document_id: String,
    /// Monotonic revision, advanced on every text or cursor change.
    pub revision: u64,
    /// Plain composer text. This contract does not attach content.
    pub text: String,
    /// Insertion position in UTF-8 bytes.
    pub cursor: usize,
    /// Optional normalized selection. `None` or a collapsed range at `cursor`
    /// permits activation; noncollapsed selections exclude composer activation.
    /// Selection changes must advance `revision`, just like cursor changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<ComposerTextRange>,
}

/// Half-open UTF-8 byte range into a specific document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposerTextRange {
    /// Inclusive start.
    pub start: usize,
    /// Exclusive end.
    pub end: usize,
}

/// Plugin-local grammar result; it is not a request to perform IO.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposerActivation {
    /// Complete token range replaced on acceptance.
    pub range: ComposerTextRange,
    /// Plugin-owned search query, with no host interpretation.
    pub query: String,
}

/// Search executed off the input/render thread using existing service routing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposerSearchRequest {
    /// Selected manifest contribution ID. New callers must provide a nonempty ID.
    /// Empty legacy requests require exactly one supported enabled contribution
    /// in the target plugin/interface. Hosts and services validate membership
    /// before dispatch; payload validation alone is not routing authorization.
    #[serde(default)]
    pub contribution_id: String,
    /// Unique in-flight request identity; changes even for repeated queries.
    pub request_id: String,
    /// Exact document revision that initiated this request.
    pub document: ComposerDocument,
    /// Validated local activation.
    pub activation: ComposerActivation,
    /// Requested upper bound, from 1 through `MAX_COMPOSER_CANDIDATES`.
    pub max_results: usize,
    /// Opaque contributor-owned interaction state, replayed on subsequent searches.
    #[serde(default)]
    pub interaction_state: BTreeMap<String, String>,
    /// Contributor action invoked for this search only; never an editor command.
    #[serde(default)]
    pub action_id: Option<String>,
}

/// Renderer-neutral candidate list and generic fallback surface.
///
/// Clients own selection/navigation, but preserve contributor order. Empty lists,
/// unsupported adapters, invalid responses and service failures leave text entry
/// available. Responses must be discarded after document changes, dismissal,
/// plugin disablement or registry replacement; cancellation is best-effort only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposerSnapshot {
    /// Echoed request identity.
    pub request_id: String,
    /// Echoed document identity.
    pub document_id: String,
    /// Echoed document revision.
    pub revision: u64,
    /// Explicit lifecycle/presentation state. Missing legacy values mean ready;
    /// legacy `incomplete = true` still means truncated via `effective_state`.
    #[serde(default)]
    pub state: ComposerSnapshotState,
    /// Contributor-ranked candidates, bounded by the request.
    pub candidates: Vec<ComposerCandidate>,
    /// Resolved contributor state to replay; bounded independently of presentation.
    #[serde(default)]
    pub interaction_state: BTreeMap<String, String>,
    /// Optional versioned chrome and contributor-owned actions.
    #[serde(default)]
    pub presentation: Option<ComposerPresentation>,
    /// Search omitted additional results; not permission for unbounded pagination.
    #[serde(default)]
    pub incomplete: bool,
}

/// Generic snapshot lifecycle; error codes never include paths or OS diagnostics.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ComposerSnapshotState {
    /// No reply yet; no candidates may be accepted.
    Loading,
    /// Completed search, possibly empty.
    #[default]
    Ready,
    /// Partial results because a bounded search budget was reached.
    Truncated,
    /// Unavailable search; preserve ordinary input and show a safe reason code.
    Error {
        /// Bounded stable identifier such as `unavailable` or `denied`.
        code: String,
    },
}

/// Generic post-acceptance behavior chosen by the contributor, not by labels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposerContinuation {
    /// Close candidate UI after applying the text edit.
    #[default]
    Finish,
    /// Re-run the same contributor's local activation on the resulting document.
    /// The plugin's grammar decides whether and how a fresh search continues.
    Reactivate,
}

/// Versioned portable picker chrome. Only version 1 is supported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposerPresentation {
    /// Exact presentation schema version; unknown versions fail closed.
    pub version: u32,
    /// Plain bounded title.
    pub title: String,
    /// Plain bounded optional hint.
    #[serde(default)]
    pub hint: Option<String>,
    /// At most eight contributor-owned actions.
    #[serde(default)]
    pub actions: Vec<ComposerAction>,
}

/// A stateless service action, not an editor mutation or execution effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposerAction {
    /// Plugin-owned action identity sent to search.
    pub id: String,
    /// Plain display label.
    pub label: String,
    /// Optional configurable keymap action identity, never raw key syntax.
    #[serde(default)]
    pub binding: Option<String>,
}

/// Plain text presentation with an atomic edit; no opaque execution effects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposerCandidate {
    /// Stable candidate identity within this response.
    pub id: String,
    /// Plain display label, never interpreted as markup or control sequences.
    pub label: String,
    /// Ordered nonoverlapping matched UTF-8 byte ranges into `label` (maximum 64).
    #[serde(default)]
    pub matched_ranges: Vec<ComposerTextRange>,
    /// Replacement state applied only on successful `Reactivate` acceptance.
    #[serde(default)]
    pub next_interaction_state: Option<BTreeMap<String, String>>,
    /// Optional bounded plain display detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Text-only edit to accept this candidate.
    pub edit: ComposerEdit,
    /// Generic next step, never inferred from label/detail or path syntax.
    #[serde(default)]
    pub continuation: ComposerContinuation,
}

/// Single atomic replacement bound to the exact originating document revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposerEdit {
    /// Target document identity.
    pub document_id: String,
    /// Target revision.
    pub revision: u64,
    /// UTF-8 byte range to replace.
    pub range: ComposerTextRange,
    /// Exact original text at `range`; checked before modifying anything.
    pub expected_text: String,
    /// Literal replacement text, never content attachment or an execution request.
    pub replacement: String,
    /// Absolute UTF-8 byte cursor in the resulting document.
    pub cursor: usize,
}

/// Payload-independent failure diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerValidationError {
    /// An identity, size, range or text boundary is invalid.
    InvalidPayload,
    /// Request, document identity, revision or expected text no longer matches.
    Stale,
    /// The document revision cannot be advanced.
    RevisionExhausted,
}

impl fmt::Display for ComposerValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidPayload => "invalid composer payload",
            Self::Stale => "stale composer payload",
            Self::RevisionExhausted => "composer revision exhausted",
        })
    }
}
impl std::error::Error for ComposerValidationError {}

fn valid_id(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= MAX_ID_BYTES && !value.chars().any(char::is_control)
}

fn valid_state(state: &BTreeMap<String, String>) -> bool {
    state.len() <= 8
        && state
            .iter()
            .all(|(key, value)| valid_id(key) && value.len() <= MAX_ID_BYTES)
}

fn valid_display(value: &str) -> bool {
    value.len() <= MAX_LABEL_BYTES && !value.chars().any(char::is_control)
}

impl ComposerDocument {
    /// Validate identity, size and UTF-8 insertion boundary.
    ///
    /// # Errors
    /// Returns `InvalidPayload` for invalid identity, size or cursor.
    pub fn validate(&self) -> Result<(), ComposerValidationError> {
        if !valid_id(&self.document_id)
            || self.text.len() > MAX_COMPOSER_DOCUMENT_BYTES
            || !self.text.is_char_boundary(self.cursor)
        {
            return Err(ComposerValidationError::InvalidPayload);
        }
        if let Some(selection) = self.selection
            && (self.text.get(selection.start..selection.end).is_none()
                || (self.cursor != selection.start && self.cursor != selection.end))
        {
            return Err(ComposerValidationError::InvalidPayload);
        }
        Ok(())
    }

    /// Whether the selection permits plugin activation. Validate the document first.
    #[must_use]
    pub fn allows_activation(&self) -> bool {
        self.selection
            .is_none_or(|selection| selection.start == selection.end)
    }
}

impl ComposerActivation {
    /// Validate bounds against the originating document.
    ///
    /// # Errors
    /// Returns `InvalidPayload` for invalid documents, ranges or oversized queries.
    pub fn validate(&self, document: &ComposerDocument) -> Result<(), ComposerValidationError> {
        document.validate()?;
        if !document.allows_activation()
            || document
                .text
                .get(self.range.start..self.range.end)
                .is_none()
            || !(self.range.start..=self.range.end).contains(&document.cursor)
            || self.query.len() > MAX_COMPOSER_QUERY_BYTES
        {
            return Err(ComposerValidationError::InvalidPayload);
        }
        Ok(())
    }
}

impl ComposerSearchRequest {
    /// Resolve one supported contribution from enabled target declarations.
    ///
    /// The caller must scope `contributions` to the selected enabled plugin and
    /// service interface. Explicit IDs require exactly one matching declaration;
    /// legacy empty IDs require exactly one supported declaration overall.
    /// Retain the resolved route with the request: that route, request ID and
    /// document revision fence replies without an additional snapshot field.
    /// This lightweight contract check does not grant resource authorization.
    ///
    /// # Errors
    /// Returns `InvalidPayload` for invalid requests, unknown IDs, duplicate matches
    /// or missing/ambiguous legacy routes.
    pub fn validate_for_contributions<'a>(
        &self,
        contributions: &'a [ComposerContribution],
    ) -> Result<&'a ComposerContribution, ComposerValidationError> {
        self.validate()?;
        let mut matches = contributions.iter().filter(|contribution| {
            contribution.is_supported()
                && (self.contribution_id.is_empty() || contribution.id == self.contribution_id)
        });
        let selected = matches
            .next()
            .ok_or(ComposerValidationError::InvalidPayload)?;
        if matches.next().is_some() {
            return Err(ComposerValidationError::InvalidPayload);
        }
        Ok(selected)
    }

    /// Validate before dispatching a service request.
    ///
    /// Empty IDs preserve legacy decoding only; routing must separately validate
    /// enabled-manifest membership and legacy uniqueness. New callers set an ID.
    ///
    /// # Errors
    /// Returns `InvalidPayload` for invalid identities, activation or result bounds.
    pub fn validate(&self) -> Result<(), ComposerValidationError> {
        self.activation.validate(&self.document)?;
        if (!self.contribution_id.is_empty() && !valid_id(&self.contribution_id))
            || !valid_id(&self.request_id)
            || !(1..=MAX_COMPOSER_CANDIDATES).contains(&self.max_results)
            || !valid_state(&self.interaction_state)
            || self.action_id.as_ref().is_some_and(|id| !valid_id(id))
        {
            return Err(ComposerValidationError::InvalidPayload);
        }
        Ok(())
    }
}

impl ComposerSnapshot {
    /// Interpret explicit state together with the legacy truncation flag.
    #[must_use]
    pub fn effective_state(&self) -> ComposerSnapshotState {
        if self.incomplete && self.state == ComposerSnapshotState::Ready {
            ComposerSnapshotState::Truncated
        } else {
            self.state.clone()
        }
    }

    /// Validate an entire response atomically before exposing any candidates.
    /// Hosts must additionally match the request document to the live document.
    ///
    /// # Errors
    /// Returns `Stale` for request/document mismatch, or `InvalidPayload` for bounds,
    /// duplicate IDs, unsafe display text or edits outside the activated token.
    pub fn validate_for(
        &self,
        request: &ComposerSearchRequest,
    ) -> Result<(), ComposerValidationError> {
        request.validate()?;
        if self.request_id != request.request_id
            || self.document_id != request.document.document_id
            || self.revision != request.document.revision
        {
            return Err(ComposerValidationError::Stale);
        }
        match &self.state {
            ComposerSnapshotState::Loading | ComposerSnapshotState::Error { .. }
                if !self.candidates.is_empty() || self.incomplete =>
            {
                return Err(ComposerValidationError::InvalidPayload);
            }
            ComposerSnapshotState::Error { code } if !valid_id(code) => {
                return Err(ComposerValidationError::InvalidPayload);
            }
            _ => {}
        }
        if !valid_state(&self.interaction_state) {
            return Err(ComposerValidationError::InvalidPayload);
        }
        if let Some(presentation) = &self.presentation {
            let mut actions = BTreeSet::new();
            if presentation.version != 1
                || !valid_display(&presentation.title)
                || presentation
                    .hint
                    .as_ref()
                    .is_some_and(|hint| !valid_display(hint))
                || presentation.actions.len() > 8
                || presentation.actions.iter().any(|action| {
                    !valid_id(&action.id)
                        || !actions.insert(&action.id)
                        || !valid_display(&action.label)
                        || action.label.trim().is_empty()
                        || action
                            .binding
                            .as_ref()
                            .is_some_and(|binding| !valid_id(binding))
                })
            {
                return Err(ComposerValidationError::InvalidPayload);
            }
        }
        if self.candidates.len() > request.max_results {
            return Err(ComposerValidationError::InvalidPayload);
        }
        let mut ids = BTreeSet::new();
        for candidate in &self.candidates {
            if candidate
                .next_interaction_state
                .as_ref()
                .is_some_and(|state| !valid_state(state))
            {
                return Err(ComposerValidationError::InvalidPayload);
            }
            let mut end = 0;
            if candidate.matched_ranges.len() > 64
                || candidate.matched_ranges.iter().any(|range| {
                    let invalid = range.start < end
                        || range.start >= range.end
                        || candidate.label.get(range.start..range.end).is_none();
                    end = range.end;
                    invalid
                })
            {
                return Err(ComposerValidationError::InvalidPayload);
            }
            if !valid_id(&candidate.id)
                || !ids.insert(&candidate.id)
                || candidate.label.trim().is_empty()
                || !valid_display(&candidate.label)
                || candidate
                    .detail
                    .as_ref()
                    .is_some_and(|detail| !valid_display(detail))
                || candidate.edit.range != request.activation.range
            {
                return Err(ComposerValidationError::InvalidPayload);
            }
            candidate.edit.apply(&request.document)?;
        }
        Ok(())
    }
}

impl ComposerEdit {
    /// Apply a validated text replacement without mutating the source on failure.
    ///
    /// # Errors
    /// Returns `Stale` for identity/revision/text mismatch, `InvalidPayload` for
    /// invalid ranges, sizes or resulting cursor, or `RevisionExhausted` at u64 max.
    pub fn apply(
        &self,
        document: &ComposerDocument,
    ) -> Result<ComposerDocument, ComposerValidationError> {
        document.validate()?;
        if !document.allows_activation() {
            return Err(ComposerValidationError::InvalidPayload);
        }
        if self.document_id != document.document_id || self.revision != document.revision {
            return Err(ComposerValidationError::Stale);
        }
        let original = document
            .text
            .get(self.range.start..self.range.end)
            .ok_or(ComposerValidationError::InvalidPayload)?;
        if original != self.expected_text {
            return Err(ComposerValidationError::Stale);
        }
        if self.replacement.len() > MAX_COMPOSER_REPLACEMENT_BYTES {
            return Err(ComposerValidationError::InvalidPayload);
        }
        let resulting_len = document.text.len() - original.len() + self.replacement.len();
        if resulting_len > MAX_COMPOSER_DOCUMENT_BYTES {
            return Err(ComposerValidationError::InvalidPayload);
        }
        let revision = document
            .revision
            .checked_add(1)
            .ok_or(ComposerValidationError::RevisionExhausted)?;
        let mut text = document.text.clone();
        text.replace_range(self.range.start..self.range.end, &self.replacement);
        if !text.is_char_boundary(self.cursor) {
            return Err(ComposerValidationError::InvalidPayload);
        }
        Ok(ComposerDocument {
            document_id: document.document_id.clone(),
            revision,
            text,
            cursor: self.cursor,
            selection: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ComposerSearchRequest {
        ComposerSearchRequest {
            contribution_id: "example.topic".into(),
            request_id: "search-1".into(),
            document: ComposerDocument {
                document_id: "draft-1".into(),
                revision: 7,
                text: "#é".into(),
                cursor: 3,
                selection: None,
            },
            activation: ComposerActivation {
                range: ComposerTextRange { start: 0, end: 3 },
                query: "é".into(),
            },
            max_results: 2,
            interaction_state: BTreeMap::new(),
            action_id: None,
        }
    }
    fn edit() -> ComposerEdit {
        ComposerEdit {
            document_id: "draft-1".into(),
            revision: 7,
            range: ComposerTextRange { start: 0, end: 3 },
            expected_text: "#é".into(),
            replacement: "topic 🦀".into(),
            cursor: 10,
        }
    }
    fn snapshot() -> ComposerSnapshot {
        ComposerSnapshot {
            request_id: "search-1".into(),
            document_id: "draft-1".into(),
            revision: 7,
            interaction_state: BTreeMap::new(),
            presentation: None,
            candidates: vec![ComposerCandidate {
                id: "topic".into(),
                matched_ranges: Vec::new(),
                next_interaction_state: None,
                label: "Rust topic".into(),
                detail: None,
                edit: edit(),
                continuation: ComposerContinuation::Finish,
            }],
            incomplete: false,
            state: ComposerSnapshotState::Ready,
        }
    }

    #[test]
    fn unicode_edit_is_atomic_and_revision_bound() {
        let document = request().document;
        let applied = edit().apply(&document).unwrap();
        assert_eq!(applied.text, "topic 🦀");
        assert_eq!(applied.cursor, 10);
        assert_eq!(applied.revision, 8);
        assert_eq!(document.text, "#é");
        assert_eq!(edit().apply(&applied), Err(ComposerValidationError::Stale));
        let mut invalid = edit();
        invalid.range.start = 2;
        assert_eq!(
            invalid.apply(&document),
            Err(ComposerValidationError::InvalidPayload)
        );
        invalid = edit();
        invalid.cursor = 7;
        assert_eq!(
            invalid.apply(&document),
            Err(ComposerValidationError::InvalidPayload)
        );
        invalid = edit();
        invalid.expected_text = "different".into();
        assert_eq!(
            invalid.apply(&document),
            Err(ComposerValidationError::Stale)
        );
        let mut exhausted = document;
        exhausted.revision = u64::MAX;
        invalid = edit();
        invalid.revision = u64::MAX;
        assert_eq!(
            invalid.apply(&exhausted),
            Err(ComposerValidationError::RevisionExhausted)
        );
    }

    #[test]
    fn response_rejects_stale_duplicate_oversized_and_unsafe_candidates() {
        let request = request();
        let mut response = snapshot();
        response.validate_for(&request).unwrap();
        response.request_id = "other".into();
        assert_eq!(
            response.validate_for(&request),
            Err(ComposerValidationError::Stale)
        );
        response = snapshot();
        response.candidates.push(response.candidates[0].clone());
        assert_eq!(
            response.validate_for(&request),
            Err(ComposerValidationError::InvalidPayload)
        );
        response.candidates[1].id = "other".into();
        response.candidates.push(response.candidates[1].clone());
        assert_eq!(
            response.validate_for(&request),
            Err(ComposerValidationError::InvalidPayload)
        );
        response = snapshot();
        response.candidates[0].label = "\u{1b}[31munsafe".into();
        assert_eq!(
            response.validate_for(&request),
            Err(ComposerValidationError::InvalidPayload)
        );
        response = snapshot();
        response.candidates[0].edit.range.end = 0;
        assert_eq!(
            response.validate_for(&request),
            Err(ComposerValidationError::InvalidPayload)
        );
        response = snapshot();
        response.candidates.clear();
        response.validate_for(&request).unwrap();
    }

    #[test]
    fn unicode_highlights_and_versioned_metadata_are_bounded() {
        let request = request();
        let mut response = snapshot();
        response.candidates[0].label = "é🦀中".into();
        response.candidates[0].matched_ranges = vec![
            ComposerTextRange { start: 0, end: 2 },
            ComposerTextRange { start: 2, end: 6 },
        ];
        response.validate_for(&request).unwrap();
        response.candidates[0].matched_ranges[1].start = 3;
        assert_eq!(
            response.validate_for(&request),
            Err(ComposerValidationError::InvalidPayload)
        );
        response.candidates[0].matched_ranges.clear();
        response.presentation = Some(ComposerPresentation {
            version: 2,
            title: "Topics".into(),
            hint: None,
            actions: Vec::new(),
        });
        assert_eq!(
            response.validate_for(&request),
            Err(ComposerValidationError::InvalidPayload)
        );
        response.presentation.as_mut().unwrap().version = 1;
        response.validate_for(&request).unwrap();
        response
            .interaction_state
            .insert("scope".into(), "x".repeat(257));
        assert_eq!(
            response.validate_for(&request),
            Err(ComposerValidationError::InvalidPayload)
        );
    }

    #[test]
    fn selection_excludes_activation_and_acceptance() {
        let mut request = request();
        request.document.selection = Some(ComposerTextRange { start: 0, end: 3 });
        request.document.validate().unwrap();
        assert!(!request.document.allows_activation());
        assert_eq!(
            request.validate(),
            Err(ComposerValidationError::InvalidPayload)
        );
        assert_eq!(
            edit().apply(&request.document),
            Err(ComposerValidationError::InvalidPayload)
        );
        request.document.selection = Some(ComposerTextRange { start: 3, end: 3 });
        request.validate().unwrap();
        assert!(edit().apply(&request.document).unwrap().selection.is_none());
        request.document.selection = Some(ComposerTextRange { start: 2, end: 3 });
        assert_eq!(
            request.document.validate(),
            Err(ComposerValidationError::InvalidPayload)
        );
    }

    #[test]
    fn snapshot_lifecycle_has_truthful_generic_states() {
        let request = request();
        let mut snapshot = snapshot();
        snapshot.candidates[0].continuation = ComposerContinuation::Reactivate;
        snapshot.validate_for(&request).unwrap();
        snapshot.state = ComposerSnapshotState::Loading;
        assert_eq!(
            snapshot.validate_for(&request),
            Err(ComposerValidationError::InvalidPayload)
        );
        snapshot.candidates.clear();
        snapshot.validate_for(&request).unwrap();
        snapshot.state = ComposerSnapshotState::Error {
            code: "unavailable".into(),
        };
        snapshot.validate_for(&request).unwrap();
        snapshot.state = ComposerSnapshotState::Ready;
        snapshot.incomplete = true;
        assert_eq!(snapshot.effective_state(), ComposerSnapshotState::Truncated);
        snapshot.validate_for(&request).unwrap();
        snapshot.state = ComposerSnapshotState::Error {
            code: "unsafe\ncode".into(),
        };
        snapshot.incomplete = false;
        assert_eq!(
            snapshot.validate_for(&request),
            Err(ComposerValidationError::InvalidPayload)
        );
    }

    #[test]
    fn distinct_contributions_share_interface_only_with_explicit_identity() {
        let declarations: Vec<_> = ["example.topic", "example.symbol"]
            .into_iter()
            .map(|id| ComposerContribution {
                id: id.into(),
                activation_kind: id.into(),
                service_interface_id: COMPOSER_INTERFACE_ID.into(),
                priority: 0,
            })
            .collect();
        let mut request = request();
        assert_eq!(
            request
                .validate_for_contributions(&declarations)
                .unwrap()
                .id,
            "example.topic"
        );
        request.contribution_id = "example.symbol".into();
        assert_eq!(
            request
                .validate_for_contributions(&declarations)
                .unwrap()
                .id,
            "example.symbol"
        );
        request.contribution_id = "unknown".into();
        request.validate().unwrap();
        assert_eq!(
            request.validate_for_contributions(&declarations),
            Err(ComposerValidationError::InvalidPayload)
        );
        request.contribution_id.clear();
        request.validate().unwrap();
        assert_eq!(
            request.validate_for_contributions(&declarations),
            Err(ComposerValidationError::InvalidPayload)
        );
        assert_eq!(
            request
                .validate_for_contributions(&declarations[..1])
                .unwrap()
                .id,
            "example.topic"
        );
        assert_eq!(
            request.validate_for_contributions(&[]),
            Err(ComposerValidationError::InvalidPayload)
        );
        let duplicates = [declarations[0].clone(), declarations[0].clone()];
        request.contribution_id = "example.topic".into();
        assert_eq!(
            request.validate_for_contributions(&duplicates),
            Err(ComposerValidationError::InvalidPayload)
        );
        let mut unsupported = declarations[0].clone();
        unsupported.service_interface_id = "bcode.composer/v99".into();
        assert_eq!(
            request.validate_for_contributions(&[unsupported]),
            Err(ComposerValidationError::InvalidPayload)
        );
    }

    #[test]
    fn malformed_nonempty_contribution_identity_fails_payload_validation() {
        let mut request = request();
        for id in [
            " ".to_owned(),
            "unsafe\nidentity".to_owned(),
            "x".repeat(MAX_ID_BYTES + 1),
        ] {
            request.contribution_id = id;
            assert_eq!(
                request.validate(),
                Err(ComposerValidationError::InvalidPayload)
            );
        }
        request.contribution_id.clear();
        request.validate().unwrap();
    }

    #[test]
    fn bounded_requests_and_replacements() {
        let mut request = request();
        request.max_results = MAX_COMPOSER_CANDIDATES + 1;
        assert_eq!(
            request.validate(),
            Err(ComposerValidationError::InvalidPayload)
        );
        request.max_results = 1;
        request.activation.query = "q".repeat(MAX_COMPOSER_QUERY_BYTES + 1);
        assert_eq!(
            request.validate(),
            Err(ComposerValidationError::InvalidPayload)
        );
        let mut invalid = edit();
        invalid.replacement = "x".repeat(MAX_COMPOSER_REPLACEMENT_BYTES + 1);
        assert_eq!(
            invalid.apply(&request.document),
            Err(ComposerValidationError::InvalidPayload)
        );
        request.document.cursor = 2;
        assert_eq!(
            request.document.validate(),
            Err(ComposerValidationError::InvalidPayload)
        );
    }
}
