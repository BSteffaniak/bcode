//! Versioned, seekable, lossless artifact container codec.
//!
//! This codec does not select canonical representations or replace files. Callers must provide
//! confined handles, verified ownership, reader compatibility fencing, and interruption-safe
//! publication before using encoded bytes as durable artifacts. Raw data is never auto-detected.

use bcode_session_models::MAX_SESSION_ARTIFACT_RANGE_BYTES;
use sha2::{Digest as _, Sha256};
use std::io::{self, Read, Seek, SeekFrom, Write};

const MAGIC: &[u8; 8] = b"BCARTZ01";
const VERSION: u32 = 1;
const CHUNK: u32 = 256 * 1024;
const HEADER_BYTES: usize = 64;
const ENTRY_BYTES: usize = 80;
const HEADER: u64 = HEADER_BYTES as u64;
const ENTRY: u64 = ENTRY_BYTES as u64;
const MAX_COMPRESSED: u64 = CHUNK as u64 + 4096;

/// Encoding policy for immutable artifacts. Levels affect encoding cost, not logical content.
#[derive(Debug, Clone, Copy)]
pub enum ArtifactCompression {
    /// Low-cost compression for warm storage.
    Light,
    /// Higher encoding effort for cold storage.
    Deep,
}

impl ArtifactCompression {
    const fn level(self) -> i32 {
        match self {
            Self::Light => 1,
            Self::Deep => 12,
        }
    }
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid compressed artifact")
}

fn number(bytes: &[u8]) -> io::Result<u64> {
    Ok(u64::from_le_bytes(bytes.try_into().map_err(|_| invalid())?))
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn data_start(count: u64) -> io::Result<u64> {
    count
        .checked_mul(ENTRY)
        .and_then(|n| n.checked_add(HEADER))
        .ok_or_else(invalid)
}

/// Encode exactly `logical_bytes` from the current source position into an empty destination.
///
/// Memory is bounded to one chunk. The caller supplies the source length and a cancellation check
/// invoked before each chunk and before finalizing. No destination is published by this function;
/// a failure leaves incomplete output that must not become authoritative. Checksums protect the
/// header, each index entry (including its ordinal), and each expanded chunk.
///
/// # Errors
///
/// Returns an error for nonempty output, source length mismatch, arithmetic overflow, cancellation,
/// codec failure, or I/O failure. Callers must not concurrently mutate either handle.
pub fn encode_artifact(
    source: &mut impl Read,
    destination: &mut (impl Write + Seek),
    logical_bytes: u64,
    compression: ArtifactCompression,
    mut check_cancelled: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    check_cancelled()?;
    if destination.seek(SeekFrom::End(0))? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "artifact destination must be empty",
        ));
    }
    let count = logical_bytes.div_ceil(u64::from(CHUNK));
    let mut position = data_start(count)?;
    let mut plain = vec![0; CHUNK as usize];
    for ordinal in 0..count {
        check_cancelled()?;
        let length =
            usize::try_from((logical_bytes - ordinal * u64::from(CHUNK)).min(u64::from(CHUNK)))
                .map_err(|_| invalid())?;
        source.read_exact(&mut plain[..length])?;
        let compressed = zstd::bulk::compress(&plain[..length], compression.level())?;
        if compressed.len() as u64 > MAX_COMPRESSED {
            return Err(invalid());
        }
        destination.seek(SeekFrom::Start(position))?;
        destination.write_all(&compressed)?;
        let mut entry = [0_u8; ENTRY_BYTES];
        entry[..8].copy_from_slice(&position.to_le_bytes());
        entry[8..16].copy_from_slice(&(compressed.len() as u64).to_le_bytes());
        entry[16..48].copy_from_slice(&digest(&plain[..length]));
        let checksum = entry_digest(ordinal, &entry[..48]);
        entry[48..].copy_from_slice(&checksum);
        destination.seek(SeekFrom::Start(HEADER + ordinal * ENTRY))?;
        destination.write_all(&entry)?;
        position = position
            .checked_add(compressed.len() as u64)
            .ok_or_else(invalid)?;
    }
    let mut excess = [0];
    if source.read(&mut excess)? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "artifact source length changed",
        ));
    }
    check_cancelled()?;
    let mut header = [0_u8; HEADER_BYTES];
    header[..8].copy_from_slice(MAGIC);
    header[8..12].copy_from_slice(&VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&CHUNK.to_le_bytes());
    header[16..24].copy_from_slice(&logical_bytes.to_le_bytes());
    header[24..32].copy_from_slice(&count.to_le_bytes());
    let checksum = digest(&header[..32]);
    header[32..].copy_from_slice(&checksum);
    destination.seek(SeekFrom::Start(0))?;
    destination.write_all(&header)?;
    destination.flush()
}

