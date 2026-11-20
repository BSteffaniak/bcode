#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Portable feedback records and the `bcode.feedback/v1` service contract.
//!
//! A feedback item is something a person said about an application, recorded with the
//! application's own key/value facets. Items are triaged by an agent (title, category,
//! severity, tags, open questions), linked to related items, and resolved. An agent may
//! resolve an item, but only a person verifies it: [`FeedbackStatus::AgentResolved`] and
//! [`FeedbackStatus::Verified`] are distinct states.
//!
//! Every request is validated whole. Out-of-bounds values are rejected, never truncated.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Plugin service interface owning the feedback store.
pub const FEEDBACK_SERVICE_INTERFACE_ID: &str = "bcode.feedback/v1";
/// Plugin that owns the feedback store.
pub const FEEDBACK_PLUGIN_ID: &str = "bcode.feedback";
/// Session origin application id of the feedback intake session.
pub const FEEDBACK_INTAKE_APP_ID: &str = "bcode.feedback";

/// Record one new item: [`RecordFeedbackRequest`] → [`FeedbackItem`].
pub const OP_RECORD: &str = "record";
/// Read one item: [`GetFeedbackRequest`] → [`FeedbackItem`].
pub const OP_GET: &str = "get";
/// List matching items newest first: [`FeedbackQuery`] → [`FeedbackPage`].
pub const OP_LIST: &str = "list";
/// Replace an item's triage: [`TriageFeedbackRequest`] → [`FeedbackItem`].
pub const OP_TRIAGE: &str = "triage";
/// Change an item's status: [`SetFeedbackStatusRequest`] → [`FeedbackItem`].
pub const OP_SET_STATUS: &str = "set_status";
/// Link two items: [`LinkFeedbackRequest`] → [`FeedbackItem`] (the source item).
pub const OP_LINK: &str = "link";
/// Read the intake session: no payload → [`IntakeSession`].
pub const OP_INTAKE_SESSION: &str = "intake_session";
/// Set the intake session if it still matches: [`ClaimIntakeSessionRequest`] → [`IntakeSession`].
pub const OP_CLAIM_INTAKE_SESSION: &str = "claim_intake_session";
/// Copy a file or directory into an item: [`AttachFeedbackRequest`] → [`FeedbackItem`].
pub const OP_ATTACH: &str = "attach";

/// Maximum UTF-8 bytes of feedback text.
pub const MAX_TEXT_BYTES: usize = 32 * 1024;
/// Maximum UTF-8 bytes of an application id.
pub const MAX_APP_ID_BYTES: usize = 64;
/// Maximum facets on one item.
pub const MAX_FACETS: usize = 32;
/// Maximum UTF-8 bytes of a facet key.
pub const MAX_FACET_KEY_BYTES: usize = 64;
/// Maximum UTF-8 bytes of a facet value.
pub const MAX_FACET_VALUE_BYTES: usize = 256;
/// Maximum UTF-8 bytes of a triage title.
pub const MAX_TITLE_BYTES: usize = 200;
/// Maximum UTF-8 bytes of a triage summary or a status note.
pub const MAX_NOTE_BYTES: usize = 4 * 1024;
/// Maximum UTF-8 bytes of a category or tag.
pub const MAX_LABEL_BYTES: usize = 64;
/// Maximum tags on one item.
pub const MAX_TAGS: usize = 16;
/// Maximum open questions on one item.
pub const MAX_OPEN_QUESTIONS: usize = 8;
/// Maximum UTF-8 bytes of one open question.
pub const MAX_QUESTION_BYTES: usize = 1024;
/// Maximum commit references on one resolution.
pub const MAX_COMMITS: usize = 16;
/// Maximum UTF-8 bytes of a commit or session reference.
pub const MAX_REFERENCE_BYTES: usize = 128;
/// Maximum items in one listed page.
pub const MAX_PAGE_LIMIT: usize = 100;
/// Value shown to models in place of a sensitive facet value.
pub const REDACTED_VALUE: &str = "[sensitive]";
/// Maximum attachments on one item.
pub const MAX_ATTACHMENTS: usize = 16;
/// Maximum total bytes of one attachment, counting every file of a directory.
pub const MAX_ATTACHMENT_BYTES: u64 = 256 * 1024 * 1024;
/// Maximum files in one directory attachment.
pub const MAX_ATTACHMENT_FILES: usize = 1024;
/// Maximum UTF-8 bytes of an attachment name or media type.
pub const MAX_ATTACHMENT_NAME_BYTES: usize = 128;

