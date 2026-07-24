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
session_view_roots = [Path("packages/session-view"), Path("packages/session-view/models")]

forbidden_shared_presentation = {
    "ratatui": "terminal renderer dependency",
    "crossterm": "terminal transport dependency",
    "hyperchad": "HyperChad renderer dependency",
    "raw_html": "preformatted HTML field",
    "html_fragment": "preformatted HTML fragment",
    "terminal_frame": "terminal frame field",
    "ansi_style": "terminal styling field",
}
for shared_root in session_view_roots:
    for path in sorted(shared_root.rglob("*.rs")):
        text = path.read_text().lower()
        for token, description in forbidden_shared_presentation.items():
            if token in text:
                raise SystemExit(
                    f"{path}: renderer-neutral session view contains {description}: {token}"
                )

def contrast_ratio(foreground: str, background: str) -> float:
    def luminance(value: str) -> float:
        channels = [int(value[index:index + 2], 16) / 255 for index in (1, 3, 5)]
        linear = [
            channel / 12.92
            if channel <= 0.04045
            else ((channel + 0.055) / 1.055) ** 2.4
            for channel in channels
        ]
        return 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2]

    lighter, darker = sorted(
        (luminance(foreground), luminance(background)), reverse=True
    )
    return (lighter + 0.05) / (darker + 0.05)


theme_text = (ui / "src" / "pages" / "home" / "theme.rs").read_text()
color_values = dict(re.findall(r"pub const (\w+): &str = \"(#[0-9a-fA-F]{6})\";", theme_text))
for foreground in (
    "TEXT", "STRONG", "MUTED", "INFO", "SUCCESS", "WARNING", "ERROR",
    "REASONING", "REMOVED_TEXT", "ADDED_TEXT",
):
    for background in ("APP", "PANEL", "INSET"):
        ratio = contrast_ratio(color_values[foreground], color_values[background])
        if ratio < 4.5:
            raise SystemExit(
                f"portable theme contrast is below 4.5:1: {foreground}/{background}={ratio:.2f}"
            )

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
    if path.name == "tests.rs" or "context/tests.rs" in path.as_posix():
        continue
    text = path.read_text()
    for token, description in forbidden_portable.items():
        if token in text:
            raise SystemExit(f"{path}: portable HyperChad UI contains {description}: {token}")

forbidden_presentation_context = {
    "access_token": "browser access token",
    "backend_url": "backend URL",
    "bind_address": "bind configuration",
    "hyperchad-event-scope": "transport event scope",
    "token=": "browser capability query",
    "when this backend": "backend-dependent user copy",
    "rendered browser": "browser-dependent user copy",
}
for path in sorted(ui.rglob("*.rs")):
    if path.name == "tests.rs" or "context/tests.rs" in path.as_posix():
        continue
    text = path.read_text()
    for token, description in forbidden_presentation_context.items():
        if token in text:
            raise SystemExit(f"{path}: portable presentation accepts {description}: {token}")

home = ui / "src" / "pages" / "home"
theme = home / "theme.rs"
theme_text = theme.read_text()
required_theme_tokens = {
    "pub(super) mod surface": "surface and border tokens",
    "pub(super) mod color": "color and status tokens",
    "pub(super) mod space": "spacing tokens",
    "pub(super) mod radius": "border-radius tokens",
    "pub(super) mod width": "readable and responsive width tokens",
    "pub(super) mod typeface": "typography tokens",
}
for token, description in required_theme_tokens.items():
    if token not in theme_text:
        raise SystemExit(f"{theme}: missing centralized {description}: {token}")

inline_style_patterns = {
    r'(?<![A-Za-z_-])(?:color|background)="#[0-9A-Fa-f]{6}"': "inline color token",
    r'border(?:-top|-right|-bottom|-left)?="[0-9]+, #[0-9A-Fa-f]{6}"': "inline border token",
    r'font-size=[0-9]+': "inline typography size",
    r'border-radius=[0-9]+': "inline border radius",
    r'(?<![A-Za-z-])(?:gap|padding|margin-top|margin-bottom)=[0-9]+': "inline spacing value",
}
for path in sorted(home.glob("*.rs")):
    if path.name in {"tests.rs", "theme.rs"}:
        continue
    text = path.read_text()
    for pattern, description in inline_style_patterns.items():
        if match := re.search(pattern, text):
            raise SystemExit(
                f"{path}: presentation bypasses centralized theme ({description}): "
                f"{match.group(0)}"
            )

forms = []
visible_control_pattern = re.compile(r"\b(input|textarea|select)\b")
for path in sorted(ui.rglob("*.rs")):
    if path.name == "tests.rs":
        continue
    for line in path.read_text().splitlines():
        declaration = line.strip()
        if visible_control_pattern.search(declaration) and "type=hidden" not in declaration:
            if "data-label-id=" not in declaration and "placeholder=" in declaration:
                raise SystemExit(
                    f"{path}: visible control lacks an explicit semantic label relationship: {declaration}"
                )
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
    required_image_contract = {
        "artifact_target(": "presentation-context resource routing",
        "supported_inline_image_content_type(": "central safe image media-type validation",
        "guarded_source": "backend-provided guarded image source",
    }
    for token, description in required_image_contract.items():
        if token not in ui_text:
            raise SystemExit(
                f"portable UI image resource is missing {description}: {token}"
            )
    if re.search(r"storage_uri[^\n]*\bimage\b[^\n]*\bsrc=", ui_text):
        raise SystemExit("portable UI uses plugin storage URI directly as an image resource")
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
backend_text = html_actix.read_text() + (root / "src" / "html_actix" / "context.rs").read_text()
for token in (
    ".with_actix_bind_address(",
    "hyperchad::renderer::assets::StaticAssetRoute",
    "hyperchad::renderer_vanilla_js::SCRIPT",
    "pub fn build_launch_url(",
    "HtmlActixPresentationContext",
    "ACCESSIBILITY_CSS",
    ":focus-visible",
    "min-height: 44px",
    "render_scope(",
    "hyperchad-event-scope=",
):
    if token not in backend_text:
        raise SystemExit(f"{html_actix}: missing selected-backend HyperChad API {token}")

if "impl std::fmt::Debug for HtmlActixPresentationContext" not in backend_text or "[REDACTED]" not in backend_text:
    raise SystemExit(f"{html_actix}: browser capability context must redact its Debug output")

cli = Path("packages/cli/src/lib.rs")
cli_text = cli.read_text()
for forbidden in (
    "hyperchad-event-scope=",
    "format!(\"token={access_token}\")",
    "Bcode HyperChad web renderer:",
    "println!(launch_url",
    "eprintln!(launch_url",
    "bcode_hyperchad::VIEWPORT",
):
    if forbidden in cli_text:
        raise SystemExit(
            f"{cli}: HTML/Actix launch construction leaked outside selected backend: {forbidden}"
        )
if "bcode_hyperchad::build_launch_url(" not in cli_text:
    raise SystemExit(f"{cli}: selected backend launch URL helper is not used")

print(
    "HyperChad application boundary guard passed "
    f"({len(forms)} canonical action forms, no Bcode-owned browser runtime)"
)
PY
