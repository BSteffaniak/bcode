//! Cross-process admission for storage reads and automatic maintenance.
//!
//! Read operations hold a shared coordinator lock; maintenance holds it exclusively. Each daemon
//! supplies its own participant file. Before exposing content, a reader durably marks that file
//! dirty; only successful tracking and completion may clean it. Failed/abandoned participants stop
//! maintenance without preventing unrelated reads. All handles are caller-confined, non-append
//! regular files; participant discovery and ownership of the complete registry remain caller-owned.

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod registry;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub use registry::StorageAdmissionRegistry;

use std::fs::File;
use std::io::{self, Read as _, Seek as _, Write as _};

const CLEAN: &[u8; 17] = b"BCSTREADV1:CLEAN!";
const DIRTY: &[u8; 17] = b"BCSTREADV1:DIRTY!";

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "storage tracking participant is not clean",
    )
}

fn read_state(file: &mut File, permit_empty: bool) -> io::Result<()> {
    let length = file.metadata()?.len();
    if permit_empty && length == 0 {
        return Ok(());
    }
    if length != CLEAN.len() as u64 {
        return Err(invalid());
    }
    file.rewind()?;
    let mut bytes = [0; CLEAN.len()];
    file.read_exact(&mut bytes)?;
    if &bytes == CLEAN {
        Ok(())
    } else {
        Err(invalid())
    }
}

