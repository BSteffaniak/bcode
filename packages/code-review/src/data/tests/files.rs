//! Reviews of files, folders, globs and the repository as they are now: selection listings, the
//! path index, `find_paths`, and uncommitted-change marks.

use super::{full_index, source};
use crate::data::ReviewData;
use crate::testing::fixture::GitFixture;
use bcode_code_review_models::{
    BrowseOptions, PathSelector, REVIEW_DIAGNOSTIC_UNRESOLVED_REVISION, REVIEW_ERROR_PATH_REJECTED,
    REVIEW_WORKTREE_MARKS_MAX_BYTES, RemovedSince, ResolveReviewSourcesRequest,
    ResolveReviewSourcesResponse, ResolvedSource, ReviewIndexEntry, ReviewIndexFlag,
    ReviewIndexRequest, ReviewPathKind, ReviewPathMatchKind, ReviewPathStatus,
    ReviewPathsFindRequest, ReviewPathsFindResponse, ReviewPathsListRequest,
    ReviewPathsResolveRequest, ReviewPathsSearchRequest, ReviewSourceKind, ReviewWorktreeFileMarks,
    ReviewWorktreeMarksRequest,
};
use std::path::Path;

const HELLO: &str = "examples/hello-plugin";

/// A repository with two example plugins (a Cargo and an npm package), committed, plus an
/// untracked file, ignored files, and an ignored folder.
fn repository() -> GitFixture {
    let fixture = GitFixture::empty();
    let files: &[(&str, &str)] = &[
        (".gitignore", "*.log\nbuild/\n"),
        (
            "examples/hello-plugin/Cargo.toml",
            "[package]\nname = \"hello-plugin\"\nversion = \"0.1.0\"\n",
        ),
        (
            "examples/hello-plugin/src/lib.rs",
            "pub fn a() {}\npub fn b() {}\npub fn c() {}\npub fn d() {}\npub fn e() {}\n",
        ),
        ("examples/hello-plugin/src/greet.rs", "pub fn greet() {}\n"),
        ("examples/hello-plugin/README.md", "# Hello\n"),
        (
            "examples/other-plugin/package.json",
            "{\"name\": \"@acme/other-plugin\"}\n",
        ),
        ("examples/other-plugin/index.js", "module.exports = {};\n"),
        ("src/lib.rs", "//! Library.\n"),
        ("README.md", "# Fixture\n"),
    ];
    for (path, text) in files {
        write(fixture.root(), path, text);
    }
    fixture.run(&["add", "-A"]);
    fixture.run(&["commit", "-q", "-m", "initial"]);
    write(
        fixture.root(),
        "examples/hello-plugin/src/new.rs",
        "// new\n",
    );
    write(fixture.root(), "examples/hello-plugin/debug.log", "log\n");
    write(fixture.root(), "build/out.bin", "bin\n");
    fixture
}

fn write(root: &Path, path: &str, text: &str) {
    let full = root.join(path);
    std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
    std::fs::write(full, text).expect("write");
}

fn folder(path: &str) -> PathSelector {
    PathSelector::Folder { path: path.into() }
}

fn file(path: &str) -> PathSelector {
    PathSelector::File { path: path.into() }
}

fn file_set(include: Vec<PathSelector>, exclude: Vec<PathSelector>) -> ReviewSourceKind {
    ReviewSourceKind::FileSet { include, exclude }
}

fn resolve(
    service: &ReviewData,
    repo: &Path,
    kind: ReviewSourceKind,
    browse: BrowseOptions,
) -> ResolveReviewSourcesResponse {
    service
        .resolve_sources(&ResolveReviewSourcesRequest {
            repo_path: repo.to_path_buf(),
            sources: vec![source("s", kind)],
            provider_resolutions: Vec::new(),
            browse,
        })
        .expect("resolve")
}

