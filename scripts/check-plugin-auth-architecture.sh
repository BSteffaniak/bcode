#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

violations=0

if rg -n '^\s*(bcode_(config|plugin|plugin_sdk|provider_auth|model)|sshenv_vault|reqwest|tokio)\s*=' \
  packages/provider-auth/models/Cargo.toml >/tmp/bcode-auth-contract-dependencies.txt; then
  echo "Auth architecture violation: portable auth contracts depend on host, vault, network, or implementation crates." >&2
  cat /tmp/bcode-auth-contract-dependencies.txt >&2
  violations=1
fi

if rg -n 'sshenv_vault|read_auth_vault|write_auth_vault|SshenvStore' \
  packages/provider-auth/models packages/plugin-sdk packages/plugin \
  --glob '*.rs' --glob 'Cargo.toml' >/tmp/bcode-auth-vault-custody.txt; then
  echo "Auth architecture violation: portable contracts or plugin registration own vault custody." >&2
  cat /tmp/bcode-auth-vault-custody.txt >&2
  violations=1
fi

web_search_production_source="$(sed '/^#\[cfg(test)\]/,$d' plugins/web-search-plugin/src/lib.rs)"
if rg -n '\.secrets\b|bcode\.web-search/exa/api_key' <<<"$web_search_production_source" \
  >/tmp/bcode-web-search-secret-map-bypass.txt; then
  echo "Auth architecture violation: web-search must use semantic invocation credentials instead of the raw secret map or host key format." >&2
  cat /tmp/bcode-web-search-secret-map-bypass.txt >&2
  violations=1
fi

if rg -n 'bcode_provider_auth\s*=|sshenv_vault\s*=' plugins/web-search-plugin/Cargo.toml \
  >/tmp/bcode-web-search-auth-custody.txt; then
  echo "Auth architecture violation: the Exa integration must use host-owned auth lifecycle instead of direct vault custody." >&2
  cat /tmp/bcode-web-search-auth-custody.txt >&2
  violations=1
fi

if rg -n 'bcode_provider_auth\s*=|sshenv_vault\s*=' plugins/model-plugin/Cargo.toml \
  >/tmp/bcode-model-auth-custody.txt; then
  echo "Auth architecture violation: model-plugin must use portable host auth operations instead of provider-auth or vault implementation crates." >&2
  cat /tmp/bcode-model-auth-custody.txt >&2
  violations=1
fi

if rg -n 'vault_path|inspect_auth_vault_security|vault_private_key_paths' \
  plugins/model-plugin --glob '*.rs' --glob '*.toml' \
  >/tmp/bcode-model-auth-inspection-bypass.txt; then
  echo "Auth architecture violation: model-plugin security inspection must be semantic and host-owned." >&2
  cat /tmp/bcode-model-auth-inspection-bypass.txt >&2
  violations=1
fi

openai_production_source="$(sed '/^#\[cfg(test)\]/,$d' plugins/openai-compatible-provider-plugin/src/lib.rs)"
if rg -n 'sshenv_vault|ProviderAuthStorageRef|vault_private_key_paths|SshenvStore' \
  plugins/openai-compatible-provider-plugin/Cargo.toml \
  >/tmp/bcode-openai-auth-custody.txt || \
  rg -n 'sshenv_vault|ProviderAuthStorageRef|vault_private_key_paths|SshenvStore' \
    <<<"$openai_production_source" >>/tmp/bcode-openai-auth-custody.txt; then
  echo "Auth architecture violation: OpenAI-compatible must use semantic host auth operations instead of direct vault custody." >&2
  cat /tmp/bcode-openai-auth-custody.txt >&2
  violations=1
fi

if rg -n 'sshenv_vault\s*=' plugins --glob 'Cargo.toml' \
  >/tmp/bcode-plugin-vault-dependencies.txt || \
  rg -n 'sshenv_vault::|vault_private_key_paths|SshenvStore|ProviderAuthStorageRef' \
    plugins --glob '*.rs' >>/tmp/bcode-plugin-vault-dependencies.txt; then
  echo "Auth architecture violation: ordinary plugins must not depend on or invoke vault implementation APIs." >&2
  cat /tmp/bcode-plugin-vault-dependencies.txt >&2
  violations=1
