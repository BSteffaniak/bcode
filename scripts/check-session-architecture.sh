#!/usr/bin/env bash
set -euo pipefail

violations=0

current_event_schema="$(sed -n 's/.*CURRENT_SESSION_EVENT_SCHEMA_VERSION: u16 = \([0-9][0-9]*\).*/\1/p' packages/session/models/src/lib.rs)"
fixture_baseline_schema="$(sed -n 's/Current fixture baseline schema: \*\*\([0-9][0-9]*\)\*\*.*/\1/p' packages/session/fixtures/migrations/README.md)"
if [[ -z "$current_event_schema" || "$current_event_schema" != "$fixture_baseline_schema" ]]; then
  echo "Session fixture-baseline violation: CURRENT_SESSION_EVENT_SCHEMA_VERSION ($current_event_schema) must match the documented fixture baseline ($fixture_baseline_schema)." >&2
  violations=1
fi

retired_interactive_kinds=(
  interactive_tool_request_created
  interactive_tool_request_resolved
)
for kind in "${retired_interactive_kinds[@]}"; do
  fixture="packages/session/fixtures/migrations/${kind//_/-}-v32.json"
  if ! rg -q "\"${kind}\"" packages/session/src/persisted.rs \
    || [[ ! -f "$fixture" ]] \
    || ! rg -q "\"${kind}\"" "$fixture"; then
    echo "Session persisted-compatibility violation: retired event ${kind} must retain its decode adapter and schema-32 fixture." >&2
    violations=1
  fi
done
for fixture in \
  packages/session/fixtures/migrations/interactive-tool-request-unresolved-v32.json \
  packages/session/fixtures/migrations/mixed-interactive-history-v32-v35.jsonl \
  packages/session/fixtures/migrations/unknown-old-event-kind-v32.json \
  packages/session/fixtures/migrations/unknown-future-event-kind-v38.json \
  packages/session/fixtures/migrations/future-schema-v39.json \
  packages/session/fixtures/migrations/malformed-json-v38.json \
  packages/session/fixtures/migrations/mismatched-session-id-v38.json \
  packages/session/fixtures/migrations/sequence-gap-v38.jsonl \
  packages/session/fixtures/migrations/tool-presentation-diff-v25.json; do
  if [[ ! -f "$fixture" ]]; then
    echo "Session persisted-compatibility violation: required historical fixture $fixture is missing." >&2
    violations=1
  fi
done
if ! rg -q 'mixed_schema_32_35_fixture_decodes_contiguously_without_reviving_interactions' packages/session/src/persisted.rs \
  || ! rg -q 'compatibility_failure_fixtures_have_exact_strict_and_degraded_outcomes' packages/session/src/persisted.rs; then
  echo "Session persisted-compatibility violation: mixed and failure-classification fixtures must remain regression-tested." >&2
  violations=1
fi
if ! rg -q 'preserve_retired_event_kind_as_legacy' packages/session/src/persisted.rs \
  || ! rg -q 'decodes_retired_interactive_tool_compatibility_fixtures' packages/session/src/persisted.rs \
  || ! rg -q 'retired_interactive_request_preserves_missing_optional_and_unknown_fields' packages/session/src/persisted.rs \
  || ! rg -q 'retired_interactive_event_reencodes_only_as_legacy_event' packages/session/src/persisted.rs; then
  echo "Session persisted-compatibility violation: retired interactive events must preserve raw payloads, remain decode-only, and stay fixture-tested." >&2
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

if ! rg -q 'create_verified_migration_backup' packages/session/src/lib.rs \
  || ! rg -q 'failed_migration_backup_prevents_every_storage_mutation' packages/session/src/lib.rs \
  || ! rg -q 'migration-backup.json' packages/session/src/lib.rs; then
  echo "Session migration-backup violation: automatic legacy migration must create and verify a retained backup before changing storage." >&2
  violations=1
fi

