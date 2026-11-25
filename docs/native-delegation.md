# Native delegation

An agent in a Bcode session can delegate a self-contained task to a fresh Bcode child session
through tools, instead of running `bcode` through the shell. The child is created and run by the
same daemon, in the same state location, under host authorization. The agent receives typed,
bounded results.

## Tools

The bundled harness plugin (`bcode.harness`) offers the tools. Native delegation is enabled by
default; external coding agents remain available as configured targets of the same tools (see
[managed external sessions](managed-external-sessions.md)).

| Tool | Purpose |
| --- | --- |
| `harness.delegate` | Create a child of this session, submit `prompt` as its first turn and return its `session_id` immediately. |
| `harness.wait` | Block until any (default) or all named children settle, fail or need attention, or a timeout passes. |
| `harness.result` | Summarize one child: status, latest assistant text, pending prompts. |
| `harness.continue` | Submit the next turn to a child. |
| `harness.interrupt` | Interrupt a child's active turn and clear its queue. |
| `harness.context` | This session's ID, parent, depth, workspace, model, agent profiles, children and limits. |

`harness.delegate` targets a Bcode child when `instance_id` is `"bcode"`, or when it is omitted
and no default external instance is configured. Bcode-child-only options:

* `title`, `agent` (an agent profile), `model` (resolved through the model catalog), `cwd` (an
  absolute working directory) and `worktree` (`cwd` is an existing registered worktree of the
  parent's repository).
* `permissions`: `inherit` (default), `agent_profile` or `always_ask`. See below.
* `cancel_with_turn` (default `false`): whether interrupting the delegating turn interrupts the
  child.

`harness.continue`, `harness.result` and `harness.interrupt` try the native interface first and
fall back to the external managed-session path for external children. `harness.wait` and
`harness.context` apply to Bcode children only. The build agent receives all of these tools;
the plan agent keeps `harness.result`, `harness.wait` and `harness.context` only.

## Authority

Plugins reach the host through the `bcode.native-delegation/v1` interface. The route is offered
only to tool invocations, and the host binds every request to the invoking session and tool
call; the payload cannot name a different caller.

* A delegated child is an ordinary child of the invoking session (see
  [child sessions](child-sessions.md)). Its canonical creation event retains an immutable
  delegation intent: the producing plugin and invocation, the delegating turn, the permission
  policy and `cancel_with_turn`.
* Continue, result, wait and interrupt require that the session's canonical parent is the
  invoking session and that its creation retains a delegation intent. Plain children and other
  sessions are refused.
* Child identities are derived from the invoking session, tool call, target and prompt. An
  identical retry reconciles with the existing child instead of creating another. Reusing an
  identity with different options fails closed.
* A client cannot assert a delegation intent through composed spawning; only the host's
  delegation operation creates one.

## Permissions and interactions

Each child turn is admitted with a permission mode derived from the retained policy:

* `inherit`: the child bypasses permission policy only while the parent's delegating turn was
  admitted in bypass, has not been revoked, and the parent session is still in bypass. Stepping
  the parent out of bypass steps such children out at once. A child never runs with more
  authority than its parent. The mode is daemon memory only, like every runtime bypass.
* `agent_profile`: the child's agent policy applies; the parent's bypass is never inherited.
* `always_ask`: every tool call the tool marks as requiring permission asks the user, even when
  an agent preauthorization or remembered skill decision would allow it. Denials still apply.

Child permission requests and questions are routed to the parent session's view through the
existing interaction route, so the user sees and answers them there. Delegation never gives the
parent agent decision authority over its child's prompts: `harness.respond` refuses Bcode
children.

## Cancellation

Parent lifecycle actions do not cancel children, with one opt-in exception: when the delegating
turn of a child created with `cancel_with_turn` is interrupted, the child's active turn is
interrupted and its queue cleared. The daemon records such children per turn; children it did not
record (for example after a restart) are found through a bounded scan of the parent's active
children and verified against their retained intent.

## Limits

```toml
[plugins.config."bcode.harness".native]
enabled = true            # offer Bcode children (default true)
max_depth = 4             # optional; unlimited when omitted
max_active_children = 32  # default 32
```

* `max_depth` bounds how deep a delegated child may sit below its root session; a root session's
  child has depth 1. The check walks at most `max_depth` ancestors.
* `max_active_children` bounds a parent's simultaneously running, queued or waiting children.
  Counting reads bounded activity pages.

Exceeding a limit returns a typed failure (`native_delegation_depth_exceeded`,
`native_delegation_active_children_exhausted`) the model can act on.

## Results

Summaries are bounded and non-mutating. They read observed activity, the child's first canonical
event and at most four 64-event pages of its history tail. They never replay a child's history.
`latest_result` is the latest assistant text of the child's latest turn (8 KiB at most) and is
untrusted child output. `settled` means idle, not accepted.

## Shell commands

Commands started by the shell tool reach the same Bcode even when they run `bcode` directly. The
daemon publishes `<state root>/runtime/cli/<artifact>/bcode`, linking to its pinned executable,
and the shell tool places that directory first on `PATH`. It also exports `BCODE_EXECUTABLE`,
`BCODE_STATE_DIR`, `BCODE_SESSION_ID` and, when a single explicit configuration file reproduces
the session's configuration, `BCODE_CONFIG`. In-process hosts without a verified Bcode executable
publish no directory.

## Persistence

Event schema 60 and writer epoch 29 retain the delegation intent in
`child_session_created.spawning.delegation` and admit the `always_ask` turn permission mode.
Historical classification and upgrades are owned by session migration; see
[session persistence](session-persistence-architecture.md).
