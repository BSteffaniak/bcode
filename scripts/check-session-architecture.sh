#!/usr/bin/env bash
set -euo pipefail

violations=0

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
  | rg -v 'packages/session/src/(lib|index|reader|migration|semantic_migration|event_migration|legacy_stream_cleanup|derived|db|lease|repair)\.rs' \
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

if ! rg -q 'validate_storage_writer_contract\(db\).*await' packages/session/src/db.rs \
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

if ! rg -q 'migrate_session_storage\(&\*\*db\.db\)' packages/session/src/db.rs \
  || ! rg -q 'set_storage_writer_contract\(&\*tx, CURRENT_SESSION_STORAGE_WRITER_EPOCH\)' packages/session/src/db.rs; then
  echo "Session migration violation: projection rebuild and writer-epoch update must share explicit migration transaction." >&2
  violations=1
fi

if ! rg -q 'pub async fn reindex_model_context\(' packages/session/src/db.rs \
  || ! sed -n '/pub async fn reindex_model_context(/,/^    }/p' packages/session/src/db.rs \
      | grep -q 'SessionMaintenanceGuard'; then
  echo "Session reindex capability violation: low-level reindex must require maintenance ownership." >&2
  violations=1
fi

if ! rg -q 'storage_writer_epoch: Some\(bcode_session::lease::CURRENT_SESSION_STORAGE_WRITER_EPOCH\)' packages/server/src/lib.rs \
  || ! rg -q 'ensure_daemon_storage_compatibility\(\)\.await' packages/server/src/lib.rs; then
  echo "Daemon storage fencing violation: startup records and startup checks must carry writer epoch." >&2
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
