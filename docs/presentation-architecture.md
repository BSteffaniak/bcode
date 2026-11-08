# Presentation architecture

Status: implemented, with the deviations listed under "Implementation status". This document is
the contract for Bcode's theme and presentation system; [`tui-themes.md`](tui-themes.md) is the
user-facing reference.

## Goals

* A theme can change how Bcode *looks* (color), how it is *shaped* (borders, icons, density), and
  how it is *structured* (what is collapsed, how much detail is shown, how items group).
* Every surface — host, bundled plugins, external plugins — participates through typed contracts.
  No surface needs hardcoded colors or hardcoded structural constants.
* Structure is renderer-neutral: the terminal and web frontends agree on what is collapsed and what
  a group contains, while each renders it natively.
* Themes stay declarative data. They cannot execute code and cannot affect authorization, dispatch,
  persistence, or execution outcomes.
* Normal interactive paths stay bounded: streaming, toggling, and resizing do bounded, local work.

## Non-goals

* A general cascade language. There are no nested selectors, combinators, or specificity rules.
* Reordering transcript content. Chronological order is product meaning, not presentation.
* Renderer-identical output. Renderers share structure, not pixels or cells.

## Layers

| Layer | Controls | Renderer-neutral | Owner |
|---|---|---|---|
| Paint | palette, semantic roles, derived colors | values yes; final styles no | theme definition; renderers convert |
| Chrome | border glyphs, icons, density, separators, motion | no | per-renderer section (`[chrome.tui]`) |
| Structure | disclosure, detail, part visibility, grouping, preview budgets | **yes** | `presentation-policy` |

A **palette** supplies paint. A **profile** supplies chrome and structure. A user selects a palette
and a profile independently, or a named theme that combines them. Every layer remains independently
overridable.

```toml
[tui.theme]
palette = "tokyonight-storm"
profile = "riced"
variant = "auto"
# `name` continues to select a combined theme definition.
```

## Crate ownership

* `bmux_color` (BMUX): terminal-independent color values and math — sRGB, OKLab/OKLCH conversion,
  mixing, lightness and chroma adjustment, relative luminance, contrast ratio, contrast
  enforcement, gradients, and hex parsing. It has no terminal, frame, or style types. Portable Bcode
  layers may depend on it; it must never depend on `bmux_tui*`, and the resolved dependency check
  enforces that transitively.
* `bcode_theme` + `bcode_theme_models` (renamed from `bcode_tui_theme`): the renderer-neutral
  definition format — parsing, inheritance, variants, overlays, discovery, palette expressions, role
  registry, `[presentation]` and opaque `[chrome.*]` sections. Colors are a portable enum, not
  `bmux_tui::style::Color`.
* `bcode_presentation_policy` + `bcode_presentation_policy_models`: structural facts, properties,
  rules, layering, compilation, resolution, the visibility floor, and the grouping projection.
  Independent of `bcode_session_view`'s canonical projection; consumes its documents.
* `bcode_tui`: conversion of resolved paint and chrome to BMUX styles and primitives, per-item
  disclosure overrides, scroll-anchor preservation, toggle hit regions.
* BMUX `tui-components`: generic `Disclosure` component, chrome inputs (border glyph sets, gradient
  borders, density), segmented status bar, icon slots with measured widths.

## Paint

### Seed palette

Themes define a standard set of seed keys. All other roles derive from seeds when unspecified.

| Seed | Meaning |
|---|---|
| `background`, `text` | canvas background and primary text |
| `accent` | primary interactive accent |
| `red`, `green`, `yellow`, `blue`, `magenta`, `cyan` | hue slots; drive state and syntax roles |
| `orange`, `violet` | optional additional hue slots |
| `surface0`, `surface1`, `surface2` | optional neutral ramp; derived from `bg`/`fg` when omitted |

Seeds may be `terminal` or ANSI values. A terminal-native theme defines seeds as ANSI colors and
lets the terminal own the actual RGB.

### Palette expressions

Palette and style values may use a bounded function set evaluated in OKLCH:

