# Code review app (`bcode review`)

`bcode review` is an interactive terminal code review: browse a change as one scrolling diff,
comment on lines and ranges, ask Bcode about the code, keep drafts and threads across restarts,
and publish to a provider such as GitHub. The same app ships as the standalone `bcode-review`
binary, which depends on the review, keymap, theme, configuration and BMUX crates but not on
bcode's chat TUI, plugin host, server, session store or agent runtime.

```sh
bcode review                    # review home of the current repository
bcode review staged             # open a transient review of the index
bcode review branch main
bcode review pr 42              # needs a review-source provider plugin
bcode review resume             # the most recently updated saved review
bcode review publish-github --pr 42 --submit
bcode-review                    # standalone binary, same subcommands
bcode-review -C ../repo
bcode-review --print-config-paths
```

Subcommands: `unstaged`, `staged`, `all`, `last-commit`, `range`, `repo`, `branch`, `compare`,
`commit`, `pr`, `stack`, `resume`, and `publish-github`. Every subcommand except
`publish-github` opens the app; `publish-github` previews (or with `--submit` submits) a review
to a GitHub pull request without opening it.

## Architecture

| Crate | Responsibility |
|---|---|
| `bcode_code_review_models` | Provider-neutral contracts: workspaces, drafts, threads, bundles, and the `bcode.code_review/v1`, `bcode.review_publisher/v1`, `bcode.review_importer/v1` and `bcode.review_source/v1` interfaces. |
| `bcode_code_review` | The data layer as plain Rust calls: repository reads (`ReviewData`), stored review state (`ReviewStore`), provider routing, viewed marks, scopes, the renderer-neutral review document, and publish planning. |
| `bcode_code_review_ui` | The BMUX program: home, new-review wizard, the review surface, keymap, theme, configuration, the shared command line, and the `ReviewHost` boundary. |
| `bcode_code_review_ui_cli` | The `bcode-review` binary. |
| `bcode_code_review_plugin` | Serves `bcode.code_review/v1` as JSON adapters over `bcode_code_review` for agents and other plugins, and (when statically bundled) contributes the app's keymap actions to bcode's keymap and registers the `bcode review` subcommand. |

The app calls the data layer directly; there is no JSON encoding or service dispatch between
the UI and its data. The daemon's plugin uses the same functions and the same state database,
so the app and the daemon agree on every record.

`bcode review` runs the app in the same process through the plugin's static CLI registration,
exactly as `bcode git` runs the git client. Builds without the
`static-bundled-code-review-plugin` feature have no `bcode review` subcommand and register no
review behavior.

### Host boundary

The UI reaches everything outside the review through `ReviewHost`: the theme and its changes,
sessions (create, send a message, follow a session's live view, the command that opens a session
in Bcode), plugin services (publishers, importers and review-source providers), the clipboard,
external URLs, background tasks, and redraw requests. `StandaloneHost` implements it for both
entry points with a daemon client (see below) and the platform clipboard and opener; it is the
only part of the app that talks to the daemon for sessions and plugin services, so the data layer
has no daemon client dependency. `run` connects any host's redraws to its loop
(`ReviewHost::connect_redraw`). Another host, for example one that embeds the review in the chat
TUI, implements the same trait and drives `ReviewApp` (a `view::View`) from its own loop,
handling `Action::OpenSession` itself, without changing the UI.

### Sessions and the daemon

The app starts without a daemon. It reaches one only to ask Bcode about the code, to follow
linked sessions, for plugin services, and for `publish-github`.

Every session the review creates records its origin (see
[Embedded sessions](embedded-sessions.md)): app id `bcode.code-review`, the label `Code review`,
and the context entries `purpose` (`question` for an inline question, `assistant` for the dock)
and `workspace` (the review workspace id). `bcode tui` shows the label as a chip, and
`bcode session list --origin bcode.code-review` lists them. The origin is a label only; the
review's own `review_assistant_sessions` table still records which revisions a session was
bound to.

