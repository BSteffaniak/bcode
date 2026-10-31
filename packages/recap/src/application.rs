//! Renderer-neutral standalone recap application operations.
use crate::cli::{Create, ModelOptions, OutputOptions, Refine};
/// Semantic progress for an explicit application job, independent of presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Resolving the selected configuration and model, without starting inference.
    ModelReadiness,
    /// Acquiring source media and captions.
    Acquisition,
    /// Extracting visual evidence.
    Media,
    /// Running grounded model composition.
    Composition,
    /// Detailed source inventory and outline analysis.
    Analysis,
    /// Detailed chapter composition.
    Writing,
    /// Source coverage audit.
    Audit,
    /// Bounded source-grounded repair.
    Repair,
    /// Latest bounded aggregate analysis progress; no source content or identities.
    AnalysisProgress {
        /// Semantic analysis phase.
        phase: bcode_recap::diagnostics::Phase,
        /// Completed work units.
        completed: u64,
        /// Planned work units, when known.
        total: Option<u64>,
    },
    /// Committing a validated revision.
    Storage,
    /// Rendering offline HTML.
    Export,
    /// The requested operation has completed.
    Complete,
}
impl From<bcode_recap::diagnostics::Phase> for Stage {
    fn from(phase: bcode_recap::diagnostics::Phase) -> Self {
        use bcode_recap::diagnostics::Phase;
        match phase {
            Phase::Validation | Phase::ModelReadiness => Self::ModelReadiness,
            Phase::Metadata | Phase::Download | Phase::Captions | Phase::Acquisition => {
                Self::Acquisition
            }
            Phase::Probe | Phase::Extraction | Phase::Media => Self::Media,
            Phase::Composition | Phase::ValidationOutput => Self::Composition,
            Phase::Analysis => Self::Analysis,
            Phase::Writing => Self::Writing,
            Phase::Audit => Self::Audit,
            Phase::Repair => Self::Repair,
            Phase::Storage => Self::Storage,
            Phase::Export => Self::Export,
        }
    }
}

impl From<bcode_recap::engine::Stage> for Stage {
    fn from(stage: bcode_recap::engine::Stage) -> Self {
        match stage {
            bcode_recap::engine::Stage::Acquisition => Self::Acquisition,
            bcode_recap::engine::Stage::Media => Self::Media,
            bcode_recap::engine::Stage::Composition => Self::Composition,
            bcode_recap::engine::Stage::Storage => Self::Storage,
            bcode_recap::engine::Stage::Export => Self::Export,
            bcode_recap::engine::Stage::Complete => Self::Complete,
        }
    }
}
use bcode_recap::{
    composition::CompositionLimits,
    contracts::{GenerationIntent, RecapDocument},
    engine::{Engine, EngineOptions, GenerateRequest},
    export::{ExportBudget, ExportReport, export_html},
    media::{MediaBudget, MediaTools},
    model::ModelExecutor,
    process::{self, Cancellation, ProcessRequest},
    source::{self, AcquisitionOptions, SourceInput},
    storage::DocumentStore,
};
use std::{
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    time::Duration,
};
/// Initial UI state. Browsing never requires inference configuration.
#[derive(Debug, Clone, clap::Args)]
pub struct TuiOptions {
    #[arg(skip = PathBuf::from(".recap"))]
    pub store: PathBuf,
    #[arg(skip = 8_u64 * 1024 * 1024)]
    pub max_document_bytes: u64,
    /// Explicit secret-safe report destination; never overwritten or written implicitly.
    #[arg(long)]
    pub diagnostic_report: Option<PathBuf>,
    #[arg(long)]
    pub source: Option<String>,
    #[arg(long)]
    pub model_config: Option<PathBuf>,
    #[arg(long)]
    pub profile: Option<String>,
    #[arg(long)]
    pub context: Option<String>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub provider: Option<String>,
    #[arg(long)]
    pub endpoint: Option<String>,
    #[arg(long)]
    pub auth_profile: Option<String>,
    #[arg(long, value_parser = crate::cli::document_id)]
    pub id: Option<String>,
    /// Export policy shared by form-created jobs; defaults come from the CLI schema.
    #[cfg(feature = "tui")]
    #[arg(skip)]
    pub output: OutputOptions,
}
impl Default for TuiOptions {
    fn default() -> Self {
        Self {
            store: ".recap".into(),
            max_document_bytes: 8 * 1024 * 1024,
            diagnostic_report: None,
            source: None,
            model_config: None,
            profile: None,
            context: None,
            model: None,
            provider: None,
            endpoint: None,
            auth_profile: None,
            id: None,
            #[cfg(feature = "tui")]
            output: OutputOptions::default(),
        }
    }
}

#[cfg(feature = "tui")]
impl TuiOptions {
    /// Build a create job from a possibly incomplete form. Execution validates before inference.
    #[must_use]
    pub fn create_request(
        &self,
        source: String,
        transcript: Option<PathBuf>,
        focus: Option<String>,
        depth: bcode_recap::contracts::Depth,
    ) -> Create {
        let mut request = Create::defaults_for_form();
        self.apply_model_selection(&mut request.model);
        request.source = source;
        request.transcript = transcript;
        request.focus = focus;
        request.depth = depth;
        request.id.clone_from(&self.id);
        request.output = self.output_options();
        request
    }

    /// Build a refinement using the same budgets and defaults as the headless command.
    #[must_use]
    pub fn refine_request(&self, id: String, focus: String) -> Refine {
        let mut request = Refine::defaults_for_form();
        self.apply_model_selection(&mut request.model);
        request.id = id;
        request.focus = focus;
        request.output = self.output_options();
        request
    }

    /// Safe output defaults: export never overwrites without explicit opt-in.
    #[must_use]
    pub fn output_options(&self) -> OutputOptions {
        self.output.clone()
    }

    fn apply_model_selection(&self, model: &mut ModelOptions) {
        model.model_config = self.model_config.clone().unwrap_or_default();
        model.profile.clone_from(&self.profile);
        model.context.clone_from(&self.context);
        model.model.clone_from(&self.model);
        model.provider.clone_from(&self.provider);
        model.endpoint.clone_from(&self.endpoint);
        model.auth_profile.clone_from(&self.auth_profile);
    }
}

