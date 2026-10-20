# Application operation boundary

Bcode application behavior is reusable without making a second transport or a generic application framework. The current boundary is a set of focused, server-owned operation modules backed by portable domain contracts.

## Workflow permission previews

Current permission previews include a version-1 finite graph: content-derived definition
keys own local grant/approval requirements, runtime-checked nodes, and call edges.
Shared calls are represented once per definition, not expanded into invocation paths.
Counts refer to distinct nodes, never numbers of approvals or invocations. Agent, plugin,
and call nodes are explicitly marked runtime-checked: the preview cannot predict their
concrete operations, arguments, paths, or future policy decisions.

The graph is advisory and grants no authority. Every runtime invocation retains its
existing operation-level policy checks and user approvals. Recursion does not inherit
blanket permission. Runtime invocation identity and audit paths are not replaced by graph
keys. Missing or unresolved call targets still fail compilation rather than disappearing
from a successful preview.

Compatibility: absence of `graph` identifies historical path-only previews; current
previews use their retained list fields for root-local facts and the graph for all child
facts. Graph-aware consumers count graph nodes. Older strict readers reject the new field;
unknown graph versions are rejected rather than interpreted as version 1. No stored
preview is silently rewritten. This graph does not itself enable compilation of recursive
package calls, whose executable identity resolution remains separate work.

## Request lifetime and ordered transport

Local IPC uses bounded read-ahead and ordered execution. Queued bytes can delay EOF
observation; disconnect is not a guaranteed prompt cancellation channel under pipelining.
The transport does not drain indefinitely or discard queued mutations to reach EOF.
Explicit cancellation should use an independent control connection.

The transport reports request-lifetime end; the server-owned workflow application adapter
chooses cancellation for disposable discovery reads. Other operations retain their own
completion semantics. This host-local signal is not a public reconnect/resume contract.
Retained-detail deadlines also terminate the awaiting read independently of worker exit;
worker admission remains held until the blocking work exits. Successful response delivery
after read EOF does not stop ordered request execution: buffered mutations are processed
before the queued EOF. A real IPC regression covers two pipelined session creations with
the peer's write half closed. Full disconnect with undeliverable responses and mixed
read/mutation pipelines still require broader behavioral validation.

## Retained workflow launch pages

Launch-catalog version 3 distinguishes pending discovery tokens from result tokens in
`next_cursor`. A result token owns a temporary indexed preview spool and one admission
permit. Subsequent pages use indexed keyset reads, not source rediscovery or compilation.
The query, limit, and exact last cursor position are bound to each single-use token.
Successful continuation rotates the token; completion, cancellation, expiration, or an
admitted failure releases retained state. Tokens expire after 60 seconds and do not survive
daemon replacement. Lost responses do not confer retry safety; restart discovery.

Source content and compilation inputs reflect the admitted discovery, not a live filesystem
view. Refresh starts a new scan. Publication changes invalidate retained pages through the
workflow-store read fence. Cancellation uses the existing discovery cancellation operation
with the result token. SQLite's bounded page cache and bounded result windows do not imply
a total disk quota or a total process-memory guarantee. No durable workflow/session state is
written by this disposable spool. Version 2 requests are rejected rather than guessed.

## Canonical call path

```text
CLI, TUI, HyperChad, SDK, or future adapter
→ typed `BcodeClient` method or shared renderer action
→ local IPC framing and exhaustive request routing
→ focused `bcode_server` operation
→ owning session, runtime, workflow, permission, plugin, or configuration domain
→ typed result or bounded ordered event stream
```

An in-process caller begins at the focused operation and uses the same domain owners. It does not construct IPC requests, open sockets, or read persistence directly.

## Operation responsibilities

Focused operation modules own reusable application coordination, including:

* Normalized authorization facts and authorization before side effects.
* Canonical domain invocation and typed results.
* Bounded reads, cancellation, ownership fencing, and terminal-state behavior.
* Explicit private context only where required, such as authenticated client identity or a connection-scoped event sink.