* Inside `bcode review` the client starts the daemon when needed, and opening a linked session
  (`o`) runs `bcode tui <session>` with the same executable.
* The standalone `bcode-review` is not the Bcode executable, so its own artifact names no daemon.
  On first use it reaches the user's installed Bcode with
  `BcodeClient::for_installed_bcode`: the executable is `BCODE_BIN` when set (a value that is
  not an executable file is reported as such, never replaced by another Bcode), else the first
  `bcode` on `PATH`; its daemon for the current state and configuration scope is verified
  (`DaemonTarget::verify`) and, when none is running, started with `bcode server start` the way
  Bcode starts its own. Linked sessions open with that same executable. A missing executable or a
  failed start is reported where the request was made (a question's toast, `publish-github`'s
  error); when the daemon goes away later, the next request locates and verifies Bcode again.

Opening a session hands the terminal to `bcode tui` and resumes the review where it was when
that exits.

### AI cards

Asking Bcode (a new question, a follow-up, or an AI command) never leaves the review. The cursor
moves to the question's card below its anchor, and the card follows the linked session live:

* The title shows a spinner while the turn runs, the status (`asking…`, `working…`,
  `needs you`, `stopping…`, `answered`, `stopped`, `failed`), the session's tokens and estimated
  cost, and the seconds the turn has run.
* The body is the question's turn: `▍you` and the question as asked, reasoning collapsed to
  `▸ thinking · N words` (a click opens it), tool lines `◌` running, `✓` done, `✗` failed with
  the last three lines of output and an `All N lines` expander, the answer as markdown that
  streams in (its last rows while streaming; `z`/More shows the whole answer), notices, and
  errors.
* A pending permission request of the session replaces the card's actions with its prompt:
  `y` allow, `a` always, `n` decline, `d` never (scope `code-review-feed`; Always and Never
  only when the request can be remembered). While the turn runs, `esc` or `ctrl+c` stops it.
  These keys apply only on such a card; elsewhere the same keys keep their review meaning.
* `o` opens the session in Bcode (`Open in Bcode`).
* When a card needs the reviewer while it is off screen (a permission request, an answer, a
  failure), a toast names its location; the scrollbar marks every card.

A card keeps a bounded summary of its session (`agent::Feed`): eight turns, 24 items per turn,
200 output lines per tool and bounded text, so a long session costs the same as a short one.
Each session has its own layout revision, so a streaming update or a spinner frame lays out only
that session's cards. Follow-ups in one session each show their own turn, found by the question;
a turn that has left the observed window says so and `o` reads it in Bcode.

### Review assistant

`ctrl+k` opens the assistant dock and focuses it; when the dock is open elsewhere it focuses it,
and when it is focused it closes it. The dock is a chat with a Bcode session working on this
review. It sits to the right of the document when the body is at least 120 columns wide (190
beside a right threads panel) and under the document otherwise; drag its divider to resize it.

* The header row carries chips: the review and the cursor's place (its line, selected range, or
  file). While the place chip is attached (`ctrl+x` detaches or attaches it), a message carries a
  short context block naming it after the text; the conversation shows the text as typed.
* The first message creates a session in the repository, binds it to the review, and sends the
  message. Reopening the review continues its newest bound session; `ctrl+n` starts a new one.
* The conversation shows every observed turn like an AI card: `▍you` and the message, reasoning,
  tool lines with their output, and the answers, with a spinner while the session works. Its
  pane title shows `needs you`, `working…`, tokens and cost. `pageup`/`pagedown` and the wheel
  scroll it; clicks open reasoning and tool output.
* A permission request of the session replaces the input with its prompt: `y` allow, `a` always,
  `n` decline, `d` never (Always and Never when the request can be remembered). `ctrl+c` stops
  the running turn, `ctrl+o` opens the session in Bcode, and `esc` returns to the review with the
  dock open. A request while the dock is hidden raises a toast.
