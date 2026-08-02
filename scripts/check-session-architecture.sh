#!/usr/bin/env bash
set -euo pipefail

violations=0

if rg -n 'leases: BTreeMap<SessionId, SessionLeaseGuard>|retains its compatibility lease while dropping idle database' \
  packages/session/src docs/session-persistence-architecture.md --glob '*.rs' --glob '*.md' \
  >/tmp/bcode-session-indefinite-owner.txt; then
  echo "Session ownership violation: manager-owned or indefinite summary-only runtime ownership was reintroduced." >&2
  cat /tmp/bcode-session-indefinite-owner.txt >&2
  violations=1
fi

if ! rg -q 'SessionOwnershipReleaseOutcome' packages/ipc/src/lib.rs \
  || ! rg -q 'ReleaseSessionOwnership' packages/ipc/src/lib.rs \
  || ! rg -q 'release_session_ownership\(' packages/client/src/lib.rs \
  || ! rg -q 'release_ownership_if_quiescent' packages/session/src/actor.rs; then
  echo "Session ownership violation: explicit typed release must route through the actor quiescence primitive." >&2
  violations=1
fi

if ! rg -q 'DaemonRecordClassification' packages/daemon-lifecycle/src/lib.rs \
  || ! rg -q 'classify_daemon_record\(' packages/daemon-lifecycle/src/lib.rs packages/cli/src/lib.rs \
  || ! rg -q 'UnreachableStale' packages/daemon-lifecycle/src/lib.rs packages/cli/src/lib.rs \
  || ! rg -q 'HistoricalProcessVerifiedProtocolUnsupported' packages/daemon-lifecycle/src/lib.rs packages/cli/src/lib.rs \
  || ! grep -F 'daemon_control_policy_never_spawns_protocol_unsupported_or_ambiguous_records' packages/cli/src/lib.rs >/dev/null \
  || ! grep -F 'current_process_identity_evidence_rejects_pid_reuse_and_accepts_exact_record' packages/daemon-lifecycle/src/lib.rs >/dev/null \
  || ! grep -F '## Ownership activity matrix' docs/session-persistence-architecture.md >/dev/null \
  || ! grep -F '## Historical daemon record classification' docs/session-persistence-architecture.md >/dev/null; then
  echo "Session historical-daemon violation: conservative classification, control policy, fixtures, or architecture documentation was removed." >&2
  violations=1
fi

if ! rg -q 'schema_version: 3' packages/session/src/lease.rs \
  || ! rg -q 'SessionOwnerLiveness' packages/session/src/lease.rs \
  || ! rg -q 'file.try_lock\(\)' packages/session/src/lease.rs \
  || ! rg -q 'file.sync_all\(\)' packages/session/src/lease.rs; then
  echo "Session ownership violation: schema-v3 lock-backed publication and shared classification must remain present." >&2
  violations=1
fi

if rg -n 'publish_transient_event|PublishTransient' packages/session/src --glob '*.rs' >/tmp/bcode-session-transient-durable-type.txt; then
  echo "Session architecture violation: live-only publication must use SessionLiveEvent, not replayable SessionEvent types." >&2
  cat /tmp/bcode-session-transient-durable-type.txt >&2
  violations=1
fi

if ! grep -F 'transient_payload_markers_never_enter_durable_or_observability_sinks' packages/server/src/lib.rs >/dev/null \
  || ! grep -F 'active-only presentation update cannot be persisted' packages/session/src/lib.rs >/dev/null; then
  echo "Session architecture violation: active-only presentation persistence guard or sink coverage was removed." >&2
  violations=1
fi

if rg -n 'SessionEventKind::ToolRequestDraft|ToolRequestDraftEvent' packages/session/src --glob '*.rs' \
  >/tmp/bcode-session-durable-request-draft.txt; then
  echo "Session architecture violation: request drafts must remain SessionLiveEvent-only and absent from durable session implementation." >&2
  cat /tmp/bcode-session-durable-request-draft.txt >&2
  violations=1
fi

if ! grep -F 'ToolContributionPersistence::Transient' packages/session/src/lib.rs >/dev/null \
  || ! grep -F 'ToolContributionPlacement::Progress' packages/session/src/lib.rs >/dev/null \
  || ! grep -F 'transient_contribution_is_rejected_before_durable_append' packages/session/src/lib.rs >/dev/null; then
  echo "Session architecture violation: transient/progress contributions must remain rejected before current durable append." >&2
  violations=1
fi

if ! python3 - <<'PY'
import re
from pathlib import Path
source = Path('packages/session/models/src/lib.rs').read_text()
documentation = Path('docs/session-view-event-coverage.md').read_text()
for enum_name, heading, next_heading in (
    ('SessionEventKind', '## Durable `SessionEventKind` coverage', '## Live `SessionLiveEventKind` coverage'),
    ('SessionLiveEventKind', '## Live `SessionLiveEventKind` coverage', '## Migration order derived from the matrix'),
):
    start = source.index(f'pub enum {enum_name}')
    end = source.index('\n}', start)
    variants = re.findall(r'^    ([A-Z][A-Za-z0-9_]*)', source[start:end], re.MULTILINE)
    section = documentation.split(heading, 1)[1].split(next_heading, 1)[0]
    missing = [variant for variant in variants if not re.search(rf'^\| `{variant}` \|', section, re.MULTILINE)]
    if missing:
        raise SystemExit(f'{enum_name} variants missing explicit coverage rows: {missing}')
    for line in section.splitlines():
        if not line.startswith('| `'):
            continue
        cells = [cell.strip() for cell in line.strip('|').split('|')]
        if len(cells) != 5:
            raise SystemExit(f'{enum_name} coverage row lacks the five required columns: {line}')
        if cells[2] not in {'transcript', 'non-transcript', 'none'}:
            raise SystemExit(f'{enum_name} coverage row has invalid transcript eligibility: {line}')
PY
then
  echo "Session architecture violation: durable/live SessionView event coverage documentation lacks explicit semantic state, transcript eligibility, or frontend presentation classification." >&2
  violations=1
fi

live_cli_description="$(sed -n '/fn session_live_event_description(/,/^}/p' packages/cli/src/lib.rs)"
if grep -Eq 'contribution\.payload|ToolRequestDraftOperation::(Append|Checkpoint).*text|payload=\{' \
  <<<"$live_cli_description"; then
  echo "Session architecture violation: raw draft/progress payload CLI rendering was introduced." >&2
  violations=1
fi

if ! grep -F 'live_progress_descriptions_are_compact_and_omit_opaque_payloads' packages/cli/src/lib.rs >/dev/null \
  || ! grep -F 'oversized_request_draft_is_rejected_without_payload_in_history_traces_or_logs' packages/server/src/lib.rs >/dev/null; then
  echo "Session architecture violation: live payload privacy regression coverage was removed." >&2
  violations=1
fi

current_event_schema="$(sed -n 's/.*CURRENT_SESSION_EVENT_SCHEMA_VERSION: u16 = \([0-9][0-9]*\).*/\1/p' packages/session/models/src/lib.rs)"
fixture_baseline_schema="$(sed -n 's/Current fixture baseline schema: \*\*\([0-9][0-9]*\)\*\*.*/\1/p' packages/session/fixtures/migrations/README.md)"
if [[ -z "$current_event_schema" || "$current_event_schema" != "$fixture_baseline_schema" ]]; then
  echo "Session fixture-baseline violation: CURRENT_SESSION_EVENT_SCHEMA_VERSION ($current_event_schema) must match the documented fixture baseline ($fixture_baseline_schema)." >&2
  violations=1
fi

retired_event_pattern='interactive_tool_request_created|interactive_tool_request_resolved|plugin_automation_turn_started|plugin_automation_turn_finished|tool_invocation_presentation|request_presentation'
retired_runtime_paths=(
  packages/session/src
  packages/session/models/src
  packages/server/src
  packages/ipc/src
  packages/tui/src
  packages/hyperchad/src
  packages/hyperchad/ui/src
)
if [[ -d packages/web-render/src ]]; then
  retired_runtime_paths+=(packages/web-render/src)
fi
if rg -n "$retired_event_pattern" "${retired_runtime_paths[@]}" --glob '*.rs' \
  >/tmp/bcode-retired-session-event-decoders.txt; then
  echo "Session hard-cutover violation: a named retired event decoder or presentation field was reintroduced." >&2
  cat /tmp/bcode-retired-session-event-decoders.txt >&2
  violations=1
fi

for removed_fixture in \
  packages/session/fixtures/migrations/interactive-tool-request-created-v32.json \
  packages/session/fixtures/migrations/interactive-tool-request-resolved-v32.json \
  packages/session/fixtures/migrations/interactive-tool-request-unresolved-v32.json \
  packages/session/fixtures/migrations/mixed-interactive-history-v32-v35.jsonl \
  packages/session/fixtures/migrations/plugin-automation-turn-started-v29.json \
  packages/session/fixtures/migrations/plugin-automation-turn-finished-v29.json \
  packages/session/fixtures/migrations/tool-presentation-diff-v25.json; do
  if [[ -e "$removed_fixture" ]]; then
    echo "Session hard-cutover violation: retired fixture remains: $removed_fixture" >&2
    violations=1
  fi
done

for fixture in \
  packages/session/fixtures/migrations/unknown-future-event-kind-v39.json \
  packages/session/fixtures/migrations/current-schema-v42.json \
  packages/session/fixtures/migrations/future-schema-v43.json \
  packages/session/fixtures/migrations/malformed-json-v39.json \
  packages/session/fixtures/migrations/mismatched-session-id-v39.json \
  packages/session/fixtures/migrations/sequence-gap-v39.jsonl; do
  if [[ ! -f "$fixture" ]]; then
    echo "Session compatibility fixture missing: $fixture" >&2
    violations=1
  fi
done

if find packages/session/fixtures/migrations -maxdepth 1 -type f \
    \( -name '*-v[0-9]*.json' -o -name '*-v[0-9]*.jsonl' \) \
    | grep -Ev -- '-v(39|40|41|42|43)\.jsonl?$' >/tmp/bcode-old-session-fixture-names.txt \
  || ! python3 - <<'PY'
from pathlib import Path
import re

invalid = []
for pattern in ("*.json", "*.jsonl"):
    for path in Path("packages/session/fixtures/migrations").glob(pattern):
        for line_number, line in enumerate(path.read_text().splitlines(), 1):
            if not line.strip():
                continue
            match = re.search(r'"schema_version"\s*:\s*(\d+)', line)
            if match is None or int(match.group(1)) not in {39, 40, 41, 42, 43}:
                invalid.append(f"{path}:{line_number}")
if invalid:
    print("\n".join(invalid))
    raise SystemExit(1)
PY
then
  echo "Session fixture hard-cutover violation: every fixture must use the current schema or the intentional next-schema compatibility case." >&2
  cat /tmp/bcode-old-session-fixture-names.txt 2>/dev/null >&2 || true
  violations=1
fi

