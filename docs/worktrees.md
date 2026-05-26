# Worktrees

Bcode can create and manage Git worktrees as isolated workspaces for sessions and agent work.

## Defaults

By default, Bcode creates worktrees under the repository root:

```text
.bcode/worktrees/<slug>
```

New branches default to:

```text
bcode/<slug>
```

The default base ref strategy is `auto`:

* New standalone/new-session worktrees prefer the repository default branch when it can be resolved.
* Existing-session flows can choose `head` when the intent is to fork current local work.
* If default branch resolution fails, Bcode falls back to the current checkout.

## Configuration

Bcode reads repo-local config from both:

```text
<repo>/bcode.toml
<repo>/.bcode/bcode.toml
```

These are merged with global config, environment config, and CLI overrides. Later layers win.

```toml
[worktree]
root = ".bcode/worktrees"
branch_prefix = "bcode/"
base_ref = "auto" # auto | default_branch | head

[worktree.setup]
enabled = true
```

Relative `worktree.root` values resolve from the repository root.

## CLI

```bash
bcode worktree list
bcode worktree create my-task
bcode worktree create my-task --session <session-id>
bcode worktree create my-task --new-session
bcode -n --worktree my-task
bcode worktree attach <session-id> <path>
bcode worktree remove <path>
```

## TUI

The command palette includes:

* `Worktree: List`
* `Worktree: Create for Current Session`
* `Worktree: Attach Current Session`
* `Worktree: Remove`

Slash commands are also available:

```text
/worktree
/worktree list
/worktree create my-task
/worktree attach ../path
```

If `worktree.config.toml` exists, Bcode applies worktree setup automatically after creation.
The setup engine is integrated into Bcode; user-facing output remains Bcode worktree output.

## Agent tools

The bundled worktree plugin exposes:

* `worktree.list`
* `worktree.create`
* `worktree.remove`

Create/remove run through Bcode's permission model because they mutate Git/filesystem state and may run setup commands.
