#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python3 - <<'PY'
from pathlib import Path
import json
import subprocess
import tomllib

root = tomllib.loads(Path("Cargo.toml").read_text())
cli = tomllib.loads(Path("packages/cli/Cargo.toml").read_text())
bcode = tomllib.loads(Path("packages/bcode/Cargo.toml").read_text())

hyperchad_dependency = cli["dependencies"]["bcode_hyperchad"]
if hyperchad_dependency.get("optional") is not True:
    raise SystemExit("bcode_cli must keep bcode_hyperchad optional")
if cli["features"].get("web-renderer") != [
    "dep:bcode_hyperchad",
    "bcode_hyperchad/renderer-html-actix",
]:
    raise SystemExit("bcode_cli/web-renderer feature wiring is not exact")
if bcode["features"].get("web-renderer") != ["cli", "bcode_cli/web-renderer"]:
    raise SystemExit("top-level bcode/web-renderer propagation is not exact")

cli_source = "\n".join(path.read_text() for path in Path("packages/cli/src").rglob("*.rs"))
for required in (
    '#[cfg(feature = "web-renderer")]',
    'Commands::Web',
    'bcode_hyperchad',
):
    if required not in cli_source:
        raise SystemExit(f"CLI web command gating is missing {required}")

metadata = json.loads(
    subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--locked"],
        text=True,
    )
)
packages = [package for package in metadata["packages"] if package["name"].startswith("hyperchad")]
if not packages:
    raise SystemExit("Cargo metadata contains no HyperChad packages")
expected_prefix = "git+https://github.com/MoosicBox/MoosicBox.git?branch=master#"
invalid_sources = [
    (package["name"], package.get("source"), package["manifest_path"])
    for package in packages
    if not (package.get("source") or "").startswith(expected_prefix)
]
if invalid_sources:
    raise SystemExit(f"HyperChad packages are not all pinned to upstream Git: {invalid_sources}")
revisions = {package["source"].rsplit("#", 1)[1] for package in packages}
if len(revisions) != 1:
    raise SystemExit(f"HyperChad packages resolve to multiple revisions: {sorted(revisions)}")

root_path = Path.cwd().resolve()
active_manifest_paths = {Path(package["manifest_path"]).resolve() for package in packages}
if any(root_path in path.parents for path in active_manifest_paths):
    raise SystemExit("an active HyperChad package resolves through a local repository path")

patch_tables = [key for key in root if key.startswith("patch")]
if patch_tables:
    raise SystemExit(f"active root Cargo patch tables are not allowed: {patch_tables}")

print(
    "web renderer feature/source guard passed "
    f"({len(packages)} upstream HyperChad packages at {next(iter(revisions))[:8]})"
)
PY

without_tree="$(cargo tree -p bcode_cli --no-default-features --prefix none)"
if grep -q '^bcode_hyperchad ' <<<"$without_tree"; then
    echo "bcode_cli without web-renderer unexpectedly includes bcode_hyperchad" >&2
    exit 1
fi

with_tree="$(cargo tree -p bcode_cli --no-default-features --features web-renderer --prefix none)"
for package in bcode_hyperchad hyperchad_renderer_html_actix hyperchad_renderer_vanilla_js; do
    if ! grep -q "^${package} " <<<"$with_tree"; then
        echo "bcode_cli with web-renderer is missing ${package}" >&2
        exit 1
    fi
done

echo "web renderer feature topology guard passed"
