def solve(rows):
    """Return sorted intersection of all tag lists; empty outer input=[]; no mutation."""
    return sorted(set(x for row in rows for x in row))
