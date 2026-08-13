#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

if rg -n 'bedrock_structured_output_unsupported.*Converse does not provide' plugins/bedrock-provider-plugin/src/lib.rs; then
  echo "Bedrock Converse must not be declared universally unsupported for structured output" >&2
  exit 1
fi

if rg -n 'fn strict_openai_schema[\s\S]*fn normalize' plugins/openai-compatible-provider-plugin/src/lib.rs; then
  echo "provider plugins must use bcode_model_schema rather than private recursive normalizers" >&2
  exit 1
fi

if rg -n '(bedrock|openai).*(structured_output|tool_schema_mode)|(structured_output|tool_schema_mode).*(bedrock|openai)' packages/server/src packages/model/src \
  | grep -vE ':[0-9]+:.*(test|Some\()'; then
  echo "host/model contracts must not branch on provider identity for structured output" >&2
  exit 1
fi

if ! rg -q 'SyntheticStructuredOutput' packages/model-provider-runtime/src/lib.rs; then
  echo "generic provider-local structured-output emulation helper is missing" >&2
  exit 1
fi

if rg -n 'strict: bool' packages/model/src/lib.rs | rg -B5 -A2 'ToolDefinition'; then
  echo "strict tool policy must remain request-level rather than ToolDefinition metadata" >&2
  exit 1
fi

echo "structured output architecture checks passed"
