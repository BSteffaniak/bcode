#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python3 - <<'PY'
from pathlib import Path
import tomllib

workspace = tomllib.loads(Path("Cargo.toml").read_text())
sha2 = workspace["workspace"]["dependencies"]["sha2"]
features = set(sha2.get("features", []))
for feature in ("asm", "asm-aarch64"):
    if feature in features:
        raise SystemExit(
            f"workspace sha2 feature {feature!r} enables sha2-asm on MSVC; "
            "keep the portable implementation for supported release targets"
        )
PY

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
