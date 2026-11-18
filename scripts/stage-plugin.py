#!/usr/bin/env python3
"""Stage a plugin's packaged backend as a discoverable plugin directory.

Packages the plugin crate itself with the selected shared packager
(scripts/package-plugin.py; no wrapper crate) and writes the crate's declaration
verbatim beside the artifact, stored under the declaration's own `library` name.
The native loader requires the artifact's embedded declaration to equal the
discovered one, so declarations are never rewritten. Used by smoke scripts;
never installs into a user plugin root.
"""
import argparse
from pathlib import Path
import runpy
import shutil
import tomllib


def main() -> None:
    root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("plugin", type=Path, help="plugin crate directory with Cargo.toml and bcode-plugin.toml")
    parser.add_argument("destination", type=Path, help="plugin directory to create inside a discovery root")
    parser.add_argument("--release", action="store_true")
    args = parser.parse_args()
    implementation = (root / args.plugin).resolve()
    declaration = implementation / "bcode-plugin.toml"
    library = Path(tomllib.loads(declaration.read_text())["runtime"]["library"])
    if library.name != str(library):
        raise SystemExit(f"{declaration}: runtime library must be a bare file name")
    packager = runpy.run_path(str(root / "scripts/package-plugin.py"))["shared_packager"]()
    shared = runpy.run_path(str(packager))
    artifact, _ = shared["build_direct"](implementation / "Cargo.toml", "backend", [], args.release,
                                         None, None, "smoke")
    destination = args.destination.resolve()
    destination.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(declaration, destination / "bcode-plugin.toml")
    shutil.copyfile(artifact, destination / library.name)
    print(destination / "bcode-plugin.toml")


if __name__ == "__main__":
    main()
