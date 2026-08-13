# Bedrock-hosted OpenAI models

Amazon Bedrock serves `OpenAI` models through the **Mantle** endpoint using the `OpenAI` Responses
API, not through `ConverseStream`. Bcode reaches them with a dedicated Bedrock transport.

## What is available

| Bedrock model id | Context | Surface |
| --- | --- | --- |
| `openai.gpt-5.6-sol` | 1M | Responses only |
| `openai.gpt-5.6-terra` | 1M | Responses only |
| `openai.gpt-5.6-luna` | 1M | Responses only |
| `openai.gpt-5.5` | 272K | Responses only |
| `openai.gpt-5.4` | 272K | Responses only |
| `openai.gpt-oss-120b` | 128K | Responses and Converse |
| `openai.gpt-oss-20b` | 128K | Responses and Converse |
| `openai.gpt-oss-safeguard-120b` | 128K | Converse only |
| `openai.gpt-oss-safeguard-20b` | 128K | Converse only |

The GPT-5.x tier is unreachable through `ConverseStream`; AWS reports `bedrock-runtime: No` for
those models. The `gpt-oss-*` pair supports both surfaces and Bcode prefers Mantle, matching the AWS
recommendation to use `bedrock-mantle` whenever possible. The Safeguard variants report
`Responses: No` and are content-moderation models rather than coding models, so they route through
Converse and are marked unsupported in the model catalog.

Per-region inference-profile prefixes (`us.`, `eu.`, `apac.`, `global.`) resolve to the same catalog
entries, so `us.openai.gpt-5.6-sol` carries the same metadata as `openai.gpt-5.6-sol`.

## Configure

No transport configuration is required. Routing follows the model: the catalog marks Mantle-only
`OpenAI` models with `api_surface = "responses"`, so selecting one sends the turn to the Mantle
Responses endpoint automatically, and `/model` / `/models` list all seven alongside the Converse
models that `ListFoundationModels` reports.

All you need is a Bedrock API key:

```sh
export AWS_BEARER_TOKEN_BEDROCK="<Bedrock long-term API key>"
export BCODE_BEDROCK_REGION=us-east-1   # optional; falls back to AWS_REGION, then us-east-1
```

Generate the API key from the Amazon Bedrock console. `AWS_BEARER_TOKEN_BEDROCK` is also accepted
through Bcode's provider auth flow as the `bearer_token` credential, which is preferred over an
environment variable.

The endpoint defaults to `https://bedrock-mantle.<region>.api.aws/openai/v1` and the adapter appends
`/responses`. Note that AWS documents this as `openai/v1/responses`, which is deliberately different
from the `v1/responses` path other models use on the responses endpoint. Override the base URL with
`BCODE_BEDROCK_MANTLE_BASE_URL` when needed; it must use HTTPS unless it points at a loopback host.

### Optional transport override

`BCODE_BEDROCK_TRANSPORT` pins every model to one surface. This is an override for testing or
unusual deployments — it is not required to use any supported model.

| Value | Surface |
| --- | --- |
| unset (default) | Per-model: Converse, Anthropic Messages, or `OpenAI` Responses, from the catalog |
| `bedrock_runtime`, `runtime` | `ConverseStream` |
| `mantle_anthropic`, `mantle` | Anthropic Messages on Mantle |
| `mantle_openai` | `OpenAI` Responses on Mantle, for every model |

When a Mantle transport is pinned, `BCODE_BEDROCK_MODEL` is required, because Mantle has no
control-plane listing to discover a default from.

## Supported features

The Responses surface supports capabilities that `ConverseStream` does not, and Bcode negotiates
them per transport:

* reasoning effort and provider-visible reasoning summaries
* JSON-schema structured output, including strict mode
* parallel tool calls
* prompt caching

Requesting these while a Converse surface is selected is still rejected, since those are genuine
Converse limitations rather than Bedrock-wide ones. Negotiation follows the selected model's surface,
so a Responses model accepts them with no configuration.

Provider-native conversation reuse is not used on this path: Bcode does not ask Mantle to persist
responses, and `store` is always sent as `false`.

## Verify access

```sh
AWS_BEARER_TOKEN_BEDROCK="<key>" \
  bcode-model-catalog verify --provider bedrock --id-pattern 'openai.gpt-5.6*'
```

Verification posts a tiny Responses request to the Mantle endpoint. Because Mantle exposes no model
listing for this surface, candidates come from catalog membership rather than provider discovery, so
`--discovered-only` yields nothing for this provider.
