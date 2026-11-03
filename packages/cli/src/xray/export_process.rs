//! CLI-private exporter process. Blocking output and recorder IO never run in the parent.

use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use bcode_client::xray::{XrayClient, XrayClientError};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::{mpsc, oneshot, watch};

use super::{ConsumerFailure, ConsumerMessage, ConsumerRecording, invalid};

const MARKER: &str = "--bcode-private-xray-export-v1";
const VERSION: u16 = 1;
const FRAME_LIMIT: usize = 2 * 1024 * 1024 + 64 * 1024;
const STATUS_LIMIT: usize = 1024;
const DRAIN: Duration = Duration::from_secs(30);
const REAP: Duration = Duration::from_secs(5);

// Deliberately no Debug: this transient initialization contains a bearer capability.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Init {
    version: u16,
    executable: ExecutableIdentity,
    artifact_digest: String,
    endpoint: String,
    bearer: String,
    destination: Option<PathBuf>,
    observation_id: String,
    session_id: String,
    previous: Option<String>,
    range_bytes: u32,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ExecutableIdentity {
    length: u64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    modified_seconds: i64,
    #[cfg(unix)]
    modified_nanos: i64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanos: i64,
}

impl ExecutableIdentity {
    fn current() -> Result<Self, ConsumerFailure> {
        let path = std::env::current_exe()
            .map_err(|_| local("X-ray exporter executable identity unavailable"))?;
        Self::at(&path)
    }