/// A single operation, run directly against the same embedded engine as the CLI.
#[derive(Debug)]
pub enum Request {
    Resume(Box<crate::cli::Resume>),
    CheckSource {
        source: String,
        tools: crate::cli::Tools,
    },
    #[cfg(feature = "tui")]
    CreateAndExport(Box<Create>),
    #[cfg(feature = "tui")]
    RefineAndExport(Box<Refine>),
    Create(Box<Create>),
    Refine(Box<Refine>),
    #[cfg(feature = "tui")]
    ExportCopy {
        id: String,
        options: OutputOptions,
    },
    Export {
        id: String,
        options: OutputOptions,
    },
    #[cfg(feature = "tui")]
    Load {
        id: String,
    },
    #[cfg(feature = "tui")]
    List {
        cursor: Option<CatalogCursor>,
        limit: usize,
    },
    /// Opening is always explicit; exports never launch external applications.
    #[cfg(feature = "tui")]
    Open {
        path: PathBuf,
    },
}
/// A request-scoped duration approval. No preference or declarative config is changed.
#[cfg(feature = "tui")]
#[derive(Debug)]
pub struct DurationApproval {
    pub request: Box<Create>,
    pub actual_seconds: u64,
    pub limit_seconds: u64,
}
#[cfg(feature = "tui")]
impl DurationApproval {
    pub fn approve(mut self) -> Box<Create> {
        self.request.budget.max_duration_seconds = self.actual_seconds;
        self.request
    }
    pub fn message(&self) -> String {
        let time = |seconds: u64| {
            format!(
                "{}h {}m {}s",
                seconds / 3600,
                seconds % 3600 / 60,
                seconds % 60
            )
        };
        format!(
            "This video is {}. Your current duration limit is {}. Run it anyway? Longer videos can take more time, disk space and AI usage. Only this video's duration allowance will change; other limits stay in place.",
            time(self.actual_seconds),
            time(self.limit_seconds)
        )
    }
}
#[derive(Debug)]
pub enum Outcome {
    #[cfg(feature = "tui")]
    DurationApproval(DurationApproval),
    Checked,
    Ready {
        document: Box<RecapDocument>,
        export: Result<ExportResult, String>,
    },
    Document(Box<RecapDocument>),
    Exported(ExportResult),
    #[cfg(feature = "tui")]
    Catalog(CatalogPage),
    #[cfg(feature = "tui")]
    Opened,
}
#[derive(Debug)]
pub struct ExportResult {
    pub path: PathBuf,
    pub report: ExportReport,
}
/// Ephemeral directory cursor. No index is written or repaired; order is filesystem order.
#[cfg(feature = "tui")]
#[derive(Debug)]
pub struct CatalogCursor {
    root: PathBuf,
    entries: std::fs::ReadDir,
}
#[cfg(feature = "tui")]
#[derive(Debug)]
pub struct CatalogPage {
    pub entries: Vec<CatalogEntry>,
    pub next: Option<CatalogCursor>,
    /// Includes non-document entries, so callers can distinguish an empty page from EOF.
    pub examined: usize,
}
#[cfg(feature = "tui")]
#[derive(Debug)]
pub struct CatalogEntry {
    pub id: String,
    pub title: String,
    pub revision: Option<u64>,
    /// Damaged entries remain visible, never repaired or silently omitted.
    pub error: Option<String>,
}

const fn reportable(request: &Request) -> bool {
    match request {
        Request::Resume(_)
        | Request::Create(_)
        | Request::Refine(_)
        | Request::Export { .. }
        | Request::CheckSource { .. } => true,
        #[cfg(feature = "tui")]
        Request::CreateAndExport(_) | Request::RefineAndExport(_) | Request::ExportCopy { .. } => {
            true
        }
        #[cfg(feature = "tui")]
        _ => false,
    }
}

/// Retain the caller observer and coalesce count updates into one fixed-size slot.
struct ProgressObserver {
    parent: Cancellation,
    operation: bcode_recap::diagnostics::Operation,
    latest: std::sync::Arc<std::sync::Mutex<Option<Stage>>>,
}
impl std::fmt::Debug for ProgressObserver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProgressObserver")
            .finish_non_exhaustive()
    }
}
impl bcode_recap::diagnostics::Observer for ProgressObserver {
    fn activity(&self, activity: bcode_recap::activity::Activity) {
        self.parent.activity(activity);
    }
    fn measure(&self, measurement: bcode_recap::diagnostics::Measurement) {
        self.parent.measure(measurement);
        bcode_recap::diagnostics::Observer::measure(&self.operation, measurement);
    }
    fn observe(&self, event: bcode_recap::diagnostics::Event) {
        if let bcode_recap::diagnostics::Event::AnalysisProgress {
            phase,
            completed,
            total,
        } = &event
        {
            *self
                .latest
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(Stage::AnalysisProgress {
                    phase: *phase,
                    completed: *completed,
                    total: *total,
                });
        }
        self.parent.emit(event.clone());
        bcode_recap::diagnostics::Observer::observe(&self.operation, event);
    }
}

/// Execute with cooperative cancellation and bounded semantic progress callbacks.
/// Create/refine return the committed document; export is a separate UI action.
pub async fn execute(
    options: &TuiOptions,
    request: Request,
    cancel: &Cancellation,
    progress: &mut (impl FnMut(Stage) + Send),
) -> Result<Outcome, Error> {
    use bcode_recap::diagnostics::{Event, Observer as _, Operation, Phase, Status};
    let identity = tempfile::Builder::new().prefix("operation-").tempdir()?;
    let id = identity
        .path()
        .file_name()
        .ok_or_else(|| Error::Action("cannot allocate operation ID".into()))?
        .to_string_lossy()
        .into_owned();
    let operation = Operation::new(id);
    let latest = std::sync::Arc::new(std::sync::Mutex::new(None));
    let cancel = cancel.observed(std::sync::Arc::new(ProgressObserver {
        parent: cancel.clone(),
        operation: operation.clone(),
        latest: latest.clone(),
    }));
    cancel.emit(Event::Phase {
        phase: Phase::Validation,
    });
    let report_path = reportable(&request)
        .then_some(options.diagnostic_report.as_deref())
        .flatten();
    let progress = std::sync::Mutex::new(progress);
    let result = {
        let mut forward = |stage| {
            let counts = latest
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(counts) = counts {
                progress
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)(counts);
            }
            progress
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)(stage);
        };
        let work = async {
            if let Some(path) = report_path {
                check_report_destination(path)?;
            }
            execute_inner(options, request, &cancel, &mut forward).await
        };
        tokio::pin!(work);
        loop {
            tokio::select! {
                result = &mut work => break result,
                () = tokio::time::sleep(Duration::from_millis(25)) => {
                    let counts = latest.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
                    if let Some(counts) = counts {
                        progress.lock().unwrap_or_else(std::sync::PoisonError::into_inner)(counts);
                    }
                }
            }
        }
    };
    let counts = latest
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(counts) = counts {
        progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)(counts);
    }
    let (status, code) = match &result {
        #[cfg(feature = "tui")]
        Ok(Outcome::DurationApproval(_)) => {
            (Status::ApprovalRequired, "duration_approval_required")
        }
        Ok(Outcome::Ready { export: Err(_), .. }) => (Status::ExportFailed, "export_failed"),
        Ok(_) => (Status::Succeeded, "ok"),
        Err(_) if cancel.is_cancelled() => (Status::Cancelled, "cancelled"),
        Err(error) => (Status::Failed, diagnostic_code(error)),
    };
    operation.observe(Event::Terminal {
        status,
        code: code.into(),
    });
    let report = operation.report();
    let report_path = report_path.filter(|_| !approval_pending(&result));
    if let Some(path) = report_path {
        // Explicit destination, atomic no-clobber write: never append unbounded logs or overwrite files.
        let saved = serde_json::to_vec_pretty(&report)
            .map_err(Error::Json)
            .and_then(|bytes| write_output(path, &bytes, false));
        if saved.is_err() {
            return Err(Error::DiagnosticWrite {
                report: Box::new(report),
                source: result.err().map(Box::new),
            });
        }
    }
    result.map_err(|source| Error::Operation {
        report: Box::new(report),
        source: Box::new(source),
    })
}

const fn approval_pending(result: &Result<Outcome, Error>) -> bool {
    #[cfg(feature = "tui")]
    {
        matches!(result, Ok(Outcome::DurationApproval(_)))
    }
    #[cfg(not(feature = "tui"))]
    {
        let _ = result;
        false
    }
}