/// Reason a feedback value or request is rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FeedbackValidationError {
    /// Feedback text is empty or longer than [`MAX_TEXT_BYTES`].
    #[error("feedback text must be 1-{MAX_TEXT_BYTES} bytes")]
    InvalidText,
    /// The application id is empty, too long, or outside `[a-z0-9._-]`.
    #[error("app id must be 1-{MAX_APP_ID_BYTES} bytes of [a-z0-9._-]")]
    InvalidAppId,
    /// More than [`MAX_FACETS`] facets.
    #[error("at most {MAX_FACETS} facets")]
    TooManyFacets,
    /// A facet key is empty, too long, or outside `[a-z0-9._-]`.
    #[error("facet key {0:?} must be 1-{MAX_FACET_KEY_BYTES} bytes of [a-z0-9._-]")]
    InvalidFacetKey(String),
    /// A facet key appears twice.
    #[error("facet key {0:?} appears more than once")]
    DuplicateFacetKey(String),
    /// A facet value is empty, too long, or contains control characters.
    #[error(
        "facet value for {0:?} must be 1-{MAX_FACET_VALUE_BYTES} bytes without control characters"
    )]
    InvalidFacetValue(String),
    /// A triage title is empty, too long, or spans lines.
    #[error("title must be 1-{MAX_TITLE_BYTES} bytes on one line")]
    InvalidTitle,
    /// A summary or note is empty or longer than [`MAX_NOTE_BYTES`].
    #[error("summaries and notes must be 1-{MAX_NOTE_BYTES} bytes")]
    InvalidNote,
    /// A category or tag is empty, too long, or outside `[a-z0-9._-]`.
    #[error("category or tag {0:?} must be 1-{MAX_LABEL_BYTES} bytes of [a-z0-9._-]")]
    InvalidLabel(String),
    /// More than [`MAX_TAGS`] tags.
    #[error("at most {MAX_TAGS} tags")]
    TooManyTags,
    /// More than [`MAX_OPEN_QUESTIONS`] questions, or one is empty or too long.
    #[error("at most {MAX_OPEN_QUESTIONS} open questions of 1-{MAX_QUESTION_BYTES} bytes")]
    InvalidOpenQuestions,
    /// More than [`MAX_COMMITS`] commits, or a commit or session reference is malformed.
    #[error("at most {MAX_COMMITS} references of 1-{MAX_REFERENCE_BYTES} bytes without whitespace")]
    InvalidReference,
    /// An item cannot link to itself.
    #[error("an item cannot link to itself")]
    SelfLink,
    /// A page limit is zero or larger than [`MAX_PAGE_LIMIT`].
    #[error("page limit must be 1-{MAX_PAGE_LIMIT}")]
    InvalidLimit,
    /// An attachment name is empty, too long, or not a plain file name.
    #[error(
        "attachment names must be 1-{MAX_ATTACHMENT_NAME_BYTES} bytes of [A-Za-z0-9._-], not starting with a dot"
    )]
    InvalidAttachmentName,
    /// An attachment media type is empty, too long, or malformed.
    #[error(
        "attachment media types look like text/plain and are at most {MAX_ATTACHMENT_NAME_BYTES} bytes"
    )]
    InvalidMediaType,
    /// An attachment source is not an absolute path.
    #[error("attachment sources must be absolute paths")]
    RelativeAttachmentSource,
    /// A feedback id does not parse.
    #[error("feedback ids look like fb-12")]
    InvalidId,
}

/// Stable, human-friendly identity of a feedback item, written `fb-<n>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FeedbackId(pub u64);

impl fmt::Display for FeedbackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "fb-{}", self.0)
    }
}

impl FromStr for FeedbackId {
    type Err = FeedbackValidationError;

