# Appearance reference

Bcode's themes and profiles aim to express any terminal agent's look without code changes. The
appearance reference measures that claim against a real target: Claude Code's fullscreen renderer
(alternate screen, `CLAUDE_CODE_NO_FLICKER=1`). The `claude-code` palette and the `claude-code-layout` profile reproduce it;
every difference they cannot express is a gap in the presentation vocabulary, closed with a
generic, registry-backed option rather than a special case.

## Running

```sh
cargo build -p bcode_tui_components --features grid-probe --bin bcode_terminal_grid_probe
cargo build -p bcode --bin bcode --features app,static-bundled-plugins,static-bundled-fake-provider-plugin

# Reference: the installed `claude` against a scripted local Messages API (no network, fake key,
# throwaway home). Output: target/appearance-reference/claude-code/<scenario>-<theme>/.
scripts/appearance-reference/capture_claude_code.py [--theme dark|light]

# Candidate: Bcode with the fake provider replaying the same turns, under a no-network sandbox.
scripts/appearance-reference/capture_bcode.py --theme claude-code --profile claude-code-layout [--variant light]

# Per-frame agreement of drawn cells (text and style), plus a side-by-side HTML report.
scripts/appearance-reference/compare.py \
  target/appearance-reference/claude-code/tour-dark \
  target/appearance-reference/bcode/tour-claude-code-claude-code-layout-dark --html report.html

# Every distinct style in a capture, with sample text: the source of the palette.
scripts/appearance-reference/palette.py target/appearance-reference/claude-code/tour-dark/*-*.json
```

Scenarios (`scripts/appearance-reference/scenarios/*.json`) hold the model turns, written as
Anthropic Messages content blocks, and per program the driver steps; snapshot labels pair frames
across programs. Content that legitimately differs (times, durations, token counts) is masked.
Product content that differs by design (tool names, slash commands, model names) is not a
presentation gap.

## Reference measurements

Dark values; the palette file carries the light variant.

| Element | Claude Code |
|---|---|
| Canvas | terminal default background |
| Secondary text | `#999999` |
| Input rules | `─` full width, `#888888`, above and below a one-row input |
| Prompt | `❯` + no-break space; user messages repeat it in `#999999` |
| User message | full-row background `#373737`, text `#ffffff` |
| Assistant text | `⏺` (`#ffffff`) + space, continuation rows indented 2 |
| Tool header | `⏺` by status (running/awaiting `#999999`, succeeded `#4eba65`, failed `#ff6b80`), bold display name, `(target)` |
| Tool output | `  ⎿  ` (`#999999`) before the first row, later rows indented 5 |
| Folded tools | `Read 1 file`, `Ran 1 shell command`: `#999999`, count bold, no marker |
| Reasoning | `Thought for 6s` (`#999999`, duration bold) |
| Turn summary | `✻ Baked for 7s · done 9:02 PM` (`#999999`) |
| Activity line | bottom of the transcript area: spinner glyph `✢✳✶✻✽` and a verb in `#d77757`, a three-cell `#eb9f7f` shimmer sweeping the verb |
| Above input | right-aligned `◐ medium · /effort` (`#999999`) |
| Footer | below input, indented 2: `⏸ manual mode on · ? for shortcuts` (`#999999`) |
| Markdown | headings bold without markers; `- ` bullets; `1. ` ordered; fenced code unframed, highlighted with the 16 ANSI colors; tables centered headers; quotes `▎` (dim) and italic text; inline code `#b1b9f9`; straight quotes kept |
| Diffs | line numbers dim, `-`/`+` markers `#dc5a5a`/`#50c850` on `#3d0100`/`#022800`, word emphasis `#044700`, Monokai syntax |
| Permission | docked in place of the input: accent rule, bold accent title, description, dashed `╌` rules (`#505050`) around the subject, question, `❯ 1. Yes` numbered options, `Esc to cancel · Tab to amend` |
| Question | docked: muted rule, header chip (inverse accent), bold question, numbered options with muted descriptions, footer hints |
| Slash menu | rows above the input rules, `❯ /name   description`, selected row accent, others muted |
| Banner | logo art (`#d77757`), bold title + muted version, model line, cwd line |
| Scrolling | follows the newest content; no rows-below indicator |

## Gaps

Status of the presentation vocabulary needed by the `claude-code-layout` profile.

| # | Gap | Status |
|---|---|---|
| 1 | Markdown code blocks need their own syntax roles (ANSI there, RGB in diffs) | open |
| 2 | Hide the header row and the session-start marker | done: `header`, `session_start` |
| 3 | Status line placement and alignment; footer row below the input | placement and indent done (`status`); segment selection open |
| 4 | Composer: rule borders, prompt glyph, no title, placeholder and send control optional | done: `borders.composer = "rules"`, `composer`, `composer.*` roles |
| 5 | Message markers: per-speaker prefix glyph and style, full-row user background, no heading row | done: `speaker = "marker"`, `markers`, full-width panel containers |
| 6 | Tool header template (`{tool}({target})`), per-tool display names, status-colored marker | open |
| 7 | Section connector (`⎿`) with hanging indent instead of section label rows | open |
| 8 | Group summary templates with plural forms (`Read {count} file{s}`) | open |
| 9 | Reasoning summary template with duration | open |
| 10 | Turn summary row (duration and completion time) | open |
| 11 | Markdown presentation options (heading markers, bullets, list spacing, code frame, table header alignment, quote glyph, smart quotes) | open |
| 12 | Activity line: placement, spinner frames, verbs, shimmer | open |
| 13 | Docked permission and question surfaces with numbered option lists | open |
| 14 | Scroll policy: follow newest instead of holding a new reply at the top | open |
| 15 | Session banner | open |
| 16 | Slash menu presentation | open |