if ! rg -q 'CURRENT_SESSION_STORAGE_WRITER_EPOCH: u32 = 4' packages/session/src/lease.rs \
  || ! rg -q 'CURRENT_SESSION_STORAGE_WRITER_EPOCH: u32 = 4' packages/ipc/src/lib.rs \
  || ! rg -q 'session_event_schema_version' packages/ipc/src/lib.rs \
  || ! rg -q 'session_compatibility_state' packages/session/src/db.rs \
  || ! rg -q 'CompatibilityDegraded' packages/session/src/db.rs \
  || ! rg -q 'epoch_three_opaque_history_migrates_to_bounded_read_only_state' packages/session/src/db.rs \
  || ! rg -q 'migrated_opaque_session_health_is_degraded_and_attach_fails_closed' packages/session/src/lib.rs; then
  echo "Session compatibility-projection violation: epoch-4 writers must maintain bounded compatibility state and keep opaque sessions read-only." >&2
  violations=1
fi

if rg -n 'decode_session_event_degraded' packages/session/src/db.rs \
  >/tmp/bcode-lossy-session-read-violations.txt; then
  echo "Session history violation: DB-backed canonical/indexed reads must not silently discard undecodable rows." >&2
  cat /tmp/bcode-lossy-session-read-violations.txt >&2
  violations=1
fi

if ! rg -q 'pub enum CompatibleSessionEvent' packages/session/src/persisted.rs \
  || ! rg -q 'pub compatibility_issues: Vec<SessionEventCompatibilityIssue>' packages/session/models/src/lib.rs \
  || ! rg -q 'filter_map\(\|event\| event.issue\(\).cloned\(\)\)' packages/session/src/db.rs; then
  echo "Session compatibility-reporting violation: bounded history must distinguish known and opaque events and return structured compatibility issues." >&2
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
  | rg -v 'packages/session/src/legacy_storage\.rs' \
  >/tmp/bcode-historical-session-root-violations.txt; then
  echo "Session historical-root violation: only legacy_storage.rs may recognize the removed epoch root." >&2
  cat /tmp/bcode-historical-session-root-violations.txt >&2
  violations=1
fi

if ! rg -q 'session_dir_path\(root, session_id\)\.join\("session\.db"\)' packages/session/src/db.rs; then
  echo "Session path violation: session_db_path must remain root/<session-id>/session.db." >&2
  violations=1
fi

if rg -n '\*\.events|sessions/index/' docs/session-persistence-architecture.md >/tmp/bcode-stale-session-docs.txt; then
  echo "Session documentation violation: obsolete file-log/index architecture is documented as current." >&2
  cat /tmp/bcode-stale-session-docs.txt >&2
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
  | rg -v 'packages/session/src/(lib|index|reader|migration|semantic_migration|event_migration|legacy_storage|legacy_stream_cleanup|derived|db|lease|repair)\.rs' \
  >/tmp/bcode-session-fs-violations.txt; then
  echo "Session persistence architecture violation: direct filesystem access outside approved store modules." >&2
  cat /tmp/bcode-session-fs-violations.txt >&2
  violations=1
fi

if ! rg -q "mod actor;" packages/session/src/lib.rs; then
  echo "Session module split violation: actor module must remain split from lib.rs." >&2
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
  rg -n '\bSessionDb::open_turso_in_root(_observed)?' packages/session/src/{actor,lib}.rs \
    | awk -F: '$1 !~ /lib.rs/ || $2 < 3700' \
    || true
)"
if [[ -n "$normal_session_open_violations" ]]; then
  echo "Session open-mode violation: production session paths must use explicit existing/runtime/initialize/maintenance opens." >&2
  printf '%s\n' "$normal_session_open_violations" >&2
  violations=1
fi

if ! rg -q 'CREATE TABLE IF NOT EXISTS artifact_references' packages/session/src/db.rs \
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

if ! rg -q 'SessionEventKind::ModelTurnStarted.*=> "model_turn_started"' packages/session/src/db.rs \
  || ! rg -q 'SessionEventKind::ModelTurnFinished.*=> "model_turn_finished"' packages/session/src/db.rs; then
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
  || ! rg -q 'pub async fn migrate_turso_in_root' packages/session/src/db.rs; then
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

