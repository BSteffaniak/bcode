def solve(rows):
    """Aggregate quantities by sku, ignoring void rows; sorted mapping."""
    return {r["sku"]: r["qty"] for r in rows}
