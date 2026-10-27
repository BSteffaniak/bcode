//! Command-line policy for the standalone visual recap application.
use std::{
    io::{Read as _, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use bcode_recap::{
    composition::CompositionLimits,
    contracts::GenerationIntent,
    contracts::RecapDocument,
    engine::{Engine, EngineOptions, GenerateRequest, Stage},
    export::{ExportBudget, export_html},
    media::{MediaBudget, MediaTools},
    model::ModelExecutor,
    process::{self, Cancellation, ProcessRequest},
    source::{self, AcquisitionOptions, SourceInput},
    storage::DocumentStore,
};
use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "recap",
    version,
    about = "Turn a video into a grounded, offline visual recap"
)]
#[command(
    after_help = "Examples:\n  recap create --source lesson.mp4 --model-config provider.toml\n  recap create --source https://youtu.be/VIDEO_ID_11 --focus 'Explain the technique' --model-config provider.toml\n  recap refine my-recap --focus 'Expand the comparison' --model-config provider.toml\n  recap export my-recap --output recap.html\n  recap doctor\n\nNo conversation is needed. Create performs real visual model inference and exports one self-contained HTML file. Model configuration is explicit; no Bcode daemon or global state is changed."
)]
struct Cli {
    /// Private recap document/media directory (independent of Bcode state).
    #[arg(long, global = true, default_value = ".recap")]
    store: PathBuf,
    /// Maximum bytes in one stored recap document.
    #[arg(long, global = true, default_value = "8388608", value_parser = positive_u64)]
    max_document_bytes: u64,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Analyze a local video or `YouTube` URL, persist a recap, and export HTML.
    Create(Box<Create>),
    /// Refine an existing recap without replacing it unless inference succeeds.
    Refine(Box<Refine>),
    /// Export a stored recap as one self-contained HTML file; no model needed.
    Export(Export),
    /// Print a validated stored document or its concise metadata; no model needed.
    Inspect(Inspect),
    /// Check required media tools and optional configured model selection.
    Doctor(Doctor),
}

#[derive(Debug, Args)]
struct ModelOptions {
    /// Explicit Bcode provider configuration file for embedded inference.
    #[arg(long, required = true)]
    model_config: PathBuf,
    /// Configured model profile (not an authentication credential).
    #[arg(long)]
    profile: Option<String>,
    /// Configured model name or alias, resolved by the model catalog.
    #[arg(long)]
    model: Option<String>,
    /// Configured provider override, resolved by the model catalog.
    #[arg(long)]
    provider: Option<String>,
    /// Configuration context to select from the explicit file.
    #[arg(long)]
    context: Option<String>,
    /// Maximum output tokens for each inference request.
    #[arg(long, default_value = "8192", value_parser = clap::value_parser!(u32).range(1..))]
    max_output_tokens: u32,
    /// Wall-clock deadline for model inference, in seconds.
    #[arg(long, default_value = "180", value_parser = positive_u64)]
    model_timeout_seconds: u64,
    /// Maximum JSON prompt bytes (over-budget evidence is rejected, not silently dropped).
    #[arg(long, default_value = "2097152", value_parser = positive_u64)]
    max_prompt_bytes: u64,
    /// Maximum JSON response bytes accepted from the model.
    #[arg(long, default_value = "1048576", value_parser = positive_u64)]
    max_response_bytes: u64,
    /// Maximum combined bytes of actual model-visible source images.
    #[arg(long, default_value = "33554432", value_parser = positive_u64)]
    max_model_image_bytes: u64,
}

#[derive(Debug, Args)]
struct Tools {
    /// yt-dlp executable; required only for `YouTube` acquisition.
    #[arg(long, default_value = "yt-dlp")]
    yt_dlp: PathBuf,
    /// ffmpeg executable for frames and short clips.
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: PathBuf,
    /// ffprobe executable for source metadata.
    #[arg(long, default_value = "ffprobe")]
    ffprobe: PathBuf,
}

