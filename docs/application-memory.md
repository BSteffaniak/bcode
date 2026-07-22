# Application memory contract

Bcode memory is an application extension and does not require Bcode's local session database.
`MemoryProvider` receives the current user query and a clone of the visible transcript, then returns
typed candidates. Bcode validates and assembles those candidates before any provider network work.

## Retrieval and relevance

Each `MemoryItem` has a stable item ID, a model message, an integer relevance score from 0 through
1,000, source provenance, privacy, and retention. Bcode orders candidates by descending relevance,
then configured provider order and provider item order for deterministic ties. Duplicate identities
are rejected. `MemoryRetrievalReport` records accepted IDs, provenance, relevance, byte size, total
bytes, and stable filtering/failure diagnostics without copying memory payloads.

## Bounds and privacy

`MemoryPolicy` independently limits accepted item count, serialized bytes per message, and total
serialized bytes. It also sets the most sensitive accepted privacy class:

* `Public` is ordinary application context;
* `Private` is user/application-private context;
* `Sensitive` requires explicit opt-in.

The default admits public/private memory, at most 16 items, 32 KiB per item, and 128 KiB total.
Empty IDs/provenance, empty messages, out-of-range relevance, disallowed privacy, oversized items,
and duplicate identities are invalid.

## Failure behavior

`FailTurn` stops before model invocation with a stable, secret-safe memory error.
`ContinueWithoutMemory` omits a failed provider or invalid item and records a diagnostic in the
report. Provider error payloads are not copied into SDK errors or diagnostics.

## Request-only and persisted memory

Provider retrieval accepts only `RequestOnly` items. They are inserted into the next model request
before the visible transcript, but are never appended to, exported from, or saved with that
transcript.

Persistence is explicit through `AgentSession::remember` and requires
`MemoryRetention::SessionTranscript`. It validates the same privacy and size policy, appends a
normal visible model message, and saves through the configured `SessionPersistenceAdapter` before
committing the in-memory mutation. Retrieved providers cannot request persistence implicitly.

`SessionContextProvider` remains a compatibility extension for arbitrary request-only context. New
memory/retrieval integrations should use `MemoryProvider` when relevance, provenance, privacy,
bounds, and diagnostics matter.
