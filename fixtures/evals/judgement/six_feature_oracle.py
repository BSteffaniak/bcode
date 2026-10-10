"""Independent bounded JSON artifact checks; never execute generated code.

These checks are task outcomes, not trigger/admission/producer evidence. A pass
cannot establish live benefit; an off/on comparison still needs coordinator capture.
"""
import ast
import copy
import json
from pathlib import Path

import context_oracle
import six_feature_development as development


def _pagination_behavior(value):
    """Interpret <=64 AST nodes using the existing bounded numeric oracle.

    Names are only page/page_size; no Python eval/exec or submitted code runs.
    Equivalent arithmetic is accepted across the deterministic acceptance domain.
    """
    if (set(value) != {"offset_formula", "order_by", "response_fields"}
            or value["order_by"] != ["created_at", "id"]
            or value["response_fields"] != ["items", "next_page"]
            or not isinstance(value["offset_formula"], str)
            or len(value["offset_formula"].encode("utf-8")) > 1024):
        return False
    tree = ast.parse(value["offset_formula"], mode="eval")
    if len(list(ast.walk(tree))) > 64:
        return False
    if any(isinstance(node, ast.Name) and node.id not in {"page", "page_size", "min", "max", "abs"}
           for node in ast.walk(tree)):
        return False
    for size in (1, 2, 7, 25, 100, 1000):
        for page in (*range(1, 65), 1000, 100000):
            class BindInputs(ast.NodeTransformer):
                def visit_Name(self, node):
                    if node.id in {"page", "page_size"}:
                        return ast.Constant(page if node.id == "page" else size)
                    return node

            expression = BindInputs().visit(copy.deepcopy(tree.body))
            actual = context_oracle._expression(expression, "__unused", 0)
            # Independent accumulated page boundaries, not formula text matching.
            expected = sum(size for _ in range(page - 1))
            if actual != expected:
                return False
    return True