fn resolved(service: &ReviewData, repo: &Path, kind: ReviewSourceKind) -> ResolvedSource {
    let response = resolve(service, repo, kind, BrowseOptions::default());
    assert!(
        response.diagnostics.is_empty(),
        "{:?}",
        response.diagnostics
    );
    response.sources.into_iter().next().expect("one source")
}

fn paths(entries: &[ReviewIndexEntry]) -> Vec<&str> {
    entries.iter().map(|entry| entry.path.as_str()).collect()
}

fn flagged<'a>(entries: &'a [ReviewIndexEntry], path: &str) -> &'a ReviewIndexEntry {
    entries
        .iter()
        .find(|entry| entry.path == path)
        .unwrap_or_else(|| panic!("{path} not listed in {:?}", paths(entries)))
}

#[test]
fn a_folder_lists_its_files_as_they_are_now_including_files_added_later() {
    let fixture = repository();
    let repo = fixture.root();
    let service = ReviewData::default();
    let source = resolved(&service, repo, file_set(vec![folder(HELLO)], Vec::new()));
    let entries = full_index(&service, repo, &source);
    assert_eq!(
        paths(&entries),
        [
            "examples/hello-plugin/Cargo.toml",
            "examples/hello-plugin/README.md",
            "examples/hello-plugin/src/greet.rs",
            "examples/hello-plugin/src/lib.rs",
            "examples/hello-plugin/src/new.rs",
        ],
        "untracked files are listed, ignored ones are not"
    );
    assert!(flagged(&entries, "examples/hello-plugin/src/new.rs").has(ReviewIndexFlag::Untracked));
    assert!(
        flagged(&entries, "examples/hello-plugin/src/lib.rs")
            .flags
            .is_empty()
    );

    // The same resolved source lists again on every open: a file added since appears.
    write(repo, "examples/hello-plugin/src/later.rs", "// later\n");
    let reopened = resolved(&service, repo, file_set(vec![folder(HELLO)], Vec::new()));
    assert_eq!(reopened, source, "nothing stored changes");
    let entries = full_index(&service, repo, &reopened);
    assert!(paths(&entries).contains(&"examples/hello-plugin/src/later.rs"));
}

#[test]
fn ignored_files_follow_the_toggle_and_a_file_named_directly_is_always_reviewed() {
    let fixture = repository();
    let repo = fixture.root();
    let service = ReviewData::default();
    let shown = resolve(
        &service,
        repo,
        file_set(vec![folder(HELLO)], Vec::new()),
        BrowseOptions {
            include_ignored: true,
            max_files: None,
        },
    );
    let entries = full_index(&service, repo, &shown.sources[0]);
    assert!(flagged(&entries, "examples/hello-plugin/debug.log").has(ReviewIndexFlag::Ignored));

    let named = resolved(
        &service,
        repo,
        file_set(
            vec![folder("src"), file("examples/hello-plugin/debug.log")],
            Vec::new(),
        ),
    );
    let entries = full_index(&service, repo, &named);
    assert_eq!(
        paths(&entries),
        ["examples/hello-plugin/debug.log", "src/lib.rs"]
    );
    assert!(entries[0].has(ReviewIndexFlag::Ignored));

    // A single-file review of an untracked or ignored file lists it too.
    for path in [
        "examples/hello-plugin/src/new.rs",
        "examples/hello-plugin/debug.log",
    ] {
        let single = resolved(&service, repo, ReviewSourceKind::File { path: path.into() });
        assert_eq!(paths(&full_index(&service, repo, &single)), [path]);
    }
}

