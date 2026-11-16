# Reviewing Files and Folders as They Are Now

A review of code as it is now, not of a change: whole files, whole folders, files matching a glob,
or the whole repository, read from the working tree. This document defines the source model, how
it resolves, the tree picker the new-review wizard uses to choose paths, the marks that show
uncommitted changes in such a review, the ignored-files toggle, every entry point (wizard, home,
file panel, CLI, review assistant), and the order in which it is built.

## Status

Built: the source model, resolution, uncommitted marks and path services in the data layer; the
`files` and `repository` kinds of `create_review`, `code_review.find_paths`, the profile recipes
and the parity test over every wizard mode; `bcode review files`, `bcode review repo --path` and
`--show-ignored`; the tree picker for Files, Repository limited to folders, **Limit to paths…**
and editing sources; the home suggestion and the file panel action; worktree marks in whole
files with `w`, the ignored-files toggle `alt+i`, both saved per review, and the `files.*`
configuration. The BMUX check column (step 3) is not built, and the picker ships its keyboard
fallback for folders (see [Deviations](#deviations-in-the-picker-and-the-review)). Deviations
found while building are written into the sections below and listed at the end.

Before this work:

* `ReviewSourceKind::File`, `FileRange` and `Repository` exist and resolve to browse mode
  (`ResolvedSourceMode::Browse`), listed with `git ls-files --stage` (tracked files only, no
  limit). A `File` whose path is a directory already lists the tracked files under it, as a
  side effect of git pathspecs.
* The wizard has no Repository mode, and its Files mode searches one path at a time
  (`review.files.search`), marking files with `tab`; each marked file becomes its own source.
* `code_review.create_review` cannot create files or repository reviews, and no tool resolves a
  natural name ("the hello plugin") to a path. The assistant compensated with shell commands and
  the `bcode` CLI, then gave up.
* Whole-file views show no sign of uncommitted changes, and ignored files cannot be reviewed.

## Decisions

1. **A selected folder is a folder selector, not a frozen file list.** Reopening a review lists
   the folder again, so files added since appear and deleted files disappear. The tree picker
   shows how many files the selection resolves to now, and the wizard's last step shows each
   picked source's count as it was chosen; a review shows the files it lists when it opens.
2. **Uncommitted changes are marked in whole-file views** of files, folder and repository
   reviews: lines changed since `HEAD` (staged or not) and untracked files. On by default
   (`files.uncommitted_marks`), toggled live per review (`w`, the palette, and the marks menu).
3. **Ignored files are hidden by default**, in the picker and in folder, glob and repository
   reviews (`files.show_ignored = false`), toggled live (`alt+i`). A file named explicitly is
   always included, ignored or not.
4. **One path syntax everywhere.** The CLI, the picker's filter, and the assistant's
   `create_review` take the same strings: `path`, `folder/`, a glob, or `path:START-END`.
5. **Tools keep parity with the wizard by construction.** Every source kind the wizard can make
   has a `create_review` argument form, enforced by an exhaustive match and a behavior test.
6. **Selection of a tree row by mouse needs a generic BMUX capability**: a tri-state check column
   in `tree_view` with its own hit region. It belongs in BMUX, not in Bcode (see step 3).

## Source model

### Selectors

`PathSelector` lives in `bcode_code_review_models` (owned types plus a syntactic parser, which
the `_models` rules permit):

```rust
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PathSelector {
    /// One file (`path`).
    File { path: String },
    /// Lines `start..=end` of one file (`path:START-END`).
    FileRange { path: String, start: u32, end: u32 },
    /// Every file under a folder, listed when the review resolves (`folder/`).
    Folder { path: String },
    /// Every file matching a git glob, listed when the review resolves (`src/**/*.rs`).
    Glob { pattern: String },
}
```

* Paths are repository-relative, `/`-separated, without `.`/`..` components, never absolute
  (validated with `git::validate_relative_path`; a rejected path is a per-source
  `path_rejected` diagnostic, as today).
* `PathSelector::parse(&str) -> Result<ParsedSelector, SelectorError>` is purely syntactic:
  * a trailing `:N-M` (or `:N`) on a string without glob characters is a range;
  * `*`, `?` or `[` make a glob (`\` escapes them);
  * a trailing `/` makes a folder;
  * anything else is `ParsedSelector::Path`, which the caller classifies as `File` or `Folder`
    by looking at the working tree (a path that exists as neither is an error naming it).
* Each selector maps to one git pathspec with explicit magic, so a path is never reinterpreted
  as a pattern: `:(literal)path`, `:(literal)folder/`, `:(glob)pattern`.

### Source kinds

One new kind, and new behavior for `Repository`:

```rust
/// Files, folders and globs as they are now, in one source.
FileSet {
    /// What is reviewed (at least one).
    include: Vec<PathSelector>,
    /// Files and folders left out of the folders and globs of `include`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    exclude: Vec<PathSelector>,
},
```

* `FileSet` is one source with one source header, however many selectors it has, so a review of
  "these three folders and that file" reads as one group instead of one group per file.
* Exclusions exist so that unselecting one file inside a selected folder keeps the folder
  dynamic. Only `File` and `Folder` selectors may be excluded.
* **A repository limited to folders is a `FileSet` of `Folder` selectors.** It is not a separate
  kind: it lists the same files. The wizard labels it `Repository: examples/, docs/`.
* `Repository` (the whole repository) keeps its kind. Its listing now includes untracked files
  that are not ignored, and follows the ignored toggle, like a folder at the root.
* `File` and `FileRange` stay as they are and remain what the wizard writes for a single file or
  range, so a review of one file still opens in older builds.
* **Compatibility.** An older build reads `file_set` as `ReviewSourceKind::Unsupported`,
  preserves it unchanged, and shows the existing "not supported by this version" warning; it
  never falls back to reviewing something else (the same rule as `working_tree_paths`). Newer
  builds read every older kind unchanged.
* Labels: `Files in examples/hello-plugin/`, `Files: src/lib.rs, docs/ +2`, with
  `(1 excluded)` appended when there are exclusions.

### Resolution

`resolve_sources` turns `FileSet` into one browse `ResolvedSource` with one new field,
`selection: Option<BrowseSelection>` (absent when serialized empty, so older peers ignore it and
a source without it keeps today's tracked-only listing of `paths`):

* `include: Vec<PathSelector>` and `exclude: Vec<PathSelector>`: used instead of `paths`; an
  empty `include` lists the whole repository.
* `untracked: BrowseUntracked`: `Tracked` (the default when absent: today's behavior),
  `Unignored` (tracked plus untracked files that are not ignored), or `All` (ignored files too).
* `max_files`: the listing limit (below).

`ResolveReviewSourcesRequest` gains `browse: BrowseOptions { include_ignored, max_files }`
(`#[serde(default)]`). The client sets it from the review's ignored toggle and `files.max_files`;
`Repository`, `FileSet`, `File` and `FileRange` resolve to `Unignored` or `All` accordingly.
`File` and `FileRange` carry a selection of their one file, so a single untracked or ignored file
is listed too (an older client, which drops the selection, lists it only when tracked).

The browse listing (`compute_listing`, `ResolvedSourceMode::Browse`) runs, with the selectors'
pathspecs and each exclusion as `:(exclude,literal)…`:

1. `git ls-files -z --stage -- <pathspecs>`: tracked files, as today;
2. with `Unignored` or `All`: `git ls-files -z --others --exclude-standard -- <pathspecs>`;
3. with `All`: `git ls-files -z --others --ignored --exclude-standard -- <pathspecs>`;
4. every explicit `File`/`FileRange` selector not found by these (an ignored file named
   directly) is checked with `symlink_metadata` and added when it is a regular file inside the
   root.

Entries are merged, de-duplicated and sorted by path. Untracked entries carry the new index
flag `ReviewIndexFlag::Untracked`, ignored ones `ReviewIndexFlag::Ignored`; the file panel and
file headers show them (below).

**Dynamic by construction.** Browse sides are the working tree, and a working-tree listing is
never cached (`listing_key` reports it live), so every open and every refresh lists again: a file
added under a selected folder appears on reopen without any stored state changing.

### Limits

Normal interactive paths stay bounded:

* `files.max_files` (default 5,000, at most 100,000) caps one source's listing. Beyond it the
  listing keeps the first `max_files` paths in path order, and `review.index.get` reports the
  full count as `untruncated_entries` (the count is only known once the source is listed, so it
  is not a resolve diagnostic). The review warns once per cut source, with a "Some files are
  not listed" toast whose text `listing_truncated_message` builds: "Files in examples/ resolves
  to 12,345 files; showing the first 5,000 (files.max_files). Choose narrower folders or raise
  the limit." A refresh that cuts the same way does not repeat it. `code_review.list_files`
  puts the same messages in `truncated`, ahead of the files, so they survive the tool output
  limit. The file panel and the source header count the files listed.
* Each git listing is read as a stream: the first `max_files` records are kept and the rest only
  counted, so a folder of millions of files costs the memory of `max_files` paths.
* Ignored listings use `--directory` when they only feed the picker (below), so an ignored
  `target/` costs one entry, not millions. A folder git prints along with its ignored files (a
  folder that is not ignored itself but holds only ignored files) keeps its files instead. With
  ignored files shown, selector counts list each ignored folder a folder selector covers with
  git, at most 100,000 files each (beyond that `truncated` marks counts as lower bounds). A
  review that includes ignored files lists them in full, under `max_files`.
* Listings and the path index leave out what is not a file now: a tracked file deleted from
  the working tree (`git ls-files --deleted`), and a nested repository that is not a submodule
  (an untracked clone, printed by git as its folder with a trailing `/`). A submodule is listed
  once, as its commit.
* Whole-file content stays bounded by the existing blob limits (64 MiB per blob, the blob cache,
  and document residency).
* Selectors per source: at most 200 include and 200 exclude selectors (an `invalid_request`
  error beyond).

### Caching

* **Review listings:** live (above); concurrent requests for one listing share one git run
  through the existing single-flight.
* **Path index** (the picker, `find_paths`, and selector counts): one listing of the repository
  (tracked, untracked, and with the toggle, ignored files collapsed to directories) built into a
  path trie with per-folder file counts, held by the code review service in an LRU of 8
  repositories, keyed by repository root and `include_ignored`. An entry is reused for at most
  2 seconds and only while the git index file's size and modification time are unchanged (a
  commit, checkout or `git add` rewrites it); `review.paths.{list,search,resolve}` take
  `refresh`, which the picker sets when it opens and on `ctrl+r`.
  The trie holds at most 200,000 paths; beyond that the index is marked `truncated`, folder
  counts read "200,000+", and the picker says so.
* **Uncommitted marks:** per file, keyed by path, `HEAD`, the index file's stamp, and the file's
  size and modification time, in a bounded in-memory LRU (1,024 files).

### Service operations

New operations of the code review service (typed requests and responses in the models crate):

| Operation | Request | Response |
|---|---|---|
| `review.paths.list` | `repo_path`, `dir` (`""` for the root), `include_ignored`, `refresh`, `cursor`, `limit` | The folder's children, folders first: `name`, `path` (folders end with `/`), `kind` (`file`/`folder`), `files` (count under a folder; `None` for an ignored folder), `changed` (files changed since `HEAD`, untracked included), `status` (`tracked`/`untracked`/`ignored`); `total_entries`, `truncated`. A folder inside an ignored folder is read from the file system. |
| `review.paths.search` | `repo_path`, `query` (fuzzy text, or a glob), `include_ignored`, `refresh`, `limit` | Matching files and folders, best first (the ranking `review.files.search` uses); a glob's file matches in path order; `total_matches`, `glob`, `truncated` |
| `review.paths.resolve` | `repo_path`, `include`, `exclude`, `include_ignored`, `refresh` | Per selector: `files` now (or an `error` such as "missing.rs: no such file"), per exclusion the files it removes, the `total` after exclusions, and `changed`, the files of `total` changed since `HEAD` |
| `review.paths.find` | `repo_path`, `query`, `kinds`, `include_ignored`, `limit` | `find_paths` matches (below) |
| `review.worktree.marks` | `repo_path`, `paths` (at most 100, so the viewport's files take one request) | Per path: `untracked`, `missing`, `added: Vec<LineSpan>`, `removed: Vec<RemovedSince>` (position and text), `was` (a changed line's text at `HEAD`, for word marks), `additions`, `deletions`, `too_large`, `binary` (content changed, no lines), `renamed_from` (the path at `HEAD` of a staged rename) |

`review.files.search` stays for older clients.

## Uncommitted marks

A browse file's lines that differ from `HEAD` are marked in the whole-file view.

* **Computation.** `review.worktree.marks` runs one `git ls-files` (which files are tracked),
  one `git diff --cached -M --name-status --diff-filter=R HEAD` (renames staged since `HEAD`:
  a renamed file's previous path is outside the request, so rename detection reads the whole
  index), and one `git diff --no-color --no-ext-diff --no-textconv -U0 HEAD -- <paths>` for the
  request's tracked files (staged and unstaged together, against `HEAD`; against the empty tree in
  a repository without commits), with `-M` and the previous paths when some were renamed, else
  `--no-renames`. A renamed file is compared with its previous content, so a pure rename marks
  no lines; a binary change has no lines and answers `binary`. Paths are read as git prints
  them: unquoted, without the tab git appends to a name with a space. Added and modified new-side lines become `added` spans; a
  changed line's old text is in `was`; removed lines beyond the changed ones become markers before
  the new-side line they precede, with their text. Untracked files are `untracked: true` and carry
  no spans. A file over 4 MiB answers `too_large` before git runs.
* **Document.** The marks reuse the document's existing per-file marks slot (`MarksState` and
  `Marks`), loaded through a document need for files in the viewport, exactly as comparison
  marks are; a browse file's marks request is the worktree request instead of a comparison one.
  Rendering, word marks excepted, is the comparison's: the gutter bar and row tint by
  `compare.mark_style`, and removed code as a one-row marker, "▾ 2 lines removed since HEAD",
  expanding to the removed text. Themes get `theme.uncommitted.bar` and `theme.uncommitted.diff`,
  which default to the `theme.since` roles.
