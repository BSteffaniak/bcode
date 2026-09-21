# Observing a submitted turn from scripts

`bcode send` normally returns admission, not execution completion. Use `--wait` to
observe the exact admitted turn through the application client:

```sh
bcode send SESSION_ID --file prompt.txt --wait --timeout-secs 600
```

`--stdin` is also supported. Pass prompt content as file/stdin data, rather than
interpolating it into a shell command. The existing bounded UTF-8 input validation
applies. `--file` rejects non-regular files (including devices and FIFOs); use
`--stdin` deliberately for streamed input. This file-type precheck is not a
sandbox or protection against concurrent path replacement: integrations must
keep prompt files in trusted private temporary storage. Reading stdin can wait
for EOF before observation starts.
`--wait` cannot be combined with `--follow-up`; observation time is bounded
between 1 and 86400 seconds (default 600).

The command subscribes before submission, correlates the receipt's turn identity,
and prints a JSON object containing `schema_version: 1`, `session_id`, `receipt`,
and `outcome` once that terminal event is observed. Only `completed` returns
success; cancelled, error, idle timeout, tool-round-limit and unavailable-provider
outcomes return a failing exit status after printing the terminal result.

## Retrying admission safely

For ordinary `send`, supply both a producer namespace and a stable key for one
logical submission:

```sh
bcode send SESSION_ID --file prompt.txt --producer ci.example \
  --idempotency-key job-42 --wait --timeout-secs 600
```

Within that session, admission identity is the producer/key pair. Reusing it
returns the original receipt, even when the prompt content has changed. It does
**not** compare prompt content and reject conflicting payloads. With `--wait`,
the reported outcome belongs to that original turn; a nonzero exit may simply
report its original failure, not rejection of the replacement prompt.

Keep the prompt and execution intent unchanged when retrying an uncertain
submission. Do not reuse a key to request different work. A deliberately new
request needs a new key, but a timeout or transport failure is not evidence that
starting new work is safe: first reconcile the original receipt and any committed
effects. Keys are session-scoped, so creating a new session does not deduplicate
an earlier job. This mechanism alone does not make workflow reruns or external
publication idempotent.

## Targeted cancellation

Scripts can request cancellation of an observed active turn without cancelling a
successor that started in the meantime:

```sh
bcode cancel SESSION_ID --turn-id TURN_ID --json
```

`--turn-id` conflicts with `--clear-queue`. The result is a cancellation request
acknowledgement, not a terminal outcome. A false acknowledgement does not prove
completion or cancellation of an admission still queued. Continue observing the
original receipt; do not resubmit work based on this acknowledgement.

## Trusted configuration selection

Use the global `--config-only FILE` option to load a required trusted config file
without merging discovered user/repository config or environment config layers.
Explicit CLI configuration overrides still apply. A missing or malformed file
fails rather than falling back to ambient configuration.

This controls configuration composition only: it does not sanitize process
credentials, disable every discovery mechanism, isolate state, or sandbox tools.
Those controls must also be supplied by the execution integration.

## Limits and recovery

* The observation timeout is not a total execution deadline. By default it does
  **not** cancel the turn, terminate tools, or resubmit the prompt.
  With `--wait --cancel-on-timeout`, the CLI makes one exact-turn cancellation
  request after an observation timeout. Cancellation transport and terminal
  recovery share one `--cancellation-grace-secs` budget (default 5, range 1–300),
  rather than adding the client's request timeout to that budget. The remaining
  time is used to recover/observe the exact turn's terminal event on the original
  subscription, even when the acknowledgement was lost. A verified outcome uses
  the ordinary terminal result and exit status (including success if completion
  won the race). If still unknown, the command fails with an unverified outcome:
  `cancellation_requested: true` means attempted,
  and `cancellation_acknowledged` is true/false on acknowledgement or null on
  transport failure. Neither value proves terminal cancellation. Other observation
  errors do not trigger cancellation; submission is never retried.
* Do not implement an exact-turn deadline by blindly following a timeout with
  `bcode cancel SESSION_ID`. Session cancellation captures the active turn when
  the daemon handles the request, not the receipt being observed. Queued
  cancellation is fenced against later successors, but if the observed turn
  completed before the request arrived, the request could capture its successor.
  A cancellation acknowledgement also is not a terminal
  outcome: integrations must still observe the admitted turn's final event.
  The application client now exposes `cancel_exact_session_turn(session_id,
  turn_id)`, carrying the expected identity through IPC to the queue fence and
  never clearing unrelated queued work. It only targets an active turn: false
  does not prove the receipt completed or that queued work was cancelled.
  Opt-in CLI timeout cancellation invokes this operation and performs a bounded
  terminal-outcome check; expiry of that check still leaves work unverified.
* A transport failure or required resynchronization does not prove completion or
  cancellation. After admission, observation failures print an envelope with
  `outcome: null`, `observation: "unverified"`, and `cancellation_requested: false`,
  retaining the receipt for investigation. Input, subscription and submission
  errors before a receipt is returned also emit an unverified envelope, with
  `receipt: null`. A submission transport error may follow a committed admission;
  never infer that work did not start or automatically submit a new turn.
  Explicit admission rejection/cancellation-before-start instead reports
  `observation: "not_started"` and includes the admission response.
  The command returns a failing exit status.
* Before live observation, the client reads one bounded canonical history page
  (at most 1000 recent events) on the subscribed connection. This recovers recent
  terminal outcomes omitted by presentation history, including repeated idempotent
  submissions, without rerunning work. The observation timeout covers this read
  and the subsequent wait. Older outcomes outside the page can still be unknown;
  a timeout is not permission to repeat effects with a new idempotency key.
* This command does not bypass permission decisions or answer interactions. A turn
  awaiting input can outlive the observation window.
* This is not yet the complete GitHub Actions integration: isolated trusted
  configuration, authentication preflight, process isolation, cancellation,
  budgets, reusable Action packaging and brokered authentication remain separate
  required integration work. No hosted broker service is implied.
