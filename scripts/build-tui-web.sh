#!/usr/bin/env bash
# Build immutable browser-local assets with the resolved BMUX bridge and exact ABI.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"
if [[ $# -ne 1 ]]; then
  echo 'usage: bash scripts/build-tui-web.sh NEW_OUTPUT_DIRECTORY' >&2
  exit 2
fi
# Check the named destination before canonicalizing: realpath otherwise follows
# a dangling final symlink and accidentally publishes at its target.
OUTPUT=$(realpath -ms "$1")
if [[ -e "$OUTPUT" || -L "$OUTPUT" ]]; then
  echo 'output directory must not exist (immutable artifacts)' >&2
  exit 2
fi
if [[ -z ${BCODE_TUI_WEB_IN_NIX:-} ]]; then
  exec nix develop --command env BCODE_TUI_WEB_IN_NIX=1 bash "$0" "$OUTPUT"
fi
# Metadata resolves the selected patch/dependency, not an unrelated worktree.
METADATA=$(cargo metadata --locked --format-version 1)
BMUX_MANIFEST=$(printf '%s' "$METADATA" | python3 -c 'import json,sys; p=[p for p in json.load(sys.stdin)["packages"] if p["name"]=="bmux_web_renderer"]; assert len(p)==1,"ambiguous BMUX renderer"; print(p[0]["manifest_path"])')
BMUX_SOURCE=$(printf '%s' "$METADATA" | python3 -c 'import json,os,sys; p=[p for p in json.load(sys.stdin)["packages"] if p["name"]=="bmux_web_renderer"]; assert len(p)==1,"ambiguous BMUX renderer"; source=p[0].get("source"); assert source or os.environ.get("BCODE_TUI_WEB_ALLOW_LOCAL_BMUX")=="1","distribution requires a resolved remote BMUX source; local integration requires BCODE_TUI_WEB_ALLOW_LOCAL_BMUX=1"; print(source or "local-integration:"+p[0]["manifest_path"])')
BRIDGE="$(dirname "$BMUX_MANIFEST")/browser/interactive-surface.js"
[[ -f "$BRIDGE" ]] || { echo 'resolved BMUX interactive bridge missing' >&2; exit 1; }
VERSION=$(python3 -c 'import tomllib; p=[p for p in tomllib.load(open("Cargo.lock","rb"))["package"] if p["name"]=="wasm-bindgen"]; assert len(p)==1,"ambiguous wasm-bindgen ABI"; print(p[0]["version"])')
TARGET=$(printf '%s' "$METADATA" | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
if [[ -n ${WASM_BINDGEN:-} ]]; then
  [[ "$WASM_BINDGEN" = /* && -x "$WASM_BINDGEN" ]] || { echo 'WASM_BINDGEN must be an absolute executable path' >&2; exit 2; }
  BINDGEN="$WASM_BINDGEN"
else
  TOOL_ROOT="$TARGET/tui-web-tools/wasm-bindgen-$VERSION"
  BINDGEN="$TOOL_ROOT/bin/wasm-bindgen"
  if [[ ! -x "$BINDGEN" ]]; then
    cargo install --locked --version "=$VERSION" --root "$TOOL_ROOT" wasm-bindgen-cli
  fi
fi
[[ $("$BINDGEN" --version) == "wasm-bindgen $VERSION" ]] || { echo 'wasm-bindgen CLI/lockfile ABI mismatch' >&2; exit 1; }
# The native Bcode shell need not ship a WASM standard library. Use the locked
# toolchain from the Cargo-resolved BMUX revision, never a sibling checkout.
WASM_LIBDIR=$(rustc --print target-libdir --target wasm32-unknown-unknown)
WASM_STDLIB=("$WASM_LIBDIR"/libstd-*.rlib)
if [[ ! -f ${WASM_STDLIB[0]} ]]; then
  BMUX_ROOT=$(dirname "$(dirname "$(dirname "$BMUX_MANIFEST")")")
  if [[ ${BCODE_TUI_WEB_IN_NIX:-} == wasm || ! -f "$BMUX_ROOT/flake.lock" ]]; then
    echo 'selected toolchain lacks wasm32-unknown-unknown standard library' >&2
    exit 1
  fi
  exec nix develop "$BMUX_ROOT" --command env BCODE_TUI_WEB_IN_NIX=wasm \
    WASM_BINDGEN="$BINDGEN" bash "$0" "$OUTPUT"
fi
# Only the C compilation for this target changes. Native tools, Rust selection
# and Cargo's target directory remain those of the selected development shell.
# Grammars do not inherit tree-sitter's DEP_* include metadata, so supply the
# same checksum-locked freestanding headers explicitly (never a WASI sysroot).
WASM_HEADERS=$(printf '%s' "$METADATA" | python3 -c '
import hashlib,json,pathlib,sys,tomllib,tarfile
metadata=json.load(sys.stdin)
packages=[p for p in metadata["packages"] if p["name"]=="tree-sitter-language"]
assert len(packages)==1, "expected exactly one tree-sitter-language header package"
p=packages[0]
locked=[entry for entry in tomllib.load(open("Cargo.lock","rb"))["package"] if entry["name"]==p["name"] and entry["version"]==p["version"] and entry.get("source")==p.get("source")]
assert len(locked)==1 and locked[0].get("checksum"), "headers must resolve to a checksum-locked registry package"
root=pathlib.Path(p["manifest_path"]).parent
# Cargo registry sources do not necessarily contain vendor checksum manifests.
# Verify the cached registry archive against Cargo.lock, then compare headers to
# its authenticated members without extracting or modifying the shared cache.
archive=root.parents[2]/"cache"/root.parent.name/(root.name+".crate")
assert hashlib.sha256(archive.read_bytes()).hexdigest()==locked[0]["checksum"], "header package checksum differs from lockfile"
headers=root/"wasm/include"
assert all((headers/name).is_file() for name in ("stdlib.h","stdint.h","string.h","stdio.h","assert.h")), "missing freestanding tree-sitter headers"
with tarfile.open(archive) as package:
    for header in headers.glob("*.h"):
        relative=header.relative_to(root).as_posix()
        member=package.extractfile(root.name+"/"+relative)
        assert member is not None and member.read()==header.read_bytes(), "freestanding header checksum mismatch"
assert not any(c.isspace() for c in str(headers)), "header path cannot contain whitespace in cc flags"
print(headers)
')
# Select nixpkgs from the shell actually used above, including the existing
# locked BMUX fallback. fetchTree checks both its immutable revision and NAR hash.
C_FLAKE="$ROOT"
if [[ ${BCODE_TUI_WEB_IN_NIX:-} == wasm ]]; then
  C_FLAKE=$(dirname "$(dirname "$(dirname "$BMUX_MANIFEST")")")
fi
NIXPKGS_LOCK=$(python3 - "$C_FLAKE/flake.lock" <<'PY'
import json,sys
lock=json.load(open(sys.argv[1]))
node=lock["nodes"][lock["nodes"][lock["root"]]["inputs"]["nixpkgs"]]["locked"]
assert node.get("type")=="github" and node.get("rev") and node.get("narHash"), "immutable nixpkgs lock required"
# A Nix quoted string containing JSON, not interpolated executable Nix source.
print(json.dumps(json.dumps(node)))
PY
)
C_TOOLS=$(nix build --no-link --print-out-paths --impure --expr "
  let source = builtins.fetchTree (builtins.fromJSON $NIXPKGS_LOCK);
      pkgs = import source { system = builtins.currentSystem; };
  in [ pkgs.llvmPackages.clang-unwrapped pkgs.llvmPackages.llvm ]")
mapfile -t C_TOOL_PATHS <<< "$C_TOOLS"
[[ ${#C_TOOL_PATHS[@]} == 2 ]] || { echo 'pinned wasm C compiler/archive tools missing or ambiguous' >&2; exit 1; }
C_COMPILERS=()
C_ARCHIVERS=()
for TOOL_PATH in "${C_TOOL_PATHS[@]}"; do
  [[ ! -x "$TOOL_PATH/bin/clang" ]] || C_COMPILERS+=("$TOOL_PATH/bin/clang")
  [[ ! -x "$TOOL_PATH/bin/llvm-ar" ]] || C_ARCHIVERS+=("$TOOL_PATH/bin/llvm-ar")
done
[[ ${#C_COMPILERS[@]} == 1 && ${#C_ARCHIVERS[@]} == 1 ]] || {
  echo 'pinned wasm C compiler/archive tools missing or ambiguous' >&2; exit 1;
}
export CC_wasm32_unknown_unknown="${C_COMPILERS[0]}"
export AR_wasm32_unknown_unknown="${C_ARCHIVERS[0]}"
export CFLAGS_wasm32_unknown_unknown="--target=wasm32-unknown-unknown -I$WASM_HEADERS"
cargo build --locked -p bcode_tui_web --target wasm32-unknown-unknown --features wasm --release
# Generate into a private sibling. A failed bindgen/copy/manifest step must not
# publish a partial distribution under the caller's final path.
mkdir -p "$(dirname "$OUTPUT")"
STAGING=$(mktemp -d "${OUTPUT}.building.XXXXXXXX")
trap '[[ ! -d "$STAGING" ]] || printf "Unpublished build retained at %s\\n" "$STAGING" >&2' EXIT
"$BINDGEN" "$TARGET/wasm32-unknown-unknown/release/bcode_tui_web.wasm" --target web --out-dir "$STAGING" --out-name bcode_tui_web --no-typescript
cp packages/tui-web/browser/index.html packages/tui-web/browser/bootstrap.js "$STAGING/"
cp packages/tui-web/src/browser-session.js "$STAGING/"
cp "$BRIDGE" "$STAGING/interactive-surface.js"
python3 - "$STAGING" "$VERSION" "$BMUX_SOURCE" "$BINDGEN" <<'PY'
import hashlib,json,pathlib,sys
root=pathlib.Path(sys.argv[1])
files={p.name:{"sha256":hashlib.sha256(p.read_bytes()).hexdigest(),"bytes":p.stat().st_size} for p in sorted(root.iterdir()) if p.is_file()}
required={"index.html","bootstrap.js","browser-session.js","interactive-surface.js","bcode_tui_web.js","bcode_tui_web_bg.wasm"}
assert files.keys()==required, "unexpected artifact files"
identity=hashlib.sha256(json.dumps(files,sort_keys=True,separators=(",",":")).encode()).hexdigest()
manifest={"schema_version":2,"protocol":{"session_view":1,"browser_bridge":1,"canvas":1},"artifact_sha256":identity,"wasm_bindgen_version":sys.argv[2],"bmux_source":sys.argv[3],"wasm_bindgen_cli_sha256":hashlib.sha256(pathlib.Path(sys.argv[4]).read_bytes()).hexdigest(),"files":files}
(root/"manifest.json").write_text(json.dumps(manifest,sort_keys=True,indent=2)+"\n")
print(f"browser artifact sha256:{identity}")
PY
# GNU mv's no-clobber rename preserves a concurrently published output as well.
mv --no-clobber --no-target-directory "$STAGING" "$OUTPUT"
[[ ! -d "$STAGING" ]] || { echo 'output appeared during build; refusing to overwrite it' >&2; exit 1; }
printf '%s\n' "$OUTPUT"
