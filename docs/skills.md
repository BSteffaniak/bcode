# Bcode Skills Architecture

Bcode skills are planned as reusable, discoverable capability packs that provide task-specific agent guidance, resources, and optional plugin-backed behavior without bypassing Bcode's existing plugin, tool, session, and permission architecture.

## Goals

* Robust: malformed or untrusted skills must not break startup or bypass permissions.
* Performant: Bcode must not load all skill bodies into every model turn.
* Maintainable: skill behavior should remain domain-driven and plugin/service-oriented.
* Extendable: folder-based skills, bundled skills, and plugin-provided skills should share one registry model.
* Compatible: support the common AI-coding-agent convention of a skill directory containing a markdown instruction file plus optional assets.

## Skill package format

Bcode recognizes skill directories using the following instruction-file priority:

```text
SKILL.md > skill.md > README.md
```

It also accepts flat markdown files directly under a skill source root, for example `skills/rust-debugging.md`; the skill ID is inferred from the file name when front matter omits `id`.

The baseline portable format is a directory with `SKILL.md`:

```text
skill-name/
  SKILL.md
  resources/
  scripts/
  examples/
```

`SKILL.md` uses TOML or YAML-like front matter followed by markdown instructions. Bcode should initially accept conservative key/value and array metadata, then preserve unknown fields for future compatibility where practical.

Example:

```markdown
---
id: rust-debugging
name: Rust Debugging
description: Diagnose Rust compiler, clippy, test, and runtime failures
version: 0.1.0
activation:
  keywords:
    - rust
    - cargo
    - clippy
permissions:
  tools:
    - filesystem.read
    - shell.run
---

# Rust Debugging

Use this skill when investigating Rust build, clippy, test, or runtime failures.

## Process

1. Inspect the exact error.
2. Identify the smallest relevant code path.
3. Prefer focused fixes.
4. Run the most relevant validation.
```

## Sources and precedence

Skill discovery should be layered and deterministic:

1. Repository-local skills: `.bcode/skills/`
2. Generic repository skills: `skills/`
3. Compatibility repository skills: `.claude/skills/` when enabled
4. User config skills: `${XDG_CONFIG_HOME}/bcode/skills/` or `~/.config/bcode/skills/`
5. User state skills: `${BCODE_STATE_DIR}/skills/`, `${XDG_STATE_HOME}/bcode/skills/`, or `~/.local/state/bcode/skills/`
6. Explicit configured paths from `bcode.toml`
7. Bundled skills from Bcode/plugin distributions

When duplicate IDs exist, higher-precedence sources shadow lower-precedence sources. Shadowed skills should appear in diagnostics rather than causing startup failure.

## Configuration

Initial global config shape:

```toml
[skills]
enabled = true
auto_activate = "suggest" # off | suggest | on
include_repo_skills = true
include_generic_repo_skills = true
include_user_skills = true
include_compat_claude_skills = true
max_context_bytes = 24000
max_skill_file_bytes = 262144
max_resource_file_bytes = 1048576
follow_symlinks = true

[skills.sources]
paths = []

[skills.disabled]
ids = ["experimental-skill"]
```

Per-agent overrides can be added after the global flow is stable:

```toml
[agent.plan.skills]
auto_activate = "suggest"
disabled = []

[agent.build.skills]
auto_activate = "suggest"
```

## Invocation modes

### Explicit invocation

Users can activate a skill directly:

```text
/skill rust-debugging
/skill rust-debugging diagnose cargo test failure
```

Explicit activation records a durable session event and causes the active skill context to be injected into subsequent model turns until deactivated or the session ends.

### Suggestion

In `suggest` mode, Bcode matches user prompts against indexed activation metadata and suggests relevant skills without applying them automatically.

### Automatic activation

In `on` mode, Bcode may automatically activate matching skills, but this should remain conservative and fully auditable through session events and traces.

## Plugin interface

Skills should have a versioned service interface so folder skills and plugin-provided skills can share one host path.

Interface ID:

```text
bcode.skill/v1
```

Operations:

* `list`: return compact `SkillSummary` values.
* `describe`: return validated metadata and diagnostics for a skill.
* `context`: return bounded prompt/context text for a skill activation.
* `invoke`: optional operation for plugin-backed behavior.

