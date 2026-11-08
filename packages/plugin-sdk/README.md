# Bcode product adapters on the shared plugin runtime

`definition` adapts existing `RustPlugin` and `ConcurrentRustPlugin` implementations
into BMUX `PluginDefinition` factories. This is a migration adapter, not a loader,
ABI, executor or activation-admission implementation. Instances are fresh for each
factory call; static registration must retain the definition rather than a global
implementation singleton. Thin dynamic packaging uses BMUX's export macro, not
Bcode's legacy export macros.

## Author declaration

The common forms derive identity/version/service contracts from the existing
manifest (no repeated metadata struct):

```rust,ignore
bcode_plugin_sdk::define_plugin!(serial MyPlugin, MANIFEST);
bcode_plugin_sdk::define_plugin!(concurrent MyConcurrentPlugin, MANIFEST);
```

Both call `RustPlugin::component_registrations()` by default. Override that inert
hook to return real CLI/frontend factories; the macro also accepts
`registrations = ...` and `host = ...`. Explicit host injection uses the same
product factory as static and dynamic packaging. Without a custom resolver the
factory uses `SharedInvocationHost` with the live shared invocation service port,
not a permanently missing-host implementation. Do not infer native function
pointers from TOML or hardcode plugin identities in the SDK.

```rust,ignore
bcode_plugin_sdk::define_plugin!(concurrent MyPlugin, metadata = metadata());
```

The generated `plugin_definition()` is inert and export-free. Metadata declares
backend/frontend roles and product contracts. The typed service provider supports
existing product requests and responses without encoding them again. Resolve
`dyn BcodePluginService` from the shared typed registry using capability
`bcode.product`, kind `Command`, interface `bcode.product-plugin/v1`. All operations
still carry their original interface ID; this registry identity is only the
migration adapter's native provider identity, not authorization for arbitrary
services. Host authorization and admission must precede native dispatch too.

Exclusive implementations serialize independent callers on the implementation
lock. A nested entry from a call that already holds that lock reports
`plugin_busy` instead of waiting on its own parent.
Use the concurrent adapter for nested service behavior; do not treat this temporary
legacy adaptation as a new runtime concurrency policy.

## Host binding

Portable operations retain the service's existing payload bytes; configuration,
credentials, workspace grants and executable callbacks are **not** accepted from
those bytes. Construct a definition through either `_with_host` function (or the
`host = ...` macro argument) with a per-instance resolver:

```rust,ignore
concurrent_plugin_definition_with_host::<MyPlugin>(metadata, |scope| {
    product_host.for_instance(scope)
})
```

`ProductInvocationHost::resolve(&PluginCallContext)` must verify that the invocation
is live and belongs to this instance, then return `ProductInvocationContext` with
host-resolved config, progress limits and a retained product `ServiceBridge`.
`SharedInvocationHost` is the default resolver: it requires the explicit live
`PluginCallContext.host_services` supplied by shared runtime wiring. It queries
`bcode.invocation-host/v1` operation `invocation_context` (Query, empty payload) for
`ProductInvocationSettings { config, transient_progress_limits }`. Missing ports
fail closed. Custom `_with_host` resolvers remain available for product hosts.
Native typed calls already carry host-bound `NativeServiceContext`.
The host is responsible for cancellation/admission fencing and capability lifetime
on both paths; a registry handle alone does not confer authority.

`ProductHostBridge::request` supplies a safe Rust-native invocation port through
`ServiceBridge::from_owned`. The bridge retains operation-family validation,
request/response bounds and cancellation checks. A response exceeding its bound
never causes the side effect to be retried. The host port must implement normal
operation authorization and end-to-end cancellation of in-progress work.

`SharedProductHostBridge(PluginHostServices)` concretely routes these operations
through M6's shared host invocation port using capability `bcode.product`, interface
`bcode.invocation-host/v1`, kind `Command`, and operation `exchange`, `receive_input`,
`invoke_service`, or `write_artifact`. The payload/response are the existing
`ServiceBridgeRequest`/`ServiceBridgeResponse`. Host wiring must supply this dispatch
contract and bind it to the actual invocation; no new broad transport schema is
introduced. Runtime admission is enforced by the retained shared port.

`ServiceCancellation::from_shared` retains live cancellation. Blocking compatibility
waiters poll shared cancellation at bounded intervals because the shared SDK's
current cancellation state exposes no wake subscription. Local cancellation still
notifies local waiters. `ServiceEventEmitter::from_shared` retains the live event
sink and exact invocation identity. `try_emit` reports rejection; compatibility
`emit` cannot report failure. Presentation/progress publishers use `try_emit` and
do not advance their authority on rejected events.

