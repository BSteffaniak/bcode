from normalize import normalize
from rank import rank
def evaluate(rows):
    """Normalize first; ranking consumes that mapping. No input mutation."""
    values = normalize(rows)
    return {"normalized":values,"ranking":rank(values)}
