# Session milestones

Milestones are durable generated annotations of settled conversational history. Each annotation has a
header, description, inclusive canonical source range, and source navigation anchor. Its publication
sequence identifies the annotation; its anchor identifies the conversation to navigate to. Navigation
opens a bounded history window. It does not rewind a session, delete events, or resubmit a turn.

## Reading and navigation

```sh
bcode session milestones list <session-id> --limit 50
bcode session milestones list <session-id> --after-sequence <publication-sequence> --limit 50
bcode session milestones generate <session-id>
```

List prints one JSON page in publication order. Limits are 1–200; `next_after_sequence` is the
exclusive cursor for the next page. Reading durable milestones does not require a generation plugin
or invoke a model. Storage damage or unavailable canonical state is reported rather than repaired by
listing.

The plugin-owned `/milestones` command provides a plain-text fallback; `/milestones generate`
explicitly requests generation. In the terminal `/timeline` dialog, `m` loads a bounded milestone
page and `p` returns to resident user prompts. `n` replaces the resident page using the exclusive
publication cursor; `m` resets to the first page. Each page holds at most 200 milestones, so browsing
does not accumulate the session's entire milestone history. Enter navigates to the selected canonical
source anchor through the existing bounded timeline jump. Existing prompt navigation remains the
default. Milestone rows display the canonical publication event's timestamp, not an inferred source
or provider clock. These terminal actions use the configurable timeline keymap; the keys above are
the defaults. While a milestone page is loading, old entries are cleared and Enter cannot navigate.
Responses are accepted only for the current session, request generation, and publication cursor;
closing the dialog or returning to prompts invalidates pending page results.

Search accepts `--content milestone` or inline `content:milestone`. Search providers may be disabled
or incomplete; search coverage and hydration diagnostics remain explicit. Only exact canonical
hydration enables navigation. Milestone search hits identify the publication event, while the
portable CLI JSON `navigation_sequence` and terminal navigation use the annotation's source anchor.
Reading, searching, hydration, and navigation never trigger generation.

## Generation policy

Generation is optional and disabled by default. Enable the bundled session-milestones plugin and
configure `[milestones]` with `enabled = true` and an explicit `model_profile` referencing a centrally
resolved model profile. Explicit generate requests still require this opt-in and model configuration;
they do not accept a client-supplied prompt or arbitrary model override. The host freezes bounded
source evidence and applies configured admission limits before auxiliary no-tools inference.
Generation instructions and source prompts are request-only, not durable session events.

The first evaluation is due after `first_turns` settled conversational turns **or** after a
settled turn reaches `subsequent_tokens` eligible normalized context tokens. Later evaluations
use `subsequent_turns` or `subsequent_tokens`; all automatic retries respect the minimum interval.
Provider rounds and tool calls are not conversational turns.

Auxiliary inference retains typed provider and service failures through collection and cleanup.
Normalized diagnostic records preserve error codes, categories, request IDs, source chains,
retry hints and actionable failure context. Raw provider response bodies are excluded and diagnostic
strings are sanitized. Usage-recording and cleanup failures accompany rather than replace the
original failure. Startup failures travel as normalized error events on the push path and in the
polling startup error payload; older consumers can still read the service code and message.

Native sshenv refresh uses the ordinary host auth boundary with registry-verified ownership.
An adjacent private `<vault>.refresh` directory retains an exclusive coordinator and versioned
refresh intent; the existing encrypted vault remains authoritative and is not copied or converted.
The intent fences credential reads and mutations during rotation and survives daemon interruption.
Only a complete, matching commit clears it after durable publication. Unresolved or incompatible
intents fail closed and are preserved for explicit maintenance; remote rotation is never replayed.
Both streaming and polling auxiliary startup supply the host credential bridge.

Accepted milestones record normalized provider/model provenance and source truncation. Evaluation
admission and completion are durable checkpoints, including successful evaluations with no milestone.
The generated display title is separate from an explicit session name. With `follow_title` enabled
(default), accepted generated titles may update display metadata; manual names retain precedence.
Milestone annotations are not model-context instructions and are not transcript rewrites.

## Application boundary

Clients use `session_milestones(session_id, query)` for bounded reads and
`generate_session_milestone(session_id)` for explicit generation. IPC carries domain-owned portable
milestone pages and a normalized generation outcome containing `success` and bounded diagnostic
`text`, without plugin-SDK, provider-native, or terminal types. Optional rich presentation does not
replace the structured page API or plugin plain-text fallback.
