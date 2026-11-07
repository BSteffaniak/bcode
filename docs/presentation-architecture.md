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
* Deviations: structural configuration lives in the renderer-neutral `[[presentation.rules]]`
  section rather than under `[tui]`. The definition crate keeps its `bcode_tui_theme` name and
  `bmux_tui` color type; splitting a renderer-neutral `bcode_theme` crate is deferred until a
  non-terminal renderer consumes theme files. `color_depth = "auto"` only honors `NO_COLOR`;
  reduced depths are explicit.
* Not yet consumed by the terminal renderer: chrome `icons`, `density`, `separators`, `speaker`,
  `surfaces`, `motion`, and per-surface `tool`/`composer` border styles (they resolve and are
  validated, and BMUX bordered components honor the panel border set and focus gradient);
  grouping projection rendering; plugin-declared facts, properties, and collapsed summaries in
  plugin manifests; `bcode theme explain`/`check` commands; the two-axis picker UI.