```toml
[palette]
diff_add_bg  = "mix($green, $bg, 0.18)"
selection    = "mix($accent, $bg, 0.28)"
muted        = "ensure_contrast(mix($fg, $bg, 0.45), $bg, 3.0)"
```

* Functions: `mix(a, b, t)`, `lighten(c, t)`, `darken(c, t)`, `saturate(c, t)`,
  `desaturate(c, t)`, `ensure_contrast(fg, bg, ratio)`. They map directly onto `bmux_color`.
* Amounts are fractions clamped to `[0, 1]`. In `mix`, `t` is the share of the first color, so
  `mix($green, $bg, 0.18)` is mostly background with a green tint. `lighten` and `darken` move
  lightness by that fraction of the remaining distance to white or black; `saturate` and
  `desaturate` scale chroma by `1 ± t`.
* `ensure_contrast` changes only lightness, by the minimum needed, moving toward whichever extreme
  contrasts more with `bg`; when the ratio is unreachable it yields that extreme.
* Arguments are palette references, literals, or nested calls; nesting depth is at most 4.
* References form a DAG; cycles are rejected with the full cycle path.
* Applying math to an ANSI, indexed, or `terminal` value emits a warning and yields the first color
  argument unchanged, unless the terminal has reported its actual palette (see terminal
  capabilities), in which case the reported RGB values are used.

### Role registry

A single registry lists every paint role: key, category, description, and fallback rule (another
role, a seed, or an expression). Plugins register namespaced roles (`git.*`, `shell.*`) with
fallbacks through their manifests.

* Unknown role keys produce warnings. `bcode theme validate --strict` makes them errors.
* Documentation tables, `bcode theme roles`, and any editor schema are generated from the registry.
* A behavioral test renders each registered surface under two maximally different palettes and
  asserts that every painted color belongs to that theme's resolved color set or is the terminal
  default. This replaces hardcoded-color source scanning.

### Generic component theme

`ComponentTheme` v2 (BMUX) adds `accent`, `border_focused`, `selected_inactive`, `hover`, `link`,
`title`, and per-state `{ fg, surface, emphasis }` styles for info, success, warning, and error.
`ComponentTheme::derive(seed)` builds a complete theme from seed colors. Every component styles type
offers `from_theme(&ComponentTheme, depth)`, and each `Default` implementation delegates to
`from_theme(&ComponentTheme::default(), Normal)`.

## Chrome

Chrome is renderer-specific and lives under `[chrome.<renderer>]`. Each value is a closed
enumeration mapped to BMUX primitives — never layout code.

```toml
[chrome.tui]
borders   = { panel = "rounded", dialog = "thick", tool = "left", composer = "rounded" }
focus     = { effect = "gradient", colors = ["$accent", "$violet"] }
icons     = "nerd"          # none | unicode | nerd
density   = "comfortable"   # compact | cozy | comfortable
separators = { status = "powerline" }   # plain | bar | slant | powerline
speaker   = "gutter"        # prefix | badge | bar | gutter
surfaces  = "layered"       # flat | layered
motion    = "subtle"        # none | subtle | full
```

* Icon sets always carry a Unicode or ASCII fallback. Widths are measured by BMUX; Nerd Font glyphs
  are treated as potentially ambiguous width and tested accordingly.
* `tui.reduce_motion = true` forces `motion = "none"`.
* Other renderers ignore `[chrome.tui]` and may define their own section.

## Structure

### Units and facts

A **unit** is anything a renderer presents: user message, assistant message, reasoning, tool
invocation, permission request, interaction, compaction, skill, activity, plugin status, and the
parts of a tool invocation (arguments, output, result, timing, diff, artifact).

Each unit exposes typed **facts** derived from the session-view document:

| Fact | Type |
|---|---|
| `kind` | unit kind |
| `tool`, `producer`, `schema` | strings; `schema` supports a trailing `/*` version wildcard |
| `status` | lifecycle status |
| `is_error` | bool |
| `turn` | `latest` \| `previous` \| `older` |
| `depth` | child-session nesting depth |
| `lines`, `items` | size buckets: `0`, `1`, `2..=10`, `11..=50`, `51..=200`, `>200` |