#[test]
fn globs_and_exclusions_select_files_and_keep_folders_dynamic() {
    let fixture = repository();
    let repo = fixture.root();
    let service = ReviewData::default();
    let source = resolved(
        &service,
        repo,
        file_set(
            vec![
                PathSelector::Glob {
                    pattern: "examples/**/*.rs".into(),
                },
                folder("examples/other-plugin"),
            ],
            vec![
                file("examples/hello-plugin/src/greet.rs"),
                file("examples/other-plugin/index.js"),
            ],
        ),
    );
    assert_eq!(
        paths(&full_index(&service, repo, &source)),
        [
            "examples/hello-plugin/src/lib.rs",
            "examples/hello-plugin/src/new.rs",
            "examples/other-plugin/package.json",
        ]
    );
    // A glob only matches as a glob, and a path with glob characters only as a path.
    write(repo, "docs/[draft].md", "draft\n");
    let literal = resolved(
        &service,
        repo,
        file_set(vec![file("docs/[draft].md")], Vec::new()),
    );
    assert_eq!(
        paths(&full_index(&service, repo, &literal)),
        ["docs/[draft].md"]
    );
}

#[test]
fn the_repository_lists_untracked_files_and_a_long_listing_is_cut_with_its_count() {
    let fixture = repository();
    let repo = fixture.root();
    let service = ReviewData::default();
    let whole = resolved(&service, repo, ReviewSourceKind::Repository);
    let entries = full_index(&service, repo, &whole);
    assert!(paths(&entries).contains(&"examples/hello-plugin/src/new.rs"));
    assert!(!paths(&entries).contains(&"examples/hello-plugin/debug.log"));
    assert!(
        !paths(&entries)
            .iter()
            .any(|path| path.starts_with("build/"))
    );

    let cut = resolve(
        &service,
        repo,
        ReviewSourceKind::Repository,
        BrowseOptions {
            include_ignored: false,
            max_files: Some(3),
        },
    );
    let page = service
        .index_get(&ReviewIndexRequest {
            repo_path: repo.to_path_buf(),
            source: cut.sources[0].clone(),
            cursor: None,
            limit: Some(100),
        })
        .expect("index");
    assert_eq!(page.total_entries, 3);
    assert_eq!(
        page.untruncated_entries,
        Some(u32::try_from(entries.len()).expect("count"))
    );
    assert_eq!(
        paths(&page.entries),
        paths(&entries[..3]),
        "the first in path order"
    );
    assert_eq!(
        bcode_code_review_models::listing_truncated_message("Repository browser", 11, 3),
        "Repository browser resolves to 11 files; showing the first 3 (files.max_files). \
         Choose narrower folders or raise the limit."
    );
}

/// Files `include` selects now by the path index (what the picker and `find_paths` count).
fn counted(service: &ReviewData, repo: &Path, include: Vec<PathSelector>, ignored: bool) -> u32 {
    service
        .paths_resolve(&ReviewPathsResolveRequest {
            repo_path: repo.to_path_buf(),
            include,
            exclude: Vec::new(),
            include_ignored: ignored,
            refresh: true,
        })
        .expect("resolve")
        .total
}

#[test]
fn nested_repositories_are_not_files_and_submodules_are_listed_once() {
    let fixture = repository();
    let repo = fixture.root();
    write(repo, "sub/a.txt", "a\n");
    fixture.run(&["add", "sub/a.txt"]);
    // A submodule: a gitlink in the index.
    let head = fixture.git(&["rev-parse", "HEAD"]);
    fixture.run(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{},sub/module", head.trim()),
    ]);
    fixture.run(&["commit", "-q", "-m", "sub"]);
    // Its folder, as an uninitialized submodule has.
    std::fs::create_dir_all(repo.join("sub/module")).expect("mkdir");
    // A nested repository that is not a submodule (an untracked clone), with a file inside.
    write(repo, "sub/nested/inner.txt", "inner\n");
    fixture.run(&["-C", "sub/nested", "init", "-q"]);
    let service = ReviewData::default();
    for ignored in [false, true] {
        for kind in [
            file_set(vec![folder("sub")], Vec::new()),
            file_set(
                vec![PathSelector::Glob {
                    pattern: "sub/**".into(),
                }],
                Vec::new(),
            ),
        ] {
            let response = resolve(
                &service,
                repo,
                kind.clone(),
                BrowseOptions {
                    include_ignored: ignored,
                    max_files: None,
                },
            );
            let entries = full_index(&service, repo, &response.sources[0]);
            assert_eq!(
                paths(&entries),
                ["sub/a.txt", "sub/module"],
                "{kind:?} ignored={ignored}"
            );
            assert!(entries[1].has(ReviewIndexFlag::Submodule));
            let ReviewSourceKind::FileSet { include, .. } = kind else {
                unreachable!()
            };
            assert_eq!(
                counted(&service, repo, include, ignored),
                2,
                "the picker counts what the review lists"
            );
        }
        let whole = resolve(
            &service,
            repo,
            ReviewSourceKind::Repository,
            BrowseOptions {
                include_ignored: ignored,
                max_files: None,
            },
        );
        let entries = full_index(&service, repo, &whole.sources[0]);
        assert!(
            !paths(&entries).iter().any(|path| path.ends_with('/')),
            "{:?}",
            paths(&entries)
        );
        assert!(paths(&entries).contains(&"sub/module"));
    }
}

