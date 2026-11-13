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
| `repo` | Browse the repository's files and comment anywhere. |
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
  as viewed. `code_review.navigate` moves the cursor to the place it names. In a session the app
  started (the dock's conversation, or a question's own session) every finished call is applied,
  including calls that finished before the app first observed the session. In a session it
  found (reopening a review continues its last assistant session), calls that had already
  finished when the session is first observed are not replayed.
* `ctrl+n` starts a new conversation at once: a lookup or a session start of the previous
  conversation that finishes afterwards never replaces it, and messages written in the previous
  conversation still go to its session.

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
| `code_review.pull_request` | read, web | The pull request behind the review (by its pull request sources, else its branch): title, state, branches, review decision, CI checks, via the GitHub CLI with the user's credentials. |
| `code_review.navigate` | read | Moves the reviewer's view to a file or line. |
| `code_review.add_comment` | write | Drafts a comment on lines (`path`, `line`, `end_line`, `side`), a file, or the review, with an optional kind and severity. |
| `code_review.reply` | write | Replies in a thread. |
| `code_review.resolve_thread` | write | Resolves or reopens a thread. |
| `code_review.mark_viewed` | write | Marks files viewed or not viewed at their current content. |
| `code_review.suggest_change` | write | A suggested comment the reviewer accepts or rejects, optionally replacing the lines (a `suggestion` block). |
| `code_review.publish` | publish | Previews, or with `submit` publishes, the comments with a built-in publisher (`local_markdown`, `markdown_file`, `json_file`). `output_path` is a path relative to the session's working directory that stays inside it; without it the file goes to `.bcode/reviews/` in the repository. |

Reads are bounded (2,000 files, 2,000 diff or content lines per call, 200 threads, 4 KiB per
comment, 128 KiB of output) and say when they were cut. Writes and publishing return what they
did. Publishing to a forge such as GitHub stays in the app (`P`), which reaches publisher plugins.

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
  exactly, moved with their code when it moved, else on their file (*on an older version*) or on
  the layer. Each has **Reply**, **Resolve**/**Reopen**, **Open ↗** and **Ask Bcode** as far as
  allowed; resolved ones fold. **Reply** sends at once and offers **Add to review**, which holds
  the reply as a draft under the thread until the next review of that pull request is submitted
  (the default is configurable).
* **The rail** marks each layer approved (✓) or changes requested (✕), with its open threads
  and a dot when something is new.
* **The threads panel** lists pull request threads too, and filters them: *on the pull request*
  and *waiting on you* (the filter key cycles).
* **Publishing** offers **Review on {provider}** first: its options are the verdict (only those
  the provider accepts) and a summary, and held replies go with it. Approve and Request changes
  in the bar open it directly at that verdict for that pull request.
* **The history panel** (`shift+b`) names each layer's reviews and whether they still apply.
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
| `c` | Continue the latest review |
| `/` | Filter reviews |
| `r` | Rename the review |
| `x` | Archive the review |
| `shift+r` | Restore the archived review |
| `d` | Show / hide the preview |
| `tab` | Switch between suggestions and reviews |
| `ctrl+r` | Reload reviews and suggestions |
| `?` | Keyboard help |
| `q` | Quit |

### New-review wizard (`code-review-wizard`)

| Keys | Action |
|---|---|
| `tab` | Mark or unmark the commit or file (include or exclude a source when editing) |
| `shift+tab` | Mark every commit or file from the last mark |
| `esc` | Back one step |

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
| `ctrl+k` | Open, focus, or close the review assistant |
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
| `shift+l` | Show only the changes since your last review (toggle) |
| `shift+m` | Mark the review pass complete |
| `enter` | Actions of the thread or AI answer (reveals a gap) |
| `z` | Collapse or expand the thread (whole or preview AI answer) |
| `shift+z` | Collapse every thread (expand all when all are collapsed); on a file's header, fold every file (open all when all are folded) |
| `shift+h` | Show / hide resolved threads |
| `h` | Show / hide viewed files in the file panel |
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
| `shift+b` | Stack history: how each pull request changed in each push |

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
| `esc` | Cancel the comment |

### AI card with a running turn or a permission request (`code-review-feed`)

| Keys | Action |
|---|---|
| `y` | Allow the AI card's permission request once |
| `a` | Always allow the AI card's permission request |
| `n` | Decline the AI card's permission request once |
| `d` | Never allow the AI card's permission request |
| `esc`, `ctrl+c` | Stop the AI card's running turn |

### Review assistant (focused) (`code-review-assistant`)

| Keys | Action |
|---|---|
| `ctrl+k` | Close the assistant |
| `esc` | Back to the review (the assistant stays open) |
| `ctrl+c` | Stop the assistant's running turn |
| `ctrl+x` | Attach or detach the cursor's place as context |
| `ctrl+n` | Start a new conversation |
| `ctrl+o` | Open the assistant's session in Bcode |
| `pageup` | Scroll the conversation up |
| `pagedown` | Scroll the conversation down |

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
  rule); and `fold_viewed_files` (default true: a viewed file folds to its header). Themes can
  set the `source.file.header`, `source.file.desk`, `source.file.edge` and `source.file.label`
  roles. See [`review-separators-and-folding.md`](review-separators-and-folding.md).
* `[plugins.config."bcode.code_review".conversation]`: pull request conversations.
  `enabled` (default true); `replies` (`"send_now"`, the default, or `"with_review"`: where a
  reply to a pull request thread goes, the other way being a button); `overview` (`"expanded"`,
  the default, `"auto"`: folded unless something is new, or `"collapsed"`); `notify` (which new
  activity makes a toast: `"reviews_and_replies"`, the default, `"all"`, `"reviews"` or
  `"none"`); `fold_resolved` (default true); `outdated_on_file` (default true: threads whose code
  could not be followed show on their file). Its `signals` table: `provider` (default true: use
  the provider's watch), `min_gap_ms` (default 2000: reads closer together are merged),
  `pause_unfocused` (default true), and `providers.<id>`, passed to that provider's watch as is
  (GitHub reads `interval_ms`, default 30000, at least 10000). Invalid values are reported and
  the defaults are used.
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
`BCODE_CODE_REVIEW_STATE_DIR` when that environment variable is set. Schema migrations of the
shared database run one at a time under the lock file `code-review.db.migrate.lock` beside it.

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
