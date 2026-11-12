//! Importing an external review: a modal flow over BMUX `picker_frame`, `menu`,
//! and `form`/`form_field`.
//!
//! 1. **Importer**: every `bcode.review_importer/v1` importer.
//! 2. **Options**: the importer's options schema as a form. When the review is one pull
//!    request (or stack layer) the request names it, so the provider's importer needs nothing.
//! 3. **Import**: the importer maps external threads onto the review's files (their hunks are
//!    read for up to [`MAX_IMPORT_FILES`] files); the snapshot replaces the previous one from the
//!    same importer (saved for durable reviews) and its threads show read-only.

use super::modal::{self, MenuPanel};
use super::options_form::{FormKey, OptionsForm};
use super::services::ReviewServices;
use super::theme::ReviewTheme;
use bcode_code_review_models::{
    ExternalReviewImportRecord, ExternalReviewImportRequest, ExternalReviewImportResponse,
    OP_REVIEW_IMPORTER_IMPORT, OP_REVIEW_IMPORTER_MANIFEST, REVIEW_IMPORTER_INTERFACE_ID,
    ResolvedSource, ReviewFile, ReviewFileHunksRequest, ReviewFileHunksResponse,
    ReviewImporterManifest, ReviewIndexEntry, ReviewPullRequestTarget,
    SaveExternalReviewImportRequest, SaveExternalReviewImportResponse,
};
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::component::LayoutNode;
use bmux_tui::composition::{Column, Flex, TextBlock};
use bmux_tui::event::Event;
use bmux_tui::geometry::Rect;
use bmux_tui::paint::PaintCx;
use bmux_tui::text::{Line, Span, Text, TextWrap};
use bmux_tui_components::menu::MenuItem;
use bmux_tui_components::picker_frame::PickerFrameComponent;
use std::path::PathBuf;

const FRAME_ID: &str = "review.import";
const MENU_ID: &str = "review.import.menu";
const FORM_ID: &str = "review.import.form";
/// Most files whose hunks are read for anchor mapping.
pub const MAX_IMPORT_FILES: usize = 500;
/// Most diff lines read for anchor mapping.
const MAX_IMPORT_LINES: usize = 200_000;

/// One importer and the plugin serving it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImporterEntry {
    /// Manifest.
    pub manifest: ReviewImporterManifest,
    /// Plugin serving it.
    pub plugin_id: String,
}

/// Work the surface runs for the flow.
#[derive(Debug, Clone)]
pub enum ImportRequest {
    /// Discover importers.
    Importers,
    /// Import with an importer.
    Import {
        /// Importer.
        importer: ImporterEntry,
        /// Options.
        options: serde_json::Value,
    },
}

/// Results of the flow's work.
#[derive(Debug)]
pub enum ImportResult {
    /// Importers discovered.
    Importers(Vec<ImporterEntry>),
    /// The import finished: the snapshot, or the error.
    Imported(Result<ExternalReviewImportRecord, String>),
}

/// What an input did to the flow.
#[derive(Debug, Clone)]
pub enum ImportOutcome {
    /// Close the flow.
    Close,
    /// Redraw.
    Redraw,
    /// Not handled.
    Ignored,
    /// Run this work.
    Request(ImportRequest),
}

/// Step of the flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportStep {
    /// Waiting for work.
    Loading(String),
    /// Importer picker.
    Importers,
    /// Options form.
    Options,
}

/// The import flow.
pub struct ImportFlow {
    step: ImportStep,
    importers: Vec<ImporterEntry>,
    importer: Option<usize>,
    form: Option<OptionsForm>,
    menu: MenuPanel,
    target: Option<ReviewPullRequestTarget>,
    error: Option<String>,
    painted: Option<(Rect, LayoutNode)>,
}

impl std::fmt::Debug for ImportFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportFlow")
            .field("step", &self.step)
            .finish_non_exhaustive()
    }
}

