# TUI themes

Bcode themes are declarative, versioned TOML files. They control terminal presentation only: theme values cannot change authorization, tool dispatch, plugin routing, session persistence, or execution outcomes.

## Configuration layers

Bcode merges configuration in this order, with later files overriding earlier fields:

1. `$XDG_CONFIG_HOME/bcode/bcode.toml`, or `~/.config/bcode/bcode.toml` when `XDG_CONFIG_HOME` is unset;
2. `<repository>/bcode.toml`;
3. `<repository>/.bcode/bcode.toml`.

Use the user file for a global selection. Use either repository file to select a project theme without changing global configuration. The interactive selector persists to the documented user layer; it never rewrites a merged configuration or project file implicitly.

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

`auto` uses BMUX-owned bounded terminal capability detection. A trustworthy `COLORFGBG` background index selects the light or dark branch; missing or malformed hints conservatively select dark. Explicit `dark` and `light` always override detection. BMUX also classifies color depth from `NO_COLOR`, `COLORTERM`, and `TERM`; this capability does not change execution semantics or replace terminal-default colors.

`paths` may add explicitly authorized theme files or directories. Bcode canonicalizes discovered paths, confines candidates to authorized roots, and bounds candidate count and bytes.

## Discovery precedence

Definitions are considered in this order, with later valid definitions taking precedence by theme id:

1. themes bundled with Bcode;
2. the user configuration theme directory;
3. `<repository>/.bcode/themes`;
4. explicitly configured files or directories.

Invalid files are diagnosed and are never partially applied. Theme files are data, not executable plugins.

## Bundled themes

* `terminal-native` is the default. It uses terminal-default foreground/background behavior and minimal decoration.
* `terminal-native-structured` keeps terminal-native colors while adding structured tool containers.
* `bcode-dark` and `bcode-light` provide first-party opaque palettes.
* `monochrome` emphasizes glyphs and modifiers rather than color distinctions.
* `high-contrast` provides strong focus, selection, warning, error, and success distinctions.
* `nord` adapts the Nord palette under its MIT license; source, copyright, and license text are recorded in `packages/tui/themes/attribution/nord.md`.

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

## Authoring examples

### Terminal-native inheritance

Use terminal defaults and override only one semantic role:

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

## Reload and errors

External theme roots are polled through a bounded, confined content signature. Valid revisions replace the complete resolved presentation atomically. Invalid or transient revisions retain the last valid configured theme; invalid/disappearing previews close and restore the configured selection. Interactive preview, cancel, and durable apply are available from the theme picker and `/theme` commands.

Typical authoring workflow:

```console
bcode theme list
bcode theme copy terminal-native ~/.config/bcode/themes/my-theme.toml
$EDITOR ~/.config/bcode/themes/my-theme.toml
bcode theme validate ~/.config/bcode/themes/my-theme.toml
```

Then select `my-theme` in `[tui.theme]`, `/theme apply my-theme`, or the theme picker. Saving a valid edit hot-reloads the full presentation without recompiling Rust. A parse error leaves the last valid presentation active and emits a bounded source-specific diagnostic; fixing the file permits the next poll to apply it atomically.

## Color and terminal behavior

`terminal`/`default` deliberately preserve the terminal backend value. Omitted fields inherit or leave the prior layer unchanged; they are not aliases for terminal defaults. Opaque `bg` values should therefore be intentional.

On reduced-color terminals, ANSI and indexed colors remain terminal-native choices throughout theme resolution, syntax highlighting, and plugin presentation; Bcode does not convert them to guessed RGB equivalents. The terminal default similarly remains the backend default. True-color RGB values are presentation inputs and may be approximated by the backend or terminal. Missing capability hints never make theme loading permissive or affect product behavior; `auto` uses the documented conservative dark fallback. For the broadest compatibility, start from `terminal-native`, retain text/glyph status cues, and avoid relying on subtle RGB differences alone.
