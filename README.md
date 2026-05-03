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
"app.permission.approve" = []
"app.permission.deny" = []
"app.permission.alwaysAllow" = []
"app.permission.alwaysDeny" = []
"transcript.pageUp" = ["pageUp"]
"transcript.pageDown" = ["pageDown"]
```

Key format follows `modifier+key`, with `ctrl`, `alt`, and `shift` modifiers. Examples: `ctrl+d`, `pageUp`, `escape`, `enter`.

Permission prompts are modal by default instead of global keybindings: `y` allows once, `n` denies, `a` always allows, `d` always denies, arrow keys choose an option, and `enter` confirms.
