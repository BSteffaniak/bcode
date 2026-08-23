#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

fail() {
    printf 'workflow-view architecture check failed: %s\n' "$1" >&2
    exit 1
}

for manifest in packages/workflow-view/Cargo.toml packages/workflow-view/models/Cargo.toml; do
    [[ -f "$manifest" ]] || fail "missing $manifest"
done

if rg -n 'bcode_ipc|bcode_workflow_store|bmux|ratatui|crossterm' \
    packages/workflow-view/Cargo.toml packages/workflow-view/models/Cargo.toml \
    packages/workflow-view/src packages/workflow-view/models/src; then
    fail 'portable workflow projection references IPC, persistence, or terminal types'
fi

if rg -n 'bcode_ipc|bcode_workflow_store|bmux|ratatui|crossterm|Terminal[<(]|Frame[<(]|\bRect\b|KeyCode|MouseEvent' \
    packages/workflow-view/models/src; then
    fail 'public portable workflow contracts contain frontend or implementation types'
fi

if rg -n 'bcode_session_models::SessionEvent|WorkflowEventKind|canonical_events|event_log' \
    plugins/workflow-plugin/src/tui.rs; then
    fail 'workflow TUI reinterprets raw workflow or session events'
fi

if rg -n 'bcode_workflow_view\s*=' packages/workflow-view/models/Cargo.toml; then
    fail 'workflow-view models depends on its parent implementation crate'
fi

rg -q 'name\s*=\s*"bcode_workflow_view"' packages/workflow-view/Cargo.toml \
    || fail 'workflow-view crate has the wrong domain identity'
rg -q 'name\s*=\s*"bcode_workflow_view_models"' packages/workflow-view/models/Cargo.toml \
    || fail 'workflow-view models crate has the wrong domain identity'
rg -q 'bcode_workflow_view_models\s*=\s*\{ workspace = true \}' packages/workflow-view/Cargo.toml \
    || fail 'workflow-view does not depend on its portable model contract through the workspace'

rg -q 'pub const WORKFLOW_VIEW_VERSION: u32 = [2-9][0-9]*;' \
    packages/workflow-view/models/src/lib.rs \
    || fail 'workflow-view contract version was not advanced for the workspace redesign'
rg -q 'validate_version' packages/workflow-view/models/src/lib.rs \
    || fail 'workflow-view contracts do not reject unsupported future versions'
rg -q 'WorkflowActionTarget' packages/workflow-view/models/src/lib.rs \
    || fail 'workflow-view actions lack typed stable target identities'
rg -q 'Presentation fields cannot authorize execution' packages/workflow-view/models/src/lib.rs \
    || fail 'workflow mutation approval presentation lost its authorization-neutral boundary'
rg -q 'WorkflowLiveEventDisposition' packages/workflow-view/models/src/lib.rs \
    || fail 'workflow live notifications lack explicit duplicate/gap/version semantics'
rg -q 'workflow_run_status_is_terminal' plugins/workflow-plugin/src/tui.rs \
    || fail 'workflow TUI lacks stable terminal-state protection'
rg -q 'action_is_current' plugins/workflow-plugin/src/tui.rs \
    || fail 'workflow TUI does not revalidate exact portable action targets'
rg -q 'const CATALOG_PAGE_SIZE: usize = 100;' packages/tui/src/plugin_surface_host.rs \
    || fail 'workflow catalog host path lacks a fixed bounded page size'
rg -q 'const RUN_DETAIL_LIMIT: usize = 1_000;' packages/tui/src/plugin_surface_host.rs \
    || fail 'workflow selected-detail host path lacks a fixed collection bound'
rg -q 'WorkflowRunWatchEvent::ResyncRequired' packages/tui/src/plugin_surface_host.rs \
    || fail 'workflow host lacks explicit bounded resynchronization handling'

if rg -n 'discover_workflow_packages|workflow_manifest_paths|WorkflowPackageDiscoverySnapshot' \
    packages/cli/src plugins/workflow-plugin/src; then
    fail 'workflow discovery is duplicated in CLI or workflow TUI ownership'
fi

if rg -n 'bmux|ratatui|crossterm|Terminal[<(]|Frame[<(]|\bRect\b|KeyCode|MouseEvent' \
    packages/workflow/src/lib.rs | rg 'WorkflowLaunch|LaunchCatalog' >/dev/null; then
    fail 'portable workflow launch contracts contain terminal implementation types'
fi

rg -q 'name\s*=\s*"bcode_workflow_discovery"' packages/workflow-discovery/Cargo.toml \
    || fail 'workflow discovery lacks a domain-owned package'
rg -q 'workflow_launch_catalog' packages/client/src/lib.rs \
    || fail 'client lacks portable workflow launch-catalog routing'
rg -q 'WorkflowLaunchCatalog' packages/ipc/src/lib.rs \
    || fail 'IPC lacks portable workflow launch-catalog contracts'
rg -q 'discover_workflows' packages/server/src/workflow_operations.rs \
    || fail 'application operations do not own workflow discovery orchestration'
rg -q 'SessionViewSnapshot' plugins/workflow-plugin/src/tui.rs \
    || fail 'workflow session drill-down does not consume renderer-neutral session snapshots'
rg -q 'subscribe_session_view' plugins/workflow-plugin/src/tui.rs \
    || fail 'workflow session drill-down does not use the bounded plugin host subscription'
rg -q 'workflow_session_projection_request' plugins/workflow-plugin/src/tui.rs \
    || fail 'workflow session drill-down lacks a fixed bounded projection request'
rg -q 'const WORKFLOW_LAUNCH_CATALOG_PAGE_SIZE: usize = 100;' plugins/workflow-plugin/src/tui.rs \
    || fail 'workflow launch catalog lacks a fixed renderer request bound'
rg -q 'fn workflow_graph_layout' plugins/workflow-plugin/src/tui.rs \
    || fail 'workflow graph geometry is not renderer-owned'
if rg -n 'WorkflowGraphLayout|WorkflowNodeCard|Rect|Point' packages/workflow/src/lib.rs \
    packages/workflow-view/models/src/lib.rs | rg 'WorkflowLaunch|WorkflowRunView|WorkflowNodeView' >/dev/null; then
    fail 'portable workflow contracts contain renderer graph geometry'
fi

printf 'workflow-view architecture check passed\n'
