# Type-enforced IPC request routing

Bcode's wire `Request` enum spans multiple product domains. The server partitions it once into domain-owned request enums, then dispatches directly to the owning handler.

This is **IPC dispatch**, not provider/model selection. The separately runnable [brouter](../packages/router/README.md) handles capability- and policy-based model routing.

## Current implementation

[`packages/server/src/request_routing.rs`](../packages/server/src/request_routing.rs) defines `RoutedRequest::from_request`. Its exhaustive match has no wildcard arm: a new wire request must be assigned to a domain before the server compiles.

Each dispatcher consumes its domain-specific enum rather than the original flat request. It therefore cannot forward an unrelated request to another dispatcher. Large domain payloads are boxed where needed to bound the routing enum and async frame sizes.

The wire format remains defined by `bcode_ipc::Request`; this internal partition does not change the client protocol.

## Why this boundary exists

Earlier dispatchers were chained by fall-through. Their async futures remained live together, accumulating unrelated stack frames and causing a measured stack overflow. Direct, typed routing removes that chain structurally rather than relying on handler order or runtime `unreachable!()` assertions.

The original migration plan is not the current architecture: exhaustive partitioning is implemented. Avoid copying historical variant counts or dispatcher lists into guidance; the source is authoritative.

## Maintaining the partition

The module carries a historical generator comment, but that generator is not present in this checkout. Treat the checked-in partition and affected dispatchers as authoritative; update them together and run the server's relevant tests and repository architecture checks. Do not add a catch-all fallback to make a new request compile.

Preserve these properties:

- Every wire variant has exactly one owning domain.
- Dispatchers cannot fall through to unrelated domains.
- Boxing decisions preserve bounded stack use.
- Serialization remains owned by the wire types.
- New request handling is tested at the owning dispatcher.
