"""Runnable normal-turn development and new frozen study tasks; no provider calls.

Task inputs do not contain oracle implementation/cases. The existing held-out JSON
and freeze are untouched. A generated CaptureJob is not proof of a normal hook.
"""
import copy
import hashlib
import json
from pathlib import Path
import random
import uuid

FEATURES = ("tool_result_triage", "context_reranking", "progress_monitoring", "recommendations",
            "session_categorization", "eval_categorization")
PREREGISTRATION = {
    "id": "judgement-full-dd77-r1-new-study-v1", "repetitions": 3,
    "primary_metric": "paired_independent_task_success", "preservation_regressions_allowed": 0,
    "negative_control_regressions_allowed": 0, "outage_control_regressions_allowed": 0,
    "latency_ratio_limit": 1.25, "extra_model_rounds_limit": 1, "extra_tool_rounds_limit": 1,
    "minimum_complete_pairs_per_feature": 9,
    "interpretation": "small preregistered study; no universal stochastic claim; missing pairs block conclusions",
    "status": "proposed_coordinator_acceptance_required",
}


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()).hexdigest()


def _csv_case(identity, feature, control, split, delimiter):
    starter = 'def encode_row(row):\n    return ' + repr(delimiter) + '.join(row) + "\\n"\n'
    if control == "negative":
        # Correct baseline still permits ordinary shell verification without an error trigger.
        starter = ('def encode_row(row):\n    return ' + repr(delimiter) +
                   '.join([\'"\' + str(cell).replace(\'"\', \'""\') + \'"\' for cell in row]) + "\\n"\n')
    smoke = ('from exporter import encode_row\n'
             'print(encode_row(["order", 17, "contains ' + delimiter + ' delimiter"]))\n')
    prompt = ("Repair the order export implementation in exporter.py. First inspect exporter.py and run "
              "`python3 smoke.py` through the ordinary shell tool to reproduce the current behavior. "
              "Then implement encode_row(row): stringify each cell, quote EVERY cell, double embedded double quotes, "
              "join cells with the deployed delimiter in format.txt, and terminate with one LF. Preserve input and smoke files. "
              "Use one return expression: literals, row/cell names, string +, str(), replace(), join(), and a single list/generator "
              "comprehension over row are supported. No imports, attributes other than replace/join, file IO or effects in exporter.py. "
              "Run smoke again after fixing; do not just report a fix.")
    if feature == "progress_monitoring":
        prompt += " Reproduce with three separate settled smoke commands before changing code; do not retry after those three without inspecting the defect."
    return {"case_id": identity, "split": split, "feature": feature, "control": control,
            "prompt": prompt, "artifact": "exporter.py", "oracle": "csv-behavior-v1",
            "files": {"exporter.py": starter, "smoke.py": smoke,
                      "format.txt": "Deployed CSV delimiter: " + delimiter + ". Every cell is quoted; line ending LF.\n"},
            "mutable_files": ["exporter.py"], "trigger_command": "python3 smoke.py",
            "history_notes": [], "hooks": [feature],
            "gap": "Requires actual ordinary tool invocation, exact settled receipt, optional ledger and subsequent application capture; authored commands are not trigger evidence."}


