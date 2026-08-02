# Reasoning presentation

Bcode treats provider-exposed reasoning as structured model output whose local presentation can vary by frontend. Showing or hiding it does not change reasoning effort, requested summary detail, provider requests, or durable execution semantics.

## TUI configuration

Fresh configuration shows every readable reasoning representation exposed by the provider:

```toml
[tui.thinking]
show = true
mode = "all"
```

Supported modes:

* `all` shows distinct provider summary/milestone and raw/detail parts.
* `summary` shows summary, milestone, and deliberate legacy compatibility parts.
* `raw` shows raw/detail parts.
* `show = false` hides all readable reasoning parts.

Every mode retains neutral activity chrome when reasoning activity evidence exists, including opaque-only, interrupted, and failed activity. Filtering is silent: Bcode does not announce unavailable representations and does not fall back from summary to raw or from raw to summary.

## TUI commands

```text
/thinking mode all
/thinking mode summary
/thinking mode raw
/thinking show
/thinking hide
/thinking status
```

## Other frontends

Renderer-neutral clients select the equivalent `ReasoningPresentationPolicy`: `all`, `summary`, `raw`, or `hidden`. Each attached frontend can choose its own bounded projection without changing provider requests or canonical session history.

Provider authors should follow the structured reasoning requirements in the [model provider contract](model-provider-contract.md). Renderer behavior and fallback rules are documented in [Renderer architecture](renderer-architecture.md).
