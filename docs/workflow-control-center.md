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

## Discover and launch

`/workflow` opens in **Discover**. Discovery is a bounded read-only application operation shared
with `bcode workflow package discover`; neither the CLI nor terminal renderer owns filesystem
scanning policy. The catalog contains configured package exports, standalone
`*.workflow.{json,yaml,yml,toml}` files that are not package members, and enabled plugin templates.

Repository roots are checked in deterministic precedence order (`.bcode/workflows`, then
`workflows`, followed by configured repository and user roots). Equal-precedence ambiguity and
malformed, escaped, unsupported, missing, unpublished, or drifted sources are shown as explicit
non-runnable states rather than guessed. Files outside automatic roots remain available only through
the explicit path inspection/import contract; automatic discovery never expands into a repository-wide
recursive scan.

Use `/` to search, `f` to cycle source kind, `r` to cycle readiness, `m` to request the next bounded
page, and `R` to refresh. Selecting a row loads exact portable detail: provenance and package/export
identity, diagnostics, requirements, effects and permission preview, input/configuration schemas,
and semantic definition nodes and edges. Ready package exports and templates use `s` to open the
retained schema-driven configuration form. Local validation is advisory; canonical application
validation remains authoritative, and rejected values stay available for correction. Discovery never
silently applies, publishes, starts, repairs, or grants authority.

## Execution graph and node activity

The Runs workspace adapts renderer-neutral workflow nodes and edges into TUI-owned deterministic
layout, viewport, connector geometry, hit regions, and a narrow terminal tree fallback. Geometry is
ephemeral presentation state and never changes dispatch, authorization, or persisted outcomes.

Selecting a node retains its exact run, node, activation, attempt, and child-session identities. The
Sessions inspector subscribes through the bounded `SessionViewSnapshot` host contract and replaces
state only for the selected session at a non-regressive revision. It never decodes raw session events
or copies transcripts into workflow storage. Activity, Transcript, Tools, Permissions, Outputs, and
Attempts remain renderer-neutral semantic views; tool rows correlate exact tool-call IDs with their
session-owned permission checkpoints. `o` opens the exact full child session when deeper interaction
is needed. Workflow actions and session permission decisions remain separate canonical operations.

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
