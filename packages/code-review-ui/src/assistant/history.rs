//! The assistant's conversation history: which conversations exist and where each started.
//!
//! * **Rows.** One bounded catalog page of the sessions the review app created (origin
//!   [`crate::host::REVIEW_SESSION_APP_ID`]) merged by session id with the review store's
//!   assistant bindings of the repository. Sessions created before origins were recorded are
//!   found only through their binding; their summaries are read separately, and a binding whose
//!   session Bcode no longer has is left out (nothing is deleted on read).
//! * **Tags.** Where a conversation started never changes: the origin's `workspace` and `layer`
//!   entries, else the binding's `started` tag, else the binding's review. Titles come from the
//!   repository's review list; a saved review it no longer has is a deleted review.
//! * **Order.** Newest activity first, then session id.
//!
//! [`merge`] is a pure function of its inputs; the panel loads them and keeps the result.

use crate::host::{
    AppSession, ORIGIN_LAYER, ORIGIN_PURPOSE, ORIGIN_WORKSPACE, REVIEW_SESSION_APP_ID,
    ReviewSessionPurpose,
};
use bcode_code_review_models::{ReviewAssistantBinding, ReviewAssistantTag};
use std::collections::{BTreeMap, BTreeSet};

/// Rows shown at most.
pub const MAX_ROWS: usize = 400;

/// Where a conversation started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tag {
    /// No review was on screen (the reviews home or the wizard).
    Home,
    /// A review.
    Review {
        /// Annotation scope id of the review.
        workspace_id: String,
        /// Its title.
        title: String,
        /// The stack layer that was current, when any.
        layer: Option<u64>,
    },
    /// A saved review that no longer exists.
    DeletedReview {
        /// Annotation scope id it had.
        workspace_id: String,
        /// The stack layer that was current, when any.
        layer: Option<u64>,
    },
}

impl Tag {
    /// Short label: `home`, the review title with `#layer`, or `deleted review`.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Home => "home".to_owned(),
            Self::Review { title, layer, .. } => {
                layer.map_or_else(|| title.clone(), |layer| format!("{title} #{layer}"))
            }
            Self::DeletedReview { layer, .. } => layer.map_or_else(
                || "deleted review".to_owned(),
                |layer| format!("deleted review #{layer}"),
            ),
        }
    }

    /// The review's annotation scope id, when the conversation started in one.
    #[must_use]
    pub fn workspace_id(&self) -> Option<&str> {
        match self {
            Self::Home => None,
            Self::Review { workspace_id, .. } | Self::DeletedReview { workspace_id, .. } => {
                Some(workspace_id)
            }
        }
    }

    /// The stack layer that was current, when any.
    #[must_use]
    pub const fn layer(&self) -> Option<u64> {
        match self {
            Self::Home => None,
            Self::Review { layer, .. } | Self::DeletedReview { layer, .. } => *layer,
        }
    }
}

/// What a conversation is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// An assistant panel conversation.
    Assistant,
    /// An Ask AI card's session (a question on a place in the diff, or an AI command).
    Card,
}

impl Kind {
    /// Label shown on the row (empty for panel conversations).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Assistant => "",
            Self::Card => "card",
        }
    }
}

/// One conversation of the history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRow {
    /// Bcode session id.
    pub session_id: String,
    /// Title (the session's, else a fallback naming what it is).
    pub title: String,
    /// Where it started.
    pub tag: Tag,
    /// What it is.
    pub kind: Kind,
    /// Last activity, in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
}

impl HistoryRow {
    /// Whether the row matches `query` (title, tag, review id, layer and kind; case-insensitive
    /// words that must all appear).
    #[must_use]
    pub fn matches(&self, query: &str) -> bool {
        let haystack = format!(
            "{} {} {} {} {}",
            self.title,
            self.tag.label(),
            self.tag.workspace_id().unwrap_or_default(),
            self.tag
                .layer()
                .map(|layer| format!("#{layer}"))
                .unwrap_or_default(),
            self.kind.label(),
        )
        .to_lowercase();
        query
            .split_whitespace()
            .all(|word| haystack.contains(&word.to_lowercase()))
    }
}

