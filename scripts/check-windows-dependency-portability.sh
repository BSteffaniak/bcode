#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

windows_tree="$(mktemp)"
trap 'rm -f "$windows_tree"' EXIT
cargo tree \
    --workspace \
    --target x86_64-pc-windows-msvc \
    --edges normal,build \
    --prefix none >"$windows_tree"
if grep -q '^sha2-asm ' "$windows_tree"; then
    echo "Windows dependency graph includes sha2-asm, whose GNU assembly sources are incompatible with MSVC" >&2
    exit 1
fi

host_tree="$(mktemp)"
trap 'rm -f "$windows_tree" "$host_tree"' EXIT
cargo tree --workspace --edges normal,build --prefix none >"$host_tree"
if grep -q '^sha2-asm ' "$host_tree"; then
    echo "host dependency graph includes sha2-asm; supported targets must use the portable SHA-2 implementation" >&2
    exit 1
fi

echo "Windows dependency portability guard passed"
