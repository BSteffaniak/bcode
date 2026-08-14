#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python3 - <<'PY'
from pathlib import Path

shared = Path("packages/session-view/src/lib.rs").read_text()
tui = "\n".join(path.read_text() for path in Path("packages/tui").rglob("*.rs"))
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

for required in (
    '"bcode.streaming_presentation"',
    "next_streaming_presentation_deadline",
    "advance_streaming_presentation",
):
    if required not in tui:
        raise SystemExit(f"TUI stream deadline adaptation missing: {required}")

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