def _unique(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate JSON field")
        result[key] = value
    return result


def _reject_constant(_):
    raise ValueError("nonfinite JSON")


def _check(identity, value):
    # Reference constraints are authored here, not included in consumer payloads.
    if identity == "checkout-dependency-repair":
        return value == {"name": "checkout-api", "dependencies": {"express": "4.21.2"},
                         "devDependencies": {"vitest": "2.1.9"}, "scripts": {"test": "vitest run"}}
    if identity == "payment-webhook-contract":
        return (isinstance(value, dict) and set(value) == {"route", "events", "response_status", "verify_signature", "health_route"}
                and value["route"] == "/hooks/payments" and type(value["response_status"]) is int
                and value["response_status"] == 204 and value["verify_signature"] is True
                and value["health_route"] == "/healthz" and isinstance(value["events"], list)
                and sorted(value["events"]) == ["payment.created", "payment.refunded"])
    if identity == "inventory-pagination-stall":
        return _pagination_behavior(value)
    if identity == "cart-parser-tool-findability":
        return value == {"selected_tool_id": "inspect-parser", "inspected_path": "cart.py"}
    if identity == "inventory-session-findability":
        return value == {"session_id": "inventory"}
    if identity == "checkout-eval-findability":
        return value == {"run_id": "checkout"}
    raise ValueError("oracle missing")


def corroborate_recommendation(session_id, turn_id, public_reader=None):
    """Require coordinator public-boundary evidence, never a submitted artifact.

    The reader must retrieve a complete ordered settled-invocation projection for
    the exact session/turn through supported application APIs. Its normalized
    envelope has session_id, turn_id, evidence_scope=current_normal_path,
    complete=True, public_reference (opaque nonempty receipt), and invocations
    (invocation_id, tool_name, path, settled_success). This is an adapter contract,
    NOT an implemented normal adapter or authentication of arbitrary dictionaries.
    No reader supplied means unavailable, not passed. Do not pass artifact claims
    or scripted fixtures as the reader in an acceptance run.
    """
    unavailable = {"status": "unavailable", "reason": "public_adapter_not_supplied"}
    if public_reader is None:
        return unavailable
    if not callable(public_reader) or not session_id or not turn_id:
        return dict(unavailable, reason="invalid_public_reader_scope")
    try:
        observed = public_reader(session_id, turn_id)
    except (OSError, ValueError, RuntimeError):
        return dict(unavailable, reason="public_evidence_read_failed")
    if (not isinstance(observed, dict) or observed.get("session_id") != session_id
            or observed.get("turn_id") != turn_id or observed.get("complete") is not True
            or observed.get("evidence_scope") != "current_normal_path"
            or not isinstance(observed.get("public_reference"), str) or not observed["public_reference"]):
        return dict(unavailable, reason="uncorroborated_public_evidence")
    invocations = observed.get("invocations")
    if not isinstance(invocations, list) or len(invocations) > 256:
        return dict(unavailable, reason="missing_or_oversized_invocation_projection")
    ids = set()
    for invocation in invocations:
        if (not isinstance(invocation, dict) or set(invocation) != {"invocation_id", "tool_name", "path", "settled_success"}
                or not all(isinstance(invocation[k], str) and invocation[k] for k in ("invocation_id", "tool_name", "path"))
                or type(invocation["settled_success"]) is not bool or invocation["invocation_id"] in ids):
            return dict(unavailable, reason="invalid_invocation_projection")
        ids.add(invocation["invocation_id"])
    reads = [i for i in invocations if i["tool_name"] == "filesystem.read"]
    found = bool(reads and reads[0]["path"] == "cart.py" and reads[0]["settled_success"] is True)
    return {"status": "corroborated" if found else "not_observed",
            "reason": "first_inspection_observed" if found else "first_inspection_not_observed",
            "public_reference": observed["public_reference"],
            "invocation_id": reads[0]["invocation_id"] if reads else None,
            "evidence_scope": "runtime_invocation_only", "artifact_quality": "separately_graded"}


def grade(identity, workspace):
    """Grade <=16 KiB strict JSON; distinguish missing/invalid from unavailable IO.

    Paths with any symlink component are rejected. No inference, model-supplied
    correctness flag or scripted control outcome is accepted as independent evidence.
    """
    item = development.scenario(identity)
    result = {"oracle_id": item["oracle_id"], "status": "fail", "reason": "invalid_artifact",
              "evidence_scope": "artifact_only", "runtime_invocation": {"status": "unavailable", "reason": "public_adapter_not_supplied"},
              "tests": {"artifact_shape": False, "independent_contract": False,
                                                             "input_preservation": False}}
    path = Path(workspace) / item["artifact"]
    try:
        if any(p.is_symlink() for p in (path, *path.parents)) or not path.is_file():
            return dict(result, reason="missing_or_unsafe_artifact")
        with path.open("rb") as stream:
            raw = stream.read(16385)
        if len(raw) > 16384:
            return dict(result, reason="artifact_too_large")
        value = json.loads(raw.decode("utf-8"), object_pairs_hook=_unique, parse_constant=_reject_constant)
        if not isinstance(value, dict):
            return result
        result["tests"]["artifact_shape"] = True
        if not _check(identity, value):
            return dict(result, reason="independent_contract_failed")
        result["tests"]["independent_contract"] = True
        for name, original in item["starter_files"].items():
            protected = Path(workspace) / name
            if protected.is_symlink() or not protected.is_file():
                return dict(result, reason="input_preservation_failed")
            with protected.open("rb") as stream:
                current = stream.read(16385)
            if current != original.encode("utf-8"):
                return dict(result, reason="input_preservation_failed")
        result["tests"]["input_preservation"] = True
        return dict(result, status="pass", reason="independent_contract_passed")
    except (UnicodeError, SyntaxError, ValueError, TypeError, ArithmeticError, RecursionError):
        return result
    except OSError:
        return dict(result, status="unavailable", reason="artifact_read_failed",
                    tests={"artifact_shape": None, "independent_contract": None, "input_preservation": None})