fn entry_digest(ordinal: u64, bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(ordinal.to_le_bytes());
    hasher.update(bytes);
    hasher.finalize().into()
}

/// Read a logical byte range from an explicitly identified compressed artifact.
///
/// Reads only the header, intersecting index entries and compressed chunks. Returns logical total
/// length and original bytes. Decoding is bounded independently of untrusted advertised lengths;
/// unrelated chunks are neither read nor validated. This is not whole-container verification.
///
/// # Errors
///
/// Returns an error for invalid range length, offset past EOF, unsupported versions, malformed or
/// truncated metadata, oversized decoder windows, expanded-size mismatch, checksum failure or I/O.
/// Never falls back to raw bytes. The source must remain immutable during the operation.
pub fn read_compressed_artifact_range(
    source: &mut (impl Read + Seek),
    offset: u64,
    length: u32,
) -> io::Result<(u64, Vec<u8>)> {
    if length == 0 || length > MAX_SESSION_ARTIFACT_RANGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid artifact range length",
        ));
    }
    let physical_bytes = source.seek(SeekFrom::End(0))?;
    source.seek(SeekFrom::Start(0))?;
    let mut header = [0; HEADER_BYTES];
    source.read_exact(&mut header)?;
    if &header[..8] != MAGIC || header[32..] != digest(&header[..32]) {
        return Err(invalid());
    }
    if header[8..12] != VERSION.to_le_bytes() || header[12..16] != CHUNK.to_le_bytes() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported artifact container version or chunk size",
        ));
    }
    let total = number(&header[16..24])?;
    let count = number(&header[24..32])?;
    let start = data_start(count)?;
    if count != total.div_ceil(u64::from(CHUNK))
        || start > physical_bytes
        || (count == 0 && physical_bytes != HEADER)
    {
        return Err(invalid());
    }
    if offset > total {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "artifact offset exceeds logical length",
        ));
    }
    let end = offset.saturating_add(u64::from(length)).min(total);
    let mut output = Vec::with_capacity(usize::try_from(end - offset).map_err(|_| invalid())?);
    if offset == end {
        return Ok((total, output));
    }
    for ordinal in offset / u64::from(CHUNK)..end.div_ceil(u64::from(CHUNK)) {
        source.seek(SeekFrom::Start(HEADER + ordinal * ENTRY))?;
        let mut entry = [0; ENTRY_BYTES];
        source.read_exact(&mut entry)?;
        if entry[48..] != entry_digest(ordinal, &entry[..48]) {
            return Err(invalid());
        }
        let position = number(&entry[..8])?;
        let compressed_len = number(&entry[8..16])?;
        let physical_end = position.checked_add(compressed_len).ok_or_else(invalid)?;
        if position < start
            || compressed_len == 0
            || compressed_len > MAX_COMPRESSED
            || physical_end > physical_bytes
        {
            return Err(invalid());
        }
        source.seek(SeekFrom::Start(position))?;
        let mut compressed = vec![0; usize::try_from(compressed_len).map_err(|_| invalid())?];
        source.read_exact(&mut compressed)?;
        let mut decoder = zstd::stream::read::Decoder::new(compressed.as_slice())?;
        decoder.window_log_max(18)?;
        let chunk_start = ordinal * u64::from(CHUNK);
        let expanded_len = (total - chunk_start).min(u64::from(CHUNK));
        let mut plain = Vec::with_capacity(usize::try_from(expanded_len).map_err(|_| invalid())?);
        decoder.take(expanded_len + 1).read_to_end(&mut plain)?;
        if plain.len() as u64 != expanded_len || entry[16..48] != digest(&plain) {
            return Err(invalid());
        }
        let from = usize::try_from(offset.saturating_sub(chunk_start)).map_err(|_| invalid())?;
        let to = usize::try_from((end - chunk_start).min(expanded_len)).map_err(|_| invalid())?;
        output.extend_from_slice(&plain[from..to]);
    }
    Ok((total, output))
}

