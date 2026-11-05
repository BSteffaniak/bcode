# Git client (`bcode git`)

`bcode git` is an interactive, lazygit-style terminal git client. The same UI ships as the
standalone `bcode-git` binary, which depends only on the git, keymap, theme and BMUX crates and
none of bcode's agent, daemon, provider or session code.

```sh
bcode git            # inside full bcode (requires the `git-ui` build feature)
bcode git -C ../repo
bcode-git            # standalone binary
bcode-git --print-config-paths
```

## Architecture

| Crate | Responsibility |
|---|---|
| `bcode_git_models` | Portable repository models (status, diffs, commits, refs, stash, worktrees, rebase todo items). |
| `bcode_git` | Runs the user's `git` CLI with machine-readable output and parses it. Builds and applies partial patches for hunk and line staging. |
| `bcode_git_ui` | The BMUX program: panels, popups, keymap actions, theme roles, diff presentation and filesystem watching. |
| `bcode_git_ui_cli` | The `bcode-git` binary. |
| `bcode_git_plugin` (`git-ui` feature) | Registers the `bcode git` subcommand. |
| `bcode_keymap`, `bcode_tui_theme` | Keybinding registry and theme definitions shared with the main TUI. |

Git is invoked as a subprocess so hooks, credential helpers, signing, attributes, LFS and
configuration behave exactly as they do in a terminal. Every invocation forces stable output
(`color.ui=never`, `core.quotepath=false`, standard diff prefixes, `--no-pager`), and read-only
commands set `GIT_OPTIONAL_LOCKS=0` so refreshing never contends with other git processes.
`GIT_TERMINAL_PROMPT=0` makes credential prompts fail fast instead of hanging the UI; use a
credential helper or SSH agent for remotes that require authentication.

All repository work runs off the input thread. Panel data loads in parallel and repeated
navigation coalesces to the latest request, so large repositories stay responsive. Mutations run
one at a time in order and are recorded in the command log (`@`).

### Partial staging

Hunk, line and range staging build a minimal patch from the selected lines and apply it with
`git apply` (`--cached` for the index, `--reverse` for unstaging and discarding). Unselected
changes are neutralised so the side the patch applies to stays untouched, and git verifies context,
so a stale selection fails instead of corrupting the index. CRLF content, files without a final
newline, renames and untracked files (via intent-to-add) are supported; binary files are refused.

### Language-aware diffs

Both sides of a changed file (old and new) are highlighted as whole files and the tokens are
projected onto diff lines by line number. Block comments, strings and other multi-line constructs
therefore keep their colors inside every hunk. Files above `highlight.max_file_bytes`, or diffs
touching more than 40 files, fall back to highlighting each hunk side as a contiguous sequence.
Paired removed/added lines get word-level emphasis on the changed span.

## Layout

By default the left column holds five windows and the right side is the main view.

