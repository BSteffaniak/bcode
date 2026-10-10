"""Offline off/on pilot preparation for six distinct features. Never launches work."""
import copy
import hashlib
import json

import six_feature_development as development


def digest(value):
    """Canonical UTF-8 JSON hash, including exact original snippet bytes."""
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":"),
                                     ensure_ascii=False).encode("utf-8")).hexdigest()


def paired_plan(selection, allowance, repetitions=2):
    """Preregister shared inputs; runtime provenance and all live gates stay unknown.

    Selection/allowance are coordinator declarations, never verification or spend
    authorization. Counterbalance arm order; don't pick cases after viewing results.
    This one-case-per-feature pilot is insufficient for six-feature promotion.
    """
    if not isinstance(selection, dict) or not selection or not isinstance(allowance, dict) or not allowance:
        raise ValueError("explicit nonempty selection and allowance required")
    if type(repetitions) is not int or not 1 <= repetitions <= 4:
        raise ValueError("pilot repetitions must be 1..4")
    tasks = development.scenarios()
    pairs = []
    for index, item in enumerate(tasks):
        for repetition in range(repetitions):
            shared = {"task": item, "selection": selection, "allowance": allowance}
            modes = ["off", "on"] if (index + repetition) % 2 == 0 else ["on", "off"]
            pairs.append({"pair_id": item["scenario_id"] + "-r" + str(repetition),
                          "scenario_id": item["scenario_id"], "feature": item["feature"],
                          "shared_input_digest": digest(shared), "shared": copy.deepcopy(shared),
                          "arms": [{"mode": mode, "allow_live": False,
                                    "judgement_overlay": {f: "auto" if mode == "on" and f == item["feature"] else "off"
                                                          for f in development.FEATURES} | {"mode": "auto" if mode == "on" else "off"},
                                    "normal_trigger_observed": None, "producer_observed": None,
                                    "application_observed": None, "optional_calls": None,
                                    "artifact_observed": None, "billing_known": None}
                                   for mode in modes]})
    return {"schema_version": 1, "split": "development", "evidence_scope": "preregistered_not_executed",
            "corpus_digest": digest(tasks), "pairs": pairs, "normal_turn_ready": False,
            "limits": ["one_independent_task_per_feature", "no_live_trigger_proof", "no_accuracy_or_benefit_claim",
                       "label_tasks_measure_findability_not_coding_completion", "heldout_unchanged"]}


def pair_evidence_gate(off, on):
    """Fail closed on missing externally captured facts; never computes accuracy.

    Callers provide exact actual trigger -> producer -> application -> independent
    artifact observations. This shape check cannot authenticate those observations.
    Scripted controls, service-only studies and historical runs cannot pass.
    """
    reasons = []
    if isinstance(off, dict) and isinstance(on, dict) and off.get("feature") != on.get("feature"):
        reasons.append("paired_feature_mismatch")
    for mode, facts in (("off", off), ("on", on)):
        if not isinstance(facts, dict):
            reasons.append(mode + ":missing_facts")
            continue
        for key in ("subject_verified", "effective_selection_verified", "paired_inputs_verified",
                    "feature_isolation_verified", "authority_preserved", "billing_known",
                    "optional_ledger_complete", "normal_trigger_observed", "application_observed",
                    "independent_artifact_observed"):
            if facts.get(key) is not True:
                reasons.append(mode + ":" + key)
        if facts.get("feature") not in development.FEATURES:
            reasons.append(mode + ":missing_or_unknown_feature")
        if facts.get("feature") == "recommendations":
            runtime = facts.get("recommendation_runtime")
            if (not isinstance(runtime, dict) or runtime.get("status") != "corroborated"
                    or runtime.get("evidence_scope") != "runtime_invocation_only"
                    or not isinstance(runtime.get("public_reference"), str) or not runtime["public_reference"]
                    or not isinstance(runtime.get("invocation_id"), str) or not runtime["invocation_id"]):
                reasons.append(mode + ":recommendation_public_invocation_uncorroborated")
        if facts.get("evidence_scope") != "current_normal_path":
            reasons.append(mode + ":not_current_normal_path")
        calls = facts.get("optional_calls")
        if not isinstance(calls, dict) or set(calls) != {"discovery", "verification", "inference"}:
            reasons.append(mode + ":missing_optional_counters")
        elif any(type(n) is not int or n < 0 for n in calls.values()):
            reasons.append(mode + ":invalid_optional_counters")
        elif mode == "off" and any(calls.values()):
            reasons.append(mode + ":optional_calls_not_zero")
        elif mode == "on" and calls["inference"] == 0 and facts.get("typed_outcome") not in {"fallback", "abstention"}:
            reasons.append(mode + ":missing_inference_or_typed_fallback")
        if mode == "on" and facts.get("producer_observed") is not True:
            reasons.append(mode + ":producer_observed")
    return {"complete_shape": not reasons, "blockers": reasons,
            "accuracy_claim": False, "benefit_claim": False, "authenticated": False}
