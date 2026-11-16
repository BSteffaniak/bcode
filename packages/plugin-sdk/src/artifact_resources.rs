//! Resource trees that ship beside the Bcode executable.
//!
//! A Bcode artifact is its executable plus the resource trees named in
//! [`ARTIFACT_RESOURCE_TREES`]. A daemon runs from an immutable snapshot of that
//! artifact, so it must never read resources from its own executable directory
//! or from wherever the artifact was originally installed. Every lookup goes
//! through [`artifact_resource_root`], which returns:
//!
//! * the snapshot named by [`BCODE_ARTIFACT_RESOURCES_ENV`] when it sits in the
//!   running executable's own daemon image directory, or
//! * the directory containing the running executable otherwise.
//!
//! The image-directory check scopes the variable to the exact daemon image: a
//! different `bcode` started by a daemon child process inherits the variable
//! but resolves its own installation instead.

use std::path::{Path, PathBuf};

/// Environment variable naming the resource snapshot a daemon was launched with.
pub const BCODE_ARTIFACT_RESOURCES_ENV: &str = "BCODE_ARTIFACT_RESOURCES";

/// Top-level directories beside the executable that belong to the artifact.
///
/// * `bcode-runtimes`: native runtimes loaded at run time, such as dynamic Tesseract.
/// * `plugins`: native plugin manifests and libraries.
pub const ARTIFACT_RESOURCE_TREES: [&str; 2] = ["bcode-runtimes", "plugins"];

/// Returns the directory holding this process's artifact resource trees.
///
/// Returns `None` when the executable location cannot be determined.
#[must_use]
pub fn artifact_resource_root() -> Option<PathBuf> {
    resolve_artifact_resource_root(
        std::env::var_os(BCODE_ARTIFACT_RESOURCES_ENV).map(PathBuf::from),
        std::env::current_exe().ok().as_deref(),
    )
}

/// Returns `relative` inside this process's artifact resource root.
///
/// The path is not checked for existence.
#[must_use]
pub fn artifact_resource_path(relative: impl AsRef<Path>) -> Option<PathBuf> {
    artifact_resource_root().map(|root| root.join(relative))
}

fn resolve_artifact_resource_root(
    snapshot: Option<PathBuf>,
    executable: Option<&Path>,
) -> Option<PathBuf> {
    let executable_dir = canonical(executable?.parent()?);
    match snapshot {
        Some(snapshot)
            if snapshot.is_absolute()
                && snapshot.parent().map(canonical).as_ref() == Some(&executable_dir) =>
        {
            Some(snapshot)
        }
        _ => Some(executable_dir),
    }
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::resolve_artifact_resource_root;
    use std::path::PathBuf;

    #[test]
    fn snapshot_in_the_executable_image_takes_precedence() {
        let image = tempfile::tempdir().expect("image");
        let image = image.path().canonicalize().expect("canonical image");
        let snapshot = image.join("resources-abc");
        assert_eq!(
            resolve_artifact_resource_root(Some(snapshot.clone()), Some(&image.join("bcode"))),
            Some(snapshot)
        );
    }

    #[test]
    fn executable_directory_is_used_without_a_snapshot() {
        let install = tempfile::tempdir().expect("install");
        let install = install.path().canonicalize().expect("canonical install");
        assert_eq!(
            resolve_artifact_resource_root(None, Some(&install.join("bcode"))),
            Some(install)
        );
    }

    #[test]
    fn inherited_snapshot_of_another_image_is_ignored() {
        let image = tempfile::tempdir().expect("image");
        let install = tempfile::tempdir().expect("install");
        let install = install.path().canonicalize().expect("canonical install");
        assert_eq!(
            resolve_artifact_resource_root(
                Some(image.path().join("resources-abc")),
                Some(&install.join("bcode")),
            ),
            Some(install.clone())
        );
        assert_eq!(
            resolve_artifact_resource_root(
                Some(PathBuf::from("resources-abc")),
                Some(&install.join("bcode")),
            ),
            Some(install)
        );
    }
}