* When a `code_review.*` tool that changes the review finishes in the dock's session (or an AI
  card's), the review reloads its comments and viewed files as it does for the reviewer's own
  edits: a drafted comment appears at its line, a resolved thread resolves, a viewed file shows
  as viewed. `code_review.navigate` moves the cursor to the place it names. Tool calls that had
  already finished when a session is first observed are not replayed.

#### Review tools

The code review plugin offers these tools to every Bcode session. A session the app started (the
assistant's, or an AI card's) is bound to its review, so the tools act on the review on screen
with the revisions it shows; any other session passes `review` with a saved review's id.

| Tool | Access | What it does |
|---|---|---|
| `code_review.list_reviews` | read | Saved reviews of the repository (id, title, sources). |
| `code_review.summary` | read | Title, sources and the revisions they resolve to, stack layers, files and lines, viewed files, threads, suggestions. |
| `code_review.list_files` | read | Files with status, added and removed lines, and whether each is viewed. |
| `code_review.file_diff` | read | One file's diff as numbered lines with hunk headers (`context`, `max_lines`). |
| `code_review.file_content` | read | A file at the head (`side = "new"`) or base (`"old"`) revision, numbered (`start_line`, `count`). |
| `code_review.list_threads` | read | Threads (location, kind, severity, resolved, comments) and pending suggestions. |
| `code_review.pull_request` | read | The pull request behind the review (by its pull request sources, else its branch): title, state, branches, review decision, CI checks, via the GitHub CLI. |
| `code_review.navigate` | read | Moves the reviewer's view to a file or line. |
| `code_review.add_comment` | write | Drafts a comment on lines (`path`, `line`, `end_line`, `side`), a file, or the review, with an optional kind and severity. |
| `code_review.reply` | write | Replies in a thread. |
| `code_review.resolve_thread` | write | Resolves or reopens a thread. |
| `code_review.mark_viewed` | write | Marks files viewed or not viewed at their current content. |
| `code_review.suggest_change` | write | A suggested comment the reviewer accepts or rejects, optionally replacing the lines (a `suggestion` block). |
| `code_review.publish` | publish | Previews, or with `submit` publishes, the comments with a built-in publisher (`local_markdown`, `markdown_file`, `json_file`). |

Reads are bounded (2,000 files, 2,000 diff or content lines per call, 200 threads, 4 KiB per
comment, 128 KiB of output) and say when they were cut. Writes and publishing return what they
did. Publishing to a forge such as GitHub stays in the app (`P`), which reaches publisher plugins.

#### Tool permissions

The tools follow Bcode's permission rules like any other tool. Reads and `navigate` are
read-only and allowed; writes and `publish` are mutating and ask, and the prompt appears in the
dock (or the AI card) as above. The plan agent has the writes and `publish` turned off. Set each
tool to `allow`, `ask`, or `deny` per agent with exact-name rules, or turn it off:

```toml
[agent.build.permission.tools]
"code_review.add_comment" = "allow"   # draft comments without asking
"code_review.mark_viewed" = "allow"
"code_review.publish" = "deny"        # never publish from an agent
"code_review.file_content" = "ask"    # ask before reading file contents

[agent.build.tools]
"code_review.pull_request" = false    # no GitHub calls
```

When a prompt offers Always or Never, the answer is remembered as a rule for the session's
agent. See [`permissions.md`](permissions.md) for how rules layer.

### Comment composer

Typing `@` and part of a path in the comment composer suggests repository paths (tracked and
untracked, ignoring ignored files) from the data layer's file search, listed inside the composer
under the editor. `up`/`down` (or `ctrl+p`/`ctrl+n`) move, `tab` or `enter` accepts, `esc` closes
the suggestions, and a click accepts one. Accepting replaces the `@word` under the cursor with
`@<path>` as one undoable edit; while suggestions are open, `enter` never submits the comment.

### Viewed files

