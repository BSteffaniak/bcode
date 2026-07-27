---
id: audit-invariants
name: Audit Invariants
description: Audit Bcode's invariant catalog for conflicts, overlap, ambiguity, misplaced policy, implementation-specific rules, and missing architectural boundaries, then interactively apply approved resolutions.
version: 0.1.0
activation:
  keywords:
    - audit invariants
    - conflicting invariants
    - invariant conflict
    - review invariants
    - resolve invariants
permissions:
  tools:
    - filesystem.read
    - filesystem.grep
    - filesystem.find
    - filesystem.edit
    - filesystem.write
    - question
---

# Audit Invariants

Use this skill for a comprehensive review of Bcode's invariant catalog. Detection does not presume rewriting: an audit may conclude that wording is sound or that an architectural decision is required before any edit.

## Authoritative definition

Read `INVARIANTS.md` first. Its introductory definition is authoritative. Read `AGENTS.md` to distinguish product and architecture conditions from contributor workflow, and inspect relevant `docs/`, scripts, and tests when evaluating established boundaries.

The catalog intentionally uses simple Markdown sections and bullets with optional bold titles. It does not require stable IDs, metadata blocks, enforcement fields, or related-document fields. Refer to an invariant by its section, bold title, and exact quoted text.

## Audit categories

Look for:

* **Direct contradiction:** two invariants require mutually exclusive outcomes.
* **Ownership conflict:** different layers are assigned the same responsibility.
* **Scope collision:** broad and narrow rules appear incompatible because their boundaries are unclear.
* **Duplicate or near-duplicate conditions:** multiple bullets express the same requirement without a meaningful distinction.
* **Compound invariant:** independently changeable requirements are combined in one bullet.
* **Misclassified policy:** contributor workflow or validation appears in `INVARIANTS.md`.
* **Implementation-specific rule:** current machinery is frozen when the intended condition is more general.
* **Unverifiable aspiration:** wording such as “fast,” “clean,” or “intuitive” lacks an evaluable condition.
* **Hidden exception:** phrases such as “where possible,” “normally,” or “unless necessary” avoid defining the actual boundary.
* **Missing invariant:** architecture documents repeatedly establish an important permanent boundary absent from the catalog.
* **Stale invariant:** the catalog no longer matches an explicitly approved architecture.

Do not promote every `must` statement in documentation. Migration instructions, validation commands, and implementation details belong elsewhere unless they express a durable condition of every valid design.

## Workflow

### 1. Establish the review scope

Unless the user narrows it, inspect the entire `INVARIANTS.md`, `AGENTS.md`, all relevant architecture documents under `docs/`, and architecture-check scripts. Use focused searches to find normative language and ownership statements; do not assume all such statements are invariants.

### 2. Build a conceptual catalog

For each invariant, identify internally:

* Subject and scope.
* Required condition.
* Responsible owner or layer.
* Prohibited state.
* Any stated or implied exception.

Compare conditions within each section and across sections.

### 3. Find and rank issues

Rank findings as:

1. Contradiction.
2. Architectural ambiguity or ownership conflict.
3. Duplicate or compound invariant.
4. Misclassification or stale implementation specificity.
5. Wording improvement.
6. Possible missing invariant.

Quote exact text and cite the section and relevant supporting architecture documents. Distinguish definite conflicts from plausible alternative interpretations.

### 4. Present findings for triage

Present a concise numbered list. For each item include:

* Category and severity.
* Conflicting or affected text.
* Why it matters.
* Candidate resolutions.

Use the Question tool to let the user choose one finding to resolve, approve reviewing findings one at a time, or stop. Do not batch mutation approvals.

### 5. Resolve one finding at a time

For the selected finding, inspect the supporting code and documentation deeply enough to determine whether one condition is stale, incorrectly scoped, duplicated, or misclassified.

Offer only relevant resolutions:

* Keep as written.
* Rewrite one invariant.
* Merge duplicate invariants.
* Split a compound invariant.
* Clarify both scopes.
* Move policy to `AGENTS.md`.
* Move mechanics or migration details to `docs/`.
* Remove an obsolete invariant.
* Add a missing invariant.
* Defer pending an architectural decision.

Never resolve a genuine product or architecture conflict based only on document order, apparent specificity, or convenience. Ask the user to choose when the options produce materially different architectures.

### 6. Approve the exact payload

Show the complete proposed edits for the current finding, including all affected files. Use the Question tool for an explicit approve, revise, or cancel decision. “Recommended” is not approval.

### 7. Apply and re-audit

Apply only the approved payload. Then re-read the edited sections and compare the complete catalog again. A resolution must not create a new contradiction elsewhere. Continue to the next finding only after reporting the result and receiving the user's choice.

### 8. Finish with unresolved decisions

Summarize:

* Resolved findings.
* Files changed.
* Remaining conflicts or ambiguities.
* Deferred architectural decisions.
* Validation performed.

## Rules

* Default to draft-only mode until the exact mutation payload is approved.
* Never act without user confirmation.
* Never skip a gate.
* Use a two-turn mutation barrier: finding, proposed payload, and direct approval precede edits.
* “Recommended” is not approval.
* Approval must come directly from the user through the Question tool in the current run; delegated, resumed, or inferred approval is invalid.
* Approval is bound to the exact payload. Any material change requires renewed approval.
* If interactive confirmation is unavailable, report findings and drafts without mutating files.
* Do not use prior broad permission as a direct-mutation shortcut.
* Process findings individually; never batch distinct resolutions into one approval.
* Preserve explicit user wording unless it conflicts with higher-priority instructions.
* Do not manufacture certainty. Label possible conflicts as ambiguous when more than one coherent interpretation exists.
* Existing violations and migration states are evidence to investigate, not implicit exceptions.