async fn execute_inner(
    options: &TuiOptions,
    request: Request,
    cancel: &Cancellation,
    progress: &mut (impl FnMut(Stage) + Send),
) -> Result<Outcome, Error> {
    check_cancel(cancel)?;
    #[cfg(feature = "tui")]
    let interactive = matches!(&request, Request::CreateAndExport(_));
    let (request, automatic_export) = match request {
        #[cfg(feature = "tui")]
        Request::CreateAndExport(mut value) => {
            value.output = available_output(&options.store, value.id.as_deref(), &value.output);
            let output = value.output.clone();
            (Request::Create(value), Some(output))
        }
        #[cfg(feature = "tui")]
        Request::RefineAndExport(mut value) => {
            value.output = available_output(&options.store, Some(&value.id), &value.output);
            let output = value.output.clone();
            (Request::Refine(value), Some(output))
        }
        Request::Resume(value) => {
            let output = value.output.clone();
            (Request::Resume(value), Some(output))
        }
        Request::Create(value) => {
            let output = value.output.clone();
            (Request::Create(value), Some(output))
        }
        Request::Refine(value) => {
            let output = value.output.clone();
            (Request::Refine(value), Some(output))
        }
        #[cfg(feature = "tui")]
        Request::ExportCopy {
            id,
            options: output,
        } => {
            let output = available_output(&options.store, Some(&id), &output);
            (
                Request::Export {
                    id,
                    options: output,
                },
                None,
            )
        }
        other => (other, None::<OutputOptions>),
    };
    #[cfg(feature = "tui")]
    let approval_request = if let Request::Create(value) = &request {
        Some(value.clone())
    } else {
        None
    };
    let store = DocumentStore::new(&options.store, options.max_document_bytes);
    let result = execute_job(options, &store, request, cancel, progress).await;
    #[cfg(feature = "tui")]
    if let (true, Err(error), Some(request)) = (interactive, &result, approval_request)
        && let Some((actual_seconds, limit_seconds)) = error.duration_limit()
        && !cancel.is_cancelled()
    {
        return Ok(Outcome::DurationApproval(DurationApproval {
            request,
            actual_seconds,
            limit_seconds,
        }));
    }
    let outcome = result?;
    if let Some(output) = automatic_export
        && let Outcome::Document(document) = outcome
    {
        cancel.emit(bcode_recap::diagnostics::Event::Phase {
            phase: bcode_recap::diagnostics::Phase::Export,
        });
        progress(Stage::Export);
        let export = export_document(&store, &document, &output).map_err(|_| {
            "Recap saved, but HTML export failed. Choose another destination and export again."
                .into()
        });
        return Ok(Outcome::Ready { document, export });
    }
    Ok(outcome)
}

async fn execute_job(
    options: &TuiOptions,
    store: &DocumentStore,
    request: Request,
    cancel: &Cancellation,
    progress: &mut (impl FnMut(Stage) + Send),
) -> Result<Outcome, Error> {
    #[cfg(not(feature = "tui"))]
    let _ = options;
    match request {
        Request::CheckSource { source, tools } => {
            check_tool(&tools.yt_dlp, "yt-dlp", cancel).await?;
            let input = SourceInput::Youtube(source::youtube_url(&source)?);
            let mut config = engine_options(std::env::temp_dir());
            config.acquisition.yt_dlp = tools.yt_dlp;
            let _source = source::acquire(&input, &config.acquisition, cancel).await?;
            Ok(Outcome::Checked)
        }
        #[cfg(feature = "tui")]
        Request::CreateAndExport(_) | Request::RefineAndExport(_) | Request::ExportCopy { .. } => {
            unreachable!("export jobs normalized")
        }
        Request::Resume(value) => resume_job(*value, store, cancel, progress).await,
        Request::Create(value) => create_job(*value, store, cancel, progress).await,
        Request::Refine(value) => {
            crate::cli::document_id(&value.id).map_err(Error::Action)?;
            crate::cli::nonempty(&value.focus).map_err(Error::Action)?;
            cancel.emit(bcode_recap::diagnostics::Event::Phase {
                phase: bcode_recap::diagnostics::Phase::ModelReadiness,
            });
            progress(Stage::ModelReadiness);
            let executor = observed_model(&value.model, cancel).await?;
            executor.observe_selection(cancel);
            check_cancel(cancel)?;
            let result = refine(store.clone(), &value, &executor, cancel, progress)
                .await
                .map(|mut document| {
                    document.remove_generation_notes();
                    Outcome::Document(Box::new(document))
                })
                .map_err(|error| model_job_error(error, &executor));
            drop(executor);
            result
        }
        #[cfg(feature = "tui")]
        Request::Load { id } => store
            .load(&id)
            .map(|mut document| {
                document.remove_generation_notes();
                Outcome::Document(Box::new(document))
            })
            .map_err(Error::from),
        Request::Export { id, options } => {
            let document = store.load(&id)?;
            cancel.emit(bcode_recap::diagnostics::Event::Phase {
                phase: bcode_recap::diagnostics::Phase::Export,
            });
            progress(Stage::Export);
            let result = export_document(store, &document, &options)?;
            progress(Stage::Complete);
            Ok(Outcome::Exported(result))
        }
        #[cfg(feature = "tui")]
        Request::List { cursor, limit } => {
            list(options, cursor, limit, cancel).map(Outcome::Catalog)
        }
        #[cfg(feature = "tui")]
        Request::Open { path } => {
            let path = std::fs::canonicalize(path)?;
            if !path.is_file()
                || path
                    .extension()
                    .is_none_or(|value| !value.eq_ignore_ascii_case("html"))
            {
                return Err(Error::Action(
                    "select an existing exported HTML file to open".into(),
                ));
            }
            let request = ProcessRequest {
                program: if cfg!(target_os = "macos") {
                    "open"
                } else if cfg!(target_os = "windows") {
                    return Err(Error::Action(
                        "open the exported HTML file in your browser".into(),
                    ));
                } else {
                    "xdg-open"
                }
                .into(),
                args: vec![path.into_os_string()],
                cwd: None,
                timeout: Duration::from_secs(15),
                max_output_bytes: 4096,
                file_budget: None,
            };
            process::run(&request, cancel).await.map_err(|_| {
                Error::Action(
                    "browser could not be opened; open the exported HTML file manually".into(),
                )
            })?;
            Ok(Outcome::Opened)
        }
    }
}
async fn resume_job(
    value: crate::cli::Resume,
    store: &DocumentStore,
    cancel: &Cancellation,
    progress: &mut (impl FnMut(Stage) + Send),
) -> Result<Outcome, Error> {
    let root = store.generation_root();
    let configuration = bcode_recap::generation::Journal::configuration(root, &value.id)
        .map_err(|error| Error::Action(error.to_string()))?;
    progress(Stage::ModelReadiness);
    let saved = bcode_recap::generation::Journal::model_configuration(root, &value.id)
        .map_err(|error| Error::Action(error.to_string()))?;
    let executor = if let Some(saved) = saved {
        let mut options = saved.options.clone();
        value.model.apply(&mut options);
        crate::bcode_adapter::BcodeExecutor::configured_resume(saved, options, value.model.model_config.is_some())
            .await.map_err(|_| Error::Action("Saved model configuration is unavailable or changed. Restore its configuration/authentication references, or explicitly override --model-config. No model request was made.".into()))?
    } else {
        let mut options = model_options(&ModelOptions::defaults_for_resume());
        value.model.apply(&mut options);
        let status = bcode_recap::generation::Journal::inspect(root, &value.id)
            .map_err(|error| Error::Action(error.to_string()))?;
        let identity = status.executor.as_deref().ok_or_else(|| {
            Error::Action("Legacy generation has no verifiable model identity".into())
        })?;
        crate::bcode_adapter::BcodeExecutor::configured_legacy(options, identity)
            .await.map_err(|_| Error::Action("This older generation saved only a model identity hash, not its settings. Current settings do not match. Supply the original model options once to adopt verified resume settings; no checkpoints were changed or model requests made.".into()))?
    };
    let executor = if let Some(observer) = cancel.observer() {
        executor.with_observer(observer)
    } else {
        executor
    };
    executor.observe_selection(cancel);
    let engine = Engine {
        executor: &executor,
        store: store.clone(),
        options: configuration,
    };
    let document = engine
        .resume_generation(
            &value.id,
            cancel,
            &mut |stage| progress(stage.into()),
            value.retry_uncertain,
        )
        .await?;
    drop(executor);
    Ok(Outcome::Document(Box::new(document)))
}

