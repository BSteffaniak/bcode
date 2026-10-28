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
    /// Committing a validated revision.
    Storage,
    /// Rendering offline HTML.
    Export,
    /// The requested operation has completed.
    Complete,
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
    #[cfg(feature = "tui")]
    CreateAndExport(Box<Create>),
    #[cfg(feature = "tui")]
    RefineAndExport(Box<Refine>),
    Create(Box<Create>),
    Refine(Box<Refine>),
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
#[derive(Debug)]
pub enum Outcome {
    #[cfg(feature = "tui")]
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

/// Execute with cooperative cancellation and bounded semantic progress callbacks.
/// Create/refine return the committed document; export is a separate UI action.
pub async fn execute(
    options: &TuiOptions,
    request: Request,
    cancel: &Cancellation,
    progress: &mut (impl FnMut(Stage) + Send),
) -> Result<Outcome, Error> {
    check_cancel(cancel)?;
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
        other => (other, None::<OutputOptions>),
    };
    let store = DocumentStore::new(&options.store, options.max_document_bytes);
    let outcome = execute_job(options, &store, request, cancel, progress).await?;
    #[cfg(feature = "tui")]
    if let Some(output) = automatic_export
        && let Outcome::Document(document) = outcome
    {
        progress(Stage::Export);
        let export = export_document(&store, &document, &output).map_err(|_| {
            "Recap saved, but HTML export failed. Choose another destination and export again."
                .into()
        });
        return Ok(Outcome::Ready { document, export });
    }
    #[cfg(not(feature = "tui"))]
    let _ = automatic_export;
    Ok(outcome)
}

async fn execute_job(
    options: &TuiOptions,
    store: &DocumentStore,
    request: Request,
    cancel: &Cancellation,
    progress: &mut (impl FnMut(Stage) + Send),
) -> Result<Outcome, Error> {
    match request {
        #[cfg(feature = "tui")]
        Request::CreateAndExport(_) | Request::RefineAndExport(_) => {
            unreachable!("export jobs normalized")
        }
        Request::Create(value) => {
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
            progress(Stage::ModelReadiness);
            let executor = configured_model(&value.model).await?;
            check_cancel(cancel)?;
            create(store.clone(), &value, &executor, cancel, progress)
                .await
                .map(|document| Outcome::Document(Box::new(document)))
                .map_err(|error| model_job_error(error, &executor))
        }
        Request::Refine(value) => {
            crate::cli::document_id(&value.id).map_err(Error::Action)?;
            crate::cli::nonempty(&value.focus).map_err(Error::Action)?;
            progress(Stage::ModelReadiness);
            let executor = configured_model(&value.model).await?;
            check_cancel(cancel)?;
            refine(store.clone(), &value, &executor, cancel, progress)
                .await
                .map(|document| Outcome::Document(Box::new(document)))
                .map_err(|error| model_job_error(error, &executor))
        }
        #[cfg(feature = "tui")]
        Request::Load { id } => store
            .load(&id)
            .map(|document| Outcome::Document(Box::new(document)))
            .map_err(Error::from),
        Request::Export { id, options } => {
            let document = store.load(&id)?;
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
    #[error("model operation failed ({code}); no automatic retry was performed", code = .0.code())]
    ModelExecution(crate::bcode_adapter::ModelFailure),
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
                Error::Action(format!(
                    "output {} already exists; choose --output or pass --force",
                    path.display()
                ))
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
    process::run(&request, cancellation).await.map_err(|_| Error::Action(format!(
        "{name} unavailable or failed its version check; install {name} or select its executable with --{name}"
    )))?;
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

pub async fn configured_model(
    options: &ModelOptions,
) -> Result<crate::bcode_adapter::BcodeExecutor, Error> {
    preflight_model(options)?;
    crate::bcode_adapter::BcodeExecutor::configured(model_options(options)).await.map_err(|_| Error::Action(
        "embedded model configuration failed; check --model-config, --profile/--context, provider authentication and multimodal model support; no fallback model was used".into()
    ))
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
            max_video_bytes: 256 * 1024 * 1024,
            max_duration_seconds: 7200.0,
            caption_language: "en".into(),
            timeout: Duration::from_secs(120),
        },
        transcription: None,
        tools: MediaTools::default(),
        media: MediaBudget::default(),
        composition: CompositionLimits::default(),
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
    Ok(configuration)
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
    let engine = Engine {
        executor,
        store,
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
    let document = engine
        .generate(&request, cancel, &mut |stage| progress(stage.into()))
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
    let engine = Engine {
        executor,
        store,
        options: configuration,
    };
    let mut intent = previous.intent;
    intent.focus = Some(options.focus.clone());
    let document = engine
        .refine(&options.id, intent, cancel, &mut |stage| {
            progress(stage.into());
        })
        .await?;
    Ok(document)
}

#[cfg(all(test, feature = "tui"))]
mod tests {
    use super::*;

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
        assert!(matches!(error, Error::Source(_)));
        let refine = options.refine_request("../escape".into(), "Compare".into());
        let error = execute(
            &options,
            Request::Refine(Box::new(refine)),
            &cancel,
            &mut |stage| stages.push(stage),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, Error::Action(_)));
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
        assert!(matches!(outcome, Err(Error::Cancelled)));
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
