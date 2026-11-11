# TUI themes

Bcode themes are declarative, versioned TOML files. They control terminal presentation only: theme values cannot change authorization, tool dispatch, plugin routing, session persistence, or execution outcomes.

Bcode ships an adaptive flagship family as `bcode`. It inherits the canonical dark presentation and
provides explicit dark and independently tuned light variant patches. Existing `bcode-dark` and
`bcode-light` IDs remain available for configuration compatibility; `bcode` is the convenient choice
when `variant = "auto"` should follow terminal background detection. `terminal-native` remains the
default unless selected explicitly.

Bcode resolves these roles once into its terminal `PresentedTheme`. At the terminal boundary,
`PresentedTheme::component_theme()` is the sole conversion into BMUX's generic `ComponentTheme`;
individual components derive their states and surfaces from that palette while Bcode retains explicit
source, diff, Markdown, and syntax overrides. Native plugin presentation carries a versioned optional
component theme alongside the legacy source/diff fields. Plugins may use the generic palette only
when `PluginTuiTheme::component_theme()` returns `Some`; older or incompatible contexts continue to
use the legacy fields without changing rendering semantics.

## Palettes, profiles, and schema version 2

A theme has two independent halves:

* a **palette** — colors, written as a handful of seed colors;
* a **profile** — chrome (borders, focus effects, icons, density, separators,
  speaker treatment, surface layering, motion) and structure (which tools are
  collapsed or summarized, grouping, sections, header fields, summaries,
  spacing; see [Structural presentation](#structural-presentation)).

```toml
[tui.theme]
name    = "catppuccin-mocha"  # palette (or any complete theme)
profile = "riced"             # optional; applied before other overlays
variant = "auto"
```

Bundled palettes: `catppuccin-mocha` (Latte as its light variant),
`catppuccin-macchiato`, `catppuccin-frappe`, `catppuccin-latte`, `tokyonight`
(Day as light), `tokyonight-storm`, `tokyonight-moon`, `gruvbox`,
`gruvbox-material`, `dracula`, `rose-pine` (Dawn as light), `rose-pine-moon`,
`kanagawa` (Lotus as light), `kanagawa-dragon`, `everforest`, `one`,
`solarized`, `github`, `github-dimmed`, `ayu`, `ayu-mirage`, `nightfox`,
`oxocarbon`, `modus`, `flexoki`, `poimandres`, `vesper`, `melange`, `zenburn`,
`monokai-classic`, `night-owl`, and the Bcode originals `neon-noir`,
`phosphor`, `amber`, `paper`, `blueprint`, `aurora`, `ember`, `glacier`, `moss`,
and `dusk`. Sources and licenses are listed in
`packages/tui-theme/themes/attribution/palettes.md`.

Bundled profiles:

| Profile | Character | Structure |
|---|---|---|
| `bare` | Stays out of the way: no chrome | Tools as one-line summaries; runs of tools fold into summary groups |
| `minimal` | Hairline rules, one accent | Finished tools summarized; lookups (`read`, `glob`, `grep`, `list`) fold into collapsed same-tool groups |
| `structured` | Status bars on tools, framed dialogs | Same-tool runs as summary groups; `arguments` sections collapsed |
| `soft` | Rounded corners, tinted surfaces, roomy padding | Long `output`/`result` sections collapse to their label row |
| `riced` | Gradient focus, Nerd Font icons, powerline separators, framed tool cards | Everything expanded, including groups; icon headers |
| `brutalist` | Heavy rules, hard edges, no motion | Host defaults |
| `retro` | Double-line frames, ASCII markers | Finished tools print a templated `[status] tool target` line |
| `editorial` | Prose first | Tools fold into collapsed groups that reasoning does not interrupt |
| `zen` | Only what is happening now is open | Everything collapsed; groups reveal only running and waiting members |
| `compact` | No padding, short previews, for small terminals | Summaries, no blank row after tools, collapsed groups |
| `accessible` | Explicit frames, verbose labels, no motion | Nothing grouped or folded; every section shown; full headers |
| `timeline` | Each turn reads as a log | Tools grouped per turn as summary groups, timing-first headers |
| `focus` | Only the latest turn is open | Earlier turns fold; sections start collapsed; output opens to a 6-line tail |

Curated combinations select both at once: `rice-mocha`, `neon`, `crt`,
`amber-terminal`, `zen-paper`, `newsroom`, `night-shift`, `quiet`,
`brutal-modus`, `forest-floor`, and `blueprint-studio`.

`/appearance` (or **Appearance: Palette and Profile** in the command palette)
opens the appearance picker, which picks a palette and a profile together.
`/theme` (**Theme: Select**) opens the same picker on the palette column and
`/profile` (**Profile: Select**) on the profile column. The two columns sit
side by side; left and right (or `h`/`l`, `tab`) switch the active column, and
up and down (or `k`/`j`) change its value, applying it immediately: the
transcript behind the picker re-renders, and the preview pane shows the
palette's catalog details (id, origin, and dark/light variant availability),
its color swatch, the profile's description, and a sample turn (an earlier
exchange, small and large reads, a failed command, a running command, and a
pending permission) rendered through the same structural pipeline, including
your `[[presentation.rules]]` and the visibility floor. Palette rows mark the
current selection and tag user, project, and explicit themes with their
origin; **Theme default** in the profile column clears the profile overlay.
Clicking a row selects it and focuses its column, and the mouse wheel moves
the selection of the column under the pointer; releasing the mouse never
applies. When candidate themes were skipped, the bottom row names the first
one and how many more there are. Enter saves whichever of the two changed to
user state, and Escape restores both. On narrow terminals the preview is
dropped first, then the columns stack, and on the smallest sizes only the
active column is shown.

The picker's keys are remappable in the `appearance_picker` keybinding scope.
Configuration written for the former `theme_picker` and `profile_picker`
scopes is still honored as `appearance_picker` bindings (the `theme_picker`
entry wins when both bind the same keys, however they are spelled, or when one
binding is a prefix of the other's sequence) until an `appearance_picker` scope is
configured, which then replaces them; rename those sections to
`appearance_picker` to migrate.

`/theme list`, `/theme preview <id>`, `/theme apply <id>`, `/theme reset`,
`/profile list`, `/profile apply <id>`, and `/profile reset` remain available.
Profile files may set a one-line `description`, shown in the picker.

### Seed palettes

`schema_version = 2` files may define seed colors and leave roles unset; Bcode
derives every registered role from the seeds and keeps text, accents, and
states readable (4.5:1, muted text 3:1) against the background. Seeds are
`background`, `text`, `accent`, and the hue slots `red`, `green`, `yellow`,
`blue`, `magenta`, `cyan`, `orange`, `violet` (`info`, `success`, `warning`,
`error` may override the state hues). Derivation only fills roles that resolve
exactly from RGB seeds; terminal-owned seeds keep their inherited roles.

Derived roles are contrast-aware. After derivation, each derived role is
measured against the background it is painted on, using the same pairs
`bcode theme check` measures (text on the raised and overlay surfaces, inline
code on its tinted fill, state text on its state surface, diff text on diff
rows, selection text on the selection). A role that falls short has its own
foreground raised, or, when it paints a tint under another role's text (state
surfaces, diff rows and emphasis), its background moved instead so the shared
text color stays consistent. Only OKLCH lightness changes, by the minimum
needed, so hue is preserved.

Authored colors are kept exactly as written. A theme that wants failing
authored roles raised the same way opts in at the top level, in any schema
version:

```toml
contrast = "enforce"   # default "derived": adjust derived roles only
```

```toml
schema_version = 2
id             = "my-palette"
display_name   = "My Palette"
extends        = ["seeded-base"]

[palette]
background = "#1a1b26"
text       = "#c0caf5"
accent     = "#7aa2f7"
red        = "#f7768e"
green      = "#9ece6a"
yellow     = "#e0af68"
blue       = "#7aa2f7"
magenta    = "#bb9af7"
cyan       = "#7dcfff"
```

### Color expressions

Palette and style values in schema-v2 files may derive colors:
`mix(a, b, t)` (`t` is the share of `a`), `lighten(c, t)`, `darken(c, t)`,
`saturate(c, t)`, `desaturate(c, t)`, `ensure_contrast(fg, bg, ratio)`, and
`or(a, b, …)` (first argument whose references exist). Math runs in OKLab/OKLCH.
Applying math to a terminal-owned color warns and keeps the first color.

```toml
[palette]
tint = "mix($accent, $background, 0.2)"

[styles."diff.added_row"]
bg = "mix($green, $background, 0.14)"
```

### Chrome

```toml
[chrome.tui]
borders    = { panel = "rounded", dialog = "thick", tool = "rounded", composer = "rounded" }
focus      = { effect = "gradient", colors = ["$accent", "$violet"] }
icons      = "nerd"          # none | unicode | nerd
density    = "comfortable"   # compact | cozy | comfortable
separators = { status = "powerline" }  # plain | bar | slant | powerline
speaker    = "badge"         # prefix | badge | bar | gutter
surfaces   = "layered"       # flat | layered
motion     = "subtle"        # none | subtle | full
```

Focus gradients need two RGB colors; terminal-owned colors keep the focused
border style. `[tui] reduce_motion = true` disables animated chrome.

* `borders.panel` and `borders.dialog` pick the border glyphs (`ascii`,
  `single`, `rounded`, `thick`, `double`) for panels and dialogs.
  `borders.tool` picks how tool units are framed: `none` drops the container,
  `left` draws a leading bar, and the framed styles draw a full-width panel.
* `icons` picks the glyph set for header icons and disclosure markers. Glyphs
  that do not measure as expected fall back to Unicode, then ASCII.
* `density` sets the default spacing around tool units and groups (`cozy` is
  the built-in default). Any `spacing` rule, from a profile or your own
  configuration, takes precedence.
* `separators.status` joins status line segments. `powerline` and `slant` use
  Nerd Font glyphs only with `icons = "nerd"`, otherwise Unicode (`❯`, `╱`)
  or, with `icons = "none"`, ASCII (`>`, `/`); `bar` is `│` or `|`. Every
  separator is three cells wide, so switching styles never changes which
  segments fit.
* `speaker` sets how user and assistant messages show who is speaking:
  `prefix` (the default) puts the name on its own heading row; `badge` paints
  the name as a filled badge; `bar` draws a colored bar beside every row of
  the message; `gutter` puts the name in an 8-column left gutter beside the
  message, so every message's text starts in the same column. Narrow
  transcripts fall back to `prefix` (below 6 columns for `bar`, 20 for
  `gutter`), and long or wide names are truncated in the gutter.
* `borders.composer` frames the message composer: `single` (the default),
  `rounded`, `thick`, `double`, and `ascii` change the frame glyphs, `left`
  draws a leading bar, and `none` draws no border. Only full frames show the
  `Message` title; the composer's size never changes.
* `surfaces = "layered"` paints tool units and group headers on the
  `surface.raised` background without moving any cell. It has no effect when
  the raised surface matches the canvas.
* `motion` sets the activity spinner cadence: `none` holds the spinner still
  (the elapsed time still updates each second), `subtle` is the default
  100ms cadence, and `full` is faster. With `focus.effect = "sweep"`, the
  focus gradient advances while the spinner runs (slowly under `subtle`,
  faster under `full`). Nothing animates while Bcode is idle. `motion` also
  times accent color transitions, the transcript's animated scroll to new
  content, and the glow on the "new messages below" bar: `none` makes them
  instant, `subtle` keeps the established timing, and `full` lengthens them.
  An explicit `[tui.theme] accent_transition = "immediate"` or
  `accent_transition_ms` takes precedence over the theme for accent changes.
  `[tui] reduce_motion = true` overrides both and makes every animation
  instant. Switching to `none` or turning `reduce_motion` on while an
  animation runs finishes it immediately.

### Structural presentation

Themes, configuration, and user state contribute ordered rules. Each rule
matches unit facts and sets properties; later rules win per property.

```toml
[[presentation.rules]]
match = { kind = "tool", tool = ["read", "glob", "grep"], status = "succeeded" }
set   = { disclosure = "collapsed", group = "consecutive_same_tool" }

[[presentation.rules]]
match = { kind = "tool", status = ["running", "failed"] }
set   = { disclosure = "expanded", preview = { lines = 12, keep = "tail" } }
```

Layers apply in this order: theme and profile rules, then
`[[presentation.rules]]` in Bcode configuration, then interactive defaults in
user state. Click toggles and focus-mode changes apply on top for the current
session only.

#### Facts

A `match` table is a conjunction; a list value matches any listed value, and an
empty `match` matches every unit.

| Fact | Values |
|---|---|
| `kind` | `user_message`, `assistant_message`, `reasoning`, `tool`, `permission`, `interaction`, `compaction`, `skill`, `activity`, `plugin_status`, `system`, `group` |
| `tool`, `producer` | strings, matched exactly |
| `schema` | string; `name/*` matches any version |
| `status` | `requested`, `running`, `waiting`, `succeeded`, `failed`, `cancelled`, `timed_out` |
| `is_error` | bool |
| `turn` | `latest`, `previous`, `older` |
| `min_depth` | child-session depth, inclusive |
| `lines`, `items` | `empty`, `one`, `few` (2–10), `some` (11–50), `many` (51–200), `huge` (over 200) |
| `target` | the operation target (path, command, pattern), matched exactly |
| `duration` | `<1s`, `1s..10s`, `10s..60s`, `>60s` |
| `last_in_turn` | bool; units without the fact never match |

Plugins may declare further facts for their own units, named under their
plugin id (`bcode.git.already_exists`). Match them under `plugin`, by value, by
any of a list, or, for integer facts, by range (`gt`, `gte`, `lt`, `lte`):

```toml
[[presentation.rules]]
match = { plugin = { "bcode.git.files" = { gt = 10 } } }
set   = { disclosure = "collapsed" }
```

Only units produced by the declaring plugin carry its facts; other units never
match. Unknown facts, and values that can never match a declared fact's type,
produce warnings.

#### Properties

| Property | Values |
|---|---|
| `disclosure` | `expanded`, `summary`, `collapsed`, `hidden` |
| `detail` | `minimal`, `normal`, `verbose` |
| `preview` | `{ lines, keep = "head" \| "tail" \| "both" }` in source lines |
| `group` | see [Groups](#groups) |
| `reveal` | member statuses shown inside a folded group |
| `sections.<name>` | `{ visible, disclosure, preview }`; see [Sections](#sections) |
| `parts.<name>` | bool; alias for `sections.<name>.visible` (`timing` is a part only) |
| `header` | `{ fields = [...] }`; see [Header fields](#header-fields) |
| `summary` | `{ source = [...], template = "..." }`; see [Summaries](#summaries) |
| `spacing` | `{ before, after }` blank rows, each 0–2; default `{ before = 0, after = 1 }` |
| `indent` | extra columns, 0–8; default 0 |
| `placement`, `container` | interaction placement and container recipe id |
| `plugin.<name>` | a plugin-declared property, for example `plugin = { "bcode.git.show_destination" = false }` |

A tool unit's disclosure decides how much of it is drawn:

| Disclosure | Rows |
|---|---|
| `expanded` | header, then each visible section with its own disclosure |
| `summary` | header, then a one-line summary |
| `collapsed` | header only |
| `hidden` | a count marker (`▸ 1 tool call hidden`) |

Every property merges independently, so one rule can set the header while
another sets disclosure. `sections` merge per section and per field. `summary`
and `spacing` are replaced as a whole; omitted fields take their defaults.
Out-of-range `spacing`, `indent`, and over-long templates are clamped and
reported as warnings.

Plugin properties are declared in a plugin's manifest with a type (`bool`,
`int` with optional bounds, or `enum`) and a default. They apply only to that
plugin's units: a rule may set one for every unit, and the other units ignore
it. Values of the wrong type or out of bounds are ignored with a warning;
unknown plugin properties produce warnings. The plugin's visual adapters receive
the resolved values when they render.

#### Groups

`group` folds runs of tool units into one group unit. It takes a shorthand
(`none`, `consecutive_tools`, `consecutive_same_tool`) or a table:

```toml
[[presentation.rules]]
match = { kind = "tool" }
set   = { group = { by = "consecutive", bridge = ["reasoning"], min = 2 } }

[[presentation.rules]]
match = { kind = "group" }
set   = { disclosure = "collapsed", reveal = ["running", "waiting", "failed", "timed_out"] }
```

* `by`: `none`, `consecutive` (any tool), `same_tool`, `same_producer`, or
  `turn`. `consecutive_tools` means `{ by = "consecutive" }`, and
  `consecutive_same_tool` means `{ by = "same_tool" }`.
* `bridge`: unit kinds that may sit between members without ending the group.
  It defaults to `["reasoning"]` when `by` is not `none`. Bridged units stay in
  place and appear when the group is expanded; they are not members and are not
  counted. Supplemental and hidden tool contributions are never members either:
  they stay inside the group of the call they belong to, never split it, and
  appear whenever that call is shown in full. A contribution of a call outside
  the group is treated like any other unit between members: it must be a bridge
  kind, and a failed one ends the group. A call counts once: when its earlier
  request and its later result are both in view, the result is the member and
  the request is shown with it, and requests directly before a group's first
  member open the group.
* `min`: the smallest run that forms a group; default and minimum 2.

Groups never reorder units and never cross turns. A group's `bridge` and `min`
come from its first member. Group units have their own facts (`kind = "group"`;
`tool` and `producer` when every member shares them; an aggregate `status`;
`is_error` when any member failed; `items`; `turn`; `depth`), so rules matching
`kind = "group"` style them. A group's disclosure means:

* `collapsed`: the group header, then revealed members only;
* `summary`: the group header, then one row per member;
* `expanded`: the group header, then every member and bridged unit, indented.

`reveal` lists the member statuses that stay visible while a group is collapsed
or summarized; the default is `["running", "waiting", "failed", "timed_out"]`.
Members awaiting a decision and failed members are always revealed, whatever
`reveal` says. Groups are never hidden.

#### Sections

A tool unit's body is a list of named sections: `arguments`, `output`,
`result`, `diff`, `artifact`, `metadata`, `visual` (a plugin-rendered body),
`confirmation`, and `error`. Plugins may add namespaced sections such as
`bcode.git.files`; unknown host section names without a dot produce warnings.

```toml
[[presentation.rules]]
match = { kind = "tool", tool = "bash" }

[presentation.rules.set.sections]
arguments = { visible = false }
output    = { disclosure = "collapsed", preview = { lines = 6, keep = "tail" } }
```

A section's preview defaults to the unit's `preview`, and its disclosure to
`expanded`. An expanded section draws its label row (`  output`) and its body.
A collapsed section draws one row with a marker and row count
(`  ▸ output · 14 rows`); `summary` adds the first line of the body; `hidden`
or `visible = false` omits it. Fields of the same section (for example one
`arguments` section per argument) share one disclosure target.

#### Header fields

`header.fields` chooses and orders the header. Tool and other unit headers may
use `marker` (the disclosure glyph), `icon`, `label` (for example
`Tool result`), `tool`, `target`, `status`, `timing`, `count`, and `title` (the
producer's title). Group headers may use `marker`, `count` (`5 tool calls`),
`tools` (`read ×3, grep ×2`), `status`, and `timing` (total duration).

| Unit | Default header |
|---|---|
| tool | `["marker", "label", "tool", "status", "timing"]` |
| group | `["marker", "count", "tools", "status", "timing"]` |

A leading `marker` is drawn as a prefix; other fields are joined with ` · `.
`timing` also needs the `timing` part to be visible. A field that has no value
for a unit is skipped. Unknown fields, and fields used on a kind they do not
apply to (for example `tools` in a rule that matches only `tool`), produce
warnings.

#### Summaries

The `summary` disclosure shows one line under the header. `summary.source`
lists where it comes from, tried in order:

* `plugin`: the producer's summary;
* `first_line`: the first non-blank line of the first visible `output`,
  `result`, or `visual` section, else of any visible section;
* `template`: `summary.template`.

The default is `["plugin", "first_line", "template"]`, so a rule that sets only
`summary.template` uses it when there is no producer summary and no body text.

```toml
[[presentation.rules]]
match = { kind = "tool" }
set   = { summary = { source = ["plugin", "template"], template = "[{status}] {tool} {target}" } }
```

Templates substitute only these names in braces: `{tool}`, `{target}`,
`{status}`, `{timing}`, `{count}`, `{title}`, `{first_line}`. There are no
conditionals or formatting directives. Unknown names produce warnings and
render empty; other braces are literal text. Templates are limited to 256 bytes,
and the result is a single trimmed line.

#### Visibility floor

A floor that no theme, configuration, or toggle can lower:

* units awaiting a decision stay expanded, with their `arguments`, `metadata`,
  and `confirmation` sections visible and expanded;
* failed units stay at least summarized, and their `error` and `output`
  sections are never hidden;
* groups are never hidden, and their failed and awaiting members are always
  revealed;
* hidden units of every kind leave a one-row count marker (`▸ 1 message
  hidden`) that can be clicked or focused to reveal them. Reasoning hidden by
  `tui.thinking.show = false` (or its toggle) is the one exception and renders
  nothing.

The floor also applies after toggles: collapsing a failed unit by hand keeps
its summary line. Rules that explicitly target these units and try to cross the floor produce
warnings naming the rule and layer.

#### Disclosure targets and mouse

Each header and section label is a disclosure target:

| Target | Meaning |
|---|---|
| `unit:<item id>` | one transcript unit |
| `group:<first member item id>` | a group |
| `section:<item id>:<name>` | one section of a unit |

Clicking a header or section label toggles that target: expanded becomes
collapsed, and anything else becomes expanded. Toggles last for the session and
are dropped when their item leaves the resident transcript window. The
`tui.tools.expandAll`, `tui.tools.collapseAll`, and `tui.tools.resetDisclosure`
command-palette actions apply to every unit and group; expand and collapse
leave hidden units and units awaiting a decision alone. Changing a group's
disclosure replaces the toggles of its members.

#### Focus mode

`alt+u` (`tui.transcript.focusUnits`) enters keyboard focus mode on the last
visible disclosure target. The focused row uses the selection style. Moving
focus scrolls the transcript from the edge it approaches: moving up puts the
target's first row on the top edge, and moving down puts its last row on the
bottom edge (a target taller than the view keeps its first row on top instead).
A unit spans its whole entry, a section runs to the next section, and a group
runs through its last item.

| Key | Action |
|---|---|
| `j`, Down | next target |
| `k`, Up | previous target |
| Enter, Space | toggle |
| `l`, Right | expand one level (group: collapsed → summary → expanded) |
| `h`, Left | collapse one level, or move to the parent (section → unit → group) |
| `e` | expand everything under the focused target |
| `d` | cycle the focused target collapsed → summary → expanded |
| `i` | open or close the inspector for the focused target |
| `shift+Down`, `shift+Up` | scroll the inspector (the mouse wheel over it also scrolls, and its scrollbar can be clicked and dragged) |
| `m` | make the focused tool unit's disclosure the default for that tool |
| Home, End | jump to the session start / latest content, loading it directly, and focus its first / last target |
| Escape | close the inspector, then leave focus mode (`alt+u` also leaves, closing the inspector) |

In focus mode the expand-all, collapse-all, and reset actions affect only the
focused target and what is under it. All keys can be remapped in the
`transcript_focus` keymap scope; other keys reach the composer as usual.

Outside focus mode `alt+d` (`tui.transcript.cycleDetail`, remappable in the
`chat` scope) cycles every unit and group together, continuing from the last
level applied to all of them. The inspector follows focus and shows the target's facts, each
resolved property with the rule and layer that set it (`default` for host
defaults), floor adjustments, any manual toggle, and grouping context. `m`
saves a rule such as `match = { kind = "tool", tool = "read" }`,
`set = { disclosure = "collapsed" }` to user state, replacing an earlier rule
for the same tool, and applies it immediately (user-state writes are
serialized, so a concurrent theme or profile save is never lost); the inspector and
`bcode theme explain` then report it under the `user_state` layer. The command
palette offers the same three actions, plus entering or leaving focus mode.

#### Explaining a unit

`bcode theme explain` resolves the same layers as the TUI for a described unit
and prints every property with the rule and layer that set it, floor
adjustments, and diagnostics. Plugin default rules appear as `plugin` layers,
read from the manifests of the enabled bundled and installed plugins (no plugin
is started); pass `producer=<plugin id>` to see a plugin's rules apply to its
own units. Plugins Bcode would refuse to load before opening their library (an
unsupported ABI version, a missing library file, or an invalid tool
presentation declaration) are listed as `skipped` with the reason and add no
layer. A library that passes those checks but fails when it is opened still
shows its layer here:

```console
bcode theme explain tool=read status=succeeded lines=40
bcode theme explain --profile focus kind=group turn=older
bcode theme explain --theme rice-mocha tool=bash status=failed --json
```

Facts are `KEY=VALUE` arguments (`kind` defaults to `tool`). A namespaced key
is a plugin fact (`producer=bcode.git bcode.git.files=12`); it is interpreted by
its declared type and, as in the TUI, kept only when `producer` is the declaring
plugin. Declared plugin properties are type-checked and their manifest defaults
are attributed to the plugin's layer. `--theme` and
`--profile` replace the configured selection, and `--profile ""` clears the
profile. `bcode theme profiles` lists the available profiles and marks the
configured one.

### Checking a theme

`bcode theme check` audits a theme in both its dark and light variants:

```console
bcode theme check                 # the configured theme, profile, and overlays
bcode theme check nord --strict   # fail on any warning
bcode theme check ./my-theme.toml --json
```

`THEME` is a theme id, a profile id (checked over `terminal-native`), or a
theme file. The report lists, per variant:

* contrast ratios (WCAG) for text on the canvas and raised/overlay surfaces,
  accents and links, state colors and state surfaces, tool titles, diff signs,
  rows, and emphasis, and selection, against 4.5:1 for text and states and 3:1
  for muted text, focus borders, diff signs, tool titles, and inactive
  selection; colors the terminal owns (`terminal` and the 16 ANSI colors) are
  shown as unmeasured;
* unknown style roles and container keys;
* registered roles that resolve only through seed derivation, and roles left
  unset (renderers use their own fallback);
* registry coverage;
* structural rule warnings from the theme's `[[presentation.rules]]`,
  including rules that try to cross the visibility floor; properties and facts
  declared by the enabled plugins are checked against their declared types
  rather than reported as unknown;
* contrast adjustments resolution made (see Seed palettes), as `adjusted`
  notes naming the role, whether its foreground or background moved, the
  color before and after, and the pair's contrast before and after.

Adjustments are informational and never count as warnings. Warnings do not fail the command unless `--strict` is given. `--json` prints a
stable, versioned structure. Every bundled theme passes `--strict` in both
variants.

### Unknown roles

Style roles that are neither registered nor in a known domain namespace
(`git.*`) produce warnings with suggestions, for example
`unknown style role "bordr.focussed"; did you mean "border.focused"?`.

### Terminal color depth

`[tui] color_depth` is `auto` (default), `truecolor`, `ansi256`, `ansi16`, or
`none`. `auto` emits colors unchanged except under `NO_COLOR` or `TERM=dumb`,
where colors are removed and modifiers remain; reduced depths are explicit
opt-ins because multiplexers and SSH often hide truecolor support. Reduced
depths map RGB colors to the perceptually nearest palette entry; ANSI and
indexed colors pass through unchanged.

## Configuration layers

Bcode merges configuration in this order, with later files overriding earlier fields:

1. `$XDG_CONFIG_HOME/bcode/bcode.toml`, or `~/.config/bcode/bcode.toml` when `XDG_CONFIG_HOME` is unset;
2. `<repository>/bcode.toml`;
3. `<repository>/.bcode/bcode.toml`.

Use the user file to define a global default. Use either repository file to define a project default without changing global configuration. Interactive selection is separate: the appearance picker and `/theme apply` persist only the selected theme name to global user state (`$BCODE_TUI_STATE` or the Bcode state directory's `tui.toml`). They never create or rewrite `bcode.toml`. `/theme reset` clears that state override and returns to the merged configured default.

A complete selection looks like:

```toml
[tui.theme]
name = "my-theme"
variant = "light" # auto, dark, or light
overlays = ["terminal-native-structured"]
paths = ["/absolute/path/to/my-theme.toml", "/absolute/path/to/theme-directory"]
agent_accent = "theme_only" # or agent_with_theme_fallback
```

`overlays` are applied left-to-right after inherited parents. `paths` are authorized explicit files or shallow directories, not recursive search roots. Relative paths follow normal configuration path resolution; absolute paths are clearest for user configuration shared across repositories.

## Configuration

Select a theme through the existing TUI configuration:

```toml
[tui.theme]
name = "terminal-native"
variant = "auto"
overlays = []
paths = []
agent_accent = "agent_with_theme_fallback"
```

`auto` uses BMUX-owned terminal capability detection. Bcode asks the terminal for its actual default colors (OSC 10/11/4) once at startup; when the terminal answers, its reported background selects the light or dark branch and the theme re-resolves. Terminals that ignore the query never answer and nothing waits on them. Without a reply, a trustworthy `COLORFGBG` background index selects the branch; missing or malformed hints conservatively select dark. The query is only sent when `variant = "auto"`. Explicit `dark` and `light` always override detection. BMUX also classifies color depth from `NO_COLOR`, `COLORTERM`, and `TERM`; this capability does not change execution semantics or replace terminal-default colors.

`paths` may add explicitly authorized theme files or directories. Bcode canonicalizes discovered paths, confines candidates to authorized roots, and bounds candidate count and bytes.

## Discovery precedence

Definitions are considered in this order, with later valid definitions taking precedence by theme id:

1. themes bundled with Bcode;
2. the user configuration theme directory;
3. `<repository>/.bcode/themes`;
4. explicitly configured files or directories.

Invalid files are diagnosed and are never partially applied. Theme files are data, not executable plugins.

## Bundled themes

The generated [theme gallery](theme-gallery.md) shows every bundled theme and profile rendered
through the TUI sample transcript, with per-variant swatch tables. Regenerate it with
`cargo run -p xtask -- theme-gallery` after changing a bundled theme, profile, or the sample
render path; `--check` fails when it is stale.

* `terminal-native` is the default. It uses terminal-default foreground/background behavior and minimal decoration.
* `terminal-native-structured` keeps terminal-native colors while adding structured tool containers.
* `bcode-dark` and `bcode-light` provide first-party opaque palettes.
* `monochrome` emphasizes glyphs and modifiers rather than color distinctions.
* `high-contrast` provides strong focus, selection, warning, error, and success distinctions.
* `nord` adapts the Nord palette under its MIT license; source, copyright, and license text are recorded in `packages/tui-theme/themes/attribution/nord.md`.

## Authoring format

Every file starts with:

```toml
schema_version = 1
id = "my-theme"
display_name = "My Theme"
extends = ["terminal-native"]
```

Colors may be RGB hex (`#RRGGBB`), ANSI names such as `ansi:bright_cyan`, indexed colors, palette references such as `$accent`, or `terminal`. `terminal` means the backend terminal default; it is not black and does not request an opaque fill.

Semantic style keys use `[styles."role"]` tables with optional `fg`, `bg`, and `modifiers`. Supported modifiers are `bold`, `dim`, `italic`, `underline`, `slow_blink`, `reversed`, `hidden`, and `crossed_out`.

```toml
[palette]
accent = "#7dd3fc"
text = "terminal"

[styles."border.focused"]
fg = "$accent"
modifiers = ["bold"]

[styles."diff.added"]
fg = "ansi:green"
```

Container recipes are bounded and declarative:

```toml
[containers."tool.failed"]
layout = "panel"
width = "full"
border = "left"
padding_x = 1
```

Layouts are `plain`, `left_bar`, or `panel`; widths are `content` or `full`; borders are `none`, `left`, or `all`.

## Semantic surfaces

Schema version 1 accepts three optional roles for the existing interactive surface hierarchy:

* `surface.raised` styles pickers, palettes, and the composer;
* `surface.overlay` styles opaque modal and dialog panels;
* `control.focused` styles focused borders, titles, inputs, and controls.

`surface.scrim` is an optional full-parent modal scrim. Because terminal cells do not support alpha,
a scrim is an opaque presentation choice and should be used sparingly. Missing `surface.raised` and
`surface.overlay` roles fall back to the resolved canvas; missing `control.focused` falls back to
`border.focused`; missing scrims remain disabled. These optional fallbacks preserve existing
schema-v1 external themes and keep `terminal-native` transparent.

## Appearance picker preview

On wide terminals, the appearance picker (`/appearance`, `/theme`, `/profile`) presents a preview
pane beside its palette and profile columns. The preview renders a bounded, renderer-owned sample
transcript; it never reads or mutates the live session event stream. Narrow or short terminals drop
the preview and keep the columns. The preview exposes the selected palette's id, source, any
validation state other than `valid`, and dark/light variant availability. Moving the selection
previews the complete live app, Enter persists through the existing user-state effect, and Escape
restores the configured presentation.

## Authoring examples

### Terminal-native inheritance

Use terminal defaults and override only one semantic role. Unspecified diff row and intraline-emphasis roles retain Bcode's default green/red backgrounds; define those roles explicitly when a theme intentionally wants another treatment.

```toml
schema_version = 1
id = "quiet-native"
display_name = "Quiet Native"
extends = ["terminal-native"]

[styles."border.focused"]
fg = "ansi:magenta"
modifiers = ["bold"]
```

### Opaque color palette

```toml
schema_version = 1
id = "ocean-dark"
display_name = "Ocean Dark"
extends = ["terminal-native"]

[palette]
background = "#08131f"
text = "#dbeafe"
muted = 245
accent = "#38bdf8"

[styles.canvas]
fg = "$text"
bg = "$background"

[styles."text.primary"]
fg = "$text"

[styles."text.muted"]
fg = "$muted"

[styles."border.focused"]
fg = "$accent"
modifiers = ["bold"]
```

Numeric color values are bounded terminal palette indices from 0 through 255. Hex colors require exactly six hexadecimal digits.

### Structured tool panels

```toml
schema_version = 1
id = "ocean-structured"
display_name = "Ocean Structured"
extends = ["bcode-dark"]

[styles."tool.running.title"]
fg = "$accent"
modifiers = ["bold"]

[styles."tool.failed.title"]
fg = "ansi:red"
modifiers = ["bold"]

[containers."tool.running"]
layout = "left_bar"
width = "full"
border = "left"
padding_x = 1

[containers."tool.failed"]
layout = "panel"
width = "full"
border = "all"
padding_x = 1
padding_y = 0
```

Container recipes affect presentation only. They do not alter tool state, authorization, dispatch, or persisted outcomes.

### Light and dark variants

```toml
schema_version = 1
id = "adaptive-ocean"
display_name = "Adaptive Ocean"
extends = ["bcode-dark"]

[variants.light.palette]
background = "#f8fafc"
text = "#172033"
muted = "#64748b"
accent = "#0369a1"

[variants.light.styles.canvas]
fg = "$text"
bg = "$background"
```

Variant patches can override palettes, styles, containers, and extension data. Explicit `light` or `dark` configuration wins over terminal detection.

### Monochrome cues

```toml
schema_version = 1
id = "mono-status"
display_name = "Monochrome Status"
extends = ["terminal-native"]

[styles."state.success"]
modifiers = ["bold"]

[styles."state.warning"]
modifiers = ["bold", "underline"]

[styles."state.error"]
modifiers = ["reversed", "bold"]
```

Keep textual status labels and glyphs intact; modifiers supplement rather than replace those non-color cues.

### Plugin extension namespace

Plugins may consume declarative data from their own namespace:

```toml
schema_version = 1
id = "shell-indicators"
display_name = "Shell Indicators"
extends = ["terminal-native"]

[extensions."bcode.shell"]
indicator = "exit-code"
success_glyph = "✓"
failure_glyph = "✗"
```

Extension tables deep-merge through inheritance and overlays and participate in the resolved fingerprint. Unknown namespaces remain data; the host does not reinterpret them as execution instructions.

## Editor support

Bcode does not currently ship a separate JSON/TOML schema artifact. The version-1 parser is the authoritative schema, `bcode theme validate <path>` provides bounded field/source diagnostics, and `bcode theme copy terminal-native <path>` produces an editable valid file without network access. If a machine-readable editor schema is added later, it must be generated and tested against this parser rather than becoming a second contract.

## Roles and fallbacks

Bcode owns coding-agent roles such as transcript speakers, tool states, Markdown, syntax, source cards, and diffs. BMUX owns generic terminal component styling. Native plugin TUI adapters receive renderer-owned semantic presentation with a fingerprint, while plugin behavior and domain state remain plugin-owned.

A definition may omit roles. Resolution inherits from parent themes and then uses Bcode's readable semantic defaults. Rich plugin presentation must retain a generic structured fallback.

## Git client roles

`bcode git` uses the standard roles (`canvas`, `text.*`, `border.*`, `selection.active`,
`state.*`, `surface.*`, `diff.*`, `syntax.*`, `source.gutter`) and these optional roles, each with a
colorful fallback derived from the theme's state colors and `accent`/`violet` palette entries:

* `git.title`, `git.title.focused`, `git.key`
* `git.cursor`, `git.cursor.inactive`, `git.range`
* `git.status.staged`, `git.status.unstaged`, `git.status.untracked`, `git.status.conflict`
* `git.directory`
* `git.branch.head`, `git.branch.local`, `git.branch.remote`, `git.tag`
* `git.branch.state.current`, `git.branch.state.synced`, `git.branch.state.ahead`,
  `git.branch.state.behind`, `git.branch.state.diverged`, `git.branch.state.gone`,
  `git.branch.state.local`: branch names by state (see the git client's
  [branch colors](git-ui.md#branch-colors)), and `git.branch.merged`, patched over a merged
  branch (dim by default)
* `git.pr.open`, `git.pr.draft`, `git.pr.merged`, `git.pr.closed`: pull request markers
* `git.activity`: spinners and progress of running work (bold accent by default)
* `git.flash.added`, `git.flash.removed`: backgrounds of rows flashing after their changes grew
  or shrank (default: the `diff.added_emphasis` and `diff.removed_emphasis` backgrounds)
* `git.commit.sha`, `git.commit.author`, `git.commit.date`

Graph lanes and author initials cycle through bright colors, kept apart from the state colors, so
each keeps a stable color without reading as a branch state. Palette entries `blue` and `orange`,
when a theme defines them, color `ahead` and `diverged` branches.

## Reload and errors

External theme roots are polled through a bounded, confined content signature. Valid revisions replace the complete resolved presentation atomically. Invalid or transient revisions retain the last valid configured theme; invalid/disappearing previews close and restore the configured selection. Interactive preview, cancel, and state-backed apply are available from the appearance picker and `/theme` commands. A globally selected custom theme must remain discoverable through globally available configured paths; when it is unavailable in a repository, Bcode retains the configured default and reports a diagnostic.

Typical authoring workflow:

```console
bcode theme list
bcode theme copy terminal-native ~/.config/bcode/themes/my-theme.toml
$EDITOR ~/.config/bcode/themes/my-theme.toml
bcode theme validate ~/.config/bcode/themes/my-theme.toml
```

Then define `my-theme` as the default in `[tui.theme]`, or select it globally with `/theme apply my-theme` or the appearance picker. Interactive selection writes user state, never `bcode.toml`; `/theme reset` restores the configured default. Saving a valid edit hot-reloads the full presentation without recompiling Rust. A parse error leaves the last valid presentation active and emits a bounded source-specific diagnostic; fixing the file permits the next poll to apply it atomically.

Syntax grammars classify source tokens into semantic roles; the resolved Bcode theme exclusively selects their final colors. The semantic roles are `syntax.text`, `syntax.comment`, `syntax.keyword`, `syntax.function`, `syntax.variable`, `syntax.string`, `syntax.number`, `syntax.type`, `syntax.operator`, and `syntax.punctuation`. Omitted roles derive from the resolved text, muted, info/accent, success, and warning presentation rather than from a bundled syntax theme.

## Color and terminal behavior

`terminal`/`default` deliberately preserve the terminal backend value. The resolved `styles.canvas`
is applied to the complete normal TUI frame before content and overlays are drawn. For
`terminal-native`, that fill still uses the terminal's default foreground/background rather than an
opaque Bcode color. Omitted fields inherit or leave the prior layer unchanged; they are not aliases
for terminal defaults. Opaque `bg` values should therefore be intentional.

For deterministic RGB themes, bundled-theme tests enforce WCAG-style contrast ratios of at least
`4.5:1` for primary text and `3:1` for muted/secondary text across canvas, raised, and overlay
surfaces. These thresholds do not apply to non-text decoration or to terminal-default, ANSI, and
indexed values whose displayed RGB colors remain terminal-owned and therefore unknown to Bcode.
Selection text is checked at the primary-text threshold when both foreground and background are
explicit RGB values. Theme authors should use redundant glyphs or modifiers for semantic state even
when color contrast passes.

On reduced-color terminals, ANSI and indexed colors remain terminal-native choices throughout theme resolution, syntax highlighting, and plugin presentation; Bcode does not convert them to guessed RGB equivalents. The terminal default similarly remains the backend default. True-color RGB values are presentation inputs and may be approximated by the backend or terminal. Missing capability hints never make theme loading permissive or affect product behavior; `auto` uses the documented conservative dark fallback. For the broadest compatibility, start from `terminal-native`, retain text/glyph status cues, and avoid relying on subtle RGB differences alone.
