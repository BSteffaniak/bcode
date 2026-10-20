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
    normalize_into(path, &mut normalized);
    normalized
}

fn normalize_into(path: &Path, normalized: &mut PathBuf) {
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
}

/// Structural ceilings for lexical path display. No component collection is allocated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathDisplayLimits {
    /// Aggregate capacity of simultaneously owned path and output buffers.
    pub scalar_capacity_bytes: usize,
    /// Collection slots (zero for this implementation).
    pub collection_entries: usize,
    /// Owned path/string nodes, including the result envelope.
    pub value_nodes: usize,
    /// Maximum owned structure depth.
    pub depth: usize,
}

/// Pre-allocation peak reservation for bounded path formatting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathDisplayUsage {
    /// Sum of all temporary path capacities and final formatted output capacity.
    pub scalar_capacity_bytes: usize,
    /// Component iterator storage is borrowed, so this is zero.
    pub collection_entries: usize,
    /// Four path buffers, one output string and its result envelope.
    pub value_nodes: usize,
    /// Result envelope and its scalar children.
    pub depth: usize,
}

/// Display text together with the caller's peak admission guard.
///
/// Temporary paths are gone on return. The caller may reconcile its guard to
/// `output.capacity()` before retaining the output; dropping the result drops
/// the admission guard. Admission callbacks must be transactional on rejection.
#[derive(Debug)]
pub struct BoundedPathDisplay<G> {
    /// Exactly the same display text as [`display`].
    pub output: String,
    /// Caller-owned reservation, deliberately retained through handoff.
    pub admission: G,
}

/// Bounded path display rejection.
#[derive(Debug, PartialEq, Eq)]
pub enum PathDisplayError<E> {
    /// Structural limit or checked arithmetic exhausted before allocation.
    LimitReached,
    /// Caller refused the peak reservation before allocation.
    Admission(E),
}

#[derive(Default)]
struct DisplayByteCount(usize);

impl fmt::Write for DisplayByteCount {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.0 = self.0.checked_add(text.len()).ok_or(fmt::Error)?;
        Ok(())
    }
}

fn component_budget(path: &Path) -> Option<(usize, usize)> {
    let mut bytes = 0usize;
    let mut formatted = DisplayByteCount::default();
    for component in path.components() {
        // One separator per component is an upper bound even for prefixes,
        // root components and components removed by lexical normalization.
        bytes = bytes
            .checked_add(component.as_os_str().as_encoded_bytes().len())?
            .checked_add(1)?;
        fmt::write(
            &mut formatted,
            format_args!("{}/", component.as_os_str().display()),
        )
        .ok()?;
    }
    Some((bytes, formatted.0))
}

struct FixedDisplayOutput(String);

impl fmt::Write for FixedDisplayOutput {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if text.len() > self.0.capacity().saturating_sub(self.0.len()) {
            return Err(fmt::Error);
        }
        self.0.push_str(text);
        Ok(())
    }
}

/// Format a borrowed path with lexical normalization and pre-allocation admission.
///
/// Preserves [`display`]'s descendant, immediate-parent and absolute-path policy.
/// Preflight uses borrowed component iterators and a counting formatter, with no
/// temporary paths or component vectors. The callback reserves all four path
/// buffers plus the final formatted string before their first allocation.
/// Fresh buffers are preallocated for every push/join, avoiding amortized growth.
/// No session-domain dependency or persistence is involved.
///
/// # Errors
/// Returns [`PathDisplayError::LimitReached`] on counter overflow, exhausted
/// structural limits or formatting overflow, or [`PathDisplayError::Admission`]
/// when the caller rejects the peak. The callback must leave its ledger unchanged
/// on rejection; its returned guard handles release on any later failure.
pub fn display_bounded<G, E>(
    path: &Path,
    working_directory: &Path,
    limits: PathDisplayLimits,
    admit: impl FnOnce(PathDisplayUsage) -> Result<G, E>,
) -> Result<BoundedPathDisplay<G>, PathDisplayError<E>> {
    let plan = (|| {
        let (cwd_bytes, cwd_output) = component_budget(working_directory)?;
        let (path_bytes, path_output) = component_budget(path)?;
        // join retains the input spelling until the second normalization pass.
        let joined_bytes = cwd_bytes
            .checked_add(path.as_os_str().as_encoded_bytes().len())?
            .checked_add(1)?;
        let normalized_bytes = cwd_bytes.checked_add(path_bytes)?.checked_add(1)?;
        // The relative presentation can add exactly one "../" prefix.
        let selected_bytes = normalized_bytes.checked_add(3)?;
        let output_bytes = cwd_output.checked_add(path_output)?.checked_add(3)?;
        let scalar_capacity_bytes = cwd_bytes
            .checked_add(joined_bytes)?
            .checked_add(normalized_bytes)?
            .checked_add(selected_bytes)?
            .checked_add(output_bytes)?;
        Some((
            cwd_bytes,
            joined_bytes,
            normalized_bytes,
            selected_bytes,
            output_bytes,
            scalar_capacity_bytes,
        ))
    })()
    .ok_or(PathDisplayError::LimitReached)?;
    let (
        cwd_bytes,
        joined_bytes,
        normalized_bytes,
        selected_bytes,
        output_bytes,
        scalar_capacity_bytes,
    ) = plan;
    let usage = PathDisplayUsage {
        scalar_capacity_bytes,
        collection_entries: 0,
        value_nodes: 6,
        depth: 2,
    };
    if usage.scalar_capacity_bytes > limits.scalar_capacity_bytes
        || usage.collection_entries > limits.collection_entries
        || usage.value_nodes > limits.value_nodes
        || usage.depth > limits.depth
    {
        return Err(PathDisplayError::LimitReached);
    }
    let admission = admit(usage).map_err(PathDisplayError::Admission)?;
    let mut cwd = PathBuf::with_capacity(cwd_bytes);
    normalize_into(working_directory, &mut cwd);
    let mut joined = PathBuf::with_capacity(joined_bytes);
    let mut normalized = PathBuf::with_capacity(normalized_bytes);
    if path.is_absolute() {
        normalize_into(path, &mut normalized);
    } else {
        joined.push(&cwd);
        joined.push(path);
        normalize_into(&joined, &mut normalized);
    }
    let mut selected = PathBuf::with_capacity(selected_bytes);
    if let Ok(relative) = normalized.strip_prefix(&cwd) {
        if relative.as_os_str().is_empty() {
            selected.push(".");
        } else {
            selected.push(relative);
        }
    } else if let Some(parent) = cwd.parent()
        && let Ok(relative) = normalized.strip_prefix(parent)
    {
        selected.push("..");
        selected.push(relative);
    } else {
        selected.push(&normalized);
    }
    let mut output = FixedDisplayOutput(String::with_capacity(output_bytes));
    fmt::write(&mut output, format_args!("{}", selected.display()))
        .map_err(|_| PathDisplayError::LimitReached)?;
    Ok(BoundedPathDisplay {
        output: output.0,
        admission,
    })
}