#[test]
fn malformed_file_sets_are_per_source_diagnostics_and_older_listings_stay_tracked_only() {
    let fixture = repository();
    let repo = fixture.root();
    let service = ReviewData::default();
    let cases = [
        (
            file_set(vec![folder("../outside")], Vec::new()),
            REVIEW_ERROR_PATH_REJECTED,
        ),
        (
            file_set(
                vec![PathSelector::Glob {
                    pattern: "/etc/*".into(),
                }],
                Vec::new(),
            ),
            REVIEW_ERROR_PATH_REJECTED,
        ),
        (
            file_set(
                vec![folder(HELLO)],
                vec![PathSelector::Glob {
                    pattern: "*.rs".into(),
                }],
            ),
            REVIEW_DIAGNOSTIC_UNRESOLVED_REVISION,
        ),
        (
            file_set(Vec::new(), Vec::new()),
            REVIEW_DIAGNOSTIC_UNRESOLVED_REVISION,
        ),
    ];
    for (kind, code) in cases {
        let response = resolve(&service, repo, kind.clone(), BrowseOptions::default());
        assert!(response.sources.is_empty(), "{kind:?}");
        assert_eq!(response.diagnostics[0].code, code, "{kind:?}");
    }
    // A client that does not know selections lists the tracked files under `paths`.
    let mut older = resolved(&service, repo, ReviewSourceKind::Repository);
    older.selection = None;
    assert!(
        !paths(&full_index(&service, repo, &older)).contains(&"examples/hello-plugin/src/new.rs")
    );
}