* **Headers and panel.** A changed file's header carries `uncommitted +3 −1`; a renamed one
  `uncommitted renamed` (with counts when it was also edited), a changed binary file
  `uncommitted binary`; an untracked file's `untracked`; an ignored file's `ignored`. File panel rows carry `M`, `?` and `!` status
  letters and folders aggregate them. The source header adds `· 2 uncommitted`.
* **Navigation.** `]`/`[` (next and previous change) stop on marked runs in a browse file.
* **Narrow widths.** The bar takes the first gutter cell (BMUX `DiffRowColumn::cells`) and is
  dropped with the full gutter, leaving the tint; header badges shrink
  `uncommitted +3 −1` → `+3 −1` → `●` by fit, measured with BMUX text layout, and hit regions
  come from the painted variant.

```
 ▾ examples/hello-plugin/src/lib.rs                     uncommitted +2 −1
    9 │ pub fn register(host: &mut Host) {
▌  10 │     host.command("hello", greet);
▌  11 │     host.command("bye", farewell);
      ▸ 1 line removed since HEAD
   12 │ }
 ▾ examples/hello-plugin/src/new.rs                               untracked
    1 │ //! Added in the working tree.
```

### Toggle and configuration

* `[plugins.config."bcode.code_review".files] uncommitted_marks = true` (default).
* `w` (`bcode.code_review.uncommitted_marks`, scope `code-review`) shows or hides the marks in
  this review; the palette has it as "Show / hide uncommitted changes (lines changed since
  HEAD)".
