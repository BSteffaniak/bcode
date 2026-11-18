//! A guide written by child agents, on the review screen (see `docs/code-review-guide.md`).
//!
//! * **Starting.** `f` in the guide panel (or "Guide: fan out the walkthrough" in the palette)
//!   starts a fan-out the reviewer asked for, so it is not a tool call and does not prompt; the
//!   assistant starts one with `bcode.code-review.guide_fan_out`, which Bcode's permission rules
//!   ask about unless allowed. The app plans the review, and above `confirm_above_tokens` asks
//!   the reviewer to confirm the estimate (children × tokens × the model's price) first.
//! * **Running.** The app starts the guide version (for a stack stepped through with at least
//!   three layers, a stack guide and one layer guide per layer), records the areas in the
//!   version's fan-out progress, and creates one child session of the assistant's conversation
//!   per area with the conversation's model and effort, bound to the review with an `Area`
//!   grant (`Generate` on its own layer guide), at most `max_parallel` at once; the rest wait.
//!   Children are followed like AI cards, so their tokens and permission requests show in the
//!   panel and the guide card (the feed keys answer a child's request from the guide).
//! * **Finishing.** When every area is done, the conversation is asked to synthesize the guide
//!   from the areas (reusing their citation ids); at `deep` depth a verifier child then records
//!   claim checks, and sections with unsupported claims are handed back for a refresh. Past the
//!   soft `budget_tokens`, queued areas stop and the guide is synthesized from what exists.
//! * **Stopping and resuming.** Stopping the conversation's turn (or `shift+x`) stops the
//!   conversation and every child, keeping what was written. The progress lives in the stored
//!   version, so after a stop or the app closing the panel offers "4/5 areas ready · Resume":
//!   missing areas are written again by new children, or the guide is synthesized.

use super::ReviewSurface;
use super::guide::Prefer;
use crate::action::Action;
use crate::assistant::LinkRequest;
use crate::context::UiContext;
use crate::guide::fanout::{self, Estimate, FanOutView};
use crate::host::{ClientToolReply, ContextBlock, TurnRequest};
use crate::loader::Message;
use crate::services::ReviewServices;
use bcode_code_review::assistant::guide::{DeclaredContext, GuideStart, ScopeInput};
use bcode_code_review::assistant::{ReviewRef, ReviewTools};
use bcode_code_review::guide::config::StrategyChoice;
use bcode_code_review::guide::plan::GuidePlan;
use bcode_code_review::guide::write::OutlineInput;
use bcode_code_review_models::{
    BindReviewAssistantRequest, GenerationArea, GenerationState, GetGuideVersionRequest, GrantMode,
    GuideAreaStatus, GuideDepth, GuideGrant, GuideStatus, GuideStrategy, GuideVersion,
    ListGuideAreasRequest, ReviewAssistantBinding, Section, SectionKind, SectionState,
};
use bmux_tui_components::status_bar::StatusSeverity;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// Id of the stack guide's section that links each layer guide.
const LAYERS_SECTION: &str = "layers";

/// What a fan-out is asked to do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FanOutRequest {
    /// `None` follows the plan; otherwise the split forced.
    pub strategy: Option<GuideStrategy>,
    /// How deep; the configured depth when `None`.
    pub depth: Option<GuideDepth>,
    /// The reviewer's directions.
    pub directions: Option<String>,
    /// The guide's title.
    pub title: Option<String>,
    /// Whether the reviewer started it (from the panel or the palette).
    pub user_started: bool,
}

/// A planned fan-out, before it starts.
#[derive(Debug, Clone)]
pub struct Prepared {
    request: FanOutRequest,
    plan: GuidePlan,
    estimate: Estimate,
}

/// Where synthesis stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Synthesis {
    /// Not asked for: areas are still being written.
    Waiting,
    /// Asked for with the guide message of this ticket, whose turn has not ended.
    Asked { ticket: u64 },
    /// Done.
    Done,
}

/// Where the verifier pass stands.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Verifier {
    /// None runs (or none is wanted yet).
    Idle,
    /// A verifier child is checking claims.
    Running(String),
    /// Done.
    Done,
}

/// A fan-out the app is running.
#[derive(Debug, Clone)]
pub struct FanOutRun {
    guide_id: String,
    version: u32,
    parent: String,
    strategy: GuideStrategy,
    depth: GuideDepth,
    directions: Option<String>,
    generation: GenerationState,
    status: GuideStatus,
    synthesis: Synthesis,
    verifier: Verifier,
}

/// Fan-out work that finished off the screen's thread.
#[derive(Debug)]
pub enum FanOutDone {
    /// The review was planned.
    Prepared(Result<Box<Prepared>, String>),
    /// The version was started and its areas recorded.
    Begun(Result<Box<FanOutRun>, String>),
    /// A child was created for `area_id` and its progress recorded.
    Spawned {
        /// The area.
        area_id: String,
        /// The progress after it, or why the child could not start.
        result: Result<(GenerationState, GuideStatus), String>,
    },
    /// The version's progress was written.
    Updated(Result<(GenerationState, GuideStatus), String>),
    /// The verifier child started, or why not.
    VerifierStarted(Result<String, String>),
    /// The verifier finished: the sections with unsupported claims.
    Verified(Result<Vec<String>, String>),
}

/// The fan-out state of the review screen.
#[derive(Debug, Default)]
pub(super) struct FanOutUi {
    /// The fan-out running.
    run: Option<FanOutRun>,
    /// A planned fan-out waiting for the reviewer's confirmation.
    confirm: Option<Prepared>,
    /// Fan-out tasks in flight; the next step (and Resume) waits for every one.
    busy: usize,
    /// The assistant's conversation (the children's parent), once it has one.
    assistant: Option<String>,
    /// Whether the conversation's turn runs.
    assistant_working: bool,
    /// A `guide_fan_out` call waiting for its plan.
    reply: Option<oneshot::Sender<ClientToolReply>>,
    /// Children seen working, so an idle child counts as finished only after its turn ran.
    seen_working: BTreeSet<String>,
    /// Tickets of guide messages whose turn ended, not looked at yet.
    settled: BTreeSet<u64>,
    /// Changes whenever what the fan-out shows changes.
    pub(super) revision: u64,
}

impl ReviewSurface {
    /// Tell the screen the assistant's conversation and whether its turn runs.
    pub fn set_assistant(&mut self, session: Option<&str>, working: bool) {
        let fan = &mut self.guide.fan_out;
        if fan.assistant.as_deref() != session {
            fan.assistant = session.map(str::to_owned);
        }
        fan.assistant_working = working;
    }

    /// Tell the screen the guide messages (by ticket) whose turn ended.
    pub fn guide_messages_settled(&mut self, tickets: Vec<u64>) {
        let fan = &mut self.guide.fan_out;
        fan.settled.extend(tickets);
        // Only the synthesis asked now is waited on.
        let waited = match fan.run.as_ref().map(|run| &run.synthesis) {
            Some(Synthesis::Asked { ticket }) => Some(*ticket),
            _ => None,
        };
        fan.settled.retain(|ticket| Some(*ticket) == waited);
    }

