# Permissions

Bcode uses an agent-scoped permission model with three verbs: `allow`, `ask`, and `deny`. Rules live in `bcode.toml` under `[agent.<agent_id>]` sections.

## Config shape

```toml
# ~/.config/bcode/bcode.toml (or $XDG_CONFIG_HOME/bcode/bcode.toml)

[agent.build.tools]
"shell.run"       = true
"filesystem.write" = true
"filesystem.edit" = true

[agent.build.permission]
external_directory = "ask"
bash  = { "*" = "ask", "cargo *" = "allow", "git push *" = "deny" }
read  = { "**" = "allow" }
write = { "**" = "ask", "target/**" = "allow" }
edit  = { "**" = "ask" }

[agent.plan.tools]
"filesystem.write" = false
"filesystem.edit"  = false

[agent.plan.permission]
external_directory = "allow"
bash  = { "*" = "deny", "cargo check *" = "allow", "git diff *" = "allow", "ls *" = "allow", "rg *" = "allow" }
read  = { "**" = "allow" }
write = { "**" = "deny" }
edit  = { "**" = "deny" }
```

## Categories

* `bash` — patterns matched against `shell.run` command strings. Pi/OpenCode-style command globs.
* `read` — path globs for read-only filesystem tools (`filesystem.read`, `filesystem.list`, `filesystem.find`, `filesystem.grep`, `filesystem.stat`, `filesystem.exists`).
* `write` — path globs for `filesystem.write`.
* `edit` — path globs for `filesystem.edit`.
* `external_directory` — a single action governing any tool argument that resolves outside the session working directory. This short-circuits before path-category matching: if a write path resolves outside `cwd` and `external_directory = "deny"`, the call is denied even when a more permissive `write` rule would match.

## Actions

* `allow` — run the tool immediately. No prompt.
* `ask` — prompt the user for approval (the permission modal in the TUI, or the daemon's pending-permission queue over IPC).
* `deny` — refuse without prompting.

## Rule resolution

Within a category, the **most specific** matching rule wins:

1. Exact literal patterns outrank wildcard patterns.
2. Among patterns of the same shape, the one with the longest literal content wins.
3. Among patterns of equal specificity, the lexicographically smaller pattern wins (stable tiebreak).

If no rule matches, the tool's side-effect falls back to:

* Read-only tools → `allow`.
* Write/execute tools → `ask` if the tool is enabled for the agent, `deny` if disabled.

Path globs use the same syntax as ripgrep (`globset`): `**` matches any number of path segments, `*` matches within a segment, `?` matches a single character, `[...]` character classes, `{a,b}` alternation.

## Tool enablement

`[agent.<id>.tools]` maps tool names to booleans. Disabling a tool short-circuits the category rules with a hard `deny`. Enabling a tool only lets it run if the category rules also permit it.

Setting `tools = { write = false }` additionally triggers the shell hard-deny for common file-writing commands (`>`, `tee`, `touch`, `cp`, `mv`, `rm`, `mkdir`, `sed -i`, etc.) in `shell.run`, so plan-style agents can't bypass the write restriction through bash.

## Built-in defaults

When no `[agent.*]` sections exist in `bcode.toml`, Bcode falls back to built-in defaults:

| Agent   | `bash`                                                                                                       | `read` | `write`           | `edit`            | `external_directory` |
|---------|--------------------------------------------------------------------------------------------------------------|--------|-------------------|-------------------|----------------------|
| `plan`  | `* = deny`, plus `allow`: `cargo check *`, `cargo test *`, `git diff *`, `git status *`, `ls *`, `rg *`      | allow  | deny (tool off)   | deny (tool off)   | allow                |
| `build` | `* = ask`                                                                                                    | allow  | unmatched → ask   | unmatched → ask   | allow                |

Any single `[agent.<id>]` section in `bcode.toml` replaces the full built-in set: define both plan and build explicitly if you want to customize one without losing the other.

## Custom agents

Any `[agent.<id>]` you declare is usable via `/agent <id>` in the TUI and the CLI. If no agent-profile plugin registers the ID, Bcode logs a warning at startup (`agent defined in bcode.toml but not registered by any agent-profile plugin`), and the agent won't appear in agent pickers, but policy evaluation and `/agent <id>` switching still work.

## Runtime rule persistence

The TUI permission prompt offers an "always allow" / "always deny" action that writes a rule back into `bcode.toml`. The rule is scoped to the currently selected agent, with category inferred from the tool:

* `shell.run` → `bash`. Persists **two** rules: the literal command (so the exact same invocation is remembered) and a broadened `<first-word> *` glob (so variations like `echo hello` after approving `echo hi` don't prompt again). If the literal command already contains a trailing `*`, only the literal rule is persisted.
* `filesystem.write` → `write` (literal path).
* `filesystem.edit`  → `edit` (literal path).
* `filesystem.{read,list,find,grep,stat,exists}` → `read` (literal path).

Filesystem paths are persisted literally by default because implicit directory globs can grant unintended access. To broaden a persisted path rule, edit `bcode.toml` directly (for example, replace `"src/foo.rs" = "allow"` with `"src/**" = "allow"`).

The CLI equivalent is:

```sh
bcode permission add --agent build --category bash --pattern 'cargo *' --action allow
```

## Precedence

The effective policy for an agent comes from, in order:

1. `$XDG_CONFIG_HOME/bcode/bcode.toml` (or `$HOME/.config/bcode/bcode.toml`).
2. `$CWD/.bcode/bcode.toml` (per-project overrides).
3. Built-in `default_config()` when the merged `[agent]` map is empty.

User config wins over project config for the same keys (later files in the merge order extend and replace). Per-agent entries replace wholesale; there is no partial merge within a single `[agent.<id>]` block across files.
