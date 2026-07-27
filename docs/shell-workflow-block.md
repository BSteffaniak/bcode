# Shell workflow block

The shell plugin owns `shell.command-plan` through `bcode.workflow-block/v1`.

Version 1 plans use **argv mode only**. Each command is an ordered non-empty array whose first item
is the executable and remaining items are exact arguments. The owner never implicitly reparses an
argv command as a shell string.

The manifest schema bounds the workspace-relative cwd, command and argument counts, per-command
timeouts, environment entries, and retained output previews. The result preserves command order by
index and reports ordinary nonzero exits as typed result data rather than transport failures.
Oversized output is represented by typed artifact references.

The block is declared mutating because arbitrary commands can modify the workspace. It therefore
requires an exact workflow grant, uses repair-required reconciliation for ambiguous accepted work,
and claims repository write access. The current owner validates this contract but returns
`not_implemented` before execution until durable acceptance receipts, result persistence, artifact
spill, and cancellation settlement are complete.
