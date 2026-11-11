//! Anchors and annotations (spec section 5.3).
//!
//! Persisted drafts, AI exchanges, and suggestions keep the existing `DraftAnchor` model. The
//! document maps each one onto its file and anchor line. An anchor whose line no longer exists,
//! or is no longer the same kind of line, is labeled outdated and shown at the file level; it is
//! never moved to a different line.

use super::keys::{FileRef, ItemKey};
use bcode_code_review_models::{
    DraftAnchor, DraftComment, ReviewAiExchange, ReviewAnchorKind, ReviewLineKind, ReviewLineSide,
    ReviewSuggestion, ReviewThreadKind, ReviewThreadSeverity,
};

/// Persisted review annotations to map onto the document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Annotations {
    /// Draft comments; comments sharing a `thread_id` form one thread.
    pub drafts: Vec<DraftComment>,
    /// AI exchanges.
    pub exchanges: Vec<ReviewAiExchange>,
    /// AI-suggested comments.
    pub suggestions: Vec<ReviewSuggestion>,
}

/// Inclusive line range of an anchor on both sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnchorLines {
    /// Old-side range, when the anchored lines include old-side lines.
    pub old: Option<(u32, u32)>,
    /// New-side range, when the anchored lines include new-side lines.
    pub new: Option<(u32, u32)>,
    /// Side of the last anchored line; the annotation is shown after it.
    pub end_side: ReviewLineSide,
    /// Kind of the last anchored line.
    pub end_kind: ReviewLineKind,
    /// Whether the anchor spans more than one line.
    pub is_range: bool,
}

impl AnchorLines {
    /// The last anchored line on `end_side`.
    #[must_use]
    pub fn end_line(&self) -> u32 {
        match self.end_side {
            ReviewLineSide::Old => self.old.map_or(0, |(_, end)| end),
            ReviewLineSide::New => self.new.map_or(0, |(_, end)| end),
        }
    }

    /// The first anchored line on `end_side`.
    #[must_use]
    pub fn start_line(&self) -> u32 {
        match self.end_side {
            ReviewLineSide::Old => self.old.map_or(0, |(start, _)| start),
            ReviewLineSide::New => self.new.map_or(0, |(start, _)| start),
        }
    }
}

/// What an anchor points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorTarget {
    /// The whole review.
    Review,
    /// A whole file.
    File,
    /// One line or a range of lines.
    Lines(AnchorLines),
}

/// A document-native anchor: what the composer comments on and what annotations are placed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocAnchor {
    /// File, for file and line anchors.
    pub file: Option<FileRef>,
    /// Target.
    pub target: AnchorTarget,
    /// Commit the file's source head resolved to when the anchor was made, when it is a
    /// commit (recorded on the persisted anchor; see [`DraftAnchor::head_oid`]).
    pub head_oid: Option<String>,
    /// Blob of the anchored side when a line anchor was made, when that side is a blob object
    /// (recorded on the persisted anchor; see [`DraftAnchor::blob_oid`]).
    pub blob_oid: Option<String>,
}

impl DocAnchor {
    /// A review-level anchor.
    #[must_use]
    pub const fn review() -> Self {
        Self {
            file: None,
            target: AnchorTarget::Review,
            head_oid: None,
            blob_oid: None,
        }
    }

    /// The persisted form of this anchor.
    ///
    /// `diff_row` is a V1 rendered-row index that V2 does not use; it is set to the last
    /// anchored line so the value stays meaningful for diagnostics.
    #[must_use]
    pub fn to_draft_anchor(&self) -> DraftAnchor {
        let (file_path, source_id) = self.file.as_ref().map_or_else(
            || (super::REVIEW_LEVEL_PATH.to_string(), None),
            |file| (file.path.clone(), Some(file.source_id.clone())),
        );
        let mut anchor = DraftAnchor {
            kind: ReviewAnchorKind::Review,
            file_path,
            diff_row: 0,
            start_diff_row: None,
            end_diff_row: None,
            old_start: None,
            old_end: None,
            new_start: None,
            new_end: None,
            old_line: None,
            new_line: None,
            line_kind: ReviewLineKind::Context,
            is_file_anchor: false,
            surface_id: None,
            source_id,
            head_oid: self.head_oid.clone(),
            blob_oid: self.blob_oid.clone(),
        };
        match &self.target {
            AnchorTarget::Review => {}
            AnchorTarget::File => anchor.kind = ReviewAnchorKind::File,
            AnchorTarget::Lines(lines) => {
                anchor.kind = ReviewAnchorKind::Range;
                anchor.diff_row = u64::from(lines.end_line());
                anchor.line_kind = lines.end_kind;
                if let Some((start, end)) = lines.old {
                    anchor.old_start = Some(start);
                    anchor.old_end = Some(end);
                    anchor.old_line = Some(end);
                }
                if let Some((start, end)) = lines.new {
                    anchor.new_start = Some(start);
                    anchor.new_end = Some(end);
                    anchor.new_line = Some(end);
                }
            }
        }
        anchor
    }
}

