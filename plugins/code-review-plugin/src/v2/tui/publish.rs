//! Publishing from the review (spec 7.3 and A.10): a modal flow over BMUX `picker_frame`,
//! `menu`, `form`/`form_field`, and `text_view`.
//!
//! 1. **Checklist**: readiness (viewed files, unviewed and open threads with the first three
//!    of each, resolved threads, drafts) and every thread with an include toggle (`space`).
//! 2. **Publisher**: built-in publishers (`review.publishers.list`) and every external
//!    `bcode.review_publisher/v1` publisher, with capability labels. A publisher of the
//!    review's pull-request provider is preselected, else `markdown_file`.
//! 3. **Options**: the publisher's options schema as a form. When the publisher publishes the
//!    review's pull requests, the target comes from the sources and is shown, not asked for.
//! 4. **Preview**: the publisher's preview of every bundle (one per pull request when the
//!    review spans several), scrollable.
//! 5. **Confirm**, then **submit**: bundles are submitted one at a time; each success saves a
//!    publish record (durable reviews). A failure returns to the preview, and retrying skips
//!    the bundles already published.
//!
//! Bundles are planned by [`crate::v2::publish`] and built from the target diffs' hunks; the
//! service never materializes the review for V2.

use super::backend::{SharedBackend, call_json};
use super::modal::{self, MenuPanel};
use super::options_form::{FormKey, OptionsForm};
use super::theme::ReviewTheme;
use crate::v2::publish::{PlanSource, PlannedBundle, PublishThread};
use bcode_code_review_models::{
    DraftAnchor, ExternalPublishReviewRequest, ListReviewPublishersResponse,
    OP_REVIEW_FILE_HUNKS_GET, OP_REVIEW_PUBLISH_BUNDLE_PREVIEW, OP_REVIEW_PUBLISH_BUNDLE_SUBMIT,
    OP_REVIEW_PUBLISH_RECORD_SAVE, OP_REVIEW_PUBLISHER_MANIFEST, OP_REVIEW_PUBLISHER_PREVIEW,
    OP_REVIEW_PUBLISHER_SUBMIT, OP_REVIEW_PUBLISHERS_LIST, OP_REVIEW_STACK_ATTRIBUTION,
    PublishBundleRequest, PublishReviewPreviewResponse, PublishReviewResponse,
    REVIEW_PUBLISHER_INTERFACE_ID, ResolvedReviewStack, ResolvedSource, ReviewBundle,
    ReviewFileHunksRequest, ReviewFileHunksResponse, ReviewFileSummary, ReviewPublisherManifest,
    ReviewPullRequestTarget, ReviewStackAttributionRequest, ReviewStackAttributionResponse,
    ReviewTarget, ReviewWorkspace, SavePublishRecordRequest,
};
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::component::{LayoutId, LayoutNode};
use bmux_tui::composition::{Column, Flex, TextBlock};
use bmux_tui::event::Event;
use bmux_tui::geometry::Rect;
use bmux_tui::paint::PaintCx;
use bmux_tui::text::{Line, Span, Text, TextWrap};
use bmux_tui_components::menu::MenuItem;
use bmux_tui_components::picker_frame::PickerFrameComponent;
use bmux_tui_components::scroll_view::{ScrollExtent, ScrollViewState};
use bmux_tui_components::text_view::{TextViewComponent, TextViewPolicy};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

const FRAME_ID: &str = "review.publish";
const MENU_ID: &str = "review.publish.menu";
const PREVIEW_ID: &str = "review.publish.preview";
const FORM_ID: &str = "review.publish.form";
/// Id of the checklist's continue row.
const CONTINUE_ID: &str = "publish.continue";
/// Default built-in publisher.
const DEFAULT_PUBLISHER: &str = "markdown_file";

/// One publisher: its manifest and, for external publishers, the plugin serving it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublisherEntry {
    /// Manifest.
    pub manifest: ReviewPublisherManifest,
    /// Plugin serving it; `None` for built-in publishers.
    pub plugin_id: Option<String>,
}

/// Readiness of the review for publishing (the checklist header).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Readiness {
    /// Viewed files.
    pub viewed: usize,
    /// All files.
    pub total: usize,
    /// First unviewed paths.
    pub unviewed: Vec<String>,
    /// Labels of the first open threads, and how many are open.
    pub open: (Vec<String>, usize),
    /// Resolved threads.
    pub resolved: usize,
    /// Draft comments.
    pub drafts: usize,
}

/// A thread in the checklist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecklistThread {
    /// Thread id.
    pub thread_id: String,
    /// Location and first line.
    pub label: String,
    /// Whether it is published.
    pub included: bool,
}

/// A bundle ready to submit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedBundle {
    /// Display label (`#101 Title`, or `review`).
    pub label: String,
    /// The bundle.
    pub bundle: ReviewBundle,
    /// Persisted anchors of its threads (recorded as published).
    pub anchors: Vec<DraftAnchor>,
    /// The publisher's preview.
    pub preview: String,
}

