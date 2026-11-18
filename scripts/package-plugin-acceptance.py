#!/usr/bin/env python3
"""Run BMUX's shared native acceptance driver over this Bcode worktree.

The driver comes from the same BMUX revision as the selected packager
(scripts/package-plugin.py: the git-pinned SDK checkout); compilation requires an
explicit --build. No installation, runtime launch or provider call. Bcode's own
test fixtures are packaged by scripts/package-plugin-fixtures.py instead.
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