async fn create_job(
    value: Create,
    store: &DocumentStore,
    cancel: &Cancellation,
    progress: &mut (impl FnMut(Stage) + Send),
) -> Result<Outcome, Error> {
    crate::cli::nonempty(&value.source).map_err(Error::Action)?;
    if value.source.starts_with("https://") {
        source::youtube_url(&value.source)?;
    } else {
        source::local_video(Path::new(&value.source), value.budget.max_video_bytes)?;
    }
    if let Some(id) = &value.id {
        crate::cli::document_id(id).map_err(Error::Action)?;
    }
    if let Some(focus) = &value.focus {
        crate::cli::nonempty(focus).map_err(Error::Action)?;
    }
    cancel.emit(bcode_recap::diagnostics::Event::Phase {
        phase: bcode_recap::diagnostics::Phase::ModelReadiness,
    });
    progress(Stage::ModelReadiness);
    let executor = observed_model(&value.model, cancel).await?;
    executor.observe_selection(cancel);
    check_cancel(cancel)?;
    let result = create(store.clone(), &value, &executor, cancel, progress)
        .await
        .map(|mut document| {
            document.remove_generation_notes();
            Outcome::Document(Box::new(document))
        })
        .map_err(|error| model_job_error(error, &executor));
    drop(executor);
    result
}

#[cfg(feature = "tui")]
fn available_output(store: &Path, id: Option<&str>, requested: &OutputOptions) -> OutputOptions {
    let mut output = requested.clone();
    if output.output.is_some() {
        return output;
    }
    let name = id.map_or_else(
        || {
            format!(
                "recap-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
            )
        },
        str::to_owned,
    );
    for suffix in 0..10_000 {
        let file = if suffix == 0 {
            format!("{name}.html")
        } else {
            format!("{name}-{suffix}.html")
        };
        let candidate = store.join(file);
        if !candidate.exists() {
            output.output = Some(candidate);
            return output;
        }
    }
    // A colliding final path still fails through the normal no-clobber write contract.
    output.output = Some(store.join(format!("{name}.html")));
    output
}

fn model_job_error(error: Error, executor: &crate::bcode_adapter::BcodeExecutor) -> Error {
    executor.failure().map_or(error, Error::ModelExecution)
}
fn check_cancel(cancel: &Cancellation) -> Result<(), Error> {
    if cancel.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}
