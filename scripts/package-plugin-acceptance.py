#!/usr/bin/env python3
"""Prepare shared native acceptance wrappers; compilation requires explicit --build.

Delegate to tooling selected by the same SDK patch or BMUX_PLUGIN_PACKAGER used
for ordinary Bcode packaging. No installation, runtime launch or provider call.
"""
from pathlib import Path
import runpy
import sys


if __name__ == "__main__":
    root = Path(__file__).resolve().parents[1]
    packager = runpy.run_path(str(root / "scripts/package-plugin.py"))["shared_packager"]()
    driver = packager.with_name("package-plugin-acceptance.py")
    if not driver.is_file():
        raise RuntimeError(f"selected SDK acceptance driver unavailable: {driver}")
    sys.argv = [str(driver), "--bcode-root", str(root), *sys.argv[1:]]
    runpy.run_path(str(driver), run_name="__main__")
