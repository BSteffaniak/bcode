//! The guided review's side of the review assistant: asking for a walkthrough, following the
//! guide while the assistant writes it, the `guide_show` tool, the guide card the conversation
//! shows, the guide on the assistant's Guide tab, and stopping a guide's writing.
//!
//! * **Asking.** "Walk me through this review" (the palette, the AI menu, or `enter` on the
//!   empty guide panel) sends the request to the assistant's conversation; the variant with
//!   directions puts it in the assistant's input for the reviewer to finish.
//! * **Following.** A finished `code_review.guide.start` or `code_review.guide.write` of the
//!   assistant's session (or a followed card session) reads the guide it named again, so the
//!   outline appears once started and sections fill in as each write lands. A start also opens
//!   the guide panel when the body has room for it.
//! * **Stopping.** When the reviewer stops the assistant's turn, every version of the review's
//!   guides that session is still writing is marked stopped (cancelled) with a revision-checked
//!   write, on screen or not; a version another conversation resumed meanwhile is left to it.
//!   What was written stays.
//! * **Guide tab.** When the body has no room for the guide panel, the app paints the guide in
//!   the assistant panel's Guide tab and routes its keys and clicks here, to the same state the
//!   panel uses.

use super::guide::Prefer;
use super::{Focus, ReviewSurface};
use crate::action::Action;
use crate::assistant::LinkRequest;
use crate::assistant::actions::GuideShowArguments;
use crate::assistant::context::GuideView;
use crate::context::UiContext;
use crate::guide::{GuideCard, GuideLoad};
use crate::loader::Message;
use crate::notes::NoteScope;
use crate::panels::SidePlacement;
use crate::services::ReviewServices;
use bcode_code_review_models::{
    GetGuideVersionRequest, GuideStatus, ListGuideVersionsRequest, ListReviewGuidesRequest,
    SaveGuideVersionRequest,
};
use bmux_keyboard::KeyStroke;
use bmux_tui::component::{Component, Constraints, EventCx, LayoutCx};
use bmux_tui::composition::{Column, Flex, TextBlock};
use bmux_tui::event::{Event, EventOutcome};
use bmux_tui::geometry::Rect;
use bmux_tui::paint::PaintCx;
use bmux_tui::text::{Line, Span, Text};
use bmux_tui_components::status_bar::StatusSeverity;
use std::sync::Arc;

/// What a `guide_show` call did at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuideShowOutcome {
    /// It ran: what it did.
    Done(String),
    /// It waits for the guide it names to be read.
    Waiting,
}

/// The message that asks the assistant for a walkthrough.
pub const WALKTHROUGH: &str = "Walk me through this review.";
/// The start of the message the reviewer finishes with directions.
pub const WALKTHROUGH_DIRECTIONS: &str = "Walk me through this review. Directions: ";
/// Layout id of the guide on the assistant's Guide tab.
pub const GUIDE_TAB_VIEW_ID: &str = "guide.tab";
/// Attempts at marking a stopped version when its revision keeps changing.
const STOP_ATTEMPTS: usize = 4;
/// Guide tools that write.
const GUIDE_START: &str = "code_review.guide.start";
const GUIDE_WRITE: &str = "code_review.guide.write";

impl ReviewSurface {
    /// Ask the assistant to walk the reviewer through the review: at once (`send`), or in its
    /// input for the reviewer to add directions.
    pub(super) fn walk_me_through(&mut self, send: bool) {
        let text = if send {
            WALKTHROUGH
        } else {
            WALKTHROUGH_DIRECTIONS
        };
        // A new walkthrough writes the versions it starts, not what an earlier request granted.
        self.set_guide_grant(None);
        self.link.requests.push(LinkRequest::Message {
            text: text.to_owned(),
            send,
        });
        self.set_status(
            if send {
                "asked the assistant to walk you through the review"
            } else {
                "add your directions in the assistant and press enter"
            },
            StatusSeverity::Info,
        );
    }

