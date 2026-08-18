#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python3 - <<'PY'
from pathlib import Path

shared_models = Path("packages/session-view/models/src/lib.rs").read_text()
shared_view = Path("packages/session-view/src/lib.rs").read_text()
tui_transcript = Path("packages/tui/src/transcript.rs").read_text()
web_transcript = Path("packages/hyperchad/ui/src/pages/home/transcript.rs").read_text()
bedrock = Path("plugins/bedrock-provider-plugin/src/lib.rs").read_text()
# Parity must hold in production code, not merely somewhere in the file. Test modules
# mention every event name, which would otherwise mask a removed emission.
bedrock_production = bedrock.split("\nmod tests {")[0]
reasoning_docs = Path("docs/reasoning-presentation.md").read_text()
provider_docs = Path("docs/model-provider-contract.md").read_text()

# Shared session-view models own the classification renderers consume. A renderer that
# re-derives "is there anything readable here?" from raw parts will drift from its peers.
for required in (
    "enum ReasoningContentAvailability",
    "Readable",
    "Filtered",
    "Withheld",
    "Pending",
    "fn content_availability",
    "readable_parts_filtered",
):
    if required not in shared_models:
        raise SystemExit(
            f"shared reasoning availability contract missing: {required}"
        )

# The new field is derived presentation state layered onto a persisted-adjacent view model,
# so it must decode from snapshots written before it existed.
if "#[serde(default)]\n    pub readable_parts_filtered" not in shared_models:
    raise SystemExit(
        "readable_parts_filtered must carry #[serde(default)] for backward-compatible decoding"
    )

# Both projection paths (live activity and replayed durable activity) must classify from
# the computed value, or replay and streaming would disagree about why text is absent.
# Counting the shorthand field init specifically avoids passing on a hardcoded `false`.
if shared_view.count("\n        readable_parts_filtered,\n") < 2:
    raise SystemExit(
        "both live and replayed reasoning projections must populate readable_parts_filtered "
        "from the computed value"
    )
if "had_readable_parts" not in shared_view:
    raise SystemExit(
        "reasoning projection must compare pre-filter and post-filter readable parts "
        "to distinguish local filtering from provider withholding"
    )

# Renderers adapt the shared classification; they must not reimplement the decision.
# The call site is checked separately from the helper definition: a renderer that keeps a
# dead helper while rendering raw text again would otherwise slip through.
for renderer_path, renderer, adapter, call_site in (
    (
        "packages/tui/src/transcript.rs",
        tui_transcript,
        "reasoning_activity_body",
        "reasoning_activity_body(activity)",
    ),
    (
        "packages/hyperchad/ui/src/pages/home/transcript.rs",
        web_transcript,
        "reasoning_activity_note",
        "reasoning_activity_note(activity)",
    ),
):
    if adapter not in renderer:
        raise SystemExit(f"{renderer_path} missing reasoning adaptation: {adapter}")
    if call_site not in renderer:
        raise SystemExit(
            f"{renderer_path} must route reasoning activity through {adapter} "
            "instead of rendering readable text directly"
        )
    if "content_availability()" not in renderer:
        raise SystemExit(
            f"{renderer_path} must adapt the shared availability classification "
            "rather than reinterpreting reasoning parts"
        )

# Availability is a shared product semantic, not a terminal concern.
for forbidden in ("enum ReasoningContentAvailability", "fn content_availability"):
    for renderer_path, renderer in (
        ("packages/tui/src/transcript.rs", tui_transcript),
        ("packages/hyperchad/ui/src/pages/home/transcript.rs", web_transcript),
    ):
        if forbidden in renderer:
            raise SystemExit(
                f"{renderer_path} must not own shared reasoning availability semantics: {forbidden}"
            )

# Provider-neutral reasoning parity. The Bedrock Messages surface originally emitted
# Started -> Finished with no parts for redacted thinking, which rendered as an
# unexplained empty item. Both surfaces must record opaque evidence and complete a part.
if bedrock_production.count("ReasoningActivityEvent::OpaqueObserved") < 2:
    raise SystemExit(
        "both Bedrock reasoning surfaces must record opaque evidence via OpaqueObserved"
    )
if bedrock_production.count("ReasoningActivityEvent::PartCompleted") < 2:
    raise SystemExit(
        "both Bedrock reasoning surfaces must emit an authoritative terminal PartCompleted"
    )

# Opaque evidence records only that non-readable state existed. Provider bytes, lengths,
# hashes, and prefixes must never ride along.
for opaque_start in (
    index
    for index in range(len(bedrock_production))
    if bedrock_production.startswith("ReasoningActivityEvent::OpaqueObserved {", index)
):
    payload_end = bedrock_production.find("}", opaque_start)
    payload = bedrock_production[opaque_start:payload_end]
    for leaked in ("data", "signature", "len()", "hash", "bytes", "text"):
        if leaked in payload:
            raise SystemExit(
                f"OpaqueObserved must not carry provider payload detail: {leaked}"
            )

# Presentation must not feed back into request construction.
for forbidden in ("readable_parts_filtered", "content_availability"):
    if forbidden in bedrock:
        raise SystemExit(
            f"provider request construction must not depend on presentation state: {forbidden}"
        )

for required in (
    "withheld readable reasoning",
    "renderer-neutral",
):
    if required not in reasoning_docs:
        raise SystemExit(
            f"reasoning presentation documentation missing: {required}"
        )

for required in ("OpaqueObserved", "must not duplicate equal text"):
    if required not in provider_docs:
        raise SystemExit(f"provider reasoning contract documentation missing: {required}")

print("reasoning presentation architecture guard passed")
PY