if rg -n 'open_turso_in_root\(session_id, root\)' packages/session/src/repair.rs >/tmp/bcode-repair-mutating-open-violations.txt; then
  echo "Session repair violation: doctor/validation paths must use existing non-migrating opens." >&2
  cat /tmp/bcode-repair-mutating-open-violations.txt >&2
  violations=1
fi

if ! rg -q 'let tx = db\.db\.begin_transaction\(\)\.await' packages/session/src/db.rs \
  || ! rg -q 'run_session_migrations\(&\*tx\)' packages/session/src/db.rs \
  || ! rg -q 'migrate_session_storage\(&\*tx, session_id, &metrics, progress\.as_ref\(\)\)' packages/session/src/db.rs \
  || ! rg -q 'set_storage_writer_contract\(db, CURRENT_SESSION_STORAGE_WRITER_EPOCH\)' packages/session/src/db.rs; then
  echo "Session migration violation: schema migration, projection replay, and writer-epoch update must share explicit migration transaction." >&2
  violations=1
fi

if ! sed -n '/async fn session_export(/,/^}/p' packages/cli/src/lib.rs \
    | grep -q 'session_export_events_from_root' \
  || ! sed -n '/async fn session_export_events_from_root(/,/^}/p' packages/cli/src/lib.rs \
    | grep -q 'open_existing_turso_in_root' \
  || ! sed -n '/async fn session_export_events_from_root(/,/^}/p' packages/cli/src/lib.rs \
    | grep -q 'all_events_strict' \
  || ! rg -q 'explicit_export_reads_legacy_stream_history_without_migration' packages/cli/src/lib.rs; then
  echo "Session export violation: pre-cutover export must read legacy canonical history explicitly without normal runtime loading or migration." >&2
  violations=1
fi

if ! rg -q 'storage_compatibility\(\)' packages/session/src/lib.rs \
  || ! rg -q 'load_gates: BTreeMap<SessionId, Arc<Mutex<\(\)>>>' packages/session/src/lib.rs \
  || ! rg -q 'migrate_legacy_session_for_load' packages/session/src/lib.rs \
  || ! rg -q 'acquire_session_maintenance_guard\(root, session_id\)' packages/session/src/lib.rs \
  || ! rg -q 'transition_session_maintenance_to_lease' packages/session/src/lib.rs; then
  echo "Session normal-load violation: manager first load must classify storage, serialize per session, and safely migrate known legacy storage under exclusive maintenance ownership." >&2
  violations=1
fi

if rg -q 'KnownLegacy \{ writer_epoch \} => Err\(SessionError::StorageMigrationRequired' packages/session/src/lib.rs; then
  echo "Session normal-load violation: recognized legacy storage must attempt guarded migration rather than fail before ownership acquisition." >&2
  violations=1
fi

if ! rg -q 'explicit_reindex_accepts_retired_interactive_events_as_inert_history' packages/session/src/db.rs \
  || ! sed -n '/async fn explicit_reindex_accepts_retired_interactive_events_as_inert_history(/,/^    }/p' packages/session/src/db.rs | grep -q 'checkpoint: canonical_tail' \
  || ! rg -q 'failed_explicit_migration_preserves_projection_and_writer_contract' packages/session/src/db.rs \
  || ! sed -n '/async fn failed_explicit_migration_preserves_projection_and_writer_contract(/,/^    }/p' packages/session/src/db.rs | grep -q 'failed migration must preserve every session storage file byte-for-byte'; then
  echo "Session migration regression violation: known-legacy reindex must reach the queried canonical tail and failed migration must preserve complete storage bytes." >&2
  violations=1
fi

