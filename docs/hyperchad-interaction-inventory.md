# HyperChad Application Interaction Inventory

This inventory classifies the current Bcode HyperChad application paths by canonical framework mechanism. It is an implementation aid for `local-web-renderer-progress.md`, not a separate product architecture.

## Governing Rule

Bcode declares presentation and user intent through public HyperChad templates, routes, forms, actions, and renderer APIs. Selected HyperChad backends own transport, event capture, and view application. Bcode must not add a parallel browser or native runtime.

Canonical `hx-*` and `fx-*` declarations are both valid HyperChad APIs. Choose the operation matching the semantic interaction; do not avoid an API based on how one backend realizes it.

## Current Paths

| Workflow | HyperChad declaration | Host handling | Authoritative effect/update | Classification |
| --- | --- | --- | --- | --- |
| Initial application load | `Router::with_route("/")` | Authorize request, load bounded session state | Render a complete `Containers` tree | Route/render |
| Select session | Canonical HyperChad `anchor href` navigation | `RoutePath::LiteralPrefix("/session/")` | Attach/hydrate selected session and render | Navigation/route |
| Older/newer history | `form` with `hx-post`, `hx-target`, `hx-swap` | Parse `HistoryWindowForm` and load a source-anchored projection window | Render authoritative snapshot | Form/route/render |
| Update composer draft | `textarea` with `hx-post` and `hx-trigger="change"` | Parse `UpdateDraftForm` | `SessionViewAction::UpdateDraft`, then render authoritative state | Control change/form/route |
| Submit message | `form` with `hx-post`, `hx-target`, `hx-swap` | Parse `PromptForm` | `SessionViewAction::SubmitMessage`, then daemon event/render | Form/route/domain action |
| Cancel turn | `form` with `hx-post`, `hx-target`, `hx-swap` | Parse `CancelTurnForm` | `SessionViewAction::CancelTurn`, then render | Form/route/domain action |
| Resolve one permission | `form` with `hx-post`, `hx-target`, `hx-swap` | Parse `PermissionForm` | `SessionViewAction::ResolvePermission`, then render | Form/route/domain action |
| Resolve permission batch | `form` with `hx-post`, `hx-target`, `hx-swap` | Parse `PermissionBatchForm` | Resolve the authoritative pending batch, then render | Form/route/domain action |
| Interactive tool input | `form` controls with `hx-post`, `hx-target`, `hx-swap` | Parse `InteractionForm`, map to `InteractionInput` | Local plugin controller plus daemon interaction resolution, then render | Form/route/interaction action |
| Live daemon updates | None in presentation | Session watcher applies events and reconnect/resync rules | Generic `Renderer::render_scoped` with a complete HyperChad view | Authoritative renderer update |
| Local disclosure | Native HyperChad `details`/`summary` elements | No Bcode host effect | Renderer-owned disclosure state | Declarative presentation |
| Responsive layout | HyperChad container style/layout attributes | No Bcode host effect | Selected renderer lays out the view | Declarative presentation |

## Canonical API Findings

- Current primary mutations are naturally grouped form submissions and correctly use HyperChad's canonical form/routing attributes.
- Current live delivery correctly calls generic `Renderer::render_scoped`; HTML/Actix may realize it through SSE, but Bcode does not encode that transport.
- Current local disclosures use semantic `details` rather than Bcode-authored event code.
- No current Bcode workflow requires replacing canonical `hx-*` declarations with `fx-*` solely for portability.
- `fx-*` should be introduced when a named application action or declarative local effect is clearer than a route/form operation, not as a blanket migration.
- Full scoped snapshots remain the correctness baseline. HyperChad fragments should be introduced only after stable component identities and measurements exist.

## Demonstrated Framework Question

Guarded route capability propagation remains unresolved at the portable presentation boundary. Action and navigation targets currently contain the launch capability so HTML/Actix routes can authorize requests. Bcode must not remove that guard, create custom transport code, or invent backend-specific fallbacks. Resolve this through an existing canonical HyperChad participant/action context or a generic framework addition in `../../MoosicBox` before removing the capability from UI inputs.
