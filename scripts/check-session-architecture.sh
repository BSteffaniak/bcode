#!/usr/bin/env bash
set -euo pipefail

violations=0

if rg -n "handle\.state" packages/session/src/lib.rs >/tmp/bcode-session-actor-violations.txt; then
  echo "Session actor architecture violation: SessionHandle state must not be accessed directly." >&2
  cat /tmp/bcode-session-actor-violations.txt >&2
  violations=1
fi

if rg -n "std::fs|OpenOptions|fs::File|File::open|File::create" packages/session/src --glob '*.rs' \
  | rg -v 'packages/session/src/(lib|index|reader|migration|event_migration)\.rs' \
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

exit "$violations"
