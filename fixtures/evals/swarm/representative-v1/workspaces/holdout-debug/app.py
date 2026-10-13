from decode import decode
from window import inside
def evaluate(request):
    """Return ids inside the window, preserving input order; no mutation."""
    start, end = decode(request["start"]), decode(request["end"])
    return [row["id"] for row in request["events"] if inside(decode(row["at"]), start, end)]
