//! A review's guides as the screen shows them.
//!
//! The version shown is read whole from the store (bounded by the guide document limits) and
//! flattened once into [`SectionEntry`] rows in reading order, with outline numbers (`3.1`) and
//! citation badge numbers (`[1]`, `[2]`, in first-use order). Everything the panel, the diff
//! markers, and navigation need is answered from that snapshot.

use bcode_code_review::document::ItemKey;
use bcode_code_review::guide::freshness::GuideFreshness;
use bcode_code_review_models::{
    Citation, CitationRelation, Freshness, GuideRecord, GuideSectionMark, GuideStatus, GuideThread,
    GuideThreadStatus, GuideVersion, GuideVersionRecord, GuideVersionSummary, ReviewGuide, ReviewLineInterval,
    ReviewLineSide, RevisionRef, Section, SectionKind, SectionState, Verification,
};
use std::collections::{BTreeMap, BTreeSet};

/// Guide jumps remembered for jumping back.
pub const MAX_JUMPS: usize = 64;

/// Where loading the review's guides stands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum GuideLoad {
    /// Not asked for yet.
    #[default]
    Idle,
    /// Reading.
    Loading,
    /// Read (the review may have no guide).
    Ready,
    /// Reading failed.
    Failed(String),
}

/// What one load read.
#[derive(Debug, Clone)]
pub struct GuideLoaded {
    /// The review's guides, most recently changed first.
    pub guides: Vec<GuideRecord>,
    /// The guide shown and its version.
    pub active: Option<(GuideRecord, GuideVersionRecord)>,
    /// The guide's reviewed sections.
    pub marks: Vec<GuideSectionMark>,
    /// Whether its citations still match the review, when that could be told.
    pub freshness: Option<GuideFreshness>,
    /// The ask threads about the version shown, oldest first.
    pub threads: Vec<GuideThread>,
    /// The versions of the guide shown, oldest first.
    pub versions: Vec<GuideVersionSummary>,
}

/// Keys the guide's banners name (empty when unbound).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuideKeys {
    /// Refresh the current section.
    pub refresh: String,
    /// Refresh every stale section.
    pub refresh_stale: String,
    /// The versions picker.
    pub versions: String,
}

/// One section of the version shown, in reading order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionEntry {
    /// Section id.
    pub id: String,
    /// Outline number (`3.1`).
    pub number: String,
    /// Nesting depth (top-level sections are 0).
    pub depth: u16,
    /// Title.
    pub title: String,
    /// What it is.
    pub kind: SectionKind,
    /// Whether it is written.
    pub state: SectionState,
    /// Citation ids it uses (its own and its findings'), first use first.
    pub citations: Vec<String>,
    /// Whether it has child sections.
    pub has_children: bool,
}

/// A cited range the screen moves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiteTarget {
    /// The citation.
    pub citation: String,
    /// Source cited, when one is.
    pub source_id: Option<String>,
    /// Repository-relative path.
    pub path: String,
    /// Side.
    pub side: ReviewLineSide,
    /// Lines in the review as it is now: the cited lines, or where they moved to.
    pub lines: ReviewLineInterval,
    /// How the range relates to the review.
    pub relation: CitationRelation,
    /// The revision it was cited at.
    pub revision: RevisionRef,
    /// The lines as cited, at [`Self::revision`].
    pub cited: ReviewLineInterval,
}

impl CiteTarget {
    /// Whether the cited lines have moved since the citation was written (so [`Self::lines`]
    /// number the source's current revision, not [`Self::revision`]).
    #[must_use]
    pub fn moved(&self) -> bool {
        self.lines != self.cited
    }
}

impl CiteTarget {
    /// The target of `citation`, at the lines it has moved to when its freshness says so.
    #[must_use]
    pub fn of(citation: &Citation, freshness: Option<&Freshness>) -> Self {
        let lines = match freshness {
            Some(Freshness::Moved { lines }) => *lines,
            _ => citation.lines,
        };
        Self {
            citation: citation.id.clone(),
            source_id: citation.source_id.clone(),
            path: citation.path.clone(),
            side: citation.side,
            lines,
            relation: citation.relation,
            revision: citation.revision.clone(),
            cited: citation.lines,
        }
    }
}