Plugins declare additional typed facts for their own tools in their manifests (for example,
`bcode.shell.exit_code: int`). Facts are presentation inputs only; they are never derived from raw
event logs and never feed authorization.

Size facts are bucketed so streaming content changes a fact only when it crosses a bucket boundary.

### Properties

A registry lists every structural property, its type, the unit kinds it applies to, and its
built-in default.

| Property | Values |
|---|---|
| `disclosure` | `expanded` \| `summary` \| `collapsed` \| `hidden` |
| `detail` | `minimal` \| `normal` \| `verbose` |
| `parts.<part>.visible` | bool |
| `preview` | `{ lines, keep = "head" \| "tail" \| "both" }`, in source lines |
| `group` | `none` \| `consecutive_same_tool` \| `consecutive_tools` |
| `placement` | `inline` \| `docked` |
| `container` | chrome recipe id |

`summary` shows the header plus a one-line summary. `collapsed` shows the header only. `hidden` is
restricted by the visibility floor. Preview budgets are source lines; renderers may clamp further to
their viewport.

Plugins register namespaced properties with typed schemas (`bcode.shell.show_exit_code: bool`).
Unknown properties produce warnings.

### Rules

```toml
[[presentation.rules]]
match = { kind = "tool", tool = ["read", "glob", "grep"], status = "succeeded" }
set   = { disclosure = "collapsed", group = "consecutive_same_tool" }

[[presentation.rules]]
match = { kind = "tool", status = ["running", "failed"] }
set   = { disclosure = "expanded" }
```

* `match` is a conjunction of facts. A list value matches any listed value. An empty `match`
  matches every unit.
* `set` patches properties. Merging is per property.
* Rules apply in order; a later rule overrides earlier values of the same property.
* Lifecycle behavior needs no special mechanism: when a fact such as `status` changes, the unit is
  re-resolved.

### Layers

From lowest to highest precedence:

1. host built-in property defaults;
2. plugin manifest defaults, scoped to that plugin's own units;
3. theme and profile `[presentation]` sections, including inheritance and overlays;
4. configuration `[tui.presentation]` (user, then repository, per the normal configuration merge);
5. interactive defaults persisted to user state (for example, "always collapse `read`").

Per-unit manual toggles are ephemeral renderer state applied over the resolved result, keyed by
stable item identity, and reconciled when items leave the resident window. Interactive defaults
persist only to user state and never modify declarative configuration.

Existing configuration (`tui.thinking`, `tui.diff_viewer`, `tui.interactions.placement`, the inline
tool preview limit, the composer row limit) maps onto built-in properties. Existing keys remain
accepted and feed layer 4.

### Visibility floor

Repository-local themes are untrusted data. The host enforces a floor that no layer can lower:

* units awaiting a user decision (permission requests, interactions) are `expanded` with their
  canonical operation facts visible;
* failed or errored units are never below `summary`;
* `hidden` units always leave a visible count marker (for example, "3 tool calls hidden");
* rules that attempt to cross the floor produce diagnostics naming the rule and layer.

The floor is applied after layering and after manual toggles.

### Resolution

Resolution is a pure function from compiled rules and unit facts to a resolved presentation.

* Rules compile once per policy fingerprint into an index keyed by `kind`, then `tool`, so resolving
  a unit evaluates only rules that can match it.
* Results are memoized by item identity, item revision, policy fingerprint, and override revision.
* A streaming update re-resolves only the streaming unit.
* A turn boundary re-resolves only units whose `turn` fact changed.
* Rule count, fact count, set entries, and plugin-declared schemas are bounded, consistent with
  existing theme limits.

### Grouping projection

Grouping is computed once in `bcode_presentation_policy` over the session-view transcript document.

* Group identity derives from member item identities, so it is stable across renderers and resizes.
* The projection updates incrementally from transcript patches and re-examines only the patched
  item's neighbors.
* A group has its own unit (kind `group`) with facts summarizing members, so rules can collapse or
  expand groups.
* Grouping never reorders items and never crosses turn boundaries.

### Plugin contract

