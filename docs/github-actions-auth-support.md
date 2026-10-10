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

## Supported scope and remaining gates

Use explicitly selected API-key authentication for the service-free trusted
automation path. Preserve billing choice: never silently fall back from a
subscription to API billing. The generic Action accepts arbitrary prompts; no
broker, hosted service or GitHub App is required for this initial path.

For subscription credentials, existing Bcode provider code refreshes enrolled
owned profiles and persists rotation through host custody. A persistent trusted
single-user installation with unattended custody access is the smallest candidate
for eliminating per-job credential copying. This implementation behavior is not
proof of provider support for Bcode unattended use. Confirm that contract before
enabling subscription automation. Revocation can still require reauthorization.

Do not copy refresh bundles into GitHub secrets/artifacts or use secret updates
as transactional rotation storage. Do not infer refresh quotas or perpetual auth.
Shared subscription gateway operation remains unverified; do not build or promise
it as a prerequisite for the simpler direct integration.

No credentials were enrolled, services deployed or live workflows dispatched
for this assessment. See [GitHub Actions](github-actions.md) for setup and trust
limitations. Live execution and subscription-support verification remain open.
