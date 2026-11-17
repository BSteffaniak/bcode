//! The guided review on the review screen (see `docs/code-review-guide.md`): reading the
//! review's guide, the guide panel and its keys, the diff's citation markers, guide navigation
//! with its jump-back stack, gap reveal for cited context, and the context peek of cited code
//! outside the review.
//!
//! * **Reading.** Once the review is open the guide is read from the store: the review's guides,
//!   the version shown (the guide's current one), its reviewed sections, and its citations'
//!   freshness against the revisions on screen. [`ReviewSurface::reload_guide`] reads it again.
//! * **Navigation.** A guide jump remembers where the cursor was ([`MAX_JUMPS`] deep) and moves
//!   to the cited line: a range in a hidden gap of a loaded file is revealed first, a file still
//!   loading is moved to once it loads, and code outside the review opens the peek.
//!
//! [`MAX_JUMPS`]: crate::guide::state::MAX_JUMPS

use super::{Focus, ReviewSurface, annotate};
use crate::action::Action;
use crate::context::UiContext;
use crate::guide::panel::{self, PanelClick, PanelModel, Peek, Placement};
use crate::guide::{CiteTarget, GuideLoad, GuideLoaded, GuideMarks};
use crate::loader::Message;
use crate::notes::NoteScope;
use crate::panels::SidePlacement;
use crate::services::ReviewServices;
use bcode_code_review::document::{FileLoad, FileRef, ItemKey, ReviewItem};
use bcode_code_review::guide::config::Markers;
use bcode_code_review_models::{
    CitationRelation, GetGuideVersionRequest, GetGuideVersionResponse, GuideScope,
    ListGuideSectionMarksRequest, ListGuideSectionMarksResponse, ListReviewGuidesRequest,
    ListReviewGuidesResponse, ResolvedSource, ReviewLineSide,
};
use bmux_keyboard::KeyStroke;
use bmux_tui::geometry::Rect;
use bmux_tui::text::{Line, Span};
use bmux_tui_components::pane::PaneState;
use bmux_tui_components::status_bar::StatusSeverity;
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::fmt::Write as _;

/// Keymap scope of the guide panel (its own keys; the review's guide keys work everywhere).
pub const GUIDE_SCOPE: &str = "code-review-guide";
/// Pane of the guide panel.
pub const GUIDE_PANE_ID: &str = "review.guide";
/// The guide panel's content.
pub const GUIDE_VIEW_ID: &str = "guide";
/// The context peek in the document's place.
pub const PEEK_ID: &str = "review.guide.peek";
/// Footer hints while the guide panel has focus: action and label.
pub(super) const HINTS: [(&str, &str); 8] = [
    ("guide_down", "move"),
    ("guide_open", "show"),
    ("guide_cite_next", "next citation"),
    ("guide_fold", "fold"),
    ("guide_copy", "copy"),
    ("guide_back", "jump back"),
    ("focus_next", "next pane"),
    ("guide_leave", "back"),
];

/// The guide on screen: what was read, the markers and panel built from it, and the panel's
/// interaction state.
#[derive(Debug)]
pub(super) struct GuideUi {
    /// What was read.
    pub(super) state: crate::guide::GuideState,
    /// The diff's markers.
    pub(super) marks: GuideMarks,
    marks_stamp: Option<(u64, bool, Markers)>,
    /// What the panel says.
    pub(super) model: PanelModel,
    model_stamp: Option<(u64, Placement, u64)>,
    /// What the panel says without a guide.
    pub(super) empty: Vec<Line>,
    /// Folded sections, by id.
    pub(super) folded: BTreeSet<String>,
    /// The panel's pane.
    pub(super) pane: Cell<PaneState>,
    /// Rows the current section is scrolled by.
    pub(super) scroll: Cell<u16>,
    /// Clicks the panel collected.
    pub(super) clicks: RefCell<Vec<PanelClick>>,
    /// The context peek, when open.
    pub(super) peek: Option<Peek>,
    /// The guide load the next answer must come from.
    generation: u64,
}

