"""Independent CSV behavior and preservation oracle; no submitted Python executes.

Runtime/public receipts are separately supplied by coordinator adapters. Missing
normal adapters remain unavailable; artifact quality never manufactures a hook.
"""
import ast
import csv
import io
import json
from pathlib import Path

import normal_study as study


def _expression(node, bindings):
    """Closed string/list subset, <=64 cells and bounded intermediate strings."""
    if isinstance(node, ast.Constant) and type(node.value) in {str, int}:
        result = node.value
    elif isinstance(node, ast.Name) and node.id in bindings:
        result = bindings[node.id]
    elif isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
        left, right = _expression(node.left, bindings), _expression(node.right, bindings)
        if not isinstance(left, str) or not isinstance(right, str):
            raise ValueError("only string addition supported")
        result = left + right
    elif isinstance(node, (ast.ListComp, ast.GeneratorExp)):
        if len(node.generators) != 1:
            raise ValueError("one bounded comprehension required")
        generator = node.generators[0]
        if (not isinstance(generator.target, ast.Name) or generator.target.id != "cell"
                or not isinstance(generator.iter, ast.Name) or generator.iter.id != "row"
                or generator.ifs or generator.is_async):
            raise ValueError("unsupported comprehension")
        result = [_expression(node.elt, dict(bindings, cell=cell)) for cell in bindings["row"]]
    elif isinstance(node, ast.Call) and not node.keywords:
        args = [_expression(arg, bindings) for arg in node.args]
        if isinstance(node.func, ast.Name) and node.func.id == "str" and len(args) == 1:
            if type(args[0]) not in {str, int}:
                raise ValueError("unsupported str input")
            result = str(args[0])
        elif isinstance(node.func, ast.Attribute):
            base = _expression(node.func.value, bindings)
            if not isinstance(base, str):
                raise ValueError("string receiver required")
            if node.func.attr == "replace" and len(args) == 2 and all(isinstance(a, str) for a in args):
                result = base.replace(*args)
            elif node.func.attr == "join" and len(args) == 1 and isinstance(args[0], list) and all(isinstance(a, str) for a in args[0]):
                result = base.join(args[0])
            else:
                raise ValueError("unsupported string method")
        else:
            raise ValueError("unsupported call")
    else:
        raise ValueError("unsupported syntax")
    if isinstance(result, str) and len(result.encode("utf-8")) > 16384:
        raise ValueError("intermediate byte bound exceeded")
    if isinstance(result, list) and len(result) > 64:
        raise ValueError("row bound exceeded")
    return result


def _parse(source):
    tree = ast.parse(source)
    if len(list(ast.walk(tree))) > 128 or len(tree.body) != 1:
        raise ValueError("one bounded function required")
    function = tree.body[0]
    if (not isinstance(function, ast.FunctionDef) or function.name != "encode_row"
            or function.decorator_list or function.returns or function.type_params
            or len(function.args.args) != 1 or function.args.args[0].arg != "row"
            or function.args.args[0].annotation or function.args.posonlyargs or function.args.kwonlyargs
            or function.args.defaults or function.args.kw_defaults or function.args.vararg or function.args.kwarg
            or len(function.body) != 1 or not isinstance(function.body[0], ast.Return)):
        raise ValueError("pure encode_row(row) required")
    return function.body[0].value


def _read(path, bound=16384):
    if any(p.is_symlink() for p in (path, *path.parents)) or not path.is_file():
        raise ValueError("missing or unsafe artifact")
    with path.open("rb") as stream:
        raw = stream.read(bound + 1)
    if len(raw) > bound:
        raise ValueError("artifact byte bound exceeded")
    return raw


def _rows():
    # Oracle fixtures never copied to task workspace or producer inputs.
    return [[], [""], ["plain", 17], ["a,b", "c;d", "e|f"], ['say "hello"', '"'],
            ["line\nbreak", "carriage\rreturn"], ["CJK 漢字", "emoji 🐈", "e\u0301"],
            [0, -9, 100001], [" "], ["quoted" * 50], ["x"] * 64]


