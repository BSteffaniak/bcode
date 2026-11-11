//! Deterministic temporary git repositories for code review tests and benchmarks.
//!
//! A fixture repository contains:
//!
//! * an initial commit (branch `fixture/base` points at it),
//! * a second commit on `main` that modifies many files and also adds, deletes, and renames
//!   files, changes a binary file, rewrites part of a very large file, and edits a file with
//!   CJK, emoji, combining marks, tabs, and very long lines,
//! * extra branches at both commits,
//! * a linear chain of three stacked branches (`fixture/stack-1` .. `fixture/stack-3`), each
//!   based on the previous one and the first based on `main`; every layer adds its own file and
//!   rewrites a distinct region of one shared file, so per-layer attribution is observable,
//! * staged and unstaged working-tree changes on `main` (one file has both) and one untracked
//!   file,
//! * optionally ([`FixtureSpec::special_entries`]) a file whose mode changes from `100644` to
//!   `100755` without a content change, a CRLF file with one edited line, an added symbolic
//!   link, and an added submodule gitlink (with `.gitmodules`).
//!
//! Commits and branches are streamed through one `git fast-import` process (a single pack, no
//! per-file loose objects), then `main` is checked out with `git reset --hard` and the
//! working-tree changes are written directly, so the 5k-file benchmark preset generates in a few
//! seconds.

use std::fmt::Write as _;
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};

/// Number of consecutive lines rewritten per change group (one hunk per group).
const CHANGE_GROUP_LINES: usize = 5;
/// Minimum distance between change-group starts so that groups produce separate hunks.
const MIN_CHANGE_STRIDE: usize = CHANGE_GROUP_LINES + 8;
/// Character count of the deliberately overlong line in the unicode file.
const LONG_LINE_CHARS: usize = 3_000;

/// Shape of a generated fixture repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixtureSpec {
    /// Ordinary text files modified by the second commit.
    pub modified_files: usize,
    /// Lines per ordinary text file.
    pub lines_per_file: usize,
    /// Lines rewritten per modified file; each yields one deletion and one addition.
    pub changed_lines_per_file: usize,
    /// Files added by the second commit.
    pub added_files: usize,
    /// Files deleted by the second commit.
    pub deleted_files: usize,
    /// Files renamed (with a one-line edit) by the second commit.
    pub renamed_files: usize,
    /// Lines in the very large file; a quarter of them are rewritten by the second commit.
    pub large_file_lines: usize,
    /// Modified files with additional staged edits on top of `main`.
    pub staged_files: usize,
    /// Modified files with additional unstaged edits on top of `main`.
    pub unstaged_files: usize,
    /// Lines rewritten per staged or unstaged working-tree file.
    pub worktree_changed_lines_per_file: usize,
    /// Extra branches unrelated to the stack.
    pub extra_branches: usize,
    /// Add the mode-change, CRLF, symbolic link, and submodule gitlink entries.
    pub special_entries: bool,
}

impl FixtureSpec {
    /// A tiny repository for fast unit and scenario tests.
    #[must_use]
    pub const fn small() -> Self {
        Self {
            modified_files: 4,
            lines_per_file: 40,
            changed_lines_per_file: 6,
            added_files: 1,
            deleted_files: 1,
            renamed_files: 1,
            large_file_lines: 400,
            staged_files: 1,
            unstaged_files: 1,
            worktree_changed_lines_per_file: 2,
            extra_branches: 2,
            special_entries: true,
        }
    }

    /// The phase 0 benchmark repository: 5k modified files and 500k changed diff lines
    /// (250k additions plus 250k deletions) in the second commit, plus the special files.
    #[must_use]
    pub const fn benchmark() -> Self {
        Self {
            modified_files: 5_000,
            lines_per_file: 200,
            changed_lines_per_file: 50,
            added_files: 20,
            deleted_files: 20,
            renamed_files: 20,
            large_file_lines: 40_000,
            staged_files: 500,
            unstaged_files: 500,
            worktree_changed_lines_per_file: 10,
            extra_branches: 8,
            // Kept off so the phase 0 V1 baseline numbers stay comparable.
            special_entries: false,
        }
    }

