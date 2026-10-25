#!/usr/bin/env bash
# Build immutable browser-local assets with the resolved BMUX bridge and exact ABI.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"
if [[ -z ${BCODE_TUI_WEB_IN_NIX:-} ]]; then
  exec nix develop --command env BCODE_TUI_WEB_IN_NIX=1 bash "$0" "$@"
fi
if [[ $# -ne 1 ]]; then
  echo 'usage: bash scripts/build-tui-web.sh NEW_OUTPUT_DIRECTORY' >&2
  exit 2
fi
OUTPUT=$(realpath -m "$1")
if [[ -e "$OUTPUT" ]]; then
  echo 'output directory must not exist (immutable artifacts)' >&2
  exit 2
fi
# Metadata resolves the selected patch/dependency, not an unrelated worktree.
METADATA=$(cargo metadata --locked --format-version 1)
BMUX_MANIFEST=$(printf '%s' "$METADATA" | python3 -c 'import json,sys; p=[p for p in json.load(sys.stdin)["packages"] if p["name"]=="bmux_web_renderer"]; assert len(p)==1,"ambiguous BMUX renderer"; print(p[0]["manifest_path"])')
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
cargo build --locked -p bcode_tui_web --target wasm32-unknown-unknown --features wasm --release
# Build once into a new directory; never overwrite a published artifact.
mkdir -p "$OUTPUT"
"$BINDGEN" "$TARGET/wasm32-unknown-unknown/release/bcode_tui_web.wasm" --target web --out-dir "$OUTPUT" --out-name bcode_tui_web --no-typescript
cp packages/tui-web/browser/index.html packages/tui-web/browser/bootstrap.js "$OUTPUT/"
cp packages/tui-web/src/browser-session.js "$OUTPUT/"
cp "$BRIDGE" "$OUTPUT/interactive-surface.js"
python3 - "$OUTPUT" "$VERSION" "$BMUX_MANIFEST" "$BINDGEN" <<'PY'
import hashlib,json,pathlib,sys
root=pathlib.Path(sys.argv[1])
files={p.name:{"sha256":hashlib.sha256(p.read_bytes()).hexdigest(),"bytes":p.stat().st_size} for p in sorted(root.iterdir()) if p.is_file()}
required={"index.html","bootstrap.js","browser-session.js","interactive-surface.js","bcode_tui_web.js","bcode_tui_web_bg.wasm"}
assert files.keys()==required, "unexpected artifact files"
identity=hashlib.sha256(json.dumps(files,sort_keys=True,separators=(",",":")).encode()).hexdigest()
manifest={"schema_version":1,"artifact_sha256":identity,"wasm_bindgen_version":sys.argv[2],"bmux_manifest":sys.argv[3],"wasm_bindgen_cli_sha256":hashlib.sha256(pathlib.Path(sys.argv[4]).read_bytes()).hexdigest(),"files":files,"capabilities":["shared_bmux_editor","shared_transcript_viewport","shared_native_markdown_projection","canonical_markdown_selection_copy"],"gaps":["plugin_surfaces","tool_permission_interaction_controls","artifact_images","history_paging","native_theme_and_keymap_transport","action_activation"]}
(root/"manifest.json").write_text(json.dumps(manifest,sort_keys=True,indent=2)+"\n")
print(f"browser artifact sha256:{identity}\n{root}")
PY
