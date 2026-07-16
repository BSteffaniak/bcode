//! Versioned, shell-owned byte-exact terminal recording format.
//!
//! The format is append-friendly while a command runs and atomically published on completion.
//! Readers reject incomplete files, validate the complete frame stream, and skip unknown frame
//! kinds by their declared payload length for forward compatibility.

use sha2::{Digest as _, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;

/// One readable committed boundary of an active shell recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellRecordingCommit {
    /// Host-local recording path that contains the committed prefix.
    pub path: PathBuf,
    /// Complete readable bytes at this revision.
    pub committed_bytes: u64,
    /// Whether the recording has been atomically finalized.
    pub finalized: bool,
}

/// Observer invoked from the recording worker after complete frames become readable.
pub type ShellRecordingCommitObserver = Arc<dyn Fn(ShellRecordingCommit) + Send + Sync>;

const MAGIC: &[u8; 8] = b"BCSHREC\0";
const FORMAT_VERSION: u16 = 3;
const SIGNAL_FORMAT_VERSION: u16 = 2;
const LEGACY_FORMAT_VERSION: u16 = 1;
const FRAME_OUTPUT: u8 = 1;
const FRAME_RESIZE: u8 = 2;
const FRAME_FINISH: u8 = 3;
const FRAME_START: u8 = 4;
const FRAME_REPLAY_OUTPUT: u8 = 5;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const ASYNC_RECORDING_QUEUE_CAPACITY: usize = 256;
const RECORDING_HEADER_BYTES: usize = 14;
const RECORDING_FRAME_HEADER_BYTES: usize = 13;

/// Incremental decoder for ordered committed prefixes of an active version-three recording.
///
/// The decoder retains only an incomplete header/frame tail. Complete frames are returned once,
/// preserving exact payload bytes and frame order across arbitrary range boundaries.
#[derive(Debug, Default)]
pub struct IncrementalShellRecordingDecoder {
    buffer: Vec<u8>,
    stream_offset: u64,
    columns: Option<u16>,
    rows: Option<u16>,
    previous_frame_offset: Option<u64>,
    saw_start: bool,
    saw_finish: bool,
}

impl IncrementalShellRecordingDecoder {
    /// Append the next contiguous recording range and decode all newly complete frames.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-contiguous range, invalid header, malformed or oversized frame,
    /// non-monotonic frame time, invalid lifecycle ordering, or replay checksum mismatch.
    pub fn push(&mut self, offset: u64, bytes: &[u8]) -> io::Result<Vec<ShellRecordingFrame>> {
        let buffered = u64::try_from(self.buffer.len()).unwrap_or(u64::MAX);
        if offset != self.stream_offset.saturating_add(buffered) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "recording range is not contiguous",
            ));
        }
        self.buffer.extend_from_slice(bytes);
        let mut consumed = if self.columns.is_none() {
            if self.buffer.len() < RECORDING_HEADER_BYTES {
                return Ok(Vec::new());
            }
            if &self.buffer[..8] != MAGIC {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid recording magic",
                ));
            }
            let version = u16::from_le_bytes([self.buffer[8], self.buffer[9]]);
            if version != FORMAT_VERSION {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "active recording version is unsupported",
                ));
            }
            self.columns = Some(u16::from_le_bytes([self.buffer[10], self.buffer[11]]));
            self.rows = Some(u16::from_le_bytes([self.buffer[12], self.buffer[13]]));
            RECORDING_HEADER_BYTES
        } else {
            0
        };
        let mut frames = Vec::new();
        loop {
            let remaining = &self.buffer[consumed..];
            if remaining.len() < RECORDING_FRAME_HEADER_BYTES {
                break;
            }
            let kind = remaining[0];
            let frame_offset = u64::from_le_bytes(
                remaining[1..9]
                    .try_into()
                    .map_err(|_| io::Error::other("recording frame offset slice"))?,
            );
            let payload_len = usize::try_from(u32::from_le_bytes(
                remaining[9..13]
                    .try_into()
                    .map_err(|_| io::Error::other("recording frame length slice"))?,
            ))
            .unwrap_or(usize::MAX);
            if payload_len > MAX_FRAME_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "recording frame exceeds limit",
                ));
            }
            let frame_len = RECORDING_FRAME_HEADER_BYTES.saturating_add(payload_len);
            if remaining.len() < frame_len {
                break;
            }
            if self.saw_finish {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "recording contains frames after finish",
                ));
            }
            if self
                .previous_frame_offset
                .is_some_and(|previous| frame_offset < previous)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "recording frame offsets are not monotonic",
                ));
            }
            let payload = remaining[RECORDING_FRAME_HEADER_BYTES..frame_len].to_vec();
            let frame = self.decode_frame(kind, frame_offset, &payload)?;
            self.previous_frame_offset = Some(frame_offset);
            frames.push(frame);
            consumed = consumed.saturating_add(frame_len);
        }
        if consumed > 0 {
            self.buffer.drain(..consumed);
            self.stream_offset = self
                .stream_offset
                .saturating_add(u64::try_from(consumed).unwrap_or(u64::MAX));
        }
        Ok(frames)
    }

    /// Return initial recording dimensions once the header has arrived.
    #[must_use]
    pub const fn dimensions(&self) -> Option<(u16, u16)> {
        match (self.columns, self.rows) {
            (Some(columns), Some(rows)) => Some((columns, rows)),
            _ => None,
        }
    }

    fn decode_frame(
        &mut self,
        kind: u8,
        offset_micros: u64,
        payload: &[u8],
    ) -> io::Result<ShellRecordingFrame> {
        let frame = match kind {
            FRAME_START if !self.saw_start && payload.is_empty() => {
                self.saw_start = true;
                ShellRecordingFrame::Start { offset_micros }
            }
            FRAME_OUTPUT if self.saw_start => ShellRecordingFrame::Output {
                offset_micros,
                bytes: payload.to_vec(),
            },
            FRAME_REPLAY_OUTPUT if self.saw_start && payload.len() >= 32 => {
                let (expected, bytes) = payload.split_at(32);
                if Sha256::digest(bytes).as_slice() != expected {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "recording replay-output checksum mismatch",
                    ));
                }
                ShellRecordingFrame::ReplayOutput {
                    offset_micros,
                    bytes: bytes.to_vec(),
                }
            }
            FRAME_RESIZE if self.saw_start && payload.len() == 4 => ShellRecordingFrame::Resize {
                offset_micros,
                columns: u16::from_le_bytes([payload[0], payload[1]]),
                rows: u16::from_le_bytes([payload[2], payload[3]]),
            },
            FRAME_FINISH if self.saw_start && payload.len() >= 40 => {
                let signal_length = usize::from(u16::from_le_bytes([payload[6], payload[7]]));
                let checksum_start = 8_usize.saturating_add(signal_length);
                if payload.len() != checksum_start.saturating_add(32) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid recording finish frame",
                    ));
                }
                let signal = std::str::from_utf8(&payload[8..checksum_start]).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid signal name")
                })?;
                self.saw_finish = true;
                ShellRecordingFrame::Finish {
                    offset_micros,
                    exit_code: (payload[0] != 0).then(|| {
                        i32::from_le_bytes([payload[1], payload[2], payload[3], payload[4]])
                    }),
                    signal: (!signal.is_empty()).then(|| signal.to_owned()),
                    timed_out: payload[5] & 1 != 0,
                    cancelled: payload[5] & 2 != 0,
                }
            }
            kind if self.saw_start => ShellRecordingFrame::Unknown {
                kind,
                offset_micros,
                payload: payload.to_vec(),
            },
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid active recording lifecycle or frame",
                ));
            }
        };
        Ok(frame)
    }
}

