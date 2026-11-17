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

## Commands

`bcode-review` takes `-C DIR` (run as if started in `DIR`) and `--print-config-paths`. Every
subcommand takes `--repo PATH` (default `.`). Without a subcommand the app opens the review home.

| Subcommand | Opens |
|---|---|
| `unstaged` | A transient review of unstaged working-tree changes. |
| `staged` | A transient review of the index. |
| `all` | Staged and unstaged changes together. |
| `last-commit` | The last commit. |
| `range BASE HEAD [--two-dot]` | A revision range (merge-base semantics unless `--two-dot`). |
| `repo [--path FOLDER]... [--show-ignored]` | Browse the repository's files as they are now (tracked, and untracked files that are not ignored) and comment anywhere; with `--path`, only those folders; with `--show-ignored`, ignored files too. |
| `files SELECTOR... [--show-ignored]` | Files, folders, globs or line ranges as they are now, not a change: `path`, `folder/`, `'src/**/*.rs'`, `path:START-END`. Folders and globs are listed again whenever the review opens. Selectors are relative to the current directory when it is inside the repository, else to its root; one that names nothing fails before the app opens ("examples/helo-plugin: no such file or folder"). See [Reviewing files and folders](review-files-and-folders.md). |
| `branch [BASE]` | The current branch against its base (the upstream, else the default branch). |
| `compare BASE HEAD [--two-dot]` | Two branches or revisions. |
| `commit REV...` | One or more commits (one range when they form a first-parent chain). |
| `pr NUMBER\|URL` | A pull request (needs a review-source provider plugin). |
| `stack [NUMBER\|URL]` | A whole stack of pull requests; without a reference, the stack of the current branch's pull request. |
| `resume [ID]` | A saved review; without an id, the most recently updated one. |
| `publish` | Does not open the app: previews (or with `--submit` submits) the drafts of `--target` (default `unstaged`) as a review of pull request `--pull` (default: the current branch's) on `--provider` (default: the one built-in provider that submits reviews), in `--repository` (default: detected), with `--verdict comment\|request-changes\|approve`, `--summary` and `--unplaced-into-summary`. Runs the provider in-process; no daemon. `publish-github` is an alias that still reads its old flags (`--pr`, `--github-repo`, `--submit-event`, `--fallback-unmapped-to-summary`; `--token-env` is ignored, since providers find their own credentials). |

Transient reviews (every subcommand but `resume`) keep their drafts per target; `ctrl+s` saves one
to the review home.

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
moves to the question's card below its anchor, and the card follows the linked session live. It
is framed like a comment thread, with a Viewed checkbox (`v`), and it folds with `z` or its
chevron (see [`review-thread-cards.md`](review-thread-cards.md)).

What a question sends:

* The review, its repository and sources, the anchor (file and line ranges, or *the whole
  review*), up to 80 of its diff lines, and the other threads on the same file (or on the
  review): the review's own and its pull requests', each with author, pull request, outdated and
  resolved state, and thread id; at most 12, the rest counted.
* Asked from a thread (**Ask Bcode** on it, `a` on it, an AI command or a follow-up with the
  cursor on it), the thread itself: where it comes from (a local draft thread or a pull request's
  thread on its forge), resolved and outdated state, its id for `code_review.get_thread`, where
  it was written (path, original lines, commit) and where the review shows it now and why, the
  diff hunk it was written on (kept even when outdated; its last 40 lines), and its comments in
  order with their authors, including replies held for the next review. Comments keep the first
  and as many of the latest as fit (2 KiB each, 8 KiB in all, at most 12), and say how many were
  left out.
* The review tools the session can call to read more (`code_review.list_threads`,
  `code_review.get_thread`, `code_review.file_diff`, ...), then the question. Every part has its
  own bound and the whole prompt at most 24 KiB: the other threads, the tools line, and the
  question are always kept; the diff lines take the room left (the first lines, the rest
  counted); only if that is not enough is the context before them cut. Cuts are marked, and a
  cut never leaves a code block open (a comment cut inside its code block closes it).

A question's new session is titled after its file (for a pull request thread, the file it was
written on) and, asked from a thread, its first comment's author (`Review: src/retry.rs ·
kevin`); a question about the whole review is titled after the review. A later question on the
same anchor is a follow-up in that session and carries the thread again.

The card:

* The title shows a spinner while the turn runs, the status (`asking…`, `working…`,
  `needs you`, `stopping…`, `answered`, `stopped`, `failed`), the session's tokens and estimated
  cost, and the seconds the turn has run.
* The body is the question's turn: `▍you` and the question as asked, in full (wrapped, with its
  blank lines), reasoning collapsed to
  `▸ thinking · N words` (a click opens it), tool lines `◌` running, `✓` done, `✗` failed with
  the last three lines of output and an `All N lines` expander, every answer of the turn as
  markdown shown in full (a streaming answer shows all its text so far; an answer over 64 KiB
  ends with a line saying the rest is in Bcode), notices, and errors.
* A pending permission request of the session replaces the card's actions with its prompt:
  `y` allow, `a` always, `n` decline, `d` never (scope `code-review-feed`; Always and Never
  only when the request can be remembered). While the turn runs, `esc` or `ctrl+c` stops it.
  These keys apply only on such a card; elsewhere the same keys keep their review meaning.
* `o` opens the session in Bcode (`Open in Bcode`).
* A session written by an older Bcode is brought forward when the card follows it (as opening it
  in Bcode does), so a card never fails for that alone. When a card cannot follow its session,
  it says why and what to do: another Bcode (an older build's daemon) has the session open, the
  session no longer exists, or the connection to Bcode was lost. **Retry** follows it again.
* When a card needs the reviewer while it is off screen (a permission request, an answer, a
  failure), a toast names its location; the scrollbar marks every card.

A card keeps a bounded summary of its session (`agent::Feed`): eight turns, 24 items per turn,
200 output lines per tool and bounded text, so a long session costs the same as a short one.
Each session has its own layout revision, so a streaming update or a spinner frame lays out only
that session's cards. Follow-ups in one session each show their own turn, found by the question;
a turn that has left the observed window says so and `o` reads it in Bcode.

### Review assistant

The review assistant is one docked conversation with a Bcode session for the whole app: it is
available on the reviews home, in the new-review wizard and in every review, and stays open with
the same conversation while you move between them. The full design, and where the build differs
from it, is in [code review assistant](code-review-assistant.md).

* **Opening it.** The `✦ Ask AI` button at the right end of every screen's header, `ctrl+k`, the
  palette entry "Ask AI" (`:` or `ctrl+p`, on home too) and the footer hint all open and focus
  it; `ctrl+k` focuses it when it is open and closes it when it is focused. A focused text field
  of the screen (the home filter, the comment composer, search) keeps `ctrl+k`. The button
  shortens to `✦ AI` and then `✦` as the header narrows, is a filled pill while the panel is
  open, and shows a spinner or `?` while the panel is closed and the conversation works or needs
  you.
* **Placement.** From 100 columns the panel docks on the right (by default 40% of the width,
  36 to 100 columns, and the screen keeps 60); narrower, it docks at the bottom (40% of the
  height, at least 8 rows, and the screen keeps 8); when neither fits, the focused panel takes
  the whole screen and `esc` returns to it. Drag the panel's inner edge to resize it; the size
  is kept as a share of the screen, so the panel keeps its proportion when the terminal is
  resized (the limits above still win). Below 24
  columns or 6 rows it shows its title row and input only, below 12 columns its title row only.
* **What it knows.** Each message carries a stored `review app view` block (the screen, the
  review's id, title, kind and sources, the stack layers and the current one, the comparison
  with each layer's result, the file, cursor and selection, threads and loaded pull request
  state) and, for that turn only, a `review app detail` block (the selected lines, threads on the
  file, the focused thread, per-layer changes, files changed since the baseline, pull request
  state). Both are bounded, and every cut names the tool with the rest. A review without pull
  request layers whose sources are compared with their last completed pass lists each source's
  result instead (`source_results`, `source_deltas`). The app's own tool
  `bcode.code-review.current_view` returns the live screen in full as JSON (`sections` narrows
  it, `max_files` lists fewer files per layer or source); an answer that would exceed 64 KiB
  shortens lists and then drops sections, always as valid JSON with a `truncated` object naming
  what was left out.
* **Conversation.** The title row names the active conversation and where it started (`✦
  Assistant · <title> · <review #layer or home>`) and carries `+ new`, `⟲` (history) and `↗`
  (open in Bcode). The chips row names what is on screen (`⌖ src/retry.rs:42`, `⌖ home`, `⌖ new
  review`); `ctrl+x` detaches it. The first message creates a Bcode session with the review app's
  origin, titled after the message and tagged with the review and stack layer on screen (or with
  none on home and in the wizard), on the app's tool connection, so its first turn already has
  the app's tools. Every message names the review assistant agent profile; while the app's tools
  and profile are not registered with Bcode (Bcode starting or restarting), messages wait and
  the panel says so, rather than running without the profile's tool limits. The conversation
  shows the text as typed.
