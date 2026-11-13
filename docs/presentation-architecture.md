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
* `bcode_tui_theme`: the theme definition format — parsing, inheritance, variants, overlays,
  discovery, palette expressions, role registry, `[presentation]` and opaque `[chrome.*]` sections.
  It may use `bmux_tui` color types; renderer-neutral structure lives in
  `bcode_presentation_policy_models` (see Deviations).
* `bcode_presentation_policy` + `bcode_presentation_policy_models`: structural facts, properties,
  rules, layering, compilation, resolution, the visibility floor, and the grouping projection.
  Independent of `bcode_session_view`'s canonical projection; consumes its documents.
* `bcode_tui_transcript`: conversion of resolved paint and chrome to BMUX styles and primitives,
  per-unit disclosure overrides, scroll-anchor preservation, toggle hit regions; `bcode_tui`
  hosts it.
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
* Derivation is contrast-aware. One pairing table (`bcode_tui_theme::contrast::PAIRS`) names, for
  each text- or state-carrying role, the background it is painted on and its threshold;
  `bcode theme check` measures it and resolution enforces it. After seed derivation, each failing
  pair whose role was derived is raised with `ensure_contrast`: the role's own foreground when it
  sets one, otherwise (a tint painted under another role's text, such as state surfaces and diff
  rows) its background. Authored colors are preserved unless the theme sets
  `contrast = "enforce"`, which applies the same adjustment to failing authored roles. Resolved
  themes record each adjustment (`contrast_adjustments`).
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
speaker   = "gutter"        # prefix | badge | bar | gutter | marker
markers   = { user = "›", assistant = "•", bullet = "●", connector = "└" }
follow    = "reply"         # reply | latest: what stays in view as replies arrive
surfaces  = "layered"       # flat | layered
motion    = "subtle"        # none | subtle | full
header    = "shown"         # shown | hidden: the header row at the top of the screen
session_start = "shown"     # shown | hidden: the marker at the start of a session
status    = { placement = "above_composer", indent = 0, segments = ["activity", "context", "usage", "children", "progress", "plugins", "message", "position", "hints"] }
activity  = { placement = "status", spinner = "⠋⠙⠹", verbs = [], shimmer = false }
dialogs   = { placement = "modal", actions = "buttons" }   # modal | docked; buttons | numbered
menus     = { frame = "framed", marker = "›", tags = "shown", rows = 6 }   # framed | plain
markdown  = { bullet = "•", marker_gap = 2, code_blocks = "framed", quote_bar = "│", table_headers = "column", smart_punctuation = true }
composer  = { prompt = "❯ ", title = "shown", placeholder = "Ask…", controls = "shown" }
```

* Border styles also include `rules`: full-width horizontal rules above and below with no sides
  (the composer, tool containers).
* Chrome text (`prompt`, `placeholder`) is one line of at most 48 bytes without control
  characters. The prompt is drawn in the two leading cells every composer frame keeps before the
  editor; a prompt measuring wider is not drawn, so it never changes the editor's geometry.
* A hidden header gives its row to the transcript. A hidden session-start marker keeps its row
  and paints it blank, so transcript geometry never depends on boundary state.
* `speaker = "marker"` drops the heading row of expanded user and assistant messages: the
  speaker's marker glyph (styled by its label role) precedes the first body row, and the body
  hangs beside it. The marker column is the glyph's measured width plus one cell; narrower than
  four body cells, and for collapsed or summarized messages, the heading row returns.
* `status.segments` lists the shown status segments in display order (unknown or repeated
  names are rejected); the default shows every segment in the order above.
* `activity.placement = "line"` moves what the agent is doing from the status line to its own
  row directly above the composer block, present only while busy. `spinner` sets the spinner
  frames (one grapheme each); `verbs` replaces the detailed activity label with one verb per
  busy period followed by the period's elapsed seconds; `shimmer` sweeps a three-cell
  `activity.shimmer` highlight across the text unless motion is `none`. The `activity.spinner`,
  `activity.text`, and `activity.shimmer` roles style the row.
* `markdown` sets Markdown structure: the unordered list bullet, the spaces after list markers
  (0..=4), `code_blocks = "plain"` for unframed fenced code, the quote bar, `table_headers =
  "center"`, and `smart_punctuation = false` to keep quotes, dashes, and ellipses as written.
  The `markdown.code.<token>` roles color fenced code, falling back to `syntax.<token>`, so code
  blocks and diffs can use different palettes.
* `dialogs.placement = "docked"` shows permission requests at the bottom of the screen in place
  of the composer and status line, with the transcript visible above, instead of a modal:
  an accent rule, the title, the request's subject between light rules, the question, the
  actions, and a hint row. `dialogs.actions = "numbered"` lists one action per row with a
  cursor, chosen with the arrow keys, Enter, a click, or the action's digit. One layout gives
  the painted rows, the hit regions, and the dock height.
* `menus` draws completion menus such as the slash command menu: `frame = "plain"` lists rows
  with no border, title, or fill, the selected row in the focused style and the others muted,
  with commands padded to one column; `marker` is the selected-row glyph, `tags` shows or hides
  item kind tags, and `rows` (1..=16) bounds the rows shown. Painting and mouse selection share
  one menu area.
* `follow = "reply"` holds a new submission's or reply's first row at the viewport top until it
  overflows; `latest` keeps the latest content at the bottom instead. Neither pulls a reader who
  scrolled away from the latest content; a submission returns to it under `latest`.
* A container recipe with `layout = "panel"`, `border = "none"`, and `width = "full"` fills its
  rows edge to edge with its role's background.
* The `composer.border` and `composer.surface` roles style the composer; unset, the composer
  uses the focused accent and `surface.raised`.

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

Plugins declare additional typed facts for their own units in their manifests (for example,
`bcode.git.files: int`), named under their plugin id. Facts are presentation inputs only; they are
never derived from raw event logs and never feed authorization. Rules match them under `plugin`:
by value, by one of a list, or, for integers, by a `{ gt, gte, lt, lte }` range
(`match = { plugin = { "bcode.git.files" = { gt = 10 } } }`). Only units the declaring plugin
produced carry its facts.

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

Plugins register namespaced properties with typed schemas and defaults
(`bcode.git.show_destination: bool = true`), set by rules as
`set = { plugin = { "bcode.git.show_destination" = false } }`. A plugin property applies only to
its plugin's units; values that violate the schema are ignored with a warning, and unknown
properties produce warnings.

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
  Layout signatures cache each item's rows on those inputs, and the compiled policy memoizes
  resolutions by the set of matching rules plus the facts the visibility floor reads (unit is a
  group, awaiting a decision, failed). That memo belongs to one compilation, so recompiling
  invalidates it; it holds at most `MAX_RESOLUTION_MEMO` (256) entries and is cleared when full.
  Overrides apply after the memo, so toggles never invalidate it. Units that match the same rules
  share one patch application, so the memo stays small no matter how long the transcript is.
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

The terminal transcript's unit producers (`bcode_tui_transcript`) lay out an anatomy from the
resolved presentation:

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

* `header.template` — a header row from a template instead of `fields`: the same registered
  names in braces, other text literal, at most 256 bytes. Template headers place fields with no
  joiner and style each field by its role: `bullet` and `icon` by the unit's status title,
  `tool` and `label` by `tool.label`, `target` by `tool.argument`, `count` by `tool.count`
  (bold `tool.metadata` when unset), and everything else, literals included, by
  `tool.metadata`.
* Header fields added for templates and field lists: `bullet` (the `markers.bullet` glyph in
  the status title style), `elapsed` (a finished unit's bare duration, `1.5s`), and, for groups,
  `plural` (`s` unless the group has one member).
* `display_name` — the name `{tool}` presents (at most 64 bytes); unset presents the tool name.
* `section_labels` — `rows` (default) gives each section a label row; `connector` drops label
  rows and paints the `markers.connector` glyph before the first body row, with every body row
  hanging after it. Paint, hit regions, and source mapping share the hanging column; where the
  connector does not fit, it is not painted.
* `hidden_marker` — `false` makes a hidden unit leave no count marker row. The visibility floor
  still keeps decisions and failures visible.
* `group.min` may be 1: a run of one unit is presented through its group header, so a group
  header template can phrase single calls (`Read {count} file{plural}`). Unset, `min` is 2.

All properties merge per property (later rules win). `sections` merge per section and per field.

### Groups

* Grouping runs over the renderer's resident window in transcript order and never reorders units
  or crosses turns. A unit joins the open group when its resolved `group.by` equals the group's,
  the key matches (`same_tool`: tool; `same_producer`: producer; `consecutive`/`turn`: any tool
  unit), and every unit between it and the previous member is a bridge kind or a unit attached to
  a member's invocation.
* Only tool calls are members. Supplemental and hidden tool contributions (plugin visuals and
  retained contributions attached to an invocation) are *attached units*: they never lead, join, or
  count toward a group (`count`, `tools`, `items`, `status`, and `is_error` ignore them). An
  attached unit that belongs to the invocation of a member already in the run stays inside the
  group, whatever its `bridge` kinds or its own status, between members and directly after the
  last member. An attached unit of any other invocation follows the ordinary bridge rules (a
  bridge kind, not failed, not awaiting a decision), so a foreign failure still ends the group
  rather than hiding inside it. An attached unit renders with the member whose invocation it belongs
  to: fully when that member renders fully or the group is expanded, otherwise not at all (the
  member's header row stands for the whole invocation). Request, progress, and result
  contributions occupy the invocation's primary slot (they are how a plugin-presented call
  renders), so they remain members.
* An invocation counts once. When a historical tool request (`tool-request:<call>`) and a later
  item of the same invocation (its result or primary contribution) are both in the window, the
  projection marks the request *superseded*: the later item is the member, and the request is an
  attached unit of it. Superseded units sit inside a group between members whatever their kind or
  status, and a contiguous span of them directly before the first member, in its turn, is claimed
  too; the group header is then presented before the first of them. Superseded units after the
  last member are left for the group of the call they belong to.
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
* The terminal transcript (`bcode_tui_transcript`) presents a group as its own group-header
  unit before the first member (and a hidden marker unit for hidden runs); members stay their
  own units. Structure is resolved per changed unit and enters only that unit's layout
  fingerprint, so a group change re-lays out only the group's units
  ([TUI V2 architecture](tui-v2-architecture.md) §3.3, U5 and L3).

### Interaction

* Disclosure targets: a unit (by item id), a group (by its first member's item id), and a named
  section of a unit (`DisclosureTarget::{Unit, Group, Section}` in `bcode_tui_transcript`).
* Header and section-label rows of the retained item layout carry their target: the same rows
  are the keyboard focus targets and the `ItemAction::ToggleDisclosure` hit regions, registered
  only for visible rows and clipped to the transcript area (L6, B3). A unit's layout fingerprint
  includes the disclosure state recorded for it, sections included, so toggling a target
  re-lays out only that unit.
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
* Detail cycling (`tui.transcript.cycleDetail`; `alt+d` in the chat scope, `d` in focus mode)
  moves disclosure collapsed → summary → expanded → collapsed (hidden enters at summary): every
  unit and group together outside focus mode, continuing from the last level applied to all,
  and only the focused target in focus mode. Levels the visibility floor refuses are skipped.
* Inspector (`tui.transcript.inspect`, `i` in focus mode): explains the focused target and
  follows focus. It paints over the transcript body, docked right as a side panel when the
  body is at least 96 columns wide and inset as a modal otherwise; its hit region is the
  painted panel, layered above transcript regions, and the mouse wheel over it scrolls it
  (`shift+Down`/`shift+Up` from the keyboard). Its scrollbar, shown when the report overflows,
  is clickable and draggable: a press on the inspector panel routes, ahead of transcript
  selection, to BMUX's text view against the body rectangle and layout recorded by the last
  paint, so a track press jumps there and starts a drag that continues outside the panel until
  release. Closing the inspector or a panel that no longer fits ends the drag and leaves no
  geometry to route through. Escape closes it before leaving focus mode, and
  leaving focus mode any other way closes it too. Transcript selection is registered before the
  panel paints, so its occlusion removes the selectable fragments it covers; beside a side panel
  selection still starts, while an inset panel disables starting a transcript selection until it
  closes.
* Make default (`tui.transcript.makeDefault`, `m` in focus mode) on a focused tool unit saves
  `{ match = { kind = "tool", tool = "<name>" }, set = { disclosure = "<shown>" } }` to user
  state through the effect queue in one write, replacing a stored rule with the same matcher,
  then recompiles from the stored user-state rules. Every user-state read-modify-write (theme,
  profile, streaming, and structural rules) is serialized in-process and replaces the file
  atomically, so concurrent saves never drop each other's fields. The three actions are also command-palette
  entries (`transcript.inspect`, `transcript.cycleDetail`, `transcript.makeDefault`), as are
  focus mode (`transcript.focusUnits`) and the bulk actions (`tools.expandAll`,
  `tools.collapseAll`, `tools.resetDisclosure`); each palette route runs the same handler as its
  keymap action.

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

Manifests declare facts and properties for the plugin's own units:

```toml
[[presentation.facts]]
name    = "bcode.git.already_exists"   # <plugin id>.<key>
type    = "bool"                       # bool | int | string
payload = "already_exists"             # or: argument = "<top-level argument field>"
schema  = "bcode.git.clone_result"     # optional contribution schema filter; also `tool`

