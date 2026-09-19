# Bundled living progress-document instructions

These instructions adapt the local-progress-doc skill and its product/architecture completion contract for an already-authorized goal. They are bundled with Bcode; do not load a user-configured skill to replace them. Starting this goal authorizes document setup and the requested work, subject to ordinary permissions. Do not ask for a second document-creation approval or choose another path. Do not stage, commit, or change Git merely to create notes.

## Governing contract

Be narrow in requested product scope and uncompromising in production implementation depth. Derive the solution from the actual codebase. Complete every required product and architectural layer. Never substitute a shortcut, generic ideal, or speculative abstraction for the repository's proper long-term design. Scale the document down for investigation or note-taking objectives.

## Research before planning

Read the exact progress document and original objective. Inspect repository instructions and the relevant current implementation. Trace the existing end-to-end flow, identify owners, boundaries and canonical sources of truth, and inspect analogous implementations. Reference actual paths, tests and observed results. Resolve researchable gaps; explicitly record unresolved assumptions rather than inventing architecture or acceptance criteria.

Preserve the requested capability, intended entry point, observable result, boundaries and non-goals. Trace the completion path from entry point through existing product wiring and owning layers to requested behavior and observable useful result. An isolated component or demonstration is not product closure.

Include work only when necessary to deliver the requested behavior, connect the intended product surface, or satisfy an actually affected architectural/lifecycle obligation. Cover persistence, compatibility, migration, permissions, cancellation, recovery, errors and cleanup when genuinely triggered. Do not invent unrelated redesign, speculative frameworks, hypothetical use cases or unrequested polish. Required production integration is not optional follow-up.

## Required document structure

Maintain purpose; original objective and guidance; current status; requested product outcome and non-goals; repository findings and evidence; completion path; architectural obligations; dependency-aware implementation phases; product-completion verification; architectural-integrity verification; validation results; decisions, blockers and next actions.

Each phase MUST have a goal, dependencies where relevant, concrete Markdown CHECKBOXES (`- [ ]` / `- [x]`), exit criteria and validation expectations. Checkboxes describe verifiable outcomes, not vague intentions. Keep separate product-closure and architectural-integrity completion gates. Record actual validation commands, outcomes and relevant source/revision context. Missing verification stays unchecked.

## Living document protocol

The scaffold is unresearched, not an approved technical design. In the first implementation iteration populate it using repository evidence and meaningful goal-specific phases, then proceed with authorized useful work. At every subsequent iteration read it before choosing work, verify recorded state against current files and external evidence, and update it before finishing.

This is a living plan: add, split, reorder, refine or remove planned work as new evidence demands. Explain significant scope/plan changes in concise decision notes. Preserve completed-work evidence; reopen checkboxes when later evidence contradicts them. Never narrow the original objective to match completed work. Record the next concrete action and unresolved blockers so another iteration can resume without reconstructing the conversation.

Keep current sections concise instead of appending a transcript. Reference large logs/artifacts rather than embedding them. Keep the document below 64 KiB and use bounded reads/edits. Ordinary filesystem permissions apply only as authorized: the document path does not grant access to other Bcode state. Missing, corrupt or inaccessible notes must be surfaced, not silently overwritten or treated as completion. Reconstruct lost notes only with explicit authorization and current evidence.

The document is fallible working memory, not canonical session/workflow state. Updating checkboxes alone is not implementation progress. Never let a Done heading override missing evidence or a failed verification. Workflow status and the original objective remain authoritative for their respective purposes.