    /// Follow finished guide writes (name, JSON arguments): read the guide each names again,
    /// and open the panel for a guide just started. Returns whether any call was a guide write.
    pub(super) fn follow_guide_tools(&mut self, tools: &[(String, String)]) -> bool {
        let mut wrote = false;
        let mut started = false;
        for (name, arguments) in tools {
            let start = name == GUIDE_START;
            if !start && name != GUIDE_WRITE {
                continue;
            }
            wrote = true;
            started |= start;
            self.guide.prefer = Some(crate::rows::guide_id_of(arguments).map_or_else(
                || {
                    if start {
                        Prefer::Newest
                    } else {
                        self.guide.prefer.clone().unwrap_or(Prefer::Newest)
                    }
                },
                Prefer::Guide,
            ));
        }
        if !wrote {
            return false;
        }
        // What is being written shows: the current version, not an older one being read.
        self.guide.view = None;
        self.reload_guide();
        if started {
            self.guide.state.engaged = true;
            self.guide.state.bump();
            if self.panels.guide_fits() && !self.panels.guide_open {
                self.panels.show_guide();
                self.persist_panels();
            }
        }
        true
    }

    /// The guide card of the guide on screen, while there is one.
    #[must_use]
    pub fn guide_card(&self) -> Option<Arc<GuideCard>> {
        let active = self.guide.state.active.as_ref()?;
        let revision = self.guide.state.revision;
        let mut cached = self.guide.card.borrow_mut();
        if let Some((at, card)) = cached.as_ref()
            && *at == revision
        {
            return Some(Arc::clone(card));
        }
        let card = Arc::new(GuideCard::of(active, revision));
        *cached = Some((revision, Arc::clone(&card)));
        Some(card)
    }

    /// Whether the review has a guide on screen.
    #[must_use]
    pub const fn has_guide(&self) -> bool {
        self.guide.state.active.is_some()
    }

    /// Whether the body as last laid out has room for the guide panel.
    #[must_use]
    pub const fn guide_docks(&self) -> bool {
        self.panels.guide_fits()
    }

    /// Whether a guide jump happened since the last call.
    pub const fn take_guide_stepped(&mut self) -> bool {
        std::mem::replace(&mut self.guide.stepped, false)
    }

    /// Open the guide panel and give it the keyboard (a guide card's Open, when it docks).
    pub fn open_guide_panel(&mut self) {
        if !self.panels.guide_open {
            self.panels.show_guide();
            self.persist_panels();
        }
        self.focus = Focus::Guide;
        self.guide.state.engaged = true;
        self.guide.state.bump();
    }

    /// Show section `entry` (in reading order) and its first citation, from a guide card.
    pub fn guide_card_show(&mut self, entry: usize, host: &UiContext) {
        let cite = self.guide.state.active.as_ref().and_then(|active| {
            active
                .entries
                .get(entry)
                .is_some_and(|entry| !entry.citations.is_empty())
                .then_some(0)
        });
        if !self.panels.guide_open && self.panels.guide_fits() {
            self.panels.show_guide();
            self.persist_panels();
        }
        self.guide_show(entry, cite, host);
        self.guide.stepped = true;
    }

    // ----------------------------------------------------------------------------------------
    // guide_show
    // ----------------------------------------------------------------------------------------

    /// Run `bcode.code-review.guide_show`: open the guide and move through it as the keys do.
    /// Returns what it did, or that it waits for the guide it names to be read; the answer then
    /// comes from [`Self::take_guide_show_answer`].
    ///
    /// # Errors
    ///
    /// * The review has no guide, the guide or version named is not the one that can be shown,
    ///   or the section or citation named is not in it.
    pub fn assistant_guide_show(
        &mut self,
        arguments: &GuideShowArguments,
        host: &UiContext,
    ) -> Result<GuideShowOutcome, String> {
        let shown = self
            .guide
            .state
            .active
            .as_ref()
            .map(|active| active.guide.guide_id.clone());
        let wanted = arguments
            .guide_id
            .as_deref()
            .filter(|id| !id.trim().is_empty());
        if let Some(wanted) = wanted
            && shown.as_deref() != Some(wanted)
        {
            // A guide just started may not be read yet: read the guides again either way, and
            // answer once they are.
            self.guide.prefer = Some(Prefer::Guide(wanted.to_owned()));
            self.reload_guide();
            self.wait_for_guide_show(arguments);
            return Ok(GuideShowOutcome::Waiting);
        }
        if let (Some(version), Some(active)) = (arguments.version, self.guide.state.active.as_ref())
            && version != active.version.version
        {
            let guide_id = active.guide.guide_id.clone();
            self.want_guide_version(&guide_id, version);
            self.wait_for_guide_show(arguments);
            return Ok(GuideShowOutcome::Waiting);
        }
        if shown.is_none() {
            return match &self.guide.state.load {
                GuideLoad::Ready => Err("this review has no guide yet; write one with \
                     code_review.guide.plan, code_review.guide.start and code_review.guide.write"
                    .to_owned()),
                GuideLoad::Failed(error) => Err(format!("the guide could not be read: {error}")),
                GuideLoad::Idle | GuideLoad::Loading => {
                    self.wait_for_guide_show(arguments);
                    Ok(GuideShowOutcome::Waiting)
                }
            };
        }
        self.show_in_guide(arguments, host)
            .map(GuideShowOutcome::Done)
    }