[[presentation.properties]]
name    = "bcode.git.show_destination"
type    = "bool"                       # bool | int (optional min, max) | enum (values)
default = true
```

* A fact's value is read by the host from one top-level field of the unit's tool arguments or
  contribution payload: `bool` from a JSON boolean, `int` from an integer or the length of an
  array or object, `string` from a string bounded to one line of 256 bytes. Facts are derived
  only for units whose known producer is the declaring plugin, at most 32 per unit, memoized per
  item revision. They are not taken from adapter render responses: an adapter's output depends on
  the resolved presentation, so facts derived from it could feed back into resolution.
* Property defaults become a plugin layer (one rule scoped to the plugin's units) stacked just
  below the plugin's own rules, so explain attributes a default to that layer. Schemas are
  registered with the policy registry, which drops schema-violating values with a warning and
  scopes every registered property to its plugin's units; an unregistered property is reported
  as unknown and applies to no unit. When plugin ids nest (`bcode` and `bcode.git`), a fact or
  property name belongs to the plugin with the longest id it is namespaced under.
* Names are `<plugin id>.<key>`, at most 64 bytes; each plugin declares at most 32 facts and 32
  properties; enums have at most 16 values. Invalid declarations are dropped with manifest
  diagnostics and never block loading.
* Adapters receive the unit's resolved plugin properties as `properties` in the context
  presentation (at most 64 entries). The field was added within context version 3: older
  adapters ignore it and older contexts decode it as empty, so no version bump is needed.

## Terminal capabilities

* BMUX queries the terminal's default foreground, background (OSC 10/11), and palette (OSC 4) with
  a bounded timeout. Reported colors drive `variant = "auto"` and resolve palette math over ANSI
  values. `COLORFGBG` remains the fallback.
* BMUX quantizes RGB to the 256-color or 16-color palette according to detected color depth, and
  drops color on monochrome terminals while preserving modifiers. ANSI and indexed values pass
  through unchanged.

## Tooling

* `bcode theme explain` and the in-TUI inspector report, for a selected unit, its facts and each
  rule and layer that set each property, including floor adjustments. Both use the policy's own
  `explain` and one shared property flattening (`bcode_tui::inspector::property_rows`); the
  inspector also reports the target's manual override, its shown disclosure (group-adjusted for
  members), and grouping context, and explains groups from their synthesized group facts.
* `bcode theme check [THEME] [--strict] [--json]` resolves a theme in both variants (once when
  they resolve identically) and reports WCAG contrast for text, accent, status, diff, and
  selection role pairs (4.5:1 for text and states; 3:1 for secondary text, focus borders, diff
  signs, tool titles, and inactive selection); unknown style roles and container keys; roles
  resolved only through seed derivation or unset (host fallbacks); registry coverage; resolver
  warnings; and the theme's structural rule diagnostics, including floor crossings, compiled with
  the enabled plugins' fact and property schemas (read from manifests, as explain does). Colors whose
  displayed value the terminal owns (the default colors and the 16 ANSI colors) are reported as
  unmeasured, not failed. Each contrast adjustment resolution made (role, pair background, fg or
  bg, before and after color, pair ratio before and after) is an informational `adjusted`
  finding and an `adjustments` entry in `--json`; adjustments never count as warnings. Warnings
  exit 0 unless `--strict`; `--json` emits a versioned structure. The analysis is `bcode_tui_theme::check`; the command compiles rule layers.
* Global actions: expand all, collapse all, cycle detail, and "make this the default for this
  tool" (see Interaction).
* `cargo run -p xtask -- theme-gallery [--check | --dry-run]` writes the generated gallery,
  [`theme-gallery.md`](theme-gallery.md). It renders the appearance picker's sample transcript
  through the transcript engine's unit producers (`bcode_tui_transcript::sample`) into a
  headless 80×112 BMUX buffer under each selected
  combination: every selectable bundled theme alone (dark variant), plus every bundled profile
  over the default theme and `bcode`. Renders are ANSI-free text, so identical renders are
  printed once with every combination that produces them (at most 64), and swatch tables give
  each theme's core role colors per variant, naming terminal-owned colors instead of guessing
  them. `--check` fails when the committed page is stale. The renderer is the `bcode_tui`
  `theme_gallery` example (`bcode_tui::theme_gallery`), run as a separate Cargo invocation so
  xtask's own graph never includes the TUI; rendering takes well under a second, and the cost
  of `--check` is compiling `bcode_tui` (shared with a workspace build).

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
* Plugin presentation (manifest rules, summaries, targets, sections) is display data scoped to
  the plugin's own units; manifest rules naming another producer are dropped.

## Delivery phases

1. BMUX: `bmux_color`; `ComponentTheme` v2; component `from_theme`; gradients; color-depth
   quantization; OSC color queries; `Disclosure` component.
2. Bcode: seeds, expressions, role registry; `bcode_presentation_policy` with
   resolution, floor, and grouping projection.
3. TUI: tool and Markdown disclosure on the shared component; override state; settings migration;
   `explain`.
4. Plugin contract v2; plugin color and structure migration.
5. Catalog: palettes, profiles, two-axis picker, generated gallery.

## Implementation status

All five delivery phases have landed. This section records what exists, where it departs from
the design above, and what remains.

### Paint and chrome

* BMUX: `bmux_color`; color-depth quantization; OSC 10/11/4 color reports (delivered through
  managed and runtime terminal input); `ComponentTheme` v2 with `derive`; `ComponentChrome`;
  gradient borders and text; the `Disclosure` component.
* Theme schema v2: seeds, palette expressions, the role registry with warnings, `[chrome.tui]`,
  `[presentation]`, and `kind`. Schema-v1 files resolve unchanged. Plugin component-theme
  contract v2.
* Contrast-aware derivation against the shared pairing table; every bundled theme passes
  `bcode theme check --strict` in both variants. Nord opts in with `contrast = "enforce"`, which
  lightens its authored error red (`#bf616a` to `#e17f87`) to reach 4.5:1 on its canvas.