def cases(split="development"):
    """Return realistic coding tasks plus navigation tasks; controls are not outcomes.

    Outage controls use identical task input but require coordinator-owned provider
    unavailability configuration; no simulated error is injected into model context.
    New-study cases are distinct immutable source definitions, not old held-out edits.
    """
    if split not in {"development", "new-heldout"}:
        raise ValueError("unknown split")
    output = []
    delimiter = "|" if split == "development" else ";"
    prefix = "dev" if split == "development" else "study"
    for feature in FEATURES:
        for control in ("positive", "negative", "outage"):
            identity = prefix + "-" + feature.replace("_", "-") + "-" + control
            if feature in {"tool_result_triage", "progress_monitoring", "recommendations", "context_reranking"}:
                item = _csv_case(identity, feature, control, split, delimiter)
                if feature == "context_reranking":
                    item["files"].pop("format.txt")
                    item["prompt"] = item["prompt"].replace("in format.txt", "from the admitted deployed historical note")
                    item["history_notes"] = [
                        {"id": "archived", "snippet": "Archived export uses comma-separated unquoted cells; obsolete before deployment."},
                        {"id": "deployed", "snippet": "Deployed order export uses delimiter " + delimiter + "; every cell is quoted, embedded quotes doubled, line ending LF."},
                        {"id": "unrelated", "snippet": "Dashboard color palette changed to graphite; unrelated to order exports."}]
                    item["gap"] = "Bind exact canonical earlier same-session historical notes absent from mandatory context; framed-byte admission and captured selection required. No arbitrary prompt injection establishes retrieval."
                    if control == "negative":
                        item["prompt"] += " Explicit current contract: delimiter " + delimiter + "; quote all cells and double embedded quotes; LF."
                output.append(item)
            else:
                session_task = feature == "session_categorization"
                cards = [{"id": "orders", "request": "Debug backend order CSV export failure", "status": "failed", "preservation_passed": False},
                         {"id": "colors", "request": "Design frontend dashboard colors", "status": "completed", "preservation_passed": True},
                         {"id": "deadline", "request": "Document setup", "status": "timed_out", "preservation_passed": True}]
                if split == "new-heldout":
                    cards[0]["id"] = "invoices"
                    cards[0]["request"] = "Debug backend invoice CSV export failure"
                request = cards[0]["request"]
                output.append({"case_id": identity, "split": split, "feature": feature, "control": control,
                               "prompt": (request + ". Find the matching session using the public session catalog and bounded open operation; write result.json with selected_id. Do not infer completion from a label." if session_task else
                                          "Find the export evaluation run whose protected inputs changed using the public evaluation result retrieval operation. Inspect its authoritative terminal diagnosis; write result.json with selected_id. Preserve failed preservation status."),
                               "artifact": "result.json", "oracle": "public-navigation-v1", "files": {}, "mutable_files": ["result.json"],
                               "history_notes": [], "hooks": [feature], "navigation_seed": cards,
                               "gap": "Requires coordinator materialization of real public session/eval records and actual public catalog/retrieval receipts. Synthetic cards are not normal navigation evidence. Eval feature requires actual terminal evaluation diagnosis, not a normal model-turn substitute."})
    return output + repair_cases(split)


def repair_cases(split="development"):
    """Three distinct implemented families, each with positive/negative/outage controls.

    Four genuinely failing ordinary smoke invocations are requested for progress;
    they execute the broken implementation, not scripted provider/error output.
    Recommendation uses the ordinary eligible catalog with unchanged permissions.
    """
    if split not in {"development", "new-heldout"}:
        raise ValueError("unknown split")
    prefix = "dev" if split == "development" else "study"
    definitions = (
        ("record-ids", "selected_ids(records)",
         "Select IDs of active records only, in ascending numeric order with duplicate IDs removed. Records contain id (integer) and active (boolean). Empty input returns an empty list.",
         "sorted(set(records))", "sorted({record['id'] for record in records if record['active']})",
         "[{'id': 9, 'active': True}, {'id': 2, 'active': False}, {'id': 9, 'active': True}]"),
        ("checkout-validation", "valid_checkout(record)",
         "Return a boolean: sku must be a nonempty string after stripping whitespace; quantity must be an integer but not bool, between 1 and 99 inclusive; email must be a string with exactly one @ and nonempty text on both sides. Missing keys are invalid. No network or locale validation.",
         "record['quantity'] > 0 and '@' in record['email']",
         "isinstance(record.get('sku'), str) and bool(record['sku'].strip()) and type(record.get('quantity')) == int and 1 <= record['quantity'] <= 99 and isinstance(record.get('email'), str) and record['email'].count('@') == 1 and all(record['email'].split('@'))",
         "{'sku': 'book', 'email': 'user@example.test'}"),
        ("confined-path", "asset_path(relative)",
         "Join a POSIX relative asset name beneath /srv/assets. Return None for empty/nonstring input, absolute paths, any .. segment, backslashes or NUL. Remove empty and . segments; if no segments remain return None. Otherwise join remaining segments beneath /srv/assets. Unicode names are preserved; no filesystem access.",
         "'/srv/assets/' + relative", "'/srv/assets/' + '/'.join([part for part in relative.split('/') if part and part != '.']) if isinstance(relative, str) and relative and not relative.startswith('/') and '..' not in relative.split('/') and '\\\\' not in relative and '\\x00' not in relative and any(part and part != '.' for part in relative.split('/')) else None",
         "None"),
    )
    output = []
    for feature in ("tool_result_triage", "progress_monitoring", "recommendations"):
        for family, signature, contract, broken, good, smoke_input in definitions:
            for control in ("positive", "negative", "outage"):
                identity = prefix + "-" + feature.replace("_", "-") + "-" + family + "-" + control
                source = "def " + signature + ":\n    return " + (good if control == "negative" else broken) + "\n"
                name = signature.split("(")[0]
                smoke = "from repair import " + name + "\nprint(" + name + "(" + smoke_input + "))\n"
                prompt = ("Repair repair.py for the local service contract in contract.txt. Read the implementation before changing it; "
                          "run `python3 smoke.py` through the ordinary shell tool to reproduce current behavior, then repair and run smoke again. "
                          "Preserve smoke.py and contract.txt. Implement one pure return expression; arithmetic, comparisons, boolean/conditional expressions, "
                          "literal collections, subscripts, one bounded comprehension, str/type/isinstance/bool/sorted/set/all/any/len, "
                          "and string strip/split/join/count/startswith plus dict.get are allowed. No imports, effects, IO, loops or arbitrary calls. " + contract)
                if feature == "progress_monitoring" and control != "negative":
                    prompt += " Capture four separate genuine smoke failures before repair; after the fourth inspect the defect instead of another identical retry."
                elif feature == "progress_monitoring":
                    prompt += " If smoke succeeds, preserve the working implementation; do not manufacture a failure or repeat successful checks unnecessarily."
                output.append({"case_id": identity, "split": split, "feature": feature, "control": control,
                               "family_id": family, "prompt": prompt, "artifact": "repair.py", "oracle": family + "-behavior-v1",
                               "files": {"repair.py": source, "smoke.py": smoke, "contract.txt": contract + "\n"},
                               "mutable_files": ["repair.py"], "trigger_command": "python3 smoke.py", "history_notes": [], "hooks": [feature],
                               "gap": "Requires actual normal trigger/application/settled receipts; artifact correctness alone does not establish optional benefit. Outage needs real coordinator configuration; catalog eligibility and permissions are unchanged."})
    return output


