#!/usr/bin/env bash
set -euo pipefail

# Guard normal session open/read/attach paths from accidental full event-log scans or repair rebuilds.
violations=$(rg -n \
  -g '*.rs' \
  'reader::read_events\(|crate::reader::read_events\(|repair_rebuild_all_from_event_log\(|rebuild_index\(' \
  packages/session/src/actor.rs packages/server/src packages/tui/src packages/client/src \
  || true)

if [[ -n "${violations}" ]]; then
  echo "normal-path full event-log scan/rebuild references found:" >&2
  echo "${violations}" >&2
  exit 1
fi