fn write_state(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    file.rewind()?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// A read admitted under the cross-process maintenance gate.
///
/// Drop without `complete` releases locks but preserves dirty state. Caller must retain this guard
/// through physical content access and durable access tracking, including spawned blocking work.
/// Cloned descriptors sharing seek offsets or lock ownership must not be passed to separate calls.
pub struct StorageReadAdmission {
    gate: File,
    participant: File,
    may_clean: bool,
}

impl StorageReadAdmission {
    /// Admit a reader without waiting for an active maintenance operation.
    ///
    /// Existing dirty state does not block reads and cannot be cleaned by this operation. This
    /// permits recovery/debugging without silently restoring eligibility after a previous failure.
    /// Concurrent reads from one participant must be coalesced by its daemon or use distinct files.
    ///
    /// # Errors
    /// Returns errors for contention, nonregular handles, or failure to persist dirty state. Callers
    /// must not expose content under an unregistered fallback if they promise automatic tiering.
    pub fn begin(gate: File, mut participant: File) -> io::Result<Self> {
        if !gate.metadata()?.is_file() || !participant.metadata()?.is_file() {
            return Err(invalid());
        }
        gate.try_lock_shared().map_err(io::Error::from)?;
        participant.try_lock().map_err(io::Error::from)?;
        let state = read_state(&mut participant, true);
        let may_clean = match state {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::InvalidData => false,
            Err(error) => return Err(error),
        };
        // Unknown/dirty representations are preserved, not rewritten as the current version.
        if may_clean {
            write_state(&mut participant, DIRTY)?;
        }
        Ok(Self {
            gate,
            participant,
            may_clean,
        })
    }

    /// Complete content access after its timestamp has been persisted.
    ///
    /// # Errors
    /// Returns an error if this participant was already dirty or if sync/unlock fails. A failed
    /// completion must disable scheduling; dropping still releases locks for unrelated readers.
    pub fn complete(mut self) -> io::Result<()> {
        if !self.may_clean {
            return Err(invalid());
        }
        write_state(&mut self.participant, CLEAN)?;
        self.participant.unlock()?;
        self.gate.unlock()
    }
}

/// Exclusive maintenance admission. The gate must remain held through conversion/publication.
pub struct StorageMaintenanceAdmission {
    _gate: File,
}

impl StorageMaintenanceAdmission {
    /// Acquire exclusive admission and verify a complete bounded participant registry snapshot.
    ///
    /// The caller must hold registry enumeration/registration coordination through this gate,
    /// supply every registered participant exactly once, and reject an incomplete registry page.
    /// This API does not infer the absence of old/unregistered clients from these records.
    ///
    /// # Errors
    /// Returns an error for contention, any unknown/dirty/empty participant, or IO. Iterator errors
    /// fail closed; a partial successful scan never grants authority.
    pub fn begin(
        gate: File,
        participants: impl IntoIterator<Item = io::Result<File>>,
    ) -> io::Result<Self> {
        if !gate.metadata()?.is_file() {
            return Err(invalid());
        }
        gate.try_lock().map_err(io::Error::from)?;
        for file in participants {
            let mut file = file?;
            if !file.metadata()?.is_file() {
                return Err(invalid());
            }
            file.try_lock_shared().map_err(io::Error::from)?;
            read_state(&mut file, false)?;
            file.unlock()?;
        }
        Ok(Self { _gate: gate })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(file: &tempfile::NamedTempFile) -> File {
        file.reopen().expect("open")
    }

    #[test]
    fn crash_child() {
        let Ok(gate_path) = std::env::var("BCODE_ADMISSION_CRASH_GATE") else {
            return;
        };
        let participant_path =
            std::env::var("BCODE_ADMISSION_CRASH_PARTICIPANT").expect("participant path");
        let gate = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(gate_path)
            .expect("gate");
        let participant = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(participant_path)
            .expect("participant");
        let _admission = StorageReadAdmission::begin(gate, participant).expect("admission");
        std::process::exit(94);
    }

    #[test]
    fn process_crash_releases_admission_lock_but_preserves_ineligible_participant() {
        let gate = tempfile::NamedTempFile::new().expect("gate");
        let participant = tempfile::NamedTempFile::new().expect("participant");
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", "storage_admission::tests::crash_child"])
            .env("BCODE_ADMISSION_CRASH_GATE", gate.path())
            .env("BCODE_ADMISSION_CRASH_PARTICIPANT", participant.path())
            .status()
            .expect("child");
        assert_eq!(status.code(), Some(94));
        let released_gate = open(&gate);
        released_gate.try_lock().expect("OS released dead owner");
        released_gate.unlock().expect("unlock");
        assert!(
            StorageMaintenanceAdmission::begin(released_gate, [Ok(open(&participant))]).is_err()
        );
        assert_eq!(
            std::fs::read(participant.path()).expect("dirty record"),
            DIRTY
        );
    }

    #[test]
    fn independent_readers_coexist_and_exclude_maintenance_until_all_complete() {
        let gate = tempfile::NamedTempFile::new().expect("gate");
        let first = tempfile::NamedTempFile::new().expect("first");
        let second = tempfile::NamedTempFile::new().expect("second");
        let a = StorageReadAdmission::begin(open(&gate), open(&first)).expect("first read");
        let b = StorageReadAdmission::begin(open(&gate), open(&second)).expect("second read");
        assert!(
            StorageMaintenanceAdmission::begin(open(&gate), [Ok(open(&first)), Ok(open(&second))])
                .is_err()
        );
        a.complete().expect("first complete");
        assert!(
            StorageMaintenanceAdmission::begin(open(&gate), [Ok(open(&first)), Ok(open(&second))])
                .is_err()
        );
        b.complete().expect("second complete");
        let maintenance =
            StorageMaintenanceAdmission::begin(open(&gate), [Ok(open(&first)), Ok(open(&second))])
                .expect("maintenance");
        assert!(StorageReadAdmission::begin(open(&gate), open(&first)).is_err());
        drop(maintenance);
        StorageReadAdmission::begin(open(&gate), open(&first))
            .expect("read again")
            .complete()
            .expect("complete");
    }

    #[test]
    fn abandoned_reader_blocks_maintenance_but_not_other_readers() {
        let gate = tempfile::NamedTempFile::new().expect("gate");
        let first = tempfile::NamedTempFile::new().expect("first");
        let second = tempfile::NamedTempFile::new().expect("second");
        drop(StorageReadAdmission::begin(open(&gate), open(&first)).expect("read"));
        StorageReadAdmission::begin(open(&gate), open(&second))
            .expect("unrelated read")
            .complete()
            .expect("tracked");
        assert!(
            StorageMaintenanceAdmission::begin(open(&gate), [Ok(open(&first)), Ok(open(&second))])
                .is_err()
        );
        assert!(
            StorageReadAdmission::begin(open(&gate), open(&first))
                .expect("damaged participant can read")
                .complete()
                .is_err()
        );
        assert_eq!(std::fs::read(first.path()).expect("dirty"), DIRTY);
    }

    #[test]
    fn unknown_state_and_incomplete_registry_are_preserved_and_rejected() {
        let gate = tempfile::NamedTempFile::new().expect("gate");
        let participant = tempfile::NamedTempFile::new().expect("participant");
        for bytes in [b"future-version".as_slice(), b"truncated"] {
            std::fs::write(participant.path(), bytes).expect("fixture");
            assert!(
                StorageMaintenanceAdmission::begin(open(&gate), [Ok(open(&participant))]).is_err()
            );
            assert!(
                StorageReadAdmission::begin(open(&gate), open(&participant))
                    .expect("read")
                    .complete()
                    .is_err()
            );
            assert_eq!(std::fs::read(participant.path()).expect("preserved"), bytes);
        }
        assert!(
            StorageMaintenanceAdmission::begin(
                open(&gate),
                [Err(io::Error::other("incomplete discovery"))]
            )
            .is_err()
        );
    }
}