/// Counts shown in the panel's title.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GuideCounts {
    /// Sections reviewed.
    pub reviewed: usize,
    /// Sections.
    pub sections: usize,
    /// Citations verified.
    pub verified: usize,
    /// Citations.
    pub citations: usize,
    /// Sections whose code changed since they were written.
    pub stale: usize,
}

/// The guide shown: one version, flattened.
#[derive(Debug, Clone)]
pub struct ActiveGuide {
    /// The guide.
    pub guide: ReviewGuide,
    /// The version shown.
    pub version: GuideVersion,
    /// Reviewed sections, by section id.
    pub marks: BTreeMap<String, GuideSectionMark>,
    /// Citation freshness, by citation id (empty when it could not be told).
    pub freshness: BTreeMap<String, Freshness>,
    /// Sections whose code changed since they were written.
    pub stale: BTreeSet<String>,
    /// Sections in reading order.
    pub entries: Vec<SectionEntry>,
    /// Badge numbers of citations, by id, in first-use order.
    pub numbers: BTreeMap<String, u16>,
    /// The section the reviewer is on (an index into `entries`).
    pub selected: usize,
    /// The citation of the selected section last shown, by position in its citations.
    pub cite: Option<usize>,
    /// The ask threads about the version, oldest first.
    pub threads: Vec<GuideThread>,
    /// In-diff files the guide cites that the review shows viewed (source id, path).
    pub viewed: BTreeSet<(String, String)>,
    /// Keys the banners name.
    pub keys: GuideKeys,
}

impl ActiveGuide {
    /// The guide shown from what a load read.
    #[must_use]
    pub fn new(
        guide: ReviewGuide,
        version: GuideVersion,
        marks: Vec<GuideSectionMark>,
        freshness: Option<GuideFreshness>,
    ) -> Self {
        let mut entries = Vec::new();
        flatten(&version.outline, "", 0, &mut entries);
        let mut numbers = BTreeMap::new();
        let ids = entries
            .iter()
            .flat_map(|entry| entry.citations.iter())
            .chain(version.citations.keys());
        for id in ids {
            if version.citations.contains_key(id) && !numbers.contains_key(id) {
                let next = u16::try_from(numbers.len() + 1).unwrap_or(u16::MAX);
                numbers.insert(id.clone(), next);
            }
        }
        let (freshness, stale) = freshness.map_or_else(Default::default, |freshness| {
            (
                freshness.citations,
                freshness.stale_sections.into_iter().collect(),
            )
        });
        Self {
            guide,
            version,
            marks: marks
                .into_iter()
                .map(|mark| (mark.section_id.clone(), mark))
                .collect(),
            freshness,
            stale,
            entries,
            numbers,
            selected: 0,
            cite: None,
            threads: Vec::new(),
            viewed: BTreeSet::new(),
            keys: GuideKeys::default(),
        }
    }

    /// The open ask threads about section `id`, oldest first.
    pub fn open_threads(&self, id: &str) -> impl Iterator<Item = &GuideThread> {
        self.threads.iter().filter(move |thread| {
            thread.section_id == id && thread.status == GuideThreadStatus::Open
        })
    }

    /// Whether section `id` is marked reviewed with its citations as they were then.
    #[must_use]
    pub fn marked(&self, id: &str) -> bool {
        let Some(mark) = self.marks.get(id) else {
            return false;
        };
        self.section(id).is_some_and(|section| {
            bcode_code_review::guide::document::section_fingerprint(
                section,
                &self.version.citations,
            ) == mark.fingerprint
        })
    }

    /// The selected section.
    #[must_use]
    pub fn current(&self) -> Option<&SectionEntry> {
        self.entries.get(self.selected)
    }

    /// The section with `id`.
    #[must_use]
    pub fn section(&self, id: &str) -> Option<&Section> {
        self.version.section(id)
    }

    /// The citation with `id`.
    #[must_use]
    pub fn citation(&self, id: &str) -> Option<&Citation> {
        self.version.citations.get(id)
    }

    /// The badge number of citation `id`.
    #[must_use]
    pub fn number(&self, id: &str) -> Option<u16> {
        self.numbers.get(id).copied()
    }