/// Target of a persisted anchor, before it is matched to a file.
#[must_use]
pub fn target_of(anchor: &DraftAnchor) -> AnchorTarget {
    match anchor.kind {
        ReviewAnchorKind::Review => AnchorTarget::Review,
        ReviewAnchorKind::File => AnchorTarget::File,
        ReviewAnchorKind::Range => {
            let range = |start: Option<u32>, end: Option<u32>, line: Option<u32>| {
                let start = start.or(line).or(end)?;
                let end = end.or(line).unwrap_or(start);
                Some((start.min(end), start.max(end)))
            };
            let old = range(anchor.old_start, anchor.old_end, anchor.old_line);
            let new = range(anchor.new_start, anchor.new_end, anchor.new_line);
            let end_side = match (anchor.line_kind, old, new) {
                (ReviewLineKind::Removed, Some(_), _) | (_, Some(_), None) => ReviewLineSide::Old,
                (_, _, Some(_)) => ReviewLineSide::New,
                (_, None, None) => return AnchorTarget::File,
            };
            let span = |range: Option<(u32, u32)>| range.is_some_and(|(s, e)| s != e);
            let is_range = span(old)
                || span(new)
                || (old.is_some() && new.is_some() && anchor.line_kind != ReviewLineKind::Context);
            AnchorTarget::Lines(AnchorLines {
                old,
                new,
                end_side,
                end_kind: anchor.line_kind,
                is_range,
            })
        }
    }
}

/// Content of one annotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnnotationBody {
    /// A draft thread.
    Thread {
        /// Comment ids in creation order.
        comment_ids: Vec<String>,
        /// Whether every comment is resolved.
        resolved: bool,
        /// Thread kind of the first comment.
        kind: ReviewThreadKind,
        /// Thread severity of the first comment.
        severity: ReviewThreadSeverity,
    },
    /// An AI exchange, or orphan suggestions.
    Agent {
        /// Exchange id.
        exchange_id: Option<String>,
    },
}

/// One annotation mapped onto the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    /// Item key (thread or agent thread).
    pub key: ItemKey,
    /// Persisted anchor.
    pub anchor: DraftAnchor,
    /// Target derived from the anchor.
    pub target: AnchorTarget,
    /// Creation time used for ordering annotations at the same anchor.
    pub created_at_ms: u64,
    /// Body.
    pub body: AnnotationBody,
    /// Suggestions shown with this annotation.
    pub suggestion_ids: Vec<String>,
    /// Session id linking drafts, exchanges, and suggestions.
    pub session_id: Option<String>,
}