    /// Parse `fb-12`, `FB-12` or `12`.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let trimmed = value.trim();
        let digits = trimmed
            .get(..3)
            .filter(|prefix| prefix.eq_ignore_ascii_case("fb-"))
            .map_or(trimmed, |_| &trimmed[3..]);
        digits
            .parse::<u64>()
            .ok()
            .filter(|id| *id > 0)
            .map(Self)
            .ok_or(FeedbackValidationError::InvalidId)
    }
}

/// Application-provided key/value context, such as `bcode.model` or `bdata.connection`.
///
/// Sensitive values are stored but replaced by [`REDACTED_VALUE`] wherever a model reads them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facet {
    /// Namespaced key in `[a-z0-9._-]`, such as `bdata.connection`.
    pub key: String,
    /// Single-line display value.
    pub value: String,
    /// Withhold the value from models.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sensitive: bool,
}

impl Facet {
    /// Create a non-sensitive facet.
    #[must_use]
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
            sensitive: false,
        }
    }

    /// Return the value a model may read: the value itself, or [`REDACTED_VALUE`].
    #[must_use]
    pub fn model_value(&self) -> &str {
        if self.sensitive {
            REDACTED_VALUE
        } else {
            &self.value
        }
    }
}

/// How much an item matters, as judged during triage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Cosmetic or nice-to-have.
    Low,
    /// Noticeable friction with a workaround.
    Medium,
    /// Blocks a workflow or loses work.
    High,
    /// Data loss, security, or unusable application.
    Critical,
}

/// Lifecycle of an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackStatus {
    /// Not yet resolved.
    Open,
    /// An agent considers it resolved; a person has not verified that.
    AgentResolved,
    /// A person verified the resolution.
    Verified,
    /// A person decided not to act on it.
    Dismissed,
}

/// Whether an agent has triaged an item yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriageState {
    /// Recorded but not yet triaged.
    Pending,
    /// Title, category and the rest have been set.
    Triaged,
}

/// Who made a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    /// The person giving feedback, through a CLI or application.
    User,
    /// A model, through feedback tools.
    Agent,
}

/// Relationship between two items.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    /// Reports the same problem as the target.
    Duplicate,
    /// Shares a cause, area or likely fix with the target.
    Related,
    /// Is a consequence of the target.
    CausedBy,
}

/// A directed link to another item.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FeedbackLink {
    /// Relationship from this item to the target.
    pub kind: LinkKind,
    /// The linked item.
    pub target: FeedbackId,
}

/// Agent-assigned interpretation of an item.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Triage {
    /// One-line title.
    pub title: String,
    /// Restated problem and expectation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Broad kind, such as `bug`, `ux` or `performance`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// How much it matters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<Severity>,
    /// Free-form labels in `[a-z0-9._-]`.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub tags: BTreeSet<String>,
    /// Questions only the person can answer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_questions: Vec<String>,
}

/// The latest status change, with who made it and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusChange {
    /// Who changed the status.
    pub by: Actor,
    /// When, in Unix milliseconds.
    pub at_ms: u64,
    /// Explanation, such as how it was fixed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Session that did the work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Commits that fixed it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<String>,
}

/// One recorded feedback item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackItem {
    /// Stable identity.
    pub id: FeedbackId,
    /// Application the feedback is about.
    pub app: String,
    /// What the person said, verbatim.
    pub text: String,
    /// Application-provided context.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facets: Vec<Facet>,
    /// When it was recorded, in Unix milliseconds.
    pub created_at_ms: u64,
    /// When it last changed, in Unix milliseconds.
    pub updated_at_ms: u64,
    /// Whether it has been triaged.
    pub triage_state: TriageState,
    /// Agent interpretation; empty until triaged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage: Option<Triage>,
    /// Lifecycle state.
    pub status: FeedbackStatus,
    /// Latest status change, absent while it has always been open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_change: Option<StatusChange>,
    /// Outgoing links.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<FeedbackLink>,
    /// Files and directories attached to the item, such as a recording clip.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<FeedbackAttachment>,
}

impl FeedbackItem {
    /// Return a copy safe for model context: sensitive facet values and the locations of
    /// sensitive attachments are redacted.
    #[must_use]
    pub fn redacted(&self) -> Self {
        let mut item = self.clone();
        for facet in &mut item.facets {
            if facet.sensitive {
                REDACTED_VALUE.clone_into(&mut facet.value);
            }
        }
        for attachment in &mut item.attachments {
            if attachment.sensitive {
                REDACTED_VALUE.clone_into(&mut attachment.path);
            }
        }
        item
    }