    /// Hold `arguments` until the guide is read, in place of any call waiting before (whose
    /// caller answers it as replaced) and of an answer nobody took.
    fn wait_for_guide_show(&mut self, arguments: &GuideShowArguments) {
        self.guide.pending_show = Some(arguments.clone());
        self.guide.show_answer = None;
    }

    /// The answer of a `guide_show` call that waited for its guide to be read, once it has one.
    pub const fn take_guide_show_answer(&mut self) -> Option<Result<String, String>> {
        self.guide.show_answer.take()
    }

    /// Run a `guide_show` call that waited for its guide to be read, and keep its answer: what
    /// it showed, or that the review has no such guide.
    pub(super) fn run_pending_guide_show(&mut self, host: &UiContext) {
        if self.guide.state.loading || self.guide.state.load != GuideLoad::Ready {
            if let GuideLoad::Failed(error) = &self.guide.state.load
                && self.guide.pending_show.take().is_some()
            {
                self.guide.show_answer = Some(Err(format!("the guide could not be read: {error}")));
            }
            return;
        }
        let Some(arguments) = self.guide.pending_show.take() else {
            return;
        };
        let wanted = arguments
            .guide_id
            .as_deref()
            .filter(|id| !id.trim().is_empty());
        let shown = self
            .guide
            .state
            .active
            .as_ref()
            .map(|active| active.guide.guide_id.as_str());
        let answer = match (wanted, shown) {
            (_, None) => Err("this review has no guide yet; write one with \
                 code_review.guide.plan, code_review.guide.start and code_review.guide.write"
                .to_owned()),
            (Some(wanted), Some(shown)) if wanted != shown => Err(format!(
                "this review has no guide {wanted}; code_review.guide.get lists its guides"
            )),
            _ => self.show_in_guide(&arguments, host),
        };
        self.guide.show_answer = Some(answer);
    }

    /// `guide_show` on the guide on screen.
    fn show_in_guide(
        &mut self,
        arguments: &GuideShowArguments,
        host: &UiContext,
    ) -> Result<String, String> {
        let Some(active) = self.guide.state.active.as_ref() else {
            return Err("this review has no guide yet".to_owned());
        };
        if let Some(version) = arguments.version
            && version != active.version.version
        {
            return Err(format!(
                "this guide has no version {version} (its current version is v{})",
                active.guide.current_version
            ));
        }
        let target = if let Some(section) = arguments.section.as_deref() {
            let section = section.trim().trim_start_matches('§');
            let entry = active
                .entries
                .iter()
                .position(|entry| entry.id == section || entry.number == section)
                .ok_or_else(|| format!("the guide has no section {section}"))?;
            let cite = (!active.entries[entry].citations.is_empty()).then_some(0);
            Some((entry, cite))
        } else if let Some(citation) = arguments.citation.as_deref() {
            let entry = active
                .entry_citing(citation)
                .ok_or_else(|| format!("no section of the guide cites {citation}"))?;
            let cite = active.entries[entry]
                .citations
                .iter()
                .position(|cited| cited == citation);
            Some((entry, cite))
        } else {
            None
        };
        if let Some(step) = arguments.step.as_deref()
            && !matches!(step, "next" | "previous" | "next_unreviewed")
        {
            return Err(format!(
                "unknown step {step:?}; use next, previous or next_unreviewed"
            ));
        }
        if !self.panels.guide_open {
            self.panels.show_guide();
            self.persist_panels();
        }
        self.guide.state.engaged = true;
        self.guide.state.bump();
        if let Some((entry, cite)) = target {
            self.guide_show(entry, cite, host);
        } else {
            match arguments.step.as_deref() {
                Some("next") => {
                    self.guide_step(true, host);
                }
                Some("previous") => {
                    self.guide_step(false, host);
                }
                Some(_) => {
                    self.guide_next_unreviewed(host);
                }
                None => {}
            }
        }
        // Moving to a place always counts as stepping through, so the guide shows even where
        // only the assistant's Guide tab has room for it.
        self.guide.stepped = true;
        if arguments.focus {
            self.focus = Focus::Guide;
        }
        Ok(self.guide_place())
    }

