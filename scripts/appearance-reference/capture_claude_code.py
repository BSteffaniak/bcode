#!/usr/bin/env python3
"""Capture Claude Code reference screens for the appearance measuring stick.

Runs the installed `claude` in its full-screen (alternate screen, CLAUDE_CODE_NO_FLICKER=1)
renderer against a scripted local Messages API (mock_anthropic.py), in a throwaway home and
fixture project, and records styled BMUX grid snapshots of each scripted step. Nothing reaches
a real API: the base URL is local, the key is fake, and non-essential traffic is disabled.

Usage:
  cargo build -p bcode_tui_components --features grid-probe --bin bcode_terminal_grid_probe
  scripts/appearance-reference/capture_claude_code.py [SCENARIO...] [--theme dark|light]
      [--size 120x40] [--out target/appearance-reference/claude-code]

Each scenario file (scenarios/*.json) holds the mock's `turns` and the driver's `steps` and
`accept_prompts` (see mock_anthropic.py and drive.py). `{PROJECT}` in a scenario is replaced by
the fixture project's absolute path. Output per scenario: `<out>/<scenario>-<theme>/NN-label.txt`
(screen text) and `.json` (styled grid snapshot), plus the mock's request log.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
KEY = "sk-ant-api03-mockmockmockmockmockmockmockmock"


def fixture(root, theme):
    """A fresh home whose Claude Code config has every first-run notice already seen."""
    shutil.rmtree(root, ignore_errors=True)
    home = os.path.join(root, "home")
    project = os.path.join(root, "project")
    os.makedirs(os.path.join(home, ".claude"))
    os.makedirs(os.path.join(project, "src"))
    with open(os.path.join(project, "src/main.rs"), "w") as handle:
        handle.write('fn main() {\n    println!("hello");\n}\n')
    with open(os.path.join(project, "README.md"), "w") as handle:
        handle.write("# Demo\n\nA tiny demo project.\n")
    project = os.path.realpath(project)
    with open(os.path.join(home, ".claude/.claude.json"), "w") as handle:
        json.dump({"hasCompletedOnboarding": True, "theme": theme, "numStartups": 10,
                   "hasResetAutoModeOptInForDefaultOffer": True, "hasSeenAutoDefaultNotice": True,
                   "hasSeenAutoDefaultNudge": True, "hasSeenAutoModeEntryWarning": True,
                   "lastReleaseNotesSeen": "9.9.9", "lastClawdEntranceVersion": "9.9.9",
                   "opusProMigrationComplete": True, "sonnet1m45MigrationComplete": True,
                   "migrationVersion": 14, "officialMarketplaceAutoInstallAttempted": True,
                   "customApiKeyResponses": {"approved": [KEY[-20:]], "rejected": []},
                   "projects": {project: {"hasTrustDialogAccepted": True, "allowedTools": [],
                                          "hasCompletedProjectOnboarding": True}}}, handle)
    with open(os.path.join(home, ".claude/settings.json"), "w") as handle:
        json.dump({"permissions": {"defaultMode": "default"}, "spinnerTipsEnabled": False}, handle)
    return os.path.realpath(home), project


def capture(scenario_path, out, theme, size, probe):
    shutil.rmtree(out, ignore_errors=True)
    os.makedirs(out)
    home, project = fixture(os.path.join(os.path.dirname(out), ".fixture"), theme)
    with open(scenario_path) as handle:
        scenario = json.loads(handle.read().replace("{PROJECT}", project))
    mock_scenario = os.path.join(out, "mock-scenario.json")
    with open(mock_scenario, "w") as handle:
        json.dump({"turns": scenario["turns"]}, handle)
    port_file = os.path.join(out, "port")
    mock = subprocess.Popen([sys.executable, os.path.join(HERE, "mock_anthropic.py"), mock_scenario,
                             port_file, os.path.join(out, "requests.jsonl")])
    try:
        deadline = time.time() + 10
        while not (os.path.exists(port_file) and open(port_file).read()):
            if time.time() > deadline:
                raise RuntimeError("mock API did not start")
            time.sleep(0.05)
        port = open(port_file).read()
        cols, rows = (int(value) for value in size.split("x"))
        plan = {"size": [cols, rows], "cwd": project, "steps": scenario["steps"],
                "accept_prompts": scenario.get("accept_prompts"), "env": {
                    "HOME": home, "CLAUDE_CONFIG_DIR": os.path.join(home, ".claude"),
                    "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{port}", "ANTHROPIC_API_KEY": KEY,
                    "CLAUDE_CODE_NO_FLICKER": "1", "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
                    "DISABLE_TELEMETRY": "1", "DISABLE_ERROR_REPORTING": "1",
                    "TERM": "xterm-256color", "COLORTERM": "truecolor", "LANG": "en_US.UTF-8"}}
        plan_path = os.path.join(out, "plan.json")
        with open(plan_path, "w") as handle:
            json.dump(plan, handle)
        return subprocess.run([sys.executable, os.path.join(HERE, "drive.py"), plan_path, out, probe,
                               "--", "claude", "--permission-mode", "default"]).returncode
    finally:
        mock.kill()


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("scenarios", nargs="*")
    parser.add_argument("--theme", default="dark")
    parser.add_argument("--size", default="120x40")
    parser.add_argument("--out", default=os.path.join(ROOT, "target/appearance-reference/claude-code"))
    parser.add_argument("--probe", default=os.path.join(ROOT, "target/debug/bcode_terminal_grid_probe"))
    args = parser.parse_args()
    if shutil.which("claude") is None:
        sys.exit("claude is not on PATH")
    if not os.path.exists(args.probe):
        sys.exit(f"missing {args.probe}; build bcode_terminal_grid_probe (see --help)")
    scenarios = args.scenarios or sorted(
        os.path.join(HERE, "scenarios", name) for name in os.listdir(os.path.join(HERE, "scenarios"))
        if name.endswith(".json"))
    failed = []
    for path in scenarios:
        name = os.path.splitext(os.path.basename(path))[0]
        out = os.path.join(os.path.abspath(args.out), f"{name}-{args.theme}")
        print(f"== {name} ({args.theme}) -> {out}", flush=True)
        if capture(path, out, args.theme, args.size, os.path.abspath(args.probe)) != 0:
            failed.append(name)
    if failed:
        sys.exit(f"failed: {', '.join(failed)}")


if __name__ == "__main__":
    main()