impl GuideUi {
    /// A fresh guide state for a newly opened review.
    pub(super) fn new() -> Self {
        Self {
            state: crate::guide::GuideState::default(),
            marks: GuideMarks::default(),
            marks_stamp: None,
            model: PanelModel::default(),
            model_stamp: None,
            empty: Vec::new(),
            folded: BTreeSet::new(),
            pane: Cell::new(PaneState::new(Rect::default())),
            scroll: Cell::new(0),
            clicks: RefCell::new(Vec::new()),
            peek: None,
            generation: 0,
        }
    }
}

/// Read the review's guide: its guides, the one shown (`preferred` when it still exists, else
/// the newest whole-review guide, else the newest), that guide's current version and reviewed
/// sections, and its citations' freshness against `sources`.
async fn read_guide(
    services: ReviewServices,
    scope: NoteScope,
    sources: Vec<ResolvedSource>,
    preferred: Option<String>,
) -> Result<Box<GuideLoaded>, String> {
    let listed: ListReviewGuidesResponse = services
        .call(ListReviewGuidesRequest {
            repo_path: scope.repo_path.clone(),
            target: scope.target.clone(),
            scope: scope.scope.clone(),
            include_archived: false,
        })
        .await
        .map_err(|error| error.message)?;
    let guides = listed.guides;
    let chosen = preferred
        .and_then(|id| guides.iter().find(|record| record.guide.guide_id == id))
        .or_else(|| {
            guides
                .iter()
                .find(|record| record.guide.scope == GuideScope::Review)
        })
        .or_else(|| guides.first())
        .cloned();
    let Some(chosen) = chosen else {
        return Ok(Box::new(GuideLoaded {
            guides,
            active: None,
            marks: Vec::new(),
            freshness: None,
        }));
    };
    let guide_id = chosen.guide.guide_id.clone();
    let version: GetGuideVersionResponse = services
        .call(GetGuideVersionRequest {
            repo_path: scope.repo_path.clone(),
            guide_id: guide_id.clone(),
            version: None,
        })
        .await
        .map_err(|error| error.message)?;
    let Some(version) = version.version else {
        return Ok(Box::new(GuideLoaded {
            guides,
            active: None,
            marks: Vec::new(),
            freshness: None,
        }));
    };
    let marks: ListGuideSectionMarksResponse = services
        .call(ListGuideSectionMarksRequest {
            repo_path: scope.repo_path.clone(),
            guide_id,
        })
        .await
        .map_err(|error| error.message)?;
    let data = services.data();
    let repo = scope.repo_path.clone();
    let shown = version.version.clone();
    // Freshness is best effort: a guide whose citations cannot be compared still shows.
    let freshness = services
        .blocking(move || {
            Ok(
                bcode_code_review::guide::freshness::assess_version(&data, &repo, &shown, &sources)
                    .ok(),
            )
        })
        .await
        .ok()
        .flatten();
    Ok(Box::new(GuideLoaded {
        guides,
        active: Some((chosen, version)),
        marks: marks.marks,
        freshness,
    }))
}

impl ReviewSurface {
    /// Read the review's guide when it was not read yet (or a reload was asked for).
    pub(super) fn request_guide(&mut self, host: &UiContext) {
        if self.guide.state.loading || self.guide.state.load != GuideLoad::Idle {
            return;
        }
        let Some(review) = self.review.as_ref() else {
            return;
        };
        let mut sources: Vec<ResolvedSource> = Vec::new();
        for group in review.doc.groups() {
            if !sources
                .iter()
                .any(|source| source.source_id == group.source.source_id)
            {
                sources.push(group.source.clone());
            }
        }
        let preferred = self
            .guide
            .state
            .active
            .as_ref()
            .map(|active| active.guide.guide_id.clone());
        let scope = self.note_scope();
        self.guide.state.loading = true;
        self.guide.state.load = GuideLoad::Loading;
        self.guide.generation = self.guide.generation.wrapping_add(1);
        let generation = self.guide.generation;
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        host.spawn(Box::pin(async move {
            let result = read_guide(services, scope, sources, preferred).await;
            inbox.push(Message::Guide { generation, result });
        }));
    }

