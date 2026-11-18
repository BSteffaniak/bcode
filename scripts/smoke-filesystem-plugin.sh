#!/usr/bin/env bash
set -euo pipefail

# Smoke tests own isolated process state and must not inherit the invoking daemon.
unset BCODE_DAEMON_LOG BCODE_IPC_ENDPOINT BCODE_IPC_ENDPOINT_NAMESPACE

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="$(mktemp -d /tmp/bcode-smoke.XXXXXX)"
cleanup() {
    rm -rf "${workdir}"
}
trap cleanup EXIT

cd "${root}"

cargo build --quiet -p bcode --features distribution

# Package the filesystem crate's backend (no wrapper crate) and stage it with its
# verbatim declaration. `plugin check` loads the staged artifact itself; service calls go
# through the CLI host, where this distribution's bundled registration takes precedence.
python3 scripts/stage-plugin.py plugins/filesystem-plugin "${workdir}/plugins/filesystem" >/dev/null

export BCODE_CONFIG="${workdir}/bcode.toml"
cat >"${BCODE_CONFIG}" <<EOF
[plugins]
enabled = ["bcode.filesystem"]
EOF

target_file="${workdir}/nested/hello.txt"
write_payload="{\"path\":\"${target_file}\",\"contents\":\"hello filesystem\"}"
read_payload="{\"path\":\"${target_file}\"}"

"${root}/target/debug/bcode" plugin services --root "${workdir}/plugins" | grep "bcode.filesystem/v1" >/dev/null
"${root}/target/debug/bcode" plugin check --root "${workdir}/plugins" | grep $'bcode.filesystem\tOK' >/dev/null
"${root}/target/debug/bcode" plugin call --root "${workdir}/plugins" bcode.filesystem/v1 write "${write_payload}" | grep '"bytes_written"' >/dev/null
"${root}/target/debug/bcode" plugin call --root "${workdir}/plugins" bcode.filesystem/v1 read "${read_payload}" | grep "hello filesystem" >/dev/null

echo "smoke-filesystem-plugin: PASS"