* **Which review the tools act on.** The session's binding names the review its `code_review.*`
  tools act on when no `review` is passed. It is rewritten before each message and whenever the
  screen changes: the review on screen, or detached on home and in the wizard, where a review
  tool called without `review` fails with "no review is on screen; pass `review`". Any tool can
  act on another saved review by passing its id.
* **What it can do in the app.** Besides the review tools, the assistant has the app's own tools
  (see [App tools](#app-tools)): it finds your pull requests and stacks, moves you within the
  review on screen, changes the comparison and the view, and opens another review, home or the
  wizard. So "create a review for my most recent PR stack", "create a review for my uncommitted
  changes in `src/`" or "mark all these files viewed" work from the conversation: the review is
  created, home lists it at once, and the assistant opens it (after asking).
* **Only the open conversation acts on the screen.** The app's tools answer only the
  conversation open in the panel; a conversation switched away from (a turn still running, or
  one continued in `bcode tui`) gets `not_active_conversation`, and switching or starting a
  conversation stops offering the app's tools to the previous one.
* **Leaving a review.** When the assistant takes you to home, the wizard or another review, the
  review closes as exiting does (its state saved first); an unsaved review with comments, AI
  answers or suggestions is not left, and the assistant is told to ask you to save it.
* **History.** `alt+h` (or `⟲`) opens the picker of every conversation of the repository, newest
  first: title, where it started, how long ago, turns, and `needs you` or `working`; Ask AI card
  sessions are labelled `card`. Typing searches; `enter` switches (and attaches the conversation
  to the screen); `alt+r` renames; `alt+d`, or the `✕` on the hovered (else selected) row, marks
  a row and a second press deletes it from Bcode (the footer names the marked row); `ctrl+o`
  opens it in `bcode tui`. Earlier per-review conversations are listed from their bindings,
  tagged with their review. In the conversation, `alt+r` renames it, `alt+d` asks before
  deleting it, and `ctrl+n` (or `+ new`) starts a new one. The palette entry "Delete every
  conversation the review app started" asks with the count, stops running turns, deletes and
  unbinds each session, and reports the ones Bcode kept.
* **Title row.** `↗` (and `ctrl+o`) opens the conversation in `bcode tui` once it has a session
  (before that, `ctrl+o` says to start or open one); `✕` closes the panel; a double click on the
  title zooms the focused panel over the whole screen, and another restores it. The screens'
  palettes list the assistant's actions ("Assistant: …").
* **Turns** show like an AI card: `▍you` and the message in full, reasoning, tool lines with their output
  and the answers in full (as on an AI card), with a spinner while the session works; the title row shows `needs you`,
  `working…`, tokens (`12.3k tok`) and cost, cutting the title to keep them. `pageup`/`pagedown`
  and the wheel scroll it; clicks open reasoning and tool output. A finished answer offers Copy
  and, in a review, Draft comment (a draft at the cursor's line, range or file). A message Bcode
  refused, or a turn that ended without an answer, says why in the conversation: the profile not
  registered yet, a model or provider failure (with "check the model Bcode uses and its
  sign-in"), the tool-round limit ("ask it to continue"), or Bcode's own message.
* **Model, effort and permissions.** The chips row also names the model (`● <model> ▾`; `Bcode's
  model (<name>)` until one is chosen), its reasoning effort (`◆ effort <value>`, only for a
  model with effort settings), the permission categories (`✎ review`, `▤ files`, `$ shell`,
  `⇪ publish`, `→ move`) and `⚒ tools`. `alt+m` (or a click on the model chip) opens a filterable
  picker of the models Bcode offers, Bcode's default first, with each model's context window,
  prices and effort range (a provider that cannot list says why). `shift+tab` cycles the effort,
  wrapping. `alt+p` (or a click on a category chip) turns a category off or on for this
  conversation: the next turn is not offered its tools (a category turned off in configuration
  stays off). `alt+t` lists the tools by category and how Bcode decides them. The model and
  effort are the session's own Bcode selection (they survive a restart and show in `bcode tui`)
  and the latest choice is also the app default every new conversation and Ask AI card session
  gets before its first message. The chips wrap to a second row, then shorten to their glyphs,
  then drop from the right; the model chip goes last.
* **Ask AI cards** name their session's model (`● <model> · <effort>`); a click on it, or
  `alt+m` and `shift+tab` on the card, changes that session's model and effort (and the app
  default). A question being written shows the model it will be asked with; `alt+m` and
  `shift+tab` in the question composer change the app default its new session gets.
* **Permissions.** A permission request replaces the input with its prompt: `y` allow, `a`
  always, `n` decline, `d` never. Always and Never are offered only when the request can be
  remembered, and never for publishing or deleting reviews. `ctrl+c` stops the running turn.
* **Live reflection.** When a `code_review.*` tool that changes the review finishes in the
  assistant's session (or an AI card's), the review on screen reloads its comments and viewed
  files as it does for the reviewer's own edits, and `code_review.navigate` moves the cursor to
  the place it names. In a session the app started every finished call is applied; in one
  switched to from the history, calls that had already finished are not replayed.
* **Changes made elsewhere.** The open review never writes its whole copy over the store: each
  save writes only what you changed since it last read or wrote the review, and keeps renames,
  archiving and viewed marks made meanwhile by the assistant, an AI card, `bcode tui`, home or
  another app (concurrent writes are checked against the row's revision and merged again when
  another write came first). The review then shows them: a rename changes its title. A review
  deleted elsewhere is never saved again; the app returns home with a toast. Changes are picked
  up on the next save, after any `create_review`, `update_review` or `delete_review` call the
  app sees, and when the terminal regains focus.

#### Review tools

The code review plugin offers these tools to every Bcode session. A session the app started (the
assistant's, or an AI card's) is bound to its review, so the tools act on the review on screen
with the revisions it shows; any session can pass `review` with a saved review's id instead. A
review named by id has its pull request and stack sources resolved without the network: from the
stack it last opened with, else the pull requests and stacks the provider last reported (the
review cache). When their commits are not in the repository yet, the tool says to open the
review in the app once.

| Tool | Access | What it does |
|---|---|---|
| `code_review.list_reviews` | read | Saved reviews of the repository, most recently updated first: id, title, kind (`stack_step_through`, `whole_stack`, `pull_request`, `branch`, `range`, `working_tree`, ...), source labels, pull request numbers, viewed files, archived, created and updated times. `include_archived`, `query` (title, kind, label or pull request number). |
| `code_review.summary` | read | Title, sources and the revisions they resolve to, stack layers, files and lines, viewed files, threads, suggestions. |
| `code_review.list_files` | read | Files with status, added and removed lines, and whether each is viewed; `truncated` first names sources cut at `files.max_files`. |
| `code_review.file_diff` | read | One file's diff as numbered lines with hunk headers (`context`, `max_lines`). |
| `code_review.file_content` | read | A file at the head (`side = "new"`) or base (`"old"`) revision, numbered (`start_line`, `count`). |
| `code_review.list_threads` | read | The review's local threads (location, kind, severity, resolved, comments), pending suggestions, and a page of its pull requests' threads (`origin = "pull_request"`: pull request, authors, the first and latest comments, resolved, outdated, where each was written and where the review shows it). `include_resolved`, `path` (the file a thread was written on or is shown on, matched whole), `offset`, `limit` (50, at most 100); the page has `total`, `next_offset`, and how many resolved threads it left out. |
| `code_review.get_thread` | read | One thread in full by `thread_id`: a local thread, or a pull request thread with its comments in order, links, and the diff hunk it was written on. |
| `code_review.pull_request` | read, web | The pull request behind the review (by its pull request sources, else its branch): title, state, branches, review decision, CI checks, via the GitHub CLI with the user's credentials. |
| `code_review.navigate` | read | Moves the reviewer's view to a file or line. |
| `code_review.add_comment` | write | Drafts a comment on lines (`path`, `line`, `end_line`, `side`), a file, or the review, with an optional kind and severity. |
| `code_review.reply` | write | Replies in one of the review's local threads. |
| `code_review.resolve_thread` | write | Resolves or reopens one of the review's local threads. |
| `code_review.mark_viewed` | write | Marks many files viewed or not viewed at their current content in one call, selected by `paths`, a `glob`, `layers` (pull request numbers or source ids), `all`, or `unchanged_since_look` (files whose change is the same as when you last looked at their layer, or completed a pass); selectors narrow each other. Answers with the files changed, those already in that state and named paths not in the review. |
| `code_review.add_comments` | write | Several draft comments in one call, each like `add_comment`, with a result per comment. |
| `code_review.resolve_threads` | write | Resolves or reopens several local threads in one call. |
| `code_review.create_review` | write | Creates a saved review from `sources`: `pull_request` (`number` or `url`), `stack_layers` (stepped through, bottom first), `stack` (one diff), `branch`, `current_branch`, `range`, `commits`, `commit`, `last_commit`, `working_tree`, `staged` or `unstaged` limited to `paths`, `files` (`paths` in the selector syntax: files, folders, globs, `path:START-END`; optional `exclude`), and `repository` (whole, or `paths` folders). With `directory`, the review is created in that directory's repository, and uncommitted changes, files and repository sources without `paths` are limited to it. Every kind the wizard creates can be created here. |
| `code_review.find_paths` | read | Repository paths by what the user calls them: folder names, package names (`Cargo.toml`, `package.json`, `pyproject.toml`), file names and path endings, best first, with file counts, `ambiguous` and `weak`. Ignored files only with `include_ignored`. |
| `code_review.update_review` | write | Renames, archives or restores one review (`review`) or several (`reviews`). |
| `code_review.delete_review` | write | Deletes reviews for good (their comments stay in the store, unreachable). |
| `code_review.suggest_change` | write | A suggested comment the reviewer accepts or rejects, optionally replacing the lines (a `suggestion` block). |
| `code_review.publish` | publish | Previews, or with `submit` publishes, the comments with a built-in publisher (`local_markdown`, `markdown_file`, `json_file`). `output_path` is a path relative to the session's working directory that stays inside it; without it the file goes to `.bcode/reviews/` in the repository. |

Reads are bounded (2,000 files, 2,000 diff or content lines per call, 200 local threads, 4 KiB
per comment; pull request threads 100 per page, 1 KiB per listed comment and 8 KiB per comment
of one thread, 16 KiB of a diff hunk keeping its end; 128 KiB of output) and say when they were
cut. A thread listing, or one thread, fits 96 KiB of JSON so it is never cut midway: a listing
shortens its page of pull request threads (down to one; `next_offset` continues it), then leaves
out local threads and then pending suggestions from the end (`omitted`, `suggestions_omitted`);
one thread leaves out its oldest replies (keeping the first comment and the latest ones,
`comments_omitted`), then shortens its diff hunk and then its longest comments. Writes and publishing return what
they did. Publishing to a forge such as GitHub goes through the app (`P`, or the assistant's
`bcode.code-review.publish`, which opens the same dialog), which reaches publisher plugins.

Pull request threads come from the review store, not the forge: whenever the review app places
its pull requests' threads and they changed, or it read them again a minute or more after the
saved read, it saves them for the review's scope (bounded: 1,000 threads, 100 comments per thread
keeping the first and the latest, 16 KiB per comment). Saves run one at a time in order, so an
older placement never replaces a newer one, and a failed save is tried again on the next
placement. `read_at_ms` is when the app read them: the oldest read of the review's pull requests,
from the forge or, before that, from its cache.
So `list_threads` and `get_thread` stay read-only and work offline, show what the reviewer sees
(as of `read_at_ms`), and say so when the app has not read them yet. A review without pull
request sources (one whose pull request was removed, say) shows none, whatever was saved for it. Their thread ids are
`<owner>/<name>#<number>:<forge id>`. `reply` and `resolve_thread` change local threads only:
on a pull request thread they still ask first, then fail with an error that says to reply or
resolve it in the review app or on the forge (or draft a comment with `add_comment`).

#### App tools

The app registers these tools for its own sessions (and the review assistant's profile offers
them). They act on the screen, so they work while the app is open; each answers with what it did
and what is on screen now (`current_view`'s sections `screen`, `review`, `stack`, `comparison`,
`file`, `home`).

| Tool | Class | What it does |
|---|---|---|
| `bcode.code-review.current_view` | read-only | The live screen (see What it knows). |
| `bcode.code-review.find_pull_requests` | read-only | Open pull requests chained into stacks, newest first, each layer with its branches and update time and each stack with a `create_review_source` for `code_review.create_review`. `filter`: `mine` (a layer's branch is a local branch: your own work), `review_requested`, `current_branch`, `all`; `query`; `limit` (10, at most 50). |
| `bcode.code-review.go_to` | read-only | Moves within the review on screen to a `layer` (pull request number or source id), a `path` and `line` (`side`), a `thread`, or a `version` of the layer (its whole change as it was then, read-only where it differs from now), as the keys do. Another review's place fails, naming `open_review`. |
| `bcode.code-review.set_comparison` | read-only | Any Compare menu choice: `baseline` (`off`, `last_looked`, `previous_push`, `ago` with `ago_minutes`, `between` with `from_ms`/`to_ms`, `pushes` with `from_push`/`to_push` as the push timeline picks them, `stack_at` with `at_ms`), `view` (`full`, `changes`), `only_changed`, `show_hidden_files`, `all_layers`, `versions` (`number`, `from`, `to`: vN to vM of one pull request), `mark_style`, `word_marks`, `seen_hunks`, `removed_inline`. Every argument is checked first: one bad argument changes nothing. Pushes and versions need the stack history; while it loads the call asks to be repeated. |
| `bcode.code-review.set_view` | read-only | `layout` (`unified`, `side_by_side`), `wrap`, `hide_resolved_threads`, `hide_viewed_files`, `file_list` (`tree`, `flat`), `files_panel`, `threads_panel`, `fold_files`, `fold_threads`. |
| `bcode.code-review.refresh` | read-only | Reloads home, or checks the review on screen for changes (`r`). |
| `bcode.code-review.open_review` | mutating | Opens a saved review of the repository (`review`), optionally at a `layer`, `path`, `line`, `thread` or `version` once it loaded; or `screen = "home"`, or `"new_review"` (the wizard, filled in with `title` and `sources` as `create_review` takes them, at its last step for you to confirm). The review on screen is left through its close path first. |
| `bcode.code-review.complete_pass` | mutating | Marks the review pass complete (`shift+m`). |
| `bcode.code-review.pull_request_review` | mutating | Opens the dialog to `approve`, `request_changes`, `comment` or `approve_viewed_layers`; you confirm it. |
| `bcode.code-review.publish` | mutating | Opens the publish dialog (`shift+p`); you confirm it. |

Read-only app tools run without asking: they change what is shown, not what is stored. Mutating
ones follow the `build` agent's rules for mutating tools, so moving you to another screen asks
first unless a rule allows it (`"bcode.code-review.open_review" = "allow"`); a denied call never
reaches the app. Without the app (a conversation continued in `bcode tui`), these tools are not
available and the review tools still work.

#### Tool permissions

The tools follow Bcode's permission rules like any other tool. Reads and `navigate` are
read-only and allowed; writes and `publish` are mutating and ask, and the prompt appears in the
dock (or the AI card) as above. The plan agent has the writes and `publish` turned off. Some
calls also declare the resources they touch, so the path and web rules apply to them as well:

* `file_diff` and `file_content` declare the file they read (resolved in the repository), so
  read rules such as `[permission.read] "**/.env" = "deny"` deny them too.
* `pull_request` declares the repository's web address (its `origin` on GitHub), so web rules
  decide it; without a web rule that allows the address it asks. An exact-name `allow` alone
  does not skip the prompt; allow the address with a web rule instead.
* `publish` with an `output_path` declares the file it writes, so write rules apply and the
  prompt names the file.

Set each tool to `allow`, `ask`, or `deny` per agent with exact-name rules, or turn it off:

```toml
[agent.build.permission.tools]
"code_review.add_comment" = "allow"   # draft comments without asking
"code_review.mark_viewed" = "allow"
"code_review.publish" = "deny"        # never publish from an agent
"code_review.file_content" = "ask"    # ask before reading file contents

[agent.build.permission.web]
"https://github.com/**" = "allow"     # pull_request without asking

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

### File panel layout

`` ` `` lists the file panel's files flat or as a tree. Flat, each file reads by its whole path,
its directories dimmed and the first to give way when the panel is narrow
(`M …review/files.rs`), in document order by default, so moving down the list follows the
review. Source groups (several sources, parts or stack layers) stay, each with its files under it;
merged, one list holds every source's files and each row names its source at its end (the first
thing to go when the row is narrow), since layers of a stack can change the same path. The filter,
hidden viewed files, comparison marks and dropped files work the same in both layouts. Switching
keeps the selected file selected (a selected directory hands its selection to its first file),
and directories closed in the tree are closed again when it comes back. The choice is saved with
the review; until a review has one, `file_layout` decides (see Configuration).

### Files, folders and the repository as they are now

A review of code as it is now, not of a change (see
[Reviewing files and folders](review-files-and-folders.md)):

* **Starting one.** The new-review wizard's **Files** type opens the tree picker; its
  **Repository** type reviews the whole repository or, with **Limit to folders…**, folders chosen
  in the same tree. **Working tree** → **Limit to paths…** chooses files and folders in the tree,
  then which uncommitted changes in them. Home's **Review files or folders…** suggestion
  (`shift+n`) opens the tree directly; in a review, `alt+r` (also in the palette) reviews the file
  panel's selected folder or file, or the cursor file's folder, as it is now; and `bcode review
  files` / `bcode review repo` do it from the command line.
* **Tree picker.** Folders expand and collapse (`right`/`l`, `left`/`h`; `left` on a file goes to
  its folder), `space` (or `tab`) selects a file or a whole folder, and `shift+tab` selects every
  row from the last one selected. A selected folder stays a folder: files added to it later are
  reviewed when the review opens again. Unselecting a file or folder inside a selected folder
  excludes it, and folders show `[x]` (all), `[-]` (some) or `[ ]` (none). Typing any other
  letter (or `/` first, for names starting with `h`, `j`, `k` or `l`) filters by name across the
  repository; a filter with `*`, `?` or `[` offers **Select pattern … · N files now**, and one
  ending in `:START-END` offers **Add lines START–END of PATH**; when the filter finds more than
  the 500 rows listed, the frame title says how many. `enter` continues with the
  selection (with nothing selected, it selects the focused row first); `esc` leaves the filter,
  clears it, then goes back a step. The summary row counts the selection's files now (changed
  files, for uncommitted changes in paths), and the last step shows each picked source's count; `alt+l`
  (or a click on it) shows the selected items with their counts, where `space`, `x`, `delete` or
  `backspace` removes one. Untracked files show `?`, files changed since `HEAD` `M`, and ignored
  files (`alt+i`, or a click on **Show ignored files**; it opens as `files.show_ignored` sets,
  and the created review keeps the picker's choice) `!`; `ctrl+r` lists the repository again.
  With the mouse, a click on a folder expands or collapses it, a click on a file selects or
  unselects it, a right click selects or unselects any row (folders included), and folders
  found by the filter are selected by a click too.
* **Editing.** On the wizard's last step, `ctrl+e` on a files, folder, repository or
  uncommitted-paths source opens it in the tree; the edited source keeps its id, so its threads and
  viewed marks stay attached.
* **Uncommitted changes.** Whole files of the working tree mark the lines changed since `HEAD`
  (staged or not) with the comparison marks, removed lines behind a "lines removed since HEAD"
  marker, and their headers say `uncommitted +N −M` (`uncommitted renamed` for a staged rename,
  `uncommitted binary` for a changed binary file); untracked and ignored files say `untracked`
  and `ignored`. `w` shows or hides the marks in this review (`files.uncommitted_marks` sets the
  default); in a review without whole working-tree files it says so and changes nothing.
* **Ignored files** are left out of folder, glob and repository sources until `alt+i` lists them
  in this review (`files.show_ignored` sets the default; in a review without such sources it
  says so and changes nothing); a file named directly is always included. Both choices are
  saved with the review, never in configuration.
* **Limits.** A source lists at most `files.max_files` files (default 5,000), keeping the first in
  path order; the review warns "Some files are not listed" with the full count. Files deleted
  from the working tree and nested repositories that are not submodules are not listed.

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

**When the stack changes.** A step-through review saves one source per layer. After a restack,
layers whose pull requests were closed or merged are not errors: once the stack is read again
(from a layer still open), the review's layers are matched to it. Layers that left are removed
and new layers are added in stack order (existing ones keep their position, comments and
viewed state), the review is saved and opened again, and one notice says what changed ("The
stack changed: #1 closed or merged (removed); #3 added"). If no layer is left to read the stack
from, one notice names the closed layers. Failures name sources by their labels.

### Changes since you looked

A review compares each pull request (a stack layer stepped through, or a single pull request),
in place, with an earlier version (see [review-stack-history.md](review-stack-history.md)).
Other sources showing a diff between two commits (a branch, a range, a whole stack) are compared
with the review's last completed pass the same way. Every comparison happens on the same review
screen: nothing opens another review, and viewed marks, comments, AI asks, search and navigation
work the same with or without one.

* **The default is the version you last looked at.** Marking a file viewed, commenting, asking
  the AI, completing the pass (`shift+m`), your own reviews on the forge, and (when nothing
  else was recorded) staying on a pull request for a few seconds record that you looked at its
  current version; earlier completed passes count too.
* **The comparison row** under the rail reads `Changes since: [you last looked · 2d ago ▾]  1
  of 3 PRs changed · 2 rebased only`, then the stack's pushes as ticks (`┃` a push that changed
  some pull request's own change, `│` one that only rebased; `[`/`]` around the pushes compared)
  and when the last push was. While layers are compared it says how far it got ("comparing 6 of
  21…"). The view options that are on follow the control as chips (`[only what changed]`,
  `[only PRs with real changes]`); a click on one opens the menu. It narrows to `Since: last
  looked ▾` and `Δ▾` (brief chips, then none); pushes share ticks when cells are few.
* **A review that moved since you looked** reopens comparing with what you last looked at (even
  when another choice was saved), with a banner: what changed, and **Only what changed**
  (`shift+l`), **Compare with…** (`b`) and **Dismiss**.
* **`b`** (or a click on the control) opens the comparison menu, a live control over the review
  behind it. Under **Show changes since** it lists the baselines as radio buttons: *you last
  looked*, *the previous push*, *1 hour ago*, *yesterday*, *a week ago*, *pushes picked on the
  timeline…* (the timeline takes the keys: `←`/`→`, `f` from, `t` to, `enter`, `esc`; a click on
  a tick compares from before it, with `shift` up to after it), *two versions of #N…* (the
  version picker), *the whole stack as it was* (listed while that history choice is in effect
  or the menu opened on it, with its time) and *nothing: show the full change*. The baseline in
  effect is marked `◉` with what it resolves to (`◉ you last looked · 4m ago`, `◉ pushes picked
  on the timeline… · 3h ago → 1h ago`, `◉ yesterday · 1d ago`), and the menu opens with the
  cursor on it. Under **View** it lists the check boxes *Only what changed* and *Only pull
  requests with real changes*; both can be set while nothing is compared, and apply once
  something is.
  * `space` (or a click) picks the row in place: the review behind the menu compares with that
    baseline, or flips that option, at once, and the menu stays open; the footer says how far a
    comparison got while one computes. Comparisons computed before are laid again at once.
  * `enter` keeps what is set and closes the menu (on a baseline not yet in effect it picks it
    first); a click outside the menu does the same.
  * `esc` puts back exactly what the menu opened on: the baseline, the options, the banner,
    and which pull requests show the files the comparison hides.
  * Rows that open another picker (pushes, versions) close the menu and open it; what the menu
    previewed stays pending until that picker compares (kept) or is cancelled (put back too).
  Nothing is saved while the menu is open; the choice is kept with the review when it closes,
  and *you last looked* is worked out again on every open. A time picked as *1 hour ago*,
  *yesterday* or *a week ago* is kept as that: a reopened review compares with that long before
  it reopens, marked as that choice.
* **The version picker** lists the current pull request's versions newest first (`v1`..`vN`,
  when, what kind of change, and which you looked at or reviewed). `f` sets *from*, `t` sets
  *to* (now by default), `enter` compares; a click sets *from*, a `shift`-click *to*. A
  comparison that ends at an older version shows only what changed, read-only: its lines take no
  comment and nothing about it is recorded as looked at.
