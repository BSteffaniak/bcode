# ChatGPT history synchronization

## Implementation status

The configuration policy below is implemented. Production retrieval, scheduling,
import publication, and session discovery integration are not yet connected. These
settings alone do not currently start network requests or import conversations.

## Default-on policy

The intended production behavior is automatic history synchronization for all
configured compatible ChatGPT subscription profiles, including newly added profiles.
Using the existing provider authentication does not require a browser or a separate
cookie store. Imported history is local persistent content; setup must disclose this
behavior and the following controls before production scheduling is enabled.

Disable ChatGPT history synchronization globally:

```toml
[session_import.chatgpt]
enabled = false
```

Exclude just one auth profile:

```toml
[session_import.chatgpt.profiles.openai-2]
enabled = false
```

Unlisted compatible profiles remain enabled. `[session_import] enabled = false`
overrides both global ChatGPT settings and profile settings. Invalid keys in the
ChatGPT settings are rejected instead of silently ignoring a misspelled opt-out.
`auto_discover_on_startup` controls startup discovery timing, not authorization for
manual synchronization.

Disabling must cancel pending work and prevent further retrieval; it must not delete
already imported history. The policy evaluator returns eligibility only: callers
still need to verify profile/provider compatibility, credential ownership, remote
account/workspace scope, cancellation, and current policy before side effects.

## Remaining integration obligations

* Provider-owned direct API adapter and existing sshenv refresh custody.
* Bounded paginated discovery and explicit scope coverage.
* Account-scoped immutable revisions published through the session domain.
* Durable incremental synchronization and cancellation on policy changes.
* Logical conversation grouping, profile/source filters, search and continuation.
* Setup disclosure, actionable auth/sync errors, and end-to-end interaction tests.
