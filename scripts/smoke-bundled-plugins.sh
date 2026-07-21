#!/usr/bin/env bash
set -euo pipefail

# Smoke tests own isolated process state and must not inherit the invoking daemon.
unset BCODE_DAEMON_LOG BCODE_IPC_ENDPOINT BCODE_IPC_ENDPOINT_NAMESPACE

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="$(mktemp -d /tmp/bcode-smoke.XXXXXX)"
install_root="${root}/target/debug/plugins"
fs_plugin_dir="${install_root}/bcode.filesystem"
shell_plugin_dir="${install_root}/bcode.shell"
openai_plugin_dir="${install_root}/bcode.openai-compatible"
default_agents_plugin_dir="${install_root}/bcode.default-agents"
cleanup() {
    rm -rf "${workdir}"
    rm -rf \
        "${fs_plugin_dir}" \
        "${shell_plugin_dir}" \
        "${openai_plugin_dir}" \
        "${default_agents_plugin_dir}"
}
trap cleanup EXIT

cd "${root}"

cargo build --quiet -p bcode --features app
rm -rf \
    "${fs_plugin_dir}" \
    "${shell_plugin_dir}" \
    "${openai_plugin_dir}" \
    "${default_agents_plugin_dir}"
./scripts/install-bundled-plugins.sh "${install_root}" >/dev/null

export XDG_CONFIG_HOME="${workdir}/config"
export BCODE_CONFIG="${workdir}/bcode.toml"
cat >"${BCODE_CONFIG}" <<EOF
[plugins]
enabled = ["bcode.filesystem", "bcode.shell", "bcode.openai-compatible", "bcode.default-agents"]
EOF

target_file="${workdir}/bundled/hello.txt"
write_payload="{\"path\":\"${target_file}\",\"contents\":\"hello bundled filesystem\"}"
read_payload="{\"path\":\"${target_file}\"}"
shell_list_payload='{}'

"${root}/target/debug/bcode" plugin list | grep "bcode.filesystem" >/dev/null
"${root}/target/debug/bcode" plugin list | grep "bcode.shell" >/dev/null
"${root}/target/debug/bcode" plugin list | grep "bcode.openai-compatible" >/dev/null
"${root}/target/debug/bcode" plugin list | grep "bcode.default-agents" >/dev/null
"${root}/target/debug/bcode" plugin services | grep "bcode.filesystem/v1" >/dev/null
"${root}/target/debug/bcode" plugin services | grep "bcode.tool/v1" >/dev/null
"${root}/target/debug/bcode" plugin services | grep "bcode.model-provider/v1" >/dev/null
"${root}/target/debug/bcode" plugin services | grep "bcode.agent-profile/v1" >/dev/null
"${root}/target/debug/bcode" plugin call bcode.model-provider/v1 models | grep '"provider_id":"openai"' >/dev/null
"${root}/target/debug/bcode" plugin call bcode.filesystem/v1 write "${write_payload}" | grep '"bytes_written"' >/dev/null
"${root}/target/debug/bcode" plugin call bcode.filesystem/v1 read "${read_payload}" | grep "hello bundled filesystem" >/dev/null
"${root}/target/debug/bcode" plugin invoke bcode.shell bcode.tool/v1 list_tools "${shell_list_payload}" | grep '"name":"shell.run"' >/dev/null

echo "smoke-bundled-plugins: PASS"