/// Result of preparing a verified, unpublished compressed artifact candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactPreparation {
    /// Candidate is byte-equivalent and meets the caller's minimum absolute space saving.
    Ready {
        /// Original logical bytes, also the raw input file length.
        logical_bytes: u64,
        /// Complete candidate length including its index and header.
        candidate_bytes: u64,
        /// Raw file bytes minus candidate bytes, not an allocated-filesystem-space estimate.
        saved_bytes: u64,
    },
    /// Candidate would grow the data or fails the requested savings threshold.
    InsufficientSavings {
        /// Original logical length.
        logical_bytes: u64,
        /// Complete encoded length.
        candidate_bytes: u64,
    },
}

/// Encode and verify an unpublished candidate, retaining the original unchanged.
///
/// Operates on complete caller-owned streams from byte zero. The destination must be empty and
/// seekable. Content is encoded with bounded memory, then compared against the entire original;
/// output is eligible only when it is strictly smaller and saves at least `minimum_saved_bytes`.
/// All container overhead counts against savings. Cancellation is checked through both passes.
///
/// This does not sync, publish, truncate, or remove any file. Even `Ready` requires durable
/// ownership, original identity/checksum verification, compatibility fencing, and interruption-safe
/// publication by the caller. A rejected or failed candidate remains disposable temporary output.
/// Both streams must remain exclusively controlled throughout preparation and publication.
///
/// # Errors
///
/// Returns an error for nonempty output, source changes, verification or codec failure,
/// cancellation, or I/O. Never modifies the original stream's contents.
pub fn prepare_compressed_artifact(
    original: &mut (impl Read + Seek),
    candidate: &mut (impl Read + Write + Seek),
    compression: ArtifactCompression,
    minimum_saved_bytes: u64,
    mut check_cancelled: impl FnMut() -> io::Result<()>,
) -> io::Result<ArtifactPreparation> {
    check_cancelled()?;
    let logical_bytes = original.seek(SeekFrom::End(0))?;
    original.rewind()?;
    encode_artifact(
        original,
        candidate,
        logical_bytes,
        compression,
        &mut check_cancelled,
    )?;
    let candidate_bytes = candidate.seek(SeekFrom::End(0))?;
    check_cancelled()?;
    let Some(saved_bytes) = logical_bytes
        .checked_sub(candidate_bytes)
        .filter(|saved| *saved > 0 && *saved >= minimum_saved_bytes)
    else {
        return Ok(ArtifactPreparation::InsufficientSavings {
            logical_bytes,
            candidate_bytes,
        });
    };
    original.rewind()?;
    let verified_bytes = verify_compressed_artifact(candidate, original, &mut check_cancelled)?;
    if verified_bytes != logical_bytes || original.seek(SeekFrom::End(0))? != logical_bytes {
        return Err(invalid());
    }
    check_cancelled()?;
    Ok(ArtifactPreparation::Ready {
        logical_bytes,
        candidate_bytes,
        saved_bytes,
    })
}