#[cfg(feature = "tui")]
fn list(
    options: &TuiOptions,
    cursor: Option<CatalogCursor>,
    limit: usize,
    cancel: &Cancellation,
) -> Result<CatalogPage, Error> {
    if !(1..=64).contains(&limit) {
        return Err(Error::Action(
            "catalog page limit must be between 1 and 64".into(),
        ));
    }
    let mut cursor = if let Some(cursor) = cursor {
        if cursor.root != options.store {
            return Err(Error::Action(
                "catalog cursor belongs to a different store; refresh the catalog".into(),
            ));
        }
        cursor
    } else {
        match std::fs::symlink_metadata(&options.store) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CatalogPage {
                    entries: Vec::new(),
                    next: None,
                    examined: 0,
                });
            }
            Err(error) => return Err(error.into()),
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(Error::Action(
                    "catalog store must be a regular private directory, not a symlink".into(),
                ));
            }
            Ok(_) => {}
        }
        CatalogCursor {
            root: options.store.clone(),
            entries: std::fs::read_dir(&options.store)?,
        }
    };
    let store = DocumentStore::new(&options.store, options.max_document_bytes);
    let mut entries = Vec::new();
    let mut examined = 0;
    for _ in 0..limit {
        check_cancel(cancel)?;
        let Some(entry) = cursor.entries.next() else {
            return Ok(CatalogPage {
                entries,
                next: None,
                examined,
            });
        };
        examined += 1;
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
            continue;
        };
        match store.load(id) {
            Ok(document) => entries.push(CatalogEntry {
                id: document.id,
                title: document.title,
                revision: Some(document.revision),
                error: None,
            }),
            Err(error) => entries.push(CatalogEntry {
                id: id.into(),
                title: id.into(),
                revision: None,
                error: Some(error.to_string()),
            }),
        }
    }
    Ok(CatalogPage {
        entries,
        next: Some(cursor),
        examined,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{message}", message = operation_message(report, source))]
    Operation {
        report: Box<bcode_recap::diagnostics::Report>,
        source: Box<Self>,
    },
    #[error("{message}", message = diagnostic_write_message(report, source.as_deref()))]
    DiagnosticWrite {
        report: Box<bcode_recap::diagnostics::Report>,
        source: Option<Box<Self>>,
    },
    #[error("model operation failed ({code}); generation progress is retained", code = .0.code())]
    ModelExecution(crate::bcode_adapter::ModelFailure),
    #[error("output {} already exists; choose another export destination or use --output FILE / --force in the CLI", .0.display())]
    OutputExists(PathBuf),
    #[error("operation cancelled")]
    Cancelled,
    #[error("{0}")]
    Action(String),
    #[error(transparent)]
    Document(#[from] bcode_recap::contracts::Error),
    #[error("file operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("document serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Engine(#[from] bcode_recap::engine::EngineError),
    #[error(transparent)]
    Source(#[from] bcode_recap::source::SourceError),
    #[error(transparent)]
    Export(#[from] bcode_recap::export::ExportError),
}

impl Error {
    #[cfg(feature = "tui")]
    fn duration_limit(&self) -> Option<(u64, u64)> {
        use bcode_recap::{engine::EngineError, source::SourceError};
        match self {
            Self::Source(SourceError::DurationExceeded {
                actual_seconds,
                limit_seconds,
            })
            | Self::Engine(EngineError::Source(SourceError::DurationExceeded {
                actual_seconds,
                limit_seconds,
            })) => Some((*actual_seconds, *limit_seconds)),
            Self::Operation { source, .. } => source.duration_limit(),
            _ => None,
        }
    }
}

fn check_report_destination(path: &Path) -> Result<(), Error> {
    if path.symlink_metadata().is_ok() {
        return Err(Error::Action(
            "diagnostic destination already exists; choose a new --diagnostic-report file".into(),
        ));
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    tempfile::NamedTempFile::new_in(parent).map_err(|_| {
        Error::Action(
            "diagnostic destination is not writable; choose another --diagnostic-report file"
                .into(),
        )
    })?;
    Ok(())
}
fn diagnostic_write_message(
    report: &bcode_recap::diagnostics::Report,
    source: Option<&Error>,
) -> String {
    let original = source.map_or_else(
        || format!("Operation completed; committed={}", report.committed()),
        |error| operation_message(report, error),
    );
    format!(
        "{original} · diagnostic report could not be written; choose a new writable --diagnostic-report file · diagnostic {}",
        report.operation_id
    )
}

fn safe_error(error: &Error) -> String {
    match error {
        Error::ModelExecution(failure) => {
            use crate::bcode_adapter::ModelFailure;
            let guidance = match failure {
                ModelFailure::ProviderAuth => {
                    "Check the selected provider authentication/profile and configured auth pool; account failover is provider-managed"
                }
                ModelFailure::Configuration => {
                    "Check model config, profile/context, relative config references and saved Settings selection; initialization did not complete"
                }
                ModelFailure::Timeout => {
                    "Model operation exceeded its deadline; inspect before explicitly retrying"
                }
                ModelFailure::RateLimited => {
                    "Provider reported a rate limit; wait before explicitly retrying"
                }
                ModelFailure::ProviderNetwork => {
                    "Provider network request failed; check connectivity"
                }
                ModelFailure::ProviderExecutionUnverified | ModelFailure::ProviderAfterOutput => {
                    "Provider execution outcome requires review; do not assume there were no effects or retry blindly"
                }
                ModelFailure::StructuredJson
                | ModelFailure::StructuredSchema
                | ModelFailure::StructuredDecode
                | ModelFailure::StructuredRepair => {
                    "Structured model output failed validation; no fabricated fallback was saved"
                }
                _ => {
                    "Check provider availability and selected model capabilities; no automatic retry was performed"
                }
            };
            format!("model operation failed ({}); {guidance}", failure.code())
        }
        Error::Io(_) | Error::Engine(bcode_recap::engine::EngineError::Io(_)) => {
            "File operation failed; check workspace/store access and free disk space".into()
        }
        Error::Json(_) => {
            "Invalid document serialization; no raw content is included in diagnostics".into()
        }
        _ => error
            .to_string()
            .chars()
            .filter(|c| !c.is_control())
            .take(1800)
            .collect(),
    }
}

fn operation_message(report: &bcode_recap::diagnostics::Report, source: &Error) -> String {
    let export_only = report.phase() == Some(bcode_recap::diagnostics::Phase::Export)
        && !report.inference_started()
        && !report.committed();
    let facts = if export_only {
        "Export-only operation; no model request was needed."
    } else if report.inference_started() {
        "Model executor was invoked; provider effects/billing are not inferred."
    } else {
        "No model request was made."
    };
    let committed = if export_only {
        "The saved recap was not modified by this export attempt."
    } else if report.committed() {
        "A revision was committed; inspect it before retrying."
    } else {
        "No revision commit was observed."
    };
    format!(
        "{} · code {} · phase {:?} · {} · {} · diagnostic {}",
        safe_error(source),
        diagnostic_code(source),
        report.phase(),
        facts,
        committed,
        report.operation_id
    )
}
fn diagnostic_code(error: &Error) -> &'static str {
    use bcode_recap::{
        engine::EngineError,
        process::ProcessError,
        source::{AcquisitionFailure, SourceError},
    };
    match error {
        Error::Source(SourceError::Acquisition { failure, .. })
        | Error::Engine(EngineError::Source(SourceError::Acquisition { failure, .. })) => {
            match failure {
                AcquisitionFailure::Forbidden => "youtube_http_403",
                AcquisitionFailure::RateLimited => "youtube_http_429",
                AcquisitionFailure::AccessRequired => "youtube_access_required",
                AcquisitionFailure::FormatUnavailable => "youtube_format_unavailable",
                AcquisitionFailure::ChallengeRuntime => "youtube_challenge_runtime",
                AcquisitionFailure::Unknown => "youtube_download_failed",
            }
        }
        Error::Engine(EngineError::Source(SourceError::Process(ProcessError::Timeout))) => {
            "acquisition_timeout"
        }
        Error::Source(SourceError::Invalid("no timestamped captions"))
        | Error::Engine(EngineError::Source(SourceError::Invalid("no timestamped captions"))) => {
            "captions_no_text"
        }
        Error::Source(SourceError::Invalid("reversed or zero-duration caption"))
        | Error::Engine(EngineError::Source(SourceError::Invalid(
            "reversed or zero-duration caption",
        ))) => "captions_invalid_timing",
        Error::Source(SourceError::DurationExceeded { .. })
        | Error::Engine(EngineError::Source(SourceError::DurationExceeded { .. })) => {
            "source_duration_limit"
        }
        Error::ModelExecution(failure) => failure.code(),
        Error::Cancelled | Error::Engine(EngineError::Cancelled) => "cancelled",
        Error::Engine(EngineError::Composition(_)) => "composition_failed",
        Error::Engine(EngineError::Media(_)) => "media_failed",
        Error::OutputExists(_) => "export_destination_exists",
        Error::Export(_) => "export_failed",
        Error::Io(_) => "filesystem_failed",
        _ => "operation_failed",
    }
}

pub fn export_document(
    store: &DocumentStore,
    document: &RecapDocument,
    options: &OutputOptions,
) -> Result<ExportResult, Error> {
    let root = store.asset_directory(&document.id)?;
    let result = export_html(
        document,
        &root,
        ExportBudget {
            max_asset_bytes: options.max_export_asset_bytes,
            max_total_media_bytes: options.max_export_media_bytes,
            ..ExportBudget::default()
        },
    )?;
    let destination = output_path(&document.id, options);
    write_output(&destination, result.html.as_bytes(), options.force)?;
    Ok(ExportResult {
        path: destination,
        report: result.report,
    })
}

pub fn export_committed(
    store: &DocumentStore,
    document: &RecapDocument,
    options: &OutputOptions,
) -> Result<ExportResult, Error> {
    export_document(store, document, options).map_err(|error| Error::Action(format!(
        "recap {} revision {} was committed, but HTML export failed: {error}. The saved revision is intact; retry recap export {} --output FILE",
        document.id, document.revision, document.id
    )))
}

pub fn output_path(id: &str, options: &OutputOptions) -> PathBuf {
    options
        .output
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{id}.html")))
}

// Stage output beside its destination so failures never truncate an existing HTML file.
pub fn write_output(path: &Path, bytes: &[u8], force: bool) -> Result<(), Error> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    staged.write_all(bytes)?;
    staged.as_file().sync_all()?;
    if force {
        staged
            .persist(path)
            .map_err(|error| Error::Io(error.error))?;
    } else {
        staged.persist_noclobber(path).map_err(|error| {
            if error.error.kind() == std::io::ErrorKind::AlreadyExists {
                Error::OutputExists(path.to_owned())
            } else {
                Error::Io(error.error)
            }
        })?;
    }
    Ok(())
}

pub async fn check_tool(
    program: &Path,
    name: &str,
    cancellation: &Cancellation,
) -> Result<(), Error> {
    use bcode_recap::diagnostics::{Event, Tool};
    let request = ProcessRequest {
        program: program.to_owned(),
        args: vec![
            if name == "yt-dlp" {
                "--version"
            } else {
                "-version"
            }
            .into(),
        ],
        cwd: None,
        timeout: Duration::from_secs(10),
        max_output_bytes: 64 * 1024,
        file_budget: None,
    };
    if cancellation.is_cancelled() {
        return Err(Error::Action("operation cancelled".into()));
    }
    let output = process::run(&request, cancellation).await.map_err(|_| Error::Action(format!(
        "{name} unavailable or failed its version check; install {name} or select its executable with --{name}"
    )))?;
    let tool = match name {
        "yt-dlp" => Tool::YtDlp,
        "ffmpeg" => Tool::Ffmpeg,
        _ => Tool::Ffprobe,
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text
        .split_whitespace()
        .find(|word| {
            word.bytes().next().is_some_and(|b| b.is_ascii_digit())
                && word.len() <= 32
                && word
                    .bytes()
                    .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-'))
        })
        .unwrap_or("unknown");
    cancellation.emit(Event::ToolVersion {
        tool,
        version: version.into(),
    });
    Ok(())
}

pub fn model_options(options: &ModelOptions) -> crate::bcode_adapter::BcodeOptions {
    crate::bcode_adapter::BcodeOptions {
        config_path: options.model_config.clone(),
        profile: options.profile.clone(),
        context: options.context.clone(),
        model: options.model.clone(),
        provider: options.provider.clone(),
        endpoint: options.endpoint.clone(),
        auth_profile: options.auth_profile.clone(),
        max_output_tokens: options.max_output_tokens,
        timeout: Duration::from_secs(options.model_timeout_seconds),
    }
}

async fn observed_model(
    options: &ModelOptions,
    cancel: &Cancellation,
) -> Result<crate::bcode_adapter::BcodeExecutor, Error> {
    let executor = configured_model(options).await?;
    Ok(if let Some(observer) = cancel.observer() {
        executor.with_observer(observer)
    } else {
        executor
    })
}

pub async fn configured_model(
    options: &ModelOptions,
) -> Result<crate::bcode_adapter::BcodeExecutor, Error> {
    preflight_model(options)?;
    crate::bcode_adapter::BcodeExecutor::configured(model_options(options))
        .await
        .map_err(Error::ModelExecution)
}

pub fn preflight_model(options: &ModelOptions) -> Result<(), Error> {
    if options.model_config.as_os_str().is_empty() {
        return Ok(());
    }
    if !std::fs::metadata(&options.model_config)
        .map_err(|_| {
            Error::Action(format!(
                "cannot read model configuration {}; supply an existing --model-config FILE",
                options.model_config.display()
            ))
        })?
        .is_file()
    {
        return Err(Error::Action(
            "--model-config must name a regular configuration file".into(),
        ));
    }
    for (name, value) in [
        ("profile", &options.profile),
        ("model", &options.model),
        ("provider", &options.provider),
        ("context", &options.context),
    ] {
        if value.as_ref().is_some_and(|value| value.trim().is_empty()) {
            return Err(Error::Action(format!("--{name} must not be blank")));
        }
    }
    Ok(())
}

pub fn check_output_destination(id: &str, options: &OutputOptions) -> Result<(), Error> {
    let path = output_path(id, options);
    if !options.force && path.try_exists()? {
        return Err(Error::Action(format!(
            "output {} already exists; choose --output or pass --force",
            path.display()
        )));
    }
    Ok(())
}

pub fn engine_options(workspace: PathBuf) -> EngineOptions {
    EngineOptions {
        acquisition: AcquisitionOptions {
            yt_dlp: "yt-dlp".into(),
            workspace,
            max_video_bytes: 0,
            max_duration_seconds: 0.0,
            caption_language: "en".into(),
            timeout: Duration::ZERO,
        },
        transcription: None,
        tools: MediaTools::default(),
        media: MediaBudget::default(),
        composition: CompositionLimits::default(),
        detailed: bcode_recap::analysis_contracts::DetailedPolicy::default(),
        inspection: bcode_recap::media::InspectionBudget::default(),
        export: ExportBudget::default(),
    }
}

pub fn read_transcript(path: &Path) -> Result<String, Error> {
    const LIMIT: u64 = 2 * 1024 * 1024;
    if !std::fs::metadata(path)?.is_file() {
        return Err(Error::Action(
            "--transcript must name a regular VTT/SRT file".into(),
        ));
    }
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > LIMIT {
        return Err(Error::Action(
            "transcript exceeds the 2 MiB limit; supply a bounded VTT/SRT transcript".into(),
        ));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| Error::Action("transcript must be UTF-8 VTT/SRT".into()))?;
    source::parse_subtitles(&text)?;
    Ok(text)
}

pub fn apply_composition_limits(
    options: &mut EngineOptions,
    model: &ModelOptions,
) -> Result<(), Error> {
    let convert = |value| {
        usize::try_from(value)
            .map_err(|_| Error::Action("model byte budget exceeds platform limits".into()))
    };
    options.composition.input_bytes = convert(model.max_prompt_bytes)?;
    options.composition.response_bytes = convert(model.max_response_bytes)?;
    options.composition.image_bytes = convert(model.max_model_image_bytes)?;
    options.detailed.allowances.max_input_bytes = options.composition.input_bytes;
    options.detailed.allowances.max_response_bytes = options.composition.response_bytes;
    options.detailed.allowances.max_image_bytes = options.composition.image_bytes;
    Ok(())
}

pub fn create_options(options: &Create) -> Result<EngineOptions, Error> {
    let mut configuration =
        engine_options(options.workspace.clone().unwrap_or_else(std::env::temp_dir));
    configuration
        .acquisition
        .yt_dlp
        .clone_from(&options.tools.yt_dlp);
    configuration.acquisition.max_video_bytes = options.budget.max_video_bytes;
    configuration.acquisition.max_duration_seconds =
        Duration::from_secs(options.budget.max_duration_seconds).as_secs_f64();
    configuration
        .acquisition
        .caption_language
        .clone_from(&options.caption_language);
    configuration.acquisition.timeout =
        Duration::from_secs(options.budget.operation_timeout_seconds);
    configuration.transcription =
        options
            .transcriber
            .as_ref()
            .map(|program| source::TranscriptionCommand {
                program: program.clone(),
                args: options.transcriber_arg.clone(),
                timeout: configuration.acquisition.timeout,
            });
    configuration.tools = MediaTools {
        ffmpeg: options.tools.ffmpeg.clone(),
        ffprobe: options.tools.ffprobe.clone(),
    };
    configuration.media.max_frames = usize::try_from(options.budget.max_frames)
        .map_err(|_| Error::Action("frame budget exceeds platform limits".into()))?;
    configuration.media.max_clips = usize::try_from(options.budget.max_clips)
        .map_err(|_| Error::Action("clip budget exceeds platform limits".into()))?;
    configuration.media.max_clip_duration_ms = options.budget.max_clip_duration_ms;
    configuration.media.max_width = options.budget.max_width;
    configuration.media.max_asset_bytes = options.budget.max_media_bytes;
    configuration.media.operation_timeout = configuration.acquisition.timeout;
    apply_composition_limits(&mut configuration, &options.model)?;
    configuration.composition.images = configuration.media.max_frames;
    for (value, target) in [
        (
            options.detailed_max_concurrent_requests,
            &mut configuration.detailed.max_concurrent_requests,
        ),
        (
            options.detailed_max_in_flight_bytes,
            &mut configuration.detailed.max_in_flight_bytes,
        ),
        (
            options.detailed_max_model_calls,
            &mut configuration.detailed.allowances.max_model_calls,
        ),
        (
            options.detailed_max_source_windows,
            &mut configuration.detailed.allowances.max_source_windows,
        ),
        (
            options.detailed_max_repair_rounds,
            &mut configuration.detailed.allowances.max_repair_rounds,
        ),
    ] {
        if let Some(value) = value {
            *target = usize::try_from(value)
                .map_err(|_| Error::Action("Detailed allowance exceeds platform limits".into()))?;
        }
    }
    configuration
        .detailed
        .validate()
        .map_err(|_| Error::Action("invalid Detailed allowances".into()))?;
    Ok(configuration)
}

/// Adapt actual typed model stages to standalone progress without interpreting source content.
struct ProgressExecutor<'a, F> {
    executor: &'a dyn ModelExecutor,
    progress: &'a std::sync::Mutex<F>,
}

impl<F: FnMut(Stage) + Send> ModelExecutor for ProgressExecutor<'_, F> {
    fn recovery_identity(&self) -> Option<String> {
        self.executor.recovery_identity()
    }
    fn resume_configuration(
        &self,
    ) -> Option<bcode_recap::model_configuration::ResumeModelConfiguration> {
        self.executor.resume_configuration()
    }

    fn execute<'a>(
        &'a self,
        request: bcode_recap::model::ModelRequest,
        cancellation: &'a Cancellation,
    ) -> bcode_recap::model::ModelFuture<'a> {
        self.executor.execute(request, cancellation)
    }

    fn execute_analysis<'a>(
        &'a self,
        request: bcode_recap::model::AnalysisRequest,
        cancellation: &'a Cancellation,
    ) -> bcode_recap::model::AnalysisFuture<'a> {
        use bcode_recap::{
            diagnostics::{Event, Phase},
            model::AnalysisStage,
        };
        let phase = match request.stage {
            AnalysisStage::EditionPlan
            | AnalysisStage::VisualPlan
            | AnalysisStage::VisualReview
            | AnalysisStage::Participants
            | AnalysisStage::ExtractionInventory
            | AnalysisStage::Outline
            | AnalysisStage::VisualInspection => Phase::Analysis,
            AnalysisStage::ChapterComposition | AnalysisStage::PassageComposition => Phase::Writing,
            AnalysisStage::SourceAudit | AnalysisStage::PassageReview => Phase::Audit,
            AnalysisStage::SourceRepair | AnalysisStage::PassagePatch => Phase::Repair,
        };
        if !cancellation.is_cancelled() {
            cancellation.emit(Event::Phase { phase });
            self.progress
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)(phase.into());
        }
        self.executor.execute_analysis(request, cancellation)
    }
}