Operation modules do not own wire framing, request IDs, response encoding, handshakes, daemon artifact negotiation, sockets, connection lifetime, or frontend rendering. Validate this distinction through typed application interfaces and operation tests rather than source scans.

## Adapter responsibilities

The local IPC adapter owns:

* Exact daemon artifact startup and handshake behavior.
* Request and response framing, request IDs, and payload encoding.
* Exhaustive `RoutedRequest` classification.
* Connection attachment, lifecycle, and bounded event delivery.
* Mapping typed normalized failures to public transport errors.

A future concrete adapter must perform equivalent plumbing and call the existing typed operations. It must not copy application behavior, inspect canonical persistence, reinterpret event logs, or expose server-private types.

## Streaming semantics

Snapshots and event streams transfer current state and bounded live updates. They are not durable resume protocols. Any future adapter that claims resumability must separately define retention, acknowledgment, replay, gap, duplicate, conflict, and reconnect semantics.

Connection-scoped subscriptions receive an explicit event sink and return the durable sequence immediately after the initial snapshot boundary. Disconnect cleanup remains transport-owned.

## Plugin interactions

Plugin workflows remain plugin-owned. Application and renderer adapters may use plugin-contributed semantic controllers and typed `InteractionInput` when available. Schema-aware raw structured resolution remains the generic fallback. Unknown producer schemas or versions are surfaced rather than guessed.

Pending exchange application coordination is transport-neutral and server-owned. `interaction_operations` owns permission and exchange registration, compatible-consumer checks, response authorization, duplicate conflicts, cancellation, consumer detachment, terminal completion, cleanup, and bounded listing. IPC dispatch only decodes/maps typed requests and responses; client connection context remains explicit input to compatibility checks. Focused in-process tests cover successful response, cancellation, last-consumer detachment, invocation mismatch, no compatible consumer, conflicting duplicate identity, unknown resolution variants, and canonical pending-state cleanup without opening a socket.

## Portable callers and retained consumers

Public operation traits belong to their domains; server handles bind verified caller
context and delegate to existing owners. A transport adapter encodes requests and maps
normalized responses. It must not redefine permission policy, acquire canonical state
through private persistence, or interpret plugin-owned payloads as application effects.

`BcodeClient::with_interaction_controller_versions(BTreeSet<u32>)` changes only the
semantic compatibility declaration, retaining resolved runtime configuration and any
configuration error. It does not manufacture renderer adapters.
`interaction_controller_consumer()` opts in to semantic version 1 and returns a
connection-retaining `InteractionControllerConsumer`. Its `InteractionApplication`
methods `open_controller`, `controller_snapshot`, and `controller_input` run on that
same connection. Inputs carry the exact exchange identity and expected host revision.
Disconnect does not transfer ownership, remove revision fencing, or authorize replay;
transport failure may leave effects unknown. Establish a semantic consumer before work
needs it, and keep it alive while driving the pending exchange. Only canonical host
consumption establishes a terminal result, not local input exhaustion or a plugin's
intermediate output.

Both `SessionApplicationHandle` and `BcodeClient` implement the portable
`SessionObservationApplication` contract. `ClientSessionObservation` consumes server-owned
semantic snapshots and ordered updates with bounded retention; gaps, limits, invalid sources
and transport loss end the stream explicitly. Close/drop releases observation, not the turn.
Resynchronization opens a fresh bounded observation, not replay. `SessionWatcher` remains
a separate bounded attachment/event surface with resync signals; neither surface is a
durable resume protocol.

Usage reads query existing accounting snapshots; they do not collect or rebuild usage.
Import discovery/loading does not implicitly create a canonical session. Search reads
and hydration remain bounded and non-mutating; purge/rebuild and backfill are explicit
maintenance operations. Keep required confirmations, selected-location checks,
operation revisions and cancellation propagation at their existing owners. Dropping an
observer is not cancellation, cancellation acceptance is not rollback, and an admission
receipt is not a completed operation.

## Typed commands and native effects