    /// Return the triage title, or the first line of the text.
    #[must_use]
    pub fn headline(&self) -> &str {
        self.triage.as_ref().map_or_else(
            || self.text.lines().next().unwrap_or_default().trim(),
            |triage| triage.title.as_str(),
        )
    }
}

/// Whether an attachment is one file or a directory tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    /// A regular file.
    File,
    /// A directory of regular files, such as a recording in its native format.
    Directory,
}

/// A file or directory copied into the feedback store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackAttachment {
    /// Position on the item, from 1.
    pub ordinal: u32,
    /// Plain name, such as `recording.gif`.
    pub name: String,
    /// File or directory.
    pub kind: AttachmentKind,
    /// Media type, such as `image/gif` or `application/x-bmux-recording`.
    pub media_type: String,
    /// Total bytes.
    pub bytes: u64,
    /// What it shows and why it was attached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Withhold its location from models.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sensitive: bool,
    /// Absolute path of the stored copy, or [`REDACTED_VALUE`] for models.
    pub path: String,
    /// When it was attached, in Unix milliseconds.
    pub created_at_ms: u64,
}

/// Copy a file or directory into an item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachFeedbackRequest {
    /// Item to attach to.
    pub id: FeedbackId,
    /// Absolute path of the file or directory to copy. Symbolic links are refused.
    pub source: String,
    /// Plain name for the stored copy.
    pub name: String,
    /// Media type, such as `text/plain`.
    pub media_type: String,
    /// What it shows and why it was attached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Withhold its location from models.
    #[serde(default)]
    pub sensitive: bool,
}

impl AttachFeedbackRequest {
    /// Check the name, media type, description and that the source is absolute.
    ///
    /// # Errors
    /// [`FeedbackValidationError::InvalidAttachmentName`], `InvalidMediaType`, `InvalidNote` or
    /// `RelativeAttachmentSource`.
    pub fn validate(&self) -> Result<(), FeedbackValidationError> {
        let name_ok = !self.name.is_empty()
            && self.name.len() <= MAX_ATTACHMENT_NAME_BYTES
            && !self.name.starts_with('.')
            && self
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
        if !name_ok {
            return Err(FeedbackValidationError::InvalidAttachmentName);
        }
        let media_ok = self.media_type.len() <= MAX_ATTACHMENT_NAME_BYTES
            && self
                .media_type
                .split_once('/')
                .is_some_and(|(kind, subtype)| {
                    !kind.is_empty()
                        && !subtype.is_empty()
                        && self.media_type.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric()
                                || matches!(byte, b'/' | b'.' | b'+' | b'-')
                        })
                });
        if !media_ok {
            return Err(FeedbackValidationError::InvalidMediaType);
        }
        if let Some(description) = &self.description {
            validate_note(description)?;
        }
        if !std::path::Path::new(&self.source).is_absolute() {
            return Err(FeedbackValidationError::RelativeAttachmentSource);
        }
        Ok(())
    }
}

/// Record a new item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordFeedbackRequest {
    /// Application the feedback is about.
    pub app: String,
    /// What the person said.
    pub text: String,
    /// Application-provided context.
    #[serde(default)]
    pub facets: Vec<Facet>,
}

impl RecordFeedbackRequest {
    /// Check every bound.
    ///
    /// # Errors
    /// * [`FeedbackValidationError::InvalidAppId`] for a malformed app id.
    /// * [`FeedbackValidationError::InvalidText`] for empty or oversized text.
    /// * A facet error for too many, duplicate, or malformed facets.
    pub fn validate(&self) -> Result<(), FeedbackValidationError> {
        validate_app_id(&self.app)?;
        if self.text.trim().is_empty() || self.text.len() > MAX_TEXT_BYTES {
            return Err(FeedbackValidationError::InvalidText);
        }
        validate_facets(&self.facets)
    }
}

/// Read one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetFeedbackRequest {
    /// Item to read.
    pub id: FeedbackId,
}

