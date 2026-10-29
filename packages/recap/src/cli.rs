//! Command-line policy for the standalone visual recap application.
use crate::application::Stage;
use crate::application::{self, Error as CliError};
use crate::application::{check_tool, configured_model, preflight_model};
#[cfg(test)]
use crate::application::{create_options, model_options, read_transcript, write_output};
use bcode_recap::{process::Cancellation, storage::DocumentStore};
use clap::{Args, CommandFactory as _, Parser, Subcommand};
#[cfg(feature = "tui")]
use std::io::IsTerminal as _;
#[cfg(test)]
use std::time::Duration;
use std::{io::Write, path::PathBuf, process::ExitCode};

#[derive(Debug, Parser)]
#[command(
    name = "recap",
    version,
    about = "Turn a video into a grounded, offline visual recap"
)]
#[command(
    after_help = "Examples:\n  recap create --source lesson.mp4 --model-config provider.toml\n  recap create --source https://youtu.be/VIDEO_ID_11 --focus 'Explain the technique' --model-config provider.toml\n  recap refine my-recap --focus 'Expand the comparison' --model-config provider.toml\n  recap export my-recap --output recap.html\n  recap doctor\n\nNo conversation is needed. Create performs real visual model inference and exports one self-contained HTML file. Model configuration is explicit; no Bcode daemon or global state is changed. Headless creation defaults to Standard. Interactive Detailed mode analyzes source coverage, writes chapters, then audits and repairs omissions within explicit resource allowances. It is not merely a longer summary and never silently falls back to Standard; failure preserves existing revisions."
)]
struct Cli {
    /// Private recap document/media directory (independent of Bcode state).
    #[arg(long, global = true, default_value = ".recap")]
    store: PathBuf,
    /// Write a bounded secret-safe operation report to a new file (never overwritten).
    #[arg(long, global = true)]
    diagnostic_report: Option<PathBuf>,
    /// Maximum bytes in one stored recap document.
    #[arg(long, global = true, default_value = "8388608", value_parser = positive_u64)]
    max_document_bytes: u64,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Browse and create recaps in the interactive terminal application.
    #[cfg(feature = "tui")]
    Tui(application::TuiOptions),
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

#[derive(Debug, Clone, Args)]
pub struct ModelOptions {
    /// Configuration override; omit to discover the existing AI setup.
    #[arg(long, default_value = "", value_parser = parse_config_path)]
    pub model_config: PathBuf,
    /// Configured model profile (not an authentication credential).
    #[arg(long)]
    pub profile: Option<String>,
    /// Configured model name or alias, resolved by the model catalog.
    #[arg(long)]
    pub model: Option<String>,
    /// Configured provider override, resolved by the model catalog.
    #[arg(long)]
    pub provider: Option<String>,
    /// Custom provider endpoint override; credentials remain provider-owned.
    #[arg(long)]
    pub endpoint: Option<String>,
    /// Existing configured authentication profile reference, never a credential.
    #[arg(long)]
    pub auth_profile: Option<String>,
    /// Configuration context to select from the explicit file.
    #[arg(long)]
    pub context: Option<String>,
    /// Maximum output tokens for each inference request.
    #[arg(long, default_value = "8192", value_parser = clap::value_parser!(u32).range(1..))]
    pub max_output_tokens: u32,
    /// Wall-clock deadline for model inference, in seconds.
    #[arg(long, default_value = "180", value_parser = positive_u64)]
    pub model_timeout_seconds: u64,
    /// Maximum JSON prompt bytes (over-budget evidence is rejected, not silently dropped).
    #[arg(long, default_value = "2097152", value_parser = positive_u64)]
    pub max_prompt_bytes: u64,
    /// Maximum JSON response bytes accepted from the model.
    #[arg(long, default_value = "1048576", value_parser = positive_u64)]
    pub max_response_bytes: u64,
    /// Maximum combined bytes of actual model-visible source images.
    #[arg(long, default_value = "33554432", value_parser = positive_u64)]
    pub max_model_image_bytes: u64,
}

#[derive(Debug, Clone, Args)]
pub struct Tools {
    /// yt-dlp executable; required only for `YouTube` acquisition.
    #[arg(long, default_value = "yt-dlp")]
    pub yt_dlp: PathBuf,
    /// ffmpeg executable for frames and short clips.
    #[arg(long, default_value = "ffmpeg")]
    pub ffmpeg: PathBuf,
    /// ffprobe executable for source metadata.
    #[arg(long, default_value = "ffprobe")]
    pub ffprobe: PathBuf,
}

#[derive(Debug, Clone, Args)]
pub struct Create {
    /// Semantic detail: Detailed preserves source information through analysis/audit.
    #[arg(long, default_value = "standard", value_parser = parse_depth)]
    pub depth: bcode_recap::contracts::Depth,
    /// Detailed model-call allowance; exhaustion fails rather than downgrading.
    #[arg(long)]
    pub detailed_max_model_calls: Option<u64>,
    /// Detailed source-window allowance, not an editorial chapter cap.
    #[arg(long)]
    pub detailed_max_source_windows: Option<u64>,
    /// Detailed repair-round allowance; zero audits without attempting repairs.
    #[arg(long)]
    pub detailed_max_repair_rounds: Option<u64>,
    /// Maximum concurrent independent Detailed model requests.
    #[arg(long, value_parser = positive_u64)]
    pub detailed_max_concurrent_requests: Option<u64>,
    /// Aggregate request/image/response byte reservation allowed in flight.
    #[arg(long, value_parser = positive_u64)]
    pub detailed_max_in_flight_bytes: Option<u64>,
    /// Existing video path or supported `YouTube` URL.
    #[arg(long)]
    pub source: String,
    /// Stable document ID. Defaults to a new unique recap ID.
    #[arg(long, value_parser = document_id)]
    pub id: Option<String>,
    /// Editorial focus. Omit for a general visual recap, not a conversation.
    #[arg(long, value_parser = nonempty)]
    pub focus: Option<String>,
    /// Explicit timestamped VTT or SRT transcript; otherwise use available captions.
    #[arg(long)]
    pub transcript: Option<PathBuf>,
    /// Explicit speech-to-text executable used only when captions/transcript are absent.
    /// It receives the video path after the literal arguments and must emit VTT/SRT.
    #[arg(long, conflicts_with = "transcript")]
    pub transcriber: Option<PathBuf>,
    /// Literal argument to --transcriber; repeat as needed (no shell expansion).
    #[arg(long, requires = "transcriber")]
    pub transcriber_arg: Vec<std::ffi::OsString>,
    /// Preferred `YouTube` caption language.
    #[arg(long, default_value = "en")]
    pub caption_language: String,
    /// Parent directory for bounded temporary downloads and extraction.
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    #[command(flatten)]
    pub model: ModelOptions,
    #[command(flatten)]
    pub tools: Tools,
    #[command(flatten)]
    pub budget: CreateBudget,
    #[command(flatten)]
    pub output: OutputOptions,
}

#[derive(Debug, Clone, Args)]
pub struct CreateBudget {
    /// Optional maximum source bytes (also enforced for local files); zero means unlimited.
    #[arg(long, default_value = "0")]
    pub max_video_bytes: u64,
    /// Optional maximum source duration in seconds; zero means unlimited.
    #[arg(long, default_value = "0")]
    pub max_duration_seconds: u64,
    /// Maximum sampled source frames.
    #[arg(long, default_value = "12", value_parser = clap::value_parser!(u32).range(1..))]
    pub max_frames: u32,
    /// Maximum silent short clips (zero disables clips).
    #[arg(long, default_value = "3")]
    pub max_clips: u32,
    /// Maximum combined duration of extracted silent clips, in milliseconds.
    #[arg(long, default_value = "24000", value_parser = positive_u64)]
    pub max_clip_duration_ms: u64,
    /// Maximum width of extracted source media, preserving aspect ratio.
    #[arg(long, default_value = "960", value_parser = clap::value_parser!(u32).range(1..))]
    pub max_width: u32,
    /// Maximum total extracted media bytes.
    #[arg(long, default_value = "25165824", value_parser = positive_u64)]
    pub max_media_bytes: u64,
    /// Optional acquisition/media subprocess deadline in seconds; zero disables it.
    #[arg(long, default_value = "0")]
    pub operation_timeout_seconds: u64,
}

#[derive(Debug, Clone, Args)]
pub struct Refine {
    /// Stored document ID, not a JSON input file.
    #[arg(value_parser = document_id)]
    pub id: String,
    /// Required editorial instruction for the next revision.
    #[arg(long, value_parser = nonempty)]
    pub focus: String,
    #[command(flatten)]
    pub model: ModelOptions,
    #[command(flatten)]
    pub output: OutputOptions,
}

#[derive(Debug, Clone, Args)]
pub struct OutputOptions {
    /// Destination HTML file (default: <id>.html in the current directory).
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    /// Explicitly permit replacing an existing output file.
    #[arg(long)]
    pub force: bool,
    /// Maximum raw bytes embedded from any single asset.
    #[arg(long, default_value = "12582912", value_parser = positive_u64)]
    pub max_export_asset_bytes: u64,
    /// Maximum total embedded media bytes, including base64 overhead.
    #[arg(long, default_value = "50331648", value_parser = positive_u64)]
    pub max_export_media_bytes: u64,
}

#[derive(Debug, Clone, Args)]
struct Export {
    #[arg(value_parser = document_id)]
    id: String,
    #[command(flatten)]
    output: OutputOptions,
}

#[derive(Debug, Clone, Args)]
struct Inspect {
    #[arg(value_parser = document_id)]
    id: String,
    /// Print the complete validated portable document as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone, Args)]
struct Doctor {
    /// Configured authentication account reference.
    #[arg(long)]
    auth_profile: Option<String>,
    #[command(flatten)]
    tools: Tools,
    /// Exercise real bounded `YouTube` acquisition (downloads video/captions, never inference).
    #[arg(long)]
    source: Option<String>,
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

fn parse_depth(value: &str) -> Result<bcode_recap::contracts::Depth, String> {
    use bcode_recap::contracts::Depth;
    match value {
        "brief" => Ok(Depth::Brief),
        "standard" => Ok(Depth::Standard),
        "detailed" => Ok(Depth::Detailed),
        _ => Err("depth must be brief, standard, or detailed".into()),
    }
}

fn positive_u64(value: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| "must be a positive integer".into())
}

pub fn nonempty(value: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        Err("must not be blank".into())
    } else {
        Ok(value.to_owned())
    }
}

pub fn document_id(value: &str) -> Result<String, String> {
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

/// Parse arguments and run the standalone application, reporting actionable failures.
pub async fn main() -> ExitCode {
    let arguments = Cli::parse();
    #[cfg(feature = "tui")]
    {
        let interactive = std::io::stdin().is_terminal()
            && std::io::stdout().is_terminal()
            && std::io::stderr().is_terminal();
        if arguments.command.is_none() || matches!(arguments.command, Some(Command::Tui(_))) {
            if arguments.command.is_none() && !interactive {
                return match Cli::command().print_help() {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(_) => ExitCode::FAILURE,
                };
            }
            if !interactive {
                eprintln!(
                    "recap: tui requires interactive stdin, stdout, and stderr terminals; use create, refine, export, inspect, or doctor for headless operation"
                );
                return ExitCode::FAILURE;
            }
            let mut options = match arguments.command {
                Some(Command::Tui(options)) => options,
                _ => application::TuiOptions::default(),
            };
            options.diagnostic_report = arguments.diagnostic_report;
            options.store = arguments.store;
            options.max_document_bytes = arguments.max_document_bytes;
            return match crate::tui::run(options).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("recap: {error}");
                    ExitCode::FAILURE
                }
            };
        }
    }
    #[cfg(not(feature = "tui"))]
    if arguments.command.is_none() {
        return match Cli::command().print_help() {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::FAILURE,
        };
    }
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

fn report_export(result: &application::ExportResult) {
    for degradation in &result.report.degradations {
        eprintln!("warning: media export degraded: {:?}", degradation.reason);
    }
    eprintln!(
        "Exported {} ({} embedded media bytes; {} omissions)",
        result.path.display(),
        result.report.embedded_media_bytes,
        result.report.degradations.len()
    );
}

async fn run(arguments: Cli, cancel: &Cancellation) -> Result<(), CliError> {
    let options = application::TuiOptions {
        store: arguments.store,
        diagnostic_report: arguments.diagnostic_report,
        max_document_bytes: arguments.max_document_bytes,
        ..application::TuiOptions::default()
    };
    let store = DocumentStore::new(&options.store, options.max_document_bytes);
    match arguments.command {
        Some(Command::Inspect(value)) => inspect(&store, &value),
        Some(Command::Export(value)) => {
            let outcome = application::execute(
                &options,
                application::Request::Export {
                    id: value.id,
                    options: value.output,
                },
                cancel,
                &mut progress,
            )
            .await?;
            if let application::Outcome::Exported(result) = outcome {
                report_export(&result);
            }
            Ok(())
        }
        Some(Command::Create(value)) => {
            if let Some(id) = &value.id {
                application::check_output_destination(id, &value.output)?;
            }
            let output = value.output.clone();
            let outcome = application::execute(
                &options,
                application::Request::Create(value),
                cancel,
                &mut progress,
            )
            .await?;
            finish_generation(&store, outcome, &output)
        }
        Some(Command::Refine(value)) => {
            application::check_output_destination(&value.id, &value.output)?;
            let output = value.output.clone();
            let outcome = application::execute(
                &options,
                application::Request::Refine(value),
                cancel,
                &mut progress,
            )
            .await?;
            finish_generation(&store, outcome, &output)
        }
        Some(Command::Doctor(value)) => {
            doctor(&value, cancel).await?;
            if let Some(source) = &value.source {
                application::execute(
                    &options,
                    application::Request::CheckSource {
                        source: source.clone(),
                        tools: value.tools.clone(),
                    },
                    cancel,
                    &mut progress,
                )
                .await?;
                eprintln!(
                    "Source video and captions acquired successfully. No model request was made; no recap was saved."
                );
            }
            Ok(())
        }
        #[cfg(feature = "tui")]
        Some(Command::Tui(_)) => Err(CliError::Action(
            "interactive routing was not initialized".into(),
        )),
        None => Err(CliError::Action(
            "interactive routing was not initialized".into(),
        )),
    }
}

fn finish_generation(
    store: &DocumentStore,
    outcome: application::Outcome,
    output: &OutputOptions,
) -> Result<(), CliError> {
    if let application::Outcome::Ready { document, export } = outcome {
        eprintln!(
            "Saved {} revision {}. Re-export anytime with: recap export {}",
            document.id, document.revision, document.id
        );
        let result = export.map_err(CliError::Action)?;
        report_export(&result);
    } else if let application::Outcome::Document(document) = outcome {
        eprintln!(
            "Saved {} revision {}. Re-export anytime with: recap export {}",
            document.id, document.revision, document.id
        );
        let result = application::export_committed(store, &document, output)?;
        report_export(&result);
    }
    Ok(())
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
        } else {
            eprintln!("ok: {name}");
        }
    }
    if let Some(config) = &options.model_config {
        let selection = ModelOptions {
            model_config: config.clone(),
            profile: options.profile.clone(),
            model: options.model.clone(),
            provider: options.provider.clone(),
            endpoint: None,
            auth_profile: options.auth_profile.clone(),
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
            "Tools available. Source availability/download was NOT checked by these version checks; use doctor --source URL. yt-dlp is needed only for YouTube; ffmpeg and ffprobe are required for visual extraction."
        );
        Ok(())
    } else {
        Err(CliError::Action(format!(
            "{failures} diagnostic checks failed; resolve the errors above and rerun recap doctor"
        )))
    }
}