    /// Diff lines (additions plus deletions) contributed by ordinary modified files in the
    /// second commit, excluding the special files.
    #[must_use]
    pub const fn modified_diff_lines(&self) -> usize {
        self.modified_files * 2 * min_usize(self.changed_lines_per_file, self.lines_per_file)
    }
}

const fn min_usize(left: usize, right: usize) -> usize {
    if left < right { left } else { right }
}

/// A generated repository; the directory is removed when this value is dropped.
#[derive(Debug)]
pub struct GitFixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    spec: FixtureSpec,
}

/// Branch holding the initial commit.
pub const BASE_BRANCH: &str = "fixture/base";
/// Branch holding the second commit; checked out with working-tree changes.
pub const HEAD_BRANCH: &str = "main";
/// Stacked branches from bottom to top.
pub const STACK_BRANCHES: [&str; 3] = ["fixture/stack-1", "fixture/stack-2", "fixture/stack-3"];
/// Path of the very large file.
pub const LARGE_FILE: &str = "large/huge.rs";
/// Path of the file with CJK, emoji, combining marks, tabs, and long lines.
pub const UNICODE_FILE: &str = "docs/unicode.txt";
/// Path of the binary file.
pub const BINARY_FILE: &str = "assets/blob.bin";
/// Path of the file every stack layer edits in a distinct region.
pub const STACK_SHARED_FILE: &str = "stack/shared.rs";
/// Lines in [`STACK_SHARED_FILE`].
const STACK_SHARED_LINES: usize = 80;
/// Path of the untracked working-tree file.
pub const UNTRACKED_FILE: &str = "untracked/note.txt";
/// Path of the file whose mode changes to executable in the second commit.
pub const MODE_CHANGE_FILE: &str = "scripts/run.sh";
/// Path of the CRLF file edited in the second commit.
pub const CRLF_FILE: &str = "text/crlf.txt";
/// Path of the symbolic link added in the second commit.
pub const SYMLINK_FILE: &str = "links/huge-link";
/// Target of [`SYMLINK_FILE`].
pub const SYMLINK_TARGET: &str = "../large/huge.rs";
/// Path of the submodule gitlink added in the second commit.
pub const SUBMODULE_PATH: &str = "vendor/sub";
/// Commit id recorded by the submodule gitlink (it does not exist in the fixture).
pub const SUBMODULE_COMMIT: &str = "1111111111111111111111111111111111111111";
/// Content of [`MODE_CHANGE_FILE`].
const MODE_CHANGE_CONTENT: &str = "#!/bin/sh\necho fixture\n";

impl GitFixture {
    /// Generate a repository with the given shape.
    ///
    /// # Panics
    ///
    /// Panics when the temporary directory cannot be created, a file cannot be written, or a
    /// git command fails. This is test support; failures are test failures.
    #[must_use]
    pub fn generate(spec: FixtureSpec) -> Self {
        let dir = tempfile::tempdir().expect("create fixture directory");
        let root = std::fs::canonicalize(dir.path()).expect("canonical fixture path");
        let fixture = Self {
            _dir: dir,
            root,
            spec,
        };
        fixture.build();
        fixture
    }

    /// Repository root (canonicalized).
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Shape used to generate this repository.
    #[must_use]
    pub const fn spec(&self) -> FixtureSpec {
        self.spec
    }

    /// Repository-relative path of an ordinary modified file.
    #[must_use]
    pub fn modified_path(index: usize) -> String {
        format!("src/mod_{:03}/file_{index:05}.rs", index / 50)
    }

    /// Repository-relative `(old, new)` paths of a renamed file.
    #[must_use]
    pub fn renamed_paths(index: usize) -> (String, String) {
        (
            format!("old/renamed_{index:03}.rs"),
            format!("moved/renamed_{index:03}.rs"),
        )
    }

