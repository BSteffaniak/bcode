"""Explicit development-only correction to optional admission budget semantics.

Protected bytes are outside the budget; physical size remains nonzero for capture.
No observations/provider answers are used and frozen held-out is never opened for writing.
"""
import itertools
import json
from pathlib import Path


def correct(data):
    if data["schema_version"] != 3 or data["split"] != "development":
        raise ValueError("only development v3 may be corrected")
    for case in data["cases"]:
        expected = case["expected"]
        if expected["kind"] != "ranking":
            continue
        candidates = case["input"]["candidates"]
        sizes = {c["id"]: c["size_units"] for c in candidates}
        if case["feature"] == "recommendations":
            # Producer returns a full permutation; it does not perform admission.
            expected.update(costs=sizes, budget=sum(sizes.values()),
                            ideal_admission_gain=sum(2 ** r - 1 for r in expected["relevance"].values()),
                            k=max(1, sum(r > 0 for r in expected["relevance"].values())))
            case["input"]["budget_units"] = expected["budget"]
            continue
        costs = {c["id"]: 0 if c["protected"] else sizes[c["id"]] for c in candidates}
        optional_total = sum(costs.values())
        if optional_total < 2:
            raise ValueError("insufficient optional candidates for admission pressure")
        budget = min(max(costs.values()), optional_total - 1)
        ids = list(costs)
        feasible = []
        for bits in itertools.product((False, True), repeat=len(ids)):
            admitted = {i for i, bit in zip(ids, bits) if bit}
            if (set(expected["required"]) <= admitted
                    and sum(costs[i] for i in admitted) <= budget):
                feasible.append(sum(2 ** expected["relevance"][i] - 1 for i in admitted))
        expected.update(costs=costs, budget=budget, ideal_admission_gain=max(feasible))
        case["input"]["budget_units"] = budget
    return data


if __name__ == "__main__":
    path = Path(__file__).resolve().parent / "development.json"
    data = correct(json.loads(path.read_text()))
    path.write_text(json.dumps(data, indent=2) + "\n")
    print("development optional-budget truth corrected; held-out untouched")