pub fn new_generation_id() -> Result<String, Error> {
    let directory = tempfile::Builder::new().prefix("recap-").tempdir()?;
    Ok(directory
        .path()
        .file_name()
        .ok_or_else(|| Error::Action("generation identity unavailable".into()))?
        .to_string_lossy()
        .into_owned())
}

pub async fn create(
    store: DocumentStore,
    options: &Create,
    executor: &dyn ModelExecutor,
    cancel: &Cancellation,
    progress: &mut (impl FnMut(Stage) + Send),
) -> Result<RecapDocument, Error> {
    let id = if let Some(id) = &options.id {
        id.clone()
    } else {
        let directory = tempfile::Builder::new().prefix("recap-").tempdir()?;
        directory
            .path()
            .file_name()
            .ok_or_else(|| Error::Action("cannot allocate recap identity; supply --id".into()))?
            .to_string_lossy()
            .into_owned()
    };
    let input = if options.source.contains("://") {
        SourceInput::Youtube(source::youtube_url(&options.source)?)
    } else {
        SourceInput::Local(PathBuf::from(&options.source))
    };
    check_tool(&options.tools.ffmpeg, "ffmpeg", cancel).await?;
    check_tool(&options.tools.ffprobe, "ffprobe", cancel).await?;
    if matches!(&input, SourceInput::Youtube(_)) {
        check_tool(&options.tools.yt_dlp, "yt-dlp", cancel).await?;
    }
    // Validate user inputs before potentially paid inference; downloading/probing still happens in the engine.
    let supplied_transcript = options
        .transcript
        .as_deref()
        .map(read_transcript)
        .transpose()?;
    let configuration = create_options(options)?;
    let progress = std::sync::Mutex::new(progress);
    let executor = ProgressExecutor {
        executor,
        progress: &progress,
    };
    let engine = Engine {
        executor: &executor,
        store: store.clone(),
        options: configuration,
    };
    let title = match &input {
        SourceInput::Local(path) => path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        SourceInput::Youtube(_) => "YouTube video".into(),
    };
    let request = GenerateRequest {
        id: id.clone(),
        source_id: id,
        source_title: title,
        input,
        transcript: supplied_transcript,
        intent: GenerationIntent {
            focus: options.focus.clone(),
            depth: options.depth,
        },
    };
    eprintln!(
        "Generation {} · resume with: recap resume {}",
        request.id, request.id
    );
    let document = engine
        .generate(&request, cancel, &mut |stage| {
            progress
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)(stage.into());
        })
        .await?;
    Ok(document)
}

