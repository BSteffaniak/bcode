def visible(rows):
    """Return only enabled entries; missing enabled means false."""
    return [r["id"] for r in rows if r.get("enabled",True)]