    /// Read the review's guide again (it was written, or the review changed).
    pub fn reload_guide(&mut self) {
        if !self.guide.state.loading {
            self.guide.state.load = GuideLoad::Idle;
        }
    }

    /// Apply a finished guide read.
    pub(super) fn apply_guide(
        &mut self,
        generation: u64,
        result: Result<Box<GuideLoaded>, String>,
    ) {
        if generation != self.guide.generation {
            return;
        }
        let before = self
            .guide
            .state
            .active
            .as_ref()
            .and_then(|active| active.current().map(|entry| entry.id.clone()));
        match result {
            Ok(loaded) => self.guide.state.apply(*loaded),
            Err(error) => self.guide.state.fail(error),
        }
        let after = self
            .guide
            .state
            .active
            .as_ref()
            .and_then(|active| active.current().map(|entry| entry.id.clone()));
        if before != after {
            self.guide.scroll.set(0);
        }
    }

    /// Apply a finished peek read.
    pub(super) fn apply_peek(&mut self, citation: &str, result: Result<Vec<String>, String>) {
        if let Some(peek) = self
            .guide
            .peek
            .as_mut()
            .filter(|peek| peek.citation == citation)
        {
            peek.content = Some(result);
        }
    }

    /// The guide shown, once read.
    #[must_use]
    pub const fn guide(&self) -> Option<&crate::guide::ActiveGuide> {
        self.guide.state.active.as_ref()
    }

    /// The guide's markers on the diff as last built.
    #[must_use]
    pub const fn guide_marks(&self) -> &GuideMarks {
        &self.guide.marks
    }

    /// The context peek, when open.
    #[must_use]
    pub const fn guide_peek(&self) -> Option<&Peek> {
        self.guide.peek.as_ref()
    }

    /// Rebuild the diff's markers when the guide, its section, or the marker setting changed.
    pub(super) fn sync_guide_marks(&mut self) {
        let markers = self.services.guide_config().markers;
        let stamp = (self.guide.state.revision, self.guide.state.engaged, markers);
        if self.guide.marks_stamp == Some(stamp) {
            return;
        }
        self.guide.marks_stamp = Some(stamp);
        self.guide.marks =
            self.guide
                .state
                .active
                .as_ref()
                .map_or_else(GuideMarks::default, |active| {
                    GuideMarks::build(
                        active,
                        markers,
                        self.guide.state.engaged,
                        self.guide.state.revision,
                    )
                });
    }

    /// Rebuild what the panel says for `placement` when the guide or the theme changed.
    pub(super) fn sync_guide_model(&mut self, placement: SidePlacement) {
        let placement = if placement == SidePlacement::Bottom {
            Placement::Under
        } else {
            Placement::Beside
        };
        let stamp = (self.guide.state.revision, placement, self.paint_revision);
        if self.guide.model_stamp == Some(stamp) {
            return;
        }
        self.guide.model_stamp = Some(stamp);
        let theme = &self.theme;
        self.guide.model =
            self.guide
                .state
                .active
                .as_ref()
                .map_or_else(PanelModel::default, |active| {
                    panel::build_model(
                        active,
                        &self.guide.folded,
                        theme,
                        placement,
                        self.guide.state.revision,
                    )
                });
        let assistant = self
            .action_keys
            .get("assistant_toggle")
            .cloned()
            .unwrap_or_else(|| "the Ask AI button".to_owned());
        self.guide.empty = match &self.guide.state.load {
            GuideLoad::Failed(error) => vec![Line::from_spans(vec![Span::styled(
                format!("The guide could not be read: {error}"),
                theme.error,
            )])],
            GuideLoad::Idle | GuideLoad::Loading => vec![Line::from_spans(vec![Span::styled(
                "Reading the guide…",
                theme.muted,
            )])],
            GuideLoad::Ready => {
                let mut lines = vec![Line::from_spans(vec![Span::styled(
                    format!("Ask the review assistant ({assistant}) to walk you through it."),
                    theme.muted,
                )])];
                if self.is_transient() {
                    lines.push(Line::from_spans(vec![Span::styled(
                        "Save the review to keep its guide.",
                        theme.notice,
                    )]));
                }
                lines
            }
        };
    }

