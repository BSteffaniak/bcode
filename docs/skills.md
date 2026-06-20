# Bcode Skills Architecture

Bcode skills are reusable, discoverable capability packs that provide task-specific agent guidance, resources, and optional plugin-backed behavior without bypassing Bcode's existing plugin, tool, session, and permission architecture.

## Goals

* Robust: malformed or untrusted skills must not break startup or bypass permissions.
* Performant: Bcode must not load all skill bodies into every model turn.
* Maintainable: skill behavior should remain domain-driven and plugin/service-oriented.
* Extendable: folder-based skills, bundled skills, and plugin-provided skills should share one registry model.
* Compatible: support common AI-coding-agent skill directories containing a markdown instruction file plus optional assets.

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

`SKILL.md` uses TOML or YAML-like front matter followed by markdown instructions.

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
```

### Model-invocation visibility

Skills are included in the model-visible skill catalog by default. To keep a skill available for explicit invocation while hiding it from automatic/model catalog discovery, set either spelling in front matter:

```yaml
disable_model_invocation: true
```

or the Pi/Agent Skills compatible spelling:

```yaml
disable-model-invocation: true
```

Hidden skills still appear in user-facing skill listings and can still be invoked directly unless disabled globally.

## Sources and precedence

Skill discovery is layered and deterministic:

1. Repository-local skills: `.bcode/skills/`
2. Generic repository skills: `skills/`
3. Compatibility repository skills: `.claude/skills/` when enabled
4. User config skills: `${XDG_CONFIG_HOME}/bcode/skills/` or `~/.config/bcode/skills/`
5. User state skills: `${BCODE_STATE_DIR}/skills/`, `${XDG_STATE_HOME}/bcode/skills/`, or `~/.local/state/bcode/skills/`
6. Explicit configured paths from `bcode.toml`
7. Bundled skills from Bcode/plugin distributions when present

When duplicate IDs exist, higher-precedence sources shadow lower-precedence sources. Shadowed skills appear in diagnostics rather than causing startup failure.

## Configuration

Global skill configuration:

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

### Skill catalog prompt configuration

Bcode injects a compact skill catalog into dynamic system context by default. This catalog contains metadata only; full skill bodies are loaded only when a skill is invoked or explicitly read.

```toml
[skills.prompt]
catalog = "summary" # off | names_only | summary
max_bytes = 8192
max_description_chars = 240
include_sources = true
include_keywords = false
```

Catalog behavior:

* `off`: do not add an available-skills catalog to model context.
* `names_only`: include IDs, names, and locations.
* `summary`: include IDs, names, descriptions, locations, sources, and optionally keywords.
* Skills with `disable_model_invocation = true` are omitted from the catalog.
* Catalog output is byte-bounded and may include a truncation marker.

The catalog uses an XML-style format similar to Pi/Agent Skills:

```xml
The following Bcode skills provide specialized instructions for specific tasks.
Use available Bcode filesystem/document tools to load a skill file when the task matches its description.
User skills are discovered from configured roots including ~/.config/bcode/skills when user skills are enabled.
When a skill file references a relative path, resolve it against the skill directory.

<available_skills>
  <skill>
    <id>rust-debugging</id>
    <name>Rust Debugging</name>
    <description>Diagnose Rust compiler, clippy, test, and runtime failures</description>
    <location>/Users/me/.config/bcode/skills/rust-debugging/SKILL.md</location>
    <source>user-config:skills</source>
  </skill>
</available_skills>
```

## System prompt configuration

The base system prompt can be used as-is or replaced while still keeping selected Bcode-managed sections.

```toml
[system_prompt]
mode = "default" # default | replace
text = ""

[system_prompt.sections]
repository_context = true
dynamic_repository_context = true
agent_suffix = true
skill_catalog = true
```

Behavior:

* `mode = "default"` uses Bcode's built-in coding-agent prompt as the base.
* `mode = "replace"` uses `text` as the base prompt.
* Enabled sections are appended to either base mode.
* `system_prompt.sections.skill_catalog = false` disables catalog injection even if `[skills.prompt]` is enabled.
* `skills.enabled = false` disables skill discovery and therefore disables the skill catalog.
* `skills.prompt.catalog = "off"` disables only the prompt catalog while keeping skills available elsewhere.

## Invocation modes

### Explicit invocation

Users can activate a skill directly:

```text
/skill rust-debugging
/skill rust-debugging diagnose cargo test failure
```

Explicit activation records durable session events and causes bounded skill context to be injected into model turns. Bcode formats invoked skill context as a visible XML-style block:

```xml
<skill id="rust-debugging" name="Rust Debugging" location="/Users/me/.config/bcode/skills/rust-debugging/SKILL.md">
References are relative to /Users/me/.config/bcode/skills/rust-debugging.
Source label: user-config:skills
Skill resource root: /Users/me/.config/bcode/skills
Version: 0.1.0

