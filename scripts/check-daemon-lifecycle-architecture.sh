#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${root}"

python3 - <<'PY'
from pathlib import Path

ipc = Path("packages/ipc/src/lib.rs").read_text(encoding="utf-8")
lifecycle = Path("packages/daemon-lifecycle/src/lib.rs").read_text(encoding="utf-8")
client = Path("packages/client/src/lib.rs").read_text(encoding="utf-8")
tui_sources = "\n".join(
    path.read_text(encoding="utf-8") for path in Path("packages/tui/src").rglob("*.rs")
)
cli = Path("packages/cli/src/lib.rs").read_text(encoding="utf-8")

required = {
    "IPC endpoint routing must not hash the running executable":
        "current_executable_fingerprint()" not in ipc,
    "IPC must expose typed exact artifact identity":
        "ArtifactId" in ipc and "artifact_id" in ipc,
    "lifecycle must own normal daemon startup":
        "pub async fn ensure_daemon_running" in lifecycle,
    "client must route auto-start through lifecycle":
        "ensure_daemon_running" in client,
    "client warm path must not retain a three-attempt loop":
        "for _ in 0..3" not in client,
    "normal TUI must not host a daemon":
        "TuiDaemonHost" not in tui_sources and "ensure_daemon_running_in_process" not in tui_sources,
    "normal TUI source must not depend on daemon lifecycle":
        "bcode_daemon_lifecycle" not in "\n".join(
            path.read_text(encoding="utf-8")
            for path in Path("packages/tui/src").rglob("*.rs")
            if path.name != "tests.rs"
        ),
    "CLI startup must not eagerly materialize the daemon image":
        "ensure_current_executable_cached()" not in cli,
}

failures = [message for message, passed in required.items() if not passed]
if failures:
    for failure in failures:
        print(f"daemon lifecycle architecture violation: {failure}")
    raise SystemExit(1)
PY

echo "daemon lifecycle architecture guard passed"