`CommandApplicationHandle` and `BcodeClient` implement the command-owned
`CommandApplication`. CLI command automation and TUI invocation use it; opaque
`PluginApplication` payloads do not independently define command semantics. The owner
resolves canonical session/workspace context and performs contextual invocation policy
checks. A retained client socket carries invocation and bounded cancellation control
under the same caller identity. Clones share their local controls; independent clients
cannot gain cancellation authority by knowing an ID. Read-ahead reserves the ID before
ordered dispatch and handles caller-scoped cancellation without queuing behind the
blocked command. Reservation grants no execution authority.

`Completed` retains the owner's response, including `success: false`. `Failed` is a known
application failure; `Unknown` means no trustworthy settled result is available and effects
may have committed. Transport loss does not establish terminal failure, success, rollback,
or safe replay. Do not retry automatically. Cancellation acceptance is distinct from
terminal cancellation and does not roll back committed effects.

Renderers adapt typed effects natively. Status/copy presentation does not implicitly
append canonical history; TUI explicitly handles `AppendText` through its canonical
presentation-note operation, while CLI emits the text without that mutation. Session
navigation uses canonical access. Failed commands do not
drive success effects, and unknown outcomes supply no effects to apply. Clipboard, focus
and surface presentation remain frontend-local. CLI reports unsupported native surfaces
with stable identities and structured fallback rather than pretending to open them.
Plugin-owned options remain plugin-owned; fallback does not transfer business rules.
HyperChad's existing session/interaction actions do not by themselves establish generic
command invocation or command-effect coverage.

## Runtime lifecycle

`RuntimeApplication` exposes status and explicit shutdown through `ServicesApplicationHandle`
and `BcodeClient`. Host-only `RuntimeShutdownPreparationApplication` returns a borrowed,
single-use operation. Preparation does not signal shutdown or reserve idleness; dropping
it does not commit. IPC acknowledges before consuming `commit`. The receipt is admission,
not proof of process exit, cleanup completion, or rollback. No serialized prepare/resume
capability or equivalence to runtime-work cancellation is implied.

## Verification

Source implementation and test definitions are not passing runtime evidence. The command
regressions cover blocked active cancellation, foreign-caller rejection, transport-loss
uncertainty/no automatic replay, and native effect handling. Their integrated results,
lifecycle tests and renderer suites must be recorded separately by validation; this document
does not certify them or declare merge readiness. Ralph read-path migration and bounded-history
limitations remain explicitly deferred; see the parity inventory.

Representative coverage combines direct in-process operation tests with local IPC integration tests. Workflow source application exercises both paths and verifies identical created/updated outcomes and canonical store effects. Permission and interaction coverage directly proves operation-owned permission resolution and exchange lifecycle behavior without transport framing, real IPC permission persistence, and IPC/HyperChad plugin-owned semantic-controller behavior. Plugin service coverage verifies direct/IPC inventory and normalized result equivalence. Both explicit-plugin and unique-interface service operations return `PluginServiceOperationResult`; conversion from the plugin runtime response is private to the operation module, and IPC framing only maps the normalized fields. Real IPC tests cover unsupported-operation results through both routing modes, plus missing-plugin and missing-interface routing failures with matching public errors that do not echo the supplied identifiers. This is representative routing/error coverage, not proof of every plugin's payload or workflow semantics. Runtime-work coverage verifies direct list/history/cancellation behavior, real IPC list/history equivalence and cancellation admission, initial watch catch-up, and post-subscription cancellation delivery. Session coverage directly exercises create, rename, working-directory mutation, bounded reads, complete-history access, damaged-state behavior, and deletion without transport framing; real IPC coverage verifies create, rename, delete, bounded history pages, and around-anchor windows. Completion of the broader application-boundary effort still requires observed integrated results for portable observation, command cancellation/effect consumers, lifecycle cleanup and individual operation parity; implemented contracts alone do not close those qualifications. Plugin event coverage verifies both no-subscriber routing and subscriber delivery through direct operations and real IPC.
