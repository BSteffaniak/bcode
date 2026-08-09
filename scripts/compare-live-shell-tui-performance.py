#!/usr/bin/env python3
"""Compare deterministic work shape and timing for Bcode TUI baseline JSONL files."""

import argparse
import json
import sys
from collections import defaultdict

IDENTITY_FIELDS = {
    "shell_output": ("output_bytes", "chunk_bytes"),
    "shell_replay": ("output_bytes", "chunk_bytes"),
    "transcript_visual_update": ("transcript_entries",),
    "renderer_parse_layout": ("format",),
    "telemetry_overhead": ("enabled",),
    "pending_live_fanout": ("workload",),
}
TIMING_FIELDS = {
    "shell_output": (
        "wall_us",
        "maximum_interarrival_us",
        "average_committed_delta",
    ),
    "shell_replay": ("wall_us",),
    "transcript_visual_update": ("wall_us", "sync_us"),
    "renderer_parse_layout": ("parse_layout_us",),
    "telemetry_overhead": ("wall_us",),
    "pending_live_fanout": (
        "wall_us",
        "push_latency_ns_p50",
        "push_latency_ns_p95",
        "push_latency_ns_p99",
    ),
}


def load(path):
    with open(path, encoding="utf-8") as source:
        records = [json.loads(line) for line in source if line.strip()]
    if not records or records[0].get("kind") != "live_shell_tui_performance_baseline":
        raise ValueError(f"{path}: unexpected baseline header")
    return records


def keyed(records):
    result = {}
    for record in records[1:]:
        domain = record.get("domain")
        fields = IDENTITY_FIELDS.get(domain)
        if fields is None:
            raise ValueError(f"unknown baseline domain: {domain!r}")
        key = (domain,) + tuple(record.get(field) for field in fields)
        if key in result:
            raise ValueError(f"duplicate baseline case: {key!r}")
        result[key] = record
    return result


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("baseline")
    parser.add_argument("candidate")
    parser.add_argument("--max-timing-regression-percent", type=float, default=25.0)
    parser.add_argument("--output")
    args = parser.parse_args()

    baseline = keyed(load(args.baseline))
    candidate = keyed(load(args.candidate))
    if baseline.keys() != candidate.keys():
        missing = sorted(set(baseline) - set(candidate))
        extra = sorted(set(candidate) - set(baseline))
        raise SystemExit(f"workload case mismatch: missing={missing!r} extra={extra!r}")

    structural_mismatches = []
    timing = []
    for key in sorted(baseline, key=repr):
        before = baseline[key]
        after = candidate[key]
        domain = key[0]
        ignored = set(IDENTITY_FIELDS[domain]) | set(TIMING_FIELDS[domain]) | {"domain"}
        structural_fields = sorted((set(before) | set(after)) - ignored)
        for field in structural_fields:
            if before.get(field) != after.get(field):
                structural_mismatches.append({
                    "case": key,
                    "field": field,
                    "baseline": before.get(field),
                    "candidate": after.get(field),
                })
        for field in TIMING_FIELDS[domain]:
            old = before.get(field)
            new = after.get(field)
            if not isinstance(old, (int, float)) or not isinstance(new, (int, float)):
                continue
            change = 0.0 if old == 0 and new == 0 else (float("inf") if old == 0 else (new - old) * 100.0 / old)
            timing.append({
                "case": key,
                "field": field,
                "baseline": old,
                "candidate": new,
                "change_percent": change,
                "passed": change <= args.max_timing_regression_percent,
            })

    report = {
        "schema_version": 1,
        "kind": "live_shell_tui_performance_comparison",
        "baseline": args.baseline,
        "candidate": args.candidate,
        "case_count": len(baseline),
        "structural_mismatches": structural_mismatches,
        "structural_equality": not structural_mismatches,
        "max_timing_regression_percent": args.max_timing_regression_percent,
        "timing": timing,
        "timing_passed": all(item["passed"] for item in timing),
    }
    encoded = json.dumps(report, indent=2, sort_keys=True, allow_nan=False)
    if args.output:
        with open(args.output, "w", encoding="utf-8") as destination:
            destination.write(encoded + "\n")
    print(encoded)
    if structural_mismatches:
        return 1
    if not report["timing_passed"]:
        return 2
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, json.JSONDecodeError) as error:
        raise SystemExit(str(error)) from error
