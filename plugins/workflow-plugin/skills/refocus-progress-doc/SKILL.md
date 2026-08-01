---
id: refocus-progress-doc
name: Refocus Progress Document
version: 1.0.0
description: Reconcile an active progress document with repository truth and draft an exact replacement for a durable workflow interaction.
allowed-tools: filesystem.read, filesystem.find, filesystem.grep, question
---

# Refocus Progress Document

Inspect the current progress document and repository, then draft a concise replacement that preserves the requested product outcome, verified completed work, unresolved architecture, blockers, validation history, and the next execution-ready task.

* Use only the exact read-only tools declared in front matter. Do not invoke shell commands.
* Do not write, edit, stage, or commit files.
* Verify completed claims against repository evidence and reset any unverified checkbox.
* Return the exact repository-relative path, expected current SHA-256, complete desired Markdown content, desired SHA-256, and a concise preview in the node's structured output.
* The workflow must present the proposal through its exact durable Apply, Revise, or Cancel interaction.
* Apply routes the exact approved payload to `bcode.progress-doc`; Revise returns bounded guidance for another draft; Cancel does not mutate.
* Do not claim replacement until the progress-document plugin returns a successful typed result.
