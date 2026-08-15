#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python3 - <<'PY'
from pathlib import Path

shared = Path("packages/session-view/src/lib.rs").read_text()
tui = "\n".join(
    path.read_text()
    for path in Path("packages/tui").rglob("*.rs")
    if path.name not in ("streaming_configurator.rs", "streaming_configurator_render.rs")
)
streaming_configurator = Path("packages/tui/src/streaming_configurator.rs").read_text()
streaming_configurator_render = Path("packages/tui/src/streaming_configurator_render.rs").read_text()
hyperchad = Path("packages/hyperchad/src/lib.rs").read_text()
config = Path("packages/config/src/lib.rs").read_text()
renderer_docs = Path("docs/renderer-architecture.md").read_text()
tui_docs = Path("docs/tui-rendering.md").read_text()

for required in (
    "StreamingPresentationPolicy",
    "next_streaming_presentation_deadline",
    "advance_streaming_presentation",
    "unicode_segmentation::UnicodeSegmentation",
    "PendingTextPresentation",
):
    if required not in shared:
        raise SystemExit(f"shared stream presentation contract missing: {required}")

for forbidden in ("StreamingInterpolationCurve::Linear", "grapheme_indices(true)"):
    if forbidden in tui or forbidden in hyperchad:
        raise SystemExit(f"renderer owns shared interpolation behavior: {forbidden}")

for forbidden in ("grapheme_indices(true)", ".graphemes(true)", "fn effective_rate"):
    if forbidden in streaming_configurator or forbidden in streaming_configurator_render:
        raise SystemExit(f"streaming configurator duplicates shared presentation behavior: {forbidden}")

for required in ("SessionView", "StreamingPresentationPolicy::immediate()"):
    if required not in streaming_configurator:
        raise SystemExit(f"streaming configurator does not reuse shared projection: {required}")

for required in (
    "next_streaming_presentation_deadline",
    "advance_streaming_presentation",
):
    if required not in tui:
        raise SystemExit(f"TUI stream deadline adaptation missing: {required}")

for forbidden in ("bcode.toml", "config_to_toml", "write_tui_toml"):
    if forbidden in streaming_configurator or forbidden in streaming_configurator_render:
        raise SystemExit(f"streaming configurator writes declarative configuration: {forbidden}")

for required in (
    "Streaming configurator",
    "bcode.streaming_configurator",
    "interactive `tui.toml` user state",
):
    if required not in tui_docs:
        raise SystemExit(f"TUI configurator documentation missing: {required}")

for required in ("exact same typed source events", "bounded, ephemeral"):
    if required not in renderer_docs:
        raise SystemExit(f"renderer configurator documentation missing: {required}")

for required in (
    "LatestSnapshotUpdates",
    "next_streaming_presentation_deadline",
    "advance_streaming_presentation",
    "with_streaming_presentation_policy",
):
    if required not in hyperchad:
        raise SystemExit(f"HyperChad stream adaptation missing: {required}")

if "mpsc::channel::<ScopedSnapshotUpdate>" in hyperchad:
    raise SystemExit("HyperChad canonical watch processing can block on animation channel capacity")

for required in (
    "PresentationStreamingConfig",
    "graphemes_per_second",
    "max_lag_ms",
    "streaming.policy()",
):
    if required not in config:
        raise SystemExit(f"shared stream configuration missing: {required}")

for required in ("accepted live assistant", "max_lag_ms", "per opaque render scope"):
    if required not in renderer_docs:
        raise SystemExit(f"renderer architecture documentation missing: {required}")

for required in ("Smooth stream scheduling", "graphemes_per_second = 300", "bounded to 1000 ms"):
    if required not in tui_docs:
        raise SystemExit(f"TUI scheduling documentation missing: {required}")

print("renderer-neutral stream presentation architecture guard passed")
PY
