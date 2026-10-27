# Account selection and pause controls

`/subscriptions` and `/auth-pool` open the same account-first picker. An account
appears once even when it belongs to several pools. The details view shows its
profile identity and pool membership; friendly labels never rename credentials.

## Select an account

Choose an account and press Enter to review routing and scope before applying:

* **Only this account** never authorizes another account as fallback.
* **Prefer this account** shows the explicit fallback accounts in attempt order.
* **This session** changes routing starting with the next turn.
* **New sessions** saves a default for the selected model preset in this
  configuration, leaving existing sessions alone.
* **Both** updates this session and that new-session default.

The model does not change. An already-running turn retains its frozen routing
through provider rounds and retries. Reset restores following configuration for
the chosen scope. Selected routing is not a claim about the account used by a
previous request; historical account usage is displayed separately when known.

## Pause, resume, and labels

Pause is separate from preference. A paused account is excluded from new turns
through all pools and account-only routes in this configuration. Choose manual
resume, one hour, or four hours. Already-running turns retain their routing.
When no eligible account remains, dispatch stops rather than using a paused
account. Resume restores eligibility without changing selection or credentials.

Rename changes only a friendly display label. Account identities remain the
owning provider plugin ID and configured authentication profile name.

## CLI

The CLI uses the same session-bound application operations and emits JSON:

```sh
bcode auth account status --session SESSION_ID
bcode auth account use openai-2 --provider bcode.openai-compatible --session SESSION_ID
bcode auth account use openai-2 --provider bcode.openai-compatible --session SESSION_ID --scope both
bcode auth account use openai-2 --provider bcode.openai-compatible --session SESSION_ID --fallback openai
bcode auth account follow-configuration --session SESSION_ID --scope both
bcode auth account pause openai --provider bcode.openai-compatible --session SESSION_ID --duration one-hour
bcode auth account resume openai --provider bcode.openai-compatible --session SESSION_ID
bcode auth account rename openai-2 Work --provider bcode.openai-compatible --session SESSION_ID
```

Omitting `--fallback` selects account-only routing. Explicit alternatives stay
within the same provider. `--scope` accepts `session`, `new-sessions`, or `both`;
the default is `session`. A new-session default requires a selected model preset.
Pause duration accepts `manual`, `one-hour`, or `four-hours`.

Pass the configuration, context, and state-root selection appropriate to the
session. These operations do not enroll accounts, discover credentials, or
expand a request's credential-custody authority. Unavailable account selections
fail closed.