if ! sed -n '/fn reject_unsupported_future_shape/,/fn is_unknown_variant_error/p' packages/session/src/persisted.rs \
  | grep -q 'schema_version != CURRENT_SESSION_EVENT_SCHEMA_VERSION' \
  || ! sed -n '/fn decode_for_migration/,/#\[cfg(test)\]/p' packages/session-migration/src/execution.rs \
    | grep -q 'HistoricalEnvelope' \
  || ! rg -q 'serde_json::from_value::<PersistedSessionEvent>' packages/session/src/persisted.rs; then
  echo "Session architecture violation: current persistence must accept only the exact current schema while released structural decoding remains migration-only." >&2
  violations=1
fi

if rg -n 'OpaqueEvent|opaque_event' packages/session/src packages/session/models/src --glob '*.rs' \
  >/tmp/bcode-current-opaque-event.txt \
  || ! rg -q 'InertHistory' packages/session/models/src/lib.rs \
  || ! awk '/kind: "opaque_event"/{found=1} found && /treatment: ReleasedEventTreatment::RetiredKnown/{exit 0} END{exit !found}' \
    packages/session-migration/src/inventory.rs; then
  echo "Session architecture violation: released opaque events must migrate to explicit inert current history and remain absent from current session naming." >&2
  cat /tmp/bcode-current-opaque-event.txt >&2 2>/dev/null || true
  violations=1
fi

if rg -n 'decode_session_event_compatible|CompatibleSessionEvent|decode_opaque_session_event|has_trustworthy_session_event_envelope' \
  packages/session/src --glob '*.rs' >/tmp/bcode-session-opaque-read-policy.txt; then
  echo "Session architecture violation: current session reads must decode strictly without opaque compatibility fallback." >&2
  cat /tmp/bcode-session-opaque-read-policy.txt >&2
  violations=1
fi

if ! rg -q 'session_format_incompatible' packages/server/src/lib.rs \
  || ! rg -q 'persisted_format_errors_are_actionable_and_not_reported_as_corruption' packages/server/src/lib.rs \
  || ! rg -q 'SessionFormatIncompatible' packages/tui/src/daemon_issue.rs \
  || ! rg -q 'session_format_incompatibility_recommends_upgrade_not_repair' packages/tui/src/daemon_issue.rs; then
  echo "Session format-diagnostic violation: unsupported persisted schemas/kinds must request upgrade/restart and must not be reported as corruption." >&2
  violations=1
fi

if ! rg -q 'format_session_compatibility_issue' packages/cli/src/lib.rs \
  || ! sed -n '/async fn paged_session_history(/,/^}/p' packages/cli/src/lib.rs \
    | grep -q 'compatibility_issues' \
  || ! rg -q 'compatibility_issue_format_is_actionable_and_specific' packages/cli/src/lib.rs; then
  echo "Session CLI compatibility violation: history and timeline must render actionable opaque-event diagnostics returned by bounded pages." >&2
  violations=1
fi

if ! rg -q 'pub session_event_schema_version: Option<u16>' packages/ipc/src/lib.rs \
  || ! rg -q 'daemon_identity_matrix_rejects_every_incompatible_capability' packages/client/src/lib.rs \
  || ! sed -n '/fn verify_daemon_identity/,/^    }/p' packages/client/src/lib.rs \
    | grep -q 'session_event_schema_version' \
  || ! sed -n '/fn verify_daemon_identity/,/^    }/p' packages/client/src/lib.rs \
    | grep -q 'storage_writer_epoch'; then
  echo "Daemon capability violation: Hello identity must advertise and reject mismatched event schema and storage writer epoch before requests." >&2
  violations=1
fi

if ! rg -q 'failed_explicit_migration_preserves_projection_and_writer_contract' packages/session/src/db.rs \
  || ! rg -q 'doctor_session_reports_future_and_corrupt_persisted_events_without_mutation' packages/session/src/repair.rs \
  || ! sed -n '/async fn repair_db_files/,/^}/p' packages/session/src/repair.rs \
    | grep -q 'initial error is not a recognized WAL short-read repair case'; then
  echo "Session maintenance-safety violation: failed migration must preserve state, and doctor/repair must not mutate unsupported semantic events." >&2
  violations=1
fi

if ! rg -q 'create_verified_migration_backup' packages/server/src/session_migration_execution.rs \
  || ! rg -q 'build_migration_backup_request' packages/session-migration/src/backup.rs \
  || ! rg -q 'build_migration_backup_request' packages/server/src/session_migration_execution.rs \
  || sed -n '/async fn create_verified_migration_backup(/,/^}/p' packages/server/src/session_migration_execution.rs | grep -q 'plan_writer_epoch_migration' \
  || ! rg -q 'backup_process_crash_boundaries_preserve_source' packages/session-migration/src/backup.rs \
  || ! rg -q 'migration-backup.json' packages/session-migration/src/backup.rs; then
  echo "Session migration-backup violation: automatic legacy migration must create and verify a retained backup before changing storage." >&2
  violations=1
fi

