//! Resolve a test-only interpreter before clearing subprocess environments.
use std::path::{Path, PathBuf};

fn executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        path.metadata()
            .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub fn python() -> PathBuf {
    let selected = std::env::var_os("BCODE_HARNESS_FIXTURE_PYTHON").map_or_else(
        || {
            let path = std::env::var_os("PATH")
                .expect("test environment must supply PATH or explicit Python");
            std::env::split_paths(&path).filter(|directory| directory.is_absolute())
                .take(256).map(|directory| directory.join("python3"))
                .find(|candidate| executable(candidate))
                .expect("python3 is required on the controlled test PATH; missing interpreter is a test failure")
        },
        |path| {
            let path = PathBuf::from(path);
            assert!(
                path.is_absolute() && executable(&path),
                "BCODE_HARNESS_FIXTURE_PYTHON must name an existing absolute executable"
            );
            path
        },
    );
    std::fs::canonicalize(selected).expect("resolve selected test Python executable")
}