* Every `[chrome.tui]` key is honored by the terminal renderer:
  * `borders.panel` and `borders.dialog` select surface border glyphs, and `focus` (`gradient`,
    `sweep`) paints focused borders with a gradient;
  * `borders.tool` selects the tool unit container (`none`, `left`, or a framed panel). Every
    transcript container is painted by BMUX's `LineFrame` with the selected glyph set; its
    geometry also gives producers their content width and column, so painting, wrapping and hit
    regions share one layout definition. Layered surfaces become the frame's fill;
  * `borders.composer` frames the input composer: framed styles change the glyphs, `left` paints
    a leading bar, and `none` paints nothing. Only full frames carry the `Message` title. Every
    style keeps the editor geometry, so composer height, editor hit regions, and cursor placement
    do not depend on it;
  * `icons` selects the header icon field and marker glyph set, with measured Unicode and ASCII
    fallbacks;
  * `density` is a lowest-precedence host layer for tool and group spacing;
  * `speaker` applies to user and assistant messages in every disclosure: `prefix` keeps the
    heading row; `badge` paints a reversed badge, degrading to the heading row when it does not
    fit; `bar` paints a one-cell bar and gap on every row; `gutter` moves the heading into a fixed
    8-cell gutter truncated by display width. One speaker layout
    (`SpeakerLayout` in `bcode_tui_transcript::layout::producers::message`) feeds both painting
    and the transcript item layout,
    so Markdown width, hit regions, and selection origins follow the body. `bar` degrades to
    `prefix` below 6 columns and `gutter` below 20;
  * `separators.status` joins status line segments with a glyph measured at a constant three
    cells, so switching styles never changes which segments fit. `plain` keeps the middle dot;
    Nerd Font slant and powerline glyphs fall back to Unicode, then ASCII, by the icon set's rules;
  * `surfaces = "layered"` gives tool units and group headers the `surface.raised` background
    when it differs from the canvas, without changing geometry. Dialogs already use
    `surface.overlay`; `flat` keeps the existing look;
  * `motion` sets the activity tick (`none`: 1s with a still spinner; `subtle`: 100ms, the
    default; `full`: 80ms) and advances a `sweep` focus gradient on those ticks. Ticks are
    scheduled only while activity is shown, so an idle UI schedules no frames. Every frame,
    including canvas-owning foregrounds, paints with the same phase; a sweep tick repaints the
    whole screen only while chrome with a focus gradient was painted since the last full frame,
    and otherwise repaints just the status line.
    `motion` also sets the durations of accent transitions (`subtle`: 220ms; `full`: 330ms),
    the transcript scroll animation (180ms; 270ms), and the latest-bar glow (420ms; 630ms);
    under `none` all three are instant and schedule no frames. When the effective motion turns
    instant (a theme change, a config apply, or a runtime `reduce_motion` toggle) while one is
    running, it settles at once: the scroll lands at its target, the latest bar shows the still
    bar, and the accent shows its target, with no further frames. Precedence, highest first:
    `tui.reduce_motion` (always instant), then an explicit user setting for that animation
    (`[tui.theme] accent_transition = "immediate"` or `accent_transition_ms`; the scroll and
    latest-bar animations have none), then the theme's `motion`. Accent transitions use the
    target theme's level. `tui.reduce_motion` forces `none`. Separators, surfaces, and motion
    live in `theme::effects`.

