#!/usr/bin/env bash
set -euo pipefail

fail() {
    printf 'Session derivation architecture violation: %s\n' "$1" >&2
    exit 1
}

if rg -n 'Request::(ForkSession|CloneSession)|ResponsePayload::SessionForked|SessionFork(Result|Summary|Kind)|SessionEventKind::SessionForked' packages plugins --glob '*.rs'; then
    fail 'legacy current-runtime fork/clone contracts remain'
fi

if rg -n 'BuiltinSlashCommand \{ name: "(fork|clone)" \}|command: "/(fork|clone)"' packages/tui/src --glob '*.rs'; then
    fail 'fork/clone remain hardcoded in TUI slash discovery'
fi

if rg -n 'session\.fork|session\.clone' packages/command packages/tui --glob '*.rs'; then
    fail 'fork/clone remain hardcoded in host command registries'
fi

if rg -n 'bcode_session\s*=' plugins/session-derivation-plugin/Cargo.toml; then
    fail 'session derivation plugin depends on the session implementation crate'
fi

if rg -n 'session_history\(' packages/session/src/derivation.rs plugins/session-derivation-plugin --glob '*.rs'; then
    fail 'derivation uses an unbounded full-history API'
fi

if rg -n 'bcode_(ipc|server|tui)|SessionDb|session_db_path|\.derivation-staging' \
  packages/plugin-sdk/src/lib.rs packages/command/src/lib.rs plugins/session-derivation-plugin/src/lib.rs; then
    fail 'portable command/derivation contracts contain daemon, persistence, or TUI implementation types'
fi

printf 'session derivation architecture checks passed\n'
