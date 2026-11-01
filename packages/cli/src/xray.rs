//! JSONL CLI adaptation for X-ray, with authorized matching-client endpoint bootstrap.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bcode_client::xray::XrayClient;
use bcode_session_models::SessionId;
use clap::Args;

use crate::CliError;
use bcode_xray::recording::{JsonlBundleRecorder, RecordingOptions, RecordingSink};

mod reconnect;

#[cfg(test)]
mod tests;

/// Explicit live reattachment policy; never implies durable replay.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum)]
pub enum ReconnectPolicy {
    /// Fail closed on the first interruption.
    #[default]
    Never,
    /// Fresh authorized observations after transport loss, with capture gaps.
    Transport,
}

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
    /// Exact serialized HTTP loopback browser Origin granted only to this bootstrapped endpoint.
    #[arg(long, requires = "origin_lifetime_seconds", conflicts_with_all = ["endpoint", "token_file"], value_parser = parse_browser_origin)]
    pub allow_origin: Option<String>,
    /// Explicit positive lifetime for the one-endpoint browser Origin grant.
    #[arg(long, requires = "allow_origin", conflicts_with_all = ["endpoint", "token_file"], value_parser = clap::value_parser!(u64).range(1..))]
    pub origin_lifetime_seconds: Option<u64>,
    /// Print endpoint and private token-file locator to stderr for trusted browser provisioning.
    /// Never prints bearer bytes. Keep this process alive; exit revokes the endpoint.
    #[arg(long, requires = "allow_origin", conflicts_with_all = ["endpoint", "token_file"])]
    pub browser_handoff: bool,
    /// Reattach after transport loss only; every new observation is explicitly gapful.
    #[arg(long, value_enum, default_value_t = ReconnectPolicy::Never)]
    pub reconnect: ReconnectPolicy,
    /// Total additional admission attempts, not reset after a successful segment.
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..))]
    pub reconnect_retries: u32,
    /// Delay between fresh admission attempts.
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(1..=60000))]
    pub reconnect_delay_ms: u64,
    /// Explicitly enable sensitive capture only during this observation.
    #[arg(long)]
    pub deep: bool,
    /// New recording bundle directory, including bounded verified referenced payloads.
    /// Never appended to or silently overwritten.
    #[arg(long)]
    pub record: Option<PathBuf>,
    /// Maximum queued envelopes while stdout, payload export or recording is busy.
    /// Each frame is separately client-bounded (two MiB SSE limit); sustained slow
    /// consumers still fail on overflow with an interrupted, incomplete bundle.
    #[arg(long, default_value_t = 256, value_parser = clap::value_parser!(u16).range(1..=4096))]
    pub consumer_queue_capacity: u16,
    /// Request finalization after this many seconds; otherwise use Ctrl-C.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    pub duration_seconds: Option<u64>,
    /// Maximum time to drain the network after requesting a delivery cutoff.
    /// Accepted local IO is joined before payload release, not detached on timeout.
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
    let origin_grant = origin_grant(args)?;
    if args.reconnect == ReconnectPolicy::Transport {
        if args.endpoint.is_some() || args.token_file.is_some() || args.browser_handoff {
            return Err(invalid(
                "--reconnect transport requires automatic bootstrap and cannot currently hand off browser credentials",
            ));
        }
        return reconnect::run(args, origin_grant).await;
    }
    match (&args.endpoint, &args.token_file) {
        (Some(endpoint), Some(token_file)) => run_endpoint(args, endpoint, token_file).await,
        (None, None) => {
            let owner = crate::session_read_client(args.session_id).await?;
            let mut connection = owner.connect("bcode-xray").await?;
            let endpoint = connection
                .open_xray_endpoint_with_origin(args.session_id, args.deep, origin_grant)
                .await?;
            // Keep the original authenticated connection: revocation is client-ID scoped.
            let result = async {
                if args.browser_handoff {
                    write_browser_handoff(
                        &mut std::io::stderr(),
                        args,
                        &endpoint.endpoint,
                        &endpoint.credential_file,
                    )?;
                }
                run_endpoint(args, &endpoint.endpoint, &endpoint.credential_file).await
            }
            .await;
            let close = connection.close_xray_endpoint(&endpoint.endpoint_id).await;
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

// Origin validation is syntax-only and never resolves DNS or broadens a grant.
// Accept only exact serialized HTTP loopback origins, matching the host boundary.
fn parse_browser_origin(origin: &str) -> Result<String, String> {
    let invalid = || {
        "expected an exact HTTP loopback Origin (no path, wildcard, credentials, query or fragment)"
            .to_owned()
    };
    let authority = origin.strip_prefix("http://").ok_or_else(invalid)?;
    if origin.len() > 256 || !origin.is_ascii() || authority.is_empty() {
        return Err(invalid());
    }
    let (host, port) = if authority.starts_with('[') {
        let end = authority.find(']').ok_or_else(invalid)?;
        let host = &authority[..=end];
        let suffix = &authority[end + 1..];
        let port = if suffix.is_empty() {
            None
        } else {
            Some(suffix.strip_prefix(':').ok_or_else(invalid)?)
        };
        (host, port)
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    let loopback = host == "localhost"
        || host == "[::1]"
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback() && ip.to_string() == host);
    if !loopback {
        return Err(invalid());
    }
    if let Some(port) = port {
        let number = port.parse::<u16>().map_err(|_| invalid())?;
        if number == 80 || number.to_string() != port {
            return Err(invalid());
        }
    }
    Ok(origin.to_owned())
}

fn origin_grant(args: &XrayArgs) -> Result<Option<bcode_ipc::XrayOriginGrantRequest>, CliError> {
    if (args.allow_origin.is_some()
        || args.origin_lifetime_seconds.is_some()
        || args.browser_handoff)
        && (args.endpoint.is_some() || args.token_file.is_some())
    {
        return Err(invalid(
            "browser Origin grants/handoff require automatic bootstrap, not an external endpoint",
        ));
    }
    match (&args.allow_origin, args.origin_lifetime_seconds) {
        (Some(origin), Some(lifetime_seconds)) if lifetime_seconds > 0 => {
            parse_browser_origin(origin)
                .map_err(|_| invalid("invalid exact loopback browser Origin"))?;
            Ok(Some(bcode_ipc::XrayOriginGrantRequest {
                origin: origin.clone(),
                lifetime_seconds,
            }))
        }
        (None, None) if !args.browser_handoff => Ok(None),
        _ => Err(invalid(
            "--allow-origin and --origin-lifetime-seconds must be supplied together with a positive lifetime",
        )),
    }
}

fn write_browser_handoff(
    output: &mut impl std::io::Write,
    args: &XrayArgs,
    endpoint: &str,
    credential_file: &Path,
) -> Result<(), CliError> {
    // Locator only. Never open/copy the bearer into JSONL, bundles or handoff output.
    serde_json::to_writer(
        &mut *output,
        &serde_json::json!({
            "kind": "xray_browser_handoff", "schema_version": 1,
            "endpoint": endpoint, "credential_file": credential_file,
            "session_id": args.session_id.to_string(), "deep_capture": args.deep,
            "allow_origin": args.allow_origin, "origin_lifetime_seconds": args.origin_lifetime_seconds,
            "instruction": "Provision the bearer from the private credential file out of band; keep this CLI alive; exit revokes access. No durable replay."
        }),
    )?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
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
    // Intake never waits for filesystem IO, stdout, or payload export. Each
    // parsed frame is client-bounded and the configured queue bounds burst storage.
    let ready = stream
        .recv()
        .await
        .map_err(|error| transport_error(&error))?
        .ok_or_else(|| invalid("X-ray ready barrier unavailable"))?;
    let capabilities: bcode_xray_models::XrayCapabilities =
        serde_json::from_value(ready.event.data.clone())?;
    let range_bytes = u32::try_from(capabilities.max_payload_range_bytes.min(64 * 1024))
        .map_err(|_| invalid("invalid X-ray payload range capability"))?;
    let (sender, receiver) = tokio::sync::mpsc::channel(usize::from(args.consumer_queue_capacity));
    let worker_client = client.clone();
    let destination = args.record.clone();
    let observation_id = stream.observation_id().to_owned();
    let session_id = args.session_id.to_string();
    let runtime = tokio::runtime::Handle::current();
    let (consumer_finished, consumer_done) = tokio::sync::oneshot::channel();
    let consumer = tokio::task::spawn_blocking(move || {
        let result = consume_segment(
            receiver,
            &worker_client,
            destination.as_deref(),
            &observation_id,
            &session_id,
            range_bytes,
            &runtime,
        );
        let _ = consumer_finished.send(());
        result
    });
    let result = receive_endpoint(args, &client, &mut stream, &sender, consumer_done, ready).await;
    // Finish is sent only after an authenticated terminal frame. FIFO ordering makes
    // the consumer verify/store all preceding payloads before it flushes the bundle.
    let boundary = stream.finalization().cloned();
    let observation_id = stream.observation_id().to_owned();
    let terminal_received = boundary.is_some();
    if result.is_err() {
        // A local intake failure must cut off capture before dropping SSE: the
        // host retains queued payloads only after an authenticated engine cutoff.
        // A failed cutoff never repairs the original intake loss or certifies the
        // bundle; drain remains bounded and may report unavailable payloads.
        let _ = client.finalize(&observation_id).await;
        client.begin_payload_drain();
    }
    // Close SSE intake before waiting on a slow exporter. Keep the endpoint lease
    // and payload capability alive until every accepted message has drained.
    drop(stream);
    if result.is_ok()
        && let Some(boundary) = boundary
    {
        let _ = sender.send(ConsumerMessage::Finish(boundary)).await;
    }
    drop(sender);
    // Never detach the blocking task on timeout: it may still hold payload references
    // or bundle descriptors. Failed intake drains accepted work then marks interrupted.
    let export_result = consumer
        .await
        .map_err(|_| invalid("X-ray consumer failed; inspect the unfinished bundle manifest"))?;
    if result.is_ok()
        && export_result.is_ok()
        && terminal_received
        && client.release(&observation_id).await.is_err()
    {
        return Err(invalid(
            "X-ray retained-payload release is unconfirmed; session work is unaffected",
        ));
    }
    result?;
    export_result
}

async fn receive_endpoint(
    args: &XrayArgs,
    client: &XrayClient,
    stream: &mut bcode_client::xray::XrayStream,
    sender: &tokio::sync::mpsc::Sender<ConsumerMessage>,
    mut consumer_done: tokio::sync::oneshot::Receiver<()>,
    ready: bcode_xray_models::XrayEnvelope,
) -> Result<(), CliError> {
    enqueue(sender, ready)?;
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
            _ = &mut consumer_done => return Err(invalid(
                "X-ray consumer stopped; recording is unfinished; inspect disk/stdout and manifest.json")),
            result = &mut stop => { result?; break; }
            envelope = stream.recv() => {
                match envelope.map_err(|error| transport_error(&error))? {
                    Some(envelope) => enqueue(sender, envelope)?,
                    None => return Ok(()),
                }
            }
        }
    }
    let drain = async {
        client
            .finalize(stream.observation_id())
            .await
            .map_err(|error| transport_error(&error))?;
        while let Some(envelope) = stream
            .recv()
            .await
            .map_err(|error| transport_error(&error))?
        {
            enqueue(sender, envelope)?;
        }
        Ok::<(), CliError>(())
    };
    tokio::time::timeout(Duration::from_secs(args.finalize_timeout_seconds), drain)
        .await
        .map_err(|_| invalid("X-ray finalization timed out; bundle is unfinished"))?
}