    /// The citation with badge `number`.
    #[must_use]
    pub fn citation_numbered(&self, number: u16) -> Option<&str> {
        self.numbers
            .iter()
            .find(|(_, at)| **at == number)
            .map(|(id, _)| id.as_str())
    }

    /// Whether this is not the guide's current version.
    #[must_use]
    pub const fn is_old_version(&self) -> bool {
        self.version.version < self.guide.current_version
    }

    /// Whether section `id` is reviewed: marked with its citations as they were then, or
    /// every in-diff file it cites is viewed.
    #[must_use]
    pub fn reviewed(&self, id: &str) -> bool {
        if self.marked(id) {
            return true;
        }
        let files = bcode_code_review::guide::coverage::cited_files(&self.version, id);
        !files.is_empty() && files.is_subset(&self.viewed)
    }

    /// Where citation `id` is, at the lines it moved to when it moved.
    #[must_use]
    pub fn target(&self, id: &str) -> Option<CiteTarget> {
        self.citation(id)
            .map(|citation| CiteTarget::of(citation, self.freshness.get(id)))
    }

    /// The freshness of citation `id`, when told.
    #[must_use]
    pub fn freshness(&self, id: &str) -> Option<&Freshness> {
        self.freshness.get(id)
    }

    /// The counts of the panel's title.
    #[must_use]
    pub fn counts(&self) -> GuideCounts {
        GuideCounts {
            reviewed: self
                .entries
                .iter()
                .filter(|entry| self.reviewed(&entry.id))
                .count(),
            sections: self.entries.len(),
            verified: self
                .version
                .citations
                .values()
                .filter(|citation| citation.verification.is_verified())
                .count(),
            citations: self.version.citations.len(),
            stale: self.stale.len(),
        }
    }

