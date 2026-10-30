//! JSONL CLI adaptation for X-ray, with authorized matching-client endpoint bootstrap.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bcode_client::xray::XrayClient;
use bcode_session_models::SessionId;
use clap::Args;

use crate::CliError;
use bcode_xray::recording::{JsonlBundleRecorder, RecordingOptions, RecordingSink};

#[cfg(test)]
mod tests;

/// Local HTTP configuration. Omit both overrides to bootstrap through the session client.
#[derive(Debug, Args)]
pub struct XrayArgs {
    /// Session to observe, including while idle. Never submits a turn.
    pub session_id: SessionId,
    /// Trusted public local HTTP origin override (requires --token-file).
    #[arg(long, requires = "token_file")]
    pub endpoint: Option<String>,
    /// Private regular file containing the bearer credential (not a URL or command argument).
    #[arg(long, requires = "endpoint")]
    pub token_file: Option<PathBuf>,
    /// Explicitly enable sensitive capture only during this observation.
    #[arg(long)]
    pub deep: bool,
    /// New recording bundle directory, including bounded verified referenced payloads.
    /// Never appended to or silently overwritten.
    #[arg(long)]
    pub record: Option<PathBuf>,
    /// Request finalization after this many seconds; otherwise use Ctrl-C.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    pub duration_seconds: Option<u64>,
    /// Maximum time to drain after requesting a delivery cutoff.
    #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..=300))]
    pub finalize_timeout_seconds: u64,
}

/// Observe across idle periods and turns, writing each envelope as one flushed JSON line.
/// Readiness is written before waiting for more events. Ctrl-C requests an observation
/// delivery cutoff, never session cancellation. Broken stdout is an error, not completion.
///
/// # Errors
/// Fails closed on unsafe credential files, unavailable transport, protocol disagreement,
/// recording failure or missing finalization. Already written output may be partial.
pub async fn run(args: &XrayArgs) -> Result<(), CliError> {
    match (&args.endpoint, &args.token_file) {
        (Some(endpoint), Some(token_file)) => run_endpoint(args, endpoint, token_file).await,
        (None, None) => {
            let owner = crate::session_read_client(args.session_id).await?;
            let lease = owner.open_xray_endpoint(args.session_id, args.deep).await?;
            // Keep the original authenticated connection: revocation is client-ID scoped.
            let result =
                run_endpoint(args, &lease.info().endpoint, &lease.info().credential_file).await;
            let close = lease.close().await;
            match (result, close) {
                (Ok(()), Ok(())) => Ok(()),
                (Err(error), Ok(())) => Err(error),
                (_, Err(_)) => Err(invalid(
                    "X-ray endpoint cleanup failed; listener revocation is unconfirmed (session work is unaffected)",
                )),
            }
        }
        _ => Err(invalid(
            "--endpoint and --token-file must be supplied together",
        )),
    }
}

async fn run_endpoint(args: &XrayArgs, endpoint: &str, token_file: &Path) -> Result<(), CliError> {
    let token = read_token(token_file)?;
    let client =
        XrayClient::new(endpoint, token.trim()).map_err(|error| transport_error(&error))?;
    let mut stream = client
        .observe(bcode_xray_models::XrayObserveRequest {
            schema_version: bcode_xray_models::XRAY_SCHEMA_VERSION,
            session_id: args.session_id.to_string(),
            deep_capture: args.deep,
        })
        .await
        .map_err(|error| transport_error(&error))?;
    let result = async {
    let mut recorder = open_recording(args.record.as_deref(), stream.observation_id(), &args.session_id.to_string())?;
    let mut stdout = std::io::stdout();
    // observe() already validated and retained this ready barrier.
    let ready = stream.recv().await.map_err(|error| transport_error(&error))?
        .ok_or_else(|| invalid("X-ray ready barrier unavailable"))?;
    let capabilities: bcode_xray_models::XrayCapabilities = serde_json::from_value(ready.event.data.clone())?;
    let range_bytes = u32::try_from(capabilities.max_payload_range_bytes.min(64 * 1024))
        .map_err(|_| invalid("invalid X-ray payload range capability"))?;
    write_envelope(&mut stdout, recorder.as_mut(), &ready)?;

    let stop = async {
        if let Some(seconds) = args.duration_seconds {
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(seconds)) => Ok(()),
                result = tokio::signal::ctrl_c() => result,
            }
        } else {
            tokio::signal::ctrl_c().await
        }
    };
    tokio::pin!(stop);
    loop {
        tokio::select! {
            result = &mut stop => {
                result?;
                break;
            }
            envelope = stream.recv() => {
                match envelope.map_err(|error| transport_error(&error))? {
                    Some(envelope) => {
                        record_envelope(&client, &mut stdout, recorder.as_mut(), &envelope, range_bytes).await?;
                    }
                    None => return finish_recording(recorder, stream.finalization()),
                }
            }
        }
    }
    let drain = async {
        client.finalize(stream.observation_id()).await.map_err(|error| transport_error(&error))?;
        while let Some(envelope) = stream.recv().await.map_err(|error| transport_error(&error))? {
            record_envelope(&client, &mut stdout, recorder.as_mut(), &envelope, range_bytes).await?;
        }
        Ok::<(), CliError>(())
    };
    tokio::time::timeout(Duration::from_secs(args.finalize_timeout_seconds), drain)
        .await.map_err(|_| invalid("X-ray finalization timed out; output and recording are incomplete"))??;
    finish_recording(recorder, stream.finalization())
    }.await;
    // Payload export happens inside the loop, including after a cutoff was established.
    // Release only after all consumed envelopes were handled, even if local export failed.
    // An active stream failure is closed by Drop; bootstrap additionally revokes its endpoint.
    if stream.finalization().is_some() && client.release(stream.observation_id()).await.is_err() {
        return Err(invalid(
            "X-ray retained-payload release is unconfirmed; session work is unaffected",
        ));
    }
    result
}