* **What shows.** The rebase is taken out: a layer only rebased collapses to one line, files
  unchanged since the baseline fold once viewed (unviewed ones stay open), lines new since then
  are tinted with a bar in the gutter, and code removed since then is a marker you can expand.
  The summary rows, markers, hunk labels and file badges name the baseline (`Changed since you
  looked`, `Changed since the previous push`, `since yesterday`, `since push 3`, `since v2`;
  `since then` for any other time).
* **Marking what is new.** Under **Mark new lines** the menu picks how new lines show: *bar and
  tint* (the default), *gutter bar only*, *tint only*, or *strong* (an accent bar, a deeper
  tint, and `new` in the gutter where it has room). **Mark changed words** (on) underlines and
  tints, within a line that replaced one in the baseline, only the words that differ from it.
  **Show removed code inline** shows code removed since in place, struck through and faint,
  instead of behind its marker. A hunk with new lines is labelled with the baseline and a
  count (`new since push 3 · 3 lines`). Under **Hunks with nothing new**, *show* (the default),
  *dim*, or *fold* them to their header (`[nothing new since you looked · show 8 lines]`; a
  click shows one, `[… · fold]` folds it again; a hunk with a comment or draft in it stays
  open and says `kept open`; the choice lasts across a refresh). These
  preview with `space` and are put back by `esc` like the rest of the menu, and last for the
  session; `compare.*` settings set the defaults.
  `]`/`[` stop on new lines first. `shift+l` switches between that full change and only what
  changed (each changed file from its old version replayed on today's base to the current code).
* **Badges.** Rail tabs say how each pull request changed: `● +40 −12 · 3 files`, `○ rebased`,
  `= unchanged` (never `✓`, which on the same tab means approved), `+ new to you`, `≈` (the
  rebase could not be taken out), `?` (the baseline's commits are gone), `⋯` (still comparing).
  The comparison menu and the history matrix (`shift+b`) end with a key to these glyphs next to
  `✓ approved` and `✕ changes requested`. *Only pull requests with real changes* folds the
  others into one `+N ○` tab (a click shows them again), and `.`/`,` skip them.
* **The file panel** follows the comparison, one pull request at a time and with every layer
  shown (`shift+o`) alike.
  * A legend over the tree names the comparison and the options on (`Changes since you last
    looked · 4m ago · only what changed`; a click opens the menu), then what it found: for one
    pull request `1 of 3 files changed since you last looked (4m ago): +1 −0`, or plainly `No
    changes since you last looked (4m ago): only rebased`; with every layer `1 of 3 PRs changed ·
    2 rebased only`. Without a comparison it says *Full change: no comparison* (with the options
    set for when something is compared); while looks are still read it says so.
  * The legend wraps within the panel while that leaves the tree at least three rows; in a
    shorter panel its rows keep one line each and the later ones are left out.
  * Files changed since the baseline are marked `●` (new to the change since then: `+`) before
    their status, in bold, with their since counts (`Δ+3−1`, `new +12`); files the same as then
    recede (dimmed); files the baseline's change had and the current one dropped are listed,
    struck through and inert, as `− path  dropped since` at the end of their group.
  * Group rows (one per layer with every layer shown) carry the same badge as the rail (`●
    +40 −12 · 3 files`, `○ rebased only`, `= no changes`, `⋯ comparing`, `? baseline gone`)
    instead of their whole change's counts; layers with nothing to look at again recede.
  * The panel lists what the document shows: files the comparison hides there (unchanged since
    then in *only what changed*, every file of a layer only rebased) are left out, counted on
    their group (`· 2 hidden`), and the legend says `2 files hidden: unchanged since then
    [show]`. Showing them (that row, the summary row's button in the document, or *Compare: show
    or hide the files the comparison hides* in the palette) shows them in both, dimmed, with
    `[hide]` to hide them again. A compared group stays listed even when all its files are
    hidden.
  * When *Only pull requests with real changes* leaves pull requests out, the legend says so
    (`2 pull requests hidden: no real changes [show all]`, or *skipped* one at a time) and the
    row turns the option off.
* **A file's versions.** Once the current pull request's versions are compared with each other,
  a file header says `changed in 2 of 4 pushes · v4 ◂ ▸`; `alt+,` and `alt+.` (or the arrows;
  `alt+[`/`alt+]` where the terminal reports them) step that file alone to the previous or next
  push that changed it (read-only), and past the newest back to the pull request's comparison.
  The file panel's row follows the version shown.
* **The history matrix** (`shift+b`) compares in place too: `enter` the cursor's pull request
  with its version before, `p` the pushes between the pin (`f`) and the cursor, `s` everything
  since the cursor's column, `a` the whole stack at this push (the menu marks it *the whole
  stack as it was*), `r` since you last looked, `i` only real changes.

### Pull request conversations

A pull request (or stack layer, or whole stack) shows its conversation as its forge has it,
without importing anything. See [`review-forge-conversation.md`](review-forge-conversation.md).

* **The pull request bar**, under the rail, is about the pull request the cursor is in (a whole
  stack shows its top layer). One line: the review decision and who approved, checks (failing
  or running first), open threads, *waiting on you*, and what is new since you last looked, with
  buttons for what the provider and your permissions allow: **Approve**, **Request changes**,
  **Comment**, **Open ↗**, and **↻** to read it again. Clicking the summary (or `ctrl+o`) goes to
  the pull request's card.
* **The card** comes before the pull request's files, as the top of its page, set apart from the
  first file like files are from each other, so a stack reads as one page per pull request. It
  holds the title, the status line with the same buttons and a ▾/▸ toggle (`z` on the card also
  folds it), the description, each reviewer's latest verdict (and whether it was given on an
  older version, or before a rebase that keeps it current), who is still asked, failing checks
  with links, and the conversation, with **Show all**. A stack also gets **Approve N reviewed
  layers**, and a whole-stack review gets one status line per layer. Cards start open; the
  `overview` setting can start them folded, or folded unless something is new.