#[test]
fn the_path_index_lists_folders_with_counts_searches_and_counts_selectors() {
    let fixture = repository();
    let repo = fixture.root();
    let service = ReviewData::default();
    let list = |dir: &str, include_ignored: bool| {
        service
            .paths_list(&ReviewPathsListRequest {
                repo_path: repo.to_path_buf(),
                dir: dir.into(),
                include_ignored,
                refresh: true,
                cursor: None,
                limit: None,
            })
            .expect("list")
            .entries
    };
    let root = list("", false);
    let names: Vec<&str> = root.iter().map(|entry| entry.path.as_str()).collect();
    assert_eq!(names, ["examples/", "src/", ".gitignore", "README.md"]);
    assert_eq!(root[0].files, Some(7));
    assert_eq!(root[0].changed, 1, "the untracked file");
    let hello = list(HELLO, false);
    let names: Vec<&str> = hello.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names, ["src", "Cargo.toml", "README.md"]);
    let src = list("examples/hello-plugin/src", false);
    let new = src
        .iter()
        .find(|entry| entry.name == "new.rs")
        .expect("new.rs");
    assert_eq!(new.status, ReviewPathStatus::Untracked);
    // Ignored files show only with the toggle; an ignored folder is one entry, listed lazily.
    let root = list("", true);
    let build = root
        .iter()
        .find(|entry| entry.path == "build/")
        .expect("build/");
    assert_eq!(
        (build.kind, build.status, build.files),
        (ReviewPathKind::Folder, ReviewPathStatus::Ignored, None)
    );
    let inside = list("build", true);
    assert_eq!(inside[0].path, "build/out.bin");
    assert!(
        list(HELLO, true)
            .iter()
            .any(|entry| entry.name == "debug.log")
    );

    let search = |query: &str| {
        service
            .paths_search(&ReviewPathsSearchRequest {
                repo_path: repo.to_path_buf(),
                query: query.into(),
                include_ignored: false,
                refresh: false,
                limit: None,
            })
            .expect("search")
    };
    assert_eq!(
        search("greet").entries[0].path,
        "examples/hello-plugin/src/greet.rs"
    );
    assert_eq!(
        search("hello-plugin").entries[0].path,
        "examples/hello-plugin/"
    );
    let glob = search("**/*.rs");
    assert!(glob.glob);
    assert_eq!(glob.total_matches, 4);

    let counts = service
        .paths_resolve(&ReviewPathsResolveRequest {
            repo_path: repo.to_path_buf(),
            include: vec![
                folder(HELLO),
                PathSelector::Glob {
                    pattern: "src/**/*.rs".into(),
                },
                file("missing.rs"),
                file("examples/hello-plugin/debug.log"),
            ],
            exclude: vec![folder("examples/hello-plugin/src")],
            include_ignored: false,
            refresh: false,
        })
        .expect("resolve");
    let files: Vec<u32> = counts.include.iter().map(|count| count.files).collect();
    assert_eq!(files, [5, 1, 0, 1]);
    assert_eq!(
        counts.include[2].error.as_deref(),
        Some("missing.rs: no such file")
    );
    assert_eq!(counts.exclude[0].files, 3);
    assert_eq!(
        counts.total, 4,
        "Cargo.toml, README.md, src/lib.rs, debug.log"
    );
}

fn find(service: &ReviewData, repo: &Path, query: &str, ignored: bool) -> ReviewPathsFindResponse {
    service
        .paths_find(&ReviewPathsFindRequest {
            repo_path: repo.to_path_buf(),
            query: query.into(),
            kinds: Vec::new(),
            include_ignored: ignored,
            limit: None,
        })
        .expect("find")
}

#[test]
fn find_paths_resolves_natural_names_to_folders_packages_and_files() {
    let fixture = repository();
    let repo = fixture.root();
    let service = ReviewData::default();
    for query in [
        "the hello plugin",
        "hello-plugin",
        "HelloPlugin",
        "hello plugin crate",
    ] {
        let found = find(&service, repo, query, false);
        let best = &found.matches[0];
        assert_eq!(best.path, "examples/hello-plugin/", "{query}");
        assert_eq!(best.kind, ReviewPathMatchKind::Package, "{query}");
        assert_eq!(best.package.as_deref(), Some("hello-plugin"));
        assert_eq!(
            best.manifest.as_deref(),
            Some("examples/hello-plugin/Cargo.toml")
        );
        assert_eq!((best.score, best.files), (100, 5), "{query}");
        assert!(!found.ambiguous, "{query}: {:?}", found.matches);
    }
    // An npm package by its scoped name.
    let other = find(&service, repo, "acme other plugin", false);
    assert_eq!(other.matches[0].path, "examples/other-plugin/");
    assert_eq!(other.matches[0].kind, ReviewPathMatchKind::Package);
    // A file by its name; a path by its ending.
    let manifest = find(&service, repo, "package.json", false);
    assert_eq!(
        manifest.matches[0].path,
        "examples/other-plugin/package.json"
    );
    let suffix = find(&service, repo, "hello-plugin/src", false);
    assert_eq!(suffix.matches[0].path, "examples/hello-plugin/src/");
    assert_eq!(suffix.matches[0].matched, "path");
    // Two files of the same name are ambiguous.
    let libs = find(&service, repo, "lib.rs", false);
    assert!(libs.ambiguous, "{:?}", libs.matches);
    // Ignored files are found only when asked for.
    assert!(
        !find(&service, repo, "debug log", false)
            .matches
            .iter()
            .any(|found| found.path.ends_with("debug.log"))
    );
    assert_eq!(
        find(&service, repo, "debug log", true).matches[0].path,
        "examples/hello-plugin/debug.log"
    );
    let none = find(&service, repo, "zebra crossing", false);
    assert!(none.matches.is_empty() && !none.ambiguous);
    let strong = find(&service, repo, "the hello plugin", false);
    assert!(!strong.weak, "{strong:?}");
}

