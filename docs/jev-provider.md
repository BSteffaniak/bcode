# TypeSafe Jev judgement provider (in progress)

Jev implements `bcode.judgement-model-provider/v1`, not `bcode.model-provider/v1–v3`. It cannot act as a session chat, text-generating, or tool-calling model. A future caller can use `bcode_client::BcodeClient::judge(provider_plugin_id, auth_profile, request)` with `provider_plugin_id = "bcode.jev"` and an explicit provider-owned auth profile; the request contains a model ID, state and named Choice, Score or YesNo questions. The client does not pass secrets. The server resolves the selected credential through `bcode_provider_auth::resolve_explicit_profile_context`; the Jev plugin reads its `api_key` credential and calls `POST /api/v1/decide` at `https://jevtypesafeai.com` by default. The working decision API did not expose a usable model-list endpoint in live verification; the plugin advertises only `jev-latest` and the observed concrete `jev-1.13.0` for that endpoint. Discovery checks credential presence but does not prove the key works; a rejected key fails during judgement. A `base_url` provider setting can point to a loopback test HTTP endpoint with `GET /api/v1/models` for fixture discovery. A provider selection without this service or a verified auth owner fails closed. The separate [TypeSafe API reference](https://docs.typesafe.ai/api.md) documents a different host; do not assume a key for one host works on the other.

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

The Jev plugin is included in the static bundle behind `static-bundled-jev-provider-plugin` and is independently disableable via normal Bcode plugin selection. The bundled-plugin feature is also part of the default `static-bundled-plugins` feature set. Provider-owned auth profile `settings.base_url` (or selected provider `model.settings.base_url`) sets the Jev endpoint, with the selected provider setting taking precedence; production endpoints must use HTTPS and only `127.0.0.1` may use HTTP for tests. The `jev-latest` alias returned `jev-1.13.0` during bounded live verification; pin the concrete model ID for calibrated workflows. Jev answers are typed, not assistant text. Goal/loop integration is out of scope.

**Status:** live application-boundary verification passed. With the user-authorized temporary key supplied only through the test process environment, the ignored `judgement_client_ipc_live_jev` test returned a validated YesNo probability and usage through `BcodeClient::judge` → daemon → Jev plugin → `https://jevtypesafeai.com/api/v1/decide`. Direct, bounded HTTPS calls to the same endpoint also returned HTTP 200 with mixed Choice/Score/Noul answers and usage; `jev-latest` resolved to `jev-1.13.0`. The separate `api.typesafe.ai` host returned 403/401 and should not be used for this website's key. Loopback tests cover malformed replies, provider failures and cancellation. Revoke the temporary key as planned; it was not placed in config files or fixtures.