/// What submitting did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitReport {
    /// Bundles published now: index and response.
    pub done: Vec<(usize, PublishReviewResponse)>,
    /// The bundle that failed and why (later bundles were not tried).
    pub failed: Option<(usize, String)>,
    /// Publish records that could not be saved.
    pub record_errors: Vec<String>,
}

/// Work the surface runs for the flow.
#[derive(Debug, Clone)]
pub enum FlowRequest {
    /// Discover publishers.
    Publishers,
    /// Plan, build, and preview the bundles.
    Prepare {
        /// Publisher.
        publisher: PublisherEntry,
        /// Options.
        options: serde_json::Value,
        /// Thread ids to publish.
        threads: BTreeSet<String>,
    },
    /// Submit the bundles not submitted yet.
    Submit {
        /// Publisher.
        publisher: PublisherEntry,
        /// Options.
        options: serde_json::Value,
        /// Every prepared bundle.
        bundles: Vec<PreparedBundle>,
        /// Indices already published.
        skip: BTreeSet<usize>,
    },
}

/// Results of the flow's work.
#[derive(Debug)]
pub enum FlowResult {
    /// Publishers discovered.
    Publishers(Result<Vec<PublisherEntry>, String>),
    /// Bundles prepared.
    Prepared(Result<Vec<PreparedBundle>, String>),
    /// Bundles submitted.
    Submitted(SubmitReport),
}

/// What an input did to the flow.
#[derive(Debug, Clone)]
pub enum FlowOutcome {
    /// Close the flow.
    Close,
    /// Redraw.
    Redraw,
    /// Not handled (the flow still captures input).
    Ignored,
    /// Run this work.
    Request(Box<FlowRequest>),
}

/// Step of the flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Waiting for work.
    Loading(String),
    /// Readiness and thread selection.
    Checklist,
    /// Publisher picker.
    Publishers,
    /// Options form.
    Options,
    /// Preview.
    Preview,
    /// Submit confirmation.
    Confirm,
}

/// The publish flow.
pub struct PublishFlow {
    step: Step,
    readiness: Readiness,
    threads: Vec<ChecklistThread>,
    /// Providers of the review's pull-request sources (publishers for them are preferred).
    pull_providers: BTreeSet<String>,
    /// Pull requests the review publishes to, per provider (shown in the options step).
    targets: BTreeMap<String, Vec<ReviewPullRequestTarget>>,
    publishers: Option<Vec<PublisherEntry>>,
    publisher: Option<usize>,
    form: Option<OptionsForm>,
    menu: MenuPanel,
    prepared: Vec<PreparedBundle>,
    submitted: BTreeSet<usize>,
    preview: Vec<Line>,
    scroll: Cell<ScrollViewState>,
    error: Option<String>,
    painted: Option<(Rect, LayoutNode)>,
}

impl std::fmt::Debug for PublishFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublishFlow")
            .field("step", &self.step)
            .finish_non_exhaustive()
    }
}

impl PublishFlow {
    /// A flow starting at the checklist.
    #[must_use]
    pub fn new(
        readiness: Readiness,
        threads: Vec<ChecklistThread>,
        targets: BTreeMap<String, Vec<ReviewPullRequestTarget>>,
        theme: &ReviewTheme,
    ) -> Self {
        let mut flow = Self {
            step: Step::Checklist,
            readiness,
            threads,
            pull_providers: targets.keys().cloned().collect(),
            targets,
            publishers: None,
            publisher: None,
            form: None,
            menu: MenuPanel::new(MENU_ID),
            prepared: Vec::new(),
            submitted: BTreeSet::new(),
            preview: Vec::new(),
            scroll: Cell::new(ScrollViewState::new()),
            error: None,
            painted: None,
        };
        flow.checklist_items(theme);
        flow
    }

    /// The current step.
    #[must_use]
    pub const fn step(&self) -> &Step {
        &self.step
    }

