"""Development tasks for six distinct consumers; preparation is never E2E evidence.

Producer inputs use current typed contracts. They contain task data, never oracle
answers. Recorded observations are seeds for a coordinator, not live tool results.
One task per feature is a pilot, not sufficient coverage for an accuracy claim.
"""
import copy
import json
import uuid
from pathlib import Path

FEATURES = ("tool_result_triage", "context_reranking", "progress_monitoring",
            "recommendations", "session_categorization", "eval_categorization")


def scenarios():
    """Return fresh task definitions, independent of treatment and generated output."""
    tasks = []

    def add(identity, feature, prompt, files, producer, trigger, outcome, gap):
        tasks.append({"scenario_id": identity, "feature": feature,
                      "split": "development", "prompt": prompt,
                      "starter_files": files, "artifact": "result.json",
                      "oracle_id": identity + "-independent-v1",
                      "producer_input": producer, "trigger_contract": trigger,
                      "observable_outcome": outcome, "api_gap": gap})

    add("checkout-dependency-repair", "tool_result_triage",
        "The checkout API fails its test command. Diagnose the settled failure, then write result.json as the corrected dependency manifest. Preserve package name, runtime dependencies and existing test settings. Do not disable tests.",
        {"package.json": json.dumps({"name": "checkout-api", "dependencies": {"express": "4.21.2"},
                                     "devDependencies": {}, "scripts": {"test": "vitest run"}}) + "\n",
         "test-contract.txt": "Tests import vitest; use approved dev version 2.1.9. Runtime packages are unchanged.\n"},
        {"kind": "service", "consumer": {"service": "tool_result_triage", "input": {
            "version": 1, "invocation_id": "checkout-test-1", "tool_name": "shell.run",
            "is_error": True, "partial": False, "excerpt_truncated": False,
            "output_excerpt": "npm test: sh: vitest: command not found; exit 127"}}},
        "An authorized ordinary shell tool must actually return the test failure; classify its original settled invocation before the next model request.",
        "Next request receives scoped transient environment advice; independent manifest checks show a repaired test dependency without changing runtime dependencies.",
        "A recorded failure is not a tool trigger. Requires live settled-tool correlation, request guidance capture and a public complete optional-call ledger.")

    task = "Update payment webhook configuration from the deployed contract; preserve signature verification and the health route. Write corrected config as result.json."
    snippets = [
        ("memo-a", "Archived prelaunch webhook contract: accept only payment.created on /hooks/payments."),
        ("memo-b", "Deployed webhook contract: accept payment.created and payment.refunded on /hooks/payments; respond 204 after enqueueing. Signature verification stays required."),
        ("memo-c", "Dashboard theme rollout: use graphite background; unrelated to webhook routing.")]
    candidates = [{"id": identity, "source": "history", "provenance": "development-note:" + identity,
                   "snippet": text, "protected": False} for identity, text in snippets]
    add("payment-webhook-contract", "context_reranking", task,
        {"webhook.json": json.dumps({"route": "/hooks/payments", "events": ["payment.created"],
                                     "response_status": 200, "verify_signature": True,
                                     "health_route": "/healthz"}) + "\n"},
        {"kind": "context", "request": {"task": task, "candidates": candidates,
                                           "budget_bytes": len(snippets[1][1].encode("utf-8"))}},
        "Hydrate exact earlier same-session notes absent from mandatory context, then observe whole-snippet byte-pressure admission on an ordinary turn.",
        "Captured selected excerpt bytes reach the base model; independent JSON checks verify both webhook event types, response status and preserved security/health configuration.",
        "Public context selection is a service study unless bound to normal-turn canonical historical references and real framed-byte budget. Requires admission capture and optional-call ledger.")

    objective = "Repair inventory pagination so page two returns distinct records without changing the public response schema."
    observations = [{"sequence": i, "action": "run inventory pagination tests",
                     "failure_key": "page-two-duplicates", "evidence_key": None,
                     "summary": "Second page repeats first-page records; same assertion after retry."}
                    for i in range(1, 4)]
    add("inventory-pagination-stall", "progress_monitoring",
        objective + " Write result.json with SQL-independent query settings: offset_formula, order_by and response_fields. offset_formula must be a pure integer arithmetic expression using page and page_size (min/max/abs allowed), without imports, attributes or comprehensions. Equivalent formulas are accepted by behavioral tests. Consult the API contract before another identical retry.",
        {"query.json": '{"offset_formula":"page - 1","order_by":["created_at"],"response_fields":["items","next_page"]}\n',
         "api-contract.txt": "Pages are one-based. Offset is (page - 1) * page_size. Stable ordering is created_at then id. Response has items and next_page only.\n"},
        {"kind": "service", "consumer": {"service": "progress_monitoring", "input": {
            "version": 1, "scope": {"kind": "session", "id": "inventory-pagination-stall"},
            "objective": objective, "observations": observations}}},
        "Actual settled repeated failures must populate the bounded normal-session progress window before subsequent model work.",
        "Scoped stall advice reaches a subsequent request, followed by independent query-contract repair; measure retries and elapsed time separately.",
        "Replaying authored observations only tests the service. Normal E2E needs actual observation provenance, advice-through-sequence correlation, subsequent artifact and ledger.")

    task = "Diagnose why the Python cart parser accepts malformed JSON. Inspect the existing parser and add a regression case without network access."
    add("cart-parser-tool-findability", "recommendations", task,
        {"cart.py": "import json\n\ndef parse_cart(raw):\n    try:\n        return json.loads(raw)\n    except ValueError:\n        return {}\n",
         "task-scope.txt": "Read the local implementation before selecting a test action. Network tools are ineligible.\n"},
        {"kind": "service", "consumer": {"service": "recommendations", "input": {
            "purpose": "recommendations", "task": task,
            "candidates": [{"id": "documentation", "description": "Read local contributor documentation", "protected": False},
                           {"id": "inspect-parser", "description": "Read the local cart.py implementation using filesystem.read", "protected": False},
                           {"id": "test-parser", "description": "Run the authorized local parser regression test using shell.run", "protected": False}]}}},
        "Construct the real already-eligible tool shortlist for this ordinary task, prepare/judge/apply its ordering, then let the model choose independently.",
        "Write result.json with selected_tool_id and inspected_path as an inspection report. Artifact quality alone does not establish invocation. A coordinator-owned public reader must independently corroborate the first settled filesystem.read of cart.py for the exact session/turn; score findability, not completed coding benefit.",
        "Service order is not model tool use. Needs original eligible shortlist, actual request order, selected invocation and optional ledger; ordering grants no permission.")

    newest = "Debug the backend inventory API pagination regression: page two repeats records. Preserve the JSON response schema."
    add("inventory-session-findability", "session_categorization",
        "Find the session for the backend inventory pagination debugging request among the supplied session cards. Write result.json with session_id. This measures navigation, not implementation correctness.",
        {"session-cards.json": json.dumps([
            {"session_id": "inventory", "request": newest},
            {"session_id": "dashboard", "request": "Design the frontend dashboard colors"},
            {"session_id": "readme", "request": "Document setup instructions"}]) + "\n"},
        {"kind": "service", "consumer": {"service": "session_categorization", "dimension": "task", "input": {
            "session_id": str(uuid.uuid5(uuid.NAMESPACE_URL, "development:inventory-session")),
            "source_sequence": 1, "newest_request": newest}}},
        "Activate the newest ordinary request; capture disposable task/domain labels for its exact session and source sequence; display them in session cards.",
        "Independent navigation artifact selects the inventory card; separately measure navigation actions/time and label preservation of session authority.",
        "Producer service and reading existing labels do not prove activation or frontend use. Need exact activation, application labels, actual navigation and optional ledger.")

    facts = {"capture_complete": True, "status": "completed", "timed_out": False,
             "false_completion": False, "preservation_passed": False,
             "artifact_observed": True, "artifact_correct": True}
    add("checkout-eval-findability", "eval_categorization",
        "Find the checkout evaluation run requiring preservation investigation, not the timeout or malformed artifact run. Write result.json with run_id. Do not change outcomes or claim the coding task succeeded.",
        {"run-cards.json": json.dumps([
            {"run_id": "checkout", "facts": facts, "detail": "Solution checks passed but protected lockfile changed."},
            {"run_id": "search", "facts": {"timed_out": True}, "detail": "Deadline exhausted."},
            {"run_id": "cart", "facts": {"artifact_correct": False}, "detail": "Malformed JSON artifact."}]) + "\n"},
        {"kind": "eval_diagnosis", "oracle": facts,
         "diagnosis": {"labels": ["preservation_violation"]}},
        "Capture an actual evaluation's independently established artifact/preservation facts, invoke advisory diagnosis attachment on that run and expose attached labels to navigation.",
        "Independent navigation artifact selects checkout while original failed preservation remains failed; labels improve findability only, not coding completion.",
        "Authored oracle facts exercise diagnosis attachment only. Need actual eval capture, exact attached-label application, navigation artifact and optional ledger.")
    return copy.deepcopy(tasks)


def scenario(identity):
    """Resolve an exact development identity; unknown cases fail closed."""
    for item in scenarios():
        if item["scenario_id"] == identity:
            return item
    raise ValueError("unknown development task")


def materialize(identity, workspace):
    """Write task data only to a fresh existing workspace; no oracle code or truth."""
    item = scenario(identity)
    root = Path(workspace)
    if any(p.is_symlink() for p in (root, *root.parents)) or not root.is_dir() or any(root.iterdir()):
        raise ValueError("fresh non-symlink workspace required")
    for name, content in item["starter_files"].items():
        (root / name).write_text(content, encoding="utf-8")
    payload = {k: v for k, v in item.items() if k != "starter_files"}
    (root / "consumer-input.json").write_text(json.dumps(payload, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return payload
