#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${root}"

python3 - <<'PY'
from pathlib import Path

build_info = Path("packages/build-info/src/lib.rs").read_text(encoding="utf-8")
binary = Path("packages/bcode/src/main.rs").read_text(encoding="utf-8")
build_script = Path("packages/bcode/build.rs").read_text(encoding="utf-8")
cli = Path("packages/cli/src/lib.rs").read_text(encoding="utf-8")
tui = Path("packages/tui/src/render.rs").read_text(encoding="utf-8")
ipc = Path("packages/ipc/src/lib.rs").read_text(encoding="utf-8")
lifecycle = Path("packages/daemon-lifecycle/src/lib.rs").read_text(encoding="utf-8")
xtask = Path("xtask/src/main.rs").read_text(encoding="utf-8")

required = {
    "build display semantics must have a domain owner":
        "pub struct BuildInfo" in build_info and "pub enum BuildMode" in build_info,
    "final product binary must construct typed build information":
        "fn build_info() -> bcode_build_info::BuildInfo" in binary,
    "distribution mode must be an explicit build input":
        "BCODE_DISTRIBUTION_BUILD" in build_script
        and 'Self::Distribution => "1"' in xtask,
    "CLI version must consume canonical build information":
        "root_command_with_build_info" in cli
        and ".version(build_info.display_version())" in cli,
    "TUI header must consume canonical build information":
        "super::build_info().display_version()" in tui,
    "visible build version must not replace exact IPC artifact identity":
        "pub const ARTIFACT_ID" in ipc
        and "ArtifactId::current" in lifecycle
        and "display_version" not in lifecycle,
    "daemon routing must not depend on build display metadata":
        "bcode_build_info" not in lifecycle
        and "BCODE_BUILD_MODE" not in lifecycle,
    "release packaging must verify exact displayed version":
        "verify_binary_version" in xtask
        and "packaged version mismatch" in xtask,
}

failures = [message for message, passed in required.items() if not passed]
if failures:
    for failure in failures:
        print(f"build version architecture violation: {failure}")
    raise SystemExit(1)
PY

echo "build version architecture guard passed"