/// Filters for listing items. Every filter that is set must match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackQuery {
    /// Exact application id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// Any of these statuses; all statuses when empty.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub statuses: BTreeSet<FeedbackStatus>,
    /// Triage state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_state: Option<TriageState>,
    /// Exact category.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Every one of these tags.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub tags: BTreeSet<String>,
    /// Every one of these exact facet key/value pairs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facets: Vec<(String, String)>,
    /// Case-insensitive substring of the text, title or summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Continue after this id (exclusive), from a previous page's `next_before`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<FeedbackId>,
    /// Maximum items returned.
    pub limit: usize,
}

impl Default for FeedbackQuery {
    fn default() -> Self {
        Self {
            app: None,
            statuses: BTreeSet::new(),
            triage_state: None,
            category: None,
            tags: BTreeSet::new(),
            facets: Vec::new(),
            text: None,
            before: None,
            limit: 25,
        }
    }
}

impl FeedbackQuery {
    /// Check the page limit.
    ///
    /// # Errors
    /// [`FeedbackValidationError::InvalidLimit`] when the limit is zero or above
    /// [`MAX_PAGE_LIMIT`].
    pub const fn validate(&self) -> Result<(), FeedbackValidationError> {
        if self.limit == 0 || self.limit > MAX_PAGE_LIMIT {
            return Err(FeedbackValidationError::InvalidLimit);
        }
        Ok(())
    }
}

/// One page of matching items, newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackPage {
    /// Matching items.
    pub items: Vec<FeedbackItem>,
    /// Pass as [`FeedbackQuery::before`] to continue; absent when nothing older exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_before: Option<FeedbackId>,
}

/// Replace an item's triage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriageFeedbackRequest {
    /// Item to triage.
    pub id: FeedbackId,
    /// New triage.
    pub triage: Triage,
}

impl TriageFeedbackRequest {
    /// Check every bound.
    ///
    /// # Errors
    /// A title, summary, label, tag-count or open-question error for out-of-bounds values.
    pub fn validate(&self) -> Result<(), FeedbackValidationError> {
        let triage = &self.triage;
        if triage.title.trim().is_empty()
            || triage.title.len() > MAX_TITLE_BYTES
            || triage.title.contains(['\n', '\r'])
        {
            return Err(FeedbackValidationError::InvalidTitle);
        }
        if let Some(summary) = &triage.summary {
            validate_note(summary)?;
        }
        if let Some(category) = &triage.category {
            validate_label(category)?;
        }
        if triage.tags.len() > MAX_TAGS {
            return Err(FeedbackValidationError::TooManyTags);
        }
        for tag in &triage.tags {
            validate_label(tag)?;
        }
        if triage.open_questions.len() > MAX_OPEN_QUESTIONS
            || triage
                .open_questions
                .iter()
                .any(|question| question.trim().is_empty() || question.len() > MAX_QUESTION_BYTES)
        {
            return Err(FeedbackValidationError::InvalidOpenQuestions);
        }
        Ok(())
    }
}

/// Change an item's status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetFeedbackStatusRequest {
    /// Item to change.
    pub id: FeedbackId,
    /// New status.
    pub status: FeedbackStatus,
    /// Who is changing it.
    pub actor: Actor,
    /// Explanation, such as how it was fixed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Session that did the work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Commits that fixed it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<String>,
}

impl SetFeedbackStatusRequest {
    /// Check every bound.
    ///
    /// # Errors
    /// [`FeedbackValidationError::InvalidNote`] or
    /// [`FeedbackValidationError::InvalidReference`] for out-of-bounds values.
    pub fn validate(&self) -> Result<(), FeedbackValidationError> {
        if let Some(note) = &self.note {
            validate_note(note)?;
        }
        if self.commits.len() > MAX_COMMITS {
            return Err(FeedbackValidationError::InvalidReference);
        }
        self.session_id
            .iter()
            .chain(&self.commits)
            .try_for_each(|reference| validate_reference(reference))
    }
}

/// Link one item to another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkFeedbackRequest {
    /// Source item.
    pub from: FeedbackId,
    /// Target item.
    pub to: FeedbackId,
    /// Relationship from source to target.
    pub kind: LinkKind,
}

