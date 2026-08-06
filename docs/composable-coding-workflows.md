# Generic composable shell and prompt workflows

## Purpose

Bcode workflows compose a small set of renderer-neutral primitives: typed shell operations, prompt
operations, deterministic dataflow and conditions, bounded control flow, durable gates, and exact
child workflow calls. Git synchronization, code review, progress planning, conflict resolution, and
other product procedures are source-authored compositions rather than host or plugin special cases.

The workflow store remains the canonical authority for definitions, runs, activations, attempts,
outputs, decisions, waits, receipts, resources, and terminal state. Source and package formats lower
to canonical definitions; they do not define another scheduler or store.

## Generic shell and prompt composition

A shell step is the generic deterministic external-operation boundary. Its owner receives exact typed
input, confines the working directory, executes the requested script or argv, and returns a bounded
typed result. Workflow and server code transport that contract without interpreting Git, build,
review, or command-specific state.

A prompt step is the generic adaptive-operation boundary. It starts an ordinary bounded Bcode turn
with explicit context target, structured input and output, tool policy, model selection, timeout, and
resources. Prompt output is untrusted and schema-validated. A later deterministic operation verifies
observable side effects before the workflow continues.

Typed plugin blocks remain an extension point for capabilities that genuinely require a domain owner,
such as authenticated external APIs or specialized devices. They are not the default mechanism for
command sequences, prompts, skills, Git procedures, reviews, or product orchestration.

## Shell authorization

Shell owns command extraction and analysis. Before execution, side-effect-free owner preparation
returns canonical command-policy facts and an opaque descriptor bound to exact input. The application
policy evaluates every executable subject and relevant redirection before process launch:

* `deny` rejects without dispatch;
* `ask` creates a durable permission wait; and
* `allow` permits dispatch.

A workflow run pins its authorization profile at admission and child runs inherit it. Every dynamic
command is prepared and evaluated again under current policy. Prompt text, skill metadata, output
text, presentation metadata, and renderer state never grant authority.

Arbitrary shell execution is conservatively mutating and repair-required because syntax cannot prove
all process effects. An interrupted ambiguous mutation is never automatically replayed.

## Typed shell results and assertions

Shell results carry status, exit and signal facts, accepted-exit facts, duration, complete byte
lengths and digests, bounded previews, encoding and truncation facts, and artifact references.
Conditions use generic typed selectors and predicates.

Source-authored exit protocols are the preferred assertion mechanism. A command uses `test`, `grep`,
`jq`, or another ordinary utility to reduce domain-specific state to a stable exit code. Exact text
comparison is valid only for complete valid UTF-8; exact byte comparison uses length and digest.
Large output remains an artifact rather than entering routine model or UI context.

## Prompt-based skill use

Skills are ordinary bounded instruction documents exposed through Bcode's generic skill catalog. A
prompt requests one in natural instructions, for example `Use the resolve-conflicts skill`. The agent
loads it through ordinary filesystem/document tooling when available.

Workflow semantics do not include skill blocks, workflow-specific skill activation modes, skill
requirements, skill-owned scheduler state, or skill-derived permission. Missing skills are handled by
the prompt's explicit structured outcome and do not change workflow authority.

The optional skills plugin owns presentation only. Repository, compatibility, user, and configured
skill roots remain available independently of the workflow plugin.

## Typed dynamic bindings

Dynamic data flows through bounded transforms over constants, prior typed outputs, selectors, arrays,
and objects. Shell values enter argv members or environment fields; source never interpolates
untrusted values into shell program text. Prompt values remain bounded structured JSON separate from
instructions. Secrets use runtime-owned delivery and never become durable workflow content
implicitly.

## Durable control flow

Conditions are deterministic predicates over typed values. Repeat, retry, parallel, fan-out, gates,
and child calls have explicit bounds. Production admission enables a capability only after durable
scheduling, cancellation, restart, duplicate-delivery, resource, exhaustion, and terminal-stability
semantics are implemented.

Active-revision lookup is forbidden during dispatch. Child calls target exact immutable definitions,
use deterministic child run identity, and do not abandon children when a parent process restarts.
Canonical terminal output belongs to workflow persistence and cannot be reopened by stale live work.

## Reusable source components

Reusable procedures belong in source-controlled child workflows. Examples include command assertion,
prompt-and-verify, adversarial review, conflict resolution, validation, formatting, checkpointing,
synchronization, progress planning, completion evaluation, and bounded implementation cycles.
Changing one of these procedures edits source rather than Rust host or plugin behavior.

A generic component package is checked in at
`fixtures/workflow-components/package.workflow-package.yaml`. Its child workflows keep commands,
prompts, schemas, limits, branch predicates, interaction gates, and child dependencies visible in
source. It includes command assertion, prompt verification, bounded remediation, isolated
adversarial review, conflict resolution, validation/formatting, checkpointing, normal non-force
synchronization, progress planning/refocus, completion evaluation, and a non-Git data-quality
example. These are examples and reusable source members, not privileged host templates; users can
copy or replace them and publication still goes through the ordinary package boundary.

A non-Git package must prove that the same shell, prompt, condition, retry, fan-out, and package
primitives are generic.

The flagship `feature-delivery` export is part of that ordinary package. It composes exact local
children for validation, review, completion evaluation, checkpointing, synchronization, conflict
resolution, and refocus. Commands, prompts, typed gates, limits, skill requests, context isolation,
and non-force synchronization remain visible in source; no specialized Git, review, progress, or
skill workflow service is required.

## Package and publication authority

A package contains bounded confined source members and exact local or external dependencies. Parsing
rejects path escape, duplicates, cycles, unsupported versions, and configured size/depth limits.
Publication is one daemon-owned transaction over the complete dependency DAG. Any conflict or error
rolls back all members, and a lock is generated only after complete canonical success.

## Frontend boundary

The TUI owns terminal canvas layout, input mapping, viewport behavior, hit testing, and drawing.
Unrestricted JSON Patch is not the primary contract for graph editing; frontends use typed semantic operations and application APIs. A frontend
cannot infer authorization or execution behavior from presentation metadata.

## Persistence and reset

Normal status, history, rendering, and context construction are bounded and non-mutating. Unsupported
workflow-store versions fail closed. Because this architecture intentionally makes a clean contract
break, incompatible workflow state is replaced only through an explicit maintenance reset that stops
writers, acquires exclusive ownership, creates and verifies a backup, initializes the new store, and
records a bounded reset receipt. No normal read performs migration or repair.

## Mechanical boundaries

Architecture checks enforce that:

* generic workflow packages do not execute shell or Git commands;
* Git, review, workflow-lifecycle, and skills-presentation plugins do not contribute product-specific
  workflow blocks;
* retired progress-document, hardcoded template, and workflow-owned skill assets are absent;
* shell remains the owner of generic `run` execution;
* prompt turns retain ordinary skill-catalog and permission boundaries;
* exact child identity and canonical terminal output remain durable; and
* frontend adapters do not own workflow semantics or persistence.