    /// The last error shown.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Labels of the menu items shown (checklist, publishers).
    #[must_use]
    pub fn menu_labels(&self) -> Vec<String> {
        self.menu
            .items()
            .iter()
            .map(|item| {
                item.lines
                    .iter()
                    .map(Line::plain_text)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    }

    /// The options form, in the options step.
    #[must_use]
    pub const fn form(&self) -> Option<&OptionsForm> {
        self.form.as_ref()
    }

    /// The prepared bundles.
    #[must_use]
    pub fn prepared(&self) -> &[PreparedBundle] {
        &self.prepared
    }

    fn checklist_items(&mut self, theme: &ReviewTheme) {
        let readiness = &self.readiness;
        let section = |id: String, spans: Vec<Span>| {
            let mut item = MenuItem::rich(id, Line::from_spans(spans));
            item.section = true;
            item
        };
        let mut items = Vec::new();
        let complete = readiness.viewed == readiness.total && readiness.open.1 == 0;
        items.push(section(
            "ready".into(),
            vec![if complete {
                Span::styled("Ready to publish", theme.badge_resolved)
            } else {
                Span::styled(
                    "Review incomplete — you can still publish",
                    theme.badge_draft,
                )
            }],
        ));
        let mut viewed = vec![Span::styled(
            format!("Viewed {}/{} files", readiness.viewed, readiness.total),
            theme.text,
        )];
        if !readiness.unviewed.is_empty() {
            viewed.push(Span::styled(
                format!(" · unviewed: {}", readiness.unviewed.join(", ")),
                theme.muted,
            ));
        }
        items.push(section("viewed".into(), viewed));
        let (open, open_count) = &readiness.open;
        let mut threads = vec![Span::styled(
            format!(
                "{open_count} open · {} resolved · {} draft{}",
                readiness.resolved,
                readiness.drafts,
                if readiness.drafts == 1 { "" } else { "s" }
            ),
            theme.text,
        )];
        if !open.is_empty() {
            threads.push(Span::styled(format!(" · {}", open.join(", ")), theme.muted));
        }
        items.push(section("threads".into(), threads));
        if !self.threads.is_empty() {
            items.push(section(
                "include".into(),
                vec![Span::styled(
                    "Threads to publish (space toggles)",
                    theme.title,
                )],
            ));
        }
        for thread in &self.threads {
            items.push(MenuItem::rich(
                format!("thread.{}", thread.thread_id),
                Line::from_spans(vec![
                    Span::styled(
                        if thread.included { "[x] " } else { "[ ] " },
                        if thread.included {
                            theme.accent
                        } else {
                            theme.muted
                        },
                    ),
                    Span::styled(thread.label.clone(), theme.text),
                ]),
            ));
        }
        items.push(MenuItem::rich(
            CONTINUE_ID,
            Line::from_spans(vec![Span::styled("Continue to publishers →", theme.accent)]),
        ));
        self.menu.set_items(items);
    }

    fn publisher_items(&mut self, theme: &ReviewTheme) {
        let Some(publishers) = &self.publishers else {
            return;
        };
        let items = publishers
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let caps = &entry.manifest.capabilities;
                let mut labels = Vec::new();
                if caps.supports_inline_comments {
                    labels.push("inline");
                }
                if caps.supports_summary_comment {
                    labels.push("summary");
                }
                if caps.supports_ranges {
                    labels.push("ranges");
                }
                if !caps.submit {
                    labels.push("preview only");
                }
                let mut spans = vec![
                    Span::styled(entry.manifest.label.clone(), theme.text),
                    Span::raw("  "),
                    Span::styled(entry.manifest.description.clone(), theme.muted),
                ];
                if !labels.is_empty() {
                    spans.push(Span::styled(
                        format!("  [{}]", labels.join(", ")),
                        theme.badge_open,
                    ));
                }
                MenuItem::rich(format!("publisher.{index}"), Line::from_spans(spans))
            })
            .collect();
        self.menu.set_items(items);
        let preferred = publishers
            .iter()
            .position(|entry| {
                entry
                    .manifest
                    .pull_request_provider
                    .as_ref()
                    .is_some_and(|provider| self.pull_providers.contains(provider))
            })
            .or_else(|| {
                publishers
                    .iter()
                    .position(|entry| entry.manifest.id == DEFAULT_PUBLISHER)
            })
            .or(self.publisher);
        if let Some(index) = self.publisher.or(preferred) {
            self.menu.activate_id(&format!("publisher.{index}"));
        }
    }

    fn selected_publisher(&self) -> Option<&PublisherEntry> {
        self.publishers.as_ref()?.get(self.publisher?)
    }

    fn included(&self) -> BTreeSet<String> {
        self.threads
            .iter()
            .filter(|thread| thread.included)
            .map(|thread| thread.thread_id.clone())
            .collect()
    }

    fn continue_to_publishers(&mut self, theme: &ReviewTheme) -> FlowOutcome {
        self.error = None;
        if self.publishers.is_some() {
            self.step = Step::Publishers;
            self.publisher_items(theme);
            return FlowOutcome::Redraw;
        }
        self.step = Step::Loading("Finding publishers…".into());
        FlowOutcome::Request(Box::new(FlowRequest::Publishers))
    }

    fn choose_publisher(&mut self, index: usize) -> FlowOutcome {
        let Some(entry) = self.publishers.as_ref().and_then(|list| list.get(index)) else {
            return FlowOutcome::Ignored;
        };
        let form = OptionsForm::from_schema(&entry.manifest.options_schema);
        if self.publisher != Some(index) || self.form.is_none() {
            self.form = Some(form);
        }
        self.publisher = Some(index);
        self.step = Step::Options;
        self.error = None;
        FlowOutcome::Redraw
    }

    fn prepare(&mut self) -> FlowOutcome {
        let (Some(publisher), Some(form)) = (self.selected_publisher().cloned(), &self.form) else {
            return FlowOutcome::Ignored;
        };
        let threads = self.included();
        self.step = Step::Loading("Building the preview…".into());
        self.prepared.clear();
        self.submitted.clear();
        FlowOutcome::Request(Box::new(FlowRequest::Prepare {
            publisher,
            options: form.values(),
            threads,
        }))
    }