if ! rg -q 'CURRENT_SESSION_STORAGE_WRITER_EPOCH: u32 = 6' packages/session/models/src/lib.rs \
  || ! rg -q 'CURRENT_WRITER_EPOCH: u32 = bcode_session_migration_target::CURRENT_WRITER_EPOCH' packages/session-migration/src/inventory.rs \
  || ! rg -q 'CURRENT_WRITER_EPOCH.*CURRENT_SESSION_STORAGE_WRITER_EPOCH' packages/session-migration-target/src/lib.rs \
  || ! rg -q 'RELEASED_HISTORICAL_ROOTS' packages/session-migration/src/inventory.rs \
  || ! rg -q 'RELEASED_HISTORICAL_EVENT_SCHEMAS' packages/session-migration/src/inventory.rs \
  || ! rg -q 'released_historical_root_inventory_is_sorted_unique_and_exact' packages/session-migration/src/inventory.rs \
  || ! rg -q 'RELEASED_EVENT_VARIANTS' packages/session-migration/src/inventory.rs \
  || ! rg -q 'released_event_schema_ranges_match_all_ref_inventory_boundaries' packages/session-migration/src/inventory.rs \
  || ! rg -q 'released_event_variant_treatments_are_sorted_unique_and_total' packages/session-migration/src/inventory.rs \
  || ! rg -q 'RELEASED_EVENT_VARIANTS.len\(\), 61' packages/session-migration/src/inventory.rs \
  || ! rg -q 'inventoried_retired_families_materialize_as_inert_current_history' packages/session-migration/src/execution.rs \
  || ! rg -q 'every inventoried retired variant must be an explicit reviewed decision' packages/session-migration/src/inventory.rs \
  || ! rg -q 'RELEASED_MIGRATION_IDS' packages/session-migration/src/inventory.rs \
  || ! rg -q 'ReleasedMigrationTreatment' packages/session-migration/src/inventory.rs \
  || ! rg -q 'RetiredSuperseded' packages/session-migration/src/inventory.rs \
  || ! rg -q 'GlobalOnly' packages/session-migration/src/inventory.rs \
  || ! rg -q 'RELEASED_PERSISTED_TABLES' packages/session-migration/src/inventory.rs \
  || ! rg -q 'RELEASED_RECORD_TREATMENTS' packages/session-migration/src/inventory.rs \
  || ! rg -q 'every_released_table_has_exactly_one_record_treatment' packages/session-migration/src/inventory.rs \
  || ! rg -q 'covered_authoritative_records' packages/session-migration/fixtures/manifest.json \
  || ! rg -q 'released_fixture_authoritative_record_coverage' packages/session-migration/src/inventory.rs \
  || ! rg -q 'permanent_fixture_manifest_rejects_duplicate_and_empty_coverage' packages/session-migration/src/inventory.rs \
  || ! rg -q 'UnsupportedEventKind' packages/session-migration/src/inventory.rs \
  || ! rg -q 'EventKindNotReleasedInSchema' packages/session-migration/src/inventory.rs \
  || ! rg -q 'DuplicateExactCoverage' packages/session-migration/src/inventory.rs \
  || ! rg -q 'insert_exact_coverage' packages/session-migration/src/inventory.rs \
  || ! rg -q 'covered_schema_event_pairs' packages/session-migration/src/execution.rs \
  || ! rg -q 'released_fixture_coverage_gaps' packages/session-migration/src/inventory.rs \
  || ! rg -q 'assert!\(gaps.is_empty\(\), "fixture coverage gaps' packages/session-migration/src/inventory.rs \
  || ! rg -q 'writer_schema_pairs' packages/session-migration/src/inventory.rs \
  || ! rg -q 'covered_writer_schema_pairs' packages/session-migration/fixtures/manifest.json \
  || ! rg -q 'writer_schema_event_combinations' packages/session-migration/src/inventory.rs \
  || ! rg -q 'RELEASED_WRITER_SCHEMA_COMBINATIONS' packages/session-migration/src/inventory.rs \
  || ! rg -q 'released_writer_schema_combinations_are_sorted_unique_and_exact' packages/session-migration/src/inventory.rs \
  || ! rg -q 'writer_edges' packages/session-migration/src/inventory.rs \
  || ! rg -q 'covered_roots' packages/session-migration/fixtures/manifest.json \
  || ! rg -q 'covered_root_writer_pairs' packages/session-migration/fixtures/manifest.json \
  || ! rg -q 'covered_tables' packages/session-migration/fixtures/manifest.json \
  || ! rg -q 'covered_table_treatments' packages/session-migration/fixtures/manifest.json \
  || ! rg -q 'migratable fixture must exercise every table treatment' packages/session-migration/src/execution.rs \
  || ! rg -q 'fixture_release_gate_accepts_complete_exact_coverage' packages/session-migration/src/planning.rs \
  || ! rg -q 'migration_ledger_endpoints.is_empty\(\)' packages/session-migration/src/inventory.rs \
  || ! rg -q 'every_released_session_ledger_endpoint_has_one_valid_fixture_case' packages/session-migration/src/inventory.rs \
  || ! grep -q 'bcode_session_migration_target = { workspace = true }' packages/session-migration/Cargo.toml \
  || ! rg -q 'pub enum CurrentMigrationTargetCapability' packages/session-migration-target/src/lib.rs \
  || ! rg -q 'current_migration_target_capabilities' packages/session-migration-target/src/lib.rs \
  || ! rg -q 'pub trait MigrationTarget: Send' packages/session-migration-target/src/lib.rs \
  || ! rg -q 'async fn materialize_current_schema' packages/session-migration-target/src/lib.rs \
  || ! rg -q 'async fn canonical_page' packages/session-migration-target/src/lib.rs \
  || ! rg -q 'async fn replace_canonical_row' packages/session-migration-target/src/lib.rs \
  || ! rg -q 'async fn write_authoritative_state' packages/session-migration-target/src/lib.rs \
  || ! rg -q 'async fn ingest_projectors' packages/session-migration-target/src/lib.rs \
  || ! rg -q 'async fn finalize_projectors' packages/session-migration-target/src/lib.rs \
  || ! rg -q 'async fn validate_strict_current' packages/session-migration-target/src/lib.rs \
  || ! rg -q 'async fn persist_migration_receipt' packages/session-migration-target/src/lib.rs \
  || ! rg -q 'async fn finalize_writer_contract' packages/session-migration-target/src/lib.rs \
  || ! rg -q 'finalize_validated_target' packages/session-migration/src/target.rs \
  || ! rg -q 'writer_finalization_occurs_only_after_strict_current_validation' packages/session-migration/src/target.rs \
  || ! rg -q 'impl bcode_session_migration_target::MigrationTarget for SessionMigrationTarget' packages/session/src/db.rs \
  || ! sed -n '/async fn migrate_turso_in_root_observed_with_fault/,/Ok(db)/p' packages/session/src/db.rs | grep -q 'target.materialize_current_schema().await' \
  || ! sed -n '/async fn migrate_turso_in_root_observed_with_fault/,/Ok(db)/p' packages/session/src/db.rs | grep -q 'target.persist_migration_receipt' \
  || ! sed -n '/async fn migrate_turso_in_root_observed_with_fault/,/Ok(db)/p' packages/session/src/db.rs | grep -q 'target.finalize_writer_contract' \
  || ! sed -n '/async fn rebuild_migration_projections/,/async fn migration_target_validation_facts/p' packages/session/src/db.rs | grep -q 'canonical_page' \
  || ! sed -n '/async fn rebuild_migration_projections/,/async fn migration_target_validation_facts/p' packages/session/src/db.rs | grep -q 'replace_canonical_row' \
  || ! sed -n '/async fn rebuild_migration_projections/,/async fn migration_target_validation_facts/p' packages/session/src/db.rs | grep -q 'ingest_projectors' \
  || ! sed -n '/async fn rebuild_migration_projections/,/async fn migration_target_validation_facts/p' packages/session/src/db.rs | grep -q 'write_authoritative_state' \
  || ! sed -n '/async fn validate_migrated_storage/,/struct MigrationProjectionState/p' packages/session/src/db.rs | grep -q 'target.finalize_projectors' \
  || ! sed -n '/async fn validate_migrated_storage/,/struct MigrationProjectionState/p' packages/session/src/db.rs | grep -q 'target.validate_strict_current' \
  || ! sed -n '/async fn acquire_session_lease_for_load/,/async fn acquire_current_session_lease/p' packages/session/src/lib.rs | grep -q 'StorageMigrationRequired' \
  || sed -n '/async fn acquire_session_lease_for_load/,/async fn acquire_current_session_lease/p' packages/session/src/lib.rs | grep -Eq 'RELEASED_|MigrationLedger|plan_writer_epoch|normalize_canonical' \
  || ! rg -q 'current_migration_target_implementation_exercises_every_operation' packages/session/src/db.rs \
  || ! rg -q 'current_migration_target_capability_surface_is_complete_and_policy_free' packages/session/src/db.rs \
  || ! rg -q 'validate_released_ledger_prefix_fixture_case' packages/session-migration/src/validation.rs \
  || ! rg -q 'migration_ledger_endpoints' packages/session-migration/fixtures/manifest.json \
  || ! rg -q 'covered_migration_ledger_prefixes' packages/session-migration/fixtures/manifest.json \
  || ! rg -q 'retired-events.jsonl' packages/session-migration/fixtures/manifest.json \
  || ! rg -q 'migration_ledger_endpoints:' packages/session-migration/src/inventory.rs \
  || ! rg -q 'NonSessionMigrationEndpoint' packages/session-migration/src/inventory.rs \
  || ! rg -q 'schema_28_store_fixture_classifies_affected_historical_events' packages/session-migration/src/execution.rs \
  || ! rg -q 'released_migration_and_table_inventories_are_sorted_unique_and_domain_complete' packages/session-migration/src/inventory.rs \
  || ! rg -q 'pub struct MigrationClassificationEvidence' packages/session-migration/src/validation.rs \
  || ! rg -q 'pub struct MigrationSourceEvidence<E>' packages/session-migration/src/backup.rs \
  || ! rg -q 'evidence: bcode_session_migration_target::ReplayEvidence' packages/session/src/db.rs \
  || ! rg -q 'migration_ledger_validation_rejects_unknown_and_non_contiguous_history' packages/session-migration/src/validation.rs \
  || ! rg -q 'classify_target_storage' packages/session-migration/src/validation.rs \
  || ! rg -q 'source_storage_classification_owns_contract_and_writer_policy' packages/session-migration/src/validation.rs \
  || ! rg -q 'storage_compatibility_facts' packages/session/src/db.rs \
  || ! rg -q 'classify_storage_facts' packages/session/src/db.rs \
  || sed '/#\[cfg(test)\]/,$d' packages/session/src/db.rs | rg -n 'MigrationLedgerFacts|completed_migration_ids|validate_migration_ledger|classify_source_storage' >/tmp/bcode-current-storage-policy-types.txt \
  || rg -n 'migration history claims the storage contract|classify_writer_epoch\(' packages/session/src/db.rs \
    >/tmp/bcode-current-storage-classification-policy-violations.txt \
  || rg -n 'unknown migration \{unknown\}|completed migrations are not a contiguous known prefix' packages/session/src --glob '*.rs' \
    >/tmp/bcode-current-migration-ledger-policy-violations.txt \
  || ! rg -q 'MigrationSourceEvidence' packages/server/src/session_migration_execution.rs \
  || ! rg -q 'released_format_migration_matrix' packages/session-migration/src/planning.rs \
  || ! rg -q 'ReleasedEventTreatmentRow' packages/session-migration/src/planning.rs \
  || ! rg -q 'ReleasedRecordTreatmentRow' packages/session-migration/src/planning.rs \
  || ! rg -q 'ReleasedRootTreatmentRow' packages/session-migration/src/planning.rs \
  || ! rg -q 'released_format_matrix_is_complete_unique_and_current_writable' packages/session-migration/src/planning.rs \
  || ! rg -q 'fixture_release_gate_accepts_complete_exact_coverage' packages/session-migration/src/planning.rs \
  || ! rg -q 'validate_released_fixture_coverage' packages/session-migration/src/planning.rs \
  || ! rg -q 'plan_writer_epoch_migration' packages/session-migration/src/planning.rs \
  || rg -n 'CURRENT_WRITER_EPOCH|RELEASED_HISTORICAL_EVENT_SCHEMAS|MIGRATION_STEPS' packages/session/src \
    --glob '*.rs' | rg -v 'CURRENT_SESSION_STORAGE_WRITER_EPOCH' \
    >/tmp/bcode-current-historical-inventory-violations.txt \
  || ! rg -q 'CURRENT_SESSION_STORAGE_WRITER_EPOCH: u32 =' packages/session/src/lease.rs \
  || ! rg -q 'bcode_session_models::CURRENT_SESSION_STORAGE_WRITER_EPOCH' packages/session/src/lease.rs \
  || ! rg -q 'bcode_session_models::CURRENT_SESSION_STORAGE_WRITER_EPOCH' packages/ipc/src/lib.rs \
  || ! rg -q 'session_event_schema_version' packages/ipc/src/lib.rs \
  || awk '/#\[cfg\(test\)\]/{exit} {print}' packages/session/src/db.rs \
    | rg -n 'session_compatibility_state|session_compatibility_issues|CompatibilityDegraded' \
    >/tmp/bcode-current-compatibility-projection.txt \
  || awk '/#\[cfg\(test\)\]/{exit} {print}' packages/session/src/lib.rs \
    | rg -n 'session_compatibility_state|session_compatibility_issues|CompatibilityDegraded' \
    >>/tmp/bcode-current-compatibility-projection.txt \
  || ! rg -q 'unknown_historical_kind_fails_writable_migration' packages/session-migration/src/execution.rs \
  || ! rg -q 'legacy_session_migrates_across_real_attach_and_send_ipc' packages/server/src/lib.rs; then
  echo "Session strict-current violation: migration must normalize known history, reject unresolved history atomically, and current runtime must not retain compatibility projections." >&2
  cat /tmp/bcode-current-historical-inventory-violations.txt >&2 2>/dev/null || true
  violations=1
fi

if ! rg -q 'historical_codec_only_applies_family_rules_to_released_schema_ranges' packages/session-migration/src/execution.rs \
  || ! rg -q 'pub fn normalize_canonical_event' packages/session-migration/src/execution.rs \
  || ! rg -q 'if !descriptor.supports_schema\(envelope.schema_version\(\)\)' packages/session-migration/src/execution.rs \
  || ! rg -q 'every_current_equivalent_pair_requires_strict_current_compatibility' packages/session-migration/src/execution.rs \
  || ! rg -q 'strict_current_payload_bypasses_historical_inventory' packages/session-migration/src/execution.rs \
  || ! rg -q 'if envelope.schema_version\(\) == crate::CURRENT_EVENT_SCHEMA' packages/session-migration/src/execution.rs \
  || ! rg -q 'historical_codec_only_applies_family_rules_to_released_schema_ranges' packages/session-migration/src/execution.rs \
  || ! rg -q 'all_released_context_usage_shapes_use_the_frozen_codec' packages/session-migration/src/execution.rs \
  || ! rg -q 'schema_28_store_fixture_classifies_affected_historical_events' packages/session-migration/src/execution.rs \
  || ! rg -q 'pub struct ReleasedFixtureManifest' packages/session-migration/src/inventory.rs \
  || ! rg -q 'ClassificationOnlyClaimsStoreCoverage' packages/session-migration/src/inventory.rs \
  || ! rg -q 'historical_store_event_kinds' packages/session-migration/src/inventory.rs \
  || ! rg -q 'load_released_fixture_manifest' packages/session-migration/src/inventory.rs \
  || ! rg -q 'permanent_fixture_manifest_is_exhaustive_and_inventory_valid' packages/session-migration/src/inventory.rs \
  || ! rg -q 'fixture_manifest_enforces_complete_sanitized_inventory' packages/session-migration/src/execution.rs \
  || ! rg -q 'pub mod historical_event_families' packages/session-migration/src/codec.rs \
  || ! rg -q 'pub enum HistoricalDecode' packages/session-migration/src/classification.rs \
  || ! test -f packages/session-migration/fixtures/manifest.json; then
  echo "Session historical-codec violation: released schema rules and exact fixture classifications must remain explicit." >&2
  violations=1
fi

if ! rg -q 'pub struct SessionMigrationReceipt' packages/session-migration/src/validation.rs \
  || ! rg -q 'pub fn build_session_migration_receipt' packages/session-migration/src/validation.rs \
  || ! rg -q 'plan_writer_epoch_migration' packages/session-migration/src/validation.rs \
  || ! rg -q 'pub fn classify_writer_epoch' packages/session-migration/src/validation.rs \
  || ! rg -q 'pub fn validate_migration_target' packages/session-migration/src/validation.rs \
  || ! rg -q 'pub const fn validate_writer_finalization' packages/session-migration/src/validation.rs \
  || ! rg -q 'migration_target_validation_facts' packages/session/src/db.rs \
  || ! rg -q 'validate_strict_target' packages/session/src/db.rs \
  || ! rg -q 'build_target_receipt' packages/session-migration/src/execution.rs \
  || ! rg -q 'build_receipt:.*MigrationReceiptBuilder' packages/session-migration-target/src/lib.rs \
  || sed -n '/async fn validate_migrated_storage(/,/^}/p' packages/session/src/db.rs | grep -q 'ProjectionStale\|ProjectionIncompatible\|CompatibilityDegraded'; then
  echo "Session migration validation ownership violation: receipt construction and migration-plan selection must remain migration-owned." >&2
  violations=1
fi

