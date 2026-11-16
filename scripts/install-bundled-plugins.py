#!/usr/bin/env python3
"""Build selected bundled definitions as shared-ABI dynamic components.

Artifacts are installed immutably. This does not activate plugins, replace a
running daemon, or silently override bundled selection.
"""
import argparse
from pathlib import Path
import runpy
import subprocess
import sys
import tomllib


def main() -> None:
    root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("install_root", type=Path, nargs="?", default=root / "target/debug/plugins")
    parser.add_argument("--plugin", action="append", default=[], help="bundled Cargo dependency name; repeatable")
    parser.add_argument("--all", action="store_true", help="package every declared bundled plugin")
    parser.add_argument("--component", choices=["backend", "frontend"], default="backend")
    parser.add_argument("--sdk-manifest", type=Path, help="selected shared SDK Cargo.toml")
    parser.add_argument("--release", action="store_true")
    parser.add_argument("--target")
    args = parser.parse_args()
    workspace = tomllib.loads((root / "Cargo.toml").read_text())
    bundled = tomllib.loads((root / "packages/bundled-plugins/Cargo.toml").read_text())
    dependencies = workspace["workspace"]["dependencies"]
    declared = {selection[4:] for feature, selections in bundled["features"].items()
                if feature.startswith("static-bundled-") for selection in selections
                if selection.startswith("dep:")}
    selected = sorted(declared) if args.all else args.plugin or [
        "bcode_filesystem_plugin", "bcode_shell_plugin",
        "bcode_openai_compatible_provider_plugin", "bcode_default_agents_plugin",
    ]
    unknown = set(selected) - declared
    if unknown:
        parser.error(f"not bundled implementation dependencies: {sorted(unknown)}")
    sdk = args.sdk_manifest
    if sdk is None:
        patch = workspace.get("patch", {}).get("crates-io", {}).get("bmux_plugin_sdk", {})
        if "path" not in patch:
            parser.error("--sdk-manifest is required without a local shared SDK patch")
        sdk = (root / patch["path"]) / "Cargo.toml"
    packager = runpy.run_path(str(root / "scripts/package-plugin.py"))["shared_packager"]()
    for package in dict.fromkeys(selected):
        dependency = dependencies[package]
        if not isinstance(dependency, dict) or "path" not in dependency:
            parser.error(f"{package} has no declared local implementation path")
        implementation = (root / dependency["path"]).resolve()
        command = [sys.executable, str(packager), "--implementation-manifest", str(implementation / "Cargo.toml"),
                   "--sdk-manifest", str(sdk.resolve()), "--output",
                   str(root / "target/plugin-packaging" / package / args.component),
                   "--component", args.component, "--build", "--install-root", str(args.install_root.resolve()),
                   "--declaration", str(implementation / "bcode-plugin.toml")]
        if args.release:
            command.append("--release")
        if args.target:
            command.extend(["--target", args.target])
        subprocess.run(command, check=True, cwd=root)


if __name__ == "__main__":
    main()