    /// The guide panel's title and status.
    pub(super) fn guide_titles(&self) -> (String, String) {
        let Some(active) = self.guide.state.active.as_ref() else {
            return ("Guide".to_owned(), String::new());
        };
        let icons = &self.theme.icons;
        let counts = active.counts();
        let mut status = format!(
            "{}{}/{}  {}{}/{}",
            icons.viewed,
            counts.reviewed,
            counts.sections,
            icons.guide_verified,
            counts.verified,
            counts.citations
        );
        if counts.stale > 0 {
            let _ = write!(status, "  {}{}", icons.guide_stale, counts.stale);
        }
        (
            format!(
                "Guide · v{} · {}",
                active.version.version, active.guide.title
            ),
            status,
        )
    }

    // ----------------------------------------------------------------------------------------
    // Navigation
    // ----------------------------------------------------------------------------------------

    /// The document file a citation cites, when the review shows it.
    fn guide_file(&self, target: &CiteTarget) -> Option<FileRef> {
        if target.relation == CitationRelation::OutsideReview {
            return None;
        }
        let review = self.review.as_ref()?;
        review
            .doc
            .file_keys()
            .find(|file| {
                file.path == target.path
                    && target
                        .source_id
                        .as_deref()
                        .is_none_or(|source| source == file.source_id)
            })
            .cloned()
    }

    /// Move the document to `target`: its first line, revealing a hidden gap first; or the
    /// file's header while it loads, moving on once it has. Returns whether the document has
    /// the file at all.
    fn guide_move(&mut self, file: &FileRef, target: &CiteTarget) -> bool {
        let Some(review) = self.review.as_mut() else {
            return false;
        };
        let key = ItemKey::Line {
            file: file.clone(),
            side: target.side,
            line: target.lines.start,
        };
        let loaded = !matches!(
            review.doc.load_state(file),
            Some(FileLoad::Unloaded | FileLoad::Loading) | None
        );
        if review.doc.index_of(&key).is_none()
            && loaded
            && review
                .doc
                .reveal_lines(file, target.lines.start, target.lines.end)
        {
            review.note_splices();
        }
        if review.doc.jump_to(&key).is_none() {
            return false;
        }
        annotate::jump_to_annotation(review, &key);
        self.guide.state.pending =
            (review.doc.index_of(&key).is_none() && !loaded).then(|| target.clone());
        true
    }

    /// Go to `target`, remembering where the cursor was: in the document when it shows the
    /// file, else in the context peek.
    fn guide_jump(&mut self, target: &CiteTarget, host: &UiContext) {
        if let Some(cursor) = self
            .review
            .as_ref()
            .and_then(|review| review.cursor.clone())
        {
            self.guide.state.push_jump(cursor);
        }
        self.guide.state.engaged = true;
        self.guide.state.bump();
        if let Some(file) = self.guide_file(target)
            && self.guide_move(&file, target)
        {
            self.guide.peek = None;
            self.focus_document_for_guide();
            return;
        }
        self.open_peek(target, host);
    }

    /// Keyboard focus stays where it is, except that the document takes it from the composer
    /// or search when a guide jump moves it.
    const fn focus_document_for_guide(&mut self) {
        if matches!(self.focus, Focus::Composer | Focus::Search) {
            self.focus = Focus::Review;
        }
    }

    /// Show cited code outside the review read-only in the document's place.
    fn open_peek(&mut self, target: &CiteTarget, host: &UiContext) {
        let relation = match target.relation {
            CitationRelation::OutsideReview => "unchanged · outside this review",
            _ => "not shown in this review",
        };
        self.guide.peek = Some(Peek {
            citation: target.citation.clone(),
            title: format!(
                "{}:{}-{} · {relation}",
                target.path, target.lines.start, target.lines.end
            ),
            lines: (target.lines.start, target.lines.end),
            content: None,
            scroll: 0,
        });
        let data = self.services.data();
        let services = self.services.clone();
        let repo = self.repo_path.clone();
        let inbox = self.inbox.clone();
        let citation = target.citation.clone();
        let revision = target.revision.clone();
        let path = target.path.clone();
        host.spawn(Box::pin(async move {
            let result = services
                .blocking(move || {
                    Ok(
                        bcode_code_review::guide::cite::read_file(&data, &repo, &revision, &path)
                            .and_then(|lines| {
                                lines
                                    .ok_or_else(|| format!("{path} is not in the repository there"))
                            }),
                    )
                })
                .await
                .unwrap_or_else(|error| Err(error.message));
            inbox.push(Message::Peek { citation, result });
        }));
    }

