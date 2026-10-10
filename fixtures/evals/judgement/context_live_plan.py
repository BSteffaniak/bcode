"""Preregister small live DEVELOPMENT schedules; never launches calls or writes history.

Input records describe earlier same-session canonical excerpts supplied by the real
execution worker. Validation here checks shape/byte equivalence, not canonical truth.
Only the server's bounded hydration and request capture establish actual admission.
No expected code or oracle cases enter this producer plan. Not a held-out freeze.
"""
import copy
import uuid

import context_scenarios as scenarios


def live_development_plan(selection, allowance, history_bindings, repetitions=2):
    """Return six-family off/on preparation bound to explicit historical inputs.

    history_bindings: pair_id -> off/on -> {session_id, turn_sequence, excerpts}.
    excerpts: optional candidate id -> {sequence, snippet}. All excerpts must be
    earlier, distinct, byte-identical original candidate content. task-policy is
    mandatory task content, NOT an invented eligible historical excerpt.
    Runtime must independently verify each reference's canonical source and that
    it is no longer present in mandatory context. Actual history hydration adds
    quotation framing: execution must capture real bytes/budget, not assume the
    256-byte synthetic optional-candidate contract measures its framed excerpts.
    """
    if type(repetitions) is not int or not 1 <= repetitions <= 4:
        raise ValueError("pilot repetitions must be 1..4")
    manifest = scenarios.equivalent_pairs(selection, allowance, repetitions, extended=True)
    if set(history_bindings) != {p["pair_id"] for p in manifest["pairs"]}:
        raise ValueError("exact schedule history bindings required")
    jobs = []
    used_sessions = set()
    for pair in manifest["pairs"]:
        item = scenarios.scenario(pair["scenario_id"], extended=True)
        optional = {c["id"]: c for c in item["candidates"] if not c["required"]}
        binding = history_bindings[pair["pair_id"]]
        if set(binding) != {"off", "on"}:
            raise ValueError("both history arms required")
        # Arm-specific session IDs cannot be equal because history accumulates.
        if binding["off"]["session_id"] == binding["on"]["session_id"]:
            raise ValueError("distinct fresh arm sessions required")
        pair["history_binding_digest"] = scenarios.digest(binding)
        pair["shared_input_digest"] = scenarios.digest({
            "declared_input_digest": pair["shared_input_digest"],
            "history_binding_digest": pair["history_binding_digest"]})
        for arm in pair["arms"]:
            actual = binding[arm]
            if set(actual) != {"session_id", "turn_sequence", "excerpts"}:
                raise ValueError("unsupported history binding fields")
            if not isinstance(actual["session_id"], str):
                raise ValueError("string session UUID required")
            canonical_session = str(uuid.UUID(actual["session_id"]))
            if canonical_session in used_sessions:
                raise ValueError("fresh session required for every scheduled arm")
            used_sessions.add(canonical_session)
            if type(actual["turn_sequence"]) is not int or actual["turn_sequence"] <= 0:
                raise ValueError("positive turn sequence required")
            if set(actual["excerpts"]) != set(optional):
                raise ValueError("exact optional excerpts required")
            references = []
            for identity in pair["source_order"]:
                if identity not in optional:
                    continue
                excerpt = actual["excerpts"][identity]
                if (set(excerpt) != {"sequence", "snippet"}
                        or type(excerpt["sequence"]) is not int
                        or not 0 < excerpt["sequence"] < actual["turn_sequence"]
                        or excerpt["snippet"] != optional[identity]["content"]):
                    raise ValueError("invalid earlier original excerpt")
                references.append({"session_id": actual["session_id"], "sequence": excerpt["sequence"]})
            if len({r["sequence"] for r in references}) != len(references):
                raise ValueError("duplicate historical event")
            jobs.append({"task_id": pair["pair_id"], "repetition": 0, "arm": arm,
                         "scenario_id": pair["scenario_id"], "oracle_id": item["oracle_id"],
                         "test_ids": scenarios.test_ids(item["scenario_id"]),
                         "pairing_id": pair["shared_input_digest"],
                         "control": item["control"],
                         "session_id": actual["session_id"],
                         "historical_context": references,
                         "mandatory_policy": next(c["content"] for c in item["candidates"] if c["required"]),
                         "prompt": item["prompt"], "starter_files": copy.deepcopy(item["starter_files"]),
                         "declared_optional_budget_bytes": item["optional_budget_bytes"]})
    return {"schema_version": 1, "split": "development", "independent_review": "pending",
            "evidence_scope": "preregistered_not_executed", "manifest": manifest,
            "tasks": scenarios.comparator_tasks(manifest), "jobs": jobs,
            "acceptance_requirements": ["canonical_reference_verification", "candidate_absent_from_mandatory_context",
                                        "real_framed_byte_pressure", "off_zero_optional_calls", "on_application_or_explicit_abstention",
                                        "no_regression_on_negative_redundant_contract",
                                        "actual_base_model_context", "independent_artifact_tests"],
            "limits": ["synthetic_numeric_microtasks", "three_original_families_not_independently_reviewed",
                       "new_contract_controls_are_development_only", "not_six_integration_coverage"]}