if ! rg -q 'pub enum SessionDiagnosisClassification' packages/session-migration/src/diagnosis.rs \
  || ! rg -q 'pub struct SessionMigrationDiagnosis' packages/session-migration/src/diagnosis.rs \
  || ! rg -q 'pub struct SessionMigrationOwnerDiagnosis' packages/session-migration/src/diagnosis.rs \
  || ! rg -q 'classify_session_diagnosis' packages/session-migration/src/diagnosis.rs \
  || rg -q '^enum SessionDiagnosisClassification' packages/cli/src/lib.rs; then
  echo "Session diagnosis ownership violation: historical/current/future diagnosis policy must remain migration-owned and CLI must only adapt database facts." >&2
  violations=1
fi

if ! rg -q '^mod current_schema;' packages/session/src/lib.rs \
  || ! rg -q 'pub fn session_migrations' packages/session/src/current_schema.rs \
  || rg -q 'bcode_session_migration|RELEASED_|Historical' packages/session/src/current_schema.rs \
  || rg -q 'fn (global_migrations|session_migrations|add_session_.*_migrations|add_sql_migration)' packages/session/src/db.rs; then
  echo "Session current-schema boundary violation: SQL schema materialization must remain isolated from database runtime behavior." >&2
  violations=1
fi

if ! rg -q '033_session_migration_receipts_table' packages/session/src/current_schema.rs \
  || ! rg -q 'build_target_migration_receipt' packages/session/src/db.rs \
  || ! sed -n '/async fn migrate_turso_in_root_observed_with_fault/,/Ok(db)/p' packages/session/src/db.rs | grep -q 'target.persist_migration_receipt' \
  || ! rg -q 'legacy_session_migrates_across_real_attach_and_send_ipc' packages/server/src/lib.rs; then
  echo "Session migration receipt violation: successful migration must transactionally retain operation, plan, canonical digest/count, classification, and completion audit metadata." >&2
  violations=1
fi

if ! rg -q 'pub async fn canonical_event_inventory' packages/session/src/db.rs \
  || ! rg -q 'strict_history_error' packages/cli/src/lib.rs \
  || ! rg -q 'event_schema_counts' packages/cli/src/lib.rs \
  || ! rg -q 'event_kind_counts' packages/cli/src/lib.rs; then
  echo "Session diagnosis violation: bounded non-decoding canonical inventory must remain available when strict historical decoding fails." >&2
  violations=1
fi

if rg -n 'decode_session_event_degraded|decode_session_event_compatible|CompatibleSessionEvent|has_trustworthy_session_event_envelope' \
  packages/session/src >/tmp/bcode-lossy-session-read-violations.txt; then
  echo "Session history violation: current canonical/indexed reads must use strict decoding only." >&2
  cat /tmp/bcode-lossy-session-read-violations.txt >&2
  violations=1
fi

if ! rg -q 'pub compatibility_issues: Vec<SessionEventCompatibilityIssue>' packages/session/models/src/lib.rs \
  || ! sed -n '/pub async fn history_page(/,/^    }/p' packages/session/src/db.rs \
    | grep -q 'let compatibility_issues = Vec::new' \
  || ! rg -q 'normal_history_reads_reject_future_events_without_mutation' packages/session/src/db.rs; then
  echo "Session strict-history violation: current bounded history must fail closed for non-current events and report no opaque compatibility issues." >&2
  violations=1
fi

if ! sed -n '/pub async fn session_history_page(/,/^    }/p' packages/session/src/lib.rs \
  | grep -q 'open_existing_turso_in_root' \
  || sed -n '/pub async fn session_history_page(/,/^    }/p' packages/session/src/lib.rs \
    | grep -Eq 'ensure_session_loaded|session_handle|acquire_session_lease'; then
  echo "Session bounded-history violation: read-only history must open the canonical DB directly without actor loading or runtime lease acquisition." >&2
  violations=1
fi

if rg -n 'incompatible_storage_writer_records|ensure_daemon_storage_compatibility' packages/server/src/lib.rs \
  >/tmp/bcode-global-daemon-storage-fence-violations.txt; then
  echo "Session storage-domain violation: daemon startup must not globally fence other fingerprints or writer epochs." >&2
  cat /tmp/bcode-global-daemon-storage-fence-violations.txt >&2
  violations=1
fi

if rg -n 'CURRENT_SESSION_EVENT_SCHEMA_VERSION\s*[-+]|schema_version\s*==\s*(2[0-9]|3[0-8])|writer-epoch-|join\("session-storage"\)' packages/session/src --glob '*.rs' \
  | rg -v 'packages/session/src/(db|ownership)\.rs' >/tmp/bcode-current-path-historical-format.txt \
  || rg -n 'decode_for_migration|HistoricalDecode|HistoricalSessionEventError' packages/session/src --glob '*.rs' \
    | rg -v 'packages/session/src/db\.rs' >/tmp/bcode-current-path-historical-codec.txt; then
  echo "Session current-boundary violation: historical schema/epoch knowledge escaped explicit migration adapters." >&2
  cat /tmp/bcode-current-path-historical-format.txt >&2 2>/dev/null || true
  cat /tmp/bcode-current-path-historical-codec.txt >&2 2>/dev/null || true
  violations=1
fi

if sed -n '/pub async fn session_history_page(/,/^    }/p' packages/session/src/lib.rs \
    | grep -Eq 'migrate_turso|rebuild_|session_history\(' \
  || sed -n '/fn load_catalog\(&self\)/,/^    }/p' packages/session/src/lib.rs \
    | grep -Eq 'migrate_turso|rebuild_|session_history\(' \
  || sed -n '/pub async fn open_existing_turso_in_root/,/^    }/p' packages/session/src/db.rs \
    | grep -Eq 'migrate_session_storage|rebuild_migration_projections'; then
  echo "Session current-read violation: catalog, bounded history, and current open paths must not migrate or full replay." >&2
  violations=1
fi

if sed '/#\[cfg(test)\]/,$d' packages/session/src/db.rs \
    | rg -q 'pub async fn migrate_turso_in_root(_observed)?' \
  || rg -n 'migrate_turso_in_root(_observed)?' packages/cli/src --glob '*.rs' \
  || rg -n 'migrate_turso_in_root(_observed)?' packages/server/src --glob '*.rs' \
    | rg -v 'session_migration_execution.rs'; then
  echo "Session migration API-surface violation: low-level historical migration entry points must remain internal to the current target adapter and tests." >&2
  violations=1
fi

if sed -n '/async fn reindex_session_model_context/,/async fn run_session_repair_command/p' packages/cli/src/lib.rs \
  | rg -q 'migrate_turso_in_root'; then
  echo "Session reindex boundary violation: explicit current reindex must require strict current storage and must not run historical migration." >&2
  violations=1
fi

if ! rg -q 'pub fn default_session_store_dir\(\)' packages/config/src/lib.rs \
  || rg -n 'default_state_dir\(\)\.join\("sessions"\)' packages/server/src packages/cli/src --glob '*.rs' \
    >/tmp/bcode-default-session-root-violations.txt \
  || rg -n 'session-storage|writer-epoch-' packages/server/src packages/cli/src --glob '*.rs' \
    >/tmp/bcode-split-session-root-violations.txt; then
  echo "Session storage-root violation: production defaults must use bcode_config::default_session_store_dir; writer epochs are per-session metadata." >&2
  cat /tmp/bcode-default-session-root-violations.txt >&2 2>/dev/null || true
  cat /tmp/bcode-split-session-root-violations.txt >&2 2>/dev/null || true
  violations=1
fi

if rg -n 'join\("session-storage"\)|writer-epoch-' packages/session/src --glob '*.rs' \
  >/tmp/bcode-current-historical-session-root-violations.txt \
  || ! rg -q 'RELEASED_HISTORICAL_ROOTS\[0\]' packages/session-migration/src/storage.rs \
  || sed '/#\[cfg(test)\]/,$d' packages/session-migration/src/storage.rs \
    | rg -n 'join\("session-storage"\).*join\("writer-epoch-2"\)' \
    >/tmp/bcode-hardcoded-historical-root-violations.txt \
  || ! rg -q 'diagnose_accidental_epoch_session_root' packages/session-migration/src/storage.rs \
  || ! rg -q 'recover_accidental_epoch_session_root' packages/session-migration/src/storage.rs \
  || ! rg -q 'relocate_session' packages/session/src/ownership.rs \
  || ! rg -q 'session_has_active_owner' packages/session/src/ownership.rs \
  || rg -n 'diagnose_accidental_epoch_session_root|recover_accidental_epoch_session_root|RELEASED_HISTORICAL_ROOTS|writer-epoch-' packages/session/src --glob '*.rs' \
    >/tmp/bcode-current-historical-policy-violations.txt \
  || rg -n 'HistoricalStorage(Relocation|Error)|bcode_session_migration' packages/session/src/ownership.rs \
    >/tmp/bcode-current-historical-adapter-policy-violations.txt; then
  echo "Session historical-root violation: historical layout discovery and recovery policy must remain migration-owned." >&2
  cat /tmp/bcode-current-historical-session-root-violations.txt >&2 2>/dev/null || true
  cat /tmp/bcode-current-historical-policy-violations.txt >&2 2>/dev/null || true
  cat /tmp/bcode-current-historical-adapter-policy-violations.txt >&2 2>/dev/null || true
  violations=1
fi

if ! rg -q 'session_dir_path\(root, session_id\)\.join\("session\.db"\)' packages/session/src/db_path.rs; then
  echo "Session path violation: session_db_path must remain root/<session-id>/session.db." >&2
  violations=1
fi

if rg -n '\*\.events|sessions/index/' docs/session-persistence-architecture.md >/tmp/bcode-stale-session-docs.txt; then
  echo "Session documentation violation: obsolete file-log/index architecture is documented as current." >&2
  cat /tmp/bcode-stale-session-docs.txt >&2
  violations=1
fi

if ! rg -q '^## Current runtime and migration boundaries$' docs/session-persistence-architecture.md \
  || ! rg -q '`bcode_session` must not depend on `bcode_session_migration`' docs/session-persistence-architecture.md \
  || ! rg -q 'advance the writer epoch as the final transactional mutation' docs/session-persistence-architecture.md \
  || ! rg -q 'one daemon instance per session' docs/session-persistence-architecture.md; then
  echo "Session migration architecture documentation violation: current-only boundaries, dependency direction, epoch ordering, and ownership handoff must remain explicit." >&2
  violations=1
fi

if sed -n '/async fn legacy_session_migrates_across_real_attach_and_send_ipc/,/^    }/p' packages/server/src/lib.rs \
  | rg -q 'exec_raw'; then
  echo "Session migration fixture violation: use typed Switchy delete/drop-table operations." >&2
  violations=1
fi

if ! scripts/check-no-normal-full-scans.sh; then
  violations=1
fi

if ! scripts/check-loop-runtime-architecture.sh; then
  violations=1
fi

if rg -n "handle\.state" packages/session/src/lib.rs >/tmp/bcode-session-actor-violations.txt; then
  echo "Session actor architecture violation: SessionHandle state must not be accessed directly." >&2
  cat /tmp/bcode-session-actor-violations.txt >&2
  violations=1
fi

