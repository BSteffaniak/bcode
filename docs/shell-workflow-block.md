# Shell workflow block

The shell plugin owns `shell.command-plan` through `bcode.workflow-block/v1`.

Version 1 plans use **argv mode only**. Each command is an ordered non-empty array whose first item
is the executable and remaining items are exact arguments. The owner never implicitly reparses an
argv command as a shell string.

A later version adds bounded unique `accepted_exit_codes` to each command, defaulting to `[0]`, plus
`continue_on_unaccepted_exit`. Existing version-1 plans retain their current zero-exit behavior.
Unaccepted ordinary exits remain typed result data distinct from spawn failure, timeout, signal
termination, and cancellation. Results expose both actual and accepted codes so deterministic
workflow predicates can route without parsing presentation text. The complete contract is specified
in [`composable-coding-workflows.md`](composable-coding-workflows.md).

The manifest schema bounds the workspace-relative cwd, command and argument counts, per-command
timeouts, environment entries, and retained output previews. The result preserves command order by
index and reports ordinary nonzero exits as typed result data rather than transport failures.
Oversized output is represented by typed artifact references.

Explicit `environment.set` values are persisted as part of exact approval/dispatch identity and must
therefore be non-secret. Names containing common secret-bearing markers (`TOKEN`, `SECRET`,
`PASSWORD`, `API_KEY`, `PRIVATE_KEY`, `ACCESS_KEY`, `AUTH`, `CREDENTIAL`, or `COOKIE`) are
rejected. Secrets must enter through an owner/runtime secret-injection facility that does not
serialize them into the workflow plan, intent, trace, or output.

The block is declared mutating because arbitrary commands can modify the workspace. It therefore
requires an exact workflow grant, uses repair-required reconciliation for ambiguous accepted work,
and claims repository write access. The owner resolves workspace-relative cwd against the canonical
workflow workspace, executes exact argv commands sequentially, applies explicit inherited/cleared
environment policy, enforces per-command timeout and cancellation, and reports ordinary nonzero
exits as typed data. Bounded previews include truncation flags; when artifact spill is enabled,
truncated complete streams are written through the invocation-scoped host artifact bridge. The
workflow runtime persists prepared intent before invoking the owner and records owner acceptance
before terminal observation, so an ambiguous accepted command is never automatically replayed.
