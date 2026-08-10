# Bcode Invariants

These invariants describe conditions that must remain true across valid changes to Bcode. They are acceptance criteria, not implementation suggestions.

An invariant is a durable condition of a valid product or architecture. Contributor workflow belongs in `AGENTS.md`; current design mechanics and migration details belong in `docs/`; validation commands belong in `AGENTS.md` or scripts; preferences should not be promoted to invariants without a concrete product or architectural reason.

## Product and platform boundaries

* **Bcode remains independently usable.** BMUX and other integrations may extend Bcode, but they must not become prerequisites for unrelated core operation.
* **Product semantics are frontend-independent.** Terminal, web, desktop, SDK, and future clients may present behavior differently, but no frontend exclusively defines shared product behavior.
* **Clients use defined application boundaries.** Frontends and integrations must not acquire private daemon, persistence, or provider implementation details to perform application behavior.
* **Daemon artifact versions are isolated.** A client may connect only to the exact daemon artifact identity it targets; when no matching daemon is available, startup coordination launches one matching daemon without replacing, blocking, or depending on other artifact versions.

## Package and dependency ownership

* **Crates are domain-owned.** A crate must represent an implemented capability or domain; generic `core`, `shared`, `common`, speculative, and placeholder crates are prohibited.
* **Shared model crates remain lightweight leaves.** Domain model crates may contain portable data types and lightweight type utilities, but not orchestration, persistence, network clients, plugin loading, provider behavior, or UI logic.
* **Dependencies point toward contracts.** Portable models and contracts must not depend on concrete implementations, renderers, daemon hosts, or parent implementation crates.
* **Implementation details do not leak through public contracts.** Cross-domain APIs use domain-owned typed abstractions rather than private implementation or third-party framework types.

## Rendering and frontend boundaries

* **Shared rendering remains renderer-neutral.** Shared application, session-view, and portable rendering layers must not depend on terminal drawing, input, frame, viewport, key, mouse, or BMUX types. Terminal-specific adaptation belongs at the TUI boundary. Behavior shared by multiple frontends must be modeled generically before being adapted by a renderer.
* **The TUI owns terminal-specific behavior.** Terminal layout, viewport behavior, scroll anchoring, hit testing, cursor behavior, terminal input mapping, frame drawing, and terminal-native plugin surfaces belong in the TUI.
* **Interactive presentation choices do not mutate declarative configuration.** Themes and other presentation defaults may be declared in configuration, but changes made through an interactive frontend persist only to user state and must not create or modify declarative configuration files.
* **Shared semantics precede renderer adaptation.** Renderers consume shared semantic session-view contracts and adapt them into native presentation; they must not independently reinterpret raw event logs when a shared semantic projection exists.
* **Renderers do not own canonical session state.** Renderers may retain ephemeral presentation state, but canonical session, interaction, and runtime state remains owned by Bcode's application and session layers.
* **Portable rendering does not special-case terminals.** Portable APIs must not add terminal branches, terminal-shaped data, or terminal dependencies to reproduce TUI behavior. Inherently terminal-only behavior stays at the terminal boundary.
* **Native presentation differences are allowed.** Renderer neutrality requires shared product semantics, not identical layout, appearance, or interaction mechanisms.
* **Plugin presentation retains a generic fallback.** Rich renderer-specific adapters may exist, but renderer-neutral structured presentation and interaction remain available as a fallback.

## Plugin ownership

* **Domain behavior belongs in plugins when practical.** Permission, provider, tool, command, integration, and plugin-contributed UI behavior should be plugin-owned rather than hardcoded in the host.
* **Plugin hosts provide plumbing rather than product behavior.** Hosts and runtimes own discovery, loading, routing, lifecycle, isolation, and contract enforcement without absorbing plugin-specific behavior.
* **Plugin interfaces are versioned and typed.** Cross-boundary plugin requests, responses, manifests, and contributed schemas are versioned, typed, and serializable.
* **Bundled plugins remain disableable.** Bundled plugins may be enabled by default, but disabling one must not break unrelated Bcode capabilities.
* **Renderers do not take ownership of plugin behavior.** A renderer may adapt a plugin-owned schema but must not become the owner of the plugin's workflow or business rules.

## Session persistence

* **Event history is canonical.** The canonical session event store is authoritative; catalogs, manifests, projections, indexes, and in-memory views are derived.
* **A session has one canonical storage path.** Writer identity, build identity, process, and frontend must not select alternate canonical storage for the same session ID.
* **Normal session reads are bounded and non-mutating.** Catalog, open, attach, history, renderer, and model-context paths must not migrate, repair, reindex, or full-replay session history.
* **Damage is surfaced rather than concealed.** Missing, stale, corrupt, ambiguous, future, or inconsistent derived state produces a degraded or repair-required result unless trustworthy sidecars permit bounded incremental catch-up.
* **Repair is explicit.** Full replay, reconstruction, reindexing, migration, and repair occur only through explicit maintenance operations with their required ownership and safety checks.
* **Historical session behavior is migration-owned.** The session runtime and session models contain only current-format behavior. Historical classification, legacy payload handling, migration planning, and conversion belong to the session-migration domain. The session domain may expose narrowly scoped current-format migration-target capabilities without owning historical policy.
* **Canonical history is never silently merged.** Duplicate or historical session roots must not be merged automatically.

