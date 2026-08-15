# Workflow Control Center

Open the plugin-owned control center with `/workflow`.

The surface subscribes to durable workflow updates and shows:

* all bounded durable runs and live status;
* selected run nodes, waits, approvals, attempts, typed outputs, and terminal state;
* explicit degraded, disconnected, and resync-required state.

## Keys

* `h` / `l` or Left / Right: select a run
* `j` / `k` or Down / Up: select a node
* `p`: pause or resume the selected run
* `c`: cancel the selected run
* `r`: retry the selected failed node attempt
* `a` / `d`: approve or deny the first pending mutation or ordinary approval
* `i`: enter JSON for the selected run's pending input wait; Enter submits, Escape cancels
* `o`: open the selected run's background child session
* `q` or Escape: close the surface

Actions use canonical plugin/application command paths and do not close the control center.

## Scriptable typed outputs

Use the independently supported CLI path to read canonical checksum-verified node outputs:

```sh
bcode workflow run-output --run-id <RUN_ID> --limit 100
```

The response includes node and activation identity, schema ID/version, checksum, artifact
reference when present, and the validated JSON value. Reviewer verdicts and findings therefore do
not require reading child-session event history.
