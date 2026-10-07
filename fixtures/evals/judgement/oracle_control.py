"""Offline oracle calibration producer, never benchmark consumer evidence."""
import argparse
import json
from pathlib import Path

from benchmark import check_workspace, load

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("split", choices=("development", "held_out"))
parser.add_argument("case_id")
parser.add_argument("control", choices=("correct", "wrong", "missing", "corrupt"))
args = parser.parse_args()
case = next(c for c in load(args.split)["cases"] if c["id"] == args.case_id)
# Called only inside a disposable, copied calibration workspace by the command executor.
if args.control != "missing":
    answer = case["answer"] if args.control != "wrong" else {"model_claim": "success"}
    Path("answer.json").write_text(json.dumps(answer) + "\n")
if args.control == "corrupt":
    Path(next(iter(case["files"]))).write_text("altered fixture input\n")
passed = check_workspace(case, Path.cwd())
expected = args.control == "correct"
print(json.dumps({"source": "scorer_calibration", "oracle_passed": passed,
                  "expected_pass": expected, "calibration_passed": passed == expected}))
raise SystemExit(0 if passed == expected else 1)