if rg -n "std::fs|OpenOptions|fs::File|File::open|File::create" packages/session/src --glob '*.rs' \
  | rg -v 'packages/session/src/(lib|index|reader|migration|event_migration|ownership|derived|db|lease|repair|store)\.rs' \
  >/tmp/bcode-session-fs-violations.txt; then
  echo "Session persistence architecture violation: direct filesystem access outside approved store modules." >&2
  cat /tmp/bcode-session-fs-violations.txt >&2
  violations=1
fi

if rg -n '^bcode_session_migration = ' packages/session/Cargo.toml \
  || rg -n 'bcode_session_migration::' packages/session/src --glob '*.rs' \
    >/tmp/bcode-session-production-migration-dependency-violations.txt; then
  echo "Session dependency-direction violation: production bcode_session must not depend on bcode_session_migration." >&2
  cat /tmp/bcode-session-production-migration-dependency-violations.txt >&2 2>/dev/null || true
  violations=1
fi

if rg -n 'RELEASED_HISTORICAL|ReleasedMigrationTreatment|HistoricalSessionEvent|OpaqueEvent|CompatibilityDegraded|writer-epoch-|join\("session-storage"\)' \
  packages/session/src --glob '*.rs' --glob '!db.rs' \
  >/tmp/bcode-session-forbidden-historical-policy-violations.txt \
  || sed -n '760,3700p' packages/session/src/db.rs \
    | rg -n 'RELEASED_HISTORICAL|ReleasedMigrationTreatment|HistoricalSessionEvent|OpaqueEvent|CompatibilityDegraded|writer-epoch-|join\("session-storage"\)' \
    >>/tmp/bcode-session-forbidden-historical-policy-violations.txt; then
  echo "Session strict-current policy violation: historical inventory, legacy-root, opaque-read, and degraded compatibility policy must remain outside production bcode_session." >&2
  cat /tmp/bcode-session-forbidden-historical-policy-violations.txt >&2 2>/dev/null || true
  violations=1
fi

if ! rg -q "mod actor;" packages/session/src/lib.rs; then
  echo "Session module split violation: actor module must remain split from lib.rs." >&2
  violations=1
fi

for session_module in attach attachment catalog context db_artifact db_connection db_context db_contract db_event_store db_path db_projection db_projection_row db_row db_runtime_work db_validation fork manifest mutation ownership runtime_work state store store_executor subscription tools; do
  if ! rg -q "(pub(\\(crate\\))? )?mod ${session_module};" packages/session/src/lib.rs \
    || [[ ! -f "packages/session/src/${session_module}.rs" ]]; then
    echo "Session module split violation: ${session_module} domain module must remain extracted from lib.rs." >&2
    violations=1
  fi
done

if rg -n 'bcode_session_migration|HistoricalStorage|RELEASED_HISTORICAL|writer-epoch-' \
  packages/session/src/{attach,attachment,catalog,context,fork,manifest,mutation,ownership,runtime_work,state,store,store_executor,tools}.rs \
  >/tmp/bcode-session-extracted-domain-migration-violations.txt; then
  echo "Session module split violation: extracted current-runtime domains must not acquire historical migration policy." >&2
  cat /tmp/bcode-session-extracted-domain-migration-violations.txt >&2
  violations=1
fi

if rg -n 'SessionDb::migrate|normalize_canonical_event|recover_accidental_epoch_session_root' \
  packages/session/src/{attach,context,fork,mutation,runtime_work,subscription,tools}.rs \
  >/tmp/bcode-session-extracted-domain-mutation-violations.txt; then
  echo "Session module split violation: normal manager domains must not invoke migration or historical normalization." >&2
  cat /tmp/bcode-session-extracted-domain-mutation-violations.txt >&2
  violations=1
fi

if ! rg -q "mod store_executor;" packages/session/src/lib.rs; then
  echo "Session module split violation: store executor module must remain split from lib.rs." >&2
  violations=1
fi

if rg -n "SessionDb::open_turso_in_root" packages/server/src --glob '*.rs' >/tmp/bcode-server-session-db-open-violations.txt; then
  echo "Session architecture violation: server code must access per-session DBs through SessionManager/SessionActor." >&2
  cat /tmp/bcode-server-session-db-open-violations.txt >&2
  violations=1
fi

normal_session_open_violations="$(
  rg -n '\bSessionDb::open_turso_in_root(_observed)?' packages/session/src/actor.rs \
    || true
)"
if [[ -n "$normal_session_open_violations" ]]; then
  echo "Session open-mode violation: production session paths must use explicit existing/runtime/initialize/maintenance opens." >&2
  printf '%s\n' "$normal_session_open_violations" >&2
  violations=1
fi

if ! rg -q 'CREATE TABLE IF NOT EXISTS artifact_references' packages/session/src/current_schema.rs \
  || ! rg -q 'MaterializedProjection::ArtifactReferences' packages/session/src/db.rs; then
  echo "Session artifact projection violation: finalized references require a checkpointed bounded projection." >&2
  violations=1
fi

artifact_read_body="$(sed -n '/async fn read_session_artifact_range(/,/^async fn handle_delete_session(/p' packages/server/src/lib.rs)"
if grep -q 'session_history' <<<"$artifact_read_body"; then
  echo "Session artifact lookup violation: normal range reads must not scan session history." >&2
  violations=1
fi
if ! grep -q 'finalized_artifact_reference' <<<"$artifact_read_body"; then
  echo "Session artifact lookup violation: finalized reads must use the bounded reference projection." >&2
  violations=1
fi

if ! rg -q 'SessionEventKind::ModelTurnStarted.*=> "model_turn_started"' packages/session/src/db_context.rs \
  || ! rg -q 'SessionEventKind::ModelTurnFinished.*=> "model_turn_finished"' packages/session/src/db_context.rs; then
  echo "Session model-context projection violation: model-turn lifecycle boundaries must remain structural context events." >&2
  violations=1
fi

model_context_types="$(sed -n '/const MODEL_CONTEXT_EVENT_TYPES:/,/^];/p' packages/session/src/db.rs)"
if grep -Eq 'context_usage_observed|request_context_observed' <<<"$model_context_types"; then
  echo "Session model-context projection violation: context occupancy belongs only in its dedicated projection." >&2
  violations=1
fi

if rg -q 'async fn update_projection_checkpoints' packages/session/src/db.rs; then
  echo "Session projection checkpoint violation: blanket checkpoint advancement is forbidden." >&2
  violations=1
fi

model_context_projector="$(sed -n '/async fn project_model_context_event(/,/^async fn project_context_occupancy_event(/p' packages/session/src/db.rs)"
if grep -q 'None => return Ok(())' <<<"$model_context_projector"; then
  echo "Session model-context projection violation: missing projection state must not silently accept append." >&2
  violations=1
fi
if ! grep -q 'ModelContextProjectionVersion' <<<"$model_context_projector" \
  || ! grep -q 'ModelContextProjectionStale' <<<"$model_context_projector"; then
  echo "Session model-context projection violation: append must reject incompatible or stale state." >&2
  violations=1
fi

if ! rg -q 'validate_storage_writer_contract_for_epoch\(&\*tx, writer_epoch\)\.await' packages/session/src/db.rs \
  || ! rg -q 'session_storage_contract' packages/session/src/db.rs; then
  echo "Session writer contract violation: durable appends require explicit writer-epoch validation." >&2
  violations=1
fi

if ! rg -q 'CURRENT_SESSION_STORAGE_WRITER_EPOCH' packages/server/src/lib.rs; then
  echo "Session lease identity violation: production daemon leases must advertise storage writer epoch." >&2
  violations=1
fi

if ! rg -q 'acquire_session_maintenance_guard\(&root, session_id\)' packages/cli/src/lib.rs; then
  echo "Session reindex violation: CLI reindex requires exclusive maintenance coordination." >&2
  violations=1
fi

if ! rg -q 'acquire_session_maintenance_guard\(root, session_id\)' packages/session/src/repair.rs; then
  echo "Session repair violation: mutating repair requires exclusive maintenance coordination." >&2
  violations=1
fi

if ! rg -q 'pub async fn open_existing_turso_in_root' packages/session/src/db.rs \
  || ! rg -q 'pub\(crate\) async fn migrate_turso_in_root' packages/session/src/db.rs; then
  echo "Session open-mode violation: runtime/read and maintenance migration opens must remain explicit." >&2
  violations=1
fi

runtime_open_body="$(sed -n '/pub struct SessionDb {/,$p' packages/session/src/db.rs | sed -n '/pub async fn open_turso_in_root_observed(/,/^    \/\/\/ Open an existing database at/p')"
if grep -Eq 'run_session_migrations|migrate_model_context_projection|rebuild_model_context_projection' <<<"$runtime_open_body"; then
  echo "Session open-mode violation: ordinary runtime open must not migrate or rebuild projections." >&2
  violations=1
fi

migration_call_count="$( (rg -n 'migrate_session_storage\(' packages/session/src/db.rs || true) | wc -l | tr -d ' ')"
if [[ "$migration_call_count" != "2" ]]; then
  echo "Session migration violation: storage migration must only be defined and called by explicit migration open." >&2
  violations=1
fi

if ! rg -q 'db\.validate_write_readiness\(\)\.await\?' packages/session/src/lib.rs \
  || ! sed -n '/async fn load_persistent_session/,/async fn release_persistent_idle_session_resources/p' packages/session/src/lib.rs | grep -q 'acquire_session_lease_for_load' \
  || ! sed -n '/async fn load_persistent_session/,/async fn release_persistent_idle_session_resources/p' packages/session/src/lib.rs | grep -q 'validate_write_readiness' \
  || ! sed -n '/async fn ensure_cached_session_loaded/,/async fn acquire_session_lease_for_load/p' packages/session/src/lib.rs | grep -q 'validate_write_readiness' \
  || ! rg -q 'write_readiness_uses_actor_connection_before_followup_append' packages/session/src/lib.rs; then
  echo "Session runtime-readiness violation: current attach/load must acquire runtime ownership before validating write readiness, including lease reacquisition." >&2
  violations=1
fi

if rg -n 'open_turso_in_root\(session_id, root\)' packages/session/src/repair.rs >/tmp/bcode-repair-mutating-open-violations.txt; then
  echo "Session repair violation: doctor/validation paths must use existing non-migrating opens." >&2
  cat /tmp/bcode-repair-mutating-open-violations.txt >&2
  violations=1
fi

if ! rg -q 'let tx = db\.db\.begin_transaction\(\)\.await' packages/session/src/db.rs \
  || ! sed -n '/async fn migrate_turso_in_root_observed_with_fault/,/Ok(db)/p' packages/session/src/db.rs | grep -q 'target.materialize_current_schema().await' \
  || ! rg -q 'migrate_session_storage\(' packages/session/src/db.rs \
  || ! sed -n '/async fn migrate_turso_in_root_observed_with_fault/,/Ok(db)/p' packages/session/src/db.rs | grep -q 'migrate_session_storage(' \
  || ! sed -n '/async fn migrate_turso_in_root_observed_with_fault/,/Ok(db)/p' packages/session/src/db.rs | grep -q 'target.finalize_writer_contract'; then
  echo "Session migration violation: schema migration, projection replay, and writer-epoch update must share explicit migration transaction." >&2
  violations=1