fi

if rg -n 'pub (api_key|access_token|refresh_token|id_token|secret|credential_value):' \
  packages/provider-auth/models packages/config/src packages/ipc/src \
  --glob '*.rs' >/tmp/bcode-auth-public-secret-fields.txt; then
  echo "Auth architecture violation: public auth/config/IPC contracts contain a plaintext credential field." >&2
  cat /tmp/bcode-auth-public-secret-fields.txt >&2
  violations=1
fi

if rg -n '(tracing::|println!|eprintln!|dbg!)[^\n]*(api_key|access_token|refresh_token|id_token|credential_value)' \
  packages/provider-auth packages/plugin-sdk packages/plugin packages/config/src packages/ipc/src plugins \
  --glob '*.rs' >/tmp/bcode-auth-secret-logging.txt; then
  echo "Auth architecture violation: auth credential fields appear in logging or debug output." >&2
  cat /tmp/bcode-auth-secret-logging.txt >&2
  violations=1
fi

if ! rg -n 'fn register_auth_providers' plugins/web-search-plugin/src/lib.rs >/dev/null ||
   ! rg -n 'fn register_auth_providers' plugins/openai-compatible-provider-plugin/src/lib.rs >/dev/null; then
  echo "Auth architecture violation: bundled Exa/OpenAI/xAI providers must remain plugin-registered." >&2
  violations=1
fi

if rg -n 'match\s+provider_id|provider_id\s*==\s*"(openai|xai|exa)"|matches!\([^\n]*provider_id[^\n]*"(openai|xai|exa)"' \
  packages/provider-auth packages/plugin/src packages/plugin-sdk/src \
  --glob '*.rs' >/tmp/bcode-auth-provider-id-routing.txt; then
  echo "Auth architecture violation: generic auth orchestration matches provider IDs instead of registered contracts." >&2
  cat /tmp/bcode-auth-provider-id-routing.txt >&2
  violations=1
fi

runtime_metadata="$({
  sed -n '/pub struct RuntimeAuthSubscriptions/,/pub fn runtime_auth_subscriptions_path/p' packages/config/src/lib.rs
})"
if grep -Eq 'pub (api_key|access_token|refresh_token|id_token|secret|credential_value):' <<<"$runtime_metadata"; then
  echo "Auth architecture violation: runtime auth metadata contains a plaintext credential field." >&2
  exit 1
fi

if ! grep -F 'pub map: BTreeMap<String, AuthCredentialMapping>' <<<"$runtime_metadata" >/dev/null ||
   ! grep -F 'pub owner_plugin_id:' <<<"$runtime_metadata" >/dev/null; then
  echo "Auth architecture violation: runtime auth metadata lost normalized credential mappings or plugin ownership." >&2
  violations=1
fi

if ! grep -F 'fn owned_ambient_auth_profile_hint' packages/cli/src/lib.rs >/dev/null ||
   ! grep -F 'AuthProviderProfileLookup::Unconfigured' packages/cli/src/lib.rs >/dev/null ||
   ! grep -F 'OwnershipUnverifiable' packages/provider-auth/src/lib.rs >/dev/null; then
  echo "Auth architecture violation: registered provider lifecycle lost source-aware hints, fresh enrollment status, or strict typed ownership." >&2
  violations=1
fi

if rg -n 'credentials\.(insert|get)\("(BCODE_|OPENAI_|XAI_|EXA_)' \
  packages/cli/src packages/provider-auth/src --glob '*.rs' \
  >/tmp/bcode-auth-storage-key-leak.txt; then
  echo "Auth architecture violation: generic auth lifecycle uses provider storage keys as canonical credential IDs." >&2
  cat /tmp/bcode-auth-storage-key-leak.txt >&2
  violations=1
fi

if [[ "$violations" -ne 0 ]]; then
  exit 1
fi

echo "dynamic plugin auth architecture guard passed"