enum AsyncRecordingCommand {
    Output {
        offset_micros: u64,
        bytes: Vec<u8>,
        replay_bytes: Option<Vec<u8>>,
    },
    Resize {
        offset_micros: u64,
        columns: u16,
        rows: u16,
    },
    Finish {
        offset_micros: u64,
        exit_code: Option<i32>,
        signal: Option<String>,
        timed_out: bool,
        cancelled: bool,
        response: mpsc::Sender<io::Result<ShellRecordingSummary>>,
    },
}

/// Cloneable non-blocking resize producer for an active recording.
#[derive(Clone)]
pub struct AsyncShellRecordingResizeSender {
    sender: mpsc::SyncSender<AsyncRecordingCommand>,
    sequence: Arc<Mutex<()>>,
    failed: Arc<AtomicBool>,
}

impl AsyncShellRecordingResizeSender {
    /// Apply and record a resize as one ordered operation relative to output frames.
    ///
    /// # Errors
    ///
    /// Returns an error when the resize operation fails, ordering state is poisoned, or the
    /// bounded recording queue cannot accept the frame. Any error permanently prevents
    /// publication of the authoritative recording.
    pub fn write_resize_with(
        &self,
        offset_micros: u64,
        columns: u16,
        rows: u16,
        resize: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        let _sequence = self.sequence.lock().map_err(|_| {
            self.failed.store(true, Ordering::SeqCst);
            io::Error::other("shell recording sequence lock poisoned")
        })?;
        if let Err(error) = resize() {
            self.failed.store(true, Ordering::SeqCst);
            return Err(error);
        }
        self.sender
            .try_send(AsyncRecordingCommand::Resize {
                offset_micros,
                columns,
                rows,
            })
            .map_err(|_| {
                self.failed.store(true, Ordering::SeqCst);
                io::Error::other("shell recording queue overflowed or disconnected")
            })
    }
}

/// Non-blocking producer for a shell recording writer thread.
pub struct AsyncShellRecordingWriter {
    sender: mpsc::SyncSender<AsyncRecordingCommand>,
    worker: Option<thread::JoinHandle<()>>,
    sequence: Arc<Mutex<()>>,
    failed: Arc<AtomicBool>,
}

impl AsyncShellRecordingWriter {
    /// Start a bounded recording writer thread.
    ///
    /// # Errors
    ///
    /// Returns an error if the partial recording or writer thread cannot be created.
    pub fn create(path: &Path, columns: u16, rows: u16) -> io::Result<Self> {
        Self::create_with_observer(path, columns, rows, None)
    }