if ! rg -q 'CURRENT_PROTOCOL_VERSION: u16 = 13' packages/ipc/src/lib.rs \
  || ! rg -q 'PrepareSessionOpen' packages/ipc/src/lib.rs \
  || ! rg -q 'WaitSessionOpenProgress' packages/ipc/src/lib.rs \
  || ! rg -q 'SessionOpenPrepared' packages/ipc/src/lib.rs \
  || ! rg -q 'session_open_preparation_requests_and_response_round_trip' packages/ipc/src/lib.rs \
  || ! rg -q 'session_open_wait_returns_newer_terminal_or_timeout_snapshot' packages/server/src/lib.rs \
  || ! rg -q 'session_open_operation_not_found' packages/server/src/lib.rs \
  || ! rg -q 'prepare_session_open_until_terminal' packages/client/src/lib.rs; then
  echo "Session migration IPC violation: protocol-v13 prepare/wait routing, bounded revision waits, exact operation errors, codec coverage, and client APIs must remain present." >&2
  violations=1
fi

if ! rg -q 'canonical_row_count_and_tail' packages/session/src/db.rs \
  || ! rg -q 'expected_backup_bytes' packages/session/src/lib.rs \
  || ! rg -q 'assert_successful_migration_progress' packages/session/src/lib.rs \
  || ! rg -q 'detached_preparation_reports_structured_backup_failure_without_mutation' packages/session/src/lib.rs \
  || ! rg -q 'publish_backup_path' packages/session/src/migration_operation.rs \
  || ! rg -q 'completed % 100 == 0' packages/session/src/db.rs \
  || ! rg -q 'MIGRATION_PROGRESS_BYTE_INTERVAL' packages/session/src/lib.rs; then
  echo "Session migration progress violation: ordered/throttled success, failure, backup, byte, decode, and replay progress coverage must remain intact." >&2
  violations=1
fi

if ! rg -q 'pub async fn prepare_session_open' packages/session/src/lib.rs \
  || ! rg -q 'migration_operations: migration_operation::SessionMigrationOperations' packages/session/src/lib.rs \
  || ! rg -q 'concurrent_preparation_joins_one_detached_legacy_migration' packages/session/src/lib.rs \
  || ! rg -q 'current_session_preparation_is_immediately_ready_without_operation' packages/session/src/lib.rs \
  || ! rg -q 'concurrent_starts_join_one_running_operation' packages/session/src/migration_operation.rs \
  || ! rg -q 'pruning_is_bounded_and_never_removes_running_operations' packages/session/src/migration_operation.rs; then
  echo "Session migration operation violation: production preparation, one-per-session joining, reconnectable snapshots, bounded retention, and current-session bypass must remain covered." >&2
  violations=1
fi

if ! rg -q 'project_materialized_event_without_checkpoints\(db, event\)' packages/session/src/db.rs \
  || ! rg -q 'project_materialized_checkpoints_at_tail\(db, tail\)' packages/session/src/db.rs \
  || ! rg -q 'BCODE_MIGRATION_BENCHMARK_PROFILE' packages/session/src/lib.rs; then
  echo "Session migration replay violation: migration must retain tail-only base checkpoint writes and focused generated profiling." >&2
  violations=1
fi

if ! rg -q 'benchmark_generated_legacy_session_migrations' packages/session/src/lib.rs \
  || ! rg -q 'generated_migration_benchmark_store_is_contiguous_and_legacy' packages/session/src/lib.rs \
  || ! sed -n '/const fn event_count(self)/,/^        }/p' packages/session/src/lib.rs | grep -q '50_000' \
  || ! sed -n '/const fn event_count(self)/,/^        }/p' packages/session/src/lib.rs | grep -q '5_000' \
  || ! sed -n '/const fn event_count(self)/,/^        }/p' packages/session/src/lib.rs | grep -q '100'; then
  echo "Session migration benchmark violation: deterministic small, medium, and 50k generated legacy stores must remain available without private content." >&2
  violations=1
fi

backup_source="$(sed -n '/const MIGRATION_BACKUP_BUFFER_BYTES/,/fn record_ensure_loaded_duration/p' packages/session/src/lib.rs)"
if grep -Eq 'fs::read\(&source|fs::read\(&destination' <<<"$backup_source" \
  || ! grep -q 'spawn_blocking' <<<"$backup_source" \
  || ! grep -q 'BufReader::with_capacity' <<<"$backup_source" \
  || ! grep -q 'BufWriter::with_capacity' <<<"$backup_source" \
  || ! grep -q 'Sha256::new' <<<"$backup_source" \
  || ! grep -q 'create_new(true)' <<<"$backup_source" \
  || ! grep -q 'remove_dir_all(destination)' <<<"$backup_source"; then
  echo "Session migration backup violation: backups must remain streaming, bounded, hash-verified, conflict-safe, cleanup-safe, and off Tokio workers." >&2
  violations=1
