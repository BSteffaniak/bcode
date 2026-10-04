# Judgement evaluation in goals and loops

The loop plugin can optionally add a read-only `loop.judgement.evaluate` workflow block after its existing read-only agent evaluator. The agent still inspects the repository/session and provides concrete evidence; a durable input transform pins the stop condition, implementation prompt, iteration limit and judgement configuration to the original run input so the agent cannot change the selected provider, credential profile or threshold in its structured output. The block sends only a bounded stop condition, iteration number and evidence to the application's versioned `bcode.judgement-application/v1` service. The application resolves the selected provider/model, verifies provider ownership and credentials, invokes the judgement provider, validates its typed response and returns only normalized results. The loop plugin never reads provider secrets or calls Jev directly. This does not select a conversational model.

Use the **Evaluation** control in **Start Loop** or **Start Goal**. It defaults to **standard agent evaluation**, which requires no judgement-model key. Press Space on the control to opt into **agent evidence + judgement model**. Enabled providers, compatible YesNo models and available named accounts are discovered through a bounded, secret-free application catalog; this is not proof that an upstream key works. Before opt-in launch, Bcode rechecks the exact model and account and blocks stale selections before creating a workflow (still not an upstream key-validity probe). Use Left/Right to change the model, Up/Down to change the account, +/- to adjust the confidence threshold (default 90%), F to choose between pausing (default) and retaining the agent verdict if the judgement service is unavailable, and R to refresh after login. The form shows loading, unavailable and account-setup hints. To enroll Jev, run `bcode auth login jev` (default profile `jev`). No slash-delimited identifiers or API key are entered into the form. An explicitly selected daemon-environment account requires the provider's declared environment credentials in the daemon process.

A validated YesNo probability at or above the threshold sets `condition_met`; a lower probability continues the loop. The agent's evidence is retained for review, and the summary records the probability, threshold and decision; vendor wire metadata is not retained. The agent's own completion vote is provisional when judgement is enabled. Goal **readiness** remains a separate agent planning decision: judgement applies to the subsequent per-iteration completion decision only, not to permission or authorization policy. Iteration continuations retain the evaluator configuration and reserve the extra block execution allowance. Existing runs keep their pinned workflow definitions and behavior. A failed or interrupted read-only external call has repair-required reconciliation, rather than silently repeating a possibly billed request after daemon restart.

Noninteractive `bcode goal` supports the same opt-in gate:

```sh
bcode goal --session <session-id> --objective 'Your goal' \
  --judge-provider bcode.jev --judge-model jev-latest --judge-account jev
```

All three selection flags are required together. `--judge-threshold` defaults to 90;
`--judge-failure pause|agent-fallback` defaults to `pause`. An explicitly empty
`--judge-account ''` selects declared daemon environment credentials. The CLI checks
the exact selection before generation and again before launch; it never takes an API
key as an argument or lets generated instructions choose the judge.

This opt-in feature requires the loop plugin and the chosen judgement provider to be enabled. Provider failures are normalized; `pause` terminates the current attempt as a failed workflow needing operator attention rather than asserting completion or automatically retrying. The provider does not inspect your repository itself: completeness depends on the read-only agent's concrete evidence. Do not put secrets, excessive logs, or untrusted instructions in the stop condition or evidence. Goal/loop runtime does not implicitly persist the full judgement request as a session turn.