/// The single long-lived session that triages new feedback.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntakeSession {
    /// Session id, absent until one has been claimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Set the intake session when the current one still equals `expected`.
///
/// Concurrent submitters each create a candidate session; exactly one claim wins and the
/// others adopt the winner from the response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimIntakeSessionRequest {
    /// The intake session the caller observed, absent when there was none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    /// The session to install.
    pub session_id: String,
}

impl ClaimIntakeSessionRequest {
    /// Check the session references.
    ///
    /// # Errors
    /// [`FeedbackValidationError::InvalidReference`] for a malformed session id.
    pub fn validate(&self) -> Result<(), FeedbackValidationError> {
        self.expected
            .iter()
            .chain(std::iter::once(&self.session_id))
            .try_for_each(|reference| validate_reference(reference))
    }
}

/// Check an application id: 1-64 bytes of `[a-z0-9._-]`.
///
/// # Errors
/// [`FeedbackValidationError::InvalidAppId`] when malformed.
pub fn validate_app_id(app: &str) -> Result<(), FeedbackValidationError> {
    if is_identifier(app, MAX_APP_ID_BYTES) {
        Ok(())
    } else {
        Err(FeedbackValidationError::InvalidAppId)
    }
}

/// Check a facet list: count, key shape, unique keys and value shape.
///
/// # Errors
/// [`FeedbackValidationError::TooManyFacets`], `InvalidFacetKey`, `DuplicateFacetKey` or
/// `InvalidFacetValue`.
pub fn validate_facets(facets: &[Facet]) -> Result<(), FeedbackValidationError> {
    if facets.len() > MAX_FACETS {
        return Err(FeedbackValidationError::TooManyFacets);
    }
    let mut keys = BTreeSet::new();
    for facet in facets {
        if !is_identifier(&facet.key, MAX_FACET_KEY_BYTES) {
            return Err(FeedbackValidationError::InvalidFacetKey(facet.key.clone()));
        }
        if !keys.insert(facet.key.as_str()) {
            return Err(FeedbackValidationError::DuplicateFacetKey(
                facet.key.clone(),
            ));
        }
        if facet.value.trim().is_empty()
            || facet.value.len() > MAX_FACET_VALUE_BYTES
            || facet.value.chars().any(char::is_control)
        {
            return Err(FeedbackValidationError::InvalidFacetValue(
                facet.key.clone(),
            ));
        }
    }
    Ok(())
}

/// Check a category or tag: 1-64 bytes of `[a-z0-9._-]`.
///
/// # Errors
/// [`FeedbackValidationError::InvalidLabel`] when malformed.
pub fn validate_label(label: &str) -> Result<(), FeedbackValidationError> {
    if is_identifier(label, MAX_LABEL_BYTES) {
        Ok(())
    } else {
        Err(FeedbackValidationError::InvalidLabel(label.to_owned()))
    }
}

fn validate_note(note: &str) -> Result<(), FeedbackValidationError> {
    if note.trim().is_empty() || note.len() > MAX_NOTE_BYTES {
        Err(FeedbackValidationError::InvalidNote)
    } else {
        Ok(())
    }
}

fn validate_reference(reference: &str) -> Result<(), FeedbackValidationError> {
    if reference.is_empty()
        || reference.len() > MAX_REFERENCE_BYTES
        || reference
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        Err(FeedbackValidationError::InvalidReference)
    } else {
        Ok(())
    }
}