# Rust Debugging
...
</skill>
```

The skill body is bounded by `skills.max_context_bytes`. Relative references should be resolved against the skill directory using available Bcode filesystem/document tools.

### Suggestion

In `suggest` mode, Bcode matches user prompts against indexed activation metadata and suggests relevant skills without applying them automatically.

### Automatic activation

In `on` mode, Bcode may automatically activate matching skills, but this should remain conservative and fully auditable through session events and traces.

## Plugin interface

Skills use a versioned service interface so folder skills and plugin-provided skills can share one host path.

Interface ID:

```text
bcode.skill/v1
```

Operations:

* `list`: return compact `SkillSummary` values.
* `describe`: return validated metadata and diagnostics for a skill.
* `context`: return bounded prompt/context text for a skill activation.
* `invoke`: optional operation for plugin-backed behavior.

Folder-based skills are handled by the server-side registry. Plugin-provided skills should not receive direct filesystem or shell privileges; they must still route execution through Bcode tools and permissions.

## Domain crates

Skill implementation uses domain-specific crates rather than generic shared crates:

```text
packages/skill/models
packages/skill
```

`packages/skill/models` contains leaf data types only:

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

`packages/skill` owns discovery, parsing, validation, indexing, catalog formatting, and context loading. It does not own TUI rendering, model-provider implementation, or session storage.

## Model-context behavior

Bcode does not inject every skill body into every model request.

Flow:

1. Index compact summaries at startup or on demand.
2. Inject a bounded metadata catalog into dynamic system context when enabled.
3. Match or list summaries without reading large bodies.
4. Lazy-load full instructions only when a skill is activated or described.
5. Inject only active/invoked skill contexts into model turns.
6. Enforce `max_context_bytes` for active skill context.
7. Include provenance in injected context and session events.

## Session events and TUI visibility

Durable skill events include:

* `SkillSuggested`
* `SkillActivated`
* `SkillDeactivated`
* `SkillContextLoaded`
* `SkillInvocationFailed`

`SkillContextLoaded` includes the skill ID, source, byte count, truncation flag, and a bounded preview. The TUI renders a transcript item when context is loaded so users can verify which skill content was injected without storing the full skill body in the event log.

## Permissions and safety

Skills are prompt/context packs by default, not privileged executables.

Rules:

* Skill permission metadata is advisory only.
* Filesystem and shell actions still go through existing tools and agent policy.
* Skill scripts are inert resources until explicitly invoked.
* Script execution, if added, must use a dedicated permission category such as `skill.script.execute` and should ask by default.
* Discovery must canonicalize paths and reject traversal outside the skill root.
* Skill context includes the exact skill file, skill directory, and skill resource root so the model can read relative references on demand instead of Bcode eagerly inlining them.
* When applying a skill, Bcode instructs the model to resolve relative files/scripts/assets from the skill directory and to map common external tool names (`Bash`, `Read`, `Edit`, `Write`) to Bcode tools.
* Symlinks are followed by default for compatibility with Nix/Home Manager and similar config managers. Set `follow_symlinks = false` to opt out.
* For directory skills such as `skills/commit-message/SKILL.md`, Bcode infers `commit-message` from the parent directory when front matter omits `id`.
* For flat skills such as `skills/commit-message.md`, Bcode infers `commit-message` from the file stem when front matter omits `id`.
* Malformed skills produce diagnostics and are skipped, not allowed to crash startup.
* Session traces record loaded skill source, byte counts, and bounded previews.

## TUI UX

Skill commands:

```text
/skills
/skill <id>
/skill active
/skill off <id>
```

Current TUI behavior:

* `/skills` lists available skills.
* `/skill <id>` activates a skill.
* Loaded skill context produces status text and a transcript preview item.

Later enhancements:

* command palette entries
* richer skill picker modal
* active-skill status chips
* expandable skill details preview
* accept/dismiss UI for suggestions

## Performance requirements

* Use `BTreeMap`/`BTreeSet` for deterministic indexes.
* Avoid broad recursive scans outside configured source roots.
* Cache parsed summaries with file metadata or content hashes.
* Lazy-load instruction bodies and resources.
* Enforce max sizes for skill files, resource files, model-visible catalog output, and model context contribution.
* Report diagnostics instead of failing entire registry builds.

## Implementation status

Implemented:

* Skill model and registry crates.
* Folder and flat-file discovery.
* Skill summary listing and explicit activation.
* Dynamic system-context skill catalog.
* Prompt catalog configuration.
* System prompt replacement/section configuration.
* Pi-compatible `disable-model-invocation` support.
* XML-style explicit skill context injection.
* Session/TUI loaded-skill previews.

Still planned:

* Dedicated behavior tests for catalog formatting and prompt config combinations.
* Live filesystem reload/watch behavior for skill roots.
* Richer TUI expand/collapse UI for loaded skill context.
* Bundled/shared skill roots when there are concrete bundled skills to ship.
* Explicit script/resource execution flows with dedicated permission categories.