* `b` in a review with browse sources opens the **marks menu**, built like the comparison menu
  (and as its second section when the review also has pull request sources): **Mark uncommitted
  changes** (on/off), **Mark style** (both, bar, tint, strong; for the session, previewed with
  `space`, put back with `esc`), and **Show ignored files**.
* The choice is user state, stored in the review's `ReviewPresentationState` as
  `uncommitted_marks: Option<bool>` (`None` follows the configuration); it never writes
  configuration.

## Ignored files

* `[plugins.config."bcode.code_review".files] show_ignored = false` (default).
* In a review, `alt+i` (`bcode.code_review.show_ignored`, scope `code-review`), the palette, and
  the marks menu toggle it; the choice is stored as `show_ignored: Option<bool>` in
  `ReviewPresentationState` and passed as `include_ignored` when the review resolves.
* In the picker, `alt+i` (`bcode.code_review.wizard_show_ignored`, scope `code-review-wizard`)
  toggles it for the picker. The picker opens with `files.show_ignored` (when editing, with the
  review's own choice). A review created or edited after a picker chose differently stores that
  choice as `show_ignored` (the workspace create request takes initial `presentation`).
* `code_review.create_review` takes `show_ignored`; the assistant reads a review's files with
  the review's choice.
* In a review without folder, glob or repository sources, `alt+i` changes nothing and says so;
  likewise `w` in a review without whole working-tree files.
* Explicit `File` and `FileRange` selectors include their file regardless.

## The tree picker

One component, `PathPicker` (`packages/code-review-ui/src/path_picker.rs`), is used for:

| Purpose | Selectable | Folder counts | Result |
|---|---|---|---|
| Files | files, folders, globs, one-file ranges | files | `File`/`FileRange` for one plain file, else `FileSet` |
| Repository limited to folders | folders | files | `FileSet` of `Folder` selectors |
| Uncommitted changes in paths | files, folders | changed files | `WorkingTreePaths` (staged/unstaged as chosen) |
| Editing a review's source | as the source's purpose | as above | the edited source, id kept |

It composes BMUX components only: `picker_frame`, `text_input` for the filter, `tree_view` for
the tree and for the selection list, `key_hint_bar` for the footer, `empty_state` for nothing to
show, and a `checkbox` for the ignored toggle. All widths come from BMUX layout; nothing is
measured by bytes or characters.

### Layout

Wide (the frame is at least 90 columns): the tree and the selection side by side.

```
┌ New review · Files ────────────────────────────────────────────────────────────────────┐
│ / hello                                                                                 │
│ ┌ Repository ─────────────────────────────────────┐ ┌ Selected · 2 items · 9 files ──┐ │
│ │ ▾ [-] examples/                       12 files  │ │ [x] examples/hello-plugin/  7  │ │
│ │   ▾ [x] hello-plugin/                  7 files  │ │ [x] README.md               1  │ │
│ │       [x] Cargo.toml                            │ │ [x] src/lib.rs:10-40        1  │ │
│ │     ▸ [x] src/                         5 files  │ │                                │ │
│ │       [x] README.md                  M          │ │                                │ │
│ │   ▸ [ ] other-plugin/                  5 files  │ │                                │ │
│ │   [ ] notes.txt                      ?          │ │                                │ │
│ └─────────────────────────────────────────────────┘ └────────────────────────────────┘ │
│ [ ] Show ignored files                                                                  │
│ space select · ←→/h l fold · / filter · alt+l selected · alt+i ignored · enter continue │
└─────────────────────────────────────────────────────────────────────────────────────────┘
```

Narrow (under 90 columns): the selection collapses to one summary row above the footer;
`alt+l` (or a click on it) shows the selection in place of the tree.

```
┌ New review · Files ──────────────────┐
│ / hello                              │
│ ▾ [-] examples/             12 files │
│   ▾ [x] hello-plugin/        7 files │
│       [x] Cargo.toml                 │
│ Selected: 2 items · 9 files  ▸       │
│ space select · / filter · enter go   │
└──────────────────────────────────────┘
```

Tiny (under 24 columns): trailing counts and status letters are hidden whole by the tree's
trailing layout before the label's minimum; labels end-truncate keeping the file name
(`shrink_span` on the directory part); the footer shows the widest hint variant that fits, down
to none. At zero width nothing paints and no hit region is registered.

### Rows

* Folder rows: chevron, check, `name/`, trailing count (`7 files`, narrow `7`). File rows:
  check, name, trailing status (`M` changed since `HEAD`, `?` untracked, `!` ignored). Ignored
  rows use the dim role.
* Ignored directories (shown only with the toggle) list lazily: expanding one asks
  `review.paths.list` for that folder.
* The check is tri-state:
  * `[x]`: selected, by its own selector or a selected ancestor folder or matching glob, and not
    excluded;
  * `[-]`: a folder with some but not all of its files selected (selections inside it, or a
    selected folder with exclusions inside it);
  * `[ ]`: nothing under it selected.
  Glyphs come from theme icons (`icons.check_on`, `icons.check_partial`, `icons.check_off`).

### Selecting

* Selecting a folder adds a `Folder` selector and removes the selectors and exclusions it now
  covers.
* Selecting a file whose folder is unselected adds a `File` selector. Selecting every file of a
  folder one by one does **not** turn them into a folder selector (that would change meaning
  when files are added); the folder shows `[x]` because all its files are selected.
* Unselecting a file or folder that is covered by a selected folder or glob adds an exclusion;
  unselecting an explicit selector removes it.
* Unselecting a `[-]` folder removes every selector and exclusion under it.
* **Filter.** Typing filters the tree by fuzzy name across the whole repository
  (`review.paths.search`): matches show as a flat list of whole paths, best first, at most 500
  rows; when there are more, the frame title says "500 of 2,345 matches, type more to narrow".
  Folders match by name too.
* **Glob.** When the filter contains `*`, `?` or `[`, it is a glob: the tree shows its matches,
  and the first row is `Select pattern src/**/*.rs · 42 files now`, which adds a `Glob`
  selector. Individual matches can still be selected one by one.
* **Ranges.** A filter ending in `:START-END` whose path part names exactly one file shows
  `Add lines 10–40 of src/lib.rs` as the first row; selecting it adds a `FileRange`.
* **Selection list.** Each selector with its current count (from `review.paths.resolve`,
  refreshed as the selection changes): `examples/hello-plugin/ 7`, `src/**/*.rs 42`,
  `src/lib.rs:10-40 1`, exclusions as `− examples/hello-plugin/tmp/`. A selector that resolves
  to nothing shows `0` in the warning role; one that no longer exists shows "missing".
  Removing a row (its check, `space`, `x`, `delete`) removes that selector or exclusion.
* **Continue.** `enter` with a non-empty selection continues to the title step with one source
  (the result column above); with an empty selection, `enter` on a file selects it and
  continues, as the Files step does today.

### Keys

The tree has two input modes. In **tree mode** (the default) letters are commands; in **filter
mode** letters type into the filter. Any printable key that is not a tree-mode key switches to
filter mode and types itself, so typing a name filters at once; names that start with `h`, `j`,
`k` or `l` start with `/`.

| Mode | Keys | Action |
|---|---|---|
| tree | `up`/`k`, `down`/`j`, `pageup`, `pagedown`, `home`, `end` | Move |
| tree | `left`/`h` | Collapse the folder, or go to its parent |
| tree | `right`/`l` | Expand the folder, or go to its first child |
| tree | `space`, `tab` | Select or unselect the row |
| tree | `shift+tab` | Select every visible row from the last selected one to this one |
| tree | `/` | Filter mode (empty) |
| tree | `enter` | Continue (see above) |
| tree | `esc` | Clear the filter; with no filter, back one step |
| filter | text | Edit the filter |
| filter | `up`, `down` | Move in the tree |
| filter | `tab` | Select or unselect the focused row |
| filter | `enter`, `esc` | Tree mode, filter kept (`esc` again clears it) |
| any | `alt+l` | Focus the selection list (and back) |
| any | `alt+i` | Show or hide ignored files |
| any | `ctrl+r` | List the repository again |
| any | `ctrl+k` | Ask AI |
| selection | `up`/`k`, `down`/`j` | Move |
| selection | `space`, `x`, `delete`, `backspace` | Remove the selector |
| selection | `esc`, `alt+l` | Back to the tree |

`tab`, `shift+tab`, `esc` and `ctrl+k` keep their wizard actions (`wizard_mark`,
`wizard_mark_range`, `wizard_back`, `wizard_assistant`). New wizard keymap actions:
`wizard_show_ignored` (`alt+i`), `wizard_selection` (`alt+l`), `wizard_relist` (`ctrl+r`), and
`wizard_edit_source` (`ctrl+e`, below). None of these keys is bound in `code-review-wizard`
today, and arrows, `h`/`l`/`j`/`k`, `space`, `/` and text are component input, as in today's
picker.

### Mouse

* A click on a row focuses it; on a folder of the tree it also expands or collapses it, on any
  other row it selects or unselects it.
* A right click on any row, a folder of the tree included, selects or unselects it.
* A click on the filter enters filter mode; a click on the summary row or the selection list
  focuses the list; a click on a selection row's check removes it.
* A click on **Show ignored files** toggles it; the wheel scrolls the pane under the pointer.
* Every hit region is registered by the BMUX component that paints it, for visible rows only, so
  clipping, scrolling and resizing never leave stale regions.

### BMUX capability

`tree_view` has no per-row control with its own hit region; Bcode must not paint a check glyph
into the label and guess its cells. The generic addition, in BMUX:

* `TreeViewItem::check: Option<TreeViewCheck>` with `TreeViewCheck::{Unchecked, Partial,
  Checked}`, painted as a leading cell group after the indentation and chevron, with glyphs and
  styles from `TreeViewStyles`, measured by BMUX text layout, never hidden by the trailing
  layout;
* a hit region for the check cells of each visible row, and
  `TreeViewOutcome::CheckToggled { visible, source }` for a click on it;
* `TreeViewKeyboardPolicy::space_checks` (space reports `CheckToggled` instead of toggling
  expansion);
* tests at tiny widths, with CJK, emoji and combining-mark labels, deep indentation, and
  agreement of painted cells with hit regions.

If that cannot land first, the picker ships keyboard selection plus a click on a file row that
toggles it, with folders selected by keyboard only, and gains the check hit region when BMUX
does.

## Wizard

Step 1 lists, in order: Working tree, Current branch, Compare branches, Commits, Pull request,
Stack, Files, Repository.

* **Files** (`whole files, folders, globs, or line ranges`): the tree picker, purpose Files.
* **Repository** (`the whole repository, or some folders, as they are now`): two rows, **Whole
  repository** (`Repository`) and **Limit to folders…** (the tree picker, purpose Repository).
* **Working tree** gains **Limit to paths…** after unstaged, staged and both: the tree picker,
  purpose Uncommitted (counts are changed files), then the staged/unstaged choice; the result is
  `WorkingTreePaths`.
* **Editing sources.** In the title step, `ctrl+e` (`wizard_edit_source`) on a `FileSet`,
  `File`, `FileRange`, `Repository` or `WorkingTreePaths` source opens the picker on it, and its
  result replaces the source with its id kept, so threads and viewed marks stay attached. `enter`
  keeps removing a source and `tab` keeps including or excluding it.
* The step-3 source list shows the count each picker source selected when it was chosen:
  `Files in examples/hello-plugin/ · 7 files now · enter removes` (changed files for
  uncommitted changes in paths). A source not chosen in the picker shows none.

## Entry points

* **CLI.** `bcode review files <SELECTOR>... [--repo PATH]` opens a transient review of the
  selectors (the syntax above). Relative selectors are relative to the process's working
  directory when it is inside the repository, else to the repository root, and must stay inside
  the repository; one plain file becomes `File`, anything else `FileSet`. `bcode review repo
  [--path FOLDER]...` opens the whole repository, or a `FileSet` of the folders. A selector that
  names nothing fails before the app opens: "examples/helo-plugin: no such file or folder".
  `--show-ignored` on both comes with the ignored toggle in step 5, which owns the review's
  presentation state it sets.
* **Submodules.** A submodule's folder is a file to the selector syntax (git lists it as one
  entry, its commit), so `vendor/sub` is `File`, never `Folder`.
