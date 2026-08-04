//! Bounded external theme discovery.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::definition::{MAX_THEME_FILE_BYTES, ThemeCatalog, ThemeError, parse_theme_definition};

/// Maximum candidate theme files discovered in one refresh.
pub const MAX_DISCOVERED_THEME_FILES: usize = 256;
/// Maximum aggregate theme bytes read in one refresh.
pub const MAX_DISCOVERED_THEME_BYTES: usize = 4 * 1024 * 1024;

/// Origin and precedence class for a discovered theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ThemeSourceKind {
    /// User configuration directory.
    User,
    /// Repository-local `.bcode/themes` directory.
    Project,
    /// Explicit authorized file/directory from configuration.
    Explicit,
}

/// One safe theme discovery root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeDiscoveryRoot {
    /// Root path or exact file path.
    pub path: PathBuf,
    /// Root precedence class.
    pub kind: ThemeSourceKind,
}

impl ThemeDiscoveryRoot {
    /// Create a discovery root.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, kind: ThemeSourceKind) -> Self {
        Self {
            path: path.into(),
            kind,
        }
    }
}

/// Bounded diagnostic from one ignored external theme candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeDiscoveryDiagnostic {
    /// Candidate path.
    pub path: PathBuf,
    /// Secret-safe failure description.
    pub message: String,
}

/// Result of one deterministic theme discovery pass.
#[derive(Debug, Clone)]
pub struct DiscoveredThemes {
    /// Bundled and valid external definitions in final precedence order.
    pub catalog: ThemeCatalog,
    /// Accepted external source path keyed by final stable theme id.
    pub sources: BTreeMap<String, PathBuf>,
    /// Invalid or unsafe candidates that were skipped.
    pub diagnostics: Vec<ThemeDiscoveryDiagnostic>,
}

/// Discover the default user/project roots plus configured explicit roots.
///
/// Root order is bundled, user, project, explicit. Later valid definitions
/// replace earlier definitions with the same stable id.
#[must_use]
pub fn default_theme_roots(
    user_config_dir: &Path,
    project_root: &Path,
    explicit: &[PathBuf],
) -> Vec<ThemeDiscoveryRoot> {
    let mut roots = vec![
        ThemeDiscoveryRoot::new(user_config_dir.join("themes"), ThemeSourceKind::User),
        ThemeDiscoveryRoot::new(
            project_root.join(".bcode").join("themes"),
            ThemeSourceKind::Project,
        ),
    ];
    roots.extend(
        explicit
            .iter()
            .cloned()
            .map(|path| ThemeDiscoveryRoot::new(path, ThemeSourceKind::Explicit)),
    );
    roots
}

/// Discover valid theme definitions from confined roots.
///
/// Missing roots are ignored. Candidate files must be regular `.toml` files.
/// Directory roots are shallow by design: every accepted candidate must remain
/// directly under the canonicalized authorized root. Symlinked candidates are
/// accepted only when their canonical target remains under that root.
///
/// # Errors
///
/// Returns an error only when embedded bundled themes are invalid. External
/// damage is represented by bounded diagnostics so one malformed custom theme
/// cannot prevent use of the remaining catalog.
pub fn discover_themes(roots: &[ThemeDiscoveryRoot]) -> Result<DiscoveredThemes, ThemeError> {
    let mut catalog = ThemeCatalog::bundled()?;
    let mut sources = BTreeMap::new();
    let mut diagnostics = Vec::new();
    let mut candidates = Vec::new();
    for root in roots {
        collect_root_candidates(root, &mut candidates, &mut diagnostics);
    }
    candidates.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.path.cmp(&right.path))
    });
    if candidates.len() > MAX_DISCOVERED_THEME_FILES {
        diagnostics.push(ThemeDiscoveryDiagnostic {
            path: PathBuf::new(),
            message: format!(
                "theme candidate limit exceeded; only the first {MAX_DISCOVERED_THEME_FILES} candidates were inspected"
            ),
        });
        candidates.truncate(MAX_DISCOVERED_THEME_FILES);
    }

    let mut aggregate_bytes = 0_usize;
    for candidate in candidates {
        let metadata = match std::fs::metadata(&candidate.path) {
            Ok(metadata) => metadata,
            Err(error) => {
                push_diagnostic(
                    &mut diagnostics,
                    &candidate.path,
                    format!("metadata: {error}"),
                );
                continue;
            }
        };
        let Ok(file_bytes) = usize::try_from(metadata.len()) else {
            push_diagnostic(
                &mut diagnostics,
                &candidate.path,
                "file size is unsupported",
            );
            continue;
        };
        if file_bytes > MAX_THEME_FILE_BYTES {
            push_diagnostic(
                &mut diagnostics,
                &candidate.path,
                format!("file exceeds the {MAX_THEME_FILE_BYTES}-byte limit"),
            );
            continue;
        }
        if aggregate_bytes.saturating_add(file_bytes) > MAX_DISCOVERED_THEME_BYTES {
            push_diagnostic(
                &mut diagnostics,
                &candidate.path,
                format!("aggregate theme byte limit {MAX_DISCOVERED_THEME_BYTES} reached"),
            );
            break;
        }
        aggregate_bytes = aggregate_bytes.saturating_add(file_bytes);
        let text = match std::fs::read_to_string(&candidate.path) {
            Ok(text) => text,
            Err(error) => {
                push_diagnostic(&mut diagnostics, &candidate.path, format!("read: {error}"));
                continue;
            }
        };
        match parse_theme_definition(candidate.path.display().to_string(), &text) {
            Ok(definition) => {
                let id = definition.id().to_owned();
                catalog.insert(definition);
                sources.insert(id, candidate.path);
            }
            Err(error) => push_diagnostic(&mut diagnostics, &candidate.path, error.to_string()),
        }
    }
    Ok(DiscoveredThemes {
        catalog,
        sources,
        diagnostics,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    path: PathBuf,
    kind: ThemeSourceKind,
}

fn collect_root_candidates(
    root: &ThemeDiscoveryRoot,
    candidates: &mut Vec<Candidate>,
    diagnostics: &mut Vec<ThemeDiscoveryDiagnostic>,
) {
    if !root.path.exists() {
        return;
    }
    let canonical_root = match std::fs::canonicalize(&root.path) {
        Ok(path) => path,
        Err(error) => {
            push_diagnostic(diagnostics, &root.path, format!("root: {error}"));
            return;
        }
    };
    if canonical_root.is_file() {
        push_candidate(
            &root.path,
            canonical_root.parent().unwrap_or_else(|| Path::new("/")),
            root.kind,
            candidates,
            diagnostics,
        );
        return;
    }
    if !canonical_root.is_dir() {
        push_diagnostic(diagnostics, &root.path, "root is not a file or directory");
        return;
    }
    let entries = match std::fs::read_dir(&canonical_root) {
        Ok(entries) => entries,
        Err(error) => {
            push_diagnostic(diagnostics, &root.path, format!("directory: {error}"));
            return;
        }
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        push_candidate(&path, &canonical_root, root.kind, candidates, diagnostics);
    }
}

fn push_candidate(
    path: &Path,
    canonical_root: &Path,
    kind: ThemeSourceKind,
    candidates: &mut Vec<Candidate>,
    diagnostics: &mut Vec<ThemeDiscoveryDiagnostic>,
) {
    if path.extension().and_then(std::ffi::OsStr::to_str) != Some("toml") {
        return;
    }
    let canonical = match std::fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(error) => {
            push_diagnostic(diagnostics, path, format!("candidate: {error}"));
            return;
        }
    };
    if !canonical.starts_with(canonical_root) {
        push_diagnostic(diagnostics, path, "candidate escapes its authorized root");
        return;
    }
    if !canonical.is_file() {
        push_diagnostic(diagnostics, path, "candidate is not a regular file");
        return;
    }
    candidates.push(Candidate {
        path: canonical,
        kind,
    });
}