## Activation and events

After activation the adapter collects existing command and auth-provider hooks
once. `extension("bcode.activation-contributions/v1", [])` returns the cached
`bcode_plugin_models::activation::ActivationContributions`. Repeated reads do not
rerun hooks. The host validates version 1, decodes each owning product contribution,
and publishes it only for the actual successful activation. On partial failure,
shared host cleanup must deactivate the instance; adapter errors do not authorize
unloading. Successful deactivation clears the snapshot; failed cleanup retains it.

Concurrent implementations owning commands, auth or subscribed events implement
`register_commands_concurrent`, `register_auth_providers_concurrent`, and
`handle_event_concurrent` respectively. The legacy mutable hooks cannot safely be
called through a concurrent shared implementation.

Shared host events use kind `bcode.product-event/v1` and a serialized Bcode
`PluginEvent` as the JSON value. This retains the original topic and arbitrary byte
payload; it never guesses whether original bytes are JSON. Other event kinds are
rejected by the adapter.

Definitions project Bcode manifests additively into the generic declaration fields:
service `capability = "bcode.product"` / `kind = "command"`, compatible concurrency
spelling, and the product event envelope subscription. Original Bcode fields remain
present for product discovery and embedded identity checks; only shared fields are
added. Corrupt/unsupported projection blocks factory creation rather than silently
creating an empty declaration. This projection does not grant host capability access.

## Component registrations

Attach actual registrations to the same declaration rather than mutating a legacy
static vtable:

```rust,ignore
bcode_plugin_sdk::define_plugin!(MyPlugin,
    metadata = metadata_with_backend_and_frontend_roles(),
    registrations = {
        let registrations = ProductRegistrations::default().with_cli(cli_registration());
        #[cfg(feature = "tui")]
        let registrations = registrations.with_frontend(tui_registry);
        registrations
    },
);
```

`BcodePluginService::cli_registration()` and `frontend_registry()` return these
only for the selected `Frontend` role. A frontend component never invokes backend
activation hooks or backend service operations. TUI construction is a factory,
not one global registry shared by hosts. CLI/TUI features remain independent of
`static-bundled`; an rlib can supply either static or dynamic packaging.

Portable CLI describe/classify use `bcode_command::cli` contracts and the original
local parser. `with_portable_cli` attaches a handler returning `CliOutcome` with
explicit output and host actions; the old print-based `StaticCliRegistration`
handler is **not** automatically executed remotely. Without a structured-output
handler, portable invocation fails unavailable. This avoids corrupting stdout or
capturing process-global output in another host. Parsing/help results are explicit
values; conflicting host action/output/failure combinations are rejected.

Controllers and composer contributors can be attached to this same registration
value with `with_interactions(factory)` / `with_composer(factory)`. The selected
native `BcodePluginService` exposes these registries plus
`frontend_registry_factory()` for B3's existing local frontend loader.
`interaction_operation` and the portable `bcode.interaction-resources/v1` operation
share one per-instance controller map, with typed requests/responses, UUID handles,
expected-revision checks, terminal-state rejection, explicit destruction and a
configurable open-controller policy. Response failure never allows replaying an
already-applied input. Submitted controller output remains a proposal requiring
authoritative application commit. `bcode.composer-activate/v1` executes the existing
validated local grammar adapter; composer search remains its existing service.

Custom extension registration takes a factory
`fn(PluginCreateContext) -> Arc<dyn ProductComponentExtension>` so implementations
cannot accidentally be shared between loaded hosts. `close()` is invoked during
deactivation; failure retains cleanup-required state for the shared lifecycle owner.
Rich native paint/selection/image resources must be supplied by the presentation
owner through these factories, not serialized as Rust layout.

Presentation owners attach negotiated `ProductComponentExtension` implementations
using `register_extension`. They retain existing resource/geometry/revision/commit
ownership. The shared runtime invokes these through its normal extension operation;
no second resource manager or Rust paint layout is introduced. Shared registration
contracts advertise attached CLI and presentation extensions for that component.

## Selected frontend and common extension port

