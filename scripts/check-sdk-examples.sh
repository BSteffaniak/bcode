#!/usr/bin/env bash
set -euo pipefail

for example in \
    custom_provider \
    custom_tool \
    hooks_observability \
    in_memory_session \
    local_session_store \
    messages_history \
    minimal_text \
    multi_step_tools \
    structured_output \
    top_level_helpers
do
    echo "running bcode SDK example: ${example}"
    cargo run -p bcode --example "${example}"
done

echo "running bcode SDK example: scripted_provider"
cargo run -p bcode --features testing --example scripted_provider

echo "running bcode SDK example: typed_workflow"
cargo run -p bcode --features testing --example typed_workflow

echo "running bcode SDK example: provider_extension"
cargo run -p bcode --features openai-compatible-provider --example provider_extension

echo "running bcode SDK example: embedded_fake_provider"
cargo run -p bcode \
    --features embedded-plugins,static-bundled-fake-provider-plugin \
    --example embedded_fake_provider

echo "running bcode SDK example: daemon_client"
cargo run -p bcode --features daemon-client --example daemon_client

echo "all bcode SDK examples passed"
