# TUI keybindings

## Contributor registry

The terminal SDK exposes `tui::keymap::KeymapRegistry`. Contributors register stable
namespaced action IDs, input scopes, descriptions, and default strokes through
`PluginTuiRegistry::register_keymap_action`. The frontend incorporates enabled
static TUI contributors and manifest-declared native dynamic plugin actions into
one effective registry. Dynamic manifests declare actions with `[[keymap_actions]]`
entries containing `id`, `scope`, `description`, and optional `defaults`.
Plugin surfaces query `PluginTuiHost::keymap_action` for dispatch and `keymap_keys`
for shortcut hints.
Libraries can use the same registry API directly. This is frontend-owned state,
not a process-wide singleton or a prerequisite for non-terminal operation.

```toml
[tui.keybindings.skill_picker]
"ctrl+i" = "tui.skill.invoke"

[tui.keybindings.scopes."metrics-dashboard"]
"ctrl+x" = "bcode.metrics.close"
"q" = ""
"escape" = ""
```

Binding an action replaces all its default keys within that scope. An empty action
ID unbinds a key. Configuration for unregistered actions is retained for subsequent
registration but cannot dispatch; `unresolved_actions()` exposes these references.
Malformed strokes and conflicting registrations return errors rather than silently
falling back. The TUI help command lists registered actions and their effective
bindings, including unbound actions.

Generic terminal controls receive BMUX adapter events only after a registered
semantic action resolves; unresolved keys never fall back to component defaults.
Literal text insertion is separate from shortcut dispatch. Explicitly overridden
or unbound keys are not reinterpreted as text by the shared control adapter.

Host-aware surface rendering and keymap preparation keep initial hints, measured
geometry, and dispatch aligned. Configuration/runtime replacement refreshes the
effective contributor snapshot; disabled plugins do not retain registered actions.
Standalone readers resolve the same configuration before entering terminal mode.
ASCII uppercase terminal events normalize to explicit `shift+letter` bindings;
use `shift+a`, not bare `A`, in configuration. Unicode literal input remains unchanged.

The child monitor toggle (`tui.monitor.toggle`) defaults to `Ctrl+G` in chat.
Global session search (`tui.session.search`) is unbound by default and remains configurable.
Workflow scheduling-page navigation is also
unbound where the former Shift-N shortcut was already consumed by new authoring.
Both actions remain configurable and appear in effective action discovery.


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

By default, releasing the left mouse button after dragging a nonempty transcript selection copies
it automatically. Successful automatic or keyboard copying keeps the highlight for one second,
then clears the selection. Failed copying preserves the selection for retry. Configure this under
`[tui.selection]`; see [selection configuration](tui-rendering.md#transcript-selection-copying).

The copy binding is configurable, and clearing can be bound explicitly without changing the
existing Escape or Ctrl+C behavior:

```toml
[tui.keybindings.chat]
"ctrl+shift+c" = "tui.transcript.copySelection"
"alt+x" = "tui.transcript.clearSelection" # Optional; no default clear-selection binding.
```

For keyboard-only copying and persistent highlighting:

```toml
[tui.selection]
auto_copy_on_mouse_release = false
clear_after_copy = false
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

`Ctrl+G` toggles the child-session tree/monitor from the normal chat scope, configurable as
`tui.monitor.toggle`. `/monitor` opens or refocuses the panel. While it owns focus,
`Up`/`Down` select a row, `Right` toggles its branch, `Enter` opens the selected session,
and `Left` opens the attached session's parent when available. `Esc` returns focus to
the composer without hiding the panel; `Ctrl+G` refocuses it from the composer and
closes it when the monitor already owns focus.
Clicking a row selects/refocuses it; the separate chevron toggles expansion rather
than opening the session. These navigation actions use the `monitor` keymap scope,
and the panel footer reflects effective bindings.

Global session search (`tui.session.search`) has no default shortcut;
open `/sessions`, then use the session picker's `Ctrl+F` search route.

Typing `/` in the composer exposes slash-command completion. Common commands include:

```text
/monitor
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

## Quick startup onboarding

The account chooser uses the `quick_onboarding` scope for navigation, confirmation,
close, connecting another account, and advanced setup. Its hints (including the
narrow-screen fallback) resolve the same effective bindings as dispatch. Advanced
setup remains available during enrollment through this scope; enrollment input
uses the existing `setup_connection` and authentication scopes.

```toml
[tui.keybindings.scopes.quick_onboarding]
"ctrl+j" = "tui.control.down"
"ctrl+y" = "tui.control.submit"
"ctrl+a" = "tui.quick.advanced"
"c" = "" # Disable the connect shortcut.
```