    /// The children of the fan-out running (to follow like AI cards).
    pub(super) fn fan_out_sessions(&self) -> Vec<String> {
        let Some(run) = self.guide.fan_out.run.as_ref() else {
            return Vec::new();
        };
        run.generation
            .areas
            .iter()
            .filter(|area| area.status == GuideAreaStatus::Running)
            .filter_map(|area| area.session_id.clone())
            .chain(match &run.verifier {
                Verifier::Running(session) => Some(session.clone()),
                _ => None,
            })
            .collect()
    }

    /// Whether a fan-out runs.
    #[must_use]
    pub const fn fan_out_running(&self) -> bool {
        self.guide.fan_out.run.is_some()
    }

    const fn bump_fan_out(&mut self) {
        self.guide.fan_out.revision = self.guide.fan_out.revision.wrapping_add(1);
        self.guide.state.bump();
    }

    // ----------------------------------------------------------------------------------------
    // Starting
    // ----------------------------------------------------------------------------------------

    /// `f` in the guide panel or the palette: confirm a fan-out waiting for it, resume the
    /// guide's unfinished fan-out, or start one.
    pub(super) fn guide_fan_out_key(&mut self, host: &UiContext) -> Action {
        if self.guide.fan_out.confirm.is_some() {
            self.confirm_fan_out(host);
        } else if self.fan_out_resumable() {
            self.resume_fan_out(host);
        } else if self.guide.fan_out.run.is_some() {
            self.set_status("child agents are writing the guide", StatusSeverity::Info);
        } else {
            // The reviewer asked for child agents: fan out even where one writer would do.
            self.start_fan_out(
                FanOutRequest {
                    strategy: None,
                    user_started: true,
                    ..FanOutRequest::default()
                },
                None,
                host,
            );
        }
        Action::Redraw
    }

    /// `guide_fan_out`: start a fan-out for the assistant; `reply` is answered once it starts
    /// or the reviewer is asked to confirm it.
    pub fn assistant_guide_fan_out(
        &mut self,
        arguments: &crate::assistant::actions::GuideFanOutArguments,
        reply: oneshot::Sender<ClientToolReply>,
        host: &UiContext,
    ) {
        let strategy = match arguments.strategy.as_deref().map(str::trim) {
            None | Some("" | "auto") => None,
            Some("per_layer") => Some(GuideStrategy::PerLayer),
            Some("areas") => Some(GuideStrategy::Areas),
            Some(other) => {
                let _ = reply.send(ClientToolReply::Failed {
                    code: "invalid_arguments".to_owned(),
                    message: format!("unknown strategy {other:?}; use auto, per_layer or areas"),
                });
                return;
            }
        };
        self.start_fan_out(
            FanOutRequest {
                strategy,
                depth: arguments.depth,
                directions: arguments.directions.clone(),
                title: arguments.title.clone(),
                user_started: false,
            },
            Some(reply),
            host,
        );
    }

    /// Plan a fan-out of the review (off the screen's thread).
    fn start_fan_out(
        &mut self,
        request: FanOutRequest,
        reply: Option<oneshot::Sender<ClientToolReply>>,
        host: &UiContext,
    ) {
        let refuse = |reply: Option<oneshot::Sender<ClientToolReply>>, message: &str| {
            if let Some(reply) = reply {
                let _ = reply.send(ClientToolReply::Failed {
                    code: "not_available".to_owned(),
                    message: message.to_owned(),
                });
            }
        };
        if self.guide.fan_out.run.is_some() || self.guide.fan_out.busy > 0 {
            let message = "child agents are already writing the guide";
            self.set_status(message, StatusSeverity::Info);
            refuse(reply, message);
            return;
        }
        let Some(parent) = self.guide.fan_out.assistant.clone() else {
            let message =
                "open the review assistant first: child agents are children of its conversation";
            self.set_status(message, StatusSeverity::Warning);
            refuse(reply, message);
            return;
        };
        if let Some(earlier) = self.guide.fan_out.reply.take() {
            refuse(Some(earlier), "a later fan-out replaced this one");
        }
        self.guide.fan_out.reply = reply;
        self.guide.fan_out.confirm = None;
        self.guide.fan_out.busy += 1;
        // The conversation starts and synthesizes the versions it fans out, not what an earlier
        // request granted it.
        self.set_guide_grant(None);
        let repo = self.note_scope().repo_path;
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        host.spawn(Box::pin(async move {
            let result = prepare(&services, repo, parent, request)
                .await
                .map(Box::new);
            inbox.push(Message::GuideFanOut(Box::new(FanOutDone::Prepared(result))));
        }));
        self.set_status("planning the fan-out…", StatusSeverity::Info);
    }

    /// The reviewer confirmed the fan-out waiting for it.
    fn confirm_fan_out(&mut self, host: &UiContext) {
        if let Some(prepared) = self.guide.fan_out.confirm.take() {
            self.bump_fan_out();
            self.begin_fan_out(prepared, host);
        }
    }

    /// `esc` on a fan-out waiting for confirmation: drop it. Returns whether one waited.
    pub(super) fn decline_fan_out(&mut self) -> bool {
        if self.guide.fan_out.confirm.take().is_none() {
            return false;
        }
        self.bump_fan_out();
        self.set_status("fan-out not started", StatusSeverity::Info);
        true
    }

    /// Start the version of a planned fan-out and record its areas.
    fn begin_fan_out(&mut self, prepared: Prepared, host: &UiContext) {
        let Some(parent) = self.guide.fan_out.assistant.clone() else {
            return;
        };
        self.guide.fan_out.busy += 1;
        let repo = self.note_scope().repo_path;
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        host.spawn(Box::pin(async move {
            let result = begin(&services, repo, parent, prepared).await.map(Box::new);
            inbox.push(Message::GuideFanOut(Box::new(FanOutDone::Begun(result))));
        }));
    }

    // ----------------------------------------------------------------------------------------
    // Finished work
    // ----------------------------------------------------------------------------------------

    /// Apply finished fan-out work.
    pub(super) fn fan_out_done(&mut self, done: FanOutDone, host: &UiContext) {
        self.guide.fan_out.busy = self.guide.fan_out.busy.saturating_sub(1);
        match done {
            FanOutDone::Prepared(result) => self.fan_out_prepared(result, host),
            FanOutDone::Begun(result) => match result {
                Ok(run) => {
                    let count = run.generation.areas.len();
                    self.guide.prefer = Some(Prefer::Guide(run.guide_id.clone()));
                    self.guide.view = None;
                    self.guide.reveal = true;
                    self.guide.state.engaged = true;
                    self.guide.fan_out.run = Some(*run);
                    self.reload_guide();
                    if self.panels.guide_fits() && !self.panels.guide_open {
                        self.panels.show_guide();
                        self.persist_panels();
                    }
                    self.answer_fan_out(Ok(format!(
                        "started: {count} child agents write the guide's areas; you will be asked \
                         to synthesize it when they are done"
                    )));
                    self.set_status(
                        format!("fanning out to {count} child agents"),
                        StatusSeverity::Info,
                    );
                }
                Err(error) => {
                    self.answer_fan_out(Err(error.clone()));
                    self.set_status(
                        format!("could not fan out: {error}"),
                        StatusSeverity::Warning,
                    );
                }
            },
            FanOutDone::Spawned { area_id, result } => {
                if let Err(error) = &result {
                    self.set_status(
                        format!("could not start the child for {area_id}: {error}"),
                        StatusSeverity::Warning,
                    );
                }
                self.fan_out_updated(result.map_err(|error| (area_id, error)));
            }
            FanOutDone::Updated(result) => {
                self.fan_out_updated(result.map_err(|error| (String::new(), error)));
            }
            FanOutDone::VerifierStarted(result) => {
                if let Some(run) = self.guide.fan_out.run.as_mut() {
                    match result {
                        Ok(session) => run.verifier = Verifier::Running(session),
                        Err(error) => {
                            run.verifier = Verifier::Done;
                            self.set_status(
                                format!("could not start the verifier: {error}"),
                                StatusSeverity::Warning,
                            );
                        }
                    }
                }
                self.reload_guide();
            }
            FanOutDone::Verified(result) => self.fan_out_verified(result),
        }
        self.bump_fan_out();
    }

