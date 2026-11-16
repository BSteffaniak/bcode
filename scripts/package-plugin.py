#!/usr/bin/env python3
"""Use BMUX's shared packager; Bcode does not own another wrapper generator."""
from pathlib import Path
import os
import runpy
import tomllib


def shared_packager() -> Path:
    """Resolve explicit tooling or the workspace's selected local SDK patch."""
    explicit = os.environ.get("BMUX_PLUGIN_PACKAGER")
    if explicit:
        path = Path(explicit).resolve()
    else:
        root = Path(__file__).resolve().parents[1]
        manifest = tomllib.loads((root / "Cargo.toml").read_text())
        sdk = manifest.get("patch", {}).get("crates-io", {}).get("bmux_plugin_sdk", {})
        if "path" not in sdk:
            raise RuntimeError("set BMUX_PLUGIN_PACKAGER to tooling from the selected BMUX SDK revision")
        path = (root / sdk["path"]).resolve().parents[1] / "scripts/package-plugin.py"
    if not path.is_file():
        raise RuntimeError(f"selected shared packager is unavailable: {path}")
    return path


if __name__ == "__main__":
    runpy.run_path(str(shared_packager()), run_name="__main__")