### Structure

* `bcode_presentation_policy`: layering, the visibility floor, explain, and the grouping
  projection; the v2 properties (`group` tables, `reveal`, `sections`, `header`, `summary`,
  `spacing`, `indent`) with per-section merging, clamping, and registry warnings for section,
  header (per unit kind), and template names; the `target`, `duration`, and `last_in_turn`
  matchers; a rule index keyed by kind, then tool; the extended floor (decision sections,
  failure sections, groups never hidden) with diagnostics for rules that cross it; bounded
  single-line summary templates; a bounded resolution memo (see "Resolution cost").
* Layer order, lowest first: host defaults, plugins, theme, configuration, user state, stacked
  by one helper (`bcode_tui_theme::selection::structure_layers`) shared by the TUI, `bcode theme
  explain`, and `bcode theme check`.
* Grouping: `by`, `bridge`, and `min`; failed and awaiting members stay members and are
  revealed; group facts; incremental updates from transcript splices that match full
  recomputation. Only real tool calls are members. Supplemental and hidden tool contributions
  are attached units: they never lead, join, or count, stay inside the group when they belong to
  a member's invocation (others must qualify as ordinary bridges), and render with their owning
  member.
* Terminal units: every transcript kind (tools, messages, reasoning, permissions,
  interactions, notes, system items) is a structural unit with facts, drawn through the unit
  anatomy. Tool units are laid out by a composer that implements the disclosure table,
  per-section disclosure and preview, header fields (including `target` and `icon`), summary
  sources, spacing, and indent. Hidden units leave a one-row count marker that is their
  disclosure target, except reasoning hidden by `tui.thinking.show = false` (a
  configuration-layer rule).
