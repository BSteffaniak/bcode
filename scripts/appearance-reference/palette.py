#!/usr/bin/env python3
"""List every distinct style in styled grid snapshots with sample text, most used first.

Usage: palette.py SNAPSHOT.json...
"""

import json
import sys
from collections import defaultdict


def color(value):
    if value is None:
        return "default"
    if "Rgb" in value:
        rgb = value["Rgb"]
        return f"#{rgb['r']:02x}{rgb['g']:02x}{rgb['b']:02x}"
    if "Indexed" in value:
        return f"idx{value['Indexed']}"
    return json.dumps(value)


def key(style):
    flags = [name for name in ("bold", "italic", "underline", "inverse", "dim", "strike") if style[name]]
    return (color(style["fg"]), color(style["bg"]), tuple(flags))


def main():
    cells = defaultdict(int)
    samples = defaultdict(list)
    for path in sys.argv[1:]:
        snapshot = json.load(open(path))
        for row in snapshot["rows"]:
            for run in row["runs"]:
                text = run["text"].strip()
                if not text:
                    continue
                k = key(snapshot["styles"][run["style"]])
                cells[k] += len(text)
                if len(samples[k]) < 4 and text not in samples[k]:
                    samples[k].append(text[:40])
    for k, count in sorted(cells.items(), key=lambda item: -item[1]):
        fg, bg, flags = k
        print(f"{count:6d}  fg={fg:9s} bg={bg:9s} {','.join(flags):14s} {samples[k]}")


if __name__ == "__main__":
    main()
