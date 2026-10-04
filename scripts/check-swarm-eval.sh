#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
binary="${BCODE_SWARM_EVAL_BINARY:?Set an absolute identified Bcode binary}"
work="$(mktemp -d "${TMPDIR:-/tmp}/bcode-swarm-eval.XXXXXX")"
mkdir -p "$work/home" "$work/state" "$work/xdg"
cat > "$work/config.toml" <<'EOF'
[plugins]
enabled = ["bcode.fake-provider", "bcode.filesystem", "bcode.default-agents", "bcode.loop", "bcode.workflow", "bcode.eval"]
[model]
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-echo"
[model.settings]
fake_structured_output_json = "loop_until:1"
[model.prompt_cache]
mode = "off"
EOF
python3 - "$root/fixtures/evals/swarm/offline.toml" "$work/suite.toml" "$binary" "$root/fixtures/evals/swarm/workspace" <<'PY'
import sys,pathlib
text=pathlib.Path(sys.argv[1]).read_text()
text=text.replace('/Volumes/ehdd/GitHub/bcode/target/aarch64-apple-darwin/release/bcode',sys.argv[3]).replace('fixture = "workspace"','fixture = '+repr(sys.argv[4]).replace("'",'"'))
pathlib.Path(sys.argv[2]).write_text(text)
PY
echo "swarm eval artifacts retained: $work"
env -i PATH="$PATH" HOME="$work/home" XDG_CONFIG_HOME="$work/xdg" BCODE_CONFIG="$work/config.toml" BCODE_STATE_DIR="$work/state" "$binary" eval run "$work/suite.toml" --output-root "$work/runs" --run-id offline --fail-under-pass-rate 1
