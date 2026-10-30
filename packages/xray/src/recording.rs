//! Explicit, sensitive diagnostic recording; never canonical history or durable replay.
//!
//! Call sinks on a consumer/blocking IO task, never on an instrumentation producer.
//! Creating a sink is the caller's explicit persistence decision after authorization.
//! A manifest left `recording` after process death is incomplete. Only successful
//! `finalize` certifies local flush; it does not certify observed execution success.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

use bcode_xray_models::{
    XRAY_SCHEMA_VERSION, XrayEnvelope, XrayFinalization, XrayGap, XrayPayloadRef,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Independently evolving on-disk bundle contract. Unknown versions must be rejected.
pub const RECORDING_SCHEMA_VERSION: u16 = 1;

/// Consumer-side recording interface, replaceable by non-filesystem implementations.
/// Payload references must be registered explicitly, not discovered by guessing JSON.
pub trait RecordingSink: Send {
    /// Persist one ordered envelope.
    ///
    /// # Errors
    /// Rejects incompatible/foreign/unordered data, exhausted bounds or failed IO.
    fn append(&mut self, envelope: &XrayEnvelope) -> Result<(), RecordingError>;
    /// Register an observed payload reference, including one unavailable for export.
    ///
    /// # Errors
    /// Rejects foreign, conflicting, unsupported or oversized references.
    fn expect_payload(&mut self, reference: &XrayPayloadRef) -> Result<(), RecordingError>;
    /// Persist a complete bounded payload; partial ranges must be assembled by caller.
    ///
    /// # Errors
    /// Rejects length/digest mismatches, invalid references, bounds or failed IO.
    fn store_payload(
        &mut self,
        reference: &XrayPayloadRef,
        bytes: &[u8],
    ) -> Result<(), RecordingError>;
    /// Flush after receiving the matching terminal control envelope.
    ///
    /// # Errors
    /// Rejects absent/conflicting finalization and reports local persistence failure.
    fn finalize(
        &mut self,
        boundary: &XrayFinalization,
    ) -> Result<RecordingManifest, RecordingError>;
    /// Explicitly close an unfinished recording without claiming finalization.
    ///
    /// # Errors
    /// Reports local persistence failure or an already closed recording.
    fn interrupt(&mut self) -> Result<RecordingManifest, RecordingError>;
}

/// Resource policy for this recording, not a total workflow/session limit.
#[derive(Clone, Debug)]
pub struct RecordingOptions {
    pub max_event_bytes: usize,
    pub max_payload_bytes: usize,
    pub max_payloads: usize,
    pub max_bundle_bytes: u64,
    /// Upper bound for the serialized manifest, including reference metadata.
    pub max_manifest_bytes: usize,
}

impl Default for RecordingOptions {
    fn default() -> Self {
        Self {
            max_event_bytes: 1024 * 1024,
            max_payload_bytes: 1024 * 1024,
            max_payloads: 4096,
            max_bundle_bytes: 1024 * 1024 * 1024,
            max_manifest_bytes: 4 * 1024 * 1024,
        }
    }
}

/// Local persistence lifecycle. Missing/corrupt manifests also mean incomplete.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingState {
    Recording,
    Finalized,
    Interrupted,
    Failed,
}

/// Only recorder-generated relative names are written; payload IDs are not paths.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RecordedPayload {
    pub reference: XrayPayloadRef,
    /// None means explicitly expected but not successfully retained locally.
    pub relative_path: Option<String>,
}

/// Integrity coordinates for the exact JSONL bytes (including newlines).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RecordedEvents {
    pub relative_path: String,
    pub byte_len: u64,
    pub sha256: String,
    pub envelope_count: u64,
    pub last_sequence: u64,
}

/// Sensitive bundle manifest, published atomically after flushing its data files.
/// Digests detect accidental corruption; they are not authentication or redaction.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingManifest {
    pub schema_version: u16,
    pub envelope_schema_version: u16,
    pub observation_id: String,
    pub session_id: String,
    pub state: RecordingState,
    /// All content is potentially sensitive, even when individual events are not.
    pub sensitive: bool,
    pub events: RecordedEvents,
    pub payloads: BTreeMap<String, RecordedPayload>,
    /// Includes explicit engine gaps and unannounced sequence discontinuities.
    pub missing_sequences: u64,
    /// Domain `*_omitted` controls, retained without claiming full evidence.
    pub omitted_events: u64,
    pub finalization: Option<XrayFinalization>,
    /// True only for a finalized, gap-free recording with all registered payloads.
    /// Does not mean full-session coverage or canonical execution completeness.
    pub evidence_complete: bool,
    /// Fixed diagnostic code only; never an OS error, path, or payload text.
    pub failure_code: Option<String>,
}

