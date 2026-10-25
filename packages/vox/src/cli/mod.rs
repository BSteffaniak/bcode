//! Standalone Vox command-line frontend. Domain behavior stays behind library APIs.
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::config::Config;
use crate::contracts::{
    Action, Cancellation, Recording, SpeechGate, Status, TextProcessor, Transcript,
};
use crate::{Error, Result};

mod interrupt;
use interrupt::Interrupt;

#[cfg(test)]
mod tests;

#[derive(Debug, Parser)]
#[command(
    name = "vox",
    version,
    about = "Local-first final-only voice dictation"
)]
struct Cli {
    /// Explicit TOML config file; missing explicit files are errors.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Machine-readable output (watch emits newline-delimited JSON).
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the foreground local control service; does not start recording.
    Serve,
    /// Start one recording in the already-running service.
    Start,
    /// Stop recording, recognize final text and deliver once.
    Stop,
    /// Toggle recording in the already-running service.
    Toggle,
    /// Cancel active recording or processing without delivering its text.
    Cancel,
    /// Read service state; never includes transcript text.
    Status {
        /// Poll for state changes until interrupted (250 ms interval).
        #[arg(long)]
        watch: bool,
    },
    /// Transcribe an existing WAV to stdout only; never type or use the clipboard.
    Transcribe(TranscribeArgs),
    /// Manage explicit local model artifacts; install is a network operation.
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
    /// Report compiled/configured capabilities, not unperformed live tests.
    Doctor,
    /// Print example configuration without writing any files.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Print compositor bindings; does not register global shortcuts.
    Shortcuts {
        #[arg(value_enum)]
        compositor: CompositorArg,
    },
}

#[derive(Debug, Args)]
struct TranscribeArgs {
    /// Input WAV file.
    wav: PathBuf,
    /// Override the local model path; does not install or download it.
    #[arg(long)]
    model: Option<PathBuf>,
    /// Explicitly override configured recognition backend; no automatic fallback.
    #[arg(long, value_enum)]
    backend: Option<BackendArg>,
    /// Recognition language hint.
    #[arg(long)]
    language: Option<String>,
    /// Vocabulary hint, repeatable (not a phrase replacement rule).
    #[arg(long)]
    vocabulary: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum BackendArg {
    Local,
    Cloud,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CompositorArg {
    Hyprland,
    Sway,
}

#[derive(Debug, Subcommand)]
enum ModelCommand {
    /// List catalog entries and installed state in the XDG/HOME model directory.
    List,
    /// Download a catalog model and verify its checksum before publishing it.
    Install { id: String },
    /// Verify a catalog model's installed artifact against its checksum.
    Verify { id: String },
    /// Remove only the selected catalog model artifact.
    Remove { id: String },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Print a secret-free starter TOML; configure a model path before serving.
    Example,
}

/// Parse process arguments and run one explicit command.
///
/// Default configuration is read from `$XDG_CONFIG_HOME/vox/config.toml`, or
/// `$HOME/.config/vox/config.toml`. An absent default file uses built-in defaults;
/// an unreadable or malformed existing file is an error. An explicit `--config`
/// is always required to exist. No command automatically launches a service,
/// registers a global shortcut, captures audio, or falls back to a cloud backend.
/// Only `serve` constructs the capture runtime; `start` and `toggle` request it
/// from the existing service. `transcribe` writes final text to stdout only.
///
/// Registers a scoped Ctrl-C handler for file recognition and model hashing or
/// installation. Cancellation is cooperative; in-flight backend calls may take
/// time to return. OS signal handling remains process-global after registration.
///
/// # Errors
/// Returns errors for invalid arguments/configuration, unsupported capabilities,
/// failed service requests, model operations, WAV processing or output writes.
/// Errors may carry internal context: frontends should use [`diagnostic`] rather
/// than printing their raw display representation at a public boundary.
pub fn run() -> Result<()> {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            error.print()?;
            return Ok(());
        }
        Err(_) => return Err(Error::Invalid("invalid arguments; use vox --help".into())),
    };
    dispatch(cli)
}