    fn fan_out_prepared(&mut self, result: Result<Box<Prepared>, String>, host: &UiContext) {
        let prepared = match result {
            Ok(prepared) => *prepared,
            Err(error) => {
                self.answer_fan_out(Err(error.clone()));
                self.set_status(
                    format!("could not plan the fan-out: {error}"),
                    StatusSeverity::Warning,
                );
                return;
            }
        };
        if prepared.plan.areas.is_empty() {
            let message = format!(
                "the plan needs one writer, not a fan-out ({}); write the guide directly",
                prepared.plan.reason
            );
            self.answer_fan_out(Err(message.clone()));
            self.set_status(message, StatusSeverity::Info);
            if prepared.request.user_started {
                self.walk_me_through(true);
            }
            return;
        }
        if prepared.plan.estimate.needs_confirmation {
            let text = prepared.estimate.text();
            self.answer_fan_out(Ok(format!(
                "the user is asked to confirm the fan-out ({text}); it starts once they do"
            )));
            self.guide.fan_out.confirm = Some(prepared);
            if !self.panels.guide_open && self.panels.guide_fits() {
                self.panels.show_guide();
                self.persist_panels();
            }
            self.set_status(
                format!("{text}: confirm in the guide"),
                StatusSeverity::Info,
            );
            return;
        }
        self.begin_fan_out(prepared, host);
    }

    /// Answer the waiting `guide_fan_out` call, when one waits.
    fn answer_fan_out(&mut self, result: Result<String, String>) {
        if let Some(reply) = self.guide.fan_out.reply.take() {
            let _ = reply.send(match result {
                Ok(done) => {
                    ClientToolReply::Completed(serde_json::json!({"done": done}).to_string())
                }
                Err(message) => ClientToolReply::Failed {
                    code: "fan_out_failed".to_owned(),
                    message,
                },
            });
        }
    }

    fn fan_out_updated(
        &mut self,
        result: Result<(GenerationState, GuideStatus), (String, String)>,
    ) {
        let Some(run) = self.guide.fan_out.run.as_mut() else {
            return;
        };
        match result {
            Ok((generation, status)) => {
                run.generation = generation;
                run.status = status;
            }
            Err((area_id, error)) => {
                if let Some(area) = run.generation.area_mut(&area_id) {
                    area.status = GuideAreaStatus::Failed;
                    area.message = Some(error);
                } else {
                    self.set_status(
                        format!("could not record the fan-out's progress: {error}"),
                        StatusSeverity::Warning,
                    );
                }
            }
        }
        self.reload_guide();
    }

    fn fan_out_verified(&mut self, result: Result<Vec<String>, String>) {
        let Some(run) = self.guide.fan_out.run.take() else {
            return;
        };
        match result {
            Ok(sections) if sections.is_empty() => {
                self.set_status(
                    "the verifier found every claim supported",
                    StatusSeverity::Success,
                );
            }
            Ok(sections) => {
                let grant = GuideGrant {
                    guide_id: run.guide_id.clone(),
                    version: Some(run.version),
                    mode: GrantMode::Refresh {
                        section_ids: sections.clone(),
                    },
                };
                self.link.requests.push(LinkRequest::GuideMessage {
                    text: fanout::refresh_prompt(&sections),
                    grant: Some(grant),
                    context: Vec::new(),
                    ticket: None,
                });
                self.set_status(
                    format!(
                        "the verifier found unsupported claims in {} section{}: asked the \
                         assistant to fix them",
                        sections.len(),
                        if sections.len() == 1 { "" } else { "s" }
                    ),
                    StatusSeverity::Info,
                );
            }
            Err(error) => self.set_status(
                format!("could not read the verifier's checks: {error}"),
                StatusSeverity::Warning,
            ),
        }
        self.reload_guide();
    }

    // ----------------------------------------------------------------------------------------
    // Driving
    // ----------------------------------------------------------------------------------------

    /// Take the fan-out's next step: finish areas whose child ended, stop at the budget, start
    /// queued areas, synthesize, verify. One step at a time; each writes the version.
    pub(super) fn pump_fan_out(&mut self, host: &UiContext) {
        if self.guide.fan_out.busy > 0 {
            return;
        }
        let Some(run) = self.guide.fan_out.run.as_ref() else {
            return;
        };
        // Children's tokens and whether they were seen working.
        let mut live = BTreeMap::new();
        let mut finished = Vec::new();
        for area in &run.generation.areas {
            let (Some(session), GuideAreaStatus::Running) = (&area.session_id, area.status) else {
                continue;
            };
            let Some(state) = self.agents.state(session) else {
                continue;
            };
            live.insert(session.clone(), state.feed.tokens);
            if state.working {
                self.guide.fan_out.seen_working.insert(session.clone());
            } else if self.guide.fan_out.seen_working.contains(session)
                || state.phase == crate::agent::AgentPhase::Failed
            {
                finished.push((area.area_id.clone(), state.feed.tokens));
            }
        }
        let tokens_changed = run.generation.areas.iter().any(|area| {
            area.session_id
                .as_ref()
                .and_then(|session| live.get(session))
                .is_some_and(|tokens| *tokens > area.tokens)
        });
        if tokens_changed && let Some(run) = self.guide.fan_out.run.as_mut() {
            for area in &mut run.generation.areas {
                if let Some(tokens) = area
                    .session_id
                    .as_ref()
                    .and_then(|session| live.get(session))
                {
                    area.tokens = area.tokens.max(*tokens);
                }
            }
            fanout::total_tokens(&mut run.generation);
            self.bump_fan_out();
        }
        let Some(run) = self.guide.fan_out.run.clone() else {
            return;
        };
        if !finished.is_empty() {
            self.finish_areas(&run, finished, host);
            return;
        }
        if fanout::over_budget(&run.generation)
            && run
                .generation
                .areas
                .iter()
                .any(|area| area.status == GuideAreaStatus::Queued)
        {
            self.write_fan_out(&run, host, |version| {
                if let Some(generation) = version.generation.as_mut() {
                    fanout::stop_for_budget(generation);
                }
            });
            return;
        }
        if let Some(area_id) = fanout::to_start(&run.generation).into_iter().next() {
            self.spawn_area(&run, &area_id, host);
            return;
        }
        if !run.generation.all_terminal() {
            return;
        }
        match run.synthesis {
            Synthesis::Waiting => self.ask_synthesis(&run, host),
            Synthesis::Asked { ticket } => {
                // Only the synthesis message's own turn ending counts: another turn of the
                // conversation (the reviewer's) may end first.
                if self.guide.fan_out.settled.remove(&ticket) || run.status == GuideStatus::Complete
                {
                    self.synthesized(&run, host);
                }
            }
            Synthesis::Done => self.after_synthesis(&run, host),
        }
    }

