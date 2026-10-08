"""Development-only ordinary coding inputs; no provider/runtime execution.

Candidates are input to the real context consumer, NOT pre-admitted model context.
The coordinator must prove ordinary admission and actual base-model context separately.
The protected policy forbids searching the hidden contract elsewhere during the task.
Independent review remains pending; this corpus is not frozen held-out evidence.
These are synthetic arithmetic microtasks with restricted pure-expression artifacts,
not representative repository maintenance or proof of live feature benefit.
Grep's mandatory result route is not an optional-context trigger.
"""
import argparse
import copy
import hashlib
import json
from pathlib import Path
import random


def digest(value):
    """SHA-256 of canonical UTF-8 JSON (sorted keys, compact separators)."""
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":"),
                                     ensure_ascii=False).encode()).hexdigest()


_DEFINITIONS = (
    ("retry-window-v1", "retry_delay(attempt)",
     "Implement retry_delay(attempt), returning the delay in milliseconds for a nonnegative integer retry attempt.",
     "Current client retry policy: attempt zero waits 75 ms; each next attempt doubles the delay. Cap at 900 ms. Attempts are nonnegative integers.",
     "Archived client retry policy: attempt zero waits 100 ms; subsequent delays grow by 100 ms. Cap at 500 ms."),
    ("local-minute-v1", "utc_minute(local_minute)",
     "Implement utc_minute(local_minute), converting an integer minute-of-day to UTC minute-of-day for the deployed service.",
     "Deployed service clock contract: local time is UTC plus 345 minutes. Subtract this offset and wrap modulo 1440. Inputs are integers from 0 through 1439.",
     "Legacy service clock: local time is UTC minus 240 minutes. Add 240 minutes and wrap modulo 1440."),
    ("bucket-route-v1", "bucket_for(key)",
     "Implement bucket_for(key), returning the deployed routing bucket for a nonnegative integer key.",
     "Active routing contract: bucket = (key * 7 + 3) modulo 11. Keys are nonnegative integers; the multiplier and salt are protocol constants, not tunable.",
     "Retired routing contract: bucket = key modulo 8. Historical dashboards still use this formula; it must not govern current routing."),
)


def corpus():
    """Return fresh public development inputs; no test cases or expected artifacts."""
    scenarios = []
    for identity, signature, task, current, archive in _DEFINITIONS:
        optional = [
            {"id": "contract-current", "content": current, "required": False},
            {"id": "contract-archive", "content": archive, "required": False},
            {"id": "observability", "content": "Operations note: dashboards refresh every 60 seconds. Diagnostic sampling is enabled. This note does not define computation or routing semantics.", "required": False},
        ]
        # Exactly one optional snippet fits, irrespective of its text length.
        # ASCII space padding is real physical input, not guessed token cost.
        for candidate in optional:
            candidate["content"] = candidate["content"].ljust(256)
        candidates = [{"id": "task-policy", "content": "Implement only solution.py. Use the admitted current contract; do not search for omitted contracts or tests. Pure numeric Python, no imports, attribute access or external effects.", "required": True}] + optional
        scenarios.append({"scenario_id": identity, "feature": "context_reranking",
                          "prompt": task + " Use the admitted deployed contract. Use one return expression with arithmetic and min/max/abs if needed.",
                          "starter_files": {"solution.py": "def " + signature + ":\n    raise NotImplementedError\n"},
                          "candidates": candidates, "optional_budget_bytes": 256,
                          "artifact": "solution.py", "oracle_id": identity + "-oracle-v1"})
    return {"schema_version": 1, "corpus_id": "context-coding-development-v1",
            "split": "development", "independent_review": "pending", "scenarios": scenarios}


def scenario(scenario_id):
    """Look up an exact scenario identity; unknown identities fail closed."""
    for item in corpus()["scenarios"]:
        if item["scenario_id"] == scenario_id:
            return item
    raise ValueError("unknown scenario identity")


