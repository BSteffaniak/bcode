class TransientError(Exception): pass
def publish(values, state, failures):
    """Fail first failures attempts, commit once on success; state tracks attempts and commits."""
    state["attempts"] += 1
    state["commits"].append(dict(values))
    if state["attempts"] <= failures: raise TransientError("injected")
    return dict(values)