#[derive(Debug, Args)]
struct Create {
    /// Existing video path or supported `YouTube` URL.
    #[arg(long)]
    source: String,
    /// Stable document ID. Defaults to a new unique recap ID.
    #[arg(long, value_parser = document_id)]
    id: Option<String>,
    /// Editorial focus. Omit for a general visual recap, not a conversation.
    #[arg(long, value_parser = nonempty)]
    focus: Option<String>,
    /// Explicit timestamped VTT or SRT transcript; otherwise use available captions.
    #[arg(long)]
    transcript: Option<PathBuf>,
    /// Explicit speech-to-text executable used only when captions/transcript are absent.
    /// It receives the video path after the literal arguments and must emit VTT/SRT.
    #[arg(long, conflicts_with = "transcript")]
    transcriber: Option<PathBuf>,
    /// Literal argument to --transcriber; repeat as needed (no shell expansion).
    #[arg(long, requires = "transcriber")]
    transcriber_arg: Vec<std::ffi::OsString>,
    /// Preferred `YouTube` caption language.
    #[arg(long, default_value = "en")]
    caption_language: String,
    /// Parent directory for bounded temporary downloads and extraction.
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[command(flatten)]
    model: ModelOptions,
    #[command(flatten)]
    tools: Tools,
    #[command(flatten)]
    budget: CreateBudget,
    #[command(flatten)]
    output: OutputOptions,
}

#[derive(Debug, Args)]
struct CreateBudget {
    /// Maximum source bytes (also enforced for local files).
    #[arg(long, default_value = "268435456", value_parser = positive_u64)]
    max_video_bytes: u64,
    /// Maximum source duration in seconds.
    #[arg(long, default_value = "7200", value_parser = positive_u64)]
    max_duration_seconds: u64,
    /// Maximum sampled source frames.
    #[arg(long, default_value = "12", value_parser = clap::value_parser!(u32).range(1..))]
    max_frames: u32,
    /// Maximum silent short clips (zero disables clips).
    #[arg(long, default_value = "3")]
    max_clips: u32,
    /// Maximum combined duration of extracted silent clips, in milliseconds.
    #[arg(long, default_value = "24000", value_parser = positive_u64)]
    max_clip_duration_ms: u64,
    /// Maximum width of extracted source media, preserving aspect ratio.
    #[arg(long, default_value = "960", value_parser = clap::value_parser!(u32).range(1..))]
    max_width: u32,
    /// Maximum total extracted media bytes.
    #[arg(long, default_value = "25165824", value_parser = positive_u64)]
    max_media_bytes: u64,
    /// Deadline for each acquisition/media subprocess, in seconds.
    #[arg(long, default_value = "120", value_parser = positive_u64)]
    operation_timeout_seconds: u64,
}

#[derive(Debug, Args)]
struct Refine {
    /// Stored document ID, not a JSON input file.
    #[arg(value_parser = document_id)]
    id: String,
    /// Required editorial instruction for the next revision.
    #[arg(long, value_parser = nonempty)]
    focus: String,
    #[command(flatten)]
    model: ModelOptions,
    #[command(flatten)]
    output: OutputOptions,
}

#[derive(Debug, Args)]
struct OutputOptions {
    /// Destination HTML file (default: <id>.html in the current directory).
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Explicitly permit replacing an existing output file.
    #[arg(long)]
    force: bool,
    /// Maximum raw bytes embedded from any single asset.
    #[arg(long, default_value = "12582912", value_parser = positive_u64)]
    max_export_asset_bytes: u64,
    /// Maximum total embedded media bytes, including base64 overhead.
    #[arg(long, default_value = "50331648", value_parser = positive_u64)]
    max_export_media_bytes: u64,
}

#[derive(Debug, Args)]
struct Export {
    #[arg(value_parser = document_id)]
    id: String,
    #[command(flatten)]
    output: OutputOptions,
}

#[derive(Debug, Args)]
struct Inspect {
    #[arg(value_parser = document_id)]
    id: String,
    /// Print the complete validated portable document as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct Doctor {
    #[command(flatten)]
    tools: Tools,
    /// Also validate this explicit embedded model configuration, without inference.
    #[arg(long)]
    model_config: Option<PathBuf>,
    #[arg(long)]
    profile: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    context: Option<String>,
}

fn positive_u64(value: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| "must be a positive integer".into())
}

fn nonempty(value: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        Err("must not be blank".into())
    } else {
        Ok(value.to_owned())
    }
}

fn document_id(value: &str) -> Result<String, String> {
    // Cheap argument validation; the store remains the authoritative contract validator.
    let reserved = matches!(
        value.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    );
    if value.is_empty()
        || reserved
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
    {
        return Err(
            "use 1-128 ASCII letters, digits, hyphens, or underscores for the document ID".into(),
        );
    }
    Ok(value.to_owned())
}