    /// Close the peek. Returns whether it was open.
    fn close_peek(&mut self) -> bool {
        self.guide.peek.take().is_some()
    }

    /// Move once a file a guide jump waits for has loaded.
    pub(super) fn retry_guide_jump(&mut self) -> bool {
        let Some(target) = self.guide.state.pending.clone() else {
            return false;
        };
        let Some(file) = self.guide_file(&target) else {
            self.guide.state.pending = None;
            return false;
        };
        let loading = self.review.as_ref().is_some_and(|review| {
            matches!(
                review.doc.load_state(&file),
                Some(FileLoad::Unloaded | FileLoad::Loading)
            )
        });
        if loading {
            return false;
        }
        self.guide.state.pending = None;
        self.guide_move(&file, &target)
    }

    /// Select section `entry` and show its citation at `cite` (when it has one).
    fn guide_show(&mut self, entry: usize, cite: Option<usize>, host: &UiContext) {
        let Some(active) = self.guide.state.active.as_mut() else {
            return;
        };
        if active.select(entry) {
            self.guide.scroll.set(0);
        }
        let id = cite.and_then(|cite| {
            active
                .current()
                .and_then(|entry| entry.citations.get(cite).cloned())
        });
        active.cite = id.as_ref().and(cite);
        let target = id.and_then(|id| active.target(&id));
        self.guide.state.engaged = true;
        self.guide.state.bump();
        if let Some(target) = target {
            self.guide_jump(&target, host);
        }
    }

    /// `guide_next` / `guide_previous`: the current section's next (previous) citation, else
    /// the next (previous) section and its first citation.
    fn guide_step(&mut self, forward: bool, host: &UiContext) -> Action {
        let Some(active) = self.guide.state.active.as_ref() else {
            self.no_guide();
            return Action::Redraw;
        };
        let count = active.current().map_or(0, |entry| entry.citations.len());
        let within = match (forward, active.cite) {
            (true, None) if count > 0 && self.guide.state.engaged => Some(0),
            (true, Some(cite)) if cite + 1 < count => Some(cite + 1),
            (false, Some(cite)) if cite > 0 => Some(cite - 1),
            _ => None,
        };
        let selected = active.selected;
        if let Some(cite) = within {
            self.guide_show(selected, Some(cite), host);
            return Action::Redraw;
        }
        if !self.guide.state.engaged && forward {
            // The first step shows where the guide starts.
            self.guide_show(selected, (count > 0).then_some(0), host);
            return Action::Redraw;
        }
        let next = if forward {
            selected + 1
        } else if let Some(previous) = selected.checked_sub(1) {
            previous
        } else {
            self.set_status("start of the guide", StatusSeverity::Info);
            return Action::Redraw;
        };
        let Some(entry) = active.entries.get(next) else {
            self.set_status("end of the guide", StatusSeverity::Info);
            return Action::Redraw;
        };
        let cite = (!entry.citations.is_empty()).then_some(0);
        self.guide_show(next, cite, host);
        Action::Redraw
    }

    /// `guide_next_unreviewed`: the next written section not reviewed, after the current one.
    fn guide_next_unreviewed(&mut self, host: &UiContext) -> Action {
        let Some(active) = self.guide.state.active.as_ref() else {
            self.no_guide();
            return Action::Redraw;
        };
        let total = active.entries.len();
        let found = (1..=total)
            .map(|offset| (active.selected + offset) % total.max(1))
            .find(|index| {
                let entry = &active.entries[*index];
                entry.state != bcode_code_review_models::SectionState::Planned
                    && !active.reviewed(&entry.id)
            });
        let Some(index) = found else {
            self.set_status("every section is reviewed", StatusSeverity::Info);
            return Action::Redraw;
        };
        let cite = (!active.entries[index].citations.is_empty()).then_some(0);
        self.guide_show(index, cite, host);
        Action::Redraw
    }