/// Secret-safe failure categories; arbitrary OS errors are deliberately not displayed.
#[derive(Debug, thiserror::Error)]
pub enum RecordingError {
    #[error("recording filesystem operation failed ({0:?})")]
    Io(io::ErrorKind),
    #[error("unsupported recording platform")]
    UnsupportedPlatform,
    #[error("invalid recording destination or options")]
    InvalidOptions,
    #[error("incompatible or foreign recording input")]
    InvalidInput,
    #[error("recording input order or finalization is invalid")]
    InvalidOrder,
    #[error("recording payload integrity mismatch")]
    Integrity,
    #[error("recording resource bound exceeded")]
    Limit,
    #[error("recording is closed or failed")]
    Closed,
}

impl From<io::Error> for RecordingError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

/// JSONL plus content-addressed payload bundle in a newly created private directory.
///
/// Destination must be absolute and absent, with existing non-symlink ancestors.
/// Unix descriptor-relative no-follow operations pin the bundle against path swaps.
/// Other platforms fail closed. Never automatically resumes or repairs a bundle.
pub struct JsonlBundleRecorder {
    directory: File,
    payload_directory: File,
    events: File,
    options: RecordingOptions,
    manifest: RecordingManifest,
    event_digest: Sha256,
    next_sequence: u64,
    terminal: Option<XrayFinalization>,
    payload_bytes: u64,
}

impl JsonlBundleRecorder {
    /// Create a fresh bundle. Never overwrites an existing destination.
    ///
    /// # Errors
    /// Rejects non-absolute/traversing/symlink paths, unsupported platforms, invalid
    /// options and IO failures. An incomplete private directory may remain on failure.
    pub fn create(
        destination: &Path,
        observation_id: &str,
        session_id: &str,
        options: RecordingOptions,
    ) -> Result<Self, RecordingError> {
        if observation_id.is_empty()
            || session_id.is_empty()
            || observation_id.len() > 4096
            || session_id.len() > 4096
            || options.max_event_bytes == 0
            || options.max_payload_bytes == 0
            || options.max_payloads == 0
            || options.max_bundle_bytes == 0
            || options.max_manifest_bytes == 0
        {
            return Err(RecordingError::InvalidOptions);
        }
        let directory = confined::create_bundle(destination)?;
        let payload_directory = confined::mkdir(&directory, "payloads")?;
        let events = confined::create(&directory, "events.jsonl")?;
        let mut recorder = Self {
            directory,
            payload_directory,
            events,
            options,
            manifest: RecordingManifest {
                schema_version: RECORDING_SCHEMA_VERSION,
                envelope_schema_version: XRAY_SCHEMA_VERSION,
                observation_id: observation_id.to_owned(),
                session_id: session_id.to_owned(),
                state: RecordingState::Recording,
                sensitive: true,
                events: RecordedEvents {
                    relative_path: "events.jsonl".into(),
                    byte_len: 0,
                    sha256: digest(&[]),
                    envelope_count: 0,
                    last_sequence: 0,
                },
                payloads: BTreeMap::new(),
                missing_sequences: 0,
                omitted_events: 0,
                finalization: None,
                evidence_complete: false,
                failure_code: None,
            },
            event_digest: Sha256::new(),
            next_sequence: 1,
            terminal: None,
            payload_bytes: 0,
        };
        recorder.publish_manifest()?;
        Ok(recorder)
    }

    /// Current in-memory state; only a successful close certifies local persistence.
    #[must_use]
    pub const fn manifest(&self) -> &RecordingManifest {
        &self.manifest
    }

    fn active(&self) -> Result<(), RecordingError> {
        if self.manifest.state != RecordingState::Recording {
            return Err(RecordingError::Closed);
        }
        Ok(())
    }

