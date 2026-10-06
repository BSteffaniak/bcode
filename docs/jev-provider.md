# TypeSafe Jev judgement provider

Jev implements `bcode.judgement-model-provider/v1`, not the conversational model-provider
interfaces. It answers bounded Choice, Score, and YesNo questions; it cannot be selected as
a chat, text-generating, or tool-calling model. The auth provider ID is `jev`; the plugin
route is `bcode.jev`.

## Credentials and endpoints

Enroll with `bcode auth login jev` and select the appropriate authentication method:

* Hosted Jev: method/credential `api_key`, declared environment key `JEV_API_KEY`, default
  endpoint `https://jevtypesafeai.com`, decision route `POST /api/v1/decide`.
* Official TypeSafe: method/credential `typesafe_api_key`, declared environment key
  `TYPESAFE_API_KEY`, default endpoint `https://api.typesafe.ai`. This is a separate credential
  type; a hosted website key must not be treated as an official API key.

Backend selection follows explicit provider `backend` settings (`hosted` or `official`),
then authentication method/credential type. Conflicting credentials fail closed. Provider
`base_url` settings may override the endpoint, subject to backend/host compatibility checks;
production uses HTTPS. Loopback `127.0.0.1` HTTP is available for tests, not arbitrary HTTP
hosts. Secrets are resolved centrally from an owned profile or declared daemon environment,
not supplied in judgement state. Do not put keys in task prompts, fixtures, or request files.

The hosted endpoint has no working authenticated model-list route in the verified API;
it advertises `jev-latest` and the observed concrete `jev-1.13.0`. Advertisement is not key
verification or an inference-readiness guarantee. Pin a concrete ID for calibrated workflows.
Official discovery uses its backend's authenticated model-list protocol rather than assuming
hosted aliases are universally available.

## Verification and automatic use

The optional `bcode.judgement-verification/v1` service reports authentication, model readiness,
and verification method independently:

* Official authenticated model discovery can prove credential acceptance and model
  advertisement. Model readiness remains `not_checked`; discovery is non-inference, **not a
  promise of zero cost** or available inference quota.
* Hosted verification performs one fixed lightweight YesNo inference probe. It can be
  billed. No application task, history, or tool result is sent in that probe. Success establishes
  credential acceptance and inference readiness; quota/rate-limit and transient failures must
  not be described as invalid credentials.

[Automatic judgement](automatic-judgement.md) defaults all six enhancements to auto, subject
to useful bounded triggers, central capability/credential selection, verification, and
conservative fallback. `[judgement] mode = "off"` or a feature's `off` makes zero optional
provider calls, including verification. Missing or ambiguous accounts, failures, and uncertainty
retain baseline behavior; explicit pins do not silently switch accounts. Central access owns
verification cache, timeout, backoff, and cancellation policy.

## Explicit application access

`bcode_client::BcodeClient::judge(provider_plugin_id, auth_profile, request)` evaluates an
explicit typed request. Use `provider_plugin_id = "bcode.jev"` and a provider-owned profile;
an explicitly empty auth profile selects provider-declared daemon environment credentials.
The client never passes secrets. `optional_judge` instead submits `OptionalRequest` through
the application policy and returns `OptionalResponse { response: None }` when disabled,
unavailable, or failed. Explicit verification is available through the application/client
verification boundary, separate from optional-feature opt-outs.

For goal/loop completion evaluation, select **Evaluation → agent evidence + judgement model**
and choose the exact provider/model/account. See [goal/loop judgement evaluation](loop-judgement-evaluation.md).
This explicit evaluator is distinct from automatic advisory progress monitoring; neither a
model probability nor provider verification transfers completion authority.

## Compatibility and safety

Production transport, endpoint selection, wire formats, and normalized errors are provider-owned.
Central application access owns credential resolution. Responses require exact question IDs,
matching answer kinds, finite valid numeric ranges, and consistent options; malformed replies
fail closed. Portable limits are 256-KiB request/response, 64 questions, and 128 choice/rubric
options. Jev imposes its own stricter score-rubric limit. Cancellation drops provider work;
no inference retries are introduced by optional consumers.

Deterministic loopback tests cover provider and application boundaries, authentication,
malformed replies, failures, and cancellation. Historical bounded live hosted calls verified
`jev-latest` resolving to `jev-1.13.0` and typed answers through client → daemon → provider;
that observation does not prove current credential validity, official-key compatibility,
free verification, or ongoing endpoint availability. Temporary live credentials must be
revoked through the issuing account; no secret belongs in repository documentation.
