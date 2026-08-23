#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

fail() {
  printf 'application operation parity check failed: %s\n' "$1" >&2
  exit 1
}

doc="docs/application-operation-parity.md"
[[ -f "$doc" ]] || fail "$doc is missing"

python3 - "$doc" <<'PY'
from pathlib import Path
import re
import sys

DOC = Path(sys.argv[1]).read_text(encoding="utf-8")


def enum_variants(path: str, enum_name: str) -> list[str]:
    source = Path(path).read_text(encoding="utf-8")
    match = re.search(rf"(?:pub\s+)?enum\s+{re.escape(enum_name)}\s*\{{", source)
    if match is None:
        raise SystemExit(f"application operation parity check failed: {enum_name} not found in {path}")

    index = match.end()
    body_start = index
    depth = 1
    while depth:
        if index >= len(source):
            raise SystemExit(
                f"application operation parity check failed: unterminated {enum_name} in {path}"
            )
        character = source[index]
        depth += int(character == "{") - int(character == "}")
        index += 1

    variants: list[str] = []
    depth = 0
    for line in source[body_start : index - 1].splitlines():
        code = line.split("//", 1)[0]
        stripped = code.strip()
        if depth == 0:
            variant = re.match(r"^([A-Z][A-Za-z0-9_]*)(?:\s*\{|\s*\(|,)", stripped)
            if variant is not None:
                variants.append(variant.group(1))
        depth += code.count("{") - code.count("}")
    return variants


inventories = [
    ("packages/session-view/models/src/lib.rs", "SessionViewAction"),
    ("packages/session-view/models/src/lib.rs", "SessionViewActionOutcome"),
    ("packages/tui/src/slash_registry.rs", "BuiltinCommandId"),
    ("packages/hyperchad/ui/src/context.rs", "PresentationAction"),
    ("packages/cli/src/lib.rs", "Commands"),
]

for path, enum_name in inventories:
    heading = f"### `{enum_name}`" if enum_name != "Commands" else "### Top-level `Commands`"
    start = DOC.find(heading)
    if start < 0:
        raise SystemExit(
            f"application operation parity check failed: missing {enum_name} inventory heading"
        )
    end = DOC.find("\n### ", start + len(heading))
    if end < 0:
        end = DOC.find("\n## ", start + len(heading))
    section = DOC[start : len(DOC) if end < 0 else end]
    missing = [variant for variant in enum_variants(path, enum_name) if f"`{variant}`" not in section]
    if missing:
        raise SystemExit(
            "application operation parity check failed: "
            f"{enum_name} variants missing from {sys.argv[1]}: {', '.join(missing)}"
        )

required_phrases = [
    "Shared application",
    "Frontend user state",
    "Frontend local",
    "Offline/lifecycle",
    "authorization",
    "cancellation",
    "JSON Lines",
    "unknown schema",
]
for phrase in required_phrases:
    if phrase.lower() not in DOC.lower():
        raise SystemExit(
            f"application operation parity check failed: required coverage phrase missing: {phrase}"
        )

classification_commands = [
    "`Onboard`", "`ArtifactId`", "`Server`", "`Session`", "`Web`", "`Plugin`",
    "`Theme`", "`Model`", "`Auth`", "`Login`", "`Permission`", "`Interaction`",
    "`Worktree`", "`Workflow`", "`RuntimeWork`", "`Cancel`", "`Attach`", "`Tui`", "`Send`",
]
classification_section = DOC.split("### Top-level CLI ownership classification", 1)
if len(classification_section) != 2:
    raise SystemExit("application operation parity check failed: top-level CLI ownership classification is missing")
classification_text = classification_section[1].split("## Maintenance rules", 1)[0]
for command in classification_commands:
    if f"| {command} |" not in classification_text:
        raise SystemExit(
            f"application operation parity check failed: top-level ownership classification missing: {command}"
        )

print("application operation parity inventory is complete for checked source enums")
PY

boundary_doc="docs/application-operation-boundary.md"
[[ -f "$boundary_doc" ]] || fail "$boundary_doc is missing"

for phrase in \
  'focused, server-owned operation modules' \
  'local IPC adapter owns' \
  'not durable resume protocols' \
  'Plugin workflows remain plugin-owned' \
  'future concrete adapter'; do
  if ! rg -Fqi "$phrase" "$boundary_doc"; then
    fail "$boundary_doc is missing required boundary coverage: $phrase"
  fi
done

if ! rg -Fq '[`application-operation-boundary.md`](application-operation-boundary.md)' "$doc"; then
  fail "$doc does not link the application operation boundary"
fi

if rg -n '^fn validate_client_(effective_config|plugin_selection|interaction_adapters)' \
  packages/server/src/lib.rs >/dev/null; then
  fail "client runtime-context validation leaked back into the IPC dispatcher"
fi

for operation in \
  validate_client_effective_config \
  validate_client_plugin_selection \
  validate_client_interaction_adapters; do
  if ! rg -Fq "pub fn $operation" packages/server/src/server_operations.rs; then
    fail "server operation boundary is missing $operation"
  fi
done

required_interaction_operations=(
  abort_tool_exchange
  add_permission_rule
  cancel_pending_permission
  cancel_pending_permissions_for_session
  client_supports_exchange
  decode_tool_exchange_resolution
  execute_tool_exchange
  has_exchange_consumer
  list_pending_tool_exchanges
  list_permissions
  remembered_skill_tool_decision
  register_pending_permission
  register_pending_tool_exchange
  request_tool_exchange
  resolve_exchanges_without_consumers
  resolve_permission
  resolve_permission_batch
  resolve_tool_exchange
  wait_for_tool_exchange_resolution
)
for operation in "${required_interaction_operations[@]}"; do
  if ! rg -q "^pub (async )?fn $operation\\b" packages/server/src/interaction_operations.rs; then
    fail "interaction operation boundary is missing $operation"
  fi
