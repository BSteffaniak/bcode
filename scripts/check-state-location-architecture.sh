#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${root}"

python3 - <<'PY'
from pathlib import Path
import re

config = Path("packages/config/src/lib.rs").read_text(encoding="utf-8")
cli = Path("packages/cli/src/lib.rs").read_text(encoding="utf-8")
ipc = Path("packages/ipc/src/lib.rs").read_text(encoding="utf-8")
client = Path("packages/client/src/lib.rs").read_text(encoding="utf-8")
server = Path("packages/server/src/lib.rs").read_text(encoding="utf-8")
catalog = Path("packages/server/src/session_catalog.rs").read_text(encoding="utf-8")
session_store = Path("packages/session/src/store.rs").read_text(encoding="utf-8")
lifecycle = Path("packages/daemon-lifecycle/src/lib.rs").read_text(encoding="utf-8")
relocation = Path("packages/session-migration/src/relocation.rs").read_text(encoding="utf-8")
invariants = Path("INVARIANTS.md").read_text(encoding="utf-8")

# Production sources that must not re-resolve the durable state root themselves.
# `bcode_config` owns resolution; every other domain receives it. Tests may still
# manipulate the environment directly, so `#[cfg(test)]` tails are excluded.
def production_source(path: Path) -> str:
    text = path.read_text(encoding="utf-8")
    marker = text.find("#[cfg(test)]")
    return text if marker == -1 else text[:marker]


state_env_readers = []
for path in list(Path("packages").rglob("*.rs")) + list(Path("plugins").rglob("*.rs")):
    if path.parts[:2] == ("packages", "config"):
        continue
    if "tests" in path.parts or path.name.endswith("_test.rs") or path.name == "tests.rs":
        continue
    source = production_source(path)
    for variable in ('"BCODE_STATE_DIR"', '"XDG_STATE_HOME"', '"BCODE_SESSION_STORE_DIR"'):
        if re.search(r"var(_os)?\(\s*" + re.escape(variable), source):
            state_env_readers.append(f"{path}: reads {variable}")

# Every attach handler must refuse an ambiguous session before it performs any
# activation or attach side effect. Checking only that the refusal helper exists
# somewhere is too weak: a new attach path can silently skip it, which is exactly
# how the projection-window path regressed. Assert per-handler ordering instead.
def attach_handlers_refuse_ambiguity_before_side_effects(source: str) -> bool:
    handlers = re.findall(
        r"^async fn (handle_attach_\w+)\(.*?^\}",
        source,
        re.MULTILINE | re.DOTALL,
    )
    if len(handlers) < 3:
        return False
    for name in handlers:
        body = re.search(
            r"^async fn " + name + r"\(.*?^\}",
            source,
            re.MULTILINE | re.DOTALL,
        ).group(0)
        refusal = body.find("ambiguous_session_location_response")
        if refusal == -1:
            return False
        # The refusal must precede runtime-work recovery, namespace activation, and
        # the attach call itself, so no side effect can precede authorization.
        for side_effect in (
            "recover_abandoned_session_runtime_work_best_effort",
            "try_activate_session_namespace",
        ):
            position = body.find(side_effect)
            if position != -1 and position < refusal:
                return False
    return True


