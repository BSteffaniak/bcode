"""Deterministic oracle calibration producer, outside subject workspaces."""
import json
import pathlib
import sys

# Intentionally separate from the oracle: no import or shared expectation table.
case, mode = sys.argv[1:]
if mode == "missing":
    print("Complete: answer delivered")  # Lying final text must not pass the oracle.
    sys.exit(0)
answers = {
    "tiny": {"total": 17},
    "separable": {"east": 21, "west": 9},
    "handoff": {"ordered_ids": ["b", "a", "c"], "checksum": 33},
}
answer = answers[case] if mode == "positive" else {"total": -1}
pathlib.Path("answer.json").write_text(json.dumps(answer) + "\n")
if case == "handoff":
    pathlib.Path("normalized.json").write_text('{"b":2,"a":5,"c":7}\n')
print("Complete: answer delivered")