fn parse_config_path(value: &str) -> Result<PathBuf, String> {
    if value.contains('\0') {
        return Err("configuration path contains an invalid character".into());
    }
    Ok(PathBuf::from(value))
}

fn progress(stage: Stage) {
    eprintln!(
        "{}",
        match stage {
            Stage::AnalysisProgress { .. } => return,
            Stage::ModelReadiness => "Resolving explicit embedded model configuration (no daemon)…",
            Stage::Acquisition => "Acquiring source and available captions…",
            Stage::Media => "Probing source and extracting real visual evidence…",
            Stage::Composition => "Composing grounded visual recap with configured model…",
            Stage::Analysis => "Analyzing source coverage and chapter outline…",
            Stage::Writing => "Writing source-grounded chapters…",
            Stage::Audit => "Auditing source coverage…",
            Stage::Repair => "Repairing source coverage gaps…",
            Stage::Storage => "Committing validated recap revision…",
            Stage::Export => "Rendering self-contained HTML…",
            Stage::Complete => "Recap revision committed.",
        }
    );
}

impl Default for OutputOptions {
    fn default() -> Self {
        let parsed =
            Cli::try_parse_from(["recap", "export", "default"]).expect("built-in export defaults");
        match parsed.command {
            Some(Command::Export(value)) => value.output,
            _ => unreachable!(),
        }
    }
}
#[cfg(feature = "tui")]
impl Create {
    /// Canonical clap defaults, without requiring a complete interactive form.
    pub fn defaults_for_form() -> Self {
        let parsed = Cli::try_parse_from([
            "recap",
            "create",
            "--source",
            "video.mp4",
            "--model-config",
            "provider.toml",
        ])
        .expect("built-in create defaults");
        match parsed.command {
            Some(Command::Create(value)) => *value,
            _ => unreachable!(),
        }
    }
}
#[cfg(feature = "tui")]
impl Refine {
    /// Canonical clap defaults, without requiring a complete interactive form.
    pub fn defaults_for_form() -> Self {
        let parsed = Cli::try_parse_from([
            "recap",
            "refine",
            "recap",
            "--focus",
            "Details",
            "--model-config",
            "provider.toml",
        ])
        .expect("built-in refine defaults");
        match parsed.command {
            Some(Command::Refine(value)) => *value,
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "tui")]
    #[test]
    fn interactive_entry_can_browse_without_model_configuration() {
        assert!(Cli::try_parse_from(["recap"]).unwrap().command.is_none());
        let parsed =
            Cli::try_parse_from(["recap", "tui", "--store", "notes", "--id", "lesson"]).unwrap();
        assert_eq!(parsed.store, PathBuf::from("notes"));
        let Some(Command::Tui(options)) = parsed.command else {
            panic!("expected tui")
        };
        assert!(options.model_config.is_none());
        assert_eq!(options.id.as_deref(), Some("lesson"));
    }