* Manifests declare default rules, facts, and namespaced properties for the plugin's own tools.
* Tool presentation payloads may carry a bounded one-line `summary`. The host falls back to tool
  name, status, and duration.
* Renderer contexts (`SerializedTuiVisualContext`, `PluginTuiTheme`) gain resolved `disclosure`,
  `detail`, `preview`, plugin-namespaced properties, chrome, and the v2 component theme, behind
  contract version bumps. Plugins render what the host resolved; they do not evaluate rules.
* Older plugins receive the existing fields and continue to render with current semantics.

## Structure v2

Structure v2 replaces post-render row trimming with a unit anatomy, renders groups as nested
units, adds section-level disclosure, extends structure to every unit kind, and adds header,
summary, and spacing properties. Everything below is renderer-neutral unless it names the terminal.

### Unit anatomy

Every unit is produced as an anatomy, never as finished rows that are trimmed afterwards:

* **header** — ordered fields from a registry: `marker` (disclosure glyph), `icon`, `label`
  (for example `Tool result`), `tool`, `target`, `status`, `timing`, `count`, `title` (producer
  title). Renderers format fields; themes choose and order them.
* **summary** — one line. Sources, tried in order: `plugin` (producer-supplied summary),
  `first_line` (first non-blank line of the first visible `output`/`result`/`visual` section, else
  of any section), `template`.
* **sections** — named, ordered bodies. Host section names: `arguments`, `output`, `result`,
  `diff`, `artifact`, `metadata`, `visual` (a plugin-rendered body), `confirmation`, `error`.
  Plugins may add namespaced section names (`bcode.git.files`).

The terminal composer (`bcode_tui`) lays out an anatomy from the resolved presentation:

| Disclosure | Output |
|---|---|
| `expanded` | header, then each visible section with its own section disclosure |
| `summary` | header, then the summary line |
| `collapsed` | header only |
| `hidden` | nothing individually; a count marker belongs to the enclosing group or turn |

A section renders as: a section label row (`  output`, carrying a section marker when the section
is collapsible), then its body bounded by the section's preview. A collapsed section renders only
its label row with a `▸` marker and a row count (`  ▸ output · 14 rows`). The composer records a
disclosure anchor for each header and each collapsible section label (see interaction), so hit
regions derive from the same rows that are painted.

### Properties added in v2

```toml
[[presentation.rules]]
match = { kind = "tool", tool = ["read", "grep", "glob"] }
set   = { group = { by = "consecutive", bridge = ["reasoning"], min = 2 },
          sections = { arguments = { visible = false }, output = { disclosure = "collapsed" } },
          header = { fields = ["marker", "tool", "target", "status", "timing"] },
          summary = { source = ["plugin", "template", "first_line"], template = "{tool} {target}" },
          spacing = { before = 0, after = 1 }, indent = 2 }

[[presentation.rules]]
match = { kind = "group" }
set   = { disclosure = "collapsed", reveal = ["running", "waiting", "failed", "timed_out"] }
```

* `group` — a string shorthand (`none`, `consecutive_same_tool`, `consecutive_tools`) or a table:
  * `by`: `none` | `consecutive` (any tool) | `same_tool` | `same_producer` | `turn`.
    `consecutive_tools` = `{ by = "consecutive" }`; `consecutive_same_tool` = `{ by = "same_tool" }`.
  * `bridge`: unit kinds that may sit between members without ending the group (default
    `["reasoning"]` when `by` is not `none`). Bridged units stay in place and render inside the
    group when it is expanded; they are not members and do not count.
  * `min`: smallest run that forms a group (default 2, minimum 2).
* `reveal` (group units) — member statuses shown in place even when the group is collapsed or
  summarized (default `["running", "waiting", "failed", "timed_out"]`). Members that are awaiting a
  decision or failed are always revealed, whatever `reveal` says (visibility floor). Failed and
  awaiting members **stay members**: groups never split around them.
* `sections.<name>` — `{ visible, disclosure, preview }`. `parts.<name> = bool` remains accepted as
  an alias for `sections.<name>.visible`. A section's preview defaults to the unit's `preview`; its
  disclosure defaults to `expanded`. Unknown host section names without a namespace produce
  warnings, as unknown parts do today.
