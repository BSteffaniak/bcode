"""External functional oracle: only the parent adjudicates expected values.

The subject executes in a disposable subprocess, not the adjudicator. Its output
is untrusted function-result data, never an assertion/pass report. This is not a
sandbox: arbitrary Python has same-user capabilities and can fabricate responses.
Use an OS security boundary for adversarial subjects; this protocol cannot prove
that a returned value was produced by a particular function implementation.
"""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time


# Expectations remain in the parent, not in the subject interpreter.
CASES = (
    ("total", [], 0),
    ("total", [1, 2, 3], 6),
    ("total", [-1, 5, 9], 13),
    ("unique_sorted", [3, 1, 3, 2, 1], [1, 2, 3]),
    ("unique_sorted", [], []),
)
MAX_RESPONSE_BYTES = 4096
WORKER = r'''
import importlib.util
import json
import sys
request = json.loads(sys.stdin.read())
spec = importlib.util.spec_from_file_location('subject_operations', sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
value = getattr(module, request['function'])(request['argument'])
if request['function'] == 'total':
    if type(value) is not int:
        raise TypeError('expected integer result')
else:
    if type(value) is not list or any(type(item) is not int for item in value):
        raise TypeError('expected integer list result')
print(json.dumps({'schema_version': 1, 'result': value}, allow_nan=False))
'''


def _unique_object(items):
    result = {}
    for key, value in items:
        if key in result:
            raise ValueError("duplicate protocol key")
        result[key] = value
    return result


def _valid_result(payload, expected):
    """Strict result schema; booleans/floats are not integer answers."""
    if (type(payload) is not dict
            or set(payload) != {"schema_version", "result"}
            or type(payload["schema_version"]) is not int
            or payload["schema_version"] != 1):
        return False
    value = payload["result"]
    if type(expected) is int:
        return type(value) is int and value == expected
    return (type(value) is list and all(type(item) is int for item in value)
            and value == expected)


def _stop_group(child):
    # Also stop descendants retaining stdout or continuing after the direct child.
    try:
        os.killpg(child.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    child.wait()


def _observe(subject, function, argument, timeout_seconds):
    # A regular file prevents unbounded communicate() buffering or pipe deadlock.
    # Polling caps retained reads, not adversarial disk writes between polls.
    with tempfile.TemporaryFile() as response:
        child = subprocess.Popen(
            [sys.executable, "-I", "-B", "-c", WORKER, str(subject)],
            stdin=subprocess.PIPE, stdout=response, stderr=subprocess.DEVNULL,
            cwd=subject.parent, start_new_session=True,
        )
        deadline = time.monotonic() + timeout_seconds
        try:
            child.stdin.write(json.dumps({"function": function,
                                          "argument": argument}).encode())
            child.stdin.close()
            while child.poll() is None:
                if (time.monotonic() >= deadline
                        or os.fstat(response.fileno()).st_size > MAX_RESPONSE_BYTES):
                    return None
                time.sleep(0.01)
            if child.returncode != 0:
                return None
        except (BrokenPipeError, OSError):
            return None
        finally:
            try:
                child.stdin.close()
            except OSError:
                pass
            _stop_group(child)
        response.seek(0)
        data = response.read(MAX_RESPONSE_BYTES + 1)
        if len(data) > MAX_RESPONSE_BYTES:
            return None
        try:
            return json.loads(data, object_pairs_hook=_unique_object)
        except (ValueError, UnicodeError, RecursionError):
            return None


def check(workspace, timeout_seconds=2.0):
    """Fail closed on missing subject, termination, bad protocol or wrong values.

    Each case gets a fresh interpreter. The five per-case deadlines fit inside
    the existing command judge's 60-second allowance.
    """
    subject = Path(workspace) / "operations.py"
    try:
        if subject.is_symlink() or not subject.is_file() or timeout_seconds <= 0:
            return False
        return all(_valid_result(_observe(subject.resolve(), function, argument,
                                          timeout_seconds), expected)
                   for function, argument, expected in CASES)
    except (OSError, ValueError):
        return False


def main():
    workspace = os.environ.get("BCODE_EVAL_WORKSPACE")
    passed = workspace is not None and check(workspace)
    print("hidden functional checks " + ("passed" if passed else "failed"))
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