#[cfg(test)]
const CONSUMER_QUEUE_CAPACITY: usize = 16;

#[derive(Debug)]
enum ConsumerMessage {
    Envelope(Box<bcode_xray_models::XrayEnvelope>),
    Finish(bcode_xray_models::XrayFinalization),
    Interrupt,
}

fn enqueue(
    sender: &tokio::sync::mpsc::Sender<ConsumerMessage>,
    envelope: bcode_xray_models::XrayEnvelope,
) -> Result<(), CliError> {
    sender.try_send(ConsumerMessage::Envelope(Box::new(envelope))).map_err(|error| match error {
        tokio::sync::mpsc::error::TrySendError::Full(_) => invalid(
            "X-ray consumer queue overflow: delivery lost; bundle is interrupted, not complete; start a new gapful capture segment"),
        tokio::sync::mpsc::error::TrySendError::Closed(_) => invalid(
            "X-ray consumer stopped; inspect the unfinished bundle and available disk/stdout"),
    })
}

#[derive(Debug)]
enum ConsumerFailure {
    Local(Box<CliError>),
    Payload(bcode_client::xray::XrayClientError),
}

impl ConsumerFailure {
    fn into_cli(self) -> CliError {
        match self {
            Self::Local(error) => *error,
            Self::Payload(error) => transport_error(&error),
        }
    }
}