    fn submit(&mut self) -> FlowOutcome {
        let (Some(publisher), Some(form)) = (self.selected_publisher().cloned(), &self.form) else {
            return FlowOutcome::Ignored;
        };
        self.step = Step::Loading("Publishing…".into());
        self.error = None;
        FlowOutcome::Request(Box::new(FlowRequest::Submit {
            publisher,
            options: form.values(),
            bundles: self.prepared.clone(),
            skip: self.submitted.clone(),
        }))
    }

    /// Apply finished work. Returns the submit report when submitting finished, for the
    /// surface's records and toasts.
    pub fn apply(&mut self, result: FlowResult, theme: &ReviewTheme) -> Option<SubmitReport> {
        match result {
            FlowResult::Publishers(Ok(publishers)) => {
                self.publishers = Some(publishers);
                self.step = Step::Publishers;
                self.publisher_items(theme);
            }
            FlowResult::Publishers(Err(error)) => {
                self.error = Some(error);
                self.step = Step::Checklist;
            }
            FlowResult::Prepared(Ok(prepared)) => {
                self.preview = preview_lines(&prepared, theme);
                self.prepared = prepared;
                self.scroll.set(ScrollViewState::new());
                self.step = Step::Preview;
            }
            FlowResult::Prepared(Err(error)) => {
                self.error = Some(error);
                self.step = Step::Options;
            }
            FlowResult::Submitted(report) => {
                self.submitted
                    .extend(report.done.iter().map(|(index, _)| *index));
                if let Some((index, error)) = &report.failed {
                    let label = self
                        .prepared
                        .get(*index)
                        .map_or_else(String::new, |bundle| format!("{}: ", bundle.label));
                    self.error = Some(format!("{label}{error}"));
                    self.step = Step::Preview;
                }
                return Some(report);
            }
        }
        None
    }

    /// Whether the flow is done: every bundle was submitted.
    #[must_use]
    pub fn finished(&self) -> bool {
        !self.prepared.is_empty() && self.submitted.len() == self.prepared.len()
    }

    /// Handle a key.
    pub fn key(&mut self, stroke: KeyStroke, theme: &ReviewTheme) -> FlowOutcome {
        let plain = stroke.modifiers.is_empty();
        match self.step.clone() {
            Step::Loading(_) => {
                if stroke.key == KeyCode::Escape && self.prepared.is_empty() {
                    return FlowOutcome::Close;
                }
                FlowOutcome::Ignored
            }
            Step::Checklist => match stroke.key {
                KeyCode::Escape if plain => FlowOutcome::Close,
                KeyCode::Space if plain => {
                    let Some(id) = self
                        .menu
                        .active_id()
                        .and_then(|id| id.strip_prefix("thread."))
                        .map(str::to_owned)
                    else {
                        return FlowOutcome::Ignored;
                    };
                    if let Some(thread) = self
                        .threads
                        .iter_mut()
                        .find(|thread| thread.thread_id == id)
                    {
                        thread.included = !thread.included;
                    }
                    self.checklist_items(theme);
                    FlowOutcome::Redraw
                }
                KeyCode::Enter if plain => self.continue_to_publishers(theme),
                key if self.menu.navigate(key) => FlowOutcome::Redraw,
                _ => FlowOutcome::Ignored,
            },
            Step::Publishers => match stroke.key {
                KeyCode::Escape if plain => {
                    self.step = Step::Checklist;
                    self.checklist_items(theme);
                    FlowOutcome::Redraw
                }
                KeyCode::Enter if plain => self.activate_publisher(),
                key if self.menu.navigate(key) => FlowOutcome::Redraw,
                _ => FlowOutcome::Ignored,
            },
            Step::Options => {
                let Some(form) = self.form.as_mut() else {
                    return FlowOutcome::Ignored;
                };
                match form.key(stroke) {
                    FormKey::Submitted => self.prepare(),
                    FormKey::Cancelled => {
                        self.step = Step::Publishers;
                        self.publisher_items(theme);
                        FlowOutcome::Redraw
                    }
                    FormKey::Redraw => FlowOutcome::Redraw,
                    FormKey::Ignored => FlowOutcome::Ignored,
                }
            }
            Step::Preview => match stroke.key {
                KeyCode::Escape if plain => {
                    if self.submitted.is_empty() {
                        self.step = Step::Options;
                        FlowOutcome::Redraw
                    } else {
                        // Some bundles are published: going back would plan them again.
                        FlowOutcome::Close
                    }
                }
                KeyCode::Enter if plain => {
                    self.step = Step::Confirm;
                    FlowOutcome::Redraw
                }
                key => {
                    let delta = match key {
                        KeyCode::Down | KeyCode::Char('j') => 1,
                        KeyCode::Up | KeyCode::Char('k') => -1,
                        KeyCode::PageDown | KeyCode::Space => 10,
                        KeyCode::PageUp => -10,
                        _ => return FlowOutcome::Ignored,
                    };
                    self.scroll_preview(delta);
                    FlowOutcome::Redraw
                }
            },
            Step::Confirm => match stroke.key {
                KeyCode::Enter if plain => self.submit(),
                KeyCode::Char('p') | KeyCode::Escape if plain => {
                    self.step = Step::Preview;
                    FlowOutcome::Redraw
                }
                _ => FlowOutcome::Ignored,
            },
        }
    }