#[derive(Debug, thiserror::Error)]
enum CliError {
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

fn inspect(store: &DocumentStore, options: &Inspect) -> Result<(), CliError> {
    let document = store.load(&options.id)?;
    let mut output = std::io::stdout().lock();
    if options.json {
        serde_json::to_writer_pretty(&mut output, &document)?;
        writeln!(output)?;
    } else {
        writeln!(output, "{} — {}", document.id, document.title)?;
        writeln!(
            output,
            "Revision: {}\nDuration: {} ms\nSections: {}\nAssets: {}\nTranscript segments: {}",
            document.revision,
            document.source.duration_ms,
            document.sections.len(),
            document.assets.len(),
            document.transcript.len()
        )?;
    }
    Ok(())
}

fn export_document(
    store: &DocumentStore,
    document: &RecapDocument,
    options: &OutputOptions,
) -> Result<(), CliError> {
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
    for degradation in &result.report.degradations {
        eprintln!("warning: media export degraded: {:?}", degradation.reason);
    }
    eprintln!(
        "Exported {} ({} embedded media bytes; {} omissions)",
        destination.display(),
        result.report.embedded_media_bytes,
        result.report.degradations.len()
    );
    Ok(())
}

fn export_committed(
    store: &DocumentStore,
    document: &RecapDocument,
    options: &OutputOptions,
) -> Result<(), CliError> {
    export_document(store, document, options).map_err(|error| CliError::Action(format!(
        "recap {} revision {} was committed, but HTML export failed: {error}. The saved revision is intact; retry recap export {} --output FILE",
        document.id, document.revision, document.id
    )))
}

fn output_path(id: &str, options: &OutputOptions) -> PathBuf {
    options
        .output
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{id}.html")))
}

// Stage output beside its destination so failures never truncate an existing HTML file.
fn write_output(path: &Path, bytes: &[u8], force: bool) -> Result<(), CliError> {
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
            .map_err(|error| CliError::Io(error.error))?;
    } else {
        staged.persist_noclobber(path).map_err(|error| {
            if error.error.kind() == std::io::ErrorKind::AlreadyExists {
                CliError::Action(format!(
                    "output {} already exists; choose --output or pass --force",
                    path.display()
                ))
            } else {
                CliError::Io(error.error)
            }
        })?;
    }
    Ok(())
}

async fn check_tool(
    program: &Path,
    name: &str,
    cancellation: &Cancellation,
) -> Result<(), CliError> {
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
        return Err(CliError::Action("operation cancelled".into()));
    }
    process::run(&request, cancellation).await.map_err(|_| CliError::Action(format!(
        "{name} unavailable or failed its version check; install {name} or select its executable with --{name}"
    )))?;
    eprintln!("ok: {name}");
    Ok(())
}