* **Home.** A suggestion row, after **New review…**: **Review files or folders…** (`whole files,
  folders, or the repository as it is now`), opening the wizard on the Files picker; the palette
  has it as "New review: files or folders…".
* **File panel.** On a folder row, `alt+r` (`bcode.code_review.files_review_folder`, scope
  `code-review-files`) and the palette's "Review this folder as it is now" open a transient
  `FileSet` review of that folder (`ctrl+s` saves it, as for every transient review). On a file
  row the same action reviews that file.

## Assistant tools

### `code_review.create_review`

The source schema gains two kinds:

```json
{
  "kind": "files",
  "paths": ["examples/hello-plugin/", "src/**/*.rs", "README.md", "src/lib.rs:10-40"],
  "exclude": ["examples/hello-plugin/tmp/"]
}
{ "kind": "repository" }
{ "kind": "repository", "paths": ["examples/", "docs/"] }
```

* `files`: `paths` (required, at least one) in the selector syntax; a plain path is a file or a
  folder by what it is in the working tree. `exclude` (files and folders) is optional. With
  `directory` and no `paths`, the directory itself is the folder.
* `repository`: without `paths`, the whole repository; with `paths` (folders), a `FileSet` of
  them. With `directory` below the root and no `paths`, that folder.
* Paths are repository-relative; an absolute path inside the repository is taken as its
  relative path. A path that escapes the repository or names nothing is an `invalid` error
  naming it ("examples/helo-plugin: no such file or folder").
