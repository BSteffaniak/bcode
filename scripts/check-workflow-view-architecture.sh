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

printf 'workflow-view architecture check passed\n'
