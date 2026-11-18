#!/usr/bin/env python3
"""Use BMUX's shared packager; Bcode does not own another one.

The packager is taken from the BMUX revision this workspace actually resolves
`bmux_plugin_sdk` to (`cargo metadata`): the git-pinned checkout, or a local
path patch when one is selected. Plugin crates package themselves wrapper-free
(`define_plugin!` plus the `dynamic-backend`/`dynamic-frontend` role features);
see docs/dynamic-plugin-packaging.md. `BMUX_PLUGIN_PACKAGER` overrides the
location only to test other tooling.
"""
from pathlib import Path
import json
import os
import runpy
import subprocess

SDK_PACKAGE = "bmux_plugin_sdk"


def workspace_root() -> Path:
    return Path(__file__).resolve().parents[1]


def selected_sdk_manifest(root: Path | None = None) -> Path:
    """The `bmux_plugin_sdk` manifest this workspace resolves, as Cargo selected it."""
    root = root or workspace_root()
    result = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--manifest-path", str(root / "Cargo.toml")],
        check=True, stdout=subprocess.PIPE, text=True, cwd=root)
    candidates = {Path(package["manifest_path"]) for package in json.loads(result.stdout)["packages"]
                  if package["name"] == SDK_PACKAGE}
    if len(candidates) != 1:
        raise RuntimeError(f"expected one resolved {SDK_PACKAGE}, found {len(candidates)}")
    return candidates.pop()


def shared_packager() -> Path:
    """BMUX's `scripts/package-plugin.py` from the selected SDK's own source tree."""
    explicit = os.environ.get("BMUX_PLUGIN_PACKAGER")
    if explicit:
        path = Path(explicit).resolve()
    else:
        # <bmux>/packages/plugin-sdk/Cargo.toml -> <bmux>/scripts/package-plugin.py
        path = selected_sdk_manifest().parents[2] / "scripts/package-plugin.py"
    if not path.is_file():
        raise RuntimeError(f"selected shared packager is unavailable: {path}")
    return path


if __name__ == "__main__":
    runpy.run_path(str(shared_packager()), run_name="__main__")
