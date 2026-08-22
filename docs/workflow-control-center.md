# Workflow Operations Workspace

Open the plugin-owned workflow operations workspace with `/workflow`.

The surface subscribes to durable workflow notifications and performs bounded semantic refetches. It presents:

* a paginated run catalog with search, filter, sort, and grouping controls;
* bounded canonical failure diagnostics for failed runs and attempts;
* a responsive execution graph and tabbed inspector for the selected run;
* exact selected inputs, approvals, attempts, outputs, child sessions, and definition provenance;
* contextual enabled and disabled actions with their exact visible targets;
* explicit loading, stale, degraded, disconnected, resynchronizing, and repair-required states.

Wide terminals use Runs, Graph, and Inspector panes. Medium terminals combine the run catalog with tabbed detail. Narrow terminals expose explicit Runs, Graph, Inspector, and Actions pages.

## Navigation

Keyboard navigation is scoped to the focused pane. `Tab` changes pane focus; clicking a pane focuses
it. Interactive tables, tabs, action buttons, and scrollable selections use BMUX component keyboard
and mouse handling.

* In Runs, Up / Down or `k` / `j` selects the previous or next run.
* In Graph, Up / Down or `k` / `j` selects a graph node.
* In Inspector, Left / Right changes sections and Up / Down selects an exact input, output, attempt,
  approval, or child session.
* In Actions, Left / Right changes the active action and Enter activates it.
* Mouse clicks focus panes and select run rows, inspector tabs, narrow-layout pages, and actions;
  wheel input moves the focused run, graph, or inspector selection.
* `Tab`: move workspace focus; on narrow terminals it also advances the active page.
* `1` through `4`: open the narrow Runs, Graph, Inspector, or Actions page.
* `[` / `]`: select the previous or next Inspector section.
* `n`: expand or collapse nested workflows and child sessions.
* `/`: edit bounded catalog search.
* `f` / `s` / `g`: cycle catalog filter, sort, or grouping.
* `m`: load the next bounded catalog page when available.

Selection uses stable workflow identities. Live catalog reorder does not silently retarget the selected run, node, wait, approval, attempt, output, or child session.

Failed runs show bounded canonical diagnostics in Overview and associate recorded failure messages
with exact attempts when dispatch or activation identity is available. Diagnostics come from the
server-owned workflow event history; the TUI does not reinterpret raw workflow events.

## Exact workflow actions

The Actions pane shows available shortcuts and disabled explanations from the portable workflow projection. The application revalidates every operation against canonical state before mutation.

* `p`: pause or resume the exact selected run.
* `c`: confirm cancellation of the exact selected run.
* `i`: open the structured input form for the exact selected input wait.
* `a` / `d`: approve or deny the exact selected ordinary or mutation approval.
* `r`: retry the exact selected failed attempt.
* `o`: open the exact selected child session.
* `V`: view the selected run's exact immutable definition revision.
* `E`: edit its existing draft when available.
* `F`: fork an immutable published revision into the authoring surface.
* `D` / `T` / `N`: open Definitions, Templates, or New Workflow in the plugin-owned authoring surface.
* `q` or Escape: close the workspace when no dialog is active.

Destructive and reconciliation-sensitive actions use explicit confirmation. A submitted action remains pending until an authoritative projection refresh confirms the resulting state. Stale or ambiguous targets fail closed.

## Typed input

The input dialog displays the exact run, node, and activation, plus its bounded prompt and expected schema. Supported simple object schemas use field controls. Complex schemas retain a JSON editor fallback. Local validation is advisory; canonical server validation remains authoritative, and rejected input stays available for correction.

## Scriptable typed outputs

The independently supported CLI can read canonical checksum-verified node outputs:

```sh
bcode workflow run-output --run-id <RUN_ID> --limit 100
```

The response includes node and activation identity, schema ID/version, checksum, artifact reference when present, and the validated JSON value. Reviewer verdicts and findings therefore do not require reading child-session event history.