def equivalent_pairs(selection, allowance, repetitions=2, source_identities=None):
    """Declare pair equivalence, not provider verification or execution evidence.

    selection and allowance are explicit JSON objects from the coordinator.
    Every repetition uses a deterministic shuffled order and counterbalanced arm order.
    A comparison still needs verified actual runtime selection and context provenance.
    """
    if not isinstance(selection, dict) or not selection or not isinstance(allowance, dict) or not allowance:
        raise ValueError("explicit selection and allowance objects required")
    if type(repetitions) is not int or not 1 <= repetitions <= 32:
        raise ValueError("repetitions must be between 1 and 32")
    data = corpus()
    pairs = []
    for item in data["scenarios"]:
        for repetition in range(repetitions):
            ids = [candidate["id"] for candidate in item["candidates"]]
            seed = int(digest([item["scenario_id"], repetition])[:16], 16)
            random.Random(seed).shuffle(ids)
            shared = {"scenario": item, "source_order": ids,
                      "selection": selection, "allowance": allowance}
            if source_identities is not None:
                if set(source_identities) != {s["scenario_id"] for s in data["scenarios"]}:
                    raise ValueError("producer identities required for every scenario")
                shared["context_selection_request"] = context_selection_request(
                    item["scenario_id"], ids, source_identities[item["scenario_id"]])
            pairs.append({"pair_id": item["scenario_id"] + "-r" + str(repetition),
                          "scenario_id": item["scenario_id"], "repetition": repetition,
                          "source_order": ids, "shared_input_digest": digest(shared),
                          "selection": copy.deepcopy(selection), "allowance": copy.deepcopy(allowance),
                          "arms": ["off", "on"] if repetition % 2 == 0 else ["on", "off"]})
    return {"schema_version": 1, "corpus_digest": digest(data),
            "oracle_digest": hashlib.sha256(Path(__file__).with_name("context_oracle.py").read_bytes()).hexdigest(),
            "producer_identities": copy.deepcopy(source_identities),
            "adapter_binding": "producer_bound" if source_identities is not None else "unbound_not_execution_ready",
            "scenario_family": "ordinary_coding", "evidence_scope": "planned_not_executed",
            "pairs": pairs}


def context_selection_request(scenario_id, source_order, source_identities):
    """Map authored inputs to the exact ContextSelectionRequest wire fields.

    The real consumer supplies each {source, provenance} producer identity. These
    must remain pinned across both arms; no file/search provenance is fabricated.
    required -> protected (which also pins original slot), content -> snippet.
    There is NO separate `pinned` field in JudgementContextCandidate v1.
    Output is preparation only: it neither calls the typed route nor proves
    optional admission. Mandatory grep responses cannot substitute for this route.
    """
    item = scenario(scenario_id)
    by_id = {candidate["id"]: candidate for candidate in item["candidates"]}
    if not isinstance(source_order, list) or sorted(source_order) != sorted(by_id):
        raise ValueError("exact candidate permutation required")
    if not isinstance(source_identities, dict) or set(source_identities) != set(by_id):
        raise ValueError("real producer identities required for every candidate")
    candidates = []
    for identity in source_order:
        origin = source_identities[identity]
        if (not isinstance(origin, dict) or set(origin) != {"source", "provenance"}
                or origin["source"] not in {"file", "search", "history", "memory", "tool_result"}
                or not isinstance(origin["provenance"], str)
                or not origin["provenance"].strip()
                or len(origin["provenance"].encode()) > 1024):
            raise ValueError("invalid producer source identity")
        authored = by_id[identity]
        candidates.append({"id": scenario_id + ":" + identity,
                           "source": origin["source"], "provenance": origin["provenance"],
                           "snippet": authored["content"], "protected": authored["required"]})
    return {"task": item["prompt"], "candidates": candidates,
            "budget_bytes": item["optional_budget_bytes"]}


def test_ids(scenario_id):
    """Stable independently authored assertion groups, never consumer inputs."""
    scenario(scenario_id)
    return [scenario_id + ":artifact-shape", scenario_id + ":numeric-contract"]


def comparator_tasks(manifest):
    """Map each scheduled permutation to one task with exactly one repetition.

    The existing manifest's repetition is a permutation schedule index, NOT an
    additional repetition count. Comparator observations use repetition=0 for
    each unique pair_id. Three families cannot meet the five-family promotion gate.
    """
    return [{"task_id": pair["pair_id"], "family_id": pair["scenario_id"],
             "feature": "context_reranking", "pairing_id": pair["shared_input_digest"],
             "oracle_id": scenario(pair["scenario_id"])["oracle_id"],
             "test_ids": test_ids(pair["scenario_id"]), "repetitions": 1,
             "requires_workflow_dispatch": False} for pair in manifest["pairs"]]


def materialize(scenario_id, workspace, source_order):
    """Write only starter and consumer candidates into a fresh existing directory.

    No oracle source/expected answers are copied. This does not launch or simulate
    the context consumer. Existing files and symlink roots are rejected.
    """
    item = scenario(scenario_id)
    ids = [candidate["id"] for candidate in item["candidates"]]
    if not isinstance(source_order, list) or sorted(source_order) != sorted(ids):
        raise ValueError("source_order must be an exact candidate permutation")
    root = Path(workspace)
    if root.is_symlink() or not root.is_dir() or any(root.iterdir()):
        raise ValueError("fresh existing non-symlink directory required")
    for name, content in item["starter_files"].items():
        (root / name).write_text(content)
    by_id = {candidate["id"]: candidate for candidate in item["candidates"]}
    payload = {key: value for key, value in item.items() if key != "starter_files"}
    payload["candidates"] = [by_id[identity] for identity in source_order]
    (root / "consumer-input.json").write_text(json.dumps(payload, indent=2) + "\n")
    return payload


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--corpus", action="store_true", required=True)
    args = parser.parse_args()
    if args.corpus:
        print(json.dumps(corpus(), indent=2))


if __name__ == "__main__":
    main()
