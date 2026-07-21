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

cargo build --quiet -p bcode --features app

cargo build --quiet -p bcode_filesystem_plugin

case "$(uname -s)" in
    Darwin)
        dylib="${root}/target/debug/libbcode_filesystem_plugin.dylib"
        ;;
    Linux)
        dylib="${root}/target/debug/libbcode_filesystem_plugin.so"
        ;;
    MINGW*|MSYS*|CYGWIN*)
        dylib="${root}/target/debug/bcode_filesystem_plugin.dll"
        ;;
    *)
        echo "unsupported platform: $(uname -s)" >&2
        exit 1
        ;;
esac

if [[ ! -f "${dylib}" ]]; then
    echo "plugin library was not built: ${dylib}" >&2
    exit 1
fi

plugin_dir="${workdir}/plugins/filesystem"
mkdir -p "${plugin_dir}"
cat >"${plugin_dir}/bcode-plugin.toml" <<EOF
id = "bcode.filesystem"
name = "Bcode Filesystem Plugin"
version = "0.0.1"

[[services]]
description = "Filesystem read/write utility service"
interface_id = "bcode.filesystem/v1"
name = "Filesystem"

[runtime]
type = "native"
abi_version = 2
library = "${dylib}"
event_symbol = "bcode_plugin_handle_event_v1"
service_symbol = "bcode_plugin_invoke_service_v1"
EOF

export BCODE_CONFIG="${workdir}/bcode.toml"
cat >"${BCODE_CONFIG}" <<EOF
[plugins]
enabled = ["bcode.filesystem"]
EOF

target_file="${workdir}/nested/hello.txt"
write_payload="{\"path\":\"${target_file}\",\"contents\":\"hello filesystem\"}"
read_payload="{\"path\":\"${target_file}\"}"

"${root}/target/debug/bcode" plugin services --root "${workdir}/plugins" | grep "bcode.filesystem/v1" >/dev/null
"${root}/target/debug/bcode" plugin call --root "${workdir}/plugins" bcode.filesystem/v1 write "${write_payload}" | grep '"bytes_written"' >/dev/null
"${root}/target/debug/bcode" plugin call --root "${workdir}/plugins" bcode.filesystem/v1 read "${read_payload}" | grep "hello filesystem" >/dev/null

echo "smoke-filesystem-plugin: PASS"