    fn activate_publisher(&mut self) -> FlowOutcome {
        let Some(index) = self
            .menu
            .active_id()
            .and_then(|id| id.strip_prefix("publisher."))
            .and_then(|index| index.parse::<usize>().ok())
        else {
            return FlowOutcome::Ignored;
        };
        self.choose_publisher(index)
    }

    fn scroll_preview(&self, delta: i64) {
        let Some((_, layout)) = &self.painted else {
            return;
        };
        let viewport = LayoutId::new(format!("{PREVIEW_ID}.viewport"));
        let Some(node) = layout.find(&viewport) else {
            return;
        };
        let mut state = self.scroll.get();
        let _ = ScrollExtent::of(node).scroll_by(&mut state, delta);
        self.scroll.set(state);
    }

    /// Paste into the options form.
    pub fn paste(&mut self, text: &str) -> FlowOutcome {
        match (&self.step, self.form.as_mut()) {
            (Step::Options, Some(form)) => match form.paste(text) {
                FormKey::Ignored => FlowOutcome::Ignored,
                _ => FlowOutcome::Redraw,
            },
            _ => FlowOutcome::Ignored,
        }
    }

    /// Route a pointer event through the painted layout.
    pub fn mouse(&mut self, event: &Event, theme: &ReviewTheme) -> FlowOutcome {
        let Some((area, layout)) = self.painted.clone() else {
            return FlowOutcome::Ignored;
        };
        let outcome = {
            let component = self.component(theme, area);
            modal::route(&component, area, &layout, event)
        };
        if let Some(id) = self.menu.take_activated() {
            self.menu.activate_id(&id);
            return match (&self.step, id.as_str()) {
                (Step::Checklist, CONTINUE_ID) => self.continue_to_publishers(theme),
                (Step::Checklist, _) => {
                    // A click toggles a thread.
                    self.key(KeyStroke::simple(KeyCode::Space), theme)
                }
                (Step::Publishers, _) => self.activate_publisher(),
                _ => FlowOutcome::Redraw,
            };
        }
        if outcome == bmux_tui::event::EventOutcome::Ignored {
            FlowOutcome::Ignored
        } else {
            FlowOutcome::Redraw
        }
    }

    const fn title(&self) -> &'static str {
        match self.step {
            Step::Loading(_) => "Publish",
            Step::Checklist => "Publish · checklist",
            Step::Publishers => "Publish · choose a publisher",
            Step::Options => "Publish · options",
            Step::Preview => "Publish · preview",
            Step::Confirm => "Publish · confirm",
        }
    }

    const fn footer(&self) -> &'static str {
        match self.step {
            Step::Loading(_) => "esc close",
            Step::Checklist => "space include · enter continue · esc close",
            Step::Publishers => "enter choose · esc back",
            Step::Options => "tab next field · ←/→ change · enter preview · esc back",
            Step::Preview => "j/k scroll · enter submit… · esc back",
            Step::Confirm => "enter submit · p preview · esc back",
        }
    }

    fn notes(&self, theme: &ReviewTheme) -> Vec<Line> {
        let mut lines = Vec::new();
        if let Some(error) = &self.error {
            lines.push(Line::from_spans(vec![Span::styled(
                error.clone(),
                theme.error,
            )]));
        }
        match &self.step {
            Step::Loading(text) => {
                lines.push(Line::from_spans(vec![Span::styled(
                    text.clone(),
                    theme.muted,
                )]));
            }
            Step::Options => {
                if let Some(provider) = self
                    .selected_publisher()
                    .and_then(|entry| entry.manifest.pull_request_provider.as_ref())
                    && let Some(targets) = self.targets.get(provider)
                {
                    let numbers: Vec<String> = targets
                        .iter()
                        .map(|target| format!("{}#{}", target.repository, target.number))
                        .collect();
                    lines.push(Line::from_spans(vec![
                        Span::styled("Publishes to ", theme.muted),
                        Span::styled(numbers.join(", "), theme.accent),
                        Span::styled(
                            if numbers.len() > 1 {
                                " (one review per pull request; from the review's sources)"
                            } else {
                                " (from the review's source)"
                            },
                            theme.muted,
                        ),
                    ]));
                }
            }
            Step::Confirm => {
                let left: Vec<&str> = self
                    .prepared
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| !self.submitted.contains(index))
                    .map(|(_, bundle)| bundle.label.as_str())
                    .collect();
                let name = self
                    .selected_publisher()
                    .map_or("the publisher", |entry| entry.manifest.label.as_str());
                lines.push(Line::from_spans(vec![Span::styled(
                    format!("Submit to {name}: {}?", left.join(", ")),
                    theme.title,
                )]));
            }
            _ => {}
        }
        lines
    }

    fn component<'a>(&'a self, theme: &ReviewTheme, area: Rect) -> PickerFrameComponent<'a> {
        let frame = modal::frame(theme, self.title(), self.footer(), area.size());
        let notes = TextBlock::new(Text::from_lines(self.notes(theme))).wrap(TextWrap::Word);
        let body = Column::new().id(format!("{FRAME_ID}.body")).child(notes);
        let body = match &self.step {
            Step::Checklist | Step::Publishers => {
                body.flex(Flex::new(1, self.menu.component(theme)))
            }
            Step::Options => match &self.form {
                Some(form) => body.flex(Flex::new(1, form.component(FORM_ID, theme))),
                None => body,
            },
            Step::Preview | Step::Confirm => body.flex(Flex::new(
                1,
                TextViewComponent::new(PREVIEW_ID, &self.preview, &self.scroll)
                    .policy(TextViewPolicy::scrollable())
                    .styles(theme.component.text_view_styles()),
            )),
            Step::Loading(_) => body,
        };
        PickerFrameComponent::new(FRAME_ID, frame, body)
    }

    /// Paint over `area`, keeping the layout for pointer routing.
    pub fn paint(&mut self, theme: &ReviewTheme, area: Rect, cx: &mut PaintCx<'_, '_>) {
        if area.width == 0 || area.height == 0 {
            self.painted = None;
            return;
        }
        let layout = {
            let component = self.component(theme, area);
            modal::paint(&component, area, cx)
        };
        self.menu.note_layout(&layout);
        self.painted = Some((area, layout));
    }
}