* Pull request, stack and stack layer sources take `provider` (default `github`), so a stack of
  any review-source provider the wizard offers can be created too.
* The kind descriptions say: "files: whole files, folders, globs or line ranges as they are now
  (not a change); repository: the whole repository or some folders as they are now".
* `ReviewSourceKind -> SourceArgument` (`create::source_argument`) is an exhaustive match over
  every kind (no wildcard arm): a new kind does not compile until the tool can express it.

### `code_review.find_paths`

A read-only plugin tool (`Access::Read`, category `review_reads`) that resolves a natural name
to repository paths.

```json
{
  "type": "object",
  "properties": {
    "query": {"type": "string", "description": "What the user called it: \"the hello plugin\", \"bcode-git\", \"the review wizard\", \"Cargo.toml\"."},
    "kinds": {"type": "array", "items": {"enum": ["folder", "package", "file"]}, "description": "What to look for (default all)."},
    "directory": {"type": "string", "description": "Repository of this directory (default the session's)."},
    "include_ignored": {"type": "boolean", "description": "Also search ignored files (default false)."},
    "limit": {"type": "integer", "minimum": 1, "maximum": 50, "description": "Matches returned (default 10)."}
  },
  "required": ["query"],
  "additionalProperties": false
}
```

Answer:

```json
{
  "matches": [
    {"path": "examples/hello-plugin/", "kind": "package", "package": "hello-plugin",
     "manifest": "examples/hello-plugin/Cargo.toml", "files": 7, "matched": "package name", "score": 100},
    {"path": "examples/hello-plugin/src/hello_plugin.rs", "kind": "file", "files": 1, "matched": "file name", "score": 90}
  ],
  "total": 2,
  "ambiguous": false,
  "weak": false,
  "truncated": false
}
```

