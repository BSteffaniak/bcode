//! Version-independent advisory coordination for this user's filesystem mutations.
//!
//! One lock deliberately serializes all cooperating mutations. It survives target
//! replacement and avoids multi-target lock ordering. This is not daemon state,
//! authorization, or protection against external writers. Never unlink the lock:
//! removing its name would let new callers lock a different file.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bcode_confined_fs::{Creation, Dir, OpenOptions};

const LOCK: &str = "mutations.lock";

/// Acquire the per-user mutation gate, polling cancellation while contended.
/// The returned file releases its advisory lock when dropped, including on unwind.
pub fn acquire(cancelled: &impl Fn() -> bool) -> io::Result<File> {
    if cancelled() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "filesystem mutation cancelled before coordination",
        ));
    }
    let directory = coordination_directory()?;
    match bcode_confined_fs::create_private_dir(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let parent = super::confined::open_directory(&directory)?;
    if !private_entry(parent.as_file(), true)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe filesystem coordination directory",
        ));
    }
    let lock = acquire_in(&parent, cancelled)?;
    verify_directory_identity(&directory, &parent)?;
    Ok(lock)
}

/// A fixed host location shared by every Bcode version and state location of this user,
/// never TMPDIR or artifact/state-specific locations.
#[cfg(unix)]
fn coordination_directory() -> io::Result<PathBuf> {
    // SAFETY: geteuid takes no arguments and has no memory safety preconditions.
    let uid = unsafe { libc::geteuid() };
    let root = Path::new("/tmp").canonicalize()?;
    let root_handle = super::confined::open_directory(&root)?;
    validate_shared_root(&root_handle.as_file().metadata()?)?;
    Ok(root.join(format!("bcode-filesystem-mutations-v1-{uid}")))
}

/// The per-user local application data root is private to this user and fixed across
/// Bcode versions and state locations.
#[cfg(windows)]
fn coordination_directory() -> io::Result<PathBuf> {
    let root = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|root| root.is_absolute())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "local application data directory is unavailable",
            )
        })?
        .canonicalize()?;
    Ok(root.join("bcode-filesystem-mutations-v1"))
}

// A shared writable root needs sticky deletion protection; otherwise another
// user can rename the private child and split cooperating callers across locks.
#[cfg(unix)]
fn validate_shared_root(metadata: &std::fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let mode = metadata.permissions().mode();
    if !metadata.is_dir() || metadata.uid() != 0 || (mode & 0o022 != 0 && mode & 0o1000 == 0) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe shared filesystem coordination root",
        ));
    }
    Ok(())
}

/// Unix requires this user's exact owner-only modes; Windows requires an owner-private ACL.
fn private_entry(file: &File, directory: bool) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let metadata = file.metadata()?;
        // SAFETY: geteuid takes no arguments and has no memory safety preconditions.
        let uid = unsafe { libc::geteuid() };
        let mode = if directory { 0o700 } else { 0o600 };
        Ok(metadata.uid() == uid && metadata.permissions().mode() & 0o777 == mode)
    }
    #[cfg(not(unix))]
    {
        let _ = directory;
        bcode_confined_fs::is_private(file)
    }
}

fn verify_directory_identity(path: &Path, pinned: &Dir) -> io::Result<()> {
    let current = super::confined::open_directory(path)?;
    if current.metadata()?.identity() != pinned.metadata()?.identity()
        || !private_entry(current.as_file(), true)?
    {
        return Err(io::Error::other(
            "filesystem coordination directory changed while waiting",
        ));
    }
    Ok(())
}

fn acquire_in(parent: &Dir, cancelled: &impl Fn() -> bool) -> io::Result<File> {
    // Exclusive creation is unnecessary because the persistent file is the lock identity.
    let file = parent.open_file(
        OsStr::new(LOCK),
        OpenOptions::new()
            .read(true)
            .write(true)
            .private(true)
            .creation(Creation::IfMissing),
    )?;
    if bcode_confined_fs::file_metadata(&file)?.link_count() != 1 || !private_entry(&file, false)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe filesystem coordination file",
        ));
    }
    loop {
        if cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "filesystem mutation cancelled while waiting for coordination",
            ));
        }
        match file.try_lock() {
            Ok(()) => {
                verify_lock_identity(parent, &file)?;
                return Ok(file);
            }
            Err(std::fs::TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(10)),
            Err(std::fs::TryLockError::Error(error))
                if error.kind() == io::ErrorKind::Interrupted => {}
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
        }
    }
}