fn preview_lines(prepared: &[PreparedBundle], theme: &ReviewTheme) -> Vec<Line> {
    let mut lines = Vec::new();
    for bundle in prepared {
        if prepared.len() > 1 {
            lines.push(Line::from_spans(vec![Span::styled(
                format!(
                    "── {} · {} thread{} ──",
                    bundle.label,
                    bundle.bundle.threads.len(),
                    if bundle.bundle.threads.len() == 1 {
                        ""
                    } else {
                        "s"
                    }
                ),
                theme.title,
            )]));
        }
        lines.extend(
            bundle
                .preview
                .lines()
                .map(|line| Line::from_spans(vec![Span::styled(line.to_owned(), theme.text)])),
        );
        lines.push(Line::default());
    }
    lines
}

// ------------------------------------------------------------------------------------------
// Work
// ------------------------------------------------------------------------------------------

/// What preparing needs from the open review.
#[derive(Debug, Clone)]
pub struct PrepareContext {
    /// Repository.
    pub repo_path: PathBuf,
    /// The workspace (title, sources, and identity).
    pub workspace: ReviewWorkspace,
    /// Resolved sources (first part of each) and their stacks, by source id.
    pub resolved: BTreeMap<String, (ResolvedSource, Option<ResolvedReviewStack>)>,
    /// File summaries per source id, in document order.
    pub files: BTreeMap<String, Vec<ReviewFileSummary>>,
    /// Old paths of renamed files, by `(source id, path)`.
    pub old_paths: BTreeMap<(String, String), String>,
    /// Threads (every draft thread, oldest comment first).
    pub threads: Vec<PublishThread>,
    /// Review target used when a bundle's diff has no commit range.
    pub fallback_target: ReviewTarget,
}

/// Discover built-in and external publishers.
///
/// # Errors
///
/// Returns the message of a failing built-in listing.
pub async fn discover(backend: SharedBackend) -> Result<Vec<PublisherEntry>, String> {
    let backend = backend.as_ref();
    let builtin: ListReviewPublishersResponse =
        call_json(backend, OP_REVIEW_PUBLISHERS_LIST, &serde_json::json!({}))
            .await
            .map_err(|error| error.message)?;
    let mut entries: Vec<PublisherEntry> = builtin
        .publishers
        .into_iter()
        .map(|manifest| PublisherEntry {
            manifest,
            plugin_id: None,
        })
        .collect();
    let plugins = backend
        .plugins_serving(REVIEW_PUBLISHER_INTERFACE_ID.to_owned())
        .await
        .unwrap_or_default();
    for plugin_id in plugins {
        let Ok(payload) = backend
            .invoke(
                plugin_id.clone(),
                REVIEW_PUBLISHER_INTERFACE_ID.to_owned(),
                OP_REVIEW_PUBLISHER_MANIFEST.to_owned(),
                b"{}".to_vec(),
            )
            .await
        else {
            continue;
        };
        if let Ok(manifest) = serde_json::from_slice::<ReviewPublisherManifest>(&payload) {
            entries.push(PublisherEntry {
                manifest,
                plugin_id: Some(plugin_id),
            });
        }
    }
    Ok(entries)
}