    #[cfg(feature = "tui")]
    #[test]
    fn tui_selection_is_optional_and_reaches_shared_job_options() {
        let parsed = Cli::try_parse_from([
            "recap",
            "tui",
            "--source",
            "lesson.mp4",
            "--model-config",
            "provider.toml",
            "--profile",
            "sol",
            "--context",
            "work",
            "--model",
            "selected",
            "--provider",
            "custom",
            "--id",
            "lesson",
        ])
        .unwrap();
        let Some(Command::Tui(options)) = parsed.command else {
            panic!("expected tui")
        };
        let transcript = Some(PathBuf::from("captions.vtt"));
        let create = options.create_request(
            options.source.clone().unwrap(),
            transcript.clone(),
            Some("Details".into()),
            bcode_recap::contracts::Depth::Detailed,
        );
        assert_eq!(create.model.profile.as_deref(), Some("sol"));
        assert_eq!(create.model.context.as_deref(), Some("work"));
        assert_eq!(create.model.model.as_deref(), Some("selected"));
        assert_eq!(create.model.provider.as_deref(), Some("custom"));
        assert_eq!(create.model.model_config, PathBuf::from("provider.toml"));
        assert_eq!(create.id.as_deref(), Some("lesson"));
        assert_eq!(create.transcript, transcript);
        assert_eq!(create.depth, bcode_recap::contracts::Depth::Detailed);
        let defaults = Create::defaults_for_form();
        let actual_engine = create_options(&create).unwrap();
        let default_engine = create_options(&defaults).unwrap();
        assert_eq!(
            actual_engine.media.max_frames,
            default_engine.media.max_frames
        );
        assert_eq!(
            actual_engine.media.max_asset_bytes,
            default_engine.media.max_asset_bytes
        );
        assert_eq!(
            actual_engine.composition.input_bytes,
            default_engine.composition.input_bytes
        );
        assert_eq!(
            actual_engine.composition.response_bytes,
            default_engine.composition.response_bytes
        );
        let refine = options.refine_request("lesson".into(), "Compare".into());
        assert_eq!(refine.focus, "Compare");
        assert_eq!(
            refine.model.max_output_tokens,
            create.model.max_output_tokens
        );
        assert_eq!(refine.model.max_prompt_bytes, create.model.max_prompt_bytes);
        assert!(!options.output_options().force);
        assert_eq!(
            options.output_options().max_export_media_bytes,
            defaults.output.max_export_media_bytes
        );
    }

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
        let Some(Command::Create(options)) = parsed.command else {
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
        assert_eq!(engine.detailed.allowances.max_input_bytes, 4096);
        assert_eq!(engine.detailed.allowances.max_response_bytes, 2048);
        assert_eq!(engine.detailed.allowances.max_image_bytes, 8192);
        assert_eq!(engine.media.max_frames, 4);
        assert_eq!(engine.media.max_clips, 0);
        assert_eq!(engine.media.operation_timeout, Duration::from_secs(9));
    }