* `header.fields` — ordered field list; unknown field names produce warnings. Default:
  `["marker", "label", "tool", "status", "timing"]`, which reproduces today's tool header.
* `summary` — `{ source = [...], template = "..." }`. Templates substitute only registered field
  names in braces (`{tool}`, `{target}`, `{status}`, `{timing}`, `{count}`, `{title}`,
  `{first_line}`); unknown names produce warnings and render empty. No logic, conditionals, or
  formatting directives. Templates are bounded to 256 bytes; output is one line.
* `spacing` — `{ before, after }` blank rows, each 0..=2. Default `{ before = 0, after = 1 }`.
* `indent` — extra columns, 0..=8; members inside an expanded group get the group's indent plus
  two columns.

All properties merge per property (later rules win). `sections` merge per section and per field.

### Groups

* Grouping runs over the renderer's resident window in transcript order and never reorders units
  or crosses turns. A unit joins the open group when its resolved `group.by` equals the group's,
  the key matches (`same_tool`: tool; `same_producer`: producer; `consecutive`/`turn`: any tool
  unit), and every unit between it and the previous member is a bridge kind.
* The group unit's id is `group:<first member item id>`; it stays stable as members stream in.
* Group facts: `kind = "group"`, `tool` (when all members share it), `producer` (likewise), `status`
  (aggregate: running > waiting > requested > failed > timed_out > cancelled > succeeded),
  `is_error` (any member failed), `items` (member count bucket), `turn`, `depth`.
* Group header fields: `marker`, `count` (`5 tool calls`), `tools` (`read ×3, grep ×2`), `status`,
  `timing` (sum of member durations). Default header: `["marker", "count", "tools", "status",
  "timing"]`.
* Group disclosure:
  * `collapsed`: group header, then revealed members rendered with their own presentation;
  * `summary`: group header, then one row per member (each member's header fields, indented,
    each row a disclosure target for that member), revealed members rendered fully;
  * `expanded`: group header, then every member and bridged unit rendered with its own
    presentation, indented.
* Terminal rendering keeps one layout entry per transcript item: the first member's entry renders
  the group header plus its own member rendering; other members render their member rendering or
  zero rows. Each item's layout signature includes its group role (leader with group signature,
  member with group disclosure, or none), so a group change re-lays out only that group's items.

### Interaction

* Disclosure targets: `unit:<item id>`, `group:<first member item id>`, and
  `section:<item id>:<section name>`. Terminal hit-region ids are `disclosure:<target>`. The
  legacy form `disclosure:<item id>` remains accepted as `unit:<item id>`.
* The composer emits one disclosure anchor (`TuiVisualAnchor` with key `disclosure:<target>` and
  the header or label row) per target. Hit regions are registered only for visible anchor rows,
  clipped to the transcript area.
* Manual overrides are ephemeral, keyed by target, bounded (oldest dropped first), and reconciled
  when items leave the resident window. The visibility floor applies after overrides: a unit
  awaiting a decision cannot be collapsed.
* Mouse: clicking a header or section label toggles that target (expanded ↔ collapsed; summary
  and hidden expand first).
* Keyboard focus mode (`tui.transcript.focusUnits`, default `alt+u`): focuses the last visible
  structural target; `j`/`k` (and Up/Down) move between visible targets in transcript order,
  including group headers and section labels of expanded units; Enter/Space toggle; `l`/Right
  expands one level (group → summary → expanded; unit → expanded; section → expanded); `h`/Left
  collapses one level or moves focus to the parent target; `e` expands everything under the
  focused target; Escape leaves focus mode. The focused target's header row is painted with the
  selection style and kept in view. All keys are remappable in the `transcript_focus` scope.
* Bulk actions (`tui.tools.expandAll`, `collapseAll`, `resetDisclosure`) act on every unit and
  group; in focus mode they act on the focused target's subtree.

### Every unit kind is structural

