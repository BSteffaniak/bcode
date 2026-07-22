# Stateful SDK chat semantics

`AgentSession` is an explicit stateful wrapper; stateless generation never creates or mutates one.
Its visible transcript is an ordered `Vec<ModelMessage>` and is the canonical model context for the
next turn, supplemented only by explicit system instructions and request-only context/memory.

## Successful turn projection

A user send commits the user message followed by the complete model/tool transcript projection:
assistant text and tool calls in provider order, one `Tool` message per model-visible tool result,
and later assistant rounds. Provider reasoning remains in `GenerateTextResponse.steps` for
application inspection but is not inserted into model context because `ModelMessage` has no neutral
reasoning-replay contract. Provider metadata/extensions remain in runtime events rather than being
invented as transcript messages.

The configured system instruction is sent through `ModelTurnRequest.system_prompt` on every provider
round and future turn; it is not duplicated in the visible transcript. Typed `MemoryProvider` and
legacy `SessionContextProvider` messages are prepended for the request only and never enter the
visible/persisted transcript unless `AgentSession::remember` is called explicitly.

## Commit, retry, and regeneration

Provider retries/fallbacks occur inside the same logical turn and do not mutate session state.
Provider, middleware, tool, cancellation, validation, or persistence failure leaves the visible
transcript unchanged. After successful generation, configured persistence receives the complete new
payload before in-memory state is replaced.

`regenerate_last_with_provider` finds the latest user message, removes every later assistant/tool
round only after replacement generation and persistence succeed, then commits the original user plus
the new complete response projection. Earlier transcript and explicit memory records remain intact.

## Branch, fork, import, export, and persistence

`branch`/`fork` clone the complete in-memory transcript and memory metadata but deliberately do not
copy the persistence adapter, preventing two branches from silently writing the same backing
record. `into_messages` exports the visible transcript; `session_from_messages` imports
caller-managed messages without hidden mutation. `PersistedSession` additionally preserves explicit
typed memory metadata under its versioned portable schema.

The SDK does not claim reconnect/resume delivery semantics for `AgentSession`; it is an in-process
conversation abstraction. Daemon transports and resumable frontend contracts define those concerns
separately.
