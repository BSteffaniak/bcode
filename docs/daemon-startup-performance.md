# Daemon startup performance baseline

Measured on 2026-07-31 from the `bcode/daemon-client-compatibility` worktree after adding the isolated daemon-startup harness.

## Environment

* Host: macOS Darwin 25.5.0, Apple arm64.
* Profile: Cargo `release` with the `distribution` feature set.
* Artifact size: 105,757,632 bytes.
* Samples: 20 per serial scenario; 16 clients for the concurrent cold scenario.
* Isolation: every cold sample used a fresh `BCODE_STATE_DIR`, `TMPDIR`, `BCODE_SOCKET`, and config. No invoking daemon variables were inherited.
* Command: `BCODE_STARTUP_TRACE=1 BCODE_DAEMON_PERF_SAMPLES=20 BCODE_DAEMON_PERF_CONCURRENT_CLIENTS=16 scripts/measure-daemon-startup.sh`.

Generated JSON reports and raw samples are written under `target/daemon-startup-performance/` and are intentionally build artifacts rather than committed source.

## Results

| Scenario | p50 | p95 | Notes |
| --- | ---: | ---: | --- |
| In-process local connect + verified `Hello` | 1.232 ms | 1.823 ms | Measured inside `server startup-probe`; excludes process launch. |
| Warm CLI `server status` | 676.060 ms | 803.212 ms | Includes full CLI process launch and status request. |
| CLI process baseline (`--version`) | 689.352 ms | 822.336 ms | Shows that warm command latency is dominated by launching the 105.8 MB distribution artifact on this host, not IPC. |
| Cached cold connect | 1.099 s | 1.344 s | Existing valid cached daemon image; includes client process launch, daemon launch, polling, and verified `Hello`. |
| First cold connect | 1.857 s | 3.101 s | Fresh state; includes image materialization and digest verification. |
| 16-client concurrent cold wall time | 5.123 s | n/a | One daemon record/process was observed after all clients completed. |

## Startup stage evidence

The captured first-startup trace reports:

| Server stage | Time |
| --- | ---: |
| Config | 1 ms |
| Plugin loading/activation | 13 ms (7 ms in plugin host) |
| Historical session recovery | <1 ms |
| IPC bind plus daemon record publication | 231 ms |
| Lazy session service construction | <1 ms |
| Remaining state construction/workflow recovery/readiness | 17 ms |
| Total server entry to ready | 265 ms |

The dominant end-to-end costs are outside plugin or session initialization:

1. Distribution artifact process launch: approximately 0.69 s p50.
2. First-cold image copy and SHA-256 verification: approximately 0.76 s p50 above cached cold.
3. Lifecycle readiness polling and repeated executable digest verification: cached cold is roughly 0.41 s above process baseline plus measured server initialization, and IPC bind itself spends about 0.23 s probing the endpoint.
4. Concurrent clients serialize behind startup coordination and each performs heavyweight client startup/identity work.

These results support retaining exact digest verification as cold-path evidence while removing executable hashing from warm routing, replacing fixed polling/retry windows, and preserving full `Hello` readiness. They do not justify a partial-readiness protocol.

## Follow-up measurements

A 2026-07-31 implementation sample (5 serial samples, 8 concurrent clients) removed redundant child-side executable hashing and skipped stale-socket confirmation when no endpoint path exists. It measured:

* warm handshake: 1.105 ms p50 / 2.244 ms p95;
* cached cold: 577.395 ms p50 / 623.110 ms p95;
* first cold: 1.385 s p50 / 2.392 s p95;
* 8-client concurrent cold: 3.128 s wall time.

This confirms warm handshake remains inside budget and first-cold median improved, but cached-cold p95, first-cold p95, and concurrent cold still require optimization. No partial-readiness protocol is justified: successful `Hello` remains the readiness boundary.

A later 2026-07-31 sample fused first-cold copy and hashing into one retained-handle read. With 5 serial samples and 8 concurrent clients it measured:

* warm handshake: 1.122 ms p50 / 2.050 ms p95;
* cached cold: 524.534 ms p50 / 552.124 ms p95;
* first cold: 1.373 s p50 / 2.682 s p95;
* 8-client concurrent cold: 2.903 s wall time.

Cached cold is now close to budget and first-cold median remains within budget, but tail and concurrent performance still require work. The full-readiness conclusion is unchanged.

## Initial budgets

These are product budgets for the replacement architecture, not allowances for the current workaround. They are locked from the measured component costs and must be enforced at the narrowest stable layer:

* In-process warm connect + verified `Hello`: p95 **<= 5 ms**.
* Warm CLI daemon-backed command overhead above same-artifact `--version`: p95 **<= 25 ms**.
* Cached cold connect, measured from an already-running client process: p95 **<= 500 ms**.
* First cold connect for an approximately 106 MB artifact, measured from an already-running client process: p95 **<= 1.5 s**.
* Concurrent cold launch: exactly one daemon process; all 16 clients complete within the first-cold p95 plus **500 ms** coordination allowance.

Full CLI totals remain host-sensitive because executable startup dominates on this machine. CI regression gates should therefore enforce the in-process handshake and lifecycle component budgets, while broader CLI totals remain benchmark reports normalized against the same binary's process baseline.