impl From<CliError> for ConsumerFailure {
    fn from(error: CliError) -> Self {
        Self::Local(Box::new(error))
    }
}

fn consume_segment(
    receiver: tokio::sync::mpsc::Receiver<ConsumerMessage>,
    client: &XrayClient,
    destination: Option<&Path>,
    observation_id: &str,
    session_id: &str,
    range_bytes: u32,
    runtime: &tokio::runtime::Handle,
) -> Result<(), CliError> {
    consume_segment_after_gap(
        receiver,
        client,
        ConsumerRecording {
            destination,
            observation_id,
            session_id,
            previous: None,
        },
        range_bytes,
        runtime,
    )
    .map_err(ConsumerFailure::into_cli)
}

#[derive(Clone, Copy)]
struct ConsumerRecording<'a> {
    destination: Option<&'a Path>,
    observation_id: &'a str,
    session_id: &'a str,
    previous: Option<&'a str>,
}

fn consume_segment_after_gap(
    mut receiver: tokio::sync::mpsc::Receiver<ConsumerMessage>,
    client: &XrayClient,
    recording: ConsumerRecording<'_>,
    range_bytes: u32,
    runtime: &tokio::runtime::Handle,
) -> Result<(), ConsumerFailure> {
    let ConsumerRecording {
        destination,
        observation_id,
        session_id,
        previous,
    } = recording;
    let mut recorder = match (destination, previous) {
        (Some(path), Some(previous)) => Some(Box::new(
            JsonlBundleRecorder::create_segment(
                path,
                observation_id,
                session_id,
                previous,
                RecordingOptions::default(),
            )
            .map_err(|_| invalid("could not create fresh gapful recording segment"))?,
        ) as Box<dyn RecordingSink>),
        _ => open_recording(destination, observation_id, session_id)?,
    };
    let mut stdout = std::io::stdout();
    while let Some(message) = receiver.blocking_recv() {
        match message {
            ConsumerMessage::Envelope(envelope) => {
                if let Err(error) = runtime.block_on(record_envelope(
                    client,
                    &mut stdout,
                    recorder.as_mut(),
                    &envelope,
                    range_bytes,
                )) {
                    if let Some(recorder) = recorder.as_mut() {
                        let _ = recorder.interrupt();
                    }
                    return Err(error);
                }
            }
            ConsumerMessage::Finish(boundary) => {
                return finish_recording(recorder, Some(&boundary)).map_err(Into::into);
            }
            ConsumerMessage::Interrupt => {
                if let Some(recorder) = recorder.as_mut() {
                    recorder
                        .interrupt()
                        .map_err(|_| invalid("could not flush interrupted segment"))?;
                }
                return Ok(());
            }
        }
    }
    if let Some(recorder) = recorder.as_mut() {
        let _ = recorder.interrupt();
    }
    Err(invalid("X-ray segment interrupted: inspect manifest.json; retain the bundle and start a new segment, never append or repair automatically").into())
}

async fn record_envelope(
    client: &XrayClient,
    stdout: &mut (impl std::io::Write + Send),
    mut recorder: Option<&mut Box<dyn RecordingSink>>,
    envelope: &bcode_xray_models::XrayEnvelope,
    range_bytes: u32,
) -> Result<(), ConsumerFailure> {
    write_envelope(stdout, recorder.as_deref_mut(), envelope)?;
    if let Some(recorder) = recorder
        && let Err(error) = client
            .record_payloads(envelope, recorder.as_mut(), range_bytes)
            .await
    {
        // Persist an explicit interrupted boundary where possible; never finalize.
        let _ = recorder.interrupt();
        return Err(ConsumerFailure::Payload(error));
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