B2 binds `ProductExtensionPort` to its actual `SharedInstance`/native execution:
`ensure_active()` checks the current activation; `invoke(contract, payload)` runs
exactly once with normal shared admission, caller-bound ports, cancellation and
outcome acknowledgment. The port retains the owner/artifact for its lifetime.
`ProductExtensions::new(port, same_instance_provider)` is the common selected
extension registry for static and dynamic components. Descriptor discovery uses
`bcode.component-extensions/v1`; calls negotiate exact registered contracts.

B3 consumes `resolve_frontend(Some(portable_adapter)).await`, retaining the returned
`SelectedProductFrontend` alongside all surfaces/controllers. Native resolution
uses the selected provider's factory. Dynamic resolution requires B3's
`PortableFrontendAdapter`, receiving the same retained owner and actual contract
descriptor; it builds M7 resource/ack/input/full-replay/local-hit proxies, each
retaining that owner. Missing portable adapters fail explicitly rather than
falling back to bundled discovery or silently dropping rich resources. Reading
`SelectedProductFrontend::registry()` checks owner admission. This lease is not an
execution fence by itself: B3 must use owner admission for subsequent native input
and painting/resource operations as well. Owner drop alone never authorizes unload.

B4 uses `cli_descriptor`, `classify_cli`, `invoke_cli` on the same registry (or the
selected native CLI registration). These paths do not depend on SDK TLS.

The SDK implements these ports/contracts, not B2's host-side execution adapter or
B3's M7 rich frontend proxies. Those remain explicit integration requirements;
merely supplying an empty adapter is not rich frontend support.

## Frontend wire compatibility

`with_frontend(factory)` automatically installs exactly one B3 portable adapter
per selected frontend instance using that same factory and `PluginCreateContext`.
Both serial and concurrent definitions take this path; backend components do not
install or advertise it. The current contract is **`bcode.tui.frontend/v2`, version
2**. Frontend-v1's flattened response discriminator collided with event outcomes;
there is no fallback/reinterpretation as v1. Registration and both shared invocation
paths use B3's `TUI_FRONTEND_CONTRACT` / `TUI_FRONTEND_VERSION` constants.

The paired `bmux.presentation.retained.v2` advertisement identifies the scene
schema embedded in frontend-v2 render responses, not a second product RPC handler.
B3 produces those scenes through M7's `PortablePresentation::from_prepared` and
validates the retained envelope. Native factory access and portable author dispatch
therefore start from the same declared factory; no `bcode_tui` dependency is required
in the producer. This source wiring is not proof of rich-resource parity: parent
serial validation still owns actual selection, cancellation, display commit,
selection/image replay, and artifact tests.

## Retained product host sessions (source-stage contract)

`definition::host_session_models` owns `bcode.product-host-session/v1` models and
closed diagnostic codes. `definition::host_session` owns the same typed author
client for static/native use. This is not another executor/ledger or ABI:
`ProductHostSessionPort` must be bound by B4 to the actual retained session owner;
a future native adapter uses M1's separate generic retained async host port.
Invocation-local `host_services` cannot be used as a deferred session transport.

`ProductExtensionPort::product_host_session()` defaults to Unsupported;
`ProductExtensions::product_host_session()` forwards without requiring new-effect
admission, so an already-owned capability can still observe/join/ack after closure.
Neither call mints a session from serialized UUIDs. The capability itself is
Rust-local and nonserializable, while job/operation/request/receipt UUIDs are only
lookup keys within that bound port. There is no global resolver or fallback.

Begin/Start IDs are retained for idempotency and conflicting-duplicate checks;
acknowledgement reclaims outcome bytes but retains tombstones until session release.
The B4 ledger must charge request retention, outcome reservation, identity metadata,
failure records and tombstones before effects. Capacity rejection precedes dispatch.
The client validates bounded fields/envelopes and response families, makes one call,
and never retries automatically. Unknown/oversized responses are Uncertain, not
verified rejection or rollback. Runtime/native decode must bound production and
allocation as well; post-allocation client checks do not establish that guarantee.

Close means AdmissionsClosed, not cancellation or settlement. Join may remain
Pending. Release requires actual joined producers/callbacks and acknowledged
outcomes. Observer/client drop does not cancel, join, acknowledge or authorize
unload; unresolved cleanup remains with the retained runtime owner. The approved
exact schema does not yet name a cancel request, so no cancel operation is invented
here. B4/M1 must finalize that separately. Session creation/injection, the generic
retained native port and end-to-end deferred TUI integration remain unsupported
until those owners bind the real capability. No source-stage schema test proves
reservation/execution/cleanup behavior of that missing integration.