    /// Sections written so far.
    #[must_use]
    pub fn written(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.state != SectionState::Planned)
            .count()
    }

    /// The section to show for citation `id`: the selected section when it cites it, else the
    /// first section in reading order that does.
    #[must_use]
    pub fn entry_citing(&self, id: &str) -> Option<usize> {
        if self
            .current()
            .is_some_and(|entry| entry.citations.iter().any(|cited| cited == id))
        {
            return Some(self.selected);
        }
        self.entries
            .iter()
            .position(|entry| entry.citations.iter().any(|cited| cited == id))
    }

    /// Citations covering `line` on `side` of `path` (in source `source_id`), first badge first.
    #[must_use]
    pub fn citations_at(
        &self,
        source_id: &str,
        path: &str,
        side: ReviewLineSide,
        line: u32,
    ) -> Vec<&str> {
        let mut found: Vec<(u16, &str)> = self
            .version
            .citations
            .values()
            .filter(|citation| {
                citation.path == path
                    && citation.side == side
                    && citation
                        .source_id
                        .as_deref()
                        .is_none_or(|cited| cited == source_id)
            })
            .filter(|citation| {
                let lines = self
                    .target(&citation.id)
                    .map_or(citation.lines, |t| t.lines);
                (lines.start..=lines.end).contains(&line)
            })
            .map(|citation| {
                (
                    self.number(&citation.id).unwrap_or(u16::MAX),
                    citation.id.as_str(),
                )
            })
            .collect();
        found.sort_unstable();
        found.into_iter().map(|(_, id)| id).collect()
    }

    /// Select the section at `index`; the citation shown starts over. Returns whether it moved.
    pub const fn select(&mut self, index: usize) -> bool {
        if index >= self.entries.len() {
            return false;
        }
        let moved = index != self.selected;
        self.selected = index;
        if moved {
            self.cite = None;
        }
        moved
    }

    /// One line saying where writing the version stands, when it is not simply complete.
    #[must_use]
    pub fn banner(&self) -> Option<Banner> {
        let written = self.written();
        let total = self.entries.len();
        let status = match &self.version.status {
            GuideStatus::Planning => Some(Banner::busy("planning the outline…".to_owned())),
            GuideStatus::Generating => Some(Banner::busy(format!(
                "writing… {written} of {total} sections"
            ))),
            GuideStatus::Synthesizing => Some(Banner::busy("combining areas…".to_owned())),
            GuideStatus::Verifying => Some(Banner::busy("checking claims…".to_owned())),
            GuideStatus::Failed { message } => Some(Banner::problem(format!(
                "writing failed: {message} · written sections stay"
            ))),
            GuideStatus::Cancelled => Some(Banner::note(format!(
                "stopped · {written} of {total} sections written"
            ))),
            GuideStatus::Complete | GuideStatus::Superseded => None,
        };
        if status.is_some() {
            return status;
        }
        let hint = |key: &str, what: &str| {
            if key.is_empty() {
                String::new()
            } else {
                format!(" · {key} {what}")
            }
        };
        if self.is_old_version() {
            return Some(Banner::note(format!(
                "v{} · pinned to {} · read-only · current is v{}{}",
                self.version.version,
                self.pinned_label(),
                self.guide.current_version,
                hint(&self.keys.versions, "versions"),
            )));
        }
        (!self.stale.is_empty()).then(|| {
            let count = self.stale.len();
            Banner::problem(format!(
                "{count} section{} stale since {}{}{}{}",
                if count == 1 { "" } else { "s" },
                self.pinned_label(),
                hint(&self.keys.refresh, "refresh"),
                hint(&self.keys.refresh_stale, "refresh stale"),
                hint(&self.keys.versions, "versions"),
            ))
        })
    }

    /// The code the version is pinned to, briefly: its first source's label and head
    /// (`#102 1a2b3c4`).
    #[must_use]
    pub fn pinned_label(&self) -> String {
        self.version.heads.values().next().map_or_else(
            || format!("v{}", self.version.version),
            |head| {
                let revision = bcode_code_review::guide::lifecycle::short_revision(&head.head);
                if head.label.is_empty() {
                    revision
                } else {
                    format!("{} {revision}", head.label)
                }
            },
        )
    }

    /// The stale sections in reading order.
    #[must_use]
    pub fn stale_in_order(&self) -> Vec<&SectionEntry> {
        self.entries
            .iter()
            .filter(|entry| self.stale.contains(&entry.id))
            .collect()
    }

    /// Whether a citation is verified, unverified, or has a problem (or went stale).
    #[must_use]
    pub fn citation_look(&self, id: &str) -> CitationLook {
        if self.freshness(id).is_some_and(Freshness::is_stale) {
            return CitationLook::Problem;
        }
        match self.citation(id).map(|citation| &citation.verification) {
            Some(Verification::Verified { .. }) => CitationLook::Verified,
            Some(Verification::Problem { .. }) => CitationLook::Problem,
            Some(Verification::Unverified) | None => CitationLook::Unverified,
        }
    }
}

/// How a citation shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CitationLook {
    /// Its quote checked out.
    Verified,
    /// Not checked.
    Unverified,
    /// A problem, or the code changed since.
    Problem,
}

/// What a banner line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerKind {
    /// Writing is under way.
    Busy,
    /// Something to know.
    Note,
    /// Something went wrong or changed.
    Problem,
}

/// A line over the guide saying where it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Banner {
    /// What it is.
    pub kind: BannerKind,
    /// Text.
    pub text: String,
}

impl Banner {
    const fn busy(text: String) -> Self {
        Self {
            kind: BannerKind::Busy,
            text,
        }
    }

    const fn note(text: String) -> Self {
        Self {
            kind: BannerKind::Note,
            text,
        }
    }

    const fn problem(text: String) -> Self {
        Self {
            kind: BannerKind::Problem,
            text,
        }
    }
}

/// Flatten `sections` (numbered under `prefix`) into `entries`, parents before children.
fn flatten(sections: &[Section], prefix: &str, depth: u16, entries: &mut Vec<SectionEntry>) {
    for (index, section) in sections.iter().enumerate() {
        let number = if prefix.is_empty() {
            (index + 1).to_string()
        } else {
            format!("{prefix}.{}", index + 1)
        };
        entries.push(SectionEntry {
            id: section.id.clone(),
            number: number.clone(),
            depth,
            title: section.title.clone(),
            kind: section.kind.clone(),
            state: section.state.clone(),
            citations: section.cited_ids().into_iter().map(str::to_owned).collect(),
            has_children: !section.children.is_empty(),
        });
        flatten(&section.children, &number, depth.saturating_add(1), entries);
    }
}

