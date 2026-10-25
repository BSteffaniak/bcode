//! OS boundary for cooperative per-user ownership of the actual pipeline.
//!
//! Linux intentionally uses the canonical /run/user/<euid> location rather than
//! an environment-selected directory: plugin and standalone processes must not
//! accidentally choose distinct locks. The lock inode is never removed. Kernel
//! descriptor lifetime releases ownership after normal exit or process death.
use crate::contracts::{Error, Result};

#[cfg(target_os = "linux")]
pub(super) struct Reservation {
    _file: std::fs::File,
}

#[cfg(not(target_os = "linux"))]
pub(super) struct Reservation;

/// Reserve actual capture ownership without waiting.
///
/// # Errors
/// Returns `Unsupported` because this OS has no secure ownership adapter.
#[cfg(not(target_os = "linux"))]
pub(super) fn reserve() -> Result<Reservation> {
    Err(Error::Unsupported(
        "cooperative capture ownership is implemented only on Linux".into(),
    ))
}

/// Reserve actual capture ownership without waiting.
///
/// # Errors
/// Returns `Busy` for contention, `Invalid` for unsafe path ownership or modes,
/// and `Io` when the canonical runtime directory or lock cannot be opened.
#[cfg(target_os = "linux")]
pub(super) fn reserve() -> Result<Reservation> {
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    // SAFETY: geteuid has no memory safety preconditions.
    let uid = unsafe { libc::geteuid() };
    // Pin each component; no symlink or environment override can redirect locks.
    let root = open_directory(Path::new("/"))?;
    let run = open_directory(&fd_path(&root).join("run"))?;
    let users = open_directory(&fd_path(&run).join("user"))?;
    for parent in [&root, &run, &users] {
        let metadata = parent.metadata()?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(Error::Invalid(
                "canonical runtime parents must be root-owned and not publicly writable".into(),
            ));
        }
    }
    let user = open_directory(&fd_path(&users).join(uid.to_string()))?;
    reserve_in(&user, uid)
}

#[cfg(target_os = "linux")]
fn open_directory(path: &std::path::Path) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    Ok(std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?)
}

#[cfg(target_os = "linux")]
fn fd_path(file: &std::fs::File) -> std::path::PathBuf {
    use std::os::fd::AsRawFd;
    std::path::PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

#[cfg(target_os = "linux")]
fn reserve_in(directory: &std::fs::File, uid: u32) -> Result<Reservation> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let metadata = directory.metadata()?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(Error::Invalid(
            "runtime directory must be private and owned by the current user".into(),
        ));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(fd_path(directory).join("vox-capture-v1.lock"))?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(Error::Invalid(
            "capture lock must be a private, singly-linked regular file".into(),
        ));
    }
    // SAFETY: fd is owned and open for this call; flock does not borrow memory.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Err(Error::Busy);
        }
        return Err(Error::Io(error));
    }
    Ok(Reservation { _file: file })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn private_root() -> (tempfile::TempDir, std::fs::File, u32) {
        let temp = tempfile::tempdir().expect("isolated runtime directory");
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private mode");
        let directory = open_directory(temp.path()).expect("open runtime directory");
        let uid = directory.metadata().expect("metadata").uid();
        (temp, directory, uid)
    }

    #[test]
    fn separately_opened_owners_contend_and_drop_releases_without_unlink() {
        let (temp, directory, uid) = private_root();
        let owner = reserve_in(&directory, uid).expect("first reservation");
        let other = open_directory(temp.path()).expect("independent descriptor");
        assert!(matches!(reserve_in(&other, uid), Err(Error::Busy)));
        let before =
            std::fs::metadata(temp.path().join("vox-capture-v1.lock")).expect("lock metadata");
        drop(owner);
        let _next = reserve_in(&other, uid).expect("released reservation");
        let after =
            std::fs::metadata(temp.path().join("vox-capture-v1.lock")).expect("lock metadata");
        assert_eq!(before.ino(), after.ino());
    }

    #[test]
    fn shutdown_keeps_reservation_until_noncooperative_constructor_returns() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        let (_temp, directory, uid) = private_root();
        let reservation = reserve_in(&directory, uid).expect("reserve");
        let (entered_tx, entered) = mpsc::sync_channel(1);
        let (release, release_rx) = mpsc::sync_channel(1);
        let handle = super::super::spawn_worker(
            move || {
                entered_tx.send(()).expect("signal constructor");
                release_rx
                    .recv_timeout(Duration::from_secs(3))
                    .expect("release constructor");
                Err(Error::Cancelled)
            },
            true,
            Some(reservation),
        )
        .expect("spawn deferred worker");
        entered
            .recv_timeout(Duration::from_secs(2))
            .expect("constructor entered");
        handle.shutdown();
        assert!(matches!(reserve_in(&directory, uid), Err(Error::Busy)));
        release.send(()).expect("release");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match reserve_in(&directory, uid) {
                Ok(_next) => break,
                Err(Error::Busy) => {
                    assert!(Instant::now() < deadline, "reservation was not released");
                    std::thread::yield_now();
                }
                Err(error) => panic!("unexpected reservation failure: {error}"),
            }
        }
    }

    #[test]
    fn unsafe_directory_symlink_and_hardlink_are_rejected() {
        let (temp, directory, uid) = private_root();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o755))
            .expect("public mode");
        assert!(reserve_in(&directory, uid).is_err());
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private mode");
        let target = temp.path().join("target");
        std::fs::write(&target, "unchanged").expect("target");
        let lock = temp.path().join("vox-capture-v1.lock");
        std::os::unix::fs::symlink(&target, &lock).expect("symlink fixture");
        assert!(reserve_in(&directory, uid).is_err());
        assert_eq!(
            std::fs::read_to_string(&target).expect("target unchanged"),
            "unchanged"
        );
        std::fs::remove_file(&lock).expect("remove fixture symlink");
        std::fs::hard_link(&target, &lock).expect("hardlink fixture");
        assert!(reserve_in(&directory, uid).is_err());
    }
}
