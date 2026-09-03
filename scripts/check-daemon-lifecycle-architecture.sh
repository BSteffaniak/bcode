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
bedrock = Path("plugins/bedrock-provider-plugin/src/lib.rs").read_text(encoding="utf-8")
tui_sources = "\n".join(
    path.read_text(encoding="utf-8") for path in Path("packages/tui/src").rglob("*.rs")
)
cli = Path("packages/cli/src/lib.rs").read_text(encoding="utf-8")
invariants = Path("INVARIANTS.md").read_text(encoding="utf-8")

required = {
    "daemon artifact isolation invariant must remain cataloged":
        "**Daemon artifact versions are isolated.**" in invariants,
    "daemon disposability invariant must remain cataloged":
        "**Daemon processes are disposable.**" in invariants,
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
    # An in-process server must not advertise a discoverable daemon record, or a client targeting a
    # different artifact could attach to it. `run_with_static_bundled` publishes a record;
    # `run_embedded_with_static_bundled` is the non-publishing entry point in-process hosts must use.
    # `bcode_tui` depends on `bcode_server`, so calling the publishing entry point is reachable.
    # Matching the qualified path avoids colliding with the TUI's own `run_with_static_bundled`.
    "TUI must not start a record-publishing server":
        "bcode_server::run_with_static_bundled" not in tui_sources,
    "embedded server path must not publish a daemon record":
        "run_with_static_bundled_inner(endpoint, static_plugins, false).await" in server,
    "historical exact responsive daemons must remain gracefully controllable":
        "DaemonRecordClassification::HistoricalExactResponsive" in cli
        and "DaemonControlPolicy::GracefulIpc" in cli,
    "protocol-unsupported and ambiguous historical evidence must remain conservative":
        "DaemonRecordClassification::HistoricalProcessVerifiedProtocolUnsupported" in cli
        and "DaemonControlPolicy::ReviewedForceOnly" in cli
        and "DaemonControlPolicy::PreserveAndRefuse" in cli,
    "Bedrock request context must not inherit daemon startup config":
        "let config = request_context\n            .is_none()\n            .then(bcode_config::load_config)" in bedrock,
    "Bedrock request context must not inherit daemon startup environment":
        "if request_context.is_some() {\n                request_value" in bedrock,
    "abandoned model turns without execution descriptors must not replay under daemon defaults":
        "Runtime work recorded before an admitted turn carried a versioned, secret-free execution" in server
        and "const fn enqueue_recovered_model_turn(" in server
        and "false\n}" in server,
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