/// The review's guides on screen.
#[derive(Debug, Default)]
pub struct GuideState {
    /// Where loading stands.
    pub load: GuideLoad,
    /// The review's guides, most recently changed first.
    pub guides: Vec<GuideRecord>,
    /// The guide shown.
    pub active: Option<ActiveGuide>,
    /// The versions of the guide shown, oldest first.
    pub versions: Vec<GuideVersionSummary>,
    /// Keys the banners name.
    pub keys: GuideKeys,
    /// Changes whenever anything shown changes.
    pub revision: u64,
    /// Whether the reviewer is using the guide (the panel was opened or a guide key used): only
    /// then does the diff show its markers.
    pub engaged: bool,
    /// Cursor positions guide jumps left from, newest last.
    pub jumps: Vec<ItemKey>,
    /// A citation to move to once its file has loaded.
    pub pending: Option<CiteTarget>,
    /// A load was asked for and has not arrived.
    pub loading: bool,
}

impl GuideState {
    /// Apply what a load read, keeping the section the reviewer was on when it is still there.
    pub fn apply(&mut self, loaded: GuideLoaded) {
        let keep = self
            .active
            .as_ref()
            .and_then(|active| active.current().map(|entry| entry.id.clone()));
        self.guides = loaded.guides;
        self.versions = loaded.versions;
        let keys = self.keys.clone();
        self.active = loaded.active.map(|(guide, version)| {
            let viewed = self
                .active
                .as_ref()
                .map(|active| active.viewed.clone())
                .unwrap_or_default();
            let mut active =
                ActiveGuide::new(guide.guide, version.version, loaded.marks, loaded.freshness);
            active.threads = loaded.threads;
            active.viewed = viewed;
            active.keys = keys;
            if let Some(index) =
                keep.and_then(|id| active.entries.iter().position(|entry| entry.id == id))
            {
                active.selected = index;
            }
            active
        });
        self.load = GuideLoad::Ready;
        self.loading = false;
        self.bump();
    }

    /// Note a failed load.
    pub fn fail(&mut self, message: String) {
        self.load = GuideLoad::Failed(message);
        self.loading = false;
        self.bump();
    }

