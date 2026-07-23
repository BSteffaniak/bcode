#!/usr/bin/env bash
set -euo pipefail

violations=0

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
if ! rg -q 'preserve_retired_event_kind_as_legacy' packages/session/src/persisted.rs \
  || ! rg -q 'decodes_retired_interactive_tool_compatibility_fixtures' packages/session/src/persisted.rs \
  || ! rg -q 'retired_interactive_request_preserves_missing_optional_and_unknown_fields' packages/session/src/persisted.rs \
  || ! rg -q 'retired_interactive_event_reencodes_only_as_legacy_event' packages/session/src/persisted.rs; then
  echo "Session persisted-compatibility violation: retired interactive events must preserve raw payloads, remain decode-only, and stay fixture-tested." >&2
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
  || ! rg -q 'migrate_session_storage\(&\*tx, session_id\)' packages/session/src/db.rs \
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