/// Plan, build, and preview the bundles.
///
/// # Errors
///
/// Returns the first failing hunk, attribution, or preview call's message.
pub async fn prepare(
    backend: SharedBackend,
    context: PrepareContext,
    publisher: PublisherEntry,
    options: serde_json::Value,
    included: BTreeSet<String>,
) -> Result<Vec<PreparedBundle>, String> {
    let provider = publisher.manifest.pull_request_provider.clone();
    let threads: Vec<PublishThread> = context
        .threads
        .iter()
        .filter(|thread| included.contains(&thread.thread_id))
        .cloned()
        .collect();
    if threads.is_empty() {
        return Err("no threads selected to publish".to_owned());
    }
    let sources: Vec<PlanSource<'_>> = context
        .workspace
        .sources
        .iter()
        .map(|source| {
            let resolved = context.resolved.get(&source.id);
            PlanSource {
                source,
                resolved: resolved.map(|(resolved, _)| resolved),
                stack: resolved.and_then(|(_, stack)| stack.as_ref()),
            }
        })
        .collect();
    let mut attributions = BTreeMap::new();
    for (source_id, path) in
        crate::v2::publish::attribution_needs(&threads, &sources, provider.as_deref())
    {
        let Some(stack) = context
            .resolved
            .get(&source_id)
            .and_then(|(_, stack)| stack.clone())
        else {
            continue;
        };
        let request = ReviewStackAttributionRequest {
            repo_path: context.repo_path.clone(),
            stack,
            path: path.clone(),
            old_path: context
                .old_paths
                .get(&(source_id.clone(), path.clone()))
                .cloned(),
        };
        // A file that cannot be attributed falls back to the top summary.
        if let Ok(response) = call_json::<_, ReviewStackAttributionResponse>(
            backend.as_ref(),
            OP_REVIEW_STACK_ATTRIBUTION,
            &request,
        )
        .await
        {
            attributions.insert((source_id, path), response);
        }
    }
    let planned = crate::v2::publish::plan(threads, &sources, provider.as_deref(), &attributions);
    let mut hunks: HunkCache = BTreeMap::new();
    let mut prepared = Vec::new();
    for bundle in planned {
        let built = build_bundle(&backend, &context, &bundle, &mut hunks).await;
        let preview = preview(&backend, &publisher, &built, &options).await?;
        prepared.push(PreparedBundle {
            label: bundle.label.clone(),
            anchors: bundle
                .threads
                .iter()
                .filter_map(|thread| thread.thread.anchor().cloned())
                .collect(),
            bundle: built,
            preview,
        });
    }
    Ok(prepared)
}

/// Hunks read while building bundles, by diff (source, base, head, part) and path.
type HunkCache = BTreeMap<
    (
        String,
        bcode_code_review_models::RevisionRef,
        bcode_code_review_models::RevisionRef,
        Option<u32>,
        String,
    ),
    Option<ReviewFileHunksResponse>,
>;

