# Bcode

Bcode is a Rust-native, TUI-first, plugin-driven coding agent with a local client/server architecture.

## TUI keybindings

TUI keybindings are configurable in `bcode.toml` under scoped `[tui.keybindings.*]` tables. Each scope maps `key = "action.id"`, matching bmux-style key-to-action configuration. Set a key to `""`, `"none"`, or `"unbind"` to remove a default binding for that key.

```toml
[tui.keybindings.chat]
"enter" = "tui.input.submit"
"shift+enter" = "tui.input.newLine"
"left" = "tui.editor.moveCursorLeft"
"right" = "tui.editor.moveCursorRight"
"alt+left" = "tui.editor.moveCursorWordLeft"
"alt+right" = "tui.editor.moveCursorWordRight"
"ctrl+left" = "tui.editor.moveCursorWordLeft"
"ctrl+right" = "tui.editor.moveCursorWordRight"
"ctrl+a" = "tui.editor.moveCursorStart"
"ctrl+e" = "tui.editor.moveCursorEnd"
"backspace" = "tui.editor.deleteCharBackward"
"delete" = "tui.editor.deleteCharForward"
"alt+backspace" = "tui.editor.deleteWordBackward"
"ctrl+w" = "tui.editor.deleteWordBackward"
"alt+delete" = "tui.editor.deleteWordForward"
"ctrl+delete" = "tui.editor.deleteWordForward"
"ctrl+u" = "tui.editor.deleteToStart"
"ctrl+k" = "tui.editor.deleteToEnd"
"escape" = "app.interrupt"
"ctrl+d" = "app.exit"
"ctrl+c" = "app.clear"
"ctrl+f" = "app.search"
"pageUp" = "transcript.pageUp"
"pageDown" = "transcript.pageDown"

[tui.keybindings.permission]
"y" = "app.permission.approve"
"n" = "app.permission.deny"
"a" = "app.permission.alwaysAllow"
"d" = "app.permission.alwaysDeny"
"left" = "tui.select.previous"
"right" = "tui.select.next"
"enter" = "tui.select.confirm"
"escape" = "tui.select.cancel"

[tui.keybindings.session_picker]
"up" = "tui.select.previous"
"down" = "tui.select.next"
"enter" = "tui.select.confirm"
"escape" = "tui.select.cancel"
```

Key format follows `modifier+key`, with `ctrl`, `alt`, and `shift` modifiers. Examples: `ctrl+d`, `alt+left`, `pageUp`, `escape`, `enter`.

The chat composer uses a Unicode-aware editor buffer. Standard composer defaults include left/right grapheme movement, `alt+left` / `alt+right` and `ctrl+left` / `ctrl+right` word movement, `ctrl+a` / `ctrl+e` start/end movement, `backspace` / `delete` character deletion, `alt+backspace` / `ctrl+w` word-backward deletion, `alt+delete` / `ctrl+delete` word-forward deletion, and `ctrl+u` / `ctrl+k` delete-to-start/end. Plain `home` and `end` remain transcript top/bottom bindings in the chat scope by default.

Permission prompts are modal by default: permission actions only apply in the permission scope, and hints are generated from the configured permission keymap.

### Permissions

Bcode uses an agent-scoped permission model with `allow` / `ask` / `deny` rules under `[agent.<id>.permission]` in `bcode.toml`. See [`docs/permissions.md`](docs/permissions.md) for the full shape, category list, and built-in defaults for the `plan` and `build` agents.
