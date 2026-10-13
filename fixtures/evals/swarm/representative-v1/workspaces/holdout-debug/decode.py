def decode(value):
    """Parse RFC3339 instants with an explicit offset or Z."""
    from datetime import datetime
    return datetime.fromisoformat(value.replace("Z", "+00:00"))