impl ImportFlow {
    /// A flow that starts by discovering importers; `target` is the review's pull request.
    #[must_use]
    pub fn new(target: Option<ReviewPullRequestTarget>) -> (Self, ImportRequest) {
        (
            Self {
                step: ImportStep::Loading("Finding importers…".into()),
                importers: Vec::new(),
                importer: None,
                form: None,
                menu: MenuPanel::new(MENU_ID),
                target,
                error: None,
                painted: None,
            },
            ImportRequest::Importers,
        )
    }

    /// The current step.
    #[must_use]
    pub const fn step(&self) -> &ImportStep {
        &self.step
    }

    /// The last error shown.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Apply finished work. Returns the imported snapshot once the import succeeded.
    pub fn apply(
        &mut self,
        result: ImportResult,
        theme: &ReviewTheme,
    ) -> Option<ExternalReviewImportRecord> {
        match result {
            ImportResult::Importers(importers) => {
                self.importers = importers;
                self.step = ImportStep::Importers;
                if self.importers.is_empty() {
                    self.error = Some(
                        "No external review importers are enabled (for example the GitHub plugin)"
                            .into(),
                    );
                }
                let items = self
                    .importers
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        MenuItem::rich(
                            format!("importer.{index}"),
                            Line::from_spans(vec![
                                Span::styled(entry.manifest.label.clone(), theme.text),
                                Span::raw("  "),
                                Span::styled(entry.manifest.description.clone(), theme.muted),
                            ]),
                        )
                    })
                    .collect();
                self.menu.set_items(items);
                None
            }
            ImportResult::Imported(Ok(record)) => Some(record),
            ImportResult::Imported(Err(error)) => {
                self.error = Some(error);
                self.step = ImportStep::Options;
                None
            }
        }
    }

    fn choose(&mut self) -> ImportOutcome {
        let Some(index) = self
            .menu
            .active_id()
            .and_then(|id| id.strip_prefix("importer."))
            .and_then(|index| index.parse::<usize>().ok())
        else {
            return ImportOutcome::Ignored;
        };
        let Some(entry) = self.importers.get(index) else {
            return ImportOutcome::Ignored;
        };
        self.form = Some(OptionsForm::from_schema(&entry.manifest.options_schema));
        self.importer = Some(index);
        self.step = ImportStep::Options;
        self.error = None;
        ImportOutcome::Redraw
    }

    /// Handle a key.
    pub fn key(&mut self, stroke: KeyStroke) -> ImportOutcome {
        let plain = stroke.modifiers.is_empty();
        match self.step.clone() {
            ImportStep::Loading(_) => {
                if stroke.key == KeyCode::Escape && plain {
                    ImportOutcome::Close
                } else {
                    ImportOutcome::Ignored
                }
            }
            ImportStep::Importers => match stroke.key {
                KeyCode::Escape if plain => ImportOutcome::Close,
                KeyCode::Enter if plain => self.choose(),
                key if self.menu.navigate(key) => ImportOutcome::Redraw,
                _ => ImportOutcome::Ignored,
            },
            ImportStep::Options => {
                let Some(form) = self.form.as_mut() else {
                    return ImportOutcome::Ignored;
                };
                match form.key(stroke) {
                    FormKey::Submitted => {
                        let Some(importer) =
                            self.importer.and_then(|index| self.importers.get(index))
                        else {
                            return ImportOutcome::Ignored;
                        };
                        let options = form.values();
                        self.step = ImportStep::Loading("Importing…".into());
                        self.error = None;
                        ImportOutcome::Request(ImportRequest::Import {
                            importer: importer.clone(),
                            options,
                        })
                    }
                    FormKey::Cancelled => {
                        self.step = ImportStep::Importers;
                        ImportOutcome::Redraw
                    }
                    FormKey::Redraw => ImportOutcome::Redraw,
                    FormKey::Ignored => ImportOutcome::Ignored,
                }
            }
        }
    }

    /// Paste into the options form.
    pub fn paste(&mut self, text: &str) -> ImportOutcome {
        match (&self.step, self.form.as_mut()) {
            (ImportStep::Options, Some(form)) => match form.paste(text) {
                FormKey::Ignored => ImportOutcome::Ignored,
                _ => ImportOutcome::Redraw,
            },
            _ => ImportOutcome::Ignored,
        }
    }

    /// Route a pointer event through the painted layout.
    pub fn mouse(&mut self, event: &Event, theme: &ReviewTheme) -> ImportOutcome {
        let Some((area, layout)) = self.painted.clone() else {
            return ImportOutcome::Ignored;
        };
        let outcome = {
            let component = self.component(theme, area);
            modal::route(&component, area, &layout, event)
        };
        if let Some(id) = self.menu.take_activated()
            && self.step == ImportStep::Importers
        {
            self.menu.activate_id(&id);
            return self.choose();
        }
        if outcome == bmux_tui::event::EventOutcome::Ignored {
            ImportOutcome::Ignored
        } else {
            ImportOutcome::Redraw
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
            ImportStep::Loading(text) => {
                lines.push(Line::from_spans(vec![Span::styled(
                    text.clone(),
                    theme.muted,
                )]));
            }
            ImportStep::Options => {
                if let Some(target) = &self.target {
                    lines.push(Line::from_spans(vec![
                        Span::styled("Imports from ", theme.muted),
                        Span::styled(
                            format!("{}#{}", target.repository, target.number),
                            theme.accent,
                        ),
                        Span::styled(
                            " (the review's source) unless the options name another",
                            theme.muted,
                        ),
                    ]));
                }
            }
            ImportStep::Importers => {}
        }
        lines
    }

    fn component<'a>(&'a self, theme: &ReviewTheme, area: Rect) -> PickerFrameComponent<'a> {
        let (title, footer) = match self.step {
            ImportStep::Loading(_) => (" Import external review ", "esc close"),
            ImportStep::Importers => (" Import · choose an importer ", "enter choose · esc close"),
            ImportStep::Options => (
                " Import · options ",
                "tab next field · ←/→ change · enter import · esc back",
            ),
        };
        let frame = modal::frame(theme, title, footer, area.size());
        let notes = TextBlock::new(Text::from_lines(self.notes(theme))).wrap(TextWrap::Word);
        let body = Column::new().id(format!("{FRAME_ID}.body")).child(notes);
        let body = match (&self.step, &self.form) {
            (ImportStep::Importers, _) => body.flex(Flex::new(1, self.menu.component(theme))),
            (ImportStep::Options, Some(form)) => {
                body.flex(Flex::new(1, form.component(FORM_ID, theme)))
            }
            _ => body,
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
            modal::paint(&component, theme, area, cx)
        };
        self.menu.note_layout(&layout);
        self.painted = Some((area, layout));
    }
}