async fn record_envelope(
    client: &XrayClient,
    stdout: &mut (impl std::io::Write + Send),
    mut recorder: Option<&mut Box<dyn RecordingSink>>,
    envelope: &bcode_xray_models::XrayEnvelope,
    range_bytes: u32,
) -> Result<(), CliError> {
    write_envelope(stdout, recorder.as_deref_mut(), envelope)?;
    if let Some(recorder) = recorder
        && let Err(error) = client
            .record_payloads(envelope, recorder.as_mut(), range_bytes)
            .await
    {
        // Persist an explicit interrupted boundary where possible; never finalize.
        let _ = recorder.interrupt();
        return Err(transport_error(&error));
    }
    Ok(())
}

fn open_recording(
    destination: Option<&Path>,
    observation_id: &str,
    session_id: &str,
) -> Result<Option<Box<dyn RecordingSink>>, CliError> {
    destination.map(|path| {
        JsonlBundleRecorder::create(path, observation_id, session_id, RecordingOptions::default())
            .map(|recorder| Box::new(recorder) as Box<dyn RecordingSink>)
            .map_err(|_| invalid("X-ray recording could not be created; destination must be a new absolute private bundle path"))
    }).transpose()
}

fn write_envelope(
    stdout: &mut impl std::io::Write,
    recorder: Option<&mut Box<dyn RecordingSink>>,
    envelope: &bcode_xray_models::XrayEnvelope,
) -> Result<(), CliError> {
    if let Some(recorder) = recorder {
        recorder
            .append(envelope)
            .map_err(|_| invalid("X-ray recording write failed; recording is incomplete"))?;
    }
    serde_json::to_writer(&mut *stdout, envelope)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

fn finish_recording(
    recorder: Option<Box<dyn RecordingSink>>,
    boundary: Option<&bcode_xray_models::XrayFinalization>,
) -> Result<(), CliError> {
    let boundary = boundary
        .ok_or_else(|| invalid("X-ray delivery cutoff unavailable; evidence is incomplete"))?;
    if let Some(mut recorder) = recorder {
        recorder
            .finalize(boundary)
            .map_err(|_| invalid("X-ray recording finalization failed; recording is incomplete"))?;
    }
    Ok(())
}

fn transport_error(error: &bcode_client::xray::XrayClientError) -> CliError {
    invalid(&error.to_string())
}

fn invalid(message: &str) -> CliError {
    CliError::InvalidArguments(message.to_owned())
}

fn read_token(path: &Path) -> Result<String, CliError> {
    let before = std::fs::symlink_metadata(path)
        .map_err(|_| invalid("X-ray credential file unavailable"))?;
    if !before.is_file() || before.file_type().is_symlink() {
        return Err(invalid("X-ray credential must be a private regular file"));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|_| invalid("X-ray credential file unavailable"))?;
    let metadata = file
        .metadata()
        .map_err(|_| invalid("X-ray credential file unavailable"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.mode() & 0o077 != 0
            || before.dev() != metadata.dev()
            || before.ino() != metadata.ino()
        {
            return Err(invalid(
                "X-ray credential must be a stable private regular file (mode 0600)",
            ));
        }
    }
    if !metadata.is_file() || metadata.len() > 4096 {
        return Err(invalid("invalid X-ray credential file"));
    }
    let mut bytes = Vec::new();
    file.take(4097)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid("X-ray credential file unavailable"))?;
    if bytes.len() > 4096 {
        return Err(invalid("invalid X-ray credential file"));
    }
    String::from_utf8(bytes).map_err(|_| invalid("invalid X-ray credential file"))
}