    /// Start a bounded recording writer thread with committed-boundary notifications.
    ///
    /// # Errors
    ///
    /// Returns an error if the partial recording or writer thread cannot be created.
    pub fn create_with_observer(
        path: &Path,
        columns: u16,
        rows: u16,
        observer: Option<ShellRecordingCommitObserver>,
    ) -> io::Result<Self> {
        let mut writer = ShellRecordingWriter::create(path, columns, rows)?;
        writer.publish_commit(observer.as_ref(), false)?;
        let (sender, receiver) = mpsc::sync_channel(ASYNC_RECORDING_QUEUE_CAPACITY);
        let worker = thread::Builder::new()
            .name("bcode-shell-recording".to_owned())
            .spawn(move || run_async_recording_writer(writer, &receiver, observer.as_ref()))?;
        Ok(Self {
            sender,
            worker: Some(worker),
            sequence: Arc::new(Mutex::new(())),
            failed: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Return a cloneable non-blocking resize producer.
    #[must_use]
    pub fn resize_sender(&self) -> AsyncShellRecordingResizeSender {
        AsyncShellRecordingResizeSender {
            sender: self.sender.clone(),
            sequence: Arc::clone(&self.sequence),
            failed: Arc::clone(&self.failed),
        }
    }

    /// Queue exact PTY bytes with bounded backpressure.
    ///
    /// This is intended for the dedicated PTY reader thread: it may wait for recording queue
    /// capacity, preserving every ordered byte while keeping filesystem I/O on the writer thread.
    ///
    /// # Errors
    ///
    /// Returns an error when ordering state is poisoned or the writer queue is disconnected.
    pub fn write_output_with(
        &mut self,
        offset_micros: u64,
        bytes: &[u8],
        replay_bytes: Option<&[u8]>,
        queued: impl FnOnce(),
    ) -> io::Result<()> {
        if self.failed.load(Ordering::SeqCst) {
            return Err(io::Error::other("shell recording writer previously failed"));
        }
        let _sequence = self.sequence.lock().map_err(|_| {
            self.failed.store(true, Ordering::SeqCst);
            io::Error::other("shell recording sequence lock poisoned")
        })?;
        self.sender
            .send(AsyncRecordingCommand::Output {
                offset_micros,
                bytes: bytes.to_vec(),
                replay_bytes: replay_bytes.map(<[u8]>::to_vec),
            })
            .map_err(|_| {
                self.failed.store(true, Ordering::SeqCst);
                io::Error::other("shell recording queue disconnected")
            })?;
        queued();
        Ok(())
    }

    /// Queue exact PTY bytes without waiting for filesystem I/O.
    ///
    /// Returns `false` if ordering is contended or the bounded writer queue cannot accept the
    /// frame. This method never waits for recording ordering or filesystem I/O. Once false is
    /// returned, finalization fails explicitly and no authoritative recording is published.
    pub fn try_write_output_with(
        &mut self,
        offset_micros: u64,
        bytes: &[u8],
        replay_bytes: Option<&[u8]>,
        queued: impl FnOnce(),
    ) -> bool {
        if self.failed.load(Ordering::SeqCst) {
            return false;
        }
        let bytes = bytes.to_vec();
        let replay_bytes = replay_bytes.map(<[u8]>::to_vec);
        let Ok(_sequence) = self.sequence.try_lock() else {
            self.failed.store(true, Ordering::SeqCst);
            return false;
        };
        if self
            .sender
            .try_send(AsyncRecordingCommand::Output {
                offset_micros,
                bytes,
                replay_bytes,
            })
            .is_err()
        {
            self.failed.store(true, Ordering::SeqCst);
            return false;
        }
        queued();
        true
    }

    /// Queue exact PTY bytes without waiting for filesystem I/O.
    pub fn try_write_output(&mut self, offset_micros: u64, bytes: &[u8]) -> bool {
        self.try_write_output_with(offset_micros, bytes, None, || {})
    }

    /// Queue a resize without waiting for filesystem I/O.
    pub fn try_write_resize(&mut self, offset_micros: u64, columns: u16, rows: u16) -> bool {
        self.resize_sender()
            .write_resize_with(offset_micros, columns, rows, || Ok(()))
            .is_ok()
    }

    /// Drain queued frames, finalize atomically, and join the writer thread.
    ///
    /// # Errors
    ///
    /// Returns an error after queue overflow/disconnection, writer I/O failure, or worker panic.
    pub fn finish(
        mut self,
        offset_micros: u64,
        exit_code: Option<i32>,
        signal: Option<String>,
        timed_out: bool,
        cancelled: bool,
    ) -> io::Result<ShellRecordingSummary> {
        if self.failed.load(Ordering::SeqCst) {
            return Err(io::Error::other(
                "shell recording queue overflowed, disconnected, or lost an ordered frame",
            ));
        }
        let _sequence = self
            .sequence
            .lock()
            .map_err(|_| io::Error::other("shell recording sequence lock poisoned"))?;
        if self.failed.load(Ordering::SeqCst) {
            return Err(io::Error::other(
                "shell recording queue overflowed, disconnected, or lost an ordered frame",
            ));
        }
        let (response, result) = mpsc::channel();
        self.sender
            .send(AsyncRecordingCommand::Finish {
                offset_micros,
                exit_code,
                signal,
                timed_out,
                cancelled,
                response,
            })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "recording writer stopped"))?;
        let result = result
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "recording writer stopped"))?;
        if self
            .worker
            .take()
            .is_some_and(|worker| worker.join().is_err())
        {
            return Err(io::Error::other("recording writer panicked"));
        }
        result
    }
}

impl Drop for AsyncShellRecordingWriter {
    fn drop(&mut self) {
        // Dropping the sender disconnects the worker. It then drops the synchronous writer and
        // leaves only the explicit partial file.
    }
}