fi
if ! rg -q 'streaming_migration_backup_handles_nested_empty_and_large_files' packages/session/src/lib.rs \
  || ! rg -q 'streaming_migration_backup_refuses_conflicts_and_cleans_failed_copy' packages/session/src/lib.rs \
  || ! rg -q 'migration_backup_faults_are_deterministic_and_cleanup_partial_output' packages/session/src/lib.rs \
  || ! rg -q 'detached_preparation_migrates_legacy_storage_before_exclusive_load' packages/session/src/lib.rs \
  || ! rg -q 'failed_migration_backup_prevents_every_storage_mutation' packages/session/src/lib.rs; then
  echo "Session migration backup violation: streaming, retained-success, conflict, cleanup, and mutation-fence regressions must remain covered." >&2
  violations=1
fi

migration_metric_sources="$(sed -n '/fn create_verified_migration_backup(/,/^}/p; /async fn migrate_legacy_session_for_load(/,/^    }/p; /async fn migrate_owned_legacy_storage(/,/^    }/p' packages/session/src/lib.rs; sed -n '/pub async fn migrate_turso_in_root_observed(/,/^    }/p; /async fn migrate_session_storage(/,/^}/p; /async fn rebuild_migration_projections(/,/^}/p; /async fn validate_migrated_storage(/,/^}/p; /async fn project_migration_event(/,/^}/p' packages/session/src/db.rs)"
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
if ! sed -n '/async fn mixed_legacy_fixture_is_discoverable_migrates_and_preserves_bounded_history(/,/^    }/p' packages/session/src/lib.rs \
  | grep -q 'missing migration stage metric'; then
  echo "Session migration observability violation: production migration regression must assert every stage metric." >&2
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

if ! rg -q 'mixed_legacy_fixture_is_discoverable_migrates_and_preserves_bounded_history' packages/session/src/lib.rs; then
  echo "Session fixture-corpus violation: mixed schema-32/schema-35 history must retain store-level discovery, migration, bounded-history, and inert-runtime coverage." >&2
  violations=1
fi

migration_load_body="$(sed -n '/async fn ensure_session_loaded_with_progress(/,/async fn refresh_summary_session(/p' packages/session/src/lib.rs)"
if ! grep -q 'session_load_gate(session_id)' <<<"$migration_load_body" \
  || ! grep -q 'let _guard = gate.lock().await' <<<"$migration_load_body" \
  || ! grep -q 'if progress.is_none()' <<<"$migration_load_body" \
  || ! grep -q 'StorageMigrationRequired' <<<"$migration_load_body" \
  || [[ "$(grep -c 'storage_compatibility' <<<"$migration_load_body")" -lt 2 ]] \
  || ! grep -q 'acquire_maintenance_session_write_lock' <<<"$migration_load_body"; then
  echo "Session migration gate violation: detached migration must retain the per-session load gate and ownership compatibility rechecks." >&2
  violations=1
fi

if ! rg -q "maintenance: &'a lease::SessionMaintenanceGuard" packages/session/src/lib.rs \
  || ! rg -q "write: &'a lease::SessionWriteGuard" packages/session/src/lib.rs \
  || ! grep -q 'validate_write_readiness().await' <<<"$migration_load_body" \
  || ! grep -q 'transition_session_maintenance_to_lease' <<<"$migration_load_body"; then
  echo "Session migration capability violation: maintenance and write guards must remain borrowed through migration and write-readiness validation before lease transition." >&2
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
  || ! rg -q 'migrated_opaque_session_health_is_degraded_and_attach_fails_closed' packages/session/src/lib.rs; then
  echo "Session normal-load violation: healthy and degraded manager opens must retain decode-free regression coverage." >&2
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

exit "$violations"
