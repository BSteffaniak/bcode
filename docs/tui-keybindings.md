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

The composer is Unicode-aware. Arrow keys move by grapheme; `Alt+Left` / `Alt+Right` and `Ctrl+Left` / `Ctrl+Right` move by word. `Ctrl+A` / `Ctrl+E` move to the start or end, while standard character, word, and line deletion bindings are available.

Example overrides:

```toml
[tui.keybindings.chat]
"enter" = "tui.input.submitSteering"
"ctrl+shift+enter" = "tui.input.submitFollowUp"
"shift+enter" = "tui.input.newLine"
"escape" = "app.interrupt"
"ctrl+d" = "app.exit"
"ctrl+f" = "app.search"
"pageUp" = "transcript.pageUp"
"pageDown" = "transcript.pageDown"
```

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

Transcript search can use `deep:`, `content:<kind>`, and `provider:<id>` query controls when matching search providers are enabled. Search previews remain derived results; opening one hydrates the corresponding canonical session location.

## Command palette and slash commands

`Ctrl+F` opens the command palette in the normal chat scope. Type to filter and press `Enter` to run a command. Plugins can contribute commands and complete TUI surfaces to the same palette.

Typing `/` in the composer exposes slash-command completion. Common commands include:

```text
/sessions
/new
/plan
/build
/compact
/worktree
/skill
/thinking
```

Available plugin commands depend on the active plugin selection.
