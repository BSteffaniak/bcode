"""Verify opt-in fake fault delivery separately from canonical acceptance verification."""
import json
from pathlib import Path


def verify_injections(control, outcome):
    attempts = []
    for path in Path(control).glob("attempt-*"):
        ready = json.loads((path / "ready.json").read_text())
        finished = json.loads((path / "finished.json").read_text())
        if ready["outcome"] != outcome or not finished.get("response_ok"):
            raise ValueError("injection did not finish with the selected response")
        text = (path / "response.json").read_text()
        prior = None
        request_path = path / "request.json"
        if request_path.exists():
            request = json.loads(request_path.read_text())
            for message in request["messages"]:
                for block in message.get("content", []):
                    prompt = block.get("text", "")
                    if "Prior structured synthesis: " in prompt:
                        prior = json.loads(prompt.split("Prior structured synthesis: ", 1)[1]
                                           .split("\n\nSource fragments (JSON lines):\n", 1)[0])
        attempts.append((ready, text, prior))
    if not attempts:
        raise ValueError("no actual controlled synthesis attempts")
    if outcome == "near-limit":
        for _, text, _ in attempts:
            if len(text) != 5924:
                raise ValueError("near-limit complete serialized carry is not 5924 characters")
            response = json.loads(text)
            if "continuation" in response:
                json.loads(response["continuation"])
            elif response.get("format") == "bcode.compaction.section-update.v2":
                background = response["sections"]["background"].strip()
                if not background.startswith("BCODE_FIXTURE_STATE_V1 "):
                    raise ValueError("near-limit response lost fixture state")
                json.loads(background.removeprefix("BCODE_FIXTURE_STATE_V1 "))
            else:
                raise ValueError("unsupported near-limit response contract")
    if outcome.endswith("repair"):
        observed = False
        for initial, text, prior in attempts:
            if initial["correction"]:
                continue
            corrections = [candidate for ready, candidate, _ in attempts
                           if ready["correction"] and ready["source_sha256"] == initial["source_sha256"]]
            if not corrections:
                if outcome in ("omit-scope-repair", "paraphrase-scope-repair"):
                    # A response without carried scope can mutate newly discovered scope
                    # without violating the preservation validator. Require an effective
                    # same-source fault/correction pair elsewhere in this run instead.
                    continue
                raise ValueError("initial injected response has no same-source correction")
            if outcome == "repair":
                try:
                    json.loads(text)
                except json.JSONDecodeError:
                    observed = True
                else:
                    raise ValueError("malformed injection was not delivered")
            elif outcome == "oversize-repair":
                if len(json.loads(text)["continuation"]) <= 65536:
                    raise ValueError("oversized injection was not delivered")
                observed = True
            else:
                bad = json.loads(text)["current_scope"]
                if prior is None or not prior.get("current_scope"):
                    continue
                expected = prior["current_scope"]
                if outcome == "omit-scope-repair" and bad == expected[1:]:
                    observed = True
                if outcome == "paraphrase-scope-repair" and bad:
                    mutated = [dict(scope) for scope in expected]
                    mutated[0]["statement"] += " (paraphrased)"
                    if bad == mutated:
                        observed = True
            for correction in corrections:
                json.loads(correction)
        if not observed:
            raise ValueError("no effective selected fault followed by correction was observed")
    return {"outcome": outcome, "attempts": len(attempts), "fault_delivery_only": True}
