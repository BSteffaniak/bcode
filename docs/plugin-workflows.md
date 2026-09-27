# Plugin-authored durable workflows

The host validates every compiled definition against [`WorkflowProductionCapabilities`] before
persistence or start. This production contract is intentionally narrower than the in-process SDK
surface. The current production host accepts `Agent`, `Branch`, `Repeat`, wait-all and fail-fast
`Parallel`, `PluginBlock`, `Input`, and `Approval`. Closure-backed `Task` is explicitly
in-process-only; `Retry`, `FanOut`, and retry edges are rejected until their complete durable
implementations ship. Fail-fast persists a generation-scoped decision and exact sibling
cancellation intents before signaling runtime owners; unsignalled intents survive restart and
terminal sibling outcomes return through ordinary attempt observation. Explicit operator retry of a terminal failed activation remains a
separate bounded store operation; ambiguous mutation requires explicit repair before any later
attempt and is never automatic retry. Deterministic branch and repeat predicates use the current explicit versioned
contract and are bounded and validated during production admission. Versioned bounded edge
transforms are supported, including immutable run-state and canonical parallel `join.left`/`join.right`
sources.

Registration and start both rerun production admission. Plugin block nodes must resolve to one
byte-equivalent enabled manifest declaration, so a definition cannot be persisted or started with
an ownerless or stale block contract.

Durable prompt nodes use the versioned `WorkflowPromptConfiguration` contract. Skills are not
selected, required, or resolved by workflow contracts. Prompt text may ask the ordinary Bcode agent
to load a named skill through the normal skill catalog and tool boundary; missing or disabled skills
therefore remain ordinary prompt/tool outcomes and never invalidate a workflow definition or widen
authority. The prompt's explicit model, tool, timeout, context-target, and structured-output policy
remain authoritative.

### Execution ownership at wait resolution

Application input/approval resolution acquires verified run execution authority before mutation.
The workflow-store owned resolution methods compare artifact, daemon, generation and fencing token
inside the same immediate transaction that resolves the wait. Foreign or stale resolutions leave
waits untouched. This applies to explicit goal resume checkpoints as well as other workflow gates;
it does not grant approval authority or establish communication routes between independent runs.

### Explicit goal resume consent

`/goal.status` identifies a blocked loop approval checkpoint and shows its exact activation ID.
`/goal.unblock <activation-id> approve|deny` resolves that checkpoint through the existing workflow
application API. The activation ID is required so a delayed command cannot approve a later checkpoint.
Approval resumes from retained goal state; it does not approve the original tool permission or assert
that an external dependency is satisfied. Resolve that original request first. This is an explicit
user-controlled fallback, not automatic authenticated dependency resolution.

### Routed permission notifications

Turn execution-options version 7 adds an explicit version-1 `interaction_route` with a
`destination_session_id`. The application currently admits self routes and routes to the execution's
recorded parent association, not arbitrary caller-selected sessions. Workflow dispatch installs this
route explicitly; ordinary user turns have no route. It is restored with the admitted turn rather
than inferred from which clients happen to be attached.

Pending permission summaries retain the canonical source session and carry the destination separately.
TUI and web permission lists include requests addressed to the viewed session. Resolution uses the
original permission owner and existing authorization path; routing grants no approval authority and
never copies permission history into the destination. Live destination invalidations request a normal
snapshot refresh. Permission identities include the daemon instance to avoid reuse across restarts.

This is a same-daemon permission-notification implementation, **not** a durable execution messaging
transport. Pending decisions remain daemon-memory observations. Cross-daemon routing, arbitrary
execution recipients, explicit associations between independently launched runs, general typed
request/response delivery, durable acknowledgments/recovery, and dependency-driven blocked-loop
resumption are not implemented here. Existing runs are not retroactively associated or approved.

### User questions in background work

Prompt configuration version 4 adds `allow_user_questions` (default `true`). Goal agent stages
and delegated prompt/task-group workers set it to `false`. The host carries the setting into
persisted turn execution options version 6, filters tools declaring the plugin-owned `ask_user`
capability from model requests, and rejects their invocation before opening an exchange—even
when discretionary permissions are bypassed. Other tools and permission checks are unchanged.
The restriction is turn-scoped, so later interactive turns in a shared session remain interactive.
Agent-authored graph edits from a restricted agent must also disable questions on added or
replaced agent nodes; violations reject the edit rather than silently rewriting its identity.

