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

## Current session-search release measurements (2026-08-03)

Release-mode validation on macOS arm64 recorded:

* Startup harness, 5 serial samples and 4 concurrent clients:
  * warm verified handshake p50/p95: 0.743/0.939 ms;
  * warm status p50/p95: 58.262/73.993 ms against 49.420/55.447 ms process baseline;
  * cached cold p50/p95: 368.414/384.227 ms;
  * first cold p50/p95: 357.017/396.619 ms;
  * search disabled cold p50/p95: 345.524/426.048 ms;
  * search enabled with empty default provider p50/p95: 332.733/382.439 ms;
  * 4-client concurrent cold wall time: 328.117 ms.
* Tantivy, 25,000 records / 26,811,315 normalized bytes:
  * ingestion 9.387 s (2,663 records/s);
  * index 4,694,193 bytes, 0.175x amplification;
  * query p50/p95/p99 0.713/11.741/27.746 ms;
  * commit p50/p95/p99 84.581/153.853/200.524 ms;
  * reopen 3.176 ms.
* Compressed provider, 25,000 records / 21,088,894 normalized bytes:
  * ingestion 9.081 s;
  * 342,373 compressed bytes, 1.6% ratio;
  * warm query p50/p95/p99 0.184/0.230/0.855 ms;
  * cold query p50/p95/p99 0.422/2.277/2.277 ms;
  * two concurrent scans 0.725 ms.
* Canonical hydration, 1,000-event session, 20 hits, 20 runs: p50/p95/p99
  2.294/23.024/23.024 ms, below the 100 ms p95 budget.
* Incremental ingestion, 16 sessions x 256 events: 198.914 ms total lag, 20,591 events/s,
  32 bounded provider calls.
* Complete real-provider orchestration, 16 sessions and both providers:
  * first complete backfill 2.132 s;
  * unchanged idempotent rerun 4.877 ms;
  * ordinary Tantivy query admitted during backfill 24.058 ms;
  * final provider roots 18,135 Tantivy bytes and 7,088 compressed-provider bytes;
  * `/usr/bin/time -l`: 0.23 s user CPU, 0.62 s system CPU, 99,368,960-byte maximum RSS
    (76,907,192-byte peak footprint).
* Actual-binary cancellation over 120 historical sessions reached terminal cancelled state within
  the one-second observation interval after cancellation request.

Startup remains within the locked warm-handshake, warm-overhead, cached-cold, first-cold, and
concurrent-launch budgets. Provider query, amplification, hydration, cancellation, cooperative-query,
and incremental-ingestion measurements remain within their existing focused budgets. CPU and peak
RSS are recorded for the complete multi-provider orchestration process. Provider-local CPU/RSS are
included in that in-process sample rather than attributed separately.

## Initial budgets

These product budgets remain locked:

* In-process warm connect + verified `Hello`: p95 **<= 5 ms**.
* Warm CLI daemon-backed command overhead above same-artifact `--version`: p95 **<= 25 ms**.
* Cached cold connect, measured from an already-running client process: p95 **<= 500 ms**.
* First cold connect for an approximately 106 MB artifact, measured from an already-running client process: p95 **<= 1.5 s**.
* Concurrent cold launch: exactly one daemon process; all 16 clients complete within the first-cold p95 plus **500 ms** coordination allowance.

The harness blocks on warm-handshake and cached-cold p95. Full CLI totals remain host-sensitive and are normalized against the same binary's process baseline. First-cold remains an open optimization target; it must not be marked complete until its p95 meets budget without weakening exact-image integrity, full readiness, or release architecture.
