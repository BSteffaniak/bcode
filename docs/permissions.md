# Permissions

Bcode uses an agent-scoped permission model with three verbs: `allow`, `ask`, and `deny`. Declarative rules live in `bcode.toml` under `[agent.<agent_id>]` sections. Runtime rules written by the TUI "always allow" prompt live in a separate state file (see [Runtime rule persistence](#runtime-rule-persistence) below), so a read-only declarative config (for example on NixOS / home-manager) is never mutated.

## Per-launch execution modes

Bcode provides two mutually exclusive CLI modes for turns admitted by the current launch:

```text
bcode --dangerously-bypass-all-permissions  # alias: --yolo
bcode --disable-all-tools                   # alias: --no-tools
```

The flags work with the default TUI, `--new`, `tui`, and `send`. They are rejected by commands that do not admit ordinary agent turns. They do not modify `bcode.toml`, runtime permission state, or a permanent session setting; relaunching without a flag restores normal behavior for future turns.

`--dangerously-bypass-all-permissions` allows otherwise valid, available tool operations without evaluating configured agent or skill permission rules and without prompting. It can enable destructive commands, data loss, secret exposure, repository modification, and arbitrary effects from enabled tools. Canonical operation validation, tool and plugin availability, read-only or disabled turn/workflow ceilings, path confinement, provider authentication, and operating-system privileges still apply.

`--disable-all-tools` exposes no agent tools to the model and rejects a tool call if a provider emits one anyway. Model generation, session context, project instructions, and non-tool application behavior remain available, so this mode is intentionally not described as hermetic.

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

* `command` — legacy Pi/OpenCode-style text globs matched against each shell command subject.
* `command_patterns` — word patterns matched against parsed, static shell words. A rule such as `"git [--no-pager] diff ..." = "allow"` accepts the optional **literal** `--no-pager` word and any number of remaining arguments; `"git stash ..." = "deny"` covers both `git stash` and its subcommands. `...` must be final and matches zero or more words. Quotes around a word do not change the matched word. No option is removed or interpreted by Bcode.

`command_patterns` is separate from `command`: existing globs retain their original meaning. A matching word pattern wins over legacy globs unless an exact legacy deny matches; the exact deny remains effective. A word-pattern allow can refine a legacy `* = deny`, and a word-pattern deny can refine `* = allow`. If no word pattern matches, legacy glob resolution applies unchanged. Among word patterns, `deny > ask > allow` wins when multiple patterns match; within the same action, more required literal words win, then more optional words, then lexical order. An invalid word pattern fails shell authorization closed, even if a legacy `* = allow` would otherwise match. The executable must be static and no assignment prefix may be present for a word pattern to match. Uncertain argument words cannot satisfy a word-pattern allow; if they could match a word-pattern deny once expanded, authorization denies rather than falling through to `* = allow`. Normal analysis completeness checks still apply.

For example, add this under `[agent.plan.permission.command_patterns]` to allow a reviewed no-pager form without a Git-specific parser exception:

```toml
"git [--no-pager] diff ..." = "allow"
```

`command_patterns` does **not** inspect arbitrary interpreter scripts (`sh -c`, `python -c`) or infer program-specific options. Such commands require separate policy decisions; matching a wrapper does not authorize the enclosed operation automatically.

* `read` — path globs for read-only filesystem tools (`filesystem.read`, `filesystem.list`, `filesystem.find`, `filesystem.grep`, `filesystem.stat`, `filesystem.exists`).
* `write` — path globs for `filesystem.write`.
* `edit` — path globs for `filesystem.edit`.
* `external_directory` — a single action governing any tool argument that resolves outside the session working directory. This short-circuits before path-category matching: if a write path resolves outside `cwd` and `external_directory = "deny"`, the call is denied even when a more permissive `write` rule would match.

## Workflow coordination defaults

The bundled build agent preauthorizes the exact execution-scoped workflow staging and
publication tools. This removes the redundant tool prompt, not application authorization:
active execution identity, current run ownership, exact retained candidates, reconciliation,
and the run capability ceiling are still checked. Workers' tool calls retain their own
permission checks. Plan-agent coordination tools remain disabled.

Generic mutating tools support exact-name overrides (no wildcard matching):

```toml
[agent.build.permission.tools]
"workflow.stage_task_group" = "ask"
"workflow.publish_run_graph_edit" = "deny"
```

These rules apply only to the generic mutating-operation fallback; they do not override
filesystem, shell, or web operation rules or disabled tools. Declarative and runtime-state
overlays retain their usual precedence. Unlisted mutating tools still ask by default.

## Reviewing pending requests

The TUI's `/permissions` command refreshes requests addressed to the viewed session and opens the normal approval/denial dialog. Requests are also available through the supported application CLI:

```sh
bcode permission list --session-id SESSION_ID --json
bcode permission approve PERMISSION_ID
bcode permission deny PERMISSION_ID
bcode permission resolve-batch BATCH_ID --approve
bcode permission resolve-batch BATCH_ID --deny
```

Use the matching executable, state root, and configuration context for the serving daemon. Approval resumes pending work; denial refuses it. `approve --remember` is supported only when the request advertises remember capability.

Permission registration fences list/resolve calls across canonical persistence and pending-map insertion. Canonical append may wake subscribers, but their fetch cannot return until registration completes. Failed persistence never exposes an approvable request. Plugin publication and routed invalidations happen after registration and outside the queue lock; notifications are invalidations, not execution authority. Refresh reconciles resolved dialogs, and fetch failures remain visible rather than silently leaving the UI waiting.

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

Environment assignment prefixes do not independently change a command's permission. `MODE=test git status` and `env MODE=test git status` match ordinary rules for `git status`, preserving allow, ask, and deny. The original source and parsed assignments remain available: explicit legacy globs such as `MODE=restricted *` or `env MODE=restricted *` still participate in normal rule precedence. Structured argument patterns match effective executable/argv values, not environment assignments. No new environment-specific schema is implied.

The recognized `env`, `/usr/bin/env`, and `/bin/env` forms support static assignments, `-i`/`--ignore-environment`, `-u NAME`/`--unset NAME`/`--unset=NAME`, and `--`. Unsupported options (including split-string execution) and unresolved dynamic `env` argv fail closed instead of hiding a utility behind the wrapper. `env` without a utility remains an environment-printing command. Dynamic executable/argument protections and redirection checks remain unchanged; substitutions in assignment values are evaluated as separate executable leaves. Variables such as `PATH`, loader options, and application configuration can alter behavior; restricting them requires explicit policy rather than an implicit ask for every assignment.

Input redirections with static paths are evaluated through `read` rules. Output, append, and read/write redirections with static paths are evaluated through `write` rules. Dynamic redirection targets fail closed. Heredoc bodies are data and are never reinterpreted as shell commands.

Analysis is bounded. Syntax errors, missing or mismatched analysis, unsupported constructs, dynamic executable identities, dynamic shell source such as unresolved `eval` or `source`, and exceeded limits cannot produce automatic `allow`. Depending on the authorization path, they are denied or surfaced for explicit permission; Bcode never falls back to raw command splitting.

A parser error means Bcode could not safely understand the shell program. A complete parse followed by denial may instead mean no configured command wildcard allows that executable subject. Add a narrow policy rule only after verifying the reported subject; parser correctness does not imply policy expansion.

#### Parsed argument patterns

`command_patterns` matches parsed argument **values**, not raw shell text or joined argv. Matching is whole-command and whole-value; quoting and empty arguments survive shell analysis. There are no tool-specific option catalogs or automatic basename/wrapper normalization.

```toml
[agent.build.permission.command_patterns]
"git [--no-pager] commit ..." = "deny"
"git [--no-pager | -C <arg> | -c <arg>]* commit ..." = "deny"
"git <options>* commit ..." = "deny"
'printf /hello world/i' = "ask"
```

* Literal values match exactly. Quote literals with single/double quotes or backslash-escape DSL punctuation; `''` matches an empty argument.
* `<arg>` matches exactly one argument. `[a | b c]` is an optional group of alternative sequences. A following `*` repeats whole branches, including zero occurrences. Groups do not nest.
* `...` matches zero or more remaining arguments and is legal only at the end. The first value must be a literal executable.
* `/expression/imsx` is a first-class whole-value Rust regex, usable wherever an argument predicate is accepted, including optional/repeated groups. Flags mean case-insensitive, multiline, dot-matches-newline, and ignore-pattern-whitespace. Unicode is enabled. Duplicate/unsupported regex flags, including `g`, are errors. This is not ECMAScript syntax. Escaped `/` and `/` inside a character class are not delimiters. Executable tokens remain literal; ordinary absolute paths such as `/usr/bin/git` and `/tmp/path` remain literals. To resolve slash-led argument collisions, `/tmp/` and `/tmp/i` select regex, `/tmp/g` rejects reserved `g`, while plain unknown suffixes such as `/tmp/z` remain paths. Explicit regex constructs with unknown flags are rejected. Quote ambiguous paths (`'/tmp/i'`) and slash-shaped literal values; escape internal regex slashes or put them in classes.

Separate option/value pairs are explicit sequences: `-C <arg>` consumes both words atomically. Thus `git -C commit status` does not match the precise repeated-group commit rule, but `git -C repo commit` does. Attached and equals spellings are separate single-value predicates, such as `/-C.+/` or `/--directory=.+/`; a pair does not imply either spelling. In particular this DSL makes no claim that Git accepts `-C=value`.

`<options>*` deliberately explores generic ambiguous interpretations. Each dash-led value other than lone `-` or `--` can be an option; an option without `=` may additionally consume one following non-dash-led argument (or lone `-`) as its value. Clusters, negative numbers, and attached values are simply option-looking values; inline `=value` stays in one word. A consumed `--` ends that options segment. Any complete matching interpretation counts; this **static, explicitly selected grammar ambiguity does not itself require approval**. No CLI-specific option semantics are inferred: false positives remain possible, including command-looking option values. Use precise pairs or ordered exceptions when necessary. Dynamic shell expansion is different and cannot become an automatic allow through this ambiguity.

Pattern bytes, grammar size, regex compilation, argv size, and matching work are bounded. Invalid patterns fail closed rather than becoming legacy globs. Assignment prefixes preserve ordinary command matching. Only the documented `env` forms are normalized; `command`, aliases, arbitrary executable paths, scripts, and subprocesses are not transparently normalized. A matcher is not a sandbox.

#### Opt-in ordered command policy

Existing policies keep their old resolution and exact legacy-deny protection. To permit intentional carve-outs, opt into version 1 explicitly:

```toml
[agent.build.permission.command_policy]
version = 1

[[agent.build.permission.command_policy.layers]]
[agent.build.permission.command_policy.layers.command_patterns]
"git <options>* commit ..." = "deny"

[[agent.build.permission.command_policy.layers]]
[agent.build.permission.command_policy.layers.command_patterns]
"git -C commit status" = "allow"
```

Existing `permission.command` and `permission.command_patterns` form an implicit base layer. Each explicit layer contains `command` legacy globs and/or `command_patterns` parsed patterns. **Last matching layer wins for each individual command.** Within one layer, action precedence is `deny > ask > allow`, with deterministic specificity/pattern tie-breaking; map serialization order is never policy order. In this opt-in mode an exact base legacy deny is not an unoverrideable guard: a later explicit exception can replace it. Unknown versions and malformed patterns fail closed.

Layers do not override disabled tools, resource/path/external-directory/skill authorization, invalid or incomplete shell analysis, dynamic executable identity, or unresolved dynamic execution into automatic allow. Redirections retain their path rules. After leaf resolution, whole-program aggregation remains `deny > ask > allow`: `allowed; denied` cannot launder a denied sibling. Policy identities include the ordered configuration, so a pinned workflow does not silently acquire later exceptions. Deploy runtime support before emitting this configuration; old binaries must not be used to interpret advanced policies.

#### Compiled policy reuse

The permission evaluator retains compiled command policies in the daemon's plugin
process, keyed by the exact base command tables and ordered layers (including
versions and actions). Sessions with identical policies share compiled snapshots;
configuration or runtime permission changes select a new snapshot. Invalid
snapshots fail closed with the compilation reason rather than using an older
valid policy. Cache entries are disposable and least-recently-used snapshots are
evicted; eviction never rejects a policy or invalidates in-flight evaluations.

There is no aggregate rule-count, layer-count, pattern-byte, compiled-state, or
regex-count ceiling. Individual patterns and regexes retain complexity/memory
budgets. Static executable indexes select relevant argv rules for evaluation and
preview while preserving sorted configuration locators and rule precedence.
Dynamic executables and oversized subjects retain conservative deny handling.

#### Non-executing preview

```sh
bcode permission check --agent build --command 'git --no-pager commit -m example' --json
```

This read-only interface parses the supplied string and calls the agent-policy service; it never runs the command. It previews the effective **declarative** policy through the enforcement evaluator, not a separately implemented matcher. Output includes the decision, source spans, static/dynamic word metadata, assignment counts, and matched-rule locators. Layer 0 is the implicit base; subsequent indices identify explicit layers in list order. Rule indices refer to sorted keys within the named `command` or `command_patterns` map. Match entries identify broad rules, the winner, regex flags, and conservative dynamic handling without echoing argument or assignment values. A preview allow is not an execution grant: session-specific runtime rules, pinned workflow policy, skill restrictions, resources, and other authorization still apply when an operation is actually requested. Use synthetic values when checking sensitive commands.

For Nix-generated configurations, keep ordered lists and advanced strings intact for Bcode. OpenCode/Pi raw-glob backends cannot generally express regex, repetition, generic options, or layer carve-outs: the shared Nix generator requires explicit reviewed target fallbacks or rejects evaluation. Upgrade/build Bcode first, run non-executing checks and pure Nix tests, then separately review and authorize activation. No rebuild or switch is part of implementing these changes.

#### Canonical command candidates

Bcode retains the exact original command subject and may add narrowly reviewed aliases. Alias candidates participate in normal specificity matching and cannot erase a more specific restrictive match against the original subject.

The initial Git aliases remove only reviewed behavior-neutral global options before a subcommand: `--no-pager`, `--no-replace-objects`, `--literal-pathspecs`, `--no-optional-locks`, and explicit `--color=...`. Bcode does not remove behavior-changing options such as `-C`, `-c`, `--config-env`, `--git-dir`, `--work-tree`, `--exec-path`, or `--namespace`. Assignment-prefixed commands use the effective command matching described above; this does not add Git-specific option stripping.

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
