# GitHub Actions authentication support assessment

## Provider evidence

Inspected on 2026-09-20:

* [OpenAI authentication](https://learn.chatgpt.com/docs/auth.md) (the former
  `https://developers.openai.com/codex/auth` redirected to this documentation).
* [Maintain Codex account auth in CI/CD (advanced)](https://learn.chatgpt.com/docs/auth/ci-cd-auth.md).

The authentication guide distinguishes ChatGPT subscription access from
usage-based API-key access. API-key requests follow the API organization's
retention/data-sharing settings and are billed through the Platform account.
The guide recommends API keys as the default for automation.

The advanced guide describes **Codex's own client** maintaining its auth cache
on trusted runners. It says: “Do not use this workflow for public or open-source
repositories.” It also explicitly describes letting Codex perform refresh rather
than calling the OAuth token endpoint yourself.

These documents do not establish support for Bcode operating a shared
subscription-authenticated model gateway. A custom-provider/proxy configuration
option in Codex is not evidence that a third-party broker may enroll, refresh,
and multiplex subscription credentials for arbitrary GitHub jobs.

## Consequences for this integration

* Use explicitly selected API-key authentication for the service-free automation
  path. Preserve the user's billing choice; do not silently fall back from a
  subscription to paid API access.
* Do not present copying Codex auth caches into GitHub secrets or runners as the
  Bcode quickstart. Besides the provider's stated scope, that pattern conflicts
  with this integration's requirement to keep refresh credentials in broker custody.
* Keep subscription gateway support blocked pending provider clarification or a
  documented supported contract covering Bcode's actual gateway and repository
  usage. Do not imply the trusted-private Codex runner guide authorizes it.
* Do not translate the guide's description of current Codex refresh timing into
  a refresh quota, token lifetime, or guaranteed unattended session duration.
* An API-key-backed broker remains a possible explicitly billed path, but still
  requires implementation and verification of OIDC authorization, bounded leases,
  budgets, provider-owned request handling, recovery and secret isolation. It is
  not delivered by this assessment.

## Remaining verification

No credentials were enrolled, OAuth endpoints called, services deployed or
GitHub jobs dispatched for this assessment. These sources establish provider
recommendations and a limitation, not successful Bcode setup or execution.
The reusable Action, isolated execution path, onboarding, supported self-host
operation and live arbitrary-prompt evidence remain required. The agreed
subscription/broker scope must not be marked complete by substituting direct
API-key execution alone.