/// Secret-safe public error message. Never renders untrusted backend/IO context.
#[must_use]
pub const fn diagnostic(error: &Error) -> &'static str {
    match error {
        Error::Invalid(_) => {
            "invalid input or configuration; consult vox --help and your config file"
        }
        Error::Unsupported(_) => {
            "required capability is not supported by this build or platform; run vox doctor"
        }
        Error::Busy => "another Vox operation is active",
        Error::Cancelled => "operation cancelled",
        Error::Io(_) => "local IO failed; check files, permissions and the running Vox service",
        Error::Backend(_) => {
            "operation failed; check configured capabilities and model installation (no automatic retry)"
        }
    }
}

fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Serve => crate::service::serve(load_config(cli.config.as_deref())?),
        Command::Start => control(Action::Start, cli.json),
        Command::Stop => control(Action::Stop, cli.json),
        Command::Toggle => control(Action::Toggle, cli.json),
        Command::Cancel => control(Action::Cancel, cli.json),
        Command::Status { watch } => status(watch, cli.json),
        Command::Transcribe(args) => {
            let mut config = load_config(cli.config.as_deref())?;
            apply_transcribe_options(&mut config, &args)?;
            let interrupt = Interrupt::new()?;
            let cancel = interrupt.cancellation();
            let recording = crate::audio::read_wav(&args.wav)?;
            cancel.check()?;
            let mut gate = crate::speech::gate(&config)?;
            let processor = crate::text::processor(&config)?;
            let transcript = transcribe_recording(
                &recording,
                &config,
                gate.as_mut(),
                processor.as_ref(),
                &cancel,
                || crate::recognition::recognizer(&config),
            )?;
            cancel.check()?;
            let mut output = io::stdout().lock();
            if cli.json {
                write_json(&mut output, &transcript)
            } else {
                writeln!(output, "{}", transcript.text).map_err(Error::from)
            }
        }
        Command::Model { command } => models(command, cli.json),
        Command::Doctor => doctor(&load_config(cli.config.as_deref())?, cli.json),
        Command::Config {
            command: ConfigCommand::Example,
        } => {
            let example = config_example()?;
            let mut output = io::stdout().lock();
            if cli.json {
                write_json(&mut output, &serde_json::json!({"toml": example}))
            } else {
                output.write_all(example.as_bytes()).map_err(Error::from)
            }
        }
        Command::Shortcuts { compositor } => {
            let compositor = match compositor {
                CompositorArg::Hyprland => crate::shortcuts::Compositor::Hyprland,
                CompositorArg::Sway => crate::shortcuts::Compositor::Sway,
            };
            let snippet = crate::shortcuts::snippet(compositor);
            let mut output = io::stdout().lock();
            if cli.json {
                write_json(&mut output, &serde_json::json!({"snippet": snippet}))
            } else {
                output.write_all(snippet.as_bytes()).map_err(Error::from)
            }
        }
    }
}

fn default_config_path() -> Option<PathBuf> {
    default_config_path_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

fn default_config_path_from(
    xdg: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    xdg.filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.filter(|path| !path.is_empty())
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|path| path.join(".config"))
        })
        .map(|base| base.join("vox/config.toml"))
}

fn load_config(explicit: Option<&Path>) -> Result<Config> {
    load_config_at(explicit, default_config_path().as_deref())
}

