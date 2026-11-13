#!/usr/bin/env python3
"""Compare a Bcode capture with a Claude Code reference capture, frame by frame.

Frames pair up by snapshot label (the file name without its `NN-` prefix; the first frame of a
repeated label pairs with the first). For each pair, every cell is compared for text and for
style (foreground, background, bold/italic/underline/dim/inverse/strike), after masking text
that legitimately differs (times, durations). Only cells that either screen draws (a character,
a background or inverse video) count, so empty space does not inflate agreement. Prints per-frame agreement and
writes an HTML report with both screens side by side and differing cells outlined.

Usage: compare.py REFERENCE_DIR CANDIDATE_DIR [--html report.html] [--mask REGEX...]
"""

import argparse
import html
import json
import os
import re
import sys

DEFAULT_MASKS = [
    r"\b\d{1,2}:\d{2}(?::\d{2})?\s?(?:AM|PM)?\b",  # clock times
    r"\b\d+(?:\.\d+)?\s?(?:ms|s|m|h)\b",             # durations
    r"\b\d+(?:\.\d+)?k? tokens\b",
]
ATTRIBUTES = ("bold", "italic", "underline", "dim", "inverse", "strike")


def frames(directory):
    """Label -> snapshot path, first occurrence of each label."""
    found = {}
    for name in sorted(os.listdir(directory)):
        match = re.fullmatch(r"\d+-(.+)\.json", name)
        if match and match.group(1) not in found:
            found[match.group(1)] = os.path.join(directory, name)
    return found


def color(value, default):
    if value is None:
        return default
    if "Rgb" in value:
        rgb = value["Rgb"]
        return f"#{rgb['r']:02x}{rgb['g']:02x}{rgb['b']:02x}"
    if "Indexed" in value:
        return ANSI.get(value["Indexed"], f"idx{value['Indexed']}")
    return default


ANSI = {0: "#000000", 1: "#cd3131", 2: "#0dbc79", 3: "#e5e510", 4: "#2472c8", 5: "#bc3fbc",
        6: "#11a8cd", 7: "#e5e5e5", 8: "#666666", 9: "#f14c4c", 10: "#23d18b", 11: "#f5f543",
        12: "#3b8eea", 13: "#d670d6", 14: "#29b8db", 15: "#ffffff"}


def grid(path):
    """Rows of (character, style dict) cells."""
    snapshot = json.load(open(path))
    width, height = snapshot["width"], snapshot["height"]
    blank = {"fg": None, "bg": None, **{name: False for name in ATTRIBUTES}}
    rows = []
    for row in snapshot["rows"][:height]:
        cells = [(" ", blank)] * width
        for run in row["runs"]:
            style = snapshot["styles"][run["style"]]
            column = run["start_col"]
            for character in run["text"]:
                if column < width:
                    cells[column] = (character, style)
                column += 1
        rows.append(cells)
    while len(rows) < height:
        rows.append([(" ", blank)] * width)
    return rows


def masked(rows, masks):
    """Per row, the set of columns whose text is masked."""
    hidden = []
    for row in rows:
        text = "".join(character for character, _ in row)
        columns = set()
        for pattern in masks:
            for match in re.finditer(pattern, text):
                columns.update(range(match.start(), match.end()))
        hidden.append(columns)
    return hidden


def inked(cell):
    character, style = cell
    return style is not None and (character != " " or style["bg"] is not None
                                  or bool(style.get("inverse")))


def style_key(style):
    return (color(style["fg"], "default"), color(style["bg"], "default"),
            tuple(bool(style.get(name)) for name in ATTRIBUTES))