fi

if ! sed -n '/async fn session_export(/,/^}/p' packages/cli/src/lib.rs \
    | grep -q 'session_read_client' \
  || ! sed -n '/async fn session_export(/,/^}/p' packages/cli/src/lib.rs \
    | grep -q 'session_history_page' \
  || ! sed -n '/async fn session_export(/,/^}/p' packages/cli/src/lib.rs \
    | grep -q 'SESSION_CLI_PAGE_LIMIT' \
  || ! sed -n '/async fn session_export(/,/^}/p' packages/cli/src/lib.rs \
    | grep -q 'has_more' \
  || ! rg -q 'async fn session_read_client' packages/cli/src/lib.rs \
  || ! sed -n '/async fn session_read_client/,/^}/p' packages/cli/src/lib.rs | grep -q 'session_owner_client' \
  || ! sed -n '/async fn session_read_client/,/^}/p' packages/cli/src/lib.rs | grep -q 'BcodeClient::default_endpoint' \
  || ! rg -q 'async fn session_owner_client' packages/cli/src/lib.rs \
  || ! sed -n '/async fn session_owner_client/,/^}/p' packages/cli/src/lib.rs | grep -q 'session_owner_record' \
  || ! sed -n '/async fn session_owner_client/,/^}/p' packages/cli/src/lib.rs | grep -q 'daemon_status_matches' \
  || ! sed -n '/async fn session_history(/,/^}/p' packages/cli/src/lib.rs | grep -q 'session_read_client' \
  || ! sed -n '/async fn session_around(/,/^}/p' packages/cli/src/lib.rs | grep -q 'session_owner_client' \
  || ! sed -n '/async fn session_inspect(/,/^}/p' packages/cli/src/lib.rs | grep -q 'session_owner_client' \
  || rg -q 'session_export_events_from_root|explicit_export_reads_legacy_stream_history_without_migration|ToolInvocationStreamEvent|ToolOutputStream' \
    packages/cli/src packages/session/src --glob '*.rs'; then
  echo "Session investigation violation: normal session reads and export must route to the verified owning daemon, while export remains explicit complete history and legacy stream decoding stays prohibited." >&2
  violations=1
fi

if ! rg -q 'storage_compatibility\(\)' packages/session/src/lib.rs \
  || ! rg -q 'load_gates: BTreeMap<SessionId, Arc<Mutex<\(\)>>>' packages/session/src/lib.rs \
  || ! rg -q 'migrate_owned_session_storage\(' packages/server/src/session_migration_execution.rs \
  || ! rg -q 'acquire_session_maintenance_guard\(root, session_id\)' packages/server/src/session_migration_execution.rs \
  || rg -q 'migrate_legacy_session_for_load|transition_session_maintenance_to_lease' packages/session/src/lib.rs; then
  echo "Session normal-load violation: manager first load must classify storage, serialize per session, and safely migrate known legacy storage under exclusive maintenance ownership." >&2
  violations=1
fi

if ! rg -q 'KnownLegacy \{ writer_epoch \} => Err\(SessionError::StorageMigrationRequired' packages/session/src/lib.rs; then
  echo "Session normal-load violation: strict current loading must fail closed with policy-free migration-required facts." >&2
  violations=1
fi

if ! rg -q 'first_non_current_event_schema' packages/session/src/db.rs \
  || ! rg -q 'first_non_current_event_schema_uses_bounded_scalar_detection' packages/session/src/db.rs \
  || ! rg -q 'current_writer_with_released_historical_events' packages/server/src/lib.rs \
  || ! rg -q 'requires_event_normalization' packages/server/src/session_migration_execution.rs \
  || ! rg -q 'current_writer_schema_40_session_migrates_on_real_open_ipc' packages/server/src/lib.rs; then
  echo "Session migration event-schema classification violation: current-writer stores with released historical event schemas must be routed through explicit first-open migration." >&2
  violations=1
fi
if ! rg -q 'unknown_historical_kind_fails_writable_migration' packages/session-migration/src/execution.rs \
  || ! rg -q 'legacy_session_migrates_across_real_attach_and_send_ipc' packages/server/src/lib.rs \
  || ! rg -q 'failed_explicit_migration_preserves_projection_and_writer_contract' packages/session/src/db.rs \
  || ! sed -n '/async fn failed_explicit_migration_preserves_projection_and_writer_contract(/,/^    }/p' packages/session/src/db.rs | grep -q 'failed migration must preserve every session storage file byte-for-byte'; then
  echo "Session migration regression violation: known history must normalize writable and unresolved/failed migration must preserve source storage." >&2
  violations=1
fi

if ! rg -q 'CURRENT_PROTOCOL_VERSION: u16 = [1-9][0-9]*' packages/ipc/src/lib.rs \
  || ! rg -q 'PrepareSessionOpen' packages/ipc/src/lib.rs \
  || ! rg -q 'WaitSessionOpenProgress' packages/ipc/src/lib.rs \
  || ! rg -q 'SessionOpenPrepared' packages/ipc/src/lib.rs \
  || ! rg -q 'session_open_preparation_requests_and_response_round_trip' packages/ipc/src/lib.rs \
  || ! rg -q 'session_open_wait_returns_newer_terminal_or_timeout_snapshot' packages/server/src/lib.rs \
  || ! rg -q 'session_open_operation_not_found' packages/server/src/lib.rs \
  || ! rg -q 'legacy_session_migrates_across_real_attach_and_send_ipc' packages/server/src/lib.rs \
  || ! rg -q 'preparation_recovers_retained_operation_after_transport_interruption' packages/client/src/lib.rs \
  || ! rg -q 'dropping_progress_receiver_stops_client_observation_cleanly' packages/client/src/lib.rs \
  || ! rg -q 'runner_drains_streaming_session_progress_through_effect_results' packages/tui/src/effects.rs \
  || ! rg -q 'migration_stage_families_and_terminal_failure_render_through_status_chrome' packages/tui/src/tests.rs \
  || ! rg -q 'session_open_progress_ignores_stale_session_updates' packages/tui/src/chat_loop.rs \
  || ! rg -q 'prepare_session_open_until_terminal' packages/client/src/lib.rs; then
  echo "Session migration IPC violation: versioned prepare/wait routing, bounded revision waits, exact operation errors, codec coverage, and client APIs must remain present." >&2
  violations=1
fi

