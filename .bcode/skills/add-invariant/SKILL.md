---
name: add-invariant
description: Evaluate a proposed Bcode requirement, classify it as an invariant, policy, architecture documentation, validation rule, or no change, and interactively apply the smallest approved update.
allowed-tools: Read(*), Glob(*), Grep(*), Question(*), Edit(*), Write(*)
---

# Add Invariant

Use this skill when a user proposes a durable rule for Bcode or asks whether a concern belongs in the invariant catalog. Do not assume that every request should create an invariant.

## Authoritative definition

Read `INVARIANTS.md` before evaluating a proposal. Its introductory definition is authoritative. Also read `AGENTS.md` and relevant architecture documents. Keep these responsibilities distinct:

* `INVARIANTS.md` describes durable conditions of a valid product or architecture.
* `AGENTS.md` describes contributor and agent workflow, coding policy, and required validation.
* `docs/` explains current architecture, mechanics, rationale, and migration state.
* Scripts and tests mechanically enforce boundaries where useful.
* Preferences and vague aspirations are not invariants.

One proposal may require changes in more than one location, or no change at all.

## Workflow

### 1. Understand the proposed condition

Determine:

* What must remain true.
* Which product or architecture boundary it protects.
* Whether it should survive implementation changes.
* What regression it prevents.
* Whether any exception is intended.

Ask concise questions only when the durable condition or intended scope is unclear.

### 2. Classify the proposal

Classify it as one or more of:

* Invariant.
* Contributor or agent policy.
* Architecture documentation.
* Validation requirement.
* Temporary migration constraint.
* Preference.
* Already covered.
* Not ready to codify.

Briefly explain the classification. Running Clippy is a policy and validation requirement; shared rendering remaining independent of terminal types is an invariant; the name of a current portable UI implementation is normally architecture documentation.

### 3. Inspect existing guidance

Read:

* `INVARIANTS.md`.
* `AGENTS.md`.
* Architecture documents relevant to the proposal.
* Relevant architecture checks when the proposal concerns an already enforced boundary.

Determine whether the proposal is already covered, clarifies an existing invariant, duplicates a narrower rule, conflicts with existing guidance, or defines a genuinely new condition.

### 4. Sculpt the smallest useful invariant

A candidate invariant should be:

* Durable across implementation changes.
* Necessary to a valid product or architecture.
* Clear enough to evaluate a proposed change.
* Scoped tightly enough not to govern unrelated code.
* Broad enough to prevent the stated regression.
* Neutral about current implementation details unless those details are themselves essential.
* Distinct from existing invariants.
* One coherent condition; split independently changeable requirements.

Use the repository's simple Markdown style: a section and a bullet with an optional bold title followed by direct explanatory text. Do not add IDs, metadata blocks, enforcement lists, or related-document fields. Keep one sentence when one sentence is sufficient.

Avoid vague escape clauses such as “where possible,” “normally,” or “unless necessary.” Define a real exception or classify the statement as a preference instead.

### 5. Check conflicts

Compare the candidate with every existing invariant. Pay particular attention to:

* Competing ownership assignments.
* Generic versus frontend-specific behavior.
* Extension points versus hardcoded behavior.
* Canonical versus derived authority.
* Mandatory versus disableable capabilities.
* Fail-open versus fail-closed behavior.
* Bounded work versus full replay.
* Portable versus implementation-specific contracts.

Quote possible conflicts and explain the incompatible interpretations. Existing violations and migration states are not implicit exceptions.

### 6. Present the exact proposed disposition

Before editing, show:

* Classification.
* Target files.
* Exact proposed invariant or policy text.
* Existing text it overlaps, replaces, or conflicts with.
* Recommended action, including no change when appropriate.

Use the Question tool to ask for approval of the exact payload. Offer approve, revise, and cancel choices. “Recommended” is not approval.

### 7. Apply only the approved payload

After direct approval, make the smallest approved change. If the approved text changes, present the revised payload and obtain approval again. Do not create a script or test merely because an invariant was added unless mechanical enforcement was part of the approved task.

### 8. Re-read and validate

Re-read every edited section and repeat the duplication and conflict check. Confirm that invariant text describes a durable condition rather than workflow or implementation mechanics. Run documentation or catalog checks if they exist, then report exactly what changed and what validation ran.

## Rules

* Default to draft-only mode until the exact mutation payload is approved.
* Never act without user confirmation.
* Never skip a gate.
* Use a two-turn mutation barrier: proposal and approval must occur before edits.
* “Recommended” is not approval.
* Approval must come directly from the user through the Question tool in the current run; delegated, resumed, or inferred approval is invalid.
* Approval is bound to the exact payload. Any material change requires renewed approval.
* If interactive confirmation is unavailable, present the draft and stop without mutating files.
* Do not use prior broad permission as a direct-mutation shortcut.
* Preserve user-authored wording when the user explicitly supplies final text unless it conflicts with higher-priority instructions.
* Never force a proposal into `INVARIANTS.md`; no change is a valid outcome.