def _repair_expression(node, env):
    """Closed, bounded expression interpreter; no eval, exec, reflection or IO."""
    def run(child):
        return _repair_expression(child, env)
    if isinstance(node, ast.Constant) and type(node.value) in {str, int, bool, type(None)}:
        value = node.value
    elif isinstance(node, ast.Name) and node.id in env:
        value = env[node.id]
    elif isinstance(node, (ast.List, ast.Tuple, ast.Set)):
        items = [run(x) for x in node.elts]
        value = set(items) if isinstance(node, ast.Set) else items
    elif isinstance(node, ast.Subscript):
        value = run(node.value)[run(node.slice)]
    elif isinstance(node, ast.IfExp):
        value = run(node.body if run(node.test) else node.orelse)
    elif isinstance(node, ast.BoolOp):
        value = run(node.values[0])
        for child in node.values[1:]:
            if isinstance(node.op, ast.And) and not value or isinstance(node.op, ast.Or) and value:
                break
            value = run(child)
    elif isinstance(node, ast.UnaryOp) and isinstance(node.op, ast.Not):
        value = not run(node.operand)
    elif isinstance(node, ast.UnaryOp) and isinstance(node.op, ast.USub):
        value = -run(node.operand)
    elif isinstance(node, ast.BinOp) and isinstance(node.op, (ast.Add, ast.Sub, ast.Mult, ast.Mod)):
        left, right = run(node.left), run(node.right)
        if isinstance(node.op, ast.Mult) and (not isinstance(left, int) or not isinstance(right, int)):
            raise ValueError("numeric multiplication only")
        if isinstance(node.op, ast.Add):
            value = left + right
        elif isinstance(node.op, ast.Sub):
            value = left - right
        elif isinstance(node.op, ast.Mult):
            value = left * right
        else:
            value = left % right
    elif isinstance(node, ast.Compare):
        left = run(node.left)
        value = True
        for op, child in zip(node.ops, node.comparators):
            right = run(child)
            if isinstance(op, ast.Eq):
                ok = left == right
            elif isinstance(op, ast.NotEq):
                ok = left != right
            elif isinstance(op, ast.Lt):
                ok = left < right
            elif isinstance(op, ast.LtE):
                ok = left <= right
            elif isinstance(op, ast.Gt):
                ok = left > right
            elif isinstance(op, ast.GtE):
                ok = left >= right
            elif isinstance(op, ast.In):
                ok = left in right
            elif isinstance(op, ast.NotIn):
                ok = left not in right
            elif isinstance(op, ast.Is):
                ok = left is right
            elif isinstance(op, ast.IsNot):
                ok = left is not right
            else:
                raise ValueError("unsupported comparison")
            if not ok:
                value = False
                break
            left = right
    elif isinstance(node, (ast.ListComp, ast.SetComp, ast.GeneratorExp)):
        if len(node.generators) != 1:
            raise ValueError("one comprehension only")
        generator = node.generators[0]
        if not isinstance(generator.target, ast.Name) or generator.is_async:
            raise ValueError("simple local binding required")
        items = run(generator.iter)
        if not isinstance(items, (list, tuple, str, set)) or len(items) > 64:
            raise ValueError("bounded iterable required")
        values = []
        for item in items:
            local = dict(env, **{generator.target.id: item})
            if all(_repair_expression(test, local) for test in generator.ifs):
                values.append(_repair_expression(node.elt, local))
        value = set(values) if isinstance(node, ast.SetComp) else values
    elif isinstance(node, ast.Call) and not node.keywords:
        args = [run(x) for x in node.args]
        functions = {"str": str, "bool": bool, "len": len, "sorted": sorted, "set": set, "all": all, "any": any,
                     "type": type, "isinstance": isinstance}
        if isinstance(node.func, ast.Name) and node.func.id in functions:
            value = functions[node.func.id](*args)
        elif isinstance(node.func, ast.Attribute):
            base = run(node.func.value)
            method = node.func.attr
            if isinstance(base, dict) and method == "get" and 1 <= len(args) <= 2:
                value = base.get(*args)
            elif isinstance(base, str) and method in {"strip", "split", "join", "count", "startswith"}:
                # Fixed dispatch, not arbitrary getattr/reflection.
                methods = {"strip": base.strip, "split": base.split, "join": base.join,
                           "count": base.count, "startswith": base.startswith}
                value = methods[method](*args)
            else:
                raise ValueError("unsupported method")
        else:
            raise ValueError("unsupported call")
    else:
        raise ValueError("unsupported repair expression")
    if isinstance(value, str) and len(value.encode()) > 16384:
        raise ValueError("string bound")
    if isinstance(value, (list, tuple, dict, set)) and len(value) > 64:
        raise ValueError("collection bound")
    if type(value) is int and abs(value) > 10 ** 12:
        raise ValueError("integer bound")
    return value


