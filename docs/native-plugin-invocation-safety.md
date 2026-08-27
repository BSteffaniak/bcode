# Native plugin invocation safety and performance architecture

## Status

This document defines the target architecture for native plugin invocation callbacks. The current
native dynamic-library ABI is version 3. Its callback handles contain invocation-scoped host
context and therefore require strict synchronous lifetime handling. The staged migration below
preserves current behavior while replacing pointer-backed invocation identity in ABI version 4.

The immediate shell-recording containment fix joins its recording worker before invocation-owned
callbacks can be destroyed. That fix remains required while ABI version 3 is supported.

## Acceptance criteria

* No plugin callback can dereference host state after its invocation closes.
* Late, duplicate, future-version, and malformed callback operations fail closed.
* Invocation closure rejects new callbacks and drains callbacks already admitted before releasing
  host-owned state.
* Statically bundled plugins use a safe typed Rust path without FFI-shaped serialization or raw
  invocation pointers.
* Dynamic-library unsafe code is confined to audited ABI adapters. Raw pointers transfer bounded
  bytes only and are never retained.
* Panics do not unwind across an `extern "C"` boundary.
* Plugin workers cannot outlive the invocation capabilities they use.
* Normal-path performance does not regress. Exceptional shutdown may wait for already-running
  invocation work, but event publication remains bounded and non-blocking with respect to durable
  or renderer work.

## Current risk

ABI version 3 passes callback function pointers together with `user_data` pointers. The host
currently points `user_data` at invocation-local callback state. SDK wrappers such as
`ServiceEventEmitter` and `ServiceBridge` hide those pointers behind cloneable Rust types and have
manual `Send`/`Sync` implementations. The compiler therefore cannot prevent a plugin from moving
one to a background worker. If that worker invokes the callback after the host call returns, it can
access released stack state.

Command and authentication registrars are activation-scoped and need no cross-thread behavior.
They must remain explicitly non-`Send` and non-`Sync` so registration callbacks cannot escape
activation.

## ABI version 4

ABI version 4 replaces invocation-state pointers with opaque monotonically allocated handles.
Payload pointers remain callback-local byte buffers.

The host owns an invocation registry with this state machine:

```text
Open -> Closing -> Closed
```

* `Open` admits a callback and increments its in-flight count.
* `Closing` rejects new callbacks and waits only for admitted callbacks to finish.
* `Closed` rejects every operation and releases the host-owned callback state.
* Handles are not reused during the daemon lifetime.

Callback operations return typed status codes including `ok`, `closing`, `closed`, `cancelled`,
`invalid_argument`, `payload_too_large`, and `host_failure`. A stale plugin handle can therefore
produce an error but cannot name freed memory.

The registry implementation must be benchmarked before selection. A single global mutex is not an
acceptable final hot path. Candidate implementations should compare sharded locking and immutable
slot/atomic lifecycle designs. Registry lookup must not perform serialization, persistence,
renderer work, or allocation after the invocation is established.

## Safe static path

Statically bundled plugins are Rust code in the same executable and do not require the C ABI.
They should receive typed, host-owned Rust invocation services directly. The static path must avoid:

* pointer/length conversion;
* JSON serialization performed only for ABI transport;
* callback registry lookup;
* dynamic symbol dispatch.

Static and dynamic adapters implement the same typed application contract. This keeps product
semantics independent of plugin transport while allowing the bundled path to remain the fastest
path.

## Invocation worker scope

The SDK will provide an invocation-owned worker scope. A plugin that needs background work must
register it with this scope rather than detaching a thread or task that captures invocation
capabilities. Closure proceeds in this order:

1. stop admitting plugin work;
2. request cancellation;
3. close event and bridge capabilities;
4. join registered workers and drain admitted callbacks;
5. return across the ABI boundary.

The shell recording worker migrates to this scope. Its local join-on-drop remains as defense in
depth.

## Unsafe boundary policy

Unsafe Rust remains necessary for dynamic library loading and bounded C ABI byte transfer. It is
not necessary in ordinary plugin business logic or the static plugin path.

Approved unsafe operations are limited to dedicated native-ABI modules that:

* load and call version-checked symbols;
* validate nullness, lengths, capacity, alignment, and arithmetic before creating slices;
* copy callback input into owned storage before application dispatch;
* never retain a pointer or reference derived from an ABI argument;
* contain panic boundaries around every export and host callback;
* document provenance, ownership, thread, and lifetime requirements for each unsafe block.

The plugin SDK and host enable `unsafe_op_in_unsafe_fn`. Architecture checks reject public safe
contract types containing raw callback state, new `unsafe impl Send/Sync` declarations outside an
explicit allowlist, and unsafe code outside approved adapter modules.

## Performance gates

Each migration change records release-mode before/after results for:

* typed static invocation latency;
* dynamic invocation latency;
* event publication throughput and tail latency;
* parallel invocation scaling;
* shell recording throughput and time to first presentation;
* terminal cleanup latency;
* allocations and bytes copied per event;
* sustained daemon CPU and resident memory.

Benchmarks use alternating baseline/candidate rounds, medians plus tail percentiles, fixed payload
matrices, and warmup. CI fails when a statistically meaningful normal-path regression exceeds the
committed budget. The initial budget is no regression beyond measurement noise; a nonzero budget
requires an explicit reviewed rationale and evidence that the cost cannot be removed elsewhere.

The safe static path is expected to improve performance by removing current FFI-shaped work.
Dynamic callback safety must not add per-event thread/task creation, durable I/O, renderer work,
or a daemon-wide contended lock.

## Delivery sequence

1. Keep join-on-drop containment and add compile-time non-escape checks for activation registrars.
2. Add reproducible release benchmarks and store baseline methodology in repository scripts.
3. Introduce typed static invocation services and migrate bundled plugins without changing dynamic
   ABI behavior.
4. Add invocation worker scope and migrate shell recording and every asynchronous publisher.
5. Add ABI version 4 handle registry, lifecycle fencing, typed statuses, and compatibility tests.
6. Migrate dynamic plugins, retain ABI version 3 only as a bounded compatibility adapter, then
   remove version 3 after the declared compatibility window.
7. Centralize and mechanically audit unsafe adapters; add panic, sanitizer, stress, and race tests.
8. Evaluate process isolation for untrusted dynamic plugins separately. It is required to contain
   arbitrary native plugin faults, but it must not become a prerequisite for Bcode or silently
   move bundled plugins onto a slower path.

## Required validation

The architecture is complete only when tests prove:

* late callbacks after closure are rejected;
* closure waits for admitted callbacks;
* handles are not reused;
* cancellation/error/panic paths join invocation workers;
* callback and close races are safe under stress and Loom-equivalent state-machine testing;
* malformed pointers and lengths fail before dereference;
* panics are converted to typed failures at every ABI boundary;
* ABI version mismatch fails closed;
* static and dynamic adapters produce equivalent typed outcomes;
* performance gates pass for serial and parallel workloads.