/// Parse arguments and run the standalone application, reporting actionable failures.
pub async fn main() -> ExitCode {
    let arguments = Cli::parse();
    let cancellation = Cancellation::default();
    let signal_cancel = cancellation.clone();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("Interrupt received; cancelling inference and media processing…");
        } else {
            eprintln!("Unable to install SIGINT handler; refusing uncancellable work.");
        }
        signal_cancel.cancel();
    });
    let result = run(arguments, &cancellation).await;
    signal_task.abort();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("recap: {error}");
            if cancellation.is_cancelled() {
                ExitCode::from(130)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

fn model_options(options: &ModelOptions) -> crate::bcode_adapter::BcodeOptions {
    crate::bcode_adapter::BcodeOptions {
        config_path: options.model_config.clone(),
        profile: options.profile.clone(),
        context: options.context.clone(),
        model: options.model.clone(),
        provider: options.provider.clone(),
        max_output_tokens: options.max_output_tokens,
        timeout: Duration::from_secs(options.model_timeout_seconds),
    }
}

async fn configured_model(
    options: &ModelOptions,
) -> Result<crate::bcode_adapter::BcodeExecutor, CliError> {
    preflight_model(options)?;
    eprintln!("Resolving explicit embedded model configuration (no daemon)…");
    crate::bcode_adapter::BcodeExecutor::configured(model_options(options)).await.map_err(|_| CliError::Action(
        "embedded model configuration failed; check --model-config, --profile/--context, provider authentication and multimodal model support; no fallback model was used".into()
    ))
}

fn preflight_model(options: &ModelOptions) -> Result<(), CliError> {
    if !std::fs::metadata(&options.model_config)
        .map_err(|_| {
            CliError::Action(format!(
                "cannot read model configuration {}; supply an existing --model-config FILE",
                options.model_config.display()
            ))
        })?
        .is_file()
    {
        return Err(CliError::Action(
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
            return Err(CliError::Action(format!("--{name} must not be blank")));
        }
    }
    Ok(())
}

async fn run(arguments: Cli, cancel: &Cancellation) -> Result<(), CliError> {
    let store = DocumentStore::new(arguments.store, arguments.max_document_bytes);
    match arguments.command {
        Command::Inspect(options) => inspect(&store, &options),
        Command::Export(options) => {
            let document = store.load(&options.id)?;
            export_document(&store, &document, &options.output)
        }
        Command::Create(options) => {
            preflight_model(&options.model)?;
            let executor = configured_model(&options.model).await?;
            create(store, &options, &executor, cancel).await
        }
        Command::Refine(options) => {
            preflight_model(&options.model)?;
            let executor = configured_model(&options.model).await?;
            refine(store, &options, &executor, cancel).await
        }
        Command::Doctor(options) => doctor(&options, cancel).await,
    }
}

async fn doctor(options: &Doctor, cancel: &Cancellation) -> Result<(), CliError> {
    let mut failures = 0;
    for (program, name) in [
        (&options.tools.yt_dlp, "yt-dlp"),
        (&options.tools.ffmpeg, "ffmpeg"),
        (&options.tools.ffprobe, "ffprobe"),
    ] {
        if let Err(error) = check_tool(program, name, cancel).await {
            eprintln!("error: {error}");
            failures += 1;
        }
    }
    if let Some(config) = &options.model_config {
        let selection = ModelOptions {
            model_config: config.clone(),
            profile: options.profile.clone(),
            model: options.model.clone(),
            provider: options.provider.clone(),
            context: options.context.clone(),
            max_output_tokens: 8192,
            model_timeout_seconds: 180,
            max_prompt_bytes: 2 * 1024 * 1024,
            max_response_bytes: 1024 * 1024,
            max_model_image_bytes: 32 * 1024 * 1024,
        };
        if let Err(error) = preflight_model(&selection) {
            eprintln!("error: {error}");
            failures += 1;
        } else if let Err(error) = configured_model(&selection).await {
            eprintln!("error: {error}");
            failures += 1;
        }
    } else if options.profile.is_some()
        || options.model.is_some()
        || options.provider.is_some()
        || options.context.is_some()
    {
        return Err(CliError::Action(
            "doctor model selection requires --model-config FILE".into(),
        ));
    } else {
        eprintln!(
            "Model not checked; pass --model-config FILE to validate selection without inference."
        );
    }
    if failures == 0 {
        eprintln!(
            "Ready. yt-dlp is needed only for YouTube; ffmpeg and ffprobe are required for visual extraction."
        );
        Ok(())
    } else {
        Err(CliError::Action(format!(
            "{failures} diagnostic checks failed; resolve the errors above and rerun recap doctor"
        )))
    }
}

fn progress(stage: Stage) {
    eprintln!(
        "{}",
        match stage {
            Stage::Acquisition => "Acquiring source and available captions…",
            Stage::Media => "Probing source and extracting real visual evidence…",
            Stage::Composition => "Composing grounded visual recap with configured model…",
            Stage::Storage => "Committing validated recap revision…",
            Stage::Export => "Rendering self-contained HTML…",
            Stage::Complete => "Recap revision committed.",
        }
    );
}

fn check_output_destination(id: &str, options: &OutputOptions) -> Result<(), CliError> {
    let path = output_path(id, options);
    if !options.force && path.try_exists()? {
        return Err(CliError::Action(format!(
            "output {} already exists; choose --output or pass --force",
            path.display()
        )));
    }
    Ok(())
}

fn engine_options(workspace: PathBuf) -> EngineOptions {
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

fn read_transcript(path: &Path) -> Result<String, CliError> {
    const LIMIT: u64 = 2 * 1024 * 1024;
    if !std::fs::metadata(path)?.is_file() {
        return Err(CliError::Action(
            "--transcript must name a regular VTT/SRT file".into(),
        ));
    }
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > LIMIT {
        return Err(CliError::Action(
            "transcript exceeds the 2 MiB limit; supply a bounded VTT/SRT transcript".into(),
        ));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| CliError::Action("transcript must be UTF-8 VTT/SRT".into()))?;
    source::parse_subtitles(&text)?;
    Ok(text)
}

fn apply_composition_limits(
    options: &mut EngineOptions,
    model: &ModelOptions,
) -> Result<(), CliError> {
    let convert = |value| {
        usize::try_from(value)
            .map_err(|_| CliError::Action("model byte budget exceeds platform limits".into()))
    };
    options.composition.input_bytes = convert(model.max_prompt_bytes)?;
    options.composition.response_bytes = convert(model.max_response_bytes)?;
    options.composition.image_bytes = convert(model.max_model_image_bytes)?;
    Ok(())
}

fn create_options(options: &Create) -> Result<EngineOptions, CliError> {
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
        .map_err(|_| CliError::Action("frame budget exceeds platform limits".into()))?;
    configuration.media.max_clips = usize::try_from(options.budget.max_clips)
        .map_err(|_| CliError::Action("clip budget exceeds platform limits".into()))?;
    configuration.media.max_clip_duration_ms = options.budget.max_clip_duration_ms;
    configuration.media.max_width = options.budget.max_width;
    configuration.media.max_asset_bytes = options.budget.max_media_bytes;
    configuration.media.operation_timeout = configuration.acquisition.timeout;
    apply_composition_limits(&mut configuration, &options.model)?;
    configuration.composition.images = configuration.media.max_frames;
    Ok(configuration)
}

async fn create(
    store: DocumentStore,
    options: &Create,
    executor: &dyn ModelExecutor,
    cancel: &Cancellation,
) -> Result<(), CliError> {
    let id = if let Some(id) = &options.id {
        id.clone()
    } else {
        let directory = tempfile::Builder::new().prefix("recap-").tempdir()?;
        directory
            .path()
            .file_name()
            .ok_or_else(|| CliError::Action("cannot allocate recap identity; supply --id".into()))?
            .to_string_lossy()
            .into_owned()
    };
    check_output_destination(&id, &options.output)?;
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
            ..GenerationIntent::default()
        },
    };
    let document = engine.generate(&request, cancel, &mut progress).await?;
    eprintln!("Rendering self-contained HTML…");
    eprintln!(
        "Saved {} revision {}. Re-export anytime with: recap --store {} export {}",
        document.id,
        document.revision,
        engine
            .store
            .asset_directory(&document.id)?
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .display(),
        document.id
    );
    export_committed(&engine.store, &document, &options.output)
}

async fn refine(
    store: DocumentStore,
    options: &Refine,
    executor: &dyn ModelExecutor,
    cancel: &Cancellation,
) -> Result<(), CliError> {
    let previous = store.load(&options.id)?;
    check_output_destination(&options.id, &options.output)?;
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
        .refine(&options.id, intent, cancel, &mut progress)
        .await?;
    eprintln!("Rendering self-contained HTML…");
    eprintln!("Saved {} revision {}", document.id, document.revision);
    export_committed(&engine.store, &document, &options.output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;

    #[test]
    fn model_and_media_budgets_reach_execution_options() {
        let parsed = Cli::try_parse_from([
            "recap",
            "create",
            "--source",
            "lesson.mp4",
            "--model-config",
            "provider.toml",
            "--profile",
            "sol",
            "--max-output-tokens",
            "321",
            "--model-timeout-seconds",
            "7",
            "--max-prompt-bytes",
            "4096",
            "--max-response-bytes",
            "2048",
            "--max-model-image-bytes",
            "8192",
            "--max-frames",
            "4",
            "--max-clips",
            "0",
            "--operation-timeout-seconds",
            "9",
        ])
        .unwrap();
        let Command::Create(options) = parsed.command else {
            panic!("expected create")
        };
        let model = model_options(&options.model);
        assert_eq!(model.max_output_tokens, 321);
        assert_eq!(model.timeout, Duration::from_secs(7));
        assert_eq!(model.profile.as_deref(), Some("sol"));
        let engine = create_options(&options).unwrap();
        assert_eq!(engine.composition.input_bytes, 4096);
        assert_eq!(engine.composition.response_bytes, 2048);
        assert_eq!(engine.composition.image_bytes, 8192);
        assert_eq!(engine.composition.images, 4);
        assert_eq!(engine.media.max_frames, 4);
        assert_eq!(engine.media.max_clips, 0);
        assert_eq!(engine.media.operation_timeout, Duration::from_secs(9));
    }

    #[test]
    fn model_budgets_reject_zero() {
        for flag in ["--max-output-tokens", "--model-timeout-seconds"] {
            assert!(
                Cli::try_parse_from([
                    "recap",
                    "create",
                    "--source",
                    "lesson.mp4",
                    "--model-config",
                    "provider.toml",
                    flag,
                    "0"
                ])
                .is_err()
            );
        }
    }

    #[test]
    fn command_schema_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn create_without_conversation_requires_real_model_configuration() {
        assert!(Cli::try_parse_from(["recap", "create", "--source", "lesson.mp4"]).is_err());
        let parsed = Cli::try_parse_from([
            "recap",
            "create",
            "--source",
            "lesson.mp4",
            "--model-config",
            "provider.toml",
        ])
        .unwrap();
        let Command::Create(options) = parsed.command else {
            panic!("expected create")
        };
        assert!(options.focus.is_none());
        assert_eq!(options.source, "lesson.mp4");
        assert_eq!(options.budget.max_frames, 12);
    }

    #[test]
    fn refine_requires_intent_and_safe_document_identity() {
        assert!(
            Cli::try_parse_from([
                "recap",
                "refine",
                "../escape",
                "--focus",
                "details",
                "--model-config",
                "provider.toml"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "recap",
                "refine",
                "lesson",
                "--model-config",
                "provider.toml"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "recap",
                "refine",
                "lesson",
                "--focus",
                "   ",
                "--model-config",
                "provider.toml"
            ])
            .is_err()
        );
    }

    #[test]
    fn offline_commands_need_no_model_or_daemon() {
        for command in ["inspect", "export"] {
            assert!(Cli::try_parse_from(["recap", command, "lesson"]).is_ok());
        }
        assert!(Cli::try_parse_from(["recap", "doctor"]).is_ok());
    }

    #[test]
    fn zero_budgets_are_rejected() {
        assert!(
            Cli::try_parse_from([
                "recap",
                "create",
                "--source",
                "lesson.mp4",
                "--model-config",
                "provider.toml",
                "--max-video-bytes",
                "0"
            ])
            .is_err()
        );
    }

    #[test]
    fn explicit_store_selection_is_accepted_before_or_after_command() {
        for arguments in [
            vec!["recap", "--store", "private-recaps", "inspect", "lesson"],
            vec!["recap", "inspect", "lesson", "--store", "private-recaps"],
        ] {
            let parsed = Cli::try_parse_from(arguments).unwrap();
            assert_eq!(parsed.store, PathBuf::from("private-recaps"));
        }
    }

    #[test]
    fn transcription_is_explicit_and_never_shell_parsed() {
        let parsed = Cli::try_parse_from([
            "recap",
            "create",
            "--source",
            "lesson.mp4",
            "--model-config",
            "provider.toml",
            "--transcriber",
            "speech-tool",
            "--transcriber-arg=--format",
            "--transcriber-arg",
            "vtt",
        ])
        .unwrap();
        let Command::Create(options) = parsed.command else {
            panic!("expected create")
        };
        assert_eq!(options.transcriber.unwrap(), PathBuf::from("speech-tool"));
        assert_eq!(
            options.transcriber_arg,
            vec![
                std::ffi::OsString::from("--format"),
                std::ffi::OsString::from("vtt")
            ]
        );
    }

    #[test]
    fn transcript_reads_real_timestamped_cues_and_rejects_invalid_input() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("captions.vtt");
        std::fs::write(&path, "WEBVTT\n\n00:00.000 --> 00:01.000\nActual words\n").unwrap();
        assert!(read_transcript(&path).unwrap().contains("Actual words"));
        std::fs::write(&path, "Not a transcript").unwrap();
        assert!(read_transcript(&path).is_err());
    }

    #[test]
    fn output_never_clobbers_without_explicit_permission() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("recap.html");
        write_output(&path, b"first", false).unwrap();
        assert!(write_output(&path, b"second", false).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        write_output(&path, b"second", true).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
    }
}
