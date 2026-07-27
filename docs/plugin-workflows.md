# Plugin-authored durable workflows

The host validates every compiled definition against [`WorkflowProductionCapabilities`] before
persistence or start. This production contract is intentionally narrower than the in-process SDK
surface. The current production host accepts `Agent`, `Branch`, `Repeat`, wait-all `Parallel`,
`PluginBlock`, `Input`, and `Approval`. Closure-backed `Task` is explicitly in-process-only;
`Retry`, `FanOut`, retry edges, and fail-fast parallel joins are rejected until their complete
durable implementations ship. Versioned bounded edge transforms are supported, including immutable
run-state and canonical parallel `join.left`/`join.right` sources.

Registration and start both rerun production admission. Plugin block nodes must resolve to one
byte-equivalent enabled manifest declaration, so a definition cannot be persisted or started with
an ownerless or stale block contract.

Use typed workflow composition for domain behavior and let the host own durable registration,
execution, discovery, and lifecycle state.

```rust
let workflow = WorkflowBuilder::new(
    "review",
    Step::map("review", |input: ReviewInput| review(input)),
)
.build()?;
let spec = WorkflowSpec::new("code-review.review", &workflow)?;
let session_id = /* active persisted session */;
let binding = PluginWorkflowBinding {
    owner_plugin_id: "bcode.code-review".into(),
    workflow_kind: "code-review.review".into(),
    scope_key: session_id.to_string(),
    display_label: Some("Code review".into()),
    single_active: true,
};
let request = PluginWorkflowStartRequest::typed(
    &spec,
    &input,
    session_id,
    binding.clone(),
    Some(stable_retry_id),
)?;
host.start_workflow(request).await?;
```

Rules:

* Keep per-run values in typed input.
* Let `WorkflowSpec` derive the exact content-addressed definition identity.
* Use the logical workflow kind for product vocabulary and durable binding.
* Use a caller-stable run ID when retrying after an uncertain start response.
* Use `single_active` only when the owner/kind/scope permits one non-terminal run.
* Find, inspect, pause, resume, or cancel through `binding.lookup()` and generic host methods.
* Do not persist workflow attempts, scheduling state, run correlation, or recovery journals in plugins.
* Keep product UI and commands plugin-owned; keep execution and reconciliation host-owned.
