# TUI keybindings

Bcode's terminal keybindings are scoped and configurable in `bcode.toml`. Each table maps a key to an action ID. Set a key to `""`, `"none"`, or `"unbind"` to remove its default binding.

Key names use `modifier+key`, with `ctrl`, `alt`, and `shift` modifiers. Examples include `ctrl+d`, `alt+left`, `pageUp`, `escape`, and `enter`.

## Chat and transcript

Common defaults:

| Key | Action |
| --- | --- |
| `Enter` | Submit steering input |
| `Ctrl+Shift+Enter` | Queue a follow-up |
| `Shift+Enter` | Insert a newline |
| `Esc` | Interrupt active work |
| `Ctrl+D` | Exit |
| `Ctrl+C` | Clear |
| `Ctrl+F` | Open search/command flow |
| `Page Up` / `Page Down` | Scroll the transcript |
| `Home` / `End` | Jump to transcript start/end |
| `Up` / `Down` | Navigate composer history |
| `Ctrl+I` | Restore the active transcript interaction into view |
| `Ctrl+Shift+C` | Copy the current transcript selection |

Transcript selection uses logical content rather than framebuffer text. Dragging from inside one
response constrains the gesture to that response; dragging from transcript-owned surrounding space
may span multiple items in transcript order. Selection follows scrolling, history loading, and
responsive reflow. Markdown responses copy canonical source where the rendered projection is
unambiguous, while unsupported transformed visuals use an explicit rendered-text fallback rather
than inventing source ranges. Copy success, absence of a selection, and clipboard failures are
reported in the normal status area.

The copy binding is configurable:

```toml
[tui.keybindings.chat]
"ctrl+shift+c" = "tui.transcript.copySelection"
```

The composer is Unicode-aware. Arrow keys move by grapheme; `Alt+Left` / `Alt+Right` and `Ctrl+Left` / `Ctrl+Right` move by word. `Ctrl+A` / `Ctrl+E` move to the start or end, while standard character, word, and line deletion bindings are available. Active plugin text inputs consume these same configured edit, selection, newline, and submit actions.

Example overrides:

```toml
[tui.keybindings.chat]
"enter" = "tui.input.submitSteering"
"ctrl+shift+enter" = "tui.input.submitFollowUp"
"shift+enter" = "tui.input.newLine"
"escape" = "app.interrupt"
"ctrl+d" = "app.exit"
"ctrl+f" = "app.search"
"ctrl+i" = "tui.interaction.focusActive"
"pageUp" = "transcript.pageUp"
"pageDown" = "transcript.pageDown"
```

## Interactive tools

Terminal interaction placement and hidden-focus behavior are independently configurable:

```toml
[tui.interactions]
placement = "transcript"
offscreen_focus = "retain"
```

`transcript` and `retain` are the defaults. `placement = "pinned"` fixes the active interaction above the composer. `offscreen_focus = "suspend"` returns ordinary input to the composer while an inline interaction is fully hidden; use `tui.interaction.focusActive` to restore it.

## Permission dialogs

Permission prompts are modal: permission actions apply only while the dialog is active, and visible hints follow the configured permission keymap.

```toml
[tui.keybindings.permission]
"y" = "app.permission.approve"
"n" = "app.permission.deny"
"a" = "app.permission.alwaysAllow"
"d" = "app.permission.alwaysDeny"
"left" = "tui.select.previous"
"right" = "tui.select.next"
"enter" = "tui.select.confirm"
"escape" = "tui.select.cancel"
```

Depending on the request, the dialog can expose one-time, whole-batch, and remembered decisions.

## Session picker

```toml
[tui.keybindings.session_picker]
"up" = "tui.select.previous"
"down" = "tui.select.next"
"enter" = "tui.select.confirm"
"escape" = "tui.select.cancel"
```

The default session picker also uses:

| Key | Action |
| --- | --- |
| `Ctrl+F` | Search session transcripts |
| `Ctrl+N` | Create a session |
| `Ctrl+R` | Rename the selected session |
| `Ctrl+D` | Delete the selected session |

Transcript search uses discoverable keyboard controls: `Alt-M` cycles
terms/phrase/prefix/fuzzy/regex, `Alt-D` toggles ordinary versus explicit deep search, `Alt-S` cycles
sorting, `Alt-N` continues with the next provider cursor, and `Up`/`Down` update the bounded preview.
`Alt-I` inventories canonical compatibility, `Alt-G` uses two-step confirmation for canonical
migration, `Alt-B` starts the separate derived-provider backfill, and `Alt-X` cancels the latest
transient maintenance operation.

Optional textual controls include `mode:`, `deep:`, `content:`, `cwd:`, `after:`, `before:`,
`provider:`, `model:`, `agent:`, `tool:`, `status:`, `field:`, and `sort:`. Press `?` in search for
the accepted values. Deep mode may scan locally retained compressed shell/tool output; ordinary mode
excludes those scan providers. Search previews remain derived results; opening one hydrates the
corresponding canonical session location. Canonical migration and search indexing remain visibly
separate operations.

## Command palette and slash commands

`Ctrl+F` opens the command palette in the normal chat scope. Type to filter and press `Enter` to run a command. Plugins can contribute commands and complete TUI surfaces to the same palette.

`Ctrl+G` opens global session search from the normal chat scope by default and is configurable as
`tui.session.search`. `Ctrl+F` remains the session-picker compatibility search route.

Typing `/` in the composer exposes slash-command completion. Common commands include:

```text
/sessions
/new
/plan
/build
/compact
/fork
/clone
/worktree
/skill
/thinking
```

Available plugin commands depend on the active plugin selection. `/fork` and `/clone` are supplied
by the bundled, disableable session-derivation plugin rather than by the TUI host. `/fork` opens a
bounded prompt selector; `/clone` derives the current stable session snapshot.