fn run_async_recording_writer(
    mut writer: ShellRecordingWriter,
    receiver: &mpsc::Receiver<AsyncRecordingCommand>,
    observer: Option<&ShellRecordingCommitObserver>,
) {
    let mut failure = None;
    while let Ok(command) = receiver.recv() {
        match command {
            AsyncRecordingCommand::Output {
                offset_micros,
                bytes,
                replay_bytes,
            } => {
                if failure.is_none() {
                    let write_result = (|| {
                        writer.write_output(offset_micros, &bytes)?;
                        if let Some(replay_bytes) = replay_bytes
                            && !replay_bytes.is_empty()
                        {
                            writer.write_replay_output(offset_micros, &replay_bytes)?;
                        }
                        writer.publish_commit(observer, false)
                    })();
                    if let Err(error) = write_result {
                        failure = Some(error);
                    }
                }
            }
            AsyncRecordingCommand::Resize {
                offset_micros,
                columns,
                rows,
            } => {
                if failure.is_none() {
                    let write_result = writer
                        .write_resize(offset_micros, columns, rows)
                        .and_then(|()| writer.publish_commit(observer, false));
                    if let Err(error) = write_result {
                        failure = Some(error);
                    }
                }
            }
            AsyncRecordingCommand::Finish {
                offset_micros,
                exit_code,
                signal,
                timed_out,
                cancelled,
                response,
            } => {
                let result = failure.map_or_else(
                    || {
                        let final_path = writer.final_path.clone();
                        let summary = writer.finish(
                            offset_micros,
                            exit_code,
                            signal.as_deref(),
                            timed_out,
                            cancelled,
                        )?;
                        if let Some(observer) = observer {
                            observer(ShellRecordingCommit {
                                committed_bytes: fs::metadata(&final_path)?.len(),
                                path: final_path,
                                finalized: true,
                            });
                        }
                        Ok(summary)
                    },
                    Err,
                );
                let _ = response.send(result);
                break;
            }
        }
    }
}

/// One decoded shell recording frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellRecordingFrame {
    /// Invocation recording began. Version 3 and later require this as the first frame.
    Start { offset_micros: u64 },
    /// Exact PTY bytes emitted at the given monotonic offset.
    Output { offset_micros: u64, bytes: Vec<u8> },
    /// Bytes from the presentation stream consumed by live rendering.
    ReplayOutput { offset_micros: u64, bytes: Vec<u8> },
    /// Terminal dimensions changed at the given monotonic offset.
    Resize {
        offset_micros: u64,
        columns: u16,
        rows: u16,
    },
    /// Invocation reached a terminal lifecycle state.
    Finish {
        offset_micros: u64,
        exit_code: Option<i32>,
        signal: Option<String>,
        timed_out: bool,
        cancelled: bool,
    },
    /// A future frame kind not interpreted by this reader.
    Unknown {
        kind: u8,
        offset_micros: u64,
        payload: Vec<u8>,
    },
}

/// Validated recording metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellRecordingSummary {
    /// Initial terminal columns.
    pub columns: u16,
    /// Initial terminal rows.
    pub rows: u16,
    /// Number of encoded frames.
    pub frame_count: u64,
    /// Total exact PTY output bytes.
    pub output_bytes: u64,
    /// SHA-256 digest of concatenated output-frame bytes.
    pub checksum_sha256: String,
}

/// Atomically finalized shell recording writer.
pub struct ShellRecordingWriter {
    final_path: PathBuf,
    partial_path: PathBuf,
    writer: BufWriter<File>,
    columns: u16,
    rows: u16,
    frame_count: u64,
    output_bytes: u64,
    checksum: Sha256,
    finished: bool,
}

