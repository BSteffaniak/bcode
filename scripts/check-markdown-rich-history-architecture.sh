#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

violations=0

# Network image transport must remain confined to the TUI image adapter. Session,
# server, catalog, and history orchestration may carry typed Markdown only.
image_transport_users="$(
  rg -l 'MarkdownImageLoader|reqwest::|\.get\([^)]*image|load_remote\(' \
    packages/session packages/server packages/client \
    packages/tui/src/history_flow.rs packages/tui/src/session_flow.rs \
    --glob '*.rs' || true
)"
if [[ -n "$image_transport_users" ]]; then
  echo "Markdown history architecture violation: history/catalog/open/attach code can start image transport" >&2
  printf '%s\n' "$image_transport_users" >&2
  violations=1
fi

# Worker execution belongs only to the private Mermaid package. Markdown parsing
# and all normal history paths must not spawn or invoke it.
worker_users="$(
  rg -l 'render_mermaid_with_worker|bcode-mermaid-worker|Command::new\([^)]*mermaid' \
    packages/session packages/server packages/client packages/tui packages/markdown-render \
    --glob '*.rs' || true
)"
if [[ -n "$worker_users" ]]; then
  echo "Markdown history architecture violation: non-worker package can start Mermaid workers" >&2
  printf '%s\n' "$worker_users" >&2
  violations=1
fi

# Rich orchestration must remain in the bounded presentation adapters rather than
# catalog/open/attach/history/reconstruction flows.
forbidden_orchestration="$(
  rg -n 'schedule_loads|MarkdownMermaidPresentationStore|MarkdownImagePresentationStore' \
    packages/tui/src/history_flow.rs packages/tui/src/session_flow.rs packages/tui/src/runtime.rs \
    packages/session packages/server packages/client \
    --glob '*.rs' || true
)"
if [[ -n "$forbidden_orchestration" ]]; then
  echo "Markdown history architecture violation: rich work escaped the resident presentation boundary" >&2
  printf '%s\n' "$forbidden_orchestration" >&2
  violations=1
fi

# Keep the explicit non-interactive image boundary and bounded prefetch controls.
if ! rg -q 'interactive_resident_frame' packages/tui/src/markdown_image.rs \
  || ! rg -q 'MAX_PREFETCH_CONTRIBUTIONS' packages/tui/src/markdown_image.rs \
  || ! rg -q 'MAX_MERMAID_PREFETCH' packages/tui/src/markdown_mermaid.rs; then
  echo "Markdown history architecture violation: resident/non-eager safeguards were removed" >&2
  violations=1
fi

if [[ "$violations" -ne 0 ]]; then
  exit 1
fi

echo "Markdown rich-history architecture guard passed"