// Reopen by the pinned directory handle after waiting: locking an unlinked
// file must never be mistaken for holding the currently named gate.
fn verify_lock_identity(parent: &Dir, locked: &File) -> io::Result<()> {
    let named = parent.metadata_at(OsStr::new(LOCK))?;
    let held = bcode_confined_fs::file_metadata(locked)?;
    if named.identity() != held.identity()
        || held.link_count() != 1
        || held.kind() != bcode_confined_fs::EntryKind::File
        || !private_entry(locked, false)?
    {
        return Err(io::Error::other(
            "filesystem coordination identity changed while waiting",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};

    #[cfg(unix)]
    #[test]
    fn unsafe_lock_leaves_are_rejected_without_changing_external_bytes() {
        let root = crate::tests::temp_dir("mutation-unsafe-leaf");
        let parent = Dir::open(&root).unwrap();
        let target = root.join("external");
        let lock = root.join("mutations.lock");
        std::fs::write(&target, "untouched").unwrap();
        std::os::unix::fs::symlink(&target, &lock).unwrap();
        assert!(acquire_in(&parent, &|| false).is_err());
        std::fs::remove_file(&lock).unwrap();
        std::fs::hard_link(&target, &lock).unwrap();
        assert!(acquire_in(&parent, &|| false).is_err());
        std::fs::remove_file(&lock).unwrap();
        let held = acquire_in(&parent, &|| false).unwrap();
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(verify_lock_identity(&parent, &held).is_err());
        assert!(acquire_in(&parent, &|| false).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
        drop(held);
        std::fs::remove_file(lock).unwrap();
        std::fs::remove_file(target).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn shared_root_requires_safe_owner_and_deletion_permissions() {
        let root = crate::tests::temp_dir("mutation-root-permissions");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(validate_shared_root(&std::fs::metadata(&root).unwrap()).is_err());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o1777)).unwrap();
        let metadata = std::fs::metadata(&root).unwrap();
        assert_eq!(validate_shared_root(&metadata).is_ok(), metadata.uid() == 0);
        std::fs::remove_dir(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn renamed_coordination_directory_is_rejected() {
        let root = crate::tests::temp_dir("mutation-directory-identity")
            .canonicalize()
            .unwrap();
        let directory = root.join("coordination");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let pinned = Dir::open(&directory).unwrap();
        verify_directory_identity(&directory, &pinned).unwrap();
        std::fs::rename(&directory, root.join("obsolete")).unwrap();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        assert!(
            verify_directory_identity(&directory, &pinned)
                .unwrap_err()
                .to_string()
                .contains("directory changed")
        );
        std::fs::remove_dir(&directory).unwrap();
        std::os::unix::fs::symlink(root.join("obsolete"), &directory).unwrap();
        assert!(verify_directory_identity(&directory, &pinned).is_err());
        std::fs::remove_file(directory).unwrap();
        drop(pinned);
        std::fs::remove_dir(root.join("obsolete")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn replaced_lock_is_rejected_after_waiting() {
        let root = crate::tests::temp_dir("mutation-replaced-lock");
        let parent = Dir::open(&root).unwrap();
        let first = std::cell::RefCell::new(Some(acquire_in(&parent, &|| false).unwrap()));
        let polls = std::cell::Cell::new(0);
        let result = acquire_in(&parent, &|| {
            polls.set(polls.get() + 1);
            if polls.get() == 2 {
                std::fs::rename(root.join("mutations.lock"), root.join("obsolete")).unwrap();
                drop(acquire_in(&parent, &|| false).unwrap());
                first.borrow_mut().take();
            }
            polls.get() > 10
        });
        assert!(result.unwrap_err().to_string().contains("identity changed"));
        drop(acquire_in(&parent, &|| false).unwrap());
        std::fs::remove_file(root.join("obsolete")).unwrap();
        std::fs::remove_file(root.join("mutations.lock")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn subprocess_mutation_contender() {
        let Some(root) = std::env::var_os("BCODE_TEST_MUTATION_ROOT") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        let parent = Dir::open(&root).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let result = acquire_in(&parent, &|| std::time::Instant::now() >= deadline);
        if std::env::var_os("BCODE_TEST_EXPECT_CONTENTION").is_some() {
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        } else {
            let _guard = result.unwrap();
            std::fs::write(root.join("target"), "child committed").unwrap();
        }
    }

    #[test]
    fn another_process_cannot_mutate_until_owner_releases() {
        let root = crate::tests::temp_dir("mutation-process-coordination");
        let parent = Dir::open(&root).unwrap();
        let guard = acquire_in(&parent, &|| false).unwrap();
        std::fs::write(root.join("target"), "original").unwrap();
        let run_child = |expect_contention| {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "coordination::tests::subprocess_mutation_contender",
                ])
                .env("BCODE_TEST_MUTATION_ROOT", &root)
                .env_remove("BCODE_TEST_EXPECT_CONTENTION");
            if expect_contention {
                command.env("BCODE_TEST_EXPECT_CONTENTION", "1");
            }
            let mut child = command.spawn().unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success());
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("mutation contender did not finish within deadline");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        run_child(true);
        assert_eq!(
            std::fs::read_to_string(root.join("target")).unwrap(),
            "original"
        );
        drop(guard);
        run_child(false);
        assert_eq!(
            std::fs::read_to_string(root.join("target")).unwrap(),
            "child committed"
        );
        std::fs::remove_file(root.join("target")).unwrap();
        std::fs::remove_file(root.join("mutations.lock")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn contention_cancels_and_drop_releases_without_replacing_lock_inode() {
        let root = crate::tests::temp_dir("mutation-coordination");
        let parent = Dir::open(&root).unwrap();
        let first = acquire_in(&parent, &|| false).unwrap();
        let identity = bcode_confined_fs::file_metadata(&first).unwrap().identity();
        let polls = std::cell::Cell::new(0);
        let error = acquire_in(&parent, &|| {
            polls.set(polls.get() + 1);
            polls.get() == 3
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        drop(first);
        let second = acquire_in(&parent, &|| false).unwrap();
        assert_eq!(
            bcode_confined_fs::file_metadata(&second)
                .unwrap()
                .identity(),
            identity
        );
        drop(second);
        std::fs::remove_file(root.join("mutations.lock")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
