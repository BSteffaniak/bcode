//! Streaming logical artifact access for lossless maintenance transitions.
//!
//! Representation selection is explicit. This module never guesses from magic bytes and never
//! publishes files. All handles must be confined and protected by the caller's durable authority.

use crate::artifact_compression::{
    ArtifactCompression, ArtifactPreparation, prepare_compressed_artifact,
    read_compressed_artifact_range, verify_compressed_artifact,
};
use std::io::{self, Read, Seek, SeekFrom, Write};

const BUFFER_BYTES: u32 = 256 * 1024;

/// Logical reader seeks are bounded to the requested buffer; direct tier transitions and explicit
/// restore paths do not materialize a full uncompressed temporary.
///
/// Explicit representation selected by a compatibility-validated storage contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactEncoding {
    /// Original bytes, with no container interpretation.
    Raw,
    /// Versioned seekable Zstd artifact container.
    ChunkedZstd,
}

/// A seekable logical byte stream over raw or chunk-compressed artifact storage.
///
/// Holds at most one bounded logical buffer; seeking does not decompress intervening content.
/// Decoding is always available, independent of scheduling configuration. The source must remain
/// immutable for the lifetime of this reader. No cache is retained after dropping it.
pub struct ArtifactReader<R> {
    source: R,
    encoding: ArtifactEncoding,
    logical_bytes: u64,
    position: u64,
    buffer_start: u64,
    buffer: Vec<u8>,
}

impl<R: Read + Seek> ArtifactReader<R> {
    /// Open a logical reader with explicit representation selection.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O or an invalid/unsupported compressed header. Opening does not
    /// validate all chunks; each touched chunk is validated during reads.
    pub fn new(mut source: R, encoding: ArtifactEncoding) -> io::Result<Self> {
        let logical_bytes = match encoding {
            ArtifactEncoding::Raw => source.seek(SeekFrom::End(0))?,
            ArtifactEncoding::ChunkedZstd => read_compressed_artifact_range(&mut source, 0, 1)?.0,
        };
        Ok(Self {
            source,
            encoding,
            logical_bytes,
            position: 0,
            buffer_start: 0,
            buffer: Vec::new(),
        })
    }

    /// Logical content length, independent of compression.
    #[must_use]
    pub const fn logical_bytes(&self) -> u64 {
        self.logical_bytes
    }

    /// Return the underlying handle without modifying its content.
    #[must_use]
    pub fn into_inner(self) -> R {
        self.source
    }
}

impl<R: Read + Seek> Read for ArtifactReader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.position >= self.logical_bytes {
            return Ok(0);
        }
        if self.encoding == ArtifactEncoding::Raw {
            self.source.seek(SeekFrom::Start(self.position))?;
            let limit =
                usize::try_from((self.logical_bytes - self.position).min(output.len() as u64))
                    .map_err(|_| io::Error::other("artifact range overflow"))?;
            let count = self.source.read(&mut output[..limit])?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "artifact source changed",
                ));
            }
            self.position += count as u64;
            return Ok(count);
        }
        let buffer_end = self.buffer_start + self.buffer.len() as u64;
        if self.position < self.buffer_start || self.position >= buffer_end {
            self.buffer_start = self.position / u64::from(BUFFER_BYTES) * u64::from(BUFFER_BYTES);
            let (total, bytes) =
                read_compressed_artifact_range(&mut self.source, self.buffer_start, BUFFER_BYTES)?;
            if total != self.logical_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "artifact source changed",
                ));
            }
            self.buffer = bytes;
        }
        let start = usize::try_from(self.position - self.buffer_start)
            .map_err(|_| io::Error::other("artifact buffer overflow"))?;
        let count = output.len().min(self.buffer.len() - start);
        output[..count].copy_from_slice(&self.buffer[start..start + count]);
        self.position += count as u64;
        Ok(count)
    }
}

impl<R: Read + Seek> Seek for ArtifactReader<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(position) => Some(position),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
            SeekFrom::End(delta) => self.logical_bytes.checked_add_signed(delta),
        }
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "invalid logical artifact seek")
        })?;
        self.position = position;
        Ok(position)
    }
}