* Terminal groups: each leader paints a one-row group header with a group anchor; members and
  bridged units render in full, as one header row, or not at all, following the group
  disclosure and `reveal`.
* Interaction: unit, group, and section targets (the legacy hit id still parses);
  target-keyed, bounded overrides reconciled when items leave the resident window; the floor
  applied after overrides; mouse toggles; keyboard focus mode; bulk actions; detail cycling;
  the inspector; and "make this the default for this tool" (see Interaction). Hit regions,
  host and plugin-namespaced sections alike, come from the same retained anchors focus walks.
* Plugin contract v3: adapter response version 5 (`summary`, `target`, `sections`); render
  context version 3 carrying the resolved presentation, decoded tolerantly; manifest
  `[presentation]` declarations decoded best-effort, whose rules become producer-scoped plugin
  layers and whose target fields feed the `target` fact only for the declaring plugin's units.
  Tool requests, drafts, contributions, results, and permission requests carry their producing
  plugin when the session knows it; only units with a genuinely unknown producer fall back to a
  tool's declaration when exactly one plugin declares it. Plugin layers and target fields are
  re-derived whenever the plugin catalog is reloaded, keyed by the effective declarations, so an
  unchanged catalog recompiles nothing. Bundled shell, filesystem, and git adapters supply
  summaries and targets.
