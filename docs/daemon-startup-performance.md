# Daemon startup performance baseline

Measured on 2026-07-31 from the `bcode/daemon-client-compatibility` worktree with the isolated daemon-startup harness.

## Environment

* Host: macOS Darwin 25.5.0, Apple arm64.
* Profile: Cargo `release` with the `distribution` feature set.
* Current artifact size: 101,639,600 bytes.
* Current full sample set: 20 serial samples; 16 clients for concurrent cold startup.
* Isolation: every cold sample used a fresh `BCODE_STATE_DIR`, `TMPDIR`, `BCODE_SOCKET`, and config. No invoking daemon variables were inherited.
* Command: `BCODE_STARTUP_TRACE=1 BCODE_DAEMON_PERF_SAMPLES=20 BCODE_DAEMON_PERF_CONCURRENT_CLIENTS=16 scripts/measure-daemon-startup.sh`.

Generated JSON reports and raw samples are written under `target/daemon-startup-performance/` and are intentionally build artifacts rather than committed source.

## Current results

| Scenario | p50 | p95 | Notes |
| --- | ---: | ---: | --- |
| In-process local connect + verified `Hello` | 0.511 ms | 4.319 ms | Measures from an already-constructed client and excludes process launch/config-context assembly. Passes the 5 ms budget. |
| Warm CLI `server status` | 65.184 ms | 89.508 ms | Includes CLI process launch and status request. |
| CLI process baseline (`--version`) | 52.048 ms | 64.683 ms | Warm status p95 overhead is 24.825 ms. Passes the 25 ms budget. |
| Cached cold connect | 252.993 ms | 263.271 ms | Existing verified image. Passes the 500 ms budget. |
| First cold connect | 1.739 s | 3.501 s | Fresh image publication. Does not meet the 1.5 s p95 budget. |
| 16-client concurrent cold wall time | 1.801 s | n/a | One daemon process/record; within first-cold budget plus 500 ms coordination allowance. |

## Current startup stage evidence

The lifecycle and server traces show:

* Copy-on-write clone from the retained exact artifact handle: under 1 ms on APFS.
* SHA-256 verification plus durable image synchronization: about 78 ms for the 101.6 MB image.
* Full server entry through verified `Hello`: about 40 ms.
* The remaining first-cold tail is dominated by launching the newly published 101.6 MB product image on this host; one captured run spent about 1.76 s between child spawn and server entry.

A measured daemon-sidecar experiment did not close this requirement. A 76 MB server-only product binary shared the exact embedded artifact ID and improved a five-sample first-cold median to 0.886 s, but its p95 was 1.648 s. The required 20-sample run measured 1.370 s p50 / 2.585 s p95 and 3.074 s concurrent cold wall time. Direct fresh-clone launch probes remained highly variable, and explicit `codesign --verify --strict` added cost without prewarming execution. The sidecar experiment was removed rather than adding release complexity that did not meet the locked tail budget.

The measured optimizations preserve the architecture boundaries:

* lifecycle readiness now uses the exact verified `Hello` contract rather than `ServerStatus`;
* readiness validates artifact, protocol, build, writer epoch, and event schema without runtime executable hashing;
* cached-image discovery verifies content-addressed metadata and cached bytes without first hashing the source artifact;
* SHA-256 uses the crate's AArch64 acceleration where available;
* first materialization attempts a retained-handle copy-on-write clone on supported macOS/Linux filesystems, verifies and synchronizes the clone, and falls back to fused stream copy plus hashing;
* successful `Hello` remains full readiness—no partial-ready protocol is justified.

## Initial budgets

These product budgets remain locked:

* In-process warm connect + verified `Hello`: p95 **<= 5 ms**.
* Warm CLI daemon-backed command overhead above same-artifact `--version`: p95 **<= 25 ms**.
* Cached cold connect, measured from an already-running client process: p95 **<= 500 ms**.
* First cold connect for an approximately 106 MB artifact, measured from an already-running client process: p95 **<= 1.5 s**.
* Concurrent cold launch: exactly one daemon process; all 16 clients complete within the first-cold p95 plus **500 ms** coordination allowance.

The harness blocks on warm-handshake and cached-cold p95. Full CLI totals remain host-sensitive and are normalized against the same binary's process baseline. First-cold remains an open optimization target; it must not be marked complete until its p95 meets budget without weakening exact-image integrity, full readiness, or release architecture.
