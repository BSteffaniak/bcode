#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${root}"

python3 - <<'PY'
from pathlib import Path
import sys
import tomllib


def dependency(path: Path, name: str):
    manifest = tomllib.loads(path.read_text())
    return manifest.get("dependencies", {}).get(name)


def features(path: Path, name: str) -> set[str]:
    value = dependency(path, name)
    if value is None:
        return set()
    if not isinstance(value, dict) or value.get("workspace") is not True:
        raise SystemExit(f"{path}: {name} must use the workspace dependency")
    requested = value.get("features", [])
    if not isinstance(requested, list) or not all(isinstance(item, str) for item in requested):
        raise SystemExit(f"{path}: {name} features must be a string array")
    if "all" in requested:
        raise SystemExit(f"{path}: production dependencies must not request {name}/all")
    return set(requested)

required_bmux = {
    Path("packages/tui/Cargo.toml"): {
        "action-row", "checkbox", "dialog", "form", "labeled-details", "modal-frame", "picker-frame",
        "scroll-area", "stepper", "text-input",
    },
    Path("plugins/blims-plugin/Cargo.toml"): {"key-hint-bar", "modal-frame", "pane", "text-view"},
    Path("plugins/code-review-plugin/Cargo.toml"): {"key-hint-bar", "modal-frame"},
    Path("plugins/eval-plugin/Cargo.toml"): {
        "action-row", "bar-chart", "dialog", "key-hint-bar", "sparkline", "tab-bar", "table",
        "text-input-box",
    },
    Path("plugins/loop-plugin/Cargo.toml"): {"key-hint-bar", "modal-frame", "status-bar", "text-input-box"},
    Path("plugins/metrics-plugin/Cargo.toml"): {
        "action-row", "bar-chart", "key-hint-bar", "sparkline", "tab-bar", "table",
    },
    Path("plugins/model-plugin/Cargo.toml"): {"pane", "text-view"},
    Path("plugins/question-plugin/Cargo.toml"): {"action-row", "key-hint-bar", "text-input-box"},
    Path("plugins/ralph-plugin/Cargo.toml"): {"key-hint-bar", "selectable-list"},
    Path("plugins/read-plugin/Cargo.toml"): {"key-hint-bar", "text-view"},
    Path("plugins/skills-plugin/Cargo.toml"): {"pane", "text-view"},
    Path("plugins/workflow-plugin/Cargo.toml"): {"key-hint-bar", "pane", "text-view"},
    Path("plugins/worktree-plugin/Cargo.toml"): {"key-hint-bar", "selectable-list", "text-input-box"},
}

required_bcode = {
    Path("packages/tui/Cargo.toml"): {
        "activity", "chrome", "composer", "diff-viewer", "permission", "setup", "source-preview",
        "source-viewer", "terminal-viewer", "transcript",
    },
    Path("packages/vim-edit/Cargo.toml"): {"diff-viewer", "syntax"},
    Path("plugins/document-plugin/Cargo.toml"): {"tool-card"},
    Path("plugins/filesystem-plugin/Cargo.toml"): {
        "diff-viewer", "source-preview", "source-viewer", "syntax", "tool-card",
    },
    Path("plugins/git-plugin/Cargo.toml"): {"tool-card"},
    Path("plugins/ocr-plugin/Cargo.toml"): {"tool-card"},
    Path("plugins/question-plugin/Cargo.toml"): {"tool-card"},
    Path("plugins/shell-plugin/Cargo.toml"): {"terminal-viewer"},
    Path("plugins/vim-edit-plugin/Cargo.toml"): {"tool-card"},
    Path("plugins/web-search-plugin/Cargo.toml"): {"tool-card"},
    Path("plugins/worktree-plugin/Cargo.toml"): {"tool-card"},
}

failures = []
for manifest, expected in required_bmux.items():
    actual = features(manifest, "bmux_tui_components")
    if actual != expected:
        failures.append(f"{manifest}: bmux_tui_components features {sorted(actual)} != {sorted(expected)}")
for manifest, expected in required_bcode.items():
    actual = features(manifest, "bcode_tui_components")
    if actual != expected:
        failures.append(f"{manifest}: bcode_tui_components features {sorted(actual)} != {sorted(expected)}")

for manifest in sorted(Path("packages").glob("**/Cargo.toml")) + sorted(Path("plugins").glob("**/Cargo.toml")):
    for dependency_name in ("bmux_tui_components", "bcode_tui_components"):
        value = dependency(manifest, dependency_name)
        if value is None:
            continue
        if not isinstance(value, dict) or value.get("workspace") is not True:
            failures.append(f"{manifest}: {dependency_name} must use workspace = true")
        if isinstance(value, dict) and "all" in value.get("features", []):
            failures.append(f"{manifest}: production {dependency_name}/all is prohibited")

if failures:
    print("TUI component feature guard failed:", file=sys.stderr)
    for failure in failures:
        print(f"* {failure}", file=sys.stderr)
    raise SystemExit(1)
print("TUI component feature guard passed")
PY
