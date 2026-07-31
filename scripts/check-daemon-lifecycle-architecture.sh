#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${root}"

python3 - <<'PY'
from pathlib import Path

ipc = Path("packages/ipc/src/lib.rs").read_text(encoding="utf-8")
lifecycle = Path("packages/daemon-lifecycle/src/lib.rs").read_text(encoding="utf-8")
client = Path("packages/client/src/lib.rs").read_text(encoding="utf-8")
server = Path("packages/server/src/lib.rs").read_text(encoding="utf-8")
tui_sources = "\n".join(
    path.read_text(encoding="utf-8") for path in Path("packages/tui/src").rglob("*.rs")
)
cli = Path("packages/cli/src/lib.rs").read_text(encoding="utf-8")
plugin_surface_host = Path("packages/tui/src/plugin_surface_host.rs").read_text(encoding="utf-8")
invariants = Path("INVARIANTS.md").read_text(encoding="utf-8")

required = {
    "daemon artifact isolation invariant must remain cataloged":
        "**Daemon artifact versions are isolated.**" in invariants,
    "IPC endpoint routing must not hash the running executable":
        "current_executable_fingerprint()" not in ipc,
    "IPC must expose typed exact artifact identity":
        "ArtifactId" in ipc and "artifact_id" in ipc,
    "lifecycle must own normal daemon startup":
        "pub async fn ensure_daemon_running" in lifecycle,
    "client must route auto-start through lifecycle":
        "ensure_daemon_running" in client,
    "normal CLI daemon availability must route through the client":
        "BcodeClient::default_endpoint()\n        .ensure_daemon_available()" in cli,
    "explicit detached server start must use lifecycle ownership":
        "ServerCommand::Start { foreground }" in cli
        and "start_server_daemon(false).await?" in cli
        and "bcode_daemon_lifecycle::ensure_daemon_running" in cli,
    "TUI must construct the canonical auto-start client":
        "BcodeClient::default_endpoint()" in Path("packages/tui/src/runtime.rs").read_text(encoding="utf-8"),
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
    "plugin surface host must use the explicit embedded server path":
        "run_embedded_with_static_bundled" in plugin_surface_host
        and "run_with_static_bundled(" not in plugin_surface_host,
    "embedded server path must not publish a daemon record":
        "run_with_static_bundled_inner(endpoint, static_plugins, false).await" in server,
    "historical exact responsive daemons must remain gracefully controllable":
        "DaemonRecordClassification::HistoricalExactResponsive" in cli
        and "DaemonControlPolicy::GracefulIpc" in cli,
    "protocol-unsupported and ambiguous historical evidence must remain conservative":
        "DaemonRecordClassification::HistoricalProcessVerifiedProtocolUnsupported" in cli
        and "DaemonControlPolicy::ReviewedForceOnly" in cli
        and "DaemonControlPolicy::PreserveAndRefuse" in cli,
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