* Plugin facts and properties (see Plugin contract v3): manifest declarations, the registry
  with typed schemas, integer range matchers, property defaults as plugin layers, scoping to the
  producer's units, and delivery to adapters in the context presentation. The inspector lists a
  unit's plugin facts and its plugin properties with their origins; `bcode theme explain`
  registers the same schemas, accepts plugin facts as namespaced `KEY=VALUE` arguments (kept only
  when `producer` is the declaring plugin, interpreted by the declared type), and lists plugin
  properties with their origins for a given `producer`. The bundled
  git plugin declares `bcode.git.already_exists` and `bcode.git.show_destination`, which its clone
  cards honor.
* Grouping counts an invocation once when its historical request and a later item are both in
  the window (superseded requests).

### Catalog and tooling

* Palettes, profiles, and combinations ship in the bundled catalog; bundled profiles (including
  `timeline` and `focus`) use groups, reveal, sections, header fields, summary templates, and
  spacing.
* Picker: one two-axis appearance picker. `/appearance` and `/theme` (palette routes
  `appearance.select` and `theme.select`) open it on the palette axis, `/profile`
  (`profile.select`) on the profile axis. It composes a palette and a profile axis state with a
  live preview (palette catalog details, then the sample transcript), a diagnostic row for skipped
  themes, and wheel scrolling of the column under the pointer; persists only the changed axes
  (the profile after the theme write completes); restores both on Escape; and degrades at narrow
  sizes by dropping the preview, then stacking the columns, then showing only the active column.
  Its keymap scope is `appearance_picker`; the former `theme_picker` and `profile_picker` scopes
  are accepted as aliases of it when `appearance_picker` itself is not configured.
