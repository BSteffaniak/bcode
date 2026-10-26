#!/usr/bin/env python3
"""Frozen retention rubric scoring; human adjudication, never keyword inference.

Input is blinded review packets, not raw provider output. A fake producer can exercise
this scorer but can never establish natural-language quality. Unknown/missing judgments
score zero; unscorable evidence blocks acceptance independently of the numerical score.
"""
import argparse
from collections import defaultdict
import hashlib
import json
from pathlib import Path

CORPUS = Path(__file__).with_name("retention_cases.json")


def packet_sha256(packet):
    data = {key: value for key, value in packet.items() if key not in ("reviews", "global_reviews")}
    return hashlib.sha256(json.dumps(data, sort_keys=True, ensure_ascii=False).encode()).hexdigest()


def judgment_supports(judgment, text):
    """Validate a human judgment's evidence location, not the proposition's semantics."""
    if judgment.get("decision") not in ("supported", "unsupported", "unscorable"):
        return False
    excerpts = judgment.get("excerpts", [])
    return (judgment["decision"] == "supported" and bool(excerpts)
            and all(isinstance(excerpt, str) and excerpt and excerpt in text for excerpt in excerpts)
            and bool(judgment.get("rationale", "").strip()))


def score(corpus, packets, repetitions):
    if repetitions < 3:
        raise ValueError("at least three paired repetitions required")
    expected = {(rep, case["id"], batch["cycle"]): batch for rep in range(repetitions)
                for case in corpus["cases"] for batch in case["batches"]}
    indexed = {}
    for packet in packets:
        key = (packet["repetition"], packet["case"], packet["cycle"])
        if key not in expected or key in indexed:
            raise ValueError("unknown or duplicate blinded review packet")
        if packet.get("format") != "bcode.compaction.retention-review-packet.v1":
            raise ValueError("unsupported review packet version")
        indexed[key] = packet
    groups = {name: defaultdict(lambda: [0, 0]) for name in ("case", "cycle", "category", "repetition_case")}
    total = passed = critical_failures = 0
    unscorable = []
    critical = {}
    manifests = {}
    global_failures = []
    for key, batch in expected.items():
        packet = indexed.get(key, {})
        summary = json.dumps(packet.get("summary"), ensure_ascii=False, sort_keys=True)
        probe = packet.get("probe", "")
        reviews = packet.get("reviews", [])
        oracle_ids = {check["id"] for check in batch["oracle"]}
        if len({review["check_id"] for review in reviews}) != len(reviews) or any(
                review["check_id"] not in oracle_ids for review in reviews):
            raise ValueError("duplicate or unknown check review")
        reviewed = {review["check_id"]: review for review in reviews}
        coverage = packet.get("coverage", {})
        valid = (packet.get("producer") == "live" and packet.get("protocol_valid") is True
                 and not packet.get("blockers") and packet.get("selected_manifest")
                 and packet.get("source_sha256") and packet.get("corpus_sha256") ==
                 hashlib.sha256(CORPUS.read_bytes()).hexdigest())
        if not valid:
            unscorable.append(list(key))
        global_reviews = packet.get("global_reviews", [])
        if len({review["index"] for review in global_reviews}) != len(global_reviews):
            raise ValueError("duplicate global review")
        if any(review["index"] not in range(len(corpus["protocol"]["global_critical_checks"]))
               for review in global_reviews):
            raise ValueError("unknown global check")
        globals_by_index = {review["index"]: review for review in global_reviews}
        for index, _ in enumerate(corpus["protocol"]["global_critical_checks"]):
            review = globals_by_index.get(index, {})
            if not (valid and review.get("method") == "human_adjudicated"
                    and review.get("grader", "").strip() and review.get("blind_label", "").strip()
                    and review.get("disagreements_resolved") is True
                    and review.get("packet_sha256") == packet_sha256(packet)
                    and review.get("decision") == "supported" and review.get("rationale", "").strip()
                    and {"portable_summary", "durable_checkpoints", "source_manifest", "probe_output"}
                        <= set(review.get("artifacts_inspected", []))):
                global_failures.append([*key, index])
        manifests[str(key)] = packet.get("covered_source_hashes")
        for check in batch["oracle"]:
            review = reviewed.get(check["id"], {})
            # Exact excerpts prevent fabricated evidence; human attribution/uncertainty
            # adjudication remains necessary and is not inferred by this check.
            ok = bool(valid and set(check["sources"]) <= set(coverage)
                      and review.get("method") == "human_adjudicated"
                      and review.get("grader", "").strip() and review.get("blind_label", "").strip()
                      and review.get("disagreements_resolved") is True
                      and review.get("packet_sha256") == packet_sha256(packet)
                      and judgment_supports(review.get("summary", {}), summary)
                      and judgment_supports(review.get("probe", {}), probe))
            total += 1
            passed += int(ok)
            if check["critical"]:
                critical[f"{key}:{check['id']}"] = ok
                critical_failures += int(not ok)
            for group, label in (("case", key[1]), ("cycle", f"{key[1]}:{key[2]}"),
                                 ("category", check["category"]), ("repetition_case", f"{key[0]}:{key[1]}")):
                counts = groups[group][label]
                counts[0] += int(ok)
                counts[1] += 1
    scores = {name: {key: 100 * count[0] / count[1] for key, count in values.items()}
              for name, values in groups.items()}
    overall = 100 * passed / total
    return {"format": "bcode.compaction.retention-score.v1", "score": overall,
            "passed_checks": passed, "total_checks": total, "critical_failures": critical_failures,
            "scores": scores, "critical": critical, "covered_source_hashes": manifests,
            "unscorable": unscorable, "global_critical_failures": global_failures,
            "accepted": not unscorable and not global_failures and critical_failures == 0 and overall >= 95
                        and all(value >= 90 for value in scores["repetition_case"].values())}


def compare(baseline, candidate):
    """Absolute candidate gates AND paired per-case/critical non-regression."""
    equal_coverage = (baseline["covered_source_hashes"] == candidate["covered_source_hashes"]
                      and all(baseline["covered_source_hashes"].values()))
    same_pairs = baseline["scores"]["repetition_case"].keys() == candidate["scores"]["repetition_case"].keys()
    no_regression = same_pairs and all(candidate["scores"]["repetition_case"][key] >= value
                                      for key, value in baseline["scores"]["repetition_case"].items())
    critical_ok = all(candidate["critical"].get(key, False) for key, ok in baseline["critical"].items() if ok)
    return {"accepted": bool(candidate["accepted"] and equal_coverage and no_regression and critical_ok),
            "equal_canonical_coverage": bool(equal_coverage), "no_paired_case_regression": no_regression,
            "critical_nonregression": critical_ok, "delta": candidate["score"] - baseline["score"]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("packets", type=Path, help="JSON list of blinded review packets")
    parser.add_argument("--repetitions", type=int, required=True)
    args = parser.parse_args()
    result = score(json.loads(CORPUS.read_text()), json.loads(args.packets.read_text()), args.repetitions)
    print(json.dumps(result, indent=2))
    return 0 if result["accepted"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
