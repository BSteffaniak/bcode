# Type-enforced IPC request routing

## Problem

`Request` is a flat 163-variant enum. `packages/server` dispatches it through nine `async fn`
dispatchers chained by fall-through: each handles some variants and forwards the rest to the next.

That chaining caused a measured stack overflow. Dispatchers are `async fn`s, so each one's generated
future stays live for the whole call, and a request that fell through five dispatchers kept all five
frames — including a 751 KB one — on the stack simultaneously. Measured cost of one fall-through hop:
**1.24 MiB** of a 2 MiB budget.

Direct routing fixed the overflow (debug 4096 → 2048 KiB) by classifying a request first and jumping
straight to its owning dispatcher. But that fix is **not enforced**, and the gap is structural:

* `request_domain` and the dispatchers are two independent lists of the same 163 variants.
* `request_domain` ends in `_ => RequestDomain::Remaining`, so a new variant silently falls through
  and reintroduces the regression with no compile error and no failing test.
* Seven fall-through arms (`request => Box::pin(next_dispatcher(..))`) still exist.
* Two `unreachable!()` panics stand in for a guarantee the type system should provide, turning a
  routing mistake into a production panic rather than a build failure.

The gap is already load-bearing: `AuthPoolList` and `SetAuthPoolPreference` reach their handler only
through the wildcard.

## Target design

One consuming partition replaces the classifier and every fall-through arm:

```rust
enum RoutedRequest {
    SessionLifecycle(SessionLifecycleRequest),
    SessionSearchAttach(SessionSearchAttachRequest),
    SessionTurn(SessionTurnRequest),
    WorkflowMutation(Box<WorkflowMutationRequest>),
    WorkflowAuthoring(WorkflowAuthoringRequest),
    WorkflowDefinition(Box<WorkflowDefinitionRequest>),
    RuntimeAndModel(Box<RuntimeAndModelRequest>),
    PermissionInteraction(PermissionInteractionRequest),
    AgentSkillPlugin(AgentSkillPluginRequest),
}

impl Request {
    /// Bind a request to the domain that owns it.
    ///
    /// Deliberately has no wildcard arm: adding a `Request` variant fails to compile until it is
    /// placed in a domain.
    fn into_routed(self) -> RoutedRequest { /* all 163 variants */ }
}
```

Each dispatcher takes its own domain type rather than `Request`.

| Property | Enforced by |
| --- | --- |
| A new variant cannot be forgotten | exhaustive `match` with no wildcard: build error |
| A dispatcher cannot fall through | it holds a domain type, so it has no `Request` to forward |
| Mis-routing cannot panic at runtime | unrepresentable, so both `unreachable!()` arms are deleted |
| Partition and dispatch cannot drift | they are the same `match` |

Wire format is unchanged. `Request` stays flat, so this is internal routing only: no protocol change
and no call sites outside `packages/server`.

## Derived partition

`scripts/derive-request-domains.py` reads the dispatchers -- the authoritative source, since a
dispatcher's arms are what really handle a variant -- and emits the partition. All 163 variants are
classified with no duplicates and no residual wildcard:

| Domain | Variants |
| --- | --- |
| SessionLifecycle | 36 |
| RuntimeAndModel | 31 |
| WorkflowMutation | 21 |
| WorkflowDefinition | 19 |
| SessionSearchAttach | 18 |
| AgentSkillPlugin | 14 |
| WorkflowAuthoring | 10 |
| SessionTurn | 9 |
| PermissionInteraction | 5 |

Domain names describe what each dispatcher handles rather than reusing the incidental names from how
the dispatchers were originally split; `RuntimeAndModel` in particular had accumulated runtime-work,
model-selection, and auth-pool variants.

The five permission variants that appear in two dispatchers are correctly owned by the child
dispatcher; the parent's forwarding arm disappears with the fall-through chain.

## Phases

1. Declare the nine domain enums, moving variant blocks verbatim so field attributes and doc
   comments survive.
2. Write `into_routed` with no wildcard. This is the enforcement point.
3. Convert each dispatcher to take its domain type; delete the seven fall-through arms and both
   `unreachable!()` arms.
4. Rewire `handle_request_inner` to `match request.into_routed()`; delete `request_domain` and
   `RequestDomain`.
5. Measure with `scripts/measure-dispatch-stack.sh` in both profiles. Gate: debug must stay
   <= 2048 KiB and release <= 256 KiB. Stop and report on regression rather than continuing.
6. Validate: `cargo fmt`; `cargo clippy --workspace --all-targets -- -D warnings`;
   `cargo test -p bcode_server --lib` (476 baseline); workspace check; session, workflow,
   loop-runtime, context-accounting, and no-full-scans guards.

## Risks

The main risk is mis-partitioning while moving variants. The compiler checks both directions:
`into_routed` will not compile if a variant is unplaced, and a dispatcher's `match` will not compile
if it receives a variant it does not handle.

Boxing the three large workflow/runtime domains keeps `RoutedRequest` small; matching through a box
is slightly awkward but confined to those dispatchers.
