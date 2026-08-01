---
id: local-progress-doc
name: Local Progress Document
version: 1.0.0
description: Draft an evidence-backed local progress document for an exact workflow interaction. The workflow host, not this skill, performs any approved write.
allowed-tools: filesystem.read, filesystem.find, filesystem.grep, question
---

# Local Progress Document

Draft a complete local progress document grounded in repository truth and the requested product outcome.

* Use only the exact read-only tools declared in front matter. Do not invoke shell commands.
* Do not write, edit, stage, or commit files.
* Return the exact proposed repository-relative path, complete desired Markdown content, SHA-256, and a concise preview in the node's structured output.
* The path defaults to `local-<workflow-slug>-progress.md`.
* The workflow must present the proposal through its exact durable Apply, Revise, or Cancel interaction.
* Apply routes the exact approved payload to `bcode.progress-doc`; Revise returns bounded guidance for another draft; Cancel does not mutate.
* Do not claim creation until the progress-document plugin returns a successful typed result.
