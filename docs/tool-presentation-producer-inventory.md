# Tool transcript presentation producer inventory

This inventory records production publishers after migration to invocation-owned primary transcript updates. Historical event decoding, compatibility projection, renderer consumption, and tests are not producers.

Every current bundled presentation schema is consumed by both renderer-native registries. The TUI registry is selected through plugin manifests and `PluginTuiRegistry`; HyperChad's `VISUAL_ADAPTERS`/`ARTIFACT_ADAPTERS` coverage is checked against the same manifest inventory. Native adapters are optional enhancements: incompatible, unavailable, or malformed adapters must preserve the canonical bounded lifecycle/result fallback and must not expose opaque payload JSON.

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

## Renderer and fallback audit

| Producer | Presentation schema flow | TUI | HyperChad/web | Adapter-independent fallback |
|---|---|---|---|---|
| Shell | `bcode.tool.request.shell.run` → `bcode.shell.run` (artifact-backed recording/result) | Manifest-routed native adapter and optional terminal playback | Native request/result adapters and host artifact routes | Canonical tool name, status/timing, bounded arguments, typed/text result, and artifact title |
| Filesystem | `bcode.filesystem.request`, request-draft schemas, then operation-specific result schema such as `bcode.filesystem.change`, `read`, `image`, `exists`, `list`, `find`, `grep`, `stat`, or artifact operation result | Manifest-routed native adapters | Native visual/artifact adapters for every manifest-owned schema | Canonical lifecycle, bounded arguments, typed/text result, and safe artifact metadata |
| Vim edit | request/request-draft schema → `bcode.vim-edit.live` → `bcode.vim-edit.playback` | Native request, live, diff, and playback adapters | Native request, live, diff, and playback adapters | Canonical lifecycle, bounded arguments/result, and artifact title; playback is enhancement-only |
| Document | `bcode.document.request` → `bcode.document.extract_result` or `bcode.document.status` | Native adapters | Native visual/artifact adapters | Canonical lifecycle, bounded arguments/result, and artifact title |
| OCR | `bcode.ocr.request` → `bcode.ocr.extract_result` or `bcode.ocr.status` | Native adapters | Native visual/artifact adapters | Canonical lifecycle, bounded arguments/result, and artifact title |
| Web search | request schema → search/fetch/status/inspect result schema | Native adapters | Native visual/artifact adapters | Canonical lifecycle, bounded arguments/result, and artifact title |
| Git | `bcode.git.clone_request` → `bcode.git.clone_result` | Native adapters | Native visual/artifact adapters | Canonical lifecycle, bounded arguments/result, and artifact title |
| Worktree | `bcode.worktree.request` → list/create/remove result schema | Native adapters | Native visual/artifact adapters | Canonical lifecycle, bounded arguments/result, and artifact title |

Audit rules:

* Schema transitions replace the same primary invocation identity; they do not create request/live/final transcript objects.
* Manifest declarations are renderer-routing declarations, not ownership of lifecycle, retention, persistence, or transcript identity.
* HyperChad tests require its native registries to cover the current manifest schema inventory; TUI routing is validated by plugin manifest and registry tests.
* Read-only filesystem results may use canonical typed results as the durable semantic source; a native request presentation does not require duplicate durable output.
* Unknown dynamic-plugin schemas intentionally use canonical fallback until that renderer has an optional native adapter. Raw opaque payloads are not fallback content.
* Every plugin invocation must provide meaningful bounded canonical fallback through its tool name, lifecycle, retained arguments, terminal text or typed result, and safe artifact metadata. A presentation payload may enrich those fields but cannot be the only meaningful transcript result; a plugin that returns intentionally sparse canonical state is contract-incompatible because bundled and dynamic adapters remain optional.
* `ToolContributionPlaced` and placement-based projection remain historical/supplemental compatibility only.
* Legacy placement decode has no date-based removal target. Removal requires an explicit supported-history migration or version cutoff plus coordinated persisted-decoder, projection, fixture, documentation, and guard updates.
