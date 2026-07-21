#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
install_root="${1:-${root}/target/debug/plugins}"

cd "${root}"

cargo build --quiet \
    -p bcode_filesystem_plugin \
    -p bcode_shell_plugin \
    -p bcode_openai_compatible_provider_plugin \
    -p bcode_default_agents_plugin

case "$(uname -s)" in
    Darwin)
        fs_dylib_name="libbcode_filesystem_plugin.dylib"
        shell_dylib_name="libbcode_shell_plugin.dylib"
        openai_dylib_name="libbcode_openai_compatible_provider_plugin.dylib"
        default_agents_dylib_name="libbcode_default_agents_plugin.dylib"
        ;;
    Linux)
        fs_dylib_name="libbcode_filesystem_plugin.so"
        shell_dylib_name="libbcode_shell_plugin.so"
        openai_dylib_name="libbcode_openai_compatible_provider_plugin.so"
        default_agents_dylib_name="libbcode_default_agents_plugin.so"
        ;;
    MINGW*|MSYS*|CYGWIN*)
        fs_dylib_name="bcode_filesystem_plugin.dll"
        shell_dylib_name="bcode_shell_plugin.dll"
        openai_dylib_name="bcode_openai_compatible_provider_plugin.dll"
        default_agents_dylib_name="bcode_default_agents_plugin.dll"
        ;;
    *)
        echo "unsupported platform: $(uname -s)" >&2
        exit 1
        ;;
esac

install_plugin_library() {
    local plugin_dir="$1"
    local dylib_name="$2"
    local built_dylib="${root}/target/debug/${dylib_name}"
    if [[ ! -f "${built_dylib}" ]]; then
        echo "plugin library was not built: ${built_dylib}" >&2
        exit 1
    fi
    mkdir -p "${plugin_dir}"
    cp "${built_dylib}" "${plugin_dir}/${dylib_name}"
}

fs_plugin_dir="${install_root}/bcode.filesystem"
install_plugin_library "${fs_plugin_dir}" "${fs_dylib_name}"
cat >"${fs_plugin_dir}/bcode-plugin.toml" <<EOF
id = "bcode.filesystem"
name = "Bcode Filesystem Plugin"
version = "0.0.1"

[[services]]
description = "Filesystem read/write utility service"
interface_id = "bcode.filesystem/v1"
name = "Filesystem"

[[services]]
description = "Model-callable filesystem tools"
interface_id = "bcode.tool/v1"
name = "Filesystem Tools"

[runtime]
type = "native"
abi_version = 2
library = "${fs_dylib_name}"
event_symbol = "bcode_plugin_handle_event_v1"
service_symbol = "bcode_plugin_invoke_service_v1"
EOF

openai_plugin_dir="${install_root}/bcode.openai-compatible"
install_plugin_library "${openai_plugin_dir}" "${openai_dylib_name}"
cat >"${openai_plugin_dir}/bcode-plugin.toml" <<EOF
id = "bcode.openai-compatible"
name = "Bcode OpenAI-Compatible Provider"
version = "0.0.1"

[[services]]
description = "OpenAI-compatible chat-completions model provider"
interface_id = "bcode.model-provider/v1"
name = "OpenAI-Compatible Model Provider"

[runtime]
type = "native"
abi_version = 2
library = "${openai_dylib_name}"
event_symbol = "bcode_plugin_handle_event_v1"
service_symbol = "bcode_plugin_invoke_service_v1"
EOF

default_agents_plugin_dir="${install_root}/bcode.default-agents"
install_plugin_library "${default_agents_plugin_dir}" "${default_agents_dylib_name}"
cat >"${default_agents_plugin_dir}/bcode-plugin.toml" <<EOF
id = "bcode.default-agents"
name = "Bcode Default Agents"
version = "0.0.1"

[[services]]
description = "Default plan/build agent profile policy provider"
interface_id = "bcode.agent-profile/v1"
name = "Default Agent Profiles"

[runtime]
type = "native"
abi_version = 2
library = "${default_agents_dylib_name}"
event_symbol = "bcode_plugin_handle_event_v1"
service_symbol = "bcode_plugin_invoke_service_v1"
EOF

shell_plugin_dir="${install_root}/bcode.shell"
install_plugin_library "${shell_plugin_dir}" "${shell_dylib_name}"
cat >"${shell_plugin_dir}/bcode-plugin.toml" <<EOF
id = "bcode.shell"
name = "Bcode Shell Plugin"
version = "0.0.1"

[[services]]
description = "Permissioned model-callable shell execution tools"
interface_id = "bcode.tool/v1"
name = "Shell Tools"

[runtime]
type = "native"
abi_version = 2
library = "${shell_dylib_name}"
event_symbol = "bcode_plugin_handle_event_v1"
service_symbol = "bcode_plugin_invoke_service_v1"
EOF

printf 'installed bundled plugins to %s\n' "${install_root}"
