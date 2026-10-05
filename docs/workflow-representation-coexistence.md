# Workflow representation coexistence

## Decision and status

Design direction adopted for implementation under the user's authorization to rearchitect
large-swarm execution while preserving repository invariants. This is not a completed
implementation, a migration approval for live state, or an invariant exception.

Independent storage and policy reviews agree that one canonical authority per run does
not require all runs to share one physical table layout. The nullable schema43/global
compatibility3 candidate is not an online coexistence solution for compatibility2
artifacts. Do not ship that candidate as proof of coexistence or silently downgrade its
already-created databases.

## Required representation contract

Keep the existing compatibility2 legacy surface usable for capped legacy runs. Add an
explicitly versioned representation for newer runs within the same workflow database.
Each run has one canonical representation, immutable during ordinary operation;
explicit verified maintenance may atomically replace its representation while preserving
one authority, receipts and fencing. No shadow legacy row with a dummy
cap, artifact-selected database, alternate authoritative catalog, automatic history merge,
or silent fallback from damaged new representation to legacy is permitted.

Before admitting any new-format run, install typed transactional guards ensuring:

* Run IDs cannot exist in both representations, including old-writer insertion and
  reserved replacement successor identities.
* Single-active bindings retain their existing identity and active-state semantics
  across representations, including old-writer updates and races.
* Unsupported run semantics cannot gain execution authority during dispatch, recovery,
  cancellation, handoff, child admission or continuation.
* Independent legacy runs continue admitting and executing without releasing a
  domain-wide lifetime lock or stopping other daemons.

Old binaries need not display new-format runs they cannot understand. They must retain
independent legacy operation and must not accidentally create, mutate or take authority
over new-format identities. New binaries merge bounded per-representation discovery
streams and surface unsupported/degraded state locally. They cannot infer authority
from discovery. Compatible newer artifacts may transfer a run through verified handoff;
representation is not permanently tied to an artifact.

Maintain shared authored-data compatibility too: definitions, revisions, presets and
packages containing unsupported policy representations must not enter legacy-readable
records and break old discovery/admission decoding. Isolate incompatible payload families
and provide bounded explicit routing, not just separate run rows.

Session detachment retains its invariant exemption: authorized representation-aware
association removal must not acquire execution authority or contact its owner, including
when execution semantics are unsupported. It must not change outcomes or execution state.

Maintain one transaction owner for identity, bindings, authority, resources and admission.
New lifecycle code uses typed Switchy operations and real async ownership. Existing
legacy adapters may remain for legacy compatibility, but are not evidence of completed
new-format typed migration. No raw-SQL exception, replacement driver, split-connection
admission or nested-runtime bridge is authorized.

## Existing compatibility3 data

Treat compatibility3 data as an explicit supported historical representation. Do not
change its compatibility number to 2 or silently rewrite nullable runs. Preserve existing
values, receipts and fencing. A conversion, if needed, requires a separately implemented
verified ownership/interruption-safe maintenance path. No live conversion is part of
this implementation's tests.

## First production slice and review disposition

Both review sessions completed and their responses were collected by the coordinator.
The independent review required authored-data isolation, the detachment exemption,
and explicit ordinary-operation versus maintenance representation semantics; all three
are incorporated above.

Start with transactional cross-representation admission exclusion, not new execution:
install extension schema/indexes/guards and canonical identity/binding admission in one
immediate transaction while retained legacy handles remain usable. Enable new-format
dispatch only after its complete lifecycle is wired. No separately committed reservation.

Switchy currently lacks typed conditional cross-table write guards. Add a finite schema
contract for before-insert and before-update rejection predicates, typed identifiers and
scalar constants, equality/null/in-value predicates, conjunction/disjunction, and indexed
existence queries with explicit equality joins. Support inspection of exact versioned
guard identity; reject conflicting same-name definitions. SQL rendering stays private to
the backend; no caller-provided trigger text, expressions or predicate fragments. Execute
DDL on the owning typed transaction and fail closed on unsupported backends/features.

Preserve current directional single-active admission semantics rather than silently
strengthen them: an incoming single-active run conflicts with any matching active run,
using the existing running/paused/repair_required set, plus replacement reservations.
An immutable run-ID registration prevents cross-family duplicate insertion/update. Any
future change to symmetric exclusivity must be an explicit product decision, not a
side effect of this migration. Cover predecessor joins for replacement reservations.