def case(identity):
    for split in ("development", "new-heldout"):
        for item in cases(split):
            if item["case_id"] == identity:
                return item
    if identity == "combined-order-export":
        item = _csv_case(identity, "tool_result_triage", "positive", "development", "|")
        item["hooks"] = ["tool_result_triage", "progress_monitoring", "recommendations", "session_categorization"]
        item["prompt"] += " Reproduce with three separate settled smoke commands, then repair after reading the source."
        item["gap"] += " Context and terminal-eval/navigation hooks are not naturally established by this coding turn; do not claim combined six-feature coverage."
        return item
    raise ValueError("unknown case")


def materialize(identity, workspace):
    """Materialize starters only; expected outputs and oracle code stay outside."""
    item = case(identity)
    root = Path(workspace)
    if any(p.is_symlink() for p in (root, *root.parents)) or not root.is_dir() or any(root.iterdir()):
        raise ValueError("fresh existing non-symlink workspace required")
    for name, text in item["files"].items():
        (root / name).write_text(text, encoding="utf-8")
    return {"case_id": identity, "prompt": item["prompt"], "artifact": item["artifact"],
            "trigger_command": item.get("trigger_command"), "gap": item["gap"]}


def capture_job(template, identity, arm, session_id, repetition=0, history_references=None):
    """Populate the existing CaptureJob normal_turn shape from a supplied template.

    Template is coordinator-owned exact subject/admission metadata. No model/profile,
    auth, permission, state, or budget is inferred. No overlay is applied here.
    Combined studies are external schedules of normal-turn jobs, not new wire arms.
    """
    item = case(identity)
    if item["feature"] == "eval_categorization":
        raise ValueError("normal_turn cannot establish terminal eval diagnosis; actual eval-attempt capture adapter required")
    if arm not in {"off", "on"} or type(repetition) is not int or not 0 <= repetition < 3:
        raise ValueError("invalid arm/repetition")
    uuid.UUID(session_id)
    if set(template) != {"schema_version", "request", "consumer", "allow_live"} or template["schema_version"] != 1:
        raise ValueError("existing CaptureJob template required")
    if template["consumer"].get("kind") != "normal_turn":
        raise ValueError("normal_turn template required")
    job = copy.deepcopy(template)
    request = job["request"]
    required = {"preregistration_id", "task_id", "repetition", "feature", "arm", "source", "capture_id", "pairing_id",
                "isolated_feature", "subject", "cache_condition", "timeout_ms"}
    if set(request) != required:
        raise ValueError("CaptureRequest fields differ from current contract")
    turn = job["consumer"]["request"]
    if set(turn) != {"session_id", "prompt", "admission", "max_events", "categorization_dimension"}:
        raise ValueError("NormalTurnRequest fields differ from current contract")
    execution = turn["admission"]["execution"]
    if item["history_notes"]:
        if not isinstance(history_references, list) or len(history_references) != len(item["history_notes"]):
            raise ValueError("canonical history references required; absent context is a gap")
        sequences = set()
        for ref in history_references:
            if (set(ref) != {"session_id", "sequence"} or ref["session_id"] != session_id
                    or type(ref["sequence"]) is not int or ref["sequence"] <= 0 or ref["sequence"] in sequences):
                raise ValueError("invalid canonical history references")
            sequences.add(ref["sequence"])
        execution["schema_version"] = 8
        execution["historical_context"] = copy.deepcopy(history_references)
    elif history_references:
        raise ValueError("unexpected history references")
    else:
        execution.pop("historical_context", None)
    shared = {"case": item, "admission": copy.deepcopy(turn["admission"]), "subject": request["subject"],
              "max_events": turn["max_events"], "timeout_ms": request["timeout_ms"]}
    # Arm-specific canonical source IDs differ; exact original bytes must be verified externally.
    shared["admission"]["execution"].pop("historical_context", None)
    request.update(preregistration_id=PREREGISTRATION["id"], task_id=identity, repetition=repetition,
                   feature=item["feature"], arm=arm, source="live_consumer",
                   capture_id=identity + "-r" + str(repetition) + "-" + arm,
                   pairing_id=digest(shared), isolated_feature=None)
    turn.update(session_id=session_id, prompt=item["prompt"])
    job["allow_live"] = False
    return job


