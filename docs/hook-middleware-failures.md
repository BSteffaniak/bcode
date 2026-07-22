# Hook and middleware failure boundaries

Bcode treats application hooks and model middleware as synchronous extension boundaries. Panics do
not unwind across the SDK API or a runtime-created stream task.

Each individual callback invocation is wrapped with `catch_unwind(AssertUnwindSafe(...))`:

* request middleware panics become `BcodeError::Hook` before cache lookup or provider invocation;
* response middleware panics become one terminal error after provider/tool lifecycle cleanup;
* before-model hook panics stop before middleware/provider execution;
* after-model hook panics replace the successful application response only after provider/tool
  cleanup has completed;
* before-tool hook panics prevent that tool invocation; and
* after-tool hook panics become canonical host-extension failures rather than escaping the task.

The typed error identifies the boundary and includes a string panic payload. Non-string payloads are
reported only as `non-string panic payload`; the SDK does not attempt to expose arbitrary panic
objects. Rust's configured panic hook may still log the panic at the point it is caught. Applications
that need different panic logging should configure the standard process panic hook.

Containment does not make callbacks transactional. Middleware and hooks must not mutate external
state before an operation they may panic during unless the application provides its own transaction
or idempotency boundary. Async tool handlers and provider implementations are separate runtime
extension contracts and return typed errors through their futures; this document does not claim
that arbitrary panics inside those futures are recoverable.

Focused tests cover buffered and streaming request/response middleware, model and tool hooks,
provider cleanup, exactly-one terminal delivery, concurrent callback execution, and cancellation
racing a contained callback panic.
