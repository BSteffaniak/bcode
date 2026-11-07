//! Where the daemon's socket lives.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use crate::protocol::PROTOCOL;

/// Longest socket path every platform accepts (`sun_path` holds 104 bytes on macOS).
pub const SOCKET_PATH_LIMIT: usize = 100;

/// The default state directory: `$XDG_STATE_HOME/bcode-git`, else
/// `~/.local/state/bcode-git`.
#[must_use]
pub fn default_state_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("state"))
        })?;
    Some(base.join("bcode-git"))
}

/// The socket of this build's daemon under `state_dir`: `daemon/<identity>.sock`.
///
/// The identity covers the running executable (path, size and modification time) and the
/// protocol version. Each build thus has its own daemon, and a rebuilt binary starts a new
/// one while clients of the old build keep theirs. Socket paths are short, so when that one
/// would be too long it lives in `/tmp/bcode-git-<uid>/` instead, named after it.
#[must_use]
pub fn socket_path(state_dir: &Path) -> PathBuf {
    let preferred = state_dir
        .join("daemon")
        .join(format!("{:016x}.sock", identity()));
    if preferred.as_os_str().len() <= SOCKET_PATH_LIMIT {
        return preferred;
    }
    let mut hasher = std::hash::DefaultHasher::new();
    preferred.hash(&mut hasher);
    PathBuf::from("/tmp")
        .join(format!("bcode-git-{}", owner()))
        .join(format!("{:016x}.sock", hasher.finish()))
}

/// The current user's id, as the owner of the home directory.
#[cfg(unix)]
fn owner() -> u32 {
    use std::os::unix::fs::MetadataExt as _;
    std::env::var_os("HOME")
        .and_then(|home| std::fs::metadata(home).ok())
        .map_or(0, |metadata| metadata.uid())
}

#[cfg(not(unix))]
const fn owner() -> u32 {
    0
}

/// Create the socket's directory when missing and make sure only the current user can use
/// it, so nobody else can listen in its place.
///
/// # Errors
/// * [`std::io::Error`] when the directory cannot be created, or belongs to another user, or
///   others may use it and that cannot be changed.
#[cfg(unix)]
pub fn private_directory(socket: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let Some(directory) = socket.parent() else {
        return Ok(());
    };
    std::fs::create_dir_all(directory)?;
    let metadata = std::fs::metadata(directory)?;
    if metadata.uid() != owner() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} belongs to another user", directory.display()),
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn identity() -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    PROTOCOL.hash(&mut hasher);
    if let Ok(executable) = std::env::current_exe() {
        executable.hash(&mut hasher);
        if let Ok(metadata) = std::fs::metadata(&executable) {
            metadata.len().hash(&mut hasher);
            if let Ok(modified) = metadata.modified() {
                modified.hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sockets_stay_short_enough_to_bind() {
        let shallow = socket_path(Path::new("/home/me/.local/state/bcode-git"));
        assert!(shallow.starts_with("/home/me/.local/state/bcode-git/daemon"));
        let deep = Path::new("/very").join("deep/".repeat(40));
        let socket = socket_path(&deep);
        assert!(socket.starts_with("/tmp"));
        assert!(socket.as_os_str().len() <= SOCKET_PATH_LIMIT);
        // Each state directory keeps its own daemon.
        assert_ne!(socket, socket_path(&deep.join("other")));
    }
}
