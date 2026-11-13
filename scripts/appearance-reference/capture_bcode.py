#!/usr/bin/env python3
"""Capture Bcode screens for the same scenarios as capture_claude_code.py.

Runs a Bcode binary with an isolated config, state and daemon. The scenario's model turns are
replayed by the bundled fake provider (`fake_event_script_turns`), with Claude Code's tool calls
translated to Bcode's tools. On Darwin the daemon and TUI run under a sandbox that denies IP
networking.

Usage:
  cargo build -p bcode --bin bcode --features app,static-bundled-plugins,static-bundled-fake-provider-plugin
  cargo build -p bcode_tui_components --features grid-probe --bin bcode_terminal_grid_probe
  scripts/appearance-reference/capture_bcode.py [SCENARIO...] [--theme NAME] [--profile NAME]
      [--variant dark|light] [--size 120x40] [--out target/appearance-reference/bcode]

Output per scenario: `<out>/<scenario>-<label>/NN-label.txt` and `.json`, where the label is the
theme and profile (`default` when neither is given) plus the variant.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import time

import fixture

# Bcode tools standing in for Claude Code's, with the arguments translated.
TOOLS = {
    "Read": lambda i: ("filesystem.read", {"path": i["file_path"]}),
    "Write": lambda i: ("filesystem.write", {"path": i["file_path"], "contents": i["content"]}),
    "Edit": lambda i: ("filesystem.edit", {"path": i["file_path"], "old_text": i["old_string"],
                                           "new_text": i["new_string"]}),
    "Bash": lambda i: ("shell.run", {"command": i["command"]}),
    "AskUserQuestion": lambda i: ("question", {"questions": [
        present({"question": q["question"], "header": q.get("header"),
                 "selection_mode": "multiple" if q.get("multiSelect") else "single",
                 "options": [present({"label": o["label"], "description": o.get("description")})
                             for o in q.get("options", [])]})
        for q in i["questions"]]}),
}


def present(fields):
    """`fields` without absent values; tool schemas reject nulls."""
    return {key: value for key, value in fields.items() if value is not None}


def chunks(text, size):
    return [text[i:i + size] for i in range(0, len(text), size)] or [""]


def event_script(turn):
    """One scenario turn as a fake-provider event script."""
    pace = int(turn.get("pace", 0.01) * 1000)
    steps = [{"event": {"type": "turn_started"}, "delay_ms": int(turn.get("delay", 0) * 1000)}]
    calls = False
    for block in turn["blocks"]:
        if block["type"] == "text":
            steps += [{"event": {"type": "text_delta", "text": piece}, "delay_ms": pace}
                      for piece in chunks(block["text"], 12)]
        elif block["type"] == "thinking":
            steps += [{"event": {"type": "reasoning_delta", "text": piece}, "delay_ms": pace}
                      for piece in chunks(block["thinking"], 12)]
        elif block["type"] == "tool_use":
            name, arguments = TOOLS[block["name"]](block["input"])
            call_id = block["id"]
            steps.append({"event": {"type": "tool_call_started", "call_id": call_id, "name": name}})
            steps += [{"event": {"type": "tool_call_delta", "call_id": call_id, "delta": piece},
                       "delay_ms": pace} for piece in chunks(json.dumps(arguments), 24)]
            steps.append({"event": {"type": "tool_call_finished",
                                    "call": {"id": call_id, "name": name, "arguments": arguments}}})
            calls = True
    steps.append({"event": {"type": "turn_finished",
                            "stop_reason": "tool_call" if calls else "end_turn"}})
    return {"steps": steps}


def config_text(scenario, theme, profile, variant):
    turns = json.dumps([event_script(turn) for turn in scenario["turns"]])
    text = f'''[plugins]
default = "none"
enabled = ["bcode.fake-provider", "bcode.default-agents", "bcode.harness", "bcode.filesystem", "bcode.shell", "bcode.question"]
disabled = ["bcode.openai-compatible", "bcode.bedrock"]

[model]
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-echo"

[model.metadata."fake-echo"]
provider_plugin_id = "bcode.fake-provider"
context_window = 200000
max_output_tokens = 32000

[model.settings]
fake_event_script_turns = {json.dumps(turns)}

[model.prompt_cache]
mode = "off"

[daemon]
idle_shutdown = false

[tui.theme]
variant = "{variant}"
'''
    if theme:
        text += f'name = "{theme}"\n'
    if profile:
        text += f'profile = "{profile}"\n'
    return text


def stop_fixture_processes(work):
    """Stop every process started from the fixture, including daemons relaunched from their
    state-root image copies."""
    subprocess.run(["pkill", "-f", os.path.realpath(work)], capture_output=True)


def capture(scenario_path, out, args):
    shutil.rmtree(out, ignore_errors=True)
    os.makedirs(out)
    work = os.path.join(os.path.dirname(out), ".fixture")
    if os.path.exists(work):
        stop_fixture_processes(work)
    shutil.rmtree(work, ignore_errors=True)
    project = fixture.project(work)
    scenario = fixture.load_scenario(scenario_path, project)
    side = scenario["bcode"]
    home, state, tmp = (os.path.join(work, name) for name in ("home", "state", "tmp"))
    for directory in (home, state, tmp):
        os.makedirs(directory)
    config = os.path.join(work, "bcode.toml")
    with open(config, "w") as handle:
        handle.write(config_text(scenario, args.theme, args.profile, args.variant))
    env = {"HOME": home, "XDG_CONFIG_HOME": os.path.join(home, ".config"),
           "XDG_STATE_HOME": os.path.join(home, ".local/state"),
           "XDG_CACHE_HOME": os.path.join(home, ".cache"), "TMPDIR": tmp,
           "PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "TERM": "xterm-256color", "COLORTERM": "truecolor",
           "LANG": "en_US.UTF-8", "BCODE_CONFIG": config, "BCODE_STATE_DIR": state,
           "RUST_MIN_STACK": "16777216", "BCODE_DISABLE_REMOTE_MODEL_CATALOG": "1",
           "BCODE_NO_ONBOARD": "1"}
    sandbox = []
    if sys.platform == "darwin" and os.path.exists("/usr/bin/sandbox-exec"):
        sandbox = ["/usr/bin/sandbox-exec", "-p",
                   '(version 1)(allow default)(deny network* (remote ip "*:*") (local ip "*:*"))']
    base = sandbox + [args.binary, "--config-only", config, "--state-root", state,
                      "--no-credential-discovery"]
    log_path = os.path.join(out, "daemon.log")
    with open(log_path, "wb") as log:
        daemon = subprocess.Popen(base + ["server", "run"], cwd=project, env=env,
                                  stdin=subprocess.DEVNULL, stdout=log, stderr=log)
    try:
        deadline = time.time() + 60
        while b"server ready; accepting clients" not in open(log_path, "rb").read():
            if daemon.poll() is not None or time.time() > deadline:
                raise RuntimeError(f"daemon did not become ready; see {log_path}")
            time.sleep(0.1)
        cols, rows = (int(value) for value in args.size.split("x"))
        plan = {"size": [cols, rows], "cwd": project, "env": env, "steps": side["steps"],
                "accept_prompts": side.get("accept_prompts")}
        plan_path = os.path.join(out, "plan.json")
        with open(plan_path, "w") as handle:
            json.dump(plan, handle)
        return subprocess.run([sys.executable, os.path.join(fixture.HERE, "drive.py"), plan_path, out,
                               args.probe, "--"] + base).returncode
    finally:
        subprocess.run(base + ["server", "stop"], env=env, cwd=project, capture_output=True,
                       timeout=30)
        if daemon.poll() is None:
            daemon.kill()
        stop_fixture_processes(work)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("scenarios", nargs="*")
    parser.add_argument("--theme")
    parser.add_argument("--profile")
    parser.add_argument("--variant", default="dark")
    parser.add_argument("--size", default="120x40")
    parser.add_argument("--out", default=os.path.join(fixture.ROOT, "target/appearance-reference/bcode"))
    parser.add_argument("--binary", default=os.path.join(fixture.ROOT, "target/debug/bcode"))
    parser.add_argument("--probe", default=fixture.DEFAULT_PROBE)
    args = parser.parse_args()
    for path in (args.binary, args.probe):
        if not os.path.exists(path):
            sys.exit(f"missing {path} (see --help)")
    args.binary, args.probe = os.path.abspath(args.binary), os.path.abspath(args.probe)
    label = "-".join(filter(None, [args.theme, args.profile])) or "default"
    failed = []
    for path in fixture.scenario_paths(args.scenarios):
        name = fixture.scenario_name(path)
        out = os.path.join(os.path.abspath(args.out), f"{name}-{label}-{args.variant}")
        print(f"== {name} ({label}, {args.variant}) -> {out}", flush=True)
        if capture(path, out, args) != 0:
            failed.append(name)
    if failed:
        sys.exit(f"failed: {', '.join(failed)}")


if __name__ == "__main__":
    main()