async fn build_bundle(
    backend: &SharedBackend,
    context: &PrepareContext,
    planned: &PlannedBundle,
    cache: &mut HunkCache,
) -> ReviewBundle {
    let mut threads = Vec::new();
    for thread in &planned.threads {
        let hunks = match &thread.diff {
            Some(diff) if !thread.summary_only => {
                let key = (
                    diff.source_id.clone(),
                    diff.base.clone(),
                    diff.head.clone(),
                    diff.part.as_ref().map(|part| part.index),
                    thread.anchor.file_path.clone(),
                );
                if !cache.contains_key(&key) {
                    let old_path = context
                        .old_paths
                        .get(&(diff.source_id.clone(), thread.anchor.file_path.clone()))
                        .cloned()
                        // Mapped layer diffs carry no rename information of their own.
                        .filter(|_| {
                            context
                                .resolved
                                .get(&diff.source_id)
                                .is_some_and(|(resolved, _)| resolved == diff)
                        });
                    let response = call_json::<_, ReviewFileHunksResponse>(
                        backend.as_ref(),
                        OP_REVIEW_FILE_HUNKS_GET,
                        &ReviewFileHunksRequest {
                            repo_path: context.repo_path.clone(),
                            source: diff.clone(),
                            path: thread.anchor.file_path.clone(),
                            old_path,
                            context_lines: None,
                            ignore_whitespace: false,
                            max_lines: None,
                        },
                    )
                    .await
                    .ok();
                    cache.insert(key.clone(), response);
                }
                cache.get(&key).and_then(Option::as_ref)
            }
            _ => None,
        };
        threads.push(crate::v2::publish::bundle_thread(thread, hunks));
    }
    let paths: BTreeSet<&str> = planned
        .threads
        .iter()
        .map(|thread| thread.anchor.file_path.as_str())
        .collect();
    let files: Vec<ReviewFileSummary> = match (&planned.target, &planned.source_id) {
        // The pull request of one source: its files.
        (Some(_), Some(source_id)) => context.files.get(source_id).cloned().unwrap_or_default(),
        // Mapped onto layers of a whole stack: the anchored files.
        (Some(_), None) => context
            .files
            .values()
            .flatten()
            .filter(|file| paths.contains(file.path.as_str()))
            .cloned()
            .collect(),
        (None, _) => context.files.values().flatten().cloned().collect(),
    };
    let diff = planned
        .threads
        .iter()
        .find_map(|thread| thread.diff.as_ref());
    let target = diff
        .and_then(|diff| {
            Some(ReviewTarget::CommitRange {
                base: diff.base.commit_oid()?.to_owned(),
                head: diff.head.commit_oid()?.to_owned(),
                merge_base: false,
            })
        })
        .unwrap_or_else(|| context.fallback_target.clone());
    let mut title = context.workspace.title.clone();
    if planned.target.is_some() {
        title = format!("{title} · {}", planned.label);
    }
    ReviewBundle {
        review_id: format!(
            "review-workspace-{}",
            crate::v2::scope::annotation_workspace_id(&context.workspace)
        ),
        title,
        repo_root: context.repo_path.clone(),
        target,
        files,
        surfaces: Vec::new(),
        threads,
        generated_at_ms: now_ms(),
        pull_request: planned.target.clone(),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

async fn preview(
    backend: &SharedBackend,
    publisher: &PublisherEntry,
    bundle: &ReviewBundle,
    options: &serde_json::Value,
) -> Result<String, String> {
    let response: PublishReviewPreviewResponse = match &publisher.plugin_id {
        None => call_json(
            backend.as_ref(),
            OP_REVIEW_PUBLISH_BUNDLE_PREVIEW,
            &PublishBundleRequest {
                publisher_id: publisher.manifest.id.clone(),
                bundle: bundle.clone(),
                options: options.clone(),
            },
        )
        .await
        .map_err(|error| error.message)?,
        Some(plugin) => {
            invoke(
                backend,
                plugin,
                OP_REVIEW_PUBLISHER_PREVIEW,
                &ExternalPublishReviewRequest {
                    bundle: bundle.clone(),
                    options: options.clone(),
                },
            )
            .await?
        }
    };
    Ok(response.preview)
}

async fn invoke<Req: serde::Serialize + Sync, Resp: serde::de::DeserializeOwned>(
    backend: &SharedBackend,
    plugin: &str,
    operation: &str,
    request: &Req,
) -> Result<Resp, String> {
    let payload = serde_json::to_vec(request).map_err(|error| error.to_string())?;
    let response = backend
        .invoke(
            plugin.to_owned(),
            REVIEW_PUBLISHER_INTERFACE_ID.to_owned(),
            operation.to_owned(),
            payload,
        )
        .await
        .map_err(|error| error.message)?;
    serde_json::from_slice(&response)
        .map_err(|error| format!("unexpected {operation} response: {error}"))
}

/// Submit the bundles not in `skip`, one at a time, saving a publish record for each success
/// when `workspace` is durable. Stops at the first failure.
pub async fn submit(
    backend: SharedBackend,
    workspace: Option<ReviewWorkspace>,
    publisher: PublisherEntry,
    options: serde_json::Value,
    bundles: Vec<PreparedBundle>,
    skip: BTreeSet<usize>,
) -> SubmitReport {
    let mut report = SubmitReport {
        done: Vec::new(),
        failed: None,
        record_errors: Vec::new(),
    };
    for (index, prepared) in bundles.iter().enumerate() {
        if skip.contains(&index) {
            continue;
        }
        let result: Result<PublishReviewResponse, String> = match &publisher.plugin_id {
            None => call_json(
                backend.as_ref(),
                OP_REVIEW_PUBLISH_BUNDLE_SUBMIT,
                &PublishBundleRequest {
                    publisher_id: publisher.manifest.id.clone(),
                    bundle: prepared.bundle.clone(),
                    options: options.clone(),
                },
            )
            .await
            .map_err(|error| error.message),
            Some(plugin) => {
                invoke(
                    &backend,
                    plugin,
                    OP_REVIEW_PUBLISHER_SUBMIT,
                    &ExternalPublishReviewRequest {
                        bundle: prepared.bundle.clone(),
                        options: options.clone(),
                    },
                )
                .await
            }
        };
        match result {
            Ok(response) => {
                if let Some(workspace) = &workspace {
                    let saved = call_json::<_, serde_json::Value>(
                        backend.as_ref(),
                        OP_REVIEW_PUBLISH_RECORD_SAVE,
                        &SavePublishRecordRequest {
                            workspace: workspace.clone(),
                            review_id: prepared.bundle.review_id.clone(),
                            publisher_id: response.publisher_id.clone(),
                            submitted: response.submitted,
                            published_thread_anchors: prepared.anchors.clone(),
                            output: response.output.clone(),
                            message: response.message.clone(),
                        },
                    )
                    .await;
                    if let Err(error) = saved {
                        report.record_errors.push(error.message);
                    }
                }
                report.done.push((index, response));
            }
            Err(error) => {
                report.failed = Some((index, error));
                break;
            }
        }
    }
    report
}