def frozen_schedule(split="new-heldout"):
    """Freeze inputs, controls and prerandomized schedules before any run/tuning."""
    items = cases(split)
    pairs = []
    for item in items:
        for repetition in range(PREREGISTRATION["repetitions"]):
            arms = ["off", "on"]
            random.Random(int(digest([item["case_id"], repetition])[:16], 16)).shuffle(arms)
            pairs.append({"case_id": item["case_id"], "repetition": repetition, "arms": arms,
                          "outage_configuration_required": item["control"] == "outage"})
    return {"schema_version": 1, "split": split, "preregistration": copy.deepcopy(PREREGISTRATION),
            "cases": items, "corpus_sha256": digest(items), "pairs": pairs,
            "evidence_scope": "frozen_not_executed", "known_gaps": sorted({i["gap"] for i in items}),
            "limits": ["three_new_distinct_repair_families_plus_existing_CSV_for_triage_progress_recommendations",
                       "cross_feature_reuse_is_not_independent_family_replication",
                       "new_study_uses_same_contracts_not_a_broad_distributional_holdout",
                       "navigation_has_no_implemented_normal_adapter",
                       "outage_requires_coordinator_verified_real_configuration",
                       "existing_heldout_and_freeze_bytes_unchanged"]}


def driver_job(template, identity, arm, session_id, repetition=0):
    """Populate the coordinator normal-live driver envelope, without admitting work.

    Subject, state/config, overlays, expected selection and hash-pinned oracle
    dependencies must already be supplied and verified by coordinator. No widening
    of tools or permissions, eligibility filtering, live opt-in or inferred identity.
    """
    item = case(identity)
    job = copy.deepcopy(template)
    required = {"capture_template", "oracle_module", "oracle_sha256", "oracle_dependencies", "arm"}
    if not required <= set(job):
        raise ValueError("coordinator driver template required")
    job["capture_template"] = capture_job(job["capture_template"], identity, arm, session_id, repetition)
    job.update(scenario_id=identity, starter_files=copy.deepcopy(item["files"]), artifact=item["artifact"])
    return job