#[cfg(test)]
mod tests {
    use super::{PathDisplayError, PathDisplayLimits, display, display_bounded};
    use std::path::{Path, PathBuf};

    fn bounded_limits() -> PathDisplayLimits {
        PathDisplayLimits {
            scalar_capacity_bytes: 65536,
            collection_entries: 0,
            value_nodes: 6,
            depth: 2,
        }
    }

    #[test]
    fn bounded_display_preserves_unicode_relative_and_home_normalization() {
        for (path, cwd) in [
            ("src/../資料/🦀.rs", "/home/user/repo"),
            ("/home/user/other/./file", "/home/user/repo/../repo"),
            ("/tmp/../var/file", "/home/user/repo"),
            (".", "/home/user/repo"),
            ("~/../資料", "relative/./repo"),
            ("../../file", "relative/repo"),
            ("", ""),
        ] {
            let mut admitted = None;
            let result =
                display_bounded(Path::new(path), Path::new(cwd), bounded_limits(), |usage| {
                    admitted = Some(usage);
                    Ok::<_, ()>(usage)
                })
                .unwrap();
            assert_eq!(result.output, display(path, cwd).to_string());
            assert_eq!(admitted, Some(result.admission));
            assert!(result.output.capacity() <= result.admission.scalar_capacity_bytes);
            assert_eq!(result.admission.collection_entries, 0);
        }
    }

    #[test]
    fn bounded_peak_rejection_leaves_caller_ledger_unchanged() {
        let path = Path::new("資料/../🦀/file");
        let cwd = Path::new("/home/user/repo");
        let peak = display_bounded(path, cwd, bounded_limits(), Ok::<_, ()>)
            .unwrap()
            .admission;
        let mut ledger = 17usize;
        let mut limits = bounded_limits();
        limits.scalar_capacity_bytes = peak.scalar_capacity_bytes - 1;
        let result = display_bounded(path, cwd, limits, |usage| {
            ledger += usage.scalar_capacity_bytes;
            Ok::<_, ()>(())
        });
        assert!(matches!(result, Err(PathDisplayError::LimitReached)));
        assert_eq!(ledger, 17);
        let result = display_bounded(path, cwd, bounded_limits(), |usage| {
            let proposed = ledger.checked_add(usage.scalar_capacity_bytes).unwrap();
            if proposed > 17 {
                return Err("peak rejected");
            }
            ledger = proposed;
            Ok(())
        });
        assert!(matches!(
            result,
            Err(PathDisplayError::Admission("peak rejected"))
        ));
        assert_eq!(ledger, 17);
        limits.scalar_capacity_bytes += 1;
        let result = display_bounded(path, cwd, limits, Ok::<_, ()>).unwrap();
        assert_eq!(result.output, display(path, cwd).to_string());
    }

    #[test]
    fn bounded_guard_releases_peak_on_drop() {
        struct Guard<'a>(&'a std::cell::Cell<usize>, usize);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                self.0.set(self.0.get() - self.1);
            }
        }
        let ledger = std::cell::Cell::new(0);
        let result = display_bounded(
            Path::new("../資料"),
            Path::new("/home/user/repo"),
            bounded_limits(),
            |usage| {
                ledger.set(usage.scalar_capacity_bytes);
                Ok::<_, ()>(Guard(&ledger, usage.scalar_capacity_bytes))
            },
        )
        .unwrap();
        assert!(ledger.get() >= result.output.capacity());
        drop(result);
        assert_eq!(ledger.get(), 0);
    }

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
