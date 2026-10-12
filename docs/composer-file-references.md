# Composer file references

The bundled `bcode.file-references` plugin contributes workspace-relative path completion to the prompt composer. Type `@` at the beginning of a token, then a fuzzy path query. Navigate suggestions with the configured up/down actions, accept with Tab or Enter, and dismiss with Escape. Slash completion takes precedence. Ordinary typing remains editor-owned.

Accepting a file replaces only the active token. Paths containing whitespace, quotes or backslashes are quoted/escaped by the plugin. Directory candidates explicitly request reactivation of the same contributor. Email addresses, escaped triggers, ordinary quoted prose and selected text do not activate completion.

References insert text only: selecting a path does not attach or read its contents into model context. Normal authorized tools remain responsible for reading referenced files. Bcode consumes the published BMUX undo API: accepting a changed completion applies one validated live-editor transaction. Undo restores the prior text, cursor and selection; redo restores the accepted edit. Rejected or text-identical edits leave the editor and undo/redo history unchanged.

## Configuration

### Composer interaction keys

The generic `composer_interaction` scope uses `tui.control.up` and
`tui.control.down` for navigation, `tui.control.submit` (Enter) and
`tui.control.focus_next` (Tab) for acceptance, and `tui.control.cancel`
(Escape) for dismissal. These actions apply to any supported composer contributor,
not just file references. Configure key-to-action overrides using the existing
contributor-scope syntax:

```toml
[tui.keybindings.scopes.composer_interaction]
"ctrl+p" = "tui.control.up"
"ctrl+n" = "tui.control.down"
"ctrl+y" = "tui.control.submit"
"ctrl+x" = "tui.control.cancel"
"tab" = ""
```

Remapping an action replaces its default keys in this scope; the example replaces
Up/Down, Enter and Escape with the listed control keys. An empty action ID disables
that scope's binding, so Tab no longer accepts a candidate in this example.
Disabling a binding does not disable the plugin or suppress ordinary chat/editor
handling of that key. Composer key arbitration applies only while the picker has
visible measured rows and no foreground modal; ordinary typing stays editor-owned.

### Composer undo and redo

Chat editor actions are separate from picker acceptance. `tui.editor.undo` defaults
to Ctrl+Z and Super+Z; `tui.editor.redo` defaults to Ctrl+Shift+Z,
Super+Shift+Z and Ctrl+Y. Configure them in the chat scope:

```toml
[tui.keybindings.chat]
"ctrl+r" = "tui.editor.undo"
"ctrl+t" = "tui.editor.redo"
```

These remaps replace the respective actions' default keys. To disable only one
shortcut while leaving the other defaults available, use an empty action ID instead:

```toml
[tui.keybindings.chat]
"super+z" = ""
"ctrl+y" = ""
```

The second example retains Ctrl+Z for undo and the Shift+Z shortcuts for redo.
Recognized undo/redo commands with empty history do not insert text or submit a
prompt. These chat actions do not enable undo/redo in picker or modal inputs.

### Plugin selection and search

The plugin is included in the standard static bundle. Disable it through normal plugin selection:

```toml
[plugins]
disabled = ["bcode.file-references"]
```

Plugin-owned search defaults can be bounded further:

```toml
[file_references]
include_hidden = false
max_results = 32
max_entries = 5000
max_depth = 16
timeout_ms = 100
max_bytes = 131072
```

The workspace root is never configurable through this plugin. Attached-session discovery uses the daemon's canonical session directory; before attachment, the launch directory must match the connected client's runtime directory. Remote discovery runs on the daemon, never through a silent client-local fallback.

## Extension boundaries

* `bcode_plugin_models::composer` defines renderer-neutral documents, activation ranges, bounded search snapshots, explicit completion continuation, and revision-bound edits. Offsets are UTF-8 bytes, not terminal cells.
* `bcode_plugin_sdk::composer` registers lightweight local activation adapters. Enabled manifest `composer_contributions` connect those adapters to the versioned `bcode.composer/v1` search service. Unsupported/missing local adapters leave ordinary composition available.
* The TUI observes document, selection, session, selected agent, workspace and plugin-runtime identity; mounts a generic BMUX picker; applies validated text edits; and cancels superseded work. It does not recognize `@`, traverse directories, rank paths, or infer directory navigation from labels.
* The file-reference plugin owns trigger grammar, quoting, ranking, bounded configuration and candidate continuation. Its search service calls the filesystem plugin through an invocation-bound nested service bridge.
* The filesystem plugin owns descriptor-confined path discovery and ignore policy. Directory discovery and policy-file reads require independent host authorization. Policy-file contents are not returned or attached. Unsupported ignore syntax, denied access and budget boundaries remain explicitly incomplete rather than silently bypassed.
* Contextual service calls are restricted to enabled composer declarations. Nested operations require exact manifest `invocation_operations` opt-in; explicit agent denials prevail. Routing eligibility is not a read-only promise or a resource grant. Only nested service calls are supported by this bridge; exchange, artifact writing and tool execution are unavailable.

Contextual searches bind to the draft's selected agent: a pending agent selection
is used when present, otherwise the current agent. The agent ID travels with the
service context so nested filesystem operations use that agent's authorization;
changes to agent, session, workspace or plugin registry invalidate pending results.
Each new search carries the selected manifest `contribution_id` alongside the plugin
and service interface route. The host and service validate enabled contribution
membership before dispatch, including plugins exposing multiple contributions on
one interface. Legacy empty IDs are eligible only when exactly one supported
enabled contribution matches. Directory reactivation retains the same plugin and
contribution rather than activating a different contributor.

Requests and replies are bounded, deadline/cancellation-aware and revision-fenced. Stale, dismissed, disabled or replaced contributions cannot edit the composer. Generic loading, empty, error and truncated states remain distinct. Native plugin loading is not a sandbox; plugins retain the repository's existing native trust boundary.