fn push_diagnostic(
    diagnostics: &mut Vec<ThemeDiscoveryDiagnostic>,
    path: &Path,
    message: impl Into<String>,
) {
    if diagnostics.len() < MAX_DISCOVERED_THEME_FILES {
        diagnostics.push(ThemeDiscoveryDiagnostic {
            path: path.to_path_buf(),
            message: message.into(),
        });
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use bmux_tui::style::Color;
    use tempfile::tempdir;

    use super::{ThemeDiscoveryRoot, ThemeSourceKind, default_theme_roots, discover_themes};
    use crate::theme::definition::ThemeSelection;

    fn theme(id: &str, accent: &str) -> String {
        format!("schema_version = 1\nid = \"{id}\"\n[palette]\naccent = \"{accent}\"\n")
    }

    #[test]
    fn default_roots_have_expected_precedence() {
        let roots = default_theme_roots(
            std::path::Path::new("/user/bcode"),
            std::path::Path::new("/project"),
            &[std::path::PathBuf::from("/explicit")],
        );
        assert_eq!(roots[0].kind, ThemeSourceKind::User);
        assert_eq!(roots[1].kind, ThemeSourceKind::Project);
        assert_eq!(roots[2].kind, ThemeSourceKind::Explicit);
        assert_eq!(
            roots[1].path,
            std::path::Path::new("/project/.bcode/themes")
        );
    }

    #[test]
    fn later_valid_sources_override_and_invalid_files_are_diagnostics() {
        let temp = tempdir().expect("tempdir");
        let user = temp.path().join("user");
        let project = temp.path().join("project");
        fs::create_dir_all(&user).expect("user dir");
        fs::create_dir_all(&project).expect("project dir");
        fs::write(user.join("same.toml"), theme("same", "#112233")).expect("user theme");
        fs::write(project.join("same.toml"), theme("same", "#445566")).expect("project theme");
        fs::write(project.join("bad.toml"), "not = [valid").expect("bad theme");

        let discovered = discover_themes(&[
            ThemeDiscoveryRoot::new(&user, ThemeSourceKind::User),
            ThemeDiscoveryRoot::new(&project, ThemeSourceKind::Project),
        ])
        .expect("discovery succeeds");
        let resolved = discovered
            .catalog
            .resolve(&ThemeSelection::new("same"))
            .expect("same resolves");

        assert_eq!(resolved.color("accent"), Some(Color::Rgb(68, 85, 102)));
        assert_eq!(discovered.diagnostics.len(), 1);
        assert!(
            discovered.sources["same"]
                .parent()
                .is_some_and(|parent| parent.ends_with("project"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("root");
        fs::create_dir_all(&root).expect("root");
        let outside = temp.path().join("outside.toml");
        fs::write(&outside, theme("outside", "#112233")).expect("outside");
        symlink(&outside, root.join("escape.toml")).expect("symlink");

        let discovered =
            discover_themes(&[ThemeDiscoveryRoot::new(&root, ThemeSourceKind::Explicit)])
                .expect("discovery succeeds");

        assert_eq!(discovered.diagnostics.len(), 1);
        assert!(
            discovered
                .catalog
                .resolve(&ThemeSelection::new("outside"))
                .is_err()
        );
    }
}
