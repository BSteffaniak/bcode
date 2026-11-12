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
| `bcode_code_review_plugin` | Serves `bcode.code_review/v1` as JSON adapters over `bcode_code_review` for agents and other plugins, declares the review keymap actions, and (when statically bundled) registers the `bcode review` subcommand. |

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
entry points with the default daemon client and the platform clipboard and opener; it is the
only part of the app that talks to the daemon for sessions and plugin services, so the data layer
has no daemon client dependency. `run` connects any host's redraws to its loop
(`ReviewHost::connect_redraw`). Another host, for example one that embeds the review in the chat
TUI, implements the same trait and drives `ReviewApp` (a `view::View`) from its own loop,
handling `Action::OpenSession` itself, without changing the UI.

### Sessions and the daemon

The app starts without a daemon. It reaches one only to ask Bcode about the code, to follow
linked sessions, for plugin services, and for `publish-github`.

* Inside `bcode review` the client starts the daemon when needed, and opening a linked session
  (`o`) runs `bcode tui <session>` with the same executable.
* The standalone `bcode-review` never starts a daemon (it is not the Bcode program). It attaches
  to a running one and opens sessions with `bcode` from the path. Without a running Bcode,
  questions and `publish-github` report that Bcode is not running.

Opening a session hands the terminal to `bcode tui` and resumes the review where it was when
that exits.

## Configuration

Both entry points read the ordinary bcode configuration files, discovered and layered like
bcode's own (`--print-config-paths` lists them in order):

* `[plugins.config."bcode.code_review"]`: where review state is stored, and presentation
  settings under its `review_v2` table (`diff_layout`, `side_by_side_breakpoint`, `tab_width`,
  `icons`).
* `[tui.keybindings.scopes."code-review*"]`: keybinding overrides of the review scopes
  `code-review`, `code-review-home`, `code-review-wizard`, `code-review-files`,
  `code-review-threads` and `code-review-composer`. `none` or `unbind` removes a default.
* `[tui.theme]`: the theme, loaded through `bcode_tui_theme` discovery like `bcode-git`.
* `[tui.diff_viewer]`: bcode's diff layout preference, followed unless the review's own table
  sets a layout.
* `[plugins] disabled`: listing `bcode.code_review` disables the app.

The review keymap actions are declared in the plugin manifest
(`plugins/code-review-plugin/bcode-plugin.toml`), which the app reads at build time, so the app
and bcode's keymap catalog share one declaration.

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