    /// Record the areas whose child ended: written (its area row, or its layer guide) or not.
    fn finish_areas(&mut self, run: &FanOutRun, finished: Vec<(String, u64)>, host: &UiContext) {
        for (area_id, _) in &finished {
            if let Some(area) = run.generation.area(area_id)
                && let Some(session) = &area.session_id
            {
                self.guide.fan_out.seen_working.remove(session);
            }
        }
        self.guide.fan_out.busy += 1;
        let repo = self.note_scope().repo_path;
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        let (guide_id, version) = (run.guide_id.clone(), run.version);
        let areas = run.generation.areas.clone();
        host.spawn(Box::pin(async move {
            let result = async {
                let outcomes =
                    area_outcomes(&services, &repo, &guide_id, version, &areas, &finished).await?;
                update(&services, repo, guide_id, version, move |stored| {
                    let Some(generation) = stored.generation.as_mut() else {
                        return;
                    };
                    for (area_id, (status, tokens, message)) in &outcomes {
                        if let Some(area) = generation.area_mut(area_id)
                            && !area.status.is_terminal()
                        {
                            area.status = *status;
                            area.tokens = area.tokens.max(*tokens);
                            area.message.clone_from(message);
                        }
                    }
                    fanout::total_tokens(generation);
                })
                .await
            }
            .await;
            inbox.push(Message::GuideFanOut(Box::new(FanOutDone::Updated(result))));
        }));
    }

    /// Write `change` to the run's version.
    fn write_fan_out(
        &mut self,
        run: &FanOutRun,
        host: &UiContext,
        change: impl FnMut(&mut GuideVersion) + Send + 'static,
    ) {
        self.guide.fan_out.busy += 1;
        let repo = self.note_scope().repo_path;
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        let (guide_id, version) = (run.guide_id.clone(), run.version);
        host.spawn(Box::pin(async move {
            let result = update(&services, repo, guide_id, version, change).await;
            inbox.push(Message::GuideFanOut(Box::new(FanOutDone::Updated(result))));
        }));
    }

    /// The review's binding for a child, with `grant`.
    fn child_binding(&mut self, grant: GuideGrant) -> ReviewAssistantBinding {
        let mut binding = self.assistant_binding(String::new());
        binding.guide_grant = Some(grant);
        binding
    }

    /// Create the child of `area_id` and record it running.
    fn spawn_area(&mut self, run: &FanOutRun, area_id: &str, host: &UiContext) {
        let Some(area) = run.generation.area(area_id).cloned() else {
            return;
        };
        let depth = depth_name(run.depth);
        let (grant, prompt) = match (&area.guide_id, run.strategy) {
            (Some(layer_guide), GuideStrategy::PerLayer) => (
                GuideGrant {
                    guide_id: layer_guide.clone(),
                    version: None,
                    mode: GrantMode::Generate,
                },
                fanout::layer_prompt(layer_guide, &area, run.directions.as_deref(), depth),
            ),
            _ => (
                GuideGrant {
                    guide_id: run.guide_id.clone(),
                    version: Some(run.version),
                    mode: GrantMode::Area {
                        area_id: area.area_id.clone(),
                    },
                },
                fanout::area_prompt(
                    &run.guide_id,
                    run.version,
                    &area,
                    run.directions.as_deref(),
                    depth,
                ),
            ),
        };
        let binding = self.child_binding(grant);
        self.guide.fan_out.busy += 1;
        let repo = self.note_scope().repo_path;
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        let (guide_id, version, parent) = (run.guide_id.clone(), run.version, run.parent.clone());
        let title = format!("Guide: {}", area.title);
        let area_id = area.area_id;
        host.spawn(Box::pin(async move {
            let result = async {
                let session = spawn_child(
                    &services,
                    &repo,
                    &parent,
                    binding,
                    (
                        title,
                        prompt,
                        crate::assistant::profile::GUIDE_WRITER_PROFILE_ID,
                    ),
                )
                .await;
                let recorded_area = area_id.clone();
                let child = session.as_ref().ok().cloned();
                let (status, session, message) = match session {
                    Ok(session) => (GuideAreaStatus::Running, Some(session), None),
                    Err(error) => (GuideAreaStatus::Failed, None, Some(error)),
                };
                let stopped = Arc::new(AtomicBool::new(false));
                let seen = Arc::clone(&stopped);
                let updated = update(&services, repo, guide_id, version, move |stored| {
                    let terminal = stored.status.is_terminal();
                    if let Some(area) = stored
                        .generation
                        .as_mut()
                        .and_then(|generation| generation.area_mut(&recorded_area))
                    {
                        // Stopped while the child was being created: it is not recorded
                        // running, and its turn is cancelled below.
                        if terminal || area.status != GuideAreaStatus::Queued {
                            seen.store(true, Ordering::Relaxed);
                            return;
                        }
                        area.status = status;
                        area.session_id.clone_from(&session);
                        area.message.clone_from(&message);
                    }
                })
                .await;
                if stopped.load(Ordering::Relaxed)
                    && let Some(child) = child
                {
                    drop(services.host().cancel_turn(child).await);
                }
                updated
            }
            .await;
            inbox.push(Message::GuideFanOut(Box::new(FanOutDone::Spawned {
                area_id,
                result,
            })));
        }));
    }

