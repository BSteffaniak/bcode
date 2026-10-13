def inside(value, start, end):
    """Use a half-open instant interval [start,end), comparing absolute time."""
    return start <= value <= end
