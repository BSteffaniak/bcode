"""Source-backed progressive-v1 fixture and independent section oracle.

These are explicit user-provided fixture facts about historical evaluation snapshots,
not claims of natural-language retention quality. Every byte retained has source meaning.
"""
import json
from pathlib import Path

FORMAT = "bcode.compaction.continuation-sections.v1"
SECTIONS = ("decisions", "active_work_blockers", "findings_evidence")


def semantic_statement(statement):
    """Fixture-only identical restatements; never infer synonyms or discard qualifiers."""
    parts = statement.split(" Fixture restatement: ")
    return parts[0] if parts[0] and all(part == parts[0] for part in parts) else statement


def records():
    corpus = json.loads(Path(__file__).with_name("retention_cases.json").read_text())
    result = []
    for case in corpus["cases"]:
        for index, batch in enumerate(case["batches"], 1):
            for check in batch["oracle"]:
                category = check["category"]
                if category in ("critical_instruction", "instruction", "uncertainty", "blocker"):
                    kind, section = "BLOCKER", "active_work_blockers"
                elif category in ("scope", "decision"):
                    kind, section = "DECISION", "decisions"
                else:
                    kind, section = "FACT", "findings_evidence"
                key = f'{case["id"]}/{index}/{check["id"]}'
                # Snapshot qualification prevents historical corrections from contradicting
                # one another. This attribution is part of the meaningful retained evidence.
                statement = (f'Historical snapshot {case["id"]}, batch {index}, '
                             f'sources {",".join(check["sources"])}: {check["proposition"]}')
                result.append((kind, section, key, statement))
    return result


def line(record):
    kind, _, key, statement = record
    return f'BCODE_FIXTURE_{kind} ' + json.dumps({"key": key, "statement": statement})


def expected(selected):
    sections = {name: {} for name in SECTIONS}
    for _, section, key, statement in selected:
        sections[section][key] = semantic_statement(statement)
    return sections


def verify_sections(evidence, selected):
    sections = json.loads(evidence["continuation"])
    if sections.get("format") != FORMAT:
        raise ValueError("missing versioned section carry")
    for name, values in expected(selected).items():
        actual = {}
        for part in sections[name].splitlines():
            for key, statement in json.loads(part).items():
                if key in actual:
                    raise ValueError("duplicate accepted evidence")
                actual[key] = semantic_statement(statement)
        if actual != values:
            raise ValueError(f"lost, invented, or changed {name}")


def verify_context(control, before, selected):
    """Verify actual provider messages captured after publication/restart, not exports."""
    candidates = [p for p in set(control.glob("context-*")) - before if (p / "context.json").exists()]
    if not candidates:
        raise ValueError("missing actual post-marker provider request capture")
    expected_records = expected(selected)
    for path in candidates:
        request = json.loads((path / "request.json").read_text())
        if "provider_context" in request:
            raise ValueError("credential custody leaked into capture")
        texts = [part.get("text", "") for message in request["messages"]
                 for part in message.get("content", []) if part.get("type") == "text"]
        wire = "\n".join(texts)
        # Provider adapters may render either nested JSON strings or plain summary text.
        # Decode only escaping, never append oracle content to a model request.
        normalized = wire
        for _ in range(3):
            normalized = normalized.replace(chr(92) + '"', '"').replace(chr(92) + "n", "\n")
        for values in expected_records.values():
            for statement in values.values():
                if statement not in wire and statement not in normalized:
                    raise ValueError("actual post-marker context lost source-backed evidence")
    return len(candidates)