Tests must include old writer INSERT/UPDATE/UPSERT and conflict clauses, same-ID races,
binding races in both orders, paused/repair-required transitions, replacement reservations,
transaction rollback and interrupted installation. RAISE behavior must not be bypassable
through OR IGNORE/REPLACE. Verify hostile identifiers/constants and old prepared-statement
recompilation. These tests prove admission isolation only; they cannot substitute for
new-format execution/recovery and actual old-artifact coexistence acceptance.

## Implementation and acceptance

- [ ] Typed upstream transactional identity/binding guard contract and adversarial tests.
- [ ] Versioned new run schema and atomic admission/authority/resource lifecycle.
- [ ] Complete graph/revision/output/cancellation/recovery/continuation routing to the
      canonical representation, including root allowance and descendant accounting.
- [ ] Bounded dual-representation catalog and normal application integration; isolate
      unsupported new-format state without disabling legacy operations.
- [ ] Actual compatibility2 artifact acceptance: simultaneous old capped/new uncapped
      work, cross-representation ID and binding races, old takeover rejection, compatible
      new-artifact handoff, and crash-safe additive installation.
- [ ] Historical compatibility3 handling and explicit safe maintenance coverage.
- [ ] Resolved architecture checks, isolated actual-source no-raw tests for the new
      implementation, full affected domain tests and plugin builds.

A table family or typed guard primitive alone is not product closure. Existing scheduler,
optional-policy and diagnostics work must be integrated, not discarded. Normal reads and
scheduler scans remain bounded; page sizes must not impose graph-size limits.

## Current validation and remaining acceptance (2026-10-04)

Implemented: typed upstream write guards; additive run admission, attempt/resource/authority
ledger; graph revisions and receipt-backed cancellation acceptance; bounded execution
frontiers; loop, delegation, child and fan-out application paths; mixed identity discovery,
basic views, scheduling pages and gate controls. Existing compatibility3 stores are preserved.

Verified on this working tree:

* `cargo fmt`, `cargo check --workspace`, and
  `cargo clippy --workspace --all-targets -- -D warnings` passed.
* `cargo test -p bcode_server --lib -- --test-threads=1`: 1,031 passed, 7 ignored.
  Earlier parallel and intermediate runs failed; they are not counted as passing validation.
* `cargo test -p bcode_workflow_store --lib`: 618 passed.
* Client, IPC, workflow, workflow-view/models, loop/workflow/fake-provider plugin library
  suites passed; affected plugin builds and `cargo run -p xtask -- architecture` passed.
* The separately invoked old-artifact acceptance test passed using the specified release
  executable in isolated temporary state and the current provider execution owner.
* Real fake-provider overlap was verified at 32 and 100 workers; 257-member evidence is
  storage/synthetic execution, not 257 live provider turns.
* Actual-source isolated tests excluded Switchy raw-SQL features. This does not claim
  that the legacy parent store has been migrated away from direct-driver SQL.

This is not full architectural closure. Remaining acceptance includes package/authored
child-call routes, cross-process recovery of pending publication cancellation, complete
continuation/replacement routing, full mixed-family rich inspection/filter parity, and
lifetime-scale fairness. Unsupported paths must remain explicit rather than fall back.
The local Cargo patch has been removed after the user pushed the Switchy implementation.
`Cargo.lock` resolves the MoosicBox git dependencies to
`7acd57cd12883bda89e747fb473cc669677ac2a8`; no sibling checkout is required.
No commits or pushes have been made by the coordinator.

## Evidence and correction of earlier claims

Research was reconciled at Bcode HEAD 642245645067. Baseline 5b8274907541 accepts
additive schema revisions while global compatibility remains 2. Its list_runs query does
not select concurrency_cap: the earlier suggestion that nullable caps necessarily poison
that specific catalog was incorrect. Its authority-transfer path does not enforce new
per-run semantics. Therefore a new ignored auxiliary policy table alone is insufficient,
but incompatibility of that approach is not impossibility of representation evolution.

Working research notes: /tmp/bcode-swarm-coexistence-design.md and
/tmp/bcode-swarm-coexistence-review.md. These are fallible design evidence, not runtime
state or verification artifacts. No old artifact or live database was mutated to obtain
these findings.
