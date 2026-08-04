# TUI themes

Bcode themes are declarative, versioned TOML files. They control terminal presentation only: theme values cannot change authorization, tool dispatch, plugin routing, session persistence, or execution outcomes.

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

## Roles and fallbacks

Bcode owns coding-agent roles such as transcript speakers, tool states, Markdown, syntax, source cards, and diffs. BMUX owns generic terminal component styling. Native plugin TUI adapters receive renderer-owned semantic presentation with a fingerprint, while plugin behavior and domain state remain plugin-owned.

A definition may omit roles. Resolution inherits from parent themes and then uses Bcode's readable semantic defaults. Rich plugin presentation must retain a generic structured fallback.

## Reload and errors

External theme roots are polled through a bounded, confined content signature. Valid revisions replace the complete resolved presentation atomically. Invalid or transient revisions retain the last valid configured theme; invalid/disappearing previews close and restore the configured selection. Interactive preview, cancel, and durable apply are available from the theme picker and `/theme` commands.