    /// Repository-relative path of a deleted file.
    #[must_use]
    pub fn deleted_path(index: usize) -> String {
        format!("old/deleted_{index:03}.rs")
    }

    /// Repository-relative path of an added file.
    #[must_use]
    pub fn added_path(index: usize) -> String {
        format!("added/new_{index:03}.rs")
    }

    /// Repository-relative path of a file added by one stack layer (1-based).
    #[must_use]
    pub fn stack_layer_path(layer: usize) -> String {
        format!("stack/layer_{layer}.rs")
    }

    /// Run git in the fixture and return its standard output.
    ///
    /// # Panics
    ///
    /// Panics when git cannot be started or exits unsuccessfully.
    #[must_use]
    pub fn git(&self, args: &[&str]) -> String {
        run_git(&self.root, args, None)
    }

    /// Run git in the fixture for its side effect, discarding standard output.
    ///
    /// # Panics
    ///
    /// Panics when git cannot be started or exits unsuccessfully.
    pub fn run(&self, args: &[&str]) {
        run_git(&self.root, args, None);
    }

    fn build(&self) {
        let spec = self.spec;
        self.run(&["init", "-q", "-b", HEAD_BRANCH]);
        let base_ref = format!("refs/heads/{BASE_BRANCH}");
        let head_ref = format!("refs/heads/{HEAD_BRANCH}");
        let mut import = FastImport::start(&self.root);

        // Initial commit.
        import.commit(&base_ref, None, "initial");
        for index in 0..spec.modified_files {
            import.modify(
                &Self::modified_path(index),
                text_file(index, spec.lines_per_file, None).as_bytes(),
            );
        }
        for index in 0..spec.deleted_files {
            import.modify(
                &Self::deleted_path(index),
                text_file(100_000 + index, spec.lines_per_file, None).as_bytes(),
            );
        }
        for index in 0..spec.renamed_files {
            import.modify(
                &Self::renamed_paths(index).0,
                text_file(200_000 + index, spec.lines_per_file, None).as_bytes(),
            );
        }
        import.modify(
            LARGE_FILE,
            text_file(900_000, spec.large_file_lines, None).as_bytes(),
        );
        import.modify(UNICODE_FILE, unicode_file(false).as_bytes());
        import.modify(BINARY_FILE, &binary_file(0));
        import.modify(
            STACK_SHARED_FILE,
            text_file(500_000, STACK_SHARED_LINES, None).as_bytes(),
        );
        if spec.special_entries {
            import_special_entries(&mut import, false);
        }

        // Second commit.
        import.commit(&head_ref, Some(&base_ref), "second");
        let changes = ChangePlan::new(spec.lines_per_file, spec.changed_lines_per_file);
        for index in 0..spec.modified_files {
            import.modify(
                &Self::modified_path(index),
                text_file(index, spec.lines_per_file, Some((&changes, "rev2"))).as_bytes(),
            );
        }
        for index in 0..spec.deleted_files {
            import.delete(&Self::deleted_path(index));
        }
        let rename_edit = ChangePlan::new(spec.lines_per_file, 1);
        for index in 0..spec.renamed_files {
            let (old, new) = Self::renamed_paths(index);
            import.delete(&old);
            import.modify(
                &new,
                text_file(
                    200_000 + index,
                    spec.lines_per_file,
                    Some((&rename_edit, "moved")),
                )
                .as_bytes(),
            );
        }
        for index in 0..spec.added_files {
            import.modify(
                &Self::added_path(index),
                text_file(300_000 + index, spec.lines_per_file, None).as_bytes(),
            );
        }
        let large_changes = ChangePlan::new(spec.large_file_lines, spec.large_file_lines / 4);
        import.modify(
            LARGE_FILE,
            text_file(
                900_000,
                spec.large_file_lines,
                Some((&large_changes, "rev2")),
            )
            .as_bytes(),
        );
        import.modify(UNICODE_FILE, unicode_file(true).as_bytes());
        import.modify(BINARY_FILE, &binary_file(1));
        if spec.special_entries {
            import_special_entries(&mut import, true);
        }

        // Extra branches alternate between the two commits.
        for index in 0..spec.extra_branches {
            let target = if index % 2 == 0 { &base_ref } else { &head_ref };
            import.reset(&format!("refs/heads/fixture/extra-{index}"), target);
        }

        import_stack(&mut import, &head_ref);
        import.finish();

        // Populate the index and working tree of `main` from the imported objects.
        self.run(&["reset", "-q", "--hard"]);
        self.build_working_tree_changes();
    }

