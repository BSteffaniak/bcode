# Database SQL transition

## Authority and scope

The current user-directed cross-repository migration authorizes a compatibility transition for
**existing usages**, followed by removal. It does not approve new arbitrary SQL, permanent
exceptions, or raw corruption fixtures. No separate fixture exception has been approved.
The durable contribution policy is in AGENTS.md; this document records temporary mechanics,
not an invariant change or a certificate of completed migration.

At the initial Bcode revision `278914fd1eca`, Switchy dependencies disable defaults and do not
yet know the upstream raw-sql boundary. Upstream is adding a default-enabled compatibility
feature. The coordinator owns local patches and explicit temporary forwarding opt-ins; merely
patching upstream without opt-ins can break unmigrated consumers. Do not enable upstream defaults.

Existing Switchy migration scope: session, database-observability, router telemetry,
blims, CLI/server database operations and existing settings/ralph/code-review database consumers.
Usage has been removed from the transition envelope after actual-source isolated production/test
compilation with no raw activation (13 tests). Shell-command-analysis's historical shadow reader
now uses typed read-only Switchy access, keyset-paged queries and structured fixtures. Its direct-driver
allowance is removed; actual-source isolated tests (22 passed, one local-corpus harness ignored)
and strict all-target clippy pass with no raw activation. Observability now has an explicit default-off
`raw-sql` forwarding feature for its existing decorator methods; session's workspace dependency
selects it during transition. The root Switchy database dependency no longer enables raw for
all consumers: telemetry and BLIMS retain their existing calls through leaf opt-ins. This moves,
not broadens, the documented existing scope. The activation guard permits the observability
forwarder only during transition; strict closure still rejects it.
The resolved guard permits explicit raw feature requests only from these owners during transition.
This is a bounded package envelope for existing calls, not proof that every call is authorized.
Remove each envelope entry after its production and test paths compile independently without raw.
New consumers, additional fragment surfaces and driver edges require specific user approval.

Existing direct rusqlite edges (the guard distinguishes normal from dev dependencies):

| Owner | Kind | Removal condition |
| --- | --- | --- |
| bcode_workflow_store | normal | typed affinity/immediate transaction, errors, locking and ownership-release parity |
| bcode_workflow_discovery | normal | typed bounded read-only discovery and error parity |
| bcode_opencode_session_import_plugin | normal | typed foreign-schema read-only import |

The observability direct-driver dev allowance has been removed. Current actual-source isolated
no-raw tests pass (five tests; manual benchmark ignored); its default-off raw forwarding remains
only for the existing session transition, not permission to restore a driver dependency.

These are migration debt under the user's migration request, not independently approved enduring
exceptions. A future malformed fixture needs precise approval and isolation, not this table.
Switchy backend-private drivers remain legitimate; Bcode must not acquire their public bypasses.

## Finite exit and validation

1. Integrate upstream typed APIs and raw gating via coordinator-owned root patches. Forward raw
   only for the existing migration scope where necessary; never turn on upstream defaults.
2. Migrate actual operations and tests, retaining storage formats, ownership fencing, bounded
   reads, explicit repair, connection affinity, transaction/busy/read-only/error semantics.
3. Remove owner opt-ins and direct driver dependencies as each domain finishes. Compile each
   migrated consumer in an isolated workspace without raw (including its relevant test graph).
   Workspace success is insufficient because features unify additively.
4. Remove the transition lists in `xtask/src/architecture/sql.rs`, make strict closure unconditional,
   and run ordinary shipped profiles, independent plugins and relevant dev/test builds with no raw.
   `BCODE_ARCHITECTURE_SQL_CLOSURE=1 cargo run -p xtask -- architecture` already provides the
   strict metadata gate: no raw activation/request anywhere and no Bcode-owned direct driver edge.

Architecture checks use resolved package IDs, activated features and dependency kinds, not source
matching. Workspace metadata includes dev edges and their feature union; isolated production
profiles/plugins prevent unrelated workspace feature contamination. Metadata probes do **not**
compile APIs, and independent dependency probes do not include the dependency's own dev graph.
Upstream compile-fail probes and isolated downstream test builds are separately required.

During transition the raw Switchy activation envelope is necessarily shared: a new raw call in an
already enabled consumer cannot be detected by metadata. Review, isolated no-raw compile probes,
and final global absence close that gap. Upstream eventual removal from defaults is a separate
compatibility-release follow-up; Bcode completion does not wait for unrelated consumers or pushes.

No final absence or successful migration is claimed by this document.