    fn at(path: &std::path::Path) -> Result<Self, ConsumerFailure> {
        let metadata = std::fs::metadata(path)
            .map_err(|_| local("X-ray exporter executable identity unavailable"))?;
        Ok(Self {
            length: metadata.len(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            modified_seconds: metadata.mtime(),
            #[cfg(unix)]
            modified_nanos: metadata.mtime_nsec(),
            #[cfg(unix)]
            changed_seconds: metadata.ctime(),
            #[cfg(unix)]
            changed_nanos: metadata.ctime_nsec(),
        })
    }
}

impl Init {
    pub(super) fn new(
        client: &XrayClient,
        destination: Option<PathBuf>,
        observation_id: String,
        session_id: String,
        previous: Option<String>,
        range_bytes: u32,
    ) -> Result<Self, ConsumerFailure> {
        let (endpoint, bearer) = client
            .export_process_transport()
            .map_err(ConsumerFailure::Payload)?;
        Ok(Self {
            version: VERSION,
            executable: ExecutableIdentity::current()?,
            artifact_digest: super::super::BUILD_INFO
                .get()
                .ok_or_else(|| local("X-ray parent loaded build identity unavailable"))?
                .digest()
                .to_owned(),
            endpoint,
            bearer,
            destination,
            observation_id,
            session_id,
            previous,
            range_bytes,
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Instruction {
    Envelope {
        envelope: Box<bcode_xray_models::XrayEnvelope>,
    },
    Finish {
        boundary: bcode_xray_models::XrayFinalization,
    },
    Interrupt,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Status {
    Hello {
        version: u16,
        executable: ExecutableIdentity,
        artifact_digest: String,
    },
    Ready {
        version: u16,
    },
    Finished,
    Interrupted,
    LocalFailure,
    PayloadFailure {
        category: PayloadFailure,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PayloadFailure {
    Transport,
    Incomplete,
    Http(u16),
    Rejected,
}

impl From<XrayClientError> for PayloadFailure {
    fn from(error: XrayClientError) -> Self {
        match error {
            XrayClientError::Transport => Self::Transport,
            XrayClientError::Incomplete => Self::Incomplete,
            XrayClientError::Http(status) => Self::Http(status),
            _ => Self::Rejected,
        }
    }
}

impl From<PayloadFailure> for XrayClientError {
    fn from(error: PayloadFailure) -> Self {
        match error {
            PayloadFailure::Transport => Self::Transport,
            PayloadFailure::Incomplete => Self::Incomplete,
            PayloadFailure::Http(status) => Self::Http(status),
            PayloadFailure::Rejected => Self::Protocol,
        }
    }
}

pub(super) struct ExportProcess {
    deadline: watch::Sender<Option<tokio::time::Instant>>,
    task: tokio::task::JoinHandle<Result<(), ConsumerFailure>>,
}

impl ExportProcess {
    /// Start once; repeated requests cannot renew the drain window.
    pub(super) fn begin_shutdown(&self) {
        self.deadline.send_if_modified(|deadline| {
            if deadline.is_some() {
                false
            } else {
                *deadline = Some(tokio::time::Instant::now() + DRAIN);
                true
            }
        });
    }

    pub(super) fn shutdown_deadline(&self) -> tokio::time::Instant {
        self.begin_shutdown();
        self.deadline
            .borrow()
            .unwrap_or_else(tokio::time::Instant::now)
    }

    pub(super) async fn join(self) -> Result<(), ConsumerFailure> {
        self.begin_shutdown();
        self.task.await.unwrap_or_else(|_| {
            Err(local(
                "X-ray exporter supervision failed; cleanup unconfirmed",
            ))
        })
    }
}

pub(super) async fn spawn(
    init: Init,
    capacity: usize,
) -> Result<
    (
        mpsc::Sender<ConsumerMessage>,
        oneshot::Receiver<()>,
        ExportProcess,
    ),
    ConsumerFailure,
> {
    let executable =
        std::env::current_exe().map_err(|_| local("X-ray exporter executable unavailable"))?;
    let mut command = tokio::process::Command::new(executable);
    command
        .arg(MARKER)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = command
        .spawn()
        .map_err(|_| local("X-ray exporter could not start"))?;
    let (sender, receiver) = mpsc::channel(capacity);
    let (ready_sender, ready) = oneshot::channel();
    let (done_sender, done) = oneshot::channel();
    let (deadline, deadline_receiver) = watch::channel(None);
    let task = tokio::spawn(async move {
        let result = supervise(child, init, receiver, deadline_receiver, ready_sender).await;
        let _ = done_sender.send(());
        result
    });
    let process = ExportProcess { deadline, task };
    if !matches!(ready.await, Ok(true)) {
        process.begin_shutdown();
        let result = process.join().await;
        return Err(result
            .err()
            .unwrap_or_else(|| local("X-ray exporter readiness unavailable")));
    }
    Ok((sender, done, process))
}

async fn supervise(
    mut child: tokio::process::Child,
    init: Init,
    receiver: mpsc::Receiver<ConsumerMessage>,
    mut deadline: watch::Receiver<Option<tokio::time::Instant>>,
    ready: oneshot::Sender<bool>,
) -> Result<(), ConsumerFailure> {
    let startup_phase = std::sync::atomic::AtomicU8::new(0);
    let operation = async {
        let mut input = child
            .stdin
            .take()
            .ok_or_else(|| local("X-ray exporter input unavailable"))?;
        let mut status = child
            .stderr
            .take()
            .ok_or_else(|| local("X-ray exporter status unavailable"))?;
        let startup = async {
            startup_phase.store(1, std::sync::atomic::Ordering::Relaxed);
            match read_frame::<_, Status>(&mut status, STATUS_LIMIT).await? {
                Status::Hello {
                    version: VERSION,
                    executable,
                    artifact_digest,
                } if executable == init.executable && artifact_digest == init.artifact_digest => {}
                _ => return Err(local("X-ray exporter executable handshake rejected")),
            }
            // Capabilities are transferred only after the child identifies itself.
            startup_phase.store(2, std::sync::atomic::Ordering::Relaxed);
            write_frame(&mut input, &init).await?;
            startup_phase.store(3, std::sync::atomic::Ordering::Relaxed);
            match read_frame::<_, Status>(&mut status, STATUS_LIMIT).await? {
                Status::Ready { version: VERSION } => Ok(()),
                _ => Err(local("X-ray exporter handshake rejected")),
            }
        };
        let startup = tokio::time::timeout(DRAIN, startup)
            .await
            .map_err(|_| local("X-ray exporter startup timed out"))?;
        let _ = ready.send(startup.is_ok());
        startup?;
        startup_phase.store(4, std::sync::atomic::Ordering::Relaxed);
        let (finished, status) = tokio::try_join!(pump(input, receiver), read_result(&mut status))?;
        if finished != status {
            return Err(local(
                "X-ray exporter completion disagrees with delivery; inspect bundle",
            ));
        }
        Ok(())
    };
    let result = tokio::select! {
        result = operation => result,
        () = shutdown_deadline(&mut deadline) => Err(local(match startup_phase.load(std::sync::atomic::Ordering::Relaxed) {
            0 | 1 => "X-ray exporter deadline before executable hello; owned child termination required",
            2 => "X-ray exporter deadline writing initialization; owned child termination required",
            3 => "X-ray exporter deadline waiting for recorder readiness; owned child termination required",
            _ => "X-ray exporter drain timed out; completion unconfirmed; inspect bundle",
        })),
    };
    if result.is_err() {
        // Only this directly owned, not-yet-reaped child is ever signalled.
        let _ = child.start_kill();
    }
    let exited = tokio::time::timeout(REAP, child.wait()).await;
    match exited {
        Ok(Ok(status)) if status.success() => result,
        Ok(Ok(_)) => result.and(Err(local(
            "X-ray exporter exited unsuccessfully; recording completion unconfirmed",
        ))),
        _ => {
            let _ = child.start_kill();
            if matches!(tokio::time::timeout(REAP, child.wait()).await, Ok(Ok(_))) {
                result.and(Err(local(
                    "X-ray exporter exit timed out; owned child terminated; completion unconfirmed",
                )))
            } else {
                let cleanup = local(
                    "X-ray exporter cleanup UNCONFIRMED: owned child reap unconfirmed; no recovery; recording completion unconfirmed",
                );
                match result {
                    Err(primary) => Err(local(&format!(
                        "{}; {}",
                        primary.into_cli(),
                        cleanup.into_cli()
                    ))),
                    Ok(()) => Err(cleanup),
                }
            }
        }
    }
}

async fn shutdown_deadline(deadline: &mut watch::Receiver<Option<tokio::time::Instant>>) {
    loop {
        let value = *deadline.borrow_and_update();
        if let Some(value) = value {
            tokio::time::sleep_until(value).await;
            return;
        }
        if deadline.changed().await.is_err() {
            return;
        }
    }
}

async fn pump(
    mut input: tokio::process::ChildStdin,
    mut receiver: mpsc::Receiver<ConsumerMessage>,
) -> Result<bool, ConsumerFailure> {
    while let Some(message) = receiver.recv().await {
        let (message, terminal) = match message {
            ConsumerMessage::Envelope(envelope) => (Instruction::Envelope { envelope }, None),
            ConsumerMessage::Finish(boundary) => (Instruction::Finish { boundary }, Some(true)),
            ConsumerMessage::Interrupt => (Instruction::Interrupt, Some(false)),
        };
        tokio::time::timeout(DRAIN, write_frame(&mut input, &message))
            .await
            .map_err(|_| local("X-ray exporter IPC stalled; accepted delivery incomplete"))??;
        if let Some(finished) = terminal {
            input
                .shutdown()
                .await
                .map_err(|_| local("X-ray exporter input close failed"))?;
            return Ok(finished);
        }
    }
    // EOF is an explicit interruption, never a synthetic finalization.
    write_frame(&mut input, &Instruction::Interrupt).await?;
    input
        .shutdown()
        .await
        .map_err(|_| local("X-ray exporter input close failed"))?;
    Ok(false)
}

async fn read_result(reader: &mut tokio::process::ChildStderr) -> Result<bool, ConsumerFailure> {
    match read_frame(reader, STATUS_LIMIT).await? {
        Status::Finished => Ok(true),
        Status::Interrupted => Ok(false),
        Status::PayloadFailure { category } => Err(ConsumerFailure::Payload(category.into())),
        _ => Err(local("X-ray exporter failed; inspect unfinished bundle")),
    }
}

fn local(message: &str) -> ConsumerFailure {
    invalid(message).into()
}

fn encode(value: &impl Serialize) -> Result<Vec<u8>, ConsumerFailure> {
    let bytes =
        serde_json::to_vec(value).map_err(|_| local("X-ray exporter protocol encoding failed"))?;
    if bytes.len() > FRAME_LIMIT {
        return Err(local("X-ray exporter protocol frame oversized"));
    }
    Ok(bytes)
}

async fn write_frame<W: tokio::io::AsyncWrite + Unpin + Send>(
    writer: &mut W,
    value: &(impl Serialize + Sync),
) -> Result<(), ConsumerFailure> {
    let bytes = encode(value)?;
    let length = u32::try_from(bytes.len()).map_err(|_| local("X-ray exporter frame oversized"))?;
    writer
        .write_u32(length)
        .await
        .map_err(|_| local("X-ray exporter IPC closed"))?;
    writer
        .write_all(&bytes)
        .await
        .map_err(|_| local("X-ray exporter IPC closed"))
}

async fn read_frame<R: tokio::io::AsyncRead + Unpin, T: serde::de::DeserializeOwned>(
    reader: &mut R,
    limit: usize,
) -> Result<T, ConsumerFailure> {
    let length = reader
        .read_u32()
        .await
        .map_err(|_| local("X-ray exporter status unavailable"))? as usize;
    if length == 0 || length > limit {
        return Err(local("X-ray exporter status oversized"));
    }
    let mut bytes = vec![0; length];
    reader
        .read_exact(&mut bytes)
        .await
        .map_err(|_| local("X-ray exporter status truncated"))?;
    serde_json::from_slice(&bytes).map_err(|_| local("X-ray exporter status invalid"))
}

fn read_instruction<T: serde::de::DeserializeOwned>(
    reader: &mut impl std::io::Read,
) -> Result<T, ConsumerFailure> {
    let mut length = [0; 4];
    reader
        .read_exact(&mut length)
        .map_err(|_| local("X-ray exporter instruction unavailable"))?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > FRAME_LIMIT {
        return Err(local("X-ray exporter instruction oversized"));
    }
    let mut bytes = vec![0; length];
    reader
        .read_exact(&mut bytes)
        .map_err(|_| local("X-ray exporter instruction truncated"))?;
    serde_json::from_slice(&bytes).map_err(|_| local("X-ray exporter instruction invalid"))
}

fn report(status: &Status) -> Result<(), ConsumerFailure> {
    let bytes = encode(status)?;
    let mut output = std::io::stderr().lock();
    let length =
        u32::try_from(bytes.len()).map_err(|_| local("X-ray exporter status oversized"))?;
    output
        .write_all(&length.to_be_bytes())
        .and_then(|()| output.write_all(&bytes))
        .and_then(|()| output.flush())
        .map_err(|_| local("X-ray exporter status closed"))
}

/// Early private exporter entry point.
///
/// Must run before runtime construction, configuration, plugins and daemon initialization.
/// An already-entered runtime fails closed.
/// Returns an exit code only for the exact private marker invocation.
#[must_use]
pub fn xray_export_process_entry(build_info: &bcode_build_info::BuildInfo) -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new(MARKER)) {
        return None;
    }
    if args.next().is_some() {
        return Some(2);
    }
    if tokio::runtime::Handle::try_current().is_ok() {
        // This dispatcher must run before the application's runtime is built.
        // Do not create a nested runtime, background thread, or depend on
        // Tokio's optional multi-thread-only blocking-region capability.
        let _ = report(&Status::LocalFailure);
        return Some(2);
    }
    Some(child_exit(build_info.digest()))
}

fn child_exit(artifact_digest: &str) -> i32 {
    let result = run_child(artifact_digest);
    let status = match result {
        Ok(finalized) => {
            if finalized {
                Status::Finished
            } else {
                Status::Interrupted
            }
        }
        Err(ConsumerFailure::Payload(error)) => Status::PayloadFailure {
            category: error.into(),
        },
        Err(ConsumerFailure::Local(_)) => Status::LocalFailure,
    };
    if report(&status).is_ok() { 0 } else { 2 }
}

fn run_child(artifact_digest: &str) -> Result<bool, ConsumerFailure> {
    let executable = ExecutableIdentity::current()?;
    report(&Status::Hello {
        version: VERSION,
        executable,
        artifact_digest: artifact_digest.to_owned(),
    })?;
    let init: Init = read_instruction(&mut std::io::stdin().lock())?;
    if init.version != VERSION
        || init.artifact_digest != artifact_digest
        || init.executable != ExecutableIdentity::current()?
        || init.observation_id.is_empty()
        || init.observation_id.len() > 128
        || init.session_id.is_empty()
        || init.session_id.len() > 128
        || init.range_bytes == 0
        || init.range_bytes > 64 * 1024
    {
        return Err(local("X-ray exporter initialization invalid"));
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| local("X-ray exporter runtime unavailable"))?;
    let _entered = runtime.enter();
    // Ctrl-C belongs to the observing parent. The child must finish or be killed
    // by that parent's shutdown policy, not die independently on group SIGINT.
    #[cfg(unix)]
    let _interrupts = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(|_| local("X-ray exporter signal registration failed"))?;
    let client = XrayClient::new(&init.endpoint, &init.bearer).map_err(ConsumerFailure::Payload)?;
    let recording = ConsumerRecording {
        destination: init.destination.as_deref(),
        observation_id: &init.observation_id,
        session_id: &init.session_id,
        previous: init.previous.as_deref(),
    };
    let mut recorder = super::open_segment_recording(recording)?;
    report(&Status::Ready { version: VERSION })?;
    let mut stdout = std::io::stdout();
    let mut terminal = None;
    loop {
        let instruction = read_instruction::<Instruction>(&mut std::io::stdin().lock());
        let result = match instruction {
            Ok(Instruction::Envelope { envelope }) => {
                if envelope.observation_id != init.observation_id
                    || terminal.is_some()
                    || envelope.event.correlation.session_id != init.session_id
                {
                    Err(local("X-ray exporter envelope identity mismatch"))
                } else {
                    if envelope.event.domain == "xray" && envelope.event.name == "finalized" {
                        let boundary: bcode_xray_models::XrayFinalization =
                            serde_json::from_value(envelope.event.data.clone())
                                .map_err(|_| local("X-ray exporter terminal invalid"))?;
                        if boundary.observation_id != init.observation_id {
                            return Err(local("X-ray exporter terminal identity mismatch"));
                        }
                        if boundary.final_sequence != envelope.sequence {
                            return Err(local("X-ray exporter terminal identity mismatch"));
                        }
                        terminal = Some(boundary);
                    }
                    runtime.block_on(super::record_envelope(
                        &client,
                        &mut stdout,
                        recorder.as_mut(),
                        &envelope,
                        init.range_bytes,
                    ))
                }
            }
            Ok(Instruction::Finish { boundary }) => {
                if terminal.as_ref() != Some(&boundary) {
                    return Err(local("X-ray exporter finish without matching terminal"));
                }
                super::finish_recording(recorder, Some(&boundary))?;
                return Ok(true);
            }
            Ok(Instruction::Interrupt) => {
                if let Some(recorder) = recorder.as_mut() {
                    recorder
                        .interrupt()
                        .map_err(|_| local("X-ray exporter interruption failed"))?;
                }
                return Ok(false);
            }
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            if let Some(recorder) = recorder.as_mut() {
                let _ = recorder.interrupt();
            }
            return Err(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_identity_disagreement_and_statuses_are_secret_free() {
        let mut changed = ExecutableIdentity::current().unwrap();
        changed.length = changed.length.saturating_add(1);
        assert!(changed != ExecutableIdentity::current().unwrap());
        let statuses = [
            Status::Hello {
                version: VERSION,
                executable: ExecutableIdentity::current().unwrap(),
                artifact_digest: "fixture-build".into(),
            },
            Status::Ready { version: VERSION },
            Status::Finished,
            Status::Interrupted,
            Status::LocalFailure,
            Status::PayloadFailure {
                category: PayloadFailure::Transport,
            },
            Status::PayloadFailure {
                category: PayloadFailure::Incomplete,
            },
            Status::PayloadFailure {
                category: PayloadFailure::Http(403),
            },
            Status::PayloadFailure {
                category: PayloadFailure::Rejected,
            },
        ];
        for status in statuses {
            let encoded = encode(&status).unwrap();
            assert!(encoded.len() <= STATUS_LIMIT);
            let value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
            let fields = value.as_object().unwrap();
            assert!(fields.keys().all(|key| matches!(
                key.as_str(),
                "kind" | "version" | "executable" | "category" | "artifact_digest"
            )));
            if let Some(identity) = fields.get("executable") {
                assert!(
                    identity
                        .as_object()
                        .unwrap()
                        .values()
                        .all(serde_json::Value::is_number)
                );
            }
        }
        let status = Status::PayloadFailure {
            category: XrayClientError::InvalidCredential.into(),
        };
        let encoded = String::from_utf8(encode(&status).unwrap()).unwrap();
        assert!(encoded.contains("rejected"));
    }

    #[tokio::test]
    async fn exporter_shutdown_deadline_is_not_renewed() {
        let (sender, process) = supervised_fixture(2).await;
        process.begin_shutdown();
        let original = process.shutdown_deadline();
        process.begin_shutdown();
        assert_eq!(process.shutdown_deadline(), original);
        process
            .deadline
            .send(Some(tokio::time::Instant::now()))
            .unwrap();
        assert!(matches!(
            process.join().await,
            Err(ConsumerFailure::Local(_))
        ));
        drop(sender);
    }

    #[test]
    fn framed_protocol_rejects_oversized_truncated_and_future_shapes() {
        let mut oversized = std::io::Cursor::new(u32::MAX.to_be_bytes());
        assert!(read_instruction::<Instruction>(&mut oversized).is_err());
        let mut truncated = std::io::Cursor::new(vec![0, 0, 0, 4, b'{']);
        assert!(read_instruction::<Instruction>(&mut truncated).is_err());
        assert!(serde_json::from_str::<Status>("{\"kind\":\"future\"}").is_err());
        let status = encode(&Status::PayloadFailure {
            category: PayloadFailure::Rejected,
        })
        .unwrap();
        assert!(status.len() < STATUS_LIMIT);
    }

    // A real directly owned subprocess, not a thread masquerading as blocked IO.
    // The test-only selector carries no endpoint, token or other capability.
    #[test]
    #[ignore = "private subprocess fixture, invoked by supervisor behavior tests"]
    fn exporter_fixture() {
        if std::env::var_os("BCODE_XRAY_EXPORT_TEST_FIXTURE").is_none() {
            return;
        }
        report(&Status::Hello {
            version: VERSION,
            executable: ExecutableIdentity::current().unwrap(),
            artifact_digest: "fixture-build".into(),
        })
        .unwrap();
        let init: Init = read_instruction(&mut std::io::stdin().lock()).unwrap();
        report(&Status::Ready { version: VERSION }).unwrap();
        match init.version {
            2 => loop {
                std::thread::park_timeout(Duration::from_secs(60));
            },
            3 => std::process::exit(17),
            4 => {
                let bytes = vec![b'x'; 64 * 1024].into_boxed_slice();
                loop {
                    std::io::stdout().write_all(&bytes).unwrap();
                }
            }
            _ => {
                loop {
                    let instruction = read_instruction::<Instruction>(&mut std::io::stdin().lock());
                    match instruction.unwrap() {
                        Instruction::Finish { .. } => break,
                        Instruction::Interrupt => {
                            report(&Status::Interrupted).unwrap();
                            return;
                        }
                        Instruction::Envelope { .. } => {}
                    }
                }
                report(&Status::Finished).unwrap();
            }
        }
    }

    #[test]
    #[ignore = "private production exporter subprocess entry"]
    fn production_exporter_fixture() {
        if std::env::var_os("BCODE_XRAY_EXPORT_TEST_FIXTURE").is_some() {
            std::process::exit(child_exit("fixture-build"));
        }
    }

    async fn supervised_fixture(mode: u16) -> (mpsc::Sender<ConsumerMessage>, ExportProcess) {
        supervised_recording_fixture(mode, None, None).await
    }

    async fn supervised_recording_fixture(
        mode: u16,
        destination: Option<PathBuf>,
        executable: Option<PathBuf>,
    ) -> (mpsc::Sender<ConsumerMessage>, ExportProcess) {
        let identity_path = executable
            .clone()
            .unwrap_or_else(|| std::env::current_exe().unwrap());
        let mut command = tokio::process::Command::new(&identity_path);
        if executable.is_some() {
            command.arg(MARKER);
        } else {
            command.args([
                "--exact",
                if mode == 5 {
                    "xray::export_process::tests::production_exporter_fixture"
                } else {
                    "xray::export_process::tests::exporter_fixture"
                },
                "--ignored",
                "--nocapture",
            ]);
        }
        command
            .env_clear()
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        if executable.is_none() {
            command.env("BCODE_XRAY_EXPORT_TEST_FIXTURE", "1");
        }
        let child = command.spawn().unwrap();
        let mut init = Init {
            version: if mode == 5 { VERSION } else { mode },
            executable: ExecutableIdentity::at(&identity_path).unwrap(),
            artifact_digest: if executable.is_some() {
                std::env::var("BCODE_XRAY_TEST_BUILD_DIGEST")
                    .expect("explicit loaded application build digest required")
            } else {
                "fixture-build".into()
            },
            endpoint: "http://127.0.0.1:1/".into(),
            bearer: "test-only".into(),
            destination,
            observation_id: "observer".into(),
            session_id: "session".into(),
            previous: None,
            range_bytes: 16 * 1024,
        };
        if mode == 6 {
            init.executable.length = init.executable.length.saturating_add(1);
        }
        if mode == 7 {
            init.artifact_digest = "different-loaded-build".into();
        }
        let (sender, receiver) = mpsc::channel(1);
        let (ready_sender, ready) = oneshot::channel();
        let (deadline, deadline_receiver) = watch::channel(None);
        let task = tokio::spawn(supervise(
            child,
            init,
            receiver,
            deadline_receiver,
            ready_sender,
        ));
        let readiness = tokio::time::timeout(Duration::from_secs(5), ready).await;
        let process = ExportProcess { deadline, task };
        let admitted = if let Ok(result) = readiness {
            result.unwrap_or(false)
        } else {
            process
                .deadline
                .send(Some(tokio::time::Instant::now()))
                .unwrap();
            let outcome = process.join().await;
            panic!("exporter readiness deadline: supervised cleanup outcome: {outcome:?}");
        };
        assert_eq!(admitted, !matches!(mode, 6 | 7));
        (sender, process)
    }

    #[tokio::test]
    async fn identity_mismatch_is_rejected_before_capability_transfer() {
        for mode in [6, 7] {
            let (sender, process) = supervised_fixture(mode).await;
            assert!(matches!(
                process.join().await,
                Err(ConsumerFailure::Local(_))
            ));
            drop(sender);
        }
    }

    async fn production_roundtrip(executable: Option<PathBuf>) {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary
            .path()
            .canonicalize()
            .unwrap()
            .join("production-export");
        let (sender, process) =
            supervised_recording_fixture(5, Some(destination.clone()), executable).await;
        let ready = super::super::tests::pipeline_envelope(1, "ready", serde_json::json!({}));
        let boundary = bcode_xray_models::XrayFinalization {
            observation_id: "observer".into(),
            final_sequence: 2,
            dropped_events: 0,
            hub_contention_drops: 0,
            evicted_payloads: 0,
        };
        let terminal = super::super::tests::pipeline_envelope(
            2,
            "finalized",
            serde_json::to_value(&boundary).unwrap(),
        );
        sender
            .send(ConsumerMessage::Envelope(Box::new(ready)))
            .await
            .unwrap();
        sender
            .send(ConsumerMessage::Envelope(Box::new(terminal)))
            .await
            .unwrap();
        sender
            .send(ConsumerMessage::Finish(boundary.clone()))
            .await
            .unwrap();
        drop(sender);
        assert!(process.join().await.is_ok());
        let manifest = bcode_xray::recording::inspect_bundle(
            &destination,
            &bcode_xray::recording::RecordingOptions::default(),
        )
        .unwrap();
        assert_eq!(
            manifest.state,
            bcode_xray::recording::RecordingState::Finalized
        );
        assert_eq!(manifest.finalization, Some(boundary));
        assert_eq!(manifest.events.envelope_count, 2);
        assert!(manifest.evidence_complete);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn production_exporter_child_records_finalizes_and_is_reaped() {
        production_roundtrip(None).await;
    }

    /// Coordinator supplies an isolated, explicitly identified application artifact.
    /// No PATH discovery or fallback to the active runtime is permitted.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "requires BCODE_XRAY_TEST_EXECUTABLE and BCODE_XRAY_TEST_BUILD_DIGEST for isolated artifact"]
    async fn production_binary_early_hook_records_and_reaps() {
        let executable = PathBuf::from(
            std::env::var_os("BCODE_XRAY_TEST_EXECUTABLE")
                .expect("explicit test artifact required"),
        );
        assert!(executable.is_absolute());
        production_roundtrip(Some(executable)).await;
    }

    #[tokio::test]
    async fn supervised_ipc_saturation_is_killed_and_reaped_without_recovery() {
        let (sender, process) = supervised_fixture(2).await;
        let mut envelope =
            super::super::tests::pipeline_envelope(1, "event", serde_json::Value::Null);
        envelope.event.data = serde_json::json!({"large": "x".repeat(1024 * 1024)});
        sender
            .send(ConsumerMessage::Envelope(Box::new(envelope.clone())))
            .await
            .unwrap();
        // Large in-flight frame plus one queue slot: no unbounded writer staging.
        let permit = tokio::time::timeout(Duration::from_secs(5), sender.reserve())
            .await
            .unwrap()
            .unwrap();
        permit.send(ConsumerMessage::Envelope(Box::new(envelope.clone())));
        assert!(
            sender
                .try_send(ConsumerMessage::Envelope(Box::new(envelope)))
                .is_err()
        );
        process
            .deadline
            .send(Some(
                tokio::time::Instant::now() + Duration::from_millis(50),
            ))
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(6), process.join())
            .await
            .unwrap();
        assert!(matches!(result, Err(ConsumerFailure::Local(_))));
        drop(sender);
    }

    #[tokio::test]
    async fn supervised_blocked_stdout_is_killed_and_reaped() {
        let (sender, process) = supervised_fixture(4).await;
        process
            .deadline
            .send(Some(
                tokio::time::Instant::now() + Duration::from_millis(50),
            ))
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(6), process.join())
            .await
            .unwrap();
        assert!(matches!(result, Err(ConsumerFailure::Local(_))));
        drop(sender);
    }

    #[tokio::test]
    async fn cancelling_owner_retains_supervision_until_owned_child_is_reaped() {
        let (sender, process) = supervised_fixture(2).await;
        let ExportProcess { deadline, task } = process;
        // Model cancellation of the owner: no detached exporter IO thread and
        // no live deadline sender. The supervisor still owns and reaps the child.
        drop(deadline);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(6), task)
                .await
                .unwrap()
                .unwrap(),
            Err(ConsumerFailure::Local(_))
        ));
        drop(sender);
    }

    #[tokio::test]
    async fn supervised_crash_is_failure_and_normal_interrupt_is_joined() {
        let (sender, process) = supervised_fixture(3).await;
        assert!(process.join().await.is_err());
        drop(sender);
        let (sender, process) = supervised_fixture(1).await;
        sender.send(ConsumerMessage::Interrupt).await.unwrap();
        assert!(process.join().await.is_ok());
    }
}
