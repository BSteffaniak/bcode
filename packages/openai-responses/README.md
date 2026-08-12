# bcode-openai-responses

OpenAI Responses API wire format types for Bcode.

This crate owns the serialized request/response shapes used by the OpenAI Responses API so that
multiple provider integrations can share one implementation of the wire format. It is a leaf
crate: it holds portable data types and lightweight helpers on owned values only.

It deliberately contains no provider behavior. Authentication, endpoint construction, HTTP
transport, retry interpretation, conversation-reuse policy, and dialect-specific request shaping
remain owned by the individual provider integrations that consume these types.