/// What the history is merged from.
#[derive(Debug, Clone, Copy)]
pub struct HistorySources<'a> {
    /// The catalog page of the review app's sessions.
    pub catalog: &'a [AppSession],
    /// The repository's assistant bindings.
    pub bindings: &'a [ReviewAssistantBinding],
    /// Summaries of sessions known only from a binding: `None` when Bcode does not have the
    /// session; absent when not read (yet).
    pub legacy: &'a BTreeMap<String, Option<AppSession>>,
    /// Titles of the repository's saved reviews, by annotation scope id.
    pub reviews: &'a BTreeMap<String, String>,
}

/// Sessions known only from a binding, whose summaries must be read before they are listed.
#[must_use]
pub fn legacy_sessions(sources: &HistorySources<'_>) -> Vec<String> {
    let listed: BTreeSet<&str> = sources
        .catalog
        .iter()
        .map(|session| session.id.as_str())
        .collect();
    sources
        .bindings
        .iter()
        .map(|binding| binding.session_id.as_str())
        .filter(|id| !listed.contains(id) && !sources.legacy.contains_key(*id))
        .map(str::to_owned)
        .collect()
}

/// Merge the catalog page and the bindings into history rows, newest activity first, at most
/// [`MAX_ROWS`].
#[must_use]
pub fn merge(sources: &HistorySources<'_>) -> Vec<HistoryRow> {
    let bindings: BTreeMap<&str, &ReviewAssistantBinding> = sources
        .bindings
        .iter()
        .map(|binding| (binding.session_id.as_str(), binding))
        .collect();
    let mut rows: BTreeMap<String, HistoryRow> = BTreeMap::new();
    for session in sources.catalog {
        if session.origin_app.as_deref() != Some(REVIEW_SESSION_APP_ID) {
            continue;
        }
        let binding = bindings.get(session.id.as_str()).copied();
        rows.insert(session.id.clone(), row(session, binding, sources.reviews));
    }
    for binding in sources.bindings {
        if rows.contains_key(&binding.session_id) {
            continue;
        }
        // Listed only once Bcode confirmed it still has the session.
        let Some(Some(session)) = sources.legacy.get(&binding.session_id) else {
            continue;
        };
        rows.insert(
            binding.session_id.clone(),
            row(session, Some(binding), sources.reviews),
        );
    }
    let mut rows: Vec<HistoryRow> = rows.into_values().collect();
    rows.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    rows.truncate(MAX_ROWS);
    rows
}

fn row(
    session: &AppSession,
    binding: Option<&ReviewAssistantBinding>,
    reviews: &BTreeMap<String, String>,
) -> HistoryRow {
    let purpose = session
        .origin_context
        .get(ORIGIN_PURPOSE)
        .map(String::as_str);
    let kind = if purpose == Some(ReviewSessionPurpose::Question.as_str()) {
        Kind::Card
    } else {
        Kind::Assistant
    };
    let tag = tag_of(session, binding, reviews);
    let title = session
        .title
        .clone()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| match kind {
            Kind::Card => "Question".to_owned(),
            Kind::Assistant => "Conversation".to_owned(),
        });
    let updated_at_ms = session
        .updated_at_ms
        .max(binding.map_or(0, |binding| binding.updated_at_ms));
    HistoryRow {
        session_id: session.id.clone(),
        title,
        tag,
        kind,
        updated_at_ms,
    }
}

/// The immutable tag: the origin's entries, else the binding's `started`, else its review.
fn tag_of(
    session: &AppSession,
    binding: Option<&ReviewAssistantBinding>,
    reviews: &BTreeMap<String, String>,
) -> Tag {
    let layer = session
        .origin_context
        .get(ORIGIN_LAYER)
        .and_then(|layer| layer.trim_start_matches('#').parse::<u64>().ok());
    if let Some(workspace) = session.origin_context.get(ORIGIN_WORKSPACE) {
        let recorded = binding
            .and_then(|binding| binding.started.as_ref())
            .filter(|started| started.workspace_id.as_deref() == Some(workspace.as_str()))
            .and_then(|started| started.title.clone());
        return review_tag(workspace, recorded, layer, reviews);
    }
    let started = binding.map(|binding| {
        binding
            .started
            .clone()
            .unwrap_or_else(|| ReviewAssistantTag {
                workspace_id: (!binding.is_detached())
                    .then(|| bcode_code_review::scope::annotation_workspace_id(&binding.workspace)),
                title: Some(binding.workspace.title.clone()),
                layer: None,
            })
    });
    match started {
        Some(ReviewAssistantTag {
            workspace_id: Some(workspace),
            title,
            layer: started_layer,
        }) => review_tag(&workspace, title, started_layer.or(layer), reviews),
        // Started on home, or a session whose origin and binding name no review.
        _ => Tag::Home,
    }
}