    fn build_working_tree_changes(&self) {
        let spec = self.spec;
        let staged_count = spec.staged_files.min(spec.modified_files);
        let unstaged_count = spec
            .unstaged_files
            .min(spec.modified_files.saturating_sub(staged_count));
        let staged_plan =
            ChangePlan::new(spec.lines_per_file, spec.worktree_changed_lines_per_file).offset(1);
        let unstaged_plan =
            ChangePlan::new(spec.lines_per_file, spec.worktree_changed_lines_per_file).offset(2);
        let committed = ChangePlan::new(spec.lines_per_file, spec.changed_lines_per_file);

        let mut staged_paths = Vec::with_capacity(staged_count);
        for index in 0..staged_count {
            let path = Self::modified_path(index);
            self.write(
                &path,
                &text_file_layers(
                    index,
                    spec.lines_per_file,
                    &[(&committed, "rev2"), (&staged_plan, "staged")],
                ),
            );
            staged_paths.push(path);
        }
        if !staged_paths.is_empty() {
            let list = staged_paths.join("\n");
            run_git(
                &self.root,
                &["add", "--pathspec-from-file=-"],
                Some(list.as_bytes()),
            );
            // The first staged file also gets an unstaged edit on top of its staged content.
            self.write(
                &staged_paths[0],
                &text_file_layers(
                    0,
                    spec.lines_per_file,
                    &[
                        (&committed, "rev2"),
                        (&staged_plan, "staged"),
                        (&unstaged_plan, "unstaged"),
                    ],
                ),
            );
        }
        for index in staged_count..staged_count + unstaged_count {
            self.write(
                &Self::modified_path(index),
                &text_file_layers(
                    index,
                    spec.lines_per_file,
                    &[(&committed, "rev2"), (&unstaged_plan, "unstaged")],
                ),
            );
        }
        self.write(UNTRACKED_FILE, "untracked working-tree note\n");
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture directory");
        }
        std::fs::write(&path, contents).expect("write fixture file");
    }
}

/// Add the special entries: in the first commit a regular script and a CRLF file; in the second
/// commit the script's mode change, a CRLF edit, a symbolic link, and a submodule gitlink.
fn import_special_entries(import: &mut FastImport, second_commit: bool) {
    if !second_commit {
        import.modify(MODE_CHANGE_FILE, MODE_CHANGE_CONTENT.as_bytes());
        import.modify(CRLF_FILE, crlf_file(false).as_bytes());
        return;
    }
    import.modify_mode("100755", MODE_CHANGE_FILE, MODE_CHANGE_CONTENT.as_bytes());
    import.modify(CRLF_FILE, crlf_file(true).as_bytes());
    import.modify_mode("120000", SYMLINK_FILE, SYMLINK_TARGET.as_bytes());
    import.gitlink(SUBMODULE_PATH, SUBMODULE_COMMIT);
    let gitmodules = format!(
        "[submodule \"{SUBMODULE_PATH}\"]\n\tpath = {SUBMODULE_PATH}\n\turl = ./missing-submodule\n"
    );
    import.modify(".gitmodules", gitmodules.as_bytes());
}

