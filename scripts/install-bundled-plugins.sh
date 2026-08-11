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

install_plugin() {
    local plugin_dir="$1"
    local dylib_name="$2"
    local source_manifest="$3"
    local built_dylib="${root}/target/debug/${dylib_name}"

    if [[ ! -f "${built_dylib}" ]]; then
        echo "plugin library was not built: ${built_dylib}" >&2
        exit 1
    fi

    mkdir -p "${plugin_dir}"
    cp "${built_dylib}" "${plugin_dir}/${dylib_name}"
    python3 - "${source_manifest}" "${plugin_dir}/bcode-plugin.toml" "${dylib_name}" <<'PY'
from pathlib import Path
import re
import sys

source_path, destination_path, library_name = sys.argv[1:]
manifest = Path(source_path).read_text()
manifest, replacements = re.subn(
    r'(?m)^library\s*=\s*"[^"]+"$',
    f'library      = "{library_name}"',
    manifest,
)
if replacements != 1:
    raise SystemExit(f"{source_path}: expected exactly one runtime library declaration")
Path(destination_path).write_text(manifest)
PY
}

install_plugin \
    "${install_root}/bcode.filesystem" \
    "${fs_dylib_name}" \
    "${root}/plugins/filesystem-plugin/bcode-plugin.toml"
install_plugin \
    "${install_root}/bcode.openai-compatible" \
    "${openai_dylib_name}" \
    "${root}/plugins/openai-compatible-provider-plugin/bcode-plugin.toml"
install_plugin \
    "${install_root}/bcode.default-agents" \
    "${default_agents_dylib_name}" \
    "${root}/plugins/default-agents-plugin/bcode-plugin.toml"
install_plugin \
    "${install_root}/bcode.shell" \
    "${shell_dylib_name}" \
    "${root}/plugins/shell-plugin/bcode-plugin.toml"

printf 'installed bundled plugins to %s\n' "${install_root}"