fn review_tag(
    workspace_id: &str,
    recorded_title: Option<String>,
    layer: Option<u64>,
    reviews: &BTreeMap<String, String>,
) -> Tag {
    if let Some(title) = reviews.get(workspace_id) {
        return Tag::Review {
            workspace_id: workspace_id.to_owned(),
            title: title.clone(),
            layer,
        };
    }
    if bcode_code_review::scope::is_transient_scope_id(workspace_id) {
        return Tag::Review {
            workspace_id: workspace_id.to_owned(),
            title: recorded_title.unwrap_or_else(|| "unsaved review".to_owned()),
            layer,
        };
    }
    Tag::DeletedReview {
        workspace_id: workspace_id.to_owned(),
        layer,
    }
}

/// `2m`, `3h`, `4d` or `5w` since `then_ms` at `now_ms`.
#[must_use]
pub fn relative_time(then_ms: u64, now_ms: u64) -> String {
    let seconds = now_ms.saturating_sub(then_ms) / 1000;
    match seconds {
        0..60 => "now".to_owned(),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        86_400..604_800 => format!("{}d", seconds / 86_400),
        _ => format!("{}w", seconds / 604_800),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_code_review_models::{REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION, ReviewWorkspace};
    use std::path::PathBuf;

    fn workspace(id: &str, title: &str) -> ReviewWorkspace {
        let mut workspace =
            bcode_code_review::scope::transient_workspace(PathBuf::from("/repo"), None, Vec::new());
        workspace.id = id.to_owned();
        workspace.title = title.to_owned();
        workspace
    }

    fn binding(session: &str, id: &str, title: &str, updated: u64) -> ReviewAssistantBinding {
        ReviewAssistantBinding {
            schema_version: REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION,
            session_id: session.to_owned(),
            workspace: workspace(id, title),
            resolved: Vec::new(),
            stacks: Vec::new(),
            created_at_ms: updated,
            updated_at_ms: updated,
            started: None,
            detached_at_ms: None,
        }
    }

    fn session(id: &str, title: &str, updated: u64, context: &[(&str, &str)]) -> AppSession {
        AppSession {
            id: id.to_owned(),
            title: Some(title.to_owned()),
            created_at_ms: updated,
            updated_at_ms: updated,
            origin_app: Some(REVIEW_SESSION_APP_ID.to_owned()),
            origin_context: context
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
        }
    }

    fn reviews(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(id, title)| ((*id).to_owned(), (*title).to_owned()))
            .collect()
    }

    #[test]
    fn legacy_bindings_are_listed_tagged_with_their_review_once_bcode_has_them() {
        let bindings = vec![
            binding("old", "rv-1", "Retry backoff", 10),
            binding("gone", "rv-1", "Retry backoff", 20),
        ];
        let reviews = reviews(&[("rv-1", "Retry backoff")]);
        let mut legacy = BTreeMap::new();
        let sources = HistorySources {
            catalog: &[],
            bindings: &bindings,
            legacy: &legacy,
            reviews: &reviews,
        };
        assert_eq!(legacy_sessions(&sources), vec!["old", "gone"]);
        // Not listed before Bcode confirmed it has the session.
        assert_eq!(merge(&sources), Vec::new());

        legacy.insert(
            "old".to_owned(),
            Some(AppSession {
                origin_app: None,
                ..session("old", "Review assistant: Retry backoff", 10, &[])
            }),
        );
        legacy.insert("gone".to_owned(), None);
        let sources = HistorySources {
            catalog: &[],
            bindings: &bindings,
            legacy: &legacy,
            reviews: &reviews,
        };
        assert_eq!(legacy_sessions(&sources), Vec::<String>::new());
        let rows = merge(&sources);
        assert_eq!(rows.len(), 1, "a session Bcode no longer has is left out");
        assert_eq!(rows[0].session_id, "old");
        assert_eq!(rows[0].tag.label(), "Retry backoff");
        assert_eq!(rows[0].kind, Kind::Assistant);
    }

    #[test]
    fn origin_entries_tag_conversations_and_cards_are_labelled() {
        let catalog = vec![
            session(
                "a",
                "Why the cap moved",
                300,
                &[
                    ("purpose", "assistant"),
                    ("workspace", "rv-1"),
                    ("layer", "#103"),
                ],
            ),
            session(
                "b",
                "Explain",
                200,
                &[("purpose", "question"), ("workspace", "rv-1")],
            ),
            session("c", "Create a review", 100, &[("purpose", "assistant")]),
            session(
                "d",
                "Old",
                50,
                &[("purpose", "assistant"), ("workspace", "rv-9")],
            ),
        ];
        // The binding moved to another review since; the tag stays where it started.
        let bindings = vec![binding("a", "rv-2", "Auth", 400)];
        let reviews = reviews(&[("rv-1", "Retry backoff"), ("rv-2", "Auth")]);
        let legacy = BTreeMap::new();
        let rows = merge(&HistorySources {
            catalog: &catalog,
            bindings: &bindings,
            legacy: &legacy,
            reviews: &reviews,
        });
        let labels: Vec<(&str, String, Kind)> = rows
            .iter()
            .map(|row| (row.session_id.as_str(), row.tag.label(), row.kind))
            .collect();
        assert_eq!(
            labels,
            vec![
                ("a", "Retry backoff #103".to_owned(), Kind::Assistant),
                ("b", "Retry backoff".to_owned(), Kind::Card),
                ("c", "home".to_owned(), Kind::Assistant),
                ("d", "deleted review".to_owned(), Kind::Assistant),
            ]
        );
        assert_eq!(rows[0].updated_at_ms, 400, "binding activity counts");
        assert!(rows[0].matches("retry #103"));
        assert!(rows[1].matches("CARD"));
        assert!(!rows[2].matches("retry"));
    }

    #[test]
    fn the_started_tag_wins_over_the_current_attachment() {
        let mut moved = binding("s", "rv-2", "Auth", 10);
        moved.started = Some(ReviewAssistantTag {
            workspace_id: Some("rv-1".to_owned()),
            title: Some("Retry".to_owned()),
            layer: Some(7),
        });
        let mut detached = binding("h", "rv-2", "Auth", 5);
        detached.detached_at_ms = Some(5);
        detached.started = Some(ReviewAssistantTag::none());
        let bindings = vec![moved, detached];
        let reviews = reviews(&[("rv-1", "Retry backoff")]);
        let legacy: BTreeMap<String, Option<AppSession>> = [
            ("s".to_owned(), Some(session("s", "t", 1, &[]))),
            ("h".to_owned(), Some(session("h", "t", 1, &[]))),
        ]
        .into_iter()
        .collect();
        let rows = merge(&HistorySources {
            catalog: &[],
            bindings: &bindings,
            legacy: &legacy,
            reviews: &reviews,
        });
        assert_eq!(rows[0].tag.label(), "Retry backoff #7");
        assert_eq!(rows[1].tag, Tag::Home);
    }

    #[test]
    fn rows_are_newest_first_and_bounded() {
        let catalog: Vec<AppSession> = (0..(MAX_ROWS as u64 + 20))
            .map(|index| session(&format!("s{index:04}"), "t", index, &[]))
            .collect();
        let legacy = BTreeMap::new();
        let rows = merge(&HistorySources {
            catalog: &catalog,
            bindings: &[],
            legacy: &legacy,
            reviews: &BTreeMap::new(),
        });
        assert_eq!(rows.len(), MAX_ROWS);
        assert!(
            rows.windows(2)
                .all(|pair| pair[0].updated_at_ms >= pair[1].updated_at_ms)
        );
        assert_eq!(rows[0].session_id, format!("s{:04}", MAX_ROWS + 19));
        // Sessions of other apps are never listed.
        let other = vec![AppSession {
            origin_app: Some("bdata".to_owned()),
            ..session("x", "t", 1, &[])
        }];
        let others = merge(&HistorySources {
            catalog: &other,
            bindings: &[],
            legacy: &legacy,
            reviews: &BTreeMap::new(),
        });
        assert_eq!(others, Vec::new());
    }

    #[test]
    fn relative_times() {
        assert_eq!(relative_time(0, 30_000), "now");
        assert_eq!(relative_time(0, 120_000), "2m");
        assert_eq!(relative_time(0, 3 * 3_600_000), "3h");
        assert_eq!(relative_time(0, 2 * 86_400_000), "2d");
        assert_eq!(relative_time(0, 15 * 86_400_000), "2w");
    }
}
