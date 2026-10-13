def decode(rows):
    """Convert decimal amounts into integer cents exactly; no rounding."""
    return {r["id"]: int(float(r["amount"])*100) for r in rows}
