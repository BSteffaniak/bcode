# Converting a palette to its upstream colors

A bundled palette adapted from another color scheme should use that scheme's
exact colors, not ones Bcode computes. Converting a palette is a data-only
change in three files:

* `themes/palettes/<id>.toml` — the palette, with exact seeds and slots;
* `themes/attribution/upstream/<id>.toml` — the upstream reference: where the
  values came from and which upstream value each slot and role must equal;
* `themes/attribution/sources.toml` — the palette's entry changes from
  `status = "pending"` to `status = "upstream"`.

`themes/palettes/tokyonight.toml` with `upstream/tokyonight.toml` is the worked
example (dark and light variants); `tokyonight-moon` and `tokyonight-storm`
show single-variant palettes.

## How resolution treats declared colors

Every color a palette declares is used exactly as written. A role whose
derivation takes a palette entry unchanged — a slot, or a seed such as `$blue`
for `syntax.function` — is never contrast-adjusted. Only colors Bcode
computes (mixes, and fallbacks that blend seeds) are raised to their contrast
threshold. `bcode theme check` still reports declared colors below threshold,
as warnings that name the kept palette entry, so an upstream scheme's low
contrast stays visible without being changed. A theme opts into adjusting
declared colors with `contrast = "enforce"`; bundled upstream palettes do not.

## Slots

Seeds (existing, required for derivation): `background`, `text`, `accent`,
the hues `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`, `orange`,
`violet`, and the optional states `info`, `success`, `warning`, `error`.

Exact slots (all optional; `bcode theme roles` lists them as `slot` rows):

| Slot | Meaning | Bcode roles |
|---|---|---|
| `muted` | secondary foreground (`fg_dark`, subtext) | `text.muted`, metadata, muted labels, `diff.muted` |
| `comment` | comment color | `syntax.comment` |
| `border` | window/split border | `border.default`, `source.border` |
| `border_focused` | focused or floating-window border | `border.focused` |
| `surface` | floats, popups, menus (`bg_float`, `bg_popup`) | `surface.raised` (and `surface.overlay` unless set) |
| `surface_overlay` | modal dialog fill, when upstream separates it | `surface.overlay` |
| `selection` | visual selection fill | `selection.active` background |
| `line_highlight` | cursor-line fill | `selection.inactive` background |
| `gutter` | line-number foreground | `source.gutter` |
| `diff_add`, `diff_remove` | added/removed line text in diff buffers | `diff.added`, `diff.removed` |
| `diff_add_bg`, `diff_remove_bg` | added/removed line fills | `diff.added_row`, `diff.removed_row` |
| `diff_add_row_fg`, `diff_remove_row_fg` | text on the added/removed line fills, only when upstream sets one | `diff.added_row`, `diff.removed_row` foreground |
| `diff_add_emphasis_bg`, `diff_remove_emphasis_bg` | intraline (word) add/remove fills, only when upstream defines separate ones | `diff.added_emphasis`, `diff.removed_emphasis` |
| `syntax_keyword` … `syntax_punctuation` | per-token code colors (below) | `syntax.*` and `markdown.code.*` |
| `markup_heading` | markup headings | `markdown.heading`, `syntax.heading` |
| `markup_link` | markup links | `link`, `markdown.link`, `syntax.link` |
| `markup_raw` | markup literals | `syntax.raw` |
| `markup_code`, `markup_code_bg` | Markdown inline code text and fill | `markdown.inline_code` |
| `markup_list` | list markers | `markdown.list_marker` |

The syntax slots match the token kinds Bcode's highlighter distinguishes:
`syntax_keyword`, `syntax_function`, `syntax_variable`, `syntax_string`,
`syntax_number` (numbers, booleans, `null`), `syntax_type`,
`syntax_operator`, `syntax_punctuation`. The highlighter does not separate
constants, parameters, properties, builtins, tags, or attributes, so there are
no slots for them; do not invent any.

Terminal ANSI colors (`color0`–`color15`) have no slots: no Bcode role paints
with them. Do not add them to palettes.

## Finding the canonical source

Prefer, in order:

1. The scheme's official Neovim port (the `candidates` in `sources.toml` list
   it first where one exists). Read the generated, fully resolved file when the
   repository ships one (tokyonight's `extras/lua/tokyonight_<style>.lua`
   contains every computed color and resolved highlight group); otherwise read
   the palette file plus the highlight group definitions
   (`colors/*.vim`, `lua/<name>/palette*.lua`, `lua/<name>/groups/*.lua`,
   `lua/<name>/highlights*.lua`).
2. The official palette specification (for example `catppuccin/palette`,
   `ayu-theme/ayu-colors`, the Solarized README table).
3. The official editor theme (VS Code `themes/*.json` token colors).

Values must come from the fetched source at a pinned commit — never from
memory or third-party ports. Fetch with the GitHub API, for example:

```console
gh api repos/<owner>/<repo>/commits/<branch> --jq .sha
curl -sfL https://raw.githubusercontent.com/<owner>/<repo>/<sha>/<path>
```

When upstream computes colors at runtime (blends, inversions), use a file
upstream generates from them (extras, ports, exported palettes) rather than
re-implementing the computation. If no generated file exists, record the
computed result only if you can reproduce upstream's algorithm exactly and
say so in the reference file's header comment; otherwise omit the slot.

## Mapping upstream groups to slots

For Neovim schemes, prefer tree-sitter captures over legacy Vim groups (they
are what current Neovim shows); resolve links (`@type` → `Type`) to the final
color. When a group is undefined, Neovim's default link applies: `Number`,
`Boolean` → `Constant`.