impl ShellRecordingWriter {
    /// Create an incomplete recording beside its final path.
    ///
    /// # Errors
    ///
    /// Returns an error if the parent directory or partial recording cannot be created or written.
    pub fn create(path: &Path, columns: u16, rows: u16) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let partial_path = path.with_extension("shell-recording.partial");
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&partial_path)?;
        let mut writer = BufWriter::new(file);
        writer.write_all(MAGIC)?;
        writer.write_all(&FORMAT_VERSION.to_le_bytes())?;
        writer.write_all(&columns.to_le_bytes())?;
        writer.write_all(&rows.to_le_bytes())?;
        let mut recording = Self {
            final_path: path.to_path_buf(),
            partial_path,
            writer,
            columns,
            rows,
            frame_count: 0,
            output_bytes: 0,
            checksum: Sha256::new(),
            finished: false,
        };
        recording.write_frame(FRAME_START, 0, &[])?;
        recording.writer.flush()?;
        recording.writer.get_ref().sync_data()?;
        Ok(recording)
    }

    /// Append exact PTY bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if the frame cannot be encoded or written.
    pub fn write_output(&mut self, offset_micros: u64, bytes: &[u8]) -> io::Result<()> {
        self.write_frame(FRAME_OUTPUT, offset_micros, bytes)?;
        self.output_bytes = self
            .output_bytes
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        self.checksum.update(bytes);
        Ok(())
    }

    /// Append bytes from the presentation stream consumed by live rendering.
    ///
    /// # Errors
    ///
    /// Returns an error if the frame cannot be encoded or written.
    pub fn write_replay_output(&mut self, offset_micros: u64, bytes: &[u8]) -> io::Result<()> {
        let mut payload = Vec::with_capacity(32_usize.saturating_add(bytes.len()));
        payload.extend_from_slice(&Sha256::digest(bytes));
        payload.extend_from_slice(bytes);
        self.write_frame(FRAME_REPLAY_OUTPUT, offset_micros, &payload)
    }

    /// Append a resize frame.
    ///
    /// # Errors
    ///
    /// Returns an error if the frame cannot be encoded or written.
    pub fn write_resize(&mut self, offset_micros: u64, columns: u16, rows: u16) -> io::Result<()> {
        let mut payload = [0_u8; 4];
        payload[..2].copy_from_slice(&columns.to_le_bytes());
        payload[2..].copy_from_slice(&rows.to_le_bytes());
        self.write_frame(FRAME_RESIZE, offset_micros, &payload)
    }

    /// Write the terminal state, sync bytes, and atomically publish the final path.
    ///
    /// # Errors
    ///
    /// Returns an error if final framing, flushing, syncing, or atomic publication fails.
    pub fn finish(
        mut self,
        offset_micros: u64,
        exit_code: Option<i32>,
        signal: Option<&str>,
        timed_out: bool,
        cancelled: bool,
    ) -> io::Result<ShellRecordingSummary> {
        let signal = signal.unwrap_or_default().as_bytes();
        let signal_length = u16::try_from(signal.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "recording signal name too long",
            )
        })?;
        let mut payload = Vec::with_capacity(40_usize.saturating_add(signal.len()));
        payload.push(u8::from(exit_code.is_some()));
        payload.extend_from_slice(&exit_code.unwrap_or_default().to_le_bytes());
        payload.push(u8::from(timed_out) | (u8::from(cancelled) << 1));
        payload.extend_from_slice(&signal_length.to_le_bytes());
        payload.extend_from_slice(signal);
        payload.extend_from_slice(&self.checksum.clone().finalize());
        self.write_frame(FRAME_FINISH, offset_micros, &payload)?;
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        fs::rename(&self.partial_path, &self.final_path)?;
        self.finished = true;
        Ok(ShellRecordingSummary {
            columns: self.columns,
            rows: self.rows,
            frame_count: self.frame_count,
            output_bytes: self.output_bytes,
            checksum_sha256: format!("{:x}", self.checksum.clone().finalize()),
        })
    }

    fn publish_commit(
        &mut self,
        observer: Option<&ShellRecordingCommitObserver>,
        finalized: bool,
    ) -> io::Result<()> {
        let Some(observer) = observer else {
            return Ok(());
        };
        if !finalized {
            self.writer.flush()?;
        }
        let path = if finalized {
            self.final_path.clone()
        } else {
            self.partial_path.clone()
        };
        let committed_bytes = fs::metadata(&path)?.len();
        observer(ShellRecordingCommit {
            path,
            committed_bytes,
            finalized,
        });
        Ok(())
    }

    fn write_frame(&mut self, kind: u8, offset_micros: u64, payload: &[u8]) -> io::Result<()> {
        let length = u32::try_from(payload.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "recording frame too large")
        })?;
        self.writer.write_all(&[kind])?;
        self.writer.write_all(&offset_micros.to_le_bytes())?;
        self.writer.write_all(&length.to_le_bytes())?;
        self.writer.write_all(payload)?;
        self.frame_count = self.frame_count.saturating_add(1);
        Ok(())
    }
}

impl Drop for ShellRecordingWriter {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.writer.flush();
        }
    }
}

