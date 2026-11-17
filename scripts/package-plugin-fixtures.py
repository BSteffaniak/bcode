#!/usr/bin/env python3
"""Package the dynamic plugin fixtures that Bcode's tests load, and export them.

Each fixture is a plugin crate packaged wrapper-free by BMUX's packager (see
scripts/package-plugin.py): `cargo rustc --crate-type cdylib` with one role
feature and `--cfg bmux_plugin_dylib`, in its own `target/plugin-dylib`
directory. Every artifact is copied to `target/plugin-fixtures/<fixture>/`
because several fixtures share one crate (and so one Cargo output name), and
its absolute path is published under the environment variable the tests read.

    python3 scripts/package-plugin-fixtures.py              # build, print the report
    eval "$(python3 scripts/package-plugin-fixtures.py --print-env)"  # for cargo test
    python3 scripts/package-plugin-fixtures.py --nextest-env  # nextest setup script

`cargo nextest run` runs this automatically for the tests that need it
(.config/nextest.toml). Rebuilds are incremental. Python 3.11+.
"""
import argparse
import json
import os
from pathlib import Path
import runpy
import shutil

# (fixture, crate directory, component, extra features, environment variable)
FIXTURES = [
    # Hosts under test build the static hello example with `tui`, so the
    # dynamic backend carries the same declarations.
    ("hello-backend", "examples/hello-plugin", "backend", ["tui"], "BCODE_HELLO_BACKEND_ARTIFACT"),
    ("hello-frontend", "examples/hello-plugin", "frontend", [], "BCODE_HELLO_FRONTEND_ARTIFACT"),
    ("question-backend", "plugins/question-plugin", "backend", [], "BCODE_QUESTION_BACKEND_ARTIFACT"),
    ("filesystem-backend", "plugins/filesystem-plugin", "backend", [], "BCODE_FILESYSTEM_BACKEND_ARTIFACT"),
    ("shell-backend", "plugins/shell-plugin", "backend", [], "BCODE_SHELL_BACKEND_ARTIFACT"),
]


def main() -> None:
    root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--fixture", action="append", choices=[fixture[0] for fixture in FIXTURES],
                        help="package only this fixture; repeatable (default: all)")
    parser.add_argument("--release", action="store_true")
    output = parser.add_mutually_exclusive_group()
    output.add_argument("--print-env", action="store_true", help="print `export NAME=path` lines")
    output.add_argument("--nextest-env", action="store_true",
                        help="append NAME=path lines to $NEXTEST_ENV (nextest setup scripts)")
    args = parser.parse_args()
    packager = runpy.run_path(str(root / "scripts/package-plugin.py"))["shared_packager"]()
    shared = runpy.run_path(str(packager))
    selected = [fixture for fixture in FIXTURES if not args.fixture or fixture[0] in args.fixture]
    destination_root = root / "target/plugin-fixtures"
    environment = {}
    for name, crate, component, features, variable in selected:
        library, _ = shared["build_direct"](root / crate / "Cargo.toml", component, features,
                                            args.release, None, None, f"bcode-fixture-{name}")
        destination = destination_root / name / library.name
        destination.parent.mkdir(parents=True, exist_ok=True)
        staged = destination.with_name(f".{library.name}.staged")
        shutil.copy2(library, staged)
        os.replace(staged, destination)
        environment[variable] = str(destination)
    report = {"schema_version": 1, "environment": environment}
    (destination_root / "artifacts.json").write_text(json.dumps(report, indent=2) + "\n")
    if args.nextest_env:
        target = os.environ.get("NEXTEST_ENV")
        if not target:
            parser.error("--nextest-env runs as a nextest setup script, which sets NEXTEST_ENV")
        with open(target, "a", encoding="utf-8") as stream:
            for variable, path in environment.items():
                stream.write(f"{variable}={path}\n")
    elif args.print_env:
        for variable, path in environment.items():
            print(f"export {variable}='{path}'")
    else:
        print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
