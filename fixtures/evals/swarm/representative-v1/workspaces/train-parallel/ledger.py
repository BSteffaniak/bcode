def solve(rows):
    """Signed total of integer entries; empty=0."""
    return sum(abs(x) for x in rows)