/// Import the linear stack on top of `head_ref`.
///
/// Each layer adds one file on top of the previous layer and rewrites its own region of the
/// shared file, keeping the regions of the layers below it.
fn import_stack(import: &mut FastImport, head_ref: &str) {
    let layer_plans: Vec<ChangePlan> = (1..=STACK_BRANCHES.len())
        .map(|layer| ChangePlan::new(STACK_SHARED_LINES, CHANGE_GROUP_LINES).offset(layer * 20))
        .collect();
    let mut parent = head_ref.to_owned();
    for (layer_index, branch) in STACK_BRANCHES.iter().enumerate() {
        let layer = layer_index + 1;
        let branch_ref = format!("refs/heads/{branch}");
        import.commit(&branch_ref, Some(&parent), &format!("stack layer {layer}"));
        import.modify(
            &GitFixture::stack_layer_path(layer),
            text_file(400_000 + layer, 20, None).as_bytes(),
        );
        let tags: Vec<String> = (1..=layer).map(|below| format!("layer{below}")).collect();
        let layers: Vec<(&ChangePlan, &str)> = layer_plans[..layer]
            .iter()
            .zip(&tags)
            .map(|(plan, tag)| (plan, tag.as_str()))
            .collect();
        import.modify(
            STACK_SHARED_FILE,
            text_file_layers(500_000, STACK_SHARED_LINES, &layers).as_bytes(),
        );
        parent = branch_ref;
    }
}

/// Which lines of a file are rewritten: groups of [`CHANGE_GROUP_LINES`] spread evenly.
#[derive(Debug, Clone, Copy)]
struct ChangePlan {
    stride: usize,
    total: usize,
    offset: usize,
}

impl ChangePlan {
    fn new(lines: usize, changed: usize) -> Self {
        let changed = changed.min(lines);
        let groups = changed.div_ceil(CHANGE_GROUP_LINES).max(1);
        let stride = (lines / groups).max(MIN_CHANGE_STRIDE.min(lines.max(1)));
        Self {
            stride: stride.max(1),
            total: changed,
            offset: 0,
        }
    }

    const fn offset(mut self, offset: usize) -> Self {
        self.offset = offset;
        self
    }

    /// Whether line `line` (0-based) is rewritten. Groups past the last line are dropped, so
    /// the effective count may be lower than requested for very short files.
    fn changes(&self, line: usize) -> bool {
        let shifted = line.checked_sub(self.offset);
        let Some(shifted) = shifted else {
            return false;
        };
        let group = shifted / self.stride;
        let within = shifted % self.stride;
        within < CHANGE_GROUP_LINES && group * CHANGE_GROUP_LINES + within < self.total
    }
}

fn text_file(file: usize, lines: usize, change: Option<(&ChangePlan, &str)>) -> String {
    text_file_layers(file, lines, change.as_slice())
}

/// Render a deterministic source-like file; later layers win for lines they change.
fn text_file_layers(file: usize, lines: usize, layers: &[(&ChangePlan, &str)]) -> String {
    let mut out = String::with_capacity(lines * 64);
    for line in 0..lines {
        let tag = layers
            .iter()
            .rev()
            .find(|(plan, _)| plan.changes(line))
            .map(|(_, tag)| *tag);
        match tag {
            Some(tag) => writeln!(
                out,
                "    let value_{file}_{line} = compute_{tag}({line}, {file}); // {tag} line {line}"
            ),
            None => writeln!(
                out,
                "    let value_{file}_{line} = compute({line}, {file}); // original line {line}"
            ),
        }
        .expect("write to string");
    }
    out
}

