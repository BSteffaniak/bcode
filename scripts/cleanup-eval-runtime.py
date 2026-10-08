#!/usr/bin/env python3
"""Release only this harness's daemons and prune offline disposable images.

Reports/canonical state are retained. An incomplete or busy cleanup preserves images.
All control and ownership decisions remain in the supported Bcode application APIs.
"""
import os
from pathlib import Path
import subprocess
import sys


def cleanup(binary, work):
    work = Path(work).absolute()
    if not work.is_dir():
        raise ValueError("harness root unavailable")
    roots = sorted({p.parent for p in work.rglob("daemon-images") if p.is_dir()})
    success = True
    for root in roots:
        env = os.environ.copy()
        env.update(HOME=str(work / "home"), XDG_CONFIG_HOME=str(work / "xdg"),
                   BCODE_STATE_DIR=str(root))
        for config in (work / "config.toml", work / "bcode.toml"):
            if config.is_file():
                env["BCODE_CONFIG"] = str(config)
                break
        # Prevent inherited routing from reaching the user's daemon.
        for key in ("BCODE_SOCKET", "BCODE_IPC_ENDPOINT", "BCODE_IPC_ENDPOINT_NAMESPACE",
                    "BCODE_CONFIG_TOML", "BCODE_SESSION_STORE_DIR", "BCODE_STATE_PROFILE"):
            env.pop(key, None)
        try:
            subprocess.run([binary, "--state-root", str(root), "server", "stop-if-idle", "--wait"],
                           env=env, capture_output=True, timeout=40, check=False)
        except (OSError, subprocess.TimeoutExpired):
            pass
        try:
            result = subprocess.run([binary, "--state-root", str(root), "server", "prune-idle-images"],
                                    env=env, capture_output=True, timeout=40, check=False)
            if result.returncode:
                success = False
                print(f"runtime images preserved; cleanup unverified: {root}", file=sys.stderr)
        except (OSError, subprocess.TimeoutExpired):
            success = False
            print(f"runtime images preserved; cleanup incomplete: {root}", file=sys.stderr)
    return success


if __name__ == "__main__":
    sys.exit(0 if cleanup(sys.argv[1], sys.argv[2]) else 1)