    #[test]
    fn detailed_cli_allowances_reach_engine_without_changing_standard_defaults() {
        let baseline = Cli::try_parse_from(["recap", "create", "--source", "video.mp4"]).unwrap();
        let Some(Command::Create(baseline)) = baseline.command else {
            panic!("expected create")
        };
        assert_eq!(baseline.depth, bcode_recap::contracts::Depth::Standard);
        let defaults = bcode_recap::analysis_contracts::DetailedPolicy::default();
        let baseline_engine = create_options(&baseline).unwrap();
        assert_eq!(
            baseline_engine.detailed.allowances.max_model_calls,
            defaults.allowances.max_model_calls
        );
        assert_eq!(
            baseline_engine.detailed.allowances.max_source_windows,
            defaults.allowances.max_source_windows
        );
        assert_eq!(
            baseline_engine.detailed.allowances.max_repair_rounds,
            defaults.allowances.max_repair_rounds
        );
        assert_eq!(
            baseline_engine.detailed.allowances.max_input_bytes,
            baseline_engine.composition.input_bytes
        );
        let parsed = Cli::try_parse_from([
            "recap",
            "create",
            "--source",
            "video.mp4",
            "--depth",
            "detailed",
            "--detailed-max-model-calls",
            "37",
            "--detailed-max-source-windows",
            "19",
            "--detailed-max-repair-rounds",
            "0",
            "--detailed-max-concurrent-requests",
            "2",
            "--detailed-max-in-flight-bytes",
            "8388608",
        ])
        .unwrap();
        let Some(Command::Create(options)) = parsed.command else {
            panic!("expected create")
        };
        assert_eq!(options.depth, bcode_recap::contracts::Depth::Detailed);
        let engine = create_options(&options).unwrap();
        assert_eq!(engine.detailed.allowances.max_model_calls, 37);
        assert_eq!(engine.detailed.allowances.max_source_windows, 19);
        assert_eq!(engine.detailed.allowances.max_repair_rounds, 0);
        assert_eq!(engine.detailed.max_concurrent_requests, 2);
        assert_eq!(engine.detailed.max_in_flight_bytes, 8 * 1024 * 1024);
        for flag in [
            "--detailed-max-model-calls",
            "--detailed-max-source-windows",
        ] {
            assert!(
                Cli::try_parse_from(["recap", "create", "--source", "video.mp4", flag, "0"])
                    .is_ok()
            );
        }
        for (flag, value) in [
            ("--depth", "unknown"),
            ("--detailed-max-concurrent-requests", "0"),
            ("--detailed-max-in-flight-bytes", "0"),
            ("--detailed-max-repair-rounds", "-1"),
        ] {
            assert!(
                Cli::try_parse_from(["recap", "create", "--source", "video.mp4", flag, value])
                    .is_err()
            );
        }
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
    fn create_without_conversation_discovers_model_configuration() {
        let automatic = Cli::try_parse_from(["recap", "create", "--source", "lesson.mp4"]).unwrap();
        assert!(
            matches!(automatic.command, Some(Command::Create(value)) if value.model.model_config.as_os_str().is_empty())
        );
        let parsed = Cli::try_parse_from([
            "recap",
            "create",
            "--source",
            "lesson.mp4",
            "--model-config",
            "provider.toml",
        ])
        .unwrap();
        let Some(Command::Create(options)) = parsed.command else {
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
    fn acquisition_limits_are_opt_in() {
        for flag in [
            "--max-video-bytes",
            "--max-duration-seconds",
            "--operation-timeout-seconds",
        ] {
            for value in ["0", "12345"] {
                let parsed =
                    Cli::try_parse_from(["recap", "create", "--source", "lesson.mp4", flag, value])
                        .unwrap();
                let Some(Command::Create(request)) = parsed.command else {
                    panic!("expected create")
                };
                let budget = request.budget;
                let selected = match flag {
                    "--max-video-bytes" => budget.max_video_bytes,
                    "--max-duration-seconds" => budget.max_duration_seconds,
                    _ => budget.operation_timeout_seconds,
                };
                assert_eq!(selected, value.parse::<u64>().unwrap());
            }
        }
        let parsed = Cli::try_parse_from(["recap", "create", "--source", "lesson.mp4"]).unwrap();
        let Some(Command::Create(request)) = parsed.command else {
            panic!("expected create")
        };
        assert_eq!(request.budget.max_video_bytes, 0);
        assert_eq!(request.budget.max_duration_seconds, 0);
        assert_eq!(request.budget.operation_timeout_seconds, 0);
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
        let Some(Command::Create(options)) = parsed.command else {
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
