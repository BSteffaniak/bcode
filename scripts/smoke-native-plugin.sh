#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="$(mktemp -d)"
cleanup() {
    rm -rf "${workdir}"
}
trap cleanup EXIT

cd "${root}"

cargo build --quiet -p bcode_hello_plugin

case "$(uname -s)" in
    Darwin)
        dylib="${root}/target/debug/libbcode_hello_plugin.dylib"
        ;;
    Linux)
        dylib="${root}/target/debug/libbcode_hello_plugin.so"
        ;;
    MINGW*|MSYS*|CYGWIN*)
        dylib="${root}/target/debug/bcode_hello_plugin.dll"
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

plugin_dir="${workdir}/plugins/hello"
mkdir -p "${plugin_dir}"
cat >"${plugin_dir}/bcode-plugin.toml" <<EOF
id = "example.hello"
name = "Hello Example Plugin"
version = "0.0.1"

[runtime]
type = "native"
abi_version = 1
library = "${dylib}"
service_symbol = "bcode_plugin_invoke_service_v1"
EOF

export BCODE_CONFIG="${workdir}/bcode.toml"
cat >"${BCODE_CONFIG}" <<EOF
[plugins]
disabled = ["example.hello"]
EOF

if cargo run --quiet -p bcode -- plugin list --root "${workdir}/plugins" | grep -q "example.hello"; then
    echo "disabled plugin should not be listed" >&2
    exit 1
fi

cat >"${BCODE_CONFIG}" <<EOF
[plugins]
enabled = ["example.hello"]
EOF

cargo run --quiet -p bcode -- plugin list --root "${workdir}/plugins" | grep -q "example.hello"
cargo run --quiet -p bcode -- plugin check --root "${workdir}/plugins" | grep -q $'example.hello\tOK'
cargo run --quiet -p bcode -- plugin invoke --root "${workdir}/plugins" example.hello example-hello/v1 echo "hello service" | grep -q "hello service"

echo "smoke-native-plugin: PASS"