* **Threads** from the forge sit on their lines in the diff, placed on the code the review shows:
  exactly, moved with their code when it moved, *near their code* when the lines they were on
  were rewritten or removed since (on what replaced them, or where they were), else on their file
  (*on an older version*, when the commit they were written on is not here) or on the layer. Each has **Reply**, **Resolve**/**Reopen**, **Open ↗** and **Ask Bcode** as far as
  allowed; resolved ones fold. **Reply** sends at once and offers **Add to review**, which holds
  the reply as a draft under the thread until the next review of that pull request is submitted
  (the default is configurable). **Ask Bcode** sends the whole thread with the question (see
  [AI cards](#ai-cards)), outdated ones included, and the review tools read the same threads
  (see [Review tools](#review-tools)).
* **The rail** marks each layer approved (✓) or changes requested (✕), with its open threads
  and a dot when something is new.
* **The threads panel** lists pull request threads too, and filters them: *on the pull request*,
  *waiting on you* and *unviewed* (the filter key cycles).
* **Threads are cards**, with three separate states (see
  [`review-thread-cards.md`](review-thread-cards.md)): *resolved* (shared: settled), *viewed*
  (yours: `v` on a thread or its ○/✓ Viewed checkbox; a comment from someone else makes it
  unviewed again, with *N new*), and *folded* (by itself when viewed or resolved, or by hand
  with `z` or the chevron).
* **Publishing** offers **Review on {provider}** first: its options are the verdict (only those
  the provider accepts) and a summary, and held replies go with it. Approve and Request changes
  in the bar open it directly at that verdict for that pull request.
* **The history matrix** (`shift+b`) names each layer's reviews and whether they still apply.
* Conversations stay current without polling: they are read when the review opens, when it is
  focused again, after your own changes, when the review's heads change, from **↻**, and when
  the provider's watch says they changed (GitHub's checks cheaply on its own schedule). New
  reviews and replies make a toast.

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

Panel dividers drag (as do `>` and `<` for the file panel), and a panel keeps the share of the
screen it was given: resizing the terminal scales the file and threads panels and the assistant
with it, as BMUX panes do, while the document keeps at least 40 columns and each panel its
minimum. File and threads panel sizes are saved with the review as shares; sizes saved in
columns by earlier versions are converted the first time the review shows them.

Panels are rounded unless the theme chooses its own glyphs, and the focused panel's border and
title take the accent, with counts at the right of each title row. The selected row of the file
tree, the threads list, home's table and every menu carries a `▌` bar (`>` with ASCII icons), so
selection does not depend on color. Publish is a filled button and the header's sources and
layout are pills.

`NO_COLOR`, a dumb terminal, or `[tui] color_depth` reduce the colors written to the terminal.
Without colors, the cursor line and selected ranges are reversed and search matches underlined.

## Keys

These are the default keys, declared in `packages/code-review-ui/keymap.toml`. `?` shows them in
the app, `:` or `ctrl+p` runs any action by name (including the unbound ones), and
`[tui.keybindings.scopes."<scope>"]` rebinds them (see Configuration). A count typed before a
motion repeats it. While a search is active, `n` moves to the next match.

### Home (`code-review-home`)

| Keys | Action |
|---|---|
| `j`, `down` | Next review |
| `k`, `up` | Previous review |
| `pagedown` | Page down |
| `pageup` | Page up |
| `g`, `home` | First review |
| `shift+g`, `end` | Last review |
| `enter` | Open the review (fold a group) |
| `space` | Fold or unfold the group |
| `n` | New review |
| `shift+n` | New review: files or folders as they are now… |
| `c` | Continue the latest review |
| `/` | Filter reviews |
| `r` | Rename the review |
| `x` | Archive the review |
| `shift+r` | Restore the archived review |
| `d` | Show / hide the preview |
| `tab` | Switch between suggestions and reviews |
| `ctrl+r` | Reload reviews and suggestions |
| `?` | Keyboard help |
| `ctrl+t` | Appearance: the review's palette and profile… |
| `q` | Quit |
| `ctrl+k` | Ask AI: open, focus, or close the review assistant |
| `:`, `ctrl+p` | Command palette |

### New-review wizard (`code-review-wizard`)

| Keys | Action |
|---|---|
| `tab` | Mark or unmark the commit, select or unselect the tree row (include or exclude a source when editing) |
| `shift+tab` | Mark every commit or tree row from the last mark |
| `esc` | Back one step |
| `ctrl+k` | Ask AI: open, focus, or close the review assistant |
| `alt+i` | Show / hide ignored files in the tree picker |
| `alt+l` | Show the tree picker's selected items (and back to the tree) |
| `ctrl+r` | List the repository again in the tree picker |
| `ctrl+e` | Edit the files, folder, repository, or uncommitted-paths source in the tree picker |

In the tree picker, arrows, `h`/`j`/`k`/`l`, `space`, `/` and typed text are the picker's own
keys (see [Files, folders and the repository as they are now](#files-folders-and-the-repository-as-they-are-now)).

### Review (`code-review`)

| Keys | Action |
|---|---|
| `j`, `down` | Move down |
| `k`, `up` | Move up |
| `ctrl+d` | Half page down |
| `ctrl+u` | Half page up |
| `pagedown` | Page down |
| `pageup` | Page up |
| `g`, `home` | Top of review |
| `shift+g`, `end` | End of review |
| `n` | Next file |
| `p` | Previous file |
| `}` | Next hunk |
| `{` | Previous hunk |
| `]` | Next change |
| `[` | Previous change |
| `e` | Reveal context below |
| `shift+e` | Reveal context above |
| `x` | Reveal hidden lines |
| `shift+x` | Reveal whole file |
| `shift+s` | Toggle unified / side-by-side |
| `shift+w` | Toggle line wrapping |
| `r`, `ctrl+r` | Refresh review |
| `q`, `esc` | Exit review |
| `tab` | Focus next pane |
| `t` | Show / hide file panel |
| `f` | Filter files |
| `>` | Widen file panel |
| `<` | Narrow file panel |
| `shift+t` | Show / hide threads panel |
| `ctrl+k` | Ask AI: open, focus, or close the review assistant |
| `c` | Comment on the line, range, or file (reply on a thread) |
| `a` | Ask Bcode about the line, range, or file |
| `shift+f` | Comment on the file |
| `shift+c` | Comment on the whole review |
| `shift+v` | Select a range of lines |
| `y` | Copy the selected lines |
| `i` | Edit the thread's latest comment |
| `shift+d` | Delete the latest comment or AI answer |
| `shift+r` | Resolve or reopen the thread |
| `shift+k` | Change the thread's kind |
| `!` | Change the thread's severity |
| `o` | Open the AI answer's Bcode session |
| `)` | Next thread |
| `(` | Previous thread |
| `v` | Mark the file viewed and go to the next unviewed file |
| `u` | Next unviewed file |
| `shift+u` | Previous unviewed file |
| `/` | Search the review |
| `shift+n` | Previous search match |
| `:`, `ctrl+p` | Command palette |
| `?` | Keyboard help |
| `ctrl+s` | Save this review to the reviews home |
| `shift+p` | Publish the review… |
| `shift+i` | Import an external review… |
| `.` | Next stack layer |
| `,` | Previous stack layer |
| `shift+l` | Only what changed since you looked / the full change with what is new marked |
| `b` | Changes since…: choose what pull requests are compared with |
| palette only | Compare: only pull requests with real changes |
| palette only | Compare: full change (no comparison) |
| palette only | Compare: with what you last looked at |
| palette only | Compare versions of this pull request… |
| palette only | Compare pushes of the stack… |
| `alt+,`, `alt+[` | This file's previous version (in a comparison) |
| `alt+.`, `alt+]` | This file's next version (in a comparison) |
| `shift+m` | Mark the review pass complete |
| `enter` | Actions of the thread or AI answer (reveals a gap) |
| `z` | Fold or open the thread or AI answer |
| `shift+z` | Collapse every thread (expand all when all are collapsed); on a file's header, fold every file (open all when all are folded) |
| `shift+h` | Show / hide resolved threads |
| `h` | Show / hide viewed files in the file panel |
| `` ` `` | List the file panel's files flat or as a tree |
| palette only | Next open thread |
| palette only | Previous open thread |
| palette only | Next AI answer |
| palette only | Previous AI answer |
| palette only | Next unviewed file, else next open thread |
| `shift+a` | Ask Bcode: review commands… |
| palette only | Ask Bcode to explain the change |
| palette only | Ask Bcode to find risks |
| palette only | Ask Bcode to suggest a review comment |
| palette only | Ask Bcode to check the tests |
| palette only | Ask Bcode to summarize the file |
| `shift+y` | Copy the AI answer |
| `s` | Turn the AI answer into a suggested comment |
| `m` | Turn the AI answer into a draft comment |
| `;` | Accept the suggestion as a draft comment |
| palette only | Ask Bcode to refine the suggestion |
| palette only | Reject the suggestion |
| palette only | Reconnect the AI answer's stream |
| palette only | Mark every file viewed |
| palette only | Mark every file unviewed |
| palette only | Go to a line of this file (type its number in the palette) |
| palette only | Copy a Markdown review report |
| palette only | Edit the review's sources… |
| palette only | Ask Bcode about this file |
| palette only | Fold this file to its header (or open it); `z` on a file's header, or its ▾/▸ chevron |
| palette only | Fold every file to its header |
| palette only | Open every file, viewed ones too |
| palette only | Copy this file's path |
| `ctrl+o` | Pull request: open or close its overview (reviews, checks, conversation) |
| palette only | Pull request: approve… |
| palette only | Pull request: request changes… |
| palette only | Pull request: comment in its conversation |
| palette only | Pull request: open its web page |
| palette only | Pull request: read its conversation again |
| palette only | Stack: approve every fully viewed layer… |
| `shift+b` | Stack history: how each pull request changed in each push (compares in place) |
| `w` | Show / hide uncommitted changes (lines changed since HEAD) in whole files |
| `alt+i` | Show / hide ignored files in folder, glob, and repository sources |
| `alt+r` | Review this folder as it is now (the file panel's folder or file, else the cursor file's folder) |

### File panel (focused) (`code-review-files`)

| Keys | Action |
|---|---|
| `enter` | Open the file (fold a directory) and go to the review |
| `space` | Fold or unfold the directory |
| `left` | Collapse the directory, or go to its parent |
| `right` | Expand the directory |
| `esc` | Back to the review |

### Threads panel (focused) (`code-review-threads`)

| Keys | Action |
|---|---|
| `enter` | Go to the thread in the review |
| `right` | Next threads filter |
| `left` | Previous threads filter |
| `esc` | Back to the review |

### Comment composer (`code-review-composer`)

| Keys | Action |
|---|---|
| `ctrl+s` | Save the comment |
| `ctrl+p` | Toggle markdown preview |
| `ctrl+t` | Switch between comment and question |
| `alt+k` | Change the thread kind |
| `alt+s` | Change the thread severity |
| `alt+m` | Choose the model a question is asked with (question mode) |
| `shift+tab` | Cycle the reasoning effort a question is asked with (question mode) |
| `esc` | Cancel the comment |

### AI card with a running turn or a permission request (`code-review-feed`)

| Keys | Action |
|---|---|
| `y` | Allow the AI card's permission request once |
| `a` | Always allow the AI card's permission request |
| `n` | Decline the AI card's permission request once |
| `d` | Never allow the AI card's permission request |
| `esc`, `ctrl+c` | Stop the AI card's running turn |
| `alt+m` | Choose the model of the AI card's session |
| `shift+tab` | Cycle the reasoning effort of the AI card's session |

### Review assistant (focused) (`code-review-assistant`)

| Keys | Action |
|---|---|
| `ctrl+k` | Close the assistant |
| `esc` | Back to the screen (the assistant stays open) |
| `ctrl+c` | Stop the assistant's running turn |
| `ctrl+x` | Attach or detach what is on screen as context |
| `ctrl+n`, `alt+n` | Start a new conversation |
| `alt+h` | Conversation history |
| `alt+r` | Rename this conversation |
| `alt+d` | Delete this conversation (asks first) |
| `ctrl+o` | Open the assistant's session in Bcode |
| `pageup` | Scroll the conversation up |
| `pagedown` | Scroll the conversation down |
| `alt+m` | Choose the assistant's model |
| `shift+tab` | Cycle the assistant's reasoning effort |
| `alt+p` | Turn the assistant's permission categories on or off for this conversation |
| `alt+t` | List the assistant's tools and how Bcode decides them |
| (palette) | Delete every conversation the review app started (asks first) |

### Conversation history (`code-review-assistant-history`)

| Keys | Action |
|---|---|
| `up`, `ctrl+p` | Previous conversation |
| `down`, `ctrl+n` | Next conversation |
| `enter` | Switch to the conversation |
| `alt+r` | Rename the conversation |
| `alt+d` | Delete the conversation (press twice) |
| `ctrl+o` | Open the conversation in Bcode |
| `esc` | Back to the conversation |

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
* `[plugins.config."bcode.code_review".compare]`: comparisons with earlier versions.
  `default_baseline` is what a review compares with until another is chosen: `"last_looked"`
  (the default), `"previous_push"`, a duration such as `"24h"`, or `"none"`. `baseline_look`
  (`"engaged"`, the default, or `"opened"`) says whether staying on a pull request counts as
  looking at it even when something else was recorded; `dwell_seconds` (default 8) is how long
  that takes. `start_view` (`"full"` or `"changes"`) is the view a comparison starts in;
  `rebased_layers` (`"collapse"`, `"hide"`, or `"show"`) how layers that were only rebased show;
  `timeline` (`"auto"` or `"always"`) when the push timeline shows. How what is new since the
  baseline is marked: `mark_style` (`"both"`, the default, `"bar"`, `"tint"`, or `"strong"`),
  `word_marks` (default true: mark the changed words of lines modified since), `seen_hunks`
  (`"show"`, the default, `"dim"`, or `"fold"`: hunks with nothing new), and `removed_inline`
  (default false: code removed since shows inline, struck through). Invalid values are reported
  and the defaults are used.
* `[plugins.config."bcode.code_review".assistant]` `model` and `effort`: the model (`"<model>"`
  or `"<provider>/<model>"`) and reasoning effort new assistant conversations and Ask AI card
  sessions get until one is chosen in the app (the app then remembers its own choice in the
  review store). Unset, Bcode's default applies.
* `[plugins.config."bcode.code_review".assistant.tools]`: which tools the review assistant may
  use, by category, every one `true` by default: `review_reads` (the review read tools, the
  app's `current_view` and `find_pull_requests`), `review_writes` (comments, replies,
  resolution, suggestions, viewed marks, creating, changing and deleting reviews, and the app's
  `complete_pass`), `publish` (`code_review.publish` and the app's `publish` and
  `pull_request_review`), `navigation` (the app's `go_to`, `set_comparison`, `set_view`,
  `refresh` and `open_review`), `file_reads` (`filesystem.read`, `list`,
  `find`, `grep`, `stat`, `exists`), `file_writes` (`filesystem.write`, `edit`, `multi_edit`)
  and `shell` (`shell.run`); `extra` lists exact names of other Bcode tools to expose, such as
  `"web.fetch"`. The app registers the agent profile `bcode.code-review.assistant` with the
  selected tools when it starts; the profile is based on Bcode's `build` agent and can only
  narrow it, so every call is still allowed, asked or denied by the `build` agent's rules
  (`[agent.build.permission.tools]`, for example `"code_review.mark_viewed" = "allow"`). With
  the defaults, reads and moves within the review run (except `code_review.pull_request`, which
  asks unless a web rule allows the repository's address; see [Tool
  permissions](#tool-permissions)); review writes, opening another screen,
  file writes and shell commands ask each time; publishing asks every time (the panel never
  offers Always for it or for deleting reviews). An unknown key or a non-boolean value is reported and the default is used.

  ```toml
  [plugins.config."bcode.code_review".assistant.tools]
  shell = false                  # never offer shell commands to the assistant
  extra = ["web.fetch"]

  [agent.build.permission.tools]
  "code_review.mark_viewed" = "allow"
  ```
* `[plugins.config."bcode.code_review".stack]`: how many layers a stack of pull requests may
  have. `max_layers` (default 64) is the limit; `over_limit` says what happens beyond it.
  `"warn"` (the default) loads every layer and shows a warning that can be dismissed: a row of
  the new-review wizard's stack step (`enter` dismisses it) and, when the review opens, a toast
  that stays until its `×` is clicked or the "Dismiss notifications" command runs. `"stop"` asks
  the provider to cut the stack at the limit, with a warning in the stack step. `remind` says
  how often the review's toast comes back: `"once"` (the default: closing it keeps it closed for
  that review, saved or not) or `"every_open"`. Invalid values are reported and the defaults are
  used.
* `[plugins.config."bcode.code_review".review_v2]` `file_header` (`auto`, the default, by the
  review pane's size; `compact`, `comfortable` or `spacious`); `file_separator` (`card`, the
  default: files as cards on a desk color; `rule`, labeled with the next file's position; `gap`;
  or `none`), with `file_separator_rows` (1 to 3; unset follows the density),
  `file_separator_label` (default true) and `file_separator_inset` (default false);
  `file_page` (`auto`, `on` or `off`: whether a desk color is used; without one cards become a
  rule); `fold_viewed_files` (default true: a viewed file folds to its header); and the file
  panel's `file_layout` (`tree`, the default, or `flat`: for reviews with no choice saved),
  `flat_order` (`document`, the default, or `path`) and `flat_groups` (`keep`, the default: one
  list per source under its group row; or `merge`: one list for every source). Themes can
  set the `source.file.header`, `source.file.desk`, `source.file.edge` and `source.file.label`
  roles. See [`review-separators-and-folding.md`](review-separators-and-folding.md).
* `[plugins.config."bcode.code_review".conversation]`: pull request conversations.
  `enabled` (default true); `replies` (`"send_now"`, the default, or `"with_review"`: where a
  reply to a pull request thread goes, the other way being a button); `overview` (`"expanded"`,
  the default, `"auto"`: folded unless something is new, or `"collapsed"`); `notify` (which new
  activity makes a toast: `"reviews_and_replies"`, the default, `"all"`, `"reviews"` or
  `"none"`); `fold_resolved` (default true); `outdated_on_file` (default true: threads whose code
  could not be followed show on their file). Its `threads` table: `fold_viewed` (default true),
  `resolve_marks_viewed` (default true), `new_comments` (`"reopen"`, the default, or
  `"keep_folded"`), `card` (`"compact"`, the default, or `"roomy"`) and `width` (default 110;
  0 for the whole width). Its `signals` table: `provider` (default true: use
  the provider's watch), `min_gap_ms` (default 2000: reads closer together are merged),
  `pause_unfocused` (default true), and `providers.<id>`, passed to that provider's watch as is
  (GitHub reads `interval_ms`, default 30000, at least 10000). Invalid values are reported and
  the defaults are used.
* `[plugins.config."bcode.code_review".files]`: reviews of files, folders and the repository as
  they are now. `uncommitted_marks` (default true: whole working-tree files mark their lines
  changed since `HEAD`; `w` toggles it per review), `show_ignored` (default false: folder, glob
  and repository sources leave ignored files out; `alt+i` toggles it per review) and `max_files`
  (default 5,000, at most 100,000: files one source lists). An invalid table is reported and the
  defaults are used.

  ```toml
  [plugins.config."bcode.code_review".files]
  uncommitted_marks = false
  show_ignored = true
  ```
* `[tui.keybindings.scopes."code-review*"]`: keybinding overrides of the review scopes
  `code-review`, `code-review-home`, `code-review-wizard`, `code-review-files`,
  `code-review-threads`, `code-review-composer`, `code-review-feed` and `code-review-assistant`.
  `none` or `unbind` removes a default.
* `[tui.theme]`: the theme, selected as bcode's chat interface selects it: `name` (the
  palette or theme, such as `catppuccin-mocha`, `nord` or `bcode-dark`; default
  `terminal-native`), `profile` (applied over the palette before other overlays), `overlays`,
  `variant` (`auto`, `dark` or `light`) and extra theme files or directories in `paths`. With
  `variant = "auto"` the app asks the terminal for its colors and presents the light variant on
  a light background (else `COLORFGBG`, else dark). Everything resolves through
  `bcode_tui_theme::appearance`, the code bcode and `bcode-git` use, and the visual system above
  derives every color the app uses from the chosen theme.
* `[plugins.config."bcode.code_review".theme]`: `name`, `profile`, `overlays` and `paths` for
  the review only; each set field replaces the `[tui.theme]` one.

#### Appearance: the review's own, or bcode's

The review can present its own palette and profile, apart from bcode's, or follow bcode's. The
appearance picker opens with `ctrl+t` (on home and in a review), from the command palette
("Appearance: the review's palette and profile…") and from home's keyboard help. It lists a
**Follow Bcode** row (with bcode's current palette and profile), the palettes and the profiles;
its first line says which mode is active. The cursor previews the row it is on on the whole
app at once, without saving anything; `f` jumps to Follow Bcode. `enter` applies: a palette or
profile row saves the review's own choice, the Follow Bcode row removes it so the review
follows bcode's again. `esc` restores what was presented. The appearance watcher keeps
running meanwhile, but its results wait until the picker closes, and results read from the
state before an apply are dropped, so the applied appearance never flickers back.

Choices are saved in bcode's user state file (`$BCODE_TUI_STATE`, else `tui.toml` in the
state directory: `$BCODE_STATE_DIR`, `$XDG_STATE_HOME/bcode` or `~/.local/state/bcode`).
bcode's own appearance picker writes `[theme]`; the review's picker writes only
`[apps.review.theme]` (`name`, `profile`), so neither changes the other. Older state files
without an `[apps]` table read as following bcode.

Precedence for the review, highest first:

1. A palette named by `[plugins.config."bcode.code_review".theme] name`, and a profile named
   by `profile` in the same table. Each is chosen for the review specifically in
   configuration, so it wins over every saved palette or profile on its axis; the picker then
   shows that axis as pinned by its key instead of listing its rows, and says that only the
   other axis applies (or, with both pinned, that choices in the picker do not apply).
2. The review's own saved choice (`[apps.review.theme]`).
3. bcode's saved choice (`[theme]`), as bcode's chat interface presents it.
4. The configured selection: `[plugins.config."bcode.code_review".theme]` `overlays` and
   `paths` over `[tui.theme]`, and its defaults.

Saved choices over the shared `[tui.theme]` configuration mirror bcode's chat interface, where
the picker's saved palette replaces the configured one. A saved palette or profile that does
not resolve is reported and the configured one is kept. The theme follows changes while the app
runs: configuration files, the user state file and theme files are checked every 750 ms, so
choosing a palette in bcode's appearance picker restyles an open review that follows bcode,
and leaves one with its own choice alone. When a host supplies the theme itself, the picker is
unavailable and the app says so.

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
`BCODE_CODE_REVIEW_STATE_DIR` when that environment variable is set. Any number of review apps
(in any repositories) and the daemon's plugin use it at once. The storage engine locks the file
exclusively while a connection is open and fails an open that finds it locked, so each store
call first takes a turn: an exclusive lock on `code-review.db.access.lock` beside it, waited for
(woken when the holder releases it) for up to 30 s, and held only while the call opens, uses,
and closes the database. Schema migrations run during a turn, the first time each process opens
the database. A call that waits out the whole 30 s fails with a "stayed in use" error instead of
a lock error.

## Testing

* `cargo test -p bcode_code_review -p bcode_code_review_ui` runs the data layer, document and
  screen tests. Screens are driven by a scripted host with a private state directory.
* `screen_benchmark`, `screen_benchmark_sizes`, `document_benchmark` and
  `data_layer_benchmark` are ignored benchmarks; run them with
  `cargo test --release -p <crate> <name> -- --ignored`.
* The `review-*` cases of `scripts/smoke-continuous-tui-pty.py` drive a real `bcode review`
  in a pseudo-terminal against an isolated fixture repository and state.

## See also

* [Embedded sessions](embedded-sessions.md): how apps such as the review share sessions, tools
  and origins with Bcode.
* [Code review plugin extension points](code-review-plugin-extension-points.md): the service,
  provider, publisher and importer interfaces other plugins implement.
* [`code-review-v2-architecture.md`](code-review-v2-architecture.md): the design record and
  measurements.
* [Permissions](permissions.md): the rules the review tools follow.
* [Reviewing files and folders as they are now](review-files-and-folders.md): folder and glob
  sources, the tree picker, uncommitted marks, and the path tools.
