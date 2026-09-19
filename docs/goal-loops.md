# Goal setup for prompt loops

`/goal` opens a goal setup modal owned by the bundled loop plugin. Enter a goal;
additional guidance and maximum iterations are optional. A blank maximum uses the
existing loop default (20). No repository or conversation scan occurs during setup.

* **Ctrl+Enter** generates prompts and starts the loop after validation.
* **Ctrl+R** generates prompts for review. Edit the iteration prompt and stop
  condition, then use **Ctrl+Enter** to start.
* **Esc** closes setup. A late generation response cannot start a loop after closing.

Inputs are frozen while generation is pending. Failure preserves the draft for retry.
Generation uses the normal bounded, tool-free structured-generation application API.
The plugin bundles generation, iteration, and evaluation guidance in `prompts/`.
The original objective and additional guidance are included as task data in both
resulting prompts; executing agents inspect repository instructions and referenced
specifications through normal tools. Generated text cannot change permissions or
iteration limits. Normal model usage costs apply to generation.

Once started, this is an ordinary loop: the existing implementation turn and
read-only completion evaluator repeat under the existing durable workflow runtime.
There is no separate goal store, lifecycle, or scheduler. `/loop.status`,
`/loop.pause`, `/loop.resume`, `/loop.stop`, and `/loop.detach` work normally.
Equivalent `/goal.*` aliases control the same session loop, including loops started
with `/loop`. Detach is only the existing repair-required operation; it does not
resolve an ambiguous prior operation or mean background execution.

An existing active loop must be explicitly replaced through the usual confirmation;
it is not cancelled merely to generate prompts. Exhausting the iteration allowance
is not evidence that the goal was achieved. Generated stop conditions and evaluator
claims remain fallible and should be checked against current evidence.

Closing setup does not undo a workflow start already submitted to the host. Use
loop status and controls to inspect or stop admitted work. Live objective editing,
new resource budgets, automatic workflow authoring, and a durable goal object are
outside this feature.