Matching rules (`review.paths.find`, over the path index, so `.gitignore` is respected):

1. **Normalize.** Lowercase; split the query and each candidate name into words on
   non-alphanumerics and camel-case boundaries; drop leading stop words (`the`, `a`, `an`, `my`,
   `our`, `this`, `that`) and trailing kind words (`folder`, `directory`, `dir`, `package`,
   `crate`, `module`, `file`) unless nothing else remains. `plugin`, `app`, `test` and similar
   stay: they are part of names.
2. **Candidates.** Folders (by base name), packages (folders holding a `Cargo.toml` with
   `[package] name`, a `package.json` with `name`, or a `pyproject.toml` with `[project] name`;
   at most 2,000 manifests, each under 256 KiB, read through the working tree), and files (by
   base name, extension included).
3. **Score**, highest wins per path:
   * 100: the query equals a path suffix (`examples/hello-plugin`), or a package name, or a
     folder or file name, after normalization (`hello plugin` = `hello-plugin` = `HelloPlugin`);
   * 90: every query word is a name word, in order;
   * 80: every query word is a name word or a name word's prefix;
   * 60: every name word is a query word (the name is a part of what was asked);
   * up to 40: a fuzzy subsequence match of the joined words (`review.files.search` scoring).
   Folder matches score 5 lower, so a package or file of the same quality ranks first; each path
   appears once, with its best match (a package on a tie). A package at the repository root is
   not a candidate: it is the whole repository, not a folder of it.