def workflow_failure_suite(workspace, subject_binary, config_toml, *, state_root,
                           profile, config_file, daemon_isolation="isolated", variant_env=None):
    """Materialize an actual v1 workflow eval suite with independent command judges.

    No execution is launched. Required keyword inputs are state_root, profile and
    config_file. Isolated mode (default) emits one config_toml overlay and explicitly
    declares isolation; the eval runtime derives its per-variant state. Shared mode
    emits no config_toml field and requires matching BCODE_STATE_DIR,
    BCODE_SESSION_STORE_DIR, BCODE_CONFIG, BCODE_MODEL_PROFILE and exact
    BCODE_CONFIG_TOML in variant_env. Profile is carried through the supported
    environment rather than the unsupported workflow variant.profile field.
    These declarations are not effective runtime identity verification.
    The ordinary task asks for repair; a preservation contract forbids changing
    protected.txt. The command oracle examines real workspace bytes. Suite failures
    are not scripted provider answers; diagnosis depends on the actual run outcome.
    """
    root = Path(workspace)
    if any(p.is_symlink() for p in (root, *root.parents)) or not root.is_dir() or any(root.iterdir()):
        raise ValueError("fresh external suite directory required")
    import tomllib
    if not Path(subject_binary).is_absolute() or not isinstance(config_toml, str):
        raise ValueError("explicit subject and overlay required")
    if (not isinstance(profile, str) or not profile.strip()
            or not Path(state_root).is_absolute() or not Path(config_file).is_absolute()
            or daemon_isolation not in {"isolated", "shared"}):
        raise ValueError("explicit absolute state/config, profile and isolation mode required")
    tomllib.loads(config_toml)
    environment = {"BCODE_STATE_DIR": str(state_root),
                   "BCODE_SESSION_STORE_DIR": str(Path(state_root) / "sessions"),
                   "BCODE_CONFIG": str(config_file), "BCODE_MODEL_PROFILE": profile,
                   "TMPDIR": "/Volumes/ehdd/bcode-eval-artifacts/tmp"}
    if variant_env is not None:
        if not isinstance(variant_env, dict) or any(not isinstance(k, str) or not isinstance(v, str)
                                                   for k, v in variant_env.items()):
            raise ValueError("string variant environment required")
        environment.update(variant_env)
    expected = {"BCODE_STATE_DIR": str(state_root),
                "BCODE_SESSION_STORE_DIR": str(Path(state_root) / "sessions"),
                "BCODE_CONFIG": str(config_file), "BCODE_MODEL_PROFILE": profile}
    if any(environment.get(k) != v for k, v in expected.items()):
        raise ValueError("variant environment conflicts with explicit state/config/profile")
    if daemon_isolation == "shared":
        if not isinstance(variant_env, dict) or any(variant_env.get(k) != v for k, v in expected.items()):
            raise ValueError("shared mode requires explicit matching variant state/config/profile environment")
        if environment.get("BCODE_CONFIG_TOML", "") != config_toml:
            raise ValueError("shared mode overlay must be supplied exactly in variant environment")
        overlay_field = ""
    else:
        if "BCODE_CONFIG_TOML" in environment:
            raise ValueError("isolated mode must not duplicate config_toml in variant environment")
        overlay_field = "config_toml = " + json.dumps(config_toml) + "\n"
    fixture = root / "workspace"
    fixture.mkdir()
    (fixture / "operations.py").write_text('def total(values):\n    """Sum all integers, including empty input."""\n    return values[0]\n', encoding="utf-8")
    (fixture / "protected.txt").write_text("keep-this-exact\n", encoding="utf-8")
    oracle = '''from pathlib import Path
import ast

def expression(node, values):
    if isinstance(node, ast.Constant) and type(node.value) is int and abs(node.value) <= 1000000:
        return node.value
    if isinstance(node, ast.Call) and isinstance(node.func, ast.Name) and node.func.id == "sum" and not node.keywords and len(node.args) == 1 and isinstance(node.args[0], ast.Name) and node.args[0].id == "values":
        return sum(values)
    if isinstance(node, ast.BinOp) and isinstance(node.op, (ast.Add, ast.Sub)):
        left, right = expression(node.left, values), expression(node.right, values)
        return left + right if isinstance(node.op, ast.Add) else left - right
    if isinstance(node, ast.Subscript) and isinstance(node.value, ast.Name) and node.value.id == "values" and isinstance(node.slice, ast.Constant) and type(node.slice.value) is int:
        return values[node.slice.value]
    raise ValueError("unsupported pure expression")

try:
    source = Path("operations.py").read_bytes()
    if len(source) > 16384:
        raise ValueError("source bound")
    tree = ast.parse(source.decode())
    if len(tree.body) != 1 or len(list(ast.walk(tree))) > 64:
        raise ValueError("single bounded function")
    f = tree.body[0]
    if not isinstance(f, ast.FunctionDef) or f.name != "total" or f.decorator_list or f.returns or f.type_params or len(f.args.args) != 1 or f.args.args[0].arg != "values" or f.args.args[0].annotation or f.args.defaults or f.args.posonlyargs or f.args.kwonlyargs or f.args.vararg or f.args.kwarg:
        raise ValueError("pure signature")
    body = f.body
    if body and isinstance(body[0], ast.Expr) and isinstance(body[0].value, ast.Constant) and isinstance(body[0].value.value, str):
        body = body[1:]
    if len(body) != 1 or not isinstance(body[0], ast.Return):
        raise ValueError("one expression")
    for values in ([], [1], [-9, 4, 7], [3, 3, 3], list(range(-25, 25))):
        expected = 0
        for value in values:
            expected += value
        if expression(body[0].value, values) != expected:
            raise ValueError("independent behavior failure")
    if Path("protected.txt").read_bytes() != b"keep-this-exact\\n":
        raise ValueError("preservation failure")
except (OSError, UnicodeError, SyntaxError, ValueError, TypeError, IndexError):
    raise SystemExit(1)
raise SystemExit(0)
'''
    # The independent command judge interprets a bounded subset; generated code never executes.
    (root / "oracle.py").write_text(oracle, encoding="utf-8")
    suite = ('schema_version = 1\nid = "normal-terminal-diagnosis"\nname = "Actual workflow oracle diagnosis"\n'
             '[run]\nrepetitions = 1\ntimeout_ms = 300000\nisolation = "temp_copy"\nrandomize_case_order = false\nfail_fast = false\n'
             '[[variants]]\nid = "single"\nexecutor = "workflow"\n' + overlay_field +
             '[variants.env]\n' + ''.join(json.dumps(k) + ' = ' + json.dumps(v) + '\n' for k, v in sorted(environment.items())) +
             '[variants.metadata]\ndaemon_isolation = ' + json.dumps(daemon_isolation) + '\n' +
             '[variants.metadata.workflow]\nbinary = ' + json.dumps(subject_binary) + '\nstrategy = "no-delegation"\n'
             'stop_condition = "Repair total(values); preserve protected.txt exactly."\nmax_iterations = 1\nconcurrency = 1\nworker_attempts = 1\n'
             '[[cases]]\nid = "sum-repair"\nfixture = "workspace"\n'
             'prompt = "Repair total(values) in operations.py using a single return sum(values) expression; preserve protected.txt exactly. Do not claim success without checking."\n'
             '[cases.metadata.workflow_oracle]\noracle_command = "python3 \'{{suite_dir}}/oracle.py\'"\n'
             '[[cases.judges]]\ntype = "command"\ncommand = "python3 \'{{suite_dir}}/oracle.py\'"\ntimeout_ms = 3000\nrequired = true\n')
    (root / "suite.toml").write_text(suite, encoding="utf-8")
    return {"suite": str(root / "suite.toml"), "suite_sha256": hashlib.sha256(suite.encode()).hexdigest(),
            "gap": "Run may succeed or fail. Only an actual terminal oracle failure qualifies for failure-category measurement; no failure is guaranteed. Command judge uses bounded independent sum behavior and preservation, not a general whole-program oracle."}


def combined_schedule():
    """Natural multi-hook task: all-off, singles, all-on, targeted leave-one-out."""
    item = case("combined-order-export")
    arms = [{"name": "all-off", "enabled": []}, {"name": "all-on", "enabled": list(FEATURES)}]
    arms += [{"name": "only-" + f, "enabled": [f]} for f in item["hooks"]]
    arms += [{"name": "without-" + f, "enabled": [x for x in FEATURES if x != f]} for f in item["hooks"]]
    blocks = []
    for repetition in range(3):
        ordered = copy.deepcopy(arms)
        random.Random(int(digest([item["case_id"], repetition])[:16], 16)).shuffle(ordered)
        blocks.append({"repetition": repetition, "arms": ordered})
    return {"case": item, "input_sha256": digest(item), "blocks": blocks,
            "isolated_feature": False, "evidence_scope": "scheduled_not_executed",
            "gap": "CaptureJob arm remains off/on; coordinator must apply and verify named multi-feature overlays separately. Session categorization is not navigation benefit; context/eval require separate actual follow-up hooks."}