1. **Status**: repository, branch, ahead/behind, operation in progress.
2. **Files** · Worktrees · Submodules: changes as a collapsible tree (`` ` `` toggles a flat list).
3. **Local** · Remotes · Tags: branches with age, tracking state and tip subject.
4. **Commits** · Reflog: history with a colored graph; unpushed commits are highlighted.
5. **Stash**.

`]`/`[` switch tabs, `1`–`9` focus side windows by position and `0` the main view. `+`/`_` cycle
the screen mode:

* **normal**: every side window beside the main view.
* **half**: only the focused side window beside the main view; the others shrink to one-line
  titles you can click (or disappear with `collapsed_windows = "hidden"`).
* **full**: the focused window fills the screen.

Terminals narrower than 84 columns stack the side windows above the main view. Mouse clicks focus
windows and select rows; clicking the selected row again opens it; the wheel scrolls. Every part of
this is configurable; see [Layout settings](#layout-settings).

## Features

* **Files**: stage/unstage files or whole directories, stage all, discard (all or unstaged only),
  reset menu (`D`), ignore or exclude, stash variants (`S`), edit or open files, resolve conflicts
  with ours/theirs.
* **Staging view** (`enter` on a file): stage, unstage or discard single lines, ranges (`v`) or
  hunks (`a`); `tab` switches between unstaged and staged changes; `e` opens the file at the line.
* **Commits**: commit (stages everything when nothing is staged), commit without hooks, amend,
  commit with `$EDITOR`. The commit editor floats over the screen with a one-line summary and an
  optional description, which together form one message (summary, blank line, description).
  `tab` switches fields, `enter` commits from the summary and adds a line in the description,
  `alt+enter` or `ctrl+s` commits from either, pasting a multi-line message splits it, the summary
  shows a length guide, `ctrl+a` toggles amend and `ctrl+l` clears the message. Rewording uses the
  same editor. The editor is prefilled; see [Commit message prefill](#commit-message-prefill).
* **History**: squash, fixup, drop, edit, reword and reorder commits (`ctrl+j`/`ctrl+k`) through
  interactive rebase; create fixup commits and autosquash; amend staged changes into an older
  commit; revert; cherry-pick copy (`C`) and paste (`V`); soft/mixed/hard reset; tags; new branch
  from a commit; bisect; commit file lists with per-file checkout and custom patches.
* **Branches and remotes**: checkout (including by name), create, rename, delete (local, forced,
  remote), merge (fast-forward, no-ff, squash), rebase, fast-forward, upstream management, remote
  branch checkout, add/edit/remove/fetch remotes, push tags.
* **Sync**: fetch, pull (merge or rebase), push with upstream setup and force-with-lease when
  behind. Optional periodic background fetch.
* **Operations in progress**: merges, rebases, cherry-picks and reverts show a banner; `m` offers
  continue, abort and skip.
* **Undo/redo** (`z`, `ctrl+z`): a journal of `HEAD` positions for every operation that moves it.
  Undo uses `git reset --keep`, so local changes that would be overwritten abort the undo.
* **Diffing mode** (`W`): compare any selected ref or commit against a marked one.
* **Search and discovery**: `/` filters any list live, `?` lists the keybindings of the focused
  view and runs the chosen action, `ctrl+p` is a command palette over all actions, and a popup
  shows continuations while a key sequence (such as `g g`) is pending.
* **Custom commands**: user-defined shell commands with prompts, placeholders and confirmation.

## Configuration

The git UI has one settings schema with two homes, and both `bcode git` and `bcode-git` read both:

* **`bcode.toml`**, under `[plugins.config."bcode.git".ui]`, so bcode users (including generated
  configurations such as a Nix-built `bcode.toml`) need nothing else.
* **Dedicated `bcode-git` files**, which use the same schema at their root, without the prefix.

The schema is one Rust type (`GitUiConfig`), so every key below works in either place:

| Dedicated file | `bcode.toml` |
|---|---|
| `diff_context = 3` (root keys) | `[plugins.config."bcode.git".ui]` `diff_context = 3` |
| `[layout]`, `[files]`, `[commit]`, `[refresh]`, … | `[plugins.config."bcode.git".ui.layout]`, … |
| `[keybindings."git.files"]` | `[plugins.config."bcode.git".ui.keybindings."git.files"]` |
| `[theme]` | `[plugins.config."bcode.git".ui.theme]` |
| `[[custom_commands]]` | `[[plugins.config."bcode.git".ui.custom_commands]]` |

Layers, lowest first (later layers override earlier ones; tables merge key by key, while values
and arrays such as `windows` or `custom_commands` are replaced):

1. Built-in defaults.
2. The `bcode.toml` family, composed exactly as bcode composes it: the user file
   `$XDG_CONFIG_HOME/bcode/bcode.toml`, `bcode.toml` and `.bcode/bcode.toml` at the repository root,
   the `$BCODE_CONFIG` file (which must exist when set), inline `$BCODE_CONFIG_TOML`, and that
   family's `[composition]` profiles.
3. The dedicated family's own `[composition]` profiles (`active_profile`, `profiles.<name>.patch`,
   `layer_order`), applied over the `bcode.toml` result.
4. `$XDG_CONFIG_HOME/bcode-git/config.toml` (default `~/.config/bcode-git/config.toml`).
5. `bcode-git.toml` and `.bcode-git/config.toml` at the repository root (shareable), then
   `bcode-git.toml` in the git directory (per clone, never committed; per worktree for linked
   worktrees).
6. The `$BCODE_GIT_CONFIG` file (must exist when set).
7. Inline `$BCODE_GIT_CONFIG_TOML`.
8. `--config FILE`, then `--config-toml TEXT` (both entry points).

Keybindings and the theme also honour bcode's shared sections: `[tui.keybindings.scopes."git.*"]`
is the base that `keybindings` refines key by key, and each unset `theme` field falls back to
`[tui.theme]`, so the git UI follows your bcode theme unless told otherwise.

Problems are reported in the UI (and by `--print-config-paths`) with the file, profile and key
that caused them, for example
`~/.config/bcode-git/config.toml (profile laptop): layout.screen_mode: unknown variant ...`;
affected settings use defaults.

```sh
bcode-git --print-config-paths      # both families: each file, read or missing; active profiles
bcode-git --print-config            # effective settings as a dedicated file
bcode-git --print-config --nested   # the same, as the bcode.toml section
```

A profile in a dedicated file:

```toml
[composition]
active_profile = "laptop"

[composition.profiles.laptop.patch.layout]
screen_mode = "half"
```

The examples below use the dedicated form; in `bcode.toml`, put each table under
`plugins.config."bcode.git".ui`.

```toml
enabled = true                  # false hides `bcode git` (the standalone binary ignores it)
diff_context = 3
log_page_size = 300
show_graph = true
relative_dates = true
editor = "nvim"                 # default: $GIT_EDITOR, $VISUAL, $EDITOR, then vi

[files]
tree = true                     # false lists files flat; `` ` `` toggles for the session

[highlight]
enabled = true
max_file_bytes = 1048576

[refresh]
watch = true                    # filesystem watching; falls back to polling
poll_seconds = 10
auto_fetch_seconds = 0          # 0 disables background fetch

[confirm]
discard = true
force_push = true
hard_reset = true
delete = true

[[custom_commands]]
key = "ctrl+b"
scope = "git.branches"
command = "gh pr create --head {branch} --title {prompt.title}"
description = "Open a pull request"
prompts = [{ name = "title", title = "PR title" }]
confirm = false
interactive = false             # true hands the terminal to the command
show_output = true
```

### Layout settings

```toml
[layout]
screen_mode = "normal"          # normal | half | full at startup; `+`/`_` change it
side_panel_width = 33           # percent of the width in normal mode (10–90)
half_side_panel_width = 50      # percent of the width in half mode
collapsed_windows = "titles"    # half mode: other windows as clickable titles, or "hidden"
initial_window = "files"        # any view name below, or "main"
windows = [                     # top to bottom; each window lists its tabs; omit views to hide them
  ["status"],
  ["files", "worktrees", "submodules"],
  ["branches", "remotes", "tags"],
  ["commits", "reflog"],
  ["stash"],
]
expand_focused = true           # give the focused side window more height
expanded_weight = 3             # its height relative to the others
main_split = "flexible"         # staged/unstaged sections: flexible | stacked | side-by-side
main_split_min_width = 180      # flexible: side by side from this terminal width
portrait = "auto"               # stack side windows above the main view: auto | always | never
portrait_below_width = 84
status_bar = true               # false hides key hints; progress and messages still show

[layout.command_log]
show = false                    # `@` toggles it
height = 8
```

View names are `status`, `files`, `worktrees`, `submodules`, `branches`, `remotes`, `tags`,
`commits`, `reflog` and `stash`. Unknown or repeated names are reported and skipped. Number keys
follow the configured order, and the `focus-<view>` actions (unbound by default) jump to a view
wherever it lives. For example, one window on the left and the changes on the right, with a flat
file list:

```toml
[layout]
screen_mode = "half"
initial_window = "files"

[files]
tree = false
```

### Commit message prefill

A new commit editor starts with the first message found in these sources:

| Source (`sources` name) | Where | Notes |
|---|---|---|
| `env` | `$BCODE_GIT_COMMIT_MSG` environment variable | The message itself; used by the first commit editor of the session only. |
| `bcode-draft` | `BCODE_GIT_COMMIT_MSG` in the git directory | bcode's draft; anything may write it. |
| `lazygit-draft` | `LAZYGIT_PENDING_COMMIT` in the git directory | lazygit's unsent message; read, never written. |
| `merge` | `MERGE_MSG` | Left by a merge, cherry-pick or revert that stopped. |
| `squash` | `SQUASH_MSG` | Left by `git merge --squash`. |
| `edit-msg` | `COMMIT_EDITMSG` | Only when it differs from `HEAD`'s message, that is, a commit that did not land. |
| `template` | `commit.template` | Resolved like git (relative to the worktree, `~` expanded). |

The git directory is per worktree, so each worktree has its own draft. Git-generated files and
templates lose their comment lines; drafts keep lines starting with `#`, which are message
content (messages are committed with `--cleanup=whitespace`). The editor title names the source
and says when a draft replaces a `MERGE_MSG` or `SQUASH_MSG`.

While the editor is open, these files are watched (the worktree's git directory and the
`commit.template` file). When one is written, created or removed, the sources are read again and
the editor is filled, but only if both fields are empty at that moment. Typed text is never
replaced; an editor emptied by deleting what was typed counts as empty. The environment message
is not offered again. Without filesystem watching, the poll interval does the same.

Drafts follow the message:

* Cancelling an edited editor saves the text as the bcode draft; cancelling an emptied editor
  removes it. Untouched text from git's own files is not copied into a draft.
* Submitting saves the draft first, so a commit that fails (for example a rejecting hook) keeps it.
  Once the commit lands, the bcode draft is removed, and so is lazygit's if its message was used.
* Amending leaves drafts alone.

```toml
[commit]
prefill = true
sources = ["env", "bcode-draft", "lazygit-draft", "merge", "squash", "edit-msg", "template"]
save_draft = true               # false: never write BCODE_GIT_COMMIT_MSG (still read it)
```

For example, `BCODE_GIT_COMMIT_MSG="$(generate-message)" bcode-git`, or have a tool write the
draft file and open the editor with `c`.

### Importing a lazygit configuration

`bcode-git --import-lazygit [CONFIG_YML]` (or `bcode git --import-lazygit`) prints settings
converted from a lazygit `config.yml`, as a dedicated config file. Without a path it uses `$LG_CONFIG_FILE` or lazygit's default
location. The output covers layout, file tree, diff context, graph, refresh settings and
keybindings; values equal to bcode's defaults are left out, and anything without a bcode
equivalent, or a key a panel binding would shadow, is listed in `# note:` comments. Review it and
append it to a dedicated file, or add `--nested` for the `bcode.toml` form:

```sh
bcode-git --import-lazygit >> ~/.config/bcode-git/config.toml
bcode-git --import-lazygit --nested >> ~/.config/bcode/bcode.toml
```

Custom command placeholders are `{file}`, `{branch}`, `{commit}`, `{remote}`, `{tag}`, `{stash}`,
`{current_branch}`, `{repo}` and `{prompt.<name>}`. Values are shell-quoted.

Themes come from `[theme]` (`name`, `overlays`, `paths`), falling back to bcode's `[tui.theme]`
(default `bcode-dark`). Besides the standard roles the UI reads
optional `git.*` roles; see [TUI themes](tui-themes.md#git-client-roles).

## Keybindings

Every action is registered in the shared keymap registry. Override or unbind keys per scope in
the git UI schema (a dedicated file shown; `[plugins.config."bcode.git".ui.keybindings."git.files"]`
in `bcode.toml`):

```toml
[keybindings."git.files"]
"x" = "bcode.git.files.toggle-staged"
"space" = ""                    # unbind ("none" and "unbind" work too)

[keybindings."git.global"]
"g s" = "bcode.git.global.focus-status"   # key sequences are space-separated strokes
```

bcode's shared `[tui.keybindings.scopes."git.*"]` tables in `bcode.toml` work as well; the git
UI schema's `keybindings` are applied over them.

A key is resolved in the focused view's scope first, then `git.list` (list navigation), then
`git.global`. The main view uses `git.main` (or `git.staging` while staging) before `git.global`.
Popups use `git.menu`, `git.input` and `git.confirm`. `?` always shows the effective bindings.

## Opting out

`bcode git` is part of the `distribution` build through the `git-ui` feature. Build without it to
leave the client and its dependencies out of bcode entirely:

```sh
cargo xtask release --features static-bundled-plugins,bundled-ocr-tesseract,web-renderer
```

At runtime, `[plugins] disabled = ["bcode.git"]` or
`[plugins.config."bcode.git".ui] enabled = false` makes `bcode git` refuse to start.

## Extension seam

`bcode_git_ui::host::GitUiHost` lets an embedding host name the UI and contribute command-palette
entries for the current context (staged changes, a commit or a file). Entries run ordinary UI
effects, so a host can add agent-backed actions without the UI depending on it. Both entry points
currently use `StandaloneHost`, which contributes nothing.

## Default keybindings

### `git.global`

| Action ID | Description | Default keys |
|---|---|---|
| `bcode.git.global.quit` | Quit | `q`, `ctrl+c` |
| `bcode.git.global.back` | Back / close | `escape` |
| `bcode.git.global.next-panel` | Focus next panel | `tab`, `l`, `right` |
| `bcode.git.global.previous-panel` | Focus previous panel | `shift+tab`, `h`, `left` |
| `bcode.git.global.focus-window-1` | Focus side window 1 | `1` |
| `bcode.git.global.focus-window-2` | Focus side window 2 | `2` |
| `bcode.git.global.focus-window-3` | Focus side window 3 | `3` |
| `bcode.git.global.focus-window-4` | Focus side window 4 | `4` |
| `bcode.git.global.focus-window-5` | Focus side window 5 | `5` |
| `bcode.git.global.focus-window-6` | Focus side window 6 | `6` |
| `bcode.git.global.focus-window-7` | Focus side window 7 | `7` |
| `bcode.git.global.focus-window-8` | Focus side window 8 | `8` |
| `bcode.git.global.focus-window-9` | Focus side window 9 | `9` |
| `bcode.git.global.focus-status` | Focus the status view | — |
| `bcode.git.global.focus-files` | Focus the files view | — |
| `bcode.git.global.focus-worktrees` | Focus the worktrees view | — |
| `bcode.git.global.focus-submodules` | Focus the submodules view | — |
| `bcode.git.global.focus-branches` | Focus the local branches view | — |
| `bcode.git.global.focus-remotes` | Focus the remotes view | — |
| `bcode.git.global.focus-tags` | Focus the tags view | — |
| `bcode.git.global.focus-commits` | Focus the commits view | — |
| `bcode.git.global.focus-reflog` | Focus the reflog view | — |
| `bcode.git.global.focus-stash` | Focus the stash view | — |
| `bcode.git.global.focus-main` | Focus main view | `0` |
| `bcode.git.global.previous-tab` | Previous tab | `[` |
| `bcode.git.global.help` | Keybindings for this view | `?` |
| `bcode.git.global.palette` | Command palette | `ctrl+p` |
| `bcode.git.global.refresh` | Refresh | `shift+r` |
| `bcode.git.global.push` | Push | `shift+p` |
| `bcode.git.global.pull` | Pull | `p` |
| `bcode.git.global.fetch` | Fetch all remotes | `f` |
| `bcode.git.global.operation-menu` | Merge / rebase options | `m` |
| `bcode.git.global.undo` | Undo last action | `z` |
| `bcode.git.global.redo` | Redo | `ctrl+z` |
| `bcode.git.global.screen-mode-next` | Enlarge focused view | `+` |
| `bcode.git.global.screen-mode-previous` | Shrink focused view | `_` |
| `bcode.git.global.scroll-main-up` | Scroll main view up | `ctrl+u`, `shift+k` |
| `bcode.git.global.scroll-main-down` | Scroll main view down | `ctrl+d`, `shift+j` |
| `bcode.git.global.diff-context-more` | More diff context | `}` |
| `bcode.git.global.diff-context-less` | Less diff context | `{` |
| `bcode.git.global.diffing-menu` | Diff against selection | `shift+w` |
| `bcode.git.global.shell-command` | Run shell command | `:` |
| `bcode.git.global.command-log` | Toggle command log | `@` |

### `git.list`

| Action ID | Description | Default keys |
|---|---|---|
| `bcode.git.list.up` | Previous item | `k`, `up` |
| `bcode.git.list.down` | Next item | `j`, `down` |
| `bcode.git.list.page-up` | Previous page | `,`, `pageup` |
| `bcode.git.list.page-down` | Next page | `.`, `pagedown` |
| `bcode.git.list.top` | First item | `<`, `home`, `g g` |
| `bcode.git.list.bottom` | Last item | `>`, `end`, `shift+g` |
| `bcode.git.list.filter` | Filter list | `/` |
| `bcode.git.list.copy` | Copy to clipboard | `ctrl+o` |

### `git.status`

| Action ID | Description | Default keys |
|---|---|---|
| `bcode.git.status.open-config` | Edit bcode config | `e` |

### `git.files`

| Action ID | Description | Default keys |
|---|---|---|
| `bcode.git.files.toggle-staged` | Stage / unstage file | `space` |
| `bcode.git.files.stage-all` | Stage / unstage all | `a` |
| `bcode.git.files.enter` | Stage lines / open directory | `enter` |
| `bcode.git.files.commit` | Commit | `c` |
| `bcode.git.files.commit-no-verify` | Commit without hooks | `w` |
| `bcode.git.files.amend` | Amend last commit | `shift+a` |
| `bcode.git.files.commit-editor` | Commit using $EDITOR | `shift+c` |
| `bcode.git.files.discard` | Discard changes | `d` |
| `bcode.git.files.reset-menu` | Reset / clean options | `shift+d` |
| `bcode.git.files.edit` | Edit file | `e` |
| `bcode.git.files.open` | Open file | `o` |
| `bcode.git.files.ignore` | Ignore / exclude file | `i` |
| `bcode.git.files.stash` | Stash all changes | `s` |
| `bcode.git.files.stash-menu` | Stash options | `shift+s` |
| `bcode.git.files.toggle-tree` | Toggle file tree | `` ` `` |
| `bcode.git.files.collapse-all` | Collapse all directories | `-` |
| `bcode.git.files.expand-all` | Expand all directories | `=` |
| `bcode.git.files.conflict-ours` | Resolve conflict with ours | `ctrl+o` |
| `bcode.git.files.conflict-theirs` | Resolve conflict with theirs | `ctrl+t` |

### `git.worktrees`

| Action ID | Description | Default keys |
|---|---|---|
| `bcode.git.worktrees.switch` | Switch to worktree | `space`, `enter` |
| `bcode.git.worktrees.new` | New worktree | `n` |
| `bcode.git.worktrees.remove` | Remove worktree | `d` |

### `git.submodules`

| Action ID | Description | Default keys |
|---|---|---|
| `bcode.git.submodules.enter` | Open submodule | `enter` |
| `bcode.git.submodules.update` | Update submodules | `u` |

### `git.branches`

| Action ID | Description | Default keys |
|---|---|---|
| `bcode.git.branches.checkout` | Checkout | `space` |
| `bcode.git.branches.new` | New branch | `n` |
| `bcode.git.branches.delete` | Delete branch | `d` |
| `bcode.git.branches.rename` | Rename branch | `shift+r` |
| `bcode.git.branches.merge` | Merge into current branch | `shift+m` |
| `bcode.git.branches.rebase` | Rebase current branch onto | `r` |
| `bcode.git.branches.checkout-by-name` | Checkout by name | `c` |
| `bcode.git.branches.fast-forward` | Fast-forward to upstream | `f` |
| `bcode.git.branches.upstream` | Upstream options | `u` |
| `bcode.git.branches.tag` | Tag branch tip | `shift+t` |
| `bcode.git.branches.view-commits` | View commits | `enter` |
| `bcode.git.branches.copy-name` | Copy branch name | `y` |

### `git.remotes`

| Action ID | Description | Default keys |
|---|---|---|
| `bcode.git.remotes.view` | View remote branches | `enter` |
| `bcode.git.remotes.new` | Add remote | `n` |
| `bcode.git.remotes.delete` | Remove remote | `d` |
| `bcode.git.remotes.edit` | Edit remote | `e` |
| `bcode.git.remotes.fetch` | Fetch remote | `f` |

### `git.remote-branches`

| Action ID | Description | Default keys |
|---|---|---|
| `bcode.git.remote-branches.checkout` | Checkout as local branch | `space` |
| `bcode.git.remote-branches.delete` | Delete remote branch | `d` |
| `bcode.git.remote-branches.merge` | Merge into current branch | `shift+m` |
| `bcode.git.remote-branches.rebase` | Rebase current branch onto | `r` |
| `bcode.git.remote-branches.set-upstream` | Set as upstream of current branch | `u` |
| `bcode.git.remote-branches.view-commits` | View commits | `enter` |

### `git.tags`

| Action ID | Description | Default keys |
|---|---|---|
| `bcode.git.tags.checkout` | Checkout tag | `space` |
| `bcode.git.tags.new` | New tag | `n` |
| `bcode.git.tags.delete` | Delete tag | `d` |
| `bcode.git.tags.push` | Push tag | `shift+p` |
| `bcode.git.tags.view-commits` | View commits | `enter` |

### `git.commits`

| Action ID | Description | Default keys |
|---|---|---|
| `bcode.git.commits.view-files` | View changed files | `enter` |
| `bcode.git.commits.checkout` | Checkout commit | `space` |
| `bcode.git.commits.squash` | Squash into parent | `s` |
| `bcode.git.commits.fixup` | Fixup into parent | `f` |
| `bcode.git.commits.create-fixup` | Create fixup commit for this | `shift+f` |
| `bcode.git.commits.autosquash` | Apply fixups above this commit | `shift+s` |
| `bcode.git.commits.reword` | Reword | `r` |
| `bcode.git.commits.drop` | Drop commit | `d` |
| `bcode.git.commits.edit` | Edit (stop rebase here) | `e` |
| `bcode.git.commits.move-down` | Move commit down | `ctrl+j` |
| `bcode.git.commits.move-up` | Move commit up | `ctrl+k` |
| `bcode.git.commits.amend-into` | Amend staged changes into commit | `shift+a` |
| `bcode.git.commits.revert` | Revert commit | `t` |
| `bcode.git.commits.cherry-pick-copy` | Copy for cherry-pick | `shift+c` |
| `bcode.git.commits.cherry-pick-paste` | Paste (cherry-pick) copied commits | `shift+v` |
| `bcode.git.commits.reset-menu` | Reset to this commit | `g` |
| `bcode.git.commits.tag` | Tag commit | `shift+t` |
| `bcode.git.commits.new-branch` | New branch from commit | `n` |
| `bcode.git.commits.bisect` | Bisect options | `b` |
| `bcode.git.commits.copy-sha` | Copy commit SHA | `y` |
| `bcode.git.commits.toggle-graph` | Toggle graph | `ctrl+g` |
| `bcode.git.commits.load-more` | Load more commits | `shift+l` |

### `git.commit-files`

| Action ID | Description | Default keys |
|---|---|---|
| `bcode.git.commit-files.view` | View diff / select lines | `enter` |
| `bcode.git.commit-files.checkout` | Checkout file from this commit | `c` |
| `bcode.git.commit-files.toggle-patch` | Toggle file in custom patch | `space` |
| `bcode.git.commit-files.patch-menu` | Custom patch options | `ctrl+p` |

### `git.reflog`

| Action ID | Description | Default keys |
|---|---|---|
| `bcode.git.reflog.checkout` | Checkout entry | `space` |
| `bcode.git.reflog.reset-menu` | Reset to entry | `g` |
| `bcode.git.reflog.view-files` | View changed files | `enter` |

### `git.stash`

| Action ID | Description | Default keys |
|---|---|---|
| `bcode.git.stash.apply` | Apply stash | `space` |
| `bcode.git.stash.pop` | Pop stash | `g` |
| `bcode.git.stash.drop` | Drop stash | `d` |
| `bcode.git.stash.rename` | Rename stash | `r` |
| `bcode.git.stash.branch` | New branch from stash | `n` |
| `bcode.git.stash.view-files` | View stashed files | `enter` |

### `git.main`

| Action ID | Description | Default keys |
|---|---|---|
| `bcode.git.main.line-up` | Scroll up | `k`, `up` |
| `bcode.git.main.line-down` | Scroll down | `j`, `down` |
| `bcode.git.main.previous-hunk` | Previous hunk | `shift+n`, `[` |

### `git.staging`

| Action ID | Description | Default keys |
|---|---|---|
| `bcode.git.staging.line-up` | Previous line | `k`, `up` |
| `bcode.git.staging.line-down` | Next line | `j`, `down` |
| `bcode.git.staging.previous-hunk` | Previous hunk | `shift+n`, `[` |
| `bcode.git.staging.toggle` | Stage / unstage selection | `space` |
| `bcode.git.staging.range` | Toggle range selection | `v` |
| `bcode.git.staging.hunk-mode` | Toggle hunk selection | `a` |
| `bcode.git.staging.switch-side` | Switch staged / unstaged | `tab` |
| `bcode.git.staging.discard` | Discard selection | `d` |
| `bcode.git.staging.edit` | Edit file at line | `e` |
| `bcode.git.staging.commit` | Commit | `c` |

### `git.menu`

| Action ID | Description | Default keys |
|---|---|---|
| `bcode.git.menu.up` | Previous option | `k`, `up`, `ctrl+p` |
| `bcode.git.menu.down` | Next option | `j`, `down`, `ctrl+n` |
| `bcode.git.menu.confirm` | Choose option | `enter` |
| `bcode.git.menu.close` | Close | `escape`, `q` |

### `git.input`

| Action ID | Description | Default keys |
|---|---|---|
| `bcode.git.input.submit` | Submit | `enter` |
| `bcode.git.input.cancel` | Cancel | `escape` |
| `bcode.git.input.toggle-amend` | Toggle amend (commit editor) | `ctrl+a` |
| `bcode.git.input.switch-field` | Switch summary / description (commit editor) | `tab`, `shift+tab` |
| `bcode.git.input.clear` | Clear the message (commit editor) | `ctrl+l` |
| `bcode.git.input.submit-any-field` | Submit from any field (commit editor) | `alt+enter`, `ctrl+s` |

### `git.confirm`

| Action ID | Description | Default keys |
|---|---|---|
| `bcode.git.confirm.yes` | Confirm | `enter`, `y` |
| `bcode.git.confirm.no` | Cancel | `escape`, `n`, `q` |
