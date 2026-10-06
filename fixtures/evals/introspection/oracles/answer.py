"""Independent fixture oracle. Never copied into a subject workspace."""
import json
import pathlib
import sys

EXPECTED = {
    "tiny": {"total": 17},
    "separable": {"east": 21, "west": 9},
    "handoff": {"ordered_ids": ["b", "a", "c"], "checksum": 33},
}
HANDOFF = {"b": 2, "a": 5, "c": 7}
INPUTS = {
    "tiny": {"value.txt": "17\n"},
    "separable": {"east.json": '{"values":[3,8,10]}\n',
                  "west.json": '{"values":[-4,6,7]}\n'},
    "handoff": {"records.json": '[{"id":"a","value":" 5 "},{"id":"c","value":"7"},{"id":"b","value":" 2"}]\n'},
}


def check(workspace, case):
    # Hard-coded expectations deliberately do not reuse subject implementation/data.
    try:
        if any((workspace / name).is_symlink()
               or (workspace / name).stat().st_size > 4096
               or (workspace / name).read_text() != expected
               for name, expected in INPUTS[case].items()):
            return False
        answer_path = workspace / "answer.json"
        if answer_path.is_symlink() or answer_path.stat().st_size > 4096:
            return False
        answer = json.loads(answer_path.read_text())
        if answer != EXPECTED[case]:
            return False
        if case == "handoff":
            normalized = workspace / "normalized.json"
            return (not normalized.is_symlink() and normalized.stat().st_size <= 4096
                    and json.loads(normalized.read_text()) == HANDOFF)
        return True
    except (OSError, ValueError, KeyError):
        return False


if __name__ == "__main__":
    # No answers or subject-controlled content are emitted into diagnostic output.
    passed = check(pathlib.Path.cwd(), sys.argv[1])
    print("independent artifact oracle: " + ("pass" if passed else "fail"))
    sys.exit(0 if passed else 1)
