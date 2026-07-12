//! Human-friendly path presentation helpers.

use std::fmt;
use std::path::{Component, Path, PathBuf};

/// A path formatted relative to a working directory when it remains concise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayPath(PathBuf);

impl fmt::Display for DisplayPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.display().fmt(formatter)
    }
}

/// Format `path` relative to `working_directory` when it is within that directory or its
/// immediate parent.
///
/// Descendants of `working_directory` are displayed without a `./` prefix. Paths elsewhere
/// within its immediate parent are displayed with one `../` prefix. Paths requiring more parent
/// traversal are displayed as normalized absolute paths.
///
/// Normalization is lexical: this function performs no filesystem access and works for paths that
/// do not exist.
#[must_use]
pub fn display(path: impl AsRef<Path>, working_directory: impl AsRef<Path>) -> DisplayPath {
    let working_directory = normalize(working_directory.as_ref());
    let path = if path.as_ref().is_absolute() {
        normalize(path.as_ref())
    } else {
        normalize(&working_directory.join(path))
    };

    if let Ok(relative) = path.strip_prefix(&working_directory) {
        return DisplayPath(if relative.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            relative.to_path_buf()
        });
    }

    if let Some(parent) = working_directory.parent()
        && let Ok(relative) = path.strip_prefix(parent)
    {
        return DisplayPath(Path::new("..").join(relative));
    }

    DisplayPath(path)
}

/// Format a path without assuming a working-directory base.
///
/// Relative paths remain relative and absolute paths remain absolute. Both are normalized
/// lexically without filesystem access.
#[must_use]
pub fn display_without_base(path: impl AsRef<Path>) -> DisplayPath {
    DisplayPath(normalize(path.as_ref()))
}

/// Format `path` relative to the process working directory when it remains concise.
///
/// If the process working directory is unavailable, relative paths are formatted against `.` and
/// absolute paths remain normalized.
#[must_use]
pub fn display_from_current_dir(path: impl AsRef<Path>) -> DisplayPath {
    display(
        path,
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    )
}

fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized
                    .components()
                    .next_back()
                    .is_some_and(|last| matches!(last, Component::Normal(_)))
                {
                    normalized.pop();
                } else if !normalized.has_root() {
                    normalized.push(component);
                }
            }
            _ => normalized.push(component),
        }
    }

    normalized
}

#[cfg(test)]
mod tests {
    use super::display;
    use std::path::{Path, PathBuf};

    fn root_path(path: &str) -> PathBuf {
        Path::new(std::path::MAIN_SEPARATOR_STR).join(path)
    }

    #[test]
    fn displays_working_directory_as_dot() {
        let cwd = root_path("repo");
        assert_eq!(display(&cwd, &cwd).to_string(), ".");
    }

    #[test]
    fn displays_descendant_relative_to_working_directory() {
        let cwd = root_path("repo");
        assert_eq!(
            display(cwd.join("src/lib.rs"), &cwd).to_string(),
            Path::new("src/lib.rs").display().to_string()
        );
    }

    #[test]
    fn displays_immediate_parent_paths_with_one_parent_component() {
        let cwd = root_path("workspace/repo");
        assert_eq!(
            display(root_path("workspace/other/file.rs"), cwd).to_string(),
            Path::new("../other/file.rs").display().to_string()
        );
    }

    #[test]
    fn displays_more_distant_paths_as_absolute() {
        let cwd = root_path("home/user/repo");
        let distant = root_path("tmp/file.rs");
        assert_eq!(
            display(&distant, cwd).to_string(),
            distant.display().to_string()
        );
    }

    #[test]
    fn resolves_and_normalizes_relative_input() {
        let cwd = root_path("workspace/repo");
        assert_eq!(
            display("src/../tests/test.rs", cwd).to_string(),
            Path::new("tests/test.rs").display().to_string()
        );
    }
}
