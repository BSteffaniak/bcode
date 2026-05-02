#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
install_root="${1:-${root}/target/debug/plugins}"

cd "${root}"

cargo build --quiet -p bcode_filesystem_plugin

case "$(uname -s)" in
    Darwin)
        dylib_name="libbcode_filesystem_plugin.dylib"
        ;;
    Linux)
        dylib_name="libbcode_filesystem_plugin.so"
        ;;
    MINGW*|MSYS*|CYGWIN*)
        dylib_name="bcode_filesystem_plugin.dll"
        ;;
    *)
        echo "unsupported platform: $(uname -s)" >&2
        exit 1
        ;;
esac

built_dylib="${root}/target/debug/${dylib_name}"
if [[ ! -f "${built_dylib}" ]]; then
    echo "plugin library was not built: ${built_dylib}" >&2
    exit 1
fi

plugin_dir="${install_root}/bcode.filesystem"
mkdir -p "${plugin_dir}"
cp "${built_dylib}" "${plugin_dir}/${dylib_name}"
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
abi_version = 1
library = "${dylib_name}"
event_symbol = "bcode_plugin_handle_event_v1"
service_symbol = "bcode_plugin_invoke_service_v1"
EOF

printf 'installed bundled plugins to %s\n' "${install_root}"