    /// Where the reviewer is in the guide, as a sentence.
    fn guide_place(&self) -> String {
        let Some(active) = self.guide.state.active.as_ref() else {
            return "no guide".to_owned();
        };
        let Some(entry) = active.current() else {
            return format!("showing guide {} (no sections yet)", active.guide.guide_id);
        };
        let cited = active
            .cite
            .and_then(|cite| entry.citations.get(cite))
            .and_then(|id| active.target(id))
            .map(|target| {
                format!(
                    " at {}:{}-{}",
                    target.path, target.lines.start, target.lines.end
                )
            })
            .unwrap_or_default();
        format!(
            "showing §{} {} ({} of {}){cited}",
            entry.number,
            entry.title,
            active.selected + 1,
            active.entries.len()
        )
    }

    /// The guide as `current_view` describes it.
    #[must_use]
    pub(super) fn guide_view(&self) -> Option<GuideView> {
        let active = self.guide.state.active.as_ref()?;
        let counts = active.counts();
        let placement = self.panels.current().map(|plan| plan.guide);
        let panel = match (self.panels.guide_open, placement) {
            (false, _) => "closed",
            (true, Some(SidePlacement::Right)) => "beside",
            (true, Some(SidePlacement::Bottom)) => "under",
            (true, _) => "no_room",
        };
        Some(GuideView {
            guide_id: active.guide.guide_id.clone(),
            title: active.guide.title.clone(),
            version: active.version.version,
            current_version: active.guide.current_version,
            status: active.version.status.as_str().to_owned(),
            sections: counts.sections,
            written: active.written(),
            reviewed: counts.reviewed,
            citations: counts.citations,
            verified: counts.verified,
            stale: counts.stale,
            current_section: active
                .current()
                .map(|entry| (entry.id.clone(), entry.number.clone(), entry.title.clone())),
            panel: panel.to_owned(),
        })
    }

    // ----------------------------------------------------------------------------------------
    // Stopping
    // ----------------------------------------------------------------------------------------

    /// The assistant's turn in `session` was stopped: mark every version of the review's guides
    /// that session is writing stopped (whether or not it is on screen). Written sections stay.
    pub fn guide_turn_stopped(&mut self, session: &str, host: &UiContext) {
        let scope = self.note_scope();
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        let session = session.to_owned();
        host.spawn(Box::pin(async move {
            let result = stop_writing(&services, &scope, &session).await;
            inbox.push(Message::GuideStopped(result));
        }));
    }

    /// The stopped versions' marks landed (or failed): read the guide again when one did.
    pub(super) fn guide_stop_written(&mut self, result: Result<usize, String>) {
        match result {
            Ok(0) => return,
            Ok(_) => self.set_status(
                "stopped writing the guide; written sections stay",
                StatusSeverity::Info,
            ),
            Err(error) => self.set_status(
                format!("could not mark the guide stopped: {error}"),
                StatusSeverity::Warning,
            ),
        }
        self.reload_guide();
    }

    // ----------------------------------------------------------------------------------------
    // The assistant's Guide tab
    // ----------------------------------------------------------------------------------------