| Slot | Neovim source | Other sources |
|---|---|---|
| `background`, `text` | `Normal` bg/fg | editor background/foreground |
| `accent` | the scheme's recognisable accent (see below) | primary accent |
| hues | the named palette colors | ANSI-equivalent palette names |
| `violet` | the scheme's purple when distinct from magenta | |
| `info`, `warning`, `error` | `DiagnosticInfo`, `DiagnosticWarn`, `DiagnosticError` fg | |
| `success` | only when upstream names one (`DiagnosticOk`); hint colors are not success | |
| `muted` | secondary fg: `fg_dark`, `NormalSB`/`StatusLine` fg, subtext | |
| `comment` | `Comment` fg | comment token |
| `border` | `WinSeparator` fg | panel border |
| `border_focused` | `FloatBorder` fg | focus border |
| `surface` | `NormalFloat` / `Pmenu` bg | widget/popup background |
| `selection` | `Visual` bg | selection background |
| `line_highlight` | `CursorLine` bg | line highlight |
| `gutter` | `LineNr` fg | line-number foreground |
| `diff_add`, `diff_remove` | `diffAdded` / `diffRemoved` fg (git colors) | inserted/deleted text |
| `diff_add_bg`, `diff_remove_bg` | `DiffAdd` / `DiffDelete` bg | inserted/deleted line background |
| `diff_add_row_fg`, `diff_remove_row_fg` | `DiffAdd` / `DiffDelete` fg | text on the inserted/deleted line background |
| `syntax_keyword` | `@keyword` | keyword token |
| `syntax_function` | `@function` | function token |
| `syntax_variable` | `@variable` | variable token |
| `syntax_string` | `@string` | string token |
| `syntax_number` | `@number` | numeric constant token |
| `syntax_type` | `@type` | type/class token |
| `syntax_operator` | `@operator` | operator token |
| `syntax_punctuation` | `@punctuation.delimiter` | punctuation token |
| `markup_heading` | `@markup.heading` | markup heading |
| `markup_link` | `@markup.link` (or `@markup.link.url` when it has a color) | markup link |
| `markup_raw` | `@markup.raw` | markup inline raw |
| `markup_code`, `markup_code_bg` | `@markup.raw.markdown_inline` fg/bg | Markdown inline code |
| `markup_list` | `@markup.list` | list marker |

### The accent

`accent` is the color a scheme is recognised by, because Bcode paints its
chrome with it (`accent`, `title`, focus, the active tab). Choose, in order:

1. a color upstream itself names as the accent (`accent`, `accent.fg`);
2. otherwise the scheme's signature color: the hue its own UI, statusline, or
   documentation leads with (Gruvbox yellow, Catppuccin mauve, Rosé Pine iris,
   Dracula purple, Oxocarbon purple, Poimandres teal, Vesper peach).

The accent must be a value from the upstream palette or highlight groups,
mapped in the reference file like any other slot. Do not pick the `Function`
color only because it is the function color: it belongs in `syntax_function`,
and is the accent only when it is also the scheme's signature color. Say in the
reference file's header comment which color the accent is and why.
Bcode's `title` role follows the accent; map it to upstream `Title` only
when the two are the same color.

When upstream lacks a slot, omit it: the role keeps its derivation. Declare a
slot even when it equals a seed, so the mapping stays explicit and audited.
Write every value as a literal `#rrggbb` in lowercase.

## Variants

A palette's base `[palette]` is its dark variant; `[variants.light.palette]`
overrides it with the light variant's full set of entries (repeat every
entry, so no dark value leaks into the light variant). Use the light style
the palette's `family` in `sources.toml` names (for example Tokyo Night day,
Catppuccin latte). A palette without a light variant has only `dark` in its
reference. Do not add or remove variants while converting.

## The reference file

`upstream/<id>.toml`:

```toml
# Header comment: what was read and anything notable about it.
palette  = "<id>"                    # equals the file name
scheme   = "<Scheme name (style)>"
source   = "https://github.com/<owner>/<repo>"
revision = "<full commit SHA>"       # never a branch name
license  = "<SPDX id from upstream LICENSE>"

[variants.dark]
file = "<path in the repository that holds the values>"

[variants.dark.colors]               # upstream name → value, as upstream spells it
bg            = "#1a1b26"            # palette names
"Comment.fg"  = "#565f89"            # highlight group attributes as Group.fg / Group.bg

[variants.dark.slots]                # every palette entry → upstream name
background = "bg"
comment    = "comment"

[variants.dark.roles]                # role.fg / role.bg → upstream name
"canvas.bg"         = "Normal.bg"
"syntax.comment.fg" = "Comment.fg"
```

Map every entry the palette declares in `slots`. Map roles for each upstream
highlight group you used, so the test checks the end-to-end rendering, not
only the palette. Also update the palette file's header comment to point at
the reference and keep `attribution/palettes.md` accurate (source and license).

## Checking a conversion

```console
cargo test -p bcode_tui_theme --test upstream_fidelity
bcode theme check <id>
```

Contrast findings on kept palette colors reproduce the upstream scheme; review
them and record them as accepted findings rather than changing a color:

```console
bcode theme check --all --bless --accepted packages/tui-theme/themes/check-accepted.toml
```

Findings on derived colors cannot be accepted for bundled themes (see
`docs/tui-themes.md`, Accepting findings).

The fidelity tests check that every palette is classified in `sources.toml`,
that each reference resolves exactly (every slot and mapped role equals its
upstream value, in every variant), and that each role a slot feeds paints that
slot's color unchanged. Then run `cargo test -p bcode_tui_theme` and the
consumer crates' tests; appearance snapshots that change because the palette
now matches upstream are expected and should be updated deliberately.