pub async fn refine(
    store: DocumentStore,
    options: &Refine,
    executor: &dyn ModelExecutor,
    cancel: &Cancellation,
    progress: &mut (impl FnMut(Stage) + Send),
) -> Result<RecapDocument, Error> {
    let previous = store.load(&options.id)?;
    let mut configuration = engine_options(std::env::temp_dir());
    apply_composition_limits(&mut configuration, &options.model)?;
    let progress = std::sync::Mutex::new(progress);
    let executor = ProgressExecutor {
        executor,
        progress: &progress,
    };
    let engine = Engine {
        executor: &executor,
        store: store.clone(),
        options: configuration,
    };
    let mut intent = previous.intent;
    intent.focus = Some(options.focus.clone());
    let document = engine
        .refine(&options.id, intent, cancel, &mut |stage| {
            progress
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)(stage.into());
        })
        .await?;
    Ok(document)
}

#[cfg(test)]
mod progress_tests {
    use super::*;
    use bcode_recap::model::{
        AnalysisRequest, AnalysisStage, ModelError, ModelFuture, ModelRequest,
    };

    #[test]
    fn analysis_counts_are_coalesced_and_preserve_the_caller_observer() {
        use bcode_recap::diagnostics::{Event, Observer as _, Operation, Phase};
        let parent = Operation::new("parent".into());
        let operation = Operation::new("operation".into());
        let latest = std::sync::Arc::new(std::sync::Mutex::new(None));
        let observer = ProgressObserver {
            parent: Cancellation::default().observed(std::sync::Arc::new(parent.clone())),
            operation: operation.clone(),
            latest: latest.clone(),
        };
        for completed in 0..200 {
            observer.observe(Event::AnalysisProgress {
                phase: Phase::Writing,
                completed,
                total: Some(200),
            });
        }
        assert_eq!(
            latest.lock().unwrap().take(),
            Some(Stage::AnalysisProgress {
                phase: Phase::Writing,
                completed: 199,
                total: Some(200),
            })
        );
        assert!(latest.lock().unwrap().is_none());
        for report in [parent.report(), operation.report()] {
            assert!(report.events.len() <= 128);
            assert!(matches!(
                report.events.last().unwrap().event,
                Event::AnalysisProgress { completed: 199, .. }
            ));
        }
    }

    #[test]
    fn measurements_forward_to_parent_and_report_without_event_payloads() {
        use bcode_recap::diagnostics::Operation;
        let parent = Operation::new("parent-metrics".into());
        let operation = Operation::new("child-metrics".into());
        let cancel = Cancellation::default()
            .observed(std::sync::Arc::new(parent.clone()))
            .observed(std::sync::Arc::new(ProgressObserver {
                parent: Cancellation::default().observed(std::sync::Arc::new(parent.clone())),
                operation: operation.clone(),
                latest: std::sync::Arc::new(std::sync::Mutex::new(None)),
            }));
        cancel.measure(bcode_recap::diagnostics::Measurement::Audit { findings: 3 });
        for metrics in [parent.measurements(), operation.measurements()] {
            assert_eq!(metrics.audits, 1);
            assert_eq!(metrics.audit_findings, 3);
        }
        assert!(cancel.observer().is_some());
    }

    struct UnsupportedExecutor;
    impl ModelExecutor for UnsupportedExecutor {
        fn execute<'a>(
            &'a self,
            _request: ModelRequest,
            _cancel: &'a Cancellation,
        ) -> ModelFuture<'a> {
            Box::pin(async { Err(ModelError::Unavailable) })
        }
    }

    #[tokio::test]
    async fn typed_analysis_reports_real_stages_without_downgrading_unsupported_execution() {
        let mut stages = Vec::new();
        let progress = std::sync::Mutex::new(|stage| stages.push(stage));
        let executor = ProgressExecutor {
            executor: &UnsupportedExecutor,
            progress: &progress,
        };
        let cancel = Cancellation::default();
        for stage in [
            AnalysisStage::ExtractionInventory,
            AnalysisStage::Outline,
            AnalysisStage::VisualInspection,
            AnalysisStage::ChapterComposition,
            AnalysisStage::SourceAudit,
            AnalysisStage::SourceRepair,
        ] {
            assert_eq!(
                executor
                    .execute_analysis(
                        AnalysisRequest {
                            stage,
                            instructions: "trusted policy".into(),
                            input: serde_json::json!({}),
                            images: vec![],
                            max_response_bytes: 1024,
                        },
                        &cancel
                    )
                    .await,
                Err(ModelError::Unavailable)
            );
        }
        drop(progress);
        assert_eq!(
            stages,
            [
                Stage::Analysis,
                Stage::Analysis,
                Stage::Analysis,
                Stage::Writing,
                Stage::Audit,
                Stage::Repair
            ]
        );
    }
}