if ! rg -q 'pub mod historical_event_families' packages/session-migration/src/codec.rs \
  || ! sed -n '/fn decode_for_migration(/,/^}/p' packages/session-migration/src/execution.rs | grep -q 'return historical_event_families::decode_schema_28' \
  || ! rg -q 'historical_event_families::decode_context_usage_observed' packages/session-migration/src/execution.rs \
  || ! rg -q 'source_kind == "context_usage_observed" && envelope.schema_version\(\) <= 31' packages/session-migration/src/execution.rs \
  || ! rg -q 'ToolArtifactDto' packages/session-migration/src/codec.rs \
  || rg -n 'Artifact \{ artifact: Box<ToolArtifact> \}' packages/session-migration/src/codec.rs >/tmp/bcode-mutable-historical-artifact-dto.txt \
  || ! rg -q 'normalize_canonical_row' packages/server/src/session_migration_execution.rs \
  || rg -n 'decode_session_event_compatible|CompatibleSessionEvent|decode_opaque_session_event' packages/session/src/persisted.rs >/tmp/bcode-current-codec-opaque-read.txt \
  || rg -n 'context_usage_observed|tool_call_finished|tool_invocation_stream|interactive_tool_request|plugin_automation_turn|legacy_turn|legacy_tool_invocation' packages/session/src/persisted.rs >/tmp/bcode-current-codec-historical-family.txt \
  || ! rg -q 'normal_history_reads_reject_future_events_without_mutation' packages/session/src/db.rs \
  || ! sed -n '/"tool_invocation_stream" =>/,/}),/p' packages/session-migration/src/codec.rs | grep -q 'RetiredKnown' \
  || rg -n 'ToolInvocationStreamEvent|ToolInvocationStream' packages/session/src packages/session-migration/src --glob '*.rs' >/tmp/bcode-retired-tool-stream-runtime.txt \
  || ! rg -q 'future_writer_is_never_downgraded' packages/session-migration/src/planning.rs \
  || ! rg -q 'MIGRATION_EVENT_PAGE_SIZE: usize = 1_000' packages/session/src/db.rs \
  || ! rg -q 'canonical_page\(completed, MIGRATION_EVENT_PAGE_SIZE\)' packages/session/src/db.rs \
  || rg -q 'canonical_migration_history' packages/session/src/db.rs \
  || ! sed -n '/async fn legacy_session_migrates_across_real_attach_and_send_ipc/,/^    }/p' packages/server/src/lib.rs | grep -q 'attach' \
  || ! sed -n '/async fn legacy_session_migrates_across_real_attach_and_send_ipc/,/^    }/p' packages/server/src/lib.rs | grep -q 'send' \
  || ! rg -q 'writer_schema_event_combinations' packages/session-migration/src/inventory.rs \
  || ! rg -q 'legacy_session_migrates_across_real_attach_and_send_ipc' packages/server/src/lib.rs \
  || ! rg -q 'released_fixture_coverage_gaps' packages/session-migration/src/inventory.rs \
  || ! rg -q 'query_raw\("SELECT MAX\(event_seq\) AS event_seq FROM events"\)' packages/session/src/db.rs \
  || ! rg -q 'released-explicit-conversions.jsonl' packages/session-migration/fixtures/manifest.json \
  || ! rg -q 'retired-events.jsonl' packages/session-migration/fixtures/manifest.json \
  || ! rg -q 'current-equivalent fixture payload must strictly decode' packages/session-migration/src/execution.rs \
  || ! rg -q 'schema_event_exclusions' packages/session-migration/src/inventory.rs \
  || ! rg -q 'owns_writer_schema_event' packages/session-migration/src/inventory.rs \
  || ! rg -q 'assert!\(gaps.is_empty\(\), "fixture coverage gaps' packages/session-migration/src/inventory.rs \
  || ! rg -q 'AuthoritativeMigrationState' packages/session-migration/src/execution.rs \
  || ! rg -q 'migration_pages_more_than_one_thousand_events_without_gaps_or_duplicates' packages/session/src/lib.rs \
  || ! rg -q 'backup_process_crash_boundaries_preserve_source' packages/session-migration/src/backup.rs \
  || ! rg -q 'retry backup with a fresh destination' packages/session-migration/src/backup.rs \
  || ! sed -n '/async fn migration_crash_boundaries_reclassify_durably/,/^    }/p' packages/session/src/db.rs | grep -q 'retry interrupted migration' \
  || ! sed -n '/fn lease_handoff_process_crash_leaves_recoverable_owner/,/^    }/p' packages/session/src/lease.rs | grep -q 'retry ownership after handoff crash' \
  || ! rg -q 'lease_handoff_process_crash_leaves_recoverable_owner' packages/session/src/lease.rs \
  || ! rg -q 'abort_at_migration_crash_boundary\("transaction_start"\)' packages/session/src/db.rs \
  || ! rg -q 'abort_at_migration_crash_boundary\("normalization"\)' packages/session/src/db.rs \
  || ! rg -q 'abort_at_migration_crash_boundary\("projection_rebuild"\)' packages/session/src/db.rs \
  || ! rg -q 'abort_at_migration_crash_boundary\("final_validation"\)' packages/session/src/db.rs \
  || ! rg -q 'abort_at_migration_crash_boundary\("before_epoch_update"\)' packages/session/src/db.rs \
  || ! rg -q 'abort_at_migration_crash_boundary\("after_epoch_update_before_commit"\)' packages/session/src/db.rs \
  || ! rg -q 'abort_at_migration_crash_boundary\("post_commit_checkpoint"\)' packages/session/src/db.rs \
  || ! rg -q 'build_target_migration_receipt' packages/session/src/db.rs \
  || ! sed -n '/let migration_outcome =/,/validate_storage_writer_contract/p' packages/session/src/db.rs | awk '/target.persist_migration_receipt/{receipt=NR} /target.finalize_writer_contract/{epoch=NR} END {exit !(receipt && epoch && receipt < epoch)}' \
  || ! sed -n '/async fn migrate_turso_in_root_observed_with_fault/,/Ok(db)/p' packages/session/src/db.rs | grep -q 'target.finalize_writer_contract' \
  || ! rg -q 'CONVERTED_TOOL_CALL_FINISHED' packages/session-migration/src/execution.rs \
  || ! rg -q 'let current = decode_current\(payload\)' packages/session-migration/src/execution.rs \
  || ! sed -n '/fn decode_for_migration(/,/^}/p' packages/session-migration/src/execution.rs | grep -q 'is_released_historical_event_schema' \
  || ! rg -q 'authoritative_state_conversion_tracks_resets_and_reconciled_observations' packages/session-migration/src/execution.rs \
  || ! rg -q 'state.authoritative.ingest\(event\)' packages/session/src/db.rs \
  || rg -n 'fn project_migration_context_occupancy_event' packages/session/src/db.rs \
    >/tmp/bcode-current-authoritative-migration-policy-violations.txt \
  || ! rg -q 'CONVERTED_CONTEXT_USAGE_OBSERVED' packages/session-migration/src/execution.rs \
  || ! rg -q 'RETIRED_TOOL_INVOCATION_STREAM' packages/session-migration/src/execution.rs \
  || rg -n 'record_histogram_with_labels|add_counter_with_labels|increment_counter_with_labels' packages/server/src/session_migration_execution.rs packages/session/src/db.rs \
  || ! rg -q 'bytes_total' packages/server/src/session_migration_execution.rs \
  || ! rg -q 'progress_reporter_throttles_intermediate_updates_and_preserves_boundaries' packages/session-migration/src/operation.rs \
  || ! rg -q 'legacy_session_migrates_across_real_attach_and_send_ipc' packages/server/src/lib.rs \
  || ! rg -q 'let mut tail = None' packages/session/src/db.rs \
  || rg -n 'update_multi\("events"\)|upsert_multi\("events"\)' packages/session/src/db.rs >/tmp/bcode-unprofiled-canonical-batch.txt \
  || ! rg -q 'deterministic_migration_faults_roll_back_every_transaction_phase' packages/session/src/db.rs \
  || ! sed -n '/async fn deterministic_migration_faults_roll_back_every_transaction_phase/,/^    }/p' packages/session/src/db.rs | grep -q 'MigrationFaultPhase::CanonicalDecode' \
  || ! sed -n '/async fn deterministic_migration_faults_roll_back_every_transaction_phase/,/^    }/p' packages/session/src/db.rs | grep -q 'MigrationFaultPhase::Projection' \
  || ! sed -n '/async fn deterministic_migration_faults_roll_back_every_transaction_phase/,/^    }/p' packages/session/src/db.rs | grep -q 'MigrationFaultPhase::FinalValidation' \
  || ! sed -n '/async fn deterministic_migration_faults_roll_back_every_transaction_phase/,/^    }/p' packages/session/src/db.rs | grep -q 'MigrationFaultPhase::Receipt' \
  || ! sed -n '/async fn deterministic_migration_faults_roll_back_every_transaction_phase/,/^    }/p' packages/session/src/db.rs | grep -q 'MigrationFaultPhase::WriterEpochFinalization' \
  || ! rg -q 'unknown_historical_kind_fails_writable_migration' packages/session-migration/src/execution.rs \
  || ! rg -q 'backup_process_crash_boundaries_preserve_source' packages/session-migration/src/backup.rs \
  || ! rg -q 'backup_refuses_conflicts_corruption_and_reserved_manifest' packages/session-migration/src/backup.rs \
  || ! rg -q 'publish_backup_path' packages/session-migration/src/operation.rs \
  || ! rg -q 'completed.is_multiple_of\(100\)' packages/session/src/db.rs \
  || ! rg -q 'DEFAULT_PROGRESS_UNIT_INTERVAL' packages/session-migration/src/operation.rs; then
  echo "Session migration boundary/progress violation: writable historical decode, degraded read-only decode, inert retired history, bounded replay, ordered progress, and transactional coverage must remain intact." >&2
  violations=1
fi

if ! rg -q 'daemon_instance_id: Some\(format!\("process-' packages/session/src/lease.rs \
  || ! rg -q 'owner\.daemon_instance_id == context\.daemon_instance_id' packages/session/src/lease.rs \
  || ! rg -q 'owner_error_exposes_actionable_identity_without_database_access' packages/session/src/lease.rs \
  || ! sed -n '/fn owner_error_exposes_actionable_identity_without_database_access/,/^    }/p' packages/session/src/lease.rs | grep -q 'daemon instance owner-build' \
  || ! rg -q 'two_current_daemons_racing_for_one_session_yield_one_owner' packages/session/src/lease.rs \
  || ! rg -q 'different_writer_versions_can_own_different_sessions' packages/session/src/lease.rs \
  || ! rg -q 'release_stop_and_killed_owner_workflows_allow_next_owner' packages/session/src/lease.rs \
  || ! rg -q 'prunes_dead_owner_before_compatibility_check' packages/session/src/lease.rs \
  || ! rg -q 'rejects_second_daemon_at_same_writer_epoch' packages/session/src/lease.rs \
  || ! rg -q 'allows_reentrant_registrations_from_one_daemon_instance' packages/session/src/lease.rs \
  || ! rg -q 'maintenance_refuses_any_live_session_owner' packages/session/src/lease.rs \
  || ! rg -q 'maintenance_to_lease_transition_prevents_incompatible_handoff_race' packages/session/src/lease.rs \
  || ! sed -n '/pub async fn migrate_owned_session_storage(/,/^}/p' packages/server/src/session_migration_execution.rs | grep -q 'acquire_maintenance_session_write_lock' \
  || ! sed -n '/pub async fn migrate_owned_session_storage(/,/^}/p' packages/server/src/session_migration_execution.rs | grep -q 'transition_session_maintenance_to_lease' \
  || ! sed -n '/pub async fn migrate_owned_session_storage(/,/^}/p' packages/server/src/session_migration_execution.rs | grep -q 'validate_write_readiness' \
  || ! rg -q 'execute_owned_legacy_storage' packages/server/src/session_migration_execution.rs \
  || ! rg -q 'progress_reporter_throttles_intermediate_updates_and_preserves_boundaries' packages/session-migration/src/operation.rs \
  || rg -n 'struct MigrationProgressReporter|MIGRATION_PROGRESS_BYTE_INTERVAL' packages/session/src --glob '*.rs' \
    >/tmp/bcode-current-migration-progress-policy-violations.txt \
  || rg -n 'fn migrate_owned_legacy_storage|fn create_verified_migration_backup|struct MigrationProgressReporter' packages/session/src/lib.rs \
  || ! rg -q 'session_migrations: bcode_session_migration::SessionMigrationService' packages/server/src/lib.rs \
  || ! rg -q 'session_migrations' packages/server/src/lib.rs \
  || ! rg -q '\.operations\(\)' packages/server/src/lib.rs \
  || ! rg -q '\.start_or_join\(' packages/server/src/lib.rs \
  || ! rg -q 'session_migrations\.(active_count|is_active)\(' packages/server/src/lib.rs \
  || ! rg -q 'pub async fn prepare_session_open' packages/session/src/lib.rs \
  || rg -n 'migration_operations: bcode_session_migration::SessionMigrationOperations|with_migration_operations' packages/session/src/lib.rs \
  || ! rg -q 'concurrent_starts_join_one_running_operation' packages/session-migration/src/operation.rs \
  || ! rg -q 'pruning_is_bounded_and_never_removes_running_operations' packages/session-migration/src/operation.rs \
  || test -e packages/session/src/migration_operation.rs; then
  echo "Session migration operation violation: production preparation, one-per-session joining, reconnectable snapshots, bounded retention, and current-session bypass must remain covered." >&2
  violations=1
fi

if ! rg -q 'project_materialized_event_without_checkpoints\(db, event\)' packages/session/src/db.rs \
  || ! sed -n '/async fn finalize_projectors/,/async fn validate_strict_current/p' packages/session/src/db.rs | grep -q 'project_migration_materialized_checkpoints_at_tail' \
  || ! rg -q 'upsert_multi\("projection_checkpoints"\)' packages/session/src/db.rs \
  || ! rg -q 'legacy_session_migrates_across_real_attach_and_send_ipc' packages/server/src/lib.rs; then
  echo "Session migration replay violation: migration must retain one batched tail checkpoint write and focused generated profiling." >&2
  violations=1
fi

if ! rg -q 'legacy_session_migrates_across_real_attach_and_send_ipc' packages/server/src/lib.rs \
  || ! rg -q 'MIGRATION_PROGRESS_OVERHEAD_PERCENT_BUDGET: u128 = 10' packages/session/src/lib.rs \
  || ! rg -q 'CURRENT_SESSION_PREPARE_P95_BUDGET_MS: u128 = 25' packages/session/src/lib.rs \
  || ! rg -q 'let mut tail = None' packages/session/src/db.rs; then
  echo "Session migration benchmark violation: deterministic generated stores and stable CI-safe acceptance thresholds must remain available without private content." >&2
  violations=1
fi

backup_source="$(sed -n '/^const BACKUP_BUFFER_BYTES/,/^fn hash_file/p' packages/session-migration/src/backup.rs)"
if grep -Eq 'fs::read\(&source|fs::read\(&destination' <<<"$backup_source" \
  || ! grep -q 'spawn_blocking' packages/session-migration/src/backup.rs \
  || ! grep -q 'BufReader::with_capacity' <<<"$backup_source" \
  || ! grep -q 'BufWriter::with_capacity' <<<"$backup_source" \
  || ! grep -q 'Sha256::new' <<<"$backup_source" \
  || ! grep -q 'create_new(true)' <<<"$backup_source" \
  || ! grep -q 'remove_dir_all(destination)' <<<"$backup_source"; then
  echo "Session migration backup violation: backups must remain streaming, bounded, hash-verified, conflict-safe, cleanup-safe, and off Tokio workers." >&2
  violations=1