Version 2/3 prompt configurations and older turn options retain interactive defaults. A restricted
configuration cannot claim an older contract version, and older executors reject the new versions.
Retries use the persisted setting. Existing pending questions are not answered or cancelled by
this change.

Tool discovery uses argument-independent `ToolList.discovery` metadata (policy version 1),
provided by each tool-owning plugin. It never prepares a synthetic null-argument invocation.
Missing or unsupported discovery policies exclude tools from restricted catalogs; ordinary
unrestricted catalogs retain compatibility with older plugins. Rebuild bundled tool plugins with
the host to supply these policies. Discovery metadata does not authorize execution: real argument
preparation and canonical authorization, including the question restriction, still run at dispatch.

Use typed workflow composition for domain behavior and let the host own durable registration,
execution, discovery, and lifecycle state.

## Structured loop blockers

New loop/goal definitions evaluate an `external_blocker` enum (`none`,
`approval_required`, `input_required`, `dependency_required`). A non-`none` result
parks at the durable `loop.blocked` approval gate before judgement or repeat, including
when a contradictory completion flag is supplied. Scheduler passes and reopening
the store do not spend another model turn or iteration. The normal authorized
workflow approval operation addresses the exact run/node/activation; conflicting
stale resolutions cannot change the accepted outcome. Approval means explicit consent
to resume the goal, not approval of the underlying tool request or evidence that a
dependency resolved. Denial fails the run without scheduling continuation. The gate
forwards retained activation input; replacement goal-state input is rejected. Outgoing
transforms restore the admitted objective, iteration limit and optional judgement policy
from immutable run input, preserve retained iteration/evidence, clear the reported
blocker and force completion false. A resume cannot attest successful completion.
Existing persisted definitions retain their original input-gate semantics; this change
applies to newly authored definitions and does not migrate or resolve existing waits.

This is a reported blocker, not an authenticated dependency reference or permission
approval. Automatic correlated dependency resolution, independent-run linkage and
cross-daemon delivery remain unfinished. A permission decision does not itself
resolve this gate. Existing persisted definitions are not rewritten. The judgement
block contract is version 2 with the additional blocker field; old block declarations
must fail compatibility checks rather than silently run under the new declaration.

## Loop iteration budgets

Plugin start requests carry renderer-neutral `WorkflowRunLimitPolicy` values; adapters must
preserve these rather than replacing them with host defaults. `/loop` sets the cycle budget to
the user's positive `max_iterations` and derives its node-attempt budget from both agent nodes
and the permitted attempts per activation. There is no separate 100-cycle or 1,000-iteration
ceiling. The iteration contract supports `1..=u32::MAX`; out-of-range input is rejected explicitly.
Node-attempt budgets use 64-bit integers (within SQLite's signed integer range), so supporting
the full iteration range does not overflow a smaller execution budget. Existing stored integer
budgets retain their values and semantics; existing runs are not rewritten. Older clients that
cannot represent a wider budget reject it rather than truncate it. Native plugin ABI version 4
covers the changed workflow-start request layout and widened budget type; ABI-3 binaries are
rejected. Rebuild native plugins with the matching SDK when updating the host.

Stop conditions, explicit cancellation, retry limits, per-agent timeouts, and execution ownership
checks still apply. These changes affect newly started loops, not budgets of already-running ones.

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

* Carry evolving/original context explicitly with `WorkflowStateEnvelope<State, Value>` when a node
  accepts or returns a narrower value.
* Put large retained values in the envelope's typed `artifacts` references rather than copying bytes
  inline.
* Keep per-run values in typed input.
* Let `WorkflowSpec` derive the exact content-addressed definition identity.
* Use the logical workflow kind for product vocabulary and durable binding.
* Use a caller-stable run ID when retrying after an uncertain start response.
* Use `single_active` only when the owner/kind/scope permits one non-terminal run.
* Find, inspect, pause, resume, or cancel through `binding.lookup()` and generic host methods.
* Do not persist workflow attempts, scheduling state, run correlation, or recovery journals in plugins.
* Keep product UI and commands plugin-owned; keep execution and reconciliation host-owned.
