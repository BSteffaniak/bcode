#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="$(mktemp -d)"
install_root="${root}/target/debug/plugins"
plugin_dir="${install_root}/bcode.filesystem"
cleanup() {
    rm -rf "${workdir}"
    rm -rf "${plugin_dir}"
}
trap cleanup EXIT

cd "${root}"
rm -rf "${plugin_dir}"
./scripts/install-bundled-plugins.sh "${install_root}" >/dev/null

export XDG_CONFIG_HOME="${workdir}/config"
export BCODE_CONFIG="${workdir}/bcode.toml"
cat >"${BCODE_CONFIG}" <<EOF
[plugins]
enabled = ["bcode.filesystem"]
EOF

target_file="${workdir}/bundled/hello.txt"
write_payload="{\"path\":\"${target_file}\",\"contents\":\"hello bundled filesystem\"}"
read_payload="{\"path\":\"${target_file}\"}"

cargo run --quiet -p bcode -- plugin list | grep -q "bcode.filesystem"
cargo run --quiet -p bcode -- plugin services | grep -q "bcode.filesystem/v1"
cargo run --quiet -p bcode -- plugin call bcode.filesystem/v1 write "${write_payload}" | grep -q '"bytes_written"'
cargo run --quiet -p bcode -- plugin call bcode.filesystem/v1 read "${read_payload}" | grep -q "hello bundled filesystem"

echo "smoke-bundled-plugins: PASS"