def _repair_behavior(item, raw):
    family = item["family_id"]
    signatures = {"record-ids": ("selected_ids", "records"), "checkout-validation": ("valid_checkout", "record"),
                  "confined-path": ("asset_path", "relative")}
    name, argument = signatures[family]
    source = raw.decode("utf-8")
    tree = ast.parse(source)
    nodes = list(ast.walk(tree))
    comprehensions = [n for n in nodes if isinstance(n, (ast.ListComp, ast.SetComp, ast.GeneratorExp))]
    nested = any(any(isinstance(child, (ast.ListComp, ast.SetComp, ast.GeneratorExp)) and child is not node
                     for child in ast.walk(node)) for node in comprehensions)
    if len(nodes) > 160 or len(tree.body) != 1 or len(comprehensions) > 2 or nested:
        raise ValueError("bounded single function and nonnested comprehensions required")
    function = tree.body[0]
    if (not isinstance(function, ast.FunctionDef) or function.name != name or function.decorator_list
            or function.returns or function.type_params or len(function.args.args) != 1
            or function.args.args[0].arg != argument or function.args.args[0].annotation
            or function.args.defaults or function.args.posonlyargs or function.args.kwonlyargs
            or function.args.vararg or function.args.kwarg or len(function.body) != 1
            or not isinstance(function.body[0], ast.Return)):
        raise ValueError("pure function shape required")
    if family == "record-ids":
        inputs = [[], [{"id": 4, "active": False}], [{"id": 9, "active": True}, {"id": 2, "active": True}, {"id": 9, "active": True}],
                  [{"id": i % 7 - 3, "active": i % 3 == 0} for i in range(64)]]
        expected = []
        for records in inputs:
            ids = []
            for record in records:
                if record["active"] and record["id"] not in ids:
                    ids.append(record["id"])
            expected.append(sorted(ids))
    elif family == "checkout-validation":
        inputs, expected = [], []
        for sku in ("book", "  ", None, 17):
            for quantity in (-1, 0, 1, 99, 100, True, "2", None):
                for email in ("a@b", "@b", "a@", "a@@b", "missing", None):
                    inputs.append({"sku": sku, "quantity": quantity, "email": email})
                    valid = isinstance(sku, str) and bool(sku.strip()) and type(quantity) is int and 1 <= quantity <= 99
                    email_valid = isinstance(email, str) and email.count("@") == 1 and all(email.split("@"))
                    expected.append(bool(valid and email_valid))
        inputs += [{}, {"sku": "x"}, {"sku": "x", "quantity": 1}]
        expected += [False] * 3
    else:
        inputs = [None, 17, "", ".", "./", "a", "a/b", "a//./b", "../a", "a/../b", "/a", "a\\b", "a\x00b", "漢字/🐈", " a "]
        expected = []
        for relative in inputs:
            if not isinstance(relative, str) or not relative or relative.startswith("/") or "\\" in relative or "\x00" in relative:
                expected.append(None)
                continue
            parts = relative.split("/")
            if ".." in parts:
                expected.append(None)
                continue
            clean = [p for p in parts if p not in ("", ".")]
            expected.append("/srv/assets/" + "/".join(clean) if clean else None)
    for value, target in zip(inputs, expected):
        result = _repair_expression(function.body[0].value, {argument: value, "int": int, "str": str, "bool": bool})
        if type(result) is not type(target) or result != target:
            return False
    return True


def grade(identity, workspace):
    """Independent bounded behavior/preservation; runtime remains separate."""
    item = study.case(identity)
    root = Path(workspace)
    tests = {"artifact_shape": False, "behavior": False, "preservation": False}
    result = {"case_id": identity, "status": "fail", "evidence_scope": "artifact_only", "tests": tests,
              "runtime": {"status": "unavailable", "reason": "public_evidence_adapter_not_supplied"}}
    try:
        for name, original in item["files"].items():
            if name not in item["mutable_files"] and _read(root / name) != original.encode("utf-8"):
                return dict(result, reason="preservation_failed")
        tests["preservation"] = True
        raw = _read(root / item["artifact"])
        if item["oracle"] == "public-navigation-v1":
            # A submitted selected_id remains only an artifact, never actual retrieval.
            def unique(pairs):
                value = {}
                for key, val in pairs:
                    if key in value:
                        raise ValueError("duplicate JSON field")
                    value[key] = val
                return value
            value = json.loads(raw.decode("utf-8"), object_pairs_hook=unique)
            tests["artifact_shape"] = isinstance(value, dict) and set(value) == {"selected_id"} and isinstance(value["selected_id"], str)
            if not tests["artifact_shape"]:
                return dict(result, reason="invalid_navigation_artifact")
            tests["behavior"] = None
            return dict(result, status="unavailable", reason="actual_public_navigation_required")
        if "family_id" in item:
            tests["artifact_shape"] = True
            tests["behavior"] = _repair_behavior(item, raw)
            return dict(result, status="pass" if tests["behavior"] else "fail",
                        reason="independent_repair_tests_passed" if tests["behavior"] else "repair_behavior_failed")
        expression = _parse(raw.decode("utf-8"))
        tests["artifact_shape"] = True
        delimiter = "|" if item["split"] == "development" else ";"
        for row in _rows():
            stream = io.StringIO(newline="")
            csv.writer(stream, delimiter=delimiter, quoting=csv.QUOTE_ALL, lineterminator="\n").writerow(row)
            if _expression(expression, {"row": row}) != stream.getvalue():
                return dict(result, reason="csv_behavior_failed")
        tests["behavior"] = True
        return dict(result, status="pass", reason="independent_csv_tests_passed")
    except (UnicodeError, SyntaxError, ValueError, TypeError, KeyError, IndexError, ArithmeticError, RecursionError):
        return dict(result, reason="invalid_or_missing_artifact")
    except OSError:
        return dict(result, status="unavailable", reason="artifact_read_failed", tests=dict.fromkeys(tests, None))


