"""Independent development correctness oracle; never imported into consumer payload.

Tests a deliberately bounded pure numeric Python subset without executing submitted
Python. This is an ordinary coding artifact oracle, not a ranking/service score.
No evaluator authority is granted to generated imports or other executable effects.
Independent review is still required before live promotion.
"""
import ast
import operator
from pathlib import Path

import context_scenarios as scenarios

_OPERATORS = {ast.Add: operator.add, ast.Sub: operator.sub, ast.Mult: operator.mul,
              ast.FloorDiv: operator.floordiv, ast.Mod: operator.mod}


def _expression(node, argument, value):
    if isinstance(node, ast.Constant) and type(node.value) is int and abs(node.value) <= 1000000:
        return node.value
    if isinstance(node, ast.Name) and node.id == argument:
        return value
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Pow):
        left = _expression(node.left, argument, value)
        right = _expression(node.right, argument, value)
        if not 0 <= right <= 32 or abs(left) > 1000:
            raise ValueError("power bound exceeded")
        result = left ** right
    elif isinstance(node, ast.BinOp) and type(node.op) in _OPERATORS:
        result = _OPERATORS[type(node.op)](_expression(node.left, argument, value),
                                         _expression(node.right, argument, value))
    elif isinstance(node, ast.UnaryOp) and isinstance(node.op, ast.USub):
        result = -_expression(node.operand, argument, value)
    elif (isinstance(node, ast.Call) and isinstance(node.func, ast.Name)
          and node.func.id in {"min", "max", "abs"} and not node.keywords
          and 1 <= len(node.args) <= 8):
        args = [_expression(child, argument, value) for child in node.args]
        if node.func.id == "abs" and len(args) == 1:
            result = abs(args[0])
        elif node.func.id in {"min", "max"} and len(args) >= 2:
            result = (min if node.func.id == "min" else max)(args)
        else:
            raise ValueError("unsupported call arity")
    else:
        raise ValueError("unsupported pure expression")
    if type(result) is not int or abs(result) > 1000000000000000:
        raise ValueError("numeric bound exceeded")
    return result


def grade(scenario_id, workspace):
    """Return pass/fail/unavailable; missing or invalid artifacts never pass.

    At most 16 KiB, 128 syntax nodes, 1440 inputs. The authored acceptance
    domain is fixed independently of generated outputs and on/off treatment.
    No submitted code is executed. Infrastructure failures are unavailable.
    """
    item = scenarios.scenario(scenario_id)
    result = {"oracle_id": item["oracle_id"], "status": "fail", "reason": "invalid_artifact",
              "tests": dict.fromkeys(scenarios.test_ids(scenario_id), False)}
    path = Path(workspace) / item["artifact"]
    if path.is_symlink() or not path.is_file():
        return dict(result, reason="missing_or_unsafe_artifact")
    try:
        with path.open("rb") as stream:
            source = stream.read(16385)
        if len(source) > 16384:
            return dict(result, reason="artifact_too_large")
        tree = ast.parse(source.decode("utf-8"))
        if len(list(ast.walk(tree))) > 128 or len(tree.body) != 1:
            return result
        function = tree.body[0]
        signature = item["starter_files"]["solution.py"].splitlines()[0]
        name = signature.split("def ")[1].split("(")[0]
        argument = signature.split("(")[1].split(")")[0]
        if (not isinstance(function, ast.FunctionDef) or function.name != name
                or function.decorator_list or function.returns or function.type_params
                or len(function.args.args) != 1 or function.args.args[0].arg != argument
                or function.args.args[0].annotation or function.args.posonlyargs
                or function.args.kwonlyargs or function.args.defaults
                or function.args.vararg or function.args.kwarg
                or len(function.body) != 1 or not isinstance(function.body[0], ast.Return)):
            return result
        result["tests"][scenario_id + ":artifact-shape"] = True
        # Independent reference cases, not exposed to materialized workspace.
        # Retry's 0..31 catches exact cap transition; minutes cover the whole
        # domain; routing crosses many periods and includes large keys.
        if scenario_id == "retry-window-v1":
            cases = []
            delay = 75
            for i in range(32):
                cases.append((i, delay))
                delay = 900 if delay * 2 > 900 else delay * 2
        elif scenario_id == "local-minute-v1":
            cases = [(i, i - 345 if i >= 345 else i + 1095) for i in range(1440)]
        elif scenario_id == "bucket-route-v1":
            cycle = [3, 10, 6, 2, 9, 5, 1, 8, 4, 0, 7]
            cases = [(i, cycle[i % 11]) for i in list(range(256)) + [10000, 100001]]
        elif scenario_id == "storage-blocks-v1":
            values = [0, 1, 4095, 4096, 4097, 8191, 8192, 8193, 1000001]
            # Quotient and remainder reference, independent of ceiling idiom.
            cases = [(i, i // 4096 + int(i % 4096 != 0)) for i in values]
        elif scenario_id == "sensor-clamp-v1":
            cases = [(i, -40 if i < -40 else 125 if i > 125 else i)
                     for i in range(-200, 301)]
        elif scenario_id == "invoice-rounding-v1":
            cases = []
            for i in list(range(1000)) + [100001, 999999]:
                whole, remainder = divmod(i * 7, 100)
                cases.append((i, whole + int(remainder >= 50)))
        else:
            raise ValueError("oracle implementation missing")
        for value, expected in cases:
            actual = _expression(function.body[0].value, argument, value)
            if actual != expected:
                return dict(result, reason="independent_test_failed")
        result["tests"][scenario_id + ":numeric-contract"] = True
        return dict(result, status="pass", reason="independent_tests_passed")
    except (UnicodeError, SyntaxError, ValueError, TypeError, ArithmeticError, RecursionError):
        return result
    except OSError:
        return dict(result, status="unavailable", reason="artifact_read_failed",
                    tests=dict.fromkeys(scenarios.test_ids(scenario_id), None))