4. **Order.** Score, then depth (shallower first), then path length, then path.
5. **Ambiguity.** `ambiguous` is true when the two best matches name different paths and score
   within 5 of each other at 80 or more. `weak` is true when the best match scores below 50:
   it matched only part of a word or scattered letters ("auth" in `oauth-client/`). The
   assistant asks only in these cases (which one, or whether the weak match is meant), or when
   nothing matched.

Counts are files under a folder now (with the ignored rule above). Output is bounded by `limit`
and the tool output limit.

### Profile

* `code_review.find_paths` joins **review reads**; `code_review.create_review` stays in
  **review writes**.
* Recipes added to the profile instructions:
  * "A review of X as it is now", "all of X", "review the X folder": `code_review.find_paths`
    with X, then `code_review.create_review` with `{kind: files, paths: [<folder>]}`, then
    `bcode.code-review.open_review`. Ask only when `find_paths` says `ambiguous` or `weak`, or
    finds nothing.
  * "X's changes", "what changed in X": a diff review (`working_tree` with `paths`, or the
    branch, range or pull request the user names), not a files review.
  * "The whole repository": `{kind: repository}`.
* Rule added: never run the `bcode` CLI through the shell to create or open reviews; when a tool
  cannot do what was asked, say what is missing.

### Parity test

`every_wizard_mode_can_be_created_through_create_review` (code-review-ui `tests/wizard.rs`,
built in step 2 for the current modes): for every `SourceType` (an exhaustive match, so a new
mode must be added to the test to compile) and each of its details (working tree: unstaged, staged, both, paths; files: a file, a range, a folder, a
glob, a mix with an exclusion; repository: whole, folders; commits; ranges; branches; pull
request, stack and stack layers against the fake provider), drive the wizard until it creates the review, convert each created source with
`create::source_argument`, run `create::plan`, and assert the same `ReviewSourceKind`s. Step 4
adds the Repository mode, the folder and glob details and **Limit to paths…**.