Viewed marks record a file's blob identities and its diff fingerprint, so a file stays viewed
across rebases and renames as `viewed_reset` allows. When a review opens, the fingerprints of
its marked files are loaded with the file list, so viewed files show as viewed on the first
frame. An unsaved review keeps its viewed files per target, like its comments: reopening the same
target shows them again, and saving the review takes them along.

### Review list

Home's "Updated" column, its order, and `resume` follow when a review last changed: its title,
sources, viewed files, reviewed revisions or archive state, or its comments. Opening or scrolling
a review records where it was left without making it updated.

### Stacks of pull requests

A review that steps through a stack (one source per pull request, "Step through every layer")
shows one pull request at a time by default. The diff and the file panel hold only the current
pull request, under a banner with its position in the stack, number, title, branches, files,
and viewed count. `.` and `,` (or a click on the rail's tabs) move to the next and previous pull
request, each returning to where it was left; viewing a pull request's last file says so and
points to the next one.

`shift+o` (also in the command palette) switches to every pull request in one document: each
starts with its banner, the file panel groups files under one row per pull request with only
the current one open, and a strip under the rail names the pull request the cursor is in. The
choice is kept with the review. A whole-stack review (one source for the entire stack) is one
diff labeled by layer and is not affected.

### Finding pull requests

The new-review wizard's pull request and stack steps list the provider's 100 most recently
updated open pull requests, and typing filters that list. When what is typed (a number, `#number`,
pull request URL, or exact head branch name) is not in it, the last row offers to look it up with
the provider; `enter` asks, and what it finds joins the list. `bcode review stack` without a
number looks up the current branch's pull request the same way, and so does the home screen's
stack suggestion when the recent list does not show the branch's pull request.

Resolving a stack asks the review discovery contributors (git history, Graphite) which branches
belong to it, so the provider looks them up in a few batched requests, and adds their warnings
(for example a layer that needs a restack) to the stack step. See
[`review-discovery.md`](review-discovery.md).

### Rendering and terminal output

Every frame repaints the whole screen from the app's state; BMUX diffs it against the previous
frame and writes only the changed cells. Large terminals stay cheap because:

* **Rows are built while idle.** After each frame the review builds the rows of the viewport and
  of the pages above and below it, and has the list lay them out
  (`VirtualListComponent::prepare_around`), so paging and scrolling reveal rows that are already
  built. Built rows are cached by item, width, presentation revisions, and the highlights their
  file's blobs have, so a highlight landing for one file leaves other files' rows cached.
* **Scrolls move rows instead of rewriting them.** The list reports how far its rows scrolled
  (`PaintCx::hint_scroll`) and the terminal is presented with scroll-region acceleration, so a
  scroll writes the rows it reveals plus what changed beside them. Full rows are scrolled when
  that saves cells (with the file panel shown, its rows are rewritten), and only the list's
  columns, with left/right margins, on terminals known to support them (iTerm2, Ghostty, xterm,
  outside multiplexers).
* **Writing never blocks input.** On Unix, frames go to the terminal from an output thread over a
  separate nonblocking open of the terminal device. While a frame is being written the app keeps
  handling input; the next frame shows the latest state, so frames superseded meanwhile are never
  written. Elsewhere, or when stdout is not a terminal, frames are written to stdout directly.
* **Highlighting is bounded to the screen.** Blobs are highlighted only for files on screen, at
  most two at a time, in steps of 2,000 lines: the first step's lines are shown at once, and a job
  whose file leaves the screen stops at its next step. Highlights for files off screen land without
  a frame; completions that arrive together are drawn by one frame.

### Visual system

Every color comes from the bcode theme through a few semantic tiers (`theme::Tiers`): the
`canvas` the panes sit on, `surface_alt` for the header and footer bars, file headers and the
comment composer card, `overlay` for the palette, pickers, modals, toasts and pills; text,
strong, muted and dim; and an accent, a second accent and text on the accent. Themes that define
few roles still get every tier (tinted surfaces on an RGB canvas, no fills with terminal
colors). Warning color only marks what needs the reviewer, such as the drafts of an unsaved
review; a modified file takes the accent and a renamed one the second accent.

Panels are rounded unless the theme chooses its own glyphs, and the focused panel's border and
title take the accent, with counts at the right of each title row. The selected row of the file
tree, the threads list, home's table and every menu carries a `▌` bar (`>` with ASCII icons), so
selection does not depend on color. Publish is a filled button and the header's sources and
layout are pills.

`NO_COLOR`, a dumb terminal, or `[tui] color_depth` reduce the colors written to the terminal.
Without colors, the cursor line and selected ranges are reversed and search matches underlined.

## Configuration

Both entry points read the ordinary bcode configuration files, discovered and layered like
bcode's own (`--print-config-paths` lists them in order):

* `[plugins.config."bcode.code_review"]`: where review state is stored, and presentation
  settings under its `review_v2` table (`diff_layout`, `side_by_side_breakpoint`, `tab_width`,
  `icons`).
* `[plugins.config."bcode.code_review"] viewed_reset`: when a viewed file stops counting as
  viewed. `"diff"` (the default) keeps it viewed while its changes are the same: the same `+`/`-`
  lines in the same sections (functions), even when the rest of the file changes, for example
  after a rebase. `"file"` resets it when either side of the file changes at all. Any other
  value is reported and the default is used.
* `[plugins.config."bcode.code_review".stack]`: how many layers a stack of pull requests may
  have. `max_layers` (default 64) is the limit; `over_limit` says what happens beyond it.
  `"warn"` (the default) loads every layer and shows a warning that can be dismissed: a row of
  the new-review wizard's stack step (`enter` dismisses it) and, when the review opens, a toast
  that stays until its `×` is clicked or the "Dismiss notifications" command runs. `"stop"` asks
  the provider to cut the stack at the limit, with a warning in the stack step. Invalid values
  are reported and the defaults are used.
* `[tui.keybindings.scopes."code-review*"]`: keybinding overrides of the review scopes
  `code-review`, `code-review-home`, `code-review-wizard`, `code-review-files`,
  `code-review-threads`, `code-review-composer`, `code-review-feed` and `code-review-assistant`.
  `none` or `unbind` removes a default.
* `[tui.theme]`: the theme, loaded through `bcode_tui_theme` discovery like `bcode-git`.
* `[tui] color_depth`: `auto` (the default: no colors under `NO_COLOR` or a dumb terminal, else
  full color), `truecolor`, `ansi256`, `ansi16`, or `none`, as in bcode.
* `[tui.diff_viewer]`: bcode's diff layout preference, followed unless the review's own table
  sets a layout.
* `[plugins] disabled`: listing `bcode.code_review` disables the app.

The review keymap actions are declared once, in `packages/code-review-ui/keymap.toml`. The app
reads them at build time, and the statically bundled plugin contributes the same declarations to
bcode's keymap (its TUI registry), so bcode knows the review scopes and validates overrides of
them.

Review state lives in `code-review.db`, shared by the app and the daemon's plugin. It is in
`code-review/` under the bcode state directory by default, in the repository's
`.bcode/code-review` with `state_location = "repo"`, in `state_dir` when that is set, and in
`BCODE_CODE_REVIEW_STATE_DIR` when that environment variable is set.

## Testing

* `cargo test -p bcode_code_review -p bcode_code_review_ui` runs the data layer, document and
  screen tests. Screens are driven by a scripted host with a private state directory.
* `screen_benchmark`, `screen_benchmark_sizes`, `document_benchmark` and
  `data_layer_benchmark` are ignored benchmarks; run them with
  `cargo test --release -p <crate> <name> -- --ignored`.
* The `review-*` cases of `scripts/smoke-continuous-tui-pty.py` drive a real `bcode review`
  in a pseudo-terminal against an isolated fixture repository and state.
