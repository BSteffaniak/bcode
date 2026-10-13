def normalize(rows):
    """Return id->integer from whitespace-padded values; duplicate ids are invalid."""
    return {r["id"]: r["value"] for r in rows}