## Build plan

Each step is one or more commits that build, pass `cargo fmt`, `cargo clippy --workspace
--all-targets -- -D warnings` and their tests on their own.

1. **Source model, resolution, marks and path services** (`code-review` models and service).
   `PathSelector` and its parser, `ReviewSourceKind::FileSet` with `Unsupported` fallback,
   `ResolvedSource::{selectors, exclude, untracked}`, `include_ignored` on resolve, the merged
   browse listing with `Untracked`/`Ignored` flags and `max_files`, the path index and
   `review.paths.{list,search,resolve,find}`, `review.worktree.marks`. Tests: parser cases
   (ranges, globs, escapes, trailing slash, rejected paths); `FileSet` round trip and an
   unknown kind preserved; a folder selector lists a file added after creation; untracked
   included, ignored hidden and shown, an explicit ignored file always included; exclusions;
   `max_files` truncation diagnostic; `Repository` lists untracked files; worktree marks for a
   modified, a staged, an untracked and a too-large file and an empty repository; `find_paths`
   for folder, package (Cargo and npm), file, suffix, camel case, ambiguity, ignored, and no
   match.
2. **Tools and CLI.** `create_review` kinds `files` and `repository`, `create::source_argument`,
   `code_review.find_paths`, profile categories and recipes, `bcode review files`,
   `bcode review repo --path`, `--show-ignored`. Tests: `plan` for each new form and each error;
   the tool round trip through the plugin; CLI parsing and resolution relative to the working
   directory; the profile offers both tools; the parity test for the kinds the wizard makes
   today.
3. **BMUX check column** (in the BMUX checkout, through `scripts/bmux-dev.sh`), committed there
   and reported; Bcode relies on it only once the dependency is updated.
4. **Wizard picker, Repository mode, and entry points.** `PathPicker` and its purposes, the
   Files and Repository modes, **Limit to paths…**, editing sources with `ctrl+e`, the home
   suggestion, the file panel action, keymap actions. Tests (keyboard and mouse, at 120, 60, 20
   and 8 columns, with CJK and emoji names): expand and collapse, tri-state after selecting a
   folder and excluding a child, filter, glob row, range row, selection list counts and removal,
   ignored toggle, hit regions agree with painted rows after scroll and resize, zero width
   registers nothing; the parity test over every `SourceType`.
5. **Marks in the review, toggles, documentation, live acceptance.** Worktree marks in the
   document, header badges, panel letters, `]`/`[`, `w`, `alt+i`, the marks menu, presentation
   state, `files.*` configuration; `docs/code-review-app.md` (modes, picker, keys, configuration)
   and `docs/code-review-assistant.md` (tools, recipes). Tests: marks on by default, off by
   configuration, toggled live and kept on reopen; ignored toggle re-resolves; narrow-width badge
   variants. Live: on the home screen, the fake provider's scripted "create a review for the
   hello plugin" calls `find_paths` → `create_review` (files, `examples/hello-plugin`) →
   `open_review` without a question, and the review shows every file under the folder with a
   modified file marked; and a tmux run of the wizard picker that selects a folder and a file and
   creates the review.

## Deviations in the picker and the review

What was built differs from the sections above as follows.

* **No BMUX check column.** Bcode can rely on a BMUX change only once its dependency is
  updated, so the picker ships the fallback: a click on a file row selects or unselects it, a
  click on a folder row of the tree expands or collapses it, and folders are selected with the
  keyboard, a right click, or a click on a folder the filter found (a filter list has no
  expansion). Check
  glyphs are painted as part of the row label (`[x]`, `[-]`, `[ ]`); hit testing uses the tree
  view's own `row_rect` over the painted layout, never the glyph's cells.
* **One pane at every width.** The selection list replaces the tree (`alt+l`, or a click on the
  summary row) instead of standing beside it at wide widths.
* **Filter results are a flat list** of whole paths, best first, not the tree with ancestors
  expanded.
* **Filter mode keys.** `enter` continues from filter mode too (with nothing selected it selects
  the focused row first), `esc` returns to tree mode, and `space` types a space (`tab` selects).
* **No marks menu on `b`, no mark style or theme roles of their own, no `]`/`[` stops.** The
  marks use the comparison's `compare.mark_style` and `theme.since` roles; `w`, `alt+i` and the
  palette toggle them.
* **Badges.** The header badge is `uncommitted +N −M`, part of the header facts that give way
  as a whole at narrow widths; untracked and ignored files show the `untracked` and `ignored`
  flags. The file panel and the source header carry no uncommitted counts.
* **One file per marks request.** A document need asks `review.worktree.marks` for one file as
  its content becomes resident, like comparison marks, instead of batching the viewport.

## See also

* [Code review app](code-review-app.md): commands, keys and configuration.
* [Code review assistant](code-review-assistant.md): tools, profile and recipes.
* [Stack history and comparisons](review-stack-history.md): the marks rendering reused here.
