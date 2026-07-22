# Tool result safety contract

Bcode keeps a tool's application response and its model-visible result separate. Every successful
or model-visible-error invocation crosses the same canonical runtime boundary before it can be
placed in model continuation context.

## Application and model views

* `ToolExecutionOutput.invocation` is the original application-visible `ToolInvocationResponse`.
  It is not truncated or redacted by the runtime and may contain `full_output`, typed application
  errors, artifacts, or diagnostics. Applications must protect access to it as sensitive data.
* `ToolExecutionOutput.model_result` is the only result added to model context.
* `ToolExecutionOutput.model_transform` records counts for redactions, text truncations, excess
  content items, oversized inline binary omissions, and unsafe/oversized reference omissions. It
  never records the sensitive values themselves.
* Runtime `ToolResult` events are generated from the transformed model result, so observers and
  continuation context see the same value. Invocation lifecycle and application result access are
  unchanged.

## Default bounds

`ToolResultPolicy::default()` applies to all inline, plugin-backed, and custom-invoker results:

* each model-visible UTF-8 text field is limited to 64 KiB and truncated at a character boundary;
* at most 64 structured content items are retained, preserving producer order;
* inline image content is limited to 5 MiB decoded bytes using the greater of declared metadata
  and the encoded-size estimate;
* application-supplied exact sensitive values are replaced with `[REDACTED]` in text;
* inline binary content containing a configured sensitive value is omitted rather than rewritten;
* references whose path, MIME type, or source path requires redaction or exceeds the text bound are
  omitted rather than corrupted into an invalid reference.

Applications configure the policy with `AgentRuntime::with_tool_result_policy`. Limits are non-zero.
Exact redaction values are held in memory, omitted from `Debug`, and never included in transformation
reports.

## Content semantics

Text, inline image bytes, and image references remain typed. Oversized inline binary fields are
omitted from the model result but remain available in the application response. References are not
dereferenced, copied, or rewritten by the runtime. Artifact results remain application-visible;
tools must explicitly return model-safe text or typed content when the model needs artifact data.

Handler failures configured with `ToolFailurePolicy::ReturnToModel` pass through the same text
redaction and size policy. Typed `ToolApplicationError` keeps application diagnostics in the
structured invocation result while sending only its explicit `model_message` through this boundary.
Infrastructure failures that abort invocation remain runtime errors and are not converted into
application success.