    /// `guide_at_line`: show the section citing the line at the cursor.
    fn guide_at_line(&mut self) -> Action {
        let Some(active) = self.guide.state.active.as_ref() else {
            self.no_guide();
            return Action::Redraw;
        };
        let line =
            self.review
                .as_ref()
                .and_then(|review| match review.doc.item(review.cursor_index)? {
                    ReviewItem::Line(line) => {
                        let (side, number) = line.key_side();
                        Some((line.file.clone(), side, number))
                    }
                    ReviewItem::LinePair(pair) => pair
                        .new
                        .as_ref()
                        .map(|side| (ReviewLineSide::New, side.line))
                        .or_else(|| {
                            pair.old
                                .as_ref()
                                .map(|side| (ReviewLineSide::Old, side.line))
                        })
                        .map(|(side, number)| (pair.file.clone(), side, number)),
                    _ => None,
                });
        let Some((file, side, number)) = line else {
            self.set_status("move to a line to see its section", StatusSeverity::Info);
            return Action::Redraw;
        };
        let found = active
            .citations_at(&file.source_id, &file.path, side, number)
            .into_iter()
            .find_map(|id| {
                let entry = active.entry_citing(id)?;
                let cite = active.entries[entry]
                    .citations
                    .iter()
                    .position(|cited| cited == id);
                Some((entry, cite))
            });
        let Some((entry, cite)) = found else {
            self.set_status(
                "no section of the guide cites this line",
                StatusSeverity::Info,
            );
            return Action::Redraw;
        };
        self.focus_guide_section(entry, cite);
        Action::Redraw
    }

    /// Select section `entry` (at citation `cite`) and show it in the panel, without moving
    /// the document.
    fn focus_guide_section(&mut self, entry: usize, cite: Option<usize>) {
        let Some(active) = self.guide.state.active.as_mut() else {
            return;
        };
        if active.select(entry) {
            self.guide.scroll.set(0);
        }
        active.cite = cite;
        self.guide.state.engaged = true;
        self.guide.state.bump();
        if !self.panels.guide_open {
            self.panels.show_guide();
            self.persist_panels();
        }
    }

    /// A click on badge `number` in the diff: show the section citing it.
    pub(super) fn guide_badge_click(&mut self, number: u16) {
        let found = self.guide.state.active.as_ref().and_then(|active| {
            let id = active.citation_numbered(number)?;
            let entry = active.entry_citing(id)?;
            let cite = active.entries[entry]
                .citations
                .iter()
                .position(|cited| cited == id);
            Some((entry, cite))
        });
        if let Some((entry, cite)) = found {
            self.focus_guide_section(entry, cite);
        }
    }

    /// `guide_back`: close the peek, else go back to where the last guide jump left.
    fn guide_back(&mut self) -> Action {
        if self.close_peek() {
            return Action::Redraw;
        }
        let Some(key) = self.guide.state.jumps.pop() else {
            self.set_status("no guide jump to go back from", StatusSeverity::Info);
            return Action::Redraw;
        };
        if let Some(review) = self.review.as_mut() {
            annotate::jump_to_annotation(review, &key);
        }
        Action::Redraw
    }

    fn no_guide(&mut self) {
        let message = match &self.guide.state.load {
            GuideLoad::Idle | GuideLoad::Loading => "reading the guide…".to_owned(),
            GuideLoad::Failed(error) => format!("the guide could not be read: {error}"),
            GuideLoad::Ready => "no guide for this review yet".to_owned(),
        };
        self.set_status(message, StatusSeverity::Info);
    }

    /// `guide_toggle`: open and focus the panel, focus it when open elsewhere, or close it.
    fn guide_toggle(&mut self) -> Action {
        if !self.panels.guide_open {
            self.panels.show_guide();
            self.focus = Focus::Guide;
            self.guide.state.engaged = true;
            self.guide.state.bump();
        } else if self.focus != Focus::Guide {
            self.focus = Focus::Guide;
        } else {
            self.panels.toggle_guide();
            self.focus = Focus::Review;
        }
        self.persist_panels();
        Action::Redraw
    }

