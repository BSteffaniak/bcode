# Invariant Guidance Architecture

`INVARIANTS.md` is the canonical repository catalog. Bcode can expose that catalog to coding models in either complete or focused form without changing the Markdown source format.

## Configuration modes

`invariants.enabled = false` is the master kill switch. Disabled mode does not load catalog content into model requests, add invariant reminders or metadata, run selector requests, or schedule reevaluation.

Enabled `full` mode is the default. It preserves the complete stable-system-prompt catalog and performs no selector work.

Enabled `relevant` mode sends the complete catalog only to an internal selector request. Primary coding-model requests receive a bounded reminder containing exact catalog entries selected for the current task. Selector output uses request-local numeric references; Bcode resolves those references back to exact parsed entries and never accepts model-authored invariant text.

## Selection lifecycle

The first coding turn in a live session synchronously prepares focused guidance before the primary model request. A configured selector model profile should normally identify a fast model; when omitted, the active model selection is reused. If a prior final-turn reevaluation is still running when the next prompt arrives, the default behavior is to use the last valid selection without waiting. `wait_for_background_on_next_prompt = true` opts into waiting for that task.

The focused selection is reused for every provider round, tool continuation, and retry within that coding turn. No selector call occurs inside the tool loop.

After a completed final assistant turn is committed and its runtime work is finished, Bcode gathers the latest user/final-assistant semantic text and launches reevaluation in a separate Tokio task. Response delivery and the interactive turn permit do not wait for that task. The resulting selection is eligible for the next turn only if its session generation is still current.

## Deadlines and cancellation

Selector deadlines are optional. Omitting `initial_timeout_ms` or `background_timeout_ms` means waiting without a deadline. `cancel_on_timeout` is false by default; a configured deadline stops waiting while the detached selector continues, and its generation-checked result may still update future guidance. When cancellation is enabled, Bcode issues the provider cancellation operation and aborts the host task. `cancel_stale` is also false by default. Stale result rejection is mandatory even when provider work is allowed to finish.

Configured timeout policies are:

* `full`: use complete-catalog behavior for that request.
* `previous`: retain the previous focused selection, with complete-catalog fallback when none exists.
* `deterministic`: use local token-overlap matching bounded by `max_selected`.
* `none`: explicitly omit invariant guidance for that request.

Provider or decoding failures retain prior focused guidance when available and otherwise use bounded deterministic matching. They do not accept hallucinated selector text.

## Prompt placement

Full mode keeps the complete catalog in the stable system prompt. Relevant mode removes the complete catalog from that prefix and adds a request-only system message titled `Relevant repository invariants`. That message can include a compact task summary followed by the exact selected invariant bullets.

The catalog remains canonical on disk. Focused guidance is derivative request state and does not redefine, edit, or exempt catalog entries.

## Performance and safety

* Full mode adds no selector calls and is the default.
* Relevant mode performs one foreground selector call only when no valid live selection exists.
* Tool rounds and retries reuse guidance without selector work.
* Post-final reevaluation is asynchronous and generation-checked.
* `max_selected` is enforced by Bcode after decoding.
* Empty, out-of-range, duplicate, or malformed selector references cannot enter the reminder.
* The selector has no tools and receives a `ToolChoice::None` request.

## Persistence boundary

Focused guidance is retained as bounded per-session state in `<state-dir>/invariant-guidance/<session-id>.json`. It contains only the catalog digest, compact task summary, exact selected entries, and source sequence; it does not copy the complete catalog or enter transcript history. The sidecar is atomically replaced after generation validation and loaded directly on reconnect, so normal attach and model-context construction do not full-replay event logs. Catalog digest changes invalidate its applicability and trigger fresh selection.

## Configuration examples

Relevant mode with a dedicated selector profile:

```toml
[invariants]
mode = "relevant"

[invariants.selector]
model_profile = "fast-selector"
max_selected = 10
max_summary_chars = 240
fallback_to_full = true
wait_for_background_on_next_prompt = false
```

Disable selector calls while retaining relevant mode and its deterministic local fallback:

```toml
[invariants]
mode = "relevant"

[invariants.selector]
enabled = false
```

Disable all invariant guidance:

```toml
[invariants]
enabled = false
```

## Targeted rollout plan

1. Run shadow selection to observe chosen entries without altering primary prompts.
2. Enable relevant mode for selected repositories or agent roles and compare prompt tokens, first-token latency, selector latency, fallback counts, and correction rate.
3. Expand relevant mode after quality review; keep full mode as the stable default and immediate rollback path.
4. Enable globally only after representative workloads show no material adherence regression.
