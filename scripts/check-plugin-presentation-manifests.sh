#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python3 - <<'PY'
from pathlib import Path
import sys
import tomllib

inventory_path = Path("scripts/plugin-presentation-manifest-inventory.tsv")
expected = {}
for line_number, raw_line in enumerate(inventory_path.read_text().splitlines(), 1):
    if not raw_line or raw_line.startswith("#"):
        continue
    fields = raw_line.split("\t")
    if len(fields) != 3:
        raise SystemExit(f"{inventory_path}:{line_number}: expected three tab-separated fields")
    plugin_id, schemas, surfaces = fields
    if plugin_id in expected:
        raise SystemExit(f"{inventory_path}:{line_number}: duplicate plugin id {plugin_id}")
    expected[plugin_id] = (
        tuple(() if schemas == "-" else schemas.split(",")),
        tuple(() if surfaces == "-" else surfaces.split(",")),
    )

actual = {}
for manifest_path in sorted(Path("plugins").glob("*/bcode-plugin.toml")):
    with manifest_path.open("rb") as manifest_file:
        manifest = tomllib.load(manifest_file)
    adapters = manifest.get("visual_adapters", [])
    surfaces = manifest.get("tui_surfaces", [])
    if not adapters and not surfaces:
        continue
    plugin_id = manifest["id"]
    if plugin_id in actual:
        raise SystemExit(f"duplicate presentation plugin id {plugin_id}")
    schemas = tuple(adapter["artifact_schema"] for adapter in adapters)
    surface_kinds = tuple(surface["kind"] for surface in surfaces)
    if len(set(schemas)) != len(schemas):
        raise SystemExit(f"{manifest_path}: duplicate visual adapter schema")
    if len(set(surface_kinds)) != len(surface_kinds):
        raise SystemExit(f"{manifest_path}: duplicate TUI surface kind")
    actual[plugin_id] = (schemas, surface_kinds)

if actual != expected:
    print("Plugin presentation manifest inventory is stale.", file=sys.stderr)
    for plugin_id in sorted(set(actual) | set(expected)):
        if actual.get(plugin_id) != expected.get(plugin_id):
            print(f"* {plugin_id}", file=sys.stderr)
            print(f"  inventory: {expected.get(plugin_id)}", file=sys.stderr)
            print(f"  manifests: {actual.get(plugin_id)}", file=sys.stderr)
    raise SystemExit(1)

print(
    "plugin presentation manifest inventory passed "
    f"({sum(len(value[0]) for value in actual.values())} visual adapters, "
    f"{sum(len(value[1]) for value in actual.values())} TUI surfaces)"
)
PY