    fn publish_manifest(&mut self) -> Result<(), RecordingError> {
        self.manifest.events.sha256 = hex::encode(self.event_digest.clone().finalize());
        let bytes = bounded_json(&self.manifest, self.options.max_manifest_bytes)?;
        // A failed prior attempt is not reused. The entire recorder is poisoned on IO.
        let mut temporary = confined::create(&self.directory, "manifest.pending")?;
        temporary.write_all(&bytes)?;
        temporary.sync_all()?;
        confined::replace(&self.directory, "manifest.pending", "manifest.json")?;
        self.directory.sync_all()?;
        Ok(())
    }

    fn fail(&mut self, error: RecordingError) -> RecordingError {
        self.manifest.state = RecordingState::Failed;
        self.manifest.evidence_complete = false;
        self.manifest.failure_code = Some(
            match error {
                RecordingError::Limit => "recording_limit",
                _ => "recording_io_failure",
            }
            .into(),
        );
        // Best effort only. A previously published recording-state manifest is still
        // explicitly incomplete if disk-full or process death prevents this update.
        let _ = self.publish_manifest();
        error
    }

    fn check_reference(&self, reference: &XrayPayloadRef) -> Result<(), RecordingError> {
        if reference.schema_version != XRAY_SCHEMA_VERSION
            || reference.session_id != self.manifest.session_id
            || reference.payload_id.is_empty()
            || reference.payload_id.len() > 4096
            || reference.content_type.is_empty()
            || reference.content_type.len() > 1024
            || reference.sha256.len() != 64
            || !reference
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(RecordingError::InvalidInput);
        }
        if reference.byte_len > self.options.max_payload_bytes as u64 {
            return Err(RecordingError::Limit);
        }
        Ok(())
    }
}

impl RecordingSink for JsonlBundleRecorder {
    fn append(&mut self, envelope: &XrayEnvelope) -> Result<(), RecordingError> {
        self.active()?;
        if envelope.schema_version != XRAY_SCHEMA_VERSION
            || envelope.observation_id != self.manifest.observation_id
            || envelope.event.correlation.session_id != self.manifest.session_id
        {
            return Err(RecordingError::InvalidInput);
        }
        if self.terminal.is_some()
            || envelope.sequence < self.next_sequence
            || envelope.sequence == u64::MAX
        {
            return Err(RecordingError::InvalidOrder);
        }
        let control = envelope.event.domain == "xray";
        if self.next_sequence == 1
            && (envelope.sequence != 1 || !control || envelope.event.name != "ready")
        {
            return Err(RecordingError::InvalidOrder);
        }
        let mut next = envelope.sequence + 1;
        let mut missing = envelope.sequence - self.next_sequence;
        let mut terminal = None;
        if control && envelope.event.name == "gap" {
            let gap: XrayGap = serde_json::from_value(envelope.event.data.clone())
                .map_err(|_| RecordingError::InvalidInput)?;
            let first_missing = gap.first_sequence;
            if first_missing != envelope.sequence
                || gap.last_sequence < first_missing
                || gap.last_sequence == u64::MAX
            {
                return Err(RecordingError::InvalidOrder);
            }
            missing = missing.saturating_add(gap.last_sequence - gap.first_sequence + 1);
            next = gap.last_sequence + 1;
        } else if control && envelope.event.name == "finalized" {
            let boundary: XrayFinalization = serde_json::from_value(envelope.event.data.clone())
                .map_err(|_| RecordingError::InvalidInput)?;
            if !finalization_matches_envelope(&boundary, envelope) {
                return Err(RecordingError::InvalidInput);
            }
            terminal = Some(boundary);
        }
        let mut bytes = bounded_json(envelope, self.options.max_event_bytes)?;
        bytes.push(b'\n');
        let length = bytes.len() as u64;
        if length
            > self.options.max_bundle_bytes.saturating_sub(
                self.manifest
                    .events
                    .byte_len
                    .saturating_add(self.payload_bytes),
            )
        {
            return Err(self.fail(RecordingError::Limit));
        }
        if let Err(error) = self
            .events
            .write_all(&bytes)
            .and_then(|()| self.events.sync_data())
        {
            return Err(self.fail(error.into()));
        }
        self.event_digest.update(&bytes);
        self.manifest.events.byte_len += length;
        self.manifest.events.envelope_count += 1;
        self.manifest.events.last_sequence = envelope.sequence;
        self.manifest.missing_sequences = self.manifest.missing_sequences.saturating_add(missing);
        if envelope.event.name.ends_with("_omitted") {
            self.manifest.omitted_events = self.manifest.omitted_events.saturating_add(1);
        }
        self.next_sequence = next;
        self.terminal = terminal;
        Ok(())
    }

