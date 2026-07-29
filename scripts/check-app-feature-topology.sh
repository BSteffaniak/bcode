#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python3 - <<'PY'
from pathlib import Path
import tomllib

manifest = tomllib.loads(Path("packages/bcode/Cargo.toml").read_text())
features = manifest["features"]
if features.get("app") != ["cli"]:
    raise SystemExit("bcode/app must remain the minimal binary feature and enable only cli")
expected_distribution = {
    "app",
    "bundled-ocr-tesseract",
    "config",
    "mermaid-renderer",
    "static-bundled-plugins",
    "web-renderer",
}
if set(features.get("distribution", [])) != expected_distribution:
    raise SystemExit("bcode/distribution feature composition is not exact")

bins = {target["name"]: target for target in manifest["bin"]}
if bins["bcode"].get("required-features") != ["app"]:
    raise SystemExit("the bcode binary must require only app")
if bins["bcode-mermaid-worker"].get("required-features") != ["mermaid-renderer"]:
    raise SystemExit("the Mermaid worker must be independently gated")
PY

minimal_tree="$(cargo tree -p bcode --no-default-features --features app --edges normal,build --prefix none)"
for package in bcode_tesseract_sys bcode_hyperchad bcode_bundled_plugins; do
    if grep -q "^${package} " <<<"$minimal_tree"; then
        echo "minimal bcode app unexpectedly includes ${package}" >&2
        exit 1
    fi
done

distribution_tree="$(cargo tree -p bcode --no-default-features --features distribution --edges normal,build --prefix none)"
for package in bcode_tesseract_sys bcode_hyperchad bcode_bundled_plugins; do
    if ! grep -q "^${package} " <<<"$distribution_tree"; then
        echo "bcode distribution is missing ${package}" >&2
        exit 1
    fi
done

echo "bcode app feature topology guard passed"
