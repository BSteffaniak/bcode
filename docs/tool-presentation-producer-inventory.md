# Tool transcript presentation producer inventory

This inventory records production publishers after migration to invocation-owned primary transcript updates. Historical event decoding, compatibility projection, renderer consumption, and tests are not producers.

| Producer/API | Classification | Migration state |
|---|---|---|
| Shell recording checkpoints (`shell-plugin`) | Primary retained-latest, artifact-backed | Migrated |
| Filesystem request/file-change visuals (`filesystem-plugin`) | Primary retained-latest | Migrated |
| Vim request/playback/diff visuals (`vim-edit-plugin`) | Primary retained-latest | Migrated |
| Web search progress (`web-search-plugin`) | Primary retained-latest | Migrated |
| Document extraction progress (`document-plugin`) | Primary retained-latest | Migrated |
| OCR progress (`ocr-plugin`) | Primary retained-latest | Migrated |
| Git operation progress (`git-plugin`) | Primary retained-latest | Migrated |
| Worktree operation progress (`worktree-plugin`) | Primary retained-latest | Migrated |
| Provider-streamed request drafts declared by filesystem and Vim manifests | Primary retained-latest | Host routes directly to the invocation primary item; placement configuration was removed |
| `TransientProgressPublisher` | Legacy compatibility API | No production plugin callers; retained only for source/API compatibility and SDK tests |
| `ToolContributionPlaced` | Historical/supplemental compatibility event | No production plugin publishers; retained for historical decode and explicit supplemental/interactive consumption |
| Question/interaction exchanges | Interactive semantic exchange, not transcript presentation | Intentionally separate; no primary presentation publisher |
| Read-only filesystem operations | Canonical tool result plus optional plugin-owned primary request visual | Migrated through filesystem owner |
| Workflow/loop control | Host/runtime semantic state, not tool transcript presentation | No migration required |

Production searches used for this inventory:

* `ToolContributionPlaced` under `plugins/` has no publisher; the sole match is code-review TUI event consumption.
* `TransientProgressPublisher` and `transient_progress(` have no production callers outside `plugin-sdk`.
* `PrimaryPresentationPublisher` is used by shell, filesystem, Vim edit, web search, document, OCR, git, and worktree producers.
* Tool presentation manifest declarations remain only for filesystem and Vim request-draft schema ownership.

New primary request/progress/result placement publication is prohibited by `scripts/check-loop-runtime-architecture.sh` and `scripts/check-plugin-presentation-manifests.sh`.
