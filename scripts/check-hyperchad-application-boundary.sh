#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python3 - <<'PY'
from pathlib import Path
import re

root = Path("packages/hyperchad")
ui = root / "ui"
host = root / "src" / "lib.rs"

runtime_suffixes = {".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".html"}
runtime_files = sorted(
    path for path in root.rglob("*") if path.is_file() and path.suffix.lower() in runtime_suffixes
)
if runtime_files:
    raise SystemExit(
        "Bcode-owned HyperChad paths contain handwritten renderer runtime/assets:\n"
        + "\n".join(f"* {path}" for path in runtime_files)
    )

forbidden_portable = {
    "actix_web": "Actix import",
    "hyperchad_renderer_html": "HTML renderer import",
    "hyperchad_renderer_vanilla_js": "vanilla-JS renderer import",
    "EventSource": "direct SSE/EventSource API",
    "XMLHttpRequest": "direct browser request API",
    "document.querySelector": "direct DOM query",
    "window.": "direct browser window API",
    "fetch(": "direct browser fetch API",
    "<script": "inline script",
}
for path in sorted(ui.rglob("*.rs")):
    if path.name == "tests.rs":
        continue
    text = path.read_text()
    for token, description in forbidden_portable.items():
        if token in text:
            raise SystemExit(f"{path}: portable HyperChad UI contains {description}: {token}")

forms = []
for path in sorted(ui.rglob("*.rs")):
    for line in path.read_text().splitlines():
        declaration = line.strip()
        if re.search(r"\bform\b", declaration) and "hx-post=" in declaration:
            forms.append((path, declaration))
            for required in ("hx-target=", "hx-swap="):
                if required not in declaration:
                    raise SystemExit(
                        f"{path}: canonical action form is missing {required}: {declaration}"
                    )

if not forms:
    raise SystemExit("no canonical HyperChad action forms found")

ui_text = "\n".join(path.read_text() for path in sorted(ui.rglob("*.rs")) if path.name != "tests.rs")
host_text = host.read_text()
if re.search(r"\bimage\b[^\n]*\bsrc=", ui_text):
    raise SystemExit("portable UI exposes an image resource without a guarded semantic asset contract")
if re.search(r"anchor\s+href=\([^\n]*(path|storage_uri)", ui_text):
    raise SystemExit("portable UI exposes a local artifact path/URI as an unguarded link")

ui_actions = set(re.findall(r"/actions/[a-z-]+", ui_text))
host_actions = set(re.findall(r'\"(/actions/[a-z-]+)', host_text))
expected_actions = {
    "/actions/submit-message",
    "/actions/cancel-turn",
    "/actions/update-draft",
    "/actions/permission",
    "/actions/permission-batch",
    "/actions/history-window",
    "/actions/interaction",
}
if ui_actions != expected_actions:
    raise SystemExit(
        "portable UI canonical action inventory mismatch: "
        f"expected={sorted(expected_actions)} actual={sorted(ui_actions)}"
    )
if host_actions != expected_actions:
    raise SystemExit(
        "host canonical action route inventory mismatch: "
        f"expected={sorted(expected_actions)} actual={sorted(host_actions)}"
    )
if "/actions/update-draft/" not in ui_text or 'RoutePath::LiteralPrefix("/actions/update-draft/"' not in host_text:
    raise SystemExit("draft updates do not use the canonical HyperChad dynamic route prefix")
if "/session/" not in ui_text or 'RoutePath::LiteralPrefix("/session/"' not in host_text:
    raise SystemExit("session navigation does not cross the canonical HyperChad router boundary")

required_host_tokens = {
    ".with_route(": "canonical HyperChad routes",
    "RouteRequest": "canonical route requests",
    "execute_session_view_action": "renderer-neutral semantic actions",
    ".render_scoped(": "canonical scoped renderer publication",
}
for token, description in required_host_tokens.items():
    if token not in host_text:
        raise SystemExit(f"{host}: missing {description}: {token}")

html_actix = root / "src" / "html_actix.rs"
backend_text = html_actix.read_text()
for token in (
    ".with_actix_bind_address(",
    "hyperchad::renderer::assets::StaticAssetRoute",
    "hyperchad::renderer_vanilla_js::SCRIPT",
):
    if token not in backend_text:
        raise SystemExit(f"{html_actix}: missing selected-backend HyperChad API {token}")

print(
    "HyperChad application boundary guard passed "
    f"({len(forms)} canonical action forms, no Bcode-owned browser runtime)"
)
PY