fn unicode_file(revised: bool) -> String {
    let mut out = String::new();
    let marker = if revised { "改訂" } else { "初版" };
    let lines = [
        format!("漢字かな交じり文の行です {marker}"),
        "全角スペース\u{3000}と半角 space が混在".to_owned(),
        format!("emoji 🦀 👩\u{200d}💻 🇯🇵 ✅ {marker}"),
        "combining: e\u{301} a\u{308} n\u{303} o\u{302}\u{323}".to_owned(),
        format!("\tindented\twith\ttabs\t{marker}"),
        "한국어 텍스트와 中文 混合".to_owned(),
        "zero\u{200b}width and RTL مرحبا".to_owned(),
    ];
    for line in &lines {
        out.push_str(line);
        out.push('\n');
    }
    let unit = if revised {
        "長い行🦀"
    } else {
        "long-line "
    };
    let mut long = String::with_capacity(LONG_LINE_CHARS * 4);
    while long.chars().count() < LONG_LINE_CHARS {
        long.push_str(unit);
    }
    out.push_str(&long);
    out.push('\n');
    for line in 0..20 {
        writeln!(out, "plain line {line}").expect("write to string");
    }
    out
}

fn crlf_file(revised: bool) -> String {
    let middle = if revised { "edited" } else { "original" };
    format!("first line\r\nmiddle {middle} line\r\nlast line\r\n")
}

fn binary_file(revision: u8) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    for value in 0..=u8::MAX {
        bytes.extend_from_slice(&[value, 0, value.wrapping_add(revision), 0xff]);
    }
    bytes
}

/// Fixed author and committer identity and timestamp for imported commits.
const FIXTURE_IDENT: &str = "Bcode Fixture <fixture@bcode.invalid> 1767225600 +0000";

/// Build a git command with an isolated, deterministic configuration.
fn git_command(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .args([
            "-c",
            "user.name=Bcode Fixture",
            "-c",
            "user.email=fixture@bcode.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "gc.auto=0",
        ])
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Run git and return its standard output.
fn run_git(root: &Path, args: &[&str], stdin: Option<&[u8]>) -> String {
    let mut command = git_command(root, args);
    command.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = command.spawn().expect("start git");
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .expect("git stdin")
            .write_all(input)
            .expect("write git stdin");
    }
    finish_git(child, args)
}