/// Group persisted annotations into threads and agent threads, attaching suggestions to the
/// exchange from the same session, else to an annotation with the same anchor, else to their
/// own agent item. The result is ordered by creation time, then key.
#[must_use]
pub fn collect(annotations: &Annotations) -> Vec<Annotation> {
    let mut out: Vec<Annotation> = Vec::new();
    let mut threads: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    let mut drafts: Vec<&DraftComment> = annotations.drafts.iter().collect();
    drafts.sort_by(|a, b| (a.created_at_ms, &a.comment_id).cmp(&(b.created_at_ms, &b.comment_id)));
    for draft in drafts {
        if let Some(index) = threads.get(draft.thread_id.as_str()) {
            let annotation = &mut out[*index];
            if let AnnotationBody::Thread {
                comment_ids,
                resolved,
                ..
            } = &mut annotation.body
            {
                comment_ids.push(draft.comment_id.clone());
                *resolved &= draft.resolved_at_ms.is_some();
            }
            if annotation.session_id.is_none() {
                annotation.session_id.clone_from(&draft.session_id);
            }
            continue;
        }
        threads.insert(&draft.thread_id, out.len());
        out.push(Annotation {
            key: ItemKey::Thread {
                thread_id: draft.thread_id.clone(),
            },
            anchor: draft.anchor.clone(),
            target: target_of(&draft.anchor),
            created_at_ms: draft.created_at_ms,
            body: AnnotationBody::Thread {
                comment_ids: vec![draft.comment_id.clone()],
                resolved: draft.resolved_at_ms.is_some(),
                kind: draft.thread_kind,
                severity: draft.severity,
            },
            suggestion_ids: Vec::new(),
            session_id: draft.session_id.clone(),
        });
    }
    for exchange in &annotations.exchanges {
        out.push(Annotation {
            key: ItemKey::AgentThread {
                id: exchange.exchange_id.clone(),
            },
            anchor: exchange.anchor.clone(),
            target: target_of(&exchange.anchor),
            created_at_ms: exchange.created_at_ms,
            body: AnnotationBody::Agent {
                exchange_id: Some(exchange.exchange_id.clone()),
            },
            suggestion_ids: Vec::new(),
            session_id: exchange.session_id.clone(),
        });
    }
    let mut suggestions: Vec<&ReviewSuggestion> = annotations.suggestions.iter().collect();
    suggestions.sort_by(|a, b| {
        (a.created_at_ms, &a.suggestion_id).cmp(&(b.created_at_ms, &b.suggestion_id))
    });
    for suggestion in suggestions {
        let by_session = suggestion.session_id.as_ref().and_then(|session| {
            out.iter().position(|annotation| {
                matches!(annotation.body, AnnotationBody::Agent { .. })
                    && annotation.session_id.as_ref() == Some(session)
            })
        });
        let host = by_session.or_else(|| {
            out.iter()
                .position(|annotation| annotation.anchor == suggestion.anchor)
        });
        if let Some(index) = host {
            out[index]
                .suggestion_ids
                .push(suggestion.suggestion_id.clone());
            continue;
        }
        out.push(Annotation {
            key: ItemKey::AgentThread {
                id: suggestion.suggestion_id.clone(),
            },
            anchor: suggestion.anchor.clone(),
            target: target_of(&suggestion.anchor),
            created_at_ms: suggestion.created_at_ms,
            body: AnnotationBody::Agent { exchange_id: None },
            suggestion_ids: vec![suggestion.suggestion_id.clone()],
            session_id: suggestion.session_id.clone(),
        });
    }
    out.sort_by(|a, b| (a.created_at_ms, &a.key).cmp(&(b.created_at_ms, &b.key)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor(kind: ReviewLineKind, old: Option<u32>, new: Option<u32>) -> DraftAnchor {
        DraftAnchor {
            kind: ReviewAnchorKind::Range,
            file_path: "a.rs".into(),
            diff_row: 0,
            start_diff_row: None,
            end_diff_row: None,
            old_start: None,
            old_end: None,
            new_start: None,
            new_end: None,
            old_line: old,
            new_line: new,
            line_kind: kind,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
            head_oid: None,
            blob_oid: None,
        }
    }

    #[test]
    fn single_line_targets_pick_the_line_side() {
        let AnchorTarget::Lines(removed) =
            target_of(&anchor(ReviewLineKind::Removed, Some(4), None))
        else {
            panic!("lines")
        };
        assert_eq!(
            (removed.end_side, removed.end_line()),
            (ReviewLineSide::Old, 4)
        );
        assert!(!removed.is_range);
        let AnchorTarget::Lines(context) =
            target_of(&anchor(ReviewLineKind::Context, Some(4), Some(6)))
        else {
            panic!("lines")
        };
        assert_eq!(
            (context.end_side, context.end_line()),
            (ReviewLineSide::New, 6)
        );
        assert!(!context.is_range);
        assert_eq!(
            target_of(&anchor(ReviewLineKind::Added, None, None)),
            AnchorTarget::File
        );
    }

    #[test]
    fn doc_anchor_round_trips_through_draft_anchor() {
        let lines = AnchorLines {
            old: Some((3, 4)),
            new: Some((3, 6)),
            end_side: ReviewLineSide::New,
            end_kind: ReviewLineKind::Added,
            is_range: true,
        };
        let doc = DocAnchor {
            file: Some(std::sync::Arc::new(super::super::FileKey::new(
                "s", None, "a.rs",
            ))),
            target: AnchorTarget::Lines(lines),
            head_oid: Some("abc".to_owned()),
            blob_oid: Some("def".to_owned()),
        };
        let draft = doc.to_draft_anchor();
        assert_eq!(draft.source_id.as_deref(), Some("s"));
        assert_eq!(draft.head_oid.as_deref(), Some("abc"));
        assert_eq!(draft.blob_oid.as_deref(), Some("def"));
        assert_eq!(target_of(&draft), AnchorTarget::Lines(lines));
    }
}