#[test]
fn find_paths_flags_a_best_match_that_only_partly_matches_the_name() {
    let fixture = GitFixture::empty();
    let repo = fixture.root();
    write(repo, "oauth-client/src/lib.rs", "//! OAuth.\n");
    write(repo, "billing/src/lib.rs", "//! Billing.\n");
    fixture.run(&["add", "-A"]);
    fixture.run(&["commit", "-q", "-m", "initial"]);
    let service = ReviewData::default();
    // Only `oauth-client/` contains "auth", inside a word: a guess to confirm, not a match.
    let found = find(&service, repo, "the auth module", false);
    assert_eq!(found.matches[0].path, "oauth-client/", "{found:?}");
    assert!(found.weak, "{found:?}");
    assert!(!found.ambiguous);
    // A whole-word match is not weak.
    let billing = find(&service, repo, "the billing module", false);
    assert_eq!(billing.matches[0].path, "billing/");
    assert!(!billing.weak, "{billing:?}");
}

fn marks(service: &ReviewData, repo: &Path, paths: &[&str]) -> Vec<ReviewWorktreeFileMarks> {
    service
        .worktree_marks(&ReviewWorktreeMarksRequest {
            repo_path: repo.to_path_buf(),
            paths: paths.iter().map(|path| (*path).to_owned()).collect(),
        })
        .expect("marks")
        .files
}

#[test]
fn uncommitted_marks_show_lines_changed_since_head_staged_or_not() {
    let fixture = repository();
    let repo = fixture.root();
    let service = ReviewData::default();
    // Line 2 changed, line 4 removed, a line added at the end; README staged.
    write(
        repo,
        "examples/hello-plugin/src/lib.rs",
        "pub fn a() {}\npub fn B() {}\npub fn c() {}\npub fn e() {}\npub fn f() {}\n",
    );
    write(
        repo,
        "examples/hello-plugin/README.md",
        "# Hello\n\nMore.\n",
    );
    fixture.run(&["add", "examples/hello-plugin/README.md"]);
    let big = "x".repeat(usize::try_from(REVIEW_WORKTREE_MARKS_MAX_BYTES).expect("size") + 1);
    write(repo, "examples/hello-plugin/big.txt", &big);
    let found = marks(
        &service,
        repo,
        &[
            "examples/hello-plugin/src/lib.rs",
            "examples/hello-plugin/README.md",
            "examples/hello-plugin/src/new.rs",
            "examples/hello-plugin/big.txt",
            "examples/hello-plugin/gone.rs",
            "examples/hello-plugin/src/greet.rs",
        ],
    );
    let lib = &found[0];
    assert_eq!(lib.added, [(2, 2), (5, 5)]);
    assert_eq!(lib.was, [(2, "pub fn b() {}".to_owned())]);
    assert_eq!(
        lib.removed,
        [RemovedSince {
            before_new_line: 4,
            lines: vec!["pub fn d() {}".to_owned()],
        }]
    );
    assert_eq!((lib.additions, lib.deletions), (2, 2));
    assert_eq!(found[1].added, [(2, 3)], "staged changes are marked");
    assert!(found[2].untracked && found[2].is_changed());
    assert!(found[3].too_large && !found[3].is_changed());
    assert!(found[4].missing);
    assert!(!found[5].is_changed(), "an unchanged file has no marks");
    // Cached by content: a change to the file is seen.
    write(
        repo,
        "examples/hello-plugin/src/greet.rs",
        "pub fn greet() {}\n// more\n",
    );
    assert_eq!(
        marks(&service, repo, &["examples/hello-plugin/src/greet.rs"])[0].added,
        [(2, 2)]
    );
}