fn finish_git(child: Child, args: &[&str]) -> String {
    let output = child.wait_with_output().expect("wait for git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Streams commits into `git fast-import`, which writes one pack instead of a loose object per
/// file; this keeps the benchmark preset fast.
struct FastImport {
    child: Child,
    input: BufWriter<ChildStdin>,
}

impl FastImport {
    const ARGS: [&'static str; 3] = ["fast-import", "--quiet", "--done"];

    fn start(root: &Path) -> Self {
        let mut child = git_command(root, &Self::ARGS)
            .stdin(Stdio::piped())
            .spawn()
            .expect("start git fast-import");
        let input = BufWriter::new(child.stdin.take().expect("fast-import stdin"));
        Self { child, input }
    }

    fn data(&mut self, bytes: &[u8]) {
        writeln!(self.input, "data {}", bytes.len()).expect("write fast-import");
        self.input.write_all(bytes).expect("write fast-import");
        self.input.write_all(b"\n").expect("write fast-import");
    }

    /// Start a commit on `reference`; file commands follow until the next command.
    fn commit(&mut self, reference: &str, parent: Option<&str>, message: &str) {
        write!(
            self.input,
            "commit {reference}\nauthor {FIXTURE_IDENT}\ncommitter {FIXTURE_IDENT}\n"
        )
        .expect("write fast-import");
        self.data(message.as_bytes());
        if let Some(parent) = parent {
            writeln!(self.input, "from {parent}").expect("write fast-import");
        }
    }

    fn modify(&mut self, path: &str, contents: &[u8]) {
        self.modify_mode("100644", path, contents);
    }

    fn modify_mode(&mut self, mode: &str, path: &str, contents: &[u8]) {
        writeln!(self.input, "M {mode} inline {path}").expect("write fast-import");
        self.data(contents);
    }

    /// Record a submodule gitlink pointing at `commit` (which need not exist).
    fn gitlink(&mut self, path: &str, commit: &str) {
        writeln!(self.input, "M 160000 {commit} {path}").expect("write fast-import");
    }

    fn delete(&mut self, path: &str) {
        writeln!(self.input, "D {path}").expect("write fast-import");
    }

    /// Point `reference` at the tip of `target`.
    fn reset(&mut self, reference: &str, target: &str) {
        write!(self.input, "reset {reference}\nfrom {target}\n\n").expect("write fast-import");
    }

    fn finish(mut self) {
        self.input.write_all(b"done\n").expect("write fast-import");
        self.input.flush().expect("flush fast-import");
        drop(self.input);
        finish_git(self.child, &Self::ARGS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numstat_totals(fixture: &GitFixture, range: &str) -> (usize, usize, usize) {
        let output = fixture.git(&["diff", "--numstat", "--find-renames", range]);
        let mut additions = 0;
        let mut deletions = 0;
        let mut binary = 0;
        for line in output.lines() {
            let mut fields = line.split('\t');
            match (fields.next(), fields.next()) {
                (Some("-"), Some("-")) => binary += 1,
                (Some(added), Some(deleted)) => {
                    additions += added.parse::<usize>().expect("numstat additions");
                    deletions += deleted.parse::<usize>().expect("numstat deletions");
                }
                _ => {}
            }
        }
        (additions, deletions, binary)
    }

    /// Write a fixture repository to `$BCODE_REVIEW_FIXTURE_DIR` for manual checks of the real
    /// binary (`bcode review range fixture/base main --ui v2` inside it).
    /// `BCODE_REVIEW_FIXTURE_SPEC=benchmark` writes the benchmark shape and `small` the small
    /// one; the default is a mid-sized review with every special entry.
    #[test]
    #[ignore = "manual tool; set BCODE_REVIEW_FIXTURE_DIR to a directory that does not exist"]
    fn write_fixture_for_manual_checks() {
        let Some(destination) = std::env::var_os("BCODE_REVIEW_FIXTURE_DIR") else {
            return;
        };
        let spec = match std::env::var("BCODE_REVIEW_FIXTURE_SPEC").as_deref() {
            Ok("benchmark") => FixtureSpec::benchmark(),
            Ok("small") => FixtureSpec::small(),
            _ => FixtureSpec {
                modified_files: 120,
                lines_per_file: 120,
                changed_lines_per_file: 24,
                large_file_lines: 4_000,
                ..FixtureSpec::small()
            },
        };
        let fixture = GitFixture::generate(spec);
        let status = Command::new("cp")
            .arg("-R")
            .arg(fixture.root())
            .arg(&destination)
            .status()
            .expect("run cp");
        assert!(status.success(), "copy the fixture to {destination:?}");
    }

    #[test]
    fn small_fixture_has_every_change_kind() {
        let fixture = GitFixture::generate(FixtureSpec::small());
        let spec = fixture.spec();
        let range = format!("{BASE_BRANCH}..{HEAD_BRANCH}");

        let status = fixture.git(&["diff", "--name-status", "--find-renames", &range]);
        assert!(status.lines().any(|line| line.starts_with('A')), "{status}");
        assert!(status.lines().any(|line| line.starts_with('D')), "{status}");
        assert!(status.lines().any(|line| line.starts_with('R')), "{status}");
        assert!(status.contains(LARGE_FILE) && status.contains(UNICODE_FILE));

        let (additions, deletions, binary) = numstat_totals(&fixture, &range);
        assert_eq!(binary, 1, "binary file reported as binary");
        assert!(additions + deletions >= spec.modified_diff_lines());

        let unicode = std::fs::read_to_string(fixture.root().join(UNICODE_FILE)).unwrap();
        assert!(unicode.contains('\t') && unicode.contains("🦀") && unicode.contains('\u{301}'));
        assert!(
            unicode
                .lines()
                .any(|line| line.chars().count() >= LONG_LINE_CHARS)
        );
    }

    #[test]
    fn small_fixture_has_stack_branches_and_working_tree_changes() {
        let fixture = GitFixture::generate(FixtureSpec::small());
        let head = fixture.git(&["rev-parse", HEAD_BRANCH]);
        let mut parent = head;
        let mut previous = HEAD_BRANCH;
        for (index, branch) in STACK_BRANCHES.into_iter().enumerate() {
            let branch_parent = fixture.git(&["rev-parse", &format!("{branch}^")]);
            assert_eq!(
                branch_parent, parent,
                "{branch} is based on the previous layer"
            );
            parent = fixture.git(&["rev-parse", branch]);
            // Each layer's own diff adds its file and changes one region of the shared file.
            let layer_range = format!("{previous}..{branch}");
            let files = fixture.git(&["diff", "--name-status", &layer_range]);
            assert!(
                files.contains(&GitFixture::stack_layer_path(index + 1)),
                "{files}"
            );
            let shared = fixture.git(&["diff", "-U0", &layer_range, "--", STACK_SHARED_FILE]);
            let hunks = shared.lines().filter(|line| line.starts_with("@@")).count();
            assert_eq!(hunks, 1, "{branch} rewrites one shared region:\n{shared}");
            assert!(shared.contains(&format!("layer{}", index + 1)), "{shared}");
            previous = branch;
        }
        assert_eq!(
            fixture.git(&["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
            HEAD_BRANCH
        );
        let branches = fixture.git(&["branch", "--list", "fixture/extra-*"]);
        assert_eq!(branches.lines().count(), fixture.spec().extra_branches);

        let staged = fixture.git(&["diff", "--cached", "--name-only"]);
        let unstaged = fixture.git(&["diff", "--name-only"]);
        let first = GitFixture::modified_path(0);
        assert!(
            staged.contains(&first) && unstaged.contains(&first),
            "mixed file"
        );
        assert!(unstaged.contains(&GitFixture::modified_path(1)));
        let untracked = fixture.git(&["ls-files", "--others", "--exclude-standard"]);
        assert_eq!(untracked.trim(), UNTRACKED_FILE);
    }

    #[test]
    fn small_fixture_has_special_entries() {
        let fixture = GitFixture::generate(FixtureSpec::small());
        let range = format!("{BASE_BRANCH}..{HEAD_BRANCH}");
        let raw = fixture.git(&["diff", "--raw", &range]);
        let line = |path: &str| {
            raw.lines()
                .find(|line| line.ends_with(path))
                .unwrap_or_else(|| panic!("{path} missing from:\n{raw}"))
                .to_string()
        };
        assert!(line(MODE_CHANGE_FILE).starts_with(":100644 100755"));
        assert!(line(SYMLINK_FILE).starts_with(":000000 120000"));
        assert!(line(SUBMODULE_PATH).starts_with(":000000 160000"));
        let crlf = fixture.git(&["show", &format!("{HEAD_BRANCH}:{CRLF_FILE}")]);
        assert!(crlf.contains("\r\n"), "{crlf:?}");
    }

    #[test]
    fn same_spec_generates_identical_history() {
        let refs = |fixture: &GitFixture| {
            fixture.git(&["for-each-ref", "--format=%(refname) %(objectname)"])
        };
        let first = GitFixture::generate(FixtureSpec::small());
        let second = GitFixture::generate(FixtureSpec::small());
        assert_ne!(first.root(), second.root());
        assert_eq!(refs(&first), refs(&second));
        assert_eq!(
            first.git(&["status", "--porcelain"]),
            second.git(&["status", "--porcelain"])
        );
    }

    #[test]
    fn change_plan_spreads_requested_lines_into_separate_groups() {
        let plan = ChangePlan::new(200, 50);
        let changed = (0..200).filter(|line| plan.changes(*line)).count();
        assert_eq!(changed, 50);
        // Groups are separated by more than twice the default diff context.
        assert!(plan.stride - CHANGE_GROUP_LINES > 6);
        let tiny = ChangePlan::new(3, 10);
        assert_eq!((0..3).filter(|line| tiny.changes(*line)).count(), 3);
    }
}