    /// Every area is done: link the layer guides (a stack guide), mark the version
    /// synthesizing, and ask the conversation to write the guide from the areas.
    fn ask_synthesis(&mut self, run: &FanOutRun, host: &UiContext) {
        let ticket = crate::assistant::next_guide_ticket();
        if let Some(stored) = self.guide.fan_out.run.as_mut() {
            stored.synthesis = Synthesis::Asked { ticket };
        }
        let layers: Vec<GenerationArea> = if run.strategy == GuideStrategy::PerLayer {
            run.generation
                .areas
                .iter()
                .filter(|area| area.status == GuideAreaStatus::Complete)
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        self.write_fan_out(run, host, move |version| {
            version.status = GuideStatus::Synthesizing;
            if !layers.is_empty() {
                link_layers(version, &layers);
            }
        });
        let grant = GuideGrant {
            guide_id: run.guide_id.clone(),
            version: Some(run.version),
            mode: GrantMode::Generate,
        };
        self.link.requests.push(LinkRequest::GuideMessage {
            text: fanout::synthesis_prompt(
                &run.guide_id,
                run.version,
                run.strategy,
                &run.generation,
            ),
            grant: Some(grant),
            context: Vec::<ContextBlock>::new(),
            ticket: Some(ticket),
        });
        self.set_status(
            "every area is written: asked the assistant to synthesize the guide",
            StatusSeverity::Info,
        );
    }

    /// The conversation's synthesis turn ended: mark the version complete (when it did not).
    fn synthesized(&mut self, run: &FanOutRun, host: &UiContext) {
        if let Some(stored) = self.guide.fan_out.run.as_mut() {
            stored.synthesis = Synthesis::Done;
        }
        if !run.status.is_terminal() {
            self.write_fan_out(run, host, |version| {
                if !version.status.is_terminal() {
                    version.status = GuideStatus::Complete;
                }
            });
        }
    }

    /// After synthesis: run the verifier when wanted, else the fan-out is done.
    fn after_synthesis(&mut self, run: &FanOutRun, host: &UiContext) {
        match &run.verifier {
            Verifier::Idle if run.generation.verify && run.status == GuideStatus::Complete => {
                self.spawn_verifier(run, host);
            }
            Verifier::Running(session) => {
                let session = session.clone();
                let Some(state) = self.agents.state(&session) else {
                    return;
                };
                if state.working {
                    self.guide.fan_out.seen_working.insert(session);
                } else if self.guide.fan_out.seen_working.remove(&session)
                    || state.phase == crate::agent::AgentPhase::Failed
                {
                    self.read_checks(run, host);
                }
            }
            Verifier::Idle if run.generation.verify && !run.status.is_terminal() => {}
            _ => {
                self.guide.fan_out.run = None;
                self.bump_fan_out();
                self.set_status("child agents finished the guide", StatusSeverity::Success);
            }
        }
    }

    /// Start the verifier child, bound with `Verify`.
    fn spawn_verifier(&mut self, run: &FanOutRun, host: &UiContext) {
        let binding = self.child_binding(GuideGrant {
            guide_id: run.guide_id.clone(),
            version: Some(run.version),
            mode: GrantMode::Verify,
        });
        self.guide.fan_out.busy += 1;
        let repo = self.note_scope().repo_path;
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        let (guide_id, version, parent) = (run.guide_id.clone(), run.version, run.parent.clone());
        host.spawn(Box::pin(async move {
            let prompt = fanout::verifier_prompt(&guide_id, version);
            let result = spawn_child(
                &services,
                &repo,
                &parent,
                binding,
                (
                    "Guide: verify claims".to_owned(),
                    prompt,
                    crate::assistant::profile::GUIDE_VERIFIER_PROFILE_ID,
                ),
            )
            .await;
            if let Ok(session) = &result {
                let session = session.clone();
                drop(
                    update(&services, repo, guide_id, version, move |stored| {
                        if let Some(generation) = stored.generation.as_mut() {
                            generation.verifier_session = Some(session.clone());
                        }
                    })
                    .await,
                );
            }
            inbox.push(Message::GuideFanOut(Box::new(FanOutDone::VerifierStarted(
                result,
            ))));
        }));
    }

    /// The verifier ended: read which sections have unsupported claims.
    fn read_checks(&mut self, run: &FanOutRun, host: &UiContext) {
        self.guide.fan_out.busy += 1;
        let repo = self.note_scope().repo_path;
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        let (guide_id, version) = (run.guide_id.clone(), run.version);
        host.spawn(Box::pin(async move {
            let result = services
                .call(GetGuideVersionRequest {
                    repo_path: repo,
                    guide_id,
                    version: Some(version),
                })
                .await
                .map_err(|error| error.message)
                .map(|response| {
                    response
                        .version
                        .map(|record| fanout::unsupported_sections(&record.version))
                        .unwrap_or_default()
                });
            inbox.push(Message::GuideFanOut(Box::new(FanOutDone::Verified(result))));
        }));
    }

    // ----------------------------------------------------------------------------------------
    // Stopping and resuming
    // ----------------------------------------------------------------------------------------

    /// Stop the fan-out: the conversation's turn and every child, keeping what was written.
    pub(super) fn stop_fan_out(&mut self, host: &UiContext) -> Action {
        let Some(run) = self.guide.fan_out.run.take() else {
            self.set_status("no fan-out is running", StatusSeverity::Info);
            return Action::Redraw;
        };
        let mut sessions: Vec<String> = run
            .generation
            .areas
            .iter()
            .filter(|area| area.status == GuideAreaStatus::Running)
            .filter_map(|area| area.session_id.clone())
            .collect();
        if let Verifier::Running(session) = &run.verifier {
            sessions.push(session.clone());
        }
        sessions.push(run.parent.clone());
        let repo = self.note_scope().repo_path;
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        self.guide.fan_out.busy += 1;
        host.spawn(Box::pin(async move {
            // Children recorded running by a spawn that finished after the run was read are
            // stopped too.
            let recorded = Arc::new(Mutex::new(Vec::new()));
            let found = Arc::clone(&recorded);
            let result = update(&services, repo, run.guide_id, run.version, move |version| {
                if let Some(generation) = version.generation.as_mut() {
                    if let Ok(mut found) = found.lock() {
                        found.extend(
                            generation
                                .areas
                                .iter()
                                .filter(|area| area.status == GuideAreaStatus::Running)
                                .filter_map(|area| area.session_id.clone()),
                        );
                    }
                    fanout::cancel(generation);
                }
                if !version.status.is_terminal() {
                    version.status = GuideStatus::Cancelled;
                }
            })
            .await;
            if let Ok(found) = recorded.lock() {
                for session in found.iter() {
                    if !sessions.contains(session) {
                        sessions.push(session.clone());
                    }
                }
            }
            for session in sessions {
                drop(services.host().cancel_turn(session).await);
            }
            inbox.push(Message::GuideFanOut(Box::new(FanOutDone::Updated(result))));
        }));
        self.bump_fan_out();
        self.set_status(
            "stopped the fan-out and its child agents; written areas stay",
            StatusSeverity::Info,
        );
        Action::Redraw
    }

    /// The assistant's turn in `session` was stopped: stop the fan-out it runs too.
    pub(super) fn fan_out_turn_stopped(&mut self, session: &str, host: &UiContext) {
        if self
            .guide
            .fan_out
            .run
            .as_ref()
            .is_some_and(|run| run.parent == session)
        {
            self.stop_fan_out(host);
        }
    }

    /// Whether the guide on screen is a fan-out this app is not running that did not finish.
    pub(super) fn fan_out_resumable(&self) -> bool {
        self.guide.fan_out.run.is_none()
            && self.guide.state.active.as_ref().is_some_and(|active| {
                !active.is_old_version() && fanout::resumable(&active.version)
            })
    }

    /// Resume the guide's unfinished fan-out: missing areas get new children, or synthesis runs.
    fn resume_fan_out(&mut self, host: &UiContext) {
        if self.guide.fan_out.busy > 0 {
            self.set_status(
                "the fan-out is still settling; resume it in a moment",
                StatusSeverity::Info,
            );
            return;
        }
        let Some(active) = self.guide.state.active.as_ref() else {
            return;
        };
        let Some(parent) = self.guide.fan_out.assistant.clone() else {
            self.set_status(
                "open the review assistant first: child agents are children of its conversation",
                StatusSeverity::Warning,
            );
            return;
        };
        let version = &active.version;
        let mut generation = version.generation.clone().unwrap_or_default();
        // Children recorded running may still run (the screen was rebuilt while they did):
        // they are stopped before new children write their areas, so no area has two writers.
        let orphans: Vec<String> = generation
            .areas
            .iter()
            .filter(|area| area.status == GuideAreaStatus::Running)
            .filter_map(|area| area.session_id.clone())
            .collect();
        fanout::requeue(&mut generation);
        let strategy = version.origin.strategy.unwrap_or(GuideStrategy::Areas);
        let run = FanOutRun {
            guide_id: active.guide.guide_id.clone(),
            version: version.version,
            parent,
            strategy,
            depth: version.origin.depth,
            directions: version.origin.directions.clone(),
            generation: generation.clone(),
            status: GuideStatus::Generating,
            synthesis: Synthesis::Waiting,
            verifier: Verifier::Idle,
        };
        self.guide.fan_out.busy += 1;
        let repo = self.note_scope().repo_path;
        let (services, inbox) = (self.services.clone(), self.inbox.clone());
        let (guide_id, number) = (run.guide_id.clone(), run.version);
        host.spawn(Box::pin(async move {
            for session in orphans {
                drop(services.host().cancel_turn(session).await);
            }
            let result = update(&services, repo, guide_id, number, move |stored| {
                stored.generation = Some(generation.clone());
                stored.status = GuideStatus::Generating;
            })
            .await;
            inbox.push(Message::GuideFanOut(Box::new(FanOutDone::Updated(result))));
        }));
        self.guide.fan_out.run = Some(run);
        self.bump_fan_out();
        self.set_status("resuming the fan-out", StatusSeverity::Info);
    }

    // ----------------------------------------------------------------------------------------
    // What shows
    // ----------------------------------------------------------------------------------------

    /// The fan-out as the panel and the card show it: the one running, one waiting for
    /// confirmation, or the guide's unfinished one to resume.
    pub(super) fn fan_out_view(&self) -> Option<FanOutView> {
        let fan = &self.guide.fan_out;
        let key = |action: &str| self.action_keys.get(action).cloned().unwrap_or_default();
        if let Some(prepared) = &fan.confirm {
            let confirm = key("guide_fan_out");
            return Some(FanOutView {
                confirm: Some(format!(
                    "{} · {} start · esc cancel",
                    prepared.estimate.text(),
                    if confirm.is_empty() {
                        "enter".to_owned()
                    } else {
                        confirm
                    },
                )),
                ..FanOutView::default()
            });
        }
        let live = |generation: &GenerationState| {
            generation
                .areas
                .iter()
                .filter_map(|area| {
                    let session = area.session_id.as_ref()?;
                    Some((session.clone(), self.agents.state(session)?.feed.tokens))
                })
                .collect::<BTreeMap<_, _>>()
        };
        if let Some(run) = &fan.run {
            let mut view = FanOutView::of(&run.generation, &live(&run.generation));
            view.phase = match (&run.synthesis, &run.verifier) {
                (_, Verifier::Running(_)) => Some("checking claims".to_owned()),
                (Synthesis::Asked { .. }, _) => Some("combining areas".to_owned()),
                _ => run.generation.note.clone(),
            };
            view.permission = self
                .fan_out_permission()
                .map(|(area, tool)| format!("{area} wants {tool}"));
            return Some(view);
        }
        let active = self.guide.state.active.as_ref()?;
        let generation = active.version.generation.as_ref()?;
        let mut view = FanOutView::of(generation, &BTreeMap::new());
        if self.fan_out_resumable() {
            view.resume = Some(fanout::resume_label(generation, &key("guide_fan_out")));
        }
        Some(view)
    }

    /// The panel's fan-out rows.
    pub(super) fn fan_out_rows(&self) -> Vec<(bmux_tui::text::Line, bmux_tui::text::Line)> {
        let Some(view) = self.fan_out_view() else {
            return Vec::new();
        };
        let keys = self.agents.keys();
        let feed: Vec<&str> = [&keys.allow, &keys.always, &keys.decline, &keys.never]
            .into_iter()
            .map(String::as_str)
            .filter(|key| !key.is_empty())
            .collect();
        fanout::progress_rows(&view, &self.theme, &feed.join("/"))
    }

    /// The first child waiting for a permission answer: its area's title and the tool.
    fn fan_out_permission(&self) -> Option<(String, String)> {
        let run = self.guide.fan_out.run.as_ref()?;
        run.generation.areas.iter().find_map(|area| {
            let session = area.session_id.as_ref()?;
            let permission = self.agents.state(session)?.feed.permission.as_ref()?;
            Some((area.title.clone(), permission.tool.clone()))
        })
    }

    /// A feed key (allow, always, decline, never) answering a child's permission request from
    /// the guide. `None` when no child asks or the key is not a feed key.
    pub(super) fn fan_out_permission_key(
        &mut self,
        stroke: bmux_keyboard::KeyStroke,
        host: &UiContext,
    ) -> Option<Action> {
        let run = self.guide.fan_out.run.as_ref()?;
        let session = run.generation.areas.iter().find_map(|area| {
            let session = area.session_id.as_ref()?;
            self.agents
                .state(session)?
                .feed
                .permission
                .as_ref()
                .filter(|permission| !self.agents.is_answering(&permission.permission_id))
                .map(|_| session.clone())
        })?;
        let action = host.keymap_action(super::FEED_SCOPE, stroke)?;
        let action = match action.strip_prefix("bcode.code_review.").unwrap_or(&action) {
            "feed_allow" => crate::rows::AgentAction::Allow,
            "feed_always" => crate::rows::AgentAction::Always,
            "feed_decline" => crate::rows::AgentAction::Decline,
            "feed_never" => crate::rows::AgentAction::Never,
            _ => return None,
        };
        self.answer_permission(&session, action);
        Some(Action::Redraw)
    }
}

/// The depth's name, as prompts say it.
const fn depth_name(depth: GuideDepth) -> &'static str {
    match depth {
        GuideDepth::Brief => "brief",
        GuideDepth::Standard => "standard",
        GuideDepth::Deep => "deep",
    }
}