done

required_interaction_operation_tests=(
  interaction_operation_cancels_pending_exchange_without_transport
  interaction_operation_executes_complete_exchange_lifecycle_without_transport
  interaction_operation_fails_closed_without_compatible_consumer
  interaction_operation_rejects_conflicting_duplicate_exchange_without_transport
  interaction_operation_rejects_incompatible_resolving_client
  interaction_operation_terminal_outcome_is_stable_against_stale_resolution
  interaction_operation_terminalizes_exchange_when_last_consumer_detaches
  interaction_operations_batch_permission_resolution_is_latched_and_batch_scoped
  cancellation_while_waiting_for_permission_resolves_denied_without_tool_start
  bypass_authorization_preserves_structural_checks_without_permission_state
  permission_resolution_crosses_real_ipc_and_persists_resolution
  plugin_inventory_operations_run_without_transport_writing
  plugin_service_operations_match_real_ipc_results
  runtime_work_list_and_history_match_real_ipc_results
  runtime_work_operations_cancel_parent_and_node_without_transport_writing
  session_create_rename_and_delete_match_real_ipc_results
  session_operations_lifecycle_without_transport_writing
  server_question_exchange_completes_original_plugin_invocation
)
for test_name in "${required_interaction_operation_tests[@]}"; do
  if ! rg -q "(async )?fn $test_name\\b" packages/server/src/lib.rs; then
    fail "interaction operation boundary is missing behavioral proof $test_name"
  fi
done

if rg -n '^(async )?fn (abort_tool_exchange|client_supports_exchange|execute_tool_exchange|has_exchange_consumer|request_tool_exchange|wait_for_tool_exchange_resolution|handle_(list_permissions|resolve_permission|resolve_permission_batch|list_pending_tool_exchanges|resolve_tool_exchange|add_permission_rule)|resolve_exchanges_without_consumers|resolve_pending_permission|take_pending_permission_for_individual|resolve_permission_batch_operation|cancel_pending_permissions_for_session|register_pending_(permission|tool_exchange)|append_permission_(requested|resolved)_event|remembered_skill_tool_decision|next_permission_(batch_)?id)\b' \
  packages/server/src/lib.rs >/dev/null; then
  fail "permission or interaction application behavior leaked back into server-root transport code"
fi
if ! rg -Fq 'PendingPermissionBatchRegistration::allocate' packages/server/src/lib.rs; then
  fail "runtime permission batches bypass focused interaction lifecycle allocation"
fi

if rg -Fq 'serde_json::from_value(resolution_json)?' packages/server/src/lib.rs; then
  fail "tool-exchange resolution decoding leaked back into generic IPC dispatch"
fi

required_plugin_operations=(
  call_service
  invoke_service
  list_contributions
  list_services
  publish_event
  project_service_response
)
for operation in "${required_plugin_operations[@]}"; do
  if ! rg -q "^pub (async )?fn $operation\\b" packages/server/src/plugin_operations.rs; then
    fail "plugin operation boundary is missing $operation"
  fi
done

required_plugin_client_methods=(
  call_plugin_service
  invoke_plugin_service
  plugin_contributions
  plugin_services
  publish_plugin_event
)
for method in "${required_plugin_client_methods[@]}"; do
  if ! rg -q "pub async fn $method\\b" packages/client/src/lib.rs; then
    fail "typed client boundary is missing $method"
  fi
done

required_session_client_methods=(
  create_session_in_working_directory
  change_session_working_directory
  delete_session
  rename_session
  session_history
  session_history_around
  session_history_page
)
for method in "${required_session_client_methods[@]}"; do
  if ! rg -q "pub async fn $method\\b" packages/client/src/lib.rs; then
    fail "typed client boundary is missing $method"
  fi
done

required_runtime_work_client_methods=(
  cancel_runtime_work
  list_runtime_work
  runtime_work_history
  watch_runtime_work
)
for method in "${required_runtime_work_client_methods[@]}"; do
  if ! rg -q "pub async fn $method\\b" packages/client/src/lib.rs; then
    fail "typed client boundary is missing $method"
  fi
done

if rg -n '^async fn handle_(cancel_runtime_work|list_runtime_work|runtime_work_history)\b' \
  packages/server/src/lib.rs >/dev/null; then
  fail "runtime-work application behavior leaked back into response-writing helpers"
fi

if rg -n '^async fn handle_(list_plugin_services|list_plugin_contributions|invoke_plugin_service|call_plugin_service|publish_plugin_event|send_plugin_service_response)\b' \
  packages/server/src/lib.rs >/dev/null; then
  fail "plugin application behavior leaked back into response-writing helpers"
fi

raw_ipc_callers="$(
  rg -l 'bcode_ipc::Request|\bRequest::' packages --glob '*.rs' \
    | grep -Ev '^packages/(client|server|ipc|daemon-lifecycle)/|(^|/)tests?(/|\.rs$)' \
    || true
)"
if [[ -n "$raw_ipc_callers" ]]; then
  printf '%s\n' "$raw_ipc_callers" >&2
  fail "production callers outside IPC/client/server/lifecycle boundaries construct raw IPC requests"
fi