fi
if ! rg -q 'streaming_backup_handles_nested_empty_and_large_files' packages/session-migration/src/backup.rs \
  || ! rg -q 'backup_refuses_conflicts_corruption_and_reserved_manifest' packages/session-migration/src/backup.rs \
  || ! rg -q 'backup_faults_are_deterministic_and_cleanup_partial_output' packages/session-migration/src/backup.rs \
  || ! rg -q 'backup_process_crash_boundaries_preserve_source' packages/session-migration/src/backup.rs \
  || ! rg -q 'legacy_session_migrates_across_real_attach_and_send_ipc' packages/server/src/lib.rs; then
  echo "Session migration backup violation: streaming, retained-success, conflict, cleanup, and mutation-fence regressions must remain covered." >&2
  violations=1
fi

migration_metric_sources="$(cat packages/server/src/session_migration_execution.rs; sed -n '/pub(crate) async fn migrate_turso_in_root_observed(/,/pub async fn open_turso_in_root/p; /async fn migrate_session_storage(/,/^}/p; /async fn rebuild_migration_projections(/,/^}/p; /async fn validate_migrated_storage(/,/^}/p; /async fn project_migration_event(/,/^}/p' packages/session/src/db.rs)"
for metric in \
  session.migration.ownership_duration_ms \
  session.migration.backup.plan_duration_ms \
  session.migration.backup.copy_duration_ms \
  session.migration.backup.verify_duration_ms \
  session.migration.schema_duration_ms \
  session.migration.canonical_decode_duration_ms \
  session.migration.projection_rebuild_duration_ms \
  session.migration.validation_duration_ms \
  session.migration.commit_duration_ms \
  session.migration.write_readiness_duration_ms \
  session.migration.canonical_events_total \
  session.migration.projected_events_total; do
  if ! grep -Fq "$metric" <<<"$migration_metric_sources"; then
    echo "Session migration observability violation: required fixed metric $metric is missing." >&2
    violations=1
  fi
done
if grep -Eq 'record_histogram_with_labels|add_counter_with_labels|increment_counter_with_labels' <<<"$migration_metric_sources"; then
  echo "Session migration observability violation: migration stage metrics must use fixed unlabeled names to keep cardinality bounded." >&2
  violations=1
fi
if ! rg -q 'progress_reporter_throttles_intermediate_updates_and_preserves_boundaries' packages/session-migration/src/operation.rs \
  || ! rg -q 'legacy_session_migrates_across_real_attach_and_send_ipc' packages/server/src/lib.rs; then
  echo "Session migration observability violation: migration progress regression must assert every stage." >&2
  violations=1
fi

if ! rg -q 'pub struct SessionOpenOperationId' packages/session/models/src/lib.rs \
  || ! rg -q 'pub enum SessionMigrationStage' packages/session/models/src/lib.rs \
  || ! rg -q 'pub enum SessionMigrationProgressUnit' packages/session/models/src/lib.rs \
  || ! rg -q 'pub enum SessionOpenFailureKind' packages/session/models/src/lib.rs \
  || ! rg -q 'pub enum SessionOpenTerminalOutcome' packages/session/models/src/lib.rs \
  || ! rg -q 'pub struct SessionOpenOperationSnapshot' packages/session/models/src/lib.rs \
  || ! rg -q 'session_open_operation_models_round_trip_and_preserve_semantics' packages/session/models/src/lib.rs; then
  echo "Session migration progress violation: operation identity, ordered stages, natural units, structured failures, terminal outcomes, snapshots, and model tests must remain explicit." >&2
  violations=1
fi

fixture_history_test="$(sed -n '/async fn bounded_history_has_exact_outcomes_for_every_migration_fixture(/,/^    }/p' packages/session/src/db.rs)"
for fixture in packages/session/fixtures/migrations/*.json packages/session/fixtures/migrations/*.jsonl; do
  fixture_name="$(basename "$fixture")"
  if ! grep -Fq "$fixture_name" <<<"$fixture_history_test"; then
    echo "Session fixture-corpus violation: $fixture_name must retain exact bounded-history classification coverage." >&2
    violations=1
  fi
done

fixture_discovery_test="$(sed -n '/async fn catalog_discovers_every_migration_fixture_without_mutation(/,/^    }/p' packages/session/src/lib.rs)"
for fixture in packages/session/fixtures/migrations/*.json packages/session/fixtures/migrations/*.jsonl; do
  fixture_name="$(basename "$fixture")"
  if ! grep -Fq "$fixture_name" <<<"$fixture_discovery_test"; then
    echo "Session fixture-corpus violation: $fixture_name must participate in byte-preserving catalog discovery coverage." >&2
    violations=1
  fi
done

if rg -q 'mixed_legacy_fixture_is_discoverable_migrates_and_preserves_bounded_history' packages/session/src/lib.rs; then
  echo "Session hard-cutover violation: retired mixed schema-32/schema-35 migration coverage was reintroduced." >&2
  violations=1
fi

migration_load_body="$(sed -n '/async fn ensure_session_loaded(/,/async fn refresh_summary_session(/p' packages/session/src/lib.rs)"
if ! grep -q 'session_load_gate(session_id)' <<<"$migration_load_body" \
  || ! grep -q 'let _guard = gate.lock().await' <<<"$migration_load_body" \
  || ! grep -q 'StorageMigrationRequired' <<<"$migration_load_body" \
  || ! rg -q 'start_or_join' packages/server/src/lib.rs \
  || ! rg -q 'migrate_owned_session_storage' packages/server/src/lib.rs; then
  echo "Session migration gate violation: detached migration must retain the per-session load gate and ownership compatibility rechecks." >&2
  violations=1
fi

migration_capability_body="$(sed -n '/pub async fn migrate_owned_session_storage(/,/^}/p' packages/server/src/session_migration_execution.rs)"
if ! rg -q "maintenance: &'a lease::SessionMaintenanceGuard" packages/server/src/session_migration_execution.rs \
  || ! rg -q "write: &'a lease::SessionWriteGuard" packages/server/src/session_migration_execution.rs \
  || ! grep -q 'validate_write_readiness().await' <<<"$migration_capability_body" \
  || ! grep -q 'transition_session_maintenance_to_lease' <<<"$migration_capability_body"; then
  echo "Session migration capability violation: maintenance and write guards must remain borrowed through migration and write-readiness validation before lease transition." >&2
  violations=1
fi

if ! rg -q 'runner_drains_streaming_session_progress_through_effect_results' packages/tui/src/effects.rs \
  || ! rg -q 'TuiEffectResult::SessionOpenProgress' packages/tui/src/chat_loop.rs \
  || sed -n '/pub enum Event {/,/^}/p' packages/ipc/src/lib.rs | grep -q 'SessionOpenProgress'; then
  echo "Session TUI progress-routing violation: migration progress must stream through typed TUI effect results, not daemon IPC events." >&2
  violations=1
fi

if ! rg -q 'preparation_recovers_retained_operation_after_transport_interruption' packages/client/src/lib.rs \
  || ! rg -q 'dropping_progress_receiver_stops_client_observation_cleanly' packages/client/src/lib.rs \
  || ! rg -q 'unrelated_events_remain_buffered_in_fifo_order_during_requests' packages/client/src/lib.rs \
  || ! rg -q 'only_ready_terminal_outcome_allows_writable_attach' packages/client/src/lib.rs; then
  echo "Session client-observer violation: reconnect, receiver-drop, FIFO buffering, and ready-only attach regression coverage must remain present." >&2
  violations=1
fi

if ! rg -q 'normal_open_does_not_decode_canonical_events' packages/session/src/lib.rs \
  || ! rg -q 'legacy_session_migrates_across_real_attach_and_send_ipc' packages/server/src/lib.rs; then
  echo "Session normal-load violation: healthy opens must remain decode-free and unresolved legacy preparation must fail closed." >&2
  violations=1
fi

if rg -q 'all_events(_strict|_degraded)?\(' <<<"$(sed -n '/async fn load_db_session_state(/,/^    }/p; /async fn load_persistent_session(/,/^    }/p' packages/session/src/lib.rs)"; then
  echo "Session normal-load violation: manager loading must not full-read or decode canonical event history." >&2
  violations=1
fi

model_context_body="$(sed -n '/pub async fn model_context_events(/,/^    }/p' packages/session/src/db.rs)"
if grep -Eq 'select\("events"\)|decode_session_event_degraded|reindex_model_context|migrate' <<<"$model_context_body" \
  || rg -q 'compatibility_model_context_events|model_context_events_query' packages/session/src/db.rs; then
  echo "Session model-context violation: normal reads must use the bounded projection and never replay, repair, or migrate canonical events." >&2
  violations=1
fi

if ! rg -q 'pub async fn reindex_model_context\(' packages/session/src/db.rs \
  || ! rg -q 'pub async fn reindex_session_projections\(' packages/session/src/db.rs \
  || ! sed -n '/pub async fn reindex_model_context(/,/^    }/p' packages/session/src/db.rs \
      | grep -q 'SessionMaintenanceGuard'; then
  echo "Session reindex capability violation: low-level reindex must require maintenance ownership." >&2
  violations=1
fi

if rg -q 'ensure_session_maintenance_daemon_compatibility\(\)\.await' packages/cli/src/lib.rs; then
  echo "Session maintenance domain violation: target maintenance must not globally reject unrelated daemon generations." >&2
  violations=1
fi

if ! rg -q 'storage_writer_epoch: Some\(bcode_session::lease::CURRENT_SESSION_STORAGE_WRITER_EPOCH\)' packages/server/src/lib.rs; then
  echo "Daemon storage identity violation: startup records must advertise the current writer epoch." >&2
  violations=1
fi

if ! rg -q 'CompactionPlanningPolicy::OverflowRecovery' packages/server/src/context_compaction.rs; then
  echo "Session compaction violation: overflow recovery must use its explicit planning policy." >&2
  violations=1
fi

if rg -q 'Option<CompactionPlan>' packages/server/src/context_compaction.rs; then
  echo "Session compaction violation: planners must return typed unavailability reasons." >&2
  violations=1
fi

if rg -n 'PluginSessionEvent|subscribe_session_events' \
  packages/plugin-sdk/src packages/tui/src/plugin_surface_host.rs plugins/code-review-plugin/src \
  --glob '*.rs' >/tmp/bcode-plugin-raw-session-events.txt; then
  echo "Session view architecture violation: plugin surfaces must consume complete semantic SessionView snapshots, not raw session events." >&2
  cat /tmp/bcode-plugin-raw-session-events.txt >&2
  violations=1
fi

if ! rg -q 'subscribe_session_view' packages/plugin-sdk/src/tui.rs \
  || ! rg -q 'SessionViewSnapshot' packages/plugin-sdk/src/tui.rs \
  || ! rg -q 'PluginTuiAction::OpenSession' packages/tui/src/plugin_surface_host.rs \
  || ! rg -q 'run_event_loop_with_input' packages/tui/src/code_review_launcher.rs packages/tui/src/runtime.rs \
  || ! rg -q 'SessionView::new\(\)' packages/tui/src/plugin_surface_host.rs \
  || ! rg -q 'PluginSessionViewUpdate::Snapshot' plugins/code-review-plugin/src/code_review_tui.rs; then
  echo "Session view architecture violation: generic plugin observation must be projected by SessionView and delivered as complete semantic snapshots." >&2
  violations=1
fi

exit "$violations"