    /// The guide as the Guide tab shows it in `width` columns: its title and status, then the
    /// outline above the current section.
    fn guide_tab_component(&self, width: u16) -> Column<'_> {
        let theme = &self.theme;
        let (title, status) = self.guide_titles();
        let header = crate::guide::panel::fit(
            &Line::from_spans(vec![Span::styled(title, theme.tiers.strong)]),
            &Line::from_spans(vec![Span::styled(status, theme.muted)]),
            usize::from(width),
        );
        let view = crate::guide::GuidePanelView::new(
            GUIDE_TAB_VIEW_ID,
            &self.guide.model,
            theme,
            true,
            &self.guide.scroll,
            &self.guide.clicks,
            &self.guide.split,
        );
        Column::new()
            .id("guide.tab.column")
            .child(TextBlock::new(Text::from_lines(vec![header])))
            .flex(Flex::new(1, view))
    }

    /// Paint the guide in `area` of the assistant's Guide tab.
    pub fn paint_guide_tab(&mut self, area: Rect, cx: &mut PaintCx<'_, '_>) {
        self.sync_guide_marks();
        self.sync_guide_model(SidePlacement::Right);
        self.guide.tab_area.set(Some(area));
        if area.width == 0 || area.height == 0 {
            return;
        }
        let component = self.guide_tab_component(area.width);
        cx.paint_component(&component, area);
    }

    /// The Guide tab was not painted in the last frame.
    pub fn guide_tab_hidden(&self) {
        self.guide.tab_area.set(None);
    }

    /// A pointer event over the guide on the Guide tab. Returns whether anything changed.
    pub fn guide_tab_mouse(&mut self, event: &Event, host: &UiContext) -> bool {
        let Some(area) = self.guide.tab_area.get() else {
            return false;
        };
        let outcome = {
            let component = self.guide_tab_component(area.width);
            let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
            let mut cx = EventCx::new(&layout);
            cx.with_transform(0, 0, i32::from(area.x), i64::from(area.y), area, |cx| {
                component.event(event, &layout, cx)
            })
        };
        let clicked = self.after_guide_event(host);
        // The tab is not a pane of the review: the review keeps the keyboard where it was.
        if self.focus == Focus::Guide
            && !self
                .panels
                .current()
                .is_some_and(|plan| plan.guide != SidePlacement::Hidden)
        {
            self.focus = Focus::Review;
        }
        clicked || outcome != EventOutcome::Ignored
    }

    /// A key for the guide on the Guide tab: the guide panel's keys, then the review's guide
    /// keys. `None` for keys the guide does not use.
    pub fn guide_tab_key(&mut self, stroke: KeyStroke, host: &UiContext) -> Option<Action> {
        if let Some(action) = self.guide_key(stroke, host) {
            return Some(action);
        }
        let action = host.keymap_action(super::SCOPE, stroke)?;
        let action = action.strip_prefix("bcode.code_review.").unwrap_or(&action);
        if action.starts_with("guide_") {
            return self.guide_action(action, host);
        }
        None
    }
}

/// Whether `status` is a writing one.
const fn writing(status: &GuideStatus) -> bool {
    matches!(
        status,
        GuideStatus::Planning
            | GuideStatus::Generating
            | GuideStatus::Synthesizing
            | GuideStatus::Verifying
    )
}

/// Mark every version of the guides of `scope` that `session` is writing stopped; returns how
/// many were marked.
///
/// # Errors
///
/// * The first call that failed.
async fn stop_writing(
    services: &ReviewServices,
    scope: &NoteScope,
    session: &str,
) -> Result<usize, String> {
    let guides = services
        .call(ListReviewGuidesRequest {
            repo_path: scope.repo_path.clone(),
            target: scope.target.clone(),
            scope: scope.scope.clone(),
            include_archived: false,
        })
        .await
        .map_err(|error| error.message)?
        .guides;
    let mut stopped = 0;
    for guide in guides {
        let versions = services
            .call(ListGuideVersionsRequest {
                repo_path: scope.repo_path.clone(),
                guide_id: guide.guide.guide_id.clone(),
            })
            .await
            .map_err(|error| error.message)?
            .versions;
        for summary in versions {
            if summary.status.as_ref().is_some_and(writing)
                && stop_version(services, scope, &summary.guide_id, summary.version, session)
                    .await?
            {
                stopped += 1;
            }
        }
    }
    Ok(stopped)
}

/// Mark version `number` of `guide_id` stopped while `session` is still writing it, re-reading
/// it after a write landed meanwhile; returns whether it was marked.
///
/// # Errors
///
/// * A call that failed, or the version still changing after [`STOP_ATTEMPTS`].
async fn stop_version(
    services: &ReviewServices,
    scope: &NoteScope,
    guide_id: &str,
    number: u32,
    session: &str,
) -> Result<bool, String> {
    let mut last = "the version kept changing".to_owned();
    for _ in 0..STOP_ATTEMPTS {
        let record = services
            .call(GetGuideVersionRequest {
                repo_path: scope.repo_path.clone(),
                guide_id: guide_id.to_owned(),
                version: Some(number),
            })
            .await
            .map_err(|error| error.message)?
            .version;
        // Another conversation may have resumed it since: then it is that conversation's.
        let Some(record) = record.filter(|record| {
            writing(&record.version.status)
                && record.version.origin.session_id.as_deref() == Some(session)
        }) else {
            return Ok(false);
        };
        let mut version = record.version;
        version.status = GuideStatus::Cancelled;
        match services
            .call(SaveGuideVersionRequest {
                repo_path: scope.repo_path.clone(),
                version,
                expected_revision: record.revision,
            })
            .await
        {
            Ok(_) => return Ok(true),
            // A write landed meanwhile: read it and try again.
            Err(error) => last = error.message,
        }
    }
    Err(last)
}
