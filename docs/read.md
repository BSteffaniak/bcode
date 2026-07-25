# Reading Markdown in the Terminal

`bcode read` renders Markdown with Bcode's terminal Markdown renderer. The command is provided by the default-enabled `bcode.read` bundled plugin and does not require the Bcode daemon.

```bash
bcode read README.md
bcode read -i README.md
```

## Input

Pass one file path, use `-` for stdin, or omit the path when stdin is redirected:

```bash
cat README.md | bcode read
bcode read - < README.md
cat README.md | bcode read -i
```

Omitting the path while stdin is an interactive terminal is an error. Input is read to EOF and must be valid UTF-8.

## Interactive pager

`-i` and `--interactive` open an alternate-screen pager. Use:

* `j`/Down and `k`/Up to move one line.
* Space/Page Down and `b`/Page Up to move one page.
* `g`/Home and `G`/End to jump to either end.
* `q` or Escape to exit.

Interactive mode requires terminal stdout and a controlling terminal for keyboard input. Exiting restores the previous screen instead of adding the rendered document to normal terminal scrollback.

## Color

`--color` accepts `always`, `auto`, or `never`:

```bash
bcode read --color always README.md
bcode read --color auto README.md | less -R
bcode read --color never README.md | grep Installation
```

The default is `always`, including when stdout is redirected. `auto` enables styles only for terminal output, while `never` preserves rendered Markdown layout without document colors or text modifiers. If `--color` is omitted, the presence of `NO_COLOR` selects `never`; an explicit `--color` always takes precedence.

Interactive mode still uses terminal control sequences for the alternate screen when `--color never` is selected, but the document itself is monochrome.

## Plugin delivery

Top-level plugin CLI contributions currently use Bcode's statically linked plugin API. Disabling the `static-bundled-read-plugin` feature removes this command from the composed CLI.