required = {
    "state location invariants must remain cataloged":
        "**Daemon state locations are isolated.**" in invariants
        and "**Aggregated session discovery does not confer authority.**" in invariants
        and "Exactly one state location owns a session's canonical storage" in invariants
        and "no location is opened as authoritative until an explicit maintenance operation"
        in invariants,
    "bcode_config must own typed state location resolution":
        "pub struct StateLocation " in config
        and "pub struct StateLocationId" in config
        and "pub struct StateLocationSet" in config
        and "pub enum StateLocationProvenance" in config
        and "pub fn resolve_state_location_set_with_environment" in config,
    "state location identity must derive only from the canonical root":
        "pub fn from_canonical_root" in config,
    "state location resolution must fail closed rather than substitute a location":
        "pub enum StateLocationError" in config
        and "NotAbsolute" in config
        and "NotADirectory" in config
        and "NotWritable" in config
        and "UnknownProfile" in config,
    "declarative [state] configuration must exist and be documented":
        "pub struct StateConfig" in config
        and '#[config_doc(section = "state")]' in config
        and 'schema_section_doc::<StateConfig>(' in config
        and "fn write_state_toml(" in config,
    "canonical session root must stay resolved by one production entry point":
        "pub fn default_session_store_dir()" in config
        and "pub fn default_session_store_dir_with_environment" in config,
    "explicit command-line state selection must outrank the environment":
        "--state-root" in cli
        and "--state-profile" in cli
        and "push_process_state_location" in cli,
    # Phase 3: two state locations must never share one daemon.
    "daemon endpoints must be scoped by state location":
        "fn socket_scope_digest(user: &str, namespace: &str, state_location_id: &str)" in ipc
        and "pub fn state_location_id()" in ipc,
    # Phase 5: daemons are fingerprinted by runtime scope, so a session outside the
    # current scope starts its own daemon rather than being opened across roots.
    "daemon identity must include the config directory, not only the state root":
        "pub fn runtime_scope_id" in config
        and "default_config_dir()" in ipc
        and "runtime_scope_id(" in ipc,
    "spawned daemons must receive an explicit config directory":
        "config_home_for_daemon" in lifecycle
        and "XDG_CONFIG_HOME" in lifecycle,
    "daemon handshake must carry a state location identity":
        "pub state_location_id: Option<String>" in ipc
        and "state_location_id: Option<String>," in ipc,
    "client must refuse a daemon serving another state location":
        "state_location_id.as_deref() == Some(expected_state_location.as_str())" in client,
    "daemon must refuse a client resolving another state location":
        "fn validate_client_state_location" in server
        and "incompatible_state_location" in server,
    "unadvertised state location must be unverifiable rather than assumed local":
        "client did not advertise a state location" in server,
    "spawned daemons must receive an explicit state location":
        "BCODE_STATE_DIR_ENV" in lifecycle
        and "BCODE_SESSION_STORE_DIR_ENV" in lifecycle,
    # Phase 4: aggregated discovery must be read-only and must surface ambiguity.
    "aggregated discovery must be per-location and read-only":
        "CatalogSourcePlan::Native { location }" in catalog
        and "load_foreign_native_source" in catalog
        and "discover_readable_session_summaries" in catalog,
    "foreign-location discovery must exist as a bounded read-only primitive":
        "pub fn discover_readable_session_summaries" in session_store
        and "load_session_manifests" in session_store,
    "duplicate session IDs across locations must surface as ambiguity rather than merge":
        "fn mark_ambiguous_locations" in catalog
        and "ambiguous" in catalog
        and "fn ambiguous_location_ids" in catalog,
    "opening an ambiguous session must fail closed":
        "session_location_ambiguous" in server
        and "ambiguous_session_location_response" in server,
    "every attach path must refuse an ambiguous session before any side effect":
        attach_handlers_refuse_ambiguity_before_side_effects(server),
    # Phase 6: relocation must be explicit, ownership-fenced, verified, and source-authoritative.
    "relocation must be owned by the session-migration domain":
        "pub fn plan_session_relocation" in relocation
        and "pub fn relocate_sessions" in relocation,
    "relocation must be ownership-fenced rather than trusting its own plan":
        "acquire_session_maintenance_guard" in cli
        and "SessionRelocationOwnership::BlockedByOwner" in relocation,
    "relocation must verify copied bytes before publishing":
        "fn verify_copy" in relocation
        and "fn copy_and_hash" in relocation
        and "Sha256" in relocation,
    "relocation must publish atomically and unlink the source only afterward":
        "fs::rename(&staging, &destination_dir)" in relocation
        and 'abort_at_relocation_crash_boundary("before_source_unlink")' in relocation,
    "relocation must be crash-safe with prunable staging and a liveness-aware prune":
        "pub fn prune_relocation_staging" in relocation
        and "fn staging_is_live" in relocation
        and "RELOCATION_LOCK_FILE" in relocation
        and "try_lock()" in relocation
        and "fn prune_relocation_staging_command" in cli,
    "abandoned staging must never be pruned while a relocation still holds it":
        "live_staging_is_reported_rather_than_pruned" in relocation,
    "relocation must never merge a destination conflict":
        "SessionRelocationBlock::DestinationConflict" in relocation
        and "if destination_dir.exists()" in relocation,
    "relocation must retain pinned artifacts at the source":
        "retained_pinned_artifacts" in relocation
        and "fn copy_session_artifacts" in relocation,
    "relocation must not carry derived or coordination state across locations":
        "derived_state_and_coordination_state_are_not_carried_across" in relocation,
    "the state location inventory must be exposed as a read-only command":
        "StateCommand" in cli and "fn list_state_locations" in cli,
    "state root resolution must not be duplicated outside bcode_config":
        not state_env_readers,
}

failures = [message for message, passed in required.items() if not passed]
if failures:
    for failure in failures:
        print(f"state location architecture violation: {failure}")
    for reader in state_env_readers:
        print(f"  duplicate state root resolution: {reader}")
    raise SystemExit(1)
PY

echo "state location architecture guard passed"
