# Permissions

Bcode uses an agent-scoped permission model with three verbs: `allow`, `ask`, and `deny`. Declarative rules live in `bcode.toml` under `[agent.<agent_id>]` sections. Runtime rules written by the TUI "always allow" prompt live in a separate state file (see [Runtime rule persistence](#runtime-rule-persistence) below), so a read-only declarative config (for example on NixOS / home-manager) is never mutated.

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

The TUI permission prompt offers an "always allow" / "always deny" action that writes a rule into a **runtime state file**, never into `bcode.toml`. This means Nix / home-manager users (and anyone with a read-only declarative config) can still click "always allow" without their config being touched.

The state file lives at `$BCODE_PERMISSIONS_STATE` if set, otherwise `$BCODE_STATE_DIR/permissions.toml`, otherwise `$XDG_STATE_HOME/bcode/permissions.toml`, otherwise `$HOME/.local/state/bcode/permissions.toml`. It uses the same `[agent.<id>.permission.<category>]` schema as `bcode.toml`, so rules can be promoted to declarative config by copying entries verbatim.

At load time, the state file is merged **on top of** `bcode.toml` per `(agent, category, pattern)`. State entries win over same-pattern declarative entries. Patterns present only in declarative config survive untouched.

The rule is scoped to the currently selected agent, with category inferred from the tool:

* `shell.run` → `bash`. Persists **two** rules: the literal command (so the exact same invocation is remembered) and a broadened `<first-word> *` glob (so variations like `echo hello` after approving `echo hi` don't prompt again). If the literal command already contains a trailing `*`, only the literal rule is persisted.
* `filesystem.write` → `write` (literal path).
* `filesystem.edit`  → `edit` (literal path).
* `filesystem.{read,list,find,grep,stat,exists}` → `read` (literal path).

Filesystem paths are persisted literally by default because implicit directory globs can grant unintended access. To broaden a persisted path rule, edit the state file (or `bcode.toml`) directly — for example, replace `"src/foo.rs" = "allow"` with `"src/**" = "allow"`.

The CLI equivalent also writes to the state file:

```sh
bcode permission add --agent build --category bash --pattern 'cargo *' --action allow
```

### Promoting runtime rules to declarative config

Because both files share the same schema, promoting a runtime rule into your declarative `bcode.toml` is a straightforward copy: open `$BCODE_STATE_DIR/permissions.toml`, pick the `[agent.<id>.permission.<category>]` entries you want permanent, and move them to your `bcode.toml`. Deleting them from the state file afterward keeps the two sources in sync.

### Why state wins

If your declarative config sets `"rm -rf /" = "ask"` and you then click **always allow**, the state file records `"rm -rf /" = "allow"` and it will win. That is deliberate: declarative config that opts into `ask` has already delegated the final decision to you. To make a rule truly unoverridable from the TUI, declaratively set it to `allow` or `deny` — the TUI never prompts for rules the evaluator short-circuits, so no "always allow" button can be clicked for them in the first place.

## Precedence

The effective policy for an agent comes from these sources, merged in order (later sources override earlier ones per `(agent, category, pattern)`):

1. Built-in `default_config()` — used only when every later source is empty for that agent.
2. `$XDG_CONFIG_HOME/bcode/bcode.toml` (or `$HOME/.config/bcode/bcode.toml`) — declarative user config.
3. `$CWD/.bcode/bcode.toml` — per-project declarative overrides.
4. Runtime permissions state file (see [Runtime rule persistence](#runtime-rule-persistence)) — highest priority, per-rule overrides.

Per-agent entries replace wholesale across declarative files; there is no partial merge within a single `[agent.<id>]` block across `bcode.toml` files. The runtime state file, in contrast, merges per-rule: a rule in the state file overrides the same-pattern rule from declarative config but does not replace declarative rules for other patterns.