    fn expect_payload(&mut self, reference: &XrayPayloadRef) -> Result<(), RecordingError> {
        self.active()?;
        self.check_reference(reference)?;
        if let Some(existing) = self.manifest.payloads.get(&reference.payload_id) {
            return if existing.reference == *reference {
                Ok(())
            } else {
                Err(RecordingError::Integrity)
            };
        }
        if self.manifest.payloads.len() >= self.options.max_payloads {
            return Err(self.fail(RecordingError::Limit));
        }
        self.manifest.payloads.insert(
            reference.payload_id.clone(),
            RecordedPayload {
                reference: reference.clone(),
                relative_path: None,
            },
        );
        if let Err(error) = self.publish_manifest() {
            return Err(self.fail(error));
        }
        Ok(())
    }

    fn store_payload(
        &mut self,
        reference: &XrayPayloadRef,
        bytes: &[u8],
    ) -> Result<(), RecordingError> {
        self.active()?;
        self.check_reference(reference)?;
        if bytes.len() as u64 != reference.byte_len || digest(bytes) != reference.sha256 {
            return Err(RecordingError::Integrity);
        }
        self.expect_payload(reference)?;
        if self
            .manifest
            .payloads
            .get(&reference.payload_id)
            .is_some_and(|entry| entry.relative_path.is_some())
        {
            return Ok(());
        }
        if reference.byte_len
            > self.options.max_bundle_bytes.saturating_sub(
                self.payload_bytes
                    .saturating_add(self.manifest.events.byte_len),
            )
        {
            return Err(self.fail(RecordingError::Limit));
        }
        // Hash the identity as well as content: arbitrary caller IDs never become paths.
        let name = format!(
            "{}-{}.bin",
            digest(reference.payload_id.as_bytes()),
            reference.sha256
        );
        let result = (|| -> Result<(), RecordingError> {
            let mut file = confined::create(&self.payload_directory, &name)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            self.payload_directory.sync_all()?;
            Ok(())
        })();
        if let Err(error) = result {
            return Err(self.fail(error));
        }
        self.payload_bytes += reference.byte_len;
        if let Some(entry) = self.manifest.payloads.get_mut(&reference.payload_id) {
            entry.relative_path = Some(format!("payloads/{name}"));
        }
        if let Err(error) = self.publish_manifest() {
            return Err(self.fail(error));
        }
        Ok(())
    }

    fn finalize(
        &mut self,
        boundary: &XrayFinalization,
    ) -> Result<RecordingManifest, RecordingError> {
        if self.manifest.state == RecordingState::Finalized
            && self.manifest.finalization.as_ref() == Some(boundary)
        {
            return Ok(self.manifest.clone());
        }
        self.active()?;
        if self.terminal.as_ref() != Some(boundary) {
            return Err(RecordingError::InvalidOrder);
        }
        if let Err(error) = self.events.sync_all() {
            return Err(self.fail(error.into()));
        }
        self.manifest.finalization = Some(boundary.clone());
        self.manifest.state = RecordingState::Finalized;
        self.manifest.evidence_complete = self.manifest.missing_sequences == 0
            && self.manifest.omitted_events == 0
            && boundary.dropped_events == 0
            && boundary.hub_contention_drops == 0
            && boundary.evicted_payloads == 0
            && self
                .manifest
                .payloads
                .values()
                .all(|entry| entry.relative_path.is_some());
        if let Err(error) = self.publish_manifest() {
            return Err(self.fail(error));
        }
        Ok(self.manifest.clone())
    }

    fn interrupt(&mut self) -> Result<RecordingManifest, RecordingError> {
        self.active()?;
        if let Err(error) = self.events.sync_all() {
            return Err(self.fail(error.into()));
        }
        self.manifest.state = RecordingState::Interrupted;
        self.manifest.evidence_complete = false;
        if let Err(error) = self.publish_manifest() {
            return Err(self.fail(error));
        }
        Ok(self.manifest.clone())
    }
}

