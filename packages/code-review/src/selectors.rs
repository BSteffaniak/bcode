//! Path selectors as people and agents write them, turned into review sources.
//!
//! The command line (`bcode review files`, `bcode review repo --path`) and the assistant's
//! `code_review.create_review` take the same strings: `path`, `folder/`, a glob, or
//! `path:START-END` (see [`PathSelector::parse`]). Each is read relative to a base folder (the
//! process's working directory, or the repository root), must stay inside the repository, and,
//! unless it is a glob, must name something in the working tree. A plain path is a file or a
//! folder by what it is there.

use crate::data::git;
use bcode_code_review_models::{ParsedSelector, PathSelector, ReviewSourceKind};
use std::path::{Component, Path, PathBuf};

/// Selectors written relative to `base` (an absolute folder), as repository-relative selectors
/// of the repository at `root` (absolute, canonical).
///
/// # Errors
///
/// * A message naming the selector when it does not parse, leaves the repository, or names
///   nothing in the working tree.
pub fn parse_selectors(
    root: &Path,
    base: &Path,
    texts: &[String],
) -> Result<Vec<PathSelector>, String> {
    texts
        .iter()
        .map(|text| parse_selector(root, base, text))
        .collect()
}

/// One selector; see [`parse_selectors`].
///
/// # Errors
///
/// * See [`parse_selectors`].
pub fn parse_selector(root: &Path, base: &Path, text: &str) -> Result<PathSelector, String> {
    let written = text.trim();
    let relative = repository_relative(root, base, written)?;
    let parsed = PathSelector::parse(&relative).map_err(|error| error.to_string())?;
    let selector = parsed.classify(|path| is_folder(root, path));
    match &selector {
        PathSelector::Glob { pattern } => {
            git::validate_glob(pattern).map_err(|error| format!("{written}: {error}"))?;
        }
        PathSelector::File { path } | PathSelector::FileRange { path, .. } => {
            git::validate_relative_path(path).map_err(|error| format!("{written}: {error}"))?;
            let full = root.join(path);
            if is_folder(root, path) {
                return Err(format!("{written}: is a folder, not a file"));
            }
            if std::fs::symlink_metadata(&full).is_err() {
                return Err(format!("{written}: no such file or folder"));
            }
        }
        PathSelector::Folder { path } => {
            git::validate_relative_path(path).map_err(|error| format!("{written}: {error}"))?;
            if !is_folder(root, path) {
                return Err(format!("{written}: no such folder"));
            }
        }
    }
    Ok(selector)
}

/// Whether `path` is a folder of the repository's working tree. A submodule is not: git lists
/// it as one entry (its commit), so it is reviewed as a file.
fn is_folder(root: &Path, path: &str) -> bool {
    root.join(path).is_dir() && !is_gitlink(root, path)
}

/// Whether the index records `path` as a submodule (mode `160000`).
fn is_gitlink(root: &Path, path: &str) -> bool {
    git::output(root, &["ls-files", "--stage", "-z", "--", path]).is_ok_and(|output| {
        git::split_nul(&output).iter().any(|record| {
            record
                .split_once('\t')
                .is_some_and(|(meta, listed)| listed == path && meta.starts_with("160000 "))
        })
    })
}

/// `text` relative to the repository root: joined to `base` and normalized without touching
/// the file system, so a glob or a range suffix survives. A trailing `/` is kept.
fn repository_relative(root: &Path, base: &Path, text: &str) -> Result<String, String> {
    if text.is_empty() {
        return Err("an empty path selects nothing".to_owned());
    }
    let joined = base.join(text);
    let mut normal = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                if !normal.pop() {
                    return Err(format!("{text}: leaves the repository"));
                }
            }
            Component::CurDir => {}
            other => normal.push(other.as_os_str()),
        }
    }
    let relative = normal
        .strip_prefix(root)
        .map_err(|_| format!("{text}: is outside the repository {}", root.display()))?;
    let mut relative = relative.to_string_lossy().replace('\\', "/");
    if relative.is_empty() {
        return Err(format!(
            "{text}: names the whole repository; review the repository instead"
        ));
    }
    if text.ends_with('/') {
        relative.push('/');
    }
    Ok(relative)
}