/// Prepare a verified colder representation directly from compressed or raw storage.
///
/// No full-size uncompressed temporary is needed. The saving threshold is measured against the
/// **current physical representation**, so light-to-deep transitions that increase disk usage are
/// rejected. Source content is never modified; candidate publication remains caller-owned.
///
/// # Errors
///
/// Returns an error for damaged/unsupported input, nonempty candidate, cancellation, I/O, or
/// verification failure. A failed or rejected candidate must remain unpublished.
pub fn prepare_artifact_transition(
    source: &mut (impl Read + Seek),
    encoding: ArtifactEncoding,
    candidate: &mut (impl Read + Write + Seek),
    target: ArtifactCompression,
    minimum_saved_bytes: u64,
    mut check_cancelled: impl FnMut() -> io::Result<()>,
) -> io::Result<ArtifactTransition> {
    check_cancelled()?;
    let previous_bytes = source.seek(SeekFrom::End(0))?;
    let mut logical = ArtifactReader::new(source, encoding)?;
    let logical_bytes = logical.logical_bytes();
    let preparation =
        prepare_compressed_artifact(&mut logical, candidate, target, 0, &mut check_cancelled)?;
    let candidate_bytes = match preparation {
        ArtifactPreparation::Ready {
            candidate_bytes, ..
        }
        | ArtifactPreparation::InsufficientSavings {
            candidate_bytes, ..
        } => candidate_bytes,
    };
    let saved_bytes = previous_bytes
        .checked_sub(candidate_bytes)
        .filter(|saved| *saved > 0 && *saved >= minimum_saved_bytes);
    if saved_bytes.is_some()
        && matches!(preparation, ArtifactPreparation::InsufficientSavings { .. })
    {
        // A candidate can save physical space relative to a bloated compressed source while not
        // beating the logical raw size. The raw-size planner skipped verification in that case.
        logical.rewind()?;
        verify_compressed_artifact(candidate, &mut logical, &mut check_cancelled)?;
    }
    Ok(ArtifactTransition {
        logical_bytes,
        previous_bytes,
        candidate_bytes,
        saved_bytes,
    })
}

/// Physical accounting for an unpublished candidate. `saved_bytes = None` means do not publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactTransition {
    /// Original logical content bytes.
    pub logical_bytes: u64,
    /// Existing representation's file length.
    pub previous_bytes: u64,
    /// Candidate representation's file length, including overhead.
    pub candidate_bytes: u64,
    /// Verified positive reduction meeting the threshold, or no eligible saving.
    pub saved_bytes: Option<u64>,
}

