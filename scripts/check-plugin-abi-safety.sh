#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

violations=0

if rg -n 'unsafe impl (Send|Sync) for (CommandRegistrar|AuthRegistrar)' \
  packages/plugin-sdk/src --glob '*.rs' >/tmp/bcode-plugin-scoped-registrars.txt; then
  echo "Plugin ABI safety violation: activation-scoped registrars must not be manually Send or Sync." >&2
  cat /tmp/bcode-plugin-scoped-registrars.txt >&2
  violations=1
fi

if rg -n '#!\[allow\(unsafe_op_in_unsafe_fn\)\]' packages/plugin-sdk/src packages/plugin/src \
  --glob '*.rs' >/tmp/bcode-plugin-unsafe-op-allow.txt; then
  echo "Plugin ABI safety violation: plugin boundary crates must not allow implicit unsafe operations." >&2
  cat /tmp/bcode-plugin-unsafe-op-allow.txt >&2
  violations=1
fi

if rg -n 'std::ptr::from_(mut|ref)\([^)]*(callback_state|cancellation)' packages/plugin/src \
  --glob '*.rs' >/tmp/bcode-plugin-stack-callback-state.txt; then
  echo "Plugin ABI safety violation: invocation callback state must cross the ABI as an opaque registered handle, not a stack pointer." >&2
  cat /tmp/bcode-plugin-stack-callback-state.txt >&2
  violations=1
fi

for crate in packages/plugin-sdk/src/lib.rs packages/plugin/src/lib.rs; do
  if ! rg -n '^#!\[deny\(unsafe_op_in_unsafe_fn\)\]$' "$crate" >/dev/null; then
    echo "Plugin ABI safety violation: $crate must deny unsafe operations outside explicit unsafe blocks." >&2
    violations=1
  fi
done

if [[ "$violations" -ne 0 ]]; then
  exit 1
fi

echo "plugin ABI safety guard passed"
