"""Shared fixture project and scenario loading for appearance reference captures."""

import json
import os
import shutil

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
DEFAULT_PROBE = os.path.join(ROOT, "target/debug/bcode_terminal_grid_probe")


def project(root):
    """A fresh fixture project under `root`; returns its real path."""
    path = os.path.join(root, "project")
    shutil.rmtree(path, ignore_errors=True)
    os.makedirs(os.path.join(path, "src"))
    with open(os.path.join(path, "src/main.rs"), "w") as handle:
        handle.write('fn main() {\n    println!("hello");\n}\n')
    with open(os.path.join(path, "README.md"), "w") as handle:
        handle.write("# Demo\n\nA tiny demo project.\n")
    return os.path.realpath(path)


def load_scenario(path, project_path):
    """The scenario with `{PROJECT}` replaced by the fixture project's path."""
    with open(path) as handle:
        return json.loads(handle.read().replace("{PROJECT}", project_path))


def scenario_paths(names):
    """Named scenario files, or every bundled scenario."""
    if names:
        return names
    directory = os.path.join(HERE, "scenarios")
    return sorted(os.path.join(directory, name) for name in os.listdir(directory)
                  if name.endswith(".json"))


def scenario_name(path):
    return os.path.splitext(os.path.basename(path))[0]