`definition::host_session::retained` now adapts the actual generic M1 retained
port. `ProductAuthorContext::capture(scope, &call)` opens while the invocation is
live, retaining only the independent port and exact scope. Its cached
`retained_session(wait)` creates one transport namespace shared by context clones.
`negotiate()` routes Query capability/interface `bcode.product-host-session/v1`,
operation `negotiate`, empty bytes; the bounded reply is `SessionNegotiation`.
Operation `request` carries unchanged product JSON requests/replies. Product UUIDs
are never truncated/hashed into transport IDs. Monotonic transport IDs and original
operation objects remain recorded through uncertain begin/read results.

Observe uses `RetainedSessionWait` supplied by the executor with capped 1–32 ms
async delays, not a blocking poll or busyloop. `recover(id)` never begins work;
`transport_ids`/`transport_request` expose bounded local recovery evidence.
`acknowledge_transport(id)` accepts only its retained transport receipt and never
sends product Acknowledge implicitly. Verified negotiation is the sole automatic
transport ack: after storing the exact contract/version/budget, receipt 1 is
acknowledged; failed ack is retained for exact retry. Its outcome reservation is
reclaimed but a 512-byte metadata tombstone and one slot remain until owner release.
Budgets with fewer than two slots fail closed rather than pretending to support
product requests. Other terminal replies stay cached and fully charged so explicit
transport ack cannot make a known response disappear from author recovery.

Exact serialized request length (including identifier escaping, envelope tags and
byte-array expansion) is counted before allocating its output buffer. Both retained
request copies, worst-case outcome/header and metadata are charged before encoding
or `begin`; exhaustion admits no host call. Original caller-owned input allocations
remain subject to caller input bounds, not magically bounded by this adapter.
The generic16-byte header is included in outcome capacity. Conflicting known
BeginJob/Start request identities reject before a new transport admission; actual
B4 ledger identity/policy checks remain authoritative. A publishing record makes
concurrent readers sleep asynchronously rather than invent an unknown outcome.

Only typed missing `open_retained_host` capability maps to Unsupported. Typed
capability denial remains secret-safe PermissionDenied uncertainty; corrupt/untyped
opener failure remains Unavailable uncertainty with no fallback. Confirmed product
or transport Close fences new BeginJob/Start while observations, join and ack stay
available. Identity records/code ownership stay retained through unknown outcomes;
there is no Drop close/cancel/ack/release claim.

Known remaining upstream gap: ambiguous generic `begin` can retain the original
request ID/reservation but cannot recover a lost operation handle. Proposed M1-owned
`recover(request_id, reservation)` is an owner-local non-mutating lookup returning
Publishing/Found/Absent/Acknowledged through a separately negotiated table tail.
It must never dispatch or reinterpret absence as permission to replay. No such
cross-owner API is implemented here; this path remains explicit uncertainty.

`ProductComponentExtension::invoke_scoped` supplies the exact host creation scope
to opt-in author extensions; existing synchronous extension behavior remains the
default. B3 can capture/store the independent context while that method's call is
live. `ProductExtensions::with_product_host_session(client)` attaches the same
negotiated author capability. Host-side `product_host_session()` still fails closed
unless B4 provides its real typed ledger override. No automatic opener, global
resolver, pointer encoding, or expired-invocation fallback is installed.

Runtime reservation authority remains B4's existing ledger, including conflicting
IDs, producer/image pins, wakeups and cleanup after observers disappear. The new
adapter tests use fake generic dispatch and do not prove real native callback
lifetime/denial/parity. Parent integration must wire B3 capture plus executor timer,
B4 routing and M3 actual activation owner; ambiguous begin without an operation ID
remains explicit uncertainty, never a request to replay effects.

## Boundary and migration limits

Clap commands/matches and native TUI/controller/paint traits remain Rust-local.
Only `StaticCliOutcome` and its product host action have gained serialization for
the portable CLI owner; parser layouts do not enter the ABI. Existing product
contracts (providers, tools, auth, workflow, generation, interaction, composer and
workspace grants) retain their payloads and authorization semantics.

Emitter/registrar values are now cloneable owned ports rather than `Copy` values.
Consumers reusing them must clone explicitly. Legacy raw callback constructors,
export macros and static vtables remain only until all production consumers and
thin packaging are migrated. They must not become a parallel permanent runtime.

This source stage is unverified. Formatting, compilation, focused behavioral tests,
architecture checks, plugin rebuilds and full integration validation are deferred.