* `bcode theme explain`, `bcode theme check`, `bcode theme profiles`, the in-TUI inspector, and
  the generated gallery, as described under Tooling. Explain reads plugin layers from the
  manifests of the enabled bundled and installed plugins without loading any library. It applies
  the host's pre-load checks (`bcode_plugin::check_manifest_before_load` and
  `check_registered_plugin_before_load`: tool presentation declarations, the declared ABI
  version, and the resolved library file's existence), the same functions the native loader
  runs before opening a library, and lists plugins they reject as skipped with the reason. A
  library that passes those checks but fails when opened (an unloadable file, a missing symbol,
  or an exported manifest that disagrees with the file) still shows its layer in explain.

### Deviations

* Structural configuration lives in the renderer-neutral `[[presentation.rules]]` section
  rather than under `[tui]`.
* The definition crate keeps its `bcode_tui_theme` name and `bmux_tui` color type; a
  renderer-neutral `bcode_theme` crate is deferred until a non-terminal renderer consumes theme
  files.
* `color_depth = "auto"` only honors `NO_COLOR`; reduced depths are explicit.
* Groups never carry a `group_size` fact; `items` buckets the member count.
* "Make this the default for this tool" persists to user state, not to the declarative
  settings file, as the invariant on interactive presentation choices requires; explain
  reports such rules under `user_state`.

### Resolution cost

Structure resolution runs only for units whose inputs changed and is memoized (bounded at 256
entries); a disclosure toggle re-resolves and re-lays out only the units it changes. Frame budgets
and their measurements are in [TUI V2 architecture](tui-v2-architecture.md) §9.2.

### Not yet done

* A CI step for `cargo run -p xtask -- theme-gallery --check`; xtask has no check aggregate yet.
