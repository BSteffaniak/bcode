# Bcode

Bcode is a Rust-native, TUI-first, plugin-driven coding agent with a local client/server architecture.

## TUI keybindings

TUI keybindings are configurable in `bcode.toml` under `[tui.keybindings]`. Each action maps to an array of key strings. Multiple keys can trigger the same action; an empty array unbinds it.

```toml
[tui.keybindings]
"tui.input.submit" = ["enter"]
"tui.input.newLine" = ["shift+enter"]
"app.interrupt" = ["escape"]
"app.exit" = ["ctrl+d"]
"app.clear" = ["ctrl+c"]
"app.permission.approve" = ["alt+y"]
"app.permission.deny" = ["alt+n"]
"app.permission.alwaysAllow" = ["alt+shift+y"]
"app.permission.alwaysDeny" = ["alt+shift+n"]
"transcript.pageUp" = ["pageUp"]
"transcript.pageDown" = ["pageDown"]
```

Key format follows `modifier+key`, with `ctrl`, `alt`, and `shift` modifiers. Examples: `ctrl+d`, `alt+shift+y`, `pageUp`, `escape`, `enter`.