/// Plan a fan-out of the review `parent` is bound to, and estimate its cost on `parent`'s model.
async fn prepare(
    services: &ReviewServices,
    repo: PathBuf,
    parent: String,
    request: FanOutRequest,
) -> Result<Prepared, String> {
    let host = services.host();
    let model = host.session_model(parent.clone()).await.ok();
    let price = match &model {
        Some(model) => host.model_catalog().await.ok().and_then(|catalog| {
            catalog
                .find(model.model.provider.as_deref(), &model.model.model_id)
                .and_then(|option| option.price.clone())
                .or_else(|| model.model.price.clone())
        }),
        None => None,
    };
    let data = services.data();
    let store = services.store().clone();
    let config = services.guide_config();
    let strategy = request.strategy;
    let depth = request.depth;
    let user_started = request.user_started;
    let mut plan = services
        .blocking(move || {
            let mut tools = ReviewTools::new(&data, &store);
            let review = tools
                .open(&repo, &ReviewRef::Session(parent))
                .map_err(|error| failure(&error.to_string()))?;
            let choice = if strategy.is_some() || user_started {
                StrategyChoice::FanOut
            } else {
                StrategyChoice::Auto
            };
            tools
                .guide_plan_of(&review, &config, Some(choice), depth)
                .map_err(|error| failure(&error.to_string()))
        })
        .await
        .map_err(|error| error.message)?;
    if let Some(forced) = request.strategy
        && plan.strategy != forced
        && forced == GuideStrategy::Areas
    {
        plan.areas.retain(|area| area.kind != "layer");
    }
    let estimate = Estimate {
        children: plan.estimate.children,
        tokens: plan.estimate.estimated_tokens,
        cost: price
            .as_ref()
            .and_then(|(input, _)| fanout::cost_of(plan.estimate.estimated_tokens, input)),
        model: model
            .map(|model| model.model.display_name)
            .unwrap_or_default(),
    };
    Ok(Prepared {
        request,
        plan,
        estimate,
    })
}