## Runtime, tools, and permissions

* **The runtime remains domain-generic.** Turn scheduling, cancellation, streaming, tool dispatch, and loop mechanics must not hardcode behavior owned by a particular tool, plugin, provider, command, or UI.
* **Cancellation is end-to-end.** Cancellation propagates through scheduling, provider work, tool execution, persisted state, and client-visible terminal outcomes.
* **Authorization precedes side effects.** Tools and commands must not perform side effects before applicable policy and user-permission decisions complete.
* **Permission decisions use canonical operation facts.** Policy evaluates normalized operation facts rather than presentation text or untrusted tool descriptions.
* **Tool results are untrusted input.** Tool output is bounded and treated as potentially partial, malformed, secret-bearing, adversarial, or misleading before entering model or UI context.
* **Presentation does not determine execution semantics.** Display metadata and renderer-specific presentation must not affect authorization, dispatch, or persisted execution outcomes.

## Models and providers

* **Provider details remain provider-owned.** Authentication, wire formats, provider-specific metadata, retry interpretation, and request conversion belong in provider integrations or the provider runtime.
* **Application contracts use normalized model semantics.** The rest of Bcode consumes normalized messages, events, usage, errors, stop reasons, and capabilities rather than provider-native payloads.
* **Model resolution is centralized.** Aliases, provider selection, capability checks, and fallback resolution use the model catalog and resolution path rather than local ad hoc matching.
* **Context accounting has one semantic source.** Request estimation, provider-reported usage, occupancy, and context display must not invent contradictory accounting rules in individual frontends or providers.
* **Retry and fallback preserve safety.** Retries and provider fallback must not duplicate committed side effects, weaken permission requirements, or misrepresent the final provider or outcome.

## Public frontend contracts

* **Frontend contracts remain portable.** Public frontend events and snapshots must not depend on TUI models, daemon IPC types, web-framework types, terminal primitives, or plugin implementation types.
* **Provider-private data does not leak.** Provider request representations, secrets, and opaque metadata do not enter public frontend contracts unless explicitly normalized and approved.
* **Event ordering is explicit.** Frontend event streams preserve defined session, turn, sequence, duplicate-delivery, and terminal-transition semantics.
* **State transfer does not imply durable resume.** Snapshots and event envelopes must not be described as reconnect-safe or durably resumable unless the transport defines retention, acknowledgment, replay, and conflict behavior.

## Security, privacy, and data handling

* **Public diagnostics are secret-safe.** Provider errors, tool failures, traces, and logs expose normalized messages without leaking secrets at public boundaries.
* **Untrusted paths remain confined.** Artifact, session, import, plugin, and tool-controlled paths are canonicalized and confined to authorized roots before access.
* **Persistence is explicit.** Request-only context, retrieved memory, provider metadata, and temporary tool data do not become durable session content implicitly.
* **Sensitive ambiguity fails closed.** Ambiguous authorization, unsupported versions, inconsistent durable state, and unverifiable ownership do not silently fall back to permissive behavior.

## Reliability and compatibility

* **Derived state is disposable.** Derived data is identifiable and its presence is not proof of canonical validity.
* **Normal interactive paths are bounded.** Attach, refresh, rendering, catalog discovery, context construction, and routine history access have bounded work and memory behavior.
* **Terminal outcomes are stable.** Once a turn, interaction, or persisted stream reaches its authoritative terminal state, stale live updates cannot reopen or overwrite it.
* **Duplicate delivery is safe where promised.** Contracts that permit retries or duplicate delivery define idempotency and conflicting-duplicate behavior.
* **Persisted and public schemas are versioned.** Persisted formats, plugin contracts, frontend contracts, and cross-process messages have explicit version semantics.
* **Unknown future state is not guessed.** Unknown schema versions and unsupported variants are preserved, rejected, or surfaced according to contract rather than silently interpreted as known older forms.
* **Migrations preserve canonical authority.** A migration may change representation but must not replace canonical authority with an index, renderer state, or transient process state.

## Invariant evolution

* **Invariant conflicts block silent implementation.** When a request and an invariant, or two invariants, require incompatible outcomes, agents surface the conflict and obtain an explicit architectural decision rather than choosing silently.
* **Exceptions are explicit.** Existing violations and migration states are not implicit exceptions; any intended exception must have a clear scope and rationale.
* **Invariant changes update the architecture coherently.** Intentionally changing an invariant requires corresponding updates to affected architecture documentation, tests, and mechanical guards where they exist.
* **Mechanically checkable boundaries should be enforced.** Important invariants gain dependency checks, architecture scripts, compile-time boundaries, or focused tests when practical.
