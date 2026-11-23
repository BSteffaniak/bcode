//! Stable identities and presentation preferences for ordered review series.

use serde::{Deserialize, Serialize};

/// Which groups of a series to display, independently of the comparison baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSeriesView {
    /// Only the selected member.
    OneMember,
    /// Each member as its own diff.
    AllMembers,
    /// One aggregate diff.
    Combined,
}

/// Saved choice for a series. No entry means the source's initial view applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSeriesPresentation {
    /// The chosen grouping.
    pub view: ReviewSeriesView,
    /// Stable member identity to restore when returning to one-member view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_member: Option<String>,
}

/// Identity of one reviewable group, independent of position or label.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewGroupKey {
    /// An ordinary source, addressed by its existing id.
    Standalone {
        /// Canonical workspace source id.
        source_id: String,
    },
    /// One member of a series.
    Member {
        /// Canonical workspace source id.
        source_id: String,
        /// Stable member identity (the full object id for a commit).
        member: String,
    },
    /// The exact aggregate for a series.
    Combined {
        /// Canonical workspace source id.
        source_id: String,
    },
}

impl ReviewGroupKey {
    /// Identity of a resolved diff. `series` is whether its source currently resolves as a
    /// multi-member series (the session's series state, not the number of requested revisions,
    /// since aliases can pin to one commit). Outside a series every diff is standalone; inside
    /// one, a diff with a part position is the member named by its head commit and a diff
    /// without one is the combined aggregate.
    #[must_use]
    pub fn for_resolved(source: &crate::ResolvedSource, series: bool) -> Self {
        let source_id = source.source_id.clone();
        if !series {
            return Self::Standalone { source_id };
        }
        match (&source.part, source.head.commit_oid()) {
            (Some(_), Some(oid)) => Self::Member {
                source_id,
                member: oid.to_owned(),
            },
            _ => Self::Combined { source_id },
        }
    }

    /// Canonical workspace source id this group belongs to.
    #[must_use]
    pub fn source_id(&self) -> &str {
        match self {
            Self::Standalone { source_id }
            | Self::Member { source_id, .. }
            | Self::Combined { source_id } => source_id,
        }
    }

    /// Stable selector for commands and keyed state. Standalone sources retain their id;
    /// `series:member:` and `series:combined:` are reserved selector namespaces. The member
    /// form length-prefixes the UTF-8 source id so delimiters in either id are unambiguous.
    #[must_use]
    pub fn selector(&self) -> String {
        match self {
            Self::Standalone { source_id } => source_id.clone(),
            Self::Member { source_id, member } => {
                format!("series:member:{}:{source_id}:{member}", source_id.len())
            }
            Self::Combined { source_id } => format!("series:combined:{source_id}"),
        }
    }
}