/// Discover importers (none when listing fails).
pub async fn discover(services: ReviewServices) -> Vec<ImporterEntry> {
    let plugins = services
        .host()
        .plugins_serving(REVIEW_IMPORTER_INTERFACE_ID.to_owned())
        .await
        .unwrap_or_default();
    let mut entries = Vec::new();
    for plugin_id in plugins {
        let Ok(payload) = services
            .host()
            .invoke_plugin(
                plugin_id.clone(),
                REVIEW_IMPORTER_INTERFACE_ID.to_owned(),
                OP_REVIEW_IMPORTER_MANIFEST.to_owned(),
                b"{}".to_vec(),
            )
            .await
        else {
            continue;
        };
        if let Ok(manifest) = serde_json::from_slice::<ReviewImporterManifest>(&payload) {
            entries.push(ImporterEntry {
                manifest,
                plugin_id,
            });
        }
    }
    entries
}

/// What importing needs from the open review.
#[derive(Debug, Clone)]
pub struct ImportContext {
    /// Repository.
    pub repo_path: PathBuf,
    /// Durable workspace id; `None` keeps the snapshot in memory only.
    pub workspace_id: Option<String>,
    /// The review's pull request, when it is one pull request or stack layer.
    pub target: Option<(String, ReviewPullRequestTarget)>,
    /// Files to map onto, with the resolved source each belongs to, in document order.
    pub files: Vec<(ResolvedSource, ReviewIndexEntry)>,
}