    /// The review's guide actions (keys that work from every pane). `None` for other actions.
    pub(super) fn guide_action(&mut self, action: &str, host: &UiContext) -> Option<Action> {
        Some(match action {
            "guide_toggle" => self.guide_toggle(),
            "guide_next" => self.guide_step(true, host),
            "guide_previous" => self.guide_step(false, host),
            "guide_next_unreviewed" => self.guide_next_unreviewed(host),
            "guide_at_line" => self.guide_at_line(),
            "guide_back" => self.guide_back(),
            _ => return None,
        })
    }

    /// Keys while the peek is open in the document's place: scrolling and leaving it; the
    /// guide's own keys keep working. `None` for keys it leaves to the review.
    pub(super) fn peek_key(&mut self, stroke: KeyStroke, host: &UiContext) -> Option<Action> {
        let peek = self.guide.peek.as_mut()?;
        let action = host.keymap_action(super::SCOPE, stroke)?;
        let action = action.strip_prefix("bcode.code_review.").unwrap_or(&action);
        let rows = |peek: &mut Peek, delta: i64| {
            peek.scroll = if delta < 0 {
                peek.scroll
                    .saturating_sub(u32::try_from(-delta).unwrap_or(0))
            } else {
                peek.scroll
                    .saturating_add(u32::try_from(delta).unwrap_or(0))
            };
        };
        match action {
            "exit" | "guide_back" => {
                self.close_peek();
            }
            "down" => rows(peek, 1),
            "up" => rows(peek, -1),
            "half_page_down" | "page_down" => rows(peek, 10),
            "half_page_up" | "page_up" => rows(peek, -10),
            action if action.starts_with("guide_") => return None,
            _ => return Some(Action::None),
        }
        Some(Action::Redraw)
    }

    /// Keys of the guide panel. `None` for keys the panel does not use.
    pub(super) fn guide_key(&mut self, stroke: KeyStroke, host: &UiContext) -> Option<Action> {
        let action = host.keymap_action(GUIDE_SCOPE, stroke)?;
        let action = action.strip_prefix("bcode.code_review.").unwrap_or(&action);
        if action == "guide_leave" {
            self.focus = Focus::Review;
            return Some(Action::Redraw);
        }
        let unavailable = match action {
            "guide_reviewed" => Some("marking sections reviewed"),
            "guide_expand" => Some("expanding sections"),
            "guide_ask" => Some("asking about sections"),
            "guide_comment" => Some("drafting comments from findings"),
            "guide_promote" | "guide_discard" => Some("provisional sections"),
            "guide_refresh" => Some("refreshing sections"),
            "guide_restructure" => Some("restructuring the guide"),
            "guide_versions" => Some("guide versions"),
            _ => None,
        };
        if let Some(what) = unavailable {
            self.set_status(format!("{what} is not available yet"), StatusSeverity::Info);
            return Some(Action::Redraw);
        }
        if self.guide.state.active.is_none() {
            self.no_guide();
            return Some(Action::Redraw);
        }
        match action {
            "guide_down" | "guide_up" => self.guide_select_row(action == "guide_down"),
            "guide_open" => {
                let (selected, cite) = self.guide_current_first_cite();
                self.guide_show(selected, cite, host);
            }
            "guide_fold" => self.guide_fold(),
            "guide_cite_next" | "guide_cite_previous" => {
                self.guide_cite(action == "guide_cite_next", host);
            }
            "guide_copy" => self.guide_copy(),
            _ => return Some(Action::None),
        }
        Some(Action::Redraw)
    }

    /// The current section and its first citation, when it has one.
    fn guide_current_first_cite(&self) -> (usize, Option<usize>) {
        self.guide
            .state
            .active
            .as_ref()
            .map_or((0, None), |active| {
                let cite = active
                    .current()
                    .is_some_and(|entry| !entry.citations.is_empty())
                    .then_some(0);
                (active.selected, cite)
            })
    }