def verify_shortening(before, response, preferred_chars):
    """Fixture diagnostic only: size preference is NOT a host capacity acceptance rule."""
    if response.get("format") != "bcode.compaction.section-consolidation.v1":
        raise ValueError("unsupported consolidation response")
    if set(response) != {"format", "sections"}:
        raise ValueError("consolidation must not claim scope or source coverage")
    patch = response["sections"]
    if not isinstance(patch, dict) or set(patch) - {*SECTIONS, "background"}:
        raise ValueError("unknown consolidation section or coverage field")
    if not all(isinstance(value, str) for value in patch.values()):
        raise ValueError("consolidation sections must be strings")
    prior = json.loads(before["continuation"])
    merged = dict(prior)
    merged.update(response.get("sections", {}))
    after = dict(before, continuation=json.dumps(merged, separators=(",", ":")))
    def meanings(sections):
        result = {}
        for name in SECTIONS:
            result[name] = {}
            for part in sections[name].splitlines():
                for key, statement in json.loads(part).items():
                    if key in result[name]:
                        raise ValueError("duplicate consolidation evidence")
                    result[name][key] = semantic_statement(statement)
        return result
    if meanings(prior) != meanings(merged) or prior["background"] != merged["background"]:
        raise ValueError("consolidation lost or changed accepted evidence")
    if after.get("current_scope") != before.get("current_scope"):
        raise ValueError("consolidation changed protected scope")
    # Compare identical JSON codecs on both sides. The provider uses compact
    # UTF-8 JSON; Python's default ASCII escaping can otherwise report growth
    # merely from reserializing CJK/emoji or map whitespace.
    before_size = len(json.dumps(prior, ensure_ascii=False, separators=(",", ":")))
    after_size = len(json.dumps(merged, ensure_ascii=False, separators=(",", ":")))
    return {"shortened": after_size < before_size, "met_preference": after_size <= preferred_chars,
            "before_chars": before_size, "after_chars": after_size,
            "hard_capacity_acceptance": "host-owned; not established by fixture"}


def verify_attempts(control):
    """Finite request bounds and observed accepted carry growth, not provider text sizes."""
    syntheses, consolidations, carries = {}, 0, []
    shortening = []
    for path in control.glob("attempt-*"):
        if (path / "context.json").exists():
            continue
        sources = (path / "sources.jsonl").read_text()
        request = json.loads((path / "request.json").read_text())
        texts = [part.get("text", "") for message in request["messages"]
                 for part in message.get("content", []) if part.get("type") == "text"]
        prompt = "\n".join(texts)
        if "bcode.compaction.section-consolidation.v1" in prompt:
            consolidations += 1
            carry_marker = "Complete accepted carry (derived, not authority):\n"
            if carry_marker not in prompt or not (path / "response.json").exists():
                raise ValueError("missing consolidation carry or delivered response evidence")
            before, _ = json.JSONDecoder().raw_decode(prompt.split(carry_marker, 1)[1])
            response = json.loads((path / "response.json").read_text())
            shortening.append(verify_shortening(before, response, 6000))
            continue
        syntheses[sources] = syntheses.get(sources, 0) + 1
        if syntheses[sources] > 4:
            raise ValueError("same-source synthesis exceeded finite repair/retry allowance")
        if "Prior structured synthesis: " in prompt:
            prior = prompt.split("Prior structured synthesis: ", 1)[1].split("\n\nSource fragments", 1)[0]
            carries.append(len(json.loads(prior)["continuation"]))
    if not carries or max(carries) <= 6000:
        raise ValueError("did not observe accepted >6000 carry entering another synthesis")
    # Safe carry growth must not force needless lossy consolidation merely to hit a
    # presentation preference. Zero means the run proved growth/retention, not repair.
    if consolidations > 2 * len(syntheses):
        raise ValueError("consolidation exceeded finite per-source allowance")
    return {"format": "bcode.fake.progressive-verification.v1", "requests": sum(syntheses.values()),
            "consolidations": consolidations, "shortening_diagnostics": shortening,
            "max_accepted_continuation_chars": max(carries)}


def phases():
    """Three genuinely growing batches; second crosses 5840, third crosses 6000.

    Batches are selected by the serialized meaningful evidence size; nothing is padded,
    repeated to force size, clipped, or preloaded into a summary.
    """
    all_records = records()
    result, accepted, batch = [], [], []
    for record in all_records:
        accepted.append(record)
        batch.append(record)
        values = expected(accepted)
        carry = {name: json.dumps(records, ensure_ascii=False, separators=(",", ":")) if records else ""
                 for name, records in values.items()}
        carry.update(format=FORMAT, background='{"keys":{},"facts":{},"pending":null}')
        size = len(json.dumps(carry, ensure_ascii=False, separators=(",", ":")))
        target = (2400, 5840)[len(result)] if len(result) < 2 else None
        if target is not None and size >= target:
            result.append(batch)
            batch = []
    if batch:
        result.append(batch)
    if len(result) != 3:
        raise ValueError("source corpus cannot exercise three growth stages")
    return result
