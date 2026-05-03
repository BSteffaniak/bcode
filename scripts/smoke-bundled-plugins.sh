#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="$(mktemp -d)"
install_root="${root}/target/debug/plugins"
fs_plugin_dir="${install_root}/bcode.filesystem"
shell_plugin_dir="${install_root}/bcode.shell"
openai_plugin_dir="${install_root}/bcode.openai-compatible"
cleanup() {
    rm -rf "${workdir}"
    rm -rf "${fs_plugin_dir}" "${shell_plugin_dir}" "${openai_plugin_dir}"
}
trap cleanup EXIT

cd "${root}"
rm -rf "${fs_plugin_dir}" "${shell_plugin_dir}" "${openai_plugin_dir}"
./scripts/install-bundled-plugins.sh "${install_root}" >/dev/null

export XDG_CONFIG_HOME="${workdir}/config"
export BCODE_CONFIG="${workdir}/bcode.toml"
cat >"${BCODE_CONFIG}" <<EOF
[plugins]
enabled = ["bcode.filesystem", "bcode.shell", "bcode.openai-compatible"]
EOF

target_file="${workdir}/bundled/hello.txt"
write_payload="{\"path\":\"${target_file}\",\"contents\":\"hello bundled filesystem\"}"
read_payload="{\"path\":\"${target_file}\"}"
shell_payload='{"tool_call_id":"smoke-shell","name":"shell.run","arguments":{"command":"printf hello-bundled-shell"}}'

cargo run --quiet -p bcode -- plugin list | grep -q "bcode.filesystem"
cargo run --quiet -p bcode -- plugin list | grep -q "bcode.shell"
cargo run --quiet -p bcode -- plugin list | grep -q "bcode.openai-compatible"
cargo run --quiet -p bcode -- plugin services | grep -q "bcode.filesystem/v1"
cargo run --quiet -p bcode -- plugin services | grep -q "bcode.tool/v1"
cargo run --quiet -p bcode -- plugin services | grep -q "bcode.model-provider/v1"
cargo run --quiet -p bcode -- plugin call bcode.model-provider/v1 models | grep -q "gpt-4.1-mini"
cargo run --quiet -p bcode -- plugin call bcode.filesystem/v1 write "${write_payload}" | grep -q '"bytes_written"'
cargo run --quiet -p bcode -- plugin call bcode.filesystem/v1 read "${read_payload}" | grep -q "hello bundled filesystem"
cargo run --quiet -p bcode -- plugin invoke bcode.shell bcode.tool/v1 invoke_tool "${shell_payload}" | grep -q "hello-bundled-shell"

echo "smoke-bundled-plugins: PASS"