fn failure(message: &str) -> bcode_code_review::provider::ProviderCallError {
    bcode_code_review::provider::ProviderCallError::new(
        bcode_code_review_models::REVIEW_SOURCE_ERROR_FAILED,
        message,
    )
}

/// The planned outline of a guide written from areas: the core sections.
fn core_outline() -> Vec<OutlineInput> {
    [
        ("s1", "purpose", "Purpose"),
        ("s2", "scope", "Scope"),
        ("s3", "reading_order", "Reading order"),
    ]
    .into_iter()
    .map(|(id, kind, title)| OutlineInput {
        id: id.to_owned(),
        kind: Some(kind.to_owned()),
        key: None,
        title: Some(title.to_owned()),
        children: Vec::new(),
    })
    .collect()
}

/// Start the fan-out's version (and the layer guides of a fan-out by layer), and record its
/// areas queued.
async fn begin(
    services: &ReviewServices,
    repo: PathBuf,
    parent: String,
    prepared: Prepared,
) -> Result<FanOutRun, String> {
    let data = services.data();
    let store = services.store().clone();
    let config = services.guide_config();
    let Prepared { request, plan, .. } = prepared;
    let strategy = plan.strategy;
    let depth = plan.depth;
    let directions = request.directions.clone();
    let session = parent.clone();
    let budget = plan.estimate.budget_tokens;
    let max_parallel = plan.max_parallel;
    let verify = plan.verifier;
    let written_in = repo.clone();
    let title = request.title.clone();
    let (guide_id, version, areas) = services
        .blocking(move || {
            start_versions(
                &data,
                &store,
                &config,
                (&repo, &session),
                (title, directions, depth, strategy),
                &plan,
            )
        })
        .await
        .map_err(|error| error.message)?;
    let generation = GenerationState {
        areas,
        tokens_used: 0,
        budget_tokens: Some(budget),
        max_parallel: Some(max_parallel),
        verifier_session: None,
        verify,
        note: None,
    };
    let recorded = generation.clone();
    let (generation, status) = update(
        services,
        written_in,
        guide_id.clone(),
        version,
        move |stored| {
            stored.generation = Some(recorded.clone());
            stored.status = GuideStatus::Generating;
        },
    )
    .await?;
    Ok(FanOutRun {
        guide_id,
        version,
        parent,
        strategy,
        depth,
        directions: request.directions,
        generation,
        status,
        synthesis: Synthesis::Waiting,
        verifier: Verifier::Idle,
    })
}

/// Start a fan-out's version as `session` (and, by layer, one layer guide per planned layer,
/// under the stack guide); the guide, its version, and the areas to record queued.
fn start_versions(
    data: &bcode_code_review::ReviewData,
    store: &bcode_code_review::ReviewStore,
    config: &bcode_code_review::guide::config::GuideConfig,
    (repo, session): (&std::path::Path, &str),
    (title, directions, depth, strategy): (
        Option<String>,
        Option<String>,
        GuideDepth,
        GuideStrategy,
    ),
    plan: &GuidePlan,
) -> Result<(String, u32, Vec<GenerationArea>), bcode_code_review::provider::ProviderCallError> {
    let fail = |error: bcode_code_review::assistant::AssistantError| failure(&error.to_string());
    let mut tools = ReviewTools::new(data, store);
    let mut review = tools
        .open(repo, &ReviewRef::Session(session.to_owned()))
        .map_err(fail)?;
    review.guide_grant = None;
    let start = |scope: Option<ScopeInput>,
                 title: Option<String>,
                 outline: Vec<OutlineInput>,
                 parent_guide_id: Option<String>| GuideStart {
        scope,
        title,
        depth: Some(depth),
        strategy: Some(strategy),
        directions: directions.clone(),
        outline,
        parent_guide_id,
        ..GuideStart::default()
    };
    let per_layer = strategy == GuideStrategy::PerLayer;
    let started = tools
        .guide_start(
            &review,
            Some(session),
            &start(
                per_layer.then(|| ScopeInput {
                    kind: Some("stack".to_owned()),
                    layer: None,
                    baseline: None,
                }),
                title,
                core_outline(),
                None,
            ),
            config,
            &DeclaredContext::default(),
        )
        .map_err(fail)?;
    let guide_id = started["guide_id"].as_str().unwrap_or_default().to_owned();
    let version = started["version"]
        .as_u64()
        .and_then(|version| u32::try_from(version).ok())
        .unwrap_or(1);
    let mut areas = Vec::new();
    for planned in &plan.areas {
        let layer_guide = if per_layer {
            let layer = planned
                .layer
                .clone()
                .unwrap_or_else(|| planned.area_id.clone());
            let started = tools
                .guide_start(
                    &review,
                    Some(session),
                    &start(
                        Some(ScopeInput {
                            kind: Some("layer".to_owned()),
                            layer: Some(layer),
                            baseline: None,
                        }),
                        Some(planned.title.clone()),
                        Vec::new(),
                        Some(guide_id.clone()),
                    ),
                    config,
                    &DeclaredContext::default(),
                )
                .map_err(fail)?;
            started["guide_id"].as_str().map(str::to_owned)
        } else {
            None
        };
        areas.push(GenerationArea {
            area_id: planned.area_id.clone(),
            title: planned.title.clone(),
            session_id: None,
            status: GuideAreaStatus::Queued,
            paths: planned.paths.clone(),
            layer: planned.layer.clone(),
            guide_id: layer_guide,
            tokens: 0,
            message: None,
        });
    }
    Ok((guide_id, version, areas))
}

/// Apply `change` to version `version` of `guide_id` in one store turn; the progress and status
/// after it.
async fn update(
    services: &ReviewServices,
    repo: PathBuf,
    guide_id: String,
    version: u32,
    mut change: impl FnMut(&mut GuideVersion) + Send + 'static,
) -> Result<(GenerationState, GuideStatus), String> {
    let store = services.store().clone();
    services
        .blocking(move || {
            store
                .update_guide_version(
                    &repo,
                    guide_id,
                    Some(version),
                    None,
                    move |stored: &mut GuideVersion| -> Result<(GenerationState, GuideStatus), ()> {
                        change(stored);
                        Ok((
                            stored.generation.clone().unwrap_or_default(),
                            stored.status.clone(),
                        ))
                    },
                )
                .map_err(|error| failure(&error.to_string()))?
                .map(|(_, after)| after)
                .map_err(|()| failure("the version could not be changed"))
        })
        .await
        .map_err(|error| error.message)
}