`unit_facts` covers user messages, assistant messages, reasoning, tools, permissions,
interactions, compaction, skills, system and plugin status items. Messages support `disclosure`
(collapsed shows the speaker header plus nothing; summary shows the first line), `spacing`,
`indent`, and `header`. Reasoning disclosure absorbs the legacy `tui.thinking` visibility setting,
which feeds the configuration layer. New facts: `target` (tool path, command, or pattern from
producer-declared argument fields; host tools use `path`, `command`, `pattern`, `query`, `url`),
`duration` (bucket: `<1s`, `1s..10s`, `10s..60s`, `>60s`), `group_size` (members, on group units),
and `last_in_turn` (bool).

### Plugin contract v3

Tool presentation payloads may carry `summary` (one line, ≤ 256 bytes), `target` (≤ 256 bytes),
and named `sections` (ordered `{ name, label, rows }`). Visual adapters receive the resolved
presentation (unit disclosure, per-section disclosure and preview, header fields) in their render
context behind `PLUGIN_TUI_VISUAL_CONTEXT_VERSION = 3`. Older adapters render one `visual`
section and continue to work unchanged.

## Terminal capabilities

* BMUX queries the terminal's default foreground, background (OSC 10/11), and palette (OSC 4) with
  a bounded timeout. Reported colors drive `variant = "auto"` and resolve palette math over ANSI
  values. `COLORFGBG` remains the fallback.
* BMUX quantizes RGB to the 256-color or 16-color palette according to detected color depth, and
  drops color on monochrome terminals while preserving modifiers. ANSI and indexed values pass
  through unchanged.

## Tooling

* `bcode theme explain` and an in-TUI inspector report, for a selected unit, its facts and each
  rule and layer that set each property, including floor adjustments.
* `bcode theme check` reports contrast, unknown keys, roles resolved only through fallbacks, and
  registry coverage.
* Global actions: expand all, collapse all, cycle detail, and "make this the default for this
  tool".

## Compatibility

* Schema-v1 theme files parse and resolve exactly as before.
* Schema v2 adds seeds, expressions, `[chrome.*]`, and `[presentation]`. Unknown future schema
  versions are rejected, not interpreted.
* Plugin contract versions advance independently; legacy fields remain until consumers migrate.

## Invariant notes

* Presentation, including structure, never affects authorization, dispatch, persistence, or
  execution outcomes.
* Structural defaults are declarative configuration; interactive changes persist to user state.
* Portable crates depend on `bmux_color` only, never on `bmux_tui*`.
* Rich renderer adapters keep the generic structured fallback.

## Delivery phases

1. BMUX: `bmux_color`; `ComponentTheme` v2; component `from_theme`; gradients; color-depth
   quantization; OSC color queries; `Disclosure` component.
2. Bcode: `bcode_theme` split; seeds, expressions, role registry; `bcode_presentation_policy` with
   resolution, floor, and grouping projection.
3. TUI: tool and Markdown disclosure on the shared component; override state; settings migration;
   `explain`.
4. Plugin contract v2; plugin color and structure migration.
5. Catalog: palettes, profiles, two-axis picker, generated gallery.

## Implementation status

* Implemented: `bmux_color`; BMUX color-depth quantization, OSC color reports (delivered through
  managed and runtime terminal input), `ComponentTheme` v2 with `derive`, `ComponentChrome`,
  gradient borders and text, and the `Disclosure` component; schema v2 (seeds, expressions, role
  registry with warnings, `[chrome.tui]`, `[presentation]`, `kind`); `bcode_presentation_policy`
  with layering, floor, explain, and grouping; tool disclosure in the TUI with click toggles, bulk
  actions, preview budgets, part visibility, and turn recency; plugin component-theme contract v2;
  palette, profile, and combination catalog; `/profile` and `profile` configuration.