/// Restore the original byte stream into an empty destination with bounded memory.
///
/// Source chunks are integrity-checked and cancellation is checked between bounded copies.
/// This is explicit maintenance/export work, not an implicit interactive read or file replacement.
///
/// # Errors
///
/// Returns an error for nonempty output, corruption, cancellation, or I/O. A partially restored
/// destination is not authoritative and must not be published by the caller.
pub fn restore_artifact(
    source: &mut (impl Read + Seek),
    encoding: ArtifactEncoding,
    destination: &mut (impl Write + Seek),
    mut check_cancelled: impl FnMut() -> io::Result<()>,
) -> io::Result<u64> {
    check_cancelled()?;
    if destination.seek(SeekFrom::End(0))? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "artifact destination must be empty",
        ));
    }
    let mut reader = ArtifactReader::new(source, encoding)?;
    let mut buffer = vec![0; BUFFER_BYTES as usize];
    loop {
        check_cancelled()?;
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        destination.write_all(&buffer[..count])?;
    }
    destination.flush()?;
    Ok(reader.logical_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn fixture() -> Vec<u8> {
        let line = "terminal output: café 世界 🦀\n";
        line.repeat(40_000).into_bytes()
    }

    #[test]
    fn raw_to_light_to_deep_and_restore_are_byte_exact() {
        let bytes = fixture();
        let mut raw = Cursor::new(bytes.clone());
        let mut light = Cursor::new(Vec::new());
        let first = prepare_artifact_transition(
            &mut raw,
            ArtifactEncoding::Raw,
            &mut light,
            ArtifactCompression::Light,
            1,
            || Ok(()),
        )
        .expect("light");
        assert!(first.saved_bytes.is_some());
        let mut deep = Cursor::new(Vec::new());
        let next = prepare_artifact_transition(
            &mut light,
            ArtifactEncoding::ChunkedZstd,
            &mut deep,
            ArtifactCompression::Deep,
            1,
            || Ok(()),
        )
        .expect("deep");
        assert_eq!(next.previous_bytes, first.candidate_bytes);
        assert_eq!(next.logical_bytes, bytes.len() as u64);
        let mut restored = Cursor::new(Vec::new());
        assert_eq!(
            restore_artifact(
                &mut deep,
                ArtifactEncoding::ChunkedZstd,
                &mut restored,
                || Ok(())
            )
            .expect("restore"),
            bytes.len() as u64
        );
        assert_eq!(restored.into_inner(), bytes);
        assert_eq!(raw.into_inner(), bytes);
    }

    #[test]
    fn seeks_and_small_reads_preserve_original_offsets() {
        let bytes = fixture();
        let mut compressed = Cursor::new(Vec::new());
        prepare_artifact_transition(
            &mut Cursor::new(&bytes),
            ArtifactEncoding::Raw,
            &mut compressed,
            ArtifactCompression::Light,
            1,
            || Ok(()),
        )
        .expect("prepare");
        let mut reader =
            ArtifactReader::new(compressed, ArtifactEncoding::ChunkedZstd).expect("reader");
        for start in [
            0,
            19,
            BUFFER_BYTES as usize - 3,
            BUFFER_BYTES as usize + 7,
            bytes.len() - 10,
        ] {
            reader.seek(SeekFrom::Start(start as u64)).expect("seek");
            let mut actual = [0; 10];
            reader.read_exact(&mut actual).expect("read");
            assert_eq!(actual, bytes[start..start + 10]);
        }
        reader.seek(SeekFrom::End(10)).expect("past eof");
        assert_eq!(reader.read(&mut [0; 1]).expect("eof"), 0);
        reader.rewind().expect("rewind");
        assert!(reader.seek(SeekFrom::Current(-1)).is_err());
    }

    #[test]
    fn repeated_tier_does_not_claim_logical_size_as_savings() {
        let bytes = fixture();
        let mut first = Cursor::new(Vec::new());
        prepare_artifact_transition(
            &mut Cursor::new(bytes),
            ArtifactEncoding::Raw,
            &mut first,
            ArtifactCompression::Light,
            1,
            || Ok(()),
        )
        .expect("first");
        let mut second = Cursor::new(Vec::new());
        let result = prepare_artifact_transition(
            &mut first,
            ArtifactEncoding::ChunkedZstd,
            &mut second,
            ArtifactCompression::Light,
            1,
            || Ok(()),
        )
        .expect("second");
        assert_eq!(result.saved_bytes, None);
        assert_eq!(first.into_inner(), second.into_inner());
    }

    #[test]
    fn explicit_encoding_and_cancellation_fail_closed() {
        let bytes = b"not a compressed container";
        assert!(ArtifactReader::new(Cursor::new(bytes), ArtifactEncoding::ChunkedZstd).is_err());
        let mut raw = ArtifactReader::new(Cursor::new(bytes), ArtifactEncoding::Raw).expect("raw");
        let mut actual = Vec::new();
        raw.read_to_end(&mut actual).expect("read");
        assert_eq!(actual, bytes);
        let mut destination = Cursor::new(Vec::new());
        assert!(
            restore_artifact(
                &mut Cursor::new(bytes),
                ArtifactEncoding::Raw,
                &mut destination,
                || Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"))
            )
            .is_err()
        );
        assert!(destination.into_inner().is_empty());
    }
}