/// The source reviewing `include` minus `exclude` as it is now: one plain file or range alone
/// stays a `File` or `FileRange` (which older builds open), anything else is a `FileSet`.
#[must_use]
pub fn files_source(include: Vec<PathSelector>, exclude: Vec<PathSelector>) -> ReviewSourceKind {
    match (include.as_slice(), exclude.is_empty()) {
        ([PathSelector::File { path }], true) => ReviewSourceKind::File { path: path.clone() },
        ([PathSelector::FileRange { path, start, end }], true) => ReviewSourceKind::FileRange {
            path: path.clone(),
            start: *start,
            end: *end,
        },
        _ => ReviewSourceKind::FileSet { include, exclude },
    }
}

/// The source reviewing the whole repository, or only `folders` of it.
///
/// # Errors
///
/// * A message naming a selector that is not a folder.
pub fn repository_source(folders: Vec<PathSelector>) -> Result<ReviewSourceKind, String> {
    if folders.is_empty() {
        return Ok(ReviewSourceKind::Repository);
    }
    if let Some(other) = folders
        .iter()
        .find(|selector| !matches!(selector, PathSelector::Folder { .. }))
    {
        return Err(format!(
            "{other}: a repository review is limited to folders; review files instead"
        ));
    }
    Ok(ReviewSourceKind::FileSet {
        include: folders,
        exclude: Vec::new(),
    })
}

/// Whether `text` would parse as a glob.
#[must_use]
pub fn is_glob(text: &str) -> bool {
    matches!(
        PathSelector::parse(text),
        Ok(ParsedSelector::Selector(PathSelector::Glob { .. }))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::fixture::GitFixture;

    fn repository() -> GitFixture {
        let fixture = GitFixture::empty();
        for path in [
            "examples/hello-plugin/src/lib.rs",
            "examples/hello-plugin/Cargo.toml",
            "README.md",
        ] {
            let full = fixture.root().join(path);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
            std::fs::write(full, "x\n").expect("write");
        }
        fixture
    }

    #[test]
    fn selectors_are_read_relative_to_the_base_and_classified_by_the_working_tree() {
        let fixture = repository();
        let root = fixture.root();
        let parse = |base: &Path, text: &str| parse_selector(root, base, text);
        assert_eq!(
            parse(root, "examples/hello-plugin"),
            Ok(PathSelector::Folder {
                path: "examples/hello-plugin".into()
            })
        );
        let inside = root.join("examples/hello-plugin");
        assert_eq!(
            parse(&inside, "src/lib.rs:2-1"),
            Ok(PathSelector::FileRange {
                path: "examples/hello-plugin/src/lib.rs".into(),
                start: 1,
                end: 2
            })
        );
        assert_eq!(
            parse(&inside, "../hello-plugin/"),
            Ok(PathSelector::Folder {
                path: "examples/hello-plugin".into()
            })
        );
        assert_eq!(
            parse(&inside, "**/*.rs"),
            Ok(PathSelector::Glob {
                pattern: "examples/hello-plugin/**/*.rs".into()
            })
        );
        let absolute = root.join("README.md").display().to_string();
        assert_eq!(
            parse(Path::new("/"), &absolute),
            Ok(PathSelector::File {
                path: "README.md".into()
            })
        );
        assert_eq!(
            parse(root, "examples/helo-plugin"),
            Err("examples/helo-plugin: no such file or folder".to_owned())
        );
        assert_eq!(
            parse(root, "README.md/"),
            Err("README.md/: no such folder".to_owned())
        );
        assert!(parse(root, "../outside").is_err());
        assert!(parse(root, ".").is_err());
        assert!(parse(root, "/etc/passwd").is_err());
    }

    #[test]
    fn one_plain_file_stays_a_file_source_and_repositories_take_folders() {
        let folder = PathSelector::Folder { path: "a".into() };
        let file = PathSelector::File { path: "b".into() };
        assert_eq!(
            files_source(vec![file.clone()], Vec::new()),
            ReviewSourceKind::File { path: "b".into() }
        );
        assert!(matches!(
            files_source(vec![folder.clone()], Vec::new()),
            ReviewSourceKind::FileSet { .. }
        ));
        assert_eq!(
            repository_source(Vec::new()),
            Ok(ReviewSourceKind::Repository)
        );
        assert!(repository_source(vec![folder]).is_ok());
        assert!(repository_source(vec![file]).is_err());
        assert!(is_glob("src/*.rs") && !is_glob("src/lib.rs"));
    }
}