* Structure v2, implemented:
  * policy: the v2 properties (`group` tables, `reveal`, `sections`, `header`, `summary`,
    `spacing`, `indent`) with per-section merging, clamping, and registry warnings for section,
    header (checked per unit kind), and template names; the `target`, `duration`, and
    `last_in_turn` matchers; a rule index keyed by kind, then tool; the extended floor (decision
    sections, failure sections, groups never hidden) with diagnostics for rules that cross it;
    bounded single-line summary-template rendering;
  * grouping projection: `by`, `bridge`, and `min`; failed and awaiting members stay members and
    are revealed; group facts; incremental updates from transcript splices;
  * terminal tool units: built as an anatomy and laid out by a composer that implements the
    disclosure table, per-section disclosure and preview, header fields (including `target` and
    `icon`), summary sources, spacing, and indent, with unit and section disclosure anchors on
    painted rows;
  * terminal groups: each leader paints a one-row group header with a group anchor; members and
    bridged units render in full, as one header row, or not at all, following the group
    disclosure and `reveal`; the projection updates incrementally as the transcript changes;
  * every transcript kind is a structural unit with facts (messages, reasoning, permissions,
    interactions, notes, and system items), drawn through a structural frame; the `target`,
    `duration`, and `last_in_turn` facts are filled; hidden units of every kind leave a
    one-row count marker that is their disclosure target; `tui.thinking.show = false` is a
    configuration-layer rule, and reasoning it hides renders no marker (the user's explicit
    choice to omit reasoning, applied before the floor);
  * interaction: unit, group, and section disclosure targets and hit ids (the legacy form still
    parses); target-keyed overrides with oldest-first eviction, reconciled when items leave the
    resident window; the visibility floor applied after overrides (a failed unit collapsed by
    hand still shows its summary line); a group choice replaces its members' unit choices;
    mouse toggles for units, groups, and sections; keyboard focus mode with remappable keys;
    bulk actions on every shown unit kind and every group (hidden units and units awaiting a
    decision are left alone), scoped to the focused subtree in focus mode;
  * plugin contract v3: adapter response version 5 with bounded `summary`, `target`, and
    `sections`, rendered as the summary source, header target, and named anatomy sections;
    render context version 3 carrying the resolved presentation, decoded tolerantly (a
    presentation an older adapter cannot decode arrives as none); manifest `[presentation]`
    declarations, decoded and checked best-effort (invalid entries are dropped with
    diagnostics and never block plugin loading), whose rules become producer-scoped plugin
    layers and whose target fields feed the `target` fact only for units the declaring plugin
    produced (units without a known producer use a tool's declaration only when a single
    plugin declares it); bundled shell, filesystem, and git adapters supply summaries and
    targets;
  * chrome in the terminal: `icons` (header icon field and marker glyph set), `density` (a
    lowest-precedence host layer for tool and group spacing), and `borders.tool`;
  * layer order, lowest first: host defaults, plugins, theme, configuration, user state,
    stacked by one shared helper (`bcode_tui_theme::selection::structure_layers`);
  * tooling: `bcode theme explain` (text and JSON, stacking the same host, theme,
    configuration, and user-state layers as the TUI through the shared helper) and
    `bcode theme profiles`;
  * catalog: bundled profiles use groups, reveal, sections, header fields, summary templates, and
    spacing, including the `timeline` and `focus` profiles.
* Deviations: structural configuration lives in the renderer-neutral `[[presentation.rules]]`
  section rather than under `[tui]`. The definition crate keeps its `bcode_tui_theme` name and
  `bmux_tui` color type; splitting a renderer-neutral `bcode_theme` crate is deferred until a
  non-terminal renderer consumes theme files. `color_depth = "auto"` only honors `NO_COLOR`;
  reduced depths are explicit. Groups never contain a `group_size` fact; `items` buckets the
  member count. Only tool units can be group members.
* Not yet done: chrome `separators`, `speaker`, `surfaces`, `motion`, and the `composer` border
  style in the terminal renderer (they resolve and are validated, and BMUX bordered components
  honor the panel border set and focus gradient); per-item resolution memoization; plugin
  declared facts and properties; plugin rule layers in `bcode theme explain` (it does not load
  plugins, so it passes no plugin layers); mouse hit regions and layout signatures for
  namespaced plugin sections (keyboard focus reads every disclosure anchor, but hit regions
  and signatures probe only host section names); the in-TUI inspector; `bcode theme check`; the two-axis picker UI.