    /// Move the panel's selection to the next (previous) outline row shown, without moving the
    /// document.
    fn guide_select_row(&mut self, forward: bool) {
        let rows: Vec<usize> = self
            .guide
            .model
            .outline
            .iter()
            .map(|row| row.entry)
            .collect();
        let Some(active) = self.guide.state.active.as_mut() else {
            return;
        };
        let position = rows.iter().position(|entry| *entry == active.selected);
        let next = match (position, forward) {
            (Some(at), true) => rows.get(at + 1).copied(),
            (Some(at), false) => at.checked_sub(1).and_then(|at| rows.get(at).copied()),
            (None, _) => rows.first().copied(),
        };
        if let Some(entry) = next
            && active.select(entry)
        {
            self.guide.scroll.set(0);
            self.guide.state.engaged = true;
            self.guide.state.bump();
        }
    }

    /// Fold or unfold the current section's children.
    fn guide_fold(&mut self) {
        let Some(entry) = self
            .guide
            .state
            .active
            .as_ref()
            .and_then(|active| active.current())
            .filter(|entry| entry.has_children)
            .map(|entry| entry.id.clone())
        else {
            return;
        };
        if !self.guide.folded.remove(&entry) {
            self.guide.folded.insert(entry);
        }
        self.guide.state.bump();
    }

    /// The current section's next (previous) citation, wrapping around.
    fn guide_cite(&mut self, forward: bool, host: &UiContext) {
        let Some(active) = self.guide.state.active.as_ref() else {
            return;
        };
        let count = active.current().map_or(0, |entry| entry.citations.len());
        if count == 0 {
            self.set_status("this section cites no code", StatusSeverity::Info);
            return;
        }
        let cite = match (active.cite, forward) {
            (None, true) => 0,
            (None, false) => count - 1,
            (Some(cite), true) => (cite + 1) % count,
            (Some(cite), false) => (cite + count - 1) % count,
        };
        let selected = active.selected;
        self.guide_show(selected, Some(cite), host);
    }

    /// Copy the current section as markdown.
    fn guide_copy(&mut self) {
        let Some(active) = self.guide.state.active.as_ref() else {
            return;
        };
        let Some(entry) = active.current() else {
            return;
        };
        let markdown = bcode_code_review::guide::export::render_markdown(
            &active.version,
            &bcode_code_review::guide::export::ExportOptions {
                title: &active.guide.title,
                links: bcode_code_review::guide::export::LinkStyle::Relative,
                include_unverified: true,
                sections: vec![entry.id.clone()],
                ..Default::default()
            },
        );
        self.clipboard = Some((markdown, "guide section"));
    }

    /// Apply the clicks the panel collected.
    pub(super) fn after_guide_event(&mut self, host: &UiContext) -> bool {
        let clicks: Vec<PanelClick> = self.guide.clicks.borrow_mut().drain(..).collect();
        let changed = !clicks.is_empty();
        for click in clicks {
            self.focus = Focus::Guide;
            match click {
                PanelClick::Section(entry) => {
                    let cite = self.guide.state.active.as_ref().and_then(|active| {
                        active
                            .entries
                            .get(entry)
                            .is_some_and(|entry| !entry.citations.is_empty())
                            .then_some(0)
                    });
                    self.guide_show(entry, cite, host);
                }
                PanelClick::Citation(id) => {
                    let found = self.guide.state.active.as_ref().and_then(|active| {
                        let cite = active
                            .current()?
                            .citations
                            .iter()
                            .position(|cited| *cited == id)?;
                        Some((active.selected, cite))
                    });
                    if let Some((entry, cite)) = found {
                        self.guide_show(entry, Some(cite), host);
                    }
                }
                PanelClick::Previous | PanelClick::Next => {
                    let forward = click == PanelClick::Next;
                    let next = self.guide.state.active.as_ref().and_then(|active| {
                        let index = if forward {
                            active.selected + 1
                        } else {
                            active.selected.checked_sub(1)?
                        };
                        let entry = active.entries.get(index)?;
                        Some((index, (!entry.citations.is_empty()).then_some(0)))
                    });
                    if let Some((index, cite)) = next {
                        self.guide_show(index, cite, host);
                    }
                }
            }
        }
        changed
    }
}
