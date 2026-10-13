def lower_median(rows):
    """Lower median; None if empty."""
    return sorted(rows)[(len(rows)-1)//2] if rows else None