/// Read and validate a complete recording.
///
/// # Errors
///
/// Returns an error for missing, incomplete, malformed, oversized, or unsupported recordings.
#[allow(clippy::too_many_lines)]
pub fn read_recording(
    path: &Path,
) -> io::Result<(ShellRecordingSummary, Vec<ShellRecordingFrame>)> {
    if path
        .extension()
        .is_some_and(|extension| extension == "partial")
    {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "recording is incomplete",
        ));
    }
    let mut reader = BufReader::new(File::open(path)?);
    let mut magic = [0_u8; 8];
    reader.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid recording magic",
        ));
    }
    let version = read_u16(&mut reader)?;
    if version != FORMAT_VERSION
        && version != SIGNAL_FORMAT_VERSION
        && version != LEGACY_FORMAT_VERSION
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported recording version",
        ));
    }
    let columns = read_u16(&mut reader)?;
    let rows = read_u16(&mut reader)?;
    let mut frames = Vec::new();
    let mut checksum = Sha256::new();
    let mut output_bytes = 0_u64;
    let mut saw_finish = false;
    let mut saw_start = false;
    let mut previous_offset = None;
    loop {
        let mut kind = [0_u8; 1];
        match reader.read_exact(&mut kind) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error),
        }
        let offset_micros = read_u64(&mut reader)?;
        if saw_finish {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "recording contains frames after finish",
            ));
        }
        if previous_offset.is_some_and(|previous| offset_micros < previous) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "recording frame offsets are not monotonic",
            ));
        }
        previous_offset = Some(offset_micros);
        let length = usize::try_from(read_u32(&mut reader)?).unwrap_or(usize::MAX);
        if length > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "recording frame exceeds limit",
            ));
        }
        let mut payload = vec![0_u8; length];
        reader.read_exact(&mut payload)?;
        let frame = match kind[0] {
            FRAME_START if version == FORMAT_VERSION => {
                if saw_start || !frames.is_empty() || !payload.is_empty() || offset_micros != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid recording start frame",
                    ));
                }
                saw_start = true;
                ShellRecordingFrame::Start { offset_micros }
            }
            FRAME_OUTPUT => {
                output_bytes =
                    output_bytes.saturating_add(u64::try_from(payload.len()).unwrap_or(u64::MAX));
                checksum.update(&payload);
                ShellRecordingFrame::Output {
                    offset_micros,
                    bytes: payload,
                }
            }
            FRAME_REPLAY_OUTPUT if version == FORMAT_VERSION => {
                if payload.len() < 32 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid recording replay-output frame length",
                    ));
                }
                let (expected, bytes) = payload.split_at(32);
                if Sha256::digest(bytes).as_slice() != expected {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "recording replay-output checksum mismatch",
                    ));
                }
                ShellRecordingFrame::ReplayOutput {
                    offset_micros,
                    bytes: bytes.to_vec(),
                }
            }
            FRAME_RESIZE => {
                if payload.len() != 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid recording resize frame length",
                    ));
                }
                ShellRecordingFrame::Resize {
                    offset_micros,
                    columns: u16::from_le_bytes([payload[0], payload[1]]),
                    rows: u16::from_le_bytes([payload[2], payload[3]]),
                }
            }
            FRAME_FINISH if version == LEGACY_FORMAT_VERSION && payload.len() == 38 => {
                let actual = checksum.clone().finalize();
                if actual.as_slice() != &payload[6..] {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "recording checksum mismatch",
                    ));
                }
                saw_finish = true;
                ShellRecordingFrame::Finish {
                    offset_micros,
                    exit_code: (payload[0] != 0).then(|| {
                        i32::from_le_bytes([payload[1], payload[2], payload[3], payload[4]])
                    }),
                    signal: None,
                    timed_out: payload[5] & 1 != 0,
                    cancelled: payload[5] & 2 != 0,
                }
            }
            FRAME_FINISH
                if matches!(version, FORMAT_VERSION | SIGNAL_FORMAT_VERSION)
                    && payload.len() >= 40 =>
            {
                let signal_length = usize::from(u16::from_le_bytes([payload[6], payload[7]]));
                let checksum_start = 8_usize.saturating_add(signal_length);
                if payload.len() != checksum_start.saturating_add(32) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid recording finish frame length",
                    ));
                }
                let actual = checksum.clone().finalize();
                if actual.as_slice() != &payload[checksum_start..] {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "recording checksum mismatch",
                    ));
                }
                let signal = std::str::from_utf8(&payload[8..checksum_start]).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid signal name")
                })?;
                saw_finish = true;
                ShellRecordingFrame::Finish {
                    offset_micros,
                    exit_code: (payload[0] != 0).then(|| {
                        i32::from_le_bytes([payload[1], payload[2], payload[3], payload[4]])
                    }),
                    signal: (!signal.is_empty()).then(|| signal.to_owned()),
                    timed_out: payload[5] & 1 != 0,
                    cancelled: payload[5] & 2 != 0,
                }
            }
            FRAME_FINISH => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid recording finish frame",
                ));
            }
            kind => ShellRecordingFrame::Unknown {
                kind,
                offset_micros,
                payload,
            },
        };
        frames.push(frame);
    }
    if version == FORMAT_VERSION && !saw_start {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "recording has no start frame",
        ));
    }
    if !saw_finish {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "recording has no finish frame",
        ));
    }
    Ok((
        ShellRecordingSummary {
            columns,
            rows,
            frame_count: u64::try_from(frames.len()).unwrap_or(u64::MAX),
            output_bytes,
            checksum_sha256: format!("{:x}", checksum.finalize()),
        },
        frames,
    ))
}

