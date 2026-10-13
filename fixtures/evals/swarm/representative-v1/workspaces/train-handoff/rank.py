def rank(values):
    """Sort ids by (numeric value,id); checksum=sum(one-based position*value)."""
    ids = sorted(values)
    return {"ordered_ids":ids,"checksum":sum((i+1)*int(values[k]) for i,k in enumerate(ids))}