/// How each finished area ended: written (its area row complete, or its layer guide written)
/// or not, with its tokens.
async fn area_outcomes(
    services: &ReviewServices,
    repo: &std::path::Path,
    guide_id: &str,
    version: u32,
    areas: &[GenerationArea],
    finished: &[(String, u64)],
) -> Result<BTreeMap<String, (GuideAreaStatus, u64, Option<String>)>, String> {
    let rows = services
        .call(ListGuideAreasRequest {
            repo_path: repo.to_path_buf(),
            guide_id: guide_id.to_owned(),
            version,
        })
        .await
        .map_err(|error| error.message)?
        .areas;
    let mut outcomes = BTreeMap::new();
    for (area_id, tokens) in finished {
        let Some(area) = areas.iter().find(|area| &area.area_id == area_id) else {
            continue;
        };
        let written = if let Some(layer_guide) = &area.guide_id {
            services
                .call(GetGuideVersionRequest {
                    repo_path: repo.to_path_buf(),
                    guide_id: layer_guide.clone(),
                    version: None,
                })
                .await
                .map_err(|error| error.message)?
                .version
                .map_or_else(
                    || Err("the layer guide is gone".to_owned()),
                    |record| {
                        if record.version.status == GuideStatus::Complete
                            || record
                                .version
                                .outline
                                .iter()
                                .any(|section| section.state == SectionState::Written)
                        {
                            Ok(())
                        } else {
                            Err("the child ended without writing its layer guide".to_owned())
                        }
                    },
                )
        } else {
            match rows.iter().find(|row| &row.area.area_id == area_id) {
                Some(row) if row.area.status == GuideAreaStatus::Complete => Ok(()),
                Some(row) if row.area.status == GuideAreaStatus::Failed => Err(row
                    .area
                    .message
                    .clone()
                    .unwrap_or_else(|| "the child reported the area failed".to_owned())),
                _ => Err("the child ended without writing its area".to_owned()),
            }
        };
        outcomes.insert(
            area_id.clone(),
            match written {
                Ok(()) => (GuideAreaStatus::Complete, *tokens, None),
                Err(message) => (GuideAreaStatus::Failed, *tokens, Some(message)),
            },
        );
    }
    Ok(outcomes)
}

/// Create a child of `parent` with `parent`'s model and effort, bind it to the review with
/// `binding`, and send it `prompt` under `profile`; returns the child's id.
async fn spawn_child(
    services: &ReviewServices,
    repo: &std::path::Path,
    parent: &str,
    binding: ReviewAssistantBinding,
    (title, prompt, profile): (String, String, &str),
) -> Result<String, String> {
    let host = services.host();
    let model = host.session_model(parent.to_owned()).await.ok();
    let child = host
        .create_child_session(parent.to_owned(), title, repo.to_path_buf())
        .await
        .map_err(|error| error.message)?;
    crate::bypass::start_session(services, &child, crate::bypass::AgentKind::Guide).await;
    if let Some(model) = model {
        let id = model
            .requested
            .clone()
            .unwrap_or_else(|| model.model.model_id.clone());
        if !id.is_empty() {
            host.set_session_model(child.clone(), model.model.provider.clone(), id)
                .await
                .map_err(|error| error.message)?;
        }
        if model.effort.is_some() {
            host.set_session_reasoning(child.clone(), model.effort)
                .await
                .map_err(|error| error.message)?;
        }
    }
    services
        .call(BindReviewAssistantRequest {
            repo_path: repo.to_path_buf(),
            binding: ReviewAssistantBinding {
                session_id: child.clone(),
                ..binding
            },
        })
        .await
        .map_err(|error| error.message)?;
    host.submit_turn(
        child.clone(),
        TurnRequest {
            text: prompt,
            agent: Some(profile.to_owned()),
            ..TurnRequest::default()
        },
    )
    .await
    .map_err(|error| error.message)?;
    Ok(child)
}

/// Add the stack guide's Layers section: one link per written layer guide (kept when present).
fn link_layers(version: &mut GuideVersion, layers: &[GenerationArea]) {
    if version.section(LAYERS_SECTION).is_some()
        || version
            .retired_sections
            .iter()
            .any(|id| id == LAYERS_SECTION)
    {
        return;
    }
    let children = layers
        .iter()
        .enumerate()
        .filter_map(|(index, area)| {
            Some(Section {
                id: format!("{LAYERS_SECTION}.{}", index + 1),
                kind: SectionKind::LayerLink {
                    guide_id: area.guide_id.clone()?,
                    source_id: area.layer.clone().unwrap_or_else(|| area.area_id.clone()),
                },
                title: area.title.clone(),
                body: "This layer has its own guide: open it here, or step to the layer."
                    .to_owned(),
                findings: Vec::new(),
                citations: Vec::new(),
                children: Vec::new(),
                state: SectionState::Written,
                refreshed_from: None,
                checks: Vec::new(),
            })
        })
        .collect();
    version.outline.push(Section {
        id: LAYERS_SECTION.to_owned(),
        kind: SectionKind::Note,
        title: "Layers".to_owned(),
        body: "Each layer's own guide, bottom first.".to_owned(),
        findings: Vec::new(),
        citations: Vec::new(),
        children,
        state: SectionState::Written,
        refreshed_from: None,
        checks: Vec::new(),
    });
}

impl super::guide::GuideUi {
    /// Whether a planned fan-out waits for the reviewer's confirmation.
    pub(super) const fn fan_out_confirming(&self) -> bool {
        self.fan_out.confirm.is_some()
    }
}

impl ReviewSurface {
    /// `enter` on a stack guide's layer link: show that layer's guide. Returns whether the
    /// current section is one.
    pub(super) fn open_layer_link(&mut self) -> bool {
        let Some(guide_id) = self
            .guide
            .state
            .active
            .as_ref()
            .and_then(|active| active.current())
            .and_then(|entry| match &entry.kind {
                SectionKind::LayerLink { guide_id, .. } => Some(guide_id.clone()),
                _ => None,
            })
        else {
            return false;
        };
        self.guide.prefer = Some(Prefer::Guide(guide_id));
        self.guide.view = None;
        self.reload_guide();
        self.set_status("showing the layer's guide", StatusSeverity::Info);
        true
    }

    /// While stepping through a stack's layers (`guide.follow_layer`), show the guide of the
    /// layer stepped to when it has one, and its stack guide when it has none.
    pub(super) fn follow_layer_guide(&mut self) {
        if !self.services.guide_config().follow_layer
            || self.shown_stack_view() != Some(super::stack::StackView::OneLayer)
        {
            return;
        }
        let Some(source) = self.cursor_source() else {
            return;
        };
        if self.guide.followed_layer.as_deref() == Some(source.as_str()) {
            return;
        }
        self.guide.followed_layer = Some(source.clone());
        let Some(active) = self.guide.state.active.as_ref() else {
            return;
        };
        let layer_guide = self.guide.state.guides.iter().find(|record| {
            record.guide.archived_at_ms.is_none()
                && matches!(&record.guide.scope,
                    bcode_code_review_models::GuideScope::Layer { source_id } if *source_id == source)
        });
        // Off a layer guide onto a layer without one: back to its stack guide.
        let target = layer_guide.map_or_else(
            || {
                active.guide.parent_guide_id.clone().filter(|_| {
                    matches!(
                        active.guide.scope,
                        bcode_code_review_models::GuideScope::Layer { .. }
                    )
                })
            },
            |record| Some(record.guide.guide_id.clone()),
        );
        if let Some(target) = target
            && target != active.guide.guide_id
        {
            self.guide.prefer = Some(Prefer::Guide(target));
            self.guide.view = None;
            self.reload_guide();
        }
    }
}
