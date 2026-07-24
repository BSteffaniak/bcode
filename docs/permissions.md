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
command  = { "*" = "ask", "cargo *" = "allow", "git push *" = "deny" }
read  = { "**" = "allow" }
write = { "**" = "ask", "target/**" = "allow" }
edit  = { "**" = "ask" }

[agent.plan.tools]
"filesystem.write" = false
"filesystem.edit"  = false

[agent.plan.permission]
external_directory = "allow"
command  = { "*" = "deny", "cargo check *" = "allow", "git diff *" = "allow", "ls *" = "allow", "rg *" = "allow" }
read  = { "**" = "allow" }
write = { "**" = "deny" }
edit  = { "**" = "deny" }
```

## Categories

* `command` — Pi/OpenCode-style command globs matched independently against every executable subject extracted from `shell.run` POSIX syntax. Rules are not matched against a naively split raw string.
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

### Shell command analysis

On Unix, `shell.run` is analyzed as one complete POSIX/`sh` program before authorization. Quotes, escapes, heredoc bodies, assignment prefixes, pipelines, lists, newlines, background execution, groups, loops, conditionals, subshells, and command substitutions are interpreted as shell syntax rather than split as plain text. Every executable leaf is evaluated independently, including leaves nested in control flow and substitutions. The aggregate decision is `deny > ask > allow`, so an allowed preamble or sibling can never hide a denied command.

Input redirections with static paths are evaluated through `read` rules. Output, append, and read/write redirections with static paths are evaluated through `write` rules. Dynamic redirection targets fail closed. Heredoc bodies are data and are never reinterpreted as shell commands.

Analysis is bounded. Syntax errors, missing or mismatched analysis, unsupported constructs, dynamic executable identities, dynamic shell source such as unresolved `eval` or `source`, and exceeded limits cannot produce automatic `allow`. Depending on the authorization path, they are denied or surfaced for explicit permission; Bcode never falls back to raw command splitting.

A parser error means Bcode could not safely understand the shell program. A complete parse followed by denial may instead mean no configured command wildcard allows that executable subject. Add a narrow policy rule only after verifying the reported subject; parser correctness does not imply policy expansion.

#### Canonical command candidates

Bcode retains the exact original command subject and may add narrowly reviewed aliases. Alias candidates participate in normal specificity matching and cannot erase a more specific restrictive match against the original subject.

The initial Git aliases remove only reviewed behavior-neutral global options before a subcommand: `--no-pager`, `--no-replace-objects`, `--literal-pathspecs`, `--no-optional-locks`, and explicit `--color=...`. Bcode does not remove behavior-changing options such as `-C`, `-c`, `--config-env`, `--git-dir`, `--work-tree`, `--exec-path`, or `--namespace`. Assignment-prefixed commands currently receive no alias, including `PATH=...`, pager, and color assignments.

Rules that allow command runners or arbitrary execution remain powerful even when parsing is correct. Review broad rules for `python -c *`, `python3 -c *`, `cargo run *`, `go run *`, `find *`, `xargs *`, `curl *`, and `timeout *` carefully: each can execute arbitrary or mutating behavior. Bcode does not add implicit code-level allows for missing entries such as `printf *` or `command -v *`.

## Tool enablement

`[agent.<id>.tools]` maps exact model-callable tool IDs to booleans. Disabling a tool short-circuits the category rules with a hard `deny`. Enabling a tool only lets it run if the category rules also permit it. Removed shorthand IDs such as `bash`, `command`, `read`, `write`, or `edit` are rejected; use exact IDs like `shell.run`, `filesystem.read`, `filesystem.write`, and `filesystem.edit`.

Setting `tools = { "filesystem.write" = false, "filesystem.edit" = false }` additionally triggers the shell hard-deny for common file-writing commands (`>`, `tee`, `touch`, `cp`, `mv`, `rm`, `mkdir`, `sed -i`, etc.) in `shell.run`, so plan-style agents can't bypass the write restriction through shell commands.

## Built-in defaults

When no `[agent.*]` sections exist in `bcode.toml`, Bcode falls back to built-in defaults:

| Agent   | `command` permission rules                                                                                    | `read` | `write`           | `edit`            | `external_directory` |
|---------|-------------------------------------------------------------------------------------------------------------|--------|-------------------|-------------------|----------------------|
| `plan`  | `* = deny`, plus `allow`: `cargo check *`, `cargo test *`, `git diff *`, `git status *`, `ls *`, `rg *`     | allow  | deny (tool off)   | deny (tool off)   | allow                |
| `build` | `* = ask`                                                                                                   | allow  | unmatched → ask   | unmatched → ask   | allow                |

Any single `[agent.<id>]` section in `bcode.toml` replaces the full built-in set: define both plan and build explicitly if you want to customize one without losing the other.

## Custom agents

Any `[agent.<id>]` you declare is usable via `/agent <id>` in the TUI and the CLI. If no agent-profile plugin registers the ID, Bcode logs a warning at startup (`agent defined in bcode.toml but not registered by any agent-profile plugin`), and the agent won't appear in agent pickers, but policy evaluation and `/agent <id>` switching still work.

### Authorization fact schema migration

Shell authorization facts use schema version 2 because command operations now carry owner-produced structured analysis. Version 1 facts are rejected explicitly as unsupported; they never fall back to legacy command splitting. Static and dynamic bundled shell plugins must be rebuilt together with the host so they emit schema version 2. There is no compatibility window for authorizing version 1 shell facts.

Existing declarative command wildcard syntax does not need migration. Runtime rules that were remembered from raw full commands or broadened first words remain ordinary wildcard rules, but users should review and remove misleading entries. New prompts identify the executable subject; broaden behavior deliberately with explicit command rules rather than relying on old first-word derivation.

## Runtime rule persistence


The TUI permission prompt offers an "always allow" / "always deny" action that writes a rule into a **runtime state file**, never into `bcode.toml`. This means Nix / home-manager users (and anyone with a read-only declarative config) can still click "always allow" without their config being touched.

The state file lives at `$BCODE_PERMISSIONS_STATE` if set, otherwise `$BCODE_STATE_DIR/permissions.toml`, otherwise `$XDG_STATE_HOME/bcode/permissions.toml`, otherwise `$HOME/.local/state/bcode/permissions.toml`. It uses the same `[agent.<id>.permission.<category>]` schema as `bcode.toml`, so rules can be promoted to declarative config by copying entries verbatim.

At load time, the state file is merged **on top of** `bcode.toml` per `(agent, category, pattern)`. State entries win over same-pattern declarative entries. Patterns present only in declarative config survive untouched.

The rule is scoped to the currently selected agent, with category inferred from the tool:

* `shell.run` → `command`. Persists the relevant executable subject shown in the permission prompt. Broaden command permissions deliberately with an explicit wildcard rule; Bcode does not derive remembered rules from assignment prefixes, shell keywords, dynamic executable names, or malformed raw fragments.
* `filesystem.write` → `write` (literal path).
* `filesystem.edit`  → `edit` (literal path).
* `filesystem.{read,list,find,grep,stat,exists}` → `read` (literal path).

Filesystem paths are persisted literally by default because implicit directory globs can grant unintended access. To broaden a persisted path rule, edit the state file (or `bcode.toml`) directly — for example, replace `"src/foo.rs" = "allow"` with `"src/**" = "allow"`.

The CLI equivalent also writes to the state file:

```sh
bcode permission add --agent build --category command --pattern 'cargo *' --action allow
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