fn read_u16(reader: &mut impl Read) -> io::Result<u16> {
    let mut bytes = [0_u8; 2];
    reader.read_exact(&mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut bytes = [0_u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incremental_decoder_preserves_exact_frames_across_single_byte_ranges() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("incremental.bcsr");
        let mut writer = ShellRecordingWriter::create(&path, 80, 24).expect("writer");
        let first = b"\xffsplit-utf8:\xe7\x95";
        let second = b"\x8c\0tail";
        writer.write_output(1, first).expect("first output");
        writer.write_resize(2, 132, 40).expect("resize");
        writer.write_output(3, second).expect("second output");
        writer
            .finish(4, Some(0), None, false, false)
            .expect("finish");
        let bytes = std::fs::read(&path).expect("recording bytes");
        let (_, expected) = read_recording(&path).expect("complete recording");
        let mut decoder = IncrementalShellRecordingDecoder::default();
        let mut actual = Vec::new();
        for (offset, byte) in bytes.iter().enumerate() {
            actual.extend(
                decoder
                    .push(
                        u64::try_from(offset).expect("offset"),
                        std::slice::from_ref(byte),
                    )
                    .expect("incremental byte"),
            );
        }
        assert_eq!(decoder.dimensions(), Some((80, 24)));
        assert_eq!(actual, expected);
        assert!(decoder.push(0, b"duplicate").is_err());
    }

    #[test]
    fn round_trip_preserves_exact_bytes_resize_timing_and_finish() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("recording.bcsr");
        let mut writer = ShellRecordingWriter::create(&path, 80, 24).expect("writer");
        let bytes = b"\xffhello\r\x1b[2Kworld\0";
        writer.write_output(17, bytes).expect("output");
        writer.write_resize(29, 132, 40).expect("resize");
        let written = writer
            .finish(41, Some(7), Some("SIGTERM"), true, false)
            .expect("finish");
        let (read, frames) = read_recording(&path).expect("read");
        assert_eq!(read.columns, 80);
        assert_eq!(read.rows, 24);
        assert_eq!(
            read.output_bytes,
            u64::try_from(bytes.len()).expect("length")
        );
        assert_eq!(read.checksum_sha256, written.checksum_sha256);
        assert_eq!(frames[0], ShellRecordingFrame::Start { offset_micros: 0 });
        assert_eq!(
            frames[1],
            ShellRecordingFrame::Output {
                offset_micros: 17,
                bytes: bytes.to_vec()
            }
        );
        assert_eq!(
            frames[2],
            ShellRecordingFrame::Resize {
                offset_micros: 29,
                columns: 132,
                rows: 40
            }
        );
        assert_eq!(
            frames[3],
            ShellRecordingFrame::Finish {
                offset_micros: 41,
                exit_code: Some(7),
                signal: Some("SIGTERM".to_owned()),
                timed_out: true,
                cancelled: false
            }
        );
    }

    #[test]
    fn contended_recording_sequence_never_blocks_live_output_and_prevents_publication() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("contended.bcsr");
        let mut writer = AsyncShellRecordingWriter::create(&path, 80, 24).expect("writer");
        let sequence = Arc::clone(&writer.sequence);
        let held = sequence.lock().expect("sequence lock");

        assert!(!writer.try_write_output(1, b"must not wait"));
        drop(held);
        assert!(writer.finish(2, Some(0), None, false, false).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn async_writer_publishes_monotonic_complete_boundaries() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("observed.bcsr");
        let commits = Arc::new(Mutex::new(Vec::<ShellRecordingCommit>::new()));
        let observer_commits = Arc::clone(&commits);
        let observer: ShellRecordingCommitObserver = Arc::new(move |commit| {
            observer_commits.lock().expect("commits").push(commit);
        });
        let mut writer =
            AsyncShellRecordingWriter::create_with_observer(&path, 80, 24, Some(observer))
                .expect("writer");
        assert!(writer.try_write_output(1, b"one"));
        assert!(writer.try_write_resize(2, 100, 40));
        writer
            .finish(3, Some(0), None, false, false)
            .expect("finish");

        let commits = commits.lock().expect("commits");
        assert!(commits.len() >= 4);
        assert!(
            commits
                .windows(2)
                .all(|window| { window[1].committed_bytes >= window[0].committed_bytes })
        );
        assert!(commits.last().expect("final commit").finalized);
        assert_eq!(commits.last().expect("final commit").path, path);
        assert!(
            commits[..commits.len() - 1]
                .iter()
                .all(|commit| !commit.finalized
                    && commit.path.extension().is_some_and(|ext| ext == "partial"))
        );
        drop(commits);
    }

    #[test]
    fn async_writer_preserves_frames_and_finalizes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("recording.bcsr");
        let mut writer = AsyncShellRecordingWriter::create(&path, 80, 24).expect("writer");
        assert!(writer.try_write_output(1, b"first"));
        assert!(writer.try_write_output(2, b"second"));
        assert!(writer.try_write_resize(3, 100, 30));
        writer
            .finish(4, Some(0), None, false, false)
            .expect("finish");
        let (_, frames) = read_recording(&path).expect("recording");
        assert_eq!(
            frames,
            vec![
                ShellRecordingFrame::Start { offset_micros: 0 },
                ShellRecordingFrame::Output {
                    offset_micros: 1,
                    bytes: b"first".to_vec(),
                },
                ShellRecordingFrame::Output {
                    offset_micros: 2,
                    bytes: b"second".to_vec(),
                },
                ShellRecordingFrame::Resize {
                    offset_micros: 3,
                    columns: 100,
                    rows: 30,
                },
                ShellRecordingFrame::Finish {
                    offset_micros: 4,
                    exit_code: Some(0),
                    signal: None,
                    timed_out: false,
                    cancelled: false,
                },
            ]
        );
    }

    #[test]
    fn legacy_version_one_recording_remains_readable() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("legacy.bcsr");
        let output = b"legacy bytes";
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&LEGACY_FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&80_u16.to_le_bytes());
        bytes.extend_from_slice(&24_u16.to_le_bytes());
        bytes.push(FRAME_OUTPUT);
        bytes.extend_from_slice(&1_u64.to_le_bytes());
        bytes.extend_from_slice(&u32::try_from(output.len()).expect("length").to_le_bytes());
        bytes.extend_from_slice(output);
        let mut finish = [0_u8; 38];
        finish[0] = 1;
        finish[1..5].copy_from_slice(&0_i32.to_le_bytes());
        finish[6..].copy_from_slice(&Sha256::digest(output));
        bytes.push(FRAME_FINISH);
        bytes.extend_from_slice(&2_u64.to_le_bytes());
        bytes.extend_from_slice(&38_u32.to_le_bytes());
        bytes.extend_from_slice(&finish);
        fs::write(&path, bytes).expect("legacy recording");

        let (_, frames) = read_recording(&path).expect("legacy recording readable");
        assert!(matches!(
            frames.last(),
            Some(ShellRecordingFrame::Finish {
                exit_code: Some(0),
                signal: None,
                timed_out: false,
                cancelled: false,
                ..
            })
        ));
    }

    #[test]
    fn version_two_signal_recording_remains_readable() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("version-two.bcsr");
        let output = b"version two bytes";
        let signal = b"SIGTERM";
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&SIGNAL_FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&80_u16.to_le_bytes());
        bytes.extend_from_slice(&24_u16.to_le_bytes());
        bytes.push(FRAME_OUTPUT);
        bytes.extend_from_slice(&1_u64.to_le_bytes());
        bytes.extend_from_slice(&u32::try_from(output.len()).expect("length").to_le_bytes());
        bytes.extend_from_slice(output);
        let mut finish = Vec::new();
        finish.push(1);
        finish.extend_from_slice(&1_i32.to_le_bytes());
        finish.push(0);
        finish.extend_from_slice(
            &u16::try_from(signal.len())
                .expect("signal length")
                .to_le_bytes(),
        );
        finish.extend_from_slice(signal);
        finish.extend_from_slice(&Sha256::digest(output));
        bytes.push(FRAME_FINISH);
        bytes.extend_from_slice(&2_u64.to_le_bytes());
        bytes.extend_from_slice(&u32::try_from(finish.len()).expect("length").to_le_bytes());
        bytes.extend_from_slice(&finish);
        fs::write(&path, bytes).expect("version two recording");

        let (_, frames) = read_recording(&path).expect("version two recording readable");
        assert!(matches!(
            frames.last(),
            Some(ShellRecordingFrame::Finish {
                exit_code: Some(1),
                signal: Some(signal),
                timed_out: false,
                cancelled: false,
                ..
            }) if signal == "SIGTERM"
        ));
    }

    #[test]
    fn malformed_lifecycle_frames_are_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");
        for (name, mutation, expected) in [
            ("non_monotonic", 1_usize, "offsets are not monotonic"),
            ("frame_after_finish", 2_usize, "frames after finish"),
        ] {
            let path = dir.path().join(format!("{name}.bcsr"));
            let mut writer = ShellRecordingWriter::create(&path, 80, 24).expect("writer");
            writer.write_output(2, b"hello").expect("output");
            writer
                .finish(3, Some(0), None, false, false)
                .expect("finish");
            let mut bytes = fs::read(&path).expect("recording bytes");
            match mutation {
                1 => {
                    let finish_offset_field = 14 + 13 + 13 + 5 + 1;
                    bytes[finish_offset_field..finish_offset_field + 8]
                        .copy_from_slice(&1_u64.to_le_bytes());
                }
                2 => {
                    bytes.push(99);
                    bytes.extend_from_slice(&4_u64.to_le_bytes());
                    bytes.extend_from_slice(&0_u32.to_le_bytes());
                }
                _ => unreachable!(),
            }
            fs::write(&path, bytes).expect("mutated recording");
            let error = read_recording(&path).expect_err("malformed lifecycle must fail");
            assert!(error.to_string().contains(expected), "{name}: {error}");
        }
    }

    #[test]
    fn replay_output_checksum_mismatch_is_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("replay-checksum.bcsr");
        let mut writer = ShellRecordingWriter::create(&path, 80, 24).expect("writer");
        writer.write_output(1, b"raw").expect("raw output");
        writer
            .write_replay_output(1, b"visible")
            .expect("replay output");
        writer
            .finish(2, Some(0), None, false, false)
            .expect("finish");
        let mut bytes = fs::read(&path).expect("recording bytes");
        let frame_offset = 8 + 2 + 2 + 2 + (1 + 8 + 4) + (1 + 8 + 4 + 3);
        let replay_payload_offset = frame_offset + 1 + 8 + 4;
        bytes[replay_payload_offset + 32] ^= 0xff;
        fs::write(&path, bytes).expect("corrupt replay output");

        let error = read_recording(&path).expect_err("replay corruption should fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error
                .to_string()
                .contains("replay-output checksum mismatch")
        );
    }

    #[test]
    fn checksum_mismatch_is_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("recording.bcsr");
        let mut writer = ShellRecordingWriter::create(&path, 80, 24).expect("writer");
        writer.write_output(1, b"hello").expect("output");
        writer
            .finish(2, Some(0), None, false, false)
            .expect("finish");
        let mut bytes = fs::read(&path).expect("recording bytes");
        let output_offset = 8 + 2 + 2 + 2 + (1 + 8 + 4) + (1 + 8 + 4);
        bytes[output_offset] ^= 0xff;
        fs::write(&path, bytes).expect("corrupt recording");
        let error = read_recording(&path).expect_err("corruption should fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("checksum mismatch"));
    }

    #[test]
    fn incomplete_recording_retains_durable_start_and_never_publishes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("recording.bcsr");
        let partial = path.with_extension("shell-recording.partial");
        {
            let mut writer = ShellRecordingWriter::create(&path, 80, 24).expect("writer");
            writer.write_output(1, b"partial").expect("output");
        }
        assert!(!path.exists());
        assert!(partial.exists());
        let bytes = fs::read(&partial).expect("partial bytes");
        assert!(bytes.len() >= 8 + 2 + 2 + 2 + 1 + 8 + 4);
        assert_eq!(&bytes[..8], MAGIC);
        assert_eq!(u16::from_le_bytes([bytes[8], bytes[9]]), FORMAT_VERSION);
        assert_eq!(bytes[14], FRAME_START);
        assert_eq!(
            u64::from_le_bytes(bytes[15..23].try_into().expect("start offset")),
            0
        );
        assert_eq!(
            u32::from_le_bytes(bytes[23..27].try_into().expect("start length")),
            0
        );
        let error = read_recording(&partial).expect_err("partial recording must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert!(error.to_string().contains("incomplete"));
    }
}
