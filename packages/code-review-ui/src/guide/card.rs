//! The guide as the assistant's conversation shows it: a live card in place of a turn's guide
//! tool lines.
//!
//! The card is display data read from the review's [`ActiveGuide`] (the object the guide panel
//! shows), never from tool output, so it fills in as the guide's sections are written and
//! agrees with the panel. Its rows are bounded ([`MAX_CARD_SECTIONS`]).

use super::ActiveGuide;
use bcode_code_review_models::{GuideStatus, SectionState};

/// Sections a card lists at most; the rest are counted.
pub const MAX_CARD_SECTIONS: usize = 12;

/// Where a guide's writing stands, as the card says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardStatus {
    /// Planning or writing sections.
    Writing,
    /// Combining or checking (fan-out and verification).
    Finishing,
    /// Written.
    Complete,
    /// Writing failed; written sections stay.
    Failed,
    /// Writing was stopped; written sections stay.
    Stopped,
    /// Another version than the one on screen (the card of a turn that wrote it): its
    /// sections are not listed.
    OtherVersion,
}

/// How a listed section stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardSectionState {
    /// Planned, not written yet.
    Planned,
    /// Written.
    Written,
    /// Written and reviewed.
    Reviewed,
    /// Provisional (an expansion or an answer not promoted yet).
    Provisional,
}

/// One section the card lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardSection {
    /// Index into the guide's sections in reading order (what a click selects).
    pub entry: usize,
    /// Outline number (`3.1`).
    pub number: String,
    /// Title.
    pub title: String,
    /// Nesting depth.
    pub depth: u16,
    /// How it stands.
    pub state: CardSectionState,
    /// Whether the reviewer is on it.
    pub current: bool,
}

/// The guide card: one guide version as the conversation shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuideCard {
    /// The guide.
    pub guide_id: String,
    /// Its title.
    pub title: String,
    /// The version shown.
    pub version: u32,
    /// The session writing (or that wrote) the version, when recorded.
    pub session: Option<String>,
    /// Where writing stands.
    pub status: CardStatus,
    /// Sections written.
    pub written: usize,
    /// Sections in all.
    pub total: usize,
    /// Sections listed, in reading order (at most [`MAX_CARD_SECTIONS`]).
    pub sections: Vec<CardSection>,
    /// Sections left out of the list.
    pub more: usize,
    /// Sections whose code changed since they were written.
    pub stale: usize,
    /// Changes whenever anything the card shows changes.
    pub revision: u64,
}

impl GuideCard {
    /// The card of `active` as read at state `revision`. The sections listed are a window of
    /// [`MAX_CARD_SECTIONS`] around the first section not written yet (where writing is), else
    /// around the section the reviewer is on.
    #[must_use]
    pub fn of(active: &ActiveGuide, revision: u64) -> Self {
        let version = &active.version;
        let status = match &version.status {
            GuideStatus::Planning | GuideStatus::Generating => CardStatus::Writing,
            GuideStatus::Synthesizing | GuideStatus::Verifying => CardStatus::Finishing,
            GuideStatus::Complete | GuideStatus::Superseded => CardStatus::Complete,
            GuideStatus::Failed { .. } => CardStatus::Failed,
            GuideStatus::Cancelled => CardStatus::Stopped,
        };
        let total = active.entries.len();
        let focus = if status == CardStatus::Writing {
            active
                .entries
                .iter()
                .position(|entry| entry.state == SectionState::Planned)
                .unwrap_or(active.selected)
        } else {
            active.selected
        };
        let first = focus
            .saturating_sub(MAX_CARD_SECTIONS / 2)
            .min(total.saturating_sub(MAX_CARD_SECTIONS));
        let sections = active
            .entries
            .iter()
            .enumerate()
            .skip(first)
            .take(MAX_CARD_SECTIONS)
            .map(|(index, entry)| CardSection {
                entry: index,
                number: entry.number.clone(),
                title: entry.title.clone(),
                depth: entry.depth,
                state: match entry.state {
                    SectionState::Planned => CardSectionState::Planned,
                    SectionState::Provisional { .. } => CardSectionState::Provisional,
                    SectionState::Written if active.reviewed(&entry.id) => {
                        CardSectionState::Reviewed
                    }
                    SectionState::Written => CardSectionState::Written,
                },
                current: index == active.selected,
            })
            .collect::<Vec<_>>();
        let more = total - sections.len();
        Self {
            guide_id: active.guide.guide_id.clone(),
            title: active.guide.title.clone(),
            version: version.version,
            session: version.origin.session_id.clone(),
            status,
            written: active.written(),
            total,
            sections,
            more,
            stale: active.stale.len(),
            revision,
        }
    }

    /// The card of version `version` of this card's guide: this card when it shows that
    /// version, else a card naming that version without listing sections (the screen shows
    /// another version), so a turn's card always names the version the turn wrote.
    #[must_use]
    pub fn for_version(&self, version: u32) -> std::borrow::Cow<'_, Self> {
        if version == self.version {
            return std::borrow::Cow::Borrowed(self);
        }
        std::borrow::Cow::Owned(Self {
            version,
            status: CardStatus::OtherVersion,
            session: None,
            written: 0,
            total: 0,
            sections: Vec::new(),
            more: 0,
            stale: 0,
            ..self.clone()
        })
    }

    /// Whether writing is under way.
    #[must_use]
    pub const fn busy(&self) -> bool {
        matches!(self.status, CardStatus::Writing | CardStatus::Finishing)
    }
}