#[cfg(all(test, feature = "tui"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn repeated_interactive_exports_preserve_existing_files_and_revision() {
        use bcode_recap::contracts::{GenerationIntent, Source};
        let root = tempfile::tempdir().unwrap();
        let options = TuiOptions {
            store: root.path().join("store"),
            ..TuiOptions::default()
        };
        let document = RecapDocument {
            schema_version: 1,
            id: "exported".into(),
            revision: 1,
            title: "Saved recap".into(),
            source: Source {
                context: bcode_recap::source_context::SourceContext::default(),
                id: "video".into(),
                title: "Video".into(),
                url: None,
                duration_ms: 1000,
            },
            intent: GenerationIntent::default(),
            transcript: vec![],
            evidence: vec![],
            assets: vec![],
            sections: vec![],
        };
        let store = DocumentStore::new(&options.store, options.max_document_bytes);
        store.save(&document, None).unwrap();
        std::fs::create_dir_all(store.asset_directory(&document.id).unwrap()).unwrap();
        let original = options.store.join("exported.html");
        std::fs::write(&original, "existing HTML must remain intact").unwrap();
        for suffix in [1, 2] {
            let outcome = execute(
                &options,
                Request::ExportCopy {
                    id: document.id.clone(),
                    options: OutputOptions::default(),
                },
                &Cancellation::default(),
                &mut |_| {},
            )
            .await
            .unwrap();
            let Outcome::Exported(result) = outcome else {
                panic!("expected export")
            };
            assert_eq!(
                result.path,
                options.store.join(format!("exported-{suffix}.html"))
            );
            assert!(result.path.is_file());
        }
        let error = execute(
            &options,
            Request::ExportCopy {
                id: document.id.clone(),
                options: OutputOptions {
                    output: Some(original.clone()),
                    ..OutputOptions::default()
                },
            },
            &Cancellation::default(),
            &mut |_| {},
        )
        .await
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("export_destination_exists"));
        assert!(message.contains("saved recap was not modified"));
        assert!(!message.contains("No revision commit was observed"));
        assert_eq!(
            std::fs::read_to_string(original).unwrap(),
            "existing HTML must remain intact"
        );
        assert_eq!(store.load(&document.id).unwrap().revision, 1);
    }

    #[test]
    fn model_and_report_write_errors_preserve_safe_facts() {
        use bcode_recap::diagnostics::{Event, Observer as _, Operation, Phase};
        let operation = Operation::new("operation-test".into());
        operation.observe(Event::Phase {
            phase: Phase::Composition,
        });
        operation.observe(Event::InferenceStarted);
        let error = Error::ModelExecution(crate::bcode_adapter::ModelFailure::ProviderAuth);
        let message = operation_message(&operation.report(), &error);
        assert!(message.contains("provider_auth"));
        assert!(message.contains("authentication"));
        assert!(message.contains("executor was invoked"));
        let combined = diagnostic_write_message(&operation.report(), Some(&error));
        assert!(combined.contains("provider_auth"));
        assert!(combined.contains("report could not be written"));
        let hostile = Error::Io(std::io::Error::other(
            "PRIVATE_TOKEN https://signed.invalid/secret\u{1b}[2J",
        ));
        assert!(!operation_message(&operation.report(), &hostile).contains("PRIVATE_TOKEN"));
    }

    #[tokio::test]
    async fn incomplete_forms_validate_without_spending_or_store_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let options = TuiOptions {
            store: directory.path().join("absent"),
            ..TuiOptions::default()
        };
        let cancel = Cancellation::default();
        let mut stages = Vec::new();
        let create = options.create_request(
            "lesson.mp4".into(),
            None,
            None,
            bcode_recap::contracts::Depth::Brief,
        );
        assert_eq!(create.depth, bcode_recap::contracts::Depth::Brief);
        let error = execute(
            &options,
            Request::Create(Box::new(create)),
            &cancel,
            &mut |stage| stages.push(stage),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, Error::Operation { source, .. } if matches!(*source, Error::Source(_)))
        );
        let refine = options.refine_request("../escape".into(), "Compare".into());
        let error = execute(
            &options,
            Request::Refine(Box::new(refine)),
            &cancel,
            &mut |stage| stages.push(stage),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, Error::Operation { source, .. } if matches!(*source, Error::Action(_)))
        );
        assert!(stages.is_empty());
        assert!(!options.store.exists());
    }

    #[test]
    fn catalog_pages_bound_examined_entries_and_surface_damage() {
        let directory = tempfile::tempdir().unwrap();
        for index in 0..19 {
            std::fs::write(
                directory.path().join(format!("ignored-{index}.txt")),
                "ignored",
            )
            .unwrap();
        }
        for id in ["broken-one", "broken-two"] {
            std::fs::write(directory.path().join(format!("{id}.json")), "not JSON").unwrap();
        }
        let options = TuiOptions {
            store: directory.path().to_owned(),
            ..TuiOptions::default()
        };
        let mut cursor = None;
        let mut examined = 0;
        let mut damaged = Vec::new();
        loop {
            let page = list(&options, cursor, 3, &Cancellation::default()).unwrap();
            assert!(page.examined <= 3);
            examined += page.examined;
            for entry in page.entries {
                assert!(entry.error.is_some());
                assert!(entry.revision.is_none());
                damaged.push(entry.id);
            }
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        damaged.sort();
        assert_eq!(examined, 21);
        assert_eq!(damaged, ["broken-one", "broken-two"]);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 21);
        assert_eq!(
            std::fs::read_to_string(directory.path().join("broken-one.json")).unwrap(),
            "not JSON"
        );
    }

    #[test]
    fn missing_catalog_is_empty_and_nonmutating() {
        let directory = tempfile::tempdir().unwrap();
        let options = TuiOptions {
            store: directory.path().join("absent"),
            ..TuiOptions::default()
        };
        let page = list(&options, None, 10, &Cancellation::default()).unwrap();
        assert!(page.entries.is_empty());
        assert!(page.next.is_none());
        assert!(!options.store.exists());
        assert!(list(&options, None, 0, &Cancellation::default()).is_err());
        assert!(list(&options, None, 65, &Cancellation::default()).is_err());
    }

    #[test]
    fn catalog_cursor_cannot_switch_stores() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("one.txt"), "test").unwrap();
        let mut options = TuiOptions {
            store: directory.path().to_owned(),
            ..TuiOptions::default()
        };
        let page = list(&options, None, 1, &Cancellation::default()).unwrap();
        options.store = directory.path().join("another");
        assert!(list(&options, page.next, 1, &Cancellation::default()).is_err());
    }

    #[tokio::test]
    async fn cancelled_jobs_do_not_access_or_create_a_store() {
        let directory = tempfile::tempdir().unwrap();
        let options = TuiOptions {
            store: directory.path().join("absent"),
            ..TuiOptions::default()
        };
        let cancel = Cancellation::default();
        cancel.cancel();
        let mut stages = Vec::new();
        let outcome = execute(
            &options,
            #[cfg(feature = "tui")]
            Request::Load {
                id: "anything".into(),
            },
            &cancel,
            &mut |stage| stages.push(stage),
        )
        .await;
        assert!(
            matches!(outcome, Err(Error::Operation { source, .. }) if matches!(*source, Error::Cancelled))
        );
        assert!(stages.is_empty());
        assert!(!options.store.exists());
    }

    #[cfg(unix)]
    #[test]
    fn catalog_does_not_follow_symlinked_roots() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("link");
        std::os::unix::fs::symlink(directory.path(), &root).unwrap();
        let options = TuiOptions {
            store: root,
            ..TuiOptions::default()
        };
        assert!(list(&options, None, 10, &Cancellation::default()).is_err());
    }
}