Folder-based skills can be implemented by a bundled provider plugin or by a server-side registry crate. Plugin-provided skills should not receive direct filesystem or shell privileges; they must still route execution through Bcode tools and permissions.

## Domain crates

When implementation begins, use domain-specific crates rather than generic shared crates:

```text
packages/skill/models
packages/skill
```

`packages/skill/models` should contain leaf data types only:

* `SkillId`
* `SkillSummary`
* `SkillManifest`
* `SkillSource`
* `SkillActivation`
* `SkillPermissionHints`
* `SkillContextRequest`
* `SkillContextResponse`
* `InvokeSkillRequest`
* `InvokeSkillResponse`
* `SkillError`

`packages/skill` should own discovery, parsing, validation, indexing, and context loading. It should not own TUI rendering, model-provider implementation, or session storage.

## Model-context behavior

Bcode must not inject every skill into every model request.

Expected flow:

1. Index compact summaries at startup or on demand.
2. Match or list summaries without reading large bodies.
3. Lazy-load full instructions only when a skill is activated or described.
4. Inject only active skill contexts into model turns.
5. Enforce `max_context_bytes` across all active skills.
6. Include provenance in injected context.

Injected context should be clearly delimited:

```text
Active Bcode skill: rust-debugging
Source: repo:.bcode/skills/rust-debugging/SKILL.md
Version: 0.1.0

Instructions:
...
```

## Session events

Add durable events so skill behavior is replayable and auditable:

* `SkillSuggested`
* `SkillActivated`
* `SkillDeactivated`
* `SkillContextLoaded`
* `SkillInvocationFailed`

Events should include skill ID, source, version when known, activation mode, and context byte counts where relevant.

## Permissions and safety

Skills are prompt/context packs by default, not privileged executables.

Rules:

* Skill permission metadata is advisory only.
* Filesystem and shell actions still go through existing tools and agent policy.
* Skill scripts are inert resources until explicitly invoked.
* Script execution, if added, must use a dedicated permission category such as `skill.script.execute` and should ask by default.
* Discovery must canonicalize paths and reject traversal outside the skill root.
* Skill context includes the exact `Skill file`, `Skill directory`, and `Skill resource root` so the model can read relative references on demand instead of Bcode eagerly inlining them.
* When applying a skill, Bcode instructs the model to resolve relative files/scripts/assets from the skill directory and to map common external tool names (`Bash`, `Read`, `Edit`, `Write`) to Bcode tools.
* Symlinks are followed by default for compatibility with Nix/Home Manager and similar config managers. Set `follow_symlinks = false` to opt out.
* For directory skills such as `skills/commit-message/SKILL.md`, Bcode infers `commit-message` from the parent directory when front matter omits `id`.
* For flat skills such as `skills/commit-message.md`, Bcode infers `commit-message` from the file stem when front matter omits `id`.
* malformed skills should produce diagnostics and be skipped, not crash startup.
* Session traces should record loaded skill source and byte counts.

## TUI UX

Initial slash commands:

```text
/skills
/skill <id>
/skill active
/skill off <id>
```

Later enhancements:

* command palette entries
* skill picker modal
* active-skill status chips
* skill details preview
* accept/dismiss UI for suggestions

## Performance requirements

* Use `BTreeMap`/`BTreeSet` for deterministic indexes.
* Avoid broad recursive scans outside configured source roots.
* Cache parsed summaries with file metadata or content hashes.
* Lazy-load instruction bodies and resources.
* Enforce max sizes for skill files, resource files, and model context contribution.
* Report diagnostics instead of failing entire registry builds.

## Implementation phases

1. Spec and docs: this document plus progress tracking.
2. Models and registry: add skill model types, parser, scanner, diagnostics, and tests.
3. Server/client IPC: list, describe, activate, deactivate, active skills.
4. TUI slash commands: `/skills`, `/skill`, `/skill active`, `/skill off`.
5. Model context integration: active skill injection with budgets and traces.
6. Plugin interface: `bcode.skill/v1` with list/describe/context/invoke.
7. Suggestions: keyword/rule matching and configurable activation mode.
8. Resources/scripts: resource loading and explicitly permissioned script execution.