#[test]
fn uncommitted_marks_in_a_repository_without_commits_mark_every_staged_line() {
    let fixture = GitFixture::empty();
    let repo = fixture.root();
    write(repo, "a.txt", "one\ntwo\n");
    fixture.run(&["add", "a.txt"]);
    let service = ReviewData::default();
    let found = marks(&service, repo, &["a.txt"]);
    assert_eq!(found[0].added, [(1, 2)]);
    assert!(!found[0].untracked);
}

#[test]
fn uncommitted_marks_read_names_with_spaces_binary_changes_and_renames() {
    let fixture = repository();
    let repo = fixture.root();
    write(repo, "docs/my notes.txt", "one\ntwo\n");
    write(repo, "docs/quote\"d.txt", "one\n");
    std::fs::write(repo.join("docs/image.png"), [0_u8, 1, 2, 3, 0, 5]).expect("write");
    write(repo, "docs/moved.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
    write(repo, "docs/edited.rs", "fn x() {}\nfn y() {}\nfn z() {}\n");
    fixture.run(&["add", "-A"]);
    fixture.run(&["commit", "-q", "-m", "docs"]);
    write(repo, "docs/my notes.txt", "one\nTWO\n");
    write(repo, "docs/quote\"d.txt", "one\nmore\n");
    std::fs::write(repo.join("docs/image.png"), [0_u8, 9, 9, 9, 0, 5]).expect("write");
    fixture.run(&["mv", "docs/moved.rs", "docs/renamed.rs"]);
    fixture.run(&["mv", "docs/edited.rs", "docs/edited-now.rs"]);
    write(
        repo,
        "docs/edited-now.rs",
        "fn x() {}\nfn Y() {}\nfn z() {}\n",
    );
    let service = ReviewData::default();
    let found = marks(
        &service,
        repo,
        &[
            "docs/my notes.txt",
            "docs/quote\"d.txt",
            "docs/image.png",
            "docs/renamed.rs",
            "docs/edited-now.rs",
        ],
    );
    assert_eq!(found[0].added, [(2, 2)], "a name with a space");
    assert_eq!((found[0].additions, found[0].deletions), (1, 1));
    assert_eq!(found[1].added, [(2, 2)], "a C-quoted name");
    assert!(found[2].binary && found[2].is_changed(), "{:?}", found[2]);
    assert!(found[2].added.is_empty());
    assert_eq!(
        found[3].renamed_from.as_deref(),
        Some("docs/moved.rs"),
        "{:?}",
        found[3]
    );
    assert!(found[3].added.is_empty(), "a pure rename marks no lines");
    assert!(found[3].is_changed());
    assert_eq!(found[4].renamed_from.as_deref(), Some("docs/edited.rs"));
    assert_eq!(
        found[4].added,
        [(2, 2)],
        "only the edited line of a renamed file"
    );
}

#[test]
fn a_file_deleted_from_the_working_tree_leaves_folder_reviews_and_counts() {
    let fixture = repository();
    let repo = fixture.root();
    std::fs::remove_file(repo.join("examples/hello-plugin/src/greet.rs")).expect("remove");
    let service = ReviewData::default();
    let source = resolved(&service, repo, file_set(vec![folder(HELLO)], Vec::new()));
    let entries = full_index(&service, repo, &source);
    assert!(
        !paths(&entries).contains(&"examples/hello-plugin/src/greet.rs"),
        "{:?}",
        paths(&entries)
    );
    assert_eq!(
        counted(&service, repo, vec![folder(HELLO)], false),
        u32::try_from(entries.len()).expect("count")
    );
}