/// Verify an entire candidate container against its original bytes before maintenance publication.
///
/// This is explicit maintenance work, never a normal range-read operation. Memory is bounded to
/// one chunk pair and cancellation is checked between chunks. Every index entry must describe the
/// exact contiguous encoded layout; gaps, overlaps, and trailing bytes are rejected. Original bytes
/// are compared directly, not merely against candidate-supplied checksums. Returns the verified
/// logical byte count. The caller must keep both handles immutable throughout verification and
/// subsequent publication, and separately validate any authoritative original checksum.
///
/// # Errors
///
/// Returns an error on cancellation, malformed/unsupported metadata, integrity failure, differing
/// original bytes or length, or I/O. Success does not grant ownership or publish any files.
pub fn verify_compressed_artifact(
    candidate: &mut (impl Read + Seek),
    original: &mut impl Read,
    mut check_cancelled: impl FnMut() -> io::Result<()>,
) -> io::Result<u64> {
    check_cancelled()?;
    // The normal reader owns header compatibility and bounded decoder validation.
    let (total, _) = read_compressed_artifact_range(candidate, 0, 1)?;
    let count = total.div_ceil(u64::from(CHUNK));
    let mut expected_position = data_start(count)?;
    let mut original_chunk = vec![0; CHUNK as usize];
    for ordinal in 0..count {
        check_cancelled()?;
        candidate.seek(SeekFrom::Start(HEADER + ordinal * ENTRY))?;
        let mut entry = [0; ENTRY_BYTES];
        candidate.read_exact(&mut entry)?;
        if entry[48..] != entry_digest(ordinal, &entry[..48])
            || number(&entry[..8])? != expected_position
        {
            return Err(invalid());
        }
        expected_position = expected_position
            .checked_add(number(&entry[8..16])?)
            .ok_or_else(invalid)?;
        let (_, plain) =
            read_compressed_artifact_range(candidate, ordinal * u64::from(CHUNK), CHUNK)?;
        original.read_exact(&mut original_chunk[..plain.len()])?;
        if original_chunk[..plain.len()] != plain {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "compressed artifact differs from original",
            ));
        }
    }
    check_cancelled()?;
    if candidate.seek(SeekFrom::End(0))? != expected_position {
        return Err(invalid());
    }
    let mut extra = [0];
    if original.read(&mut extra)? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "artifact original length differs",
        ));
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn encode(bytes: &[u8], policy: ArtifactCompression) -> Vec<u8> {
        let mut destination = Cursor::new(Vec::new());
        encode_artifact(
            &mut &bytes[..],
            &mut destination,
            bytes.len() as u64,
            policy,
            || Ok(()),
        )
        .expect("encode");
        destination.into_inner()
    }

    #[test]
    fn preparation_counts_container_overhead_and_preserves_original() {
        for bytes in [vec![], b"tiny artifact".to_vec()] {
            let mut original = Cursor::new(bytes.clone());
            let mut candidate = Cursor::new(Vec::new());
            assert!(matches!(
                prepare_compressed_artifact(
                    &mut original,
                    &mut candidate,
                    ArtifactCompression::Light,
                    0,
                    || Ok(())
                )
                .expect("prepare"),
                ArtifactPreparation::InsufficientSavings { .. }
            ));
            assert_eq!(original.into_inner(), bytes);
        }
        let bytes = vec![42; CHUNK as usize * 2];
        let mut original = Cursor::new(bytes.clone());
        let mut candidate = Cursor::new(Vec::new());
        let ArtifactPreparation::Ready {
            logical_bytes,
            candidate_bytes,
            saved_bytes,
        } = prepare_compressed_artifact(
            &mut original,
            &mut candidate,
            ArtifactCompression::Deep,
            4096,
            || Ok(()),
        )
        .expect("prepare")
        else {
            panic!("compressible candidate")
        };
        assert_eq!(logical_bytes, bytes.len() as u64);
        assert_eq!(candidate_bytes, candidate.get_ref().len() as u64);
        assert_eq!(saved_bytes, logical_bytes - candidate_bytes);
        assert_eq!(original.get_ref(), &bytes);
        assert_eq!(
            read_compressed_artifact_range(&mut candidate, u64::from(CHUNK), 1)
                .expect("read")
                .1,
            [42]
        );
        let rejected = prepare_compressed_artifact(
            &mut original,
            &mut Cursor::new(Vec::new()),
            ArtifactCompression::Deep,
            logical_bytes,
            || Ok(()),
        )
        .expect("savings rejected");
        assert!(matches!(
            rejected,
            ArtifactPreparation::InsufficientSavings { .. }
        ));
    }

    #[test]
    fn preparation_cancellation_leaves_original_unchanged() {
        let bytes = vec![42; CHUNK as usize * 2];
        let mut original = Cursor::new(bytes.clone());
        let mut candidate = Cursor::new(Vec::new());
        let error = prepare_compressed_artifact(
            &mut original,
            &mut candidate,
            ArtifactCompression::Light,
            1,
            || Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled")),
        )
        .expect_err("cancelled");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(original.into_inner(), bytes);
        assert!(candidate.into_inner().is_empty());
    }

    #[test]
    fn maintenance_verification_compares_every_original_byte() {
        for bytes in [
            vec![],
            b"terminal output".to_vec(),
            vec![42; CHUNK as usize * 3 + 5],
        ] {
            for policy in [ArtifactCompression::Light, ArtifactCompression::Deep] {
                let encoded = encode(&bytes, policy);
                assert_eq!(
                    verify_compressed_artifact(
                        &mut Cursor::new(&encoded),
                        &mut bytes.as_slice(),
                        || Ok(())
                    )
                    .expect("verified"),
                    bytes.len() as u64
                );
                let mut longer = bytes.clone();
                longer.push(1);
                assert!(
                    verify_compressed_artifact(
                        &mut Cursor::new(&encoded),
                        &mut longer.as_slice(),
                        || Ok(())
                    )
                    .is_err()
                );
                if !bytes.is_empty() {
                    let mut changed = bytes.clone();
                    *changed.last_mut().expect("last") ^= 1;
                    assert!(
                        verify_compressed_artifact(
                            &mut Cursor::new(&encoded),
                            &mut changed.as_slice(),
                            || Ok(())
                        )
                        .is_err()
                    );
                    assert!(
                        verify_compressed_artifact(
                            &mut Cursor::new(&encoded),
                            &mut &bytes[..bytes.len() - 1],
                            || Ok(())
                        )
                        .is_err()
                    );
                }
            }
        }
    }

    #[test]
    fn maintenance_verification_rejects_trailing_data_and_overlapping_chunks() {
        let bytes = vec![42; CHUNK as usize * 2];
        let encoded = encode(&bytes, ArtifactCompression::Light);
        let mut trailing = encoded.clone();
        trailing.push(0);
        assert!(
            verify_compressed_artifact(
                &mut Cursor::new(trailing),
                &mut bytes.as_slice(),
                || Ok(())
            )
            .is_err()
        );
        let mut overlap = encoded;
        let first = overlap[HEADER_BYTES..HEADER_BYTES + 48].to_vec();
        let second = HEADER_BYTES + ENTRY_BYTES;
        overlap[second..second + 48].copy_from_slice(&first);
        let checksum = entry_digest(1, &first);
        overlap[second + 48..second + ENTRY_BYTES].copy_from_slice(&checksum);
        // Individual range bytes remain valid, but this is not the canonical contiguous layout.
        assert!(
            read_compressed_artifact_range(&mut Cursor::new(&overlap), u64::from(CHUNK), 1).is_ok()
        );
        assert!(
            verify_compressed_artifact(&mut Cursor::new(overlap), &mut bytes.as_slice(), || Ok(()))
                .is_err()
        );
    }

    #[test]
    fn maintenance_verification_is_cancellable_between_chunks() {
        let bytes = vec![42; CHUNK as usize * 3];
        let encoded = encode(&bytes, ArtifactCompression::Light);
        let mut calls = 0;
        let mut original = Cursor::new(&bytes);
        let error = verify_compressed_artifact(&mut Cursor::new(encoded), &mut original, || {
            calls += 1;
            if calls == 3 {
                Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"))
            } else {
                Ok(())
            }
        })
        .expect_err("cancelled");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(original.position(), u64::from(CHUNK));
    }

    #[test]
    fn exact_ranges_across_chunks_and_eof_for_both_tiers() {
        let bytes: Vec<_> = (0..CHUNK as usize * 3 + 17)
            .map(|i| u8::try_from(i % 251).expect("bounded byte"))
            .collect();
        for tier in [ArtifactCompression::Light, ArtifactCompression::Deep] {
            let mut encoded = Cursor::new(encode(&bytes, tier));
            for (offset, length) in [
                (0, 1),
                (u64::from(CHUNK) - 7, CHUNK + 19),
                (bytes.len() as u64 - 3, 100),
                (bytes.len() as u64, 1),
            ] {
                let (total, actual) =
                    read_compressed_artifact_range(&mut encoded, offset, length).expect("range");
                assert_eq!(total, bytes.len() as u64);
                assert_eq!(
                    actual,
                    bytes[usize::try_from(offset).expect("fixture offset")
                        ..bytes.len().min(
                            usize::try_from(offset).expect("fixture offset") + length as usize
                        )]
                );
            }
            assert!(
                read_compressed_artifact_range(&mut encoded, bytes.len() as u64 + 1, 1).is_err()
            );
            assert!(read_compressed_artifact_range(&mut encoded, 0, 0).is_err());
            assert!(
                read_compressed_artifact_range(
                    &mut encoded,
                    0,
                    MAX_SESSION_ARTIFACT_RANGE_BYTES + 1
                )
                .is_err()
            );
        }
        assert_eq!(
            read_compressed_artifact_range(
                &mut Cursor::new(encode(b"", ArtifactCompression::Light)),
                0,
                1
            )
            .expect("empty"),
            (0, vec![])
        );
    }

    #[test]
    fn corruption_future_versions_and_truncation_fail_closed() {
        let original = encode(b"hello artifact", ArtifactCompression::Light);
        for at in [0, 16, 32, 64, 80, 112, original.len() - 1] {
            let mut damaged = original.clone();
            damaged[at] ^= 1;
            assert!(
                read_compressed_artifact_range(&mut Cursor::new(damaged), 0, 100).is_err(),
                "byte {at}"
            );
        }
        for length in [0, 8, 63, 100, original.len() - 1] {
            assert!(
                read_compressed_artifact_range(&mut Cursor::new(&original[..length]), 0, 100)
                    .is_err()
            );
        }
        let mut future = original;
        future[8..12].copy_from_slice(&2_u32.to_le_bytes());
        let checksum = digest(&future[..32]);
        future[32..64].copy_from_slice(&checksum);
        assert_eq!(
            read_compressed_artifact_range(&mut Cursor::new(future), 0, 1)
                .expect_err("future")
                .kind(),
            io::ErrorKind::Unsupported
        );
        assert!(read_compressed_artifact_range(&mut Cursor::new(vec![0; 200]), 0, 1).is_err());
    }

    #[test]
    fn writer_rejects_length_changes_nonempty_output_and_cancellation() {
        for claimed in [2, 4] {
            assert!(
                encode_artifact(
                    &mut &b"abc"[..],
                    &mut Cursor::new(Vec::new()),
                    claimed,
                    ArtifactCompression::Light,
                    || Ok(())
                )
                .is_err()
            );
        }
        let mut existing = Cursor::new(b"existing".to_vec());
        assert!(
            encode_artifact(
                &mut &b"abc"[..],
                &mut existing,
                3,
                ArtifactCompression::Light,
                || Ok(())
            )
            .is_err()
        );
        assert_eq!(existing.into_inner(), b"existing");
        let mut output = Cursor::new(Vec::new());
        let mut calls = 0;
        let error = encode_artifact(
            &mut &b"abc"[..],
            &mut output,
            3,
            ArtifactCompression::Deep,
            || {
                calls += 1;
                if calls == 3 {
                    Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"))
                } else {
                    Ok(())
                }
            },
        )
        .expect_err("cancelled before header commit");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(read_compressed_artifact_range(&mut output, 0, 1).is_err());
    }

    #[test]
    fn rejects_forged_expansion_and_index_overflow() {
        let mut encoded = encode(&vec![42; CHUNK as usize], ArtifactCompression::Light);
        // Valid checksums do not grant permission to exceed the logical expansion size.
        encoded[16..24].copy_from_slice(&1_u64.to_le_bytes());
        let checksum = digest(&encoded[..32]);
        encoded[32..64].copy_from_slice(&checksum);
        assert!(read_compressed_artifact_range(&mut Cursor::new(&encoded), 0, 1).is_err());
        encoded[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
        encoded[24..32].copy_from_slice(&u64::MAX.to_le_bytes());
        let checksum = digest(&encoded[..32]);
        encoded[32..64].copy_from_slice(&checksum);
        assert!(read_compressed_artifact_range(&mut Cursor::new(&encoded), 0, 1).is_err());
    }

    struct Metered {
        cursor: Cursor<Vec<u8>>,
        bytes_read: usize,
    }

    impl Read for Metered {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let count = self.cursor.read(buffer)?;
            self.bytes_read += count;
            Ok(count)
        }
    }

    impl Seek for Metered {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.cursor.seek(position)
        }
    }

    #[test]
    fn range_io_is_independent_of_artifact_length() {
        let bytes = vec![42; CHUNK as usize * 40];
        let encoded = encode(&bytes, ArtifactCompression::Light);
        let mut source = Metered {
            cursor: Cursor::new(encoded),
            bytes_read: 0,
        };
        assert_eq!(
            read_compressed_artifact_range(&mut source, u64::from(CHUNK) * 39, 1)
                .expect("range")
                .1,
            [42]
        );
        assert!(
            source.bytes_read
                <= HEADER_BYTES
                    + ENTRY_BYTES
                    + usize::try_from(MAX_COMPRESSED).expect("bounded chunk")
        );
    }

    #[test]
    fn small_reads_do_not_touch_unrelated_chunks() {
        let mut encoded = encode(&vec![42; CHUNK as usize * 3], ArtifactCompression::Light);
        // Destroy the first chunk index. A read in the third chunk must still be bounded and work.
        encoded[HEADER_BYTES] ^= 1;
        let mut source = Cursor::new(encoded);
        assert_eq!(
            read_compressed_artifact_range(&mut source, u64::from(CHUNK) * 2, 1)
                .expect("third chunk")
                .1,
            [42]
        );
        assert!(read_compressed_artifact_range(&mut source, 0, 1).is_err());
    }
}
