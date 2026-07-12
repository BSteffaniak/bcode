//! Versioned, shell-owned byte-exact terminal recording format.
//!
//! The format is append-friendly while a command runs and atomically published on completion.
//! Readers reject incomplete files, validate the complete frame stream, and skip unknown frame
//! kinds by their declared payload length for forward compatibility.

use sha2::{Digest as _, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write as _};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"BCSHREC\0";
const FORMAT_VERSION: u16 = 1;
const FRAME_OUTPUT: u8 = 1;
const FRAME_RESIZE: u8 = 2;
const FRAME_FINISH: u8 = 3;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// One decoded shell recording frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellRecordingFrame {
    /// Exact PTY bytes emitted at the given monotonic offset.
    Output { offset_micros: u64, bytes: Vec<u8> },
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
        Ok(Self {
            final_path: path.to_path_buf(),
            partial_path,
            writer,
            columns,
            rows,
            frame_count: 0,
            output_bytes: 0,
            checksum: Sha256::new(),
            finished: false,
        })
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
        timed_out: bool,
        cancelled: bool,
    ) -> io::Result<ShellRecordingSummary> {
        let mut payload = [0_u8; 38];
        payload[0] = u8::from(exit_code.is_some());
        payload[1..5].copy_from_slice(&exit_code.unwrap_or_default().to_le_bytes());
        payload[5] = u8::from(timed_out) | (u8::from(cancelled) << 1);
        let checksum = self.checksum.clone().finalize();
        payload[6..].copy_from_slice(&checksum);
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
    if version != FORMAT_VERSION {
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
    loop {
        let mut kind = [0_u8; 1];
        match reader.read_exact(&mut kind) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error),
        }
        let offset_micros = read_u64(&mut reader)?;
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
            FRAME_OUTPUT => {
                output_bytes =
                    output_bytes.saturating_add(u64::try_from(payload.len()).unwrap_or(u64::MAX));
                checksum.update(&payload);
                ShellRecordingFrame::Output {
                    offset_micros,
                    bytes: payload,
                }
            }
            FRAME_RESIZE if payload.len() == 4 => ShellRecordingFrame::Resize {
                offset_micros,
                columns: u16::from_le_bytes([payload[0], payload[1]]),
                rows: u16::from_le_bytes([payload[2], payload[3]]),
            },
            FRAME_FINISH if payload.len() == 38 => {
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
                    timed_out: payload[5] & 1 != 0,
                    cancelled: payload[5] & 2 != 0,
                }
            }
            kind => ShellRecordingFrame::Unknown {
                kind,
                offset_micros,
                payload,
            },
        };
        frames.push(frame);
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
    fn round_trip_preserves_exact_bytes_resize_timing_and_finish() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("recording.bcsr");
        let mut writer = ShellRecordingWriter::create(&path, 80, 24).expect("writer");
        let bytes = b"\xffhello\r\x1b[2Kworld\0";
        writer.write_output(17, bytes).expect("output");
        writer.write_resize(29, 132, 40).expect("resize");
        let written = writer.finish(41, Some(7), true, false).expect("finish");
        let (read, frames) = read_recording(&path).expect("read");
        assert_eq!(read.columns, 80);
        assert_eq!(read.rows, 24);
        assert_eq!(
            read.output_bytes,
            u64::try_from(bytes.len()).expect("length")
        );
        assert_eq!(read.checksum_sha256, written.checksum_sha256);
        assert_eq!(
            frames[0],
            ShellRecordingFrame::Output {
                offset_micros: 17,
                bytes: bytes.to_vec()
            }
        );
        assert_eq!(
            frames[1],
            ShellRecordingFrame::Resize {
                offset_micros: 29,
                columns: 132,
                rows: 40
            }
        );
        assert_eq!(
            frames[2],
            ShellRecordingFrame::Finish {
                offset_micros: 41,
                exit_code: Some(7),
                timed_out: true,
                cancelled: false
            }
        );
    }

    #[test]
    fn checksum_mismatch_is_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("recording.bcsr");
        let mut writer = ShellRecordingWriter::create(&path, 80, 24).expect("writer");
        writer.write_output(1, b"hello").expect("output");
        writer.finish(2, Some(0), false, false).expect("finish");
        let mut bytes = fs::read(&path).expect("recording bytes");
        let output_offset = 8 + 2 + 2 + 2 + 1 + 8 + 4;
        bytes[output_offset] ^= 0xff;
        fs::write(&path, bytes).expect("corrupt recording");
        let error = read_recording(&path).expect_err("corruption should fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("checksum mismatch"));
    }

    #[test]
    fn incomplete_recording_is_not_published() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("recording.bcsr");
        let partial = path.with_extension("shell-recording.partial");
        {
            let mut writer = ShellRecordingWriter::create(&path, 80, 24).expect("writer");
            writer.write_output(1, b"partial").expect("output");
        }
        assert!(!path.exists());
        assert!(partial.exists());
        assert!(read_recording(&partial).is_err());
    }
}