def public_outcome(identity, session_id, turn_id, public_reader=None):
    """Validate a coordinator public-boundary projection, NOT authenticate fixtures.

    Required: exact case/session/turn, current_normal_path, complete=true, opaque
    public_reference, observed_hook/application booleans and terminal, plus ordered
    settled receipts. Recommendation additionally requires filesystem.read of
    exporter.py; navigation requires actual retrieval of the correct seeded record.
    A caller must implement a supported public reader; no normal adapter is invented.
    """
    item = study.case(identity)
    unavailable = {"status": "unavailable", "reason": "public_evidence_adapter_not_supplied"}
    if not callable(public_reader):
        return unavailable
    try:
        facts = public_reader(identity, session_id, turn_id)
    except (OSError, ValueError, RuntimeError):
        return dict(unavailable, reason="public_evidence_read_failed")
    if (not isinstance(facts, dict) or facts.get("case_id") != identity or facts.get("session_id") != session_id
            or facts.get("turn_id") != turn_id or facts.get("scope") != "current_normal_path"
            or facts.get("complete") is not True or not isinstance(facts.get("public_reference"), str)
            or not facts["public_reference"] or facts.get("observed_hook") is not True
            or facts.get("application_observed") is not True or facts.get("terminal") != "completed"):
        return dict(unavailable, reason="missing_normal_path_corroboration")
    receipts = facts.get("receipts")
    if not isinstance(receipts, list) or len(receipts) > 256:
        return dict(unavailable, reason="missing_receipts")
    previous = 0
    seen = set()
    for receipt in receipts:
        if (not isinstance(receipt, dict) or type(receipt.get("sequence")) is not int or receipt["sequence"] <= previous
                or not isinstance(receipt.get("invocation_id"), str) or not receipt["invocation_id"]
                or receipt["invocation_id"] in seen or type(receipt.get("settled_success")) is not bool
                or not isinstance(receipt.get("operation"), str)):
            return dict(unavailable, reason="invalid_receipts")
        previous = receipt["sequence"]
        seen.add(receipt["invocation_id"])
    if item["feature"] in {"tool_result_triage", "progress_monitoring"}:
        smoke = [r for r in receipts if r["operation"] == "shell.run" and r.get("command") == "python3 smoke.py"]
        failures = [r for r in smoke if r["settled_success"] is False]
        required = (4 if "family_id" in item else 3) if item["feature"] == "progress_monitoring" else 1
        if item["control"] != "negative" and len(failures) < required:
            return dict(unavailable, reason="actual_settled_failure_trigger_missing")
        if not smoke or not smoke[-1]["settled_success"]:
            return {"status": "fail", "reason": "post_repair_smoke_not_observed"}
    if item["feature"] == "recommendations":
        reads = [r for r in receipts if r["operation"] == "filesystem.read"]
        if not reads or reads[0].get("path") != item["artifact"] or not reads[0]["settled_success"]:
            return {"status": "fail", "reason": "source_inspection_not_observed"}
    elif item["oracle"] == "public-navigation-v1":
        target = "orders" if item["split"] == "development" else "invoices"
        operation = "session.open" if item["feature"] == "session_categorization" else "eval.result.retrieve"
        matches = [r for r in receipts if r["operation"] == operation and r.get("record_id") == target and r["settled_success"]]
        if not matches:
            return {"status": "fail", "reason": "navigation_not_observed"}
        if item["feature"] == "eval_categorization" and (facts.get("preservation_passed") is not False
                or not isinstance(facts.get("terminal_diagnosis"), list)
                or "preservation_violation" not in facts["terminal_diagnosis"]):
            return dict(unavailable, reason="actual_terminal_diagnosis_missing")
    return {"status": "corroborated", "public_reference": facts["public_reference"],
            "evidence_scope": "runtime_only", "artifact_quality": "separately_graded"}
