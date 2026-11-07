//! Which worktree paths git ignores, so their changes never reach git.
//!
//! This is only a pre-filter: git still decides status. When unsure (an unreadable ignore
//! file, an excludes file configured only in the repository), a path is kept, never
//! dropped, so the filter can cost work but not correctness.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// Directories whose ignore files are remembered before the cache starts over.
const CACHED_DIRECTORIES: usize = 4096;

/// Ignore rules of one worktree, loaded lazily per directory.
pub struct IgnoreFilter {
    /// Worktree root (as change events name it).
    root: PathBuf,
    /// `info/exclude` of the repository.
    exclude: PathBuf,
    /// `info/exclude` and the global excludes file: the lowest precedence.
    base: Gitignore,
    /// `.gitignore` of each directory asked about (relative), `None` when it has none.
    directories: BTreeMap<PathBuf, Option<Gitignore>>,
    /// Whether each directory asked about is ignored (relative).
    ignored: BTreeMap<PathBuf, bool>,
}

impl IgnoreFilter {
    /// Rules for the worktree at `root` of the repository whose common git directory is
    /// `common_dir`.
    pub fn new(root: &Path, common_dir: &Path) -> Self {
        let exclude = common_dir.join("info").join("exclude");
        Self {
            base: base_rules(root, &exclude),
            root: root.to_owned(),
            exclude,
            directories: BTreeMap::new(),
            ignored: BTreeMap::new(),
        }
    }

    /// Forget every loaded rule, after an ignore file changed.
    pub fn reset(&mut self) {
        self.base = base_rules(&self.root, &self.exclude);
        self.directories.clear();
        self.ignored.clear();
    }

    /// Whether git ignores `relative` (a path under the root). A path inside an ignored
    /// directory is ignored whatever deeper rules say, as in git.
    pub fn is_ignored(&mut self, relative: &Path, is_dir: bool) -> bool {
        let mut ancestors: Vec<&Path> = relative
            .ancestors()
            .skip(1)
            .filter(|ancestor| !ancestor.as_os_str().is_empty())
            .collect();
        ancestors.reverse();
        if ancestors
            .into_iter()
            .any(|ancestor| self.directory_ignored(ancestor))
        {
            return true;
        }
        self.decide(relative, is_dir) == Some(true)
    }

    fn directory_ignored(&mut self, relative: &Path) -> bool {
        if let Some(ignored) = self.ignored.get(relative) {
            return *ignored;
        }
        if self.ignored.len() >= CACHED_DIRECTORIES {
            self.ignored.clear();
        }
        let ignored = self.decide(relative, true) == Some(true);
        self.ignored.insert(relative.to_owned(), ignored);
        ignored
    }

    /// The deciding rule for `relative` itself: the deepest `.gitignore` with a matching
    /// pattern wins, then the base rules. `None` when nothing matches.
    fn decide(&mut self, relative: &Path, is_dir: bool) -> Option<bool> {
        let path = self.root.join(relative);
        let mut directory = relative.parent();
        while let Some(current) = directory {
            if let Some(rules) = self.directory_rules(current) {
                match rules.matched(&path, is_dir) {
                    Match::Ignore(_) => return Some(true),
                    Match::Whitelist(_) => return Some(false),
                    Match::None => {}
                }
            }
            directory = current.parent();
        }
        match self.base.matched(&path, is_dir) {
            Match::Ignore(_) => Some(true),
            Match::Whitelist(_) => Some(false),
            Match::None => None,
        }
    }

    fn directory_rules(&mut self, relative: &Path) -> Option<&Gitignore> {
        if !self.directories.contains_key(relative) {
            if self.directories.len() >= CACHED_DIRECTORIES {
                self.directories.clear();
            }
            let directory = self.root.join(relative);
            let file = directory.join(".gitignore");
            let rules = file.is_file().then(|| {
                let mut builder = GitignoreBuilder::new(&directory);
                // An unreadable file adds no rules, so its paths are kept.
                let _ = builder.add(&file);
                builder.build().unwrap_or_else(|_| Gitignore::empty())
            });
            self.directories.insert(relative.to_owned(), rules);
        }
        self.directories.get(relative).and_then(Option::as_ref)
    }
}

fn base_rules(root: &Path, exclude: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    for file in
        std::iter::once(exclude.to_owned()).chain(ignore::gitignore::gitconfig_excludes_path())
    {
        if file.is_file() {
            let _ = builder.add(file);
        }
    }
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_git_precedence_and_ignored_directories() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let common = root.join(".git");
        std::fs::create_dir_all(common.join("info")).unwrap();
        std::fs::write(common.join("info/exclude"), "*.local\n").unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
        std::fs::create_dir_all(root.join("src/keep")).unwrap();
        std::fs::write(root.join("src/.gitignore"), "!keep.log\n").unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::write(root.join("target/.gitignore"), "!*\n").unwrap();

        let mut filter = IgnoreFilter::new(root, &common);
        assert!(filter.is_ignored(Path::new("target"), true));
        // Nothing inside an ignored directory comes back, whatever deeper rules say.
        assert!(filter.is_ignored(Path::new("target/debug/out.o"), false));
        assert!(filter.is_ignored(Path::new("app.log"), false));
        assert!(!filter.is_ignored(Path::new("src/keep.log"), false));
        assert!(filter.is_ignored(Path::new("notes.local"), false));
        assert!(!filter.is_ignored(Path::new("src/main.rs"), false));

        std::fs::write(root.join(".gitignore"), "").unwrap();
        filter.reset();
        assert!(!filter.is_ignored(Path::new("target/debug/out.o"), false));
    }
}