/// Import with `importer`: read the files' hunks (bounded), ask the importer, and save the
/// snapshot (durable reviews).
///
/// # Errors
///
/// Returns the importer's or the save's error message.
pub async fn import(
    services: ReviewServices,
    context: ImportContext,
    importer: ImporterEntry,
    options: serde_json::Value,
) -> Result<ExternalReviewImportRecord, String> {
    let mut files = Vec::new();
    let mut lines = 0usize;
    for (source, entry) in context.files.iter().take(MAX_IMPORT_FILES) {
        if lines >= MAX_IMPORT_LINES {
            break;
        }
        let hunks: Option<ReviewFileHunksResponse> = services
            .call(ReviewFileHunksRequest {
                repo_path: context.repo_path.clone(),
                source: source.clone(),
                path: entry.path.clone(),
                old_path: entry.old_path.clone(),
                context_lines: None,
                ignore_whitespace: false,
                max_lines: None,
            })
            .await
            .ok();
        let Some(hunks) = hunks else {
            continue;
        };
        lines += hunks
            .hunks
            .iter()
            .map(|hunk| hunk.lines.len())
            .sum::<usize>();
        let deleted = entry.status == bcode_code_review_models::ReviewFileStatus::Deleted;
        files.push(ReviewFile {
            old_path: entry
                .old_path
                .clone()
                .or_else(|| deleted.then(|| entry.path.clone())),
            new_path: (!deleted).then(|| entry.path.clone()),
            status: entry.status,
            additions: entry.additions,
            deletions: entry.deletions,
            hunks: hunks.hunks,
            is_binary: hunks.binary,
        });
    }
    let request = ExternalReviewImportRequest {
        repo_root: context.repo_path.clone(),
        files,
        options,
        pull_request: context.target.as_ref().map(|(_, target)| target.clone()),
    };
    let payload = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
    let response = services
        .host()
        .invoke_plugin(
            importer.plugin_id.clone(),
            REVIEW_IMPORTER_INTERFACE_ID.to_owned(),
            OP_REVIEW_IMPORTER_IMPORT.to_owned(),
            payload,
        )
        .await
        .map_err(|error| error.message)?;
    let mut import: ExternalReviewImportResponse = serde_json::from_slice(&response)
        .map_err(|error| format!("unexpected import response: {error}"))?;
    if import.importer_id != importer.manifest.id {
        return Err(format!(
            "importer {} answered as {}",
            importer.manifest.id, import.importer_id
        ));
    }
    // Mapped anchors belong to the pull request's source.
    if let Some((source_id, _)) = &context.target {
        for thread in &mut import.threads {
            if let Some(anchor) = thread.mapping.anchor.as_mut() {
                anchor.source_id.get_or_insert_with(|| source_id.clone());
            }
        }
    }
    match &context.workspace_id {
        Some(workspace_id) => {
            let saved: SaveExternalReviewImportResponse = services
                .call(SaveExternalReviewImportRequest {
                    repo_path: context.repo_path.clone(),
                    workspace_id: workspace_id.clone(),
                    import,
                })
                .await
                .map_err(|error| error.message)?;
            Ok(saved.record)
        }
        None => Ok(ExternalReviewImportRecord {
            workspace_id: String::new(),
            importer_id: import.importer_id,
            threads: import.threads,
            warnings: import.warnings,
            updated_at_ms: 0,
        }),
    }
}
