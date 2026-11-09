# OpenCode Go

Use OpenCode Go in Bcode with a personal API key. No browser-login flow, OpenCode client installation, or Node dependency is required.

## Connect

1. In [OpenCode Console](https://opencode.ai), open **Account → Personal API keys**, choose the workspace, and create a key.
2. In Bcode's onboarding, select **OpenCode Go** and paste the key into the secure API-key prompt. To reopen setup, run `bcode --onboard`.
3. Choose an available Go model in Bcode's model picker.

Bcode stores the key through its standard authentication system. Do not put it in command arguments, TOML, chat messages, or logs. Existing Sol and OpenAI authentication remains independent; connecting Go does not replace those credentials.

### Existing auth CLI

You can also enroll the key from an interactive terminal:

```sh
bcode auth login opencode-go
```

This uses the plugin-registered `api_key` method and prompts for the key without echoing it. To select the method explicitly:

```sh
bcode auth login opencode-go --method api_key
```

Then select OpenCode Go and a Go model in Bcode. `bcode auth providers` lists authentication providers registered by enabled plugins; `bcode auth status opencode-go` checks the provider's authentication status. Enrollment alone is not proof that inference succeeds.

## Routing and billing

* Go uses Chat Completions at `https://opencode.ai/inference/go/openai/v1`; Go model discovery uses `https://opencode.ai/inference/go/v1/models`.
* Bcode sends its own user-agent and a stable `x-opencode-session` derived from the Bcode session ID. You do not need to configure these headers.
* Bcode does not automatically fall back from Go to pay-as-you-go service. However, **Extra Usage** in OpenCode Console can add charges beyond the Go allowance. Disable Extra Usage **in Console** if you do not want those charges; Bcode does not control that setting.

## Scope and limitations

This integration supports API-key authentication. It is not a browser-login integration or MCP OAuth, and it does not implement OpenCode Console client-policy enforcement. Do not rely on Bcode to enforce Console client policies or spending limits.

For current key, harness, plan, and billing details, see OpenCode's official [harness documentation](https://opencode.ai/docs/console/harnesses) and [Go documentation](https://opencode.ai/docs/console/go/).