    /// Note that something shown changed.
    pub const fn bump(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// Remember `from` to jump back to (bounded; repeats of the newest are not kept twice).
    pub fn push_jump(&mut self, from: ItemKey) {
        if self.jumps.last() == Some(&from) {
            return;
        }
        if self.jumps.len() >= MAX_JUMPS {
            self.jumps.remove(0);
        }
        self.jumps.push(from);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_code_review_models::{GuideOrigin, GuideScope, REVIEW_GUIDE_SCHEMA_VERSION};

    fn section(id: &str, citations: &[&str], children: Vec<Section>) -> Section {
        Section {
            id: id.to_owned(),
            kind: SectionKind::Step,
            title: format!("title {id}"),
            body: String::new(),
            findings: Vec::new(),
            citations: citations.iter().map(|id| (*id).to_owned()).collect(),
            children,
            state: SectionState::Written,
            refreshed_from: None,
            checks: Vec::new(),
        }
    }

    fn citation(id: &str, start: u32, end: u32) -> Citation {
        Citation {
            id: id.to_owned(),
            source_id: Some("src".to_owned()),
            path: "a.rs".to_owned(),
            side: ReviewLineSide::New,
            lines: ReviewLineInterval { start, end },
            revision: RevisionRef::WorkingTree,
            blob_oid: None,
            quote_hash: String::new(),
            excerpt: String::new(),
            relation: CitationRelation::Changed,
            verification: Verification::Unverified,
        }
    }

    fn active(outline: Vec<Section>, citations: Vec<Citation>) -> ActiveGuide {
        let guide = ReviewGuide {
            guide_id: "g".to_owned(),
            review_key: "k".to_owned(),
            scope: GuideScope::Review,
            parent_guide_id: None,
            title: "t".to_owned(),
            current_version: 2,
            created_at_ms: 0,
            updated_at_ms: 0,
            archived_at_ms: None,
        };
        let version = GuideVersion {
            guide_id: "g".to_owned(),
            version: 1,
            schema_version: REVIEW_GUIDE_SCHEMA_VERSION,
            heads: BTreeMap::new(),
            based_on: None,
            origin: GuideOrigin::default(),
            status: GuideStatus::Complete,
            generation: None,
            outline,
            citations: citations
                .into_iter()
                .map(|citation| (citation.id.clone(), citation))
                .collect(),
            retired_sections: Vec::new(),
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        ActiveGuide::new(guide, version, Vec::new(), None)
    }

    #[test]
    fn sections_are_numbered_in_reading_order_and_badges_by_first_use() {
        let guide = active(
            vec![
                section("s1", &["c9"], Vec::new()),
                section(
                    "s3",
                    &[],
                    vec![
                        section("s3.1", &["c2", "c9"], Vec::new()),
                        section("s3.2", &["c1"], Vec::new()),
                    ],
                ),
            ],
            vec![
                citation("c1", 1, 2),
                citation("c2", 5, 9),
                citation("c9", 3, 3),
            ],
        );
        let numbers: Vec<&str> = guide
            .entries
            .iter()
            .map(|entry| entry.number.as_str())
            .collect();
        assert_eq!(numbers, ["1", "2", "2.1", "2.2"]);
        assert_eq!(guide.number("c9"), Some(1));
        assert_eq!(guide.number("c2"), Some(2));
        assert_eq!(guide.number("c1"), Some(3));
        assert_eq!(guide.citation_numbered(2), Some("c2"));
        assert_eq!(
            guide.citations_at("src", "a.rs", ReviewLineSide::New, 5),
            ["c2"]
        );
        assert_eq!(
            guide.citations_at("other", "a.rs", ReviewLineSide::New, 5),
            Vec::<&str>::new()
        );
        assert_eq!(guide.entry_citing("c9"), Some(0));
        assert!(guide.is_old_version());
        assert!(
            guide
                .banner()
                .is_some_and(|banner| banner.kind == BannerKind::Note)
        );
    }

    #[test]
    fn banners_say_where_writing_stands() {
        let mut guide = active(
            vec![section("s1", &[], Vec::new()), {
                let mut planned = section("s2", &[], Vec::new());
                planned.state = SectionState::Planned;
                planned
            }],
            Vec::new(),
        );
        guide.guide.current_version = 1;
        assert_eq!(guide.banner(), None, "a complete current version has none");
        guide.version.status = GuideStatus::Generating;
        let banner = guide.banner().unwrap();
        assert_eq!(banner.kind, BannerKind::Busy);
        assert!(banner.text.contains("1 of 2"), "{banner:?}");
        guide.version.status = GuideStatus::Failed {
            message: "provider down".to_owned(),
        };
        let banner = guide.banner().unwrap();
        assert_eq!(banner.kind, BannerKind::Problem);
        assert!(banner.text.contains("provider down"), "{banner:?}");
        guide.version.status = GuideStatus::Complete;
        guide.stale.insert("s1".to_owned());
        assert!(
            guide
                .banner()
                .unwrap()
                .text
                .contains("1 section stale since v1")
        );
        guide.keys = GuideKeys {
            refresh: "r".to_owned(),
            refresh_stale: "alt+r".to_owned(),
            versions: "V".to_owned(),
        };
        let banner = guide.banner().unwrap().text;
        assert!(
            banner.contains("r refresh · alt+r refresh stale · V versions"),
            "{banner}"
        );
    }

    #[test]
    fn jumps_are_bounded_and_skip_repeats() {
        let mut state = GuideState::default();
        for index in 0..(MAX_JUMPS + 10) {
            let key = ItemKey::Composer;
            state.push_jump(key.clone());
            if index == 0 {
                assert_eq!(state.jumps.len(), 1);
            }
        }
        assert_eq!(state.jumps.len(), 1);
        for index in 0..(MAX_JUMPS + 10) {
            state.push_jump(ItemKey::Thread {
                thread_id: index.to_string(),
            });
        }
        assert_eq!(state.jumps.len(), MAX_JUMPS);
    }
}