fn load_config_at(explicit: Option<&Path>, default: Option<&Path>) -> Result<Config> {
    if let Some(path) = explicit {
        return Config::load(path);
    }
    if let Some(path) = default {
        match std::fs::metadata(path) {
            Ok(_) => return Config::load(path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let config = Config::default();
    config.validate()?;
    Ok(config)
}

fn apply_transcribe_options(config: &mut Config, args: &TranscribeArgs) -> Result<()> {
    if let Some(backend) = args.backend {
        config.backend = match backend {
            BackendArg::Local => "local",
            BackendArg::Cloud => "cloud",
        }
        .into();
    }
    if let Some(model) = &args.model {
        if config.backend != "local" {
            return Err(Error::Invalid(
                "--model is a local model path; cloud model names belong in config".into(),
            ));
        }
        config.model_path = Some(model.clone());
    }
    if let Some(language) = &args.language {
        config.recognition.language = Some(language.clone());
    }
    config
        .recognition
        .vocabulary
        .extend(args.vocabulary.iter().cloned());
    // File transcription must not acquire any desktop delivery adapter.
    config.delivery = crate::contracts::DeliveryMode::Stdout;
    config.validate()
}

fn transcribe_recording<F>(
    recording: &Recording,
    config: &Config,
    gate: &mut dyn SpeechGate,
    processor: &dyn TextProcessor,
    cancel: &Cancellation,
    recognizer: F,
) -> Result<Transcript>
where
    F: FnOnce() -> Result<Box<dyn crate::contracts::Recognizer>>,
{
    cancel.check()?;
    if recording.sample_rate == 0 {
        return Err(Error::Invalid("WAV sample rate must be nonzero".into()));
    }
    let max_frames = u64::from(recording.sample_rate) * u64::from(config.max_recording_seconds);
    if u64::try_from(recording.samples.len()).unwrap_or(u64::MAX) > max_frames {
        return Err(Error::Invalid(
            "WAV exceeds configured maximum recording duration".into(),
        ));
    }
    let has_speech = gate.has_speech(recording)?;
    cancel.check()?;
    if !has_speech {
        return Ok(Transcript {
            text: String::new(),
        });
    }
    let mut recognizer = recognizer()?;
    let transcript = recognizer.recognize(recording, &config.recognition, cancel)?;
    cancel.check()?;
    let text = processor.process(&transcript.text)?;
    cancel.check()?;
    Ok(Transcript { text })
}

fn control(action: Action, json: bool) -> Result<()> {
    // A single request, never a retry after an ambiguous delivery failure.
    emit_status(crate::service::action(action)?, json)
}

fn status(watch: bool, json: bool) -> Result<()> {
    let mut previous = None;
    loop {
        let current = crate::service::status()?;
        if previous.as_ref() != Some(&current) {
            emit_status(current.clone(), json)?;
            previous = Some(current);
        }
        if !watch {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn safe_status(status: Status) -> Status {
    match status {
        Status::Failed { .. } => Status::Failed {
            message: "Vox operation failed; check configuration and capabilities".into(),
        },
        status => status,
    }
}

fn emit_status(status: Status, json: bool) -> Result<()> {
    // Defense in depth: do not expose arbitrary runtime diagnostic strings.
    let status = safe_status(status);
    let mut output = io::stdout().lock();
    if json {
        write_json(&mut output, &status)?;
    } else {
        match status {
            Status::Idle => writeln!(output, "idle")?,
            Status::Recording { operation } => writeln!(output, "recording {operation}")?,
            Status::Transcribing { operation } => writeln!(output, "transcribing {operation}")?,
            Status::Delivering { operation } => writeln!(output, "delivering {operation}")?,
            Status::Failed { .. } => {
                writeln!(output, "failed: check configuration and capabilities")?;
            }
        }
    }
    output.flush()?;
    Ok(())
}

fn write_json<W: Write, T: Serialize>(output: &mut W, value: &T) -> Result<()> {
    serde_json::to_writer(&mut *output, value)
        .map_err(|_| Error::Backend("could not write JSON output".into()))?;
    writeln!(output)?;
    Ok(())
}

fn models(command: ModelCommand, json: bool) -> Result<()> {
    let mut output = io::stdout().lock();
    match command {
        ModelCommand::List => {
            let entries = crate::models::list()?;
            if json {
                return write_json(&mut output, &entries);
            }
            writeln!(
                output,
                "Model directory: {}",
                crate::models::data_dir()?.display()
            )?;
            writeln!(
                output,
                "Presence is metadata-only; use vox model verify <id> for integrity."
            )?;
            for entry in entries {
                writeln!(
                    output,
                    "{}\t{:?}\t{}\t{} bytes\t{}",
                    entry.entry.id,
                    entry.state,
                    entry.entry.name,
                    entry.entry.size_bytes,
                    entry.entry.license
                )?;
            }
        }
        ModelCommand::Install { id } => {
            let interrupt = Interrupt::new()?;
            let path = crate::models::install(&id, &interrupt.cancellation())?;
            if json {
                write_json(
                    &mut output,
                    &serde_json::json!({"installed": true, "id": id, "path": path}),
                )?;
            } else {
                writeln!(output, "Installed and verified: {}", path.display())?;
            }
        }
        ModelCommand::Verify { id } => {
            let interrupt = Interrupt::new()?;
            let path = crate::models::verify_with_cancel(&id, &interrupt.cancellation())?;
            if json {
                write_json(
                    &mut output,
                    &serde_json::json!({"verified": true, "id": id, "path": path}),
                )?;
            } else {
                writeln!(output, "Verified: {}", path.display())?;
            }
        }
        ModelCommand::Remove { id } => {
            crate::models::remove(&id)?;
            if json {
                write_json(&mut output, &serde_json::json!({"absent": true, "id": id}))?;
            } else {
                writeln!(output, "Selected model is absent")?;
            }
        }
    }
    Ok(())
}

fn config_example() -> Result<String> {
    let config = toml::to_string_pretty(&Config::default())
        .map_err(|_| Error::Backend("could not encode example config".into()))?;
    Ok(format!(
        "# Vox config.toml — local recognition, focused typing, no cloud fallback.\n\
# Install a model explicitly with vox model install <id>, then set:\n\
# model_path = '/absolute/path/to/installed/model'\n\
# Secrets belong in the environment, never in this file.\n\
# hold_shortcut / toggle_shortcut require an embedding platform adapter.\n\
# For compositor bindings use vox shortcuts hyprland or vox shortcuts sway.\n\
# Clipboard delivery must be explicitly selected; it is never a fallback.\n\n{config}"
    ))
}

#[derive(Serialize)]
struct AudioBuildCapabilities {
    native_audio_compiled: bool,
    local_inference_compiled: bool,
}

#[derive(Serialize)]
struct DesktopBuildCapabilities {
    wayland_adapter_compiled: bool,
    x11_adapter_compiled: bool,
}

#[derive(Serialize)]
struct DoctorReport {
    configuration: &'static str,
    recognition_backend: String,
    delivery_mode: crate::contracts::DeliveryMode,
    #[serde(flatten)]
    audio: AudioBuildCapabilities,
    #[serde(flatten)]
    desktop: DesktopBuildCapabilities,
    global_shortcut_portal: &'static str,
    model: &'static str,
    live_checks: &'static str,
}

fn doctor_report(config: &Config) -> DoctorReport {
    let model =
        config
            .model_path
            .as_ref()
            .map_or("not configured", |path| match std::fs::metadata(path) {
                Ok(metadata) if metadata.is_file() => {
                    "file exists; integrity and compatibility not verified"
                }
                Ok(_) => "not a regular file",
                Err(error) if error.kind() == io::ErrorKind::NotFound => "missing",
                Err(_) => "metadata unavailable",
            });
    DoctorReport {
        configuration: "valid",
        recognition_backend: config.backend.clone(),
        delivery_mode: config.delivery,
        audio: AudioBuildCapabilities {
            native_audio_compiled: cfg!(feature = "native-audio"),
            local_inference_compiled: cfg!(feature = "local-inference"),
        },
        desktop: DesktopBuildCapabilities {
            wayland_adapter_compiled: cfg!(all(target_os = "linux", feature = "wayland")),
            x11_adapter_compiled: cfg!(all(target_os = "linux", feature = "x11")),
        },
        global_shortcut_portal: "not implemented; use compositor bindings or a host adapter",
        model,
        live_checks: "not performed: microphone, recognition, cloud, focused typing, clipboard, compositor and service",
    }
}

fn doctor(config: &Config, json: bool) -> Result<()> {
    let report = doctor_report(config);
    let mut output = io::stdout().lock();
    if json {
        return write_json(&mut output, &report);
    }
    writeln!(output, "Vox configuration: {}", report.configuration)?;
    writeln!(
        output,
        "Recognition backend: {} (explicit; no fallback)",
        report.recognition_backend
    )?;
    writeln!(output, "Delivery: {:?}", report.delivery_mode)?;
    writeln!(output, "Compiled adapters (not live-tested):")?;
    writeln!(
        output,
        "  microphone: {}",
        report.audio.native_audio_compiled
    )?;
    writeln!(
        output,
        "  local inference: {}",
        report.audio.local_inference_compiled
    )?;
    writeln!(
        output,
        "  Wayland: {}",
        report.desktop.wayland_adapter_compiled
    )?;
    writeln!(output, "  X11: {}", report.desktop.x11_adapter_compiled)?;
    writeln!(output, "Model: {}", report.model)?;
    writeln!(output, "Shortcut portal: {}", report.global_shortcut_portal)?;
    writeln!(output, "Live checks: {}", report.live_checks)?;
    Ok(())
}
