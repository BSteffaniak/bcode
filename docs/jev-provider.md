# TypeSafe Jev judgement provider (in progress)

Jev implements `bcode.judgement-model-provider/v1`, not `bcode.model-provider/v1–v3`. It cannot act as a session chat, text-generating, or tool-calling model. A future caller can use `bcode_client::BcodeClient::judge(provider_plugin_id, auth_profile, request)` with `provider_plugin_id = "bcode.jev"` and an explicit provider-owned auth profile; the request contains a model ID, state and named Choice, Score or YesNo questions. The client does not pass secrets. The server resolves the selected credential through `bcode_provider_auth::resolve_explicit_profile_context`; the Jev plugin reads its `api_key` credential and calls `GET /v1/models` and `POST /v1/systemone` at `https://api.typesafe.ai` by default. Its `base_url` provider setting can point to a loopback test HTTP endpoint. A provider selection without this service or a verified auth owner fails closed. See [TypeSafe API](https://docs.typesafe.ai/api.md) and [models](https://docs.typesafe.ai/models.md).

For a client already connected to the matching Bcode daemon, a non-conversational call looks like:

```rust,no_run
use std::collections::BTreeMap;
use bcode_model::judgement::{Question, Request, State};

# async fn example(client: &bcode_client::BcodeClient) -> Result<(), bcode_client::ClientError> {
let result = client.judge(
    "bcode.jev".into(),
    "my-jev-auth-profile".into(),
    Request {
        model_id: "jev-latest".into(),
        state: State::Text("Customer asks for a refund".into()),
        questions: BTreeMap::from([(
            "refund_requested".into(),
            Question::YesNo { instructions: "Does the customer request a refund?".into() },
        )]),
    },
).await?;
let probability = &result.answers["refund_requested"];
# let _ = probability;
# Ok(())
# }
```

The auth profile must be explicitly configured for and owned by `bcode.jev` with an `api_key` credential; keep the credential in an approved secret backend, not the judgement request. The plugin does not retry `POST /v1/systemone`: after an ambiguous transport failure it cannot establish whether the request was evaluated. Client disconnect cancels pending work; application discovery and judgement have bounded timeouts and upstream bodies are not exposed in public errors.

The Jev plugin is included in the static bundle behind `static-bundled-jev-provider-plugin` and is independently disableable via normal Bcode plugin selection. The bundled-plugin feature is also part of the default `static-bundled-plugins` feature set. Provider-owned auth profile `settings.base_url` (or selected provider `model.settings.base_url`) sets the Jev endpoint, with the selected provider setting taking precedence; production endpoints must use HTTPS and only `127.0.0.1` may use HTTP for tests. The `jev-latest` and `jev-preview` aliases are documented by TypeSafe, but they may resolve to newer model versions; pin a versioned ID for calibrated workflows. Jev answers are typed, not assistant text. Goal/loop integration is out of scope.

**Status:** offline application integration verified, live vendor verification not performed. Fake-loopback Jev tests cover authenticated discovery/judgement, oversized discovery and secret-safe upstream failure, plus answer conversion and pending-work host cancellation; a held-open discovery socket closes after cancellation and runtime teardown. A fake judgement provider unit test, client→daemon→fake-provider integration test, and authenticated client→daemon→Jev loopback HTTP integration test exist. No authorized live vendor test has run: the temporary credential is unavailable via an approved ephemeral channel. Do not copy or save temporary API keys in config, fixtures, logs or documentation. Additional vendor response/transport cases should be checked against current TypeSafe behavior before treating this path as live-verified.
