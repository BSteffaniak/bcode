#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python3 - <<'PY'
from pathlib import Path
import re

matrix_path = Path("scripts/hyperchad-semantic-parity-matrix.tsv")
rows = []
for line_number, raw_line in enumerate(matrix_path.read_text().splitlines(), 1):
    if not raw_line or raw_line.startswith("#"):
        continue
    fields = raw_line.split("\t")
    if len(fields) != 7:
        raise SystemExit(f"{matrix_path}:{line_number}: expected seven tab-separated fields")
    row = dict(
        domain=fields[0],
        semantic=fields[1],
        source=fields[2],
        component=fields[3],
        behavior=fields[4],
        automated=fields[5],
        manual=fields[6],
    )
    if row["manual"] not in {"pending", "passed", "not_applicable"}:
        raise SystemExit(f"{matrix_path}:{line_number}: invalid manual status {row['manual']}")
    for field in ("source", "component", "behavior", "automated"):
        if not row[field].strip():
            raise SystemExit(f"{matrix_path}:{line_number}: empty {field}")
    rows.append(row)

required_domains = {
    "transcript",
    "live",
    "plugin_artifact",
    "permission",
    "interaction",
    "runtime",
    "session",
    "composer",
    "application",
}
domains = {row["domain"] for row in rows}
if not required_domains <= domains:
    raise SystemExit(f"parity matrix is missing domains: {sorted(required_domains - domains)}")

models = Path("packages/session-view/models/src/lib.rs").read_text()
match = re.search(
    r"pub enum TranscriptViewItemKind \{(?P<body>.*?)\n\}",
    models,
    re.DOTALL,
)
if not match:
    raise SystemExit("could not locate TranscriptViewItemKind")
variants = {
    re.match(r"\s*([A-Z][A-Za-z0-9]+)", line).group(1)
    for line in match.group("body").splitlines()
    if re.match(r"\s*[A-Z][A-Za-z0-9]+\s*\{", line)
}
expected_semantics = {
    re.sub(r"(?<!^)(?=[A-Z])", "_", variant).lower() for variant in variants
}
actual_semantics = {
    row["semantic"] for row in rows if row["domain"] == "transcript"
}
if actual_semantics != expected_semantics:
    raise SystemExit(
        "transcript parity matrix mismatch: "
        f"missing={sorted(expected_semantics - actual_semantics)} "
        f"extra={sorted(actual_semantics - expected_semantics)}"
    )

coverage_text = "\n".join(
    path.read_text()
    for path in [
        Path("packages/hyperchad/ui/src/pages/home/tests.rs"),
        Path("packages/hyperchad/src/lib.rs"),
        Path("packages/server/src/lib.rs"),
        Path("packages/session-view/src/actions.rs"),
    ]
)

ui_tests = Path("packages/hyperchad/ui/src/pages/home/tests.rs").read_text()
required_component_coverage = {
    "every_non_tool_transcript_state_survives_reconnect_snapshot_and_renders",
    "every_registered_visual_adapter_has_a_fixture",
    "every_registered_artifact_adapter_has_a_fixture",
    "visual_adapters_are_schema_version_specific_and_keep_fallbacks",
    "tool_lifecycle_card_covers_request_running_success_failure_and_timeout",
    "unknown_interactions_keep_bounded_active_controls_and_resolved_history",
    "representative_complete_session_survives_reconnect_and_renders_every_domain",
}
missing_component_coverage = sorted(
    test for test in required_component_coverage if f"fn {test}(" not in ui_tests
)
if missing_component_coverage:
    raise SystemExit(
        f"semantic component coverage is incomplete: {missing_component_coverage}"
    )

known_scripts = {path.name for path in Path("scripts").glob("check-*.sh")}
for row in rows:
    for reference in (value.strip() for value in row["automated"].split(";")):
        if reference.endswith(".sh"):
            if reference not in known_scripts:
                raise SystemExit(f"matrix references missing script: {reference}")
        elif reference not in coverage_text:
            raise SystemExit(f"matrix references missing automated coverage: {reference}")

if not any(row["manual"] == "pending" for row in rows):
    raise SystemExit("matrix must retain pending manual status until manual acceptance is recorded")

print(
    "HyperChad semantic parity matrix guard passed "
    f"({len(rows)} rows, {len(variants)} transcript variants, manual acceptance pending)"
)
PY

scripts/check-plugin-presentation-manifests.sh >/dev/null