fn is_identifier(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value
            .bytes()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_through_their_display_form() {
        let id = FeedbackId(42);
        assert_eq!(id.to_string(), "fb-42");
        assert_eq!("fb-42".parse::<FeedbackId>(), Ok(id));
        assert_eq!("FB-42".parse::<FeedbackId>(), Ok(id));
        assert_eq!("42".parse::<FeedbackId>(), Ok(id));
        assert!("fb-0".parse::<FeedbackId>().is_err());
        assert!("fb-x".parse::<FeedbackId>().is_err());
        assert!("".parse::<FeedbackId>().is_err());
    }

    #[test]
    fn records_reject_out_of_bounds_values_instead_of_truncating() {
        let valid = RecordFeedbackRequest {
            app: "bdata".into(),
            text: "copy failed".into(),
            facets: vec![Facet::new("bdata.connection", "prod")],
        };
        assert_eq!(valid.validate(), Ok(()));

        let mut bad_app = valid.clone();
        bad_app.app = "BData".into();
        assert_eq!(
            bad_app.validate(),
            Err(FeedbackValidationError::InvalidAppId)
        );

        let mut empty = valid.clone();
        empty.text = "  ".into();
        assert_eq!(empty.validate(), Err(FeedbackValidationError::InvalidText));

        let mut duplicate = valid.clone();
        duplicate.facets.push(Facet::new("bdata.connection", "dev"));
        assert!(matches!(
            duplicate.validate(),
            Err(FeedbackValidationError::DuplicateFacetKey(_))
        ));

        let mut multiline = valid;
        multiline.facets = vec![Facet::new("k", "a\nb")];
        assert!(matches!(
            multiline.validate(),
            Err(FeedbackValidationError::InvalidFacetValue(_))
        ));
    }

    #[test]
    fn redaction_hides_only_sensitive_values() {
        let item = FeedbackItem {
            id: FeedbackId(1),
            app: "bdata".into(),
            text: "slow".into(),
            facets: vec![
                Facet::new("bdata.dialect", "postgres"),
                Facet {
                    key: "bdata.connection".into(),
                    value: "postgres://user:secret@db".into(),
                    sensitive: true,
                },
            ],
            created_at_ms: 1,
            updated_at_ms: 1,
            triage_state: TriageState::Pending,
            triage: None,
            status: FeedbackStatus::Open,
            status_change: None,
            links: Vec::new(),
            attachments: vec![FeedbackAttachment {
                ordinal: 1,
                name: "clip".into(),
                kind: AttachmentKind::Directory,
                media_type: "application/x-bmux-recording".into(),
                bytes: 10,
                description: None,
                sensitive: true,
                path: "/state/feedback/attachments/fb-1/1-clip".into(),
                created_at_ms: 1,
            }],
        };
        let redacted = item.redacted();
        assert_eq!(redacted.attachments[0].path, REDACTED_VALUE);
        assert_eq!(redacted.facets[0].value, "postgres");
        assert_eq!(redacted.facets[1].value, REDACTED_VALUE);
        assert!(!serde_json::to_string(&redacted).unwrap().contains("secret"));
    }

    #[test]
    fn attachments_need_plain_names_typed_media_and_absolute_sources() {
        let valid = AttachFeedbackRequest {
            id: FeedbackId(1),
            source: "/tmp/clip.gif".into(),
            name: "clip.gif".into(),
            media_type: "image/gif".into(),
            description: None,
            sensitive: false,
        };
        assert_eq!(valid.validate(), Ok(()));
        for name in ["", "../escape", ".hidden", "a/b", "spaced name"] {
            let request = AttachFeedbackRequest {
                name: name.into(),
                ..valid.clone()
            };
            assert_eq!(
                request.validate(),
                Err(FeedbackValidationError::InvalidAttachmentName),
                "{name:?}"
            );
        }
        let relative = AttachFeedbackRequest {
            source: "clip.gif".into(),
            ..valid.clone()
        };
        assert_eq!(
            relative.validate(),
            Err(FeedbackValidationError::RelativeAttachmentSource)
        );
        let untyped = AttachFeedbackRequest {
            media_type: "gif".into(),
            ..valid
        };
        assert_eq!(
            untyped.validate(),
            Err(FeedbackValidationError::InvalidMediaType)
        );
    }

    #[test]
    fn triage_rejects_multiline_titles_and_malformed_tags() {
        let mut request = TriageFeedbackRequest {
            id: FeedbackId(1),
            triage: Triage {
                title: "Copy fails on symlinks".into(),
                tags: BTreeSet::from(["clipboard".to_owned()]),
                ..Triage::default()
            },
        };
        assert_eq!(request.validate(), Ok(()));
        request.triage.title = "a\nb".into();
        assert_eq!(
            request.validate(),
            Err(FeedbackValidationError::InvalidTitle)
        );
        request.triage.title = "ok".into();
        request.triage.tags.insert("Has Space".into());
        assert!(matches!(
            request.validate(),
            Err(FeedbackValidationError::InvalidLabel(_))
        ));
    }
}
