#!/usr/bin/env python3
"""Summarize newline-delimited daemon startup measurements."""

from __future__ import annotations

import argparse
import json
import math
import platform
import statistics
from pathlib import Path


def percentile(samples: list[int], fraction: float) -> int:
    """Return the nearest-rank percentile for sorted integer samples."""
    if not samples:
        raise ValueError("at least one sample is required")
    rank = max(1, math.ceil(fraction * len(samples)))
    return samples[rank - 1]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--scenario", required=True)
    parser.add_argument("--profile", required=True)
    parser.add_argument("--command", required=True)
    parser.add_argument("--artifact-bytes", required=True, type=int)
    parser.add_argument("--p95-limit-us", type=int)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("samples", type=Path)
    args = parser.parse_args()

    samples = sorted(
        int(line.strip())
        for line in args.samples.read_text(encoding="utf-8").splitlines()
        if line.strip()
    )
    if not samples:
        raise SystemExit("measurement file contained no samples")

    report = {
        "schema_version": 1,
        "scenario": args.scenario,
        "profile": args.profile,
        "command": args.command,
        "artifact_bytes": args.artifact_bytes,
        "machine": {
            "system": platform.system(),
            "release": platform.release(),
            "machine": platform.machine(),
            "processor": platform.processor(),
        },
        "sample_count": len(samples),
        "samples_us": samples,
        "min_us": samples[0],
        "p50_us": percentile(samples, 0.50),
        "p95_us": percentile(samples, 0.95),
        "max_us": samples[-1],
        "mean_us": round(statistics.fmean(samples)),
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(
        f"{args.scenario}: n={len(samples)} "
        f"p50={report['p50_us']}us p95={report['p95_us']}us "
        f"min={report['min_us']}us max={report['max_us']}us"
    )
    if args.p95_limit_us is not None and report["p95_us"] > args.p95_limit_us:
        raise SystemExit(
            f"{args.scenario} p95 {report['p95_us']}us exceeds "
            f"{args.p95_limit_us}us limit"
        )


if __name__ == "__main__":
    main()