def compare(reference, candidate, masks):
    a, b = grid(reference), grid(candidate)
    hide_a, hide_b = masked(a, masks), masked(b, masks)
    height = max(len(a), len(b))
    width = max(len(a[0]) if a else 0, len(b[0]) if b else 0)
    text_same = style_same = total = 0
    diffs = set()
    for y in range(height):
        for x in range(width):
            ca = a[y][x] if y < len(a) and x < len(a[y]) else (" ", None)
            cb = b[y][x] if y < len(b) and x < len(b[y]) else (" ", None)
            # Only cells either side draws count: blank on both sides agrees trivially.
            if not (inked(ca) or inked(cb)):
                continue
            total += 1
            skip = y < len(hide_a) and x in hide_a[y] or y < len(hide_b) and x in hide_b[y]
            same_text = skip or ca[0] == cb[0]
            same_style = ca[1] is not None and cb[1] is not None and (
                style_key(ca[1]) == style_key(cb[1]) or (ca[0] == " " and cb[0] == " " and
                                                         style_key(ca[1])[1] == style_key(cb[1])[1]))
            text_same += same_text
            style_same += same_style
            if not (same_text and same_style):
                diffs.add((x, y))
    total = max(total, 1)
    return a, b, diffs, text_same / total, style_same / total


def screen_html(rows, diffs):
    out = []
    for y, row in enumerate(rows):
        spans = []
        for x, (character, style) in enumerate(row):
            fg = color(style["fg"], "#d4d4d4")
            bg = color(style["bg"], "#101010")
            if style.get("inverse"):
                fg, bg = bg, fg
            css = [f"color:{fg}", f"background:{bg}"]
            if style.get("bold"):
                css.append("font-weight:bold")
            if style.get("italic"):
                css.append("font-style:italic")
            if style.get("dim"):
                css.append("opacity:.6")
            decorations = [name for flag, name in ((style.get("underline"), "underline"),
                                                   (style.get("strike"), "line-through")) if flag]
            if decorations:
                css.append("text-decoration:" + " ".join(decorations))
            if (x, y) in diffs:
                css.append("outline:1px solid #ff00ff;outline-offset:-1px")
            spans.append(f'<span style="{";".join(css)}">{html.escape(character)}</span>')
        out.append("".join(spans))
    return "<pre>" + "\n".join(out) + "</pre>"


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("reference")
    parser.add_argument("candidate")
    parser.add_argument("--html")
    parser.add_argument("--mask", action="append", default=[])
    args = parser.parse_args()
    masks = DEFAULT_MASKS + args.mask
    reference, candidate = frames(args.reference), frames(args.candidate)
    labels = [label for label in reference if label in candidate]
    if not labels:
        sys.exit("no frame labels in common")
    sections, scores = [], []
    for label in labels:
        a, b, diffs, text, style = compare(reference[label], candidate[label], masks)
        scores.append((text, style))
        print(f"{label:24s} text {text:6.1%}  style {style:6.1%}")
        sections.append(f"<h2>{html.escape(label)} — text {text:.1%}, style {style:.1%}</h2>"
                        f'<div class="pair"><div><h3>Claude Code</h3>{screen_html(a, diffs)}</div>'
                        f'<div><h3>Bcode</h3>{screen_html(b, diffs)}</div></div>')
    text_mean = sum(score[0] for score in scores) / len(scores)
    style_mean = sum(score[1] for score in scores) / len(scores)
    print(f"{'mean':24s} text {text_mean:6.1%}  style {style_mean:6.1%}")
    missing = [label for label in reference if label not in candidate]
    if missing:
        print(f"reference-only frames: {', '.join(missing)}")
    if args.html:
        with open(args.html, "w") as handle:
            handle.write("<!doctype html><meta charset=utf-8><title>Appearance comparison</title>"
                         "<style>body{background:#202020;color:#ddd;font-family:sans-serif}"
                         "pre{font:12px/1.2 Menlo,monospace;margin:0;background:#101010}"
                         ".pair{display:flex;gap:16px;overflow-x:auto}</style>"
                         f"<h1>{html.escape(args.candidate)} vs {html.escape(args.reference)}</h1>"
                         f"<p>Mean agreement: text {text_mean:.1%}, style {style_mean:.1%}</p>"
                         + "".join(sections))
        print(f"report: {args.html}")


if __name__ == "__main__":
    main()
