#!/usr/bin/env python3
"""Stage a plugin's shared-ABI backend wrapper as a discoverable plugin directory.

Builds the thin wrapper with the selected shared packager (scripts/package-plugin.py)
and writes the implementation's declaration verbatim beside the wrapper artifact,
stored under the declaration's own `library` name. The native loader requires the
artifact's embedded declaration to equal the discovered one, so declarations are
never rewritten. Used by smoke scripts; never installs into a user plugin root.
"""
import argparse
from pathlib import Path
import runpy
import shutil
import tomllib


def main() -> None:
    root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("plugin", type=Path, help="implementation directory with Cargo.toml and bcode-plugin.toml")
    parser.add_argument("destination", type=Path, help="plugin directory to create inside a discovery root")
    parser.add_argument("--release", action="store_true")
    args = parser.parse_args()
    implementation = (root / args.plugin).resolve()
    declaration = implementation / "bcode-plugin.toml"
    library = Path(tomllib.loads(declaration.read_text())["runtime"]["library"])
    if library.name != str(library):
        raise SystemExit(f"{declaration}: runtime library must be a bare file name")
    workspace = tomllib.loads((root / "Cargo.toml").read_text())
    patch = workspace.get("patch", {}).get("crates-io", {}).get("bmux_plugin_sdk", {})
    if "path" not in patch:
        raise SystemExit("a local shared SDK patch is required to select the SDK manifest")
    sdk = (root / patch["path"]).resolve() / "Cargo.toml"
    packager = runpy.run_path(str(root / "scripts/package-plugin.py"))["shared_packager"]()
    shared = runpy.run_path(str(packager))
    output = root / "target/plugin-packaging" / implementation.name / "backend"
    with shared["packaging_owner"](output):
        manifest = shared["generate"](implementation / "Cargo.toml", sdk, output, "backend", [], "smoke")
        artifact = shared["build"](manifest, args.release, None,
                                   root / "target/plugin-packaging/target")
    destination = args.destination.resolve()
    destination.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(declaration, destination / "bcode-plugin.toml")
    shutil.copyfile(artifact, destination / library.name)
    print(destination / "bcode-plugin.toml")


if __name__ == "__main__":
    main()