impl Drop for JsonlBundleRecorder {
    fn drop(&mut self) {
        if self.manifest.state == RecordingState::Recording {
            let _ = self.interrupt();
        }
    }
}

fn finalization_matches_envelope(boundary: &XrayFinalization, envelope: &XrayEnvelope) -> bool {
    let XrayFinalization {
        observation_id,
        final_sequence,
        ..
    } = boundary;
    (observation_id, *final_sequence) == (&envelope.observation_id, envelope.sequence)
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Bound serialization while writing, rather than allocating an unbounded JSON copy.
fn bounded_json(value: &impl Serialize, limit: usize) -> Result<Vec<u8>, RecordingError> {
    struct Limited {
        bytes: Vec<u8>,
        limit: usize,
        exceeded: bool,
    }
    impl Write for Limited {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                self.exceeded = true;
                return Err(io::Error::other("recording bound"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Limited {
        bytes: Vec::new(),
        limit,
        exceeded: false,
    };
    if serde_json::to_writer(&mut writer, value).is_err() {
        return Err(if writer.exceeded {
            RecordingError::Limit
        } else {
            RecordingError::InvalidInput
        });
    }
    Ok(writer.bytes)
}

#[cfg(unix)]
mod confined {
    use super::{File, Path, RecordingError};
    use rustix::fs::{Mode, OFlags, mkdirat, open, openat, renameat};
    use std::path::Component;

    fn map(error: rustix::io::Errno) -> RecordingError {
        RecordingError::Io(std::io::Error::from(error).kind())
    }

    pub(super) fn create_bundle(path: &Path) -> Result<File, RecordingError> {
        if !path.is_absolute() {
            return Err(RecordingError::InvalidOptions);
        }
        let mut names = Vec::new();
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => names.push(name),
                _ => return Err(RecordingError::InvalidOptions),
            }
        }
        let (name, ancestors) = names.split_last().ok_or(RecordingError::InvalidOptions)?;
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mut parent = File::from(open("/", flags, Mode::empty()).map_err(map)?);
        for ancestor in ancestors {
            parent = File::from(openat(&parent, *ancestor, flags, Mode::empty()).map_err(map)?);
        }
        mkdirat(&parent, *name, Mode::RUSR | Mode::WUSR | Mode::XUSR).map_err(map)?;
        let directory = File::from(openat(&parent, *name, flags, Mode::empty()).map_err(map)?);
        parent.sync_all()?;
        Ok(directory)
    }

    pub(super) fn mkdir(parent: &File, name: &str) -> Result<File, RecordingError> {
        mkdirat(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR).map_err(map)?;
        let directory = openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(map)?;
        parent.sync_all()?;
        Ok(File::from(directory))
    }

    pub(super) fn create(parent: &File, name: &str) -> Result<File, RecordingError> {
        let fd = openat(
            parent,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(map)?;
        Ok(File::from(fd))
    }

    pub(super) fn replace(parent: &File, from: &str, to: &str) -> Result<(), RecordingError> {
        renameat(parent, from, parent, to).map_err(map)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use bcode_xray_models::{XrayCorrelation, XrayEvent};
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tempfile::TempDir;

    fn setup() -> (TempDir, std::path::PathBuf, JsonlBundleRecorder) {
        let temporary = tempfile::tempdir().unwrap();
        // macOS temp roots can have a symlink ancestor; canonicalize trusted test input.
        let path = temporary.path().canonicalize().unwrap().join("bundle");
        let recorder =
            JsonlBundleRecorder::create(&path, "observer", "session", RecordingOptions::default())
                .unwrap();
        (temporary, path, recorder)
    }

    fn envelope(sequence: u64, name: &str, data: serde_json::Value) -> XrayEnvelope {
        XrayEnvelope {
            schema_version: XRAY_SCHEMA_VERSION,
            observation_id: "observer".into(),
            sequence,
            timestamp_unix_ms: 0,
            event: XrayEvent {
                domain: "xray".into(),
                name: name.into(),
                data,
                sensitive: false,
                correlation: XrayCorrelation {
                    session_id: "session".into(),
                    ..XrayCorrelation::default()
                },
            },
        }
    }

    fn ready(recorder: &mut JsonlBundleRecorder) {
        recorder
            .append(&envelope(1, "ready", serde_json::json!({})))
            .unwrap();
    }

    fn boundary(sequence: u64) -> XrayFinalization {
        XrayFinalization {
            observation_id: "observer".into(),
            final_sequence: sequence,
            dropped_events: 0,
            hub_contention_drops: 0,
            evicted_payloads: 0,
        }
    }

    fn finish(
        recorder: &mut JsonlBundleRecorder,
        boundary: &XrayFinalization,
    ) -> RecordingManifest {
        recorder
            .append(&envelope(
                boundary.final_sequence,
                "finalized",
                serde_json::to_value(boundary).unwrap(),
            ))
            .unwrap();
        recorder.finalize(boundary).unwrap()
    }

    fn reference(bytes: &[u8]) -> XrayPayloadRef {
        XrayPayloadRef {
            schema_version: XRAY_SCHEMA_VERSION,
            payload_id: "../../untrusted".into(),
            session_id: "session".into(),
            content_type: "application/octet-stream".into(),
            byte_len: bytes.len() as u64,
            sha256: digest(bytes),
        }
    }

    #[test]
    fn terminal_boundary_requires_matching_observation_and_sequence() {
        let end = boundary(2);
        let mut terminal = envelope(2, "finalized", serde_json::to_value(&end).unwrap());
        assert!(finalization_matches_envelope(&end, &terminal));
        terminal.observation_id = "foreign-observer".into();
        assert!(!finalization_matches_envelope(&end, &terminal));
        terminal.observation_id = end.observation_id.clone();
        terminal.sequence = 3;
        assert!(!finalization_matches_envelope(&end, &terminal));
    }

    #[test]
    fn finalized_bundle_preserves_exact_envelopes_and_payload_integrity() {
        let (_temporary, path, mut recorder) = setup();
        ready(&mut recorder);
        let data = b"sensitive\0bytes\n";
        let payload = reference(data);
        recorder.store_payload(&payload, data).unwrap();
        recorder.store_payload(&payload, data).unwrap(); // identical retry is safe
        let manifest = finish(&mut recorder, &boundary(2));
        assert_eq!(manifest.state, RecordingState::Finalized);
        assert!(manifest.evidence_complete);
        let events = std::fs::read(path.join("events.jsonl")).unwrap();
        assert_eq!(events.len() as u64, manifest.events.byte_len);
        assert_eq!(digest(&events), manifest.events.sha256);
        let envelopes: Vec<XrayEnvelope> = events
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(envelopes.len(), 2);
        assert_eq!(envelopes[0], envelope(1, "ready", serde_json::json!({})));
        let relative = manifest.payloads[&payload.payload_id]
            .relative_path
            .as_ref()
            .unwrap();
        assert_eq!(Path::new(relative).components().count(), 2);
        assert_eq!(std::fs::read(path.join(relative)).unwrap(), data);
        let persisted: RecordingManifest =
            serde_json::from_slice(&std::fs::read(path.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(persisted, manifest);
        assert_eq!(recorder.finalize(&boundary(2)).unwrap(), manifest);
        assert!(matches!(
            recorder.append(&envelope(3, "late", serde_json::Value::Null)),
            Err(RecordingError::Closed)
        ));
    }

    #[test]
    fn explicit_and_transport_gaps_remain_incomplete_after_finalization() {
        let (_temporary, _path, mut recorder) = setup();
        ready(&mut recorder);
        recorder
            .append(&envelope(
                2,
                "gap",
                serde_json::json!({
                    "first_sequence": 2, "last_sequence": 5, "reason": "queue_full"
                }),
            ))
            .unwrap();
        // Missing 6 and 7 represent a transport loss, independent of engine gaps.
        let manifest = finish(&mut recorder, &boundary(8));
        assert_eq!(manifest.missing_sequences, 6);
        assert!(!manifest.evidence_complete);
        assert_eq!(manifest.state, RecordingState::Finalized);
    }

    #[test]
    fn omission_control_preserves_content_without_claiming_complete_evidence() {
        let (_temporary, _path, mut recorder) = setup();
        ready(&mut recorder);
        let mut omission = envelope(
            2,
            "capture_omitted",
            serde_json::json!({
                "reason": "payload_too_large"
            }),
        );
        omission.event.domain = "context".into();
        recorder.append(&omission).unwrap();
        let manifest = finish(&mut recorder, &boundary(3));
        assert_eq!(manifest.omitted_events, 1);
        assert!(!manifest.evidence_complete);
    }

    #[test]
    fn missing_payload_and_contention_are_not_complete_evidence() {
        let (_temporary, _path, mut recorder) = setup();
        ready(&mut recorder);
        recorder.expect_payload(&reference(b"not fetched")).unwrap();
        let mut end = boundary(2);
        end.hub_contention_drops = 1;
        let manifest = finish(&mut recorder, &end);
        assert!(!manifest.evidence_complete);
        assert!(
            manifest
                .payloads
                .values()
                .all(|p| p.relative_path.is_none())
        );
    }

    #[test]
    fn finalization_receipt_without_delivered_terminal_envelope_is_insufficient() {
        let (_temporary, path, mut recorder) = setup();
        ready(&mut recorder);
        assert!(matches!(
            recorder.finalize(&boundary(2)),
            Err(RecordingError::InvalidOrder)
        ));
        drop(recorder);
        let manifest: RecordingManifest =
            serde_json::from_slice(&std::fs::read(path.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest.state, RecordingState::Interrupted);
        assert!(!manifest.evidence_complete);
        assert!(manifest.finalization.is_none());
    }

    #[test]
    fn disk_write_failure_poisoning_preserves_incomplete_manifest() {
        let (_temporary, path, mut recorder) = setup();
        ready(&mut recorder);
        // A read-only descriptor deterministically injects a write failure without
        // relying on disk quotas or root-sensitive permission checks.
        recorder.events = File::open(path.join("events.jsonl")).unwrap();
        assert!(matches!(
            recorder.append(&envelope(2, "event", serde_json::Value::Null)),
            Err(RecordingError::Io(_))
        ));
        assert_eq!(recorder.manifest().state, RecordingState::Failed);
        assert!(matches!(
            recorder.finalize(&boundary(3)),
            Err(RecordingError::Closed)
        ));
        let manifest: RecordingManifest =
            serde_json::from_slice(&std::fs::read(path.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest.state, RecordingState::Failed);
        assert!(!manifest.evidence_complete);
    }

    #[test]
    fn final_manifest_publication_failure_cannot_certify_local_completion() {
        let (_temporary, path, mut recorder) = setup();
        ready(&mut recorder);
        let end = boundary(2);
        recorder
            .append(&envelope(
                2,
                "finalized",
                serde_json::to_value(&end).unwrap(),
            ))
            .unwrap();
        // A pending-path collision deterministically models an unavailable atomic
        // publication destination without relying on filesystem quotas.
        std::fs::create_dir(path.join("manifest.pending")).unwrap();
        assert!(matches!(
            recorder.finalize(&end),
            Err(RecordingError::Io(_))
        ));
        assert_eq!(recorder.manifest().state, RecordingState::Failed);
        assert!(!recorder.manifest().evidence_complete);
        let persisted: RecordingManifest =
            serde_json::from_slice(&std::fs::read(path.join("manifest.json")).unwrap()).unwrap();
        // Even when failure reporting itself cannot publish, the previous atomic
        // manifest remains explicitly unfinished, not falsely finalized.
        assert_eq!(persisted.state, RecordingState::Recording);
        assert!(!persisted.evidence_complete);
    }

    #[test]
    fn fresh_manifest_survives_abrupt_loss_as_explicitly_unfinished() {
        let (_temporary, path, mut recorder) = setup();
        ready(&mut recorder);
        // Inspect the last atomic manifest while the writer remains alive: this is
        // exactly the manifest process death before a close would leave behind.
        let manifest: RecordingManifest =
            serde_json::from_slice(&std::fs::read(path.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest.state, RecordingState::Recording);
        assert!(!manifest.evidence_complete);
    }

    #[test]
    fn invalid_identity_digest_order_and_conflicting_reference_are_rejected() {
        let (_temporary, _path, mut recorder) = setup();
        let mut foreign = envelope(1, "ready", serde_json::Value::Null);
        foreign.event.correlation.session_id = "foreign".into();
        assert!(matches!(
            recorder.append(&foreign),
            Err(RecordingError::InvalidInput)
        ));
        ready(&mut recorder);
        assert!(matches!(
            recorder.append(&envelope(1, "ready", serde_json::Value::Null)),
            Err(RecordingError::InvalidOrder)
        ));
        let payload = reference(b"abc");
        assert!(matches!(
            recorder.store_payload(&payload, b"bad"),
            Err(RecordingError::Integrity)
        ));
        recorder.expect_payload(&payload).unwrap();
        let mut conflicting = payload.clone();
        conflicting.content_type = "text/plain".into();
        assert!(matches!(
            recorder.expect_payload(&conflicting),
            Err(RecordingError::Integrity)
        ));
        foreign = envelope(2, "future", serde_json::json!({"preserve": true}));
        foreign.schema_version += 1;
        assert!(matches!(
            recorder.append(&foreign),
            Err(RecordingError::InvalidInput)
        ));
    }

    #[test]
    fn bundle_limits_close_failed_rather_than_silently_truncating() {
        let (_temporary, _path, mut recorder) = setup();
        ready(&mut recorder);
        recorder.options.max_bundle_bytes = recorder.manifest.events.byte_len;
        assert!(matches!(
            recorder.append(&envelope(2, "event", serde_json::Value::Null)),
            Err(RecordingError::Limit)
        ));
        assert_eq!(recorder.manifest().state, RecordingState::Failed);
        assert!(!recorder.manifest().evidence_complete);
        assert!(matches!(
            bounded_json(&serde_json::json!({"large": "abcdef"}), 2),
            Err(RecordingError::Limit)
        ));
    }

    #[test]
    fn private_modes_no_overwrite_and_symlink_ancestors_fail_closed() {
        let (temporary, path, mut recorder) = setup();
        ready(&mut recorder);
        let payload = reference(b"private");
        recorder.store_payload(&payload, b"private").unwrap();
        let manifest = finish(&mut recorder, &boundary(2));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(path.join("payloads"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        for relative in [
            "events.jsonl",
            "manifest.json",
            manifest.payloads[&payload.payload_id]
                .relative_path
                .as_ref()
                .unwrap(),
        ] {
            assert_eq!(
                std::fs::metadata(path.join(relative))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(
            JsonlBundleRecorder::create(&path, "other", "session", RecordingOptions::default())
                .is_err()
        );
        let alias = temporary.path().canonicalize().unwrap().join("alias");
        symlink(&path, &alias).unwrap();
        assert!(
            JsonlBundleRecorder::create(
                &alias.join("escape"),
                "observer",
                "session",
                RecordingOptions::default()
            )
            .is_err()
        );
        assert!(!path.join("escape").exists());
        assert!(
            JsonlBundleRecorder::create(
                Path::new("relative"),
                "o",
                "s",
                RecordingOptions::default()
            )
            .is_err()
        );
    }

    #[test]
    fn replaced_payload_directory_symlink_cannot_redirect_pinned_writer() {
        let (temporary, path, mut recorder) = setup();
        let elsewhere = temporary.path().canonicalize().unwrap().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::rename(path.join("payloads"), path.join("original-payloads")).unwrap();
        symlink(&elsewhere, path.join("payloads")).unwrap();
        let payload = reference(b"private");
        recorder.store_payload(&payload, b"private").unwrap();
        assert_eq!(std::fs::read_dir(elsewhere).unwrap().count(), 0);
        assert_eq!(
            std::fs::read_dir(path.join("original-payloads"))
                .unwrap()
                .count(),
            1
        );
        // No external content was touched. Reader must still reject the substituted
        // symlink rather than trusting a manifest-relative path when opening a bundle.
        recorder.interrupt().unwrap();
    }
}

#[cfg(not(unix))]
mod confined {
    use super::{File, Path, RecordingError};
    pub(super) fn create_bundle(_: &Path) -> Result<File, RecordingError> {
        Err(RecordingError::UnsupportedPlatform)
    }
    pub(super) fn mkdir(_: &File, _: &str) -> Result<File, RecordingError> {
        Err(RecordingError::UnsupportedPlatform)
    }
    pub(super) fn create(_: &File, _: &str) -> Result<File, RecordingError> {
        Err(RecordingError::UnsupportedPlatform)
    }
    pub(super) fn replace(_: &File, _: &str, _: &str) -> Result<(), RecordingError> {
        Err(RecordingError::UnsupportedPlatform)
    }
}
